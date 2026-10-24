//! The file-backed settings the dashboard's Settings pages edit (mirrors the Python
//! `update_slack_webhook`, `*_block_template`, `write_custom_instructions` and
//! `chat_tools/settings.py`): atomic writes, unknown keys preserved, and
//! `keychain:` references kept in place of plaintext secrets.

use crate::keychain;
use crate::msg::{fail, ok, Msg, Outcome};
use crate::pyjson;
use serde_json::{json, Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// `notification_key` choices of a Block Kit template and their English labels.
pub const BLOCK_TEMPLATE_NOTIFICATION_KEYS: [(&str, &str); 4] = [
    (
        "gitlab_wrapup_failed",
        "GitLab loop: end-of-run digest failed",
    ),
    ("gitlab_issues_incomplete", "GitLab loop: issues incomplete"),
    (
        "topic_monitor_incomplete",
        "Topic monitor: topics incomplete",
    ),
    ("inbox_triage_digest", "Inbox Triage: run digest"),
];

/// The chat write tools (`bin/chat_tools/registry.py`, mode WRITE), name and description.
pub const CHAT_WRITE_TOOLS: [(&str, &str); 4] = [
    (
        "calendar.create_event",
        "Create a Google Calendar event (start/end: ISO date-time WITH a UTC offset; attendees: comma-separated emails who will be invited; needs the user's approval)",
    ),
    (
        "github.comment",
        "Post a comment on a GitHub issue or pull request (needs the user's approval)",
    ),
    (
        "gitlab.comment",
        "Post a comment on a GitLab issue (needs the user's approval)",
    ),
    (
        "mail.send",
        "Send a plain-text email from the user's Gmail mailbox (to/cc: comma-separated plain addresses, at most 10 in total; needs the user's approval)",
    ),
];

// ----------------------------------------------------------------------- files

/// Write `bytes` to `path` through a same-directory temp file (mode `mode`), then
/// rename over the target.
pub fn atomic_write(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(
        ".{name}.{}.tmp",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    let result = (|| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(m) = mode {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(m);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        #[cfg(unix)]
        if let Some(m) = mode {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(m))?;
        }
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// `_atomic_write_json`: `json.dump(indent=2)`, 0600.
pub fn atomic_write_json(value: &Value, path: &Path) -> std::io::Result<()> {
    atomic_write(
        path,
        pyjson::to_string_pretty(value).as_bytes(),
        Some(0o600),
    )
}

/// A JSON object from `path`, `{}` when missing, malformed or not an object.
pub fn read_object(path: &Path) -> Value {
    match std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        Some(v @ Value::Object(_)) => v,
        _ => Value::Object(Map::new()),
    }
}

fn obj_mut(v: &mut Value) -> &mut Map<String, Value> {
    if !v.is_object() {
        *v = Value::Object(Map::new());
    }
    v.as_object_mut().expect("object")
}

fn io_fail(e: std::io::Error) -> Outcome {
    fail(Msg::raw(e.to_string()))
}

/// `~/.slack/config.json` of `user_home`.
pub fn slack_config_path(user_home: &Path) -> PathBuf {
    user_home.join(".slack").join("config.json")
}

/// A secret config field: show the value a `keychain:` reference resolves to (best
/// effort, falling back to the reference text itself).
pub fn display_secret(value: &str) -> String {
    if keychain::is_ref(value) {
        keychain::resolve_value(value).unwrap_or_else(|_| value.to_string())
    } else {
        value.to_string()
    }
}

/// The value to store for a secret field being replaced by `new`: when the existing
/// value is a `keychain:` reference the secret goes to the store it names and the
/// reference stays; otherwise the plaintext is kept (the Python behaviour).
pub fn store_secret(existing: Option<&str>, new: &str) -> Result<String, String> {
    if let Some((service, account)) = existing.and_then(keychain::parse_ref) {
        let store = keychain::open_store(service).map_err(|e| e.to_string())?;
        store.set(account, new).map_err(|e| e.to_string())?;
        return Ok(existing.unwrap_or_default().to_string());
    }
    Ok(new.to_string())
}

// ------------------------------------------------------------------- Slack config

pub fn update_slack_webhook(webhook_url: &str, config_path: &Path) -> Outcome {
    let webhook_url = webhook_url.trim();
    if webhook_url.is_empty() {
        return fail(Msg::tr("Webhook URL is required"));
    }
    let mut config = read_object(config_path);
    let existing = config
        .get("webhook_url")
        .and_then(Value::as_str)
        .map(str::to_string);
    let value = match store_secret(existing.as_deref(), webhook_url) {
        Ok(v) => v,
        Err(e) => return fail(Msg::raw(e)),
    };
    obj_mut(&mut config).insert("webhook_url".into(), json!(value));
    match atomic_write_json(&config, config_path) {
        Ok(()) => ok(Msg::tr("Slack webhook updated")),
        Err(e) => io_fail(e),
    }
}

/// `read_default_block_templates`: `docs/slack-templates/*.json` keyed by file stem.
pub fn read_default_block_templates(dir: &Path) -> Map<String, Value> {
    let mut templates = Map::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return templates;
    };
    let mut paths: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let Some(data) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            continue;
        };
        let Some(blocks) = data.get("blocks").filter(|b| b.is_array()) else {
            continue;
        };
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        templates.insert(
            stem,
            json!({
                "blocks": blocks,
                "notification_key": data.get("notification_key").cloned().unwrap_or(Value::Null),
            }),
        );
    }
    templates
}

pub fn upsert_block_template(
    name: &str,
    blocks_json: &str,
    notification_key: &str,
    original_name: &str,
    config_path: &Path,
) -> Outcome {
    let (name, original_name, notification_key) =
        (name.trim(), original_name.trim(), notification_key.trim());
    if name.is_empty() {
        return fail(Msg::tr("Template name is required"));
    }
    if !notification_key.is_empty()
        && !BLOCK_TEMPLATE_NOTIFICATION_KEYS
            .iter()
            .any(|(k, _)| *k == notification_key)
    {
        return fail(Msg::tra(
            "Unknown notification key: {key}",
            &[("key", notification_key)],
        ));
    }
    let blocks: Value = match serde_json::from_str(blocks_json) {
        Ok(b) => b,
        Err(_) => return fail(Msg::tr("Blocks JSON is invalid")),
    };
    if !blocks.is_array() {
        return fail(Msg::tr("Blocks JSON must be a list"));
    }

    let mut config = read_object(config_path);
    let templates = obj_mut(
        obj_mut(&mut config)
            .entry("block_templates")
            .or_insert_with(|| json!({})),
    );
    let renaming = !original_name.is_empty() && original_name != name;
    if renaming {
        if !templates.contains_key(original_name) {
            return fail(Msg::tra(
                "Unknown template: {name}",
                &[("name", original_name)],
            ));
        }
        if templates.contains_key(name) {
            return fail(Msg::tra(
                "Template name already in use: {name}",
                &[("name", name)],
            ));
        }
        templates.shift_remove(original_name);
    }
    let mut unbound_from: Option<String> = None;
    if !notification_key.is_empty() {
        for (other_name, other) in templates.iter_mut() {
            if other_name != name
                && other.get("notification_key").and_then(Value::as_str) == Some(notification_key)
            {
                other["notification_key"] = Value::Null;
                unbound_from = Some(other_name.clone());
                break;
            }
        }
    }
    let is_new = !templates.contains_key(name);
    templates.insert(
        name.to_string(),
        json!({
            "blocks": blocks,
            "notification_key": if notification_key.is_empty() { Value::Null } else { json!(notification_key) },
        }),
    );
    if let Err(e) = atomic_write_json(&config, config_path) {
        return io_fail(e);
    }
    let base = if renaming {
        Msg::tra(
            "Renamed template {old} to {new}",
            &[("old", original_name), ("new", name)],
        )
    } else if is_new {
        Msg::tra("Added template {name}", &[("name", name)])
    } else {
        Msg::tra("Updated template {name}", &[("name", name)])
    };
    match unbound_from {
        Some(other) => ok(Msg::tra(
            "{message} (was previously bound to '{name}')",
            &[("message", &base.english()), ("name", &other)],
        )),
        None => ok(base),
    }
}

pub fn delete_block_template(name: &str, config_path: &Path) -> Outcome {
    let mut config = read_object(config_path);
    let removed = config
        .get_mut("block_templates")
        .and_then(Value::as_object_mut)
        .and_then(|t| t.shift_remove(name));
    if removed.is_none() {
        return fail(Msg::tra("Unknown template: {name}", &[("name", name)]));
    }
    match atomic_write_json(&config, config_path) {
        Ok(()) => ok(Msg::tra("Deleted template {name}", &[("name", name)])),
        Err(e) => io_fail(e),
    }
}

/// Posts `(text, blocks)` to the webhook; injectable so tests never reach Slack.
pub type Poster<'a> = &'a dyn Fn(&str, &str, &Value) -> Result<u16, String>;

pub fn send_test_block_template(
    name: &str,
    config_path: &Path,
    defaults_dir: &Path,
    post: Poster,
) -> Outcome {
    let config = read_object(config_path);
    let template = config
        .get("block_templates")
        .and_then(|t| t.get(name))
        .cloned()
        .or_else(|| {
            read_default_block_templates(defaults_dir)
                .get(name)
                .cloned()
        });
    let Some(template) = template else {
        return fail(Msg::tra("Unknown template: {name}", &[("name", name)]));
    };
    let blocks = crate::slack::substitute_message(
        template.get("blocks").unwrap_or(&json!([])),
        "(test message)",
    );
    let result = crate::slack::load_webhook_url(config_path, None)
        .and_then(|url| post(&url, "(test message)", &blocks));
    match result {
        Ok(_) => ok(Msg::tra(
            "Sent test message for template {name}",
            &[("name", name)],
        )),
        Err(e) => fail(Msg::tra("Test message failed: {error}", &[("error", &e)])),
    }
}

/// The real poster.
pub fn post_to_slack(url: &str, text: &str, blocks: &Value) -> Result<u16, String> {
    crate::slack::post_message(url, text, Some(blocks))
}

// ----------------------------------------------------------------- instructions

pub fn read_custom_instructions(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Overwrite the instructions file (clearing with "" is deliberate).
pub fn write_custom_instructions(text: &str, path: &Path) -> Outcome {
    match atomic_write(path, text.as_bytes(), None) {
        Ok(()) => ok(Msg::tr("Instructions saved")),
        Err(e) => io_fail(e),
    }
}

// -------------------------------------------------------------------- chat tools

fn is_write_tool(name: &str) -> bool {
    CHAT_WRITE_TOOLS.iter().any(|(n, _)| *n == name)
}

/// Write tools the user chose to run without review (a missing/corrupt file or an
/// unknown name means review).
pub fn auto_approved(path: &Path) -> Vec<String> {
    let names = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("auto_approve").cloned());
    let Some(Value::Array(names)) = names else {
        return Vec::new();
    };
    let mut out: Vec<String> = names
        .iter()
        .filter_map(Value::as_str)
        .filter(|n| is_write_tool(n))
        .map(str::to_string)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Replace the whole list; every name must be a known write tool.
pub fn save_auto_approve(names: &[String], path: &Path) -> Result<(), String> {
    let mut chosen: Vec<&String> = names.iter().collect();
    chosen.sort();
    chosen.dedup();
    if let Some(unknown) = chosen.iter().find(|n| !is_write_tool(n)) {
        return Err(format!("{unknown} is not a chat write tool"));
    }
    let value = json!({"auto_approve": chosen});
    atomic_write_json(&value, path).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- plugin settings

/// The `settings_fields` keys of the plugin behind a registry `entry_point`
/// (`bin.loop_plugins.<module>` only; anything else has none).
pub fn plugin_setting_keys(entry_point: &str) -> Vec<String> {
    use crate::connectors_config::ConfigConnectors;
    use crate::loopkit::LoopPlugin;
    use std::sync::Arc;
    let Some(module) = entry_point.strip_prefix("bin.loop_plugins.") else {
        return Vec::new();
    };
    if module.is_empty() || !module.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
        return Vec::new();
    }
    let Some(module) = crate::loop_plugins::module_for_entry(entry_point) else {
        return Vec::new();
    };
    let connectors = Arc::new(ConfigConnectors::real());
    let plugin: Arc<dyn LoopPlugin> = if module == "linkedin_autopilot" {
        Arc::new(crate::loop_plugins::linkedin_autopilot::LinkedInAutopilot::new(connectors))
    } else {
        match crate::loop_plugins::build(module, connectors) {
            Some(p) => p,
            None => return Vec::new(),
        }
    };
    plugin
        .settings_fields()
        .iter()
        .map(|f| f.key.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn webhook_update_keeps_unknown_keys_and_is_0600() {
        let d = dir();
        let p = d.path().join(".slack").join("config.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, r#"{"webhook_url":"old","extra":{"a":1}}"#).unwrap();
        let (okk, msg) = update_slack_webhook("  https://hooks/x ", &p);
        assert!(okk, "{msg:?}");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["webhook_url"], "https://hooks/x");
        assert_eq!(v["extra"]["a"], 1);
        #[cfg(unix)]
        assert_eq!(mode(&p), 0o600);
        assert!(!update_slack_webhook("  ", &p).0);
    }

    #[test]
    fn a_keychain_reference_stays_and_the_secret_goes_to_the_store() {
        let d = dir();
        let store = d.path().join("kc");
        std::env::set_var("LOOPX_KEYCHAIN", format!("file:{}", store.display()));
        let p = d.path().join("config.json");
        let reference = keychain::format_ref(keychain::SERVICE_CONFIG, "slack/webhook_url");
        std::fs::write(&p, json!({"webhook_url": reference}).to_string()).unwrap();
        let (okk, _) = update_slack_webhook("https://new", &p);
        assert!(okk);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["webhook_url"], reference.as_str());
        assert_eq!(display_secret(&reference), "https://new");
        std::env::remove_var("LOOPX_KEYCHAIN");
    }

    #[test]
    fn block_templates_validate_bind_rename_and_delete() {
        let d = dir();
        let p = d.path().join("config.json");
        let (a, m) = upsert_block_template("one", "[]", "gitlab_wrapup_failed", "", &p);
        assert!(a && m.english() == "Added template one");
        let (a, m) = upsert_block_template(
            "two",
            "[{\"type\":\"divider\"}]",
            "gitlab_wrapup_failed",
            "",
            &p,
        );
        assert!(a);
        assert_eq!(
            m.english(),
            "Added template two (was previously bound to 'one')"
        );
        let v = read_object(&p);
        assert_eq!(v["block_templates"]["one"]["notification_key"], Value::Null);
        assert!(!upsert_block_template("x", "{}", "", "", &p).0);
        assert!(!upsert_block_template("x", "nope", "", "", &p).0);
        assert!(!upsert_block_template("x", "[]", "bogus", "", &p).0);
        assert!(!upsert_block_template("", "[]", "", "", &p).0);
        let (a, m) = upsert_block_template("three", "[]", "", "one", &p);
        assert!(a && m.english() == "Renamed template one to three");
        assert!(!upsert_block_template("two", "[]", "", "three", &p).0);
        assert!(!upsert_block_template("z", "[]", "", "missing", &p).0);
        assert!(delete_block_template("three", &p).0);
        assert!(!delete_block_template("three", &p).0);
    }

    #[test]
    fn test_message_falls_back_to_defaults_and_reports_errors() {
        let d = dir();
        let p = d.path().join("config.json");
        std::fs::write(&p, r#"{"webhook_url":"https://hook"}"#).unwrap();
        let defaults = d.path().join("tpl");
        std::fs::create_dir_all(&defaults).unwrap();
        std::fs::write(
            defaults.join("alert.json"),
            r#"{"blocks":[{"type":"section","text":{"type":"mrkdwn","text":"{{message}}"}}]}"#,
        )
        .unwrap();
        std::fs::write(defaults.join("bad.json"), r#"{"blocks":3}"#).unwrap();
        let seen = std::sync::Mutex::new(Vec::new());
        let poster = |url: &str, text: &str, blocks: &Value| {
            seen.lock()
                .unwrap()
                .push((url.to_string(), text.to_string(), blocks.clone()));
            Ok(200)
        };
        let (a, m) = send_test_block_template("alert", &p, &defaults, &poster);
        assert!(a, "{m:?}");
        let calls = seen.lock().unwrap();
        assert_eq!(calls[0].0, "https://hook");
        assert_eq!(calls[0].2[0]["text"]["text"], "(test message)");
        assert!(!send_test_block_template("bad", &p, &defaults, &poster).0);
        let failing = |_: &str, _: &str, _: &Value| Err("boom".to_string());
        let (a, m) = send_test_block_template("alert", &p, &defaults, &failing);
        assert!(!a);
        assert_eq!(m.english(), "Test message failed: boom");
    }

    #[test]
    fn instructions_roundtrip_and_chat_tools() {
        let d = dir();
        let p = d.path().join("home").join("instructions.md");
        assert_eq!(read_custom_instructions(&p), "");
        assert!(write_custom_instructions("be kind\n", &p).0);
        assert_eq!(read_custom_instructions(&p), "be kind\n");
        assert!(write_custom_instructions("", &p).0);
        assert_eq!(read_custom_instructions(&p), "");

        let c = d.path().join("chat_tools.json");
        assert!(auto_approved(&c).is_empty());
        save_auto_approve(&["mail.send".into(), "gitlab.comment".into()], &c).unwrap();
        assert_eq!(auto_approved(&c), vec!["gitlab.comment", "mail.send"]);
        #[cfg(unix)]
        assert_eq!(mode(&c), 0o600);
        assert!(save_auto_approve(&["rm.rf".into()], &c).is_err());
        assert_eq!(auto_approved(&c).len(), 2);
    }
}
