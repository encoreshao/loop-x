//! Which AI CLI (claude, codex, gemini, kimi, cursor or grok) the loops use, from
//! `<LOOP_ENGINEERING_HOME>/ai_cli.json`. Mirrors `bin/ai_cli_config.py`: a
//! missing, corrupt or unrecognised file always falls back to `claude`.

use crate::config::home_dir;
use crate::pyjson;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const VALID_CLIS: [&str; 6] = ["claude", "codex", "gemini", "kimi", "cursor", "grok"];

/// Executable names a CLI may be installed under, in lookup order. Cursor's
/// agent ships as `cursor-agent` and, on newer installs, also as `agent`; the
/// specific name goes first so an unrelated `agent` earlier on PATH is not run.
pub fn binary_candidates(cli: &str) -> &'static [&'static str] {
    match cli {
        "claude" => &["claude"],
        "codex" => &["codex"],
        "gemini" => &["gemini"],
        "kimi" => &["kimi"],
        "grok" => &["grok"],
        "cursor" => &["cursor-agent", "agent"],
        _ => &[],
    }
}

/// The first of [`binary_candidates`] that `path_lookup` reports as present.
pub fn resolve_binary(cli: &str, path_lookup: &dyn Fn(&str) -> bool) -> Option<String> {
    binary_candidates(cli)
        .iter()
        .find(|name| path_lookup(name))
        .map(|name| name.to_string())
}

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

/// Rewrites one key of the file, keeping the others.
fn update_config(path: &Path, key: &str, value: Option<Value>) -> Result<(), String> {
    let mut config = load_config(path);
    match value {
        Some(v) => config.insert(key.into(), v),
        None => config.remove(key),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, pyjson::to_string_pretty(&Value::Object(config)))
        .map_err(|e| e.to_string())
}

/// Persist the choice; the `Ok` text is the message Python prints.
pub fn set_selected_cli(cli: &str, path: &Path) -> Result<String, String> {
    if !VALID_CLIS.contains(&cli) {
        return Err(format!("Unknown AI CLI: {cli}"));
    }
    update_config(path, "cli", Some(json!(cli)))?;
    Ok(format!("Switched to {cli}"))
}

/// The usage threshold above which scheduled runs pause (see `quota`); `None`
/// switches it off.
pub fn set_pause_above_percent(percent: Option<i64>, path: &Path) -> Result<String, String> {
    match percent {
        Some(p) if !crate::quota::PAUSE_PERCENT_CHOICES.contains(&p) => {
            Err(format!("Unknown usage threshold: {p}"))
        }
        Some(p) => {
            update_config(path, "pause_above_percent", Some(json!(p)))?;
            Ok(format!(
                "Scheduled runs pause above {p}% of the subscription's usage"
            ))
        }
        None => {
            update_config(path, "pause_above_percent", None)?;
            Ok("Scheduled runs no longer pause on usage".into())
        }
    }
}

/// Turns moving a run to another Claude account at its usage limit on or off.
pub fn set_auto_switch_accounts(on: bool, path: &Path) -> Result<String, String> {
    update_config(path, "auto_switch_accounts", on.then(|| json!(true)))?;
    Ok(if on {
        "Runs move to another Claude account when one reaches its usage limit".into()
    } else {
        "Runs no longer move to another Claude account".into()
    })
}

fn registered_accounts(config: &serde_json::Map<String, Value>) -> Vec<Value> {
    config
        .get("claude_accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn store_accounts(path: &Path, accounts: Vec<Value>) -> Result<(), String> {
    let value = (!accounts.is_empty()).then(|| Value::Array(accounts));
    update_config(path, "claude_accounts", value)
}

/// Registers another signed-in Claude account (its `CLAUDE_CONFIG_DIR`).
pub fn add_claude_account(name: &str, config_dir: &str, path: &Path) -> Result<String, String> {
    let (name, dir) = (name.trim(), config_dir.trim());
    if name.is_empty() {
        return Err("Give the account a name".into());
    }
    if name.eq_ignore_ascii_case("default") {
        return Err(
            "\"Default\" is the account Claude Code already uses; pick another name".into(),
        );
    }
    if !(dir.starts_with('/') || dir.starts_with("~/")) {
        return Err("The config folder must be an absolute path or start with ~/".into());
    }
    let mut accounts = registered_accounts(&load_config(path));
    let text = |a: &Value, key: &str| a.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    if accounts.iter().any(|a| text(a, "name") == name) {
        return Err(format!("There is already an account named {name}"));
    }
    if accounts.iter().any(|a| text(a, "config_dir") == dir) {
        return Err("That config folder is already registered".into());
    }
    accounts.push(json!({"name": name, "config_dir": dir}));
    store_accounts(path, accounts)?;
    Ok(format!("Added the Claude account {name}"))
}

/// Forgets a registered account; the folder itself is left alone.
pub fn remove_claude_account(name: &str, path: &Path) -> Result<String, String> {
    let mut accounts = registered_accounts(&load_config(path));
    let before = accounts.len();
    accounts.retain(|a| a.get("name").and_then(Value::as_str) != Some(name));
    if accounts.len() == before {
        return Err(format!("No account named {name}"));
    }
    store_accounts(path, accounts)?;
    Ok(format!("Removed the Claude account {name}"))
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
    fn reads_the_newer_clis() {
        let d = tempfile::tempdir().unwrap();
        for cli in ["gemini", "kimi", "cursor", "grok"] {
            let text = format!(r#"{{"cli": "{cli}"}}"#);
            assert_eq!(get_selected_cli(&write(d.path(), &text)), cli);
        }
    }

    #[test]
    fn switching_cli_keeps_the_pause_threshold_and_the_threshold_can_be_cleared() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("ai_cli.json");
        assert_eq!(
            set_pause_above_percent(Some(90), &p).unwrap(),
            "Scheduled runs pause above 90% of the subscription's usage"
        );
        set_selected_cli("codex", &p).unwrap();
        assert_eq!(load_config(&p)["cli"], "codex");
        assert_eq!(load_config(&p)["pause_above_percent"], 90);
        assert!(set_pause_above_percent(Some(42), &p).is_err());
        assert_eq!(
            set_pause_above_percent(None, &p).unwrap(),
            "Scheduled runs no longer pause on usage"
        );
        assert!(!load_config(&p).contains_key("pause_above_percent"));
        assert_eq!(get_selected_cli(&p), "codex");
    }

    #[test]
    fn cursor_prefers_the_specific_binary_name() {
        assert_eq!(binary_candidates("cursor"), ["cursor-agent", "agent"]);
        assert_eq!(
            resolve_binary("cursor", &|_| true).as_deref(),
            Some("cursor-agent")
        );
    }

    #[test]
    fn cursor_resolves_to_whichever_binary_exists() {
        assert_eq!(
            resolve_binary("cursor", &|n| n == "agent").as_deref(),
            Some("agent")
        );
        assert_eq!(
            resolve_binary("cursor", &|n| n == "cursor-agent").as_deref(),
            Some("cursor-agent")
        );
        assert_eq!(resolve_binary("cursor", &|_| false), None);
        assert_eq!(
            resolve_binary("gemini", &|n| n == "gemini").as_deref(),
            Some("gemini")
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

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn auto_switch_is_stored_as_a_flag_and_removed_when_off() {
        let d = tempfile::tempdir().unwrap();
        let p = write(d.path(), r#"{"cli": "claude"}"#);
        set_auto_switch_accounts(true, &p).unwrap();
        assert_eq!(
            read(&p),
            json!({"cli": "claude", "auto_switch_accounts": true})
        );
        set_auto_switch_accounts(false, &p).unwrap();
        assert_eq!(read(&p), json!({"cli": "claude"}));
    }

    #[test]
    fn accounts_are_added_in_order_and_removed_by_name() {
        let d = tempfile::tempdir().unwrap();
        let p = write(d.path(), r#"{"cli": "claude"}"#);
        add_claude_account("work", "~/.claude-work", &p).unwrap();
        add_claude_account("  side ", " /srv/claude-side ", &p).unwrap();
        assert_eq!(
            read(&p)["claude_accounts"],
            json!([
                {"name": "work", "config_dir": "~/.claude-work"},
                {"name": "side", "config_dir": "/srv/claude-side"}
            ])
        );
        remove_claude_account("work", &p).unwrap();
        assert_eq!(
            read(&p)["claude_accounts"],
            json!([{"name": "side", "config_dir": "/srv/claude-side"}])
        );
        remove_claude_account("side", &p).unwrap();
        assert!(read(&p).get("claude_accounts").is_none());
        assert!(remove_claude_account("side", &p).is_err());
    }

    #[test]
    fn a_bad_account_is_refused_and_nothing_is_written() {
        let d = tempfile::tempdir().unwrap();
        let p = write(d.path(), r#"{"cli": "claude"}"#);
        add_claude_account("work", "/srv/a", &p).unwrap();
        for (name, dir) in [
            ("", "/srv/b"),
            ("Default", "/srv/b"),
            ("work", "/srv/b"),
            ("other", ""),
            ("other", "relative/dir"),
            ("other", "/srv/a"),
        ] {
            assert!(add_claude_account(name, dir, &p).is_err(), "{name} {dir}");
        }
        assert_eq!(read(&p)["claude_accounts"].as_array().unwrap().len(), 1);
    }
}
