//! Is the selected AI CLI ready for an unattended run? Checked with the
//! 6-hourly sign-in probes (`auth_health`), so a CLI that is missing, too old
//! for the flags LoopX passes, or signed out is reported in Slack before a
//! scheduled run fails on it.
//!
//! The CLI is looked up on the login shell's `PATH` as well as ours: launchd
//! starts the scheduler with a bare `PATH` that misses Homebrew and
//! `~/.local/bin`, while runs themselves go through `zsh -i -l`. "Too old" is
//! feature detection, not a version table: the CLI's `--help` must list every
//! flag LoopX passes it.

use crate::auth_health::{ProbeFailure, ProbeResult};
use crate::doctor::find_on_path;
use crate::proc;
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const PATH_MARKER: &str = "__LOOPX_PATH__";

/// The arguments that print the help for the mode LoopX runs, and the flags
/// that help must list.
fn required_flags(cli: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
    match cli {
        "claude" => Some((
            &["--help"],
            &[
                "--add-dir",
                "--allowedTools",
                "--disallowedTools",
                "--max-budget-usd",
                "--settings",
            ],
        )),
        "codex" => Some((&["exec", "--help"], &["--sandbox"])),
        _ => None,
    }
}

/// The non-interactive sign-in check, and how to sign in again. Only CLIs whose
/// status command never starts an interactive login are listed.
fn sign_in_check(cli: &str) -> Option<(&'static [&'static str], &'static str)> {
    match cli {
        "claude" => Some((&["auth", "status"], "claude auth login")),
        "codex" => Some((&["login", "status"], "codex login")),
        _ => None,
    }
}

fn run(program: &Path, args: &[&str]) -> Option<proc::ProcOutput> {
    let out = proc::run(
        Command::new(program).args(args),
        None,
        Some(COMMAND_TIMEOUT),
    )
    .ok()?;
    (!out.timed_out).then_some(out)
}

fn first_line(out: &proc::ProcOutput) -> Option<String> {
    out.stdout
        .lines()
        .chain(out.stderr.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(String::from)
}

/// Whether a sign-in status says signed in: `claude auth status` prints JSON
/// with `loggedIn`; otherwise the exit code says.
fn signed_in(out: &proc::ProcOutput) -> bool {
    match serde_json::from_str::<Value>(out.stdout.trim()) {
        Ok(v) if v.get("loggedIn").is_some() => v["loggedIn"] == Value::Bool(true),
        _ => out.success(),
    }
}

/// Probe `cli` on the colon-separated `path`.
pub fn check(cli: &str, path: &str) -> ProbeResult {
    let program = crate::ai_cli_config::binary_candidates(cli)
        .iter()
        .find_map(|name| find_on_path(name, path))
        .ok_or_else(|| {
            ProbeFailure::hard(format!(
                "{cli} is not installed or not on your login shell's PATH. Install it, or switch CLI in Settings -> AI CLI."
            ))
        })?;
    let version = run(&program, &["--version"])
        .filter(proc::ProcOutput::success)
        .and_then(|o| first_line(&o))
        .unwrap_or_else(|| cli.to_string());
    if let Some((help_args, flags)) = required_flags(cli) {
        if let Some(help) = run(&program, help_args).filter(proc::ProcOutput::success) {
            let text = format!("{}{}", help.stdout, help.stderr);
            let missing: Vec<&str> = flags
                .iter()
                .copied()
                .filter(|f| !text.contains(f))
                .collect();
            if !missing.is_empty() {
                return Ok((
                    false,
                    format!(
                        "{version} is too old: it has no {}. Update {cli}; runs fail until then.",
                        missing.join(", ")
                    ),
                ));
            }
        }
    }
    if let Some((status_args, login)) = sign_in_check(cli) {
        // A status command that times out or can't run proves nothing either way.
        if let Some(status) = run(&program, status_args) {
            if !signed_in(&status) {
                return Err(ProbeFailure::hard(format!(
                    "{cli} is not signed in. Open Terminal and run `{login}`; scheduled runs fail until then."
                )));
            }
            return Ok((true, format!("{version}, signed in")));
        }
    }
    Ok((true, version))
}

/// The login shell's `PATH` from `zsh -i -l -c` output, `None` if it has no marker.
fn parse_shell_path(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(PATH_MARKER))
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from)
}

/// `first`'s directories, then those of `second` it lacks.
fn merge_paths(first: &str, second: &str) -> String {
    let mut dirs: Vec<&str> = Vec::new();
    for d in first.split(':').chain(second.split(':')) {
        if !d.is_empty() && !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs.join(":")
}

/// `current` with the login shell's `PATH` in front, as a run would see it.
/// Falls back to `current` when the shell can't be asked.
pub fn login_shell_path(shell: &str, current: &str) -> String {
    let script = format!("printf '\\n{PATH_MARKER}%s\\n' \"$PATH\"");
    let login = proc::run(
        Command::new(shell).args(["-i", "-l", "-c", &script]),
        None,
        Some(COMMAND_TIMEOUT),
    )
    .ok()
    .and_then(|o| parse_shell_path(&o.stdout));
    match login {
        Some(login) => merge_paths(&login, current),
        None => current.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::testutil::script;

    /// A fake `claude` answering `--version`, `--help` and `auth status`.
    fn fake_claude(dir: &Path, help: &str, status: &str) {
        script(
            dir,
            "claude",
            &format!(
                "case \"$1\" in\n--version) echo '2.1.316 (Claude Code)';;\n--help) echo '{help}';;\nauth) echo '{status}';;\nesac"
            ),
        );
    }

    const FULL_HELP: &str =
        "--add-dir --allowedTools --disallowedTools --max-budget-usd --settings";

    #[test]
    fn a_signed_in_current_claude_is_healthy() {
        let d = tempfile::tempdir().unwrap();
        fake_claude(d.path(), FULL_HELP, r#"{"loggedIn": true}"#);
        let r = check("claude", &d.path().display().to_string());
        assert_eq!(
            r,
            Ok((true, "2.1.316 (Claude Code), signed in".to_string()))
        );
    }

    #[test]
    fn a_signed_out_claude_is_a_hard_failure_with_the_login_command() {
        let d = tempfile::tempdir().unwrap();
        fake_claude(d.path(), FULL_HELP, r#"{"loggedIn": false}"#);
        match check("claude", &d.path().display().to_string()) {
            Err(ProbeFailure::Hard(m)) => assert!(m.contains("`claude auth login`"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_claude_without_a_flag_loopx_passes_is_too_old() {
        let d = tempfile::tempdir().unwrap();
        fake_claude(
            d.path(),
            "--add-dir --allowedTools --disallowedTools",
            r#"{"loggedIn": true}"#,
        );
        let (ok, message) = check("claude", &d.path().display().to_string()).unwrap();
        assert!(!ok);
        assert_eq!(
            message,
            "2.1.316 (Claude Code) is too old: it has no --max-budget-usd, --settings. Update claude; runs fail until then."
        );
    }

    #[test]
    fn a_missing_cli_names_itself_and_the_path() {
        let d = tempfile::tempdir().unwrap();
        match check("codex", &d.path().display().to_string()) {
            Err(ProbeFailure::Hard(m)) => assert!(m.starts_with("codex is not installed"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn codex_sign_in_is_its_login_status_exit_code() {
        let d = tempfile::tempdir().unwrap();
        script(
            d.path(),
            "codex",
            "case \"$1\" in\n--version) echo 'codex-cli 0.46.0';;\nexec) echo '--sandbox';;\nlogin) echo 'Not logged in'; exit 1;;\nesac",
        );
        match check("codex", &d.path().display().to_string()) {
            Err(ProbeFailure::Hard(m)) => assert!(m.contains("`codex login`"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn clis_without_a_safe_status_command_only_need_to_be_found() {
        let d = tempfile::tempdir().unwrap();
        script(d.path(), "gemini", "echo 'gemini 0.9.0'");
        assert_eq!(
            check("gemini", &d.path().display().to_string()),
            Ok((true, "gemini 0.9.0".to_string()))
        );
    }

    #[test]
    fn the_login_shell_path_comes_first_and_falls_back_to_ours() {
        let d = tempfile::tempdir().unwrap();
        let shell = script(
            d.path(),
            "fake-zsh",
            "echo 'welcome banner'; printf '\\n__LOOPX_PATH__/opt/homebrew/bin:/usr/bin\\n'",
        );
        assert_eq!(
            login_shell_path(&shell.display().to_string(), "/usr/bin:/bin"),
            "/opt/homebrew/bin:/usr/bin:/bin"
        );
        let broken = script(d.path(), "broken-zsh", "exit 1");
        assert_eq!(
            login_shell_path(&broken.display().to_string(), "/usr/bin"),
            "/usr/bin"
        );
    }
}
