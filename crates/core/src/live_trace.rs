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

/// Where a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A ledger event: a step of a run, issue or loop.
    Step,
    /// A tool an agent is using right now (the agent hooks' trace).
    Agent,
    /// A step of a project profile build, section refresh or sync.
    Profile,
}

impl Source {
    pub fn css(self) -> &'static str {
        match self {
            Source::Step => "step",
            Source::Agent => "agent",
            Source::Profile => "profile",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceLine {
    pub source: Source,
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
        source: Source::Step,
        time: local_time(&at),
        at,
        scope,
        level,
        text,
    })
}

/// Which lines to keep; every field narrows the result.
#[derive(Debug, Default, Clone)]
pub struct Filter {
    /// Case-insensitive part of the scope (`kurrant.web`, `#1528`, a run id).
    pub scope: Option<String>,
    pub source: Option<Source>,
    /// Only warnings and failures.
    pub attention: bool,
    /// Case-insensitive part of the scope or the text.
    pub text: Option<String>,
}

impl Filter {
    pub fn matches(&self, l: &TraceLine) -> bool {
        let has = |hay: &str, needle: &str| hay.to_lowercase().contains(&needle.to_lowercase());
        self.scope.as_deref().is_none_or(|q| has(&l.scope, q))
            && self.source.is_none_or(|s| s == l.source)
            && (!self.attention || matches!(l.level, Level::Warn | Level::Err))
            && self
                .text
                .as_deref()
                .is_none_or(|q| has(&l.text, q) || has(&l.scope, q))
    }
}

/// One agent hook record as a trace line. Tool results are skipped: the line that
/// announced the tool is the step.
fn describe_agent(rec: &Value) -> Option<TraceLine> {
    if crate::profile_progress::is_profile_record(rec) {
        return describe_profile(rec);
    }
    let detail = s(rec, "detail");
    let tool = s(rec, "tool");
    let (level, text) = match s(rec, "event") {
        "PreToolUse" => (
            Level::Run,
            if detail.is_empty() {
                tool.to_string()
            } else {
                format!("{tool}: {detail}")
            },
        ),
        "Notification" => (Level::Warn, format!("needs input: {detail}")),
        "Start" => (Level::Info, "agent started".to_string()),
        "Stop" => (Level::Info, "agent finished its turn".to_string()),
        _ => return None,
    };
    let at = DateTime::parse_from_rfc3339(s(rec, "at"))
        .map(|t| events::fmt_timestamp(t.with_timezone(&chrono::Utc)))
        .unwrap_or_else(|_| s(rec, "at").to_string());
    Some(TraceLine {
        source: Source::Agent,
        time: local_time(&at),
        at,
        scope: s(rec, "label").to_string(),
        level,
        text,
    })
}

/// One profile job's trace line (see `profile_progress`): only its step code,
/// counts, alias, section slug and a reason from the fixed list are shown;
/// anything malformed is dropped.
fn describe_profile(rec: &Value) -> Option<TraceLine> {
    use crate::profile_progress::{Step, FAIL_REASONS};
    let kind = s(rec, "kind");
    let n = |k: &str| rec.get(k).and_then(Value::as_u64).filter(|x| *x <= 1000);
    let (step, steps) = (n("step")?, n("steps")?);
    let code = Step::from_code(s(rec, "step_code"))?;
    let alias = s(rec, "alias");
    // A project alias: short, one line, no control characters.
    let plausible =
        |a: &str| !a.is_empty() && a.chars().count() <= 128 && !a.chars().any(char::is_control);
    // The scope is built from the kind and the alias, never the line's own label.
    let scope = match kind {
        "sync" if alias.is_empty() || plausible(alias) => "Profile sync".to_string(),
        "build" | "refresh" if plausible(alias) => format!("Profile \u{b7} {alias}"),
        _ => return None,
    };
    let name = match code {
        Step::Project if plausible(alias) => alias.to_string(),
        Step::Project => return None,
        c => c.name().to_string(),
    };
    let section = crate::project_profile::SectionKey::from_slug(s(rec, "section"));
    let (level, text) = match s(rec, "phase") {
        "start" => (
            Level::Info,
            match (kind, section) {
                ("build", _) => "building the profile".to_string(),
                ("refresh", Some(k)) => format!("refreshing the {} section", k.slug()),
                ("refresh", None) => "refreshing a section".to_string(),
                _ => "profile sync started".to_string(),
            },
        ),
        "step" if code == Step::Project => (Level::Info, format!("project {step}/{steps}: {name}")),
        "step" => (Level::Info, format!("step {step}/{steps}: {name}")),
        "done" => (Level::Ok, "finished".to_string()),
        "failed" => {
            let reason = s(rec, "reason");
            let reason = if FAIL_REASONS.contains(&reason) {
                reason
            } else {
                "error"
            };
            (
                Level::Err,
                format!("failed at step {step}/{steps} ({name}): {reason}"),
            )
        }
        "interrupted" => (Level::Err, "stopped before finishing".to_string()),
        _ => return None,
    };
    let at = DateTime::parse_from_rfc3339(s(rec, "at"))
        .map(|t| events::fmt_timestamp(t.with_timezone(&chrono::Utc)))
        .unwrap_or_else(|_| s(rec, "at").to_string());
    Some(TraceLine {
        source: Source::Profile,
        time: local_time(&at),
        at,
        scope,
        level,
        text,
    })
}

fn agent_lines(status: &Path) -> Vec<TraceLine> {
    let path = agent_activity::trace_path(&agent_activity::beside(status));
    std::fs::read_to_string(path)
        .map(|t| {
            t.lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter_map(|v| describe_agent(&v))
                .collect()
        })
        .unwrap_or_default()
}

/// The newest `limit` trace lines that pass `filter`, oldest first: ledger steps
/// from today and yesterday merged with the agents' tool steps under `base`
/// (beside `status`, the issue loop's status file). `after` keeps only lines newer than that `at` cursor.
pub fn recent(
    events_dir: &Path,
    status: &Path,
    limit: usize,
    after: Option<&str>,
    filter: &Filter,
) -> Vec<TraceLine> {
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
    let day_start = format!("{since}T00:00:00");
    let mut lines: Vec<TraceLine> = raw
        .iter()
        .filter_map(describe)
        .map(|mut l| {
            if let Some(scope) = issue_of.get(&l.scope) {
                l.scope.clone_from(scope);
            }
            l
        })
        .chain(
            agent_lines(status)
                .into_iter()
                .filter(|l| l.at >= day_start),
        )
        .filter(|l| after.is_none_or(|a| l.at.as_str() > a))
        .filter(|l| filter.matches(l))
        .collect();
    lines.sort_by(|a, b| a.at.cmp(&b.at));
    let skip = lines.len().saturating_sub(limit);
    lines.split_off(skip)
}

/// What is running right now, one line per active agent (`label  Tool: detail`)
/// and per profile job (`Profile · web  Asking the AI · step 3 of 5`).
pub fn running_now(status: &Path) -> Vec<String> {
    let mut out = agents_running_now(status);
    out.extend(
        crate::profile_progress::running(&agent_activity::beside(status))
            .iter()
            .map(|j| format!("{}  {}", j.label(), j.step_text())),
    );
    out
}

/// [`running_now`] without the profile jobs (a page that lists those itself, translated).
pub fn agents_running_now(status: &Path) -> Vec<String> {
    agent_activity::read_all(&agent_activity::beside(status))
        .iter()
        .filter(|a| s(a, "event") != "Stop" && !crate::profile_progress::is_profile_record(a))
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
        let all = recent(d.path(), d.path(), 10, None, &Filter::default());
        assert_eq!(all.len(), 3);
        assert!(all[0].text.starts_with("run started"));
        let last_two = recent(d.path(), d.path(), 2, None, &Filter::default());
        assert_eq!(last_two.len(), 2);
        assert_eq!(last_two[0].text, "working on it");
        let newer = recent(d.path(), d.path(), 10, Some(&all[1].at), &Filter::default());
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
        let lines = recent(d.path(), d.path(), 10, None, &Filter::default());
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.scope == "web #7"), "{lines:?}");
    }

    fn agent_rec(at: &str, event: &str, tool: &str, detail: &str) -> Value {
        json!({"at": at, "label": "web #7", "event": event, "tool": tool, "detail": detail})
    }

    #[test]
    fn agent_tool_steps_merge_into_the_trace_and_results_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("outputs")).unwrap();
        let status = d.path().join("outputs/status.json");
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        let body = [
            agent_rec(&now, "PreToolUse", "Bash", "cargo test"),
            agent_rec(&now, "PostToolUse", "Bash", "cargo test"),
        ]
        .iter()
        .map(|v| v.to_string() + "\n")
        .collect::<String>();
        std::fs::write(d.path().join("outputs/agent-trace.jsonl"), body).unwrap();
        let lines = recent(
            &d.path().join("events"),
            &status,
            10,
            None,
            &Filter::default(),
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(
            (lines[0].source, lines[0].text.as_str()),
            (Source::Agent, "Bash: cargo test")
        );
        assert_eq!(lines[0].scope, "web #7");
    }

    #[test]
    fn filters_narrow_by_scope_source_attention_and_text() {
        let step = describe(&ev(
            "issue.escalated",
            json!({"reason": "verification_failed"}),
        ))
        .unwrap();
        let agent = describe_agent(&agent_rec(
            "2026-11-09T12:00:00+00:00",
            "PreToolUse",
            "Edit",
            "src/a.rs",
        ))
        .unwrap();
        let f = |f: Filter| (f.matches(&step), f.matches(&agent));
        assert_eq!(f(Filter::default()), (true, true));
        assert_eq!(
            f(Filter {
                scope: Some("KURRANT".into()),
                ..Default::default()
            }),
            (true, false)
        );
        assert_eq!(
            f(Filter {
                source: Some(Source::Agent),
                ..Default::default()
            }),
            (false, true)
        );
        assert_eq!(
            f(Filter {
                attention: true,
                ..Default::default()
            }),
            (true, false)
        );
        assert_eq!(
            f(Filter {
                text: Some("a.rs".into()),
                ..Default::default()
            }),
            (false, true)
        );
    }

    fn profile_line(
        phase: &str,
        kind: &str,
        step: u64,
        steps: u64,
        code: &str,
        extra: Value,
    ) -> Value {
        let mut v = json!({"at": "2026-11-09T12:00:00+00:00", "label": "Profile \u{b7} web", "event": "Profile",
            "tool": "profile", "detail": "x", "kind": kind, "section": null, "alias": "web",
            "step": step, "steps": steps, "step_code": code, "phase": phase});
        if let Some(o) = extra.as_object() {
            for (k, val) in o {
                v[k] = val.clone();
            }
        }
        v
    }

    #[test]
    fn profile_job_lines_read_as_steps_scoped_to_the_job() {
        let l = describe_agent(&profile_line("step", "build", 3, 5, "ai", json!({}))).unwrap();
        assert_eq!(l.source, Source::Profile);
        assert_eq!(l.scope, "Profile \u{b7} web");
        assert_eq!(
            (l.level, l.text.as_str()),
            (Level::Info, "step 3/5: Asking the AI")
        );
        let start =
            describe_agent(&profile_line("start", "build", 0, 5, "start", json!({}))).unwrap();
        assert_eq!(start.text, "building the profile");
        let refresh = describe_agent(&profile_line(
            "start",
            "refresh",
            0,
            4,
            "start",
            json!({"section": "stack"}),
        ))
        .unwrap();
        assert_eq!(refresh.text, "refreshing the stack section");
        let done = describe_agent(&profile_line("done", "build", 5, 5, "save", json!({}))).unwrap();
        assert_eq!((done.level, done.text.as_str()), (Level::Ok, "finished"));
        let sync = describe_agent(&profile_line(
            "step",
            "sync",
            2,
            3,
            "project",
            json!({"label": "Profile sync", "alias": "api"}),
        ))
        .unwrap();
        assert_eq!(
            (sync.scope.as_str(), sync.text.as_str()),
            ("Profile sync", "project 2/3: api")
        );
    }

    #[test]
    fn a_profile_failure_is_an_error_with_only_a_known_reason() {
        let l = describe_agent(&profile_line(
            "failed",
            "build",
            3,
            5,
            "ai",
            json!({"reason": "the AI call failed"}),
        ))
        .unwrap();
        assert_eq!(l.level, Level::Err);
        assert_eq!(
            l.text,
            "failed at step 3/5 (Asking the AI): the AI call failed"
        );
        let odd = describe_agent(&profile_line(
            "failed",
            "build",
            3,
            5,
            "ai",
            json!({"reason": "cat ~/.ssh/id_rsa"}),
        ))
        .unwrap();
        assert_eq!(odd.text, "failed at step 3/5 (Asking the AI): error");
        let stopped = describe_agent(&profile_line(
            "interrupted",
            "build",
            2,
            5,
            "issues",
            json!({}),
        ))
        .unwrap();
        assert_eq!(
            (stopped.level, stopped.text.as_str()),
            (Level::Err, "stopped before finishing")
        );
        assert!(Filter {
            attention: true,
            ..Default::default()
        }
        .matches(&l));
    }

    #[test]
    fn a_profile_lines_scope_comes_from_its_kind_and_a_plausible_alias() {
        let l = describe_agent(&profile_line(
            "step",
            "build",
            1,
            5,
            "scan",
            json!({"label": "<b>anything</b>"}),
        ))
        .unwrap();
        assert_eq!(l.scope, "Profile \u{b7} web");
        let s = describe_agent(&profile_line(
            "start",
            "sync",
            0,
            0,
            "start",
            json!({"label": "x", "alias": ""}),
        ))
        .unwrap();
        assert_eq!(s.scope, "Profile sync");
        for alias in ["we\u{1b}[31mb", "a\nb", &"x".repeat(129), ""] {
            assert!(
                describe_agent(&profile_line(
                    "step",
                    "build",
                    1,
                    5,
                    "scan",
                    json!({"alias": alias})
                ))
                .is_none(),
                "{alias:?}"
            );
        }
        assert!(describe_agent(&profile_line(
            "step",
            "sync",
            1,
            2,
            "project",
            json!({"alias": "a\tb"})
        ))
        .is_none());
    }

    #[test]
    fn malformed_profile_lines_are_ignored() {
        for bad in [
            profile_line("step", "build", 3, 5, "rm -rf", json!({})),
            profile_line("step", "nope", 3, 5, "ai", json!({})),
            profile_line("wat", "build", 3, 5, "ai", json!({})),
            profile_line("step", "build", 3, 5, "ai", json!({"step": "three"})),
            profile_line("step", "build", 3, 5, "ai", json!({"steps": 5000})),
        ] {
            assert!(describe_agent(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn profile_lines_filter_as_their_own_source_and_show_by_default() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("outputs")).unwrap();
        let status = d.path().join("outputs/status.json");
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        let body = [
            profile_line("step", "build", 1, 5, "scan", json!({"at": now})),
            agent_rec(&now, "PreToolUse", "Bash", "cargo test"),
        ]
        .iter()
        .map(|v| v.to_string() + "\n")
        .collect::<String>();
        std::fs::write(d.path().join("outputs/agent-trace.jsonl"), body).unwrap();
        let events = d.path().join("events");
        assert_eq!(
            recent(&events, &status, 10, None, &Filter::default()).len(),
            2
        );
        let only = recent(
            &events,
            &status,
            10,
            None,
            &Filter {
                source: Some(Source::Profile),
                ..Default::default()
            },
        );
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].text, "step 1/5: Scanning repo");
        assert_eq!(Source::Profile.css(), "profile");
    }

    #[test]
    fn running_now_names_profile_jobs_and_the_agents_only_list_leaves_them_out() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("outputs")).unwrap();
        let status = d.path().join("outputs/status.json");
        let base = agent_activity::beside(&status);
        let job = crate::profile_progress::ProfileJob::start(
            &base,
            crate::profile_progress::JobKind::Build,
            "web",
        );
        crate::profile_progress::Progress::step(&job, 3, 5, crate::profile_progress::Step::Ai);
        agent_activity::start(&agent_activity::for_task(&base, "api", "2"), "api #2", "t").unwrap();
        let all = running_now(&status);
        assert!(
            all.contains(&"Profile \u{b7} web  Asking the AI \u{b7} step 3 of 5".to_string()),
            "{all:?}"
        );
        assert_eq!(all.len(), 2, "{all:?}");
        assert_eq!(agents_running_now(&status), ["api #2  thinking"]);
    }

    #[test]
    fn a_line_renders_time_scope_glyph_and_text() {
        let l = TraceLine {
            source: Source::Step,
            at: "a".into(),
            time: "19:57:03".into(),
            scope: "web #1".into(),
            level: Level::Ok,
            text: "done".into(),
        };
        assert_eq!(l.render(8), "19:57:03  web #1    ✓ done");
    }
}
