//! `outputs/inbox-triage/status.json`: per-inbox state the dashboard's Inbox
//! Triage page reads (idle/running/failed/needs_reauth, last counts, the latest
//! urgent list). Never holds message body text. Mirrors `bin/inbox_status.py`.

use crate::inbox_seen::output_dir_for;
use crate::mail_time::isoformat_utc;
use crate::pyjson;
use crate::runner_util::repo_root;
use chrono::Utc;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub fn default_status_path() -> PathBuf {
    output_dir_for(&repo_root()).join("status.json")
}

/// `{"inboxes": {...}}`; empty when missing, corrupt or malformed.
pub fn read(path: &Path) -> Value {
    match std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        Some(v) if v.get("inboxes").is_some_and(Value::is_object) => v,
        _ => json!({"inboxes": {}}),
    }
}

fn save(data: &Value, path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, pyjson::to_string_pretty(data))?;
    std::fs::rename(&tmp, path)
}

/// Merge `extra` into the inbox's entry and stamp `state` / `updated_at`.
pub fn write(
    name: &str,
    state: &str,
    path: &Path,
    extra: &[(&str, Value)],
) -> std::io::Result<Value> {
    let mut data = read(path);
    let inboxes = data["inboxes"]
        .as_object_mut()
        .expect("read() guarantees an object");
    let entry = inboxes
        .entry(name)
        .or_insert_with(|| Value::Object(Map::new()));
    let obj = entry
        .as_object_mut()
        .ok_or_else(|| std::io::Error::other("entry is not an object"))?;
    for (k, v) in extra {
        obj.insert((*k).to_string(), v.clone());
    }
    obj.insert("state".into(), json!(state));
    obj.insert("updated_at".into(), json!(isoformat_utc(Utc::now())));
    let out = entry.clone();
    save(&data, path)?;
    Ok(out)
}

/// Drop a deleted inbox's entry. True if there was one.
pub fn remove(name: &str, path: &Path) -> std::io::Result<bool> {
    let mut data = read(path);
    if data["inboxes"]
        .as_object_mut()
        .expect("object")
        .remove(name)
        .is_none()
    {
        return Ok(false);
    }
    save(&data, path)?;
    Ok(true)
}

pub fn any_running(path: &Path) -> bool {
    read(path)["inboxes"].as_object().is_some_and(|m| {
        m.values()
            .any(|e| e.get("state") == Some(&json!("running")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn read_missing_is_empty() {
        let d = tempdir().unwrap();
        assert_eq!(read(&d.path().join("status.json")), json!({"inboxes": {}}));
    }

    #[test]
    fn write_merges_per_inbox() {
        let d = tempdir().unwrap();
        let p = d.path().join("status.json");
        write("w", "running", &p, &[]).unwrap();
        write("h", "idle", &p, &[("counts", json!({"fyi": 1}))]).unwrap();
        write("w", "failed", &p, &[("error", json!("boom"))]).unwrap();
        let data = read(&p);
        assert_eq!(data["inboxes"]["w"]["state"], "failed");
        assert_eq!(data["inboxes"]["w"]["error"], "boom");
        assert_eq!(data["inboxes"]["h"]["counts"], json!({"fyi": 1}));
        assert!(data["inboxes"]["h"]["updated_at"].is_string());
    }

    #[test]
    fn later_writes_keep_earlier_fields() {
        let d = tempdir().unwrap();
        let p = d.path().join("status.json");
        write("w", "idle", &p, &[("counts", json!({"a": 1}))]).unwrap();
        write("w", "running", &p, &[]).unwrap();
        assert_eq!(read(&p)["inboxes"]["w"]["counts"], json!({"a": 1}));
    }

    #[test]
    fn any_running_sees_a_running_inbox() {
        let d = tempdir().unwrap();
        let p = d.path().join("status.json");
        assert!(!any_running(&p));
        write("w", "running", &p, &[]).unwrap();
        assert!(any_running(&p));
    }

    #[test]
    fn corrupt_status_reads_empty() {
        let d = tempdir().unwrap();
        let p = d.path().join("status.json");
        std::fs::write(&p, "[1,2").unwrap();
        assert_eq!(read(&p), json!({"inboxes": {}}));
    }

    #[test]
    fn remove_drops_only_that_inbox() {
        let d = tempdir().unwrap();
        let p = d.path().join("status.json");
        write("w", "idle", &p, &[]).unwrap();
        write("h", "failed", &p, &[]).unwrap();
        assert!(remove("w", &p).unwrap());
        assert_eq!(
            read(&p)["inboxes"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["h"]
        );
        assert!(!remove("w", &p).unwrap());
    }

    #[test]
    fn remove_on_a_missing_file_is_a_no_op() {
        let d = tempdir().unwrap();
        let p = d.path().join("status.json");
        assert!(!remove("w", &p).unwrap());
        assert!(!p.exists());
    }
}
