//! The hardened one-shot AI call shared by every loop that feeds untrusted
//! content to the model (mirrors `bin/agents/sealed.py`). Claude gets no tools,
//! no MCP servers and no session record. The other CLIs (codex, gemini, kimi,
//! cursor) get the most restrictive headless invocation each offers, but none can
//! be locked down that far, so [`crate::ai_cli_safety`] classes them
//! `Unrestricted` and callers surface its warning instead of refusing.

use super::{cursor, gemini, kimi};
use crate::ai_cli_config;
use crate::ai_cli_safety::{self, CallSite};
use crate::cost;
use crate::proc::{self, ProcOutput};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Why a sealed call failed. Every kind but [`SealedFailKind::Reply`] carries
/// only text loopx wrote itself (an exit code, a timeout, a spawn error), never
/// the CLI's stdout or stderr, so callers may persist it; a `Reply` message can
/// quote the CLI's output, which can quote the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealedFailKind {
    NotStarted,
    TimedOut,
    Exited,
    Reply,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct SealedCallFailed(pub String, pub SealedFailKind);

#[derive(Debug, Clone, PartialEq)]
pub struct SealedResult {
    pub text: String,
    pub cost_usd: Option<f64>,
    pub duration_ms: i64,
}

/// The failure text a caller may persist or show: loopx's own text for every
/// kind except `Reply`, whose message can quote CLI output (and so the prompt);
/// that becomes a fixed line naming the CLI.
pub fn storable_message(cli: &str, e: &SealedCallFailed) -> String {
    match e.1 {
        SealedFailKind::Reply => format!("{cli} returned an unusable reply"),
        _ => e.0.clone(),
    }
}

/// No tools and no MCP servers (without `--strict-mcp-config` the user's own
/// claude.ai connectors, possibly including one able to deliver mail, would
/// load). The prompt goes on stdin so content never appears in argv / `ps`.
/// `--no-session-persistence` keeps the stdin prompt out of
/// `~/.claude/projects/...`; `disableAllHooks` stops the user's own Stop /
/// SessionEnd hooks from reading anything.
pub fn sealed_command(max_budget_usd: Option<f64>) -> Vec<String> {
    sealed_command_for("claude", max_budget_usd)
}

/// Kimi's non-interactive mode switch (its adapter passes the prompt with `-p`;
/// here the prompt arrives on stdin instead).
pub(crate) const KIMI_PRINT_FLAG: &str = "--print";
/// Gemini's approval mode that never auto-approves a tool (no `--yolo`).
pub(crate) const GEMINI_APPROVAL_MODE_FLAG: &str = "--approval-mode";
pub(crate) const GEMINI_APPROVAL_MODE_DEFAULT: &str = "default";
/// Fixed text appended to Gemini's stdin prompt, which `-p` requires.
const GEMINI_STDIN_INSTRUCTION: &str = "Follow the instructions above.";
/// Cursor's read-only Q&A mode.
pub(crate) const CURSOR_MODE_FLAG: &str = "--mode";
pub(crate) const CURSOR_MODE_ASK: &str = "ask";

/// Whether the prompt has to ride in argv (visible in `ps`) because the CLI has
/// no way to read it from stdin. Only Cursor Agent: `agent --print` takes the
/// prompt as a positional argument and documents no stdin or prompt-file input.
pub fn prompt_in_argv(cli: &str) -> bool {
    cli == "cursor"
}

/// The sealed command (program first) for `cli`. The Claude arm is the original
/// hardened command. The others use read-only / ask / default-approval modes with
/// no session persistence where the CLI has such an option, and read the prompt
/// from stdin unless [`prompt_in_argv`]; `max_budget_usd` only applies to Claude
/// (the others report no cost).
pub fn sealed_command_for(cli: &str, max_budget_usd: Option<f64>) -> Vec<String> {
    let strs = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match cli {
        "gemini" => {
            let mut cmd = strs(&["gemini"]);
            cmd.extend(strs(&[
                gemini::GEMINI_OUTPUT_FORMAT_FLAG,
                gemini::GEMINI_OUTPUT_FORMAT,
                GEMINI_APPROVAL_MODE_FLAG,
                GEMINI_APPROVAL_MODE_DEFAULT,
                gemini::GEMINI_PROMPT_FLAG,
                GEMINI_STDIN_INSTRUCTION,
            ]));
            cmd
        }
        "kimi" => strs(&[
            "kimi",
            KIMI_PRINT_FLAG,
            kimi::KIMI_OUTPUT_FORMAT_FLAG,
            kimi::KIMI_OUTPUT_FORMAT,
        ]),
        "cursor" => {
            let mut cmd = vec![cursor::CursorAgent::default().program];
            cmd.extend(strs(&[
                cursor::CURSOR_PRINT_FLAG,
                CURSOR_MODE_FLAG,
                CURSOR_MODE_ASK,
                cursor::CURSOR_OUTPUT_FORMAT_FLAG,
                cursor::CURSOR_OUTPUT_FORMAT,
            ]));
            cmd
        }
        // `-` makes `codex exec` read the prompt from stdin; the scratch dir is
        // not a repo. Codex records the prompt under ~/.codex/sessions (the
        // safety warning says so); this Codex has no flag to stop that.
        "codex" => strs(&[
            "codex",
            "exec",
            "--sandbox",
            "read-only",
            "--skip-git-repo-check",
            "-c",
            "approval_policy=never",
            "-",
        ]),
        _ => claude_command(max_budget_usd),
    }
}

fn claude_command(max_budget_usd: Option<f64>) -> Vec<String> {
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

/// Log the `ai_cli_safety` warning for `cli` at `site` (if any) as a
/// `WARNING: ...` line. The sealed call no longer refuses unrestricted CLIs, so
/// every caller that feeds untrusted text to it uses this.
pub fn warn_if_unrestricted(cli: &str, site: CallSite, log: &mut dyn FnMut(&str)) {
    if let Some(w) = ai_cli_safety::warning(cli, site) {
        log(&format!("WARNING: {w}"));
    }
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
    let fail = |m: &str| Err(SealedCallFailed(m.into(), SealedFailKind::Reply));
    let fail_as = |m: String, kind| Err(SealedCallFailed(m, kind));
    let cli = match hooks.cli_fn {
        Some(f) => f(),
        None => ai_cli_config::selected_cli(),
    };
    let mut argv = match hooks.command_fn {
        Some(f) => f(max_budget_usd),
        None => sealed_command_for(&cli, max_budget_usd),
    };
    // Argv fallback for a CLI that cannot read stdin (see `prompt_in_argv`).
    let stdin_prompt = if prompt_in_argv(&cli) {
        argv.push(prompt.to_string());
        ""
    } else {
        prompt
    };
    let started = Instant::now();
    let scratch = Scratch::new().map_err(|e| {
        SealedCallFailed(
            format!("cannot create scratch dir: {e}"),
            SealedFailKind::NotStarted,
        )
    })?;
    let out = match hooks.runner {
        Some(r) => r(&argv, stdin_prompt, timeout_seconds, &scratch.0),
        None => real_runner(&argv, stdin_prompt, timeout_seconds, &scratch.0),
    };
    drop(scratch);
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            log(&format!("FAILED (could not start {cli})"));
            return fail_as(
                crate::runner_util::could_not_start(&cli, &argv[0], &e),
                SealedFailKind::NotStarted,
            );
        }
    };
    if out.timed_out {
        log(&format!(
            "FAILED (timed out after {}s)",
            super::fmt_seconds(timeout_seconds)
        ));
        return fail_as(format!("{cli} call timed out"), SealedFailKind::TimedOut);
    }
    if out.code != Some(0) {
        let code = out.code.unwrap_or(-1);
        log(&format!("FAILED (exited {code})"));
        return fail_as(format!("{cli} exited {code}"), SealedFailKind::Exited);
    }
    let elapsed = started.elapsed();
    if cli != "claude" {
        let reply = match super::extract_new_cli_reply(&cli, &out.stdout) {
            Some(r) => r,
            None => super::non_empty_reply(&cli, out.stdout.clone()),
        };
        return match reply {
            Ok(text) => {
                log(&format!("ok ({:.1}s)", elapsed.as_secs_f64()));
                Ok(SealedResult {
                    text,
                    cost_usd: None,
                    duration_ms: i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
                })
            }
            Err(message) => {
                // The CLI's own message can quote the prompt, so it is returned
                // to the caller but never logged.
                log("FAILED (unusable reply)");
                fail(&message)
            }
        };
    }
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
    fn reports_is_error_and_garbage() {
        let mut logs = vec![];
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

    #[test]
    fn claude_arm_of_sealed_command_for_is_the_existing_command() {
        assert_eq!(sealed_command_for("claude", None), sealed_command(None));
        assert_eq!(
            sealed_command_for("claude", Some(0.5)),
            sealed_command(Some(0.5))
        );
    }

    #[test]
    fn other_clis_use_their_headless_flag_and_no_unsafe_approval_flags() {
        for (cli, flag) in [
            ("gemini", "-p"),
            ("kimi", "--print"),
            ("cursor", "--print"),
            ("codex", "exec"),
        ] {
            let cmd = sealed_command_for(cli, Some(1.0));
            assert!(cmd.contains(&flag.to_string()), "{cli}: {cmd:?}");
            for unsafe_flag in ["--yolo", "--force", "workspace-write", "--max-budget-usd"] {
                assert!(
                    !cmd.contains(&unsafe_flag.to_string()),
                    "{cli} must not carry {unsafe_flag}"
                );
            }
        }
        let g = sealed_command_for("gemini", None);
        assert_eq!(
            g[g.iter().position(|a| a == "--approval-mode").unwrap() + 1],
            "default"
        );
        let c = sealed_command_for("cursor", None);
        assert_eq!(c[c.iter().position(|a| a == "--mode").unwrap() + 1], "ask");
        let x = sealed_command_for("codex", None);
        assert!(x.contains(&"read-only".to_string()));
        assert!(!x.contains(&"--ephemeral".to_string()));
        assert_eq!(x.last().unwrap(), "-");
    }

    #[test]
    fn no_codex_argv_uses_the_flag_codex_does_not_have() {
        use crate::agents::{codex, RunOptions};
        let opts = RunOptions::new(std::path::Path::new("/x"), 5.0);
        let mut all = vec![
            sealed_command_for("codex", None),
            codex::codex_args("p", &opts),
        ];
        let mut c = crate::chat::ChatCommand::new("loopx chat-tool");
        c.cli = "codex".into();
        all.push(c.build_command("q", "T"));
        all.push(c.build_title_command("u", "r"));
        for argv in all {
            assert!(!argv.iter().any(|a| a.contains("--ephemeral")), "{argv:?}");
        }
    }

    #[test]
    fn only_cursor_falls_back_to_argv_for_the_prompt() {
        for cli in ["claude", "gemini", "kimi", "codex"] {
            assert!(!prompt_in_argv(cli), "{cli}");
        }
        assert!(prompt_in_argv("cursor"));
    }

    fn call_with_fake(cli: &str, body: &str) -> Result<SealedResult, SealedCallFailed> {
        let d = tempfile::tempdir().unwrap();
        let fake = script(d.path(), "fakecli", body);
        let command_fn = move |_: Option<f64>| vec![fake.display().to_string()];
        let cli = cli.to_string();
        let cli_fn = move || cli.clone();
        sealed_call(
            "SECRET-BODY",
            30.0,
            &mut |_| {},
            None,
            &SealedHooks {
                runner: None,
                cli_fn: Some(&cli_fn),
                command_fn: Some(&command_fn),
            },
        )
    }

    #[test]
    fn a_fake_gemini_kimi_cursor_and_codex_run_and_return_their_text() {
        // stdin must carry the prompt for the CLIs that read it.
        let echo = r#"read -r line; printf '{"response":"%s"}' "$line""#;
        assert_eq!(call_with_fake("gemini", echo).unwrap().text, "SECRET-BODY");
        let r = call_with_fake("cursor", r#"cat >/dev/null; echo '{"result":"cur"}'"#).unwrap();
        assert_eq!((r.text.as_str(), r.cost_usd), ("cur", None));
        let r = call_with_fake("kimi", "cat; echo ' kimi'").unwrap();
        assert_eq!(r.text.trim(), "SECRET-BODY kimi");
        let r = call_with_fake("codex", "cat >/dev/null; echo codex-says").unwrap();
        assert_eq!(r.text.trim(), "codex-says");
    }

    #[test]
    fn cursor_gets_the_prompt_as_the_last_argument_and_nothing_on_stdin() {
        let seen = std::cell::RefCell::new((vec![], String::new()));
        let runner = |argv: &[String], input: &str, _: f64, _: &Path| {
            *seen.borrow_mut() = (argv.to_vec(), input.to_string());
            ok_out(r#"{"result":"x"}"#)
        };
        let cli_fn = || "cursor".to_string();
        sealed_call(
            "SECRET-BODY",
            5.0,
            &mut |_| {},
            None,
            &SealedHooks {
                runner: Some(&runner),
                cli_fn: Some(&cli_fn),
                command_fn: None,
            },
        )
        .unwrap();
        let (argv, input) = seen.into_inner();
        assert_eq!(argv.last().unwrap(), "SECRET-BODY");
        assert_eq!(input, "");
    }

    #[test]
    fn each_failure_says_whether_its_text_is_loopx_own() {
        let kind = |cli: &'static str, out: std::io::Result<ProcOutput>| {
            let out = std::cell::RefCell::new(Some(out));
            let runner = |_: &[String], _: &str, _: f64, _: &Path| out.borrow_mut().take().unwrap();
            let cli_fn = move || cli.to_string();
            sealed_call(
                "p",
                5.0,
                &mut |_| {},
                None,
                &SealedHooks {
                    runner: Some(&runner),
                    cli_fn: Some(&cli_fn),
                    command_fn: None,
                },
            )
            .unwrap_err()
        };
        let run = |code: Option<i32>, stdout: &str, timed_out: bool| {
            Ok(ProcOutput {
                code,
                stdout: stdout.into(),
                stderr: "SECRET".into(),
                timed_out,
            })
        };
        let e = kind("gemini", run(Some(3), "SECRET", false));
        assert_eq!(
            (e.0.as_str(), e.1),
            ("gemini exited 3", SealedFailKind::Exited)
        );
        let e = kind("claude", run(None, "", true));
        assert_eq!(e.1, SealedFailKind::TimedOut);
        let e = kind(
            "gemini",
            run(Some(0), r#"{"error":{"message":"quota"}}"#, false),
        );
        assert_eq!(e.1, SealedFailKind::Reply);
        let e = kind(
            "kimi",
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "gone")),
        );
        assert_eq!(e.1, SealedFailKind::NotStarted);
        assert!(e.0.starts_with("could not start "), "{}", e.0);
        assert!(e.0.contains("(kimi CLI): gone"), "{}", e.0);
        assert!(!e.0.contains("SECRET"));
    }

    #[test]
    fn non_claude_failures_name_the_cli_and_map_to_exit_and_timeout() {
        assert_eq!(
            call_with_fake("gemini", "exit 4").unwrap_err().0,
            "gemini exited 4"
        );
        assert_eq!(
            call_with_fake("kimi", "   ").unwrap_err().0,
            "empty output from kimi"
        );
        assert_eq!(
            call_with_fake("gemini", "echo '{\"error\":{\"message\":\"quota\"}}'")
                .unwrap_err()
                .0,
            "quota"
        );
    }

    #[test]
    fn the_argv_sealed_call_runs_never_holds_the_prompt_except_for_cursor() {
        for cli in ["gemini", "kimi", "codex"] {
            let seen = std::cell::RefCell::new((vec![], String::new()));
            let runner = |argv: &[String], input: &str, _: f64, _: &Path| {
                *seen.borrow_mut() = (argv.to_vec(), input.to_string());
                ok_out(r#"{"response":"x"}"#)
            };
            let cli_fn = move || cli.to_string();
            let _ = sealed_call(
                "SECRET-BODY",
                5.0,
                &mut |_| {},
                None,
                &SealedHooks {
                    runner: Some(&runner),
                    cli_fn: Some(&cli_fn),
                    command_fn: None,
                },
            );
            let (argv, input) = seen.into_inner();
            assert!(
                argv.iter().all(|a| !a.contains("SECRET-BODY")),
                "{cli}: {argv:?}"
            );
            assert_eq!(input, "SECRET-BODY", "{cli}");
        }
    }

    #[test]
    fn warn_if_unrestricted_logs_for_other_clis_only() {
        for (cli, want) in [
            ("claude", false),
            ("codex", true),
            ("gemini", true),
            ("kimi", true),
            ("cursor", true),
        ] {
            let mut logs: Vec<String> = vec![];
            warn_if_unrestricted(cli, CallSite::Plugin, &mut |m| logs.push(m.into()));
            assert_eq!(logs.len(), usize::from(want), "{cli}");
            if want {
                assert!(logs[0].starts_with("WARNING: "), "{logs:?}");
            }
        }
    }
}
