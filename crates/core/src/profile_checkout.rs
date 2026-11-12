//! A loopx-managed, read-only shallow clone of a project's repository, so Memory
//! can profile a project known only by its GitLab path. The clone lives at
//! `<home>/checkouts/<alias>`; it is disposable and never the issue loop's
//! workspace. A user-owned `local_path` is never touched (see [`prepare`]).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Minimum time between two network fetches of one managed clone.
pub const FETCH_THROTTLE: Duration = Duration::from_secs(10 * 60);
/// Every git command is killed after this long.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Marker whose mtime is the last successful fetch (inside `.git`, so it is never scanned).
const MARKER: &str = "loopx-fetched";

/// Where to clone from and how to authenticate.
#[derive(Debug, Clone)]
pub struct Remote {
    pub url: String,
    pub branch: String,
    /// Extra environment for git (the credentials), never put on the command line.
    pub auth_env: Vec<(String, String)>,
}

/// A usable managed checkout; `warning` is set when the update failed and the
/// previous clone is used as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Synced {
    pub dir: PathBuf,
    pub warning: Option<String>,
}

/// `<home>/checkouts/<alias>`.
pub fn managed_dir(home: &Path, alias: &str) -> PathBuf {
    home.join("checkouts").join(alias)
}

fn tmp_path(dir: &Path) -> PathBuf {
    let mut name = dir.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    dir.with_file_name(name)
}

fn remove_any(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else if path.symlink_metadata().is_ok() {
        let _ = std::fs::remove_file(path);
    }
}

/// One git command with a deadline; its stdout on success. `Err` is one safe line
/// (secrets hidden). Isolated from the user's git setup: no credential helper (a
/// 401 must not make the keychain helper erase a stored credential), no LFS
/// download, no tracing that could print the Authorization header.
fn run_git(args: &[&str], remote: &Remote) -> Result<String, String> {
    use std::io::Read;
    let mut cmd = std::process::Command::new("git");
    cmd.args(["-c", "credential.helper="])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_TRACE")
        .env_remove("GIT_TRACE2")
        .env_remove("GIT_TRACE2_EVENT")
        .env_remove("GIT_TRACE_CURL")
        .env_remove("GIT_TRACE_PACKET")
        .env_remove("GIT_CURL_VERBOSE")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in &remote.auth_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot run git: {e}"))?;
    let mut stderr = child.stderr.take();
    let mut stdout = child.stdout.take();
    let err_reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(s) = stderr.as_mut() {
            let _ = s.read_to_string(&mut text);
        }
        text
    });
    let out_reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_string(&mut text);
        }
        text
    });
    let deadline = std::time::Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(st) => break st,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("git timed out".into());
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let text = err_reader.join().unwrap_or_default();
    let out = out_reader.join().unwrap_or_default();
    if status.success() {
        return Ok(out);
    }
    if text.contains("not found in upstream") || text.contains("Remote branch") {
        return Err(format!(
            "the repository is empty or has no branch {}",
            remote.branch
        ));
    }
    let line = text
        .lines()
        .rev()
        .find(|l| l.contains("fatal:") || l.contains("error:"))
        .or_else(|| text.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("git failed");
    Err(crate::profile_sync::safe_failure_line(line))
}

fn marker(dir: &Path) -> PathBuf {
    dir.join(".git").join(MARKER)
}

fn touch_marker(dir: &Path) {
    let _ = std::fs::write(marker(dir), b"");
    // Rewrite so the mtime moves even when the file already existed.
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(marker(dir));
}

fn fetched_recently(dir: &Path, now: SystemTime) -> bool {
    std::fs::metadata(marker(dir))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| now.duration_since(t).ok())
        .is_some_and(|age| age < FETCH_THROTTLE)
}

/// The commit the remote's branch points at, from one `git ls-remote` (no objects
/// are transferred).
pub fn remote_tip(remote: &Remote) -> Result<String, String> {
    let refspec = format!("refs/heads/{}", remote.branch);
    let out = run_git(&["ls-remote", "--", &remote.url, &refspec], remote)?;
    out.split_whitespace()
        .next()
        .filter(|sha| !sha.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("the remote has no branch {}", remote.branch))
}

/// Whether the project's remote branch still points at `saved_head`, so nothing
/// was committed since a profile was written. `false` whenever that cannot be
/// told (lookup or network failure): the caller then does the full check.
pub fn unchanged_since(
    instance: &str,
    project_id: &str,
    saved_head: &str,
    remote: &dyn Fn(&str, &str) -> Result<Remote, String>,
) -> bool {
    remote(instance, project_id)
        .and_then(|r| remote_tip(&r))
        .is_ok_and(|tip| tip == saved_head)
}

/// Clone `remote` into `<dir>.tmp` and move it to `dir`, replacing what is there.
/// Nothing at `dir` changes unless the clone succeeded.
fn fresh_clone(dir: &Path, remote: &Remote) -> Result<(), String> {
    let tmp = tmp_path(dir);
    remove_any(&tmp);
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let tmp_s = tmp.to_string_lossy().into_owned();
    if let Err(e) = run_git(
        &[
            "clone",
            "--depth",
            "1",
            "--single-branch",
            "--no-tags",
            "--branch",
            &remote.branch,
            "--",
            &remote.url,
            &tmp_s,
        ],
        remote,
    ) {
        remove_any(&tmp);
        return Err(e);
    }
    remove_any(dir);
    std::fs::rename(&tmp, dir).map_err(|e| {
        remove_any(&tmp);
        e.to_string()
    })?;
    touch_marker(dir);
    Ok(())
}

/// Whether `dir` is a clone of exactly `remote.url`.
fn is_clone_of(dir: &Path, remote: &Remote) -> bool {
    dir.join(".git").exists()
        && run_git(
            &[
                "-C",
                &dir.to_string_lossy(),
                "config",
                "--get",
                "remote.origin.url",
            ],
            remote,
        )
        .is_ok_and(|url| url.trim() == remote.url)
}

/// Make `dir` a shallow clone of `remote`: clone when missing, not a repository or
/// a clone of another URL; otherwise fetch and reset, at most once per
/// [`FETCH_THROTTLE`]. A failed update first tries a fresh clone (a killed git can
/// leave locks behind); if that fails too the previous clone is kept and a warning
/// returned. A failed first clone is an `Err` and leaves nothing behind.
pub fn sync_clone(dir: &Path, remote: &Remote, now: SystemTime) -> Result<Synced, String> {
    // `..`, `.` or a root has no file name: never delete or reset such a path.
    if dir.file_name().is_none() {
        return Err("invalid checkout directory".into());
    }
    if remote.branch.is_empty() || remote.branch.starts_with('-') {
        return Err("invalid branch name".into());
    }
    let ok = |warning| {
        Ok(Synced {
            dir: dir.to_path_buf(),
            warning,
        })
    };
    if !is_clone_of(dir, remote) {
        fresh_clone(dir, remote)?;
        return ok(None);
    }
    if fetched_recently(dir, now) {
        return ok(None);
    }
    // Nothing new on the remote: no fetch (a failed lookup falls through to one).
    if remote_tip(remote)
        .is_ok_and(|tip| crate::profile_scan::head_sha(dir).as_deref() == Some(&tip))
    {
        touch_marker(dir);
        return ok(None);
    }
    let dir_s = dir.to_string_lossy().into_owned();
    let refspec = format!("refs/heads/{}", remote.branch);
    let update = run_git(
        &["-C", &dir_s, "fetch", "--depth", "1", "origin", &refspec],
        remote,
    )
    .and_then(|_| run_git(&["-C", &dir_s, "reset", "--hard", "FETCH_HEAD"], remote));
    match update {
        Ok(_) => {
            touch_marker(dir);
            ok(None)
        }
        Err(first) => match fresh_clone(dir, remote) {
            Ok(()) => ok(None),
            Err(_) => ok(Some(format!(
                "could not update the clone, using the last one: {first}"
            ))),
        },
    }
}

/// A project's checkout, ready to scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub dir: PathBuf,
    pub warning: Option<String>,
}

type RemoteKey = (String, String);
static TEST_REMOTES: std::sync::Mutex<Vec<(RemoteKey, Remote)>> = std::sync::Mutex::new(Vec::new());

/// Answer [`remote_for`] for `(instance, project_id)` with `remote`, without GitLab.
/// For tests only (they use a local bare repository and a unique project id).
#[doc(hidden)]
pub fn register_test_remote(instance: &str, project_id: &str, remote: Remote) {
    let mut all = TEST_REMOTES.lock().unwrap_or_else(|e| e.into_inner());
    all.push(((instance.to_string(), project_id.to_string()), remote));
}

/// [`remote_for`] could not use GitLab for this project's instance.
pub const NOT_CONFIGURED: &str = "GitLab is not set up for this project's instance";
/// GitLab refused the token, or the project is not visible to it.
pub const CANNOT_READ: &str = "GitLab token cannot read this repository";

/// Whether `msg` says GitLab access cannot work for this project (not set up, or
/// the token cannot read it) rather than something that went wrong this time.
/// The weekly sync skips such a project quietly instead of failing every week.
pub fn is_access_problem(msg: &str) -> bool {
    msg == NOT_CONFIGURED || msg == CANNOT_READ
}

/// The clone URL, default branch and credentials of a GitLab project, from the
/// instance configured in `~/.gitlab/config.json`.
pub fn remote_for(instance: &str, project_id: &str) -> Result<Remote, String> {
    {
        let all = TEST_REMOTES.lock().unwrap_or_else(|e| e.into_inner());
        let key = (instance.to_string(), project_id.to_string());
        if let Some((_, r)) = all.iter().rev().find(|(k, _)| *k == key) {
            return Ok(r.clone());
        }
    }
    let cfg = crate::gitlab::load_config();
    let client = crate::gitlab::Client::from_config(cfg.as_ref(), Some(instance), None)
        .map_err(|_| NOT_CONFIGURED.to_string())?;
    let project = client.get_project(project_id).map_err(|e| match e {
        crate::gitlab::GitlabError::Http {
            status: 401 | 403 | 404,
            ..
        } => CANNOT_READ.to_string(),
        other => crate::profile_sync::safe_failure_line(&other.to_string()),
    })?;
    let text = |k: &str| project.get(k).and_then(|v| v.as_str()).unwrap_or("");
    if text("default_branch").is_empty() {
        return Err("the repository is empty".into());
    }
    if text("http_url_to_repo").is_empty() {
        return Err("GitLab did not give a clone URL".into());
    }
    Ok(Remote {
        url: text("http_url_to_repo").to_string(),
        branch: text("default_branch").to_string(),
        auth_env: client.git_auth_env(),
    })
}

/// The directory to scan for `t`. A user-owned checkout is returned as it is and
/// never fetched; a managed one is cloned or updated first.
pub fn prepare(
    t: &crate::profile_sync::TrackedProject,
    home: &Path,
    now: SystemTime,
) -> Result<Prepared, String> {
    prepare_with(t, home, now, &remote_for)
}

/// [`prepare`] with the remote lookup injected (tests use a local repository).
pub fn prepare_with(
    t: &crate::profile_sync::TrackedProject,
    home: &Path,
    now: SystemTime,
    remote: &dyn Fn(&str, &str) -> Result<Remote, String>,
) -> Result<Prepared, String> {
    if !t.managed {
        return if t.work_dir.is_dir() {
            Ok(Prepared {
                dir: t.work_dir.clone(),
                warning: None,
            })
        } else {
            Err("checkout not found".into())
        };
    }
    crate::project_profile::check_alias(&t.alias)?;
    let remote = remote(&t.instance, &t.project_id)?;
    let s = sync_clone(&managed_dir(home, &t.alias), &remote, now)?;
    Ok(Prepared {
        dir: s.dir,
        warning: s.warning,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::profile_scan::head_sha;

    pub(crate) fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .env_remove("GIT_DIR")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A bare "remote" plus a working copy that pushes to it.
    pub(crate) struct Fixture {
        _tmp: tempfile::TempDir,
        pub(crate) bare: PathBuf,
        pub(crate) work: PathBuf,
        pub(crate) base: PathBuf,
    }

    pub(crate) fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("remote.git");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        git(&bare, &["init", "--bare", "-b", "main"]);
        git(&work, &["init", "-b", "main"]);
        let f = Fixture {
            base: tmp.path().join("checkouts"),
            _tmp: tmp,
            bare,
            work,
        };
        commit(&f, "one");
        f
    }

    pub(crate) fn commit(f: &Fixture, name: &str) {
        std::fs::write(f.work.join(name), name).unwrap();
        git(&f.work, &["add", "."]);
        git(&f.work, &["commit", "-m", name]);
        git(
            &f.work,
            &["push", &format!("file://{}", f.bare.display()), "main"],
        );
    }

    pub(crate) fn remote(f: &Fixture) -> Remote {
        Remote {
            url: format!("file://{}", f.bare.display()),
            branch: "main".into(),
            auth_env: vec![],
        }
    }

    fn later() -> SystemTime {
        SystemTime::now() + FETCH_THROTTLE + Duration::from_secs(60)
    }

    #[test]
    fn managed_dir_is_under_checkouts() {
        assert_eq!(
            managed_dir(Path::new("/h"), "api"),
            PathBuf::from("/h/checkouts/api")
        );
    }

    #[test]
    fn first_clone_creates_checkout_with_head_sha() {
        let f = fixture();
        let dir = f.base.join("api");
        let s = sync_clone(&dir, &remote(&f), SystemTime::now()).unwrap();
        assert_eq!(s.dir, dir);
        assert!(dir.join("one").is_file());
        assert_eq!(head_sha(&dir), head_sha(&f.work));
        assert!(!tmp_path(&dir).exists());
    }

    #[test]
    fn update_after_new_commit_moves_head() {
        let f = fixture();
        let dir = f.base.join("api");
        sync_clone(&dir, &remote(&f), SystemTime::now()).unwrap();
        commit(&f, "two");
        let s = sync_clone(&dir, &remote(&f), later()).unwrap();
        assert_eq!(s.warning, None);
        assert_eq!(head_sha(&dir), head_sha(&f.work));
        assert!(dir.join("two").is_file());
    }

    #[test]
    fn update_within_throttle_does_not_fetch() {
        let f = fixture();
        let dir = f.base.join("api");
        sync_clone(&dir, &remote(&f), SystemTime::now()).unwrap();
        let before = head_sha(&dir);
        commit(&f, "two");
        sync_clone(&dir, &remote(&f), SystemTime::now()).unwrap();
        assert_eq!(head_sha(&dir), before);
    }

    #[test]
    fn stale_tmp_dir_is_replaced() {
        let f = fixture();
        let dir = f.base.join("api");
        std::fs::create_dir_all(tmp_path(&dir).join("junk")).unwrap();
        sync_clone(&dir, &remote(&f), SystemTime::now()).unwrap();
        assert!(dir.join("one").is_file());
        assert!(!tmp_path(&dir).exists());
    }

    #[test]
    fn dir_without_git_is_recloned() {
        let f = fixture();
        let dir = f.base.join("api");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stray"), "x").unwrap();
        sync_clone(&dir, &remote(&f), SystemTime::now()).unwrap();
        assert!(dir.join(".git").exists());
        assert!(!dir.join("stray").exists());
    }

    #[test]
    fn failed_update_keeps_cache_with_warning() {
        let f = fixture();
        let dir = f.base.join("api");
        sync_clone(&dir, &remote(&f), SystemTime::now()).unwrap();
        std::fs::remove_dir_all(&f.bare).unwrap();
        let s = sync_clone(&dir, &remote(&f), later()).unwrap();
        assert!(s.warning.is_some());
        assert!(dir.join("one").is_file());
    }

    #[test]
    fn failed_first_clone_is_an_error_and_leaves_nothing() {
        let f = fixture();
        let dir = f.base.join("api");
        let mut r = remote(&f);
        r.url = "file:///does/not/exist".into();
        assert!(sync_clone(&dir, &r, SystemTime::now()).is_err());
        assert!(!dir.exists());
        assert!(!tmp_path(&dir).exists());
    }

    #[test]
    fn token_stays_out_of_argv_config_and_errors() {
        let f = fixture();
        let token = "glpat-aaaaaaaaaaaaaaaaaaaa";
        let dir = f.base.join("api");
        let mut r = remote(&f);
        r.auth_env = vec![
            ("GIT_CONFIG_COUNT".into(), "0".into()),
            ("X_TOKEN".into(), token.into()),
        ];
        sync_clone(&dir, &r, SystemTime::now()).unwrap();
        let config = std::fs::read_to_string(dir.join(".git/config")).unwrap();
        assert!(!config.contains(token));
        // The path is echoed by git on failure; a secret-shaped one is hidden.
        let mut bad = remote(&f);
        bad.url = format!("file:///nope/{token}");
        let err = sync_clone(&f.base.join("other"), &bad, SystemTime::now()).unwrap_err();
        assert!(!err.contains("glpat-"), "{err}");
    }

    #[test]
    fn empty_remote_gives_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("e.git");
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--bare", "-b", "main"]);
        let r = Remote {
            url: format!("file://{}", bare.display()),
            branch: "main".into(),
            auth_env: vec![],
        };
        let err = sync_clone(&tmp.path().join("c"), &r, SystemTime::now()).unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    fn tracked(alias: &str, dir: &Path, managed: bool) -> crate::profile_sync::TrackedProject {
        crate::profile_sync::TrackedProject {
            alias: alias.into(),
            instance: "work".into(),
            project_id: "g/p".into(),
            work_dir: dir.to_path_buf(),
            managed,
        }
    }

    #[test]
    fn prepare_user_checkout_is_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tracked("api", tmp.path(), false);
        let boom =
            |_: &str, _: &str| -> Result<Remote, String> { panic!("must not look up a remote") };
        let p = prepare_with(&t, tmp.path(), SystemTime::now(), &boom).unwrap();
        assert_eq!(p.dir, tmp.path());
    }

    #[test]
    fn prepare_user_missing_dir_says_checkout_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tracked("api", &tmp.path().join("gone"), false);
        let boom = |_: &str, _: &str| -> Result<Remote, String> { panic!("no lookup") };
        assert_eq!(
            prepare_with(&t, tmp.path(), SystemTime::now(), &boom).unwrap_err(),
            "checkout not found"
        );
    }

    #[test]
    fn prepare_managed_clones_via_injected_remote() {
        let f = fixture();
        let home = f.base.parent().unwrap().join("home");
        let t = tracked("scraper", &managed_dir(&home, "scraper"), true);
        let r = remote(&f);
        let seen = std::cell::RefCell::new(vec![]);
        let lookup = |i: &str, p: &str| -> Result<Remote, String> {
            seen.borrow_mut().push((i.to_string(), p.to_string()));
            Ok(r.clone())
        };
        let p = prepare_with(&t, &home, SystemTime::now(), &lookup).unwrap();
        assert_eq!(p.dir, managed_dir(&home, "scraper"));
        assert!(p.dir.join("one").is_file());
        assert_eq!(seen.borrow()[0], ("work".to_string(), "g/p".to_string()));
    }

    #[test]
    fn prepare_managed_lookup_error_is_returned() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tracked("s", &managed_dir(tmp.path(), "s"), true);
        let lookup =
            |_: &str, _: &str| -> Result<Remote, String> { Err("the repository is empty".into()) };
        assert_eq!(
            prepare_with(&t, tmp.path(), SystemTime::now(), &lookup).unwrap_err(),
            "the repository is empty"
        );
    }

    #[test]
    fn a_dotdot_directory_is_refused_and_nothing_is_deleted() {
        let f = fixture();
        let home = f.base.parent().unwrap().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("projects.json"), "{}").unwrap();
        let dir = home.join("checkouts").join("..");
        assert!(sync_clone(&dir, &remote(&f), SystemTime::now()).is_err());
        assert!(home.join("projects.json").is_file());
        let t = tracked("..", &dir, true);
        let lookup = |_: &str, _: &str| -> Result<Remote, String> { Ok(remote(&f)) };
        assert!(prepare_with(&t, &home, SystemTime::now(), &lookup).is_err());
        assert!(home.join("projects.json").is_file());
    }

    #[test]
    fn a_clone_of_another_url_is_replaced_not_updated() {
        let a = fixture();
        let b = fixture();
        commit(&b, "only-in-b");
        let dir = a.base.join("api");
        sync_clone(&dir, &remote(&a), SystemTime::now()).unwrap();
        let s = sync_clone(&dir, &remote(&b), later()).unwrap();
        assert_eq!(s.warning, None);
        assert!(dir.join("only-in-b").is_file());
        assert_eq!(head_sha(&dir), head_sha(&b.work));
    }

    #[test]
    fn a_stale_git_lock_is_recovered_by_cloning_again() {
        let f = fixture();
        let dir = f.base.join("api");
        sync_clone(&dir, &remote(&f), SystemTime::now()).unwrap();
        std::fs::write(dir.join(".git/shallow.lock"), "").unwrap();
        commit(&f, "two");
        let s = sync_clone(&dir, &remote(&f), later()).unwrap();
        assert_eq!(s.warning, None, "{s:?}");
        assert_eq!(head_sha(&dir), head_sha(&f.work));
    }

    #[test]
    fn a_branch_that_looks_like_an_option_is_refused() {
        let f = fixture();
        let mut r = remote(&f);
        r.branch = "--upload-pack=evil".into();
        assert!(sync_clone(&f.base.join("api"), &r, SystemTime::now()).is_err());
        assert!(!f.base.join("api").exists());
    }

    #[test]
    fn tracked_managed_projects_with_unsafe_aliases_are_skipped() {
        let home = tempfile::tempdir().unwrap();
        let cfg = home.path().join("projects.json");
        std::fs::write(
            &cfg,
            r#"{"projects":{"..":{"instance":"w","project_id":"g/a"},"ok":{"instance":"w","project_id":"g/b"}}}"#,
        )
        .unwrap();
        let list = crate::profile_sync::tracked_projects_in(&cfg);
        assert_eq!(
            list.iter().map(|t| t.alias.as_str()).collect::<Vec<_>>(),
            ["ok"]
        );
    }

    #[test]
    fn remote_tip_is_the_branch_head_and_errors_when_unreachable() {
        let f = fixture();
        assert_eq!(remote_tip(&remote(&f)).unwrap(), head_sha(&f.work).unwrap());
        commit(&f, "two");
        assert_eq!(remote_tip(&remote(&f)).unwrap(), head_sha(&f.work).unwrap());
        let mut r = remote(&f);
        r.url = "file:///does/not/exist".into();
        assert!(remote_tip(&r).is_err());
    }

    #[test]
    fn an_unchanged_tip_skips_the_fetch_even_past_the_throttle() {
        let f = fixture();
        let dir = f.base.join("api");
        sync_clone(&dir, &remote(&f), SystemTime::now()).unwrap();
        // A fetch would hit this lock and fall back to a fresh clone, removing the file.
        std::fs::write(dir.join(".git/shallow.lock"), "").unwrap();
        std::fs::write(dir.join("untracked"), "x").unwrap();
        let s = sync_clone(&dir, &remote(&f), later()).unwrap();
        assert_eq!(s.warning, None);
        assert!(dir.join("untracked").is_file(), "no fetch, no re-clone");
    }

    #[test]
    fn unchanged_since_compares_the_remote_tip_with_a_saved_head() {
        let f = fixture();
        let tip = head_sha(&f.work).unwrap();
        let r = remote(&f);
        let lookup = |_: &str, _: &str| -> Result<Remote, String> { Ok(r.clone()) };
        assert!(unchanged_since("work", "g/p", &tip, &lookup));
        commit(&f, "two");
        assert!(!unchanged_since("work", "g/p", &tip, &lookup));
        let down = |_: &str, _: &str| -> Result<Remote, String> { Err("down".into()) };
        assert!(
            !unchanged_since("work", "g/p", &tip, &down),
            "unknown means do the full check"
        );
    }
}
