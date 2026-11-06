//! Fix jobs the MR page's "Fix now" launches: `<dir>/<id>.json` plus `<id>.log`.
//!
//! A `running` record's pid is the runner subprocess (`loopx mr-fix run <mr_ref>`), which
//! calls [`finish`] itself when done. Records are reconciled against the live pid.

use crate::run_control::{kill_process_group, process_alive};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Launches the runner with its log path; returns the child pid (its own process group).
pub type Spawn<'a> = dyn Fn(&Path) -> Result<i64, String> + 'a;

#[derive(Debug, Clone, PartialEq)]
pub struct FixJob {
    pub id: String,
    /// The merge request reference; stored as data only.
    pub mr_ref: String,
    pub title: String,
    /// running | done | failed | stopped | nothing
    pub state: String,
    pub pid: Option<i64>,
    pub started_at: String,
    /// e.g. "Fixed 5 of 7"
    pub summary: String,
    pub log_path: PathBuf,
}

const FINAL_STATES: [&str; 4] = ["done", "failed", "stopped", "nothing"];

/// `<loopx_home>/mr-fix/jobs`.
pub fn jobs_dir(loopx_home: &Path) -> PathBuf {
    loopx_home.join("mr-fix").join("jobs")
}

pub fn running<'a>(jobs: &'a [FixJob], mr_ref: &str) -> Option<&'a FixJob> {
    jobs.iter()
        .find(|j| j.state == "running" && j.mr_ref == mr_ref)
}

/// The newest job (by `started_at`) for `mr_ref`.
pub fn latest_for<'a>(jobs: &'a [FixJob], mr_ref: &str) -> Option<&'a FixJob> {
    jobs.iter()
        .filter(|j| j.mr_ref == mr_ref)
        .max_by(|a, b| a.started_at.cmp(&b.started_at).then(a.id.cmp(&b.id)))
}

/// A job id is a filename component: ASCII alphanumerics, `-` and `_` only.
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn new_id() -> String {
    format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
}

fn record_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn write_record(dir: &Path, r: &FixJob) -> Result<(), String> {
    let v = json!({
        "id": r.id, "mr_ref": r.mr_ref, "title": r.title, "state": r.state,
        "pid": r.pid, "started_at": r.started_at, "summary": r.summary,
    });
    crate::settings_store::atomic_write_json(&v, &record_path(dir, &r.id))
        .map_err(|e| e.to_string())
}

fn read_record(dir: &Path, id: &str) -> Option<FixJob> {
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(record_path(dir, id)).ok()?).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    Some(FixJob {
        id: id.to_string(),
        mr_ref: s("mr_ref")?,
        title: s("title").unwrap_or_default(),
        state: s("state")?,
        pid: v.get("pid").and_then(Value::as_i64),
        started_at: s("started_at").unwrap_or_default(),
        summary: s("summary").unwrap_or_default(),
        log_path: dir.join(format!("{id}.log")),
    })
}

/// Under the record's cross-process lock, re-read it and, only if it is still
/// `running`, apply `change` and write it. Returns the record as it is on disk
/// afterwards (`None` if it vanished). A runner's `finish` or a user's `stop` that
/// landed first is never overwritten by a stale in-memory copy.
fn update_if_running(
    dir: &Path,
    id: &str,
    change: impl FnOnce(&mut FixJob),
) -> Result<Option<FixJob>, String> {
    crate::file_lock::with_lock(&record_path(dir, id), || {
        let Some(mut cur) = read_record(dir, id) else {
            return Ok(None);
        };
        if cur.state == "running" {
            change(&mut cur);
            write_record(dir, &cur)?;
        }
        Ok(Some(cur))
    })
    .map_err(|e| e.to_string())?
}

/// Settle a `running` record whose process is gone. The runner records its own result
/// via [`finish`]; a record still `running` with a dead pid ended unexpectedly.
fn reconcile(dir: &Path, r: FixJob) -> FixJob {
    if r.state != "running" {
        return r;
    }
    let dead = match r.pid {
        Some(p) => !process_alive(p),
        // Pre-spawn placeholder: abandoned only if it never got a pid for a while.
        None => chrono::DateTime::parse_from_rfc3339(&r.started_at)
            .map(|t| (chrono::Utc::now() - t.to_utc()).num_seconds() > 120)
            .unwrap_or(true),
    };
    if !dead {
        return r;
    }
    match update_if_running(dir, &r.id, |c| {
        c.state = "failed".to_string();
        c.summary = "the run ended unexpectedly".to_string();
    }) {
        Ok(Some(fresh)) => fresh,
        _ => r,
    }
}

/// All jobs, newest first, with `running` ones reconciled against their pid.
pub fn list(dir: &Path) -> Vec<FixJob> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<FixJob> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".json")?;
            if !is_safe_id(id) {
                return None;
            }
            read_record(dir, id)
        })
        .map(|r| reconcile(dir, r))
        .collect();
    out.sort_by(|a, b| b.started_at.cmp(&a.started_at).then(b.id.cmp(&a.id)));
    out
}

/// One job by id, settled against its pid; `None` for an unsafe or unknown id.
pub fn get(dir: &Path, id: &str) -> Option<FixJob> {
    if !is_safe_id(id) {
        return None;
    }
    read_record(dir, id).map(|r| reconcile(dir, r))
}

static START_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Record and launch a job; fails if one is already running for `mr_ref`. The check,
/// the `running` record (written before spawning) and the spawn are serialized
/// process-wide; a failed spawn marks the record `failed`.
pub fn start(dir: &Path, mr_ref: &str, title: &str, spawn: &Spawn<'_>) -> Result<FixJob, String> {
    let _guard = START_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !super::valid_mr_ref(mr_ref) {
        return Err("invalid merge request reference".to_string());
    }
    if running(&list(dir), mr_ref).is_some() {
        return Err("a fix for this merge request is already running".to_string());
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let id = new_id();
    let rec = FixJob {
        log_path: dir.join(format!("{id}.log")),
        id,
        mr_ref: mr_ref.to_string(),
        title: title.to_string(),
        state: "running".to_string(),
        pid: None,
        started_at: chrono::Utc::now().to_rfc3339(),
        summary: String::new(),
    };
    write_record(dir, &rec)?;
    let pid = match spawn(&rec.log_path) {
        Ok(p) => p,
        Err(e) => {
            let _ = update_if_running(dir, &rec.id, |c| {
                c.state = "failed".to_string();
                c.summary = e.clone();
            });
            return Err(e);
        }
    };
    // Record the pid, but never over a result the runner already wrote.
    match update_if_running(dir, &rec.id, |c| c.pid = Some(pid)) {
        Ok(Some(fresh)) => Ok(fresh),
        other => {
            kill_process_group(pid, Duration::from_secs(5), Duration::from_millis(100));
            let _ = update_if_running(dir, &rec.id, |c| {
                c.state = "failed".to_string();
                c.summary = "could not record the run".to_string();
            });
            Err(other
                .err()
                .unwrap_or_else(|| "the job record vanished".to_string()))
        }
    }
}

/// Record a job's outcome. Only a `running` record changes: a settled one (stopped by
/// the user, say) is left as it is.
pub fn finish(dir: &Path, id: &str, state: &str, summary: &str) -> Result<(), String> {
    if !is_safe_id(id) {
        return Err("invalid job id".to_string());
    }
    if !FINAL_STATES.contains(&state) {
        return Err(format!("invalid final state: {state}"));
    }
    update_if_running(dir, id, |r| {
        r.state = state.to_string();
        r.summary = summary.to_string();
    })?
    .map(|_| ())
    .ok_or_else(|| "no such job".to_string())
}

/// Stop a running job (SIGTERM then SIGKILL to its process group) and mark it stopped.
/// Errors, leaving the record `running`, if the process survives.
pub fn stop(dir: &Path, id: &str) -> Result<(), String> {
    if !is_safe_id(id) {
        return Err("invalid job id".to_string());
    }
    // Held across the kill, so a finishing runner cannot interleave with the status.
    crate::file_lock::with_lock(&record_path(dir, id), || {
        let mut r = read_record(dir, id).ok_or_else(|| "no such job".to_string())?;
        if r.state == "running" {
            if let Some(pid) = r.pid.filter(|p| process_alive(*p)) {
                kill_process_group(pid, Duration::from_secs(5), Duration::from_millis(100));
                if process_alive(pid) {
                    return Err("the job's process did not exit".to_string());
                }
            }
            r.state = "stopped".to_string();
            write_record(dir, &r)?;
        }
        Ok(())
    })
    .map_err(|e| e.to_string())?
}

/// Launch `<exe> mr-fix run <mr_ref>` detached (own process group), output to `log`.
/// `mr_ref` is one argv element, never interpreted by a shell.
pub fn spawn_runner(log: &Path, exe: &str, mr_ref: &str) -> Result<i64, String> {
    let argv: Vec<String> = [exe, "mr-fix", "run", mr_ref]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let cwd = log.parent().unwrap_or_else(|| Path::new("."));
    crate::creator::spawn::spawn_agent(&argv, cwd, log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn me() -> i64 {
        std::process::id() as i64
    }
    fn fake(pid: i64) -> impl Fn(&Path) -> Result<i64, String> {
        move |_l| Ok(pid)
    }

    #[test]
    fn jobs_dir_is_under_the_loopx_home() {
        assert_eq!(
            jobs_dir(Path::new("/h")),
            Path::new("/h").join("mr-fix").join("jobs")
        );
    }

    #[test]
    fn start_refuses_a_second_running_job_for_the_same_mr() {
        let d = tempfile::tempdir().unwrap();
        let a = start(d.path(), "a:1!1", "T", &fake(me())).unwrap();
        assert_eq!(a.state, "running");
        assert!(start(d.path(), "a:1!1", "T", &fake(me())).is_err());
        start(d.path(), "a:1!2", "T2", &fake(me())).unwrap();
        let jobs = list(d.path());
        assert_eq!(jobs.len(), 2);
        assert_eq!(running(&jobs, "a:1!1").unwrap().id, a.id);
        assert!(running(&jobs, "a:1!3").is_none());
    }

    #[test]
    fn a_failed_spawn_marks_the_job_failed() {
        let d = tempfile::tempdir().unwrap();
        let bad = |_l: &Path| Err("nope".to_string());
        assert_eq!(start(d.path(), "a:1!1", "T", &bad).unwrap_err(), "nope");
        let jobs = list(d.path());
        assert_eq!(jobs[0].state, "failed");
        start(d.path(), "a:1!1", "T", &fake(me())).unwrap();
    }

    fn dead_record(dir: &Path, id: &str, state: &str) {
        let mut c = std::process::Command::new("true").spawn().unwrap();
        c.wait().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("{id}.json")),
            json!({"id": id, "mr_ref": "m", "title": "T", "state": state, "pid": c.id(),
                   "started_at": "2026-11-05T00:00:00Z", "summary": "kept"})
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn a_dead_pid_settles_running_to_failed() {
        let d = tempfile::tempdir().unwrap();
        dead_record(d.path(), "20261105T000000-aaaaaaaa", "running");
        let j = &list(d.path())[0];
        assert_eq!(j.state, "failed");
        assert!(j.summary.contains("ended unexpectedly"), "{}", j.summary);
        assert_eq!(
            get(d.path(), "20261105T000000-aaaaaaaa").unwrap().state,
            "failed"
        );
    }

    #[test]
    fn reconcile_never_overwrites_a_settled_record() {
        let d = tempfile::tempdir().unwrap();
        for (i, s) in ["done", "failed", "stopped", "nothing"].iter().enumerate() {
            let id = format!("20261105T00000{i}-bbbbbbbb");
            dead_record(d.path(), &id, s);
            let j = get(d.path(), &id).unwrap();
            assert_eq!((j.state.as_str(), j.summary.as_str()), (*s, "kept"));
        }
    }

    #[test]
    fn finish_records_state_and_summary() {
        let d = tempfile::tempdir().unwrap();
        let r = start(d.path(), "a:1!1", "T", &fake(me())).unwrap();
        finish(d.path(), &r.id, "done", "Fixed 5 of 7").unwrap();
        let j = get(d.path(), &r.id).unwrap();
        assert_eq!(
            (j.state.as_str(), j.summary.as_str()),
            ("done", "Fixed 5 of 7")
        );
        // A settled record is not changed again.
        finish(d.path(), &r.id, "failed", "later").unwrap();
        assert_eq!(get(d.path(), &r.id).unwrap().summary, "Fixed 5 of 7");
        assert!(finish(d.path(), &r.id, "bogus", "x").is_err());
        assert!(finish(d.path(), "../x", "done", "x").is_err());
        assert!(finish(d.path(), "20261105T000009-nothere0", "done", "x").is_err());
    }

    #[test]
    fn unsafe_ids_are_rejected() {
        let d = tempfile::tempdir().unwrap();
        for id in ["", "../x", "a/b", "a.b", ".."] {
            assert!(get(d.path(), id).is_none(), "{id}");
            assert!(stop(d.path(), id).is_err(), "{id}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn stop_kills_the_process_group_and_marks_stopped() {
        use std::os::unix::process::CommandExt;
        let d = tempfile::tempdir().unwrap();
        let spawn = |_l: &Path| {
            let c = std::process::Command::new("sleep")
                .arg("30")
                .process_group(0)
                .spawn()
                .map_err(|e| e.to_string())?;
            Ok(i64::from(c.id()))
        };
        let rec = start(d.path(), "a:1!1", "T", &spawn).unwrap();
        stop(d.path(), &rec.id).unwrap();
        assert!(!crate::run_control::process_alive(rec.pid.unwrap()));
        assert_eq!(list(d.path())[0].state, "stopped");
    }

    #[test]
    fn latest_for_is_the_newest_job_of_that_mr() {
        let d = tempfile::tempdir().unwrap();
        let mk = |id: &str, mr: &str, at: &str| FixJob {
            id: id.into(),
            mr_ref: mr.into(),
            title: String::new(),
            state: "done".into(),
            pid: None,
            started_at: at.into(),
            summary: String::new(),
            log_path: d.path().join(id),
        };
        let jobs = vec![
            mk("a", "m1", "2026-11-05T00:00:00Z"),
            mk("b", "m1", "2026-11-06T00:00:00Z"),
            mk("c", "m2", "2026-11-07T00:00:00Z"),
        ];
        assert_eq!(latest_for(&jobs, "m1").unwrap().id, "b");
        assert!(latest_for(&jobs, "zz").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn spawn_runner_runs_the_given_exe_and_logs() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("j.log");
        let pid = spawn_runner(&log, "/bin/echo", "a:1!7").unwrap();
        assert!(pid > 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            if text.contains("loopx-exit=0") {
                assert!(text.contains("mr-fix run a:1!7"), "{text}");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no exit marker: {text}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn start_refuses_an_invalid_mr_ref() {
        let d = tempfile::tempdir().unwrap();
        let called = std::cell::Cell::new(false);
        let spawn = |_l: &Path| {
            called.set(true);
            Ok(me())
        };
        for bad in ["-x:1!2", "a:b!c", ""] {
            assert!(start(d.path(), bad, "T", &spawn).is_err(), "{bad}");
        }
        assert!(!called.get());
        assert!(list(d.path()).is_empty());
    }

    #[test]
    fn a_stale_running_copy_never_overwrites_a_finished_record() {
        let d = tempfile::tempdir().unwrap();
        dead_record(d.path(), "20261105T000000-cccccccc", "running");
        let stale = read_record(d.path(), "20261105T000000-cccccccc").unwrap();
        // The runner finishes between reconcile's read and its liveness check.
        finish(d.path(), &stale.id, "done", "Fixed 5 of 7").unwrap();
        let r = reconcile(d.path(), stale);
        assert_eq!(
            (r.state.as_str(), r.summary.as_str()),
            ("done", "Fixed 5 of 7")
        );
        let j = read_record(d.path(), &r.id).unwrap();
        assert_eq!(
            (j.state.as_str(), j.summary.as_str()),
            ("done", "Fixed 5 of 7")
        );
    }

    #[test]
    fn a_runner_that_finishes_before_start_returns_keeps_its_result() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().to_path_buf();
        let spawn = |log: &Path| {
            let id = log.file_stem().unwrap().to_string_lossy().into_owned();
            finish(&dir, &id, "failed", "no worktree").unwrap();
            Ok(me())
        };
        let rec = start(d.path(), "a:1!1", "T", &spawn).unwrap();
        assert_eq!(rec.state, "failed");
        let j = get(d.path(), &rec.id).unwrap();
        assert_eq!(
            (j.state.as_str(), j.summary.as_str()),
            ("failed", "no worktree")
        );
        assert_eq!(j.pid, None);
    }
}
