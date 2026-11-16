//! File-backed, flock'd set of recently handled Slack event ids.

use std::path::PathBuf;

const KEEP: usize = 500;

pub struct SeenEvents {
    path: PathBuf,
}

impl SeenEvents {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// True when `event_id` was already recorded; records nothing.
    pub fn contains(&self, event_id: &str) -> bool {
        !event_id.is_empty()
            && std::fs::read_to_string(&self.path).is_ok_and(|t| t.lines().any(|l| l == event_id))
    }

    /// True the first time an id is seen; remembers the last 500 on disk.
    /// If the file cannot be updated this fails open (true): handling a
    /// request twice beats silently dropping one.
    /// An empty id is never "first time" (fail closed).
    pub fn first_time(&self, event_id: &str) -> bool {
        if event_id.is_empty() {
            return false;
        }
        crate::file_lock::update_locked(&self.path, |current| {
            let mut ids: Vec<&str> = current.lines().filter(|l| !l.is_empty()).collect();
            if ids.contains(&event_id) {
                return (current.to_string(), false);
            }
            ids.push(event_id);
            let start = ids.len().saturating_sub(KEEP);
            let mut next = ids[start..].join("\n");
            next.push('\n');
            (next, true)
        })
        .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_time_then_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let s = SeenEvents::new(dir.path().join("seen"));
        assert!(s.first_time("a"));
        assert!(!s.first_time("a"));
        assert!(s.first_time("b"));
    }

    #[test]
    fn empty_id_is_never_first_time() {
        let dir = tempfile::tempdir().unwrap();
        let s = SeenEvents::new(dir.path().join("seen"));
        assert!(!s.first_time(""));
        assert!(!s.first_time(""));
        assert!(s.first_time("a"));
    }

    #[test]
    fn contains_reports_without_recording() {
        let dir = tempfile::tempdir().unwrap();
        let s = SeenEvents::new(dir.path().join("seen"));
        assert!(!s.contains("a"));
        assert!(!s.contains("a"), "asking does not record");
        assert!(s.first_time("a"));
        assert!(s.contains("a"));
        assert!(!s.contains("b"));
        assert!(!s.contains(""));
    }

    #[test]
    fn survives_a_new_instance() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("seen");
        assert!(SeenEvents::new(p.clone()).first_time("a"));
        assert!(!SeenEvents::new(p).first_time("a"));
    }

    #[test]
    fn trims_to_the_last_500() {
        let dir = tempfile::tempdir().unwrap();
        let s = SeenEvents::new(dir.path().join("seen"));
        for i in 0..600 {
            assert!(s.first_time(&format!("e{i}")));
        }
        assert!(s.first_time("e0"), "oldest id is forgotten");
        assert!(!s.first_time("e599"), "newest id is remembered");
    }
}
