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
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Scheduled => "scheduled",
            Reason::Manual => "manual",
            Reason::CatchUp => "catch-up",
        }
    }

    fn priority(self) -> i32 {
        match self {
            Reason::Manual => PRIORITY_MANUAL,
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

    fn read_file(&self) -> QueueFile {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Adds `loop_name` unless it is already queued or running.
    pub fn enqueue(&self, loop_name: &str, reason: Reason, now: &str) -> std::io::Result<Enqueued> {
        self.with_file(|f| {
            if let Some(e) = f.entries.iter().find(|e| {
                e.loop_name == loop_name && matches!(e.state.as_str(), "queued" | "running")
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
            });
        })
    }

    /// Starts the next waiting entry unless `max_running` are already running.
    pub fn claim_next(&self, now: &str, max_running: usize) -> std::io::Result<Option<Entry>> {
        self.with_file(|f| {
            if f.entries.iter().filter(|e| e.state == "running").count() >= max_running {
                return None;
            }
            let next = f
                .entries
                .iter()
                .filter(|e| e.state == "queued")
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
        self.with_file(|f| {
            let mut n = 0;
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
                    n += 1;
                }
            }
            n
        })
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

/// Drains the queue: starts each waiting entry in turn with `spawn`, waits for it
/// and records the outcome, until nothing is waiting. `spawn` returns the child
/// so its pid is recorded (the Stop button kills it). Returns how many ran.
pub fn drain(
    queue: &RunQueue,
    now: &dyn Fn() -> String,
    max_running: usize,
    spawn: &mut dyn FnMut(&Entry) -> std::io::Result<std::process::Child>,
) -> usize {
    let _ = queue.recover(&now(), &|pid| crate::run_control::process_alive(pid));
    let mut ran = 0;
    while let Ok(Some(entry)) = queue.claim_next(&now(), max_running) {
        ran += 1;
        match spawn(&entry) {
            Ok(mut child) => {
                let _ = queue.set_pid(&entry.id, i64::from(child.id()));
                let code = child.wait().ok().and_then(|s| s.code());
                let (state, note) = match code {
                    Some(0) => ("done", None),
                    Some(crate::run_loop::PRECHECK_SKIPPED_EXIT) => {
                        ("skipped", Some("Its precheck found nothing to do"))
                    }
                    _ => ("failed", None),
                };
                let _ = queue.finish(&entry.id, state, code, note, &now());
            }
            Err(e) => {
                let _ = queue.finish(&entry.id, "failed", None, Some(&e.to_string()), &now());
            }
        }
    }
    ran
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue() -> (tempfile::TempDir, RunQueue) {
        let d = tempfile::tempdir().unwrap();
        let q = RunQueue::new(d.path().join("run_queue.json"));
        (d, q)
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
                _ => std::process::Command::new("/no/such/program").spawn(),
            }
        });
        assert_eq!(ran, 4);
        assert_eq!(order, ["ok", "bad", "missing", "idle"]);
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
