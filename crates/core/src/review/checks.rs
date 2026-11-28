//! Pre-merge checks: plain-language rules in `.loopx/checks/<id>.md` (front matter
//! `name`, `paths`, `mode`) that the review model evaluates in the same call, plus
//! built-ins: `title` and `description` (model-free) and `issue_assessment` (the
//! linked issues, judged by the model). Results are advisory on others' merge requests.

use super::glob;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const DIR: &str = ".loopx/checks";
pub const MAX_FILES: usize = 20;
pub const MAX_FILE_BYTES: usize = 10 * 1024;
const MAX_INSTRUCTIONS: usize = 4_000;
const MAX_REASON: usize = 300;
pub const RESULTS: [&str; 3] = ["passed", "failed", "inconclusive"];
pub const MODES: [&str; 3] = ["off", "warning", "error"];

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub id: String,
    pub name: String,
    pub paths: Vec<String>,
    /// `warning` | `error`.
    pub mode: String,
    pub instructions: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Front {
    name: Option<String>,
    paths: Option<Vec<String>>,
    mode: Option<String>,
}

/// One `.loopx/checks/<file>`; `Err` says why it is ignored.
pub fn parse(file_name: &str, text: &str) -> Result<Check, String> {
    let id = file_name.strip_suffix(".md").ok_or("not a .md file")?;
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("file name must be letters, digits, - or _".into());
    }
    if text.len() > MAX_FILE_BYTES {
        return Err(format!("larger than {} KB", MAX_FILE_BYTES / 1024));
    }
    let (front, body) = match text.strip_prefix("---\n") {
        Some(rest) => {
            let (yaml, body) = rest
                .split_once("\n---")
                .ok_or("front matter is not closed with ---")?;
            let front: Front = serde_norway::from_str(yaml).map_err(|e| e.to_string())?;
            (front, body.trim_start_matches(['-', '\n']).to_string())
        }
        None => (Front::default(), text.to_string()),
    };
    let mode = front.mode.unwrap_or_else(|| "warning".into());
    if !MODES.contains(&mode.as_str()) {
        return Err(format!("mode must be off, warning or error, got {mode:?}"));
    }
    let instructions: String = body.trim().chars().take(MAX_INSTRUCTIONS).collect();
    if instructions.is_empty() {
        return Err("no instructions".into());
    }
    Ok(Check {
        id: id.to_string(),
        name: front.name.unwrap_or_else(|| id.replace(['-', '_'], " ")),
        paths: front.paths.unwrap_or_default(),
        mode,
        instructions,
    })
}

impl Check {
    /// A check with `paths` applies only when a changed file matches one.
    pub fn applies(&self, changed: &[String]) -> bool {
        self.mode != "off"
            && (self.paths.is_empty()
                || changed
                    .iter()
                    .any(|c| self.paths.iter().any(|p| glob::matches(p, c))))
    }

    pub fn prompt_json(&self) -> Value {
        json!({"id": self.id, "name": self.name, "instructions": self.instructions})
    }
}

pub fn issue_assessment(mode: &str) -> Check {
    Check {
        id: "issue_assessment".into(),
        name: "Does what the linked issue asks".into(),
        paths: Vec::new(),
        mode: mode.into(),
        instructions: "Using linked_issues, check that the change does what the linked issue(s) ask for, completely, and contains no unrelated changes. Inconclusive when the issues are too vague to judge.".into(),
    }
}

/// `#12` / `group/proj#12` references after closing keywords, plus issue URLs.
pub fn linked_issue_numbers(description: &str) -> Vec<u64> {
    let lower = description.to_lowercase();
    let mut out = Vec::new();
    for kw in [
        "close",
        "closes",
        "closed",
        "fix",
        "fixes",
        "fixed",
        "resolve",
        "resolves",
        "resolved",
        "implement",
        "implements",
    ] {
        let mut rest = lower.as_str();
        while let Some(i) = rest.find(kw) {
            let after = &rest[i + kw.len()..];
            let boundary = i == 0 || !rest.as_bytes()[i - 1].is_ascii_alphanumeric();
            let tail = after.trim_start_matches([':', ' ']);
            if boundary {
                if let Some(num) = tail.strip_prefix('#') {
                    let digits: String = num.chars().take_while(char::is_ascii_digit).collect();
                    if let Ok(n) = digits.parse() {
                        out.push(n);
                    }
                }
            }
            rest = after;
        }
    }
    for part in lower.split("/-/issues/").skip(1) {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(n) = digits.parse() {
            out.push(n);
        }
    }
    out.sort_unstable();
    out.dedup();
    out.truncate(3);
    out
}

/// Model-free built-ins: `(id, name, result, reason)`.
pub fn builtin_results(
    modes: &BTreeMap<String, String>,
    title: &str,
    description: &str,
) -> Vec<Value> {
    let mut out = Vec::new();
    let on = |k: &str| modes.get(k).filter(|m| *m != "off").cloned();
    if let Some(mode) = on("title") {
        let t = title.trim();
        let lower = t.to_lowercase();
        let reason = if t.chars().count() < 10 {
            Some("the title is too short to say what changes")
        } else if t.chars().count() > 100 {
            Some("the title is longer than 100 characters")
        } else if ["wip", "draft", "fix", "update", "changes"].contains(&lower.as_str()) {
            Some("the title does not say what changes")
        } else {
            None
        };
        out.push(result("title", "Title says what changes", &mode, reason));
    }
    if let Some(mode) = on("description") {
        let meaningful: String = description
            .lines()
            .filter(|l| {
                let l = l.trim();
                !l.is_empty()
                    && !l.starts_with('#')
                    && !l.starts_with("<!--")
                    && !l.starts_with("- [ ]")
            })
            .collect::<Vec<_>>()
            .join(" ");
        let reason = (meaningful.chars().count() < 30)
            .then_some("the description is empty or only the template");
        out.push(result(
            "description",
            "Description explains the change",
            &mode,
            reason,
        ));
    }
    out
}

fn result(id: &str, name: &str, mode: &str, failure: Option<&str>) -> Value {
    json!({"id": id, "name": name, "mode": mode,
           "result": if failure.is_some() { "failed" } else { "passed" },
           "reason": failure.unwrap_or("")})
}

/// The model's `check_results` for the checks it was given; a missing answer is
/// `inconclusive`.
pub fn model_results(answer: &Value, asked: &[Check]) -> Vec<Value> {
    asked
        .iter()
        .map(|c| {
            let got = answer["check_results"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| r["id"] == c.id.as_str());
            let res = got
                .and_then(|r| r["result"].as_str())
                .filter(|r| RESULTS.contains(r))
                .unwrap_or("inconclusive");
            let reason: String = got
                .and_then(|r| r["reason"].as_str())
                .unwrap_or(if got.is_none() { "not answered" } else { "" })
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(MAX_REASON)
                .collect();
            json!({"id": c.id, "name": c.name, "mode": c.mode, "result": res, "reason": reason})
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_files_parse_with_front_matter() {
        let c = parse("migrations-reversible.md", "---\nname: Migrations are reversible\npaths: ['db/migrate/**']\nmode: error\n---\nEvery migration defines `down`.\n").unwrap();
        assert_eq!(
            (c.id.as_str(), c.name.as_str(), c.mode.as_str()),
            (
                "migrations-reversible",
                "Migrations are reversible",
                "error"
            )
        );
        assert_eq!(c.paths, ["db/migrate/**"]);
        assert_eq!(c.instructions, "Every migration defines `down`.");
        let plain = parse("api_docs.md", "Public API changes update docs/api.md.").unwrap();
        assert_eq!(
            (plain.name.as_str(), plain.mode.as_str()),
            ("api docs", "warning")
        );
        assert!(parse("x.txt", "a").is_err());
        assert!(parse("bad name.md", "a").is_err());
        assert!(parse("x.md", "---\nmode: block\n---\nrule")
            .unwrap_err()
            .contains("mode"));
        assert!(parse("x.md", "---\nseverity: high\n---\nrule")
            .unwrap_err()
            .contains("unknown field"));
        assert!(parse("x.md", "---\nname: a\n---\n")
            .unwrap_err()
            .contains("no instructions"));
    }

    #[test]
    fn checks_apply_by_path_and_mode() {
        let c = parse("m.md", "---\npaths: ['db/**']\n---\nrule").unwrap();
        assert!(c.applies(&["db/migrate/1.rb".into()]));
        assert!(!c.applies(&["app/a.rb".into()]));
        let off = parse("o.md", "---\nmode: off\n---\nrule").unwrap();
        assert!(!off.applies(&["a".into()]));
    }

    #[test]
    fn linked_issues_from_closing_keywords_and_urls() {
        let d = "Closes #12 and fixes: #7.\nSee https://gitlab.example.com/g/p/-/issues/40 too. prefix#9 refix #5";
        assert_eq!(linked_issue_numbers(d), [7, 12, 40]);
        assert!(linked_issue_numbers("no links").is_empty());
    }

    #[test]
    fn builtins_are_model_free_and_opt_in() {
        let none = builtin_results(&BTreeMap::new(), "x", "");
        assert!(none.is_empty());
        let modes: BTreeMap<String, String> = [
            ("title".into(), "warning".into()),
            ("description".into(), "error".into()),
        ]
        .into();
        let r = builtin_results(&modes, "fix", "## Summary\n<!-- what -->\n- [ ] tests");
        assert_eq!(
            (r[0]["result"].as_str(), r[1]["result"].as_str()),
            (Some("failed"), Some("failed"))
        );
        assert_eq!(r[1]["mode"], "error");
        let r = builtin_results(
            &modes,
            "Add refunds to the admin order page",
            "Admins can refund an order from the order page; the gateway call is retried.",
        );
        assert!(r.iter().all(|x| x["result"] == "passed"));
    }

    #[test]
    fn model_results_cover_every_asked_check() {
        let asked = vec![
            parse("a.md", "rule a").unwrap(),
            parse("b.md", "rule b").unwrap(),
        ];
        let answer = json!({"check_results": [{"id": "a", "result": "failed", "reason": "no down"},
                                              {"id": "zzz", "result": "passed"}]});
        let r = model_results(&answer, &asked);
        assert_eq!(
            (r[0]["result"].as_str(), r[0]["reason"].as_str()),
            (Some("failed"), Some("no down"))
        );
        assert_eq!(
            (r[1]["result"].as_str(), r[1]["reason"].as_str()),
            (Some("inconclusive"), Some("not answered"))
        );
    }
}
