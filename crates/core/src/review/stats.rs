//! Review quality from the MR Review findings ledgers: how many findings were posted,
//! how many the next commits addressed, and how many drew replies.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReviewStats {
    pub mrs: usize,
    pub findings: usize,
    pub addressed: usize,
    pub replied: usize,
    pub by_severity: BTreeMap<String, usize>,
}

impl ReviewStats {
    /// Share of findings a later commit addressed, `None` before any finding.
    pub fn addressed_rate(&self) -> Option<f64> {
        (self.findings > 0).then(|| self.addressed as f64 / self.findings as f64)
    }
}

pub fn compute(loops_root: &Path) -> ReviewStats {
    let dir = loops_root.join("mr-review-loop").join("findings");
    let mut s = ReviewStats::default();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let findings = v["findings"].as_array().cloned().unwrap_or_default();
        if findings.is_empty() {
            continue;
        }
        s.mrs += 1;
        for f in &findings {
            s.findings += 1;
            if f["status"] == "addressed" {
                s.addressed += 1;
            }
            if f["replies"].as_array().is_some_and(|r| !r.is_empty()) {
                s.replied += 1;
            }
            let sev = f["severity"].as_str().unwrap_or("minor").to_string();
            *s.by_severity.entry(sev).or_default() += 1;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_findings_addressed_and_replied_across_ledgers() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(compute(d.path()), ReviewStats::default());
        let dir = d.path().join("mr-review-loop/findings");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.json"),
            r#"{"findings": [
            {"id": "1", "severity": "major", "status": "addressed"},
            {"id": "2", "severity": "minor", "status": "open", "replies": [{"note_id": 1}]}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("b.json"),
            r#"{"findings": [{"id": "3", "severity": "major", "status": "open"}]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("empty.json"), r#"{"findings": []}"#).unwrap();
        let s = compute(d.path());
        assert_eq!((s.mrs, s.findings, s.addressed, s.replied), (2, 3, 1, 1));
        assert_eq!(s.by_severity["major"], 2);
        assert!((s.addressed_rate().unwrap() - 1.0 / 3.0).abs() < 1e-9);
    }
}
