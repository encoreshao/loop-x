//! Slack incoming-webhook posting. Mirrors `bin/slack_notify.py`; the config is
//! `~/.slack/config.json` (see docs/contract/json-files.md).

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub fn default_config_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".slack").join("config.json")
}

fn read_config(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// The webhook for `bundle` if it has a non-empty override, else the default.
pub fn load_webhook_url(path: &Path, bundle: Option<&str>) -> Result<String, String> {
    let config = read_config(path)?;
    if let Some(url) = bundle
        .filter(|b| !b.is_empty())
        .and_then(|b| config.pointer(&format!("/bundle_webhooks/{}", escape_pointer(b))))
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
    {
        return resolve(url);
    }
    let url = config
        .get("webhook_url")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{}: missing \"webhook_url\"", path.display()))?;
    resolve(url)
}

/// A plain webhook URL, or a `keychain:` reference resolved through the secret store.
fn resolve(value: &str) -> Result<String, String> {
    crate::keychain::resolve_value(value).map_err(|e| e.to_string())
}

fn escape_pointer(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// Replace every literal `{{message}}` in any string within `node`.
pub fn substitute_message(node: &Value, message: &str) -> Value {
    match node {
        Value::String(s) => Value::String(s.replace("{{message}}", message)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|i| substitute_message(i, message))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_message(v, message)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Python truthiness of a JSON value (`if blocks:`).
pub fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// POST `{text, blocks?}` to the webhook; returns the HTTP status.
pub fn post_message(webhook_url: &str, text: &str, blocks: Option<&Value>) -> Result<u16, String> {
    let mut body = json!({ "text": text });
    if let Some(blocks) = blocks.filter(|b| is_truthy(b)) {
        body["blocks"] = blocks.clone();
    }
    match ureq::post(webhook_url)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
    {
        Ok(resp) => Ok(resp.status()),
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP Error {code}")),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(body: &str) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.json");
        std::fs::write(&p, body).unwrap();
        (d, p)
    }

    #[test]
    fn bundle_override_then_fallback() {
        let (_d, p) = config(r#"{"webhook_url":"D","bundle_webhooks":{"a/b":"X","blank":""}}"#);
        assert_eq!(load_webhook_url(&p, Some("a/b")).unwrap(), "X");
        assert_eq!(load_webhook_url(&p, Some("blank")).unwrap(), "D");
        assert_eq!(load_webhook_url(&p, Some("none")).unwrap(), "D");
        assert_eq!(load_webhook_url(&p, None).unwrap(), "D");
    }

    #[test]
    fn malformed_references_are_an_error_not_a_url() {
        let (_d, p) = config(r#"{"webhook_url":"keychain:not-a-valid-ref"}"#);
        assert!(load_webhook_url(&p, None)
            .unwrap_err()
            .contains("malformed"));
    }

    #[test]
    fn substitutes_nested_strings() {
        let v = json!([{"text": {"text": "alert: {{message}}"}, "n": 1}]);
        assert_eq!(
            substitute_message(&v, "boom"),
            json!([{"text": {"text": "alert: boom"}, "n": 1}])
        );
    }

    #[test]
    fn truthiness_matches_python() {
        for falsy in [
            json!(null),
            json!(false),
            json!(0),
            json!(""),
            json!([]),
            json!({}),
        ] {
            assert!(!is_truthy(&falsy), "{falsy}");
        }
        assert!(is_truthy(&json!([1])));
    }
}
