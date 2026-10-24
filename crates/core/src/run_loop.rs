//! One registered loop's run, right now. Ports `run-loop-now.sh`: resolve the
//! registry entry, give the run an identity, log to the per-run dated log, record
//! `running`/`idle`/`failed` status, emit `run.started`/`run.failed`, and run the
//! loop's entry point (`loopx _entry ...`) under its timeout in a login shell so
//! the user's PATH and `claude` auth are available.

use crate::config::default_config_path as projects_config_path;
use crate::events::EventFields;
use crate::gitlab_runner::{self, RunCtx};
use crate::runner_util::{
    append_unified_log, emit_best_effort, notify_slack_best_effort, repo_root, unified_log_path,
};
use crate::{ai_cli_config, loop_scheduler, loops_config, status, topic_runner};
use chrono::{Local, Utc};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Exit code of a timed-out run, like GNU `timeout`.
pub const TIMEOUT_EXIT_CODE: i32 = 124;

#[derive(Debug, Clone)]
pub struct RunLoopOpts {
    pub root: PathBuf,
    /// The binary re-invoked as `<exe> _entry <entry_point> <run_id> ...`.
    pub exe: String,
    /// Wrap the entry in `zsh -i -l -c` (the real thing); tests turn it off.
    pub login_shell: bool,
}

impl RunLoopOpts {
    pub fn real() -> Self {
        Self {
            root: repo_root(),
            exe: std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "loopx".to_string()),
            // `LOOPX_NO_LOGIN_SHELL=1` runs the entry directly (tests, CI).
            login_shell: std::env::var_os("LOOPX_NO_LOGIN_SHELL").is_none()
                && (Path::new("/bin/zsh").exists() || Path::new("/usr/bin/zsh").exists()),
        }
    }
}

fn stamp() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// The registry fields a run needs.
struct Entry {
    entry_point: String,
    timeout_seconds: f64,
    log_suffix: String,
    emit_run_events: bool,
}

fn resolve_entry(loop_name: &str) -> Result<Entry, String> {
    let entry = loops_config::get_loop(loop_name, &loops_config::default_config_path(), None)
        .map_err(|e| e.to_string())?;
    let missing = |k: &str| format!("Loop '{loop_name}' is missing required registry field '{k}'");
    let entry_point = entry
        .get("entry_point")
        .ok_or_else(|| missing("entry_point"))
        .map(loops_config::py_str)?;
    let timeout_raw = entry
        .get("timeout_seconds")
        .ok_or_else(|| missing("timeout_seconds"))
        .map(loops_config::py_str)?;
    let timeout_seconds: f64 = timeout_raw.trim().parse().map_err(|_| {
        format!("Loop '{loop_name}' has a non-numeric timeout_seconds: {timeout_raw}")
    })?;
    Ok(Entry {
        entry_point,
        timeout_seconds,
        log_suffix: entry
            .get("log_suffix")
            .map_or(String::new(), loops_config::py_str),
        emit_run_events: entry
            .get("emit_run_events")
            .is_some_and(loops_config::py_truthy),
    })
}

fn fresh_run_id() -> String {
    format!("run_{}", Utc::now().format("%Y%m%d_%H%M%S"))
}

/// Run `loop_name` to completion; the return value is the process exit code.
pub fn run_loop_now(opts: &RunLoopOpts, loop_name: &str, extra_args: &[String]) -> i32 {
    let unified = unified_log_path(&opts.root);
    let entry = match resolve_entry(loop_name) {
        Ok(e) => e,
        Err(msg) => {
            // Before anything else that could fail, so a bad registry entry is
            // never a silent exit with nothing logged and no alert.
            append_unified_log(
                &format!(
                    "[{}] ---- {loop_name} ---- FAILED to resolve its loop registry entry: {msg} ----",
                    stamp()
                ),
                &unified,
            );
            notify_slack_best_effort(
                &format!("*{loop_name} loop FAILED* - could not resolve its registry entry: {msg}"),
                None,
                None,
            );
            return 1;
        }
    };

    let run_id = fresh_run_id();
    std::env::set_var("LOOP_RUN_ID", &run_id);
    let history = opts.root.join("outputs").join("history");
    let _ = std::fs::create_dir_all(&history);
    let _ = std::fs::create_dir_all(opts.root.join("outputs").join("topic-monitor"));
    let date_stamp = Local::now().format("%Y-%m-%d").to_string();
    let log_name = format!("{date_stamp}{}.log", entry.log_suffix);
    let log_path = history.join(&log_name);
    append_unified_log(
        &format!("[{}] ---- {loop_name} ---- run started ----", stamp()),
        &unified,
    );

    let status_path = loop_scheduler::status_path_for_loop(loop_name, Some(&opts.root));
    let events_dir = None::<&Path>;
    let fail = |exit: i32| {
        append_unified_log(
            &format!(
                "[{}] ---- {loop_name} ---- run FAILED (exit {exit}) ----",
                stamp()
            ),
            &unified,
        );
        notify_slack_best_effort(
            &format!(
                "*{loop_name} loop FAILED* (exit {exit}) \u{2014} see outputs/history/{log_name}"
            ),
            None,
            None,
        );
        let _ = status::write_status(&status_path, "failed", &[("last_exit_code", json!(exit))]);
        if entry.emit_run_events {
            emit_best_effort(
                "run.failed",
                Some(&run_id),
                EventFields {
                    data: Some(json!({"exit_code": exit})),
                    ..Default::default()
                },
                events_dir,
            );
        }
        exit
    };

    if entry.emit_run_events {
        // "dashboard" when the extras carry a scoped single-issue run (the chat
        // tool's alias+issue_iid pair); otherwise a scheduled run.
        let trigger = if extra_args.len() == 2 {
            "dashboard"
        } else {
            "scheduled"
        };
        emit_best_effort(
            "run.started",
            Some(&run_id),
            EventFields {
                data: Some(json!({"trigger": trigger})),
                ..Default::default()
            },
            events_dir,
        );
    }

    // Our pid is also the run's process-group id (every caller launches us in a
    // new session), so the dashboard's Stop button can killpg() the whole tree
    // below us: this process, the login shell, the runner and the CLI.
    if let Err(e) = status::write_status(
        &status_path,
        "running",
        &[("pid", json!(std::process::id()))],
    ) {
        eprintln!("loopx: could not write status: {e}");
        return fail(1);
    }

    let log_file = match OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("loopx: cannot open {}: {e}", log_path.display());
            return fail(1);
        }
    };
    let mut args = vec![
        "_entry".to_string(),
        entry.entry_point.clone(),
        run_id.clone(),
    ];
    args.extend(extra_args.iter().cloned());
    let mut command = if opts.login_shell {
        let mut c = Command::new("zsh");
        c.args(["-i", "-l", "-c", "exec \"$@\"", "zsh", &opts.exe])
            .args(&args);
        c
    } else {
        let mut c = Command::new(&opts.exe);
        c.args(&args);
        c
    };
    command.current_dir(&opts.root);
    command.stdin(Stdio::null());
    match (log_file.try_clone(), log_file.try_clone()) {
        (Ok(out), Ok(err)) => {
            command.stdout(out).stderr(err);
        }
        _ => return fail(1),
    }
    let exit = match run_with_timeout(&mut command, entry.timeout_seconds) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("loopx: could not start the loop: {e}");
            1
        }
    };
    if exit != 0 {
        return fail(exit);
    }
    let _ = status::write_status(&status_path, "idle", &[("last_exit_code", json!(0))]);
    append_unified_log(
        &format!(
            "[{}] ---- {loop_name} ---- run finished (exit 0) ----",
            stamp()
        ),
        &unified,
    );
    0
}

/// Wait for `command`, killing its whole process group after `timeout_seconds`
/// (exit 124). The child stays in our process group so a dashboard Stop
/// (`killpg` of our pid) reaches it too.
fn run_with_timeout(command: &mut Command, timeout_seconds: f64) -> std::io::Result<i32> {
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_seconds.max(0.0));
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(exit_code(status));
        }
        if Instant::now() >= deadline {
            terminate_tree(&mut child);
            return Ok(TIMEOUT_EXIT_CODE);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

#[cfg(not(unix))]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

#[cfg(unix)]
fn terminate_tree(child: &mut std::process::Child) {
    // SIGTERM the whole group (the child, the CLI it started, ...). We are in
    // that group too, so ignore the signal ourselves first.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::killpg(libc::getpgrp(), libc::SIGTERM);
    }
    let grace = Instant::now() + Duration::from_secs(10);
    while Instant::now() < grace {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
fn terminate_tree(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

// ---- entry dispatch ---------------------------------------------------------

/// Registry entry points that run in this binary.
pub fn run_entry(entry_point: &str, argv: &[String]) -> i32 {
    let root = repo_root();
    crate::bundled::seed(&root);
    match entry_point {
        "bin.gitlab_loop_runner" | "gitlab-loop" => {
            let ctx = RunCtx::new(root);
            gitlab_runner::main(&ctx.clone(), argv, gitlab_runner::cli_invoker(ctx))
        }
        "bin.topic_monitor_runner" | "topic-loop" => {
            let ctx = topic_runner::TopicCtx::new(root);
            topic_runner::main(&ctx.clone(), argv, topic_runner::cli_invoker(ctx))
        }
        "bin.inbox_triage_runner" | "inbox-triage-loop" => {
            crate::inbox_runner::main(&crate::inbox_runner::InboxCtx::new(root), argv, None)
        }
        other => match crate::loop_plugins::run_entry(other, argv) {
            Some(code) => code,
            None => {
                eprintln!("loopx: entry point '{other}' has no runner in this binary");
                1
            }
        },
    }
}

/// Helper used by `loopx doctor`-style checks: which AI CLI the runs will use.
pub fn selected_cli_summary() -> Value {
    json!({
        "ai_cli": ai_cli_config::selected_cli(),
        "projects_config": projects_config_path().display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    // The run mutates process-wide env (LOOP_RUN_ID, LOOP_ENGINEERING_HOME).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn fake_exe(dir: &Path, body: &str) -> String {
        let p = dir.join("fake-loopx");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    fn setup(dir: &Path, emit: bool, timeout: i64) {
        std::fs::write(
            dir.join("loops.json"),
            json!([{
                "name": "gitlab-loop", "entry_point": "bin.gitlab_loop_runner",
                "timeout_seconds": timeout, "log_suffix": "-x", "emit_run_events": emit,
                "enabled": true, "schedule": {"frequency": "daily", "hour": 1, "minute": 0}
            }])
            .to_string(),
        )
        .unwrap();
    }

    fn with_env<R>(dir: &Path, f: impl FnOnce() -> R) -> R {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("LOOP_ENGINEERING_HOME", dir);
        std::env::set_var("LOOP_EVENTS_DIR", dir.join("events"));
        std::env::set_var("HOME", dir);
        f()
    }

    fn opts(dir: &Path, exe: String) -> RunLoopOpts {
        RunLoopOpts {
            root: dir.to_path_buf(),
            exe,
            login_shell: false,
        }
    }

    #[test]
    fn successful_run_records_status_logs_and_events() {
        let d = tempfile::tempdir().unwrap();
        setup(d.path(), true, 30);
        let exe = fake_exe(d.path(), "echo \"args: $@\"; echo oops >&2");
        let code = with_env(d.path(), || {
            run_loop_now(
                &opts(d.path(), exe),
                "gitlab-loop",
                &["web".into(), "7".into()],
            )
        });
        assert_eq!(code, 0);
        let st = status::read_status(&d.path().join("outputs/status.json"));
        assert_eq!(st["state"], "idle");
        assert_eq!(st["last_exit_code"], 0);
        let logs: Vec<_> = std::fs::read_dir(d.path().join("outputs/history"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(logs.len(), 1);
        assert!(logs[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-x.log"));
        let log = std::fs::read_to_string(&logs[0]).unwrap();
        assert!(
            log.contains("args: _entry bin.gitlab_loop_runner run_"),
            "{log}"
        );
        assert!(log.contains(" web 7"));
        assert!(log.contains("oops"), "stderr goes to the same log");
        let unified = std::fs::read_to_string(d.path().join("logs/loop-engineering.log")).unwrap();
        assert!(unified.contains("---- gitlab-loop ---- run started ----"));
        assert!(unified.contains("run finished (exit 0)"));
        let events = std::fs::read_to_string(
            std::fs::read_dir(d.path().join("events"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        assert!(events.contains("\"run.started\""));
        assert!(
            events.contains("\"trigger\": \"dashboard\""),
            "two extras = dashboard"
        );
        assert!(!events.contains("run.failed"));
    }

    #[test]
    fn failing_run_is_marked_failed_and_alerted() {
        let d = tempfile::tempdir().unwrap();
        setup(d.path(), true, 30);
        let exe = fake_exe(d.path(), "exit 3");
        let code = with_env(d.path(), || {
            run_loop_now(&opts(d.path(), exe), "gitlab-loop", &[])
        });
        assert_eq!(code, 3);
        let st = status::read_status(&d.path().join("outputs/status.json"));
        assert_eq!(st["state"], "failed");
        assert_eq!(st["last_exit_code"], 3);
        let unified = std::fs::read_to_string(d.path().join("logs/loop-engineering.log")).unwrap();
        assert!(unified.contains("run FAILED (exit 3)"));
        let events = std::fs::read_to_string(
            std::fs::read_dir(d.path().join("events"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        assert!(events.contains("\"run.failed\"") && events.contains("\"exit_code\": 3"));
        assert!(events.contains("\"trigger\": \"scheduled\""));
    }

    #[test]
    fn unknown_loop_fails_before_anything_else() {
        let d = tempfile::tempdir().unwrap();
        setup(d.path(), false, 30);
        let code = with_env(d.path(), || {
            run_loop_now(&opts(d.path(), "/bin/true".into()), "no-such-loop", &[])
        });
        assert_eq!(code, 1);
        let unified = std::fs::read_to_string(d.path().join("logs/loop-engineering.log")).unwrap();
        assert!(unified.contains("FAILED to resolve its loop registry entry"));
        assert!(!d.path().join("outputs/status.json").exists());
    }

    #[test]
    fn run_events_are_skipped_when_the_loop_does_not_emit_them() {
        let d = tempfile::tempdir().unwrap();
        setup(d.path(), false, 30);
        let exe = fake_exe(d.path(), "exit 0");
        with_env(d.path(), || {
            run_loop_now(&opts(d.path(), exe), "gitlab-loop", &[])
        });
        assert!(!d.path().join("events").exists());
    }
}
