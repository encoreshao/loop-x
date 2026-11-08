//! Creator jobs in LoopX's notification system.
//!
//! - **Mac app:** the snapshot lists a Find videos and a Generate video entry among the
//!   loops ([`loop_statuses`]), so the island shows them while they run, with the
//!   current stage, and the app posts its usual started / completed / failed
//!   notifications when they change state.
//! - **Slack:** [`notify_finished`] sends one message per finished job (done, failed, or
//!   exited cleanly with nothing produced), once, however many times it is called.

use super::jobs::{self, JobKind, JobRecord};
use crate::slack_report::{Field, Link, Report, State, Status};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::path::Path;

pub const FIND_LOOP: &str = "creator-find-videos";
pub const GENERATE_LOOP: &str = "creator-generate-video";
/// A failed job keeps the entry red for this long, then it reads idle again.
const FAILURE_HOURS: i64 = 24;
/// How many notified job ids are remembered.
const REMEMBERED: usize = 500;

fn loop_name(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Scout => FIND_LOOP,
        JobKind::Generate | JobKind::Showcase => GENERATE_LOOP,
    }
}

/// An address or topic for one line: no scheme, at most 60 characters.
fn short(key: &str) -> String {
    let bare = key
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    if bare.chars().count() <= 60 {
        return bare.to_string();
    }
    let cut: String = bare.chars().take(60).collect();
    format!("{cut}…")
}

fn stage_text(dir: &Path, rec: &JobRecord) -> String {
    match jobs::progress(dir, &rec.id) {
        Some(p) => format!("Stage {} of {} · {}", p.stage, p.total, p.label),
        None => "Starting".to_string(),
    }
}

fn reason(rec: &JobRecord) -> &'static str {
    match (rec.state.as_str(), rec.kind) {
        ("no_output", JobKind::Generate | JobKind::Showcase) => "No video produced",
        ("no_output", JobKind::Scout) => "No results",
        ("incomplete", _) => "Incomplete",
        _ => "Failed",
    }
}

fn finished_at(rec: &JobRecord) -> Option<DateTime<Utc>> {
    jobs::ended_time(rec)
        .map(DateTime::<Utc>::from)
        .or_else(|| {
            DateTime::parse_from_rfc3339(&rec.started_at)
                .ok()
                .map(|t| t.with_timezone(&Utc))
        })
}

/// The entries the snapshot adds to its loops: one per kind of job that has run.
/// `settled` is every job newest first (see [`jobs::settle_outputs`]). A running job
/// makes its entry `running` with the current stage; otherwise the newest finished one
/// decides: `failed` (for a day) when it failed or produced nothing, else `idle`.
pub fn loop_statuses(
    dir: &Path,
    settled: &[JobRecord],
    now: DateTime<Utc>,
) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    // One entry per loop: Generate and Showcase jobs share "creator-generate-video".
    for kind in [JobKind::Scout, JobKind::Generate] {
        let mine: Vec<&JobRecord> = settled
            .iter()
            .filter(|j| loop_name(j.kind) == loop_name(kind))
            .collect();
        let Some(newest) = mine.first() else {
            continue;
        };
        let doc = if let Some(run) = mine.iter().find(|j| j.state == "running") {
            json!({
                "state": "running",
                "updated_at": run.started_at,
                "current_issue": short(&run.key),
                "current_step": stage_text(dir, run),
            })
        } else {
            let at = finished_at(newest).unwrap_or(now);
            let failed = matches!(newest.state.as_str(), "failed" | "no_output" | "incomplete")
                && now - at < chrono::Duration::hours(FAILURE_HOURS);
            if failed {
                json!({
                    "state": "failed",
                    "updated_at": at.to_rfc3339(),
                    "current_issue": short(&newest.key),
                    "current_step": reason(newest),
                })
            } else {
                json!({"state": "idle", "updated_at": at.to_rfc3339()})
            }
        };
        out.push((loop_name(kind).to_string(), doc));
    }
    out
}

fn took(rec: &JobRecord) -> Option<String> {
    let start = DateTime::parse_from_rfc3339(&rec.started_at).ok()?;
    let secs = (finished_at(rec)? - start.with_timezone(&Utc))
        .num_seconds()
        .max(0);
    Some(if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    })
}

/// The Slack message for a finished job.
pub fn finished_report(dir: &Path, rec: &JobRecord) -> Report {
    let generate = rec.kind.makes_video();
    let (title, state, status, summary) = match (rec.state.as_str(), generate) {
        ("done", true) => (
            "Video generated",
            State::Done,
            "Finished",
            "A new short video is ready to review under Creator → Videos.",
        ),
        ("done", false) => (
            "Video search finished",
            State::Done,
            "Finished",
            "The results are in Creator → Library.",
        ),
        ("no_output", true) => (
            "Video generation produced no video",
            State::Failed,
            "No video produced",
            "The run ended without error but no video file was written. Open its log under Creator → Jobs.",
        ),
        ("incomplete", _) => (
            "Video generation incomplete",
            State::Failed,
            "Incomplete",
            "The agent finished but says the video still needs work. Open its log under Creator → Jobs.",
        ),
        ("no_output", false) => (
            "Video search found nothing",
            State::Failed,
            "No results",
            "The run ended without error but wrote no results. Open its log under Creator → Jobs.",
        ),
        (_, true) => (
            "Video generation failed",
            State::Failed,
            "Failed",
            "The run did not finish. Open its log under Creator → Jobs.",
        ),
        (_, false) => (
            "Video search failed",
            State::Failed,
            "Failed",
            "The run did not finish. Open its log under Creator → Jobs.",
        ),
    };
    let mut fields = vec![Field {
        label: if generate { "Source" } else { "Topic" }.into(),
        value: short(&rec.key),
        url: rec.key.starts_with("https://").then(|| rec.key.clone()),
    }];
    if state == State::Failed {
        if let Some(p) = jobs::progress(dir, &rec.id) {
            fields.push(Field {
                label: "Got as far as".into(),
                value: p.label.to_string(),
                url: None,
            });
        }
    }
    if let Some(t) = took(rec) {
        fields.push(Field {
            label: "Took".into(),
            value: t,
            url: None,
        });
    }
    Report {
        title: title.into(),
        status: Some(Status {
            state,
            text: status.into(),
        }),
        summary: Some(summary.into()),
        fields,
        footer: vec![Link {
            text: "LoopX Creator".into(),
            url: None,
        }],
        ..Report::default()
    }
}

fn read_notified(path: &Path) -> Option<Vec<String>> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some(
        v.as_array()?
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
    )
}

fn write_notified(path: &Path, ids: &[String]) {
    let keep = &ids[ids.len().saturating_sub(REMEMBERED)..];
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = crate::settings_store::atomic_write_json(&json!(keep), path);
}

/// Send one message for every finished job (done, failed or no output) not yet
/// announced, oldest first, and remember it. `send` posts a report and says whether it
/// went out; a job is remembered either way, so a missing Slack setup is not retried
/// forever. The first call ever only records what already finished, so turning this on
/// never replays old history. Returns how many were sent.
pub fn notify_finished(
    dir: &Path,
    notified_path: &Path,
    user_home: &Path,
    send: &dyn Fn(&Report) -> bool,
) -> usize {
    let settled = jobs::settle_outputs(jobs::list_jobs(dir), user_home);
    let finished: Vec<&JobRecord> = settled
        .iter()
        .filter(|j| {
            matches!(
                j.state.as_str(),
                "done" | "failed" | "no_output" | "incomplete"
            )
        })
        .collect();
    let Some(mut known) = read_notified(notified_path) else {
        let ids: Vec<String> = finished.iter().rev().map(|j| j.id.clone()).collect();
        write_notified(notified_path, &ids);
        return 0;
    };
    let mut sent = 0;
    for rec in finished.iter().rev() {
        if known.contains(&rec.id) {
            continue;
        }
        if send(&finished_report(dir, rec)) {
            sent += 1;
        }
        known.push(rec.id.clone());
    }
    write_notified(notified_path, &known);
    sent
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn record(dir: &Path, id: &str, kind: &str, key: &str, alive: bool, started: &str, log: &str) {
        let pid = if alive {
            i64::from(std::process::id())
        } else {
            let mut c = std::process::Command::new("true").spawn().unwrap();
            c.wait().unwrap();
            i64::from(c.id())
        };
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("{id}.json")),
            json!({"id": id, "kind": kind, "key": key, "state": "running", "pid": pid,
                   "started_at": started})
            .to_string(),
        )
        .unwrap();
        std::fs::write(dir.join(format!("{id}.log")), log).unwrap();
    }

    fn step(cmd: &str) -> String {
        format!(
            "{}\n",
            json!({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "name": "Bash", "input": {"command": cmd}}]}})
        )
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn stamp(ago_minutes: i64) -> String {
        (now() - chrono::Duration::minutes(ago_minutes)).to_rfc3339()
    }

    #[test]
    fn a_running_job_makes_its_entry_running_with_the_stage() {
        let d = tempfile::tempdir().unwrap();
        record(
            d.path(),
            "20261105T000001-gen000000",
            "generate",
            "https://www.ted.com/talks/linda_hill_how_to_manage",
            true,
            &stamp(3),
            &step("python scripts/transcribe.py a.wav"),
        );
        let settled = jobs::list_jobs(d.path());
        let out = loop_statuses(d.path(), &settled, now());
        assert_eq!(out.len(), 1);
        let (name, doc) = &out[0];
        assert_eq!(name, GENERATE_LOOP);
        assert_eq!(doc["state"], "running");
        assert_eq!(doc["current_step"], "Stage 3 of 9 · Transcribe");
        assert_eq!(
            doc["current_issue"],
            "www.ted.com/talks/linda_hill_how_to_manage"
        );
    }

    #[test]
    fn a_recent_failure_is_failed_an_old_one_or_a_success_is_idle_and_unused_kinds_are_absent() {
        let d = tempfile::tempdir().unwrap();
        record(
            d.path(),
            "20261105T000001-scout000",
            "scout",
            "leadership",
            false,
            &stamp(10),
            "boom\n",
        );
        record(
            d.path(),
            "20261105T000002-gen000000",
            "generate",
            "https://x.io/a",
            false,
            &stamp(60 * 30),
            "boom\n",
        );
        // The old job's log was last written when it ended, a day and a half ago.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 3600);
        std::fs::File::options()
            .write(true)
            .open(d.path().join("20261105T000002-gen000000.log"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let settled = jobs::list_jobs(d.path());
        let out = loop_statuses(d.path(), &settled, now());
        let get = |n: &str| {
            out.iter()
                .find(|(k, _)| k == n)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(get(FIND_LOOP)["state"], "failed");
        assert_eq!(get(FIND_LOOP)["current_step"], "Failed");
        assert_eq!(
            get(GENERATE_LOOP)["state"],
            "idle",
            "a day-old failure is no longer red"
        );
        // A kind that never ran has no entry.
        let only_scout = loop_statuses(d.path(), &settled[..1], now());
        assert_eq!(only_scout.len(), 1);
    }

    #[test]
    fn a_clean_exit_that_produced_nothing_reads_failed() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        record(
            d.path(),
            "20261105T000001-gen000000",
            "generate",
            "https://x.io/a",
            false,
            &stamp(5),
            "loopx-exit=0\n",
        );
        let settled = jobs::settle_outputs(jobs::list_jobs(d.path()), home.path());
        let out = loop_statuses(d.path(), &settled, now());
        assert_eq!(out[0].1["state"], "failed");
        assert_eq!(out[0].1["current_step"], "No video produced");
    }

    fn result_line(text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type": "result", "subtype": "success", "is_error": false, "result": text})
        )
    }

    #[test]
    fn a_clean_exit_whose_closing_result_says_incomplete_reads_failed_with_the_reason() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let log = [
            result_line(
                "Rendered a draft.\nResult: incomplete — the 864 px crop check was not done",
            ),
            "loopx-exit=0\n".to_string(),
        ]
        .concat();
        record(
            d.path(),
            "20261105T000001-gen000000",
            "generate",
            "https://x.io/a",
            false,
            &stamp(5),
            &log,
        );
        let settled = jobs::settle_outputs(jobs::list_jobs(d.path()), home.path());
        assert_eq!(settled[0].state, "incomplete");
        assert_eq!(
            jobs::incomplete_reason(&settled[0]).as_deref(),
            Some("the 864 px crop check was not done")
        );
        let out = loop_statuses(d.path(), &settled, now());
        assert_eq!(out[0].1["state"], "failed");
        assert_eq!(out[0].1["current_step"], "Incomplete");
        let r = finished_report(d.path(), &settled[0]);
        assert_eq!(r.title, "Video generation incomplete");
        assert_eq!(r.status.as_ref().unwrap().state, State::Failed);
        // A "Result: complete" closing line, or none (older runs), changes nothing.
        let ok = [
            result_line("All good.\nResult: complete"),
            "loopx-exit=0\n".to_string(),
        ]
        .concat();
        record(
            d.path(),
            "20261105T000002-gen000000",
            "generate",
            "https://x.io/b",
            false,
            &stamp(4),
            &ok,
        );
        let settled = jobs::settle_outputs(jobs::list_jobs(d.path()), home.path());
        let second = settled
            .iter()
            .find(|j| j.id.ends_with("000002-gen000000"))
            .unwrap();
        assert_ne!(second.state, "incomplete");
        assert!(jobs::incomplete_reason(second).is_none());
    }

    #[test]
    fn the_slack_message_says_what_happened_and_where_it_got_to() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        record(
            d.path(),
            "20261105T000001-gen000000",
            "generate",
            "https://www.ted.com/talks/x",
            false,
            &stamp(5),
            &[
                step("python scripts/transcribe.py a.wav"),
                "loopx-exit=1\n".to_string(),
            ]
            .concat(),
        );
        let settled = jobs::settle_outputs(jobs::list_jobs(d.path()), home.path());
        let r = finished_report(d.path(), &settled[0]);
        assert_eq!(r.title, "Video generation failed");
        assert_eq!(r.status.as_ref().unwrap().state, State::Failed);
        let field = |l: &str| {
            r.fields
                .iter()
                .find(|f| f.label == l)
                .map(|f| f.value.clone())
        };
        assert_eq!(field("Source").as_deref(), Some("www.ted.com/talks/x"));
        assert_eq!(
            r.fields[0].url.as_deref(),
            Some("https://www.ted.com/talks/x")
        );
        assert_eq!(field("Got as far as").as_deref(), Some("Transcribe"));
        assert!(field("Took").is_some());
        // A search: a topic, never a link.
        record(
            d.path(),
            "20261105T000002-scout000",
            "scout",
            "<!channel> leadership",
            false,
            &stamp(5),
            "loopx-exit=1\n",
        );
        let settled = jobs::settle_outputs(jobs::list_jobs(d.path()), home.path());
        let scout = settled.iter().find(|j| j.kind == JobKind::Scout).unwrap();
        let r = finished_report(d.path(), scout);
        assert_eq!(r.title, "Video search failed");
        assert_eq!(r.fields[0].label, "Topic");
        assert!(r.fields[0].url.is_none());
    }

    #[test]
    fn each_finished_job_is_announced_once_and_history_is_not_replayed() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let notified = d.path().join("notified.json");
        // History from before notifications existed.
        record(
            d.path(),
            "20261105T000001-old000000",
            "generate",
            "https://x.io/old",
            false,
            &stamp(500),
            "boom\n",
        );
        let sent = RefCell::new(Vec::<String>::new());
        let send = |r: &Report| {
            sent.borrow_mut().push(r.title.clone());
            true
        };
        assert_eq!(
            notify_finished(d.path(), &notified, home.path(), &send),
            0,
            "first call records only"
        );
        assert!(sent.borrow().is_empty());
        // A new failure, a running job and a stopped one.
        record(
            d.path(),
            "20261105T000002-new000000",
            "generate",
            "https://x.io/new",
            false,
            &stamp(2),
            "boom\n",
        );
        record(
            d.path(),
            "20261105T000003-live00000",
            "scout",
            "q",
            true,
            &stamp(1),
            "",
        );
        assert_eq!(notify_finished(d.path(), &notified, home.path(), &send), 1);
        assert_eq!(sent.borrow().as_slice(), ["Video generation failed"]);
        // Again: nothing new.
        assert_eq!(notify_finished(d.path(), &notified, home.path(), &send), 0);
        assert_eq!(sent.borrow().len(), 1);
    }

    #[test]
    fn a_failed_send_is_not_retried_forever() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let notified = d.path().join("notified.json");
        assert_eq!(
            notify_finished(d.path(), &notified, home.path(), &|_| true),
            0
        );
        record(
            d.path(),
            "20261105T000002-new000000",
            "generate",
            "https://x.io/new",
            false,
            &stamp(2),
            "boom\n",
        );
        let tries = RefCell::new(0);
        let send = |_: &Report| {
            *tries.borrow_mut() += 1;
            false
        };
        assert_eq!(notify_finished(d.path(), &notified, home.path(), &send), 0);
        assert_eq!(notify_finished(d.path(), &notified, home.path(), &send), 0);
        assert_eq!(*tries.borrow(), 1);
    }
}
