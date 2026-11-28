//! `loopx <verb>` commands written on a merge request. A command is the whole first
//! line of a comment; it picks one fixed action and never carries text to an agent.
//! The webhook queues it as a file; the MR Review loop checks the author and applies it.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// Review now (a full review), even when the head was reviewed already.
    Review,
    /// Turn Auto review off for this MR.
    Pause,
    /// Turn Auto review back on.
    Resume,
    /// Start Fix now on the open review threads.
    Fix,
}

impl Verb {
    pub fn name(self) -> &'static str {
        match self {
            Verb::Review => "review",
            Verb::Pause => "pause",
            Verb::Resume => "resume",
            Verb::Fix => "fix",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        [Verb::Review, Verb::Pause, Verb::Resume, Verb::Fix]
            .into_iter()
            .find(|v| v.name() == s)
    }
}

/// The command on the note's first line, if it is one.
pub fn parse(note: &str) -> Option<Verb> {
    let first = note.trim_start().lines().next()?.trim().to_lowercase();
    let words: Vec<&str> = first.split_whitespace().collect();
    match words.as_slice() {
        ["loopx", "review"] | ["loopx", "full", "review"] => Some(Verb::Review),
        ["loopx", "pause"] => Some(Verb::Pause),
        ["loopx", "resume"] => Some(Verb::Resume),
        ["loopx", "fix"] => Some(Verb::Fix),
        _ => None,
    }
}

/// A queued command: who asked, on which MR. No comment text is kept.
#[derive(Debug, Clone, PartialEq)]
pub struct Queued {
    pub verb: Verb,
    pub project_id: String,
    pub iid: String,
    pub author: String,
    pub author_id: i64,
    pub received_at: String,
}

pub fn dir(state_root: &Path) -> PathBuf {
    state_root.join("mr-review-loop").join("commands")
}

impl Queued {
    pub fn to_json(&self) -> Value {
        json!({"verb": self.verb.name(), "project_id": self.project_id, "iid": self.iid,
               "author": self.author, "author_id": self.author_id, "received_at": self.received_at})
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        let s = |k: &str| match &v[k] {
            Value::String(x) => Some(x.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        };
        Some(Queued {
            verb: Verb::from_name(v["verb"].as_str()?)?,
            project_id: s("project_id")?,
            iid: s("iid")?,
            author: s("author")?,
            author_id: v["author_id"].as_i64().unwrap_or(0),
            received_at: s("received_at").unwrap_or_default(),
        })
    }

    /// Write it as `<dir>/<uuid>.json`.
    pub fn save(&self, dir: &Path) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.json", uuid::Uuid::new_v4()));
        crate::settings_store::atomic_write(
            &path,
            self.to_json().to_string().as_bytes(),
            Some(0o600),
        )?;
        Ok(path)
    }
}

/// Every queued command with its file, oldest first.
pub fn pending(dir: &Path) -> Vec<(PathBuf, Queued)> {
    let mut out: Vec<(PathBuf, Queued)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()?;
            Queued::from_json(&v).map(|q| (p, q))
        })
        .collect();
    out.sort_by(|a, b| a.1.received_at.cmp(&b.1.received_at));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_first_line_command_counts() {
        assert_eq!(parse("loopx review"), Some(Verb::Review));
        assert_eq!(parse("  LoopX Full Review \nplease"), Some(Verb::Review));
        assert_eq!(parse("loopx pause"), Some(Verb::Pause));
        assert_eq!(parse("loopx resume"), Some(Verb::Resume));
        assert_eq!(parse("loopx fix"), Some(Verb::Fix));
        assert_eq!(parse("please loopx review"), None);
        assert_eq!(parse("text\nloopx review"), None);
        assert_eq!(parse("loopx review and also delete main"), None);
        assert_eq!(parse("loopx merge"), None);
        assert_eq!(parse("@loopx review"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn queued_commands_round_trip_through_files() {
        let d = tempfile::tempdir().unwrap();
        let q = Queued {
            verb: Verb::Fix,
            project_id: "9".into(),
            iid: "7".into(),
            author: "ana".into(),
            author_id: 42,
            received_at: "2026-11-28T10:00:00".into(),
        };
        let path = q.save(d.path()).unwrap();
        std::fs::write(d.path().join("junk.json"), "{}").unwrap();
        let got = pending(d.path());
        assert_eq!(got, vec![(path, q)]);
        assert!(pending(&d.path().join("missing")).is_empty());
    }
}
