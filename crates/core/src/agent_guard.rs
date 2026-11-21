//! Runner-side checks around a gate-mode agent session, for CLIs whose tool
//! limits are only instructions: git state the agent must never touch is
//! snapshotted before and compared after.

use crate::proc;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const LS_REMOTE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
pub struct GuardSnapshot {
    /// The issue's loop branches on `origin` (name → sha); `None` when the
    /// remote could not be read, which skips the push check.
    pub remote_heads: Option<BTreeMap<String, String>>,
    /// `None` wherever git could not be read: unknown is never a change.
    pub remotes: Option<String>,
    pub hooks: Option<BTreeMap<String, String>>,
    pub hooks_path: Option<String>,
    pub checkout_status: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GuardReport {
    pub violations: Vec<String>,
    pub warnings: Vec<String>,
}

fn git_code(dir: &Path, args: &[&str], timeout: Duration) -> Option<(i32, String)> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0");
    let out = proc::run(&mut cmd, None, Some(timeout)).ok()?;
    Some((out.code?, out.stdout))
}

fn git(dir: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    git_code(dir, args, timeout)
        .filter(|(code, _)| *code == 0)
        .map(|(_, out)| out)
}

/// `core.hooksPath`, `""` when unset (`git config` exits 1).
fn hooks_path(dir: &Path) -> Option<String> {
    match git_code(
        dir,
        &["config", "--get", "core.hooksPath"],
        Duration::from_secs(10),
    )? {
        (0, out) => Some(out.trim().to_string()),
        (1, _) => Some(String::new()),
        _ => None,
    }
}

fn remote_heads(dir: &Path, issue_iid: u64) -> Option<BTreeMap<String, String>> {
    let out = git(dir, &["ls-remote", "--heads", "origin"], LS_REMOTE_TIMEOUT)?;
    Some(
        out.lines()
            .filter_map(|l| {
                let (sha, name) = l.split_once('\t')?;
                let name = name.strip_prefix("refs/heads/")?;
                crate::providers::branch::is_loop_branch(name, issue_iid)
                    .then(|| (name.to_string(), sha.to_string()))
            })
            .collect(),
    )
}

fn hooks(dir: &Path) -> Option<BTreeMap<String, String>> {
    let timeout = Duration::from_secs(10);
    let common = git(dir, &["rev-parse", "--git-common-dir"], timeout)?;
    let common = PathBuf::from(common.trim());
    let hooks_dir = if common.is_absolute() {
        common
    } else {
        dir.join(common)
    }
    .join("hooks");
    let Ok(entries) = std::fs::read_dir(hooks_dir) else {
        return Some(BTreeMap::new());
    };
    let hooks = entries
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| {
            let bytes = std::fs::read(e.path()).ok()?;
            Some((
                e.file_name().to_string_lossy().into_owned(),
                crate::sha256::hex(&crate::sha256::sha256(&bytes)),
            ))
        })
        .collect();
    Some(hooks)
}

pub fn snapshot(local_path: &Path, issue_iid: u64, use_worktree: bool) -> GuardSnapshot {
    let timeout = Duration::from_secs(30);
    GuardSnapshot {
        remote_heads: remote_heads(local_path, issue_iid),
        remotes: git(local_path, &["remote", "-v"], timeout),
        hooks: hooks(local_path),
        hooks_path: hooks_path(local_path),
        checkout_status: use_worktree
            .then(|| git(local_path, &["status", "--porcelain"], timeout))
            .flatten(),
    }
}

pub fn compare(before: &GuardSnapshot, after: &GuardSnapshot) -> GuardReport {
    let mut report = GuardReport::default();
    if let (Some(b), Some(a)) = (&before.remote_heads, &after.remote_heads) {
        for (name, sha) in a {
            if b.get(name) != Some(sha) {
                report
                    .violations
                    .push(format!("loop branch {name} was pushed to origin"));
            }
        }
    }
    if let (Some(b), Some(a)) = (&before.remotes, &after.remotes) {
        if b != a {
            report
                .violations
                .push("git remotes changed during the agent session".into());
        }
    }
    // Hooks are shared by every worktree of the project and rewritten by hook
    // managers on install, so a change is reported but never blamed on the agent.
    if let (Some(b), Some(a)) = (&before.hooks, &after.hooks) {
        for (name, hash) in a {
            if b.get(name) != Some(hash) {
                report
                    .warnings
                    .push(format!("git hook {name} was added or changed"));
            }
        }
    }
    if let (Some(b), Some(a)) = (&before.hooks_path, &after.hooks_path) {
        if b != a {
            report
                .warnings
                .push(format!("git core.hooksPath changed to {a}"));
        }
    }
    if let (Some(b), Some(a)) = (&before.checkout_status, &after.checkout_status) {
        if b != a {
            report
                .warnings
                .push("the checkout's working tree changed during the agent session".into());
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git(dir: &Path, args: &[&str]) {
        let s = Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            s.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&s.stderr)
        );
    }

    struct Repo {
        _d: tempfile::TempDir,
        local: std::path::PathBuf,
    }

    fn repo() -> Repo {
        let d = tempfile::tempdir().unwrap();
        let origin = d.path().join("origin.git");
        fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        let local = d.path().join("local");
        fs::create_dir(&local).unwrap();
        git(&local, &["init", "-q", "-b", "main"]);
        fs::write(local.join("a.txt"), "1").unwrap();
        git(&local, &["add", "."]);
        git(&local, &["commit", "-qm", "init"]);
        git(
            &local,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&local, &["push", "-q", "origin", "main"]);
        Repo { _d: d, local }
    }

    #[test]
    fn pushed_loop_branch_is_a_violation() {
        let r = repo();
        let before = snapshot(&r.local, 7, true);
        assert!(before.remote_heads.is_some());
        git(&r.local, &["checkout", "-qb", "feature#7-fix"]);
        git(&r.local, &["push", "-q", "origin", "feature#7-fix"]);
        git(&r.local, &["checkout", "-q", "main"]);
        let report = compare(&before, &snapshot(&r.local, 7, true));
        assert_eq!(report.violations.len(), 1, "{report:?}");
        assert!(report.violations[0].contains("feature#7-fix"), "{report:?}");
    }

    #[test]
    fn other_branches_on_remote_are_ignored() {
        let r = repo();
        let before = snapshot(&r.local, 7, true);
        git(&r.local, &["push", "-q", "origin", "main:feature#8-other"]);
        git(&r.local, &["push", "-q", "origin", "main:release"]);
        assert!(compare(&before, &snapshot(&r.local, 7, true))
            .violations
            .is_empty());
    }

    #[test]
    fn unreachable_remote_is_not_a_violation() {
        let r = repo();
        git(
            &r.local,
            &["remote", "set-url", "origin", "/nonexistent/origin.git"],
        );
        let before = snapshot(&r.local, 7, true);
        assert!(before.remote_heads.is_none());
        let report = compare(&before, &snapshot(&r.local, 7, true));
        assert!(
            report.violations.is_empty() && report.warnings.is_empty(),
            "{report:?}"
        );
    }

    #[test]
    fn new_remote_is_a_violation() {
        let r = repo();
        let before = snapshot(&r.local, 7, true);
        git(
            &r.local,
            &["remote", "add", "exfil", "https://example.invalid/x.git"],
        );
        let report = compare(&before, &snapshot(&r.local, 7, true));
        assert_eq!(
            report.violations,
            ["git remotes changed during the agent session"]
        );
    }

    #[test]
    fn changed_hook_is_a_warning() {
        let r = repo();
        let hooks = r.local.join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        let before = snapshot(&r.local, 7, true);
        fs::write(hooks.join("pre-push"), "#!/bin/sh\nexit 0\n").unwrap();
        let report = compare(&before, &snapshot(&r.local, 7, false));
        assert!(report.violations.is_empty(), "{report:?}");
        assert_eq!(report.warnings, ["git hook pre-push was added or changed"]);
    }

    #[test]
    fn hooks_path_change_is_a_warning() {
        let r = repo();
        let before = snapshot(&r.local, 7, false);
        git(&r.local, &["config", "core.hooksPath", ".husky/_"]);
        let report = compare(&before, &snapshot(&r.local, 7, false));
        assert!(report.violations.is_empty(), "{report:?}");
        assert_eq!(report.warnings, ["git core.hooksPath changed to .husky/_"]);
    }

    #[test]
    fn unreadable_git_state_is_not_a_change() {
        let r = repo();
        let after = snapshot(&r.local, 7, true);
        let unknown = snapshot(Path::new("/nonexistent-checkout"), 7, true);
        assert!(unknown.remotes.is_none() && unknown.hooks.is_none());
        assert!(unknown.hooks_path.is_none());
        let report = compare(&unknown, &after);
        assert!(
            report.violations.is_empty() && report.warnings.is_empty(),
            "{report:?}"
        );
    }

    #[test]
    fn checkout_change_is_a_warning_not_a_violation() {
        let r = repo();
        let before = snapshot(&r.local, 7, true);
        fs::write(r.local.join("a.txt"), "edited by the user").unwrap();
        let report = compare(&before, &snapshot(&r.local, 7, true));
        assert!(report.violations.is_empty(), "{report:?}");
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn checkout_is_not_watched_without_a_worktree() {
        let r = repo();
        let before = snapshot(&r.local, 7, false);
        assert!(before.checkout_status.is_none());
        fs::write(r.local.join("a.txt"), "the agent's own work").unwrap();
        assert!(compare(&before, &snapshot(&r.local, 7, false))
            .warnings
            .is_empty());
    }
}
