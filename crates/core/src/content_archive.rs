//! Small newest-first JSON archive for content the loops produce (draft
//! replies, RSS highlights, meeting briefs) so the dashboard can show it any
//! time after the Slack message has scrolled away (mirrors
//! `bin/content_archive.py`). One file holds a list of records; only the
//! newest `cap` are kept.

use crate::pyjson;
use serde_json::Value;
use std::path::Path;

pub const DEFAULT_CAP: usize = 300;

/// The archived records, newest first; empty when missing or unreadable.
pub fn load(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Array(a) => Some(a.into_iter().filter(Value::is_object).collect()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Put `records` (in the given order) ahead of what is stored, keep the
/// newest `cap`, and write atomically. Does nothing for an empty batch.
pub fn append(path: &Path, records: &[Value], cap: usize) -> std::io::Result<()> {
    let mut all: Vec<Value> = records.iter().filter(|r| r.is_object()).cloned().collect();
    if all.is_empty() {
        return Ok(());
    }
    all.extend(load(path));
    all.truncate(cap);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(
        ".{name}.{}.tmp",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    let mut text = pyjson::to_string_pretty(&Value::Array(all));
    text.push('\n');
    let result = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn newest_first_and_capped() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub").join("content.json");
        append(&p, &[json!({"n": 1}), json!({"n": 2})], 3).unwrap();
        append(&p, &[json!({"n": 3}), json!("junk"), json!({"n": 4})], 3).unwrap();
        let got: Vec<i64> = load(&p).iter().map(|r| r["n"].as_i64().unwrap()).collect();
        assert_eq!(got, [3, 4, 1]);
    }

    #[test]
    fn empty_batch_writes_nothing_and_bad_file_is_empty() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.json");
        append(&p, &[], 3).unwrap();
        assert!(!p.exists());
        std::fs::write(&p, "{nope").unwrap();
        assert!(load(&p).is_empty());
        std::fs::write(&p, "{}").unwrap();
        assert!(load(&p).is_empty());
    }
}
