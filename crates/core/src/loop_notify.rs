//! Route a loop's notification to the connectors listed in its `notify` field,
//! or to the default Slack webhook when it lists none (mirrors `bin/notify.py`),
//! plus `resolve_blocks` from `bin/slack_notify.py` (notification key ->
//! Block Kit template). Never fails: every target yields one
//! `(connector_id, ok, message)` result, and messages never carry secrets.
//!
//! Connectors are injected: a [`ConnectorLoader`] turns an account id into a
//! [`NotifyConnector`], so the later connectors port plugs in without changes here.

use crate::slack;
use link_flatten::flatten_links;
use serde_json::Value;
use std::path::Path;

pub const DEFAULT_ID: &str = "slack-default";

/// `(connector_id, ok, message)`.
pub type NotifyResult = (String, bool, String);

/// Why a connector send failed; only the class (or HTTP status) is ever reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// An HTTP error with this status: reported as `HTTP <status>`.
    Http(u16),
    /// Anything else: reported as this class name.
    Other(String),
}

impl SendError {
    fn describe(&self) -> String {
        match self {
            SendError::Http(s) => format!("HTTP {s}"),
            SendError::Other(name) => name.clone(),
        }
    }
}

/// Why a connector could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    Unknown,
    Config,
    Other(String),
}

pub trait NotifyConnector {
    /// Capability names, e.g. `notify`.
    fn capabilities(&self) -> Vec<String>;
    /// Connector type (`slack`, `webhook`, ...); only those two get Slack mrkdwn links.
    fn kind(&self) -> String;
    fn send(&self, text: &str, blocks: Option<&Value>) -> Result<(), SendError>;
}

pub type ConnectorLoader<'a> = &'a dyn Fn(&str) -> Result<Box<dyn NotifyConnector>, LoadError>;
/// Looks up a loop registry entry by name (`Err` = unknown / unreadable).
pub type LoopLookup<'a> = &'a dyn Fn(&str) -> Result<Value, String>;
/// Sends to the default Slack webhook.
pub type DefaultSender<'a> = &'a dyn Fn(&str, Option<&Value>) -> Result<(), SendError>;

mod link_flatten {
    /// Slack `<url|label>` as plain `label (url)` (and `<url>` as `url`) for chat
    /// services that would show the markup literally.
    pub fn flatten_links(text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(start) = rest.find("<http") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            match parse_link(after) {
                Some((url, label, consumed)) => {
                    match label {
                        Some(l) if !l.is_empty() => out.push_str(&format!("{l} ({url})")),
                        _ => out.push_str(url),
                    }
                    rest = &after[consumed..];
                }
                None => {
                    out.push('<');
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// `after` starts at `http`. Returns (url, label, bytes consumed incl. `>`).
    fn parse_link(after: &str) -> Option<(&str, Option<&str>, usize)> {
        let scheme = if after.starts_with("https://") {
            8
        } else if after.starts_with("http://") {
            7
        } else {
            return None;
        };
        let url_end = after[scheme..]
            .find(|c: char| c == '|' || c == '>' || c.is_whitespace())
            .map(|i| i + scheme)?;
        if url_end == scheme {
            return None;
        }
        let url = &after[..url_end];
        match after[url_end..].chars().next()? {
            '>' => Some((url, None, url_end + 1)),
            '|' => {
                let label_start = url_end + 1;
                let close = after[label_start..].find('>')? + label_start;
                Some((url, Some(&after[label_start..close]), close + 1))
            }
            _ => None,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn flattens_links() {
            assert_eq!(
                flatten_links("see <https://x.io/a|the MR> and <http://y.io> ok"),
                "see the MR (https://x.io/a) and http://y.io ok"
            );
            assert_eq!(
                flatten_links("a < b <https://x |no"),
                "a < b <https://x |no"
            );
            assert_eq!(flatten_links("<https://x.io|>"), "https://x.io");
            assert_eq!(flatten_links("plain"), "plain");
        }
    }
}

/// Route `text` for `loop_name`. With no (or a non-list/empty) `notify` field
/// the default Slack sender is used; otherwise every listed connector is tried
/// independently.
pub fn notify(
    loop_name: &str,
    text: &str,
    blocks: Option<&Value>,
    loop_lookup: LoopLookup,
    loader: ConnectorLoader,
    default_sender: DefaultSender,
) -> Vec<NotifyResult> {
    let ids: Option<Vec<Value>> = loop_lookup(loop_name)
        .ok()
        .and_then(|l| l.get("notify").and_then(Value::as_array).cloned())
        .filter(|a| !a.is_empty());
    let Some(ids) = ids else {
        return vec![match default_sender(text, blocks) {
            Ok(()) => (DEFAULT_ID.to_string(), true, "sent".to_string()),
            Err(e) => (DEFAULT_ID.to_string(), false, e.describe()),
        }];
    };
    ids.iter()
        .map(|id| {
            let account_id = match id {
                Value::String(s) => s.clone(),
                other => crate::pydumps::py_str(other),
            };
            let (ok, message) = send_one(&account_id, text, blocks, loader);
            (account_id, ok, message)
        })
        .collect()
}

fn send_one(
    account_id: &str,
    text: &str,
    blocks: Option<&Value>,
    loader: ConnectorLoader,
) -> (bool, String) {
    let conn = match loader(account_id) {
        Ok(c) => c,
        Err(LoadError::Unknown) => return (false, "unknown connector".to_string()),
        Err(LoadError::Config) => return (false, "connector config error".to_string()),
        Err(LoadError::Other(name)) => return (false, name),
    };
    if !conn.capabilities().iter().any(|c| c == "notify") {
        return (false, "not a notify connector".to_string());
    }
    let kind = conn.kind();
    let rendered;
    let body = if kind == "slack" || kind == "webhook" {
        text
    } else {
        rendered = flatten_links(text);
        &rendered
    };
    match conn.send(body, blocks) {
        Ok(()) => (true, "sent".to_string()),
        Err(e) => (false, e.describe()),
    }
}

/// The Block Kit blocks for `notification_key` with `{{message}}` substituted,
/// or `None` for a falsy key, a missing/malformed config, or no bound template.
/// Best-effort: never errors, so a broken template can't stop the plain alert.
pub fn resolve_blocks(
    notification_key: Option<&str>,
    message: &str,
    config_path: &Path,
) -> Option<Value> {
    let key = notification_key.filter(|k| !k.is_empty())?;
    let config: Value = serde_json::from_str(&std::fs::read_to_string(config_path).ok()?).ok()?;
    let config = config.as_object()?;
    let templates = match config.get("block_templates") {
        None => return None,
        Some(Value::Object(t)) => t,
        Some(_) => return None,
    };
    for template in templates.values() {
        let template = template.as_object()?;
        if template.get("notification_key").and_then(Value::as_str) == Some(key) {
            let blocks = template
                .get("blocks")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()));
            return Some(slack::substitute_message(&blocks, message));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    struct Fake {
        caps: Vec<String>,
        kind: String,
        fail: Option<SendError>,
        sent: Rc<RefCell<Vec<String>>>,
    }

    impl NotifyConnector for Fake {
        fn capabilities(&self) -> Vec<String> {
            self.caps.clone()
        }
        fn kind(&self) -> String {
            self.kind.clone()
        }
        fn send(&self, text: &str, _b: Option<&Value>) -> Result<(), SendError> {
            match &self.fail {
                Some(e) => Err(e.clone()),
                None => {
                    self.sent.borrow_mut().push(text.to_string());
                    Ok(())
                }
            }
        }
    }

    fn fake(
        caps: &[&str],
        kind: &str,
        fail: Option<SendError>,
        sent: &Rc<RefCell<Vec<String>>>,
    ) -> Fake {
        Fake {
            caps: caps.iter().map(|s| s.to_string()).collect(),
            kind: kind.into(),
            fail,
            sent: sent.clone(),
        }
    }

    fn no_default(_: &str, _: Option<&Value>) -> Result<(), SendError> {
        Ok(())
    }

    fn lookup_with(v: Value) -> impl Fn(&str) -> Result<Value, String> {
        move |_| Ok(v.clone())
    }

    fn never_load(_: &str) -> Result<Box<dyn NotifyConnector>, LoadError> {
        Err(LoadError::Unknown)
    }

    #[test]
    fn default_routing_when_no_notify_field() {
        let sent = RefCell::new(Vec::new());
        let sender = |t: &str, _: Option<&Value>| {
            sent.borrow_mut().push(t.to_string());
            Ok(())
        };
        let out = notify(
            "l",
            "hi",
            None,
            &lookup_with(json!({"name": "l"})),
            &never_load,
            &sender,
        );
        assert_eq!(out, vec![("slack-default".into(), true, "sent".into())]);
        assert_eq!(*sent.borrow(), ["hi"]);
    }

    #[test]
    fn non_list_or_failed_lookup_uses_default() {
        for lookup in [
            Box::new(lookup_with(json!({"notify": "oops"})))
                as Box<dyn Fn(&str) -> Result<Value, String>>,
            Box::new(lookup_with(json!({"notify": []}))),
            Box::new(|_: &str| Err("KeyError".to_string())),
        ] {
            let out = notify("l", "hi", None, &*lookup, &never_load, &no_default);
            assert_eq!(out[0].0, "slack-default");
            assert!(out[0].1);
        }
    }

    #[test]
    fn default_sender_failure_is_reported_not_raised() {
        let boom = |_: &str, _: Option<&Value>| Err(SendError::Other("OSError".into()));
        let out = notify("l", "hi", None, &lookup_with(json!({})), &never_load, &boom);
        assert_eq!(out, vec![("slack-default".into(), false, "OSError".into())]);
        let http = |_: &str, _: Option<&Value>| Err(SendError::Http(500));
        let out = notify("l", "hi", None, &lookup_with(json!({})), &never_load, &http);
        assert_eq!(out[0].2, "HTTP 500");
    }

    #[test]
    fn routes_to_each_connector_and_one_failure_does_not_stop_others() {
        let sent = Rc::new(RefCell::new(Vec::new()));
        let conns: HashMap<&str, (Vec<&str>, Option<SendError>)> = HashMap::from([
            (
                "a",
                (
                    vec!["notify"],
                    Some(SendError::Other("RuntimeError".into())),
                ),
            ),
            ("b", (vec!["notify"], None)),
            ("gh", (vec!["issues"], None)),
        ]);
        let loader = |id: &str| -> Result<Box<dyn NotifyConnector>, LoadError> {
            match id {
                "ghost" => Err(LoadError::Unknown),
                "badcfg" => Err(LoadError::Config),
                _ => {
                    let (caps, fail) = conns.get(id).cloned().ok_or(LoadError::Unknown)?;
                    Ok(Box::new(fake(&caps, "feishu", fail, &sent)))
                }
            }
        };
        let out = notify(
            "x",
            "see <https://x.io|MR>",
            None,
            &lookup_with(json!({"notify": ["a", "b", "gh", "ghost", "badcfg"]})),
            &loader,
            &no_default,
        );
        assert_eq!(
            out,
            vec![
                ("a".into(), false, "RuntimeError".into()),
                ("b".into(), true, "sent".into()),
                ("gh".into(), false, "not a notify connector".into()),
                ("ghost".into(), false, "unknown connector".into()),
                ("badcfg".into(), false, "connector config error".into()),
            ]
        );
        // non-Slack connector types get links flattened
        assert_eq!(*sent.borrow(), ["see MR (https://x.io)"]);
    }

    #[test]
    fn slack_and_webhook_keep_mrkdwn_links() {
        for kind in ["slack", "webhook"] {
            let sent = Rc::new(RefCell::new(Vec::new()));
            let loader = |_: &str| -> Result<Box<dyn NotifyConnector>, LoadError> {
                Ok(Box::new(fake(&["notify"], kind, None, &sent)))
            };
            notify(
                "x",
                "<https://x.io|MR>",
                None,
                &lookup_with(json!({"notify": ["w"]})),
                &loader,
                &no_default,
            );
            assert_eq!(*sent.borrow(), ["<https://x.io|MR>"]);
        }
    }

    #[test]
    fn http_error_message_has_no_secret() {
        let sent = Rc::new(RefCell::new(Vec::new()));
        let loader = |_: &str| -> Result<Box<dyn NotifyConnector>, LoadError> {
            Ok(Box::new(fake(
                &["notify"],
                "webhook",
                Some(SendError::Http(500)),
                &sent,
            )))
        };
        let out = notify(
            "x",
            "hi",
            None,
            &lookup_with(json!({"notify": ["w"]})),
            &loader,
            &no_default,
        );
        assert_eq!(out, vec![("w".into(), false, "HTTP 500".into())]);
    }

    fn write_config(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.json");
        std::fs::write(&p, body).unwrap();
        (d, p)
    }

    #[test]
    fn resolve_blocks_substitutes_matching_template() {
        let (_d, p) = write_config(
            r#"{"block_templates": {"t1": {"notification_key": "other", "blocks": []},
               "t2": {"notification_key": "gitlab_wrapup_failed",
                      "blocks": [{"type": "section", "text": {"type": "mrkdwn", "text": "x {{message}}"}}]}}}"#,
        );
        let blocks = resolve_blocks(Some("gitlab_wrapup_failed"), "disk full", &p).unwrap();
        assert_eq!(blocks[0]["text"]["text"], "x disk full");
    }

    #[test]
    fn resolve_blocks_none_cases() {
        let (_d, p) =
            write_config(r#"{"block_templates": {"t": {"notification_key": "a", "blocks": []}}}"#);
        assert!(resolve_blocks(Some("zzz"), "m", &p).is_none());
        assert!(resolve_blocks(None, "m", &p).is_none());
        assert!(resolve_blocks(Some(""), "m", &p).is_none());
        assert!(resolve_blocks(Some("a"), "m", Path::new("/definitely/not/here")).is_none());
        let (_d2, bad) = write_config("not json");
        assert!(resolve_blocks(Some("a"), "m", &bad).is_none());
        let (_d3, odd) =
            write_config(r#"{"block_templates": {"t": 3, "u": {"notification_key": "a"}}}"#);
        assert!(resolve_blocks(Some("a"), "m", &odd).is_none());
        let (_d4, noblocks) =
            write_config(r#"{"block_templates": {"u": {"notification_key": "a"}}}"#);
        assert_eq!(resolve_blocks(Some("a"), "m", &noblocks), Some(json!([])));
    }
}
