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
    // Only a brand-new branch needs the issue's title and labels for its name.
    let seed = match worktree::find_issue_branch(repo_path, issue_iid) {
        Ok(None) => branch_seed(repo_path, issue_iid, &config),
        _ => None,
    };
    let prepared = if worktree::uses_worktree_for(repo_path, &config) {
        worktree::new_worktree(
            repo_path,
            default_branch,
            issue_iid,
            worktree_root,
            seed.as_ref(),
        )
    } else {
        worktree::prepare_in_place(repo_path, default_branch, issue_iid, seed.as_ref())
    };
    match prepared {
        Ok(path) => {
            // stdout stays the path alone; the branch goes to stderr for the agent.
            if let Ok(Some(branch)) = worktree::find_issue_branch(repo_path, issue_iid) {
                eprintln!("Branch: {branch}");
            }
            println!("{path}");
            ExitCode::SUCCESS
        }
        Err(f) => {
            eprintln!("{}", f.message);
            ExitCode::from(f.code as u8)
        }
    }
}

/// The title and labels of issue `issue_iid` of the project checked out at
/// `repo_path`, fetched from its tracker. `None` (the branch is then named
/// `feature#<iid>`) when the project or issue cannot be read.
fn branch_seed(
    repo_path: &str,
    issue_iid: &str,
    config: &std::path::Path,
) -> Option<worktree::BranchSeed> {
    let (alias, project) = worktree::project_for_checkout(repo_path, config)?;
    let n: u64 = issue_iid.parse().ok()?;
    let issue = if loopx_core::hosted::is_hosted(&project) {
        let src = loopx_core::connectors_config::ConfigConnectors::real();
        let r = loopx_core::hosted::resolve_project(&src, &project).ok()?;
        loopx_core::hosted::issue_json(r.tracker.provider(), &r.tracker.issue(n).ok()?)
    } else {
        let gitlab = loopx_core::gitlab::load_config();
        let (project_id, instance, bundle) =
            loopx_core::gitlab::resolve_project_alias(gitlab.as_ref(), &alias, None);
        loopx_core::gitlab::Client::from_config(
            gitlab.as_ref(),
            instance.as_deref(),
            bundle.as_deref(),
        )
        .ok()?
        .get_issue(&project_id, n as i64)
        .ok()?
    };
    Some(worktree::seed_of_issue(&issue))
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

/// Push new commits on an issue branch whose merge request already exists.
pub fn mr_push(args: &[String]) -> ExitCode {
    let [repo_path, branch] = args else {
        eprintln!("Usage: loopx mr push <repo_path> <branch>");
        return ExitCode::from(1);
    };
    if let Err(msg) = mr::check_branch(branch) {
        eprintln!("{msg}");
        return ExitCode::from(1);
    }
    let argv = mr::branch_push_command(repo_path, branch);
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
