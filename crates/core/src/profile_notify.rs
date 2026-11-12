//! One Slack message per project profile job that ends: a build, a section
//! refresh, "Build all profiles", the weekly sync (or "Sync now") and a batch of
//! event-driven refreshes. [`report`] turns what ended into a
//! [`Report`](crate::slack_report::Report) (a pure function); [`send_if_enabled`]
//! reads the `notify_slack` switch from the sync store, checks the text for
//! anything secret-shaped and hands the report to an injected sender.
//!
//! A message carries only aliases, section names, counts, durations, a state and
//! a reason from the fixed list in `profile_progress::FAIL_REASONS`: never repo
//! text, agent output, file contents or an error's own words. It has no link: no
//! other LoopX Slack message links to the local dashboard.
//!
//! Sending is best effort and never fails or blocks a job: the production sender
//! ([`slack_sender`]) posts from a short detached thread with a bounded timeout to
//! the default webhook, and does nothing when no webhook is configured.

use crate::profile_progress::{self, FAIL_REASONS};
use crate::profile_sync::{self, Outcome};
use crate::project_profile::{contains_secret, Origin, Profile, SectionKey};
use crate::slack_report::{fmt_duration, Field, Link, Report, Section, State, Status};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Lines a summary lists before "+N more".
pub const MAX_LINES: usize = 20;
/// How long one webhook post may take before it is given up.
pub const POST_TIMEOUT: Duration = Duration::from_secs(20);

/// What a sender looks like: posts a report, says whether it went out.
pub type Notifier = Arc<dyn Fn(&Report) -> bool + Send + Sync>;

/// What a finished whole-profile build wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildDone {
    /// Sections the AI wrote.
    pub written: usize,
    /// Sections written by a person, kept as they were.
    pub kept: usize,
}

impl BuildDone {
    pub fn of(p: &Profile) -> BuildDone {
        let kept = p
            .sections
            .values()
            .filter(|s| s.origin == Origin::Human)
            .count();
        BuildDone {
            written: p.sections.len() - kept,
            kept,
        }
    }
}

/// One project of "Build all profiles".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowResult {
    Built,
    Failed(&'static str),
    /// Another run held the project, so it was left out.
    Busy,
}

/// One entry of an event-queue batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRow {
    pub alias: String,
    /// `None` when the project failed before any section (its checkout is gone).
    pub section: Option<SectionKey>,
    /// `None`: a proposal was raised.
    pub failed: Option<&'static str>,
}

/// A profile job that ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Finished {
    Build {
        alias: String,
        result: Result<BuildDone, &'static str>,
        took: Duration,
    },
    Refresh {
        alias: String,
        section: SectionKey,
        /// `Ok(true)`: a proposal was raised; `Ok(false)`: nothing new.
        result: Result<bool, &'static str>,
        took: Duration,
    },
    BuildAll {
        rows: Vec<(String, RowResult)>,
        took: Duration,
    },
    Sync {
        outcomes: Vec<(String, Outcome)>,
        took: Duration,
    },
    Queue {
        rows: Vec<QueueRow>,
        took: Duration,
    },
}

/// The fixed reason for any failure text: an entry of `FAIL_REASONS`, never the text.
pub fn reason(err: &str) -> &'static str {
    if let Some(r) = FAIL_REASONS.iter().find(|r| **r == err) {
        return r;
    }
    profile_progress::failure_reason(None, err)
}

fn took_field(took: Duration) -> Field {
    Field {
        label: "Took".into(),
        value: fmt_duration(took.as_secs_f64()),
        url: None,
    }
}

fn project_field(alias: &str) -> Field {
    Field {
        label: "Project".into(),
        value: alias.to_string(),
        url: None,
    }
}

/// A fixed reason as a sentence: capitalised, with a full stop.
fn sentence(reason: &str) -> String {
    let mut c = reason.chars();
    match c.next() {
        Some(f) => format!("{}{}.", f.to_uppercase(), c.as_str()),
        None => String::new(),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// At most [`MAX_LINES`] lines, then "+N more".
fn capped_lines(lines: Vec<String>) -> String {
    let more = lines.len().saturating_sub(MAX_LINES);
    let mut out: Vec<String> = lines
        .into_iter()
        .take(MAX_LINES)
        .map(|l| format!("\u{2022} {l}"))
        .collect();
    if more > 0 {
        out.push(format!("+{more} more"));
    }
    out.join("\n")
}

/// Done when nothing failed, Failed when nothing went well, else Warning.
fn mixed_state(ok: usize, failed: usize) -> State {
    match (ok, failed) {
        (_, 0) => State::Done,
        (0, _) => State::Failed,
        _ => State::Warning,
    }
}

fn message(
    title: String,
    state: State,
    status: &str,
    summary: String,
    fields: Vec<Field>,
    lines: Vec<String>,
) -> Report {
    Report {
        title,
        status: Some(Status {
            state,
            text: status.to_string(),
        }),
        summary: (!summary.is_empty()).then_some(summary),
        fields,
        sections: if lines.is_empty() {
            Vec::new()
        } else {
            vec![Section::Text {
                heading: String::new(),
                text: capped_lines(lines),
            }]
        },
        footer: vec![Link {
            text: "LoopX Memory".into(),
            url: None,
        }],
    }
}

/// The message for `f`, or `None` when there is nothing worth saying (a sync
/// where every project was skipped, a queue batch that raised nothing).
pub fn report(f: &Finished) -> Option<Report> {
    Some(match f {
        Finished::Build {
            alias,
            result: Ok(done),
            took,
        } => {
            let kept = match done.kept {
                0 => "No sections edited by a person needed keeping.".to_string(),
                1 => "1 section edited by a person was kept as it was.".to_string(),
                n => format!("{n} sections edited by a person were kept as they were."),
            };
            message(
                format!("Project profile built \u{b7} {alias}"),
                State::Done,
                "Draft ready for review",
                format!(
                    "{} written. {kept} Review the draft on the Memory page.",
                    plural(done.written, "section", "sections")
                ),
                vec![project_field(alias), took_field(*took)],
                Vec::new(),
            )
        }
        Finished::Build {
            alias,
            result: Err(why),
            took,
        } => message(
            format!("Project profile build failed \u{b7} {alias}"),
            State::Failed,
            "Failed",
            sentence(why),
            vec![project_field(alias), took_field(*took)],
            Vec::new(),
        ),
        Finished::Refresh {
            alias,
            section,
            result,
            took,
        } => {
            let name = section.heading();
            let fields = vec![
                project_field(alias),
                Field {
                    label: "Section".into(),
                    value: name.to_string(),
                    url: None,
                },
                took_field(*took),
            ];
            match result {
                Ok(true) => message(
                    format!("Profile section proposed \u{b7} {alias} \u{b7} {name}"),
                    State::Done,
                    "Waiting for review",
                    format!("A new {name} section waits under Proposals on the Memory page."),
                    fields,
                    Vec::new(),
                ),
                Ok(false) => message(
                    format!("Profile section checked \u{b7} {alias} \u{b7} {name}"),
                    State::Done,
                    "No change proposed",
                    "The AI found nothing new for this section.".into(),
                    fields,
                    Vec::new(),
                ),
                Err(why) => message(
                    format!("Profile section refresh failed \u{b7} {alias} \u{b7} {name}"),
                    State::Failed,
                    "Failed",
                    sentence(why),
                    fields,
                    Vec::new(),
                ),
            }
        }
        Finished::BuildAll { rows, took } => {
            let count = |want: fn(&RowResult) -> bool| rows.iter().filter(|(_, r)| want(r)).count();
            let ok = count(|r| *r == RowResult::Built);
            let failed = count(|r| matches!(r, RowResult::Failed(_)));
            let busy = count(|r| *r == RowResult::Busy);
            if ok == 0 && failed == 0 {
                // Everything was skipped: nothing was built, so nothing to report.
                return None;
            }
            let mut title = format!("Project profiles built: {ok} ok, {failed} failed");
            if busy > 0 {
                title.push_str(&format!(", {busy} skipped"));
            }
            let lines = rows
                .iter()
                .map(|(alias, r)| match r {
                    RowResult::Built => format!("{alias} \u{2014} ok"),
                    RowResult::Failed(why) => format!("{alias} \u{2014} failed: {why}"),
                    RowResult::Busy => format!("{alias} \u{2014} skipped: already running"),
                })
                .collect();
            let status = if failed == 0 {
                "Drafts ready for review".to_string()
            } else {
                format!("{failed} of {} failed", rows.len())
            };
            message(
                title,
                mixed_state(ok, failed),
                &status,
                String::new(),
                vec![took_field(*took)],
                lines,
            )
        }
        Finished::Sync { outcomes, took } => {
            let drafted = outcomes
                .iter()
                .filter(|(_, o)| *o == Outcome::Drafted)
                .count();
            let proposals: usize = outcomes
                .iter()
                .map(|(_, o)| match o {
                    Outcome::Proposed(n) => *n,
                    _ => 0,
                })
                .sum();
            let failed = outcomes
                .iter()
                .filter(|(_, o)| matches!(o, Outcome::Failed(_)))
                .count();
            if drafted == 0 && proposals == 0 && failed == 0 {
                return None;
            }
            let lines = outcomes
                .iter()
                .filter_map(|(alias, o)| match o {
                    Outcome::Drafted => Some(format!("{alias} \u{2014} drafted")),
                    Outcome::Proposed(0) | Outcome::Skipped => None,
                    Outcome::Proposed(n) => Some(format!(
                        "{alias} \u{2014} {}",
                        plural(*n, "proposal", "proposals")
                    )),
                    Outcome::Failed(msg) => {
                        Some(format!("{alias} \u{2014} failed: {}", reason(msg)))
                    }
                })
                .collect();
            let worked = outcomes
                .iter()
                .filter(|(_, o)| matches!(o, Outcome::Drafted | Outcome::Proposed(1..)))
                .count();
            let status = if failed == 0 {
                "Finished".to_string()
            } else {
                format!("{failed} failed")
            };
            message(
                format!(
                    "Weekly profile sync: {drafted} drafted, {}, {failed} failed of {}",
                    plural(proposals, "proposal", "proposals"),
                    plural(outcomes.len(), "project", "projects")
                ),
                mixed_state(worked, failed),
                &status,
                String::new(),
                vec![took_field(*took)],
                lines,
            )
        }
        Finished::Queue { rows, took } => {
            if rows.is_empty() {
                return None;
            }
            let failed = rows.iter().filter(|r| r.failed.is_some()).count();
            let proposed = rows.len() - failed;
            let lines = rows
                .iter()
                .map(|r| {
                    let what = match r.section {
                        Some(k) => format!("{} \u{b7} {}", r.alias, k.heading()),
                        None => r.alias.clone(),
                    };
                    match r.failed {
                        None => format!("{what} \u{2014} proposed"),
                        Some(why) => format!("{what} \u{2014} failed: {why}"),
                    }
                })
                .collect();
            let status = if failed == 0 {
                "Waiting for review".to_string()
            } else {
                format!("{failed} failed")
            };
            message(
                format!(
                    "Profile updates from recent runs: {}, {failed} failed",
                    plural(proposed, "proposal", "proposals")
                ),
                mixed_state(proposed, failed),
                &status,
                String::new(),
                vec![took_field(*took)],
                lines,
            )
        }
    })
}

/// Send `f`'s message through `send` when the `notify_slack` switch in the sync
/// store at `store` is on (a missing store is the default: on). Reads the store,
/// never writes it; an unreadable store sends nothing. A message whose text holds
/// anything secret-shaped is dropped. Never panics and never fails: returns
/// whether a message went out.
pub fn send_if_enabled(store: &Path, f: &Finished, send: &dyn Fn(&Report) -> bool) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !profile_sync::load(store).is_ok_and(|s| s.settings.notify_slack) {
            return false;
        }
        let Some(r) = report(f) else {
            return false;
        };
        let text = serde_json::to_string(&r).unwrap_or_default();
        if text.is_empty() || contains_secret(&text).is_some() {
            return false;
        }
        send(&r)
    }))
    .unwrap_or(false)
}

/// Whether `config` (`~/.slack/config.json`) names a default webhook. Reads the
/// file only: a `keychain:` reference is not resolved.
pub fn webhook_configured(config: &Path) -> bool {
    std::fs::read_to_string(config)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("webhook_url")?.as_str().map(|u| !u.trim().is_empty()))
        .unwrap_or(false)
}

/// The production sender: the default webhook in `config`, the selected message
/// template, posted from a detached thread with a [`POST_TIMEOUT`], so a slow
/// Slack never holds up the job. Returns `false` at once, sending nothing, when
/// no webhook is configured.
pub fn slack_sender(config: PathBuf) -> Notifier {
    Arc::new(move |r: &Report| {
        if !webhook_configured(&config) {
            return false;
        }
        let (r, config) = (r.clone(), config.clone());
        std::thread::Builder::new()
            .name("profile-slack".into())
            .spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::slack_report::send_with(&r, None, &config, &|url, text, blocks| {
                        crate::slack::post_message_bounded(url, text, blocks, POST_TIMEOUT)
                            .map(|_| ())
                    })
                }));
            })
            .is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn text(r: &Report) -> String {
        crate::slack_report::plain_text(r)
    }

    #[test]
    fn a_finished_build_says_draft_ready_with_counts_and_time() {
        let r = report(&Finished::Build {
            alias: "web".into(),
            result: Ok(BuildDone {
                written: 9,
                kept: 1,
            }),
            took: secs(125),
        })
        .unwrap();
        assert_eq!(r.title, "Project profile built \u{b7} web");
        let st = r.status.clone().unwrap();
        assert_eq!(
            (st.state, st.text.as_str()),
            (State::Done, "Draft ready for review")
        );
        let t = text(&r);
        assert!(t.contains("9 sections written"), "{t}");
        assert!(t.contains("1 section edited by a person was kept"), "{t}");
        assert!(t.contains("2m 5s"), "{t}");
        let none_kept = report(&Finished::Build {
            alias: "web".into(),
            result: Ok(BuildDone {
                written: 10,
                kept: 0,
            }),
            took: secs(3),
        })
        .unwrap();
        assert!(text(&none_kept).contains("No sections edited by a person"));
    }

    #[test]
    fn a_failed_build_gives_the_fixed_reason_and_never_the_error_text() {
        let raw = "claude said: /Users/me/secret-project README is wrong";
        let r = report(&Finished::Build {
            alias: "web".into(),
            result: Err(reason(raw)),
            took: secs(7),
        })
        .unwrap();
        assert_eq!(r.title, "Project profile build failed \u{b7} web");
        assert_eq!(r.status.as_ref().unwrap().state, State::Failed);
        let t = text(&r);
        assert!(!t.contains("README") && !t.contains("/Users/me"), "{t}");
        let cli = report(&Finished::Build {
            alias: "web".into(),
            result: Err(reason("FileNotFoundError")),
            took: secs(1),
        })
        .unwrap();
        let t = text(&cli);
        assert!(
            t.contains(
                "The AI command could not be started \u{2014} check Settings \u{2192} AI CLI"
            ),
            "{t}"
        );
        assert!(!t.contains("FileNotFoundError"), "{t}");
        assert!(t.contains("1.0s"), "{t}");
    }

    #[test]
    fn reasons_are_always_from_the_fixed_list() {
        for raw in [
            "anything at all",
            "FileNotFoundError",
            "glpat-abcdefghij0123456789",
            "the AI call failed",
            "checkout not found",
        ] {
            assert!(FAIL_REASONS.contains(&reason(raw)), "{raw}");
        }
        assert_eq!(reason("the AI call failed"), "the AI call failed");
        assert_eq!(reason("checkout not found"), "checkout missing");
    }

    #[test]
    fn a_section_refresh_names_the_project_and_section() {
        let ok = report(&Finished::Refresh {
            alias: "api".into(),
            section: SectionKey::Stack,
            result: Ok(true),
            took: secs(40),
        })
        .unwrap();
        assert_eq!(ok.title, "Profile section proposed \u{b7} api \u{b7} Stack");
        assert_eq!(ok.status.as_ref().unwrap().state, State::Done);
        let same = report(&Finished::Refresh {
            alias: "api".into(),
            section: SectionKey::Stack,
            result: Ok(false),
            took: secs(40),
        })
        .unwrap();
        assert_eq!(
            same.title,
            "Profile section checked \u{b7} api \u{b7} Stack"
        );
        assert_eq!(same.status.as_ref().unwrap().text, "No change proposed");
        let bad = report(&Finished::Refresh {
            alias: "api".into(),
            section: SectionKey::Gotchas,
            result: Err("the AI call failed"),
            took: secs(40),
        })
        .unwrap();
        assert_eq!(
            bad.title,
            "Profile section refresh failed \u{b7} api \u{b7} Gotchas"
        );
        assert_eq!(bad.status.as_ref().unwrap().state, State::Failed);
        assert!(text(&bad).contains("The AI call failed"));
    }

    #[test]
    fn build_all_with_every_row_busy_sends_nothing() {
        let rows = vec![
            ("a".to_string(), RowResult::Busy),
            ("b".to_string(), RowResult::Busy),
        ];
        assert!(report(&Finished::BuildAll {
            rows,
            took: secs(5)
        })
        .is_none());
        assert!(report(&Finished::BuildAll {
            rows: Vec::new(),
            took: secs(5)
        })
        .is_none());
        let mixed = report(&Finished::BuildAll {
            rows: vec![
                ("a".to_string(), RowResult::Built),
                ("b".to_string(), RowResult::Busy),
            ],
            took: secs(5),
        })
        .unwrap();
        assert_eq!(
            mixed.title,
            "Project profiles built: 1 ok, 0 failed, 1 skipped"
        );
    }

    #[test]
    fn build_all_is_one_summary_capped_at_twenty_lines() {
        let mut rows: Vec<(String, RowResult)> = (0..24)
            .map(|i| (format!("p{i:02}"), RowResult::Built))
            .collect();
        rows[1].1 = RowResult::Failed("the AI call failed");
        rows.push(("busy".into(), RowResult::Busy));
        let r = report(&Finished::BuildAll {
            rows,
            took: secs(600),
        })
        .unwrap();
        assert_eq!(
            r.title,
            "Project profiles built: 23 ok, 1 failed, 1 skipped"
        );
        assert_eq!(r.status.as_ref().unwrap().state, State::Warning);
        let t = text(&r);
        assert!(t.contains("p00 \u{2014} ok"), "{t}");
        assert!(t.contains("p01 \u{2014} failed: the AI call failed"), "{t}");
        assert!(t.contains("+5 more"), "{t}");
        assert!(!t.contains("p20"), "only twenty lines: {t}");
        let all_ok = report(&Finished::BuildAll {
            rows: vec![
                ("a".into(), RowResult::Built),
                ("b".into(), RowResult::Built),
            ],
            took: secs(60),
        })
        .unwrap();
        assert_eq!(all_ok.title, "Project profiles built: 2 ok, 0 failed");
        assert_eq!(all_ok.status.as_ref().unwrap().state, State::Done);
        let all_bad = report(&Finished::BuildAll {
            rows: vec![("a".into(), RowResult::Failed("error"))],
            took: secs(60),
        })
        .unwrap();
        assert_eq!(all_bad.status.as_ref().unwrap().state, State::Failed);
    }

    #[test]
    fn a_sync_is_one_summary_only_when_something_happened() {
        let mut outcomes: Vec<(String, Outcome)> = vec![
            ("a".into(), Outcome::Drafted),
            ("b".into(), Outcome::Drafted),
            ("c".into(), Outcome::Proposed(3)),
            (
                "d".into(),
                Outcome::Failed("TimeoutExpired at /Users/me/x".into()),
            ),
        ];
        outcomes.extend((0..8).map(|i| (format!("s{i}"), Outcome::Skipped)));
        let r = report(&Finished::Sync {
            outcomes,
            took: secs(300),
        })
        .unwrap();
        assert_eq!(
            r.title,
            "Weekly profile sync: 2 drafted, 3 proposals, 1 failed of 12 projects"
        );
        assert_eq!(r.status.as_ref().unwrap().state, State::Warning);
        let t = text(&r);
        assert!(t.contains("a \u{2014} drafted"), "{t}");
        assert!(t.contains("c \u{2014} 3 proposals"), "{t}");
        assert!(t.contains("d \u{2014} failed: error"), "{t}");
        assert!(!t.contains("/Users/me") && !t.contains("s0"), "{t}");
        // Every project skipped, or nothing raised: no message at all.
        let quiet: Vec<(String, Outcome)> = vec![
            ("a".into(), Outcome::Skipped),
            ("b".into(), Outcome::Proposed(0)),
        ];
        assert!(report(&Finished::Sync {
            outcomes: quiet,
            took: secs(5)
        })
        .is_none());
        assert!(report(&Finished::Sync {
            outcomes: vec![],
            took: secs(0)
        })
        .is_none());
    }

    #[test]
    fn a_queue_batch_is_one_message_only_with_a_proposal_or_a_failure() {
        let rows = vec![
            QueueRow {
                alias: "web".into(),
                section: Some(SectionKey::Stack),
                failed: None,
            },
            QueueRow {
                alias: "api".into(),
                section: Some(SectionKey::Gotchas),
                failed: Some("the AI call failed"),
            },
            QueueRow {
                alias: "old".into(),
                section: None,
                failed: Some("checkout missing"),
            },
        ];
        let r = report(&Finished::Queue {
            rows,
            took: secs(30),
        })
        .unwrap();
        assert_eq!(
            r.title,
            "Profile updates from recent runs: 1 proposal, 2 failed"
        );
        let t = text(&r);
        assert!(t.contains("web \u{b7} Stack \u{2014} proposed"), "{t}");
        assert!(
            t.contains("api \u{b7} Gotchas \u{2014} failed: the AI call failed"),
            "{t}"
        );
        assert!(t.contains("old \u{2014} failed: checkout missing"), "{t}");
        assert!(report(&Finished::Queue {
            rows: vec![],
            took: secs(1)
        })
        .is_none());
    }

    fn store_with(dir: &Path, notify: Option<bool>) -> PathBuf {
        let p = dir.join("profile_sync.json");
        let mut v = serde_json::json!({"settings": {"enabled": true, "weekday": "Mon", "hour": 9, "max_projects_per_run": 20}});
        if let Some(n) = notify {
            v["settings"]["notify_slack"] = serde_json::json!(n);
        }
        std::fs::write(&p, v.to_string()).unwrap();
        p
    }

    fn built() -> Finished {
        Finished::Build {
            alias: "web".into(),
            result: Ok(BuildDone {
                written: 10,
                kept: 0,
            }),
            took: secs(9),
        }
    }

    #[test]
    fn the_switch_decides_and_nothing_is_written() {
        let d = tempfile::tempdir().unwrap();
        let sent = RefCell::new(Vec::<String>::new());
        let send = |r: &Report| {
            sent.borrow_mut().push(r.title.clone());
            true
        };
        // An old store file without the field: on.
        let old = store_with(d.path(), None);
        assert!(send_if_enabled(&old, &built(), &send));
        assert_eq!(sent.borrow().len(), 1);
        // No store at all: on, and still no file is created.
        let none = d.path().join("nope/profile_sync.json");
        assert!(send_if_enabled(&none, &built(), &send));
        assert!(!none.exists());
        // Off: nothing sent, the file untouched.
        let off = store_with(d.path(), Some(false));
        let before = std::fs::read_to_string(&off).unwrap();
        assert!(!send_if_enabled(&off, &built(), &send));
        assert_eq!(sent.borrow().len(), 2);
        assert_eq!(std::fs::read_to_string(&off).unwrap(), before);
        // An unreadable store sends nothing and stays as it is.
        std::fs::write(&off, "{corrupt").unwrap();
        assert!(!send_if_enabled(&off, &built(), &send));
        assert_eq!(std::fs::read_to_string(&off).unwrap(), "{corrupt");
        assert_eq!(sent.borrow().len(), 2);
    }

    #[test]
    fn a_message_with_secret_shaped_text_is_dropped() {
        let d = tempfile::tempdir().unwrap();
        let store = store_with(d.path(), Some(true));
        let sent = RefCell::new(0);
        let send = |_: &Report| {
            *sent.borrow_mut() += 1;
            true
        };
        let f = Finished::Build {
            alias: ["glpat-", "abcdefghij0123456789"].concat(),
            result: Ok(BuildDone {
                written: 10,
                kept: 0,
            }),
            took: secs(1),
        };
        assert!(report(&f).is_some(), "the builder itself does not judge");
        assert!(!send_if_enabled(&store, &f, &send));
        assert_eq!(*sent.borrow(), 0);
    }

    #[test]
    fn a_sender_that_panics_or_fails_is_contained() {
        let d = tempfile::tempdir().unwrap();
        let store = store_with(d.path(), Some(true));
        assert!(!send_if_enabled(&store, &built(), &|_| panic!("boom")));
        assert!(!send_if_enabled(&store, &built(), &|_| false));
    }

    #[test]
    fn a_job_with_nothing_to_say_sends_nothing() {
        let d = tempfile::tempdir().unwrap();
        let store = store_with(d.path(), Some(true));
        let sent = RefCell::new(0);
        let send = |_: &Report| {
            *sent.borrow_mut() += 1;
            true
        };
        let quiet = Finished::Sync {
            outcomes: vec![("a".into(), Outcome::Skipped)],
            took: secs(1),
        };
        assert!(!send_if_enabled(&store, &quiet, &send));
        assert_eq!(*sent.borrow(), 0);
    }

    #[test]
    fn the_webhook_check_reads_the_config_only() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.json");
        assert!(!webhook_configured(&p));
        std::fs::write(&p, r#"{"webhook_url": ""}"#).unwrap();
        assert!(!webhook_configured(&p));
        std::fs::write(&p, "{nope").unwrap();
        assert!(!webhook_configured(&p));
        std::fs::write(&p, r#"{"webhook_url": "keychain:loop/x"}"#).unwrap();
        assert!(webhook_configured(&p));
        std::fs::write(&p, r#"{"webhook_url": "http://127.0.0.1:9/"}"#).unwrap();
        assert!(webhook_configured(&p));
    }

    #[test]
    fn the_real_sender_skips_silently_without_a_webhook() {
        let d = tempfile::tempdir().unwrap();
        let send = slack_sender(d.path().join("missing.json"));
        assert!(!send(&report(&built()).unwrap()));
    }

    #[test]
    fn the_real_sender_posts_once_to_the_configured_webhook() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(1) {
                let mut s = stream.unwrap();
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read the headers and the body (Content-Length long).
                loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let t = String::from_utf8_lossy(&buf).to_string();
                    if let Some((head, body)) = t.split_once("\r\n\r\n") {
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if body.len() >= len {
                            break;
                        }
                    }
                }
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
                tx.send(String::from_utf8_lossy(&buf).to_string()).unwrap();
            }
        });
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("config.json");
        std::fs::write(
            &cfg,
            format!(r#"{{"webhook_url": "http://127.0.0.1:{port}/hook"}}"#),
        )
        .unwrap();
        let send = slack_sender(cfg);
        assert!(send(&report(&built()).unwrap()), "handed to the poster");
        let got = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(got.starts_with("POST /hook"), "{got}");
        assert!(got.contains("Project profile built"), "{got}");
    }
}
