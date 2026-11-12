//! `loopx worktree new` (per-issue git worktrees) and `loopx mr open` (push a
//! loop branch and open a merge request).

use loopx_core::{mr, worktree};
use std::process::{Command, ExitCode};

pub fn worktree_new(args: &[String]) -> ExitCode {
    let [repo_path, default_branch, issue_iid, worktree_root] = args else {
        eprintln!(
            "Usage: loopx worktree new <repo_path> <default_branch> <issue_iid> <worktree_root>"
        );
        return ExitCode::from(1);
    };
    // A project with worktree off is branched in place instead.
    let config = loopx_core::config::default_config_path();
    let prepared = if worktree::uses_worktree_for(repo_path, &config) {
        worktree::new_worktree(repo_path, default_branch, issue_iid, worktree_root)
    } else {
        worktree::prepare_in_place(repo_path, default_branch, issue_iid)
    };
    match prepared {
        Ok(path) => {
            println!("{path}");
            ExitCode::SUCCESS
        }
        Err(f) => {
            eprintln!("{}", f.message);
            ExitCode::from(f.code as u8)
        }
    }
}

pub fn mr_open(args: &[String]) -> ExitCode {
    let [repo_path, branch, target_branch, title] = args else {
        eprintln!("Usage: loopx mr open <repo_path> <branch> <target_branch> <title>");
        return ExitCode::from(1);
    };
    if let Err(msg) = mr::check_branch(branch) {
        eprintln!("{msg}");
        return ExitCode::from(1);
    }
    let argv = mr::push_command(repo_path, branch, target_branch, title);
    if std::env::var("DRY_RUN").as_deref() == Ok("1") {
        for arg in &argv {
            println!("{arg}");
        }
        return ExitCode::SUCCESS;
    }
    match Command::new(&argv[0]).args(&argv[1..]).status() {
        Ok(status) if status.success() => {
            queue_profile_refresh(repo_path, target_branch, branch);
            ExitCode::SUCCESS
        }
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(e) => {
            eprintln!("failed to run git: {e}");
            ExitCode::from(1)
        }
    }
}

/// The agent opened this merge request itself (non-gated run): queue the profile
/// sections its branch touches. Best-effort and bounded (this process exits right
/// after, so it runs inline rather than on a detached thread); never changes the
/// exit code.
fn queue_profile_refresh(repo_path: &str, target: &str, branch: &str) {
    let want = std::fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.into());
    let alias = loopx_core::profile_sync::tracked_projects()
        .into_iter()
        .find(|p| std::fs::canonicalize(&p.work_dir).unwrap_or(p.work_dir.clone()) == want)
        .map(|p| p.alias);
    if let Some(alias) = alias {
        loopx_core::profile_sync::queue_branch_refresh_blocking(
            &alias, repo_path, target, branch, None,
        );
    }
}
