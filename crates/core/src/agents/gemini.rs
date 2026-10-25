//! `GeminiAgent`: `gemini --output-format json --yolo -p <prompt>`. The reply is
//! the `response` field of Gemini's JSON; `estimated_cost_usd` is always `None`.

use super::{
    error_message, json_string_field, non_empty_reply, run_text_cli, Agent, AgentResult, RunOptions,
};
use crate::runner_util::unreadable_output;

pub const GEMINI_PROMPT_FLAG: &str = "-p";
pub const GEMINI_OUTPUT_FORMAT_FLAG: &str = "--output-format";
pub const GEMINI_OUTPUT_FORMAT: &str = "json";
pub const GEMINI_APPROVAL_FLAG: &str = "--yolo";
pub const GEMINI_INCLUDE_DIRS_FLAG: &str = "--include-directories";

pub struct GeminiAgent {
    pub program: String,
}

impl Default for GeminiAgent {
    fn default() -> Self {
        Self {
            program: "gemini".into(),
        }
    }
}

/// The argv after the program name.
pub fn gemini_args(prompt: &str, opts: &RunOptions) -> Vec<String> {
    let mut args: Vec<String> = [
        GEMINI_OUTPUT_FORMAT_FLAG,
        GEMINI_OUTPUT_FORMAT,
        GEMINI_APPROVAL_FLAG,
    ]
    .map(String::from)
    .to_vec();
    if !opts.add_dirs.is_empty() {
        let dirs: Vec<String> = opts
            .add_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect();
        args.push(GEMINI_INCLUDE_DIRS_FLAG.into());
        args.push(dirs.join(","));
    }
    args.push(GEMINI_PROMPT_FLAG.into());
    args.push(prompt.to_string());
    args
}

/// The `response` field of Gemini's JSON output.
pub fn gemini_text(stdout: &str) -> Option<String> {
    json_string_field(stdout, "response")
}

/// The reply, or the failure message: an `error` field (Gemini can exit 0 with
/// one), unparseable output, or an empty reply.
pub fn gemini_reply(stdout: &str) -> Result<String, String> {
    if let Some(error) = serde_json::from_str::<serde_json::Value>(stdout.trim())
        .ok()
        .and_then(|v| v.get("error").cloned())
    {
        return Err(error_message(&error));
    }
    match gemini_text(stdout) {
        Some(text) => non_empty_reply("gemini", text),
        None => Err(unreadable_output("gemini", stdout)),
    }
}

impl Agent for GeminiAgent {
    fn run(&self, prompt: &str, opts: &RunOptions) -> AgentResult {
        run_text_cli(
            &self.program,
            gemini_args(prompt, opts),
            opts,
            &gemini_reply,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::script;
    use super::super::{get_agent, Agent, AgentStatus, RunOptions};
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn args_have_headless_yolo_json_and_dirs() {
        let mut o = RunOptions::new(std::path::Path::new("."), 5.0);
        assert_eq!(
            gemini_args("go", &o),
            ["--output-format", "json", "--yolo", "-p", "go"]
        );
        o.add_dirs = vec![PathBuf::from("/r"), PathBuf::from("/w")];
        let args = gemini_args("go", &o);
        let i = args
            .iter()
            .position(|a| a == "--include-directories")
            .unwrap();
        assert_eq!(args[i + 1], "/r,/w");
        assert_eq!(args.last().unwrap(), "go");
    }

    #[test]
    fn text_extracts_response() {
        assert_eq!(
            gemini_text(r#"{"response":"hi","stats":{}}"#).as_deref(),
            Some("hi")
        );
        assert_eq!(gemini_text("not json"), None);
        assert_eq!(gemini_text(r#"{"error":{"message":"x"}}"#), None);
    }

    #[test]
    fn success_failure_timeout_and_missing() {
        let d = tempfile::tempdir().unwrap();
        let mk = |name: &str, body: &str| GeminiAgent {
            program: script(d.path(), name, body).display().to_string(),
        };
        let r =
            mk("g1", r#"echo '{"response":"done"}'"#).run("p", &RunOptions::new(d.path(), 30.0));
        assert_eq!(
            (r.status, r.output.as_str(), r.estimated_cost_usd),
            (AgentStatus::Success, "done", None)
        );
        let r = mk("g2", "echo plain").run("p", &RunOptions::new(d.path(), 30.0));
        assert_eq!(r.status, AgentStatus::Failed);
        assert!(r.output.contains("unreadable output from gemini"));
        assert!(r.output.contains("plain"));
        let r = mk("g3", "echo nope >&2; exit 3").run("p", &RunOptions::new(d.path(), 30.0));
        assert_eq!((r.status, r.exit_code), (AgentStatus::Failed, Some(3)));
        assert!(r.output.contains("nope"));
        assert_eq!(
            mk("g4", "exec sleep 5")
                .run("p", &RunOptions::new(d.path(), 0.3))
                .status,
            AgentStatus::Timeout
        );
        let a = GeminiAgent {
            program: "/nonexistent/gemini-bin".into(),
        };
        let r = a.run("p", &RunOptions::new(d.path(), 5.0));
        assert_eq!(r.status, AgentStatus::Failed);
        assert!(r.output.contains("/nonexistent/gemini-bin"));
    }

    #[test]
    fn error_field_and_empty_text_are_visible_failures() {
        let d = tempfile::tempdir().unwrap();
        let mk = |name: &str, body: &str| GeminiAgent {
            program: script(d.path(), name, body).display().to_string(),
        };
        let r = mk("g5", r#"echo '{"error":{"message":"quota exceeded"}}'"#)
            .run("p", &RunOptions::new(d.path(), 30.0));
        assert_eq!((r.status, r.exit_code), (AgentStatus::Failed, Some(0)));
        assert!(r.output.contains("quota exceeded"), "{}", r.output);
        assert!(!r.output.contains("unreadable"), "{}", r.output);
        let r = mk("g6", r#"echo '{"response":"   "}'"#).run("p", &RunOptions::new(d.path(), 30.0));
        assert_eq!(r.status, AgentStatus::Failed);
        assert!(
            r.output.contains("empty output from gemini"),
            "{}",
            r.output
        );
    }

    #[test]
    fn get_agent_resolves_gemini() {
        assert!(get_agent(Some("gemini"), None).is_ok());
    }
}
