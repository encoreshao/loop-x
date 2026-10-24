//! The hardened, tool-less, MCP-less, non-persisting Claude call shared by every
//! loop that feeds untrusted content to the model (mirrors
//! `bin/agents/sealed.py`). Claude-only: codex always gives the model a shell
//! and records the prompt under `~/.codex/sessions/`, so it is refused.

use crate::ai_cli_config;
use crate::cost;
use crate::proc::{self, ProcOutput};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct SealedCallFailed(pub String);

#[derive(Debug, Clone, PartialEq)]
pub struct SealedResult {
    pub text: String,
    pub cost_usd: Option<f64>,
    pub duration_ms: i64,
}

/// No tools and no MCP servers (without `--strict-mcp-config` the user's own
/// claude.ai connectors, possibly including one able to deliver mail, would
/// load). The prompt goes on stdin so content never appears in argv / `ps`.
/// `--no-session-persistence` keeps the stdin prompt out of
/// `~/.claude/projects/...`; `disableAllHooks` stops the user's own Stop /
/// SessionEnd hooks from reading anything. There is deliberately no codex
/// command.
pub fn sealed_command(max_budget_usd: Option<f64>) -> Vec<String> {
    let mut cmd: Vec<String> = [
        "claude",
        "-p",
        "--output-format",
        "json",
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        r#"{"mcpServers": {}}"#,
        "--no-session-persistence",
        "--settings",
        r#"{"disableAllHooks": true}"#,
    ]
    .map(String::from)
    .to_vec();
    cmd.extend(cost::budget_args(max_budget_usd));
    cmd
}

type Runner<'a> = &'a dyn Fn(&[String], &str, f64, &Path) -> std::io::Result<ProcOutput>;

/// Injection points; `Default` is the production wiring.
#[derive(Default)]
pub struct SealedHooks<'a> {
    pub runner: Option<Runner<'a>>,
    pub cli_fn: Option<&'a dyn Fn() -> String>,
    pub command_fn: Option<&'a dyn Fn(Option<f64>) -> Vec<String>>,
}

fn real_runner(
    argv: &[String],
    input: &str,
    timeout: f64,
    cwd: &Path,
) -> std::io::Result<ProcOutput> {
    let (program, args) = argv.split_first().expect("non-empty command");
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(cwd);
    proc::run(
        &mut cmd,
        Some(input),
        Some(Duration::from_secs_f64(timeout)),
    )
}

/// A disposable scratch directory (never a repo, so no CLAUDE.md/auto-memory
/// loads), removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Self> {
        let p = std::env::temp_dir().join(format!("loop-sealed-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&p)?;
        Ok(Self(p))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run one sealed call. `log` receives only status/timing strings, never prompt
/// or response content.
pub fn sealed_call(
    prompt: &str,
    timeout_seconds: f64,
    log: &mut dyn FnMut(&str),
    max_budget_usd: Option<f64>,
    hooks: &SealedHooks<'_>,
) -> Result<SealedResult, SealedCallFailed> {
    let fail = |m: &str| Err(SealedCallFailed(m.into()));
    let cli = match hooks.cli_fn {
        Some(f) => f(),
        None => ai_cli_config::selected_cli(),
    };
    if cli != "claude" {
        return fail("sealed calls require the Claude CLI (codex gives the model a shell)");
    }
    let argv = match hooks.command_fn {
        Some(f) => f(max_budget_usd),
        None => sealed_command(max_budget_usd),
    };
    let started = Instant::now();
    let scratch =
        Scratch::new().map_err(|e| SealedCallFailed(format!("cannot create scratch dir: {e}")))?;
    let out = match hooks.runner {
        Some(r) => r(&argv, prompt, timeout_seconds, &scratch.0),
        None => real_runner(&argv, prompt, timeout_seconds, &scratch.0),
    };
    drop(scratch);
    let out = match out {
        Ok(o) => o,
        Err(_) => {
            log("FAILED (could not start claude)");
            return fail("claude could not be started");
        }
    };
    if out.timed_out {
        log(&format!(
            "FAILED (timed out after {}s)",
            super::fmt_seconds(timeout_seconds)
        ));
        return fail("claude call timed out");
    }
    if out.code != Some(0) {
        let code = out.code.unwrap_or(-1);
        log(&format!("FAILED (exited {code})"));
        return fail(&format!("claude exited {code}"));
    }
    let elapsed = started.elapsed();
    let envelope = match serde_json::from_str::<Value>(&out.stdout) {
        Ok(v) if v.is_object() => v,
        _ => {
            log("FAILED (unparseable CLI envelope)");
            return fail("claude returned an unparseable envelope");
        }
    };
    if envelope
        .get("is_error")
        .is_some_and(crate::loops_config::py_truthy)
    {
        log("FAILED (is_error)");
        return fail("claude reported an error");
    }
    log(&format!("ok ({:.1}s)", elapsed.as_secs_f64()));
    Ok(SealedResult {
        text: envelope
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        cost_usd: envelope.get("total_cost_usd").and_then(Value::as_f64),
        duration_ms: i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
    })
}

#[cfg(test)]
mod tests {
    use super::super::testutil::script;
    use super::*;
    use serde_json::json;

    fn ok_out(stdout: &str) -> std::io::Result<ProcOutput> {
        Ok(ProcOutput {
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
            timed_out: false,
        })
    }

    fn claude() -> String {
        "claude".into()
    }

    #[test]
    fn command_is_sealed() {
        let cmd = sealed_command(None);
        assert_eq!(
            cmd[cmd.iter().position(|a| a == "--tools").unwrap() + 1],
            ""
        );
        assert!(cmd.contains(&"--strict-mcp-config".to_string()));
        let mcp: Value =
            serde_json::from_str(&cmd[cmd.iter().position(|a| a == "--mcp-config").unwrap() + 1])
                .unwrap();
        assert_eq!(mcp, json!({"mcpServers": {}}));
        assert!(cmd.contains(&"--no-session-persistence".to_string()));
        let settings: Value =
            serde_json::from_str(&cmd[cmd.iter().position(|a| a == "--settings").unwrap() + 1])
                .unwrap();
        assert_eq!(settings, json!({"disableAllHooks": true}));
        assert!(!cmd.contains(&"--max-budget-usd".to_string()));
        let capped = sealed_command(Some(0.5));
        assert_eq!(
            capped[capped.iter().position(|a| a == "--max-budget-usd").unwrap() + 1],
            "0.50"
        );
    }

    fn call(
        stdout: &str,
        cli: &str,
        logs: &mut Vec<String>,
    ) -> Result<SealedResult, SealedCallFailed> {
        let runner = |argv: &[String], input: &str, _t: f64, cwd: &Path| {
            assert!(
                !argv.join(" ").contains("SECRET-BODY"),
                "prompt only on stdin"
            );
            assert_eq!(input, "SECRET-BODY");
            assert!(cwd
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("loop-sealed-"));
            assert!(cwd.is_dir());
            ok_out(stdout)
        };
        let cli = cli.to_string();
        let cli_fn = move || cli.clone();
        sealed_call(
            "SECRET-BODY",
            30.0,
            &mut |m| logs.push(m.to_string()),
            None,
            &SealedHooks {
                runner: Some(&runner),
                cli_fn: Some(&cli_fn),
                command_fn: None,
            },
        )
    }

    #[test]
    fn parses_envelope() {
        let mut logs = vec![];
        let r = call(
            &json!({"result": "ok", "total_cost_usd": 0.02}).to_string(),
            "claude",
            &mut logs,
        )
        .unwrap();
        assert_eq!((r.text.as_str(), r.cost_usd), ("ok", Some(0.02)));
    }

    #[test]
    fn refuses_codex_and_reports_is_error_and_garbage() {
        let mut logs = vec![];
        assert!(call("{}", "codex", &mut logs).is_err());
        assert_eq!(
            call(r#"{"is_error": true}"#, "claude", &mut logs)
                .unwrap_err()
                .0,
            "claude reported an error"
        );
        assert_eq!(
            call("nope", "claude", &mut logs).unwrap_err().0,
            "claude returned an unparseable envelope"
        );
        assert_eq!(
            call("[1]", "claude", &mut logs).unwrap_err().0,
            "claude returned an unparseable envelope"
        );
    }

    #[test]
    fn log_never_contains_content() {
        let mut logs = vec![];
        call(
            &json!({"result": "SECRET-ANSWER"}).to_string(),
            "claude",
            &mut logs,
        )
        .unwrap();
        assert!(!logs.is_empty() && logs.iter().all(|l| !l.contains("SECRET")));
        assert!(logs[0].starts_with("ok ("));
    }

    #[test]
    fn budget_reaches_command_fn_once() {
        let seen = std::cell::RefCell::new(vec![]);
        let command_fn = |b: Option<f64>| {
            seen.borrow_mut().push(b);
            vec!["claude".to_string()]
        };
        let runner = |_: &[String], _: &str, _: f64, _: &Path| ok_out(r#"{"result":"x"}"#);
        let cli_fn = claude;
        sealed_call(
            "p",
            5.0,
            &mut |_| {},
            Some(1.234),
            &SealedHooks {
                runner: Some(&runner),
                cli_fn: Some(&cli_fn),
                command_fn: Some(&command_fn),
            },
        )
        .unwrap();
        assert_eq!(*seen.borrow(), [Some(1.234)]);
    }

    #[test]
    fn nonzero_exit_and_timeout_fail_without_leaking() {
        let cli_fn = claude;
        let mut logs: Vec<String> = vec![];
        let exit = |_: &[String], _: &str, _: f64, _: &Path| {
            Ok(ProcOutput {
                code: Some(2),
                stdout: "SECRET".into(),
                stderr: "SECRET".into(),
                timed_out: false,
            })
        };
        let e = sealed_call(
            "p",
            5.0,
            &mut |m| logs.push(m.into()),
            None,
            &SealedHooks {
                runner: Some(&exit),
                cli_fn: Some(&cli_fn),
                command_fn: None,
            },
        )
        .unwrap_err();
        assert_eq!(e.0, "claude exited 2");
        let slow = |_: &[String], _: &str, _: f64, _: &Path| {
            Ok(ProcOutput {
                code: None,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: true,
            })
        };
        let e = sealed_call(
            "p",
            5.0,
            &mut |m| logs.push(m.into()),
            None,
            &SealedHooks {
                runner: Some(&slow),
                cli_fn: Some(&cli_fn),
                command_fn: None,
            },
        )
        .unwrap_err();
        assert_eq!(e.0, "claude call timed out");
        assert!(logs.iter().all(|l| !l.contains("SECRET")));
    }

    #[test]
    fn real_runner_pipes_stdin_to_a_fake_cli_in_a_scratch_cwd() {
        let d = tempfile::tempdir().unwrap();
        let fake = script(
            d.path(),
            "fakeclaude",
            r#"cat >/dev/null; printf '{"result":"%s"}' "$(basename "$PWD")""#,
        );
        let command_fn = move |_: Option<f64>| vec![fake.display().to_string()];
        let cli_fn = claude;
        let r = sealed_call(
            "hello",
            30.0,
            &mut |_| {},
            None,
            &SealedHooks {
                runner: None,
                cli_fn: Some(&cli_fn),
                command_fn: Some(&command_fn),
            },
        )
        .unwrap();
        assert!(r.text.starts_with("loop-sealed-"));
    }
}
