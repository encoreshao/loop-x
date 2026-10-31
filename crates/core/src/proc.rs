//! Run a child process to completion with an optional timeout and optional
//! stdin, capturing stdout/stderr as text. The std equivalent of
//! `subprocess.run(..., capture_output=True, text=True, timeout=...)` that the
//! Python agents, verifiers and sealed call rely on.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// A finished (or timed-out) child.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcOutput {
    /// Python's `returncode`: the exit code, or `-<signal>` when killed by one.
    /// `None` only when the child was killed because it timed out.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl ProcOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

fn drain<R: Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    })
}

#[cfg(unix)]
fn exit_code(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.code().or_else(|| status.signal().map(|s| -s))
}

#[cfg(not(unix))]
fn exit_code(status: std::process::ExitStatus) -> Option<i32> {
    status.code()
}

/// Run `cmd`, feeding it `input` on stdin (or /dev/null when `None`). On
/// timeout the child is killed and whatever it printed so far is returned with
/// `timed_out = true`. `Err` only when the process could not be spawned.
pub fn run(
    cmd: &mut Command,
    input: Option<&str>,
    timeout: Option<Duration>,
) -> std::io::Result<ProcOutput> {
    run_watched(cmd, input, timeout, None).map(|(out, _)| out)
}

/// A file the child touches while it makes progress; `after` without a change
/// (counted from the start, if it never changes) means the child is stuck.
#[derive(Debug, Clone)]
pub struct Stall {
    pub file: PathBuf,
    pub after: Duration,
}

/// The newest of `start` and `file`'s modification time, as an instant.
fn last_activity(file: &Path, start: Instant, started_at: SystemTime) -> Instant {
    std::fs::metadata(file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.duration_since(started_at).ok())
        .map_or(start, |since| start + since)
}

/// [`run`] that also kills the child when `stall`'s file goes quiet; the
/// `bool` says it was stopped for that (the output then has `timed_out`).
pub fn run_watched(
    cmd: &mut Command,
    input: Option<&str>,
    timeout: Option<Duration>,
    stall: Option<&Stall>,
) -> std::io::Result<(ProcOutput, bool)> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = cmd.spawn()?;
    let out = drain(child.stdout.take().expect("piped stdout"));
    let err = drain(child.stderr.take().expect("piped stderr"));
    if let Some(text) = input {
        let mut stdin = child.stdin.take().expect("piped stdin");
        let bytes = text.as_bytes().to_vec();
        // A child that exits without reading its stdin must not wedge us.
        thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    let (start, started_at) = (Instant::now(), SystemTime::now());
    let deadline = timeout.map(|t| start + t);
    let mut next_stall_check = start;
    let (code, timed_out, stalled) = loop {
        if let Some(status) = child.try_wait()? {
            break (exit_code(status), false, false);
        }
        let now = Instant::now();
        let stalled = stall.is_some_and(|s| {
            if now < next_stall_check {
                return false;
            }
            next_stall_check = now + Duration::from_millis(100);
            now.duration_since(last_activity(&s.file, start, started_at)) >= s.after
        });
        if stalled || deadline.is_some_and(|d| now >= d) {
            let _ = child.kill();
            let _ = child.wait();
            break (None, true, stalled);
        }
        thread::sleep(Duration::from_millis(5));
    };
    let out = ProcOutput {
        code,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
        timed_out,
    };
    Ok((out, stalled))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.arg("-c").arg(script);
        c
    }

    #[test]
    fn captures_output_and_exit_code() {
        let o = run(&mut sh("echo out; echo err >&2; exit 3"), None, None).unwrap();
        assert_eq!(o.code, Some(3));
        assert_eq!(o.stdout, "out\n");
        assert_eq!(o.stderr, "err\n");
        assert!(!o.timed_out && !o.success());
    }

    #[test]
    fn feeds_stdin() {
        let o = run(&mut sh("cat"), Some("hello"), None).unwrap();
        assert_eq!(o.stdout, "hello");
        assert!(o.success());
    }

    #[test]
    fn kills_on_timeout_and_keeps_partial_output() {
        let start = Instant::now();
        let o = run(
            &mut sh("echo partial; exec sleep 30"),
            None,
            Some(Duration::from_millis(300)),
        )
        .unwrap();
        assert!(o.timed_out);
        assert_eq!(o.code, None);
        assert_eq!(o.stdout, "partial\n");
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_child_whose_activity_file_goes_quiet_is_stopped_as_stalled() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("activity.json");
        let start = Instant::now();
        let watch = Stall {
            file: file.clone(),
            after: Duration::from_millis(400),
        };
        let (o, stalled) = run_watched(
            &mut sh("exec sleep 30"),
            None,
            Some(Duration::from_secs(20)),
            Some(&watch),
        )
        .unwrap();
        assert!(stalled && o.timed_out);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_child_that_keeps_reporting_activity_is_not_stalled() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("activity.json");
        let script = format!(
            "for i in 1 2 3 4 5 6 7 8; do touch '{}'; sleep 0.1; done; echo ok",
            file.display()
        );
        let watch = Stall {
            file,
            after: Duration::from_millis(400),
        };
        let (o, stalled) = run_watched(
            &mut sh(&script),
            None,
            Some(Duration::from_secs(20)),
            Some(&watch),
        )
        .unwrap();
        assert!(!stalled && !o.timed_out);
        assert_eq!(o.stdout, "ok\n");
    }

    #[test]
    fn missing_binary_is_an_error() {
        assert!(run(&mut Command::new("/nonexistent/bin"), None, None).is_err());
    }
}
