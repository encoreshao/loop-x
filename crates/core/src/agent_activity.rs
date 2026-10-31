//! What a running Claude agent is doing right now. Each GitLab-loop `claude -p`
//! call gets `--settings` with hooks that run `loopx _agent-hook <file>
//! <label>`; the hook overwrites `outputs/agent-activity.json` with the latest
//! event. The dashboard reads it to say "running Bash: cargo test", and the
//! runner stops a call whose file has not changed for [`STALL_AFTER`].

use crate::settings_store::atomic_write;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// No hook event for this long means the agent is wedged. Twice Claude Code's
/// own 10-minute cap on a single Bash command, so a slow test suite never trips it.
pub const STALL_AFTER: Duration = Duration::from_secs(20 * 60);

const FILE_NAME: &str = "agent-activity.json";
const DETAIL_CHARS: usize = 120;
const HOOK_EVENTS: [&str; 4] = ["PreToolUse", "PostToolUse", "Notification", "Stop"];

/// The activity file, next to the GitLab loop's `status.json`.
pub fn path(base_dir: Option<&Path>) -> PathBuf {
    beside(&crate::loop_scheduler::status_path_for_loop(
        "gitlab-loop",
        base_dir,
    ))
}

/// The activity file next to the GitLab loop's status file `status_path`.
pub fn beside(status_path: &Path) -> PathBuf {
    status_path.with_file_name(FILE_NAME)
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The `--settings` JSON that reports every hook event to `file`.
pub fn hook_settings(exe: &str, file: &Path, label: &str) -> String {
    let command = format!(
        "{} _agent-hook {} {}",
        sh_quote(exe),
        sh_quote(&file.display().to_string()),
        sh_quote(label)
    );
    let hook = json!([{"type": "command", "command": command, "timeout": 10}]);
    let hooks: serde_json::Map<String, Value> = HOOK_EVENTS
        .iter()
        .map(|event| {
            let entry = if event.ends_with("ToolUse") {
                json!([{"matcher": "*", "hooks": hook}])
            } else {
                json!([{"hooks": hook}])
            };
            (event.to_string(), entry)
        })
        .collect();
    json!({ "hooks": hooks }).to_string()
}

fn clip(s: &str) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() > DETAIL_CHARS {
        let cut: String = line.chars().take(DETAIL_CHARS).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

/// The one-line gist of a tool call: the command, file or pattern it works on.
fn tool_detail(input: &Value) -> Option<String> {
    ["command", "file_path", "pattern", "url", "description"]
        .iter()
        .find_map(|k| input.get(k).and_then(Value::as_str))
        .map(clip)
        .filter(|d| !d.is_empty())
}

/// The activity record for one hook's stdin, or `None` when it isn't a hook payload.
pub fn from_hook_input(stdin: &str, label: &str, at: &str) -> Option<Value> {
    let input: Value = serde_json::from_str(stdin.trim()).ok()?;
    let event = input.get("hook_event_name")?.as_str()?;
    let detail = match event {
        "Notification" => input.get("message").and_then(Value::as_str).map(clip),
        _ => input.get("tool_input").and_then(tool_detail),
    };
    Some(json!({
        "at": at,
        "label": label,
        "event": event,
        "tool": input.get("tool_name"),
        "detail": detail,
    }))
}

/// Overwrite `file` with the record for one hook's stdin. Malformed input is
/// ignored: a hook must never get in the agent's way.
pub fn record(file: &Path, stdin: &str, label: &str, at: &str) -> std::io::Result<()> {
    match from_hook_input(stdin, label, at) {
        Some(v) => atomic_write(file, v.to_string().as_bytes(), None),
        None => Ok(()),
    }
}

/// Mark the start of a call, so the dashboard and the stall watch never read
/// the previous call's last event.
pub fn start(file: &Path, label: &str, at: &str) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let v = json!({"at": at, "label": label, "event": "Start", "tool": null, "detail": null});
    atomic_write(file, v.to_string().as_bytes(), None)
}

/// The timestamp every record carries.
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// `loopx _agent-hook <file> <label>`: record the hook payload on `stdin`.
/// Never fails and prints nothing, so it can't change what the agent does.
pub fn hook_main(args: &[String], mut stdin: impl std::io::Read) {
    let [file, label, ..] = args else {
        return;
    };
    let mut input = String::new();
    if stdin.read_to_string(&mut input).is_ok() {
        let _ = record(Path::new(file), &input, label, &now_iso());
    }
}

pub fn read(file: &Path) -> Option<Value> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
    v.is_object().then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_hook_every_event_into_the_file() {
        let s: Value = serde_json::from_str(&hook_settings(
            "/Apps/Loop X/loopx",
            Path::new("/o/agent-activity.json"),
            "web #1",
        ))
        .unwrap();
        for event in HOOK_EVENTS {
            let cmd = s["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap();
            assert_eq!(
                cmd,
                "'/Apps/Loop X/loopx' _agent-hook '/o/agent-activity.json' 'web #1'"
            );
        }
        assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"], "*");
        assert!(s["hooks"]["Stop"][0].get("matcher").is_none());
    }

    #[test]
    fn a_bash_call_is_recorded_with_its_command() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.json");
        let stdin = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cargo test --workspace\necho done","description":"Run tests"}}"#;
        record(&f, stdin, "web #1", "2026-10-31T10:00:00+00:00").unwrap();
        let v = read(&f).unwrap();
        assert_eq!(v["event"], "PreToolUse");
        assert_eq!(v["tool"], "Bash");
        assert_eq!(v["detail"], "cargo test --workspace");
        assert_eq!(v["label"], "web #1");
        assert_eq!(v["at"], "2026-10-31T10:00:00+00:00");
    }

    #[test]
    fn edits_name_the_file_and_notifications_keep_the_message() {
        let v = from_hook_input(
            r#"{"hook_event_name":"PostToolUse","tool_name":"Edit","tool_input":{"file_path":"/wt/src/a.rs","old_string":"x"}}"#,
            "l",
            "t",
        )
        .unwrap();
        assert_eq!(
            (v["tool"].as_str(), v["detail"].as_str()),
            (Some("Edit"), Some("/wt/src/a.rs"))
        );
        let v = from_hook_input(
            r#"{"hook_event_name":"Notification","message":"Claude needs your permission to use Bash"}"#,
            "l",
            "t",
        )
        .unwrap();
        assert_eq!(v["detail"], "Claude needs your permission to use Bash");
        assert!(v["tool"].is_null());
    }

    #[test]
    fn long_details_are_clipped() {
        let long = "x".repeat(300);
        let v = from_hook_input(
            &json!({"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command": long}}).to_string(),
            "l",
            "t",
        )
        .unwrap();
        assert_eq!(
            v["detail"].as_str().unwrap().chars().count(),
            DETAIL_CHARS + 1
        );
    }

    #[test]
    fn garbage_input_writes_nothing_and_does_not_fail() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.json");
        record(&f, "not json", "l", "t").unwrap();
        record(&f, r#"{"no_event": 1}"#, "l", "t").unwrap();
        assert!(!f.exists());
        assert!(read(&f).is_none());
    }

    #[test]
    fn the_hook_command_records_stdin_and_ignores_bad_args() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.json");
        let args = [f.display().to_string(), "web #4".to_string()];
        hook_main(&args[..1], r#"{"hook_event_name":"Stop"}"#.as_bytes());
        assert!(!f.exists());
        hook_main(&args, r#"{"hook_event_name":"Stop"}"#.as_bytes());
        let v = read(&f).unwrap();
        assert_eq!(
            (v["event"].as_str(), v["label"].as_str()),
            (Some("Stop"), Some("web #4"))
        );
    }

    #[test]
    fn start_resets_the_file() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("sub/a.json");
        start(&f, "web #2", "t0").unwrap();
        let v = read(&f).unwrap();
        assert_eq!(
            (v["event"].as_str(), v["label"].as_str()),
            (Some("Start"), Some("web #2"))
        );
    }

    #[test]
    fn the_file_sits_next_to_the_gitlab_status() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            path(Some(d.path())),
            d.path().join("outputs/agent-activity.json")
        );
    }
}
