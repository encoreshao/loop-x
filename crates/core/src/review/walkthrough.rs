//! The optional walkthrough in the review summary: the change grouped by purpose, an
//! optional Mermaid sequence diagram, a 1-5 review effort and a merge risk. The model's
//! answer is cleaned and the diagram reduced to a safe subset of `sequenceDiagram`.

use serde_json::{json, Value};

pub const MAX_GROUPS: usize = 8;
const MAX_TEXT: usize = 200;
const MAX_PATHS: usize = 12;
const MAX_DIAGRAM_LINES: usize = 40;
pub const RISKS: [&str; 5] = ["minimal", "low", "moderate", "high", "critical"];

fn text(v: &Value, cap: usize) -> String {
    v.as_str()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(cap)
        .collect()
}

/// A `sequenceDiagram` with only participants, messages, notes and blocks; anything
/// else (links, `click`, init directives, HTML) drops the whole diagram.
pub fn sanitize_mermaid(src: &str) -> Option<String> {
    let lines: Vec<&str> = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.first() != Some(&"sequenceDiagram")
        || lines.len() < 2
        || lines.len() > MAX_DIAGRAM_LINES
    {
        return None;
    }
    let banned = ['<', '>', '`', '{', '}', '"', '\\'];
    let ok = |l: &str| {
        let lower = l.to_lowercase();
        if lower.contains("click")
            || lower.contains("http")
            || lower.contains("%%")
            || lower.contains("href")
        {
            return false;
        }
        let words = [
            "participant ",
            "actor ",
            "note ",
            "alt ",
            "else",
            "opt ",
            "loop ",
            "par ",
            "and ",
            "end",
            "autonumber",
        ];
        if words
            .iter()
            .any(|w| lower.starts_with(w) || lower == w.trim())
        {
            return !l.chars().any(|c| banned.contains(&c));
        }
        let arrow = ["-->>", "->>", "--x", "-x", "--)", "-)", "-->", "->"]
            .iter()
            .find(|a| l.contains(**a));
        let Some(a) = arrow else { return false };
        let (from, rest) = l.split_once(a).unwrap_or(("", ""));
        let (to, msg) = rest.split_once(':').unwrap_or((rest, ""));
        let ident = |s: &str| {
            let s = s.trim().trim_start_matches(['+', '-']);
            !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')
        };
        ident(from) && ident(to) && !msg.chars().any(|c| banned.contains(&c) || c == '>')
    };
    lines[1..].iter().all(|l| ok(l)).then(|| lines.join("\n"))
}

/// The cleaned `walkthrough` from the model's answer, or `None` when absent or empty.
pub fn clean(answer: &Value, changed_paths: &[&str]) -> Option<Value> {
    let w = answer.get("walkthrough").filter(|v| v.is_object())?;
    let groups: Vec<Value> = w["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| {
            let title = text(&g["title"], 80);
            let paths: Vec<&str> = g["paths"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|p| changed_paths.contains(p))
                .take(MAX_PATHS)
                .collect();
            (!title.is_empty()).then(|| json!({"title": title, "paths": paths, "summary": text(&g["summary"], MAX_TEXT)}))
        })
        .take(MAX_GROUPS)
        .collect();
    let effort = w["effort"].as_u64().filter(|e| (1..=5).contains(e));
    let risk = w["merge_risk"].as_str().filter(|r| RISKS.contains(r));
    let sequence = w["sequence"].as_str().and_then(sanitize_mermaid);
    if groups.is_empty() && effort.is_none() && risk.is_none() {
        return None;
    }
    Some(
        json!({"groups": groups, "effort": effort, "merge_risk": risk,
                "merge_risk_reason": text(&w["merge_risk_reason"], MAX_TEXT), "sequence": sequence}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mermaid_keeps_a_plain_sequence_and_drops_anything_risky() {
        let good = "sequenceDiagram\n  participant API\n  Client->>API: POST /orders\n  API-->>Client: 201\n  alt invalid\n    API--xClient: 422\n  end";
        assert!(sanitize_mermaid(good).is_some());
        for bad in [
            "graph TD\nA-->B",
            "sequenceDiagram\nclick API callback",
            "sequenceDiagram\nA->>B: <img src=x>",
            "sequenceDiagram\n%%{init: {}}%%\nA->>B: x",
            "sequenceDiagram\nA->>B: see https://x",
            "sequenceDiagram",
            "sequenceDiagram\nrm -rf /",
        ] {
            assert_eq!(sanitize_mermaid(bad), None, "{bad}");
        }
    }

    #[test]
    fn clean_keeps_known_paths_and_valid_fields() {
        let answer = json!({"walkthrough": {
            "groups": [{"title": "Billing API", "paths": ["app/a.rb", "ghost.rb"], "summary": "Adds  refunds"},
                       {"title": "", "paths": ["app/a.rb"]}],
            "effort": 3, "merge_risk": "moderate", "merge_risk_reason": "touches payments",
            "sequence": "sequenceDiagram\nA->>B: refund"}});
        let w = clean(&answer, &["app/a.rb"]).unwrap();
        assert_eq!(
            w["groups"],
            json!([{"title": "Billing API", "paths": ["app/a.rb"], "summary": "Adds refunds"}])
        );
        assert_eq!(
            (w["effort"].as_u64(), w["merge_risk"].as_str()),
            (Some(3), Some("moderate"))
        );
        assert!(w["sequence"].is_string());
        let bad = json!({"walkthrough": {"effort": 9, "merge_risk": "huge", "groups": []}});
        assert_eq!(clean(&bad, &[]), None);
        assert_eq!(clean(&json!({}), &[]), None);
    }
}
