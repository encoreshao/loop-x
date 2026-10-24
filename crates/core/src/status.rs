//! Run-status files the dashboard and schedulers read: `outputs/status.json`
//! (and `outputs/status/<loop>.json`), plus the shared per-topic
//! `outputs/topic-monitor/status.json`. Mirrors `write_status` /
//! `write_topic_status` in `bin/web/dashboard_server.py`.

use crate::config::home_dir;
use crate::pyjson;
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub fn topic_status_path() -> PathBuf {
    home_dir()
        .join("outputs")
        .join("topic-monitor")
        .join("status.json")
}

/// Python's `datetime.now(timezone.utc).isoformat()`: microseconds, `+00:00`.
fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, false)
}

fn write_doc(path: &Path, doc: &Value) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, pyjson::to_string_pretty(doc))
}

/// `{"state": "never_run"}` if missing, `{"state": "unknown"}` if corrupt.
pub fn read_status(path: &Path) -> Value {
    if !path.exists() {
        return json!({"state": "never_run"});
    }
    match std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        Some(v @ Value::Object(_)) => v,
        _ => json!({"state": "unknown"}),
    }
}

/// Replace the whole status document with `{state, updated_at, ...extra}`.
pub fn write_status(path: &Path, state: &str, extra: &[(&str, Value)]) -> std::io::Result<Value> {
    let mut doc = Map::new();
    doc.insert("state".into(), json!(state));
    doc.insert("updated_at".into(), json!(now_iso()));
    for (k, v) in extra {
        doc.insert((*k).to_string(), v.clone());
    }
    let doc = Value::Object(doc);
    write_doc(path, &doc)?;
    Ok(doc)
}

/// `{"topics": {}}` for a missing, corrupt or wrongly-shaped file.
pub fn read_topic_status(path: &Path) -> Value {
    let parsed = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());
    match parsed {
        Some(v) if v.get("topics").is_some_and(Value::is_object) => v,
        _ => json!({"topics": {}}),
    }
}

/// Update one topic's entry, leaving every other topic untouched.
pub fn write_topic_status(
    path: &Path,
    topic: &str,
    state: &str,
    extra: &[(&str, Value)],
) -> std::io::Result<Value> {
    let mut doc = read_topic_status(path);
    let mut entry = Map::new();
    entry.insert("state".into(), json!(state));
    entry.insert("updated_at".into(), json!(now_iso()));
    for (k, v) in extra {
        entry.insert((*k).to_string(), v.clone());
    }
    doc["topics"]
        .as_object_mut()
        .expect("read_topic_status guarantees an object")
        .insert(topic.to_string(), Value::Object(entry));
    write_doc(path, &doc)?;
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_corrupt_status_read_as_synthetic_states() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("status.json");
        assert_eq!(read_status(&p)["state"], "never_run");
        std::fs::write(&p, "{nope").unwrap();
        assert_eq!(read_status(&p)["state"], "unknown");
        std::fs::write(&p, "[1]").unwrap();
        assert_eq!(read_status(&p)["state"], "unknown");
    }

    #[test]
    fn write_replaces_whole_document_and_creates_dirs() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a/b/status.json");
        write_status(&p, "running", &[("pid", json!(42))]).unwrap();
        write_status(&p, "idle", &[("last_exit_code", json!(0))]).unwrap();
        let doc = read_status(&p);
        assert_eq!(doc["state"], "idle");
        assert_eq!(doc["last_exit_code"], 0);
        assert!(doc.get("pid").is_none());
        assert!(doc["updated_at"].as_str().unwrap().ends_with("+00:00"));
    }

    #[test]
    fn topic_status_merges_per_topic() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("topic-monitor/status.json");
        write_topic_status(&p, "a", "running", &[("current_step", json!("x"))]).unwrap();
        write_topic_status(&p, "b", "idle", &[]).unwrap();
        let doc = write_topic_status(&p, "a", "failed", &[]).unwrap();
        assert_eq!(doc["topics"]["a"]["state"], "failed");
        assert!(doc["topics"]["a"].get("current_step").is_none());
        assert_eq!(doc["topics"]["b"]["state"], "idle");
        assert_eq!(read_topic_status(&p), doc);
    }

    #[test]
    fn topic_status_tolerates_wrong_shape() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("status.json");
        std::fs::write(&p, r#"{"topics": []}"#).unwrap();
        assert_eq!(read_topic_status(&p), json!({"topics": {}}));
    }
}
