//! Pending write actions awaiting an Approve click (port of `pending.py`).
//! One JSON file per action under `<repo_root>/outputs/chat-pending/`. Args
//! are frozen when the model proposes the action; [`claim`] is single use,
//! expires after [`TTL_SECONDS`] and is bound to the chat session that
//! proposed it. Nothing here executes a tool.

use super::Env;
use crate::pydumps::dumps;
use serde_json::{json, Value};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const TTL_SECONDS: f64 = 900.0;

/// A stored pending action.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub tool: String,
    pub args: Value,
    pub session: String,
    pub summary: String,
    pub created: f64,
}

impl Pending {
    /// The record as stored on disk (key order as Python writes it).
    pub fn to_value(&self) -> Value {
        json!({"tool": self.tool, "args": self.args, "session": self.session,
               "summary": self.summary, "created": self.created})
    }

    fn from_value(v: &Value) -> Self {
        let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        Self {
            tool: text("tool"),
            args: v.get("args").cloned().unwrap_or(Value::Null),
            session: text("session"),
            summary: text("summary"),
            created: v.get("created").and_then(Value::as_f64).unwrap_or(0.0),
        }
    }
}

/// `<repo_root>/outputs/chat-pending`.
pub fn pending_dir(env: &Env) -> PathBuf {
    env.repo_root.join("outputs").join("chat-pending")
}

fn dir(env: &Env) -> io::Result<PathBuf> {
    let path = pending_dir(env);
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn valid_id(id: &str) -> bool {
    (32..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Stores a proposed action and returns its id (32 hex chars).
pub fn create(
    env: &Env,
    tool: &str,
    args: &Value,
    session: &str,
    summary: &str,
    now: Option<f64>,
) -> io::Result<String> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let record = json!({"tool": tool, "args": args, "session": session, "summary": summary,
                        "created": now.unwrap_or_else(unix_now)});
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir(env)?.join(format!("{id}.json")))?;
    file.write_all(dumps(&record).as_bytes())?;
    Ok(id)
}

fn load(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn valid(record: &Option<Value>, session: &str, now: Option<f64>) -> bool {
    let Some(Value::Object(m)) = record else {
        return false;
    };
    if session.is_empty() || m.get("session").and_then(Value::as_str) != Some(session) {
        return false;
    }
    let Some(created) = m.get("created").and_then(Value::as_f64) else {
        return false;
    };
    now.unwrap_or_else(unix_now) - created <= TTL_SECONDS
}

fn read(env: &Env, id: &str, session: &str, now: Option<f64>) -> Option<Pending> {
    if !valid_id(id) {
        return None;
    }
    let record = dir(env)
        .ok()
        .and_then(|d| load(&d.join(format!("{id}.json"))));
    valid(&record, session, now).then(|| Pending::from_value(&record.unwrap_or(Value::Null)))
}

/// The live record for this session, or `None`; never consumes it.
pub fn lookup(env: &Env, id: &str, session: &str, now: Option<f64>) -> Option<Pending> {
    read(env, id, session, now)
}

/// Consumes the record (single use): renames it to `<id>.claimed`, so only
/// one caller wins, and returns what was actually claimed.
pub fn claim(env: &Env, id: &str, session: &str, now: Option<f64>) -> Option<Pending> {
    read(env, id, session, now)?;
    let dir = dir(env).ok()?;
    let path = dir.join(format!("{id}.json"));
    let claimed = dir.join(format!("{id}.claimed"));
    fs::rename(&path, &claimed).ok()?;
    let record = load(&claimed);
    valid(&record, session, now).then(|| Pending::from_value(&record.unwrap_or(Value::Null)))
}

/// Removes the record when it belongs to this session.
pub fn discard(env: &Env, id: &str, session: &str) -> bool {
    if read(env, id, session, None).is_none() {
        return false;
    }
    match dir(env) {
        Ok(d) => fs::remove_file(d.join(format!("{id}.json"))).is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn env(tmp: &Path) -> Env {
        Env::new(tmp.join("repo"), tmp.join("home"))
    }

    #[test]
    fn create_then_claim_returns_the_frozen_record() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let id = create(
            &e,
            "gitlab.comment",
            &json!({"body": "hi"}),
            "s1",
            "g/p#1",
            Some(1000.0),
        )
        .unwrap();
        let r = claim(&e, &id, "s1", Some(1001.0)).unwrap();
        assert_eq!(
            (r.tool.as_str(), &r.args),
            ("gitlab.comment", &json!({"body": "hi"}))
        );
        assert_eq!(
            (r.summary.as_str(), r.session.as_str(), r.created),
            ("g/p#1", "s1", 1000.0)
        );
    }

    #[test]
    fn file_layout_matches_python() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let id = create(&e, "t", &json!({"a": "é"}), "s1", "sum", Some(1000.5)).unwrap();
        let path = d
            .path()
            .join("repo/outputs/chat-pending")
            .join(format!("{id}.json"));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            r#"{"tool": "t", "args": {"a": "\u00e9"}, "session": "s1", "summary": "sum", "created": 1000.5}"#
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()));
        claim(&e, &id, "s1", Some(1001.0)).unwrap();
        assert!(!path.exists() && path.with_extension("claimed").exists());
    }

    #[test]
    fn claim_is_single_use() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let id = create(&e, "t", &json!({}), "s1", "", Some(1000.0)).unwrap();
        assert!(claim(&e, &id, "s1", Some(1001.0)).is_some());
        assert!(claim(&e, &id, "s1", Some(1002.0)).is_none());
    }

    #[test]
    fn claim_refuses_other_session_and_leaves_it_claimable() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let id = create(&e, "t", &json!({}), "s1", "", Some(1000.0)).unwrap();
        assert!(claim(&e, &id, "other", Some(1001.0)).is_none());
        assert!(claim(&e, &id, "s1", Some(1002.0)).is_some());
    }

    #[test]
    fn claim_refuses_expired() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let id = create(&e, "t", &json!({}), "s1", "", Some(1000.0)).unwrap();
        assert!(claim(&e, &id, "s1", Some(1000.0 + TTL_SECONDS + 1.0)).is_none());
        let id = create(&e, "t", &json!({}), "s1", "", Some(1000.0)).unwrap();
        assert!(claim(&e, &id, "s1", Some(1000.0 + TTL_SECONDS)).is_some());
    }

    #[test]
    fn ids_are_unguessable_and_path_safe() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        assert!(claim(&e, "../../etc/passwd", "s1", None).is_none());
        assert!(create(&e, "t", &json!({}), "s1", "", None).unwrap().len() >= 32);
    }

    #[test]
    fn discard_removes_only_own_session() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let id = create(&e, "t", &json!({}), "s1", "", None).unwrap();
        assert!(!discard(&e, &id, "other"));
        assert!(discard(&e, &id, "s1"));
        assert!(claim(&e, &id, "s1", None).is_none());
    }

    #[test]
    fn lookup_does_not_consume_and_applies_the_same_checks() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let id = create(
            &e,
            "gitlab.comment",
            &json!({"body": "stored"}),
            "s1",
            "g/p#1",
            Some(1000.0),
        )
        .unwrap();
        let r = lookup(&e, &id, "s1", Some(1001.0)).unwrap();
        assert_eq!(
            (r.args, r.summary.as_str()),
            (json!({"body": "stored"}), "g/p#1")
        );
        assert!(lookup(&e, &id, "s1", Some(1002.0)).is_some());
        assert!(lookup(&e, &id, "other", Some(1001.0)).is_none());
        assert!(lookup(&e, &id, "s1", Some(1000.0 + TTL_SECONDS + 1.0)).is_none());
        assert!(lookup(&e, "../../etc/passwd", "s1", None).is_none());
        assert!(lookup(&e, &"f".repeat(32), "s1", None).is_none());
        assert!(claim(&e, &id, "s1", Some(1003.0)).is_some());
        assert!(lookup(&e, &id, "s1", Some(1004.0)).is_none());
    }

    #[test]
    fn an_empty_session_never_matches() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let id = create(&e, "t", &json!({}), "", "", Some(1000.0)).unwrap();
        assert!(lookup(&e, &id, "", Some(1001.0)).is_none());
        assert!(claim(&e, &id, "", Some(1001.0)).is_none());
        assert!(!discard(&e, &id, ""));
    }

    #[test]
    fn a_record_without_a_numeric_created_is_never_valid() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let dir = dir(&e).unwrap();
        let id = "a".repeat(32);
        fs::write(
            dir.join(format!("{id}.json")),
            r#"{"session": "s1", "created": true}"#,
        )
        .unwrap();
        assert!(lookup(&e, &id, "s1", Some(1.0)).is_none());
        fs::write(dir.join(format!("{id}.json")), "not json").unwrap();
        assert!(lookup(&e, &id, "s1", Some(1.0)).is_none());
    }

    #[test]
    fn the_dir_follows_the_env_repo_root() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let id = create(&e, "t", &json!({}), "s1", "", None).unwrap();
        assert!(d
            .path()
            .join("repo/outputs/chat-pending")
            .join(format!("{id}.json"))
            .exists());
    }
}
