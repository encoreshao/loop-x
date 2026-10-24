//! Per-issue opt-out of the gitlab-issue loop's tracking
//! (`<LOOP_ENGINEERING_HOME>/issue_tracking.json`). Mirrors
//! `bin/issue_tracking_config.py`: only DISABLED issues have an entry, keyed
//! `"<alias>#<issue_iid>"`; anything without one is enabled.

use crate::config::home_dir;
use crate::pyjson;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub fn default_tracking_path() -> PathBuf {
    home_dir().join("issue_tracking.json")
}

fn key(alias: &str, issue_iid: impl std::fmt::Display) -> String {
    format!("{alias}#{issue_iid}")
}

/// A missing file is empty; unreadable or non-object JSON is an error (Python
/// lets `json.load` raise).
fn read(path: &Path) -> Result<Map<String, Value>, String> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    match serde_json::from_str::<Value>(&text).map_err(|e| e.to_string())? {
        Value::Object(m) => Ok(m),
        _ => Err(format!("{} must contain a JSON object", path.display())),
    }
}

/// Same-directory temp file then rename, so a reader never sees a torn file.
fn write(entries: &Map<String, Value>, path: &Path) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, pyjson::to_string_pretty(entries)).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Whether the loop should work on this issue (default true). Python raises on
/// a corrupt file; here that is an `Err` the caller may treat as it likes.
pub fn is_issue_enabled(
    alias: &str,
    issue_iid: impl std::fmt::Display,
    path: &Path,
) -> Result<bool, String> {
    let entries = read(path)?;
    Ok(match entries.get(&key(alias, issue_iid)) {
        None => true,
        Some(entry) => match entry.get("enabled") {
            None => true,
            Some(Value::Bool(b)) => *b,
            // Python `bool(x)` truthiness for odd stored values.
            Some(Value::Null) => false,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Value::String(s)) => !s.is_empty(),
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::Object(o)) => !o.is_empty(),
        },
    })
}

/// Record whether the loop should keep tracking this issue. Re-enabling removes
/// the entry. Returns the same message Python does.
pub fn set_issue_enabled(
    alias: &str,
    issue_iid: impl std::fmt::Display + Copy,
    enabled: bool,
    path: &Path,
) -> Result<String, String> {
    let mut entries = read(path)?;
    let k = key(alias, issue_iid);
    if enabled {
        entries.shift_remove(&k);
    } else {
        entries.insert(k, json!({ "enabled": false }));
    }
    write(&entries, path)?;
    let verb = if enabled { "Enabled" } else { "Disabled" };
    Ok(format!("{verb} tracking for #{issue_iid}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_by_default_when_file_or_entry_missing() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("issue_tracking.json");
        assert!(is_issue_enabled("a", 1, &p).unwrap());
        std::fs::write(&p, "{}").unwrap();
        assert!(is_issue_enabled("a", 1, &p).unwrap());
    }

    #[test]
    fn disable_then_enable_round_trips_and_removes_entry() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub").join("issue_tracking.json");
        assert_eq!(
            set_issue_enabled("a", 7, false, &p).unwrap(),
            "Disabled tracking for #7"
        );
        assert!(!is_issue_enabled("a", 7, &p).unwrap());
        assert!(is_issue_enabled("a", 8, &p).unwrap());
        assert!(is_issue_enabled("b", 7, &p).unwrap());
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "{\n  \"a#7\": {\n    \"enabled\": false\n  }\n}"
        );
        assert_eq!(
            set_issue_enabled("a", 7, true, &p).unwrap(),
            "Enabled tracking for #7"
        );
        assert!(is_issue_enabled("a", 7, &p).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{}");
    }

    #[test]
    fn other_entries_survive_and_no_temp_file_is_left() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("issue_tracking.json");
        set_issue_enabled("a", 1, false, &p).unwrap();
        set_issue_enabled("a", 2, false, &p).unwrap();
        set_issue_enabled("a", 1, true, &p).unwrap();
        assert!(!is_issue_enabled("a", 2, &p).unwrap());
        let names: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["issue_tracking.json"]);
    }

    #[test]
    fn corrupt_file_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("issue_tracking.json");
        std::fs::write(&p, "{nope").unwrap();
        assert!(is_issue_enabled("a", 1, &p).is_err());
    }
}
