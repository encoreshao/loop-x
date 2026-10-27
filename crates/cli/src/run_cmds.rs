//! `loopx run`, `loopx scheduler`, `loopx topic run` and the internal
//! `loopx _entry` (replace `run-loop-now.sh`, `bin/loop_scheduler.py` and the
//! per-loop runner scripts).

use chrono::Local;
use loopx_core::run_loop::{self, RunLoopOpts};
use loopx_core::{ledger, loop_scheduler, loops_config, run_queue};
use std::process::{Command, ExitCode};

fn code(c: i32) -> ExitCode {
    ExitCode::from(c.clamp(0, 255) as u8)
}

/// `loopx run <loop_name> [extra_args...]`: one registered loop's run, now.
pub fn run(args: &[String]) -> ExitCode {
    let Some(name) = args.first() else {
        eprintln!("Usage: loopx run <loop_name> [extra_args...]");
        return ExitCode::from(2);
    };
    code(run_loop::run_loop_now(
        &RunLoopOpts::real(),
        name,
        &args[1..],
    ))
}

/// `loopx topic run`: the topic monitor loop, now (alias of `loopx run topic-loop`).
pub fn topic_run(_args: &[String]) -> ExitCode {
    code(run_loop::run_loop_now(
        &RunLoopOpts::real(),
        "topic-loop",
        &[],
    ))
}

/// `loopx _entry <entry_point> <run_id> [extra...]`: the process `loopx run`
/// supervises; not meant to be typed by hand.
pub fn entry(args: &[String]) -> ExitCode {
    let Some((entry_point, rest)) = args.split_first() else {
        eprintln!("Usage: loopx _entry <entry_point> <run_id> [extra_args...]");
        return ExitCode::from(2);
    };
    code(run_loop::run_entry(entry_point, rest))
}

/// `loopx scheduler`: the tick, run every minute. Queues every loop that is due
/// and not already queued or running, then makes sure a worker is draining the
/// queue; it never waits for a loop itself.
pub fn scheduler(_args: &[String]) -> ExitCode {
    // One ticker at a time: the LoopX app and a launchd `loopx scheduler` may both
    // be on, and both would otherwise queue the same due loop.
    let Some(_poll) = loop_scheduler::try_lock_poll(&loop_scheduler::default_lock_path()) else {
        println!("loop_scheduler: another scheduler is already polling; skipping");
        return ExitCode::SUCCESS;
    };
    // Best-effort, idempotent: catches runs that predate loop.result events.
    ledger::run_startup_backfill(None, None);
    // Throttled to every few hours; never fails the tick.
    loopx_core::auth_health::run_if_due(None, None, None, None);
    let loops = match loops_config::list_loops(&loops_config::default_config_path(), None) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("loop_scheduler: could not load the loops registry: {e}");
            return ExitCode::SUCCESS;
        }
    };
    let queue = run_queue::RunQueue::new(run_queue::default_queue_path());
    let queued = loop_scheduler::enqueue_due_loops(
        &loops,
        &loop_scheduler::default_state_path(),
        &queue,
        Local::now().naive_local(),
    );
    if !queued.is_empty() {
        println!("loop_scheduler: queued {}", queued.join(", "));
    }
    if !queue.snapshot(0).queued.is_empty() {
        start_worker_if_idle();
    }
    ExitCode::SUCCESS
}

/// Starts a detached `loopx queue-worker` unless one already holds the worker
/// lock. Two ticks racing here is fine: the second worker finds the lock taken
/// and exits at once.
fn start_worker_if_idle() {
    use std::os::unix::process::CommandExt;
    let lock = run_queue::default_worker_lock_path();
    match loop_scheduler::try_lock_poll(&lock) {
        Some(guard) => drop(guard),
        None => return,
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("queue-worker")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    if let Err(e) = cmd.spawn() {
        eprintln!("loop_scheduler: could not start the queue worker: {e}");
    }
}

/// `loopx queue-worker`: drains the run queue, one loop after another, then exits.
/// Only one worker runs at a time (the worker lock).
pub fn queue_worker(_args: &[String]) -> ExitCode {
    let Some(_lock) = loop_scheduler::try_lock_poll(&run_queue::default_worker_lock_path()) else {
        return ExitCode::SUCCESS;
    };
    let queue = run_queue::RunQueue::new(run_queue::default_queue_path());
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "loopx".to_string());
    let now = || loop_scheduler::isoformat(Local::now().naive_local());
    let ran = run_queue::drain(&queue, &now, 1, &mut |entry| {
        spawn_run(&exe, &entry.loop_name)
    });
    if ran > 0 {
        println!("queue-worker: ran {ran} loop(s)");
    }
    ExitCode::SUCCESS
}

/// A child `loopx run <name>` in its own process group (so the dashboard's
/// Stop button can `killpg` it without hitting the worker).
fn spawn_run(exe: &str, name: &str) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    Command::new(exe)
        .args(["run", name])
        .process_group(0)
        .spawn()
}
