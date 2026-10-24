//! `loopx worktree new` and `loopx mr open` (replace bin/scripts/new_worktree.sh
//! and bin/scripts/open_merge_request.sh).

use loopx_core::{mr, worktree};
use std::process::{Command, ExitCode};

pub fn worktree_new(args: &[String]) -> ExitCode {
    let [repo_path, default_branch, issue_iid, worktree_root] = args else {
        eprintln!(
            "Usage: loopx worktree new <repo_path> <default_branch> <issue_iid> <worktree_root>"
        );
        return ExitCode::from(1);
    };
    match worktree::new_worktree(repo_path, default_branch, issue_iid, worktree_root) {
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
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(e) => {
            eprintln!("failed to run git: {e}");
            ExitCode::from(1)
        }
    }
}
