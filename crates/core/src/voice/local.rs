//! The user's own faster-whisper: one long-lived Python worker per interpreter, so the model
//! loads once, stopped after [`IDLE`] without a request. ffmpeg decodes the recording to raw
//! 16 kHz mono samples first, because faster-whisper's own decoder breaks on some PyAV versions.

use crate::proc;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

const WORKER_SCRIPT: &str = include_str!("worker.py");
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const DECODE_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
pub const IDLE: Duration = Duration::from_secs(600);

struct Worker {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    last_used: Instant,
    _cwd: tempfile::TempDir,
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

static WORKERS: Mutex<Option<HashMap<PathBuf, Worker>>> = Mutex::new(None);
static REAPER: Once = Once::new();

fn start_reaper() {
    REAPER.call_once(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(Duration::from_secs(60));
            if let Ok(mut guard) = WORKERS.lock() {
                if let Some(map) = guard.as_mut() {
                    map.retain(|_, w| w.last_used.elapsed() < IDLE);
                }
            }
        });
    });
}

/// A fresh empty directory to run Python in: `-c` puts the working directory on `sys.path`.
fn python_command(python: &Path) -> std::io::Result<(Command, tempfile::TempDir)> {
    let cwd = tempfile::tempdir()?;
    let mut cmd = Command::new(python);
    cmd.current_dir(cwd.path()).args(["-E", "-u"]);
    Ok((cmd, cwd))
}

/// Whether `python` can import faster-whisper.
pub fn has_faster_whisper(python: &Path) -> bool {
    let Ok((mut cmd, _cwd)) = python_command(python) else {
        return false;
    };
    cmd.args(["-c", "import faster_whisper, numpy"]);
    proc::run(&mut cmd, None, Some(PROBE_TIMEOUT)).is_ok_and(|o| o.success())
}

fn spawn(python: &Path) -> Result<Worker, String> {
    let (mut cmd, cwd) = python_command(python).map_err(|e| e.to_string())?;
    let mut child = cmd
        .args(["-c", WORKER_SCRIPT])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", python.display()))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    Ok(Worker {
        child,
        stdin,
        lines,
        last_used: Instant::now(),
        _cwd: cwd,
    })
}

fn ask(worker: &mut Worker, request: &serde_json::Value) -> Result<String, String> {
    writeln!(worker.stdin, "{request}").map_err(|e| format!("worker stopped: {e}"))?;
    worker
        .stdin
        .flush()
        .map_err(|e| format!("worker stopped: {e}"))?;
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = match worker.lines.recv_timeout(left) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => return Err("transcription timed out".into()),
            Err(RecvTimeoutError::Disconnected) => return Err("the Whisper worker exited".into()),
        };
        let Ok(reply) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(text) = reply.get("text").and_then(|v| v.as_str()) {
            return Ok(text.trim().to_string());
        }
        if let Some(err) = reply.get("error").and_then(|v| v.as_str()) {
            return Err(err.to_string());
        }
    }
}

/// ffmpeg: any recording to raw little-endian f32, 16 kHz mono.
fn decode(ffmpeg: &Path, input: &Path, output: &Path) -> Result<(), String> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(input)
        .args(["-f", "f32le", "-ac", "1", "-ar", "16000"])
        .arg(output);
    let out = proc::run(&mut cmd, None, Some(DECODE_TIMEOUT)).map_err(|e| e.to_string())?;
    if !out.success() {
        return Err(format!(
            "ffmpeg could not read the recording: {}",
            out.stderr.trim()
        ));
    }
    Ok(())
}

pub struct Request<'a> {
    pub python: &'a Path,
    pub ffmpeg: &'a Path,
    pub model: &'a str,
    pub language: &'a str,
    pub audio: &'a [u8],
    pub extension: &'a str,
}

/// The transcript, trimmed (empty when nothing was said).
pub fn transcribe(req: &Request) -> Result<String, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let input = dir.path().join(format!("input.{}", req.extension));
    let samples = dir.path().join("audio.f32");
    std::fs::write(&input, req.audio).map_err(|e| e.to_string())?;
    decode(req.ffmpeg, &input, &samples)?;
    if std::fs::metadata(&samples).map_or(0, |m| m.len()) == 0 {
        return Ok(String::new());
    }
    let request = serde_json::json!({
        "audio": samples.to_string_lossy(),
        "model": req.model,
        "language": req.language,
    });
    start_reaper();
    let mut guard = WORKERS.lock().map_err(|_| "worker lock poisoned")?;
    let map = guard.get_or_insert_with(HashMap::new);
    let python = req.python.to_path_buf();
    if let Some(w) = map.get_mut(&python) {
        if !matches!(w.child.try_wait(), Ok(None)) {
            map.remove(&python);
        }
    }
    if !map.contains_key(&python) {
        map.insert(python.clone(), spawn(&python)?);
    }
    let worker = map.get_mut(&python).expect("just inserted");
    let result = ask(worker, &request);
    match &result {
        Ok(_) => worker.last_used = Instant::now(),
        Err(_) => {
            map.remove(&python);
        }
    }
    result
}

/// faster-whisper model names (`small`, `large-v3`, ...) already in the Hugging Face cache.
pub fn cached_models(hub: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(hub)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let model = name
                .strip_prefix("models--Systran--faster-whisper-")?
                .to_string();
            let complete = std::fs::read_dir(e.path().join("snapshots"))
                .into_iter()
                .flatten()
                .flatten()
                .any(|s| s.path().join("model.bin").exists());
            complete.then_some(model)
        })
        .collect();
    names.sort();
    names
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    pub(crate) fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    pub(crate) fn fake_ffmpeg(dir: &Path) -> PathBuf {
        script(
            dir,
            "ffmpeg",
            "for a; do last=$a; done\nprintf 'abcd' > \"$last\"",
        )
    }

    /// A stand-in interpreter: logs each start and request next to itself, answers with `reply`.
    pub(crate) fn fake_python(dir: &Path, reply: &str) -> PathBuf {
        let log = dir.join("requests.log");
        let starts = dir.join("starts.log");
        script(
            dir,
            "python3",
            &format!(
                "echo start >> '{}'\nwhile IFS= read -r line; do echo \"$line\" >> '{}'; echo 'noise'; echo '{reply}'; done",
                starts.display(),
                log.display()
            ),
        )
    }

    fn request<'a>(python: &'a Path, ffmpeg: &'a Path) -> Request<'a> {
        Request {
            python,
            ffmpeg,
            model: "small",
            language: "fr",
            audio: b"webm-bytes",
            extension: "webm",
        }
    }

    #[test]
    fn one_worker_serves_every_request_and_gets_the_model_and_language() {
        let d = tempfile::tempdir().unwrap();
        let py = fake_python(
            d.path(),
            r#"{"text":"  bonjour tout le monde ","language":"fr"}"#,
        );
        let ff = fake_ffmpeg(d.path());
        assert_eq!(
            transcribe(&request(&py, &ff)).unwrap(),
            "bonjour tout le monde"
        );
        assert_eq!(
            transcribe(&request(&py, &ff)).unwrap(),
            "bonjour tout le monde"
        );
        let starts = std::fs::read_to_string(d.path().join("starts.log")).unwrap();
        assert_eq!(starts.lines().count(), 1);
        let log = std::fs::read_to_string(d.path().join("requests.log")).unwrap();
        let first: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(first["model"], "small");
        assert_eq!(first["language"], "fr");
        assert!(first["audio"].as_str().unwrap().ends_with("audio.f32"));
    }

    #[test]
    fn a_worker_error_is_reported_and_the_worker_is_replaced() {
        let d = tempfile::tempdir().unwrap();
        let py = fake_python(d.path(), r#"{"error":"RuntimeError: no model"}"#);
        let ff = fake_ffmpeg(d.path());
        assert_eq!(
            transcribe(&request(&py, &ff)).unwrap_err(),
            "RuntimeError: no model"
        );
        let _ = transcribe(&request(&py, &ff));
        let starts = std::fs::read_to_string(d.path().join("starts.log")).unwrap();
        assert_eq!(starts.lines().count(), 2);
    }

    #[test]
    fn a_failed_decode_never_reaches_python() {
        let d = tempfile::tempdir().unwrap();
        let py = fake_python(d.path(), r#"{"text":"x"}"#);
        let ff = script(d.path(), "ffmpeg", "echo 'Invalid data' >&2; exit 1");
        let err = transcribe(&request(&py, &ff)).unwrap_err();
        assert!(err.contains("Invalid data"), "{err}");
        assert!(!d.path().join("starts.log").exists());
    }

    #[test]
    fn silence_decodes_to_nothing_and_skips_the_model() {
        let d = tempfile::tempdir().unwrap();
        let py = fake_python(d.path(), r#"{"text":"x"}"#);
        let ff = script(d.path(), "ffmpeg", "for a; do last=$a; done\n: > \"$last\"");
        assert_eq!(transcribe(&request(&py, &ff)).unwrap(), "");
        assert!(!d.path().join("starts.log").exists());
    }

    #[test]
    fn the_probe_tells_a_working_interpreter_from_a_broken_one() {
        let d = tempfile::tempdir().unwrap();
        assert!(has_faster_whisper(&script(d.path(), "ok", "exit 0")));
        assert!(!has_faster_whisper(&script(d.path(), "bad", "exit 1")));
        assert!(!has_faster_whisper(&d.path().join("missing")));
    }

    #[test]
    fn only_fully_downloaded_systran_models_count() {
        let d = tempfile::tempdir().unwrap();
        let hub = d.path();
        for (dir, complete) in [
            ("models--Systran--faster-whisper-small", true),
            ("models--Systran--faster-whisper-large-v3", true),
            ("models--Systran--faster-whisper-medium", false),
            ("models--other--thing", true),
        ] {
            let snap = hub.join(dir).join("snapshots/abc");
            std::fs::create_dir_all(&snap).unwrap();
            if complete {
                std::fs::write(snap.join("model.bin"), "x").unwrap();
            }
        }
        assert_eq!(cached_models(hub), vec!["large-v3", "small"]);
        assert!(cached_models(&hub.join("nope")).is_empty());
    }
}
