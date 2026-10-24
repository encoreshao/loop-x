//! Run a child process to completion with an optional timeout and optional
//! stdin, capturing stdout/stderr as text. The std equivalent of
//! `subprocess.run(..., capture_output=True, text=True, timeout=...)` that the
//! Python agents, verifiers and sealed call rely on.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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
    let deadline = timeout.map(|t| Instant::now() + t);
    let (code, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (exit_code(status), false);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            let _ = child.kill();
            let _ = child.wait();
            break (None, true);
        }
        thread::sleep(Duration::from_millis(5));
    };
    Ok(ProcOutput {
        code,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
        timed_out,
    })
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
    fn missing_binary_is_an_error() {
        assert!(run(&mut Command::new("/nonexistent/bin"), None, None).is_err());
    }
}
