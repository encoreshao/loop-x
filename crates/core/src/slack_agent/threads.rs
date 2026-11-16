//! Slack thread -> chat session map, so follow-ups in a thread keep context.
//!
//! A small JSON object `{"<channel>:<thread_ts>": "<session_id>"}`, written
//! under the file lock so the listener and any helper never lose an entry.

use serde_json::{Map, Value};
use std::path::PathBuf;

pub struct ThreadMap {
    path: PathBuf,
}

fn key(channel: &str, thread_ts: &str) -> String {
    format!("{channel}:{thread_ts}")
}

fn parse(text: &str) -> Map<String, Value> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

impl ThreadMap {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn session_for(&self, channel: &str, thread_ts: &str) -> Option<String> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        parse(&text)
            .get(&key(channel, thread_ts))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    pub fn bind(&self, channel: &str, thread_ts: &str, session_id: &str) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let entry = (key(channel, thread_ts), Value::String(session_id.into()));
        let _ = crate::file_lock::update_locked(&self.path, move |cur| {
            let mut map = parse(cur);
            map.insert(entry.0, entry.1);
            let text = serde_json::to_string_pretty(&Value::Object(map)).unwrap_or_default();
            (text, ())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_thread_has_no_session() {
        let d = tempfile::tempdir().unwrap();
        let map = ThreadMap::new(d.path().join("threads.json"));
        assert_eq!(map.session_for("C1", "1.0"), None);
    }

    #[test]
    fn a_bound_thread_returns_its_session_and_only_its_own() {
        let d = tempfile::tempdir().unwrap();
        let map = ThreadMap::new(d.path().join("threads.json"));
        map.bind("C1", "1.0", "abc");
        map.bind("C2", "1.0", "def");
        assert_eq!(map.session_for("C1", "1.0").as_deref(), Some("abc"));
        assert_eq!(map.session_for("C2", "1.0").as_deref(), Some("def"));
        assert_eq!(map.session_for("C1", "2.0"), None);
    }

    #[test]
    fn bindings_persist_across_instances() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("sub/threads.json");
        ThreadMap::new(path.clone()).bind("C1", "1.0", "abc");
        assert_eq!(
            ThreadMap::new(path).session_for("C1", "1.0").as_deref(),
            Some("abc")
        );
    }
}
