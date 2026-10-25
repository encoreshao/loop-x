//! `KimiAgent`: `kimi --output-format text --yolo -p <prompt>`. The reply is
//! stdout as is; `estimated_cost_usd` is always `None`.

use super::{non_empty_reply, run_text_cli, Agent, AgentResult, RunOptions};

pub const KIMI_PROMPT_FLAG: &str = "-p";
pub const KIMI_OUTPUT_FORMAT_FLAG: &str = "--output-format";
pub const KIMI_OUTPUT_FORMAT: &str = "text";
pub const KIMI_APPROVAL_FLAG: &str = "--yolo";

pub struct KimiAgent {
    pub program: String,
}

impl Default for KimiAgent {
    fn default() -> Self {
        Self {
            program: "kimi".into(),
        }
    }
}

/// The argv after the program name.
pub fn kimi_args(prompt: &str, _opts: &RunOptions) -> Vec<String> {
    [
        KIMI_OUTPUT_FORMAT_FLAG,
        KIMI_OUTPUT_FORMAT,
        KIMI_APPROVAL_FLAG,
        KIMI_PROMPT_FLAG,
        prompt,
    ]
    .map(String::from)
    .to_vec()
}

/// Stdout as the reply; whitespace-only is a failure.
pub fn kimi_reply(stdout: &str) -> Result<String, String> {
    non_empty_reply("kimi", stdout.to_string())
}

impl Agent for KimiAgent {
    fn run(&self, prompt: &str, opts: &RunOptions) -> AgentResult {
        run_text_cli(&self.program, kimi_args(prompt, opts), opts, &kimi_reply)
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::script;
    use super::super::{get_agent, Agent, AgentStatus, RunOptions};
    use super::*;

    #[test]
    fn args_have_headless_yolo_and_text_format() {
        let o = RunOptions::new(std::path::Path::new("."), 5.0);
        assert_eq!(
            kimi_args("go", &o),
            ["--output-format", "text", "--yolo", "-p", "go"]
        );
    }

    #[test]
    fn success_failure_timeout_and_missing() {
        let d = tempfile::tempdir().unwrap();
        let mk = |name: &str, body: &str| KimiAgent {
            program: script(d.path(), name, body).display().to_string(),
        };
        let r = mk("k1", "echo done").run("p", &RunOptions::new(d.path(), 30.0));
        assert_eq!(
            (r.status, r.output.as_str(), r.estimated_cost_usd),
            (AgentStatus::Success, "done\n", None)
        );
        let r = mk("k2", "echo nope >&2; exit 3").run("p", &RunOptions::new(d.path(), 30.0));
        assert_eq!((r.status, r.exit_code), (AgentStatus::Failed, Some(3)));
        assert!(r.output.contains("nope"));
        assert_eq!(
            mk("k3", "exec sleep 5")
                .run("p", &RunOptions::new(d.path(), 0.3))
                .status,
            AgentStatus::Timeout
        );
        let a = KimiAgent {
            program: "/nonexistent/kimi-bin".into(),
        };
        let r = a.run("p", &RunOptions::new(d.path(), 5.0));
        assert_eq!(r.status, AgentStatus::Failed);
        assert!(r.output.contains("/nonexistent/kimi-bin"));
    }

    #[test]
    fn empty_output_is_a_visible_failure() {
        let d = tempfile::tempdir().unwrap();
        let a = KimiAgent {
            program: script(d.path(), "k4", "printf '  \\n'")
                .display()
                .to_string(),
        };
        let r = a.run("p", &RunOptions::new(d.path(), 30.0));
        assert_eq!((r.status, r.exit_code), (AgentStatus::Failed, Some(0)));
        assert!(r.output.contains("empty output from kimi"), "{}", r.output);
    }

    #[test]
    fn get_agent_resolves_kimi() {
        assert!(get_agent(Some("kimi"), None).is_ok());
    }
}
