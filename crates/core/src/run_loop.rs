//! One registered loop's run, right now. Ports `run-loop-now.sh`: resolve the
//! registry entry, give the run an identity, log to the per-run dated log, record
//! `running`/`idle`/`failed` status, emit `run.started`/`run.failed`, and run the
//! loop's entry point (`loopx _entry ...`) under its timeout in a login shell so
//! the user's PATH and `claude` auth are available.

use crate::ai_cli_safety::{self, CallSite};
use crate::config::default_config_path as projects_config_path;
use crate::events::EventFields;
use crate::gitlab_runner::{self, RunCtx};
use crate::runner_util::{append_unified_log, emit_best_effort, repo_root, unified_log_path};
use crate::{
    ai_cli_config, loop_scheduler, loops_config, quota, slack_report, status, topic_runner,
};
use chrono::{Local, Utc};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Exit code of a timed-out run, like GNU `timeout`.
pub const TIMEOUT_EXIT_CODE: i32 = 124;
/// Exit code of a scheduled run its precheck found nothing to do for (`EX_TEMPFAIL`).
pub const PRECHECK_SKIPPED_EXIT: i32 = 75;
/// Exit code of a scheduled run held back near the AI subscription's usage limit.
pub const QUOTA_PAUSED_EXIT: i32 = 76;
/// How long a loop's `schedule.precheck` command may take.
const PRECHECK_TIMEOUT_SECONDS: f64 = 60.0;

#[derive(Debug, Clone)]
pub struct RunLoopOpts {
    pub root: PathBuf,
    /// The binary re-invoked as `<exe> _entry <entry_point> <run_id> ...`.
    pub exe: String,
    /// Wrap the entry in `zsh -i -l -c` (the real thing); tests turn it off.
    pub login_shell: bool,
    /// `caffeinate`, run as `-i -s -w <our pid>` so the Mac does not sleep mid-run
    /// even when the app (and its own keep-awake) is closed.
    pub caffeinate: Option<PathBuf>,
    /// Reads the Claude subscription's usage windows before a scheduled run,
    /// when the user set a pause threshold.
    pub usage_probe: Option<quota::UsageProbe>,
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
            caffeinate: Some(PathBuf::from("/usr/bin/caffeinate")).filter(|p| p.exists()),
            usage_probe: Some(quota::live_claude_usage),
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
    precheck: Option<String>,
    /// The loop's own AI CLI account folder (`ai_config_dir`), `~` expanded.
    ai_config_dir: Option<PathBuf>,
}

/// `~/x` under `$HOME`; anything else as is.
fn expand_home(dir: &str) -> PathBuf {
    match (dir.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(dir),
    }
}

/// The variable that points `cli` at another account's config folder.
fn account_var(cli: &str) -> Option<&'static str> {
    match cli {
        "claude" => Some("CLAUDE_CONFIG_DIR"),
        "codex" => Some("CODEX_HOME"),
        "grok" => Some("GROK_HOME"),
        _ => None,
    }
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
        precheck: entry
            .get("schedule")
            .and_then(|s| s.get("precheck"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(String::from),
        ai_config_dir: entry
            .get("ai_config_dir")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(expand_home),
    })
}

/// Which AI CLI call site a registry entry point runs.
fn call_site(entry_point: &str) -> CallSite {
    match entry_point {
        "bin.topic_monitor_runner" | "topic-loop" => CallSite::TopicMonitor,
        "bin.inbox_triage_runner" | "inbox-triage-loop" => CallSite::InboxTriage,
        other if crate::loop_plugins::module_for_entry(other).is_some() => CallSite::Plugin,
        _ => CallSite::GitlabLoop,
    }
}

fn fresh_run_id() -> String {
    format!("run_{}", Utc::now().format("%Y%m%d_%H%M%S"))
}

/// The one Slack message for a loop run that failed outright.
pub fn failed_report(loop_name: &str, status: &str, detail: &str) -> slack_report::Report {
    slack_report::alert(
        &format!("{loop_name} loop"),
        slack_report::State::Failed,
        status,
        detail,
    )
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
            slack_report::send(
                &failed_report(
                    loop_name,
                    "Could not start",
                    &format!("Its loop registry entry could not be resolved: {msg}"),
                ),
                None,
            );
            return 1;
        }
    };

    // Only the scheduler's own runs are gated; "Run now" always runs.
    let scheduled = matches!(
        std::env::var("LOOPX_RUN_REASON").as_deref(),
        Ok("scheduled" | "catch-up")
    );
    // A loop on its own account has its own limits: the shared pause and usage
    // check are about the default account.
    let own_account = entry.ai_config_dir.is_some();
    if scheduled && !own_account {
        if let Some(why) = quota_pause(opts) {
            append_unified_log(
                &format!("[{}] ---- {loop_name} ---- paused: {why} ----", stamp()),
                &unified,
            );
            // Due again once the pause lifts, instead of losing today's slot.
            loop_scheduler::forget_attempt(loop_name, &loop_scheduler::default_state_path());
            return QUOTA_PAUSED_EXIT;
        }
    }
    if let Some(command) = entry.precheck.as_deref().filter(|_| scheduled) {
        if let Some(why) = failed_precheck(opts, command) {
            append_unified_log(
                &format!("[{}] ---- {loop_name} ---- skipped: {why} ----", stamp()),
                &unified,
            );
            return PRECHECK_SKIPPED_EXIT;
        }
    }
    if let Some(caffeinate) = &opts.caffeinate {
        // Exits on its own when this process does.
        let _ = Command::new(caffeinate)
            .args(["-i", "-s", "-w", &std::process::id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

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

    // Gemini, Kimi, Cursor and Codex run without a tool allowlist: say so once,
    // in the log and on the run-start event.
    let warning = ai_cli_safety::warning(
        &ai_cli_config::selected_cli(),
        call_site(&entry.entry_point),
    );
    if let Some(w) = warning {
        append_unified_log(&format!("[{}] WARNING: {w}", stamp()), &unified);
    }

    let status_path = loop_scheduler::status_path_for_loop(loop_name, Some(&opts.root));
    let previous = status::read_status(&status_path);
    // Where this run's output starts in the day's (shared) log.
    let log_start = std::fs::metadata(&log_path).map_or(0, |m| m.len());
    let events_dir = None::<&Path>;
    let fail = |exit: i32| {
        let failures = repeated_failures(&previous, exit) + 1;
        let output = run_output(&log_path, log_start);
        if let Some(until) = (!own_account).then(|| limit_pause(&output)).flatten() {
            append_unified_log(
                &format!(
                    "[{}] ---- {loop_name} ---- run ended (exit {exit}): hit the AI CLI usage limit; scheduled runs paused until {until} ----",
                    stamp()
                ),
                &unified,
            );
            slack_report::send(
                &failed_report(
                    loop_name,
                    "Paused: AI CLI usage limit",
                    &format!("Scheduled runs resume after {until}. \"Run now\" still works."),
                ),
                None,
            );
            if scheduled {
                loop_scheduler::forget_attempt(loop_name, &loop_scheduler::default_state_path());
            }
        } else if failures > 1 {
            append_unified_log(
                &format!(
                    "[{}] ---- {loop_name} ---- run FAILED (exit {exit}): failed the same way {failures} runs in a row; Slack alert held back ----",
                    stamp()
                ),
                &unified,
            );
        } else {
            let (why, headline, detail) = if quota::sign_in_lapsed(&output) {
                let cli = ai_cli_config::selected_cli();
                (
                    ": the AI CLI is not signed in",
                    "Sign-in needed".to_string(),
                    format!(
                        "The AI CLI ({cli}) is not signed in or its sign-in expired. Open Terminal, run `{cli}` and sign in again; scheduled runs keep failing until then."
                    ),
                )
            } else {
                (
                    "",
                    format!("Failed (exit {exit})"),
                    format!("See outputs/history/{log_name} for the run's log."),
                )
            };
            append_unified_log(
                &format!(
                    "[{}] ---- {loop_name} ---- run FAILED (exit {exit}){why} ----",
                    stamp()
                ),
                &unified,
            );
            slack_report::send(&failed_report(loop_name, &headline, &detail), None);
        }
        let _ = status::write_status(
            &status_path,
            "failed",
            &[
                ("last_exit_code", json!(exit)),
                ("consecutive_failures", json!(failures)),
            ],
        );
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
                data: Some(match warning {
                    Some(w) => json!({"trigger": trigger, "warning": w}),
                    None => json!({"trigger": trigger}),
                }),
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
    // Set after the login shell's rc files, so a profile can't point the CLI
    // back at the default account.
    let cli = ai_cli_config::selected_cli();
    let account = match (&entry.ai_config_dir, account_var(&cli)) {
        (Some(dir), Some(var)) => vec![format!("{var}={}", dir.display())],
        (Some(_), None) => {
            append_unified_log(
                &format!("[{}] WARNING: {loop_name} has its own AI CLI account, but {cli} can't switch accounts; using its default one", stamp()),
                &unified,
            );
            Vec::new()
        }
        _ => Vec::new(),
    };
    let mut command = if opts.login_shell {
        let mut c = Command::new("zsh");
        c.args(["-i", "-l", "-c", "exec \"$@\"", "zsh", "/usr/bin/env"])
            .args(&account)
            .arg(&opts.exe)
            .args(&args);
        c
    } else {
        let mut c = Command::new("/usr/bin/env");
        c.args(&account).arg(&opts.exe).args(&args);
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

/// "14:30" (local) for a pause's end.
fn local_hm(t: chrono::DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%H:%M").to_string()
}

/// Why a scheduled run should not start: an active pause for the selected CLI,
/// or (Claude, opted in) a usage window above the threshold, which starts one.
/// An unreadable usage check never holds a run back.
fn quota_pause(opts: &RunLoopOpts) -> Option<String> {
    let cli = ai_cli_config::selected_cli();
    let path = quota::pause_path();
    let now = Utc::now();
    if let Some(p) = quota::active_pause(&path, &cli, now) {
        return Some(format!("{}, until {}", p.reason, local_hm(p.until)));
    }
    if cli != "claude" {
        return None;
    }
    let threshold = quota::pause_threshold(&ai_cli_config::load_config(
        &ai_cli_config::default_config_path(),
    ))?;
    let windows = match (opts.usage_probe?)() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("loopx: usage check skipped: {e}");
            return None;
        }
    };
    let pause = quota::pause_for(
        &cli,
        quota::over_threshold(&windows, threshold)?,
        threshold,
        now,
    );
    let _ = quota::write_pause(&path, &pause);
    Some(format!("{}, until {}", pause.reason, local_hm(pause.until)))
}

/// The last 64 KB this run wrote to the day's log, from `start`.
fn run_output(log_path: &Path, start: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 64 * 1024;
    let read = || -> std::io::Result<Vec<u8>> {
        let mut f = std::fs::File::open(log_path)?;
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(start.max(len.saturating_sub(TAIL))))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(buf)
    };
    read().map_or_else(
        |_| String::new(),
        |b| String::from_utf8_lossy(&b).into_owned(),
    )
}

/// When this run's output says the AI CLI hit its usage limit: pause scheduled
/// runs for [`quota::DEFAULT_PAUSE_MINUTES`] and return when they resume.
fn limit_pause(output: &str) -> Option<String> {
    if !quota::limit_hit(output) {
        return None;
    }
    let pause = quota::Pause {
        cli: ai_cli_config::selected_cli(),
        until: Utc::now() + chrono::Duration::minutes(quota::DEFAULT_PAUSE_MINUTES),
        reason: "the AI CLI reported its usage limit".into(),
    };
    let _ = quota::write_pause(&quota::pause_path(), &pause);
    Some(local_hm(pause.until))
}

/// How many runs in a row before this one failed with `exit`, from the status
/// the last run left: a repeat is counted rather than alerted again.
fn repeated_failures(previous: &Value, exit: i32) -> i64 {
    let same = previous.get("state").and_then(Value::as_str) == Some("failed")
        && previous.get("last_exit_code").and_then(Value::as_i64) == Some(i64::from(exit));
    if !same {
        return 0;
    }
    previous
        .get("consecutive_failures")
        .and_then(Value::as_i64)
        .unwrap_or(1)
}

/// Runs a loop's precheck in the checkout; `Some(reason)` when it says there is
/// nothing to do (any non-zero exit, a timeout, or a command that cannot start).
fn failed_precheck(opts: &RunLoopOpts, command: &str) -> Option<String> {
    let mut c = if opts.login_shell {
        let mut c = Command::new("zsh");
        c.args(["-l", "-c", command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    };
    let out_path = opts
        .root
        .join("outputs")
        .join(format!(".precheck-{}.out", std::process::id()));
    let started = (|| {
        let _ = std::fs::create_dir_all(opts.root.join("outputs"));
        let out = std::fs::File::create(&out_path)?;
        c.current_dir(&opts.root)
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out)
            // Its own group, so a timeout kills the precheck and not this run.
            .process_group(0)
            .spawn()
    })();
    let exit = match started {
        Ok(child) => wait_or_kill(child, PRECHECK_TIMEOUT_SECONDS),
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            return Some(format!("its precheck could not start: {e}"));
        }
    };
    let tail = std::fs::read_to_string(&out_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    let tail = tail.trim();
    let tail: String = tail
        .char_indices()
        .rev()
        .nth(399)
        .map_or(tail, |(i, _)| &tail[i..])
        .to_string();
    match exit {
        Some(0) => None,
        Some(code) if tail.is_empty() => Some(format!("its precheck exited {code}")),
        Some(code) => Some(format!("its precheck exited {code}: {tail}")),
        None => Some(format!(
            "its precheck took longer than {PRECHECK_TIMEOUT_SECONDS}s"
        )),
    }
}

/// The child's exit code, or `None` after killing its group at the deadline.
fn wait_or_kill(mut child: std::process::Child, timeout_seconds: f64) -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_seconds);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(exit_code(status)),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                unsafe {
                    libc::killpg(child.id() as i32, libc::SIGKILL);
                }
                let _ = child.wait();
                return None;
            }
        }
    }
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

    #[test]
    fn a_failed_run_is_a_failed_report() {
        let text =
            slack_report::plain_text(&failed_report("topic-loop", "Failed (exit 3)", "See log."));
        assert_eq!(
            text,
            "*topic-loop loop* :x: Failed (exit 3)\nSee log.\n\n_LoopX_"
        );
    }

    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    #[test]
    fn call_site_treats_the_topic_loop_alias_as_the_topic_monitor() {
        for ep in ["bin.topic_monitor_runner", "topic-loop"] {
            assert_eq!(call_site(ep), CallSite::TopicMonitor, "{ep}");
        }
        for ep in ["bin.gitlab_loop_runner", "gitlab-loop"] {
            assert_eq!(call_site(ep), CallSite::GitlabLoop, "{ep}");
        }
        for ep in ["bin.inbox_triage_runner", "inbox-triage-loop"] {
            assert_eq!(call_site(ep), CallSite::InboxTriage, "{ep}");
        }
        for ep in [
            "bin.loop_plugins.rss_watch",
            "meeting-prep-loop",
            "rss-watch-loop",
        ] {
            assert_eq!(call_site(ep), CallSite::Plugin, "{ep}");
        }
    }

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
            caffeinate: None,
            usage_probe: None,
        }
    }

    fn pause_file(dir: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(dir.join("quota_pause.json")).unwrap())
            .unwrap()
    }

    fn attempted(dir: &Path) -> bool {
        loop_scheduler::read_state(&dir.join("loop_scheduler_state.json"))
            .contains_key("gitlab-loop")
    }

    fn mark_attempted(dir: &Path) {
        std::fs::write(
            dir.join("loop_scheduler_state.json"),
            r#"{"gitlab-loop": {"last_attempted_date": "2026-10-31"}}"#,
        )
        .unwrap();
    }

    fn usage_93() -> Result<Vec<crate::quota::Window>, String> {
        Ok(vec![crate::quota::Window {
            label: "5-hour",
            used_percent: 93.0,
            resets_at: Some(Utc::now() + chrono::Duration::hours(2)),
        }])
    }

    fn usage_40() -> Result<Vec<crate::quota::Window>, String> {
        Ok(vec![crate::quota::Window {
            label: "5-hour",
            used_percent: 40.0,
            resets_at: None,
        }])
    }

    fn usage_unknown() -> Result<Vec<crate::quota::Window>, String> {
        Err("no Claude Code sign-in found".into())
    }

    fn use_account(dir: &Path, account: &str) {
        let mut loops: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("loops.json")).unwrap())
                .unwrap();
        loops[0]["ai_config_dir"] = json!(account);
        std::fs::write(dir.join("loops.json"), loops.to_string()).unwrap();
    }

    #[test]
    fn a_loop_on_its_own_account_runs_with_that_config_dir_and_skips_the_shared_pause() {
        let d = tempfile::tempdir().unwrap();
        setup(d.path(), false, 30);
        use_account(d.path(), "~/.claude-bot");
        std::fs::write(
            d.path().join("ai_cli.json"),
            r#"{"cli": "claude", "pause_above_percent": 90}"#,
        )
        .unwrap();
        let seen = d.path().join("seen");
        let mut o = opts(
            d.path(),
            fake_exe(
                d.path(),
                &format!(
                    "echo \"$CLAUDE_CONFIG_DIR|$CODEX_HOME\" > {}",
                    seen.display()
                ),
            ),
        );
        // The shared usage check is about the default account: not this loop's.
        o.usage_probe = Some(usage_93);
        let code = with_env(d.path(), || {
            with_reason("scheduled", || run_loop_now(&o, "gitlab-loop", &[]))
        });
        assert_eq!(code, 0);
        assert_eq!(
            std::fs::read_to_string(&seen).unwrap().trim(),
            format!("{}|", d.path().join(".claude-bot").display())
        );
        assert!(!d.path().join("quota_pause.json").exists());

        // Codex reads its account from CODEX_HOME.
        std::fs::write(d.path().join("ai_cli.json"), r#"{"cli": "codex"}"#).unwrap();
        use_account(d.path(), "/srv/codex-bot");
        let code = with_env(d.path(), || {
            with_reason("manual", || run_loop_now(&o, "gitlab-loop", &[]))
        });
        assert_eq!(code, 0);
        // Only CODEX_HOME is ours to set; CLAUDE_CONFIG_DIR is whatever we inherited.
        let seen = std::fs::read_to_string(&seen).unwrap();
        assert!(seen.trim().ends_with("|/srv/codex-bot"), "{seen}");
    }

    #[test]
    fn each_cli_with_an_account_folder_gets_its_own_variable() {
        assert_eq!(account_var("claude"), Some("CLAUDE_CONFIG_DIR"));
        assert_eq!(account_var("codex"), Some("CODEX_HOME"));
        assert_eq!(account_var("grok"), Some("GROK_HOME"));
        assert_eq!(account_var("gemini"), None);
    }

    #[test]
    fn a_limit_hit_on_a_loops_own_account_does_not_pause_the_others() {
        let d = tempfile::tempdir().unwrap();
        setup(d.path(), false, 30);
        use_account(d.path(), "/srv/claude-bot");
        std::fs::write(d.path().join("ai_cli.json"), r#"{"cli": "claude"}"#).unwrap();
        let o = opts(
            d.path(),
            fake_exe(d.path(), "echo 'Claude AI usage limit reached'; exit 1"),
        );
        let code = with_env(d.path(), || {
            with_reason("scheduled", || run_loop_now(&o, "gitlab-loop", &[]))
        });
        assert_eq!(code, 1);
        assert!(!d.path().join("quota_pause.json").exists());
    }

    #[test]
    fn a_scheduled_run_over_the_usage_threshold_pauses_and_waits_for_the_reset() {
        let d = tempfile::tempdir().unwrap();
        setup(d.path(), true, 30);
        std::fs::write(
            d.path().join("ai_cli.json"),
            r#"{"cli": "claude", "pause_above_percent": 90}"#,
        )
        .unwrap();
        mark_attempted(d.path());
        let marker = d.path().join("ran");
        let mut o = opts(
            d.path(),
            fake_exe(d.path(), &format!("touch {}", marker.display())),
        );
        o.usage_probe = Some(usage_93);
        let code = with_env(d.path(), || {
            with_reason("scheduled", || run_loop_now(&o, "gitlab-loop", &[]))
        });
        assert_eq!(code, QUOTA_PAUSED_EXIT);
        assert!(!marker.exists(), "the agent never started");
        let pause = pause_file(d.path());
        assert_eq!(pause["cli"], "claude");
        assert_eq!(pause["reason"], "5-hour usage at 93% (pausing at 90%)");
        assert!(
            !attempted(d.path()),
            "the slot runs again once the limit resets"
        );
        let unified = std::fs::read_to_string(d.path().join("logs/loop-engineering.log")).unwrap();
        assert!(
            unified.contains("paused: 5-hour usage at 93% (pausing at 90%), until"),
            "{unified}"
        );

        // The next scheduled run skips on the pause without asking again.
        o.usage_probe = Some(usage_40);
        let code = with_env(d.path(), || {
            with_reason("catch-up", || run_loop_now(&o, "gitlab-loop", &[]))
        });
        assert_eq!(code, QUOTA_PAUSED_EXIT);
        // "Run now" always runs.
        let code = with_env(d.path(), || {
            with_reason("manual", || run_loop_now(&o, "gitlab-loop", &[]))
        });
        assert_eq!(code, 0);
        assert!(marker.exists());
    }

    #[test]
    fn usage_under_the_threshold_unknown_or_switched_off_runs() {
        let d = tempfile::tempdir().unwrap();
        setup(d.path(), false, 30);
        for (config, probe) in [
            (
                r#"{"cli": "claude", "pause_above_percent": 90}"#,
                usage_40 as quota::UsageProbe,
            ),
            (
                r#"{"cli": "claude", "pause_above_percent": 90}"#,
                usage_unknown,
            ),
            (r#"{"cli": "claude"}"#, usage_93),
            (r#"{"cli": "codex", "pause_above_percent": 90}"#, usage_93),
        ] {
            std::fs::write(d.path().join("ai_cli.json"), config).unwrap();
            let mut o = opts(d.path(), fake_exe(d.path(), "exit 0"));
            o.usage_probe = Some(probe);
            let code = with_env(d.path(), || {
                with_reason("scheduled", || run_loop_now(&o, "gitlab-loop", &[]))
            });
            assert_eq!(code, 0, "{config}");
            assert!(!d.path().join("quota_pause.json").exists(), "{config}");
        }
    }

    #[test]
    fn a_run_whose_cli_sign_in_lapsed_says_to_sign_in_again() {
        let d = tempfile::tempdir().unwrap();
        setup(d.path(), false, 30);
        let exe = fake_exe(
            d.path(),
            "echo 'Invalid API key · Please run /login'; exit 1",
        );
        let code = with_env(d.path(), || {
            run_loop_now(&opts(d.path(), exe), "gitlab-loop", &[])
        });
        assert_eq!(code, 1);
        let unified = std::fs::read_to_string(d.path().join("logs/loop-engineering.log")).unwrap();
        assert!(
            unified.contains("run FAILED (exit 1): the AI CLI is not signed in"),
            "{unified}"
        );
        assert!(!d.path().join("quota_pause.json").exists());
    }

    #[test]
    fn a_run_that_hits_the_cli_usage_limit_pauses_scheduled_runs() {
        let d = tempfile::tempdir().unwrap();
        setup(d.path(), false, 30);
        std::fs::write(d.path().join("ai_cli.json"), r#"{"cli": "codex"}"#).unwrap();
        mark_attempted(d.path());
        let exe = fake_exe(
            d.path(),
            "echo \"ERROR: You've hit your usage limit. Try again later.\"; exit 1",
        );
        let code = with_env(d.path(), || {
            with_reason("scheduled", || {
                run_loop_now(&opts(d.path(), exe), "gitlab-loop", &[])
            })
        });
        assert_eq!(code, 1);
        let pause = pause_file(d.path());
        assert_eq!(pause["cli"], "codex");
        assert_eq!(pause["reason"], "the AI CLI reported its usage limit");
        assert!(!attempted(d.path()));
        let unified = std::fs::read_to_string(d.path().join("logs/loop-engineering.log")).unwrap();
        assert!(
            unified.contains("hit the AI CLI usage limit; scheduled runs paused until"),
            "{unified}"
        );
        assert!(!unified.contains("run FAILED"), "{unified}");
    }

    fn setup_with_schedule(dir: &Path, schedule: Value) {
        std::fs::write(
            dir.join("loops.json"),
            json!([{
                "name": "gitlab-loop", "entry_point": "bin.gitlab_loop_runner",
                "timeout_seconds": 30, "log_suffix": "-x", "emit_run_events": true,
                "enabled": true, "schedule": schedule
            }])
            .to_string(),
        )
        .unwrap();
    }

    fn with_reason<R>(reason: &str, f: impl FnOnce() -> R) -> R {
        std::env::set_var("LOOPX_RUN_REASON", reason);
        let out = f();
        std::env::remove_var("LOOPX_RUN_REASON");
        out
    }

    #[test]
    fn a_failing_precheck_skips_a_scheduled_run_without_starting_the_agent() {
        let d = tempfile::tempdir().unwrap();
        setup_with_schedule(
            d.path(),
            json!({"frequency": "daily", "hour": 1, "minute": 0,
                   "precheck": "echo nothing to do; exit 1"}),
        );
        let marker = d.path().join("ran");
        let exe = fake_exe(d.path(), &format!("touch {}", marker.display()));
        let code = with_env(d.path(), || {
            with_reason("scheduled", || {
                run_loop_now(&opts(d.path(), exe), "gitlab-loop", &[])
            })
        });
        assert_eq!(code, PRECHECK_SKIPPED_EXIT);
        assert!(!marker.exists(), "the agent never started");
        let unified = std::fs::read_to_string(d.path().join("logs/loop-engineering.log")).unwrap();
        assert!(
            unified.contains("skipped: its precheck exited 1: nothing to do"),
            "{unified}"
        );
        assert!(
            !d.path().join("events").exists(),
            "no run.started/run.failed"
        );
        assert!(
            !d.path().join("outputs/status.json").exists(),
            "the last real run's status is left alone"
        );
    }

    #[test]
    fn a_passing_precheck_or_a_manual_run_goes_ahead() {
        let d = tempfile::tempdir().unwrap();
        for (precheck, reason) in [("true", "catch-up"), ("exit 1", "manual")] {
            setup_with_schedule(
                d.path(),
                json!({"frequency": "daily", "hour": 1, "minute": 0, "precheck": precheck}),
            );
            let marker = d.path().join(format!("ran-{reason}"));
            let exe = fake_exe(d.path(), &format!("touch {}", marker.display()));
            let code = with_env(d.path(), || {
                with_reason(reason, || {
                    run_loop_now(&opts(d.path(), exe), "gitlab-loop", &[])
                })
            });
            assert_eq!(code, 0, "{reason}");
            assert!(marker.exists(), "{reason}");
        }
    }

    #[test]
    fn the_run_keeps_the_mac_awake_until_it_exits() {
        let d = tempfile::tempdir().unwrap();
        setup(d.path(), false, 30);
        let args_file = d.path().join("caffeinate-args");
        let caffeinate = d.path().join("caffeinate");
        std::fs::write(
            &caffeinate,
            format!("#!/bin/sh\necho \"$@\" > {}\n", args_file.display()),
        )
        .unwrap();
        std::fs::set_permissions(&caffeinate, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut o = opts(d.path(), fake_exe(d.path(), "sleep 0.3"));
        o.caffeinate = Some(caffeinate);
        assert_eq!(
            with_env(d.path(), || run_loop_now(&o, "gitlab-loop", &[])),
            0
        );
        let args = std::fs::read_to_string(&args_file).unwrap();
        assert_eq!(args.trim(), format!("-i -s -w {}", std::process::id()));
    }

    #[test]
    fn a_repeat_of_the_same_failure_is_counted_not_alerted_again() {
        let d = tempfile::tempdir().unwrap();
        setup(d.path(), false, 30);
        let status_path = d.path().join("outputs/status.json");
        let run = |body: &str| {
            let exe = fake_exe(d.path(), body);
            with_env(d.path(), || {
                run_loop_now(&opts(d.path(), exe), "gitlab-loop", &[])
            })
        };
        let log = || std::fs::read_to_string(d.path().join("logs/loop-engineering.log")).unwrap();

        assert_eq!(run("exit 3"), 3);
        assert_eq!(status::read_status(&status_path)["consecutive_failures"], 1);
        assert!(!log().contains("Slack alert held back"));

        assert_eq!(run("exit 3"), 3);
        assert_eq!(status::read_status(&status_path)["consecutive_failures"], 2);
        assert!(
            log().contains("failed the same way 2 runs in a row; Slack alert held back"),
            "{}",
            log()
        );

        // A different failure is news again.
        assert_eq!(run("exit 4"), 4);
        assert_eq!(status::read_status(&status_path)["consecutive_failures"], 1);
        assert_eq!(log().matches("Slack alert held back").count(), 1);

        // A success resets the count.
        assert_eq!(run("exit 0"), 0);
        assert_eq!(run("exit 4"), 4);
        assert_eq!(status::read_status(&status_path)["consecutive_failures"], 1);
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
    fn unrestricted_cli_warns_in_the_log_and_run_started_event() {
        let d = tempfile::tempdir().unwrap();
        setup(d.path(), true, 30);
        std::fs::write(d.path().join("ai_cli.json"), r#"{"cli": "gemini"}"#).unwrap();
        let exe = fake_exe(d.path(), "true");
        let code = with_env(d.path(), || {
            run_loop_now(&opts(d.path(), exe), "gitlab-loop", &[])
        });
        assert_eq!(code, 0);
        let unified = std::fs::read_to_string(d.path().join("logs/loop-engineering.log")).unwrap();
        let warning =
            crate::ai_cli_safety::warning("gemini", crate::ai_cli_safety::CallSite::GitlabLoop)
                .unwrap();
        assert_eq!(unified.matches(warning).count(), 1, "{unified}");
        let events = read_events(d.path());
        assert!(events.contains("\"warning\""), "{events}");
        assert!(events.contains(warning), "{events}");
    }

    #[test]
    fn restricted_cli_has_no_warning() {
        let d = tempfile::tempdir().unwrap();
        setup(d.path(), true, 30);
        std::fs::write(d.path().join("ai_cli.json"), r#"{"cli": "claude"}"#).unwrap();
        let exe = fake_exe(d.path(), "true");
        with_env(d.path(), || {
            run_loop_now(&opts(d.path(), exe), "gitlab-loop", &[])
        });
        let unified = std::fs::read_to_string(d.path().join("logs/loop-engineering.log")).unwrap();
        assert!(!unified.contains("full tool access"), "{unified}");
        assert!(!read_events(d.path()).contains("\"warning\""));
    }

    fn read_events(dir: &Path) -> String {
        std::fs::read_to_string(
            std::fs::read_dir(dir.join("events"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap()
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
