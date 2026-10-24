//! Dashboard run control: start a loop run detached ("Run now") and stop one
//! ("Stop", a `killpg` of the pid recorded in the status file). Mirrors
//! `trigger_manual_run`, `trigger_topic_monitor_run`, `trigger_inbox_triage_run`,
//! `_process_alive`, `_kill_process_group`, `stop_gitlab_loop` and
//! `stop_topic_loop` of `bin/web/dashboard_server.py`. The launcher is the
//! `loopx run <loop>` command (what `run-loop-now.sh` became); it is passed in so
//! tests can substitute a fake.

use crate::status;
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};

/// A valid pid recorded in a status file: a positive integer other than 1.
/// (Python would happily `killpg(0)`; here such values count as "no process".)
pub fn recorded_pid(status: &Value) -> Option<i64> {
    status
        .get("pid")
        .and_then(Value::as_i64)
        .filter(|p| *p > 1 && *p <= i64::from(i32::MAX))
}

/// `_process_alive`: reaps the pid if it is our own exited child (so a finished
/// run is not mistaken for a live zombie), else a plain existence check.
#[cfg(unix)]
pub fn process_alive(pid: i64) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    let mut wstatus = 0;
    let r = unsafe { libc::waitpid(pid, &mut wstatus, libc::WNOHANG) };
    if r == 0 {
        return true;
    }
    if r == pid {
        return false;
    }
    // ECHILD: not our child.
    match unsafe { libc::kill(pid, 0) } {
        0 => true,
        _ => std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM),
    }
}

#[cfg(not(unix))]
pub fn process_alive(_pid: i64) -> bool {
    false
}

/// `_kill_process_group`: SIGTERM the whole group (every run is launched in its
/// own session, so its pid is its pgid), escalating to SIGKILL if it is still
/// alive after `wait`.
#[cfg(unix)]
pub fn kill_process_group(pid: i64, wait: Duration, poll: Duration) {
    let Ok(pid) = i32::try_from(pid) else { return };
    if pid <= 1 {
        return;
    }
    if unsafe { libc::killpg(pid, libc::SIGTERM) } != 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    {
        return;
    }
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if !process_alive(i64::from(pid)) {
            return;
        }
        std::thread::sleep(poll);
    }
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
pub fn kill_process_group(_pid: i64, _wait: Duration, _poll: Duration) {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// No run was in progress.
    NoRun,
    /// A stale "running" state was cleared (no live process).
    ClearedStale,
    Stopped,
}

fn stop_with(status: &Value, status_path: &Path, wait: Duration) -> bool {
    let pid = recorded_pid(status);
    let stale = pid.is_none_or(|p| !process_alive(p));
    if let (false, Some(p)) = (stale, pid) {
        kill_process_group(p, wait, Duration::from_millis(100));
    }
    let _ = status::write_status(status_path, "stopped", &[]);
    stale
}

/// `stop_gitlab_loop`.
pub fn stop_gitlab_loop(status_path: &Path) -> StopOutcome {
    stop_gitlab_loop_with(status_path, Duration::from_secs(2))
}

pub fn stop_gitlab_loop_with(status_path: &Path, wait: Duration) -> StopOutcome {
    let status = status::read_status(status_path);
    if status.get("state").and_then(Value::as_str) != Some("running") {
        return StopOutcome::NoRun;
    }
    if stop_with(&status, status_path, wait) {
        StopOutcome::ClearedStale
    } else {
        StopOutcome::Stopped
    }
}

/// `stop_topic_loop`: acts when either the loop-level status or any topic entry
/// says "running", and flips every running topic entry to "stopped".
pub fn stop_topic_loop(status_path: &Path, topic_status_path: &Path) -> StopOutcome {
    stop_topic_loop_with(status_path, topic_status_path, Duration::from_secs(2))
}

pub fn stop_topic_loop_with(
    status_path: &Path,
    topic_status_path: &Path,
    wait: Duration,
) -> StopOutcome {
    let status = status::read_status(status_path);
    let topics = status::read_topic_status(topic_status_path);
    let running: Vec<String> = topics["topics"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(_, e)| e.get("state").and_then(Value::as_str) == Some("running"))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default();
    if status.get("state").and_then(Value::as_str) != Some("running") && running.is_empty() {
        return StopOutcome::NoRun;
    }
    let stale = stop_with(&status, status_path, wait);
    for name in running {
        let _ = status::write_topic_status(topic_status_path, &name, "stopped", &[]);
    }
    if stale {
        StopOutcome::ClearedStale
    } else {
        StopOutcome::Stopped
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerOutcome {
    AlreadyRunning,
    /// The launcher could not be started (carries the path shown to the user).
    LauncherMissing(String),
    Started,
}

/// What "Run now" runs: `cmd` is the command prefix (e.g. `["/path/loopx",
/// "run"]`), the loop name is appended; `cwd` is the checkout.
pub struct Launcher<'a> {
    pub cmd: &'a [String],
    pub cwd: &'a Path,
    /// Shown in the "not found" message when `cmd` is empty.
    pub fallback_path: &'a Path,
}

/// `subprocess.Popen(..., start_new_session=True)` with every stdio detached.
#[cfg(unix)]
pub fn launch_detached(launcher: &Launcher, loop_name: &str) -> TriggerOutcome {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let Some((program, args)) = launcher.cmd.split_first() else {
        return TriggerOutcome::LauncherMissing(launcher.fallback_path.display().to_string());
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .arg(loop_name)
        .current_dir(launcher.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    match command.spawn() {
        // The server never waits on it; `process_alive` reaps it later.
        Ok(child) => {
            std::mem::forget(child);
            TriggerOutcome::Started
        }
        Err(_) => TriggerOutcome::LauncherMissing(program.clone()),
    }
}

#[cfg(not(unix))]
pub fn launch_detached(launcher: &Launcher, _loop_name: &str) -> TriggerOutcome {
    TriggerOutcome::LauncherMissing(launcher.fallback_path.display().to_string())
}

/// `trigger_manual_run`: refuse while the loop's status says "running".
pub fn trigger_manual_run(
    status_path: &Path,
    loop_name: &str,
    launcher: &Launcher,
) -> TriggerOutcome {
    let status = status::read_status(status_path);
    if status.get("state").and_then(Value::as_str) == Some("running") {
        return TriggerOutcome::AlreadyRunning;
    }
    launch_detached(launcher, loop_name)
}

/// `trigger_topic_monitor_run`: refuse while any topic is running.
pub fn trigger_topic_monitor_run(
    topic_status_path: &Path,
    loop_name: &str,
    launcher: &Launcher,
) -> TriggerOutcome {
    let topics = status::read_topic_status(topic_status_path);
    let any_running = topics["topics"].as_object().is_some_and(|m| {
        m.values()
            .any(|e| e.get("state").and_then(Value::as_str) == Some("running"))
    });
    if any_running {
        return TriggerOutcome::AlreadyRunning;
    }
    launch_detached(launcher, loop_name)
}

/// `trigger_inbox_triage_run`: refuse only while the loop-level status says
/// "running" AND its recorded pid is still alive.
pub fn trigger_inbox_triage_run(
    status_path: &Path,
    loop_name: &str,
    launcher: &Launcher,
) -> TriggerOutcome {
    let status = status::read_status(status_path);
    let alive = recorded_pid(&status).is_some_and(process_alive);
    if status.get("state").and_then(Value::as_str) == Some("running") && alive {
        return TriggerOutcome::AlreadyRunning;
    }
    launch_detached(launcher, loop_name)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::process::{Command, Stdio};

    fn sleeper() -> std::process::Child {
        use std::os::unix::process::CommandExt;
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[test]
    fn recorded_pid_rejects_nonsense() {
        assert_eq!(recorded_pid(&json!({"pid": 4242})), Some(4242));
        for bad in [
            json!({}),
            json!({"pid": 0}),
            json!({"pid": 1}),
            json!({"pid": -5}),
            json!({"pid": "7"}),
        ] {
            assert_eq!(recorded_pid(&bad), None);
        }
    }

    #[test]
    fn stop_without_a_running_state_does_nothing() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("status.json");
        assert_eq!(stop_gitlab_loop(&p), StopOutcome::NoRun);
        status::write_status(&p, "idle", &[]).unwrap();
        assert_eq!(stop_gitlab_loop(&p), StopOutcome::NoRun);
        assert_eq!(status::read_status(&p)["state"], "idle");
    }

    #[test]
    fn a_stale_running_state_is_cleared() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("status.json");
        // A pid that is not running: a child we already reaped.
        let mut done = Command::new("true").spawn().unwrap();
        let dead = i64::from(done.id());
        done.wait().unwrap();
        status::write_status(&p, "running", &[("pid", json!(dead))]).unwrap();
        assert_eq!(stop_gitlab_loop(&p), StopOutcome::ClearedStale);
        assert_eq!(status::read_status(&p)["state"], "stopped");
        status::write_status(&p, "running", &[]).unwrap();
        assert_eq!(stop_gitlab_loop(&p), StopOutcome::ClearedStale);
    }

    #[test]
    #[allow(clippy::zombie_processes)]
    fn stop_kills_the_recorded_process_group() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("status.json");
        let child = sleeper();
        status::write_status(&p, "running", &[("pid", json!(child.id()))]).unwrap();
        assert!(process_alive(i64::from(child.id())));
        assert_eq!(
            stop_gitlab_loop_with(&p, Duration::from_secs(5)),
            StopOutcome::Stopped
        );
        assert_eq!(status::read_status(&p)["state"], "stopped");
        // Reaped by process_alive's waitpid: it is gone.
        assert!(!process_alive(i64::from(child.id())));
    }

    #[test]
    fn stopping_the_topic_loop_flips_running_topics() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("status.json");
        let t = d.path().join("topics.json");
        assert_eq!(stop_topic_loop(&p, &t), StopOutcome::NoRun);
        status::write_topic_status(&t, "a", "running", &[]).unwrap();
        status::write_topic_status(&t, "b", "idle", &[]).unwrap();
        status::write_status(&p, "idle", &[]).unwrap();
        assert_eq!(stop_topic_loop(&p, &t), StopOutcome::ClearedStale);
        let doc = status::read_topic_status(&t);
        assert_eq!(doc["topics"]["a"]["state"], "stopped");
        assert_eq!(doc["topics"]["b"]["state"], "idle");
        assert_eq!(status::read_status(&p)["state"], "stopped");
    }

    #[test]
    fn triggers_refuse_while_running_and_report_a_missing_launcher() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("status.json");
        let t = d.path().join("topics.json");
        let marker = d.path().join("started");
        let cmd: Vec<String> = [
            "sh".to_string(),
            "-c".to_string(),
            format!("echo \"$1\" > {}", marker.display()),
            "sh".to_string(),
        ]
        .to_vec();
        let launcher = Launcher {
            cmd: &cmd,
            cwd: d.path(),
            fallback_path: d.path(),
        };
        status::write_status(&p, "running", &[]).unwrap();
        assert_eq!(
            trigger_manual_run(&p, "gitlab-loop", &launcher),
            TriggerOutcome::AlreadyRunning
        );
        assert!(!marker.exists());
        status::write_status(&p, "idle", &[]).unwrap();
        assert_eq!(
            trigger_manual_run(&p, "rss-loop", &launcher),
            TriggerOutcome::Started
        );
        for _ in 0..100 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "rss-loop");

        status::write_topic_status(&t, "a", "running", &[]).unwrap();
        assert_eq!(
            trigger_topic_monitor_run(&t, "topic-loop", &launcher),
            TriggerOutcome::AlreadyRunning
        );

        // Inbox: "running" with a dead pid does not block.
        let mut done = Command::new("true").spawn().unwrap();
        let dead = done.id();
        done.wait().unwrap();
        status::write_status(&p, "running", &[("pid", json!(dead))]).unwrap();
        let none: Vec<String> = Vec::new();
        let missing = Launcher {
            cmd: &none,
            cwd: d.path(),
            fallback_path: Path::new("/x/run-loop-now.sh"),
        };
        assert_eq!(
            trigger_inbox_triage_run(&p, "inbox-triage-loop", &missing),
            TriggerOutcome::LauncherMissing("/x/run-loop-now.sh".into())
        );
        let mut live = sleeper();
        status::write_status(&p, "running", &[("pid", json!(live.id()))]).unwrap();
        assert_eq!(
            trigger_inbox_triage_run(&p, "inbox-triage-loop", &launcher),
            TriggerOutcome::AlreadyRunning
        );
        let _ = live.kill();
        let _ = live.wait();
    }
}
