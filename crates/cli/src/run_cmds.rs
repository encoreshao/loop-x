//! `loopx run`, `loopx scheduler`, `loopx topic run` and the internal
//! `loopx _entry` (replace `run-loop-now.sh`, `bin/loop_scheduler.py` and the
//! per-loop runner scripts).

use chrono::Local;
use loopx_core::run_loop::{self, RunLoopOpts};
use loopx_core::{ledger, loop_scheduler, loops_config, quota, run_queue, scheduler_health};
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
pub fn scheduler(args: &[String]) -> ExitCode {
    match args {
        [] => {}
        [cmd, rest @ ..] if cmd == "status" => return scheduler_status(rest),
        _ => {
            eprintln!("Usage: loopx scheduler [status [--json]]");
            return ExitCode::from(2);
        }
    }
    // One ticker at a time: the LoopX app and a launchd `loopx scheduler` may both
    // be on, and both would otherwise queue the same due loop.
    let Some(_poll) = loop_scheduler::try_lock_poll(&loop_scheduler::default_lock_path()) else {
        println!("loop_scheduler: another scheduler is already polling; skipping");
        return ExitCode::SUCCESS;
    };
    // Best-effort, idempotent: catches runs that predate loop.result events.
    ledger::run_startup_backfill(None, None);
    // Throttled to every few hours; never fails the tick.
    loopx_core::auth_health::register_sources(loopx_core::auth_health::config_sources(
        loopx_core::connectors_config::Paths::default(),
    ));
    loopx_core::auth_health::run_if_due(None, None, None, None);
    let loops = match loops_config::list_loops(&loops_config::default_config_path(), None) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("loop_scheduler: could not load the loops registry: {e}");
            return ExitCode::SUCCESS;
        }
    };
    let queue = run_queue::RunQueue::new(run_queue::default_queue_path());
    // Near the AI subscription's usage limit: due loops stay due (and run once
    // it resets) instead of being queued now. Manual runs already queued still run.
    let cli = loopx_core::ai_cli_config::selected_cli();
    let pause = quota::active_pause(&quota::pause_path(), &cli, chrono::Utc::now());
    let queued = match pause {
        Some(p) => {
            println!(
                "loop_scheduler: {cli} paused until {} ({}); nothing queued",
                p.until.with_timezone(&Local).format("%H:%M"),
                p.reason
            );
            Vec::new()
        }
        None => loop_scheduler::enqueue_due_loops(
            &loops,
            &loop_scheduler::default_state_path(),
            &queue,
            Local::now().naive_local(),
        ),
    };
    if !queued.is_empty() {
        println!("loop_scheduler: queued {}", queued.join(", "));
    }
    let heartbeat = scheduler_health::heartbeat_path(&loopx_core::config::home_dir());
    if let Err(e) = scheduler_health::record_tick(&heartbeat, Local::now().naive_local(), &queued) {
        eprintln!("loop_scheduler: could not record the tick: {e}");
    }
    if !queue.snapshot(0).queued.is_empty() {
        start_worker_if_idle();
    }
    ExitCode::SUCCESS
}

/// `loopx scheduler status [--json]`: is the scheduler ticking, what is queued
/// and when each loop is next due. Exits 1 when it is not running correctly.
fn scheduler_status(args: &[String]) -> ExitCode {
    let json = match args {
        [] => false,
        [a] if a == "--json" => true,
        _ => {
            eprintln!("Usage: loopx scheduler status [--json]");
            return ExitCode::from(2);
        }
    };
    let report = scheduler_health::report(
        &loopx_core::config::home_dir(),
        Local::now().naive_local(),
        &loopx_core::run_control::process_alive,
    );
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        print!("{}", scheduler_health::render(&report));
    }
    if report["healthy"] == true {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
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
    let ran = run_queue::drain(&queue, &now, 1, &mut |entry| spawn_run(&exe, entry));
    if ran > 0 {
        println!("queue-worker: ran {ran} loop(s)");
    }
    ExitCode::SUCCESS
}

/// A child `loopx run <name>` in its own process group (so the dashboard's
/// Stop button can `killpg` it without hitting the worker). `LOOPX_RUN_REASON`
/// tells the run whether the scheduler started it (its precheck applies).
fn spawn_run(exe: &str, entry: &run_queue::Entry) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    Command::new(exe)
        .args(["run", &entry.loop_name])
        .env("LOOPX_RUN_REASON", &entry.reason)
        .process_group(0)
        .spawn()
}
