//! Runs a batch's tasks (GitLab issues, topics) side by side, with three rules:
//!
//! - at most `max_parallel` run at once (1 = the old one-by-one behavior);
//! - tasks that share a `group` run one at a time (a project worked on in its
//!   own checkout, where two agents would edit the same files);
//! - each task holds its own cross-process lock while it runs, and a task whose
//!   lock is already held (by another run, process or thread) is skipped, so the
//!   exact same task never runs twice at once.

use crate::file_lock::FileLock;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};

/// One task to schedule.
pub struct Task<T> {
    pub item: T,
    /// Held for as long as the task runs; already held means skip it.
    pub lock_path: PathBuf,
    /// Tasks with the same group never overlap.
    pub group: Option<String>,
}

/// What became of one task, in input order.
#[derive(Debug)]
pub enum Outcome<R> {
    Ran(R),
    /// Another run holds the task's lock.
    AlreadyRunning,
    /// The lock could not be taken (I/O error); the task did not run.
    LockFailed(String),
}

struct Queue {
    waiting: VecDeque<usize>,
    busy_groups: HashSet<String>,
}

/// Runs every task, at most `max_parallel` at once (at least 1), and returns
/// each one's outcome in input order.
pub fn run_tasks<T: Sync, R: Send>(
    tasks: &[Task<T>],
    max_parallel: usize,
    run: impl Fn(&T) -> R + Sync,
) -> Vec<Outcome<R>> {
    let workers = max_parallel.max(1).min(tasks.len().max(1));
    let queue = Mutex::new(Queue {
        waiting: (0..tasks.len()).collect(),
        busy_groups: HashSet::new(),
    });
    let wake = Condvar::new();
    let results: Mutex<Vec<Option<Outcome<R>>>> =
        Mutex::new((0..tasks.len()).map(|_| None).collect());

    // The next task whose group is free, or `None` once nothing is left.
    let claim = || -> Option<usize> {
        let mut q = queue.lock().unwrap();
        loop {
            if q.waiting.is_empty() {
                return None;
            }
            let pos = q.waiting.iter().position(|&i| {
                tasks[i]
                    .group
                    .as_ref()
                    .is_none_or(|g| !q.busy_groups.contains(g))
            });
            if let Some(pos) = pos {
                let i = q.waiting.remove(pos).expect("position is in range");
                if let Some(g) = &tasks[i].group {
                    q.busy_groups.insert(g.clone());
                }
                return Some(i);
            }
            // Everything left waits on a busy group: sleep until one frees.
            q = wake.wait(q).unwrap();
        }
    };
    let release = |i: usize| {
        if let Some(g) = &tasks[i].group {
            queue.lock().unwrap().busy_groups.remove(g);
        }
        wake.notify_all();
    };

    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                while let Some(i) = claim() {
                    let task = &tasks[i];
                    let outcome = match FileLock::try_exclusive(&task.lock_path) {
                        Ok(Some(_held)) => Outcome::Ran(run(&task.item)),
                        Ok(None) => Outcome::AlreadyRunning,
                        Err(e) => Outcome::LockFailed(e.to_string()),
                    };
                    results.lock().unwrap()[i] = Some(outcome);
                    release(i);
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|r| r.expect("every task was claimed"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn tasks(dir: &std::path::Path, groups: &[Option<&str>]) -> Vec<Task<usize>> {
        groups
            .iter()
            .enumerate()
            .map(|(i, g)| Task {
                item: i,
                lock_path: dir.join(format!("t{i}.lock")),
                group: g.map(str::to_string),
            })
            .collect()
    }

    /// Runs `tasks`, each sleeping a little, and reports the peak number that
    /// ran at once overall and the peak within group "g".
    fn peaks(tasks: &[Task<usize>], max: usize) -> (usize, usize, Vec<Outcome<usize>>) {
        let (now, peak, now_g, peak_g) = (
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        );
        let out = run_tasks(tasks, max, |&i| {
            let in_g = tasks[i].group.as_deref() == Some("g");
            peak.fetch_max(now.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            if in_g {
                peak_g.fetch_max(now_g.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            }
            std::thread::sleep(Duration::from_millis(40));
            if in_g {
                now_g.fetch_sub(1, Ordering::SeqCst);
            }
            now.fetch_sub(1, Ordering::SeqCst);
            i * 10
        });
        (peak.into_inner(), peak_g.into_inner(), out)
    }

    #[test]
    fn runs_up_to_the_limit_at_once_and_returns_results_in_order() {
        let d = tempfile::tempdir().unwrap();
        let t = tasks(d.path(), &[None; 7]);
        let (peak, _, out) = peaks(&t, 3);
        assert_eq!(peak, 3);
        let values: Vec<usize> = out
            .into_iter()
            .map(|o| match o {
                Outcome::Ran(v) => v,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(values, [0, 10, 20, 30, 40, 50, 60]);
    }

    #[test]
    fn a_limit_of_one_runs_them_one_by_one() {
        let d = tempfile::tempdir().unwrap();
        let (peak, _, _) = peaks(&tasks(d.path(), &[None; 4]), 1);
        assert_eq!(peak, 1);
        let (peak, _, _) = peaks(&tasks(d.path(), &[None; 4]), 0);
        assert_eq!(peak, 1);
    }

    #[test]
    fn tasks_of_one_group_never_overlap_but_others_still_run_alongside() {
        let d = tempfile::tempdir().unwrap();
        let t = tasks(
            d.path(),
            &[Some("g"), Some("g"), Some("g"), None, None, Some("h")],
        );
        let (peak, peak_g, out) = peaks(&t, 5);
        assert_eq!(peak_g, 1);
        assert!(peak >= 3, "other tasks ran alongside the group: {peak}");
        assert!(out.iter().all(|o| matches!(o, Outcome::Ran(_))));
    }

    #[test]
    fn a_task_whose_lock_is_held_elsewhere_is_skipped_not_run_twice() {
        let d = tempfile::tempdir().unwrap();
        let t = tasks(d.path(), &[None, None, None]);
        let _other_run = FileLock::try_exclusive(&t[1].lock_path).unwrap().unwrap();
        let ran = AtomicUsize::new(0);
        let out = run_tasks(&t, 3, |_| ran.fetch_add(1, Ordering::SeqCst));
        assert_eq!(ran.into_inner(), 2);
        assert!(matches!(out[1], Outcome::AlreadyRunning));
        assert!(matches!(out[0], Outcome::Ran(_)) && matches!(out[2], Outcome::Ran(_)));
    }

    #[test]
    fn the_same_task_listed_twice_in_one_batch_runs_once_at_a_time() {
        let d = tempfile::tempdir().unwrap();
        let mut t = tasks(d.path(), &[None, None]);
        t[1].lock_path = t[0].lock_path.clone();
        let out = run_tasks(&t, 2, |_| std::thread::sleep(Duration::from_millis(80)));
        let ran = out.iter().filter(|o| matches!(o, Outcome::Ran(_))).count();
        let skipped = out
            .iter()
            .filter(|o| matches!(o, Outcome::AlreadyRunning))
            .count();
        assert_eq!((ran, skipped), (1, 1));
    }

    #[test]
    fn the_lock_is_held_while_the_task_runs_and_freed_after() {
        let d = tempfile::tempdir().unwrap();
        let t = tasks(d.path(), &[None]);
        let path = t[0].lock_path.clone();
        run_tasks(&t, 1, |_| assert!(crate::file_lock::is_held(&path)));
        assert!(!crate::file_lock::is_held(&path));
    }

    #[test]
    fn no_tasks_is_fine() {
        let out: Vec<Outcome<()>> = run_tasks::<(), ()>(&[], 5, |_| ());
        assert!(out.is_empty());
    }
}
