//! Per-merge-request record of what the review posted:
//! `<state root>/mr-review-loop/findings/<mr>.json`. It lets the next review look only
//! at new commits, recognise earlier findings by id, and say which ones the new
//! commits addressed.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Entry {
    pub id: String,
    pub path: String,
    pub line: i64,
    pub severity: String,
    pub title: String,
    /// Head SHA of the review that first raised it.
    pub sha: String,
    /// `open` | `addressed` | `closed` (kept after a full review for its replies).
    pub status: String,
    pub addressed_sha: Option<String>,
    /// Replies people wrote in the finding's thread, once it was published.
    pub replies: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Ledger {
    pub last_sha: Option<String>,
    /// Incremental reviews since the last full one.
    pub delta_reviews: u32,
    pub findings: Vec<Entry>,
}

fn slug(mr_ref: &str) -> String {
    mr_ref
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn path(state_root: &Path, mr_ref: &str) -> PathBuf {
    state_root
        .join("mr-review-loop")
        .join("findings")
        .join(format!("{}.json", slug(mr_ref)))
}

impl Entry {
    fn from_json(v: &Value) -> Option<Self> {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let id = s("id");
        (!id.is_empty()).then(|| Entry {
            id,
            path: s("path"),
            line: v["line"].as_i64().unwrap_or(0),
            severity: s("severity"),
            title: s("title"),
            sha: s("sha"),
            status: if s("status").is_empty() {
                "open".into()
            } else {
                s("status")
            },
            addressed_sha: v["addressed_sha"].as_str().map(str::to_string),
            replies: v["replies"].as_array().cloned().unwrap_or_default(),
        })
    }

    fn to_json(&self) -> Value {
        json!({"id": self.id, "path": self.path, "line": self.line, "severity": self.severity,
               "title": self.title, "sha": self.sha, "status": self.status,
               "addressed_sha": self.addressed_sha, "replies": self.replies})
    }

    /// What the model sees of an earlier finding.
    pub fn prompt_json(&self) -> Value {
        json!({"id": self.id, "path": self.path, "line": self.line, "severity": self.severity,
               "title": self.title})
    }
}

impl Ledger {
    pub fn load(state_root: &Path, mr_ref: &str) -> Self {
        let Ok(text) = std::fs::read_to_string(path(state_root, mr_ref)) else {
            return Self::default();
        };
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ledger {
            last_sha: v["last_sha"].as_str().map(str::to_string),
            delta_reviews: v["delta_reviews"].as_u64().unwrap_or(0) as u32,
            findings: v["findings"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Entry::from_json)
                .collect(),
        }
    }

    pub fn save(&self, state_root: &Path, mr_ref: &str) -> std::io::Result<()> {
        let p = path(state_root, mr_ref);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let data = json!({"v": 1, "last_sha": self.last_sha, "delta_reviews": self.delta_reviews,
                          "findings": self.findings.iter().map(Entry::to_json).collect::<Vec<_>>()});
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&data)?)?;
        std::fs::rename(tmp, p)
    }

    pub fn open(&self) -> Vec<&Entry> {
        self.findings
            .iter()
            .filter(|e| e.status == "open")
            .collect()
    }

    /// After a review of `head`: mark `addressed` ids, add the newly posted findings.
    /// A full review starts the list over.
    pub fn record(&mut self, head: &str, full: bool, addressed: &[String], posted: &[Value]) {
        if full {
            // Replies are kept for the weekly learning pass; the rest starts over.
            self.findings.retain(|e| !e.replies.is_empty());
            for e in &mut self.findings {
                e.status = "closed".into();
            }
            self.delta_reviews = 0;
        } else {
            self.delta_reviews += 1;
        }
        for e in self.findings.iter_mut().filter(|e| e.status == "open") {
            if addressed.contains(&e.id) {
                e.status = "addressed".into();
                e.addressed_sha = Some(head.to_string());
            }
        }
        for f in posted {
            let id = f["id"].as_str().unwrap_or("");
            if id.is_empty()
                || self
                    .findings
                    .iter()
                    .any(|e| e.id == id && e.status == "open")
            {
                continue;
            }
            self.findings.push(Entry {
                id: id.to_string(),
                path: f["path"].as_str().unwrap_or("").to_string(),
                line: f["line"].as_i64().unwrap_or(0),
                severity: f["severity"].as_str().unwrap_or("").to_string(),
                title: f["title"].as_str().unwrap_or("").to_string(),
                sha: head.to_string(),
                status: "open".into(),
                addressed_sha: None,
                replies: Vec::new(),
            });
        }
        self.last_sha = Some(head.to_string());
    }
}

pub const MAX_REPLY_CHARS: usize = 1000;

impl Ledger {
    /// Record replies from the MR's discussions: every later, human note in a thread
    /// opened by one of our findings (`is_ours` tells LoopX's own notes apart).
    pub fn add_replies(&mut self, discussions: &[Value], is_ours: &dyn Fn(&str) -> bool) -> usize {
        let mut added = 0;
        for d in discussions {
            let notes = d["notes"].as_array().cloned().unwrap_or_default();
            let Some(first) = notes.first() else { continue };
            let Some(marker) =
                super::render::parse_finding_marker(first["body"].as_str().unwrap_or(""))
            else {
                continue;
            };
            let Some(entry) = self.findings.iter_mut().find(|e| e.id == marker.id) else {
                continue;
            };
            for n in notes.iter().skip(1) {
                let body = n["body"].as_str().unwrap_or("");
                let id = n["id"].clone();
                if n["system"].as_bool().unwrap_or(false)
                    || body.trim().is_empty()
                    || is_ours(body)
                    || entry.replies.iter().any(|r| r["note_id"] == id)
                {
                    continue;
                }
                entry.replies.push(json!({"note_id": id,
                    "author": n["author"]["username"].as_str().unwrap_or(""),
                    "text": body.chars().take(MAX_REPLY_CHARS).collect::<String>()}));
                added += 1;
            }
        }
        added
    }
}

/// The model's `previous` answer: ids it says the new commits addressed (only ids
/// that are open in the ledger count).
pub fn addressed_ids(answer: &Value, ledger: &Ledger) -> Vec<String> {
    let open: Vec<&str> = ledger.open().iter().map(|e| e.id.as_str()).collect();
    answer["previous"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["status"] == "addressed")
        .filter_map(|p| p["id"].as_str())
        .filter(|id| open.contains(id))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(id: &str) -> Value {
        json!({"id": id, "path": "a.rb", "line": 3, "severity": "major", "title": format!("t{id}")})
    }

    #[test]
    fn round_trips_and_records_reviews() {
        let d = tempfile::tempdir().unwrap();
        let mut l = Ledger::load(d.path(), "work:9!7");
        assert_eq!(l, Ledger::default());
        l.record("h1", true, &[], &[f("a"), f("b")]);
        l.save(d.path(), "work:9!7").unwrap();
        assert!(path(d.path(), "work:9!7").ends_with("mr-review-loop/findings/work_9_7.json"));
        let mut l = Ledger::load(d.path(), "work:9!7");
        assert_eq!(l.last_sha.as_deref(), Some("h1"));
        assert_eq!(l.open().len(), 2);
        l.record("h2", false, &["a".into()], &[f("b"), f("c")]);
        assert_eq!(l.delta_reviews, 1);
        let a = l.findings.iter().find(|e| e.id == "a").unwrap();
        assert_eq!(
            (a.status.as_str(), a.addressed_sha.as_deref()),
            ("addressed", Some("h2"))
        );
        let ids: Vec<&str> = l.open().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["b", "c"], "b is not duplicated");
        l.record("h3", true, &[], &[f("z")]);
        assert_eq!((l.findings.len(), l.delta_reviews), (1, 0));
    }

    #[test]
    fn replies_to_our_findings_are_recorded_once() {
        let mut l = Ledger::default();
        l.record("h1", true, &[], &[f("a1b2c3d4e5")]);
        let ours = super::super::render::finding_note(
            &json!({"id": "a1b2c3d4e5", "path": "a.rb", "line": 3,
            "severity": "major", "category": "data", "body": "b"}),
        );
        let discussions = json!([
            {"id": "d1", "notes": [{"id": 1, "body": ours},
                                   {"id": 2, "body": "Small table, lock_timeout is not needed here.", "author": {"username": "ana"}},
                                   {"id": 3, "body": "Looks addressed in `h`.\n\n_LOOPX_", "author": {"username": "me"}},
                                   {"id": 4, "body": "assigned", "system": true}]},
            {"id": "d2", "notes": [{"id": 5, "body": "plain thread"}, {"id": 6, "body": "reply"}]}
        ]);
        let is_ours = |b: &str| b.ends_with("_LOOPX_");
        assert_eq!(l.add_replies(discussions.as_array().unwrap(), &is_ours), 1);
        assert_eq!(
            l.add_replies(discussions.as_array().unwrap(), &is_ours),
            0,
            "deduped"
        );
        let r = &l.findings[0].replies[0];
        assert_eq!(
            (r["note_id"].as_i64(), r["author"].as_str()),
            (Some(2), Some("ana"))
        );
    }

    #[test]
    fn only_open_ids_count_as_addressed() {
        let mut l = Ledger::default();
        l.record("h1", true, &[], &[f("a"), f("b")]);
        l.record("h2", false, &["b".into()], &[]);
        let answer = json!({"previous": [{"id": "a", "status": "addressed"}, {"id": "b", "status": "addressed"},
                                         {"id": "x", "status": "addressed"}, {"id": "a", "status": "open"}]});
        assert_eq!(addressed_ids(&answer, &l), ["a"]);
        assert!(addressed_ids(&json!({}), &l).is_empty());
    }
}
