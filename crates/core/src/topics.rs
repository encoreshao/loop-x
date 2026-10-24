//! Per-machine topic configuration: `~/.loopx/topics.json`, a JSON
//! array of topics. Mirrors the read side of `bin/topic_config.py`.

use crate::config::home_dir;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum TopicError {
    #[error(
        "No topic config at {0}. Copy config/topics.json.template there and list the topics you \
         want to monitor."
    )]
    Missing(PathBuf),
    #[error("cannot read {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("{0} is not valid JSON: {1}")]
    Parse(PathBuf, serde_json::Error),
    #[error("{0} must contain a JSON array of topics, got {1}")]
    NotArray(PathBuf, &'static str),
    #[error("topic entry without a string \"name\" in {0}")]
    NoName(PathBuf),
    #[error("No topic named '{0}' in {1}")]
    Unknown(String, PathBuf),
}

pub fn default_config_path() -> PathBuf {
    home_dir().join("topics.json")
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

pub fn load(path: &Path) -> Result<Vec<Value>, TopicError> {
    let text = std::fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => TopicError::Missing(path.to_path_buf()),
        _ => TopicError::Io(path.to_path_buf(), e),
    })?;
    let data: Value =
        serde_json::from_str(&text).map_err(|e| TopicError::Parse(path.to_path_buf(), e))?;
    match data {
        Value::Array(items) => Ok(items),
        other => Err(TopicError::NotArray(path.to_path_buf(), type_name(&other))),
    }
}

fn name_of<'a>(topic: &'a Value, path: &Path) -> Result<&'a str, TopicError> {
    topic
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| TopicError::NoName(path.to_path_buf()))
}

pub fn names(path: &Path) -> Result<Vec<String>, TopicError> {
    load(path)?
        .iter()
        .map(|t| name_of(t, path).map(str::to_owned))
        .collect()
}

pub fn get(path: &Path, name: &str) -> Result<Value, TopicError> {
    for topic in load(path)? {
        if name_of(&topic, path)? == name {
            return Ok(topic);
        }
    }
    Err(TopicError::Unknown(name.to_string(), path.to_path_buf()))
}

// ---- mutations (`topic_config.upsert_topic` / `rename_topic` / `set_enabled` /
// `delete_topic`) and the rename migration of the dashboard ----

fn py_repr(s: &str) -> String {
    crate::loops_config::py_repr_str(s)
}

/// `_load_topics_or_empty`: a missing file is `[]`; anything else unreadable is
/// an error message.
fn load_or_empty(path: &Path) -> Result<Vec<Value>, String> {
    match load(path) {
        Ok(items) => Ok(items),
        Err(TopicError::Missing(_)) => Ok(Vec::new()),
        Err(e) => Err(e.to_string()),
    }
}

/// `_write_topics`: JSON with 2-space indent via a same-directory temp file.
fn write_topics(topics: &[Value], path: &Path) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, crate::pyjson::to_string_pretty(&topics)).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn name_is(topic: &Value, name: &str) -> bool {
    topic.get("name").and_then(Value::as_str) == Some(name)
}

/// Add a topic or update one in place; an edit keeps the existing `enabled`.
pub fn upsert_topic(
    name: &str,
    label: &str,
    brief: &str,
    slack_bundle: &str,
    path: &Path,
) -> (bool, String) {
    let (name, label, brief, slack_bundle) =
        (name.trim(), label.trim(), brief.trim(), slack_bundle.trim());
    if name.is_empty() {
        return (false, "Topic name is required".into());
    }
    if label.is_empty() {
        return (false, "Label is required".into());
    }
    if brief.is_empty() {
        return (false, "Brief is required".into());
    }
    let mut topics = match load_or_empty(path) {
        Ok(t) => t,
        Err(e) => return (false, e),
    };
    let existing = topics.iter().position(|t| name_is(t, name));
    let enabled = existing
        .and_then(|i| topics[i].get("enabled").cloned())
        .unwrap_or(Value::Bool(true));
    let entry = serde_json::json!({
        "name": name,
        "label": label,
        "brief": brief,
        "slack_bundle": if slack_bundle.is_empty() { Value::Null } else { Value::String(slack_bundle.to_string()) },
        "enabled": enabled,
    });
    match existing {
        Some(i) => topics[i] = entry,
        None => topics.push(entry),
    }
    if let Err(e) = write_topics(&topics, path) {
        return (false, format!("cannot write {}: {e}", path.display()));
    }
    (
        true,
        format!(
            "{} topic {name}",
            if existing.is_none() {
                "Added"
            } else {
                "Updated"
            }
        ),
    )
}

/// Rename a topic's identity in `topics.json` only (see [`migrate_topic_rename`]).
pub fn rename_topic(old_name: &str, new_name: &str, path: &Path) -> (bool, String) {
    let (old_name, new_name) = (old_name.trim(), new_name.trim());
    if new_name.is_empty() {
        return (false, "Topic name is required".into());
    }
    if old_name == new_name {
        return (true, "No change to topic name".into());
    }
    let mut topics = match load_or_empty(path) {
        Ok(t) => t,
        Err(e) => return (false, e),
    };
    if topics.iter().any(|t| name_is(t, new_name)) {
        return (
            false,
            format!("A topic named {} already exists", py_repr(new_name)),
        );
    }
    let Some(topic) = topics.iter_mut().find(|t| name_is(t, old_name)) else {
        return (
            false,
            format!("No topic named {} in {}", py_repr(old_name), path.display()),
        );
    };
    topic["name"] = Value::String(new_name.to_string());
    if let Err(e) = write_topics(&topics, path) {
        return (false, format!("cannot write {}: {e}", path.display()));
    }
    (true, format!("Renamed topic {old_name} to {new_name}"))
}

/// Flip one topic's `enabled`.
pub fn set_enabled(name: &str, enabled: bool, path: &Path) -> (bool, String) {
    let mut topics = match load_or_empty(path) {
        Ok(t) => t,
        Err(e) => return (false, e),
    };
    let Some(topic) = topics.iter_mut().find(|t| name_is(t, name)) else {
        return (
            false,
            format!("No topic named {} in {}", py_repr(name), path.display()),
        );
    };
    topic["enabled"] = Value::Bool(enabled);
    if let Err(e) = write_topics(&topics, path) {
        return (false, format!("cannot write {}: {e}", path.display()));
    }
    (
        true,
        format!(
            "{} topic {name}",
            if enabled { "Enabled" } else { "Disabled" }
        ),
    )
}

pub fn delete_topic(name: &str, path: &Path) -> (bool, String) {
    let topics = match load_or_empty(path) {
        Ok(t) => t,
        Err(e) => return (false, e),
    };
    let remaining: Vec<Value> = topics
        .iter()
        .filter(|t| !name_is(t, name))
        .cloned()
        .collect();
    if remaining.len() == topics.len() {
        return (false, format!("Unknown topic: {name}"));
    }
    if let Err(e) = write_topics(&remaining, path) {
        return (false, format!("cannot write {}: {e}", path.display()));
    }
    (true, format!("Deleted topic {name}"))
}

/// `list_topic_history`: the `<date>-<topic>.md` names in `history_dir`, newest
/// first; only that topic's when `topic` is given (an exact `YYYY-MM-DD-` prefix,
/// so `news` never matches `ai-news`).
pub fn list_topic_history(history_dir: &Path, topic: Option<&str>) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(history_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".md") && n.len() > 3)
        .filter(|n| match topic {
            None => true,
            Some(t) => {
                let b = n.as_bytes();
                let suffix = format!("-{t}.md");
                n.len() == 10 + suffix.len()
                    && n.ends_with(&suffix)
                    && b[..10].iter().enumerate().all(|(i, c)| match i {
                        4 | 7 => *c == b'-',
                        _ => c.is_ascii_digit(),
                    })
            }
        })
        .collect();
    names.sort_by(|a, b| b.cmp(a));
    names
}

/// `_migrate_topic_rename`: move a renamed topic's saved briefings, its dedup
/// state file and its status entry to the new name. Each piece is best effort.
pub fn migrate_topic_rename(
    old_name: &str,
    new_name: &str,
    status_path: &Path,
    history_dir: &Path,
    state_dir: &Path,
) {
    let old_suffix = format!("{old_name}.md");
    for filename in list_topic_history(history_dir, Some(old_name)) {
        let stem = &filename[..filename.len() - old_suffix.len()];
        let _ = std::fs::rename(
            history_dir.join(&filename),
            history_dir.join(format!("{stem}{new_name}.md")),
        );
    }
    let base = |n: &str| {
        Path::new(n)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let old_state = state_dir.join(format!("{}.json", base(old_name)));
    if old_state.exists() {
        let _ = std::fs::rename(
            &old_state,
            state_dir.join(format!("{}.json", base(new_name))),
        );
    }
    let mut data = crate::status::read_topic_status(status_path);
    let moved = data
        .get_mut("topics")
        .and_then(Value::as_object_mut)
        .and_then(|t| t.shift_remove(old_name).map(|v| (t, v)));
    if let Some((topics, value)) = moved {
        topics.insert(new_name.to_string(), value);
        if let Some(dir) = status_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(status_path, crate::pyjson::to_string_pretty(&data));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(body: &str) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("topics.json");
        std::fs::write(&p, body).unwrap();
        (d, p)
    }

    #[test]
    fn names_and_get() {
        let (_d, p) = file(r#"[{"name":"a","label":"A"},{"name":"b"}]"#);
        assert_eq!(names(&p).unwrap(), vec!["a", "b"]);
        assert_eq!(get(&p, "a").unwrap()["label"], "A");
        assert!(matches!(get(&p, "zzz"), Err(TopicError::Unknown(..))));
    }

    #[test]
    fn rejects_non_array() {
        let (_d, p) = file(r#"{"a":1}"#);
        let err = load(&p).unwrap_err().to_string();
        assert!(err.contains("must contain a JSON array of topics, got dict"));
    }

    #[test]
    fn upsert_rename_enable_delete() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub").join("topics.json");
        assert_eq!(
            upsert_topic(" ", "L", "B", "", &p),
            (false, "Topic name is required".into())
        );
        assert_eq!(
            upsert_topic("a", "", "B", "", &p),
            (false, "Label is required".into())
        );
        assert_eq!(
            upsert_topic("a", "L", "", "", &p),
            (false, "Brief is required".into())
        );
        assert_eq!(
            upsert_topic("a", "L", "B", "", &p),
            (true, "Added topic a".into())
        );
        let first = std::fs::read_to_string(&p).unwrap();
        assert!(first.contains("\"slack_bundle\": null") && first.contains("\"enabled\": true"));
        assert_eq!(
            set_enabled("a", false, &p),
            (true, "Disabled topic a".into())
        );
        // An edit keeps enabled=false.
        assert_eq!(
            upsert_topic("a", "L2", "B2", "x", &p),
            (true, "Updated topic a".into())
        );
        let t = get(&p, "a").unwrap();
        assert_eq!(
            (t["enabled"].clone(), t["slack_bundle"].clone()),
            (false.into(), "x".into())
        );
        upsert_topic("b", "L", "B", "", &p);
        assert_eq!(
            rename_topic("a", "b", &p),
            (false, "A topic named 'b' already exists".into())
        );
        assert_eq!(
            rename_topic("zz", "c", &p).1,
            format!("No topic named 'zz' in {}", p.display())
        );
        assert_eq!(
            rename_topic("a", "a", &p),
            (true, "No change to topic name".into())
        );
        assert_eq!(
            rename_topic("a", "c", &p),
            (true, "Renamed topic a to c".into())
        );
        assert_eq!(names(&p).unwrap(), vec!["c", "b"]);
        assert_eq!(
            delete_topic("nope", &p),
            (false, "Unknown topic: nope".into())
        );
        assert_eq!(delete_topic("c", &p), (true, "Deleted topic c".into()));
        assert!(!set_enabled("c", true, &p).0);
        assert_eq!(names(&p).unwrap(), vec!["b"]);
    }

    #[test]
    fn history_listing_is_anchored_and_migration_moves_everything() {
        let d = tempfile::tempdir().unwrap();
        let hist = d.path().join("history");
        let state = d.path().join("state");
        std::fs::create_dir_all(&hist).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        for n in [
            "2026-05-01-news.md",
            "2026-05-02-news.md",
            "2026-05-02-ai-news.md",
            "notes.txt",
        ] {
            std::fs::write(hist.join(n), "x").unwrap();
        }
        assert_eq!(
            list_topic_history(&hist, Some("news")),
            vec!["2026-05-02-news.md", "2026-05-01-news.md"]
        );
        assert_eq!(list_topic_history(&hist, None).len(), 3);
        std::fs::write(state.join("news.json"), "[]").unwrap();
        let status = d.path().join("status.json");
        std::fs::write(
            &status,
            r#"{"topics": {"news": {"state": "idle"}, "other": {}}}"#,
        )
        .unwrap();
        migrate_topic_rename("news", "headlines", &status, &hist, &state);
        assert!(
            hist.join("2026-05-02-headlines.md").exists()
                && !hist.join("2026-05-02-news.md").exists()
        );
        assert!(hist.join("2026-05-02-ai-news.md").exists());
        assert!(state.join("headlines.json").exists() && !state.join("news.json").exists());
        let data: Value = serde_json::from_str(&std::fs::read_to_string(&status).unwrap()).unwrap();
        assert_eq!(data["topics"]["headlines"]["state"], "idle");
        assert!(data["topics"].get("news").is_none() && data["topics"].get("other").is_some());
    }
}
