//! Per-issue git worktrees for the loop. Mirrors `bin/scripts/new_worktree.sh`:
//! branch `loop/issue-<iid>`, path `<worktree_root>/<repo>-issue-<iid>`.

use std::path::Path;
use std::process::Command;

/// A failure with the message to show and the exit code to end with.
#[derive(Debug)]
pub struct Failure {
    pub message: String,
    pub code: i32,
}

impl Failure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 1,
        }
    }
}

pub fn branch_name(issue_iid: &str) -> String {
    format!("loop/issue-{issue_iid}")
}

/// The worktree path exactly as the shell script built it (plain concatenation).
pub fn worktree_path(repo_path: &str, issue_iid: &str, worktree_root: &str) -> String {
    let name = Path::new(repo_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{worktree_root}/{name}-issue-{issue_iid}")
}

/// Run `git -C <dir> <args>` with all of git's output sent to stderr, so
/// stdout carries only the final worktree path. Returns whether it succeeded.
fn git(dir: &str, args: &[&str]) -> Result<bool, Failure> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| Failure::new(format!("failed to run git: {e}")))?;
    eprint!("{}", String::from_utf8_lossy(&out.stdout));
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    Ok(out.status.success())
}

fn git_ok(dir: &str, args: &[&str]) -> Result<(), Failure> {
    if git(dir, args)? {
        Ok(())
    } else {
        Err(Failure::new(format!("git {} failed", args.join(" "))))
    }
}

fn git_stdout(dir: &str, args: &[&str]) -> Result<String, Failure> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| Failure::new(format!("failed to run git: {e}")))?;
    if !out.status.success() {
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
        return Err(Failure::new(format!("git {} failed", args.join(" "))));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Why a configured checkout cannot host per-issue worktrees.
#[derive(Debug, PartialEq, Eq)]
pub enum CheckoutProblem {
    Empty,
    NotAbsolute,
    NotADirectory,
    NotAGitRepo,
    NoOrigin,
}

/// Why a configured worktree root cannot be used.
#[derive(Debug, PartialEq, Eq)]
pub enum RootProblem {
    Empty,
    NotAbsolute,
    /// The root lies inside this checkout, so worktrees would nest in the repo.
    InsideCheckout(String),
    CannotCreate(String),
}

/// Expand a leading `~` / `~/` against `home`.
pub fn expand_home(path: &str, home: &Path) -> String {
    if path == "~" {
        return home.display().to_string();
    }
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest).display().to_string(),
        None => path.to_string(),
    }
}

fn git_succeeds(dir: &str, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Whether `local_path` is an absolute path to a git work tree with an `origin`
/// remote, which is everything `new_worktree` needs before it can fetch.
pub fn check_checkout(local_path: &str) -> Result<(), CheckoutProblem> {
    if local_path.trim().is_empty() {
        return Err(CheckoutProblem::Empty);
    }
    let path = Path::new(local_path);
    if !path.is_absolute() {
        return Err(CheckoutProblem::NotAbsolute);
    }
    if !path.is_dir() {
        return Err(CheckoutProblem::NotADirectory);
    }
    if !git_succeeds(local_path, &["rev-parse", "--is-inside-work-tree"]) {
        return Err(CheckoutProblem::NotAGitRepo);
    }
    if !git_succeeds(local_path, &["remote", "get-url", "origin"]) {
        return Err(CheckoutProblem::NoOrigin);
    }
    Ok(())
}

/// Whether `root` is an absolute directory (created if missing, so the agent's
/// `--add-dir` always points at something) outside every checkout in `checkouts`.
pub fn check_worktree_root(root: &str, checkouts: &[String]) -> Result<(), RootProblem> {
    if root.trim().is_empty() {
        return Err(RootProblem::Empty);
    }
    let path = Path::new(root);
    if !path.is_absolute() {
        return Err(RootProblem::NotAbsolute);
    }
    if let Some(repo) = checkouts
        .iter()
        .find(|c| !c.is_empty() && path.starts_with(c.as_str()))
    {
        return Err(RootProblem::InsideCheckout(repo.clone()));
    }
    std::fs::create_dir_all(path).map_err(|e| RootProblem::CannotCreate(e.to_string()))
}

fn usable_checkout(repo_path: &str) -> Result<(), Failure> {
    check_checkout(repo_path).map_err(|p| {
        Failure::new(match p {
            CheckoutProblem::Empty => "No local_path configured for this project".to_string(),
            CheckoutProblem::NotAbsolute => format!("local_path must be absolute: {repo_path}"),
            CheckoutProblem::NotADirectory => format!("local_path is not a directory: {repo_path}"),
            CheckoutProblem::NotAGitRepo => {
                format!("local_path is not a git repository: {repo_path}")
            }
            CheckoutProblem::NoOrigin => {
                format!("local_path has no `origin` remote to fetch from: {repo_path}")
            }
        })
    })
}

/// Create (or bring up to date) the worktree for an issue and return its path.
pub fn new_worktree(
    repo_path: &str,
    default_branch: &str,
    issue_iid: &str,
    worktree_root: &str,
) -> Result<String, Failure> {
    if default_branch.trim().is_empty() {
        return Err(Failure::new(
            "No target branch configured for this project (set target_branch in projects.json)",
        ));
    }
    usable_checkout(repo_path)?;
    if !Path::new(worktree_root).is_absolute() {
        return Err(Failure::new(format!(
            "worktree_root must be an absolute path: {worktree_root}"
        )));
    }
    let branch = branch_name(issue_iid);
    let wt = worktree_path(repo_path, issue_iid, worktree_root);
    std::fs::create_dir_all(worktree_root)
        .map_err(|e| Failure::new(format!("cannot create {worktree_root}: {e}")))?;

    // Always start from the latest default branch, never stale code.
    git_ok(repo_path, &["fetch", "origin", default_branch])?;

    let ref_name = format!("refs/heads/{branch}");
    let origin_ref = format!("origin/{default_branch}");
    if git(repo_path, &["show-ref", "--verify", "--quiet", &ref_name])? {
        if !Path::new(&wt).is_dir() {
            git_ok(repo_path, &["worktree", "add", &wt, &branch])?;
        }
        // Merging into a dirty worktree would mix leftover edits into the sync.
        if !git_stdout(&wt, &["status", "--porcelain"])?
            .trim()
            .is_empty()
        {
            return Err(Failure::new(format!(
                "Worktree has uncommitted changes, refusing to sync: {wt}"
            )));
        }
        // On conflict abort the merge: a dangling MERGE_HEAD would wedge every later run.
        if !git(&wt, &["merge", "--no-edit", &origin_ref])? {
            git_ok(&wt, &["merge", "--abort"])?;
            return Err(Failure::new(format!(
                "Merge conflict pulling in {default_branch}, aborted: {wt}"
            )));
        }
    } else {
        git_ok(
            repo_path,
            &["worktree", "add", "-b", &branch, &wt, &origin_ref],
        )?;
    }

    // Gitignored per-developer files `git worktree add` never brings along;
    // copied on every run so later changes are picked up too.
    for rel in [".ruby-version", "config/database.yml"] {
        let src = Path::new(repo_path).join(rel);
        if src.is_file() {
            std::fs::copy(&src, Path::new(&wt).join(rel))
                .map_err(|e| Failure::new(format!("cannot copy {rel} into {wt}: {e}")))?;
        }
    }
    // Rails credentials keys (RAILS_MASTER_KEY and the per-environment ones), so
    // development and test can decrypt the project's own credentials. Never the
    // production key, and only a key the worktree ignores, so it can't be committed.
    for rel in [
        "config/master.key",
        "config/credentials/development.key",
        "config/credentials/test.key",
    ] {
        let src = Path::new(repo_path).join(rel);
        if !src.is_file() || !git(&wt, &["check-ignore", "-q", rel])? {
            continue;
        }
        let dst = Path::new(&wt).join(rel);
        if let Some(dir) = dst.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| Failure::new(format!("cannot create {}: {e}", dir.display())))?;
        }
        std::fs::copy(&src, &dst)
            .map_err(|e| Failure::new(format!("cannot copy {rel} into {wt}: {e}")))?;
    }

    // Submodule checkouts are per-worktree; init them on every run.
    ensure_submodules(&wt)?;
    Ok(wt)
}

/// Init every submodule of `wt` and fail if any is left empty. An interrupted
/// `submodule update` can leave a checkout that git status still reports as
/// clean, so the status (`-` prefix = not initialised) is checked afterwards.
pub(crate) fn ensure_submodules(wt: &str) -> Result<(), Failure> {
    let args = ["submodule", "update", "--init", "--recursive"];
    let out = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(args)
        .output()
        .map_err(|e| Failure::new(format!("failed to run git: {e}")))?;
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    eprint!("{}", String::from_utf8_lossy(&out.stdout));
    if !stderr.is_empty() {
        eprintln!("{stderr}");
    }
    // Why the update failed, for the error the agent hands to the doctor.
    let why = if out.status.success() || stderr.is_empty() {
        String::new()
    } else {
        format!("; `git {}` said: {stderr}", args.join(" "))
    };
    let missing = uninitialised_submodules(wt)?;
    if !missing.is_empty() {
        return Err(Failure::new(format!(
            "submodules not initialised: {}{why}",
            missing.join(", ")
        )));
    }
    if !out.status.success() {
        return Err(Failure::new(format!("git {} failed{why}", args.join(" "))));
    }
    Ok(())
}

/// Paths of the submodules (recursively) that `git submodule status` marks
/// with a leading `-`, i.e. not checked out.
fn uninitialised_submodules(wt: &str) -> Result<Vec<String>, Failure> {
    let out = git_stdout(wt, &["submodule", "status", "--recursive"])?;
    Ok(out
        .lines()
        .filter(|l| l.starts_with('-'))
        .filter_map(|l| l[1..].split_whitespace().nth(1))
        .map(str::to_string)
        .collect())
}

/// Branch the checkout itself to `loop/issue-<iid>` (projects with
/// `use_worktree: false`) and return its path. The branch the user was on is
/// recorded so `restore_in_place` can put it back.
pub fn prepare_in_place(
    repo_path: &str,
    default_branch: &str,
    issue_iid: &str,
) -> Result<String, Failure> {
    if default_branch.trim().is_empty() {
        return Err(Failure::new(
            "No target branch configured for this project (set target_branch in projects.json)",
        ));
    }
    usable_checkout(repo_path)?;
    let marker = restore_marker(repo_path)?;
    let branch = branch_name(issue_iid);
    let origin_ref = format!("origin/{default_branch}");
    let is_clean = || -> Result<bool, Failure> {
        Ok(git_stdout(repo_path, &["status", "--porcelain"])?
            .trim()
            .is_empty())
    };
    let sync = || -> Result<(), Failure> {
        // On conflict abort the merge: a dangling MERGE_HEAD would wedge every later run.
        if !git(repo_path, &["merge", "--no-edit", &origin_ref])? {
            git_ok(repo_path, &["merge", "--abort"])?;
            return Err(Failure::new(format!(
                "Merge conflict pulling in {default_branch}, aborted: {repo_path}"
            )));
        }
        Ok(())
    };
    if marker.exists() {
        if current_branch(repo_path)? == branch {
            // A retry of this same issue: bring its branch up to date.
            if !is_clean()? {
                return Err(Failure::new(format!(
                    "checkout has uncommitted changes, refusing to sync: {repo_path}"
                )));
            }
            git_ok(repo_path, &["fetch", "origin", default_branch])?;
            sync()?;
            return Ok(repo_path.to_string());
        }
        if !is_clean()? {
            return Err(Failure::new(format!(
                "an earlier in-place run was not restored: {repo_path} (see {})",
                marker.display()
            )));
        }
        // A run that died before restoring: put the user's branch back first.
        restore_in_place(repo_path)?;
    }
    // Never touch the user's own work: no stash, no clean, just refuse.
    if !is_clean()? {
        return Err(Failure::new(format!(
            "checkout has uncommitted or untracked changes: {repo_path}"
        )));
    }
    let original = current_branch(repo_path)?;
    git_ok(repo_path, &["fetch", "origin", default_branch])?;

    // Record the way back before leaving the user's branch, and undo both if
    // switching fails part-way.
    std::fs::write(&marker, &original)
        .map_err(|e| Failure::new(format!("cannot write {}: {e}", marker.display())))?;
    let switched = if git(
        repo_path,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )? {
        git_ok(repo_path, &["checkout", "-q", &branch]).and_then(|_| sync())
    } else {
        git_ok(repo_path, &["checkout", "-q", "-b", &branch, &origin_ref])
    };
    if let Err(e) = switched {
        if current_branch(repo_path).is_ok_and(|b| b != original) {
            let _ = git(repo_path, &["checkout", "-q", &original]);
        }
        let _ = std::fs::remove_file(&marker);
        return Err(e);
    }
    Ok(repo_path.to_string())
}

/// The checked-out branch, or the commit when HEAD is detached.
fn current_branch(repo_path: &str) -> Result<String, Failure> {
    match git_stdout(repo_path, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        Ok(b) => Ok(b.trim().to_string()),
        Err(_) => Ok(git_stdout(repo_path, &["rev-parse", "HEAD"])?
            .trim()
            .to_string()),
    }
}

/// Whether the tracked project at `local_path` (per the projects.json at
/// `config_path`) uses per-issue worktrees; true unless it says `false`.
pub fn uses_worktree_for(local_path: &str, config_path: &Path) -> bool {
    let Ok(config) = crate::config::Config::load(config_path) else {
        return true;
    };
    config
        .top_level("projects")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flat_map(|p| p.values())
        .find(|p| p.get("local_path").and_then(serde_json::Value::as_str) == Some(local_path))
        .is_none_or(crate::config::use_worktree)
}

/// Put an in-place checkout back on the branch `prepare_in_place` recorded.
/// `None` when there was nothing to restore. A failed switch keeps the record.
pub fn restore_in_place(repo_path: &str) -> Result<Option<String>, Failure> {
    let marker = restore_marker(repo_path)?;
    let Ok(original) = std::fs::read_to_string(&marker) else {
        return Ok(None);
    };
    let original = original.trim().to_string();
    let current = git_stdout(repo_path, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    // A checkout carries uncommitted edits across, which would land the
    // loop's leftovers on the user's own branch: refuse instead.
    let dirty = !git_stdout(repo_path, &["status", "--porcelain"])?
        .trim()
        .is_empty();
    if dirty || !git(repo_path, &["checkout", "-q", &original])? {
        let why = if dirty {
            " (it has uncommitted changes)"
        } else {
            ""
        };
        return Err(Failure::new(format!(
            "could not switch {repo_path} back from {current} to {original}{why}"
        )));
    }
    let _ = std::fs::remove_file(&marker);
    Ok(Some(original))
}

/// What [`sweep_finished_issue_branches`] did, by issue iid.
#[derive(Debug, Default)]
pub struct Sweep {
    pub removed: Vec<String>,
    /// Finished, but kept: the iid and why.
    pub skipped: Vec<(String, String)>,
}

/// Every worktree of `repo_path` as `(path, branch)`, the main checkout first.
fn worktrees(repo_path: &str) -> Result<Vec<(String, String)>, Failure> {
    let mut out = Vec::new();
    let mut path = None;
    for line in git_stdout(repo_path, &["worktree", "list", "--porcelain"])?.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(p.to_string());
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            if let Some(p) = path.take() {
                out.push((p, b.to_string()));
            }
        }
    }
    Ok(out)
}

/// Delete each local `loop/issue-<iid>` branch whose issue `is_finished` says
/// is done (closed, or its MR merged), removing its worktree first. A worktree
/// with uncommitted changes, or a branch the checkout itself is on, is kept.
pub fn sweep_finished_issue_branches(
    repo_path: &str,
    is_finished: &mut dyn FnMut(&str) -> bool,
) -> Result<Sweep, Failure> {
    // Forget worktrees whose directory was deleted by hand.
    git(repo_path, &["worktree", "prune"])?;
    let trees = worktrees(repo_path)?;
    let refs = git_stdout(
        repo_path,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/loop/",
        ],
    )?;
    let mut sweep = Sweep::default();
    for branch in refs.lines() {
        let Some(iid) = branch.strip_prefix("loop/issue-") else {
            continue;
        };
        if iid.is_empty() || !iid.chars().all(|c| c.is_ascii_digit()) || !is_finished(iid) {
            continue;
        }
        let tree = trees.iter().position(|(_, b)| b == branch);
        if tree == Some(0) {
            sweep.skipped.push((
                iid.to_string(),
                format!("{branch} is checked out in {repo_path}"),
            ));
            continue;
        }
        if let Some(path) = tree.map(|i| trees[i].0.as_str()) {
            if !git_stdout(path, &["status", "--porcelain"])?
                .trim()
                .is_empty()
            {
                sweep
                    .skipped
                    .push((iid.to_string(), format!("{path} has uncommitted changes")));
                continue;
            }
            // Clean, so --force only gets past git's refusal to remove a
            // worktree that has submodules.
            if !git(repo_path, &["worktree", "remove", "--force", path])? {
                sweep
                    .skipped
                    .push((iid.to_string(), format!("could not remove {path}")));
                continue;
            }
        }
        // -D: a squash-merged MR leaves the branch looking unmerged locally.
        if git(repo_path, &["branch", "-D", branch])? {
            sweep.removed.push(iid.to_string());
        } else {
            sweep
                .skipped
                .push((iid.to_string(), format!("could not delete {branch}")));
        }
    }
    Ok(sweep)
}

/// One local `loop/issue-<iid>` branch, as the dashboard's Worktrees view lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueBranch {
    pub iid: String,
    pub branch: String,
    /// The worktree it is checked out in, if any (never the checkout itself).
    pub worktree: Option<String>,
    /// Checked out in the project's own checkout (`use_worktree: false`).
    pub in_checkout: bool,
    /// Its worktree (or the checkout) has uncommitted changes.
    pub dirty: bool,
    /// The tip commit's date, ISO 8601.
    pub last_commit: String,
}

fn is_dirty(dir: &str) -> Result<bool, Failure> {
    Ok(!git_stdout(dir, &["status", "--porcelain"])?
        .trim()
        .is_empty())
}

/// Every local `loop/issue-<iid>` branch of `repo_path`, by issue number.
pub fn list_issue_branches(repo_path: &str) -> Result<Vec<IssueBranch>, Failure> {
    // Forget worktrees whose directory was deleted by hand.
    git(repo_path, &["worktree", "prune"])?;
    let trees = worktrees(repo_path)?;
    let refs = git_stdout(
        repo_path,
        &[
            "for-each-ref",
            "--format=%(refname:short) %(committerdate:iso-strict)",
            "refs/heads/loop/",
        ],
    )?;
    let mut out = Vec::new();
    for line in refs.lines() {
        let (branch, date) = line.split_once(' ').unwrap_or((line, ""));
        let Some(iid) = branch.strip_prefix("loop/issue-") else {
            continue;
        };
        if iid.is_empty() || !iid.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let tree = trees.iter().position(|(_, b)| b == branch);
        let dirty = match tree {
            Some(i) => is_dirty(&trees[i].0)?,
            None => false,
        };
        out.push(IssueBranch {
            iid: iid.to_string(),
            branch: branch.to_string(),
            worktree: tree.filter(|&i| i > 0).map(|i| trees[i].0.clone()),
            in_checkout: tree == Some(0),
            dirty,
            last_commit: date.to_string(),
        });
    }
    out.sort_by_key(|b| b.iid.parse::<u64>().unwrap_or(u64::MAX));
    Ok(out)
}

/// Delete the local `loop/issue-<iid>` branch and its worktree, uncommitted
/// changes and all. A branch the checkout itself is on is refused, and the
/// remote branch (if it was pushed) is left alone.
pub fn remove_issue_branch(repo_path: &str, iid: &str) -> Result<(), Failure> {
    if iid.is_empty() || !iid.chars().all(|c| c.is_ascii_digit()) {
        return Err(Failure::new(format!("not an issue number: {iid}")));
    }
    let branch = branch_name(iid);
    let Some(found) = list_issue_branches(repo_path)?
        .into_iter()
        .find(|b| b.branch == branch)
    else {
        return Err(Failure::new(format!(
            "no local branch {branch} in {repo_path}"
        )));
    };
    if found.in_checkout {
        return Err(Failure::new(format!(
            "{branch} is checked out in {repo_path}; switch the checkout to another branch first"
        )));
    }
    if let Some(path) = &found.worktree {
        // Twice: also past a lock, and past uncommitted or untracked files.
        if !git(
            repo_path,
            &["worktree", "remove", "--force", "--force", path],
        )? {
            return Err(Failure::new(format!(
                "could not remove the worktree {path}"
            )));
        }
    }
    git_ok(repo_path, &["branch", "-D", &branch])
}

/// `<git-dir>/loopx-restore-branch`: the branch to return an in-place checkout to.
fn restore_marker(repo_path: &str) -> Result<std::path::PathBuf, Failure> {
    let git_dir = git_stdout(repo_path, &["rev-parse", "--absolute-git-dir"])?;
    Ok(Path::new(git_dir.trim()).join("loopx-restore-branch"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_is_plain_concatenation_of_basename() {
        assert_eq!(
            worktree_path("/src/kurrant.web", "42", "/wt"),
            "/wt/kurrant.web-issue-42"
        );
        assert_eq!(worktree_path("/src/app/", "7", "/wt/"), "/wt//app-issue-7");
    }

    fn git_repo(dir: &Path, with_origin: bool) {
        let run = |args: &[&str]| {
            assert!(Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        };
        run(&["init", "-q"]);
        if with_origin {
            run(&["remote", "add", "origin", "https://example.invalid/x.git"]);
        }
    }

    #[test]
    fn checkout_must_be_an_absolute_git_repo_with_origin() {
        let d = tempfile::tempdir().unwrap();
        let p = |n: &str| d.path().join(n).display().to_string();
        assert_eq!(check_checkout(""), Err(CheckoutProblem::Empty));
        assert_eq!(
            check_checkout("rel/repo"),
            Err(CheckoutProblem::NotAbsolute)
        );
        assert_eq!(
            check_checkout(&p("missing")),
            Err(CheckoutProblem::NotADirectory)
        );
        std::fs::create_dir(p("plain")).unwrap();
        assert_eq!(
            check_checkout(&p("plain")),
            Err(CheckoutProblem::NotAGitRepo)
        );
        std::fs::create_dir(p("local")).unwrap();
        git_repo(&d.path().join("local"), false);
        assert_eq!(check_checkout(&p("local")), Err(CheckoutProblem::NoOrigin));
        std::fs::create_dir(p("ok")).unwrap();
        git_repo(&d.path().join("ok"), true);
        assert_eq!(check_checkout(&p("ok")), Ok(()));
    }

    #[test]
    fn worktree_root_is_absolute_created_and_outside_checkouts() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(check_worktree_root("", &[]), Err(RootProblem::Empty));
        assert_eq!(
            check_worktree_root("wt", &[]),
            Err(RootProblem::NotAbsolute)
        );
        assert_eq!(
            check_worktree_root("~/wt", &[]),
            Err(RootProblem::NotAbsolute)
        );
        let repo = d.path().join("repo").display().to_string();
        let inside = d.path().join("repo/wt").display().to_string();
        assert_eq!(
            check_worktree_root(&inside, std::slice::from_ref(&repo)),
            Err(RootProblem::InsideCheckout(repo))
        );
        let fresh = d.path().join("a/b/wt");
        assert_eq!(
            check_worktree_root(&fresh.display().to_string(), &[]),
            Ok(())
        );
        assert!(fresh.is_dir());
    }

    #[test]
    fn tilde_expands_against_home_only_at_the_start() {
        let home = Path::new("/h");
        assert_eq!(expand_home("~", home), "/h");
        assert_eq!(expand_home("~/wt", home), "/h/wt");
        assert_eq!(expand_home("/a/~/b", home), "/a/~/b");
        assert_eq!(expand_home("~other", home), "~other");
    }

    #[test]
    fn new_worktree_names_the_misconfiguration() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("wt").display().to_string();
        let msg = |r: &str, b: &str, w: &str| new_worktree(r, b, "1", w).unwrap_err().message;
        assert!(msg("/nope", "main", &root).contains("not a directory"));
        assert!(msg("/nope", "", &root).contains("target branch"));
        let repo = d.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git_repo(&repo, true);
        let repo = repo.display().to_string();
        assert!(msg(&repo, "main", "~/wt").contains("absolute"));
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
            ])
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A clone of a bare origin whose `main` has one commit, checked out on
    /// `feature-x`.
    fn clone_on_feature(d: &Path) -> String {
        let origin = d.join("origin.git");
        std::fs::create_dir(&origin).unwrap();
        sh(&origin, &["init", "-q", "--bare", "-b", "main"]);
        let local = d.join("local");
        sh(d, &["clone", "-q", &origin.display().to_string(), "local"]);
        sh(&local, &["checkout", "-q", "-b", "main"]);
        std::fs::write(local.join("a.txt"), "1").unwrap();
        sh(&local, &["add", "."]);
        sh(&local, &["commit", "-qm", "init"]);
        sh(&local, &["push", "-q", "origin", "main"]);
        sh(&local, &["checkout", "-q", "-b", "feature-x"]);
        local.display().to_string()
    }

    fn head(repo: &str) -> String {
        sh(Path::new(repo), &["rev-parse", "--abbrev-ref", "HEAD"])
    }

    #[test]
    fn in_place_refuses_dirty_checkout_untouched() {
        let d = tempfile::tempdir().unwrap();
        let repo = clone_on_feature(d.path());
        std::fs::write(Path::new(&repo).join("stray.txt"), "s").unwrap();
        let before = sh(Path::new(&repo), &["status", "--porcelain"]);
        let err = prepare_in_place(&repo, "main", "7").unwrap_err();
        assert!(
            err.message.contains("uncommitted or untracked"),
            "{}",
            err.message
        );
        assert_eq!(sh(Path::new(&repo), &["status", "--porcelain"]), before);
        assert_eq!(head(&repo), "feature-x");
        assert_eq!(restore_in_place(&repo).unwrap(), None);
    }

    #[test]
    fn in_place_creates_issue_branch_and_restore_returns() {
        let d = tempfile::tempdir().unwrap();
        let repo = clone_on_feature(d.path());
        assert_eq!(prepare_in_place(&repo, "main", "7").unwrap(), repo);
        assert_eq!(head(&repo), "loop/issue-7");
        assert_eq!(
            restore_in_place(&repo).unwrap(),
            Some("feature-x".to_string())
        );
        assert_eq!(head(&repo), "feature-x");
        assert_eq!(restore_in_place(&repo).unwrap(), None);
        // A second run reuses the existing issue branch.
        prepare_in_place(&repo, "main", "7").unwrap();
        assert_eq!(head(&repo), "loop/issue-7");
    }

    #[test]
    fn in_place_refuses_an_unrestored_dirty_checkout() {
        let d = tempfile::tempdir().unwrap();
        let repo = clone_on_feature(d.path());
        prepare_in_place(&repo, "main", "7").unwrap();
        std::fs::write(Path::new(&repo).join("a.txt"), "leftover").unwrap();
        let err = prepare_in_place(&repo, "main", "8").unwrap_err();
        assert!(err.message.contains("was not restored"), "{}", err.message);
        assert_eq!(head(&repo), "loop/issue-7");
    }

    #[test]
    fn in_place_retry_on_the_same_issue_reuses_the_branch() {
        let d = tempfile::tempdir().unwrap();
        let repo = clone_on_feature(d.path());
        prepare_in_place(&repo, "main", "7").unwrap();
        assert_eq!(prepare_in_place(&repo, "main", "7").unwrap(), repo);
        assert_eq!(head(&repo), "loop/issue-7");
        assert_eq!(
            restore_in_place(&repo).unwrap(),
            Some("feature-x".to_string())
        );
    }

    #[test]
    fn a_stale_record_on_a_clean_checkout_is_restored_first() {
        let d = tempfile::tempdir().unwrap();
        let repo = clone_on_feature(d.path());
        prepare_in_place(&repo, "main", "7").unwrap();
        // The runner died before restoring; the next issue heals it.
        prepare_in_place(&repo, "main", "8").unwrap();
        assert_eq!(head(&repo), "loop/issue-8");
        assert_eq!(
            restore_in_place(&repo).unwrap(),
            Some("feature-x".to_string())
        );
    }

    #[test]
    fn restore_refuses_leftover_edits_and_keeps_the_record() {
        let d = tempfile::tempdir().unwrap();
        let repo = clone_on_feature(d.path());
        prepare_in_place(&repo, "main", "7").unwrap();
        std::fs::write(Path::new(&repo).join("a.txt"), "agent leftover").unwrap();
        let err = restore_in_place(&repo).unwrap_err();
        assert!(
            err.message.contains("loop/issue-7") && err.message.contains("feature-x"),
            "{}",
            err.message
        );
        assert_eq!(head(&repo), "loop/issue-7");
        assert!(restore_marker(&repo).unwrap().exists());
    }

    #[test]
    fn worktree_is_skipped_only_for_a_project_that_turned_it_off() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("projects.json");
        std::fs::write(
            &p,
            r#"{"projects": {"a": {"local_path": "/a", "use_worktree": false}, "b": {"local_path": "/b"}}}"#,
        )
        .unwrap();
        assert!(!uses_worktree_for("/a", &p));
        assert!(uses_worktree_for("/b", &p));
        assert!(uses_worktree_for("/unknown", &p));
        assert!(uses_worktree_for("/a", &d.path().join("missing.json")));
    }

    #[test]
    fn restore_without_marker_is_noop() {
        let d = tempfile::tempdir().unwrap();
        let repo = clone_on_feature(d.path());
        assert_eq!(restore_in_place(&repo).unwrap(), None);
        assert_eq!(head(&repo), "feature-x");
    }

    fn branches(repo: &str) -> String {
        sh(
            Path::new(repo),
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads/loop/",
            ],
        )
    }

    #[test]
    fn finished_issue_branches_and_their_worktrees_are_removed() {
        let d = tempfile::tempdir().unwrap();
        let repo = clone_on_feature(d.path());
        let root = d.path().join("wt").display().to_string();
        let wt3 = new_worktree(&repo, "main", "3", &root).unwrap();
        let wt4 = new_worktree(&repo, "main", "4", &root).unwrap();
        let wt5 = new_worktree(&repo, "main", "5", &root).unwrap();
        std::fs::write(Path::new(&wt5).join("wip.txt"), "x").unwrap();
        sh(Path::new(&repo), &["branch", "loop/issue-6"]);
        sh(Path::new(&repo), &["branch", "loop/other"]);
        let mut asked = Vec::new();
        let sweep = sweep_finished_issue_branches(&repo, &mut |iid| {
            asked.push(iid.to_string());
            iid != "4"
        })
        .unwrap();
        assert_eq!(asked, ["3", "4", "5", "6"]);
        assert_eq!(sweep.removed, ["3", "6"]);
        assert_eq!(sweep.skipped.len(), 1);
        assert_eq!(sweep.skipped[0].0, "5");
        assert!(
            sweep.skipped[0].1.contains("uncommitted"),
            "{:?}",
            sweep.skipped
        );
        assert!(!Path::new(&wt3).exists());
        assert!(Path::new(&wt4).exists() && Path::new(&wt5).exists());
        assert_eq!(branches(&repo), "loop/issue-4\nloop/issue-5\nloop/other");
    }

    #[test]
    fn rails_keys_are_copied_only_when_gitignored() {
        let d = tempfile::tempdir().unwrap();
        let repo = clone_on_feature(d.path());
        let local = Path::new(&repo);
        sh(local, &["checkout", "-q", "main"]);
        std::fs::write(
            local.join(".gitignore"),
            "/config/master.key\n/config/credentials/development.key\n/config/credentials/production.key\n",
        )
        .unwrap();
        sh(local, &["add", ".gitignore"]);
        sh(local, &["commit", "-qm", "ignore keys"]);
        sh(local, &["push", "-q", "origin", "main"]);
        std::fs::create_dir_all(local.join("config/credentials")).unwrap();
        for (rel, body) in [
            ("config/master.key", "m"),
            ("config/credentials/development.key", "d"),
            // Not gitignored: never copied, so it can't be committed by accident.
            ("config/credentials/test.key", "t"),
            // Gitignored, but production secrets have no place in a worktree.
            ("config/credentials/production.key", "p"),
        ] {
            std::fs::write(local.join(rel), body).unwrap();
        }
        let root = d.path().join("wt").display().to_string();
        let wt = new_worktree(&repo, "main", "7", &root).unwrap();
        let read = |rel: &str| std::fs::read_to_string(Path::new(&wt).join(rel)).ok();
        assert_eq!(read("config/master.key").as_deref(), Some("m"));
        assert_eq!(
            read("config/credentials/development.key").as_deref(),
            Some("d")
        );
        assert_eq!(read("config/credentials/test.key"), None);
        assert_eq!(read("config/credentials/production.key"), None);
        assert_eq!(sh(Path::new(&wt), &["status", "--porcelain"]), "");
    }

    #[test]
    fn a_finished_branch_checked_out_in_place_is_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let repo = clone_on_feature(d.path());
        prepare_in_place(&repo, "main", "7").unwrap();
        let sweep = sweep_finished_issue_branches(&repo, &mut |_| true).unwrap();
        assert!(sweep.removed.is_empty());
        assert!(
            sweep.skipped[0].1.contains("checked out"),
            "{:?}",
            sweep.skipped
        );
        assert_eq!(head(&repo), "loop/issue-7");
    }

    #[test]
    fn issue_branches_are_listed_with_their_worktree_and_state() {
        let d = tempfile::tempdir().unwrap();
        let repo = clone_on_feature(d.path());
        let root = d.path().join("wt").display().to_string();
        let wt3 = new_worktree(&repo, "main", "3", &root).unwrap();
        let wt4 = new_worktree(&repo, "main", "4", &root).unwrap();
        std::fs::write(Path::new(&wt4).join("wip.txt"), "x").unwrap();
        sh(Path::new(&repo), &["branch", "loop/issue-6"]);
        sh(Path::new(&repo), &["branch", "loop/other"]);
        let list = list_issue_branches(&repo).unwrap();
        let iids: Vec<_> = list.iter().map(|b| b.iid.as_str()).collect();
        assert_eq!(iids, ["3", "4", "6"]);
        let canon = |p: &str| std::fs::canonicalize(p).unwrap();
        assert_eq!(list[0].branch, "loop/issue-3");
        assert_eq!(canon(list[0].worktree.as_deref().unwrap()), canon(&wt3));
        assert!(!list[0].dirty && !list[0].in_checkout);
        assert!(list[1].dirty);
        assert_eq!(list[2].worktree, None);
        assert!(!list[2].dirty);
        assert!(list.iter().all(|b| !b.last_commit.is_empty()));
    }

    #[test]
    fn removing_an_issue_branch_takes_its_worktree_and_uncommitted_work() {
        let d = tempfile::tempdir().unwrap();
        let repo = clone_on_feature(d.path());
        let root = d.path().join("wt").display().to_string();
        let wt5 = new_worktree(&repo, "main", "5", &root).unwrap();
        std::fs::write(Path::new(&wt5).join("wip.txt"), "x").unwrap();
        sh(Path::new(&repo), &["branch", "loop/issue-6"]);
        remove_issue_branch(&repo, "5").unwrap();
        assert!(!Path::new(&wt5).exists());
        remove_issue_branch(&repo, "6").unwrap();
        assert_eq!(branches(&repo), "");
        let err = remove_issue_branch(&repo, "9").unwrap_err();
        assert!(err.message.contains("loop/issue-9"), "{}", err.message);
        let err = remove_issue_branch(&repo, "../main").unwrap_err();
        assert!(err.message.contains("issue number"), "{}", err.message);
    }

    #[test]
    fn an_issue_branch_checked_out_in_place_is_listed_but_not_removed() {
        let d = tempfile::tempdir().unwrap();
        let repo = clone_on_feature(d.path());
        prepare_in_place(&repo, "main", "7").unwrap();
        let list = list_issue_branches(&repo).unwrap();
        assert!(list[0].in_checkout, "{list:?}");
        let err = remove_issue_branch(&repo, "7").unwrap_err();
        assert!(err.message.contains("checked out"), "{}", err.message);
        assert_eq!(head(&repo), "loop/issue-7");
    }

    #[test]
    fn branch_naming() {
        assert_eq!(branch_name("9"), "loop/issue-9");
    }

    /// A repo with one committed submodule at `sub`, whose source is `<dir>/lib`.
    fn repo_with_submodule(dir: &Path) -> String {
        let run = |cwd: &Path, args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(["-c", "protocol.file.allow=always"])
                .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        let lib = dir.join("lib");
        let main = dir.join("main");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::create_dir_all(&main).unwrap();
        run(&lib, &["init", "-q"]);
        std::fs::write(lib.join("f"), "x").unwrap();
        run(&lib, &["add", "f"]);
        run(&lib, &["commit", "-q", "-m", "lib"]);
        run(&main, &["init", "-q"]);
        run(
            &main,
            &["submodule", "add", "-q", &lib.display().to_string(), "sub"],
        );
        run(&main, &["commit", "-q", "-m", "add sub"]);
        main.display().to_string()
    }

    fn deinit(main: &str) {
        assert!(Command::new("git")
            .args(["-C", main, "submodule", "deinit", "-f", "sub"])
            .output()
            .unwrap()
            .status
            .success());
    }

    #[test]
    fn uninitialised_submodules_lists_dash_entries() {
        let d = tempfile::tempdir().unwrap();
        let main = repo_with_submodule(d.path());
        assert_eq!(
            uninitialised_submodules(&main).unwrap(),
            Vec::<String>::new()
        );
        deinit(&main);
        assert_eq!(uninitialised_submodules(&main).unwrap(), vec!["sub"]);
    }

    #[test]
    fn new_worktree_fails_when_a_submodule_stays_empty() {
        let d = tempfile::tempdir().unwrap();
        let main = repo_with_submodule(d.path());
        deinit(&main);
        // Neither the source nor the cached clone is left, so the update
        // cannot populate the submodule.
        std::fs::remove_dir_all(d.path().join("lib")).unwrap();
        std::fs::remove_dir_all(d.path().join("main/.git/modules")).unwrap();
        let err = ensure_submodules(&main).unwrap_err();
        assert!(
            err.message.contains("submodules not initialised: sub"),
            "{}",
            err.message
        );
        // Why the update could not populate it.
        assert!(
            err.message.contains("git submodule update"),
            "{}",
            err.message
        );
        assert!(err.message.contains("fatal:"), "{}", err.message);
    }
}
