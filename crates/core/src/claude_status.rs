//! What Settings → AI CLI shows about the signed-in Claude Code: who it is
//! (`claude auth status`) and how much of the subscription is used (the endpoint
//! Claude Code's own `/usage` reads, see [`crate::quota`]). Display only; the
//! pause setting in `quota` is what acts on the numbers.

use crate::doctor::find_on_path;
use crate::proc;
use crate::quota::{UsageProbe, Window};
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// The signed-in account, as `claude auth status` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    /// `authMethod`: "claude.ai" for a subscription sign-in.
    pub method: String,
    pub email: Option<String>,
    pub org: Option<String>,
    /// `subscriptionType`: "team", "max", "pro", ...
    pub plan: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AccountState {
    SignedIn(Account),
    SignedOut,
}

/// One look at the account and its usage; either half can fail on its own.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub account: Result<AccountState, String>,
    pub usage: Result<Vec<Window>, String>,
}

/// The account in `claude auth status`'s JSON output.
pub fn parse_auth_status(stdout: &str) -> Result<AccountState, String> {
    let first = stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no output");
    let v: Value = serde_json::from_str(stdout.trim())
        .map_err(|_| format!("claude auth status said: {first}"))?;
    if v.get("loggedIn") != Some(&Value::Bool(true)) {
        return Ok(AccountState::SignedOut);
    }
    let text = |key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    Ok(AccountState::SignedIn(Account {
        method: text("authMethod").unwrap_or_default(),
        email: text("email"),
        org: text("orgName"),
        plan: text("subscriptionType"),
    }))
}

/// Runs `claude auth status` from `path` (a colon-separated `PATH`).
pub fn read_account(path: &str) -> Result<AccountState, String> {
    read_account_in(path, None)
}

/// [`read_account`] for the account whose config folder is `config_dir`
/// (`CLAUDE_CONFIG_DIR`); `None` is the default account.
pub fn read_account_in(path: &str, config_dir: Option<&Path>) -> Result<AccountState, String> {
    let program = find_on_path("claude", path)
        .ok_or("claude is not installed or not on your login shell's PATH")?;
    let mut cmd = Command::new(program);
    cmd.args(["auth", "status"]);
    if let Some(dir) = config_dir {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    let out = proc::run(&mut cmd, None, Some(COMMAND_TIMEOUT))
        .map_err(|e| format!("could not run claude auth status: {e}"))?;
    if out.timed_out {
        return Err("claude auth status timed out".into());
    }
    // A signed-out claude prints its JSON and exits non-zero; the JSON decides.
    parse_auth_status(&out.stdout)
}

/// The account, then, only when signed in, the usage from `probe`.
pub fn snapshot(path: &str, probe: UsageProbe) -> Snapshot {
    snapshot_in(path, None, &probe)
}

/// [`snapshot`] for the account whose config folder is `config_dir`.
pub fn snapshot_in(
    path: &str,
    config_dir: Option<&Path>,
    probe: &dyn Fn() -> Result<Vec<Window>, String>,
) -> Snapshot {
    let account = read_account_in(path, config_dir);
    let usage = match &account {
        Ok(AccountState::SignedIn(_)) => probe(),
        Ok(AccountState::SignedOut) => Err("not signed in".to_string()),
        Err(e) => Err(e.clone()),
    };
    Snapshot { account, usage }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::testutil::script;

    const TEAM: &str = r#"{
  "loggedIn": true,
  "authMethod": "claude.ai",
  "apiProvider": "firstParty",
  "email": "loopx@ekohe.com",
  "orgId": "991a0667",
  "orgName": "Ekohe",
  "subscriptionType": "team"
}"#;

    fn account(plan: Option<&str>) -> Account {
        Account {
            method: "claude.ai".into(),
            email: Some("loopx@ekohe.com".into()),
            org: Some("Ekohe".into()),
            plan: plan.map(String::from),
        }
    }

    #[test]
    fn parses_a_subscription_sign_in() {
        assert_eq!(
            parse_auth_status(TEAM),
            Ok(AccountState::SignedIn(account(Some("team"))))
        );
    }

    #[test]
    fn a_missing_email_org_and_plan_are_none_not_errors() {
        let got = parse_auth_status(r#"{"loggedIn": true, "authMethod": "apiKey"}"#);
        assert_eq!(
            got,
            Ok(AccountState::SignedIn(Account {
                method: "apiKey".into(),
                email: None,
                org: None,
                plan: None,
            }))
        );
    }

    #[test]
    fn logged_in_false_is_signed_out() {
        assert_eq!(
            parse_auth_status(r#"{"loggedIn": false}"#),
            Ok(AccountState::SignedOut)
        );
    }

    #[test]
    fn output_that_is_not_json_is_an_error_quoting_the_first_line() {
        let err = parse_auth_status("\nSomething broke\nmore").unwrap_err();
        assert!(err.contains("Something broke"), "{err}");
        assert!(parse_auth_status("").is_err());
    }

    #[test]
    fn read_account_runs_claude_auth_status_from_the_path() {
        let d = tempfile::tempdir().unwrap();
        script(
            d.path(),
            "claude",
            &format!("[ \"$1 $2\" = \"auth status\" ] && cat <<'EOF'\n{TEAM}\nEOF"),
        );
        assert_eq!(
            read_account(&d.path().display().to_string()),
            Ok(AccountState::SignedIn(account(Some("team"))))
        );
    }

    #[test]
    fn a_signed_out_claude_that_exits_nonzero_is_still_signed_out() {
        let d = tempfile::tempdir().unwrap();
        script(d.path(), "claude", "echo '{\"loggedIn\": false}'; exit 1");
        assert_eq!(
            read_account(&d.path().display().to_string()),
            Ok(AccountState::SignedOut)
        );
    }

    #[test]
    fn read_account_without_claude_on_the_path_says_so() {
        let d = tempfile::tempdir().unwrap();
        let err = read_account(&d.path().display().to_string()).unwrap_err();
        assert!(err.contains("not installed"), "{err}");
    }

    fn windows() -> Result<Vec<Window>, String> {
        Ok(vec![Window {
            label: "5-hour",
            used_percent: 42.0,
            resets_at: None,
        }])
    }

    #[test]
    fn a_snapshot_carries_the_account_and_its_usage() {
        let d = tempfile::tempdir().unwrap();
        script(d.path(), "claude", &format!("cat <<'EOF'\n{TEAM}\nEOF"));
        let s = snapshot(&d.path().display().to_string(), windows);
        assert_eq!(s.account, Ok(AccountState::SignedIn(account(Some("team")))));
        assert_eq!(s.usage, windows());
    }

    #[test]
    fn a_signed_out_snapshot_does_not_probe_usage() {
        let d = tempfile::tempdir().unwrap();
        script(d.path(), "claude", "echo '{\"loggedIn\": false}'");
        let s = snapshot(&d.path().display().to_string(), || {
            panic!("usage must not be probed when signed out")
        });
        assert_eq!(s.account, Ok(AccountState::SignedOut));
        assert!(s.usage.is_err());
    }

    #[test]
    fn a_usage_failure_leaves_the_account_intact() {
        let d = tempfile::tempdir().unwrap();
        script(d.path(), "claude", &format!("cat <<'EOF'\n{TEAM}\nEOF"));
        let s = snapshot(&d.path().display().to_string(), || Err("offline".into()));
        assert_eq!(s.account, Ok(AccountState::SignedIn(account(Some("team")))));
        assert_eq!(s.usage, Err("offline".into()));
    }

    #[test]
    fn another_accounts_status_is_read_with_its_own_config_folder() {
        let d = tempfile::tempdir().unwrap();
        script(
            d.path(),
            "claude",
            "echo \"{\\\"loggedIn\\\": true, \\\"authMethod\\\": \\\"claude.ai\\\", \\\"email\\\": \\\"$CLAUDE_CONFIG_DIR\\\"}\"",
        );
        let path = d.path().display().to_string();
        let s = snapshot_in(&path, Some(Path::new("/work-dir")), &|| Ok(vec![]));
        match s.account {
            Ok(AccountState::SignedIn(a)) => assert_eq!(a.email.as_deref(), Some("/work-dir")),
            other => panic!("{other:?}"),
        }
    }
}
