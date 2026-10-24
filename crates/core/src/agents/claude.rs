//! `ClaudeAgent`: `claude -p ... --output-format <fmt> <prompt>` (mirrors
//! `bin/agents/claude.py`).

use super::{failure_result, Agent, AgentResult, AgentStatus, RunOptions};
use crate::cost;
use crate::proc;
use serde_json::Value;
use std::process::Command;
use std::time::{Duration, Instant};

pub struct ClaudeAgent {
    /// The executable to run (`claude`; tests point it at a fake script).
    pub program: String,
}

impl Default for ClaudeAgent {
    fn default() -> Self {
        Self {
            program: "claude".into(),
        }
    }
}

/// The argv after the program name.
pub fn claude_args(prompt: &str, opts: &RunOptions) -> Vec<String> {
    let mut args = vec!["-p".to_string()];
    for dir in &opts.add_dirs {
        args.push("--add-dir".into());
        args.push(dir.display().to_string());
    }
    args.extend(["--permission-mode".to_string(), "acceptEdits".to_string()]);
    if let Some(t) = &opts.allowed_tools {
        args.extend(["--allowedTools".to_string(), t.clone()]);
    }
    if let Some(t) = &opts.disallowed_tools {
        args.extend(["--disallowedTools".to_string(), t.clone()]);
    }
    args.extend(cost::budget_args(opts.max_budget_usd));
    args.extend([
        "--output-format".to_string(),
        opts.output_format.clone(),
        prompt.to_string(),
    ]);
    args
}

/// Last 800 characters, like `proc.stdout[-800:]`.
fn tail_800(s: &str) -> String {
    let n = s.chars().count();
    s.chars().skip(n.saturating_sub(800)).collect()
}

fn int(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_i64)
}

impl Agent for ClaudeAgent {
    fn run(&self, prompt: &str, opts: &RunOptions) -> AgentResult {
        let mut cmd = Command::new(&self.program);
        cmd.args(claude_args(prompt, opts)).current_dir(&opts.cwd);
        let start = Instant::now();
        let ms = |s: Instant| i64::try_from(s.elapsed().as_millis()).unwrap_or(i64::MAX);
        let out = match proc::run(
            &mut cmd,
            None,
            Some(Duration::from_secs_f64(opts.timeout_seconds)),
        ) {
            Ok(o) => o,
            Err(e) => {
                return AgentResult::failed(
                    format!("cannot run {}: {e}", self.program),
                    None,
                    ms(start),
                )
            }
        };
        if out.timed_out || out.code != Some(0) {
            return failure_result(&out, opts.timeout_seconds, ms(start));
        }
        let duration_ms = ms(start);

        if opts.output_format != "json" {
            return AgentResult {
                status: AgentStatus::Success,
                output: out.stdout,
                exit_code: Some(0),
                duration_ms,
                input_tokens: None,
                output_tokens: None,
                estimated_cost_usd: None,
            };
        }
        let parsed: Option<Value> = if out.stdout.is_empty() {
            None
        } else {
            match serde_json::from_str::<Value>(&out.stdout) {
                Ok(v) if v.is_object() => Some(v),
                _ => return AgentResult::failed(tail_800(&out.stdout), Some(0), duration_ms),
            }
        };
        // Python's `if parsed`: an empty object counts as no result.
        let parsed = parsed.filter(|v| v.as_object().is_some_and(|m| !m.is_empty()));
        let output = parsed.as_ref().map_or_else(
            || "(no result text in CLI output)".to_string(),
            cost::extract_result_text,
        );
        let usage = parsed.as_ref().and_then(cost::extract_claude_usage);
        AgentResult {
            status: AgentStatus::Success,
            output,
            exit_code: Some(0),
            duration_ms,
            input_tokens: int(usage.as_ref().and_then(|u| u.get("input_tokens"))),
            output_tokens: int(usage.as_ref().and_then(|u| u.get("output_tokens"))),
            estimated_cost_usd: usage
                .as_ref()
                .and_then(|u| u.get("cost_usd"))
                .and_then(Value::as_f64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::script;
    use super::*;
    use std::path::PathBuf;

    fn agent(dir: &std::path::Path, body: &str) -> ClaudeAgent {
        ClaudeAgent {
            program: script(dir, "claude", body).display().to_string(),
        }
    }

    fn opts(dir: &std::path::Path, secs: f64, fmt: &str) -> RunOptions {
        let mut o = RunOptions::new(dir, secs);
        o.output_format = fmt.into();
        o
    }

    #[test]
    fn extracts_json_result_and_cost() {
        let d = tempfile::tempdir().unwrap();
        let a = agent(
            d.path(),
            r#"echo '{"result":"Fixed the issue.","total_cost_usd":0.42,"usage":{"input_tokens":100,"output_tokens":50},"modelUsage":{"claude-sonnet":{}}}'"#,
        );
        let r = a.run("do the thing", &opts(d.path(), 30.0, "json"));
        assert_eq!(r.status, AgentStatus::Success);
        assert_eq!(r.output, "Fixed the issue.");
        assert_eq!(
            (r.exit_code, r.input_tokens, r.output_tokens),
            (Some(0), Some(100), Some(50))
        );
        assert_eq!(r.estimated_cost_usd, Some(0.42));
    }

    #[test]
    fn text_format_has_no_cost() {
        let d = tempfile::tempdir().unwrap();
        let r =
            agent(d.path(), "printf 'plain text result'").run("p", &opts(d.path(), 30.0, "text"));
        assert_eq!(
            (r.status, r.output.as_str(), r.estimated_cost_usd),
            (AgentStatus::Success, "plain text result", None)
        );
    }

    #[test]
    fn nonzero_exit_is_failed_with_stderr_detail() {
        let d = tempfile::tempdir().unwrap();
        let r = agent(d.path(), "echo boom >&2; exit 1").run("p", &opts(d.path(), 30.0, "text"));
        assert_eq!((r.status, r.exit_code), (AgentStatus::Failed, Some(1)));
        assert!(r.output.contains("boom"));
    }

    #[test]
    fn non_json_or_non_object_stdout_is_failed() {
        let d = tempfile::tempdir().unwrap();
        let r = agent(d.path(), "echo 'warning: something happened'")
            .run("p", &opts(d.path(), 30.0, "json"));
        assert_eq!((r.status, r.exit_code), (AgentStatus::Failed, Some(0)));
        assert!(r.output.contains("warning: something happened"));
        let r = agent(d.path(), "printf 123").run("p", &opts(d.path(), 30.0, "json"));
        assert_eq!((r.status, r.exit_code), (AgentStatus::Failed, Some(0)));
    }

    #[test]
    fn empty_stdout_is_a_success_with_placeholder() {
        let d = tempfile::tempdir().unwrap();
        let r = agent(d.path(), "true").run("p", &opts(d.path(), 30.0, "json"));
        assert_eq!(
            (r.status, r.output.as_str()),
            (AgentStatus::Success, "(no result text in CLI output)")
        );
    }

    #[test]
    fn timeout_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let r = agent(d.path(), "exec sleep 5").run("p", &opts(d.path(), 0.3, "text"));
        assert_eq!(r.status, AgentStatus::Timeout);
        assert!(r.output.contains("timed out after 0.3s"));
    }

    #[test]
    fn missing_binary_is_a_failed_result() {
        let d = tempfile::tempdir().unwrap();
        let a = ClaudeAgent {
            program: "/nonexistent/claude".into(),
        };
        assert_eq!(
            a.run("p", &opts(d.path(), 5.0, "json")).status,
            AgentStatus::Failed
        );
    }

    #[test]
    fn passes_add_dirs_tool_flags_and_prompt_last() {
        let d = tempfile::tempdir().unwrap();
        let argv = d.path().join("argv.txt");
        let a = agent(
            d.path(),
            &format!("printf '%s\\n' \"$@\" > {}\necho '{{}}'", argv.display()),
        );
        let mut o = opts(d.path(), 30.0, "json");
        o.allowed_tools = Some("Read Edit".into());
        o.disallowed_tools = Some("Bash(git merge*)".into());
        o.add_dirs = vec![PathBuf::from("/r/repo"), PathBuf::from("/r/wt")];
        a.run("a prompt", &o);
        let lines: Vec<String> = std::fs::read_to_string(&argv)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(
            lines,
            [
                "-p",
                "--add-dir",
                "/r/repo",
                "--add-dir",
                "/r/wt",
                "--permission-mode",
                "acceptEdits",
                "--allowedTools",
                "Read Edit",
                "--disallowedTools",
                "Bash(git merge*)",
                "--output-format",
                "json",
                "a prompt"
            ]
        );
    }

    #[test]
    fn budget_flag_only_when_capped() {
        let d = tempfile::tempdir().unwrap();
        let mut o = opts(d.path(), 5.0, "json");
        assert!(!claude_args("p", &o).contains(&"--max-budget-usd".to_string()));
        o.max_budget_usd = Some(0.5);
        let args = claude_args("p", &o);
        assert_eq!(
            args[args.iter().position(|a| a == "--max-budget-usd").unwrap() + 1],
            "0.50"
        );
    }
}
