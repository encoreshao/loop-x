//! The issue loop's end-of-run Slack report: one message per Slack bundle,
//! built from this run's events - the same ledger the dashboard and the
//! wrap-up session read - so it covers what the agent did as well as what the
//! loop did itself.

/// How Slack names the issue loop (it works GitLab, GitHub and other trackers).
pub const ISSUE_LOOP_TITLE: &str = "Issue Loop";

use crate::issue_runner::{escalation_reason_text, mr_iid};
use crate::slack_report::{fmt_duration, Item, Link, Report, Section, State, Status};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    MrOpened(Option<String>),
    Answered,
    Waiting {
        reminder_sent: bool,
    },
    Escalated(String),
    /// The issue's run never reached an outcome (crash, budget, timeout).
    Incomplete(String),
    /// Checked; nothing new to act on.
    Checked,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IssueLine {
    pub alias: String,
    pub iid: String,
    pub title: String,
    pub web_url: Option<String>,
    pub outcome: Outcome,
}

/// The run-level facts the events don't carry.
#[derive(Debug, Clone, Default)]
pub struct RunFacts {
    /// "Daily Issue Loop" for the scheduled batch, else the run's own name.
    pub title: String,
    pub run_id: String,
    pub elapsed_seconds: f64,
    pub cost_usd: Option<f64>,
    /// The wrap-up session's error, when it failed.
    pub wrapup_error: Option<String>,
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// One line per issue this run queued, in queue order. `unfinished` is
/// `(issue_run_id, "state: stop_reason")` for each issue whose run did not
/// complete; it only applies where no event recorded an outcome.
pub fn issue_lines(
    run_id: &str,
    events: &[Value],
    unfinished: &[(String, String)],
) -> Vec<IssueLine> {
    let mut lines: Vec<(String, IssueLine)> = Vec::new();
    for e in events
        .iter()
        .filter(|e| e["run_id"].as_str() == Some(run_id))
    {
        let Some(key) = e["issue_run_id"].as_str() else {
            continue;
        };
        let pos = match lines.iter().position(|(k, _)| k == key) {
            Some(p) => p,
            None => {
                lines.push((
                    key.to_string(),
                    IssueLine {
                        alias: text(&e["project"]),
                        iid: text(&e["issue_iid"]),
                        title: String::new(),
                        web_url: None,
                        outcome: Outcome::Checked,
                    },
                ));
                lines.len() - 1
            }
        };
        let line = &mut lines[pos].1;
        let data = &e["data"];
        match e["event_type"].as_str().unwrap_or("") {
            "issue.queued" => {
                line.title = text(&data["title"]);
                line.web_url = data["web_url"]
                    .as_str()
                    .filter(|u| !u.is_empty())
                    .map(str::to_owned);
            }
            "issue.completed" => {
                line.outcome = match data["action"].as_str() {
                    Some("fix") => Outcome::MrOpened(
                        data["mr_url"]
                            .as_str()
                            .filter(|u| !u.is_empty())
                            .map(str::to_owned),
                    ),
                    Some("answer") => Outcome::Answered,
                    Some("wait_for_review") => Outcome::Waiting {
                        reminder_sent: data["reminder_sent"].as_bool().unwrap_or(false),
                    },
                    _ => Outcome::Checked,
                }
            }
            "issue.escalated" => {
                let reason = data["reason"].as_str().unwrap_or("escalated");
                line.outcome = Outcome::Escalated(escalation_reason_text(
                    reason,
                    data["stop_reason"].as_str(),
                ));
            }
            _ => {}
        }
    }
    for (key, detail) in unfinished {
        if let Some((_, line)) = lines.iter_mut().find(|(k, _)| k == key) {
            if matches!(line.outcome, Outcome::Checked) {
                line.outcome = Outcome::Incomplete(detail.clone());
            }
        }
    }
    lines.into_iter().map(|(_, l)| l).collect()
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn item(line: &IssueLine, text: String) -> Item {
    Item {
        title: format!("#{} ({})", line.iid, line.alias),
        text,
        url: line.web_url.clone(),
        link: None,
    }
}

/// The report for `lines` (one bundle's share of the run).
pub fn build(lines: &[IssueLine], facts: &RunFacts) -> Report {
    let count = |f: fn(&Outcome) -> bool| lines.iter().filter(|l| f(&l.outcome)).count();
    let mrs = count(|o| matches!(o, Outcome::MrOpened(_)));
    let answered = count(|o| matches!(o, Outcome::Answered));
    let escalated = count(|o| matches!(o, Outcome::Escalated(_)));
    let incomplete = count(|o| matches!(o, Outcome::Incomplete(_)));
    let total = lines.len();
    let elapsed = fmt_duration(facts.elapsed_seconds);
    let problems = incomplete > 0 || facts.wrapup_error.is_some();
    let status = Status {
        state: if problems {
            State::Warning
        } else {
            State::Done
        },
        text: match (problems, total) {
            (false, 0) => format!("Completed ({elapsed}, no assigned issues)"),
            (false, _) => format!(
                "Completed ({elapsed}, {}/{total} issues)",
                total - incomplete
            ),
            (true, _) => format!(
                "Finished with problems ({elapsed}, {}/{total} issues)",
                total - incomplete
            ),
        },
    };
    let summary = if total == 0 {
        "No assigned issues today - nothing to do.".to_string()
    } else {
        [
            plural(total, "issue checked", "issues checked"),
            plural(mrs, "MR opened", "MRs opened"),
            format!("{answered} answered directly"),
            format!("{escalated} escalated"),
        ]
        .join(" \u{b7} ")
    };

    let mut sections = Vec::new();
    let mut list = |heading: &str, items: Vec<Item>| {
        if !items.is_empty() {
            sections.push(Section::List {
                heading: heading.into(),
                items,
            });
        }
    };
    list(
        "Merge requests opened",
        lines
            .iter()
            .filter_map(|l| match &l.outcome {
                Outcome::MrOpened(url) => Some(Item {
                    link: Some(Link {
                        text: url
                            .as_deref()
                            .and_then(mr_iid)
                            .map_or("view MR".to_string(), |iid| format!("!{iid}")),
                        url: url.clone(),
                    }),
                    ..item(l, l.title.clone())
                }),
                _ => None,
            })
            .collect(),
    );
    list(
        "Answered directly",
        lines
            .iter()
            .filter(|l| l.outcome == Outcome::Answered)
            .map(|l| item(l, l.title.clone()))
            .collect(),
    );
    list(
        "Waiting on review",
        lines
            .iter()
            .filter_map(|l| match l.outcome {
                Outcome::Waiting { reminder_sent } => Some(item(
                    l,
                    if reminder_sent {
                        format!("{} (a week without review - reminder sent)", l.title)
                    } else {
                        l.title.clone()
                    },
                )),
                _ => None,
            })
            .collect(),
    );
    list(
        "Nothing new",
        lines
            .iter()
            .filter(|l| l.outcome == Outcome::Checked)
            .map(|l| item(l, l.title.clone()))
            .collect(),
    );

    let mut projects: Vec<&str> = Vec::new();
    for l in lines {
        if !projects.contains(&l.alias.as_str()) {
            projects.push(&l.alias);
        }
    }
    if projects.len() > 1 {
        let rows = projects
            .iter()
            .map(|p| {
                let mine: Vec<&IssueLine> = lines.iter().filter(|l| l.alias == *p).collect();
                let n = |f: fn(&Outcome) -> bool| {
                    mine.iter().filter(|l| f(&l.outcome)).count().to_string()
                };
                vec![
                    p.to_string(),
                    mine.len().to_string(),
                    n(|o| matches!(o, Outcome::MrOpened(_))),
                    n(|o| matches!(o, Outcome::Answered)),
                    n(|o| matches!(o, Outcome::Escalated(_) | Outcome::Incomplete(_))),
                ]
            })
            .collect();
        sections.push(Section::Table {
            heading: "By project".into(),
            columns: ["Project", "Checked", "MRs", "Answered", "Needs you"]
                .map(String::from)
                .to_vec(),
            rows,
        });
    }

    let mut needs: Vec<Item> = lines
        .iter()
        .filter_map(|l| match &l.outcome {
            Outcome::Escalated(why) => Some(item(l, format!("{} - {why}", l.title))),
            Outcome::Incomplete(detail) => Some(item(
                l,
                format!(
                    "{} - the run did not finish ({detail}); see the loop's log.",
                    l.title
                ),
            )),
            _ => None,
        })
        .collect();
    if let Some(err) = &facts.wrapup_error {
        needs.push(Item {
            title: "End-of-run wrap-up failed".into(),
            text: format!("daily-review.md was not written: {err}"),
            ..Item::default()
        });
    }
    if !needs.is_empty() {
        sections.push(Section::Callout {
            heading: if needs.len() == 1 {
                "One thing needs you".into()
            } else {
                format!("{} things need you", needs.len())
            },
            text: String::new(),
            items: needs,
        });
    }

    let mut footer = vec![
        Link {
            text: format!("LoopX {}", ISSUE_LOOP_TITLE),
            url: None,
        },
        Link {
            text: format!("run {}", facts.run_id),
            url: None,
        },
    ];
    if let Some(cost) = facts.cost_usd {
        footer.push(Link {
            text: format!("${cost:.2}"),
            url: None,
        });
    }
    Report {
        title: if facts.title.is_empty() {
            "Daily Issue Loop".into()
        } else {
            facts.title.clone()
        },
        status: Some(status),
        summary: Some(summary),
        fields: Vec::new(),
        sections,
        footer,
    }
}

/// A report with nothing new in it: every issue was checked with nothing to
/// act on, or is still waiting on a reviewer with no reminder due.
pub fn is_quiet(lines: &[IssueLine]) -> bool {
    lines.iter().all(|l| {
        matches!(
            l.outcome,
            Outcome::Checked
                | Outcome::Waiting {
                    reminder_sent: false
                }
        )
    })
}

/// `lines` split by Slack bundle, in first-seen order; a run with no issues
/// is one empty group for the default webhook.
pub fn by_bundle(
    lines: Vec<IssueLine>,
    bundle_of: &dyn Fn(&str) -> Option<String>,
) -> Vec<(Option<String>, Vec<IssueLine>)> {
    let mut groups: Vec<(Option<String>, Vec<IssueLine>)> = Vec::new();
    for line in lines {
        let bundle = bundle_of(&line.alias);
        match groups.iter_mut().find(|(b, _)| *b == bundle) {
            Some((_, g)) => g.push(line),
            None => groups.push((bundle, vec![line])),
        }
    }
    if groups.is_empty() {
        groups.push((None, Vec::new()));
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(kind: &str, alias: &str, iid: i64, data: Value) -> Value {
        json!({"event_type": kind, "run_id": "R", "issue_run_id": format!("R_{alias}_{iid}"), "project": alias, "issue_iid": iid, "data": data})
    }

    fn events() -> Vec<Value> {
        vec![
            ev(
                "issue.queued",
                "web",
                12,
                json!({"title": "Login loop", "web_url": "https://g/web/-/issues/12"}),
            ),
            ev(
                "issue.queued",
                "api",
                15,
                json!({"title": "Refund rounding", "web_url": "https://g/api/-/issues/15"}),
            ),
            ev(
                "issue.queued",
                "api",
                18,
                json!({"title": "Vague ask", "web_url": ""}),
            ),
            ev(
                "issue.queued",
                "api",
                19,
                json!({"title": "Crashy", "web_url": ""}),
            ),
            ev(
                "issue.queued",
                "web",
                20,
                json!({"title": "Old MR", "web_url": ""}),
            ),
            ev("issue.started", "web", 12, json!({})),
            ev(
                "issue.completed",
                "web",
                12,
                json!({"action": "fix", "mr_url": "https://g/web/-/merge_requests/34"}),
            ),
            ev("issue.completed", "api", 15, json!({"action": "answer"})),
            ev(
                "issue.escalated",
                "api",
                18,
                json!({"reason": "needs_clarification"}),
            ),
            ev(
                "issue.completed",
                "web",
                20,
                json!({"action": "wait_for_review", "reminder_sent": true}),
            ),
            json!({"event_type": "issue.completed", "run_id": "OTHER", "issue_run_id": "OTHER_web_12", "project": "web", "issue_iid": 12, "data": {"action": "answer"}}),
        ]
    }

    fn lines() -> Vec<IssueLine> {
        issue_lines(
            "R",
            &events(),
            &[
                ("R_api_19".into(), "failed: agent_failed".into()),
                ("R_web_12".into(), "ignored".into()),
            ],
        )
    }

    #[test]
    fn outcomes_come_from_this_runs_events_in_queue_order() {
        let l = lines();
        let got: Vec<(&str, &Outcome)> = l.iter().map(|l| (l.iid.as_str(), &l.outcome)).collect();
        assert_eq!(
            got,
            [
                (
                    "12",
                    &Outcome::MrOpened(Some("https://g/web/-/merge_requests/34".into()))
                ),
                ("15", &Outcome::Answered),
                (
                    "18",
                    &Outcome::Escalated(
                        "The issue needs clarification - the agent asked on the issue.".into()
                    )
                ),
                ("19", &Outcome::Incomplete("failed: agent_failed".into())),
                (
                    "20",
                    &Outcome::Waiting {
                        reminder_sent: true
                    }
                ),
            ]
        );
        assert_eq!(l[0].title, "Login loop");
        assert_eq!(l[2].web_url, None);
    }

    #[test]
    fn the_report_has_status_summary_lists_table_and_needs_you() {
        let facts = RunFacts {
            title: String::new(),
            run_id: "R".into(),
            elapsed_seconds: 724.0,
            cost_usd: Some(1.234),
            wrapup_error: None,
        };
        let r = build(&lines(), &facts);
        let status = r.status.as_ref().unwrap();
        assert_eq!(status.state, State::Warning);
        assert_eq!(status.text, "Finished with problems (12m 4s, 4/5 issues)");
        assert_eq!(
            r.summary.as_deref(),
            Some(
                "5 issues checked \u{b7} 1 MR opened \u{b7} 1 answered directly \u{b7} 1 escalated"
            )
        );
        let headings: Vec<&str> = r
            .sections
            .iter()
            .map(|s| match s {
                Section::List { heading, .. }
                | Section::Table { heading, .. }
                | Section::Callout { heading, .. }
                | Section::Steps { heading, .. }
                | Section::Text { heading, .. } => heading.as_str(),
            })
            .collect();
        assert_eq!(
            headings,
            [
                "Merge requests opened",
                "Answered directly",
                "Waiting on review",
                "By project",
                "2 things need you"
            ]
        );
        let Section::List { items, .. } = &r.sections[0] else {
            panic!()
        };
        assert_eq!(items[0].title, "#12 (web)");
        assert_eq!(items[0].link.as_ref().unwrap().text, "!34");
        let Section::Table { rows, .. } = &r.sections[3] else {
            panic!()
        };
        assert_eq!(rows[1], ["api", "3", "0", "1", "2"]);
        let Section::Callout { items, .. } = &r.sections[4] else {
            panic!()
        };
        assert!(items[1]
            .text
            .starts_with("Crashy - the run did not finish (failed: agent_failed)"));
        assert_eq!(
            r.footer.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["LoopX Issue Loop", "run R", "$1.23"]
        );
    }

    #[test]
    fn a_quiet_morning_still_reports_and_a_failed_wrapup_needs_you() {
        let r = build(
            &[],
            &RunFacts {
                run_id: "R".into(),
                elapsed_seconds: 3.2,
                ..RunFacts::default()
            },
        );
        assert_eq!(
            r.status.unwrap().text,
            "Completed (3.2s, no assigned issues)"
        );
        assert_eq!(
            r.summary.as_deref(),
            Some("No assigned issues today - nothing to do.")
        );
        assert!(r.sections.is_empty());

        let r = build(
            &[],
            &RunFacts {
                wrapup_error: Some("AgentError: boom".into()),
                ..RunFacts::default()
            },
        );
        assert_eq!(r.status.as_ref().unwrap().state, State::Warning);
        let Section::Callout { heading, items, .. } = &r.sections[0] else {
            panic!()
        };
        assert_eq!(heading, "One thing needs you");
        assert!(items[0].text.contains("AgentError: boom"));
    }

    #[test]
    fn issues_are_grouped_by_their_projects_slack_bundle() {
        let groups = by_bundle(lines(), &|alias| {
            (alias == "api").then(|| "team-api".to_string())
        });
        let shape: Vec<(Option<&str>, usize)> = groups
            .iter()
            .map(|(b, l)| (b.as_deref(), l.len()))
            .collect();
        assert_eq!(shape, [(None, 2), (Some("team-api"), 3)]);
        assert_eq!(by_bundle(Vec::new(), &|_| None).len(), 1);
    }
}
