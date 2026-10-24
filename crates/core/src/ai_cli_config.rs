//! Which AI CLI (claude or codex) the loops use, from
//! `<LOOP_ENGINEERING_HOME>/ai_cli.json`. Mirrors `bin/ai_cli_config.py`: a
//! missing, corrupt or unrecognised file always falls back to `claude`.

use crate::config::home_dir;
use crate::pyjson;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const VALID_CLIS: [&str; 2] = ["claude", "codex"];

pub fn default_config_path() -> PathBuf {
    home_dir().join("ai_cli.json")
}

/// The parsed object, or an empty map when the file is missing, not JSON, or
/// not an object.
pub fn load_config(path: &Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

/// The stored `cli` if recognised, else `"claude"`.
pub fn get_selected_cli(path: &Path) -> String {
    match load_config(path).get("cli").and_then(Value::as_str) {
        Some(cli) if VALID_CLIS.contains(&cli) => cli.to_string(),
        _ => "claude".to_string(),
    }
}

/// [`get_selected_cli`] against [`default_config_path`] (resolved now).
pub fn selected_cli() -> String {
    get_selected_cli(&default_config_path())
}

/// Persist the choice; the `Ok` text is the message Python prints.
pub fn set_selected_cli(cli: &str, path: &Path) -> Result<String, String> {
    if !VALID_CLIS.contains(&cli) {
        return Err(format!("Unknown AI CLI: {cli}"));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, pyjson::to_string_pretty(&json!({ "cli": cli })))
        .map_err(|e| e.to_string())?;
    Ok(format!("Switched to {cli}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, text: &str) -> PathBuf {
        let p = dir.join("ai_cli.json");
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn defaults_to_claude_when_file_missing() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(get_selected_cli(&d.path().join("nope.json")), "claude");
    }

    #[test]
    fn reads_stored_value() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            get_selected_cli(&write(d.path(), r#"{"cli": "codex"}"#)),
            "codex"
        );
    }

    #[test]
    fn falls_back_on_invalid_corrupt_or_non_object() {
        let d = tempfile::tempdir().unwrap();
        for text in [r#"{"cli": "gpt5"}"#, "{not json", "[1]", r#"{"cli": 3}"#] {
            assert_eq!(get_selected_cli(&write(d.path(), text)), "claude", "{text}");
        }
    }

    #[test]
    fn set_writes_file_and_creates_parents() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("nested").join("ai_cli.json");
        assert_eq!(set_selected_cli("codex", &p).unwrap(), "Switched to codex");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "{\n  \"cli\": \"codex\"\n}"
        );
        assert_eq!(get_selected_cli(&p), "codex");
    }

    #[test]
    fn set_rejects_unknown_without_writing() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("ai_cli.json");
        assert_eq!(
            set_selected_cli("gpt5", &p).unwrap_err(),
            "Unknown AI CLI: gpt5"
        );
        assert!(!p.exists());
    }
}
