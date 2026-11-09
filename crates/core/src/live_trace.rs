//! One terminal-style trace of everything the loops do: the event ledger's steps
//! (`run.started`, `issue.classified`, `verification.passed`, ...) turned into
//! one readable line each, plus what a running agent is doing right now.
//! `loopx watch` prints it and the dashboard's Terminal view renders the same
//! lines, so both show the same steps in the same words.

use crate::{agent_activity, events};
use chrono::{DateTime, Local};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// A step started or is in progress.
    Run,
    /// A step finished well.
    Ok,
    /// Needs a look: escalated, skipped, blocked.
    Warn,
    /// A step failed.
    Err,
    /// Bookkeeping.
    Info,
}

impl Level {
    pub fn css(self) -> &'static str {
        match self {
            Level::Run => "run",
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Err => "err",
            Level::Info => "info",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Level::Run => "▶",
            Level::Ok => "✓",
            Level::Warn => "!",
            Level::Err => "✗",
            Level::Info => "·",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceLine {
    /// The event's own timestamp, for ordering and "since" cursors.
    pub at: String,
    /// Local `HH:MM:SS`.
    pub time: String,
    /// Who: `project #iid` for an issue, else the run or loop.
    pub scope: String,
    pub level: Level,
    pub text: String,
}

impl TraceLine {
    /// `19:57:03  kurrant.web #1528  ✓ done: mr_opened`
    pub fn render(&self, scope_width: usize) -> String {
        format!(
            "{}  {:<w$}  {} {}",
            self.time,
            self.scope,
            self.level.glyph(),
            self.text,
            w = scope_width
        )
    }
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn clip(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let head: String = one_line.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

fn local_time(ts: &str) -> String {
    DateTime::parse_from_rfc3339(ts).map_or_else(
        |_| ts.chars().skip(11).take(8).collect(),
        |t| t.with_timezone(&Local).format("%H:%M:%S").to_string(),
    )
}

/// One ledger event as a trace line; `None` for pure noise.
pub fn describe(ev: &Value) -> Option<TraceLine> {
    let kind = s(ev, "event_type");
    let data = ev.get("data").cloned().unwrap_or(Value::Null);
    let d = |k: &str| s(&data, k);
    let n = |k: &str| data.get(k).and_then(Value::as_i64);
    let iteration = n("iteration").map(|i| format!(" #{i}")).unwrap_or_default();
    let (level, text) = match kind {
        "policy.allowed" | "loop.result" | "memory.reused" => return None,
        "run.started" => (Level::Run, format!("run started ({})", d("trigger"))),
        "run.completed" => (
            Level::Ok,
            format!("run finished, {} issue(s)", n("issues").unwrap_or(0)),
        ),
        "run.failed" => (
            Level::Err,
            format!("run failed (exit {})", n("exit_code").unwrap_or(1)),
        ),
        "run.wrapup_failed" => (
            Level::Err,
            format!("wrap-up failed: {}", clip(d("error"), 100)),
        ),
        "issue.queued" => (Level::Info, format!("queued: {}", clip(d("title"), 90))),
        "issue.started" => (Level::Run, "working on it".to_string()),
        "issue.classified" => (
            Level::Info,
            format!(
                "classified: {} {} ~{}m, risk {}",
                d("type"),
                d("complexity"),
                n("estimated_minutes").unwrap_or(0),
                d("risk_level")
            ),
        ),
        "verification.commands" => {
            let steps: Vec<&str> = ["install", "lint", "test"]
                .into_iter()
                .filter(|k| !d(k).is_empty())
                .collect();
            (
                Level::Info,
                format!("verification plan: {}", steps.join(" → ")),
            )
        }
        "verification.started" => (Level::Run, format!("verifying{iteration}")),
        "verification.passed" => (Level::Ok, format!("verification passed{iteration}")),
        "verification.failed" => (Level::Err, format!("verification failed{iteration}")),
        "verification.external_skipped" => (Level::Warn, "project checks skipped".to_string()),
        "verification.external_completed" => {
            let pass = data.get("passed").and_then(Value::as_bool).unwrap_or(false);
            (
                if pass { Level::Ok } else { Level::Err },
                format!("project checks {}", if pass { "passed" } else { "failed" }),
            )
        }
        "issue.completed" => {
            let mut t = format!("done: {}", d("outcome"));
            if !d("mr_url").is_empty() {
                t.push_str(&format!(" {}", d("mr_url")));
            }
            (Level::Ok, t)
        }
        "issue.escalated" => (Level::Warn, format!("escalated: {}", d("reason"))),
        "issue.agent_failed" => (
            Level::Err,
            format!("agent failed: {}", clip(d("reason"), 100)),
        ),
        "loop.started" => (Level::Run, format!("loop started ({})", d("definition"))),
        "loop.completed" => (Level::Ok, format!("loop completed ({})", d("stop_reason"))),
        "loop.failed" => (Level::Err, format!("loop failed ({})", d("stop_reason"))),
        "loop.stopped" => (Level::Warn, format!("loop stopped ({})", d("stop_reason"))),
        "iteration.started" => (Level::Run, format!("iteration{iteration} started")),
        "iteration.completed" => (Level::Ok, format!("iteration{iteration} {}", d("state"))),
        "setup.doctor_started" => (Level::Run, "repairing the worktree setup".to_string()),
        "setup.doctor_finished" => (Level::Ok, format!("setup repair {}", d("status"))),
        "setup.blocked" => (
            Level::Warn,
            format!("setup blocked: {}", clip(d("command"), 80)),
        ),
        "approval.requested" => (Level::Warn, "waiting for your approval".to_string()),
        "approval.decided" => (Level::Info, format!("approval {}", d("decision"))),
        "approval.revision_started" => (Level::Run, "revising after your feedback".to_string()),
        "memory.created" => (Level::Info, format!("lesson saved ({})", d("category"))),
        other => (Level::Info, other.to_string()),
    };
    let at = s(ev, "timestamp").to_string();
    let issue = ev.get("issue_iid").and_then(Value::as_i64);
    let project = s(ev, "project");
    let scope = match (project.is_empty(), issue) {
        (false, Some(i)) => format!("{project} #{i}"),
        (false, None) => project.to_string(),
        _ => s(ev, "run_id").to_string(),
    };
    Some(TraceLine {
        time: local_time(&at),
        at,
        scope,
        level,
        text,
    })
}

/// The newest `limit` trace lines, oldest first. Reads today's and yesterday's
/// ledger files only. `after` keeps only lines newer than that `at` cursor.
pub fn recent(events_dir: &Path, limit: usize, after: Option<&str>) -> Vec<TraceLine> {
    let today = Local::now().date_naive();
    let since = (today - chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let raw = events::iter_events(Some(events_dir), Some(&since), None);
    // A loop's own events carry only its run id; the issue events name that id as
    // their `issue_run_id`, so they tell which issue the loop was working on.
    let mut issue_of = std::collections::HashMap::new();
    for ev in &raw {
        if let (Some(id), Some(line)) =
            (ev.get("issue_run_id").and_then(Value::as_str), describe(ev))
        {
            if !line.scope.is_empty() && ev.get("issue_iid").is_some_and(|v| !v.is_null()) {
                issue_of.insert(id.to_string(), line.scope);
            }
        }
    }
    let mut lines: Vec<TraceLine> = raw
        .iter()
        .filter_map(describe)
        .map(|mut l| {
            if let Some(scope) = issue_of.get(&l.scope) {
                l.scope.clone_from(scope);
            }
            l
        })
        .filter(|l| after.is_none_or(|a| l.at.as_str() > a))
        .collect();
    lines.sort_by(|a, b| a.at.cmp(&b.at));
    let skip = lines.len().saturating_sub(limit);
    lines.split_off(skip)
}

/// What is running right now, one line per active agent: `label  Tool: detail`.
pub fn running_now(base: &Path) -> Vec<String> {
    let status = crate::loop_scheduler::status_path_for_loop("issue-loop", Some(base));
    agent_activity::read_all(&agent_activity::beside(&status))
        .iter()
        .filter(|a| s(a, "event") != "Stop")
        .map(|a| {
            let what = [s(a, "tool"), s(a, "detail")]
                .iter()
                .filter(|p| !p.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(": ");
            format!(
                "{}  {}",
                s(a, "label"),
                if what.is_empty() { "thinking" } else { &what }
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(kind: &str, data: Value) -> Value {
        json!({"timestamp": "2026-11-09T12:00:00.000Z", "event_type": kind, "run_id": "run_1",
               "project": "kurrant.web", "issue_iid": 1528, "data": data})
    }

    #[test]
    fn an_issue_event_is_scoped_to_project_and_iid() {
        let l = describe(&ev(
            "issue.escalated",
            json!({"reason": "verification_failed"}),
        ))
        .unwrap();
        assert_eq!(l.scope, "kurrant.web #1528");
        assert_eq!(l.level, Level::Warn);
        assert_eq!(l.text, "escalated: verification_failed");
    }

    #[test]
    fn verification_steps_read_in_order() {
        let a = describe(&ev("verification.started", json!({"iteration": 2}))).unwrap();
        let b = describe(&ev("verification.failed", json!({"iteration": 2}))).unwrap();
        assert_eq!((a.text.as_str(), a.level), ("verifying #2", Level::Run));
        assert_eq!(
            (b.text.as_str(), b.level),
            ("verification failed #2", Level::Err)
        );
    }

    #[test]
    fn noise_events_are_dropped_and_unknown_ones_kept() {
        assert!(describe(&ev("policy.allowed", json!({}))).is_none());
        assert_eq!(
            describe(&ev("brand.new", json!({}))).unwrap().text,
            "brand.new"
        );
    }

    #[test]
    fn a_completed_issue_shows_the_merge_request() {
        let l = describe(&ev(
            "issue.completed",
            json!({"outcome": "mr_opened", "mr_url": "https://x/mr/1"}),
        ))
        .unwrap();
        assert_eq!(l.text, "done: mr_opened https://x/mr/1");
        assert_eq!(l.level, Level::Ok);
    }

    #[test]
    fn recent_returns_the_newest_lines_oldest_first_and_honors_the_cursor() {
        let d = tempfile::tempdir().unwrap();
        for kind in ["run.started", "issue.started", "run.completed"] {
            events::emit(
                kind,
                "run_1",
                events::EventFields::default(),
                Some(d.path()),
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        let all = recent(d.path(), 10, None);
        assert_eq!(all.len(), 3);
        assert!(all[0].text.starts_with("run started"));
        let last_two = recent(d.path(), 2, None);
        assert_eq!(last_two.len(), 2);
        assert_eq!(last_two[0].text, "working on it");
        let newer = recent(d.path(), 10, Some(&all[1].at));
        assert_eq!(newer.len(), 1);
    }

    #[test]
    fn a_loops_own_events_take_the_scope_of_the_issue_it_worked_on() {
        let d = tempfile::tempdir().unwrap();
        let issue = events::EventFields {
            issue_run_id: Some("run_1_web_7".into()),
            project: Some("web".into()),
            issue_iid: Some(7),
            ..Default::default()
        };
        events::emit("issue.started", "run_1", issue, Some(d.path())).unwrap();
        events::emit(
            "verification.started",
            "run_1_web_7",
            events::EventFields::default(),
            Some(d.path()),
        )
        .unwrap();
        let lines = recent(d.path(), 10, None);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.scope == "web #7"), "{lines:?}");
    }

    #[test]
    fn a_line_renders_time_scope_glyph_and_text() {
        let l = TraceLine {
            at: "a".into(),
            time: "19:57:03".into(),
            scope: "web #1".into(),
            level: Level::Ok,
            text: "done".into(),
        };
        assert_eq!(l.render(8), "19:57:03  web #1    ✓ done");
    }
}
