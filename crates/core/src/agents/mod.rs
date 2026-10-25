//! The agent adapter: build and run the `claude` / `codex` CLI for one prompt.
//! Mirrors `bin/agents/` (`base.py`, `claude.py`, `codex.py`, `sealed.py`).
//!
//! [`Agent::run`] never panics or errors: a timeout, a non-zero exit, or a
//! binary that can't be started all become an [`AgentResult`] with status
//! `timeout` / `failed`. (Python lets a missing binary raise; reporting it as a
//! failed result is the only difference.)

pub mod claude;
pub mod codex;
pub mod cursor;
pub mod gemini;
pub mod kimi;
pub mod sealed;

use crate::ai_cli_config;
use crate::proc::ProcOutput;
use std::path::{Path, PathBuf};

pub const VALID_PROVIDERS: [&str; 5] = ai_cli_config::VALID_CLIS;

/// `"success"`, `"failed"` or `"timeout"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Success,
    Failed,
    Timeout,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Success => "success",
            AgentStatus::Failed => "failed",
            AgentStatus::Timeout => "timeout",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentResult {
    pub status: AgentStatus,
    pub output: String,
    pub exit_code: Option<i32>,
    pub duration_ms: i64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub estimated_cost_usd: Option<f64>,
}

impl AgentResult {
    pub fn failed(output: impl Into<String>, exit_code: Option<i32>, duration_ms: i64) -> Self {
        Self {
            status: AgentStatus::Failed,
            output: output.into(),
            exit_code,
            duration_ms,
            input_tokens: None,
            output_tokens: None,
            estimated_cost_usd: None,
        }
    }
}

/// Everything about one call except the prompt.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub cwd: PathBuf,
    pub timeout_seconds: f64,
    /// `--allowedTools` value (claude only).
    pub allowed_tools: Option<String>,
    /// `--disallowedTools` value (claude only).
    pub disallowed_tools: Option<String>,
    pub add_dirs: Vec<PathBuf>,
    /// `"json"` (default) or `"text"`; only claude looks at it.
    pub output_format: String,
    /// `--max-budget-usd` cap (claude only).
    pub max_budget_usd: Option<f64>,
}

impl RunOptions {
    pub fn new(cwd: &Path, timeout_seconds: f64) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            timeout_seconds,
            allowed_tools: None,
            disallowed_tools: None,
            add_dirs: Vec::new(),
            output_format: "json".into(),
            max_budget_usd: None,
        }
    }
}

pub trait Agent: Send {
    fn run(&self, prompt: &str, opts: &RunOptions) -> AgentResult;
}

/// Python's `str(number)` for a timeout: `900`, `0.5`.
pub fn fmt_seconds(s: f64) -> String {
    if s.fract() == 0.0 && s.abs() < 1e15 {
        format!("{}", s as i64)
    } else {
        format!("{s}")
    }
}

/// `(status, reason)` for a timed-out or non-zero-exit child.
pub fn classify_subprocess_error(out: &ProcOutput, timeout_seconds: f64) -> (AgentStatus, String) {
    if out.timed_out {
        (
            AgentStatus::Timeout,
            format!("timed out after {}s", fmt_seconds(timeout_seconds)),
        )
    } else {
        (
            AgentStatus::Failed,
            format!("exited {}", out.code.unwrap_or(-1)),
        )
    }
}

/// The result for a failed/timed-out child: the reason, then any captured
/// stderr (else stdout), so a timeout isn't reported with no detail.
pub fn failure_result(out: &ProcOutput, timeout_seconds: f64, duration_ms: i64) -> AgentResult {
    let (status, reason) = classify_subprocess_error(out, timeout_seconds);
    let detail = if out.stderr.is_empty() {
        &out.stdout
    } else {
        &out.stderr
    };
    AgentResult {
        status,
        output: if detail.is_empty() {
            reason
        } else {
            format!("{reason}\n{detail}")
        },
        exit_code: out.code,
        duration_ms,
        input_tokens: None,
        output_tokens: None,
        estimated_cost_usd: None,
    }
}

/// Run `program args` in `opts.cwd` and fold the outcome into an
/// [`AgentResult`]. `extract` turns stdout into the reply text; `None` on exit 0
/// is reported as "unreadable output from <cli>". These CLIs emit no cost.
pub(crate) fn run_text_cli(
    cli: &str,
    program: &str,
    args: Vec<String>,
    opts: &RunOptions,
    extract: &dyn Fn(&str) -> Option<String>,
) -> AgentResult {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).current_dir(&opts.cwd);
    let start = std::time::Instant::now();
    let ms = |s: std::time::Instant| i64::try_from(s.elapsed().as_millis()).unwrap_or(i64::MAX);
    let out = match crate::proc::run(
        &mut cmd,
        None,
        Some(std::time::Duration::from_secs_f64(opts.timeout_seconds)),
    ) {
        Ok(o) => o,
        Err(e) => {
            return AgentResult::failed(format!("cannot run {program}: {e}"), None, ms(start))
        }
    };
    if out.timed_out || out.code != Some(0) {
        return failure_result(&out, opts.timeout_seconds, ms(start));
    }
    match extract(&out.stdout) {
        Some(text) => AgentResult {
            status: AgentStatus::Success,
            output: text,
            exit_code: Some(0),
            duration_ms: ms(start),
            input_tokens: None,
            output_tokens: None,
            estimated_cost_usd: None,
        },
        None => AgentResult::failed(
            format!("unreadable output from {cli}\n{}", out.stdout),
            Some(0),
            ms(start),
        ),
    }
}

/// The string field `key` of the JSON object in `stdout`.
pub(crate) fn json_string_field(stdout: &str, key: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    v.get(key)?.as_str().map(String::from)
}

/// The adapter for `provider`; `None` / `"default"` resolve through
/// `ai_cli.json` (`config_path`, or the per-machine default).
pub fn get_agent(
    provider: Option<&str>,
    config_path: Option<&Path>,
) -> Result<Box<dyn Agent>, String> {
    let provider = match provider {
        None | Some("default") => match config_path {
            Some(p) => ai_cli_config::get_selected_cli(p),
            None => ai_cli_config::selected_cli(),
        },
        Some(p) => p.to_string(),
    };
    match provider.as_str() {
        "claude" => Ok(Box::new(claude::ClaudeAgent::default())),
        "codex" => Ok(Box::new(codex::CodexAgent::default())),
        "gemini" => Ok(Box::new(gemini::GeminiAgent::default())),
        "kimi" => Ok(Box::new(kimi::KimiAgent::default())),
        "cursor" => Ok(Box::new(cursor::CursorAgent::default())),
        other => Err(format!(
            "Unknown agent provider: {}",
            crate::loops_config::py_repr_str(other)
        )),
    }
}

/// Python `json.dumps` escaping of non-ASCII (`ensure_ascii=True`) applied to
/// compact serde output.
pub(crate) fn ascii_json(value: &serde_json::Value) -> String {
    let text = serde_json::to_string(value).unwrap_or_default();
    let mut out = String::with_capacity(text.len());
    let mut buf = [0u16; 2];
    for ch in text.chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else {
            for unit in ch.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// Write an executable shell script `name` into `dir`.
    pub fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(code: Option<i32>, timed_out: bool, stdout: &str, stderr: &str) -> ProcOutput {
        ProcOutput {
            code,
            stdout: stdout.into(),
            stderr: stderr.into(),
            timed_out,
        }
    }

    #[test]
    fn classify_reports_timeout_and_exit() {
        let (s, r) = classify_subprocess_error(&out(None, true, "", ""), 900.0);
        assert_eq!(
            (s, r.as_str()),
            (AgentStatus::Timeout, "timed out after 900s")
        );
        let (s, r) = classify_subprocess_error(&out(Some(1), false, "", ""), 900.0);
        assert_eq!((s, r.as_str()), (AgentStatus::Failed, "exited 1"));
        assert_eq!(fmt_seconds(0.5), "0.5");
    }

    #[test]
    fn failure_result_folds_reason_and_detail() {
        let r = failure_result(&out(Some(2), false, "so", "boom"), 5.0, 7);
        assert_eq!(r.output, "exited 2\nboom");
        assert_eq!((r.exit_code, r.duration_ms), (Some(2), 7));
        let r = failure_result(&out(Some(2), false, "so", ""), 5.0, 7);
        assert_eq!(r.output, "exited 2\nso");
        let r = failure_result(&out(None, true, "", ""), 5.0, 7);
        assert_eq!(
            (r.status, r.output.as_str(), r.exit_code),
            (AgentStatus::Timeout, "timed out after 5s", None)
        );
    }

    #[test]
    fn get_agent_resolves_provider_and_default() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("ai_cli.json");
        std::fs::write(&cfg, r#"{"cli": "codex"}"#).unwrap();
        assert!(get_agent(None, Some(&cfg)).is_ok());
        assert!(get_agent(Some("default"), Some(&cfg)).is_ok());
        assert!(get_agent(Some("claude"), Some(&cfg)).is_ok());
        assert_eq!(
            get_agent(Some("nope"), Some(&cfg)).err().unwrap(),
            "Unknown agent provider: 'nope'"
        );
    }

    #[test]
    fn ascii_json_escapes_like_python() {
        assert_eq!(
            ascii_json(&serde_json::json!(["/a/é", "b"])),
            "[\"/a/\\u00e9\",\"b\"]"
        );
    }
}
