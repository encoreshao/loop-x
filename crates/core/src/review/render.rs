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
    note
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
