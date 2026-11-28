//! Turn the model's review answer into the validated, capped shape every renderer uses.
use super::taxonomy::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// Stable across line shifts and re-reviews of the same problem: path, area and the
/// normalised title (the body's start when there is no title).
pub fn finding_id(path: &str, category: &str, title: &str) -> String {
    let words = title
        .chars()
        .take(MAX_TITLE)
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    crate::loopkit::sha1_hex(format!("{path}|{category}|{words}").as_bytes())[..10].to_string()
}

pub struct Cleaned {
    pub summary: String,
    pub verdict: &'static str,
    pub overview: Vec<String>,
    pub risks: Vec<Value>,
    /// Most severe first, not yet capped (gate findings are prepended by the caller).
    pub findings: Vec<Value>,
}

/// `allowed` = paths in the diff; findings elsewhere, below `min_severity`, or malformed are dropped.
pub fn clean(answer: &Value, allowed: &BTreeSet<&str>, min_severity: &str) -> Cleaned {
    let floor = sev_rank(min_severity);
    let mut kept: Vec<Value> = Vec::new();
    for f in answer
        .get("findings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(obj) = f.as_object() else { continue };
        let line = obj
            .get("line")
            .and_then(|v| if v.is_boolean() { None } else { v.as_i64() });
        let sev = obj.get("severity").and_then(Value::as_str);
        let body = obj.get("body").and_then(Value::as_str);
        let path = obj.get("path").and_then(Value::as_str);
        let (Some(path), Some(line), Some(sev), Some(body)) = (path, line, sev, body) else {
            continue;
        };
        if !allowed.contains(path)
            || line < 1
            || !SEVERITIES.contains(&sev)
            || sev_rank(sev) < floor
            || body.trim().is_empty()
        {
            continue;
        }
        let short = |k: &str, cap: usize| {
            obj.get(k)
                .and_then(Value::as_str)
                .map(|t| t.trim().chars().take(cap).collect::<String>())
                .unwrap_or_default()
        };
        let category = obj
            .get("category")
            .and_then(Value::as_str)
            .filter(|c| AREAS.iter().any(|(k, _)| k == c))
            .unwrap_or("correctness");
        let title = short("title", MAX_TITLE);
        let body: String = body.trim().chars().take(MAX_BODY).collect();
        kept.push(json!({
            "id": finding_id(path, category, if title.is_empty() { &body } else { &title }),
            "path": path, "line": line, "severity": sev, "category": category,
            "title": title,
            "body": body,
            "suggestion": short("suggestion", MAX_BODY),
            "fix_prompt": short("fix_prompt", MAX_FIX_PROMPT),
            "hints": obj.get("hints").and_then(Value::as_array).into_iter().flatten()
                .filter_map(Value::as_str)
                .filter(|h| h.split_once(':').is_some_and(|(t, r)| !r.is_empty()
                    && super::tools::ALL.iter().any(|x| x.name() == t)))
                .take(5)
                .collect::<Vec<_>>(),
        }));
    }
    // Most severe first, so the cap keeps what matters.
    kept.sort_by_key(|f| std::cmp::Reverse(sev_rank(f["severity"].as_str().unwrap_or(""))));
    let summary = answer
        .get("summary")
        .and_then(Value::as_str)
        .map(|s| s.trim().chars().take(MAX_SUMMARY).collect::<String>())
        .unwrap_or_default();
    let verdict = answer
        .get("verdict")
        .and_then(Value::as_str)
        .and_then(|v| VERDICTS.iter().copied().find(|k| *k == v))
        .unwrap_or("comment");
    let overview: Vec<String> = answer
        .get("overview")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|b| b.trim().chars().take(MAX_NOTE).collect::<String>())
        .filter(|b| !b.is_empty())
        .take(MAX_OVERVIEW)
        .collect();
    let mut risks: Vec<Value> = answer
            .get("risks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| {
                let area = r.get("area")?.as_str()?;
                let level = r.get("level")?.as_str()?;
                let note = r.get("note")?.as_str()?.trim();
                (AREAS.iter().any(|(k, _)| *k == area) && LEVELS.contains(&level) && !note.is_empty())
                    .then(|| json!({"area": area, "level": level, "note": note.chars().take(MAX_NOTE).collect::<String>()}))
            })
            .collect();
    risks.sort_by_key(|r| {
        std::cmp::Reverse(LEVELS.iter().position(|l| r["level"] == *l).unwrap_or(0))
    });
    risks.truncate(MAX_RISKS);
    Cleaned {
        summary,
        verdict,
        overview,
        risks,
        findings: kept,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn findings_get_a_stable_id_and_a_capped_fix_prompt() {
        let allowed: BTreeSet<&str> = ["a.rb"].into();
        let answer = |line: i64, title: &str| {
            json!({"verdict": "comment", "findings": [{"path": "a.rb", "line": line, "severity": "major",
                   "category": "data", "title": title, "body": "b", "fix_prompt": "x".repeat(900)}]})
        };
        let one = clean(&answer(3, "No  lock_timeout"), &allowed, "minor").findings;
        let moved = clean(&answer(40, "no lock_timeout"), &allowed, "minor").findings;
        assert_eq!(one[0]["id"], moved[0]["id"]);
        assert_eq!(one[0]["id"].as_str().unwrap().len(), 10);
        assert_eq!(
            one[0]["fix_prompt"].as_str().unwrap().chars().count(),
            MAX_FIX_PROMPT
        );
        let other = clean(&answer(3, "Missing index"), &allowed, "minor").findings;
        assert_ne!(one[0]["id"], other[0]["id"]);
        let untitled =
            json!({"findings": [{"path": "a.rb", "line": 1, "severity": "major", "body": "b"}]});
        let f = &clean(&untitled, &allowed, "minor").findings[0];
        assert_eq!(f["id"], json!(finding_id("a.rb", "correctness", "b")));
        assert_eq!(f["fix_prompt"], "");
    }
}
