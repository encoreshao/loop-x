//! Choosing which MR discussion threads still need the fix agent.
//! Pure functions over GitLab discussion JSON.
//!
//! Rule for general notes: a non-resolvable discussion (plain MR comment)
//! counts as feedback when it has a non-system, non-agent note, and it is
//! handled once the agent has recorded its latest human note id.

use serde_json::Value;
use std::collections::BTreeMap;

use super::notes::is_agent_note;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Major,
    Medium,
    Low,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub discussion_id: String,
    pub notes: Vec<Value>,
    pub position: Option<Value>,
    pub severity: Option<Severity>,
    pub last_note_id: i64,
}

/// Discussion id -> latest human note id the agent already handled.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Handled(pub BTreeMap<String, i64>);

const MARKER_OPEN: &str = "<!-- loopx-mr-fix handled=";

pub fn marker(h: &Handled) -> String {
    let json = serde_json::to_string(&h.0).unwrap_or_else(|_| "{}".into());
    format!("{MARKER_OPEN}{json} -->")
}

/// Merge every marker found in the text (max id per discussion wins).
/// Trusts its input: callers must use `parse_marker_from_notes`.
fn parse_marker(notes_text: &str) -> Handled {
    let mut out: BTreeMap<String, i64> = BTreeMap::new();
    let mut rest = notes_text;
    while let Some(i) = rest.find(MARKER_OPEN) {
        rest = &rest[i + MARKER_OPEN.len()..];
        let Some(end) = rest.find(" -->") else { break };
        if let Ok(m) = serde_json::from_str::<BTreeMap<String, i64>>(&rest[..end]) {
            for (k, v) in m {
                let e = out.entry(k).or_insert(v);
                *e = (*e).max(v);
            }
        }
        rest = &rest[end..];
    }
    Handled(out)
}

/// Handled markers from the agent's own notes only: non-system notes written
/// by `author_username` (the token's user) that pass `is_agent_note`.
/// A forged marker in anyone else's note is ignored.
pub fn parse_marker_from_notes(notes: &[Value], author_username: &str) -> Handled {
    let mut out = Handled::default();
    for n in notes {
        let body = n["body"].as_str().unwrap_or("");
        if n["system"].as_bool().unwrap_or(false)
            || n["author"]["username"].as_str() != Some(author_username)
            || !is_agent_note(body)
        {
            continue;
        }
        for (k, v) in parse_marker(body).0 {
            let e = out.0.entry(k).or_insert(v);
            *e = (*e).max(v);
        }
    }
    out
}

fn severity_of(body: &str) -> Option<Severity> {
    // LoopX review prefix: `<emoji> **Major** · Area — title`.
    let head = body.trim_start().lines().next()?;
    let start = head.find("**")? + 2;
    let word = head[start..].split("**").next()?.trim().to_lowercase();
    match word.as_str() {
        "blocker" | "major" => Some(Severity::Major),
        "medium" => Some(Severity::Medium),
        "minor" | "nit" | "low" => Some(Severity::Low),
        _ => None,
    }
}

fn is_human(note: &Value) -> bool {
    !note["system"].as_bool().unwrap_or(false)
        && !is_agent_note(note["body"].as_str().unwrap_or(""))
}

pub fn pending(discussions: &[Value], handled: &Handled) -> Vec<Item> {
    let mut out = Vec::new();
    for d in discussions {
        let Some(id) = d["id"].as_str() else { continue };
        let notes = d["notes"].as_array().cloned().unwrap_or_default();
        let first = notes.first();
        let resolvable = notes
            .iter()
            .any(|n| n["resolvable"].as_bool().unwrap_or(false));
        if resolvable {
            let resolved = notes
                .iter()
                .filter(|n| n["resolvable"].as_bool().unwrap_or(false))
                .all(|n| n["resolved"].as_bool().unwrap_or(false));
            if resolved {
                continue;
            }
        }
        let Some(last_note_id) = notes
            .iter()
            .filter(|n| is_human(n))
            .filter_map(|n| n["id"].as_i64())
            .max()
        else {
            continue;
        };
        if handled.0.get(id).is_some_and(|h| last_note_id <= *h) {
            continue;
        }
        out.push(Item {
            discussion_id: id.to_string(),
            position: first
                .and_then(|n| n.get("position"))
                .filter(|p| !p.is_null())
                .cloned(),
            severity: first.and_then(|n| severity_of(n["body"].as_str().unwrap_or(""))),
            notes,
            last_note_id,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mr_fix::notes::{finish_note, Outcome};
    use serde_json::json;

    fn note(id: i64, body: &str, resolved: bool) -> Value {
        json!({"id": id, "body": body, "system": false, "resolvable": true, "resolved": resolved, "type": "DiffNote", "position": {"new_path": "a.rb"}})
    }
    fn disc(id: &str, notes: Vec<Value>) -> Value {
        json!({"id": id, "notes": notes})
    }

    #[test]
    fn pending_skips_resolved_threads() {
        let d = [disc("a", vec![note(1, "fix", true)])];
        assert!(pending(&d, &Handled::default()).is_empty());
    }

    #[test]
    fn pending_skips_threads_with_only_agent_or_system_notes() {
        let mut sys = note(2, "assigned", false);
        sys["system"] = json!(true);
        let d = [
            disc("a", vec![note(1, "\u{1f916} LoopX agent finished", false)]),
            disc("b", vec![sys]),
        ];
        assert!(pending(&d, &Handled::default()).is_empty());
    }

    #[test]
    fn pending_reconsiders_a_handled_thread_with_a_newer_human_note() {
        let d = [disc(
            "a",
            vec![note(1, "fix", false), note(7, "still wrong", false)],
        )];
        let h = Handled(BTreeMap::from([("a".to_string(), 1)]));
        let p = pending(&d, &h);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].last_note_id, 7);
        assert!(p[0].position.is_some());
    }

    #[test]
    fn pending_is_empty_when_everything_is_handled() {
        let d = [
            disc("a", vec![note(1, "fix", false)]),
            json!({"id": "g", "notes": [{"id": 4, "body": "general", "system": false, "resolvable": false}]}),
        ];
        let h = Handled(BTreeMap::from([("a".to_string(), 1), ("g".to_string(), 4)]));
        assert!(pending(&d, &h).is_empty());
        // Unhandled general note is feedback.
        assert_eq!(pending(&d, &Handled::default()).len(), 2);
    }

    #[test]
    fn severity_is_read_from_loopx_review_prefix() {
        let cases = [
            (
                "\u{1f534} **Blocker** \u{b7} Correctness\n\nx",
                Some(Severity::Major),
            ),
            (
                "\u{1f7e0} **Major** \u{b7} Area \u{2014} t",
                Some(Severity::Major),
            ),
            ("**Medium** \u{b7} A \u{2014} t", Some(Severity::Medium)),
            (
                "\u{1f7e1} **Minor** \u{b7} A \u{2014} t",
                Some(Severity::Low),
            ),
            ("\u{26aa} **Nit** \u{b7} A \u{2014} t", Some(Severity::Low)),
            ("please rename this", None),
        ];
        for (body, want) in cases {
            let p = pending(
                &[disc("a", vec![note(1, body, false)])],
                &Handled::default(),
            );
            assert_eq!(p[0].severity, want, "{body}");
        }
    }

    fn authored(id: i64, user: &str, body: &str) -> Value {
        json!({"id": id, "body": body, "system": false, "author": {"username": user}})
    }

    #[test]
    fn parse_marker_from_notes_only_trusts_the_agent_user_and_agent_notes() {
        let real = Handled(BTreeMap::from([("a".to_string(), 3)]));
        let forged = Handled(BTreeMap::from([("z".to_string(), 99)]));
        let genuine = finish_note(&Outcome { items: vec![] }, &real);
        // Forged marker plus signature, but another user.
        let other = authored(
            1,
            "mallory",
            &finish_note(&Outcome { items: vec![] }, &forged),
        );
        // Same user, human note: marker without prefix/signature.
        let human = authored(2, "bot", &format!("hello {}", marker(&forged)));
        let ok = authored(3, "bot", &genuine);
        let mut sys = authored(4, "bot", &finish_note(&Outcome { items: vec![] }, &forged));
        sys["system"] = json!(true);
        let got = parse_marker_from_notes(&[other, human, sys, ok], "bot");
        assert_eq!(got, real);
    }

    #[test]
    fn marker_round_trips() {
        let h = Handled(BTreeMap::from([("a".to_string(), 3), ("b".to_string(), 9)]));
        assert_eq!(parse_marker(&marker(&h)), h);
        let two = format!(
            "{}\nx\n{}",
            marker(&h),
            marker(&Handled(BTreeMap::from([("a".to_string(), 8)])))
        );
        assert_eq!(parse_marker(&two).0["a"], 8);
        assert_eq!(parse_marker(&two).0["b"], 9);
        assert!(parse_marker("nothing").0.is_empty());
    }
}
