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
    /// `open` | `addressed`.
    pub status: String,
    pub addressed_sha: Option<String>,
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
        })
    }

    fn to_json(&self) -> Value {
        json!({"id": self.id, "path": self.path, "line": self.line, "severity": self.severity,
               "title": self.title, "sha": self.sha, "status": self.status,
               "addressed_sha": self.addressed_sha})
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
            self.findings.clear();
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
            });
        }
        self.last_sha = Some(head.to_string());
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
