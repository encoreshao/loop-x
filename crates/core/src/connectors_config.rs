//! Connector account registry (mirrors `bin/connectors_config.py`):
//! `~/.loopx/connectors.json` holds the native accounts (no secrets -
//! those live in the Keychain via [`SecretStore`], keyed by the connector id).
//! GitLab instances (`~/.gitlab/config.json`), Slack webhooks
//! (`~/.slack/config.json`) and mailboxes (`inboxes.json`) are read through as
//! external, read-only accounts that their own pages keep managing. Nothing
//! returned by `list_accounts`/`get_account` ever contains a secret.
//!
//! Also here: the per-checkout connector test results
//! (`outputs/connectors/test-results.json`) and [`notify_loop`], the real
//! wiring of `bin/notify.py` (`loop_notify::notify`) to the connectors.

use crate::connectors::secrets::{open_connector_store, SecretStore};
use crate::connectors::{
    self, build, get_type, Account, Auth, Connector, ConnectorSource, HttpClient, LoadFailure,
    NotifyAdapter, UreqClient,
};
use crate::loop_notify::{self, LoadError, NotifyResult, SendError};
use crate::{config, keychain, loops_config, pydumps, pyjson, runner_util, slack};
use chrono::Utc;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const NATIVE: &str = "native";

/// `ConnectorConfigError`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct ConnectorConfigError(pub String);

/// `^[a-z0-9][a-z0-9-]{0,47}$`: an id a native account may have (and loops'
/// `notify` may list).
pub fn is_valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    id.chars().count() <= 48
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Where each account source lives; `None` = the per-machine default,
/// resolved at call time.
#[derive(Debug, Clone, Default)]
pub struct Paths {
    pub config_path: Option<PathBuf>,
    pub gitlab_config_path: Option<PathBuf>,
    pub slack_config_path: Option<PathBuf>,
    pub inbox_config_path: Option<PathBuf>,
}

fn user_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

pub fn default_config_path() -> PathBuf {
    config::home_dir().join("connectors.json")
}

impl Paths {
    pub fn config(&self) -> PathBuf {
        self.config_path.clone().unwrap_or_else(default_config_path)
    }
    pub fn gitlab(&self) -> PathBuf {
        self.gitlab_config_path
            .clone()
            .unwrap_or_else(|| user_home().join(".gitlab").join("config.json"))
    }
    pub fn slack(&self) -> PathBuf {
        self.slack_config_path
            .clone()
            .unwrap_or_else(slack::default_config_path)
    }
    pub fn inbox(&self) -> PathBuf {
        self.inbox_config_path
            .clone()
            .unwrap_or_else(|| config::home_dir().join("inboxes.json"))
    }
}

/// `_str`: `None` -> `""`, a string as is, anything else `str(value)`.
fn text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => pydumps::py_str(other),
    }
}

/// `default` when the file is missing; `ConnectorConfigError` if unreadable.
fn read_json(path: &Path, default: Value) -> Result<Value, ConnectorConfigError> {
    match std::fs::read_to_string(path) {
        Ok(t) => serde_json::from_str(&t)
            .map_err(|e| ConnectorConfigError(format!("Could not read {}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(default),
        Err(e) => Err(ConnectorConfigError(format!(
            "Could not read {}: {e}",
            path.display()
        ))),
    }
}

/// Best-effort: external files are owned by other pages, so a broken one just
/// contributes no accounts rather than breaking the registry.
fn read_external(path: &Path) -> Map<String, Value> {
    match read_json(path, json!({})) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

fn read_native_raw(config_path: &Path) -> Result<Vec<Map<String, Value>>, ConnectorConfigError> {
    match read_json(config_path, json!([]))? {
        Value::Array(a) => Ok(a
            .into_iter()
            .filter_map(|e| match e {
                Value::Object(m) if m.get("id").is_some_and(Value::is_string) => Some(m),
                _ => None,
            })
            .collect()),
        _ => Err(ConnectorConfigError(format!(
            "{} must contain a JSON list",
            config_path.display()
        ))),
    }
}

fn native_view(entry: &Map<String, Value>) -> Account {
    let id = text(entry.get("id"));
    let settings = match entry.get("settings") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let label = text(entry.get("label"));
    Account {
        kind: text(entry.get("type")),
        label: if label.is_empty() { id.clone() } else { label },
        id,
        enabled: entry.get("enabled") != Some(&Value::Bool(false)),
        settings: settings
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(text(Some(v)))))
            .collect(),
        managed_by: NATIVE.to_string(),
        // A non-secret marker set_oauth_secret writes, so the list view can say
        // "Connected" without ever reading the Keychain.
        oauth_connected: entry
            .get("oauth_connected_at")
            .is_some_and(slack::is_truthy),
    }
}

fn external_account(
    id: &str,
    kind: &str,
    label: &str,
    settings: Value,
    managed_by: &str,
) -> Account {
    Account {
        id: id.to_string(),
        kind: kind.to_string(),
        label: label.to_string(),
        enabled: true,
        settings: settings.as_object().cloned().unwrap_or_default(),
        managed_by: managed_by.to_string(),
        oauth_connected: false,
    }
}

fn external(gitlab: &Path, slack_path: &Path, inbox: &Path) -> Vec<Account> {
    let mut accounts = Vec::new();
    if let Some(Value::Object(instances)) = read_external(gitlab).get("instances") {
        for (name, inst) in instances {
            if let Value::Object(inst) = inst {
                accounts.push(external_account(
                    name,
                    "gitlab",
                    name,
                    json!({"url": text(inst.get("url"))}),
                    "gitlab-config",
                ));
            }
        }
    }
    let slack_cfg = read_external(slack_path);
    if slack_cfg.get("webhook_url").is_some_and(slack::is_truthy) {
        accounts.push(external_account(
            "slack-default",
            "slack",
            "Slack",
            json!({}),
            "slack-config",
        ));
    }
    if let Some(Value::Object(bundles)) = slack_cfg.get("bundle_webhooks") {
        for bundle in bundles.keys() {
            accounts.push(external_account(
                &format!("slack-{bundle}"),
                "slack",
                &format!("Slack ({bundle})"),
                json!({}),
                "slack-config",
            ));
        }
    }
    if let Some(Value::Array(boxes)) = read_external(inbox).get("inboxes") {
        for b in boxes {
            let Some(name) = b.get("name").and_then(Value::as_str) else {
                continue;
            };
            let label = text(b.get("label"));
            accounts.push(external_account(
                name,
                "mailbox",
                if label.is_empty() { name } else { &label },
                json!({"provider": text(b.get("provider")), "account": text(b.get("account"))}),
                "inboxes",
            ));
        }
    }
    accounts
}

/// Native accounts, then the external ones.
pub fn list_accounts(paths: &Paths) -> Result<Vec<Account>, ConnectorConfigError> {
    let mut accounts: Vec<Account> = read_native_raw(&paths.config())?
        .iter()
        .map(native_view)
        .collect();
    accounts.extend(external(&paths.gitlab(), &paths.slack(), &paths.inbox()));
    Ok(accounts)
}

/// `Ok(None)` is Python's `KeyError(account_id)`.
pub fn get_account(
    account_id: &str,
    paths: &Paths,
) -> Result<Option<Account>, ConnectorConfigError> {
    Ok(list_accounts(paths)?
        .into_iter()
        .find(|a| a.id == account_id))
}

fn with_capability(accounts: Vec<Account>, capability: &str) -> Vec<Account> {
    accounts
        .into_iter()
        .filter(|a| a.enabled && get_type(&a.kind).is_some_and(|t| t.has_capability(capability)))
        .collect()
}

pub fn accounts_with_capability(
    capability: &str,
    paths: &Paths,
) -> Result<Vec<Account>, ConnectorConfigError> {
    Ok(with_capability(list_accounts(paths)?, capability))
}

// ---- writes ---------------------------------------------------------------------

fn write_entries(entries: &[Map<String, Value>], config_path: &Path) -> std::io::Result<()> {
    if let Some(dir) = config_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Unique per write: the dashboard serves requests on several threads.
    let name = config_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = config_path.with_file_name(format!(
        ".{name}.{}.tmp",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    let body = pyjson::to_string_pretty(&Value::Array(
        entries.iter().cloned().map(Value::Object).collect(),
    ));
    let result = std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, config_path));
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Serialises read-modify-write of connectors.json (and the Keychain entries
/// that go with it) across the dashboard's request threads.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Best effort: put every snapshotted Keychain entry back as it was (`None` =
/// it did not exist) after the config write failed.
fn restore_secrets(store: &dyn SecretStore, snapshot: &[(String, Option<String>)]) {
    for (reference, value) in snapshot {
        let _ = match value {
            None => store.delete(reference),
            Some(v) => store.set(reference, v),
        };
    }
}

fn has_control_char(s: &str) -> bool {
    s.chars().any(|c| (c as u32) < 32 || c as u32 == 127)
}

fn now_stamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Create or edit a native account. `fields` is the form dict: `id`, `type`,
/// `label`, `enabled` (bool) plus the type's setting keys, all text. A blank
/// `secret` keeps the stored one. Returns `(ok, message)`.
pub fn upsert_account(
    fields: &Value,
    secret: Option<&str>,
    original_id: &str,
    paths: &Paths,
    store: &dyn SecretStore,
) -> (bool, String) {
    let _guard = lock();
    let empty = Map::new();
    let fields = fields.as_object().unwrap_or(&empty);
    if fields
        .iter()
        .any(|(k, v)| !(v.is_null() || v.is_string() || (k == "enabled" && v.is_boolean())))
    {
        return (false, "All connector fields must be text".to_string());
    }
    let field = |k: &str| {
        fields
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let (new_id, type_name, label) = (field("id"), field("type"), field("label"));
    let original_id = original_id.trim().to_string();
    if !is_valid_id(&new_id) {
        return (
            false,
            "Connector id must be lowercase letters, digits and dashes".to_string(),
        );
    }
    let Some(info) = get_type(&type_name) else {
        return (false, format!("Unknown connector type {type_name}"));
    };
    if info.external {
        return (
            false,
            format!("{type_name} accounts are managed on their own page"),
        );
    }
    if label.is_empty() {
        return (false, "Label is required".to_string());
    }
    let config_path = paths.config();
    let mut entries = match read_native_raw(&config_path) {
        Ok(e) => e,
        Err(e) => return (false, format!("Could not read connectors: {e}")),
    };
    let existing_idx = if original_id.is_empty() {
        None
    } else {
        entries
            .iter()
            .position(|e| text(e.get("id")) == original_id)
    };
    if !original_id.is_empty() && existing_idx.is_none() {
        return (false, format!("No connector with id {original_id}"));
    }
    if let Some(i) = existing_idx {
        let old_type = text(entries[i].get("type"));
        if old_type != type_name {
            // The stored secret belongs to the old type's service; reusing it
            // for another type would send e.g. a GitHub token to a Jira site.
            return (
                false,
                format!(
                    "Connector {original_id} is a {old_type} account; its type cannot be changed"
                ),
            );
        }
    }
    let taken = entries
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != existing_idx)
        .map(|(_, e)| text(e.get("id")))
        .chain(
            external(&paths.gitlab(), &paths.slack(), &paths.inbox())
                .into_iter()
                .map(|a| a.id),
        )
        .any(|id| id == new_id);
    if taken {
        return (
            false,
            format!("Connector id {new_id} is already used by another account"),
        );
    }
    let settings: Map<String, Value> = info
        .fields
        .iter()
        .map(|f| (f.key.to_string(), Value::String(field(f.key))))
        .collect();
    let secret = secret.unwrap_or("").trim().to_string();
    if info.auth == Auth::OauthGoogle && !secret.is_empty() {
        // Its secret is a refresh token that only the Connect with Google flow
        // (set_oauth_secret) may store - never one pasted into a form.
        return (
            false,
            format!(
                "{} connects with Google sign-in; it does not take a pasted secret",
                info.label
            ),
        );
    }
    // A control character would end up inside an HTTP header value.
    if has_control_char(&secret) {
        return (
            false,
            "The secret must not contain control characters or line breaks".to_string(),
        );
    }
    let errors = info.validate(&settings, &secret, existing_idx.is_none());
    if !errors.is_empty() {
        return (false, errors.join("; "));
    }
    let enabled = match fields.get("enabled") {
        Some(Value::Bool(b)) => *b,
        _ => existing_idx.is_none_or(|i| entries[i].get("enabled") != Some(&Value::Bool(false))),
    };
    let created_at = existing_idx
        .map(|i| &entries[i])
        .and_then(|e| e.get("created_at"))
        .filter(|v| slack::is_truthy(v))
        .cloned()
        .unwrap_or_else(|| Value::String(now_stamp()));
    let mut entry = Map::new();
    entry.insert("id".into(), new_id.clone().into());
    entry.insert("type".into(), type_name.clone().into());
    entry.insert("label".into(), label.into());
    entry.insert("enabled".into(), enabled.into());
    entry.insert("settings".into(), Value::Object(settings));
    entry.insert("created_at".into(), created_at);
    if let Some(at) = existing_idx
        .and_then(|i| entries[i].get("oauth_connected_at"))
        .filter(|v| slack::is_truthy(v))
    {
        entry.insert("oauth_connected_at".into(), at.clone());
    }
    let renamed = existing_idx.is_some() && original_id != new_id;
    // What the Keychain held before, so a failed config write can undo the
    // secret changes instead of leaving an orphan or a broken rename.
    let keychain_failed = || {
        (
            false,
            "Could not store the secret in the Keychain".to_string(),
        )
    };
    let mut touched = vec![new_id.clone()];
    if renamed {
        touched.push(original_id.clone());
    }
    let mut snapshot: Vec<(String, Option<String>)> = Vec::new();
    if !secret.is_empty() || touched.len() > 1 {
        for r in &touched {
            match store.get(r) {
                Ok(v) => snapshot.push((r.clone(), v)),
                Err(_) => return keychain_failed(),
            }
        }
    }
    let stored = (|| -> Result<(), keychain::SecretError> {
        if !secret.is_empty() {
            store.set(&new_id, &secret)?;
            if renamed {
                store.delete(&original_id)?;
            }
        } else if renamed {
            if let Some(old) = store.get(&original_id)?.filter(|o| !o.is_empty()) {
                store.set(&new_id, &old)?;
            }
            store.delete(&original_id)?;
        }
        Ok(())
    })();
    if stored.is_err() {
        // No exception detail: Keychain stderr can echo the secret.
        return keychain_failed();
    }
    match existing_idx {
        Some(i) => entries[i] = entry,
        None => entries.push(entry),
    }
    if let Err(e) = write_entries(&entries, &config_path) {
        restore_secrets(store, &snapshot);
        return (false, format!("Could not save connectors: {e}"));
    }
    (true, format!("Saved connector {new_id}"))
}

/// Store the refresh token a Connect with Google sign-in returned for a native
/// account whose type has `auth == OauthGoogle`. No message includes the token.
pub fn set_oauth_secret(
    account_id: &str,
    refresh_token: Option<&str>,
    paths: &Paths,
    store: &dyn SecretStore,
) -> (bool, String) {
    let _guard = lock();
    let config_path = paths.config();
    let entry = match read_native_raw(&config_path) {
        Ok(entries) => entries
            .into_iter()
            .find(|e| text(e.get("id")) == account_id),
        Err(_) => return (false, "Could not read connectors".to_string()),
    };
    let Some(entry) = entry else {
        return (false, format!("No connector with id {account_id}"));
    };
    if get_type(&text(entry.get("type"))).is_none_or(|t| t.auth != Auth::OauthGoogle) {
        return (
            false,
            format!("Connector {account_id} does not use Google sign-in"),
        );
    }
    let token = refresh_token.unwrap_or("").trim();
    if token.is_empty() || has_control_char(token) {
        return (
            false,
            "Google returned an unusable refresh token".to_string(),
        );
    }
    if store.set(account_id, token).is_err() {
        // No exception detail: Keychain stderr can echo the secret.
        return (
            false,
            "Could not store the secret in the Keychain".to_string(),
        );
    }
    if let Ok(mut entries) = read_native_raw(&config_path) {
        for e in entries.iter_mut() {
            if text(e.get("id")) == account_id {
                e.insert("oauth_connected_at".into(), now_stamp().into());
            }
        }
        // The token is stored; only the "Connected" marker would be missing.
        let _ = write_entries(&entries, &config_path);
    }
    let label = text(entry.get("label"));
    (
        true,
        format!(
            "Connected {}",
            if label.is_empty() {
                account_id.to_string()
            } else {
                label
            }
        ),
    )
}

/// Delete a native account: the secret first, then the config entry.
pub fn delete_account(
    account_id: &str,
    config_path: Option<&Path>,
    store: &dyn SecretStore,
) -> (bool, String) {
    let _guard = lock();
    let config_path = config_path.map_or_else(default_config_path, Path::to_path_buf);
    let entries = match read_native_raw(&config_path) {
        Ok(e) => e,
        Err(e) => return (false, format!("Could not read connectors: {e}")),
    };
    if !entries.iter().any(|e| text(e.get("id")) == account_id) {
        return (
            false,
            format!(
                "Only connectors created here can be deleted; manage {account_id} on its own page"
            ),
        );
    }
    if let Err(e) = store.delete(account_id) {
        return (false, format!("Could not remove the stored secret: {e}"));
    }
    let kept: Vec<_> = entries
        .into_iter()
        .filter(|e| text(e.get("id")) != account_id)
        .collect();
    if let Err(e) = write_entries(&kept, &config_path) {
        return (false, format!("Could not save connectors: {e}"));
    }
    (true, format!("Deleted connector {account_id}"))
}

// ---- opening a connector ----------------------------------------------------------

/// `load_connector`: the live connector for `account_id`, with its secret from
/// the Keychain (native accounts) or the external config file.
pub fn load_connector(
    account_id: &str,
    store: &dyn SecretStore,
    paths: &Paths,
    http: Arc<dyn HttpClient>,
) -> Result<Box<dyn Connector>, LoadFailure> {
    let account = get_account(account_id, paths)
        .map_err(|e| LoadFailure::Config(e.0))?
        .ok_or(LoadFailure::Unknown)?;
    if get_type(&account.kind).is_none() {
        return Err(LoadFailure::Unknown);
    }
    let resolve = |raw: Option<&str>| -> Result<Option<String>, LoadFailure> {
        match raw {
            None => Ok(None),
            Some(v) => keychain::resolve_value(v)
                .map(Some)
                .map_err(|_| LoadFailure::Other("KeychainError".into())),
        }
    };
    let secret = match account.managed_by.as_str() {
        NATIVE => store
            .get(account_id)
            .map_err(|_| LoadFailure::Other("KeychainError".into()))?,
        "gitlab-config" => {
            let cfg = read_external(&paths.gitlab());
            let token = cfg
                .get("instances")
                .and_then(|i| i.get(account_id))
                .and_then(|i| i.get("token"))
                .and_then(Value::as_str);
            resolve(token)?
        }
        "slack-config" => {
            let cfg = read_external(&paths.slack());
            let url = if account_id == "slack-default" {
                cfg.get("webhook_url").and_then(Value::as_str)
            } else {
                cfg.get("bundle_webhooks")
                    .and_then(|b| b.get(&account_id["slack-".len()..]))
                    .and_then(Value::as_str)
            };
            resolve(url)?
        }
        _ => None,
    };
    build(account, secret, http).ok_or(LoadFailure::Unknown)
}

/// The real `accounts_with_capability` / `load_connector` pair for plugins.
pub struct ConfigConnectors {
    paths: Paths,
    store: Option<Arc<dyn SecretStore>>,
    http: Arc<dyn HttpClient>,
}

impl ConfigConnectors {
    /// Default paths, the Keychain and the real HTTP client.
    pub fn real() -> Self {
        Self {
            paths: Paths::default(),
            store: None,
            http: Arc::new(UreqClient::default()),
        }
    }

    pub fn with_paths(mut self, paths: Paths) -> Self {
        self.paths = paths;
        self
    }

    pub fn with_store(mut self, store: Arc<dyn SecretStore>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_http(mut self, http: Arc<dyn HttpClient>) -> Self {
        self.http = http;
        self
    }

    fn store(&self) -> Result<Arc<dyn SecretStore>, LoadFailure> {
        match &self.store {
            Some(s) => Ok(s.clone()),
            None => open_connector_store()
                .map(Arc::from)
                .map_err(|_| LoadFailure::Other("KeychainError".into())),
        }
    }
}

impl ConnectorSource for ConfigConnectors {
    /// A broken connectors file contributes no accounts (the Python raises
    /// `ConnectorConfigError`; the message goes to stderr here instead).
    fn accounts_with_capability(&self, capability: &str) -> Vec<Account> {
        match accounts_with_capability(capability, &self.paths) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("loopx: connector accounts unavailable: {e}");
                Vec::new()
            }
        }
    }

    fn load(&self, account_id: &str) -> Result<Box<dyn Connector>, LoadFailure> {
        let store = self.store()?;
        load_connector(account_id, store.as_ref(), &self.paths, self.http.clone())
    }
}

// ---- notify.py wiring -------------------------------------------------------------

/// Error class name for a default-Slack failure, as Python's notify.py reports it.
pub fn slack_error_class(msg: &str) -> String {
    if msg.starts_with("HTTP Error") {
        "HTTPError"
    } else if msg.contains("No such file") {
        "FileNotFoundError"
    } else if msg.contains("missing \"webhook_url\"") {
        "KeyError"
    } else if msg.contains("expected") || msg.contains("EOF while") || msg.contains("key must be") {
        "JSONDecodeError"
    } else {
        "URLError"
    }
    .to_string()
}

/// `notify.notify(loop_name, text, blocks)` against the real registry,
/// connectors and default Slack webhook. Never fails.
pub fn notify_loop(loop_name: &str, text: &str, blocks: Option<&Value>) -> Vec<NotifyResult> {
    notify_loop_with(loop_name, text, blocks, &ConfigConnectors::real())
}

/// [`notify_loop`] with the connector source injected.
pub fn notify_loop_with(
    loop_name: &str,
    text: &str,
    blocks: Option<&Value>,
    source: &dyn ConnectorSource,
) -> Vec<NotifyResult> {
    let lookup = |name: &str| -> Result<Value, String> {
        loops_config::get_loop(name, &loops_config::default_config_path(), None)
            .map_err(|e| e.to_string())
    };
    let loader = |id: &str| -> Result<Box<dyn loop_notify::NotifyConnector>, LoadError> {
        source
            .load(id)
            .map(|c| Box::new(NotifyAdapter(c)) as Box<dyn loop_notify::NotifyConnector>)
            .map_err(LoadError::from)
    };
    let sender = |text: &str, blocks: Option<&Value>| -> Result<(), SendError> {
        let url = slack::load_webhook_url(&slack::default_config_path(), None)
            .map_err(|m| SendError::Other(slack_error_class(&m)))?;
        slack::post_message(&url, text, blocks)
            .map(|_| ())
            .map_err(|m| SendError::Other(slack_error_class(&m)))
    };
    loop_notify::notify(loop_name, text, blocks, &lookup, &loader, &sender)
}

// ---- connector test results ---------------------------------------------------------

/// `<checkout root>/outputs/connectors/test-results.json`.
pub fn test_results_path() -> PathBuf {
    runner_util::repo_root()
        .join("outputs")
        .join("connectors")
        .join("test-results.json")
}

pub fn read_test_results(path: &Path) -> Map<String, Value> {
    match std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

static RESULTS_LOCK: Mutex<()> = Mutex::new(());

fn write_test_results(results: &Map<String, Value>, path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(
        "{name}.{}.tmp",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    let result = std::fs::write(
        &tmp,
        pyjson::to_string_pretty(&Value::Object(results.clone())),
    )
    .and_then(|_| std::fs::rename(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Atomically merge one `{ok, message, at}` entry into the test-results file.
/// Never holds a secret: `message` is the connector's own user-facing summary.
pub fn record_test_result(
    account_id: &str,
    ok: bool,
    message: &str,
    path: &Path,
) -> std::io::Result<()> {
    let _g = RESULTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut results = read_test_results(path);
    results.insert(
        account_id.to_string(),
        json!({"ok": ok, "message": message.chars().take(500).collect::<String>(), "at": now_stamp()}),
    );
    write_test_results(&results, path)
}

/// Keep per-id state in step with a successful delete (`new_id` `None`) or
/// rename: the last test result moves or goes, and every loop's `notify` list
/// drops or renames the id. Best effort.
pub fn account_renamed_or_deleted(old_id: &str, new_id: Option<&str>, path: &Path) {
    account_renamed_or_deleted_in(old_id, new_id, path, &loops_config::default_config_path());
}

/// [`account_renamed_or_deleted`] with an explicit `loops.json` (the dashboard's).
pub fn account_renamed_or_deleted_in(
    old_id: &str,
    new_id: Option<&str>,
    path: &Path,
    loops_path: &Path,
) {
    {
        let _g = RESULTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut results = read_test_results(path);
        if let Some(entry) = results.remove(old_id) {
            if let Some(new_id) = new_id.filter(|n| !n.is_empty()) {
                results.insert(new_id.to_string(), entry);
            }
            let _ = write_test_results(&results, path);
        }
    }
    let _ = loops_config::replace_notify_id(old_id, new_id, loops_path);
}

/// Run one account's probe and record its result (the Test button): returns
/// `(ok, message)`, never fails. An unknown id is not recorded.
pub fn run_connector_test(
    account_id: &str,
    store: &dyn SecretStore,
    paths: &Paths,
    http: Arc<dyn HttpClient>,
    results_path: &Path,
) -> (bool, String) {
    let (ok, message, record) = match load_connector(account_id, store, paths, http) {
        Ok(c) => {
            let (ok, m) = c.test();
            (ok, m, true)
        }
        Err(LoadFailure::Unknown) => (false, format!("No connector with id {account_id}"), false),
        Err(LoadFailure::Config(d)) => (false, format!("Could not read connectors: {d}"), true),
        Err(LoadFailure::Other(class)) => (false, format!("Request failed ({class})"), true),
    };
    if record {
        let _ = record_test_result(account_id, ok, &message, results_path);
    }
    (ok, message)
}

#[allow(unused_imports)]
use connectors as _connectors_mod;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MemoryStore;
    use std::collections::BTreeMap;

    fn store() -> MemoryStore {
        MemoryStore::new("loop-engineering.connectors")
    }

    struct Env {
        _d: tempfile::TempDir,
        paths: Paths,
    }

    fn env() -> Env {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        std::fs::write(
            p.join("gitlab.json"),
            json!({"instances": {"work": {"url": "https://gl.example", "token": "glpat"}}})
                .to_string(),
        )
        .unwrap();
        std::fs::write(
            p.join("slack.json"),
            json!({"webhook_url": "https://hooks.slack.com/x",
                   "bundle_webhooks": {"ops": "https://hooks.slack.com/ops"}})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            p.join("inboxes.json"),
            json!({"default_categories": [], "inboxes": [
                {"name": "me-gmail", "label": "Me", "provider": "gmail", "account": "a@b.co", "enabled": true}]})
            .to_string(),
        )
        .unwrap();
        let paths = Paths {
            config_path: Some(p.join("connectors.json")),
            gitlab_config_path: Some(p.join("gitlab.json")),
            slack_config_path: Some(p.join("slack.json")),
            inbox_config_path: Some(p.join("inboxes.json")),
        };
        Env { _d: d, paths }
    }

    fn gh(id: &str, label: &str) -> Value {
        json!({"id": id, "type": "github", "label": label, "api_url": "https://api.github.com", "username": "u"})
    }

    fn cal(id: &str, label: &str) -> Value {
        json!({"id": id, "type": "google_calendar", "label": label, "calendar_id": "primary"})
    }

    fn data(s: &MemoryStore, ids: &[&str]) -> BTreeMap<String, Option<String>> {
        ids.iter()
            .map(|i| (i.to_string(), s.get(i).unwrap()))
            .collect()
    }

    #[test]
    fn list_accounts_merges_external_and_leaks_no_secret() {
        let e = env();
        let accounts: BTreeMap<_, _> = list_accounts(&e.paths)
            .unwrap()
            .into_iter()
            .map(|a| (a.id.clone(), a))
            .collect();
        assert_eq!(
            (
                accounts["work"].kind.as_str(),
                accounts["work"].managed_by.as_str()
            ),
            ("gitlab", "gitlab-config")
        );
        assert_eq!(accounts["slack-default"].managed_by, "slack-config");
        assert_eq!(accounts["slack-ops"].kind, "slack");
        assert_eq!(accounts["slack-ops"].label, "Slack (ops)");
        assert_eq!(
            (
                accounts["me-gmail"].kind.as_str(),
                accounts["me-gmail"].managed_by.as_str()
            ),
            ("mailbox", "inboxes")
        );
        assert_eq!(
            Value::Object(accounts["me-gmail"].settings.clone()),
            json!({"provider": "gmail", "account": "a@b.co"})
        );
        let blob =
            serde_json::to_string(&accounts.values().map(Account::to_value).collect::<Vec<_>>())
                .unwrap();
        assert!(!blob.contains("glpat") && !blob.contains("hooks.slack.com"));
    }

    #[test]
    fn upsert_native_stores_the_secret_in_the_store_not_the_json() {
        let e = env();
        let s = store();
        assert!(upsert_account(&gh("gh", "GH"), Some("tok"), "", &e.paths, &s).0);
        assert_eq!(s.get("gh").unwrap().as_deref(), Some("tok"));
        assert!(!std::fs::read_to_string(e.paths.config())
            .unwrap()
            .contains("tok"));
        assert_eq!(
            get_account("gh", &e.paths).unwrap().unwrap().managed_by,
            "native"
        );
    }

    #[test]
    fn blank_secret_keeps_the_existing_one() {
        let e = env();
        let s = store();
        upsert_account(&gh("gh", "GH"), Some("tok"), "", &e.paths, &s);
        let (ok, _) = upsert_account(&gh("gh", "GH2"), Some(""), "gh", &e.paths, &s);
        assert!(ok);
        assert_eq!(s.get("gh").unwrap().as_deref(), Some("tok"));
        assert_eq!(get_account("gh", &e.paths).unwrap().unwrap().label, "GH2");
    }

    #[test]
    fn new_accounts_need_a_secret_and_a_valid_unique_id() {
        let e = env();
        let s = store();
        let (ok, msg) = upsert_account(&gh("gh", "x"), Some(""), "", &e.paths, &s);
        assert!(!ok && msg.contains("token"), "{msg}");
        let (ok, msg) = upsert_account(&gh("work", "x"), Some("t"), "", &e.paths, &s);
        assert!(!ok && msg.contains("already"));
        upsert_account(&gh("gh", "x"), Some("t"), "", &e.paths, &s);
        let (ok, msg) = upsert_account(&gh("gh", "x"), Some("t"), "", &e.paths, &s);
        assert!(!ok && msg.contains("already"));
        assert!(!upsert_account(&gh("../x", "x"), Some("t"), "", &e.paths, &s).0);
    }

    #[test]
    fn unknown_and_external_types_are_refused() {
        let e = env();
        let s = store();
        let mut v = gh("a", "x");
        v["type"] = "nope".into();
        assert!(!upsert_account(&v, Some("t"), "", &e.paths, &s).0);
        v["type"] = "mailbox".into();
        assert!(!upsert_account(&v, Some("t"), "", &e.paths, &s).0);
    }

    #[test]
    fn validation_errors_reach_the_message_and_nothing_is_written() {
        let e = env();
        let mut v = gh("a", "x");
        v["api_url"] = "ftp://x".into();
        let (ok, msg) = upsert_account(&v, Some("t"), "", &e.paths, &store());
        assert!(!ok && msg.contains("http"));
        assert!(!e.paths.config().exists());
    }

    #[test]
    fn non_string_values_are_refused_without_crashing() {
        let e = env();
        let v =
            json!({"id": "a", "type": "github", "label": 5, "api_url": null, "username": ["x"]});
        assert!(!upsert_account(&v, Some("t"), "", &e.paths, &store()).0);
        let mut v = gh("a", "x");
        v["id"] = true.into();
        assert!(!upsert_account(&v, Some("t"), "", &e.paths, &store()).0);
        let mut v = gh("a", "x");
        v["api_url"] = true.into();
        assert!(!upsert_account(&v, Some("t"), "", &e.paths, &store()).0);
        let mut v = gh("a", "x");
        v["enabled"] = false.into();
        assert!(upsert_account(&v, Some("t"), "", &e.paths, &store()).0);
        assert!(!get_account("a", &e.paths).unwrap().unwrap().enabled);
    }

    #[test]
    fn rename_moves_the_secret() {
        let e = env();
        let s = store();
        upsert_account(&gh("a", "x"), Some("t"), "", &e.paths, &s);
        assert!(upsert_account(&gh("b", "x"), Some(""), "a", &e.paths, &s).0);
        assert_eq!(
            data(&s, &["a", "b"]),
            BTreeMap::from([("a".into(), None), ("b".into(), Some("t".into()))])
        );
        let native: Vec<_> = list_accounts(&e.paths)
            .unwrap()
            .into_iter()
            .filter(|a| a.managed_by == "native")
            .map(|a| a.id)
            .collect();
        assert_eq!(native, ["b"]);
    }

    #[test]
    fn delete_removes_the_secret_then_the_entry() {
        let e = env();
        let s = store();
        upsert_account(&gh("gh", "x"), Some("t"), "", &e.paths, &s);
        let (ok, _) = delete_account("gh", Some(&e.paths.config()), &s);
        assert!(ok && s.get("gh").unwrap().is_none());
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(e.paths.config()).unwrap())
                .unwrap(),
            json!([])
        );
        assert!(!delete_account("work", Some(&e.paths.config()), &s).0);
    }

    #[test]
    fn capability_lookup_skips_disabled_and_unknown_types() {
        let e = env();
        let ids = |cap| {
            accounts_with_capability(cap, &e.paths)
                .unwrap()
                .into_iter()
                .map(|a| a.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("merge_requests"), ["work"]);
        upsert_account(&gh("gh", "x"), Some("t"), "", &e.paths, &store());
        assert_eq!(ids("merge_requests"), ["gh", "work"]);
        assert_eq!(ids("notify"), ["slack-default", "slack-ops"]);
        assert_eq!(ids("mail"), ["me-gmail"]);
    }

    #[test]
    fn unknown_id_is_none_and_malformed_json_is_a_config_error() {
        let e = env();
        assert!(get_account("nope", &e.paths).unwrap().is_none());
        std::fs::write(e.paths.config(), "{not json").unwrap();
        assert!(list_accounts(&e.paths).is_err());
        let (ok, msg) = upsert_account(&gh("gh", "x"), Some("t"), "", &e.paths, &store());
        assert!(!ok && msg.to_lowercase().contains("connectors"));
        let (ok, msg) = delete_account("gh", Some(&e.paths.config()), &store());
        assert!(!ok && msg.to_lowercase().contains("connectors"));
        std::fs::write(e.paths.config(), "{}").unwrap();
        assert!(list_accounts(&e.paths).is_err());
    }

    #[test]
    fn hand_edited_values_do_not_crash() {
        let e = env();
        std::fs::write(
            e.paths.config(),
            json!([{"id": "x", "type": "github", "label": 7, "enabled": "yes", "settings": {"api_url": 3}}, "junk"]).to_string(),
        )
        .unwrap();
        let native: Vec<_> = list_accounts(&e.paths)
            .unwrap()
            .into_iter()
            .filter(|a| a.managed_by == "native")
            .collect();
        assert_eq!(native.len(), 1);
        assert_eq!(
            (
                native[0].label.as_str(),
                native[0].enabled,
                native[0].setting("api_url")
            ),
            ("7", true, "3")
        );
    }

    #[test]
    fn missing_files_mean_no_accounts() {
        let d = tempfile::tempdir().unwrap();
        let p = |n: &str| Some(d.path().join(n));
        let paths = Paths {
            config_path: p("a"),
            gitlab_config_path: p("b"),
            slack_config_path: p("c"),
            inbox_config_path: p("d"),
        };
        assert!(list_accounts(&paths).unwrap().is_empty());
    }

    #[test]
    fn id_length_boundary() {
        let e = env();
        assert!(upsert_account(&gh(&"a".repeat(48), "x"), Some("t"), "", &e.paths, &store()).0);
        assert!(!upsert_account(&gh(&"a".repeat(49), "x"), Some("t"), "", &e.paths, &store()).0);
        assert!(is_valid_id("a-1") && !is_valid_id("-a") && !is_valid_id("A") && !is_valid_id(""));
    }

    #[test]
    fn secrets_with_control_characters_are_refused() {
        for secret in ["ghp_TOP\nSECRET", "a\rb", "a\0b", "a\tb", "a\x7fb"] {
            let e = env();
            let s = store();
            let (ok, msg) = upsert_account(&gh("gh", "x"), Some(secret), "", &e.paths, &s);
            assert!(!ok && msg.to_lowercase().contains("control") && !msg.contains(secret));
            assert!(s.get("gh").unwrap().is_none() && !e.paths.config().exists());
        }
    }

    #[test]
    fn changing_an_existing_accounts_type_is_refused() {
        let e = env();
        let s = store();
        upsert_account(&gh("gh", "x"), Some("github-token"), "", &e.paths, &s);
        let mut v = gh("gh", "x");
        v["type"] = "jira".into();
        let (ok, msg) = upsert_account(&v, Some(""), "gh", &e.paths, &s);
        assert!(!ok && msg.to_lowercase().contains("type"));
        assert_eq!(get_account("gh", &e.paths).unwrap().unwrap().kind, "github");
    }

    struct FailingStore(MemoryStore);
    impl SecretStore for FailingStore {
        fn service(&self) -> String {
            self.0.service()
        }
        fn get(&self, a: &str) -> Result<Option<String>, keychain::SecretError> {
            self.0.get(a)
        }
        fn set(&self, _a: &str, s: &str) -> Result<(), keychain::SecretError> {
            Err(keychain::SecretError::Failed(format!(
                "security: could not add {s}"
            )))
        }
        fn delete(&self, _a: &str) -> Result<(), keychain::SecretError> {
            Err(keychain::SecretError::Failed("locked".into()))
        }
        fn list(&self) -> Result<Vec<String>, keychain::SecretError> {
            self.0.list()
        }
    }

    #[test]
    fn store_failure_message_carries_no_exception_detail() {
        let e = env();
        let (ok, msg) = upsert_account(
            &gh("gh", "GH"),
            Some("SUPERSECRET"),
            "",
            &e.paths,
            &FailingStore(store()),
        );
        assert!(!ok && msg == "Could not store the secret in the Keychain");
        // a failing delete blocks the delete before the config is touched
        let s = store();
        upsert_account(&gh("gh", "GH"), Some("t"), "", &e.paths, &s);
        let (ok, _) = delete_account("gh", Some(&e.paths.config()), &FailingStore(s));
        assert!(
            !ok && std::fs::read_to_string(e.paths.config())
                .unwrap()
                .contains("\"gh\"")
        );
    }

    #[test]
    fn oauth_types_need_no_secret_and_refuse_a_pasted_one() {
        let e = env();
        let s = store();
        let (ok, msg) = upsert_account(&cal("gcal", "Cal"), None, "", &e.paths, &s);
        assert!(ok, "{msg}");
        assert_eq!(
            Value::Object(get_account("gcal", &e.paths).unwrap().unwrap().settings),
            json!({"calendar_id": "primary"})
        );
        let e2 = env();
        let (ok, msg) = upsert_account(
            &cal("gcal", "Cal"),
            Some("pasted-refresh-token"),
            "",
            &e2.paths,
            &s,
        );
        assert!(!ok && !msg.contains("pasted-refresh-token") && !e2.paths.config().exists());
    }

    #[test]
    fn oauth_secret_is_kept_and_renamed_with_the_account() {
        let e = env();
        let s = store();
        upsert_account(&cal("gcal", "Cal"), None, "", &e.paths, &s);
        s.set("gcal", "RT").unwrap();
        assert!(upsert_account(&cal("gcal2", "Renamed"), Some(""), "gcal", &e.paths, &s).0);
        assert_eq!(
            data(&s, &["gcal", "gcal2"]),
            BTreeMap::from([("gcal".into(), None), ("gcal2".into(), Some("RT".into()))])
        );
    }

    #[test]
    fn set_oauth_secret_stores_the_token_and_marks_connected() {
        let e = env();
        let s = store();
        upsert_account(&cal("gcal", "Cal"), None, "", &e.paths, &s);
        assert!(
            !get_account("gcal", &e.paths)
                .unwrap()
                .unwrap()
                .oauth_connected
        );
        let (ok, msg) = set_oauth_secret("gcal", Some("RT-SECRET"), &e.paths, &s);
        assert!(ok && !msg.contains("RT-SECRET"));
        assert_eq!(s.get("gcal").unwrap().as_deref(), Some("RT-SECRET"));
        assert!(
            get_account("gcal", &e.paths)
                .unwrap()
                .unwrap()
                .oauth_connected
        );
        assert!(!std::fs::read_to_string(e.paths.config())
            .unwrap()
            .contains("RT-SECRET"));
        upsert_account(&cal("gcal", "Edited"), Some(""), "gcal", &e.paths, &s);
        assert!(
            get_account("gcal", &e.paths)
                .unwrap()
                .unwrap()
                .oauth_connected
        );
    }

    #[test]
    fn set_oauth_secret_refuses_other_accounts_and_bad_tokens() {
        let e = env();
        let s = store();
        upsert_account(&gh("gh", "GH"), Some("tok"), "", &e.paths, &s);
        for id in ["gh", "nope", "work", "me-gmail"] {
            let (ok, msg) = set_oauth_secret(id, Some("RT-SECRET"), &e.paths, &s);
            assert!(!ok && !msg.contains("RT-SECRET"), "{id}");
        }
        upsert_account(&cal("gcal", "Cal"), None, "", &e.paths, &s);
        for bad in [Some(""), Some("a\nb"), None] {
            assert!(!set_oauth_secret("gcal", bad, &e.paths, &s).0);
        }
        assert!(s.get("gcal").unwrap().is_none());
        let (ok, msg) =
            set_oauth_secret("gcal", Some("RT-SECRET"), &e.paths, &FailingStore(store()));
        assert!(!ok && !msg.contains("RT-SECRET") && msg.contains("Keychain"));
    }

    #[test]
    fn a_failed_config_write_restores_the_keychain() {
        // The config path is a directory, so the final rename fails.
        let e = env();
        let s = store();
        upsert_account(&gh("gh", "x"), Some("old"), "", &e.paths, &s);
        let blocked = Paths {
            config_path: Some(e.paths.config()),
            ..e.paths.clone()
        };
        std::fs::remove_file(blocked.config()).unwrap();
        std::fs::create_dir(blocked.config()).unwrap();
        let (ok, _) = upsert_account(&gh("gh2", "x"), Some("new"), "", &blocked, &s);
        assert!(!ok);
        assert_eq!(
            s.get("gh2").unwrap(),
            None,
            "a new account leaves no secret behind"
        );
    }

    #[test]
    fn write_temp_names_are_unique() {
        let d = tempfile::tempdir().unwrap();
        write_entries(&[], &d.path().join("c.json")).unwrap();
        write_entries(&[], &d.path().join("c.json")).unwrap();
        let leftovers = std::fs::read_dir(d.path()).unwrap().count();
        assert_eq!(leftovers, 1, "temp files are cleaned up");
    }

    #[test]
    fn load_connector_native_and_external() {
        let e = env();
        let s = store();
        upsert_account(&gh("gh", "GH"), Some("tok"), "", &e.paths, &s);
        let http = || -> Arc<dyn HttpClient> { Arc::new(UreqClient::default()) };
        assert_eq!(
            load_connector("gh", &s, &e.paths, http())
                .unwrap()
                .info()
                .name,
            "github"
        );
        assert_eq!(
            load_connector("work", &s, &e.paths, http())
                .unwrap()
                .info()
                .name,
            "gitlab"
        );
        assert_eq!(
            load_connector("slack-default", &s, &e.paths, http())
                .unwrap()
                .info()
                .name,
            "slack"
        );
        assert_eq!(
            load_connector("slack-ops", &s, &e.paths, http())
                .unwrap()
                .info()
                .name,
            "slack"
        );
        assert_eq!(
            load_connector("me-gmail", &s, &e.paths, http())
                .unwrap()
                .info()
                .name,
            "mailbox"
        );
        assert!(matches!(
            load_connector("ghost", &s, &e.paths, http()),
            Err(LoadFailure::Unknown)
        ));
    }

    #[test]
    fn external_secrets_reach_the_connector() {
        use crate::connectors::testutil::FakeHttp;
        let e = env();
        let s = store();
        let http = FakeHttp::ok(json!({"username": "me"}));
        let c = load_connector("work", &s, &e.paths, http.clone()).unwrap();
        assert_eq!(c.test(), (true, "Connected as me".to_string()));
        assert!(http.calls.lock().unwrap()[0]
            .headers
            .contains(&("PRIVATE-TOKEN".into(), "glpat".into())));
        let c = load_connector("slack-ops", &s, &e.paths, http.clone()).unwrap();
        c.send("x", None).unwrap();
        assert_eq!(
            http.webhooks.lock().unwrap()[0].0,
            "https://hooks.slack.com/ops"
        );
        // a native secret comes from the store
        upsert_account(&gh("gh", "GH"), Some("tok"), "", &e.paths, &s);
        let c = load_connector("gh", &s, &e.paths, http.clone()).unwrap();
        c.test();
        assert_eq!(
            http.calls.lock().unwrap().last().unwrap().token.as_deref(),
            Some("tok")
        );
    }

    #[test]
    fn broken_config_fails_the_load_with_a_config_error() {
        let e = env();
        std::fs::write(e.paths.config(), "{not json").unwrap();
        assert!(matches!(
            load_connector("gh", &store(), &e.paths, Arc::new(UreqClient::default())),
            Err(LoadFailure::Config(_))
        ));
    }

    #[test]
    fn test_results_are_merged_moved_and_capped() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("out").join("test-results.json");
        record_test_result("gh", true, "Connected as u", &p).unwrap();
        record_test_result("other", false, &"x".repeat(600), &p).unwrap();
        let r = read_test_results(&p);
        assert_eq!(r["gh"]["ok"], true);
        assert_eq!(r["gh"]["message"], "Connected as u");
        assert!(r["gh"]["at"].as_str().unwrap().ends_with('Z'));
        assert_eq!(r["other"]["message"].as_str().unwrap().len(), 500);
    }

    #[test]
    fn run_connector_test_records_unless_the_id_is_unknown() {
        use crate::connectors::testutil::FakeHttp;
        let e = env();
        let d = tempfile::tempdir().unwrap();
        let results = d.path().join("results.json");
        let http = FakeHttp::ok(json!({"username": "me"}));
        let (ok, msg) = run_connector_test("work", &store(), &e.paths, http.clone(), &results);
        assert_eq!((ok, msg.as_str()), (true, "Connected as me"));
        assert_eq!(read_test_results(&results)["work"]["ok"], true);
        let (ok, msg) = run_connector_test("ghost", &store(), &e.paths, http, &results);
        assert!(!ok && msg == "No connector with id ghost");
        assert!(!read_test_results(&results).contains_key("ghost"));
    }
}
