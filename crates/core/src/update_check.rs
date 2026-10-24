//! Is there something new on origin? `git fetch`, compare HEAD with
//! `origin/<branch>`, cache the answer in `<state dir>/status.json` for the
//! dashboard banner and the desktop app's native notification. Mirrors
//! `bin/update_check.py`. Never fails: a failed fetch (offline) records
//! `error` and keeps the last known `behind`.
//!
//! The state directory is `$LOOP_UPDATE_DIR`, else `<repo root>/outputs/update`.
//! On-disk formats (shared with the `/update` page, see
//! docs/contract/json-files.md) are byte-compatible with the Python's:
//! `status.json` and `notified.json` are `json.dumps(indent=2)` without a
//! trailing newline.

use crate::golden_eval::finished_at_now;
use crate::proc;
use crate::pyjson;
use crate::runner_util::repo_root;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const STATE_ENV: &str = "LOOP_UPDATE_DIR";
pub const CHECK_INTERVAL_SECONDS: u64 = 6 * 3600;
/// The loop rewrites PROGRESS.md on every run; it must not block an upgrade.
pub const IGNORED_DIRTY_FILES: [&str; 1] = ["PROGRESS.md"];
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// `$LOOP_UPDATE_DIR`, else `<repo root>/outputs/update`.
pub fn default_state_dir() -> PathBuf {
    match std::env::var_os(STATE_ENV).filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => repo_root().join("outputs").join("update"),
    }
}

/// The checkout to update: `$LOOPX_ROOT`, else the nearest ancestor of the
/// binary or the working directory that is a git checkout, else the repo root.
pub fn default_repo_dir() -> PathBuf {
    if let Some(r) = std::env::var_os("LOOPX_ROOT").filter(|v| !v.is_empty()) {
        return PathBuf::from(r);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        candidates.extend(exe.ancestors().skip(1).map(Path::to_path_buf));
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.extend(cwd.ancestors().map(Path::to_path_buf));
    }
    candidates
        .into_iter()
        .find(|c| c.join(".git").exists())
        .unwrap_or_else(repo_root)
}

/// A background fetch must fail fast, never wait on a credential prompt.
pub fn non_interactive_env() -> Vec<(String, String)> {
    let ssh = std::env::var("GIT_SSH_COMMAND").unwrap_or_else(|_| "ssh".to_string());
    vec![
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        (
            "GIT_SSH_COMMAND".to_string(),
            format!("{ssh} -o BatchMode=yes"),
        ),
    ]
}

/// Output of `git -C repo <args>`; `Err` when git cannot run or times out.
fn git(repo: &Path, args: &[&str], env: &[(String, String)]) -> Result<proc::ProcOutput, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = proc::run(&mut cmd, None, Some(GIT_TIMEOUT)).map_err(|e| e.to_string())?;
    if out.timed_out {
        return Err(format!(
            "Command '['git', '-C', '{}', {}]' timed out after 60 seconds",
            repo.display(),
            args.iter()
                .map(|a| format!("'{a}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(out)
}

/// Trimmed stdout on success, `""` on a non-zero exit.
fn out(repo: &Path, args: &[&str]) -> Result<String, String> {
    let r = git(repo, args, &[])?;
    Ok(if r.success() {
        r.stdout.trim().to_string()
    } else {
        String::new()
    })
}

/// `status.json` as last written, or `{}`.
pub fn read_status(state_dir: Option<&Path>) -> Map<String, Value> {
    let dir = state_dir.map_or_else(default_state_dir, Path::to_path_buf);
    match std::fs::read_to_string(dir.join("status.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

/// Write `<state_dir>/<name>` atomically (`.<name>.tmp` + rename) as
/// `json.dumps(data, indent=2)`.
pub fn write_json(state_dir: &Path, name: &str, data: &Value) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let tmp = state_dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, pyjson::to_string_pretty(data))?;
    std::fs::rename(tmp, state_dir.join(name))
}

fn safe_write(state_dir: &Path, status: &mut Map<String, Value>) {
    if let Err(e) = write_json(state_dir, "status.json", &Value::Object(status.clone())) {
        if status.get("error").is_none_or(Value::is_null) {
            status.insert("error".into(), json!(e.to_string()));
        }
    }
}

fn is_clean(repo: &Path) -> bool {
    // Raw stdout: trimming would eat the porcelain's leading status column.
    let Ok(r) = git(
        repo,
        &["status", "--porcelain", "--untracked-files=no"],
        &[],
    ) else {
        return true;
    };
    r.stdout.lines().all(|line| {
        let file = line.get(3..).unwrap_or("").trim();
        IGNORED_DIRTY_FILES.contains(&file)
    })
}

fn set(status: &mut Map<String, Value>, key: &str, value: Value) {
    status.insert(key.to_string(), value);
}

/// The git half of [`check`]; `Err` is recorded as `error`.
fn compare(repo: &Path, fetch: bool, status: &mut Map<String, Value>) -> Result<bool, String> {
    let branch = out(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    set(status, "branch", json!(branch));
    set(
        status,
        "current",
        json!(out(repo, &["describe", "--tags", "--always"])?),
    );
    set(status, "clean", json!(is_clean(repo)));
    if fetch {
        let fetched = git(
            repo,
            &["fetch", "--tags", "--quiet", "origin"],
            &non_interactive_env(),
        )?;
        if !fetched.success() {
            let msg = fetched.stderr.trim();
            set(
                status,
                "error",
                json!(if msg.is_empty() {
                    "git fetch failed"
                } else {
                    msg
                }),
            );
            return Ok(false);
        }
    }
    let remote = format!("origin/{branch}");
    let verified = git(repo, &["rev-parse", "--verify", "--quiet", &remote], &[])?;
    if branch.is_empty() || branch == "HEAD" || !verified.success() {
        set(status, "behind", json!(0));
        set(status, "ahead", json!(0));
        set(status, "commits", json!([]));
        set(
            status,
            "error",
            json!(format!("no {remote} to compare against")),
        );
        return Ok(false);
    }
    let count = |range: String| -> Result<i64, String> {
        let text = out(repo, &["rev-list", "--count", &range])?;
        if text.is_empty() {
            return Ok(0);
        }
        text.parse()
            .map_err(|_| format!("invalid literal for int() with base 10: '{text}'"))
    };
    set(status, "behind", json!(count(format!("HEAD..{remote}"))?));
    set(status, "ahead", json!(count(format!("{remote}..HEAD"))?));
    set(
        status,
        "origin_sha",
        json!(out(repo, &["rev-parse", &remote])?),
    );
    let range = format!("HEAD..{remote}");
    let log = out(repo, &["log", "--format=%h%x09%s", "-20", &range])?;
    let commits: Vec<Value> = log
        .lines()
        .map(|line| {
            let (sha, subject) = line.split_once('\t').unwrap_or((line, ""));
            json!({"sha": sha, "subject": subject})
        })
        .collect();
    set(status, "commits", Value::Array(commits));
    let ff = git(repo, &["merge-base", "--is-ancestor", "HEAD", &remote], &[])?;
    set(status, "can_fast_forward", json!(ff.success()));
    let latest = out(repo, &["describe", "--tags", "--abbrev=0", &remote])?;
    let here = out(repo, &["describe", "--tags", "--abbrev=0", "HEAD"])?;
    set(
        status,
        "new_release",
        json!(!latest.is_empty() && latest != here),
    );
    set(status, "latest_tag", json!(latest));
    Ok(true)
}

/// Fetch (unless `fetch` is false), compare, and cache the answer in
/// `<state_dir>/status.json`. Returns the status; never fails.
pub fn check(repo: Option<&Path>, state_dir: Option<&Path>, fetch: bool) -> Map<String, Value> {
    let repo = repo.map_or_else(default_repo_dir, Path::to_path_buf);
    let state_dir = state_dir.map_or_else(default_state_dir, Path::to_path_buf);
    let previous = read_status(Some(&state_dir));
    let prev = |key: &str, default: Value| previous.get(key).cloned().unwrap_or(default);
    let mut status = Map::new();
    set(&mut status, "checked_at", json!(finished_at_now()));
    set(&mut status, "branch", json!(""));
    set(&mut status, "behind", prev("behind", json!(0)));
    set(&mut status, "ahead", prev("ahead", json!(0)));
    set(&mut status, "commits", prev("commits", json!([])));
    set(&mut status, "current", json!(""));
    set(&mut status, "latest_tag", prev("latest_tag", json!("")));
    set(
        &mut status,
        "new_release",
        prev("new_release", json!(false)),
    );
    set(&mut status, "origin_sha", prev("origin_sha", json!("")));
    set(&mut status, "clean", json!(true));
    set(&mut status, "can_fast_forward", json!(true));
    set(&mut status, "error", Value::Null);
    if let Err(e) = compare(&repo, fetch, &mut status) {
        set(&mut status, "error", json!(e));
    }
    safe_write(&state_dir, &mut status);
    status
}

/// Text for a native notification the first time this origin sha is seen as
/// new; `None` afterwards (and when nothing is new).
pub fn take_notification(state_dir: Option<&Path>) -> Option<String> {
    let dir = state_dir.map_or_else(default_state_dir, Path::to_path_buf);
    let status = read_status(Some(&dir));
    let behind = status
        .get("behind")
        .is_some_and(crate::loop_serialize::truthy);
    let sha = status
        .get("origin_sha")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    if !behind {
        return None;
    }
    let seen = std::fs::read_to_string(dir.join("notified.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("sha").and_then(Value::as_str).map(str::to_string));
    if seen.as_deref() == Some(sha) {
        return None;
    }
    // Python lets a failed notified.json write raise; here the notification
    // simply repeats until the write works.
    let _ = write_json(&dir, "notified.json", &json!({"sha": sha}));
    let what = if status
        .get("new_release")
        .is_some_and(crate::loop_serialize::truthy)
    {
        status
            .get("latest_tag")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        format!(
            "{} new change(s)",
            crate::pydumps::py_str(&status["behind"])
        )
    };
    Some(format!(
        "Loop X update available: {what}. Open Loop X and click Upgrade now."
    ))
}

static BACKGROUND_STARTED: AtomicBool = AtomicBool::new(false);

/// One daemon thread per process: check now, then every `interval`. `None`
/// when a watcher is already running.
pub fn start_background(
    interval: Duration,
    repo: Option<PathBuf>,
    state_dir: Option<PathBuf>,
) -> Option<std::thread::JoinHandle<()>> {
    start_background_with(interval, move || {
        check(repo.as_deref(), state_dir.as_deref(), true);
    })
}

/// [`start_background`] with the check injected; a panicking check does not
/// end the watcher for good.
pub fn start_background_with(
    interval: Duration,
    check_fn: impl Fn() + Send + 'static,
) -> Option<std::thread::JoinHandle<()>> {
    if BACKGROUND_STARTED.swap(true, Ordering::SeqCst) {
        return None;
    }
    std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || loop {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&check_fn));
            std::thread::sleep(interval);
        })
        .ok()
}

/// Test helper: allow another watcher to start.
#[cfg(test)]
pub(crate) fn reset_background() {
    BACKGROUND_STARTED.store(false, Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// `git -C cwd args...` with a fixed identity and a scratch HOME.
    pub fn tgit(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("HOME", cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    pub fn commit(cwd: &Path, name: &str, text: &str) {
        std::fs::write(cwd.join(name), text).unwrap();
        tgit(cwd, &["add", name]);
        tgit(cwd, &["commit", "-m", &format!("add {name}")]);
    }

    pub struct Repos {
        pub root: PathBuf,
        pub work: PathBuf,
        pub other: PathBuf,
        pub state: PathBuf,
    }

    impl Drop for Repos {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// origin.git + `work` (main, tagged v0.1.0, pushed) + `other` (a second clone).
    pub fn repos() -> Repos {
        let root = std::env::temp_dir().join(format!("upd-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&root).unwrap();
        let run = |args: &[&str]| {
            let o = Command::new("git")
                .args(args)
                .env("HOME", &root)
                .output()
                .unwrap();
            assert!(o.status.success(), "{args:?}");
        };
        let (bare, work, other) = (
            root.join("origin.git"),
            root.join("work"),
            root.join("other"),
        );
        run(&["init", "--bare", "-b", "main", bare.to_str().unwrap()]);
        run(&["clone", bare.to_str().unwrap(), work.to_str().unwrap()]);
        tgit(&work, &["checkout", "-B", "main"]);
        commit(&work, "a.txt", "x");
        tgit(&work, &["tag", "-a", "v0.1.0", "-m", "v0.1.0"]);
        tgit(&work, &["push", "-u", "origin", "main", "--tags"]);
        run(&["clone", bare.to_str().unwrap(), other.to_str().unwrap()]);
        tgit(&other, &["checkout", "main"]);
        let state = root.join("state");
        Repos {
            root,
            work,
            other,
            state,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    fn get<'a>(s: &'a Map<String, Value>, k: &str) -> &'a Value {
        &s[k]
    }

    #[test]
    fn up_to_date() {
        let r = repos();
        let s = check(Some(&r.work), Some(&r.state), true);
        assert_eq!(get(&s, "behind"), &json!(0));
        assert_eq!(get(&s, "error"), &Value::Null);
        assert_eq!(get(&s, "can_fast_forward"), &json!(true));
        assert_eq!(get(&s, "branch"), &json!("main"));
        assert_eq!(get(&s, "current"), &json!("v0.1.0"));
        assert_eq!(read_status(Some(&r.state))["behind"], json!(0));
    }

    #[test]
    fn status_file_keeps_the_python_key_order_and_format() {
        let r = repos();
        check(Some(&r.work), Some(&r.state), true);
        let text = std::fs::read_to_string(r.state.join("status.json")).unwrap();
        let keys: Vec<String> = serde_json::from_str::<Map<String, Value>>(&text)
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            keys,
            [
                "checked_at",
                "branch",
                "behind",
                "ahead",
                "commits",
                "current",
                "latest_tag",
                "new_release",
                "origin_sha",
                "clean",
                "can_fast_forward",
                "error"
            ]
        );
        assert!(text.starts_with("{\n  \"checked_at\": \""));
        assert!(!text.ends_with('\n'));
        let at = read_status(Some(&r.state))["checked_at"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(at.ends_with("+00:00") && at.len() == 25, "{at}");
        assert!(!r.state.join(".status.json.tmp").exists());
    }

    #[test]
    fn new_commits_and_release_detected() {
        let r = repos();
        commit(&r.other, "b.txt", "x");
        commit(&r.other, "c.txt", "x");
        tgit(&r.other, &["tag", "-a", "v0.2.0", "-m", "v0.2.0"]);
        tgit(&r.other, &["push", "origin", "main", "--tags"]);
        let s = check(Some(&r.work), Some(&r.state), true);
        assert_eq!(
            (get(&s, "behind"), get(&s, "ahead")),
            (&json!(2), &json!(0))
        );
        let subjects: Vec<&str> = s["commits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["subject"].as_str().unwrap())
            .collect();
        assert_eq!(subjects, ["add c.txt", "add b.txt"]);
        assert_eq!(get(&s, "latest_tag"), &json!("v0.2.0"));
        assert_eq!(get(&s, "new_release"), &json!(true));
        assert_eq!(get(&s, "can_fast_forward"), &json!(true));
        assert!(!s["origin_sha"].as_str().unwrap().is_empty());
        assert!(s["commits"][0]["sha"].as_str().unwrap().len() >= 7);
    }

    #[test]
    fn dirty_tracked_file_is_not_clean_but_progress_md_and_untracked_are_ignored() {
        let r = repos();
        std::fs::write(r.work.join("untracked.txt"), "u").unwrap();
        commit(&r.work, "PROGRESS.md", "x");
        tgit(&r.work, &["push", "origin", "main"]);
        std::fs::write(r.work.join("PROGRESS.md"), "changed by the loop").unwrap();
        assert_eq!(
            check(Some(&r.work), Some(&r.state), true)["clean"],
            json!(true)
        );
        std::fs::write(r.work.join("a.txt"), "edited").unwrap();
        assert_eq!(
            check(Some(&r.work), Some(&r.state), true)["clean"],
            json!(false)
        );
    }

    #[test]
    fn diverged_cannot_fast_forward() {
        let r = repos();
        commit(&r.other, "b.txt", "x");
        tgit(&r.other, &["push", "origin", "main"]);
        commit(&r.work, "local.txt", "x");
        let s = check(Some(&r.work), Some(&r.state), true);
        assert_eq!(
            (&s["behind"], &s["ahead"], &s["can_fast_forward"]),
            (&json!(1), &json!(1), &json!(false))
        );
    }

    #[test]
    fn offline_keeps_last_known_behind_and_records_error() {
        let r = repos();
        commit(&r.other, "b.txt", "x");
        tgit(&r.other, &["push", "origin", "main"]);
        assert_eq!(
            check(Some(&r.work), Some(&r.state), true)["behind"],
            json!(1)
        );
        tgit(
            &r.work,
            &[
                "remote",
                "set-url",
                "origin",
                r.root.join("gone.git").to_str().unwrap(),
            ],
        );
        let s = check(Some(&r.work), Some(&r.state), true);
        assert!(s["error"].as_str().is_some_and(|e| !e.is_empty()));
        assert_eq!(s["behind"], json!(1));
    }

    #[test]
    fn no_upstream_branch_is_an_error_not_a_crash() {
        let r = repos();
        let solo = r.root.join("solo");
        std::fs::create_dir_all(&solo).unwrap();
        tgit(&solo, &["init", "-b", "main"]);
        commit(&solo, "a.txt", "x");
        let s = check(Some(&solo), Some(&r.root.join("s")), false);
        assert_eq!(s["error"], json!("no origin/main to compare against"));
        assert_eq!(s["behind"], json!(0));
        assert_eq!(s["commits"], json!([]));
    }

    #[test]
    fn notification_once_per_origin_sha() {
        let r = repos();
        assert_eq!(take_notification(Some(&r.state)), None);
        commit(&r.other, "b.txt", "x");
        tgit(&r.other, &["push", "origin", "main"]);
        check(Some(&r.work), Some(&r.state), true);
        let text = take_notification(Some(&r.state)).unwrap();
        assert_eq!(
            text,
            "Loop X update available: 1 new change(s). Open Loop X and click Upgrade now."
        );
        assert_eq!(take_notification(Some(&r.state)), None);
        let sha = read_status(Some(&r.state))["origin_sha"].clone();
        assert_eq!(
            std::fs::read_to_string(r.state.join("notified.json")).unwrap(),
            format!("{{\n  \"sha\": {sha}\n}}")
        );
        commit(&r.other, "c.txt", "x");
        tgit(&r.other, &["push", "origin", "main"]);
        check(Some(&r.work), Some(&r.state), true);
        assert!(take_notification(Some(&r.state)).is_some());
    }

    #[test]
    fn notification_names_the_release_tag() {
        let r = repos();
        commit(&r.other, "b.txt", "x");
        tgit(&r.other, &["tag", "-a", "v0.2.0", "-m", "v0.2.0"]);
        tgit(&r.other, &["push", "origin", "main", "--tags"]);
        check(Some(&r.work), Some(&r.state), true);
        assert_eq!(
            take_notification(Some(&r.state)).unwrap(),
            "Loop X update available: v0.2.0. Open Loop X and click Upgrade now."
        );
    }

    #[test]
    fn env_override_for_state_dir() {
        let r = repos();
        static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(STATE_ENV, r.root.join("e"));
        assert_eq!(default_state_dir(), r.root.join("e"));
        std::env::remove_var(STATE_ENV);
        assert!(default_state_dir().ends_with("outputs/update"));
    }

    #[test]
    fn fetch_never_prompts_for_credentials() {
        let env = non_interactive_env();
        assert!(env.contains(&("GIT_TERMINAL_PROMPT".to_string(), "0".to_string())));
        let ssh = env.iter().find(|(k, _)| k == "GIT_SSH_COMMAND").unwrap();
        assert!(ssh.1.contains("-o BatchMode=yes"));
    }

    #[test]
    fn check_never_fails_when_the_state_dir_is_unwritable() {
        let r = repos();
        let blocker = r.root.join("blocker");
        std::fs::write(&blocker, "a file, not a directory").unwrap();
        let s = check(Some(&r.work), Some(&blocker.join("state")), true);
        assert!(s["error"].as_str().is_some_and(|e| !e.is_empty()));
    }

    #[test]
    fn background_watcher_survives_a_failing_check() {
        reset_background();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let handle = start_background_with(Duration::from_millis(10), move || {
            if c.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("boom");
            }
        });
        assert!(handle.is_some());
        assert!(start_background_with(Duration::from_millis(10), || {}).is_none());
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while calls.load(Ordering::SeqCst) < 3 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(calls.load(Ordering::SeqCst) >= 3);
        reset_background();
    }
}
