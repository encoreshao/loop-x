//! The dashboard chat / loop inbox store: `outputs/messages.json` (one flat list
//! of messages) and its sibling `chat-sessions.json`. Mirrors the message and
//! session half of `bin/web/dashboard_server.py` (`read_messages`,
//! `append_message`, `pop_unseen_user_messages`, `create_chat_session`, ...).
//!
//! Two OS processes write these files (the dashboard's chat threads and the
//! scheduled loop's own `loopx messages ...` calls), so every read-modify-write
//! holds an exclusive `flock` on a sibling `<file>.lock` (opened `a+`, never
//! deleted) and rewrites the whole file atomically (0600, temp file in the same
//! directory, `rename`), exactly like Python.

use crate::mail_time::isoformat_utc;
use crate::pyjson;
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// The id of the session that groups messages written before sessions existed.
pub const LEGACY_CHAT_SESSION_ID: &str = "earlier";
/// English title of the legacy group (translated when rendered).
pub const LEGACY_CHAT_SESSION_TITLE: &str = "Earlier messages";
const CHAT_TITLE_MAX_CHARS: usize = 48;

/// `<repo_root>/outputs/messages.json`.
pub fn messages_path_for(repo_root: &Path) -> PathBuf {
    repo_root.join("outputs").join("messages.json")
}

/// `chat_sessions_path_for`: `chat-sessions.json` beside the messages file.
pub fn chat_sessions_path_for(messages_path: &Path) -> PathBuf {
    messages_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("chat-sessions.json")
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// An exclusive `flock` on `<path>.lock`, released on drop.
struct FileLock {
    file: std::fs::File,
}

impl FileLock {
    fn exclusive(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(with_suffix(path, ".lock"))?;
        // SAFETY: flock on a descriptor we own; blocks until acquired.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: unlocking our own descriptor.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// `_atomic_write_json`: `json.dump(indent=2)` into a temp file beside `path`,
/// chmod 0600, rename over the target.
pub fn atomic_write_json(value: &Value, path: &Path) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(
        ".{name}.{}.tmp",
        &crate::events::new_uuid_hex()[..8]
    ));
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(pyjson::to_string_pretty(value).as_bytes())?;
        f.flush()?;
        drop(f);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Python's `str.splitlines()` line boundaries.
fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\u{0b}'
            | '\u{0c}'
            | '\u{1c}'
            | '\u{1d}'
            | '\u{1e}'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// `heuristic_chat_title`: first non-blank line, markdown punctuation dropped,
/// whitespace collapsed, cut on a word boundary at 48 characters.
pub fn heuristic_chat_title(text: &str) -> String {
    let line = text
        .split(is_line_break)
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let stripped: String = line
        .chars()
        .filter(|c| !matches!(c, '*' | '_' | '`' | '#' | '>' | '[' | ']'))
        .collect();
    let line = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return "New chat".to_string();
    }
    if line.chars().count() <= CHAT_TITLE_MAX_CHARS {
        return line;
    }
    let head: String = line.chars().take(CHAT_TITLE_MAX_CHARS).collect();
    let word = head.rsplit_once(' ').map_or(head.as_str(), |(a, _)| a);
    let cut = word.trim_end_matches([' ', ',', '.', ';', ':', '-']);
    let base = if cut.is_empty() { head.as_str() } else { cut };
    format!("{base}…")
}

/// `read_chat_sessions` result.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatSessions {
    pub current: Option<String>,
    pub sessions: Vec<Value>,
    /// Whether the file was there (and a JSON object) at all.
    pub exists: bool,
}

/// One row of `list_chat_sessions`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub title_source: String,
    pub started_at: String,
    pub last_at: String,
    pub count: usize,
}

/// The store, rooted at one `messages.json`.
#[derive(Debug, Clone)]
pub struct MessageStore {
    path: PathBuf,
}

fn message_session_id(m: &Value) -> String {
    match m.get("session").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => LEGACY_CHAT_SESSION_ID.to_string(),
    }
}

fn has_no_session(m: &Value) -> bool {
    !matches!(m.get("session"), Some(Value::String(s)) if !s.is_empty())
        && !matches!(m.get("session"), Some(v) if v.as_bool() == Some(true)
            || v.as_i64().is_some_and(|n| n != 0))
}

impl MessageStore {
    pub fn new(messages_path: impl Into<PathBuf>) -> Self {
        Self {
            path: messages_path.into(),
        }
    }

    pub fn for_root(repo_root: &Path) -> Self {
        Self::new(messages_path_for(repo_root))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `read_messages`: the whole thread, oldest first; `[]` when missing or
    /// malformed; non-object elements are dropped.
    pub fn read_messages(&self) -> Vec<Value> {
        read_array(&self.path)
    }

    pub fn read_chat_sessions(&self) -> ChatSessions {
        let missing = ChatSessions {
            current: None,
            sessions: Vec::new(),
            exists: false,
        };
        let Ok(text) = std::fs::read_to_string(chat_sessions_path_for(&self.path)) else {
            return missing;
        };
        let Ok(Value::Object(data)) = serde_json::from_str::<Value>(&text) else {
            return missing;
        };
        let sessions = data
            .get("sessions")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|r| r.get("id").is_some_and(Value::is_string))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let current = data
            .get("current")
            .and_then(Value::as_str)
            .map(str::to_string);
        ChatSessions {
            current,
            sessions,
            exists: true,
        }
    }

    /// `_update_chat_sessions`: read-modify-write under the sessions flock.
    fn update_sessions<R>(
        &self,
        mutate: impl FnOnce(&mut Map<String, Value>) -> R,
    ) -> std::io::Result<R> {
        let path = chat_sessions_path_for(&self.path);
        let _lock = FileLock::exclusive(&path)?;
        let cur = self.read_chat_sessions();
        let mut data = Map::new();
        data.insert(
            "current".into(),
            cur.current.map_or(Value::Null, Value::String),
        );
        data.insert("sessions".into(), Value::Array(cur.sessions));
        let out = mutate(&mut data);
        atomic_write_json(&Value::Object(data), &path)?;
        Ok(out)
    }

    /// The session new messages go to (`None` after "New chat").
    pub fn current_chat_session_id(&self) -> Option<String> {
        let data = self.read_chat_sessions();
        if data.exists {
            return data.current;
        }
        if self.read_messages().iter().any(has_no_session) {
            return Some(LEGACY_CHAT_SESSION_ID.to_string());
        }
        None
    }

    pub fn chat_session_exists(&self, session_id: &str) -> bool {
        if session_id.is_empty() {
            return false;
        }
        if session_id == LEGACY_CHAT_SESSION_ID {
            return self.read_messages().iter().any(has_no_session);
        }
        self.read_chat_sessions()
            .sessions
            .iter()
            .any(|r| r["id"].as_str() == Some(session_id))
    }

    /// `create_chat_session`: opens a session titled from `first_text`, makes it
    /// current, returns its id (12 hex chars).
    pub fn create_chat_session(&self, first_text: &str, now: DateTime<Utc>) -> String {
        let session_id = crate::events::new_uuid_hex()[..12].to_string();
        let record = json!({
            "id": session_id,
            "started_at": isoformat_utc(now),
            "title": heuristic_chat_title(first_text),
            "title_source": "auto",
        });
        let id = session_id.clone();
        let _ = self.update_sessions(move |data| {
            if let Some(Value::Array(a)) = data.get_mut("sessions") {
                a.push(record);
            }
            data.insert("current".into(), Value::String(id));
        });
        session_id
    }

    pub fn set_current_chat_session(&self, session_id: Option<&str>) {
        let value = session_id.map_or(Value::Null, |s| Value::String(s.to_string()));
        let _ = self.update_sessions(move |data| {
            data.insert("current".into(), value);
        });
    }

    /// "New chat": clears the current session; nothing is deleted.
    pub fn start_new_chat_session(&self) {
        self.set_current_chat_session(None);
    }

    pub fn set_chat_session_title(&self, session_id: &str, title: &str, source: &str) {
        let (id, title, source) = (
            session_id.to_string(),
            title.to_string(),
            source.to_string(),
        );
        let _ = self.update_sessions(move |data| {
            if let Some(Value::Array(a)) = data.get_mut("sessions") {
                for r in a.iter_mut() {
                    if r["id"].as_str() == Some(id.as_str()) {
                        r["title"] = Value::String(title.clone());
                        r["title_source"] = Value::String(source.clone());
                    }
                }
            }
        });
    }

    pub fn chat_session_messages(&self, session_id: &str) -> Vec<Value> {
        if session_id.is_empty() {
            return Vec::new();
        }
        self.read_messages()
            .into_iter()
            .filter(|m| message_session_id(m) == session_id)
            .collect()
    }

    /// `list_chat_sessions`: sessions with at least one message, latest first.
    pub fn list_chat_sessions(&self) -> Vec<SessionInfo> {
        struct Stat {
            count: usize,
            first_at: String,
            last_at: String,
        }
        let mut order: Vec<String> = Vec::new();
        let mut stats: std::collections::HashMap<String, Stat> = std::collections::HashMap::new();
        for m in self.read_messages() {
            let sid = message_session_id(&m);
            let ts =
                crate::pydumps::py_str(m.get("timestamp").unwrap_or(&Value::String(String::new())));
            let ts = if m.get("timestamp").is_none() {
                String::new()
            } else {
                ts
            };
            let e = stats.entry(sid.clone()).or_insert_with(|| {
                order.push(sid.clone());
                Stat {
                    count: 0,
                    first_at: String::new(),
                    last_at: String::new(),
                }
            });
            e.count += 1;
            if e.first_at.is_empty() {
                e.first_at = ts.clone();
            }
            if ts > e.last_at {
                e.last_at = ts;
            }
        }
        let mut sessions = Vec::new();
        for record in self.read_chat_sessions().sessions {
            let id = record["id"].as_str().unwrap_or("").to_string();
            if let Some(entry) = stats.remove(&id) {
                let title = match record.get("title") {
                    Some(Value::String(s)) if !s.is_empty() => s.clone(),
                    _ => "New chat".to_string(),
                };
                sessions.push(SessionInfo {
                    id,
                    title,
                    title_source: record
                        .get("title_source")
                        .and_then(Value::as_str)
                        .unwrap_or("auto")
                        .to_string(),
                    started_at: record
                        .get("started_at")
                        .and_then(Value::as_str)
                        .map_or(entry.first_at.clone(), str::to_string),
                    last_at: entry.last_at,
                    count: entry.count,
                });
            }
        }
        if let Some(legacy) = stats.remove(LEGACY_CHAT_SESSION_ID) {
            sessions.push(SessionInfo {
                id: LEGACY_CHAT_SESSION_ID.to_string(),
                title: LEGACY_CHAT_SESSION_TITLE.to_string(),
                title_source: "ai".to_string(),
                started_at: legacy.first_at,
                last_at: legacy.last_at,
                count: legacy.count,
            });
        }
        // Python's sort is stable and `reverse=True` keeps equal keys in order.
        sessions.sort_by(|a, b| b.last_at.cmp(&a.last_at));
        sessions
    }

    /// `delete_chat_session`: false when the session does not exist.
    pub fn delete_chat_session(&self, session_id: &str) -> bool {
        if !self.chat_session_exists(session_id) {
            return false;
        }
        if let Ok(_lock) = FileLock::exclusive(&self.path) {
            let remaining: Vec<Value> = self
                .read_messages()
                .into_iter()
                .filter(|m| message_session_id(m) != session_id)
                .collect();
            let _ = atomic_write_json(&Value::Array(remaining), &self.path);
        }
        if session_id != LEGACY_CHAT_SESSION_ID || self.read_chat_sessions().exists {
            let id = session_id.to_string();
            let _ = self.update_sessions(move |data| {
                if let Some(Value::Array(a)) = data.get_mut("sessions") {
                    a.retain(|r| r["id"].as_str() != Some(id.as_str()));
                }
                if data.get("current").and_then(Value::as_str) == Some(id.as_str()) {
                    data.insert("current".into(), Value::Null);
                }
            });
        }
        true
    }

    /// `resolve_chat_session_for_send`.
    pub fn resolve_chat_session_for_send(
        &self,
        requested: &str,
        text: &str,
        now: DateTime<Utc>,
    ) -> String {
        if self.chat_session_exists(requested) {
            self.set_current_chat_session(Some(requested));
            return requested.to_string();
        }
        self.create_chat_session(text, now)
    }

    /// `append_message`: one message with the current UTC timestamp. `session`
    /// `None` means the current session, or a new one if "New chat" cleared it.
    pub fn append_message(
        &self,
        from: &str,
        text: &str,
        session: Option<&str>,
        tools: Option<&Value>,
    ) {
        self.append_message_at(from, text, session, tools, Utc::now());
    }

    pub fn append_message_at(
        &self,
        from: &str,
        text: &str,
        session: Option<&str>,
        tools: Option<&Value>,
        now: DateTime<Utc>,
    ) {
        let session = match session {
            Some(s) => s.to_string(),
            None => self
                .current_chat_session_id()
                .unwrap_or_else(|| self.create_chat_session(text, now)),
        };
        let Ok(_lock) = FileLock::exclusive(&self.path) else {
            return;
        };
        let mut messages = self.read_messages();
        let mut entry = Map::new();
        entry.insert("from".into(), json!(from));
        entry.insert("text".into(), json!(text));
        entry.insert("timestamp".into(), json!(isoformat_utc(now)));
        if from == "user" {
            entry.insert("seen_by_loop".into(), json!(false));
        }
        if session != LEGACY_CHAT_SESSION_ID {
            entry.insert("session".into(), json!(session));
        }
        if let Some(t) = tools {
            let non_empty = match t {
                Value::Array(a) => !a.is_empty(),
                Value::Null => false,
                _ => true,
            };
            if non_empty {
                entry.insert("tools".into(), t.clone());
            }
        }
        messages.push(Value::Object(entry));
        let _ = atomic_write_json(&Value::Array(messages), &self.path);
    }

    /// `pop_unseen_user_messages`: every unseen user message, marked seen in the
    /// same atomic write.
    pub fn pop_unseen_user_messages(&self) -> Vec<Value> {
        let Ok(_lock) = FileLock::exclusive(&self.path) else {
            return Vec::new();
        };
        let mut messages = self.read_messages();
        let unseen_at = |m: &Value| {
            m.get("from").and_then(Value::as_str) == Some("user")
                && m.get("seen_by_loop") == Some(&Value::Bool(false))
        };
        let unseen: Vec<Value> = messages.iter().filter(|m| unseen_at(m)).cloned().collect();
        if !unseen.is_empty() {
            for m in &mut messages {
                if unseen_at(m) {
                    m["seen_by_loop"] = Value::Bool(true);
                }
            }
            let _ = atomic_write_json(&Value::Array(messages), &self.path);
        }
        unseen
    }

    /// `send_chat_message`: `(session_id)` or `None` for an empty message.
    /// `session` `None` means "the current session, or a new one"; `""` always
    /// means new.
    pub fn send_chat_message(&self, text: &str, session: Option<&str>) -> Option<String> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let requested = match session {
            Some(s) => s.to_string(),
            None => self.current_chat_session_id().unwrap_or_default(),
        };
        let now = Utc::now();
        let id = self.resolve_chat_session_for_send(&requested, text, now);
        self.append_message_at("user", text, Some(&id), None, now);
        Some(id)
    }

    /// `delete_message`: false when no message has this exact timestamp.
    /// (Python does not take the flock here; neither do we.)
    pub fn delete_message(&self, timestamp: &str) -> bool {
        let messages = self.read_messages();
        let remaining: Vec<Value> = messages
            .iter()
            .filter(|m| m.get("timestamp").and_then(Value::as_str) != Some(timestamp))
            .cloned()
            .collect();
        if remaining.len() == messages.len() {
            return false;
        }
        let _ = atomic_write_json(&Value::Array(remaining), &self.path);
        true
    }
}

fn read_array(path: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Array(a)) => a.into_iter().filter(Value::is_object).collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn store() -> (tempfile::TempDir, MessageStore) {
        let d = tempfile::tempdir().unwrap();
        let s = MessageStore::for_root(d.path());
        (d, s)
    }

    #[test]
    fn titles_follow_the_python_heuristic() {
        assert_eq!(heuristic_chat_title(""), "New chat");
        assert_eq!(
            heuristic_chat_title("\n\n  **Hello** `world`  "),
            "Hello world"
        );
        let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda";
        assert_eq!(
            heuristic_chat_title(long),
            "alpha beta gamma delta epsilon zeta eta theta…"
        );
        let solid = "x".repeat(60);
        assert_eq!(heuristic_chat_title(&solid), format!("{}…", "x".repeat(48)));
    }

    #[test]
    fn append_and_read_roundtrip_with_key_order() {
        let (d, s) = store();
        assert!(s.read_messages().is_empty());
        let now = Utc.with_ymd_and_hms(2026, 10, 22, 8, 10, 0).unwrap();
        let sid = s.create_chat_session("What now?", now);
        assert_eq!(sid.len(), 12);
        s.append_message_at("user", "What now?", Some(&sid), None, now);
        s.append_message_at(
            "loop",
            "Idle.",
            Some(&sid),
            Some(&json!([{"name": "x", "summary": "", "ok": true}])),
            now,
        );
        let raw = std::fs::read_to_string(d.path().join("outputs/messages.json")).unwrap();
        assert!(raw.contains("\"seen_by_loop\": false"));
        let msgs = s.read_messages();
        assert_eq!(msgs.len(), 2);
        let keys: Vec<&String> = msgs[1].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["from", "text", "timestamp", "session", "tools"]);
        let keys: Vec<&String> = msgs[0].as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            ["from", "text", "timestamp", "seen_by_loop", "session"]
        );
        assert_eq!(msgs[0]["timestamp"], "2026-10-22T08:10:00+00:00");
    }

    #[test]
    fn pop_unseen_marks_seen_once() {
        let (_d, s) = store();
        s.append_message("user", "hi", Some("abc"), None);
        s.append_message("loop", "yo", Some("abc"), None);
        assert_eq!(s.pop_unseen_user_messages().len(), 1);
        assert!(s.pop_unseen_user_messages().is_empty());
        assert_eq!(s.read_messages()[0]["seen_by_loop"], true);
    }

    #[test]
    fn sessions_current_new_chat_and_delete() {
        let (_d, s) = store();
        assert_eq!(s.current_chat_session_id(), None);
        let id = s.send_chat_message("  first  ", None).unwrap();
        assert_eq!(s.current_chat_session_id().as_deref(), Some(id.as_str()));
        // continuing the current session
        assert_eq!(s.send_chat_message("second", None).unwrap(), id);
        s.start_new_chat_session();
        assert_eq!(s.current_chat_session_id(), None);
        let other = s.send_chat_message("third", None).unwrap();
        assert_ne!(other, id);
        let listed = s.list_chat_sessions();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].count + listed[1].count, 3);
        assert!(s.delete_chat_session(&id));
        assert!(!s.delete_chat_session(&id));
        assert_eq!(s.list_chat_sessions().len(), 1);
        assert!(s.send_chat_message("   ", None).is_none());
    }

    #[test]
    fn legacy_messages_form_the_earlier_session() {
        let (d, s) = store();
        std::fs::create_dir_all(d.path().join("outputs")).unwrap();
        std::fs::write(
            d.path().join("outputs/messages.json"),
            r#"[{"from":"user","text":"old","timestamp":"2026-01-01T00:00:00+00:00"}, 3]"#,
        )
        .unwrap();
        assert_eq!(s.read_messages().len(), 1);
        assert_eq!(s.current_chat_session_id().as_deref(), Some("earlier"));
        assert!(s.chat_session_exists("earlier"));
        let listed = s.list_chat_sessions();
        assert_eq!(listed[0].id, "earlier");
        assert_eq!(listed[0].title, "Earlier messages");
        assert!(s.delete_chat_session("earlier"));
        assert!(s.read_messages().is_empty());
        // The legacy delete wrote no sessions file.
        assert!(!d.path().join("outputs/chat-sessions.json").exists());
    }

    #[test]
    fn append_without_a_session_opens_one() {
        let (_d, s) = store();
        s.append_message("loop", "note", None, None);
        let m = &s.read_messages()[0];
        assert!(m["session"].as_str().is_some_and(|x| x.len() == 12));
        assert_eq!(
            s.current_chat_session_id().as_deref(),
            m["session"].as_str()
        );
    }

    #[test]
    fn delete_message_matches_the_exact_timestamp() {
        let (_d, s) = store();
        let now = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        s.append_message_at("user", "a", Some("s1"), None, now);
        assert!(!s.delete_message("nope"));
        assert!(s.delete_message("2026-01-02T03:04:05+00:00"));
        assert!(s.read_messages().is_empty());
    }

    #[test]
    fn set_title_updates_the_record() {
        let (_d, s) = store();
        let id = s.send_chat_message("hello there", None).unwrap();
        s.set_chat_session_title(&id, "Greeting", "ai");
        let l = s.list_chat_sessions();
        assert_eq!(
            (l[0].title.as_str(), l[0].title_source.as_str()),
            ("Greeting", "ai")
        );
    }
}
