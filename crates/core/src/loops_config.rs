//! The registry of scheduled loops (`<LOOP_ENGINEERING_HOME>/loops.json`).
//! Mirrors `bin/loops_config.py`, including its quirks: template loops the
//! user's file lacks are appended on read (and persisted by the next write),
//! renamed loops are migrated on load, and the error texts match.
//!
//! Entries stay [`serde_json::Value`] objects: the registry is user-edited JSON
//! with optional keys (`notify`, `settings`, ...) that other tools add, and a
//! write must round-trip keys this module doesn't know about.

use crate::config::home_dir;
use crate::pyjson;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const MAX_SETTING_CHARS: usize = 1000;
const INTERVAL_MINUTES_CHOICES: [i64; 3] = [15, 30, 45];

#[derive(Debug, thiserror::Error)]
pub enum LoopsError {
    /// Python `FileNotFoundError`.
    #[error("{0}")]
    NotFound(String),
    /// Python `ValueError` (bad loop name).
    #[error("{0}")]
    Invalid(String),
    /// Python `KeyError`: `str()` of it is the repr of the message.
    #[error("\"{0}\"")]
    UnknownLoop(String),
    #[error("{0}")]
    Json(String),
    #[error("{0}")]
    Io(String),
}

pub fn default_config_path() -> PathBuf {
    home_dir().join("loops.json")
}

/// `config/loops.json.template` of this checkout: `LOOP_LOOPS_TEMPLATE` if set,
/// else found by walking up from the running binary (`target/debug/loopx` and
/// an installed `<repo>/bin/loopx` both resolve), else a path that doesn't
/// exist (nothing to backfill).
pub fn default_template_path() -> PathBuf {
    if let Some(p) = std::env::var_os("LOOP_LOOPS_TEMPLATE").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    let rel = Path::new("config").join("loops.json.template");
    let mut dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    for _ in 0..5 {
        let Some(d) = dir else { break };
        if d.join(&rel).is_file() {
            return d.join(&rel);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    PathBuf::from("/nonexistent/config/loops.json.template")
}

fn valid_loop_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn valid_connector_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.len() <= 48
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn name_of(entry: &Value) -> Option<&str> {
    entry.get("name").and_then(Value::as_str)
}

fn unknown(name: &str) -> String {
    format!("No loop named {} in the loops registry", py_repr_str(name))
}

/// Python `repr(str)`.
pub fn py_repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python `repr()` of a JSON-shaped value.
pub fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) if n.is_f64() => format!("{:?}", n.as_f64().unwrap_or_default()),
        Value::Number(n) => n.to_string(),
        Value::String(s) => py_repr_str(s),
        Value::Array(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{}: {}", py_repr_str(k), py_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Python `str()` of a JSON-shaped value (strings unquoted).
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_repr(other),
    }
}

/// Python truthiness.
pub fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `shlex.quote`.
pub fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

// Loops renamed after release: (old, new, label renames).
type Rename = (
    &'static str,
    &'static str,
    [(&'static str, &'static str); 1],
);
const RENAMED_LOOPS: [Rename; 1] = [(
    "calendar-prep-loop",
    "meeting-prep-loop",
    [("Calendar Prep", "Meeting Prep")],
)];

fn migrate_renamed(loops: Vec<Value>) -> Vec<Value> {
    let names: Vec<Option<String>> = loops
        .iter()
        .map(|l| name_of(l).map(str::to_owned))
        .collect();
    let mut out = Vec::new();
    for mut entry in loops {
        let old = name_of(&entry).map(str::to_owned);
        if let Some((old, new, labels)) = RENAMED_LOOPS
            .iter()
            .find(|(o, _, _)| Some(*o) == old.as_deref())
        {
            if names.iter().any(|n| n.as_deref() == Some(*new)) {
                continue; // the new entry already exists: the stale one is dropped
            }
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("name".into(), json!(new));
                let label = obj.get("label").cloned();
                match label {
                    Some(Value::String(l)) => {
                        let renamed = labels
                            .iter()
                            .find(|(from, _)| *from == l)
                            .map_or(l.clone(), |(_, to)| (*to).to_string());
                        obj.insert("label".into(), json!(renamed));
                    }
                    Some(other) => {
                        obj.insert("label".into(), other);
                    }
                    None => {}
                }
                if let Some(Value::String(suffix)) = obj.get("log_suffix").cloned() {
                    obj.insert("log_suffix".into(), json!(suffix.replace(old, new)));
                }
            }
        }
        out.push(entry);
    }
    out
}

fn read_json_array(path: &Path) -> Result<Vec<Value>, LoopsError> {
    let text = std::fs::read_to_string(path).map_err(|e| LoopsError::Io(e.to_string()))?;
    match serde_json::from_str::<Value>(&text).map_err(|e| LoopsError::Json(e.to_string()))? {
        Value::Array(a) => Ok(a),
        _ => Err(LoopsError::Json(format!(
            "{} must contain a JSON array",
            path.display()
        ))),
    }
}

/// The full registry in file order, then any template loop not in the file.
pub fn list_loops(
    config_path: &Path,
    template_path: Option<&Path>,
) -> Result<Vec<Value>, LoopsError> {
    if !config_path.exists() {
        return Err(LoopsError::NotFound(format!(
            "No loops config at {}. Copy config/loops.json.template there \
             (bin/scripts/setup.sh does this automatically).",
            config_path.display()
        )));
    }
    let loops = migrate_renamed(read_json_array(config_path)?);
    let template = template_path.map_or_else(default_template_path, Path::to_path_buf);
    let missing = missing_template_loops(&loops, &template);
    Ok(loops.into_iter().chain(missing).collect())
}

fn missing_template_loops(loops: &[Value], template_path: &Path) -> Vec<Value> {
    let Ok(template) = read_json_array(template_path) else {
        return Vec::new();
    };
    template
        .into_iter()
        .filter(|entry| !loops.iter().any(|l| l.get("name") == entry.get("name")))
        .collect()
}

/// One registry entry by name.
pub fn get_loop(
    name: &str,
    config_path: &Path,
    template_path: Option<&Path>,
) -> Result<Value, LoopsError> {
    if !valid_loop_name(name) {
        return Err(LoopsError::Invalid(format!(
            "Invalid loop name {} - loop names may only contain letters, digits, '.', '_', and '-'.",
            py_repr_str(name)
        )));
    }
    list_loops(config_path, template_path)?
        .into_iter()
        .find(|l| name_of(l) == Some(name))
        .ok_or_else(|| LoopsError::UnknownLoop(unknown(name)))
}

/// Atomic write (temp file in the same directory, then rename).
pub fn write_loops(loops: &[Value], config_path: &Path) -> Result<(), LoopsError> {
    let io = |e: std::io::Error| LoopsError::Io(e.to_string());
    if let Some(dir) = config_path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let file = config_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let tmp = config_path.with_file_name(format!(".{file}.tmp"));
    std::fs::write(&tmp, pyjson::to_string_pretty(&loops)).map_err(io)?;
    std::fs::rename(&tmp, config_path).map_err(io)
}

type Outcome = Result<String, String>;

fn with_loop(
    name: &str,
    config_path: &Path,
    template_path: Option<&Path>,
    change: impl FnOnce(&mut Map<String, Value>) -> Result<String, String>,
) -> Outcome {
    let mut loops = list_loops(config_path, template_path).map_err(|e| e.to_string())?;
    let Some(obj) = loops
        .iter_mut()
        .find(|l| name_of(l) == Some(name))
        .and_then(Value::as_object_mut)
    else {
        return Err(unknown(name));
    };
    let message = change(obj)?;
    write_loops(&loops, config_path).map_err(|e| e.to_string())?;
    Ok(message)
}

/// Flip the entry's `enabled`. `Err` (and nothing written) for an unknown loop.
pub fn set_enabled(
    name: &str,
    enabled: bool,
    config_path: &Path,
    template_path: Option<&Path>,
) -> Outcome {
    with_loop(name, config_path, template_path, |loop_| {
        loop_.insert("enabled".into(), json!(enabled));
        Ok(format!(
            "{} {name}",
            if enabled { "Enabled" } else { "Disabled" }
        ))
    })
}

/// Set the loop's `notify` connector-id list (empty removes the key).
pub fn set_notify(
    name: &str,
    ids: &Value,
    config_path: &Path,
    template_path: Option<&Path>,
) -> Outcome {
    let Value::Array(ids) = ids else {
        return Err("notify must be a list of connector ids".into());
    };
    for id in ids {
        if !id.as_str().is_some_and(valid_connector_id) {
            return Err(format!("Invalid connector id {}", py_repr(id)));
        }
    }
    with_loop(name, config_path, template_path, |loop_| {
        if ids.is_empty() {
            loop_.shift_remove("notify");
        } else {
            loop_.insert("notify".into(), Value::Array(ids.clone()));
        }
        Ok(format!("Updated notify for {name}"))
    })
}

/// Merge string values for `allowed_keys` into the loop's `settings` dict
/// (each truncated to [`MAX_SETTING_CHARS`]); other settings are kept.
pub fn set_settings(
    name: &str,
    values: &Value,
    allowed_keys: &[&str],
    config_path: &Path,
    template_path: Option<&Path>,
) -> Outcome {
    let Value::Object(values) = values else {
        return Err("settings must be an object".into());
    };
    let clean: Vec<(String, String)> = values
        .iter()
        .filter(|(k, _)| allowed_keys.contains(&k.as_str()))
        .filter_map(|(k, v)| {
            v.as_str()
                .map(|s| (k.clone(), s.chars().take(MAX_SETTING_CHARS).collect()))
        })
        .collect();
    with_loop(name, config_path, template_path, |loop_| {
        let mut merged = match loop_.get("settings") {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        for (k, v) in clean {
            merged.insert(k, Value::String(v));
        }
        loop_.insert("settings".into(), Value::Object(merged));
        Ok(format!("Updated settings for {name}"))
    })
}

/// Drop connector `old_id` from every loop's `notify` list, or rename it to
/// `new_id`, without duplicates; an emptied list is removed. Touches only the
/// user's own file (no template backfill) and writes only on a change.
pub fn replace_notify_id(
    old_id: &str,
    new_id: Option<&str>,
    config_path: &Path,
) -> Result<usize, LoopsError> {
    let mut loops = match read_json_array(config_path) {
        Ok(l) => l,
        Err(LoopsError::Io(_)) if !config_path.exists() => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut changed = 0;
    for entry in &mut loops {
        let Some(obj) = entry.as_object_mut() else {
            continue;
        };
        let Some(Value::Array(ids)) = obj.get("notify") else {
            continue;
        };
        if !ids.iter().any(|i| i.as_str() == Some(old_id)) {
            continue;
        }
        let mut updated: Vec<Value> = Vec::new();
        for id in ids {
            let id = if id.as_str() == Some(old_id) {
                new_id.map(|n| json!(n))
            } else {
                Some(id.clone())
            };
            if let Some(id) = id {
                if !updated.contains(&id) {
                    updated.push(id);
                }
            }
        }
        if updated.is_empty() {
            obj.shift_remove("notify");
        } else {
            obj.insert("notify".into(), Value::Array(updated));
        }
        changed += 1;
    }
    if changed > 0 {
        write_loops(&loops, config_path)?;
    }
    Ok(changed)
}

fn required_fields(frequency: &str) -> Option<&'static [&'static str]> {
    Some(match frequency {
        "daily" => &["hour", "minute"],
        "weekly" => &["weekdays", "hour", "minute"],
        "monthly" => &["day", "hour", "minute"],
        "hourly" => &["interval_hours"],
        _ => return None,
    })
}

const FIELD_BOUNDS: [(&str, i64, i64); 4] = [
    ("hour", 0, 23),
    ("minute", 0, 59),
    ("day", 1, 31),
    ("interval_hours", 1, 24),
];

fn int_value(v: &Value) -> Option<i64> {
    if v.is_f64() {
        None
    } else {
        v.as_i64()
    }
}

/// Validate a `{"frequency": ...}` schedule and write it onto the loop.
pub fn set_schedule(
    name: &str,
    schedule: &Value,
    config_path: &Path,
    template_path: Option<&Path>,
) -> Outcome {
    let write = || {
        with_loop(name, config_path, template_path, |loop_| {
            loop_.insert("schedule".into(), schedule.clone());
            Ok(format!("Updated schedule for {name}"))
        })
    };
    let Some(sched) = schedule.as_object() else {
        return Err("Unknown schedule frequency: None".into());
    };
    let frequency = sched.get("frequency").cloned().unwrap_or(Value::Null);
    let Some(required) = frequency.as_str().and_then(required_fields) else {
        return Err(format!(
            "Unknown schedule frequency: {}",
            py_repr(&frequency)
        ));
    };
    let frequency = frequency.as_str().unwrap_or_default();
    if frequency == "hourly" && sched.contains_key("interval_minutes") {
        let minutes = &sched["interval_minutes"];
        if sched.contains_key("interval_hours") {
            return Err("Give either interval_hours or interval_minutes, not both".into());
        }
        if !int_value(minutes).is_some_and(|m| INTERVAL_MINUTES_CHOICES.contains(&m)) {
            return Err(format!(
                "interval_minutes must be one of 15, 30, 45, got {}",
                py_repr(minutes)
            ));
        }
        return write();
    }
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|f| !sched.contains_key(*f))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "Schedule for {} is missing: {}",
            py_repr_str(frequency),
            missing.join(", ")
        ));
    }
    for (field, lo, hi) in FIELD_BOUNDS {
        if required.contains(&field) {
            if let Some(value) = sched.get(field) {
                if !int_value(value).is_some_and(|v| (lo..=hi).contains(&v)) {
                    return Err(format!(
                        "{field} must be an integer between {lo} and {hi}, got {}",
                        py_repr(value)
                    ));
                }
            }
        }
    }
    if frequency == "weekly" {
        let weekdays = &sched["weekdays"];
        let all = weekdays.as_str() == Some("all");
        let list_ok = weekdays.as_array().is_some_and(|a| {
            a.iter()
                .all(|d| int_value(d).is_some_and(|d| (1..=7).contains(&d)))
        });
        if !all && !list_ok {
            return Err(format!(
                "weekdays must be \"all\" or a list of integers 1-7, got {}",
                py_repr(weekdays)
            ));
        }
    }
    write()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fx {
        _dir: tempfile::TempDir,
        config: PathBuf,
        none: PathBuf,
    }

    fn fx(entries: Value) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("loops.json");
        std::fs::write(&config, entries.to_string()).unwrap();
        Fx {
            config,
            none: dir.path().join("no-template.json"),
            _dir: dir,
        }
    }

    fn read(f: &Fx) -> Vec<Value> {
        read_json_array(&f.config).unwrap()
    }

    fn two() -> Value {
        json!([
            {"name": "gitlab-loop", "entry_point": "bin.gitlab_loop_runner", "enabled": true},
            {"name": "topic-loop", "entry_point": "bin.topic_monitor_runner"},
        ])
    }

    #[test]
    fn missing_file_is_not_found() {
        let d = tempfile::tempdir().unwrap();
        let e = list_loops(&d.path().join("loops.json"), Some(&d.path().join("t"))).unwrap_err();
        assert!(matches!(e, LoopsError::NotFound(_)));
        assert!(e.to_string().contains("No loops config at"));
    }

    #[test]
    fn lists_in_order_and_gets_by_name() {
        let f = fx(two());
        let all = list_loops(&f.config, Some(&f.none)).unwrap();
        assert_eq!(Value::Array(all), two());
        let l = get_loop("topic-loop", &f.config, Some(&f.none)).unwrap();
        assert_eq!(l["entry_point"], "bin.topic_monitor_runner");
    }

    #[test]
    fn unknown_name_is_key_error_and_bad_name_is_value_error() {
        let f = fx(two());
        let e = get_loop("nope", &f.config, Some(&f.none)).unwrap_err();
        assert!(matches!(e, LoopsError::UnknownLoop(_)));
        assert_eq!(
            e.to_string(),
            "\"No loop named 'nope' in the loops registry\""
        );
        for bad in ["../x", "a b", ""] {
            assert!(matches!(
                get_loop(bad, &f.config, Some(&f.none)).unwrap_err(),
                LoopsError::Invalid(_)
            ));
        }
    }

    #[test]
    fn template_loops_are_backfilled_after_user_entries() {
        let f = fx(two());
        let template = f.none.with_file_name("template.json");
        std::fs::write(
            &template,
            json!([{"name": "gitlab-loop", "x": 1}, {"name": "new-loop", "enabled": false}])
                .to_string(),
        )
        .unwrap();
        let all = list_loops(&f.config, Some(&template)).unwrap();
        let names: Vec<_> = all.iter().map(|l| l["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["gitlab-loop", "topic-loop", "new-loop"]);
        assert!(all[0].get("x").is_none(), "the user's own entry wins");
    }

    #[test]
    fn unreadable_template_means_nothing_to_backfill() {
        let f = fx(two());
        std::fs::write(&f.none, "{broken").unwrap();
        assert_eq!(list_loops(&f.config, Some(&f.none)).unwrap().len(), 2);
    }

    #[test]
    fn renamed_loop_is_migrated_or_dropped() {
        let f = fx(
            json!([{"name": "calendar-prep-loop", "label": "Calendar Prep", "log_suffix": "-calendar-prep-loop"}]),
        );
        let l = &list_loops(&f.config, Some(&f.none)).unwrap()[0];
        assert_eq!(l["name"], "meeting-prep-loop");
        assert_eq!(l["label"], "Meeting Prep");
        assert_eq!(l["log_suffix"], "-meeting-prep-loop");
        let f = fx(json!([{"name": "calendar-prep-loop"}, {"name": "meeting-prep-loop"}]));
        let all = list_loops(&f.config, Some(&f.none)).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0]["name"], "meeting-prep-loop");
    }

    #[test]
    fn set_enabled_flips_and_unknown_writes_nothing() {
        let f = fx(json!([{"name": "a"}]));
        assert_eq!(
            set_enabled("a", false, &f.config, Some(&f.none)).unwrap(),
            "Disabled a"
        );
        assert_eq!(read(&f)[0]["enabled"], false);
        assert_eq!(
            set_enabled("a", true, &f.config, Some(&f.none)).unwrap(),
            "Enabled a"
        );
        let before = std::fs::read_to_string(&f.config).unwrap();
        assert_eq!(
            set_enabled("zzz", true, &f.config, Some(&f.none)).unwrap_err(),
            "No loop named 'zzz' in the loops registry"
        );
        assert_eq!(std::fs::read_to_string(&f.config).unwrap(), before);
    }

    #[test]
    fn set_notify_validates_and_empty_removes() {
        let f = fx(json!([{"name": "a", "notify": ["x"]}]));
        let t = Some(f.none.as_path());
        assert_eq!(
            set_notify("a", &json!(["slack-default", "g2"]), &f.config, t).unwrap(),
            "Updated notify for a"
        );
        assert_eq!(read(&f)[0]["notify"], json!(["slack-default", "g2"]));
        assert_eq!(
            set_notify("a", &json!(["Bad Id"]), &f.config, t).unwrap_err(),
            "Invalid connector id 'Bad Id'"
        );
        assert_eq!(
            set_notify("a", &json!("x"), &f.config, t).unwrap_err(),
            "notify must be a list of connector ids"
        );
        set_notify("a", &json!([]), &f.config, t).unwrap();
        assert!(read(&f)[0].get("notify").is_none());
        assert!(set_notify("nope", &json!([]), &f.config, t).is_err());
    }

    #[test]
    fn set_settings_filters_truncates_and_merges() {
        let f = fx(json!([{"name": "a", "settings": {"keep": "1", "q": "old"}}]));
        let t = Some(f.none.as_path());
        let long = "x".repeat(1500);
        set_settings(
            "a",
            &json!({"q": long, "n": 5, "other": "dropped", "k": "v"}),
            &["q", "n", "k"],
            &f.config,
            t,
        )
        .unwrap();
        let s = &read(&f)[0]["settings"];
        assert_eq!(s["keep"], "1");
        assert_eq!(s["q"].as_str().unwrap().len(), 1000);
        assert_eq!(s["k"], "v");
        assert!(s.get("n").is_none() && s.get("other").is_none());
        assert_eq!(
            set_settings("a", &json!([]), &["q"], &f.config, t).unwrap_err(),
            "settings must be an object"
        );
    }

    #[test]
    fn replace_notify_id_renames_dedups_and_removes() {
        let f = fx(json!([
            {"name": "a", "notify": ["old", "new"]},
            {"name": "b", "notify": ["old"]},
            {"name": "c", "notify": ["other"]},
        ]));
        assert_eq!(replace_notify_id("old", Some("new"), &f.config).unwrap(), 2);
        let l = read(&f);
        assert_eq!(l[0]["notify"], json!(["new"]));
        assert_eq!(l[1]["notify"], json!(["new"]));
        assert_eq!(l[2]["notify"], json!(["other"]));
        assert_eq!(replace_notify_id("new", None, &f.config).unwrap(), 2);
        assert!(read(&f)[0].get("notify").is_none());
        let before = std::fs::read_to_string(&f.config).unwrap();
        assert_eq!(replace_notify_id("ghost", None, &f.config).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&f.config).unwrap(), before);
        assert_eq!(replace_notify_id("x", None, &f.none).unwrap(), 0);
    }

    #[test]
    fn set_schedule_accepts_the_four_shapes() {
        let f = fx(json!([{"name": "a"}]));
        for s in [
            json!({"frequency": "daily", "hour": 9, "minute": 30}),
            json!({"frequency": "weekly", "weekdays": [1, 5], "hour": 0, "minute": 0}),
            json!({"frequency": "weekly", "weekdays": "all", "hour": 23, "minute": 59}),
            json!({"frequency": "monthly", "day": 31, "hour": 1, "minute": 2}),
            json!({"frequency": "hourly", "interval_hours": 6}),
            json!({"frequency": "hourly", "interval_minutes": 15}),
        ] {
            assert_eq!(
                set_schedule("a", &s, &f.config, Some(&f.none)).unwrap(),
                "Updated schedule for a"
            );
            assert_eq!(read(&f)[0]["schedule"], s);
        }
    }

    #[test]
    fn set_schedule_rejects_bad_input_without_writing() {
        let f = fx(json!([{"name": "a"}]));
        let cases = [
            (
                json!({"frequency": "yearly"}),
                "Unknown schedule frequency: 'yearly'",
            ),
            (json!({}), "Unknown schedule frequency: None"),
            (
                json!({"frequency": "daily", "hour": 9}),
                "Schedule for 'daily' is missing: minute",
            ),
            (
                json!({"frequency": "daily", "hour": 24, "minute": 0}),
                "hour must be an integer between 0 and 23, got 24",
            ),
            (
                json!({"frequency": "daily", "hour": 1.5, "minute": 0}),
                "hour must be an integer between 0 and 23, got 1.5",
            ),
            (
                json!({"frequency": "monthly", "day": 0, "hour": 1, "minute": 0}),
                "day must be an integer between 1 and 31, got 0",
            ),
            (
                json!({"frequency": "weekly", "weekdays": [0], "hour": 1, "minute": 0}),
                "weekdays must be \"all\" or a list of integers 1-7, got [0]",
            ),
            (
                json!({"frequency": "hourly", "interval_hours": 25}),
                "interval_hours must be an integer between 1 and 24, got 25",
            ),
            (
                json!({"frequency": "hourly", "interval_minutes": 20}),
                "interval_minutes must be one of 15, 30, 45, got 20",
            ),
            (
                json!({"frequency": "hourly", "interval_minutes": 15, "interval_hours": 1}),
                "Give either interval_hours or interval_minutes, not both",
            ),
        ];
        for (s, msg) in cases {
            assert_eq!(
                set_schedule("a", &s, &f.config, Some(&f.none)).unwrap_err(),
                msg
            );
        }
        assert!(read(&f)[0].get("schedule").is_none());
        let ok = json!({"frequency": "daily", "hour": 1, "minute": 1});
        assert!(set_schedule("nope", &ok, &f.config, Some(&f.none)).is_err());
    }

    #[test]
    fn write_is_python_formatted_and_leaves_no_temp() {
        let f = fx(json!([]));
        write_loops(&[json!({"name": "é"})], &f.config).unwrap();
        assert_eq!(
            std::fs::read_to_string(&f.config).unwrap(),
            "[\n  {\n    \"name\": \"\\u00e9\"\n  }\n]"
        );
        assert_eq!(
            std::fs::read_dir(f.config.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn python_helpers() {
        assert_eq!(shell_quote("abc-1.2"), "abc-1.2");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(
            py_repr(&json!({"a": [1, 2.0, null, true, "x'y"]})),
            "{'a': [1, 2.0, None, True, \"x'y\"]}"
        );
        assert!(!py_truthy(&json!(0)) && py_truthy(&json!("x")) && !py_truthy(&json!([])));
    }
}
