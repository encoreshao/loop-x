//! Persisted per-issue state for merge-request review threads: where the loop
//! is with each discussion, plus a ledger of note ids the loop itself posted
//! (it posts with the user's own token, so authorship can't identify them).
//! Stored at `<memory root>/<alias>/mr-threads/<iid>.json`.

use crate::memory_store::default_memory_root;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

/// Marker appended to every note the loop posts.
pub const LOOP_SIGNATURE: &str = "<sub>— posted by loopx</sub>";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadStatus {
    New,
    Working,
    Answered,
    WaitingOnReviewer,
    NeedsYou,
    Resolved,
    Decided,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ThreadState {
    pub discussion_id: String,
    pub last_note_id: i64,
    pub status: ThreadStatus,
    pub plan_summary: String,
    pub confidence: Option<u8>,
    pub attempts: u8,
    pub updated_at: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct IssueThreads {
    #[serde(default)]
    pub threads: BTreeMap<String, ThreadState>,
    #[serde(default)]
    pub ledger: BTreeSet<i64>,
}

fn path_for(alias: &str, issue_iid: i64, root: Option<&Path>) -> PathBuf {
    root.map(Path::to_path_buf)
        .unwrap_or_else(default_memory_root)
        .join(alias)
        .join("mr-threads")
        .join(format!("{issue_iid}.json"))
}

impl IssueThreads {
    /// A missing or corrupt file yields an empty state.
    pub fn load(alias: &str, issue_iid: i64, root: Option<&Path>) -> IssueThreads {
        std::fs::read_to_string(path_for(alias, issue_iid, root))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, alias: &str, issue_iid: i64, root: Option<&Path>) -> io::Result<()> {
        let path = path_for(alias, issue_iid, root);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        std::fs::write(path, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_and_corrupt_file_is_empty() {
        let root = tempfile::tempdir().unwrap();
        let r = Some(root.path());
        assert_eq!(IssueThreads::load("p", 7, r), IssueThreads::default());

        let mut t = IssueThreads::default();
        t.ledger.insert(42);
        t.threads.insert(
            "abc".into(),
            ThreadState {
                discussion_id: "abc".into(),
                last_note_id: 9,
                status: ThreadStatus::WaitingOnReviewer,
                plan_summary: "rename x".into(),
                confidence: Some(80),
                attempts: 1,
                updated_at: "2026-11-02T00:00:00Z".into(),
            },
        );
        t.save("p", 7, r).unwrap();
        assert!(root.path().join("p/mr-threads/7.json").exists());
        assert_eq!(IssueThreads::load("p", 7, r), t);

        std::fs::write(root.path().join("p/mr-threads/7.json"), "{not json").unwrap();
        assert_eq!(IssueThreads::load("p", 7, r), IssueThreads::default());
    }
}
