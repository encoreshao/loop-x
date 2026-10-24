//! `CodexAgent`: `codex exec --sandbox workspace-write ...` (mirrors
//! `bin/agents/codex.py`). `estimated_cost_usd` is always `None`: the Codex CLI
//! emits no structured cost.

use super::{ascii_json, failure_result, Agent, AgentResult, AgentStatus, RunOptions};
use crate::proc;
use serde_json::json;
use std::process::Command;
use std::time::{Duration, Instant};

pub struct CodexAgent {
    pub program: String,
}

impl Default for CodexAgent {
    fn default() -> Self {
        Self {
            program: "codex".into(),
        }
    }
}

/// The argv after the program name.
pub fn codex_args(prompt: &str, opts: &RunOptions) -> Vec<String> {
    let mut args: Vec<String> = ["exec", "--sandbox", "workspace-write"]
        .map(String::from)
        .to_vec();
    if !opts.add_dirs.is_empty() {
        let roots: Vec<String> = opts
            .add_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect();
        args.push("-c".into());
        args.push(format!(
            "sandbox_workspace_write.writable_roots={}",
            ascii_json(&json!(roots))
        ));
    }
    args.extend(
        [
            "-c",
            "approval_policy=never",
            "-c",
            "sandbox_workspace_write.network_access=true",
        ]
        .map(String::from),
    );
    args.push(prompt.to_string());
    args
}

impl Agent for CodexAgent {
    fn run(&self, prompt: &str, opts: &RunOptions) -> AgentResult {
        let mut cmd = Command::new(&self.program);
        cmd.args(codex_args(prompt, opts)).current_dir(&opts.cwd);
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
        AgentResult {
            status: AgentStatus::Success,
            output: out.stdout,
            exit_code: Some(0),
            duration_ms: ms(start),
            input_tokens: None,
            output_tokens: None,
            estimated_cost_usd: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::script;
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn command_without_and_with_writable_roots() {
        let o = RunOptions::new(std::path::Path::new("."), 5.0);
        assert_eq!(
            codex_args("go", &o),
            [
                "exec",
                "--sandbox",
                "workspace-write",
                "-c",
                "approval_policy=never",
                "-c",
                "sandbox_workspace_write.network_access=true",
                "go"
            ]
        );
        let mut o = o;
        o.add_dirs = vec![PathBuf::from("/r"), PathBuf::from("/w")];
        let args = codex_args("go", &o);
        assert_eq!(args[3], "-c");
        assert_eq!(
            args[4],
            r#"sandbox_workspace_write.writable_roots=["/r","/w"]"#
        );
        assert_eq!(args.last().unwrap(), "go");
    }

    #[test]
    fn success_returns_stdout_and_no_cost() {
        let d = tempfile::tempdir().unwrap();
        let a = CodexAgent {
            program: script(d.path(), "codex", "echo done").display().to_string(),
        };
        let r = a.run("p", &RunOptions::new(d.path(), 30.0));
        assert_eq!(
            (r.status, r.output.as_str(), r.estimated_cost_usd),
            (AgentStatus::Success, "done\n", None)
        );
    }

    #[test]
    fn failure_and_timeout() {
        let d = tempfile::tempdir().unwrap();
        let a = CodexAgent {
            program: script(d.path(), "codex", "echo nope >&2; exit 3")
                .display()
                .to_string(),
        };
        let r = a.run("p", &RunOptions::new(d.path(), 30.0));
        assert_eq!((r.status, r.exit_code), (AgentStatus::Failed, Some(3)));
        assert!(r.output.contains("nope"));
        let a = CodexAgent {
            program: script(d.path(), "codex2", "exec sleep 5")
                .display()
                .to_string(),
        };
        assert_eq!(
            a.run("p", &RunOptions::new(d.path(), 0.3)).status,
            AgentStatus::Timeout
        );
    }
}
