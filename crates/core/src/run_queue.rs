//! The run queue behind Mission Control: loops that are due (or run by hand) are
//! enqueued here and a single worker starts them one by one. The queue is one
//! JSON file (`run_queue.json` in the loopx home) shared by the scheduler tick,
//! the worker and the dashboard; every change is a read-modify-write under an
//! exclusive `flock` on a sibling lock file, written atomically, so a crash never
//! leaves a half-written queue.

use crate::config::home_dir;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Finished entries kept for the "Recent" list.
const MAX_FINISHED: usize = 50;
/// A manual "Run now" jumps ahead of scheduled work.
pub const PRIORITY_MANUAL: i32 = 10;
pub const PRIORITY_SCHEDULED: i32 = 0;

pub fn default_queue_path() -> PathBuf {
    home_dir().join("run_queue.json")
}

/// Held by the one process that is draining the queue.
pub fn default_worker_lock_path() -> PathBuf {
    home_dir().join("run_queue_worker.lock")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Scheduled,
    Manual,
    /// A slot missed while the Mac slept or the app was closed.
    CatchUp,
    /// A matching event landed for an event-triggered loop.
    Event,
    /// A person decided an approval a parked issue was waiting on.
    Approval,
    /// A timer someone set came due.
    Timer,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Scheduled => "scheduled",
            Reason::Manual => "manual",
            Reason::CatchUp => "catch-up",
            Reason::Event => "event",
            Reason::Approval => "approval",
            Reason::Timer => "timer",
        }
    }

    fn priority(self) -> i32 {
        match self {
            Reason::Manual | Reason::Approval | Reason::Timer => PRIORITY_MANUAL,
            _ => PRIORITY_SCHEDULED,
        }
    }
}

/// `queued` -> `running` -> `done` | `failed` | `cancelled` | `skipped`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    #[serde(rename = "loop")]
    pub loop_name: String,
    pub reason: String,
    pub priority: i32,
    pub enqueued_at: String,
    pub state: String,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub pid: Option<i64>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub cancel_requested: bool,
    /// Extra arguments for `loopx run <loop>`, e.g. one issue's `alias iid`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct QueueFile {
    #[serde(default)]
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Enqueued {
    /// Added at `position` among the waiting entries (0 = next to start).
    Queued { id: String, position: usize },
    /// The loop is already waiting or running, so nothing was added.
    Coalesced { state: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum CancelOutcome {
    /// Removed before it started.
    Removed,
    /// Running: the caller must kill this process tree.
    Running(Option<i64>),
    NotFound,
}

/// What the dashboard shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub running: Vec<Entry>,
    /// In start order.
    pub queued: Vec<Entry>,
    /// Newest first.
    pub recent: Vec<Entry>,
}

#[derive(Debug, Clone)]
pub struct RunQueue {
    path: PathBuf,
}

fn waiting_order(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    b.priority
        .cmp(&a.priority)
        .then_with(|| a.enqueued_at.cmp(&b.enqueued_at))
        .then_with(|| a.id.cmp(&b.id))
}

impl RunQueue {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read-modify-write under the queue lock; `f`'s result is returned.
    fn with_file<T>(&self, f: impl FnOnce(&mut QueueFile) -> T) -> std::io::Result<T> {
        use std::os::unix::io::AsRawFd;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path.with_extension("lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut file = self.read_file();
        let out = f(&mut file);
        let finished: Vec<usize> = file
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !matches!(e.state.as_str(), "queued" | "running"))
            .map(|(i, _)| i)
            .collect();
        if finished.len() > MAX_FINISHED {
            let drop: Vec<usize> = finished[..finished.len() - MAX_FINISHED].to_vec();
            let mut i = 0;
            file.entries.retain(|_| {
                i += 1;
                !drop.contains(&(i - 1))
            });
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(out)
        // `lock` drops here and releases the flock.
    }

    /// A renamed loop's entries read under its current id.
    fn read_file(&self) -> QueueFile {
        let mut file: QueueFile = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        for e in &mut file.entries {
            let current = crate::loops_config::canonical_loop_name(&e.loop_name);
            if current != e.loop_name {
                e.loop_name = current.to_string();
            }
        }
        file
    }

    /// Adds `loop_name` unless it is already queued or running.
    pub fn enqueue(&self, loop_name: &str, reason: Reason, now: &str) -> std::io::Result<Enqueued> {
        self.enqueue_with_args(loop_name, &[], reason, now)
    }

    /// [`RunQueue::enqueue`] for `loopx run <loop> <args...>`; it coalesces only
    /// with an entry of the same loop and the same args.
    pub fn enqueue_with_args(
        &self,
        loop_name: &str,
        args: &[String],
        reason: Reason,
        now: &str,
    ) -> std::io::Result<Enqueued> {
        let loop_name = crate::loops_config::canonical_loop_name(loop_name);
        self.with_file(|f| {
            if let Some(e) = f.entries.iter().find(|e| {
                e.loop_name == loop_name
                    && e.args == args
                    && matches!(e.state.as_str(), "queued" | "running")
            }) {
                return Enqueued::Coalesced {
                    state: e.state.clone(),
                };
            }
            let entry = Entry {
                id: uuid::Uuid::new_v4().simple().to_string(),
                loop_name: loop_name.to_string(),
                reason: reason.as_str().to_string(),
                priority: reason.priority(),
                enqueued_at: now.to_string(),
                state: "queued".into(),
                started_at: None,
                finished_at: None,
                pid: None,
                exit_code: None,
                note: None,
                cancel_requested: false,
                args: args.to_vec(),
            };
            let id = entry.id.clone();
            f.entries.push(entry);
            let mut waiting: Vec<&Entry> =
                f.entries.iter().filter(|e| e.state == "queued").collect();
            waiting.sort_by(|a, b| waiting_order(a, b));
            let position = waiting.iter().position(|e| e.id == id).unwrap_or(0);
            Enqueued::Queued { id, position }
        })
    }

    /// Records a run that was due but did not start, straight into "Recent".
    pub fn record_skipped(
        &self,
        loop_name: &str,
        reason: Reason,
        note: &str,
        now: &str,
    ) -> std::io::Result<()> {
        self.with_file(|f| {
            f.entries.push(Entry {
                id: uuid::Uuid::new_v4().simple().to_string(),
                loop_name: loop_name.to_string(),
                reason: reason.as_str().to_string(),
                priority: reason.priority(),
                enqueued_at: now.to_string(),
                state: "skipped".into(),
                started_at: None,
                finished_at: Some(now.to_string()),
                pid: None,
                exit_code: None,
                note: Some(note.to_string()),
                cancel_requested: false,
                args: Vec::new(),
            });
        })
    }

    /// Starts the next waiting entry unless `max_running` are already running.
    /// A loop that is already running is never started a second time.
    pub fn claim_next(&self, now: &str, max_running: usize) -> std::io::Result<Option<Entry>> {
        self.with_file(|f| {
            let running: Vec<String> = f
                .entries
                .iter()
                .filter(|e| e.state == "running")
                .map(|e| e.loop_name.clone())
                .collect();
            if running.len() >= max_running {
                return None;
            }
            let next = f
                .entries
                .iter()
                .filter(|e| e.state == "queued" && !running.contains(&e.loop_name))
                .min_by(|a, b| waiting_order(a, b))?
                .id
                .clone();
            let e = f.entries.iter_mut().find(|e| e.id == next)?;
            e.state = "running".into();
            e.started_at = Some(now.to_string());
            Some(e.clone())
        })
    }

    pub fn set_pid(&self, id: &str, pid: i64) -> std::io::Result<()> {
        self.with_file(|f| {
            if let Some(e) = f.entries.iter_mut().find(|e| e.id == id) {
                e.pid = Some(pid);
            }
        })
    }

    /// Records how a claimed entry ended. A stop that was asked for wins over the exit code.
    pub fn finish(
        &self,
        id: &str,
        state: &str,
        exit_code: Option<i32>,
        note: Option<&str>,
        now: &str,
    ) -> std::io::Result<()> {
        self.with_file(|f| {
            if let Some(e) = f.entries.iter_mut().find(|e| e.id == id) {
                e.state = if e.cancel_requested {
                    "cancelled".into()
                } else {
                    state.to_string()
                };
                e.exit_code = exit_code;
                e.note = note.map(String::from);
                e.finished_at = Some(now.to_string());
            }
        })
    }

    /// Removes a waiting entry, or flags a running one for the caller to kill.
    pub fn cancel(&self, id: &str, now: &str) -> std::io::Result<CancelOutcome> {
        self.with_file(|f| {
            let Some(e) = f.entries.iter_mut().find(|e| e.id == id) else {
                return CancelOutcome::NotFound;
            };
            match e.state.as_str() {
                "queued" => {
                    e.state = "cancelled".into();
                    e.finished_at = Some(now.to_string());
                    CancelOutcome::Removed
                }
                "running" => {
                    e.cancel_requested = true;
                    CancelOutcome::Running(e.pid)
                }
                _ => CancelOutcome::NotFound,
            }
        })
    }

    /// Moves a waiting entry to the front of the queue.
    pub fn move_to_front(&self, id: &str) -> std::io::Result<bool> {
        self.with_file(|f| {
            let top = f
                .entries
                .iter()
                .filter(|e| e.state == "queued")
                .map(|e| e.priority)
                .max();
            let Some(top) = top else { return false };
            match f
                .entries
                .iter_mut()
                .find(|e| e.id == id && e.state == "queued")
            {
                Some(e) => {
                    e.priority = top + 1;
                    true
                }
                None => false,
            }
        })
    }

    /// Fails every `running` entry whose process is gone (a worker that died).
    pub fn recover(&self, now: &str, alive: &dyn Fn(i64) -> bool) -> std::io::Result<usize> {
        let ended = self.with_file(|f| {
            let mut ended = Vec::new();
            for e in f.entries.iter_mut().filter(|e| e.state == "running") {
                if !e.pid.is_some_and(alive) {
                    e.state = if e.cancel_requested {
                        "cancelled"
                    } else {
                        "failed"
                    }
                    .into();
                    e.note = Some("The run's process ended without reporting a result".into());
                    e.finished_at = Some(now.to_string());
                    ended.push((e.loop_name.clone(), e.cancel_requested));
                }
            }
            ended
        })?;
        for (loop_name, cancelled) in &ended {
            self.settle_loop_status(loop_name, *cancelled);
        }
        Ok(ended.len())
    }

    /// A run killed before it could write its own result leaves its loop's
    /// status file saying `running` forever; mark it ended.
    fn settle_loop_status(&self, loop_name: &str, cancelled: bool) {
        let path = crate::loop_scheduler::status_path_for_loop(loop_name, self.path.parent());
        let status = crate::status::read_status(&path);
        if status.get("state").and_then(serde_json::Value::as_str) != Some("running") {
            return;
        }
        let state = if cancelled { "stopped" } else { "failed" };
        let _ = crate::status::write_status(&path, state, &[]);
    }

    pub fn snapshot(&self, recent: usize) -> Snapshot {
        let f = self.read_file();
        let mut snap = Snapshot::default();
        for e in f.entries.iter().rev() {
            match e.state.as_str() {
                "running" => snap.running.push(e.clone()),
                "queued" => snap.queued.push(e.clone()),
                _ if snap.recent.len() < recent => snap.recent.push(e.clone()),
                _ => {}
            }
        }
        snap.running.reverse();
        snap.queued.sort_by(waiting_order);
        snap
    }

    pub fn is_active(&self, loop_name: &str) -> bool {
        let loop_name = crate::loops_config::canonical_loop_name(loop_name);
        self.read_file()
            .entries
            .iter()
            .any(|e| e.loop_name == loop_name && matches!(e.state.as_str(), "queued" | "running"))
    }
}

/// The Stop button: a waiting entry is removed, a running one has its process
/// group killed (the worker then records it as `cancelled`).
pub fn stop_entry(queue: &RunQueue, id: &str, now: &str) -> std::io::Result<CancelOutcome> {
    let outcome = queue.cancel(id, now)?;
    if let CancelOutcome::Running(Some(pid)) = outcome {
        crate::run_control::kill_process_group(
            pid,
            std::time::Duration::from_secs(3),
            std::time::Duration::from_millis(100),
        );
    }
    Ok(outcome)
}

/// Drains the queue: keeps up to `max_running` entries running at once (each
/// a different loop: a loop is never queued twice), starting the next waiting
/// one as soon as a slot frees, and records each outcome, until nothing is
/// waiting or running. `spawn` returns the child so its pid is recorded (the
/// Stop button kills it). Returns how many ran.
pub fn drain(
    queue: &RunQueue,
    now: &dyn Fn() -> String,
    max_running: usize,
    spawn: &mut dyn FnMut(&Entry) -> std::io::Result<std::process::Child>,
) -> usize {
    const POLL: std::time::Duration = std::time::Duration::from_millis(100);
    let max_running = max_running.max(1);
    let _ = queue.recover(&now(), &|pid| crate::run_control::process_alive(pid));
    let mut ran = 0;
    let mut running: Vec<(Entry, std::process::Child)> = Vec::new();
    loop {
        while running.len() < max_running {
            let Ok(Some(entry)) = queue.claim_next(&now(), max_running) else {
                break;
            };
            ran += 1;
            match spawn(&entry) {
                Ok(child) => {
                    let _ = queue.set_pid(&entry.id, i64::from(child.id()));
                    running.push((entry, child));
                }
                Err(e) => {
                    let _ = queue.finish(&entry.id, "failed", None, Some(&e.to_string()), &now());
                }
            }
        }
        if running.is_empty() {
            return ran;
        }
        std::thread::sleep(POLL);
        let mut still = Vec::with_capacity(running.len());
        for (entry, mut child) in running {
            match child.try_wait() {
                Ok(None) => still.push((entry, child)),
                Ok(Some(status)) => record_exit(queue, &entry, status.code(), now),
                Err(_) => record_exit(queue, &entry, None, now),
            }
        }
        running = still;
    }
}

fn record_exit(queue: &RunQueue, entry: &Entry, code: Option<i32>, now: &dyn Fn() -> String) {
    let (state, note) = match code {
        Some(0) => ("done", None),
        Some(crate::run_loop::PRECHECK_SKIPPED_EXIT) => {
            ("skipped", Some("Its precheck found nothing to do"))
        }
        Some(crate::run_loop::QUOTA_PAUSED_EXIT) => (
            "skipped",
            Some("Paused near the AI subscription's usage limit; runs again after it resets"),
        ),
        _ => ("failed", None),
    };
    let _ = queue.finish(&entry.id, state, code, note, &now());
    let cancelled = queue
        .read_file()
        .entries
        .iter()
        .any(|e| e.id == entry.id && e.cancel_requested);
    queue.settle_loop_status(&entry.loop_name, cancelled);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue() -> (tempfile::TempDir, RunQueue) {
        let d = tempfile::tempdir().unwrap();
        let q = RunQueue::new(d.path().join("run_queue.json"));
        (d, q)
    }

    fn sleeper(secs: &str) -> std::io::Result<std::process::Child> {
        std::process::Command::new("sleep").arg(secs).spawn()
    }

    #[test]
    fn a_queued_gitlab_loop_is_the_issue_loop() {
        let (_d, q) = queue();
        std::fs::write(
            q.path(),
            r#"{"entries": [{"id": "1", "loop": "gitlab-loop", "reason": "scheduled", "priority": 0,
                "enqueued_at": "t0", "state": "queued"}]}"#,
        )
        .unwrap();
        assert!(q.is_active("issue-loop"));
        assert_eq!(q.snapshot(5).queued[0].loop_name, "issue-loop");
        // Not queued twice under its two names.
        assert!(matches!(
            q.enqueue("issue-loop", Reason::Manual, "t1").unwrap(),
            Enqueued::Coalesced { .. }
        ));
    }

    #[test]
    fn entries_with_args_coalesce_only_with_the_same_args() {
        let (_d, q) = queue();
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        q.enqueue("issue-loop", Reason::Scheduled, "t0").unwrap();
        let web7 = q
            .enqueue_with_args("issue-loop", &args(&["web", "7"]), Reason::Event, "t1")
            .unwrap();
        assert!(matches!(web7, Enqueued::Queued { .. }), "{web7:?}");
        assert!(matches!(
            q.enqueue_with_args("issue-loop", &args(&["web", "7"]), Reason::Event, "t2")
                .unwrap(),
            Enqueued::Coalesced { .. }
        ));
        assert!(matches!(
            q.enqueue_with_args("issue-loop", &args(&["web", "8"]), Reason::Event, "t3")
                .unwrap(),
            Enqueued::Queued { .. }
        ));
        let queued = q.snapshot(0).queued;
        assert_eq!(queued.len(), 3);
        assert_eq!(queued[1].args, ["web", "7"]);
        assert!(queued[0].args.is_empty());
    }

    #[test]
    fn a_loop_already_running_is_not_started_again() {
        let (_d, q) = queue();
        let args = vec!["web".to_string(), "7".to_string()];
        q.enqueue("issue-loop", Reason::Scheduled, "t0").unwrap();
        q.enqueue_with_args("issue-loop", &args, Reason::Event, "t1")
            .unwrap();
        q.enqueue("topic-loop", Reason::Scheduled, "t2").unwrap();
        let first = q.claim_next("t", 3).unwrap().unwrap();
        assert!(first.args.is_empty());
        let next = q.claim_next("t", 3).unwrap().unwrap();
        assert_eq!(next.loop_name, "topic-loop");
        assert_eq!(q.claim_next("t", 3).unwrap(), None);
        q.finish(&first.id, "done", Some(0), None, "t").unwrap();
        assert_eq!(q.claim_next("t", 3).unwrap().unwrap().args, args);
    }

    #[test]
    fn drain_runs_up_to_the_limit_of_loops_at_once() {
        let (_d, q) = queue();
        for name in ["a", "b", "c"] {
            q.enqueue(name, Reason::Scheduled, "t0").unwrap();
        }
        let started = std::time::Instant::now();
        let ran = drain(&q, &|| "t".to_string(), 3, &mut |_| sleeper("0.5"));
        let took = started.elapsed();
        assert_eq!(ran, 3);
        assert!(took < std::time::Duration::from_millis(1200), "{took:?}");
        let snap = q.snapshot(10);
        assert!(snap.running.is_empty() && snap.queued.is_empty());
        assert_eq!(snap.recent.iter().filter(|e| e.state == "done").count(), 3);
    }

    #[test]
    fn drain_with_a_limit_of_one_still_runs_them_one_by_one() {
        let (_d, q) = queue();
        for name in ["a", "b", "c"] {
            q.enqueue(name, Reason::Scheduled, "t0").unwrap();
        }
        let started = std::time::Instant::now();
        drain(&q, &|| "t".to_string(), 1, &mut |_| sleeper("0.3"));
        assert!(started.elapsed() >= std::time::Duration::from_millis(900));
    }

    #[test]
    fn entries_start_in_arrival_order_one_at_a_time() {
        let (_d, q) = queue();
        q.enqueue("a", Reason::Scheduled, "2026-10-27T10:00:00")
            .unwrap();
        q.enqueue("b", Reason::Scheduled, "2026-10-27T10:00:01")
            .unwrap();
        let a = q.claim_next("t1", 1).unwrap().unwrap();
        assert_eq!(a.loop_name, "a");
        assert!(q.claim_next("t2", 1).unwrap().is_none(), "one at a time");
        q.finish(&a.id, "done", Some(0), None, "t3").unwrap();
        assert_eq!(q.claim_next("t4", 1).unwrap().unwrap().loop_name, "b");
    }

    #[test]
    fn a_loop_already_waiting_or_running_is_not_queued_twice() {
        let (_d, q) = queue();
        assert!(matches!(
            q.enqueue("a", Reason::Scheduled, "t0").unwrap(),
            Enqueued::Queued { position: 0, .. }
        ));
        assert_eq!(
            q.enqueue("a", Reason::Manual, "t1").unwrap(),
            Enqueued::Coalesced {
                state: "queued".into()
            }
        );
        q.claim_next("t2", 1).unwrap();
        assert_eq!(
            q.enqueue("a", Reason::Scheduled, "t3").unwrap(),
            Enqueued::Coalesced {
                state: "running".into()
            }
        );
    }

    #[test]
    fn a_manual_run_jumps_ahead_of_scheduled_work() {
        let (_d, q) = queue();
        q.enqueue("a", Reason::Scheduled, "t0").unwrap();
        let out = q.enqueue("b", Reason::Manual, "t1").unwrap();
        assert!(matches!(out, Enqueued::Queued { position: 0, .. }));
        assert_eq!(q.claim_next("t2", 1).unwrap().unwrap().loop_name, "b");
    }

    #[test]
    fn move_to_front_and_cancel_work_on_waiting_entries() {
        let (_d, q) = queue();
        q.enqueue("a", Reason::Scheduled, "t0").unwrap();
        q.enqueue("b", Reason::Scheduled, "t1").unwrap();
        let Enqueued::Queued { id: c, .. } = q.enqueue("c", Reason::Scheduled, "t2").unwrap()
        else {
            panic!()
        };
        assert!(q.move_to_front(&c).unwrap());
        let snap = q.snapshot(10);
        assert_eq!(
            snap.queued
                .iter()
                .map(|e| e.loop_name.as_str())
                .collect::<Vec<_>>(),
            ["c", "a", "b"]
        );
        assert_eq!(q.cancel(&c, "t3").unwrap(), CancelOutcome::Removed);
        let snap = q.snapshot(10);
        assert_eq!(snap.queued.len(), 2);
        assert_eq!(snap.recent[0].state, "cancelled");
        assert_eq!(q.cancel("nope", "t4").unwrap(), CancelOutcome::NotFound);
    }

    #[test]
    fn cancelling_a_running_entry_returns_its_pid_and_wins_over_the_exit_code() {
        let (_d, q) = queue();
        q.enqueue("a", Reason::Scheduled, "t0").unwrap();
        let a = q.claim_next("t1", 1).unwrap().unwrap();
        q.set_pid(&a.id, 4242).unwrap();
        assert_eq!(
            q.cancel(&a.id, "t2").unwrap(),
            CancelOutcome::Running(Some(4242))
        );
        q.finish(&a.id, "failed", Some(143), None, "t3").unwrap();
        assert_eq!(q.snapshot(10).recent[0].state, "cancelled");
    }

    #[test]
    fn recover_fails_a_running_entry_whose_process_is_gone() {
        let (_d, q) = queue();
        q.enqueue("a", Reason::Scheduled, "t0").unwrap();
        let a = q.claim_next("t1", 1).unwrap().unwrap();
        q.set_pid(&a.id, 99).unwrap();
        assert_eq!(q.recover("t2", &|_| true).unwrap(), 0);
        assert_eq!(q.recover("t3", &|_| false).unwrap(), 1);
        let snap = q.snapshot(10);
        assert!(snap.running.is_empty());
        assert_eq!(snap.recent[0].state, "failed");
        assert!(!q.is_active("a"));
    }

    fn loop_status(q: &RunQueue, loop_name: &str) -> serde_json::Value {
        crate::status::read_status(&crate::loop_scheduler::status_path_for_loop(
            loop_name,
            q.path().parent(),
        ))
    }

    fn set_loop_status(q: &RunQueue, loop_name: &str, doc: serde_json::Value) {
        let path = crate::loop_scheduler::status_path_for_loop(loop_name, q.path().parent());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, doc.to_string()).unwrap();
    }

    #[test]
    fn recovering_a_dead_run_clears_the_loops_running_status() {
        let (_d, q) = queue();
        q.enqueue("issue-loop", Reason::Scheduled, "t0").unwrap();
        let a = q.claim_next("t1", 1).unwrap().unwrap();
        q.set_pid(&a.id, 99).unwrap();
        set_loop_status(
            &q,
            "issue-loop",
            serde_json::json!({"state": "running", "current_issue": "web #1", "current_step": "analyzing"}),
        );
        assert_eq!(q.recover("t2", &|_| false).unwrap(), 1);
        assert_eq!(loop_status(&q, "issue-loop")["state"], "failed");
    }

    #[test]
    fn a_run_that_exits_without_reporting_leaves_its_loop_failed_or_stopped() {
        let (_d, q) = queue();
        q.enqueue("a", Reason::Scheduled, "t0").unwrap();
        set_loop_status(
            &q,
            "a",
            serde_json::json!({"state": "running", "current_step": "x"}),
        );
        drain(&q, &|| "now".to_string(), 1, &mut |_| {
            std::process::Command::new("sh")
                .args(["-c", "exit 1"])
                .spawn()
        });
        assert_eq!(loop_status(&q, "a")["state"], "failed");

        q.enqueue("b", Reason::Scheduled, "t0").unwrap();
        let b = q.claim_next("t1", 1).unwrap().unwrap();
        q.cancel(&b.id, "t2").unwrap();
        set_loop_status(&q, "b", serde_json::json!({"state": "running"}));
        record_exit(&q, &b, Some(143), &|| "t3".to_string());
        assert_eq!(loop_status(&q, "b")["state"], "stopped");
    }

    #[test]
    fn a_run_that_reported_its_own_result_keeps_it() {
        let (_d, q) = queue();
        q.enqueue("a", Reason::Scheduled, "t0").unwrap();
        set_loop_status(
            &q,
            "a",
            serde_json::json!({"state": "idle", "last_exit_code": 0}),
        );
        drain(&q, &|| "now".to_string(), 1, &mut |_| {
            std::process::Command::new("sh")
                .args(["-c", "exit 0"])
                .spawn()
        });
        assert_eq!(loop_status(&q, "a")["state"], "idle");
        assert_eq!(loop_status(&q, "a")["last_exit_code"], 0);
    }

    #[test]
    fn a_corrupt_file_reads_as_an_empty_queue() {
        let (_d, q) = queue();
        std::fs::write(q.path(), "not json").unwrap();
        assert_eq!(q.snapshot(5), Snapshot::default());
        assert!(matches!(
            q.enqueue("a", Reason::Scheduled, "t").unwrap(),
            Enqueued::Queued { .. }
        ));
    }

    #[test]
    fn only_the_newest_finished_entries_are_kept() {
        let (_d, q) = queue();
        for i in 0..(MAX_FINISHED + 5) {
            q.enqueue(&format!("l{i}"), Reason::Scheduled, "t").unwrap();
            let e = q.claim_next("t", 1).unwrap().unwrap();
            q.finish(&e.id, "done", Some(0), None, "t").unwrap();
        }
        assert_eq!(q.snapshot(1000).recent.len(), MAX_FINISHED);
    }

    #[test]
    fn drain_runs_every_entry_in_order_and_records_exit_codes() {
        let (_d, q) = queue();
        q.enqueue("ok", Reason::Scheduled, "t0").unwrap();
        q.enqueue("bad", Reason::Scheduled, "t1").unwrap();
        q.enqueue("missing", Reason::Scheduled, "t2").unwrap();
        q.enqueue("idle", Reason::Scheduled, "t3").unwrap();
        q.enqueue("full", Reason::Scheduled, "t4").unwrap();
        let mut order = Vec::new();
        let ran = drain(&q, &|| "now".to_string(), 1, &mut |e| {
            order.push(e.loop_name.clone());
            match e.loop_name.as_str() {
                "ok" => std::process::Command::new("sh")
                    .args(["-c", "exit 0"])
                    .spawn(),
                "bad" => std::process::Command::new("sh")
                    .args(["-c", "exit 3"])
                    .spawn(),
                "idle" => std::process::Command::new("sh")
                    .args(["-c", "exit 75"])
                    .spawn(),
                "full" => std::process::Command::new("sh")
                    .args(["-c", "exit 76"])
                    .spawn(),
                _ => std::process::Command::new("/no/such/program").spawn(),
            }
        });
        assert_eq!(ran, 5);
        assert_eq!(order, ["ok", "bad", "missing", "idle", "full"]);
        let snap = q.snapshot(10);
        let by = |n: &str| {
            snap.recent
                .iter()
                .find(|e| e.loop_name == n)
                .unwrap()
                .clone()
        };
        assert_eq!(
            (by("ok").state.as_str(), by("ok").exit_code),
            ("done", Some(0))
        );
        assert_eq!(
            (by("bad").state.as_str(), by("bad").exit_code),
            ("failed", Some(3))
        );
        assert_eq!(by("missing").state, "failed");
        assert!(by("missing").note.is_some());
        assert_eq!(
            (by("idle").state.as_str(), by("idle").note.as_deref()),
            ("skipped", Some("Its precheck found nothing to do"))
        );
        assert_eq!(
            (by("full").state.as_str(), by("full").note.as_deref()),
            (
                "skipped",
                Some("Paused near the AI subscription's usage limit; runs again after it resets")
            )
        );
    }

    #[test]
    fn stopping_a_running_entry_kills_its_process_group_and_the_worker_records_it_cancelled() {
        use std::os::unix::process::CommandExt;
        let (_d, q) = queue();
        q.enqueue("a", Reason::Scheduled, "t0").unwrap();
        let mut spawned = false;
        let q2 = q.clone();
        let stopper = std::thread::spawn(move || {
            // Wait for the worker to record the pid, then press Stop.
            loop {
                let snap = q2.snapshot(0);
                if let Some(e) = snap.running.first().filter(|e| e.pid.is_some()) {
                    return stop_entry(&q2, &e.id, "t-stop").unwrap();
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        });
        let ran = drain(&q, &|| "now".to_string(), 1, &mut |_| {
            spawned = true;
            std::process::Command::new("sh")
                .args(["-c", "sleep 30"])
                .process_group(0)
                .spawn()
        });
        assert!(spawned);
        assert_eq!(ran, 1);
        assert!(matches!(
            stopper.join().unwrap(),
            CancelOutcome::Running(Some(_))
        ));
        let snap = q.snapshot(5);
        assert_eq!(snap.recent[0].state, "cancelled");
        assert!(snap.running.is_empty());
    }

    #[test]
    fn concurrent_enqueues_never_lose_an_entry() {
        let (_d, q) = queue();
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let q = q.clone();
                std::thread::spawn(move || {
                    q.enqueue(&format!("l{i}"), Reason::Scheduled, "t").unwrap()
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(q.snapshot(0).queued.len(), 8);
    }
}
