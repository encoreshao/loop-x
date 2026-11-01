//! `GrokAgent`: xAI's Grok Build CLI,
//! `grok --no-auto-update --output-format json --yolo -p <prompt>`. The reply is
//! the `text` field of Grok's single JSON object; `estimated_cost_usd` is always
//! `None` here (the runners read their own output).

use super::{
    error_message, json_string_field, non_empty_reply, run_text_cli, Agent, AgentResult, RunOptions,
};
use crate::runner_util::unreadable_output;

pub const GROK_PROMPT_FLAG: &str = "-p";
pub const GROK_OUTPUT_FORMAT_FLAG: &str = "--output-format";
pub const GROK_OUTPUT_FORMAT: &str = "json";
/// Auto-approves every tool call (Grok's alias for `--permission-mode bypassPermissions`).
pub const GROK_APPROVAL_FLAG: &str = "--yolo";
/// Skips the background update check, which a headless run should never do.
pub const GROK_NO_UPDATE_FLAG: &str = "--no-auto-update";
pub const GROK_PERMISSION_MODE_FLAG: &str = "--permission-mode";
/// Runs only pre-approved tools (plus Grok's built-in read-only handling) and
/// denies everything else; a headless call never waits for an approval.
pub const GROK_PERMISSION_MODE_DONT_ASK: &str = "dontAsk";
pub const GROK_ALLOW_FLAG: &str = "--allow";
pub const GROK_DENY_FLAG: &str = "--deny";
/// Every built-in tool, for a call that must use none: deny rules always win,
/// even over Grok's read-only handling.
pub const GROK_ALL_TOOLS: [&str; 9] = [
    "Bash",
    "Read",
    "Edit",
    "Write",
    "Grep",
    "Glob",
    "WebFetch",
    "WebSearch",
    "MCPTool",
];

pub struct GrokAgent {
    pub program: String,
}

impl Default for GrokAgent {
    fn default() -> Self {
        Self {
            program: "grok".into(),
        }
    }
}

/// The argv after the program name. Grok has no extra-directory flag: the run's
/// cwd is its workspace and `--yolo` lets it reach the worktree.
pub fn grok_args(prompt: &str, _opts: &RunOptions) -> Vec<String> {
    [
        GROK_NO_UPDATE_FLAG,
        GROK_OUTPUT_FORMAT_FLAG,
        GROK_OUTPUT_FORMAT,
        GROK_APPROVAL_FLAG,
        GROK_PROMPT_FLAG,
        prompt,
    ]
    .map(String::from)
    .to_vec()
}

/// `--deny <tool>` for every built-in tool.
pub fn grok_deny_all_tools() -> Vec<String> {
    GROK_ALL_TOOLS
        .iter()
        .flat_map(|t| [GROK_DENY_FLAG.to_string(), (*t).to_string()])
        .collect()
}

/// The `text` field of Grok's JSON output.
pub fn grok_text(stdout: &str) -> Option<String> {
    json_string_field(stdout, "text")
}

/// The reply, or the failure message: an `error` field, unparseable output, or
/// an empty reply.
pub fn grok_reply(stdout: &str) -> Result<String, String> {
    if let Some(error) = serde_json::from_str::<serde_json::Value>(stdout.trim())
        .ok()
        .and_then(|v| v.get("error").cloned())
        .filter(|e| !e.is_null())
    {
        return Err(error_message(&error));
    }
    match grok_text(stdout) {
        Some(text) => non_empty_reply("grok", text),
        None => Err(unreadable_output("grok", stdout)),
    }
}

impl Agent for GrokAgent {
    fn run(&self, prompt: &str, opts: &RunOptions) -> AgentResult {
        run_text_cli(&self.program, grok_args(prompt, opts), opts, &grok_reply)
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::script;
    use super::super::{get_agent, Agent, AgentStatus, RunOptions};
    use super::*;

    #[test]
    fn args_are_headless_json_yolo_without_update_checks() {
        let o = RunOptions::new(std::path::Path::new("."), 5.0);
        assert_eq!(
            grok_args("go", &o),
            [
                "--no-auto-update",
                "--output-format",
                "json",
                "--yolo",
                "-p",
                "go"
            ]
        );
    }

    #[test]
    fn deny_all_tools_denies_each_built_in_tool() {
        let args = grok_deny_all_tools();
        assert_eq!(args.len(), GROK_ALL_TOOLS.len() * 2);
        assert_eq!(&args[..4], ["--deny", "Bash", "--deny", "Read"]);
    }

    #[test]
    fn reply_is_the_text_field_and_errors_are_failures() {
        let ok = r#"{"text":"hi","stopReason":"end_turn","sessionId":"s","total_cost_usd":0.01}"#;
        assert_eq!(grok_reply(ok).as_deref(), Ok("hi"));
        assert_eq!(
            grok_reply(r#"{"error":{"message":"quota"}}"#).unwrap_err(),
            "quota"
        );
        assert!(grok_reply("plain")
            .unwrap_err()
            .contains("unreadable output from grok"));
        assert!(grok_reply(r#"{"text":"  "}"#)
            .unwrap_err()
            .contains("empty output from grok"));
    }

    #[test]
    fn success_failure_timeout_and_missing() {
        let d = tempfile::tempdir().unwrap();
        let mk = |name: &str, body: &str| GrokAgent {
            program: script(d.path(), name, body).display().to_string(),
        };
        let r = mk("g1", r#"echo '{"text":"done"}'"#).run("p", &RunOptions::new(d.path(), 30.0));
        assert_eq!(
            (r.status, r.output.as_str(), r.estimated_cost_usd),
            (AgentStatus::Success, "done", None)
        );
        let r = mk("g2", "echo nope >&2; exit 1").run("p", &RunOptions::new(d.path(), 30.0));
        assert_eq!((r.status, r.exit_code), (AgentStatus::Failed, Some(1)));
        assert!(r.output.contains("nope"));
        assert_eq!(
            mk("g3", "exec sleep 5")
                .run("p", &RunOptions::new(d.path(), 0.3))
                .status,
            AgentStatus::Timeout
        );
        let r = GrokAgent {
            program: "/nonexistent/grok-bin".into(),
        }
        .run("p", &RunOptions::new(d.path(), 5.0));
        assert_eq!(r.status, AgentStatus::Failed);
        assert!(r.output.contains("/nonexistent/grok-bin"));
    }

    #[test]
    fn get_agent_resolves_grok() {
        assert!(get_agent(Some("grok"), None).is_ok());
    }
}
