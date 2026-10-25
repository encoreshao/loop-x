//! `CursorAgent`: `agent --print --force --output-format json --workspace <cwd>
//! <prompt>`. The reply is the `result` field of Cursor's JSON;
//! `estimated_cost_usd` is always `None`.

use super::{json_string_field, run_text_cli, Agent, AgentResult, RunOptions};
use crate::ai_cli_config;
use crate::doctor::find_on_path;

pub const CURSOR_PRINT_FLAG: &str = "--print";
pub const CURSOR_APPROVAL_FLAG: &str = "--force";
pub const CURSOR_OUTPUT_FORMAT_FLAG: &str = "--output-format";
pub const CURSOR_OUTPUT_FORMAT: &str = "json";
pub const CURSOR_WORKSPACE_FLAG: &str = "--workspace";

pub struct CursorAgent {
    pub program: String,
}

impl Default for CursorAgent {
    /// `agent` or `cursor-agent`, whichever is on `PATH`; `agent` otherwise.
    fn default() -> Self {
        let path = std::env::var("PATH").unwrap_or_default();
        let program =
            ai_cli_config::resolve_binary("cursor", &|n| find_on_path(n, &path).is_some())
                .unwrap_or_else(|| "agent".into());
        Self { program }
    }
}

/// The argv after the program name.
pub fn cursor_args(prompt: &str, opts: &RunOptions) -> Vec<String> {
    vec![
        CURSOR_PRINT_FLAG.into(),
        CURSOR_APPROVAL_FLAG.into(),
        CURSOR_OUTPUT_FORMAT_FLAG.into(),
        CURSOR_OUTPUT_FORMAT.into(),
        CURSOR_WORKSPACE_FLAG.into(),
        opts.cwd.display().to_string(),
        prompt.to_string(),
    ]
}

/// The `result` field of Cursor's JSON output.
pub fn cursor_text(stdout: &str) -> Option<String> {
    json_string_field(stdout, "result")
}

impl Agent for CursorAgent {
    fn run(&self, prompt: &str, opts: &RunOptions) -> AgentResult {
        run_text_cli(
            "cursor",
            &self.program,
            cursor_args(prompt, opts),
            opts,
            &cursor_text,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::script;
    use super::super::{get_agent, Agent, AgentStatus, RunOptions};
    use super::*;

    #[test]
    fn args_have_print_force_json_and_workspace() {
        let o = RunOptions::new(std::path::Path::new("/ws"), 5.0);
        assert_eq!(
            cursor_args("go", &o),
            [
                "--print",
                "--force",
                "--output-format",
                "json",
                "--workspace",
                "/ws",
                "go"
            ]
        );
    }

    #[test]
    fn text_extracts_result() {
        assert_eq!(
            cursor_text(r#"{"type":"result","result":"hi"}"#).as_deref(),
            Some("hi")
        );
        assert_eq!(cursor_text("not json"), None);
        assert_eq!(cursor_text(r#"{"is_error":true}"#), None);
    }

    #[test]
    fn success_failure_timeout_and_missing() {
        let d = tempfile::tempdir().unwrap();
        let mk = |name: &str, body: &str| CursorAgent {
            program: script(d.path(), name, body).display().to_string(),
        };
        let r = mk("c1", r#"echo '{"result":"done"}'"#).run("p", &RunOptions::new(d.path(), 30.0));
        assert_eq!(
            (r.status, r.output.as_str(), r.estimated_cost_usd),
            (AgentStatus::Success, "done", None)
        );
        let r = mk("c2", "echo plain").run("p", &RunOptions::new(d.path(), 30.0));
        assert_eq!(r.status, AgentStatus::Failed);
        assert!(r.output.contains("unreadable output from cursor"));
        assert!(r.output.contains("plain"));
        let r = mk("c3", "echo nope >&2; exit 3").run("p", &RunOptions::new(d.path(), 30.0));
        assert_eq!((r.status, r.exit_code), (AgentStatus::Failed, Some(3)));
        assert!(r.output.contains("nope"));
        assert_eq!(
            mk("c4", "exec sleep 5")
                .run("p", &RunOptions::new(d.path(), 0.3))
                .status,
            AgentStatus::Timeout
        );
        let a = CursorAgent {
            program: "/nonexistent/agent-bin".into(),
        };
        let r = a.run("p", &RunOptions::new(d.path(), 5.0));
        assert_eq!(r.status, AgentStatus::Failed);
        assert!(r.output.contains("/nonexistent/agent-bin"));
    }

    #[test]
    fn get_agent_resolves_cursor() {
        assert!(get_agent(Some("cursor"), None).is_ok());
    }
}
