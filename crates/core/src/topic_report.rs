//! A Topic Monitor briefing as data: the agent saves it with
//! `loopx topic report <name> '<json>'`, and the runner posts it as one Slack
//! report when the topic's session ends - so the message carries the real run
//! time and still goes out (saying so) when the session fails.

use crate::slack_report::{domain, fmt_duration, Item, Link, Report, Section, State, Status};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Set on the topic agent's environment: where `loopx topic report` writes.
pub const PATH_ENV: &str = "LOOPX_TOPIC_REPORT";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Briefing {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub items: Vec<BriefingItem>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BriefingItem {
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub url: String,
}

/// `<root>/outputs/topic-monitor/reports/<name>.json`: the runner clears it
/// before each session, so it only ever holds the current run's briefing.
pub fn default_path(root: &Path, name: &str) -> PathBuf {
    root.join("outputs")
        .join("topic-monitor")
        .join("reports")
        .join(format!("{name}.json"))
}

/// Validate `json` as a briefing and write it to `path`.
pub fn save(path: &Path, json: &str) -> Result<Briefing, String> {
    let briefing: Briefing = serde_json::from_str(json).map_err(|e| {
        format!("Not a briefing ({e}). Expected {{\"summary\": \"...\", \"items\": [{{\"title\": \"...\", \"description\": \"...\", \"url\": \"https://...\"}}]}}")
    })?;
    if let Some(bad) = briefing
        .items
        .iter()
        .find(|i| !(i.url.starts_with("https://") || i.url.starts_with("http://")))
    {
        return Err(format!("Item \"{}\" needs an http(s) url", bad.title));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(&briefing).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(briefing)
}

pub fn load(path: &Path) -> Option<Briefing> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// How the topic's session ended, as the runner saw it.
pub enum Ending<'a> {
    Completed,
    Failed(&'a str),
}

pub fn build(
    label: &str,
    date: &str,
    briefing: Option<&Briefing>,
    ending: Ending,
    elapsed_seconds: f64,
) -> Report {
    let elapsed = fmt_duration(elapsed_seconds);
    let count = briefing.map_or(0, |b| b.items.len());
    let status = match (&ending, briefing) {
        (Ending::Failed(_), _) => Status {
            state: State::Failed,
            text: format!("Did not finish ({elapsed})"),
        },
        (Ending::Completed, None) => Status {
            state: State::Warning,
            text: format!("Completed without a briefing ({elapsed})"),
        },
        (Ending::Completed, Some(_)) => Status {
            state: State::Done,
            text: match count {
                0 => format!("Completed ({elapsed}, nothing new)"),
                1 => format!("Completed ({elapsed}, 1 new item)"),
                n => format!("Completed ({elapsed}, {n} new items)"),
            },
        },
    };
    let summary = match briefing {
        Some(b) if !b.summary.trim().is_empty() => b.summary.trim().to_string(),
        Some(_) => "Nothing notable since the last run.".to_string(),
        None => "No briefing was saved this run.".to_string(),
    };
    let mut sections = Vec::new();
    if let Some(b) = briefing.filter(|b| !b.items.is_empty()) {
        sections.push(Section::List {
            heading: "What's new".into(),
            items: b
                .items
                .iter()
                .map(|i| Item {
                    title: i.title.clone(),
                    text: i.description.clone(),
                    url: None,
                    link: Some(Link {
                        text: domain(&i.url),
                        url: Some(i.url.clone()),
                    }),
                })
                .collect(),
        });
    }
    if let Ending::Failed(why) = ending {
        sections.push(Section::Callout {
            heading: "Needs you".into(),
            text: format!(
                "The topic's session did not finish: {why}. The next run tries again; the loop's log has the details."
            ),
            items: Vec::new(),
        });
    }
    Report {
        title: format!("{label} briefing ({date})"),
        status: Some(status),
        summary: Some(summary),
        fields: Vec::new(),
        sections,
        footer: vec![Link {
            text: "LoopX Topic Monitor".into(),
            url: None,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn briefing() -> Briefing {
        serde_json::from_str(
            r#"{"summary": "Two launches.", "items": [
                {"title": "Acme ships X", "description": "A new model.", "url": "https://www.techcrunch.com/x"},
                {"title": "Beta raises", "url": "https://siliconangle.com/b"}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn save_validates_and_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let p = default_path(d.path(), "ai");
        assert!(p.ends_with("outputs/topic-monitor/reports/ai.json"));
        let json = serde_json::to_string(&briefing()).unwrap();
        save(&p, &json).unwrap();
        assert_eq!(load(&p), Some(briefing()));
        assert!(save(&p, "not json")
            .unwrap_err()
            .starts_with("Not a briefing"));
        let e = save(&p, r#"{"items": [{"title": "t", "url": "javascript:x"}]}"#).unwrap_err();
        assert!(e.contains("needs an http(s) url"), "{e}");
    }

    #[test]
    fn a_completed_topic_lists_its_items_with_their_source_domain() {
        let r = build(
            "AI news",
            "2026-10-28",
            Some(&briefing()),
            Ending::Completed,
            81.3,
        );
        assert_eq!(r.title, "AI news briefing (2026-10-28)");
        let s = r.status.unwrap();
        assert_eq!(
            (s.state, s.text.as_str()),
            (State::Done, "Completed (81.3s, 2 new items)")
        );
        assert_eq!(r.summary.as_deref(), Some("Two launches."));
        let Section::List { heading, items } = &r.sections[0] else {
            panic!()
        };
        assert_eq!(heading, "What's new");
        assert_eq!(items[0].text, "A new model.");
        assert_eq!(items[0].link.as_ref().unwrap().text, "techcrunch.com");
    }

    #[test]
    fn a_quiet_day_and_a_failed_session_still_report() {
        let quiet = Briefing::default();
        let r = build("AI", "d", Some(&quiet), Ending::Completed, 5.0);
        assert_eq!(r.status.unwrap().text, "Completed (5.0s, nothing new)");
        assert_eq!(
            r.summary.as_deref(),
            Some("Nothing notable since the last run.")
        );
        assert!(r.sections.is_empty());

        let r = build(
            "AI",
            "d",
            None,
            Ending::Failed("timed out after 600s"),
            600.0,
        );
        assert_eq!(r.status.as_ref().unwrap().state, State::Failed);
        let Section::Callout { text, .. } = &r.sections[0] else {
            panic!()
        };
        assert!(text.contains("timed out after 600s"), "{text}");

        let r = build("AI", "d", None, Ending::Completed, 5.0);
        assert_eq!(r.status.unwrap().state, State::Warning);
    }
}
