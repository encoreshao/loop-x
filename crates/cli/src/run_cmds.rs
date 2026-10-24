//! `loopx run`, `loopx scheduler`, `loopx topic run` and the internal
//! `loopx _entry` (replace `run-loop-now.sh`, `bin/loop_scheduler.py` and the
//! per-loop runner scripts).

use chrono::Local;
use loopx_core::run_loop::{self, RunLoopOpts};
use loopx_core::{ledger, loop_scheduler, loops_config};
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

/// `loopx scheduler`: what launchd runs every 15 minutes. Starts every loop
/// that is due and not already running, one after another.
pub fn scheduler(_args: &[String]) -> ExitCode {
    // One poller at a time: the LoopX app and a launchd `loopx scheduler` may both
    // be on, and both would otherwise start the same due loop.
    let Some(_poll) = loop_scheduler::try_lock_poll(&loop_scheduler::default_lock_path()) else {
        println!("loop_scheduler: another scheduler is already polling; skipping");
        return ExitCode::SUCCESS;
    };
    // Best-effort, idempotent: catches runs that predate loop.result events.
    ledger::run_startup_backfill(None, None);
    // Throttled to every few hours; never fails the poll.
    loopx_core::auth_health::run_if_due(None, None, None, None);
    let loops = match loops_config::list_loops(&loops_config::default_config_path(), None) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("loop_scheduler: could not load the loops registry: {e}");
            return ExitCode::SUCCESS;
        }
    };
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "loopx".to_string());
    let is_running = |name: &str| {
        let path = loop_scheduler::status_path_for_loop(name, None);
        loop_scheduler::read_status_state(&path) == "running"
    };
    // `run_due_loops` hands over `["bash", <run-loop-now path>, <name>]`; the
    // name is all we need, since this binary is the runner now.
    let mut runner = |argv: &[String]| -> std::io::Result<()> {
        let name = argv.get(2).cloned().unwrap_or_default();
        spawn_run(&exe, &name)
    };
    let attempted = loop_scheduler::run_due_loops(
        &loops,
        &loop_scheduler::default_state_path(),
        std::path::Path::new("unused"),
        Local::now().naive_local(),
        &is_running,
        &mut runner,
    );
    if !attempted.is_empty() {
        println!("loop_scheduler: attempted {}", attempted.join(", "));
    }
    ExitCode::SUCCESS
}

/// A child `loopx run <name>` in its own process group (so the dashboard's
/// Stop button can `killpg` it without hitting the scheduler), awaited.
fn spawn_run(exe: &str, name: &str) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    Command::new(exe)
        .args(["run", name])
        .process_group(0)
        .status()
        .map(|_| ())
}
