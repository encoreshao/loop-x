//! Moving a Claude Code run to another signed-in account when the one it is on
//! reaches its usage limit. Each extra account is its own `CLAUDE_CONFIG_DIR`;
//! the default account (no variable) is always the first candidate.

use crate::quota::Window;
use serde_json::Value;
use std::path::PathBuf;

/// One signed-in Claude account on this machine.
#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub name: String,
    /// `CLAUDE_CONFIG_DIR`; `None` is the default account.
    pub config_dir: Option<PathBuf>,
}

/// The default account followed by `ai_cli.json`'s `claude_accounts`.
pub fn accounts(ai_cli_config: &serde_json::Map<String, Value>) -> Vec<Account> {
    let text = |v: &Value, key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let mut out = vec![Account {
        name: "Default".into(),
        config_dir: None,
    }];
    let registered = ai_cli_config
        .get("claude_accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    for entry in registered {
        if let (Some(name), Some(dir)) = (text(entry, "name"), text(entry, "config_dir")) {
            out.push(Account {
                name,
                config_dir: Some(expand_home(&dir)),
            });
        }
    }
    out
}

/// `~/x` under `$HOME`; anything else as is.
fn expand_home(dir: &str) -> PathBuf {
    match (dir.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(dir),
    }
}

/// The account with the most room left: every window under `threshold`, the
/// lowest fullest-window wins, ties go to the earlier one. An account whose
/// usage could not be read is skipped, since its room is unknown.
pub fn pick_next(
    candidates: &[(Account, Result<Vec<Window>, String>)],
    threshold: f64,
) -> Option<&Account> {
    let mut best: Option<(&Account, f64)> = None;
    for (account, usage) in candidates {
        let Ok(windows) = usage else { continue };
        let fullest = windows.iter().map(|w| w.used_percent).fold(0.0, f64::max);
        if fullest >= threshold || best.is_some_and(|(_, b)| fullest >= b) {
            continue;
        }
        best = Some((account, fullest));
    }
    best.map(|(a, _)| a)
}

/// The live usage probe for one account: the default account through Claude
/// Code's own sign-in, any other through the token in its config folder.
pub fn live_usage(account: &Account) -> Result<Vec<Window>, String> {
    match &account.config_dir {
        None => crate::quota::live_claude_usage(),
        Some(dir) => crate::quota::claude_usage_for_dir(
            &crate::connectors::http::UreqClient::default(),
            crate::quota::USAGE_URL,
            dir,
        ),
    }
}

/// What the new account's run is told about the run that hit the limit.
pub struct Handoff<'a> {
    pub loop_name: &'a str,
    pub run_id: &'a str,
    pub from: &'a Account,
    pub to: &'a Account,
    /// This run's log so far; only its tail is kept.
    pub log_tail: &'a str,
    /// The day's log file, for anything the tail cut off.
    pub log_path: &'a std::path::Path,
}

/// Lines of log tail a note carries, so reading it stays cheap.
const NOTE_LOG_LINES: usize = 60;

/// The note, built from what loopx already has on disk, so the account that is
/// out of room spends nothing writing it.
pub fn handoff_note(h: &Handoff) -> String {
    let lines: Vec<&str> = h.log_tail.lines().collect();
    let tail = lines[lines.len().saturating_sub(NOTE_LOG_LINES)..].join("\n");
    format!(
        "# Account switch handoff\n\n\
         Loop `{loop_name}`, run `{run_id}`. The run was on the Claude account `{from}`, which \
         reached its usage limit and stopped. You are the same run continuing on account `{to}`.\n\n\
         This is a fresh session with no memory of what happened. Before doing anything, check the \
         current state (the worktree and branch, open merge requests, comments already posted, \
         events already emitted) and continue from there. Do not redo work that is already \
         visible in that state, and do not repeat a comment or merge request that already exists.\n\n\
         ## Where the previous account stopped\n\n\
         The last {n} lines of its output (the full log is `{log}`):\n\n```\n{tail}\n```\n",
        loop_name = h.loop_name,
        run_id = h.run_id,
        from = h.from.name,
        to = h.to.name,
        n = lines.len().min(NOTE_LOG_LINES),
        log = h.log_path.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(name: &str) -> Account {
        Account {
            name: name.into(),
            config_dir: Some(PathBuf::from(format!("/a/{name}"))),
        }
    }

    fn win(label: &'static str, used: f64) -> Window {
        Window {
            label,
            used_percent: used,
            resets_at: None,
        }
    }

    #[test]
    fn picks_the_account_with_the_most_room_left() {
        let c = vec![
            (
                acct("a"),
                Ok(vec![win("5-hour", 60.0), win("weekly", 10.0)]),
            ),
            (
                acct("b"),
                Ok(vec![win("5-hour", 20.0), win("weekly", 30.0)]),
            ),
            (acct("c"), Ok(vec![win("5-hour", 85.0)])),
        ];
        // b's fullest window is 30%, a's is 60%, c's is 85%.
        assert_eq!(pick_next(&c, 90.0).unwrap().name, "b");
    }

    #[test]
    fn skips_accounts_at_the_threshold_or_with_unreadable_usage() {
        let c = vec![
            (acct("a"), Ok(vec![win("5-hour", 90.0)])),
            (acct("b"), Err("no sign-in".into())),
            (acct("c"), Ok(vec![win("weekly", 95.0)])),
        ];
        assert!(pick_next(&c, 90.0).is_none());
    }

    #[test]
    fn a_tie_goes_to_the_earlier_account() {
        let c = vec![
            (acct("a"), Ok(vec![win("5-hour", 40.0)])),
            (acct("b"), Ok(vec![win("5-hour", 40.0)])),
        ];
        assert_eq!(pick_next(&c, 90.0).unwrap().name, "a");
    }

    #[test]
    fn accounts_lists_the_default_first_then_the_registered_ones() {
        let cfg = serde_json::json!({"claude_accounts": [
            {"name": "work", "config_dir": "/w"},
            {"name": "", "config_dir": "/skipped"},
            {"name": "no dir"}
        ]});
        let got = accounts(cfg.as_object().unwrap());
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].config_dir, None);
        assert_eq!(got[1].name, "work");
        assert_eq!(got[1].config_dir, Some(PathBuf::from("/w")));
    }

    #[test]
    fn the_note_names_both_accounts_and_keeps_only_the_log_tail() {
        let log: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        let (from, to) = (acct("a"), acct("b"));
        let note = handoff_note(&Handoff {
            loop_name: "gitlab-issue-loop",
            run_id: "run_1",
            from: &from,
            to: &to,
            log_tail: &log,
            log_path: std::path::Path::new("/r/outputs/history/2026-11-02.log"),
        });
        assert!(note.contains("gitlab-issue-loop") && note.contains("run_1"));
        assert!(note.contains("`a`") && note.contains("`b`"));
        assert!(note.contains("line 200") && note.contains("line 141"));
        assert!(!note.contains("line 140\n"), "older lines are cut");
        assert!(note.contains("/r/outputs/history/2026-11-02.log"));
        assert!(
            note.contains("check the current state"),
            "the new run is told to verify, not redo"
        );
    }
}
