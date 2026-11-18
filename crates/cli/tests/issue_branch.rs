//! `loopx worktree new` and `loopx mr push|open` run as real processes on
//! scratch repos: the issue branch is `<type>#<iid>-<slug>`, reported on
//! stderr, and only issue branches can be pushed.

use std::path::Path;
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A sandboxed `loopx` (scratch HOME and config home, so no real config or
/// GitLab is read) run with `args`.
fn loopx(home: &Path, args: &[&str], dry_run: bool) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loopx"));
    cmd.args(args)
        .env("HOME", home)
        .env("LOOP_ENGINEERING_HOME", home.join(".loopx"));
    if dry_run {
        cmd.env("DRY_RUN", "1");
    }
    cmd.output().unwrap()
}

fn repo_with_origin(dir: &Path) -> String {
    let origin = dir.join("origin.git");
    let local = dir.join("app");
    std::fs::create_dir_all(&local).unwrap();
    git(
        dir,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    git(&local, &["init", "-q", "-b", "main"]);
    std::fs::write(local.join("a.txt"), "a").unwrap();
    git(&local, &["add", "a.txt"]);
    git(&local, &["commit", "-qm", "init"]);
    git(
        &local,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&local, &["push", "-q", "origin", "main"]);
    local.display().to_string()
}

#[test]
fn worktree_new_reports_the_issue_branch_it_made_or_reused() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let repo = repo_with_origin(d.path());
    let root = d.path().join("wt").display().to_string();

    // Not a tracked project, so no title or labels: `feature#<iid>`.
    let out = loopx(
        &home,
        &["worktree", "new", &repo, "main", "12", &root],
        false,
    );
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stdout.trim(), format!("{root}/app-issue-12"));
    assert!(stderr.contains("Branch: feature#12\n"), "{stderr}");

    // An existing branch of the issue (here a legacy one) is reused.
    git(Path::new(&repo), &["branch", "loop/issue-13"]);
    let out = loopx(
        &home,
        &["worktree", "new", &repo, "main", "13", &root],
        false,
    );
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("Branch: loop/issue-13\n"));
}

#[test]
fn mr_push_and_open_take_only_issue_branches() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path();
    let out = loopx(
        home,
        &["mr", "push", "/r", "bug#1303-dashboard-scrollbar"],
        true,
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "git\n-C\n/r\npush\n-u\norigin\nbug#1303-dashboard-scrollbar\n"
    );
    let out = loopx(
        home,
        &[
            "mr",
            "open",
            "/r",
            "feature#1412-org-website-liveness",
            "main",
            "feat: #1412 x",
        ],
        true,
    );
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("\nfeature#1412-org-website-liveness\n"));
    for bad in ["main", "feat#1", "loop/mr-3"] {
        let out = loopx(home, &["mr", "push", "/r", bad], true);
        assert!(!out.status.success(), "{bad}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Refusing to push non-loop branch"),
            "{bad}"
        );
    }
}
