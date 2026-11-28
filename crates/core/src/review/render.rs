//! Markdown for review findings and the overall review note.
use super::taxonomy::{AREAS, MAX_TITLE};
use crate::loops_config::py_str;
use serde_json::Value;

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Draft notes are Markdown on GitLab: break word-start @mentions so a
/// model-echoed @all / @group cannot notify anyone, and a line-start `/`
/// so it cannot run as a quick action once the user publishes the note.
pub fn defuse(text: &str) -> String {
    let mut out = String::new();
    let mut prev: Option<char> = None;
    let mut line_start = true;
    for c in text.chars() {
        if c == '/' && line_start {
            out.push('\u{200b}');
        }
        out.push(c);
        if c == '@' && !prev.is_some_and(is_word) {
            out.push('\u{200b}');
        }
        line_start = matches!(c, '\n' | '\r') || (line_start && c.is_whitespace());
        prev = Some(c);
    }
    out.replace("![", "!\u{200b}[")
}

pub(crate) fn flat_path(path: &str) -> String {
    crate::pre_review::one_line(path).replace('`', "'")
}

pub(crate) fn text_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

pub(crate) fn severity_badge(sev: &str) -> &'static str {
    match sev {
        "blocker" => "\u{1f534} **Blocker**",
        "major" => "\u{1f7e0} **Major**",
        "minor" => "\u{1f7e1} **Minor**",
        _ => "\u{26aa} **Nit**",
    }
}

pub(crate) fn area_label(key: &str) -> &'static str {
    AREAS
        .iter()
        .find(|(k, _)| *k == key)
        .map_or("Correctness", |(_, l)| l)
}

/// One line for a Markdown table cell.
pub(crate) fn cell(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// An inline draft note: severity and area up front, then the problem and
/// (when given) the fix, so it reads at a glance in the MR.
pub fn finding_note(f: &Value) -> String {
    let sev = text_of(f, "severity");
    let title = text_of(f, "title");
    let mut head = format!(
        "{} \u{b7} {}",
        severity_badge(&sev),
        area_label(&text_of(f, "category"))
    );
    if !title.is_empty() {
        head.push_str(&format!(" \u{2014} {}", defuse(&title)));
    }
    let mut note = format!("{head}\n\n{}", defuse(&text_of(f, "body")));
    let fix = text_of(f, "suggestion");
    if !fix.is_empty() {
        note.push_str(&format!("\n\n**Suggested fix:** {}", defuse(&fix)));
    }
    let tools: Vec<String> = f
        .get("hints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|h| h.split_once(':'))
        .map(|(t, r)| format!("{t} `{}`", r.replace('`', "'")))
        .collect();
    if !tools.is_empty() {
        note.push_str(&format!(
            "\n\n<sub>\u{1f9f0} Tools: {}</sub>",
            defuse(&tools.join(", "))
        ));
    }
    let agent = text_of(f, "fix_prompt");
    if !agent.is_empty() {
        note.push_str(&format!(
            "\n\n<details><summary>\u{1f916} Prompt for AI agents</summary>\n\n{}\n\n</details>",
            defuse(&agent)
        ));
    }
    if let Some(marker) = finding_marker(f) {
        note.push_str(&format!("\n\n{marker}"));
    }
    note
}

const MARKER_OPEN: &str = "<!-- loopx-finding ";

/// A LoopX review finding as recorded in its note's hidden marker.
#[derive(Debug, Clone, PartialEq)]
pub struct FindingRef {
    pub id: String,
    pub severity: String,
    pub category: String,
    pub path: String,
    pub line: i64,
    pub fix_prompt: String,
}

/// The hidden `<!-- loopx-finding {json} -->` line; `None` without an id. `-`, `>` and
/// `@` are JSON escapes, so no field can close the comment early or mention anyone.
pub fn finding_marker(f: &Value) -> Option<String> {
    let id = text_of(f, "id");
    if id.is_empty() {
        return None;
    }
    let data = serde_json::json!({"v": 1, "id": id, "sev": text_of(f, "severity"),
        "cat": text_of(f, "category"), "path": py_str(&f["path"]), "line": f["line"],
        "fix": text_of(f, "fix_prompt")});
    let json = data
        .to_string()
        .replace('-', "\\u002d")
        .replace('>', "\\u003e")
        .replace('@', "\\u0040");
    Some(format!("{MARKER_OPEN}{json} -->"))
}

/// The finding marker in a note body, if any (the first one wins).
pub fn parse_finding_marker(body: &str) -> Option<FindingRef> {
    let start = body.find(MARKER_OPEN)? + MARKER_OPEN.len();
    let end = start + body[start..].find(" -->")?;
    let v: Value = serde_json::from_str(&body[start..end]).ok()?;
    let id = v["id"].as_str().filter(|s| !s.is_empty())?;
    Some(FindingRef {
        id: id.to_string(),
        severity: text_of(&v, "sev"),
        category: text_of(&v, "cat"),
        path: text_of(&v, "path"),
        line: v["line"].as_i64().unwrap_or(0),
        fix_prompt: text_of(&v, "fix"),
    })
}

/// The overall review note: verdict, what the MR does, a risk table by area
/// and the findings index. `None` when the answer has nothing to say.
pub fn summary_note(answer: &Value) -> Option<String> {
    let summary = text_of(answer, "summary");
    let overview: Vec<String> = answer
        .get("overview")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(defuse)
        .collect();
    let risks: Vec<&Value> = answer
        .get("risks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let findings: Vec<&Value> = answer
        .get("findings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let checks: Vec<String> = answer
        .get("checks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|l| defuse(&crate::pre_review::one_line(l)))
        .filter(|l| !l.is_empty())
        .collect();
    if summary.is_empty()
        && overview.is_empty()
        && risks.is_empty()
        && findings.is_empty()
        && checks.is_empty()
    {
        return None;
    }
    let verdict = match answer.get("verdict").and_then(Value::as_str) {
        Some("request_changes") => "\u{26d4} Changes requested",
        Some("approve") => "\u{2705} Looks good to merge",
        _ => "\u{1f4ac} Mergeable after comments",
    };
    let mut out = vec![format!("### LoopX pre-review \u{2014} {verdict}")];
    if !summary.is_empty() {
        out.push(defuse(&summary));
    }
    let scope: Vec<String> = answer
        .get("scope")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|l| defuse(&crate::pre_review::one_line(l)))
        .filter(|l| !l.is_empty())
        .collect();
    if !scope.is_empty() {
        out.push(format!("**Review scope:** {}", scope.join("; ")));
    }
    if !checks.is_empty() {
        out.push(format!(
            "**Automated checks** (the same model-free gates LoopX runs on its own fixes; advisory)\n{}",
            checks.join("\n")
        ));
    }
    if !overview.is_empty() {
        let bullets: Vec<String> = overview.iter().map(|b| format!("- {b}")).collect();
        out.push(format!("**What this MR does**\n{}", bullets.join("\n")));
    }
    if risks.is_empty() {
        out.push("**Risk assessment:** no notable risks found.".to_string());
    } else {
        let mut table = vec![
            "**Risk assessment**".to_string(),
            String::new(),
            "| Area | Level | Notes |".to_string(),
            "| --- | --- | --- |".to_string(),
        ];
        for r in risks {
            let level = match text_of(r, "level").as_str() {
                "high" => "\u{1f534} High",
                "medium" => "\u{1f7e0} Medium",
                _ => "\u{1f7e2} Low",
            };
            table.push(format!(
                "| {} | {level} | {} |",
                area_label(&text_of(r, "area")),
                cell(&defuse(&text_of(r, "note")))
            ));
        }
        out.push(table.join("\n"));
    }
    if findings.is_empty() {
        out.push("**Findings:** none worth raising.".to_string());
    } else {
        let mut lines = vec!["**Findings** (posted as inline notes)".to_string()];
        for f in findings {
            let title = text_of(f, "title");
            let title = if title.is_empty() {
                text_of(f, "body").chars().take(MAX_TITLE).collect()
            } else {
                title
            };
            lines.push(format!(
                "- {} \u{b7} {} \u{2014} `{}:{}` {}",
                severity_badge(&text_of(f, "severity")),
                area_label(&text_of(f, "category")),
                flat_path(&py_str(&f["path"])),
                py_str(&f["line"]),
                cell(&defuse(&title))
            ));
        }
        out.push(lines.join("\n"));
    }
    Some(out.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finding_note_carries_the_agent_prompt_and_a_parseable_marker() {
        let f = json!({"id": "a1b2c3d4e5", "path": "db/m-1.rb", "line": 3, "severity": "major",
                       "category": "data", "title": "t", "body": "b",
                       "fix_prompt": "Add `lock_timeout` --> then @all /close"});
        let note = finding_note(&f);
        assert!(note.contains("<summary>\u{1f916} Prompt for AI agents</summary>"));
        let marker_line = note.lines().last().unwrap();
        assert!(marker_line.starts_with("<!-- loopx-finding "));
        assert_eq!(marker_line.matches("-->").count(), 1, "{marker_line}");
        let parsed = parse_finding_marker(&note).unwrap();
        assert_eq!(parsed.id, "a1b2c3d4e5");
        assert_eq!(parsed.path, "db/m-1.rb");
        assert_eq!(parsed.line, 3);
        assert_eq!(parsed.severity, "major");
        assert_eq!(parsed.fix_prompt, "Add `lock_timeout` --> then @all /close");
        assert!(!note.contains("then @all"), "the visible prompt is defused");
    }

    #[test]
    fn no_id_means_no_marker_and_garbage_markers_are_ignored() {
        let f =
            json!({"path": "a", "line": 1, "severity": "minor", "category": "tests", "body": "b"});
        assert!(!finding_note(&f).contains("loopx-finding"));
        assert!(!finding_note(&f).contains("<details>"));
        assert_eq!(parse_finding_marker("<!-- loopx-finding {nope -->"), None);
        assert_eq!(
            parse_finding_marker(r#"<!-- loopx-finding {"id": ""} -->"#),
            None
        );
    }
}
