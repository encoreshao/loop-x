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

    // Submodule checkouts are per-worktree; init them on every run.
    git_ok(&wt, &["submodule", "update", "--init", "--recursive"])?;
    Ok(wt)
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

    #[test]
    fn branch_naming() {
        assert_eq!(branch_name("9"), "loop/issue-9");
    }
}
