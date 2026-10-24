//! Which chat write tools skip the Approve card (port of `settings.py`).
//! Empty by default: every write waits for a human click. The file
//! `<home_dir>/chat_tools.json` is changed only by the dashboard's
//! CSRF-guarded settings route.

use super::{write_tool_names, Env};
use crate::pyjson::to_string_pretty;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

/// A hand-written, secret-free message.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SettingsError {
    #[error("{0} is not a chat write tool")]
    NotWriteTool(String),
    /// The file could not be written (Python raises `OSError`).
    #[error("{0}")]
    Io(String),
}

/// `<home_dir>/chat_tools.json`.
pub fn settings_path(env: &Env) -> PathBuf {
    env.home_dir.join("chat_tools.json")
}

/// Names of write tools the user chose to run without review. A missing or
/// corrupt file, or an entry that is not a known write tool, means review.
pub fn auto_approved(env: &Env) -> BTreeSet<String> {
    auto_approved_in(env, &write_tool_names().into_iter().collect())
}

pub(crate) fn auto_approved_in(env: &Env, write_tools: &BTreeSet<String>) -> BTreeSet<String> {
    let Ok(text) = fs::read_to_string(settings_path(env)) else {
        return BTreeSet::new();
    };
    let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&text) else {
        return BTreeSet::new();
    };
    let Some(Value::Array(names)) = m.get("auto_approve") else {
        return BTreeSet::new();
    };
    names
        .iter()
        .filter_map(Value::as_str)
        .filter(|n| write_tools.contains(*n))
        .map(str::to_string)
        .collect()
}

/// Turns review off (`enabled`) or back on for one write tool.
pub fn set_auto_approve(env: &Env, tool: &str, enabled: bool) -> Result<(), SettingsError> {
    if !write_tool_names().iter().any(|n| n == tool) {
        return Err(SettingsError::NotWriteTool(tool.to_string()));
    }
    let mut chosen = auto_approved(env);
    if enabled {
        chosen.insert(tool.to_string());
    } else {
        chosen.remove(tool);
    }
    save(env, &chosen)
}

/// Replaces the whole list (every name must be a known write tool). Written
/// atomically (temp file, then rename) with mode 0600, `indent=2`, sorted.
pub fn save<I, S>(env: &Env, names: I) -> Result<(), SettingsError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let names: BTreeSet<String> = names.into_iter().map(|s| s.as_ref().to_string()).collect();
    let known = write_tool_names();
    if let Some(bad) = names.iter().find(|n| !known.contains(n)) {
        return Err(SettingsError::NotWriteTool(bad.clone()));
    }
    let io = |e: std::io::Error| SettingsError::Io(e.to_string());
    let path = settings_path(env);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io)?;
    }
    let tmp = path.with_file_name(".chat_tools.json.tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(io)?;
    file.write_all(to_string_pretty(&json!({"auto_approve": names})).as_bytes())
        .map_err(io)?;
    drop(file);
    fs::rename(&tmp, &path).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use super::super::*;
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn env(tmp: &std::path::Path) -> Env {
        Env::new(tmp.join("repo"), tmp.join("home"))
    }

    #[test]
    fn nothing_is_skipped_by_default() {
        let d = tempfile::tempdir().unwrap();
        assert!(auto_approved(&env(d.path())).is_empty());
    }

    #[test]
    fn set_and_clear_round_trip_with_private_file_mode() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        set_auto_approve(&e, "gitlab.comment", true).unwrap();
        assert_eq!(
            auto_approved(&e),
            BTreeSet::from(["gitlab.comment".to_string()])
        );
        let path = settings_path(&e);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        set_auto_approve(&e, "gitlab.comment", false).unwrap();
        assert!(auto_approved(&e).is_empty());
    }

    #[test]
    fn file_is_indent_2_sorted_without_trailing_newline() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        save(&e, ["mail.send", "gitlab.comment"]).unwrap();
        assert_eq!(
            fs::read_to_string(settings_path(&e)).unwrap(),
            "{\n  \"auto_approve\": [\n    \"gitlab.comment\",\n    \"mail.send\"\n  ]\n}"
        );
        save(&e, Vec::<String>::new()).unwrap();
        assert_eq!(
            fs::read_to_string(settings_path(&e)).unwrap(),
            "{\n  \"auto_approve\": []\n}"
        );
        assert!(!settings_path(&e)
            .with_file_name(".chat_tools.json.tmp")
            .exists());
    }

    #[test]
    fn only_registered_write_tools_can_be_skipped() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        for name in ["gitlab.read_issue", "docs.save", "nope.tool"] {
            let err = set_auto_approve(&e, name, true).unwrap_err();
            assert_eq!(err.to_string(), format!("{name} is not a chat write tool"));
        }
        assert_eq!(
            save(&e, ["mail.send", "zzz", "aaa"])
                .unwrap_err()
                .to_string(),
            "aaa is not a chat write tool"
        );
        assert!(!settings_path(&e).exists());
    }

    #[test]
    fn corrupt_or_unknown_entries_are_ignored() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        fs::create_dir_all(&e.home_dir).unwrap();
        fs::write(settings_path(&e), "{not json").unwrap();
        assert!(auto_approved(&e).is_empty());
        fs::write(settings_path(&e), "[1]").unwrap();
        assert!(auto_approved(&e).is_empty());
        fs::write(
            settings_path(&e),
            json!({"auto_approve": ["gitlab.comment", "gitlab.read_issue", 5]}).to_string(),
        )
        .unwrap();
        assert_eq!(
            auto_approved(&e),
            BTreeSet::from(["gitlab.comment".to_string()])
        );
    }

    fn gl_env(tmp: &std::path::Path) -> (Env, Fake) {
        let mut gl = account("gl", "gitlab");
        gl.settings
            .insert("url".into(), json!("https://gl.example"));
        let mut fake = Fake::new(gl.clone());
        fake.api = Some(Arc::new(|_, _, _| Ok(json!({"id": 7}))));
        let (e, _) = env_with(tmp, vec![gl, account("gh", "github")], &fake);
        (e, fake)
    }

    const ARGS: &str = r#"{"url": "https://gl.example/g/p/-/issues/3", "body": "hi"}"#;

    #[test]
    fn a_skipped_write_runs_immediately_and_others_still_wait() {
        let d = tempfile::tempdir().unwrap();
        let (e, fake) = gl_env(d.path());
        set_auto_approve(&e, "gitlab.comment", true).unwrap();
        let out = run(&e, "gitlab.comment", ARGS, "s1");
        assert_eq!(out["result"], json!({"note_id": 7}));
        assert_eq!(out["auto_approved"], true);
        assert_eq!(fake.log().len(), 1);
        let other = run(
            &e,
            "github.comment",
            r#"{"repo": "o/r", "number": 1, "body": "x"}"#,
            "s1",
        );
        assert_eq!(other["status"], "pending_approval");
        assert_eq!(fake.log().len(), 1);
    }

    #[test]
    fn default_still_requires_approval() {
        let d = tempfile::tempdir().unwrap();
        let (e, fake) = gl_env(d.path());
        let out = run(&e, "gitlab.comment", ARGS, "s1");
        assert_eq!(out["status"], "pending_approval");
        assert!(fake.log().is_empty());
    }
}
