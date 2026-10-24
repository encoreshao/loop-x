//! The Inbox Triage loop's per-machine config: `~/.loopx/inboxes.json`
//! (which mailboxes to triage, and how) and `mail_oauth.json` (the one-time OAuth
//! app credentials). Mirrors `bin/inbox_config.py`. Refresh tokens are NOT here:
//! they live in the Keychain (see [`crate::mail_auth`]).

use crate::config::home_dir;
use crate::loops_config::py_truthy;
use crate::pyjson;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const PROVIDERS: [&str; 2] = ["gmail", "outlook"];
pub const OAUTH_PROVIDERS: [&str; 2] = ["google", "microsoft"];
pub const LABEL_PREFIX: &str = "Loop/";

pub fn default_config_path() -> PathBuf {
    home_dir().join("inboxes.json")
}

pub fn default_oauth_path() -> PathBuf {
    home_dir().join("mail_oauth.json")
}

/// The shipped category list (a fresh copy: callers may mutate it).
pub fn default_categories() -> Value {
    json!([
        {"key": "urgent", "label": "Loop/Urgent", "draft": true,
         "description": "Needs a response today: a direct ask from a real person with a deadline, a blocker, or a client/boss escalation."},
        {"key": "action", "label": "Loop/Action", "draft": false,
         "description": "Needs a reply or task from the user, but not today."},
        {"key": "fyi", "label": "Loop/FYI", "draft": false,
         "description": "Worth reading, no action: updates, CCs, announcements."},
        {"key": "notifications", "label": "Loop/Notifications", "draft": false,
         "description": "Automated mail: GitLab, CI, calendar, SaaS alerts."},
        {"key": "newsletters", "label": "Loop/Newsletters", "draft": false,
         "description": "Marketing, digests, subscriptions."},
    ])
}

#[derive(Debug, thiserror::Error)]
pub enum InboxConfigError {
    /// Python's `FileNotFoundError`.
    #[error("No inbox config at {0}. Copy config/inboxes.json.template there, or add an inbox from the dashboard's Inbox Triage setup page.")]
    NotFound(String),
    /// Python's `ValueError`.
    #[error("{0}")]
    Invalid(String),
    #[error("cannot read {0}: {1}")]
    Io(String, String),
}

// ---------------------------------------------------------------- addresses

/// `email.utils.parseaddr(value)[1].strip().lower()`: the address of the first
/// mailbox in a header value (`Name <a@b>`, `"Last, First" <a@b>`, `a@b (Name)`).
pub fn parse_address(header: &str) -> String {
    let cleaned = strip_comments(header);
    let mut in_quote = false;
    let mut escaped = false;
    let mut open = None;
    for (i, c) in cleaned.char_indices() {
        if escaped {
            escaped = false;
        } else if in_quote && c == '\\' {
            escaped = true;
        } else if c == '"' {
            in_quote = !in_quote;
        } else if c == '<' && !in_quote {
            open = Some(i);
            break;
        }
    }
    let addr = match open {
        Some(i) => {
            let rest = &cleaned[i + 1..];
            let inner = rest.split('>').next().unwrap_or("");
            // An obsolete source route: `<@a,@b:user@host>`.
            let inner = match (inner.starts_with('@'), inner.split_once(':')) {
                (true, Some((_, tail))) => tail,
                _ => inner,
            };
            inner.split_whitespace().collect::<String>()
        }
        None => {
            let first = cleaned.split(',').next().unwrap_or("");
            let tokens: Vec<&str> = first.split_whitespace().collect();
            tokens
                .iter()
                .find(|t| t.contains('@'))
                .or(tokens.first())
                .map(|t| t.trim_matches('"').to_string())
                .unwrap_or_default()
        }
    };
    addr.trim().to_lowercase()
}

/// Drop RFC 5322 `( comments )` (nestable) that sit outside quoted strings.
fn strip_comments(s: &str) -> String {
    let (mut out, mut depth, mut in_quote, mut escaped) = (String::new(), 0usize, false, false);
    for c in s.chars() {
        if escaped {
            escaped = false;
            if depth == 0 {
                out.push(c);
            }
            continue;
        }
        match c {
            '\\' if in_quote || depth > 0 => {
                escaped = true;
                if depth == 0 {
                    out.push(c);
                }
            }
            '"' if depth == 0 => {
                in_quote = !in_quote;
                out.push(c);
            }
            '(' if !in_quote => depth += 1,
            ')' if !in_quote && depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

pub fn sender_matches(address: &str, patterns: &[String]) -> bool {
    let address = address.trim().to_lowercase();
    if address.is_empty() {
        return false;
    }
    for pattern in patterns {
        let pattern = pattern.trim().to_lowercase();
        if pattern.is_empty() {
            continue;
        }
        if pattern.starts_with('@') {
            if address.ends_with(&pattern) {
                return true;
            }
        } else if address == pattern {
            return true;
        }
    }
    false
}

/// `inbox["vip_senders"]` / `["exclude_senders"]` as plain strings.
pub fn string_list(inbox: &Value, key: &str) -> Vec<String> {
    inbox
        .get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

// --------------------------------------------------------------- validation

fn is_name_slug(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_email(s: &str) -> bool {
    if s.chars().any(char::is_whitespace) {
        return false;
    }
    let mut parts = s.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let chars: Vec<char> = domain.chars().collect();
    !local.is_empty() && (1..chars.len().saturating_sub(1)).any(|i| chars[i] == '.')
}

/// Errors for a category list (an inbox's own, or `default_categories`). Every
/// label must start with `Loop/`: "only Loop/* labels are ever applied" is a
/// safety promise, since a hand-edited `TRASH`/`SPAM`/`UNREAD` would otherwise
/// be applied to real mail.
pub fn validate_categories(categories: &Value) -> Vec<String> {
    let Some(list) = categories.as_array() else {
        return vec!["must be a list".into()];
    };
    let mut errors = Vec::new();
    let mut keys: Vec<&str> = Vec::new();
    for (index, category) in list.iter().enumerate() {
        let wher = format!("category #{}", index + 1);
        let Some(obj) = category.as_object() else {
            errors.push(format!(
                "{wher} must be an object with key/label/description/draft"
            ));
            continue;
        };
        match obj.get("key").and_then(Value::as_str) {
            Some(k) if !k.trim().is_empty() => {
                if keys.contains(&k) {
                    errors.push(format!(
                        "{wher}: duplicate key {}",
                        crate::loops_config::py_repr_str(k)
                    ));
                } else {
                    keys.push(k);
                }
            }
            _ => errors.push(format!("{wher} needs a non-empty key")),
        }
        let label_ok = obj.get("label").and_then(Value::as_str).is_some_and(|l| {
            l.starts_with(LABEL_PREFIX) && !l[LABEL_PREFIX.len()..].trim().is_empty()
        });
        if !label_ok {
            errors.push(format!(
                "{wher}: label must start with 'Loop/' (e.g. 'Loop/Urgent')"
            ));
        }
        if !obj.get("description").is_some_and(Value::is_string) {
            errors.push(format!("{wher} needs a description string"));
        }
        if !obj.get("draft").is_some_and(Value::is_boolean) {
            errors.push(format!("{wher} needs draft: true or false"));
        }
    }
    if !keys.contains(&"urgent") {
        errors.push(
            "must include an 'urgent' category - VIP senders are always marked urgent".into(),
        );
    }
    errors
}

pub fn validate_inbox(inbox: &Value) -> Vec<String> {
    let Some(obj) = inbox.as_object() else {
        return vec!["inbox must be an object".into()];
    };
    let mut errors = Vec::new();
    if !obj
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(is_name_slug)
    {
        errors.push("name must be a lowercase slug (a-z, 0-9, -)".into());
    }
    if !obj
        .get("provider")
        .and_then(Value::as_str)
        .is_some_and(|p| PROVIDERS.contains(&p))
    {
        errors.push(format!("provider must be one of {}", PROVIDERS.join(", ")));
    }
    if !obj
        .get("account")
        .and_then(Value::as_str)
        .is_some_and(is_email)
    {
        errors.push("account must be an email address".into());
    }
    if obj
        .get("label")
        .and_then(Value::as_str)
        .is_none_or(|l| l.trim().is_empty())
    {
        errors.push("label is required".into());
    }
    for key in ["vip_senders", "exclude_senders"] {
        if obj.get(key).is_some_and(|v| !v.is_array()) {
            errors.push(format!("{key} must be a list"));
        }
    }
    match obj.get("categories") {
        None | Some(Value::Null) => {}
        Some(c) if !c.is_array() => errors.push("categories must be null or a list".into()),
        Some(c) => errors.extend(
            validate_categories(c)
                .into_iter()
                .map(|e| format!("categories {e}")),
        ),
    }
    errors
}

fn validate_config(mut data: Value, path: &Path) -> Result<Value, InboxConfigError> {
    let bad = |m: String| Err(InboxConfigError::Invalid(m));
    let Some(obj) = data.as_object_mut() else {
        return bad(format!(
            "{} must be a JSON object with an 'inboxes' array",
            path.display()
        ));
    };
    if !obj.get("inboxes").is_some_and(Value::is_array) {
        return bad(format!(
            "{} must be a JSON object with an 'inboxes' array",
            path.display()
        ));
    }
    obj.entry("default_categories")
        .or_insert_with(default_categories);
    let errors = validate_categories(&obj["default_categories"]);
    if !errors.is_empty() {
        return bad(format!(
            "{}: default_categories {}",
            path.display(),
            errors.join("; ")
        ));
    }
    let mut seen: Vec<String> = Vec::new();
    for inbox in obj["inboxes"].as_array().into_iter().flatten() {
        let errors = validate_inbox(inbox);
        let name = inbox.get("name").cloned().unwrap_or(Value::Null);
        if !errors.is_empty() {
            return bad(format!(
                "{}: inbox {}: {}",
                path.display(),
                crate::loops_config::py_repr(&name),
                errors.join("; ")
            ));
        }
        let n = name.as_str().unwrap_or_default().to_string();
        if seen.contains(&n) {
            return bad(format!(
                "{}: duplicate inbox name {}",
                path.display(),
                crate::loops_config::py_repr(&name)
            ));
        }
        seen.push(n);
    }
    Ok(data)
}

// ------------------------------------------------------------------- load

pub fn load_config(path: &Path) -> Result<Value, InboxConfigError> {
    if !path.exists() {
        return Err(InboxConfigError::NotFound(path.display().to_string()));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| InboxConfigError::Io(path.display().to_string(), e.to_string()))?;
    let data: Value = serde_json::from_str(&text)
        .map_err(|e| InboxConfigError::Invalid(format!("{}: invalid JSON: {e}", path.display())))?;
    validate_config(data, path)
}

pub fn load_config_or_empty(path: &Path) -> Result<Value, InboxConfigError> {
    match load_config(path) {
        Err(InboxConfigError::NotFound(_)) => {
            Ok(json!({"default_categories": default_categories(), "inboxes": []}))
        }
        other => other,
    }
}

/// `inbox["categories"]`, else the config's defaults, else the shipped ones.
pub fn categories_for(inbox: &Value, config: &Value) -> Vec<Value> {
    let truthy_list = |v: Option<&Value>| {
        v.and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .cloned()
    };
    truthy_list(inbox.get("categories"))
        .or_else(|| truthy_list(config.get("default_categories")))
        .unwrap_or_else(|| default_categories().as_array().cloned().unwrap_or_default())
}

pub fn get_inbox(name: &str, path: &Path) -> Result<Value, InboxConfigError> {
    load_config(path)?["inboxes"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|i| i["name"] == name)
        .cloned()
        .ok_or_else(|| {
            InboxConfigError::Invalid(format!(
                "No inbox named {}",
                crate::loops_config::py_repr_str(name)
            ))
        })
}

pub fn enabled_inboxes(path: &Path) -> Result<Vec<Value>, InboxConfigError> {
    Ok(load_config(path)?["inboxes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i.get("enabled").is_none_or(py_truthy))
        .cloned()
        .collect())
}

// ------------------------------------------------------------------ writes

/// Atomic write (temp file + rename), `json.dump(..., indent=2)`.
fn write_json(data: &Value, path: &Path, mode: Option<u32>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, pyjson::to_string_pretty(data))?;
    #[cfg(unix)]
    if let Some(m) = mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(m))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    std::fs::rename(&tmp, path)
}

fn stripped(fields: &Value, key: &str) -> String {
    fields
        .get(key)
        .filter(|v| py_truthy(v))
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Create (`is_new`) or update an inbox: `(ok, message)` like the Python.
pub fn upsert_inbox(fields: &Value, is_new: bool, path: &Path) -> (bool, String) {
    let list = |k: &str| -> Value {
        match fields.get(k) {
            Some(Value::Array(a)) => Value::Array(a.clone()),
            _ => json!([]),
        }
    };
    let bundle = stripped(fields, "slack_bundle");
    let mut inbox = json!({
        "name": stripped(fields, "name"),
        "label": stripped(fields, "label"),
        "provider": stripped(fields, "provider"),
        "account": stripped(fields, "account"),
        "enabled": fields.get("enabled").is_none_or(py_truthy),
        "urgent_brief": stripped(fields, "urgent_brief"),
        "vip_senders": list("vip_senders"),
        "exclude_senders": list("exclude_senders"),
        "categories": fields.get("categories").cloned().unwrap_or(Value::Null),
        "slack_bundle": if bundle.is_empty() { Value::Null } else { Value::String(bundle) },
    });
    let errors = validate_inbox(&inbox);
    if !errors.is_empty() {
        return (false, errors.join("; "));
    }
    let mut config = match load_config_or_empty(path) {
        Ok(c) => c,
        Err(e) => return (false, e.to_string()),
    };
    let name = inbox["name"].as_str().unwrap_or_default().to_string();
    let inboxes = config["inboxes"].as_array_mut().expect("validated");
    let pos = inboxes.iter().position(|i| i["name"] == name.as_str());
    if is_new {
        if pos.is_some() {
            return (
                false,
                format!(
                    "An inbox named {} already exists",
                    crate::loops_config::py_repr_str(&name)
                ),
            );
        }
        inboxes.push(inbox.clone());
    } else {
        let Some(pos) = pos else {
            return (
                false,
                "Inbox names cannot be renamed after creation - delete and re-add instead".into(),
            );
        };
        let existing = inboxes[pos].clone();
        inbox["enabled"] = existing
            .get("enabled")
            .cloned()
            .unwrap_or(Value::Bool(true));
        // A form save that doesn't carry categories at all must not wipe
        // hand-configured ones; an explicit key (list or null) still wins.
        if fields.get("categories").is_none() {
            inbox["categories"] = existing.get("categories").cloned().unwrap_or(Value::Null);
        }
        inboxes[pos] = inbox.clone();
    }
    if let Err(e) = write_json(&config, path, None) {
        return (false, format!("cannot write {}: {e}", path.display()));
    }
    (
        true,
        format!(
            "Saved inbox {}",
            inbox["label"].as_str().unwrap_or_default()
        ),
    )
}

pub fn delete_inbox(name: &str, path: &Path) -> (bool, String) {
    let mut config = match load_config_or_empty(path) {
        Ok(c) => c,
        Err(e) => return (false, e.to_string()),
    };
    let inboxes = config["inboxes"].as_array_mut().expect("validated");
    let before = inboxes.len();
    inboxes.retain(|i| i["name"] != name);
    if inboxes.len() == before {
        return (
            false,
            format!("No inbox named {}", crate::loops_config::py_repr_str(name)),
        );
    }
    if let Err(e) = write_json(&config, path, None) {
        return (false, format!("cannot write {}: {e}", path.display()));
    }
    (true, format!("Deleted inbox {name}"))
}

pub fn set_enabled(name: &str, enabled: bool, path: &Path) -> (bool, String) {
    let mut config = match load_config_or_empty(path) {
        Ok(c) => c,
        Err(e) => return (false, e.to_string()),
    };
    let inboxes = config["inboxes"].as_array_mut().expect("validated");
    let Some(inbox) = inboxes.iter_mut().find(|i| i["name"] == name) else {
        return (
            false,
            format!("No inbox named {}", crate::loops_config::py_repr_str(name)),
        );
    };
    inbox["enabled"] = Value::Bool(enabled);
    let label = inbox["label"].as_str().unwrap_or_default().to_string();
    if let Err(e) = write_json(&config, path, None) {
        return (false, format!("cannot write {}: {e}", path.display()));
    }
    (
        true,
        format!("{label} {}", if enabled { "resumed" } else { "paused" }),
    )
}

pub fn load_oauth(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

pub fn save_oauth_client(
    provider: &str,
    client_id: &str,
    client_secret: &str,
    path: &Path,
) -> (bool, String) {
    if !OAUTH_PROVIDERS.contains(&provider) {
        return (
            false,
            format!(
                "Unknown provider {}",
                crate::loops_config::py_repr_str(provider)
            ),
        );
    }
    let (client_id, client_secret) = (client_id.trim(), client_secret.trim());
    if client_id.is_empty() {
        return (false, "Client ID is required".into());
    }
    if provider == "google" && client_secret.is_empty() {
        return (false, "Google also needs its client secret".into());
    }
    let mut data = load_oauth(path);
    let mut entry = Map::new();
    entry.insert("client_id".into(), json!(client_id));
    if provider == "google" {
        entry.insert("client_secret".into(), json!(client_secret));
    }
    data.insert(provider.into(), Value::Object(entry));
    if let Err(e) = write_json(&Value::Object(data), path, Some(0o600)) {
        return (false, format!("cannot write {}: {e}", path.display()));
    }
    let mut title = provider.to_string();
    title[..1].make_ascii_uppercase();
    (true, format!("Saved {title} OAuth client"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn inbox(over: Value) -> Value {
        let mut base = json!({
            "name": "work-gmail", "label": "Work Gmail", "provider": "gmail",
            "account": "me@example.com", "enabled": true, "urgent_brief": "",
            "vip_senders": [], "exclude_senders": [], "categories": null, "slack_bundle": null,
        });
        for (k, v) in over.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    fn write(path: &Path, inboxes: Value, defaults: Option<Value>) {
        std::fs::write(
            path,
            json!({"default_categories": defaults.unwrap_or_else(default_categories), "inboxes": inboxes})
                .to_string(),
        )
        .unwrap();
    }

    fn has(errors: &[String], needle: &str) -> bool {
        errors.iter().any(|e| e.contains(needle))
    }

    #[test]
    fn missing_file_is_not_found_and_names_the_template() {
        let d = tempdir().unwrap();
        let e = load_config(&d.path().join("inboxes.json")).unwrap_err();
        assert!(matches!(e, InboxConfigError::NotFound(_)));
        assert!(e.to_string().contains("inboxes.json.template"));
    }

    #[test]
    fn or_empty_returns_the_defaults_when_missing() {
        let d = tempdir().unwrap();
        let c = load_config_or_empty(&d.path().join("inboxes.json")).unwrap();
        assert_eq!(
            c,
            json!({"default_categories": default_categories(), "inboxes": []})
        );
    }

    #[test]
    fn invalid_inbox_fields_are_rejected() {
        for (over, fragment) in [
            (json!({"name": "Bad Name"}), "name"),
            (json!({"name": ""}), "name"),
            (json!({"provider": "yahoo"}), "provider"),
            (json!({"account": "not-an-email"}), "account"),
            (json!({"vip_senders": "a@b.com"}), "vip_senders"),
        ] {
            assert!(has(&validate_inbox(&inbox(over)), fragment), "{fragment}");
        }
    }

    #[test]
    fn custom_categories_need_urgent() {
        let custom =
            json!([{"key": "fyi", "label": "Loop/FYI", "description": "x", "draft": false}]);
        assert!(has(
            &validate_inbox(&inbox(json!({"categories": custom}))),
            "urgent"
        ));
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let d = tempdir().unwrap();
        let p = d.path().join("inboxes.json");
        write(&p, json!([inbox(json!({})), inbox(json!({}))]), None);
        assert!(load_config(&p)
            .unwrap_err()
            .to_string()
            .contains("duplicate"));
    }

    #[test]
    fn categories_for_defaults_override_and_copy() {
        let config = json!({"default_categories": default_categories(), "inboxes": []});
        assert_eq!(
            categories_for(&inbox(json!({})), &config),
            default_categories().as_array().unwrap().clone()
        );
        let custom =
            json!([{"key": "urgent", "label": "Loop/Urgent", "description": "x", "draft": true}]);
        assert_eq!(
            categories_for(&inbox(json!({"categories": custom.clone()})), &config),
            custom.as_array().unwrap().clone()
        );
        // Falls back to the shipped list when the config has none.
        assert_eq!(categories_for(&inbox(json!({})), &json!({})).len(), 5);
    }

    #[test]
    fn enabled_inboxes_skip_disabled() {
        let d = tempdir().unwrap();
        let p = d.path().join("inboxes.json");
        write(
            &p,
            json!([
                inbox(json!({})),
                inbox(json!({"name": "home", "enabled": false}))
            ]),
            None,
        );
        let names: Vec<_> = enabled_inboxes(&p)
            .unwrap()
            .iter()
            .map(|i| i["name"].clone())
            .collect();
        assert_eq!(names, vec![json!("work-gmail")]);
    }

    #[test]
    fn upsert_creates_then_updates_preserving_enabled() {
        let d = tempdir().unwrap();
        let p = d.path().join("inboxes.json");
        assert!(upsert_inbox(&inbox(json!({})), true, &p).0);
        set_enabled("work-gmail", false, &p);
        assert!(
            upsert_inbox(
                &inbox(json!({"label": "Renamed label", "enabled": true})),
                false,
                &p
            )
            .0
        );
        let saved = get_inbox("work-gmail", &p).unwrap();
        assert_eq!(saved["label"], "Renamed label");
        assert_eq!(saved["enabled"], false);
    }

    #[test]
    fn upsert_new_with_existing_name_and_rename_are_rejected() {
        let d = tempdir().unwrap();
        let p = d.path().join("inboxes.json");
        upsert_inbox(&inbox(json!({})), true, &p);
        let (ok, m) = upsert_inbox(&inbox(json!({})), true, &p);
        assert!(!ok && m.contains("already exists"));
        let (ok, m) = upsert_inbox(&inbox(json!({"name": "renamed"})), false, &p);
        assert!(!ok && m.contains("cannot be renamed"));
        assert_eq!(
            load_config(&p).unwrap()["inboxes"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn delete_inbox_removes_then_reports_missing() {
        let d = tempdir().unwrap();
        let p = d.path().join("inboxes.json");
        upsert_inbox(&inbox(json!({})), true, &p);
        assert!(delete_inbox("work-gmail", &p).0);
        assert_eq!(load_config(&p).unwrap()["inboxes"], json!([]));
        assert!(!delete_inbox("work-gmail", &p).0);
    }

    #[test]
    fn upsert_edit_without_categories_keeps_existing_and_explicit_replaces() {
        let d = tempdir().unwrap();
        let p = d.path().join("inboxes.json");
        let custom = json!([{"key": "urgent", "label": "Loop/Urgent", "description": "Needs me today", "draft": true},
                            {"key": "fyi", "label": "Loop/FYI", "description": "Everything else", "draft": false}]);
        upsert_inbox(&inbox(json!({"categories": custom.clone()})), true, &p);
        let mut fields = inbox(json!({"label": "Edited"}));
        fields.as_object_mut().unwrap().remove("categories");
        assert!(upsert_inbox(&fields, false, &p).0);
        let saved = get_inbox("work-gmail", &p).unwrap();
        assert_eq!(
            (&saved["label"], &saved["categories"]),
            (&json!("Edited"), &custom)
        );
        assert!(upsert_inbox(&inbox(json!({"categories": null})), false, &p).0);
        assert_eq!(
            get_inbox("work-gmail", &p).unwrap()["categories"],
            Value::Null
        );
        let one = json!([custom[0].clone()]);
        assert!(upsert_inbox(&inbox(json!({"categories": one.clone()})), false, &p).0);
        assert_eq!(get_inbox("work-gmail", &p).unwrap()["categories"], one);
    }

    #[test]
    fn oauth_client_is_0600_and_merges() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempdir().unwrap();
        let p = d.path().join("mail_oauth.json");
        assert!(save_oauth_client("google", "gid", "gsecret", &p).0);
        assert_eq!(
            save_oauth_client("microsoft", "mid", "", &p),
            (true, "Saved Microsoft OAuth client".into())
        );
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            Value::Object(load_oauth(&p)),
            json!({"google": {"client_id": "gid", "client_secret": "gsecret"}, "microsoft": {"client_id": "mid"}})
        );
    }

    #[test]
    fn oauth_client_rejects_blank_or_unknown() {
        let d = tempdir().unwrap();
        let p = d.path().join("mail_oauth.json");
        assert!(!save_oauth_client("google", "", "s", &p).0);
        assert!(!save_oauth_client("google", "id", "", &p).0);
        assert!(!save_oauth_client("yahoo", "id", "", &p).0);
        assert!(load_oauth(&d.path().join("nope.json")).is_empty());
    }

    #[test]
    fn parse_address_cases() {
        for (header, expected) in [
            ("Alice <Alice@Example.com>", "alice@example.com"),
            ("bob@example.com", "bob@example.com"),
            ("\"Doe, Jane\" <jane@x.org>", "jane@x.org"),
            ("", ""),
            ("a@x.com (Alice), b@y.com", "a@x.com"),
            ("\"<weird>\" <real@x.org>", "real@x.org"),
            ("  Padded <P@X.COM> ", "p@x.com"),
        ] {
            assert_eq!(parse_address(header), expected, "{header}");
        }
    }

    #[test]
    fn sender_matching_exact_and_domain() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(sender_matches("A@Client.com", &s(&["a@client.com"])));
        assert!(sender_matches("x@client.com", &s(&["@client.com"])));
        assert!(!sender_matches("x@notclient.com", &s(&["@client.com"])));
        assert!(!sender_matches("", &s(&["@client.com"])));
        assert!(!sender_matches("x@client.com", &s(&["", "  "])));
    }

    #[test]
    fn malformed_custom_categories_are_rejected() {
        let good =
            json!({"key": "urgent", "label": "Loop/Urgent", "description": "today", "draft": true});
        let with = |k: &str, v: Value| {
            let mut g = good.clone();
            g[k] = v;
            g
        };
        let without = |k: &str| {
            let mut g = good.clone();
            g.as_object_mut().unwrap().remove(k);
            g
        };
        for (bad, needle) in [
            (with("label", json!("TRASH")), "Loop/"),
            (with("label", json!("SPAM")), "Loop/"),
            (with("label", json!("Loop/")), "Loop/"),
            (with("label", json!(5)), "Loop/"),
            (without("description"), "description"),
            (without("draft"), "draft"),
            (with("draft", json!("yes")), "draft"),
            (with("key", json!("")), "key"),
            (json!("urgent"), "object"),
        ] {
            let errors = validate_inbox(&inbox(json!({"categories": [bad.clone()]})));
            assert!(has(&errors, needle), "{bad}: {errors:?}");
        }
        let dup = json!([good.clone(), with("label", json!("Loop/Other"))]);
        assert!(has(
            &validate_inbox(&inbox(json!({"categories": dup}))),
            "duplicate"
        ));
        let ok = json!([good, {"key": "fyi", "label": "Loop/FYI", "description": "Everything else", "draft": false}]);
        assert!(validate_inbox(&inbox(json!({"categories": ok}))).is_empty());
    }

    #[test]
    fn default_categories_are_validated_on_load() {
        let d = tempdir().unwrap();
        let p = d.path().join("inboxes.json");
        let good =
            json!({"key": "urgent", "label": "Loop/Urgent", "description": "today", "draft": true});
        for (defaults, needle) in [
            (json!("not a list"), "default_categories"),
            (
                json!([{"key": "fyi", "label": "Loop/FYI", "description": "x", "draft": false}]),
                "urgent",
            ),
            (
                json!([{"key": "urgent", "label": "UNREAD", "description": "x", "draft": true}]),
                "Loop/",
            ),
            (
                json!([{"key": "urgent", "label": "Loop/Urgent"}]),
                "description",
            ),
        ] {
            std::fs::write(
                &p,
                json!({"default_categories": defaults, "inboxes": []}).to_string(),
            )
            .unwrap();
            let e = load_config(&p).unwrap_err().to_string();
            assert!(
                e.contains("default_categories") && e.contains(needle),
                "{e}"
            );
        }
        let _ = good;
    }

    #[test]
    fn shipped_defaults_and_template_pass_validation() {
        assert!(validate_categories(&default_categories()).is_empty());
        let template =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/inboxes.json.template");
        let d = tempdir().unwrap();
        let p = d.path().join("inboxes.json");
        std::fs::copy(template, &p).unwrap();
        let c = load_config(&p).unwrap();
        assert!(c["inboxes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["enabled"] == false));
    }

    #[test]
    fn written_file_is_python_pretty_json() {
        let d = tempdir().unwrap();
        let p = d.path().join("inboxes.json");
        upsert_inbox(&inbox(json!({"urgent_brief": "é"})), true, &p);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("\"urgent_brief\": \"\\u00e9\""));
        assert!(!text.ends_with('\n'));
    }

    #[test]
    fn default_paths_follow_loop_engineering_home() {
        // Resolved at call time from the (process-wide) home; only the file name is checked.
        assert!(default_config_path().ends_with("inboxes.json"));
        assert!(default_oauth_path().ends_with("mail_oauth.json"));
    }
}
