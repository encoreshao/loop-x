//! The browser check of a UI fix (`ui_check`): the pages a fix's handoff
//! declares in `ui_checks` are loaded in headless Chrome on the branch's own
//! dev server, and, for projects with `"verifiers": ["a11y"]`, checked with
//! axe against the target branch.

use crate::screenshot::{check_serve_command, MockRule, ServeCommand, Session};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

pub const MAX_PAGES: usize = 6;
const MAX_PATH_CHARS: usize = 200;
const MAX_LABEL_CHARS: usize = 80;
const MAX_WAIT_FOR_CHARS: usize = 200;
const MAX_SCREENS_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageSpec {
    pub label: String,
    pub path: String,
    pub wait_for: Option<String>,
    pub dark: bool,
    pub viewport: (u32, u32),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiChecks {
    pub serve: String,
    #[serde(skip)]
    pub serve_cmd: ServeCommand,
    pub origin: String,
    #[serde(default)]
    pub mocks: Vec<MockRule>,
    #[serde(default)]
    pub session: Session,
    pub pages: Vec<PageSpec>,
}

impl UiChecks {
    /// Checks a spec that came from a file again, field by field, and fills
    /// in `serve_cmd`: the `screenshot check` subprocess trusts nothing.
    pub fn revalidate(&mut self) -> Result<(), String> {
        self.serve_cmd = check_serve_command(&self.serve).map_err(|e| format!("serve: {e}"))?;
        self.origin = check_origin(&self.origin)?;
        if self.pages.is_empty() || self.pages.len() > MAX_PAGES {
            return Err(format!(
                "pages: 1 to {MAX_PAGES} pages, got {}",
                self.pages.len()
            ));
        }
        for p in &self.pages {
            check_path(&p.path)?;
            if p.label.chars().count() > MAX_LABEL_CHARS || p.label.contains(['\n', '\r']) {
                return Err("pages.label: too long or not one line".into());
            }
            if let Some(w) = &p.wait_for {
                check_wait_for(w)?;
            }
            check_viewport(p.viewport)?;
        }
        Ok(())
    }

    pub fn url(&self, page: &PageSpec) -> String {
        format!("{}{}", self.origin, page.path)
    }
}

/// `http://<localhost|127.0.0.1|[::1]>:<port>`, normalised (a trailing `/`
/// dropped, the host lowercased), or why not.
pub fn check_origin(raw: &str) -> Result<String, String> {
    let bad = |why: &str| Err(format!("origin: {why}"));
    if raw
        .chars()
        .any(|c| c == '\\' || c.is_whitespace() || c.is_control())
    {
        return bad("must be one plain URL");
    }
    let Some(rest) = raw.strip_prefix("http://") else {
        return bad("must start with http://");
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.contains(['/', '?', '#']) {
        return bad("no path, query or fragment");
    }
    if rest.contains('@') {
        return bad("no user info");
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && (!h.contains(':') || h.starts_with('[')) => (h, p),
        _ => return bad("needs an explicit port"),
    };
    let host = host.to_ascii_lowercase();
    if !matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
        return bad("host must be localhost, 127.0.0.1 or [::1]");
    }
    match port.parse::<u16>() {
        Ok(p) if p > 0 && port.chars().all(|c| c.is_ascii_digit()) => {
            Ok(format!("http://{host}:{p}"))
        }
        _ => bad("needs an explicit port"),
    }
}

fn check_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('\\')
        || path.chars().any(|c| c.is_whitespace() || c.is_control())
        || path.chars().count() > MAX_PATH_CHARS
    {
        return Err(format!(
            "pages.path: must start with one / and be at most {MAX_PATH_CHARS} plain characters"
        ));
    }
    Ok(())
}

fn check_wait_for(w: &str) -> Result<(), String> {
    if w.trim().is_empty() || w.chars().count() > MAX_WAIT_FOR_CHARS {
        return Err(format!(
            "pages.wait_for: 1 to {MAX_WAIT_FOR_CHARS} characters"
        ));
    }
    Ok(())
}

fn check_viewport((w, h): (u32, u32)) -> Result<(), String> {
    if !(320..=2560).contains(&w) || !(320..=4000).contains(&h) {
        return Err("pages.viewport: width 320-2560, height 320-4000".into());
    }
    Ok(())
}

fn parse_viewport(raw: &str) -> Result<(u32, u32), String> {
    let bad = || format!("pages.viewport: must look like 1280x800, got {raw:?}");
    let (w, h) = raw.split_once('x').ok_or_else(bad)?;
    let size = (w.parse().map_err(|_| bad())?, h.parse().map_err(|_| bad())?);
    check_viewport(size)?;
    Ok(size)
}

/// A bare `*.json` file name inside `screens`, read as `T`. The file must
/// still be inside `screens` once every symlink is resolved.
fn screens_file<T: serde::de::DeserializeOwned>(
    screens: &Path,
    field: &str,
    name: &Value,
) -> Result<T, String> {
    let bad = |why: String| format!("{field}: {why}");
    let name = name
        .as_str()
        .ok_or_else(|| bad("must be a file name".into()))?;
    let ok_name = (6..=69).contains(&name.len())
        && name.ends_with(".json")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && name != ".json"
        && !name.starts_with("..");
    if !ok_name {
        return Err(bad(format!("{name:?} is not a plain .json file name")));
    }
    let root = screens
        .canonicalize()
        .map_err(|_| bad(format!("{} does not exist", screens.display())))?;
    let file = screens
        .join(name)
        .canonicalize()
        .map_err(|_| bad(format!("{name} was not found in the screens folder")))?;
    if !file.starts_with(&root) || !file.is_file() {
        return Err(bad(format!("{name} is not inside the screens folder")));
    }
    let size = std::fs::metadata(&file).map_or(0, |m| m.len());
    if size > MAX_SCREENS_FILE_BYTES {
        return Err(bad(format!("{name} is larger than 1 MB")));
    }
    let text = std::fs::read_to_string(&file).map_err(|e| bad(e.to_string()))?;
    serde_json::from_str(&text).map_err(|e| bad(format!("{name}: {e}")))
}

/// The handoff's `ui_checks`, validated: `Ok(None)` when there is none, `Err`
/// with a one-line reason naming the field otherwise.
pub fn from_handoff(handoff: &Value, screens: &Path) -> Result<Option<UiChecks>, String> {
    let raw = match handoff.get("ui_checks") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(o)) => o,
        Some(_) => return Err("ui_checks: must be an object".into()),
    };
    let str_field = |k: &str| raw.get(k).and_then(Value::as_str);
    let serve = str_field("serve")
        .ok_or("serve: missing")?
        .trim()
        .to_string();
    let serve_cmd = check_serve_command(&serve).map_err(|e| format!("serve: {e}"))?;
    let origin = check_origin(str_field("origin").ok_or("origin: missing")?)?;
    let pages_raw = raw
        .get("pages")
        .and_then(Value::as_array)
        .ok_or("pages: missing")?;
    if pages_raw.is_empty() || pages_raw.len() > MAX_PAGES {
        return Err(format!(
            "pages: 1 to {MAX_PAGES} pages, got {}",
            pages_raw.len()
        ));
    }
    let mut pages = Vec::new();
    for p in pages_raw {
        let path = p
            .get("path")
            .and_then(Value::as_str)
            .ok_or("pages.path: missing")?;
        check_path(path)?;
        let label = p
            .get("label")
            .and_then(Value::as_str)
            .map(crate::pre_review::one_line)
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| path.to_string());
        let label = crate::pre_review::clip(&label, MAX_LABEL_CHARS);
        let wait_for = match p.get("wait_for") {
            None | Some(Value::Null) => None,
            Some(Value::String(w)) => {
                check_wait_for(w)?;
                Some(w.clone())
            }
            Some(_) => return Err("pages.wait_for: must be a CSS selector".into()),
        };
        let dark = match p.get("color_scheme") {
            None | Some(Value::Null) => false,
            Some(v) => match v.as_str() {
                Some("light") => false,
                Some("dark") => true,
                _ => return Err("pages.color_scheme: light or dark".into()),
            },
        };
        let viewport = match p.get("viewport") {
            None | Some(Value::Null) => (1280, 800),
            Some(v) => parse_viewport(v.as_str().unwrap_or_default())?,
        };
        pages.push(PageSpec {
            label,
            path: path.to_string(),
            wait_for,
            dark,
            viewport,
        });
    }
    let mocks = match raw.get("mocks") {
        None | Some(Value::Null) => Vec::new(),
        Some(name) => screens_file(screens, "mocks", name)?,
    };
    let session = match raw.get("session") {
        None | Some(Value::Null) => Session::default(),
        Some(name) => screens_file(screens, "session", name)?,
    };
    Ok(Some(UiChecks {
        serve,
        serve_cmd,
        origin,
        mocks,
        session,
        pages,
    }))
}

const UI_EXTENSIONS: [&str; 12] = [
    ".tsx", ".jsx", ".vue", ".svelte", ".html", ".erb", ".haml", ".slim", ".css", ".scss", ".sass",
    ".less",
];
const UI_DIRS: [&str; 3] = ["app/views/", "app/components/", "app/javascript/"];

/// Whether a change to these paths is a UI change that should have declared
/// pages to check.
pub fn touches_ui(changed_paths: &[String]) -> bool {
    changed_paths.iter().any(|p| {
        let lower = p.to_ascii_lowercase();
        UI_EXTENSIONS.iter().any(|e| lower.ends_with(e))
            || UI_DIRS
                .iter()
                .any(|d| lower.starts_with(d) || lower.contains(&format!("/{d}")))
    })
}

// ---- keeping the worktree as it was ----------------------------------------------------

use crate::loop_verifiers::diff_base;
use std::collections::BTreeSet;

/// git for the worktree guard: no hooks (a post-checkout hook could build or
/// install), no LFS downloads, and paths are never globs (`app/[id]/x`).
fn git_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["-c", "core.hooksPath=/dev/null", "--literal-pathspecs"])
        .args(args)
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .current_dir(dir);
    let out = crate::proc::run(&mut cmd, None, Some(std::time::Duration::from_secs(60)))
        .map_err(|e| e.to_string())?;
    if out.code == Some(0) {
        Ok(out.stdout)
    } else {
        Err(out.stderr.trim().to_string())
    }
}

/// `git status --porcelain` entries (`XY path`), untracked files listed one
/// by one. Ignored files are never listed, so they are never touched.
fn status_entries(dir: &Path) -> Result<BTreeSet<String>, String> {
    let out = git_in(
        dir,
        &["status", "--porcelain", "--untracked-files=all", "-z"],
    )?;
    let mut entries = BTreeSet::new();
    let mut parts = out.split('\0').filter(|p| !p.is_empty());
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        if matches!(&entry[..1], "R" | "C") {
            parts.next();
        }
        entries.insert(entry.to_string());
    }
    Ok(entries)
}

/// The worktree's status before a check, so whatever the check changes can
/// be put back.
pub struct TreeSnapshot {
    before: BTreeSet<String>,
}

impl TreeSnapshot {
    pub fn take(dir: &Path) -> Result<Self, String> {
        Ok(Self {
            before: status_entries(dir)?,
        })
    }

    /// Puts back tracked files the check changed and removes untracked files
    /// it created; a path already changed or staged before the snapshot, and
    /// every ignored file, stays as it is. Returns what it fixed.
    pub fn restore(&self, dir: &Path) -> Vec<String> {
        let Ok(now) = status_entries(dir) else {
            return Vec::new();
        };
        let dirty_before: BTreeSet<&str> = self.before.iter().map(|e| &e[3..]).collect();
        let mut fixed = Vec::new();
        for entry in now.difference(&self.before) {
            let path = &entry[3..];
            if dirty_before.contains(path) {
                continue;
            }
            if entry.starts_with("??") {
                let full = dir.join(path);
                if std::fs::remove_file(&full).is_ok() {
                    fixed.push(path.to_string());
                    let mut parent = full.parent();
                    while let Some(p) = parent.filter(|p| *p != dir) {
                        if std::fs::remove_dir(p).is_err() {
                            break;
                        }
                        parent = p.parent();
                    }
                }
            } else if git_in(
                dir,
                &[
                    "restore",
                    "--source=HEAD",
                    "--staged",
                    "--worktree",
                    "--",
                    path,
                ],
            )
            .is_ok()
            {
                fixed.push(path.to_string());
            }
        }
        fixed
    }

    /// True when the worktree's status is what it was at the snapshot.
    pub fn unchanged(&self, dir: &Path) -> bool {
        status_entries(dir).is_ok_and(|now| now == self.before)
    }
}

/// Restores a [`TreeSnapshot`] even when the check in between panics.
struct RestoreOnDrop<'a> {
    snapshot: Option<TreeSnapshot>,
    dir: &'a Path,
}

impl RestoreOnDrop<'_> {
    /// Restores now; `Err` lists what is still different afterwards.
    fn finish(mut self) -> Result<(), String> {
        let snapshot = self.snapshot.take().expect("finished once");
        snapshot.restore(self.dir);
        if snapshot.unchanged(self.dir) {
            return Ok(());
        }
        let now = status_entries(self.dir).unwrap_or_default();
        let left: Vec<String> = now.difference(&snapshot.before).take(5).cloned().collect();
        Err(format!(
            "the check left changes behind: {}",
            left.join(", ")
        ))
    }
}

impl Drop for RestoreOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(s) = self.snapshot.take() {
            s.restore(self.dir);
        }
    }
}

/// Where a switched [`BaseCheckout`] records the branch to return to, inside
/// the worktree's own git dir, so a runner killed mid-baseline can be undone
/// by [`recover_base_checkout`].
const BASE_MARKER: &str = "loopx-ui-check-base";

fn marker_path(dir: &Path) -> Result<PathBuf, String> {
    let git_dir = git_in(dir, &["rev-parse", "--absolute-git-dir"])?;
    Ok(PathBuf::from(git_dir.trim()).join(BASE_MARKER))
}

/// Undoes a browser check that was interrupted while the worktree sat on the
/// merge-base. Only a worktree still detached at the recorded base is reset;
/// anything else just loses the marker, so recovery never destroys work and
/// never blocks the issue twice. `Ok(None)` when there was no marker.
pub fn recover_base_checkout(dir: &Path) -> Result<Option<String>, String> {
    let marker = marker_path(dir)?;
    let Ok(text) = std::fs::read_to_string(&marker) else {
        return Ok(None);
    };
    let drop_marker = || {
        let _ = std::fs::remove_file(&marker);
    };
    let lines: Vec<&str> = text.lines().collect();
    let [branch, sha, base, ..] = lines[..] else {
        drop_marker();
        return Ok(None);
    };
    let on_branch = git_in(dir, &["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty());
    let head = git_in(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    if on_branch.as_deref() == Some(branch) {
        drop_marker();
        return Ok(Some(format!(
            "{} is already on {branch}; dropped a stale browser-check marker",
            dir.display()
        )));
    }
    if on_branch.is_some() || head != base {
        drop_marker();
        return Ok(Some(format!(
            "{} is at {head}, not at the browser check's base {base}; left as it is",
            dir.display()
        )));
    }
    let guard = BaseCheckout {
        dir: dir.to_path_buf(),
        branch: branch.to_string(),
        sha: sha.to_string(),
        done: true,
    };
    let restored = guard.restore();
    // Reported once; a marker that can't be acted on must not block every run.
    drop_marker();
    restored?;
    Ok(Some(format!(
        "put {} back on {branch} after an interrupted browser check",
        dir.display()
    )))
}

/// An ignored path in the worktree that the merge-base tracks: switching
/// there would overwrite it, and switching back would delete it.
fn ignored_clash(dir: &Path, base: &str) -> Result<Option<String>, String> {
    let ignored = git_in(
        dir,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
    )?;
    let ignored: Vec<&str> = ignored.split('\0').filter(|p| !p.is_empty()).collect();
    if ignored.is_empty() {
        return Ok(None);
    }
    let tracked = git_in(dir, &["ls-tree", "-r", "-z", "--name-only", base])?;
    Ok(tracked
        .split('\0')
        .filter(|p| !p.is_empty())
        .find(|t| {
            ignored
                .iter()
                .any(|i| t == i || (i.ends_with('/') && t.starts_with(i)))
        })
        .map(str::to_string))
}

/// The issue's worktree switched to the target branch's merge-base for the
/// accessibility baseline. Dropping it (or [`BaseCheckout::restored`]) puts
/// the branch back exactly as it was.
pub struct BaseCheckout {
    dir: std::path::PathBuf,
    branch: String,
    sha: String,
    done: bool,
}

impl BaseCheckout {
    /// Refuses unless the project uses a worktree, the tree is clean and
    /// HEAD is a branch.
    pub fn switch(dir: &Path, target: &str, uses_worktree: bool) -> Result<Self, String> {
        if !uses_worktree {
            return Err("the project works in its own checkout".into());
        }
        if !status_entries(dir)?.is_empty() {
            return Err("the worktree has uncommitted changes".into());
        }
        let branch = git_in(dir, &["symbolic-ref", "--short", "-q", "HEAD"])
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|b| !b.is_empty())
            .ok_or("HEAD is not on a branch")?;
        let sha = git_in(dir, &["rev-parse", "HEAD"])?.trim().to_string();
        let base = diff_base(dir, target)?;
        if let Some(path) = ignored_clash(dir, &base)? {
            return Err(format!(
                "the merge-base tracks {path}, which is ignored here and would be overwritten"
            ));
        }
        let marker = marker_path(dir)?;
        std::fs::write(&marker, format!("{branch}\n{sha}\n{base}\n"))
            .map_err(|e| format!("cannot write {}: {e}", marker.display()))?;
        let guard = Self {
            dir: dir.to_path_buf(),
            branch,
            sha,
            done: false,
        };
        git_in(dir, &["switch", "--detach", "--quiet", &base])
            .map_err(|e| format!("cannot switch to the merge-base: {e}"))?;
        Ok(guard)
    }

    pub fn restored(mut self) -> Result<(), String> {
        self.done = true;
        self.restore()
    }

    fn restore(&self) -> Result<(), String> {
        let dir = &self.dir;
        if git_in(dir, &["switch", "--quiet", &self.branch]).is_err() {
            // The tree was clean before the switch, so nothing is lost.
            git_in(dir, &["switch", "--quiet", "-f", &self.branch])
                .map_err(|e| format!("cannot switch back to {}: {e}", self.branch))?;
        }
        let clean = TreeSnapshot {
            before: BTreeSet::new(),
        };
        clean.restore(dir);
        let sha = git_in(dir, &["rev-parse", "HEAD"])?.trim().to_string();
        if sha != self.sha {
            return Err(format!("HEAD is {sha}, expected {}", self.sha));
        }
        let left = status_entries(dir)?;
        if !left.is_empty() {
            return Err(format!(
                "the worktree is not clean after the check: {}",
                left.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        let _ = std::fs::remove_file(marker_path(dir)?);
        Ok(())
    }
}

impl Drop for BaseCheckout {
    fn drop(&mut self) {
        if !self.done {
            self.done = true;
            if let Err(e) = self.restore() {
                eprintln!("ui_check: restoring {}: {e}", self.dir.display());
            }
        }
    }
}

// ---- the verifier ----------------------------------------------------------------------

use crate::loop_result::VerificationResult;
use crate::loop_runtime::LoopContext;
use crate::loop_verifiers::{IssueWork, Verifier, WorkDir};
use crate::screenshot::{already_served, problems, PageReport, PageVerdict, Problem};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The `screenshot check` subprocess's own deadline.
pub const CHECK_DEADLINE: Duration = Duration::from_secs(300);
/// How long the verifier waits for one `screenshot check` before SIGTERM.
pub const CHECK_WAIT: Duration = Duration::from_secs(330);
const KILL_GRACE: Duration = Duration::from_secs(10);
const MAX_LINES: usize = 10;
const MAX_LINE_CHARS: usize = 200;
const MAX_NEW_EVIDENCE: usize = 20;

/// Runs one `screenshot check` on `spec` in `cwd` and returns its report.
pub trait UiCheckRunner: Send + Sync {
    fn run(
        &self,
        spec: &Value,
        cwd: &Path,
        path_env: &str,
        deadline: Duration,
    ) -> Result<Value, String>;
}

/// `loopx screenshot check` as a child process in its own group: SIGTERM at
/// the deadline (its handler stops the dev server and Chrome), SIGKILL to the
/// group `grace` later.
pub struct SubprocessRunner {
    pub exe: Option<PathBuf>,
    pub grace: Duration,
    /// Extra environment for the child, on top of `PATH`.
    pub env: Vec<(String, String)>,
}

impl Default for SubprocessRunner {
    fn default() -> Self {
        Self {
            exe: None,
            grace: KILL_GRACE,
            env: Vec::new(),
        }
    }
}

impl UiCheckRunner for SubprocessRunner {
    fn run(
        &self,
        spec: &Value,
        cwd: &Path,
        path_env: &str,
        deadline: Duration,
    ) -> Result<Value, String> {
        use std::os::unix::process::CommandExt;
        let exe = match &self.exe {
            Some(e) => e.clone(),
            None => std::env::current_exe().map_err(|e| e.to_string())?,
        };
        let tmp = tempfile::Builder::new()
            .prefix("loopx-uicheck-")
            .tempdir()
            .map_err(|e| format!("cannot make a temp dir: {e}"))?;
        let (spec_file, out) = (tmp.path().join("spec.json"), tmp.path().join("report.json"));
        std::fs::write(&spec_file, spec.to_string()).map_err(|e| e.to_string())?;
        let stderr = std::fs::File::create(tmp.path().join("stderr")).map_err(|e| e.to_string())?;
        let mut child = std::process::Command::new(exe)
            .args(["screenshot", "check", "--spec"])
            .arg(&spec_file)
            .arg("--out")
            .arg(&out)
            .current_dir(cwd)
            .env("PATH", path_env)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(stderr)
            .spawn()
            .map_err(|e| format!("cannot start the browser check: {e}"))?;
        let pid = child.id() as i32;
        let until = Instant::now() + deadline;
        let mut timed_out = false;
        let status = loop {
            if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
                break Some(s);
            }
            if Instant::now() >= until {
                timed_out = true;
                // SAFETY: signalling the child this function spawned.
                unsafe { libc::kill(pid, libc::SIGTERM) };
                let grace_until = Instant::now() + self.grace;
                while Instant::now() < grace_until {
                    if child.try_wait().map_err(|e| e.to_string())?.is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        if timed_out {
            return Err(format!(
                "the browser check did not finish within {}s",
                deadline.as_secs()
            ));
        }
        match std::fs::read_to_string(&out) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("bad report: {e}")),
            Err(_) => {
                let err = std::fs::read_to_string(tmp.path().join("stderr")).unwrap_or_default();
                Err(crate::pre_review::clip(
                    &format!(
                        "the browser check exited ({}) without a report: {}",
                        status.map(|s| s.to_string()).unwrap_or_default(),
                        err.trim()
                    ),
                    MAX_LINE_CHARS,
                ))
            }
        }
    }
}

/// Dev-server output with secrets taken out: URL passwords, then anything
/// the project-profile scan would redact.
fn redact_output(text: &str) -> String {
    static URL_PASSWORD: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"([A-Za-z][A-Za-z0-9+.-]*://[^\s:/@]+):[^\s@/]+@").expect("static regex")
    });
    let text = URL_PASSWORD.replace_all(text, "$1:[redacted]@");
    crate::profile_scan::redact(&text).0
}

/// The project's `"verifiers": [...]` opt-ins that this verifier knows.
fn a11y_enabled(project: &Value) -> bool {
    let mut on = false;
    for v in project
        .get("verifiers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match v.as_str() {
            Some("a11y") => on = true,
            Some("perf") => eprintln!("ui_check: verifiers \"perf\" is reserved and not built yet"),
            other => eprintln!("ui_check: unknown verifiers entry {other:?} ignored"),
        }
    }
    on
}

/// Every path the branch changed since it left the target branch.
fn changed_paths(dir: &Path, target: &str) -> Result<Vec<String>, String> {
    let base = diff_base(dir, target)?;
    let out = git_in(
        dir,
        &["diff", "--name-only", "-z", &format!("{base}..HEAD")],
    )?;
    Ok(out
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect())
}

pub struct UiCheckVerifier {
    pub name: String,
    pub work: IssueWork,
    runner: Arc<dyn UiCheckRunner>,
    repo_root: PathBuf,
    find_chrome: fn() -> Option<PathBuf>,
    /// The worktree, its branch and HEAD as the check found them.
    pin: std::cell::RefCell<Option<(PathBuf, Option<String>, String)>>,
}

impl UiCheckVerifier {
    pub fn new(
        name: &str,
        work: IssueWork,
        runner: Arc<dyn UiCheckRunner>,
        repo_root: PathBuf,
    ) -> Self {
        Self {
            name: name.into(),
            work,
            runner,
            repo_root,
            find_chrome: crate::linkedin_cdp::find_chrome,
            pin: Default::default(),
        }
    }

    pub fn with_find_chrome(mut self, f: fn() -> Option<PathBuf>) -> Self {
        self.find_chrome = f;
        self
    }
}

fn vacuous(name: &str, why: &str, undeclared_ui: bool) -> VerificationResult {
    VerificationResult {
        name: name.into(),
        passed: true,
        exit_code: None,
        duration_ms: 0,
        output: why.into(),
        evidence: json!({"vacuous": true, "gate": "ui", "undeclared_ui": undeclared_ui}),
    }
}

fn error(name: &str, why: String, extra: Value) -> VerificationResult {
    let mut evidence = json!({"error": true, "gate": "ui"});
    if let (Some(e), Value::Object(x)) = (evidence.as_object_mut(), extra) {
        e.extend(x);
    }
    VerificationResult {
        name: name.into(),
        passed: false,
        exit_code: None,
        duration_ms: 0,
        output: crate::pre_review::clip(&why, MAX_LINE_CHARS),
        evidence,
    }
}

/// One declared page as the branch run saw it.
struct PageOutcome {
    label: String,
    path: String,
    status: &'static str,
    problems: Vec<Problem>,
    violations: Option<Vec<crate::a11y::Violation>>,
}

fn page_outcomes(report: &Value, pages: &[PageSpec]) -> Vec<PageOutcome> {
    let entries = report["pages"].as_array().cloned().unwrap_or_default();
    pages
        .iter()
        .map(|spec| {
            let entry = entries
                .iter()
                .find(|e| e["path"] == json!(spec.path) && e["label"] == json!(spec.label));
            let mut out = PageOutcome {
                label: spec.label.clone(),
                path: spec.path.clone(),
                status: "failed",
                problems: Vec::new(),
                violations: None,
            };
            let Some(entry) = entry else {
                return out;
            };
            let Ok(r) = serde_json::from_value::<PageReport>(entry["report"].clone()) else {
                return out;
            };
            match problems(&r, &spec.path) {
                PageVerdict::Skipped(why) => {
                    out.status = "skipped";
                    out.problems.push(Problem {
                        rule: "skipped".into(),
                        detail: why,
                        blocking: false,
                    });
                }
                PageVerdict::Checked(p) => {
                    out.status = if p.is_empty() { "ok" } else { "problems" };
                    if r.loaded && r.status.is_none_or(|s| s < 400) {
                        out.violations = r.axe.and_then(Result::ok);
                    }
                    out.problems = p;
                }
            }
            out
        })
        .collect()
}

/// The base run's violations per page path; `None` for a page the base
/// could not measure. A page the base answers with 404 is new on the branch.
fn base_violations(report: &Value, pages: &[PageSpec]) -> Vec<Option<Vec<crate::a11y::Violation>>> {
    let entries = report["pages"].as_array().cloned().unwrap_or_default();
    pages
        .iter()
        .map(|spec| {
            let entry = entries
                .iter()
                .find(|e| e["path"] == json!(spec.path) && e["label"] == json!(spec.label))?;
            let r: PageReport = serde_json::from_value(entry["report"].clone()).ok()?;
            if r.loaded && r.status == Some(404) {
                return Some(Vec::new());
            }
            match problems(&r, &spec.path) {
                PageVerdict::Checked(p) if !p.iter().any(|p| p.rule == "page-load") => {
                    r.axe.and_then(Result::ok)
                }
                _ => None,
            }
        })
        .collect()
}

impl UiCheckVerifier {
    /// [`Self::check_inner`], then a hard failure if the worktree did not end
    /// where it started: every later gate would look at the wrong commit.
    fn check(&self) -> VerificationResult {
        let result = self.check_inner();
        let Some((dir, branch, sha)) = self.pin.borrow_mut().take() else {
            return result;
        };
        let now_branch = git_in(&dir, &["symbolic-ref", "--short", "-q", "HEAD"])
            .ok()
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty());
        let now = git_in(&dir, &["rev-parse", "HEAD"])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if now_branch == branch && now == sha {
            return result;
        }
        let was = format!("{} {sha}", branch.as_deref().unwrap_or("(detached)"));
        let output = format!(
            "the browser check left the worktree at {} {now} instead of {was}",
            now_branch.as_deref().unwrap_or("(detached)")
        );
        eprintln!("ui_check: {output}");
        VerificationResult {
            name: self.name.clone(),
            passed: false,
            exit_code: None,
            duration_ms: result.duration_ms,
            output: crate::pre_review::clip(&output, MAX_LINE_CHARS),
            evidence: json!({"gate": "ui", "error": true, "worktree_moved": true,
                             "hard_failure": true, "restore_failed": true}),
        }
    }

    fn check_inner(&self) -> VerificationResult {
        let start = Instant::now();
        let name = &self.name;
        if self.work.handoff_path.is_none() {
            return vacuous(name, "the browser check runs only in gate mode", false);
        }
        let (dir, project, handoff) = match self.work.resolve() {
            Ok(WorkDir::Vacuous(why)) => return vacuous(name, why, false),
            Ok(WorkDir::Ready {
                dir,
                project,
                handoff,
            }) => (dir, project, handoff.unwrap_or(Value::Null)),
            Err(e) => return error(name, e, json!({})),
        };
        if crate::config::use_worktree(&project) {
            match recover_base_checkout(&dir) {
                Ok(Some(note)) => eprintln!("ui_check: {note}"),
                Ok(None) => {}
                Err(e) => {
                    return error(
                        name,
                        format!("the worktree is still on the merge-base of an earlier check: {e}"),
                        json!({"restore_failed": true}),
                    )
                }
            }
        }
        if let Ok(sha) = git_in(&dir, &["rev-parse", "HEAD"]) {
            let branch = git_in(&dir, &["symbolic-ref", "--short", "-q", "HEAD"])
                .ok()
                .map(|b| b.trim().to_string())
                .filter(|b| !b.is_empty());
            *self.pin.borrow_mut() = Some((dir.clone(), branch, sha.trim().to_string()));
        }
        let target = handoff
            .get("target_branch")
            .and_then(Value::as_str)
            .unwrap_or("main")
            .to_string();
        let screens = crate::mr_screenshots::screens_dir(
            &self.repo_root,
            &self.work.alias,
            &self.work.issue_iid,
        );
        let checks = match from_handoff(&handoff, &screens) {
            Err(why) => {
                let why =
                    crate::pre_review::clip(&format!("ui_checks ignored: {why}"), MAX_LINE_CHARS);
                return vacuous(name, &why, false);
            }
            Ok(None) => {
                let undeclared = changed_paths(&dir, &target).is_ok_and(|p| touches_ui(&p));
                let why = if undeclared {
                    "the fix changed UI files but declared no pages - browser check skipped"
                } else {
                    "no pages declared - browser check skipped"
                };
                return vacuous(name, why, undeclared);
            }
            Ok(Some(c)) => c,
        };
        let uses_worktree = crate::config::use_worktree(&project);
        if !uses_worktree {
            return vacuous(name, "browser check needs a worktree", false);
        }
        if (self.find_chrome)().is_none() {
            return vacuous(
                name,
                "Google Chrome was not found - browser check skipped",
                false,
            );
        }
        // Issues run in parallel: one check per origin at a time, held for the
        // branch and the base run, and never another issue's dev server.
        let busy = || {
            let why = format!(
                "another check or screenshot is using {} - browser check skipped",
                checks.origin
            );
            vacuous(name, &why, false)
        };
        let port = checks.origin.rsplit(':').next().unwrap_or("0");
        let screens_root = self.repo_root.join("outputs").join("screens");
        let _ = std::fs::create_dir_all(&screens_root);
        let lock_file = screens_root.join(format!(".ui-check-{port}.lock"));
        let _origin_lock = match crate::file_lock::FileLock::try_exclusive(&lock_file) {
            Ok(Some(lock)) => lock,
            Ok(None) => return busy(),
            Err(e) => {
                return error(
                    name,
                    format!("cannot lock {}: {e}", lock_file.display()),
                    json!({}),
                )
            }
        };
        if already_served(&format!("{}/", checks.origin)) {
            return busy();
        }
        let a11y = a11y_enabled(&project);
        let path_env = crate::node_env::path_for(
            &dir,
            &std::env::var("PATH").unwrap_or_default(),
            &crate::node_env::Toolchains::from_env(),
        );
        let mut spec = serde_json::to_value(&checks).unwrap_or(Value::Null);
        spec["cwd"] = json!(dir);
        spec["axe"] = json!(a11y);
        spec["deadline_s"] = json!(CHECK_DEADLINE.as_secs());

        let leftover: std::cell::RefCell<Option<String>> = Default::default();
        let run = |spec: &Value| -> Result<Value, String> {
            let guard = RestoreOnDrop {
                snapshot: Some(TreeSnapshot::take(&dir)?),
                dir: &dir,
            };
            let result = self.runner.run(spec, &dir, &path_env, CHECK_WAIT);
            if let Err(e) = guard.finish() {
                leftover.borrow_mut().get_or_insert(e);
            }
            result
        };
        let branch = match run(&spec) {
            Ok(r) => r,
            Err(e) => return error(name, format!("browser check could not run: {e}"), json!({})),
        };
        let tail = branch["serve"]["tail"].as_str().map(redact_output);
        let serve = json!({
            "command": checks.serve,
            "started": branch["serve"]["started"] == json!(true),
            "tail": tail,
        });
        if branch["serve"]["started"] != json!(true) {
            eprintln!(
                "ui_check: the dev server did not start: {}",
                tail.as_deref().unwrap_or("no output")
            );
            return error(
                name,
                "dev server did not start; see the run log".into(),
                json!({"serve": serve, "undeclared_ui": false}),
            );
        }
        let outcomes = page_outcomes(&branch, &checks.pages);
        if outcomes.iter().all(|o| o.status == "failed") {
            return error(
                name,
                "no declared page could be checked in Chrome".into(),
                json!({"serve": serve, "undeclared_ui": false}),
            );
        }

        let mut a11y_ev = json!({"enabled": a11y});
        let mut new_blocking: Vec<(String, crate::a11y::NewViolation)> = Vec::new();
        let mut restore_failed = None;
        if a11y {
            a11y_ev["axe_version"] = json!(crate::a11y::AXE_VERSION);
            let any = outcomes
                .iter()
                .any(|o| o.violations.as_ref().is_some_and(|v| !v.is_empty()));
            let (baseline, base): (String, Option<Vec<Option<Vec<crate::a11y::Violation>>>>) =
                if !any {
                    ("skipped_clean".into(), None)
                } else {
                    match BaseCheckout::switch(&dir, &target, uses_worktree) {
                        Err(why) => (format!("unavailable ({why})"), None),
                        Ok(guard) => {
                            let result = run(&spec);
                            if let Err(e) = guard.restored() {
                                restore_failed = Some(e);
                            }
                            match result {
                                Err(e) => (format!("unavailable ({e})"), None),
                                Ok(r) if r["serve"]["started"] != json!(true) => (
                                    "unavailable (the target branch's dev server did not start)"
                                        .into(),
                                    None,
                                ),
                                Ok(r) => {
                                    ("measured".into(), Some(base_violations(&r, &checks.pages)))
                                }
                            }
                        }
                    }
                };
            let (mut new_all, mut existing) = (Vec::new(), 0usize);
            for (i, o) in outcomes.iter().enumerate() {
                let Some(head) = &o.violations else { continue };
                let base_page = base.as_ref().and_then(|b| b.get(i).cloned().flatten());
                let fresh = match &base_page {
                    Some(b) => crate::a11y::new_violations(head, b),
                    None => Vec::new(),
                };
                existing += head.len() - fresh.len().min(head.len());
                for v in fresh {
                    if crate::a11y::blocks(&v) {
                        new_blocking.push((o.label.clone(), v.clone()));
                    }
                    new_all.push(json!({"page": o.label, "id": v.id, "impact": v.impact,
                                        "help": v.help, "targets": v.targets}));
                }
            }
            a11y_ev["baseline"] = json!(crate::pre_review::clip(&baseline, MAX_LINE_CHARS));
            a11y_ev["new_count"] = json!(new_all.len());
            new_all.truncate(MAX_NEW_EVIDENCE);
            a11y_ev["new"] = json!(new_all);
            a11y_ev["existing"] = json!(existing);
        }

        let mut blocking = Vec::new();
        let mut lines = Vec::new();
        for o in &outcomes {
            for p in o.problems.iter().filter(|p| p.blocking) {
                blocking.push(json!({"rule": p.rule, "page": o.label, "kind": "smoke"}));
                lines.push(format!("{}: {} {}", o.label, p.rule, p.detail));
            }
        }
        for (label, v) in &new_blocking {
            let target = v.targets.first().cloned().unwrap_or_default();
            blocking.push(json!({"rule": v.id, "page": label, "kind": "a11y",
                                 "impact": v.impact, "target": target}));
            lines.push(format!(
                "{label}: {} ({}) {target}",
                v.id,
                v.impact.as_str()
            ));
        }
        let pages_ev: Vec<Value> = outcomes
            .iter()
            .map(|o| {
                json!({"label": o.label, "path": o.path, "status": o.status,
                       "problems": o.problems.iter().map(|p| json!({"rule": p.rule, "detail": p.detail, "blocking": p.blocking})).collect::<Vec<_>>()})
            })
            .collect();
        let mut evidence = json!({
            "gate": "ui",
            "pages": pages_ev,
            "blocking": blocking,
            "a11y": a11y_ev,
            "serve": serve,
            "undeclared_ui": false,
        });
        if branch["deadline_hit"] == json!(true) {
            evidence["deadline_hit"] = json!(true);
        }
        let duration_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
        if let Some(e) = restore_failed.or(leftover.into_inner()) {
            eprintln!("ui_check: the worktree was not restored after the check: {e}");
            evidence["restore_failed"] = json!(true);
            evidence["error"] = json!(true);
            return VerificationResult {
                name: name.clone(),
                passed: false,
                exit_code: None,
                duration_ms,
                output: crate::pre_review::clip(
                    &format!("the worktree was not restored after the check: {e}"),
                    MAX_LINE_CHARS,
                ),
                evidence,
            };
        }
        let passed = blocking.is_empty();
        let output = if passed {
            let clean = outcomes.iter().filter(|o| o.status == "ok").count();
            let mut summary = vec![format!("{clean} page(s) loaded clean")];
            for o in outcomes.iter().filter(|o| o.status != "ok") {
                let why = o
                    .problems
                    .first()
                    .map_or("could not be checked".to_string(), |p| {
                        format!("{} {}", p.rule, p.detail)
                    });
                summary.push(format!("{}: {why}", o.label));
            }
            summary
        } else {
            lines
        };
        VerificationResult {
            name: name.clone(),
            passed,
            exit_code: Some(if passed { 0 } else { 1 }),
            duration_ms,
            output: output
                .iter()
                .take(MAX_LINES)
                .map(|l| crate::pre_review::clip(l, MAX_LINE_CHARS))
                .collect::<Vec<_>>()
                .join("\n"),
            evidence,
        }
    }
}

impl Verifier for UiCheckVerifier {
    fn verify(&self, _context: &LoopContext<'_>) -> VerificationResult {
        self.check()
    }

    fn skip_after_failure(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn good() -> Value {
        json!({"action": "fix", "ui_checks": {
            "serve": "yarn dev", "origin": "http://localhost:5173",
            "pages": [{"label": "Order form", "path": "/orders/new", "wait_for": "#order-form",
                       "color_scheme": "dark", "viewport": "1024x768"}]}})
    }

    fn with(edit: impl FnOnce(&mut Value)) -> Value {
        let mut h = good();
        edit(&mut h["ui_checks"]);
        h
    }

    #[test]
    fn handoff_ui_checks_are_validated_field_by_field() {
        let screens = tempfile::tempdir().unwrap();
        let ok = from_handoff(&good(), screens.path()).unwrap().unwrap();
        assert_eq!(ok.origin, "http://localhost:5173");
        assert_eq!(ok.serve_cmd.1, vec!["yarn", "dev"]);
        assert_eq!(
            ok.pages,
            vec![PageSpec {
                label: "Order form".into(),
                path: "/orders/new".into(),
                wait_for: Some("#order-form".into()),
                dark: true,
                viewport: (1024, 768),
            }]
        );
        let cases: Vec<(&str, Value)> = vec![
            ("serve", with(|u| u["serve"] = json!("rm -rf /"))),
            ("serve", with(|u| u["serve"] = json!("yarn dev && x"))),
            (
                "origin",
                with(|u| u["origin"] = json!("https://example.com")),
            ),
            (
                "origin",
                with(|u| u["origin"] = json!("http://example.com:3000")),
            ),
            ("origin", with(|u| u["origin"] = json!("http://localhost"))),
            (
                "origin",
                with(|u| u["origin"] = json!("http://user@localhost:3000")),
            ),
            (
                "origin",
                with(|u| u["origin"] = json!("http://localhost:3000/app")),
            ),
            (
                "origin",
                with(|u| u["origin"] = json!("http://localhost:3000\\@evil.test")),
            ),
            (
                "pages.path",
                with(|u| u["pages"][0]["path"] = json!("//evil")),
            ),
            ("pages.path", with(|u| u["pages"][0]["path"] = json!("a/b"))),
            (
                "pages.path",
                with(|u| u["pages"][0]["path"] = json!("/a\\b")),
            ),
            (
                "pages",
                with(|u| u["pages"] = json!(vec![json!({"path": "/"}); 7])),
            ),
            ("pages", with(|u| u["pages"] = json!([]))),
            (
                "pages.viewport",
                with(|u| u["pages"][0]["viewport"] = json!("10x10")),
            ),
            (
                "pages.color_scheme",
                with(|u| u["pages"][0]["color_scheme"] = json!("sepia")),
            ),
            (
                "pages.wait_for",
                with(|u| u["pages"][0]["wait_for"] = json!("x".repeat(201))),
            ),
            ("ui_checks", json!({"ui_checks": "yes"})),
        ];
        for (field, h) in cases {
            let err = from_handoff(&h, screens.path()).unwrap_err();
            assert!(err.starts_with(field), "{field}: {err}");
            assert!(!err.contains('\n'), "{err}");
        }
        let ok = with(|u| {
            u["origin"] = json!("http://127.0.0.1:3000/");
            u["pages"] = json!([{"path": "/a?tab=2"}, {"path": "/b", "color_scheme": "light"}]);
        });
        let ok = from_handoff(&ok, screens.path()).unwrap().unwrap();
        assert_eq!(ok.origin, "http://127.0.0.1:3000");
        assert_eq!(ok.pages[0].label, "/a?tab=2");
        assert_eq!(ok.pages[0].viewport, (1280, 800));
        assert!(!ok.pages[1].dark);
        assert_eq!(ok.url(&ok.pages[0]), "http://127.0.0.1:3000/a?tab=2");
        let v6 = with(|u| u["origin"] = json!("http://[::1]:3000"));
        assert!(from_handoff(&v6, screens.path()).is_ok());
    }

    #[test]
    fn mocks_and_session_must_stay_inside_the_screens_dir() {
        let root = tempfile::tempdir().unwrap();
        let screens = root.path().join("screens");
        std::fs::create_dir(&screens).unwrap();
        std::fs::write(root.path().join("x.json"), "[]").unwrap();
        std::fs::write(
            screens.join("mocks.json"),
            r#"[{"url_pattern": "/api/me", "json": {"name": "Test"}}]"#,
        )
        .unwrap();
        std::fs::write(
            screens.join("session.json"),
            r#"{"local_storage": {"token": "fake"}}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(root.path().join("x.json"), screens.join("out.json")).unwrap();
        let ok = with(|u| {
            u["mocks"] = json!("mocks.json");
            u["session"] = json!("session.json");
        });
        let ok = from_handoff(&ok, &screens).unwrap().unwrap();
        assert_eq!(ok.mocks[0].url_pattern, "/api/me");
        assert_eq!(ok.session.local_storage["token"], "fake");
        for (field, name) in [
            ("mocks", json!("../x.json")),
            ("mocks", json!("/etc/x.json")),
            ("mocks", json!("out.json")),
            ("session", json!("out.json")),
            ("mocks", json!("missing.json")),
            ("mocks", json!("mocks.txt")),
            ("mocks", json!(3)),
        ] {
            let h = with(|u| u[field] = name.clone());
            let err = from_handoff(&h, &screens).unwrap_err();
            assert!(err.starts_with(field), "{name}: {err}");
        }
        let h = with(|u| u["session"] = json!("mocks.json"));
        assert!(from_handoff(&h, &screens)
            .unwrap_err()
            .starts_with("session"));
    }

    #[test]
    fn absent_ui_checks_is_none_and_labels_are_flattened() {
        let screens = tempfile::tempdir().unwrap();
        assert!(from_handoff(&json!({"action": "fix"}), screens.path())
            .unwrap()
            .is_none());
        assert!(from_handoff(&json!({"ui_checks": null}), screens.path())
            .unwrap()
            .is_none());
        let h = with(|u| {
            u["pages"][0]["label"] = json!(format!("/quick\n  action {}", "x".repeat(200)))
        });
        let got = from_handoff(&h, screens.path()).unwrap().unwrap();
        let label = &got.pages[0].label;
        assert!(label.starts_with("/quick action x"), "{label}");
        assert!(label.chars().count() <= 80 && !label.contains('\n'));
    }

    #[test]
    fn revalidate_refuses_a_tampered_spec() {
        let screens = tempfile::tempdir().unwrap();
        let ok = from_handoff(&good(), screens.path()).unwrap().unwrap();
        let text = serde_json::to_string(&ok).unwrap();
        let mut back: UiChecks = serde_json::from_str(&text).unwrap();
        back.revalidate().unwrap();
        assert_eq!(back.serve_cmd, ok.serve_cmd);
        let mut bad = back.clone();
        bad.serve = "node evil.js".into();
        assert!(bad.revalidate().is_err());
        let mut bad = back.clone();
        bad.origin = "http://example.com:80".into();
        assert!(bad.revalidate().is_err());
        let mut bad = back.clone();
        bad.pages[0].path = "//x".into();
        assert!(bad.revalidate().is_err());
        let mut bad = back;
        bad.pages = vec![bad.pages[0].clone(); 7];
        assert!(bad.revalidate().is_err());
    }

    #[test]
    fn touches_ui_matches_view_and_style_files_only() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        for yes in [
            "src/App.tsx",
            "web/Button.JSX",
            "a.vue",
            "b.svelte",
            "public/index.html",
            "app/views/orders/new.html.erb",
            "app/views/x.haml",
            "x.slim",
            "styles/a.css",
            "a.scss",
            "a.sass",
            "a.less",
            "app/components/card_component.rb",
            "app/javascript/controllers/x_controller.js",
            "engines/shop/app/views/x.rb",
        ] {
            assert!(touches_ui(&s(&[yes])), "{yes}");
        }
        for no in [
            "app/models/order.rb",
            "src/api.ts",
            "README.md",
            "Gemfile.lock",
            "spec/views_helper.rb",
        ] {
            assert!(!touches_ui(&s(&[no])), "{no}");
        }
        assert!(!touches_ui(&[]));
    }

    pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
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
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A clone of a bare `origin` whose `main` has `page.html` (v1) and an
    /// ignored `node_modules/`, checked out on `feature` with one commit
    /// that changes the page and adds `new.html`.
    pub(crate) fn repo_on_feature() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let origin = root.path().join("origin.git");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        let seed = root.path().join("seed");
        std::fs::create_dir(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "main"]);
        std::fs::write(seed.join("page.html"), "v1\n").unwrap();
        std::fs::write(seed.join(".gitignore"), "node_modules/\n").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-qm", "base"]);
        git(
            &seed,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&seed, &["push", "-q", "origin", "main"]);
        let work = root.path().join("work");
        git(
            root.path(),
            &["clone", "-q", origin.to_str().unwrap(), "work"],
        );
        git(&work, &["switch", "-q", "-c", "feature"]);
        std::fs::write(work.join("page.html"), "v2\n").unwrap();
        std::fs::write(work.join("new.html"), "new\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-qm", "fix"]);
        (root, work)
    }

    fn state(dir: &Path) -> (String, String, String) {
        (
            git(dir, &["symbolic-ref", "--short", "-q", "HEAD"]),
            git(dir, &["rev-parse", "HEAD"]),
            git(dir, &["status", "--porcelain", "--untracked-files=all"]),
        )
    }

    #[test]
    fn base_checkout_restores_the_branch_after_success_error_and_panic() {
        let (_root, work) = repo_on_feature();
        let before = state(&work);
        let base = git(&work, &["merge-base", "HEAD", "origin/main"]);

        let guard = BaseCheckout::switch(&work, "main", true).unwrap();
        assert_eq!(git(&work, &["rev-parse", "HEAD"]), base);
        assert_eq!(
            std::fs::read_to_string(work.join("page.html")).unwrap(),
            "v1\n"
        );
        assert!(!work.join("new.html").exists());
        guard.restored().unwrap();
        assert_eq!(state(&work), before);

        let failing = || -> Result<(), String> {
            let _guard = BaseCheckout::switch(&work, "main", true)?;
            Err("the baseline check failed".into())
        };
        assert!(failing().is_err());
        assert_eq!(state(&work), before);

        let w = work.clone();
        let panicked = std::panic::catch_unwind(move || {
            let _guard = BaseCheckout::switch(&w, "main", true).unwrap();
            std::fs::write(w.join("page.html"), "edited while switched\n").unwrap();
            panic!("boom while switched");
        });
        assert!(panicked.is_err());
        assert_eq!(state(&work), before);
        assert_eq!(
            std::fs::read_to_string(work.join("page.html")).unwrap(),
            "v2\n"
        );
    }

    #[test]
    fn files_the_check_created_are_removed_and_tracked_edits_restored() {
        let (_root, work) = repo_on_feature();
        let before = state(&work);
        let guard = BaseCheckout::switch(&work, "main", true).unwrap();
        std::fs::write(work.join("page.html"), "rewritten by the dev server\n").unwrap();
        std::fs::create_dir_all(work.join("gen/deep")).unwrap();
        std::fs::write(work.join("gen/deep/next-env.d.ts"), "x").unwrap();
        std::fs::create_dir_all(work.join("node_modules/x")).unwrap();
        std::fs::write(work.join("node_modules/x/index.js"), "kept").unwrap();
        guard.restored().unwrap();
        assert_eq!(state(&work), before);
        assert!(!work.join("gen").exists());
        assert!(work.join("node_modules/x/index.js").exists());

        // The same snapshot cleanup around a branch run keeps what was dirty before.
        std::fs::write(work.join("mine.txt"), "the agent's own file").unwrap();
        let snap = TreeSnapshot::take(&work).unwrap();
        std::fs::write(work.join("next-env.d.ts"), "x").unwrap();
        std::fs::write(work.join("page.html"), "changed\n").unwrap();
        let mut fixed = snap.restore(&work);
        fixed.sort();
        assert_eq!(fixed, vec!["next-env.d.ts", "page.html"]);
        assert!(work.join("mine.txt").exists());
        assert_eq!(
            std::fs::read_to_string(work.join("page.html")).unwrap(),
            "v2\n"
        );
    }

    #[test]
    fn an_interrupted_baseline_is_put_back_on_the_branch() {
        let (_root, work) = repo_on_feature();
        let before = state(&work);
        assert_eq!(recover_base_checkout(&work).unwrap(), None);
        let guard = BaseCheckout::switch(&work, "main", true).unwrap();
        std::fs::write(work.join("page.html"), "the base dev server wrote this\n").unwrap();
        std::fs::write(work.join("cache.tmp"), "x").unwrap();
        // A killed runner never runs Drop.
        std::mem::forget(guard);
        assert!(git(&work, &["status", "--porcelain"]).contains("page.html"));
        let note = recover_base_checkout(&work).unwrap().unwrap();
        assert!(note.contains("feature"), "{note}");
        assert_eq!(state(&work), before);
        assert_eq!(recover_base_checkout(&work).unwrap(), None);
    }

    #[test]
    fn staged_or_dirty_work_from_before_the_check_is_never_reset() {
        let (_root, work) = repo_on_feature();
        std::fs::write(work.join("page.html"), "staged by the agent\n").unwrap();
        git(&work, &["add", "page.html"]);
        std::fs::write(work.join("added.txt"), "new and staged").unwrap();
        git(&work, &["add", "added.txt"]);
        let snap = TreeSnapshot::take(&work).unwrap();
        std::fs::write(work.join("page.html"), "rewritten by the dev server\n").unwrap();
        std::fs::write(work.join("added.txt"), "rewritten too").unwrap();
        snap.restore(&work);
        assert_eq!(git(&work, &["show", ":page.html"]), "staged by the agent");
        assert_eq!(git(&work, &["show", ":added.txt"]), "new and staged");
        assert!(work.join("added.txt").exists());
        assert!(!snap.unchanged(&work));
    }

    #[test]
    fn a_base_that_tracks_an_ignored_file_is_never_switched_to() {
        let (_root, work) = repo_on_feature();
        // The base tracks `.env.local`; the branch stops tracking it and ignores it.
        git(&work, &["switch", "-q", "main"]);
        git(&work, &["reset", "-q", "--hard", "origin/main"]);
        std::fs::write(work.join(".env.local"), "BASE=1\n").unwrap();
        git(&work, &["add", ".env.local"]);
        git(&work, &["commit", "-qm", "env"]);
        git(&work, &["push", "-q", "origin", "main"]);
        git(&work, &["switch", "-q", "feature"]);
        git(&work, &["rebase", "-q", "main"]);
        git(&work, &["rm", "-q", "--cached", ".env.local"]);
        std::fs::write(work.join(".gitignore"), "node_modules/\n.env.local\n").unwrap();
        git(&work, &["commit", "-qam", "untrack env"]);
        std::fs::write(work.join(".env.local"), "MINE=1\n").unwrap();
        let err = BaseCheckout::switch(&work, "main", true).err().unwrap();
        assert!(err.contains(".env.local"), "{err}");
        assert_eq!(
            std::fs::read_to_string(work.join(".env.local")).unwrap(),
            "MINE=1\n"
        );
        assert_eq!(git(&work, &["symbolic-ref", "--short", "HEAD"]), "feature");
    }

    #[test]
    fn base_switch_restore_and_recovery_never_run_git_hooks() {
        let (root, work) = repo_on_feature();
        let marker = root.path().join("hook-ran");
        // A hooks dir of this temp repo's own, set in its local config: never
        // the machine's global `core.hooksPath`.
        let hooks = root.path().join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        git(
            &work,
            &["config", "core.hooksPath", hooks.to_str().unwrap()],
        );
        for hook in ["post-checkout", "post-merge"] {
            let path = hooks.join(hook);
            std::fs::write(&path, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
        }
        // The hook does run for an ordinary switch, so the test can see it.
        git(&work, &["switch", "-q", "main"]);
        assert!(marker.exists());
        std::fs::remove_file(&marker).unwrap();
        git(&work, &["switch", "-q", "feature"]);
        std::fs::remove_file(&marker).unwrap();

        BaseCheckout::switch(&work, "main", true)
            .unwrap()
            .restored()
            .unwrap();
        std::mem::forget(BaseCheckout::switch(&work, "main", true).unwrap());
        recover_base_checkout(&work).unwrap().unwrap();
        assert!(!marker.exists(), "a git hook ran during the browser check");
    }

    fn write_marker(work: &Path, branch: &str, sha: &str, base: &str) {
        std::fs::write(
            marker_path(work).unwrap(),
            format!("{branch}\n{sha}\n{base}\n"),
        )
        .unwrap();
    }

    #[test]
    fn recovery_on_the_branch_only_drops_the_marker_and_keeps_uncommitted_work() {
        let (_root, work) = repo_on_feature();
        let sha = git(&work, &["rev-parse", "HEAD"]);
        let base = git(&work, &["merge-base", "HEAD", "origin/main"]);
        write_marker(&work, "feature", &sha, &base);
        std::fs::write(work.join("page.html"), "the agent's next edit\n").unwrap();
        std::fs::write(work.join("notes.txt"), "untracked work").unwrap();
        recover_base_checkout(&work).unwrap();
        assert_eq!(
            std::fs::read_to_string(work.join("page.html")).unwrap(),
            "the agent's next edit\n"
        );
        assert!(work.join("notes.txt").exists());
        assert!(!marker_path(&work).unwrap().exists());
        assert_eq!(recover_base_checkout(&work).unwrap(), None);
    }

    #[test]
    fn recovery_with_an_unexpected_head_drops_the_marker_and_touches_nothing() {
        let (_root, work) = repo_on_feature();
        let sha = git(&work, &["rev-parse", "HEAD"]);
        // Detached, but not at the recorded base: someone moved it on purpose.
        git(&work, &["switch", "-q", "--detach", "HEAD"]);
        std::fs::write(work.join("page.html"), "dirty\n").unwrap();
        write_marker(
            &work,
            "feature",
            &sha,
            "0000000000000000000000000000000000000000",
        );
        let note = recover_base_checkout(&work).unwrap().unwrap();
        assert!(note.contains("left as it is"), "{note}");
        assert_eq!(
            std::fs::read_to_string(work.join("page.html")).unwrap(),
            "dirty\n"
        );
        assert_eq!(git(&work, &["rev-parse", "HEAD"]), sha);
        assert!(!marker_path(&work).unwrap().exists());
        assert_eq!(recover_base_checkout(&work).unwrap(), None);
        // A marker naming a branch that is gone never wedges either.
        write_marker(&work, "no-such-branch", &sha, &sha);
        assert!(recover_base_checkout(&work).is_err());
        assert!(!marker_path(&work).unwrap().exists());
        assert_eq!(recover_base_checkout(&work).unwrap(), None);
    }

    #[test]
    fn restore_treats_bracketed_paths_literally() {
        let (_root, work) = repo_on_feature();
        for p in ["app/[id]/page.tsx", "app/i/page.tsx", "app/d/page.tsx"] {
            std::fs::create_dir_all(work.join(p).parent().unwrap()).unwrap();
            std::fs::write(work.join(p), "v1\n").unwrap();
        }
        git(&work, &["add", "."]);
        git(&work, &["commit", "-qm", "routes"]);
        std::fs::write(work.join("app/i/page.tsx"), "the agent's edit\n").unwrap();
        let snap = TreeSnapshot::take(&work).unwrap();
        std::fs::write(work.join("app/[id]/page.tsx"), "rewritten\n").unwrap();
        std::fs::write(work.join("app/d/page.tsx"), "rewritten\n").unwrap();
        snap.restore(&work);
        let read = |p: &str| std::fs::read_to_string(work.join(p)).unwrap();
        assert_eq!(read("app/i/page.tsx"), "the agent's edit\n");
        assert_eq!(read("app/[id]/page.tsx"), "v1\n");
        assert_eq!(read("app/d/page.tsx"), "v1\n");
    }

    #[test]
    fn a_dirty_detached_or_non_worktree_checkout_is_never_switched() {
        let (_root, work) = repo_on_feature();
        let sha = git(&work, &["rev-parse", "HEAD"]);
        let err = BaseCheckout::switch(&work, "main", false).err().unwrap();
        assert!(err.contains("own checkout"), "{err}");
        std::fs::write(work.join("page.html"), "dirty\n").unwrap();
        assert!(BaseCheckout::switch(&work, "main", true)
            .err()
            .unwrap()
            .contains("uncommitted"));
        git(&work, &["checkout", "-q", "--", "page.html"]);
        std::fs::write(work.join("untracked.txt"), "x").unwrap();
        assert!(BaseCheckout::switch(&work, "main", true).is_err());
        std::fs::remove_file(work.join("untracked.txt")).unwrap();
        git(&work, &["switch", "-q", "--detach", "HEAD"]);
        assert!(BaseCheckout::switch(&work, "main", true)
            .err()
            .unwrap()
            .contains("not on a branch"));
        git(&work, &["switch", "-q", "feature"]);
        assert!(BaseCheckout::switch(&work, "no-such-branch", true).is_err());
        assert_eq!(git(&work, &["rev-parse", "HEAD"]), sha);
        assert_eq!(git(&work, &["symbolic-ref", "--short", "HEAD"]), "feature");
    }

    #[test]
    fn merge_base_prefers_origin_target_then_local_target() {
        let (_root, work) = repo_on_feature();
        let origin_base = git(&work, &["merge-base", "HEAD", "origin/main"]);
        // A local `main` that moved on is ignored while `origin/main` exists.
        git(&work, &["branch", "-f", "main", "HEAD"]);
        let guard = BaseCheckout::switch(&work, "main", true).unwrap();
        assert_eq!(git(&work, &["rev-parse", "HEAD"]), origin_base);
        guard.restored().unwrap();
        git(&work, &["remote", "remove", "origin"]);
        let feature = git(&work, &["rev-parse", "HEAD"]);
        let guard = BaseCheckout::switch(&work, "main", true).unwrap();
        assert_eq!(git(&work, &["rev-parse", "HEAD"]), feature);
        guard.restored().unwrap();
    }

    // ---- verifier ----

    type RunFn = dyn Fn(&Value, &Path) -> Result<Value, String> + Send + Sync;

    struct Fake {
        f: Box<RunFn>,
        calls: std::sync::Mutex<Vec<(Value, String)>>,
    }

    impl Fake {
        fn new(
            f: impl Fn(&Value, &Path) -> Result<Value, String> + Send + Sync + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                f: Box::new(f),
                calls: Default::default(),
            })
        }
        fn calls(&self) -> Vec<(Value, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl UiCheckRunner for Fake {
        fn run(&self, spec: &Value, cwd: &Path, _: &str, _: Duration) -> Result<Value, String> {
            let head = git(cwd, &["rev-parse", "HEAD"]);
            self.calls.lock().unwrap().push((spec.clone(), head));
            (self.f)(spec, cwd)
        }
    }

    fn page(path: &str) -> PageReport {
        PageReport {
            final_url: format!("http://127.0.0.1:1{path}"),
            status: Some(200),
            loaded: true,
            ..PageReport::default()
        }
    }

    fn report(pages: &[(&str, &str, PageReport)]) -> Value {
        json!({"serve": {"started": true}, "deadline_hit": false,
               "pages": pages.iter().map(|(l, p, r)| json!({"label": l, "path": p, "report": r})).collect::<Vec<_>>()})
    }

    fn contrast(targets: &[&str]) -> crate::a11y::Violation {
        crate::a11y::Violation {
            id: "color-contrast".into(),
            impact: crate::a11y::Impact::Serious,
            help: "contrast".into(),
            targets: targets.iter().map(|t| t.to_string()).collect(),
            nodes: targets.len(),
        }
    }

    struct Setup {
        root: tempfile::TempDir,
        work: std::path::PathBuf,
    }

    /// The issue worktree `<root>/wt/web-issue-7` on `feature`, a fix
    /// handoff declaring `/orders` and `/settings`, and a loop dir.
    fn setup() -> Setup {
        let (root, work) = repo_on_feature();
        let wt = root.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        let dest = wt.join("web-issue-7");
        std::fs::rename(&work, &dest).unwrap();
        // Nothing listens on the discard port; a released ephemeral port could
        // be bound by a parallel test and read as "already served".
        let port = 9;
        let s = Setup { root, work: dest };
        s.handoff(Some(
            json!({"serve": "yarn dev", "origin": format!("http://127.0.0.1:{port}"),
                              "pages": [{"label": "Orders", "path": "/orders"},
                                        {"label": "Settings", "path": "/settings"}]}),
        ));
        s
    }

    impl Setup {
        fn handoff(&self, ui: Option<Value>) {
            let mut h = json!({"action": "fix", "target_branch": "main", "branch": "x"});
            if let Some(u) = ui {
                h["ui_checks"] = u;
            }
            std::fs::write(self.root.path().join("handoff.json"), h.to_string()).unwrap();
        }

        fn verifier(&self, project: Value, runner: Arc<dyn UiCheckRunner>) -> UiCheckVerifier {
            let wt = self.root.path().join("wt");
            let mut work = IssueWork::new("web", 7)
                .with_project_fn(move |_| Ok(project.clone()))
                .with_worktree_root_fn(move || Ok(wt.clone()));
            work.handoff_path = Some(self.root.path().join("handoff.json"));
            UiCheckVerifier::new("ui_check", work, runner, self.root.path().join("loop"))
                .with_find_chrome(|| Some(PathBuf::from("/bin/sh")))
        }
    }

    fn project() -> Value {
        json!({"local_path": "/src/web"})
    }

    fn a11y_project() -> Value {
        json!({"local_path": "/src/web", "verifiers": ["a11y", "perf", "nonsense"]})
    }

    fn verify(v: &UiCheckVerifier) -> VerificationResult {
        let d = crate::loop_definition::LoopDefinition::from_value(
            &json!({"name": "t", "version": 1, "trigger": {"type": "manual"}, "goal": {"type": "g"}}),
        )
        .unwrap();
        v.verify(&LoopContext {
            iteration: 1,
            definition: &d,
            previous: None,
        })
    }

    fn verify_dyn(v: &dyn Verifier) -> VerificationResult {
        let d = crate::loop_definition::LoopDefinition::from_value(
            &json!({"name": "t", "version": 1, "trigger": {"type": "manual"}, "goal": {"type": "g"}}),
        )
        .unwrap();
        v.verify(&LoopContext {
            iteration: 1,
            definition: &d,
            previous: None,
        })
    }

    fn never() -> Arc<Fake> {
        Fake::new(|_, _| panic!("the browser check must not run"))
    }

    #[test]
    fn vacuous_without_handoff_worktree_chrome_or_pages() {
        let s = setup();
        let fake = never();
        let mut v = s.verifier(project(), fake.clone());
        v.work.handoff_path = None;
        assert!(verify(&v).output.contains("only in gate mode"));
        let v = s.verifier(
            json!({"local_path": s.work.display().to_string(), "use_worktree": false}),
            fake.clone(),
        );
        assert_eq!(verify(&v).output, "browser check needs a worktree");
        let v = s
            .verifier(project(), fake.clone())
            .with_find_chrome(|| None);
        assert_eq!(
            verify(&v).output,
            "Google Chrome was not found - browser check skipped"
        );
        s.handoff(Some(json!({"serve": "rm -rf /", "origin": "http://localhost:3000", "pages": [{"path": "/"}]})));
        let r = verify(&s.verifier(project(), fake.clone()));
        assert!(
            r.output.starts_with("ui_checks ignored: serve:"),
            "{}",
            r.output
        );
        assert_eq!(
            r.evidence,
            json!({"vacuous": true, "gate": "ui", "undeclared_ui": false})
        );
        let h = s.root.path().join("handoff.json");
        std::fs::write(&h, json!({"action": "answer"}).to_string()).unwrap();
        assert!(verify(&s.verifier(project(), fake.clone())).evidence["vacuous"] == json!(true));
        assert!(fake.calls().is_empty());
        assert!(v.skip_after_failure());
    }

    #[test]
    fn undeclared_ui_is_flagged_when_view_files_changed() {
        let s = setup();
        s.handoff(None);
        let r = verify(&s.verifier(project(), never()));
        assert!(r.passed);
        assert_eq!(r.evidence["undeclared_ui"], json!(true), "{}", r.output);
        assert!(r.output.contains("changed UI files"));
        // A fix that only touches non-UI files is not flagged.
        git(&s.work, &["reset", "-q", "--hard", "origin/main"]);
        std::fs::write(s.work.join("model.rb"), "x").unwrap();
        git(&s.work, &["add", "."]);
        git(&s.work, &["commit", "-qm", "model"]);
        let r = verify(&s.verifier(project(), never()));
        assert_eq!(r.evidence["undeclared_ui"], json!(false));
        assert_eq!(r.output, "no pages declared - browser check skipped");
    }

    #[test]
    fn smoke_problems_block_and_console_errors_do_not() {
        let s = setup();
        let fake = Fake::new(|_, _| {
            let mut orders = page("/orders");
            orders.exceptions = vec!["TypeError: total is undefined".into()];
            let mut settings = page("/settings");
            settings.console_errors = vec!["Warning: key".into()];
            Ok(report(&[
                ("Orders", "/orders", orders),
                ("Settings", "/settings", settings),
            ]))
        });
        let r = verify(&s.verifier(project(), fake.clone()));
        assert!(!r.passed, "{}", r.output);
        assert_eq!(r.output, "Orders: page-error TypeError: total is undefined");
        assert_eq!(
            r.evidence["blocking"],
            json!([{"rule": "page-error", "page": "Orders", "kind": "smoke"}])
        );
        assert_eq!(r.evidence["pages"][1]["status"], "problems");
        assert_eq!(
            r.evidence["pages"][1]["problems"][0]["blocking"],
            json!(false)
        );
        assert_eq!(r.evidence["a11y"], json!({"enabled": false}));
        assert_eq!(fake.calls().len(), 1);
        let spec = &fake.calls()[0].0;
        assert_eq!(spec["cwd"], json!(s.work));
        assert_eq!(spec["deadline_s"], json!(300));
        assert_eq!(spec["serve"], "yarn dev");

        let clean = Fake::new(|_, _| {
            let mut settings = page("/settings");
            settings.console_errors = vec!["Warning: key".into()];
            Ok(report(&[
                ("Orders", "/orders", page("/orders")),
                ("Settings", "/settings", settings),
            ]))
        });
        let r = verify(&s.verifier(project(), clean));
        assert!(r.passed, "{}", r.output);
        assert!(
            r.output.starts_with("1 page(s) loaded clean"),
            "{}",
            r.output
        );
    }

    #[test]
    fn dev_server_output_never_reaches_the_mr_and_is_redacted_in_evidence() {
        let s = setup();
        let tail = "booting\nDATABASE_URL=postgres://app:hunter2@db.internal:5432/web\nSECRET_KEY_BASE=abc123secretvalue\nError: connection refused";
        let fake = Fake::new(move |_, _| {
            Ok(json!({"serve": {"started": false, "tail": tail}, "pages": []}))
        });
        let r = verify(&s.verifier(project(), fake));
        assert!(!r.passed);
        assert_eq!(r.output, "dev server did not start; see the run log");
        let ev = r.evidence.to_string();
        for secret in ["hunter2", "abc123secretvalue"] {
            assert!(!ev.contains(secret), "{ev}");
        }
        assert!(ev.contains("connection refused"), "{ev}");
        let result = crate::loop_result::LoopResult::new(
            "l",
            "r",
            "gitlab-issue",
            crate::loop_state::LoopState::Completed,
            vec![crate::loop_result::IterationResult {
                iteration: 1,
                state: crate::loop_state::LoopState::Completed,
                verification_results: vec![r],
                budget: json!({}),
                progressed: true,
            }],
            "completed",
        );
        let mr = crate::pre_review::pre_review_section(&result).unwrap();
        assert!(
            mr.contains(
                "- Browser check: could not run (dev server did not start; see the run log)"
            ),
            "{mr}"
        );
        assert!(
            !mr.contains("hunter2") && !mr.contains("connection refused"),
            "{mr}"
        );
    }

    #[test]
    fn a_check_that_moves_head_is_a_hard_failure() {
        let s = setup();
        let fake = Fake::new(|_, cwd| {
            git(cwd, &["switch", "-q", "--detach", "HEAD~1"]);
            Ok(report(&[("Orders", "/orders", page("/orders"))]))
        });
        let r = verify(&s.verifier(project(), fake));
        assert!(!r.passed);
        assert_eq!(r.evidence["worktree_moved"], json!(true), "{}", r.evidence);
        assert_eq!(r.evidence["hard_failure"], json!(true));
        assert!(r.output.contains("feature"), "{}", r.output);
        let wrapped = crate::loop_verifiers::ObserveOnly {
            inner: Box::new(s.verifier(project(), Fake::new(|_, _| Ok(json!({}))))),
        };
        // ObserveOnly never hides it: the gates after it must not check the wrong commit.
        git(&s.work, &["switch", "-q", "feature"]);
        let moving = crate::loop_verifiers::ObserveOnly {
            inner: Box::new(s.verifier(
                project(),
                Fake::new(|_, cwd| {
                    git(cwd, &["switch", "-q", "--detach", "HEAD~1"]);
                    Ok(report(&[("Orders", "/orders", page("/orders"))]))
                }),
            )),
        };
        let r = verify_dyn(&moving);
        assert!(!r.passed, "{}", r.evidence);
        assert_eq!(r.evidence["mode"], "observe");
        git(&s.work, &["switch", "-q", "feature"]);
        assert!(verify_dyn(&wrapped).passed);
    }

    #[test]
    fn a11y_runs_only_for_opted_in_projects() {
        let s = setup();
        let ok = || {
            Ok(report(&[
                ("Orders", "/orders", page("/orders")),
                ("Settings", "/settings", page("/settings")),
            ]))
        };
        let fake = Fake::new(move |_, _| ok());
        verify(&s.verifier(project(), fake.clone()));
        verify(&s.verifier(a11y_project(), fake.clone()));
        let calls = fake.calls();
        assert_eq!(calls[0].0["axe"], json!(false));
        assert_eq!(calls[1].0["axe"], json!(true));
        assert!(!a11y_enabled(&json!({"verifiers": ["perf"]})));
        assert!(!a11y_enabled(&json!({"verifiers": "a11y"})));
    }

    #[test]
    fn baseline_runs_only_when_the_branch_has_violations() {
        let s = setup();
        let mut clean = page("/orders");
        clean.axe = Some(Ok(vec![]));
        let r1 = report(&[
            ("Orders", "/orders", clean.clone()),
            ("Settings", "/settings", clean.clone()),
        ]);
        let fake = Fake::new(move |_, _| Ok(r1.clone()));
        let r = verify(&s.verifier(a11y_project(), fake.clone()));
        assert_eq!(fake.calls().len(), 1);
        assert_eq!(r.evidence["a11y"]["baseline"], "skipped_clean");
        assert_eq!(r.evidence["a11y"]["axe_version"], crate::a11y::AXE_VERSION);

        let feature = git(&s.work, &["rev-parse", "HEAD"]);
        let merge_base = git(&s.work, &["merge-base", "HEAD", "origin/main"]);
        let mut head = page("/orders");
        head.axe = Some(Ok(vec![contrast(&["#old", "#new"])]));
        let mut base = page("/orders");
        base.axe = Some(Ok(vec![contrast(&["#old"])]));
        let mut settings = page("/settings");
        settings.axe = Some(Ok(vec![contrast(&["#s"])]));
        let (h, b) = (
            report(&[
                ("Orders", "/orders", head),
                ("Settings", "/settings", settings.clone()),
            ]),
            report(&[
                ("Orders", "/orders", base),
                ("Settings", "/settings", settings),
            ]),
        );
        let fake = Fake::new(move |_, cwd| {
            let at_base = git(cwd, &["rev-parse", "HEAD"])
                == git(cwd, &["merge-base", "HEAD", "origin/main"]);
            Ok(if at_base { b.clone() } else { h.clone() })
        });
        let r = verify(&s.verifier(a11y_project(), fake.clone()));
        let calls = fake.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            (calls[0].1.as_str(), calls[1].1.as_str()),
            (feature.as_str(), merge_base.as_str())
        );
        assert_eq!(r.evidence["a11y"]["baseline"], "measured");
        assert_eq!(r.evidence["a11y"]["existing"], json!(1));
        assert_eq!(r.evidence["a11y"]["new"][0]["targets"], json!(["#new"]));
        assert!(!r.passed);
        assert_eq!(r.output, "Orders: color-contrast (serious) #new");
        assert_eq!(r.evidence["blocking"][0]["kind"], "a11y");
        assert_eq!(git(&s.work, &["rev-parse", "HEAD"]), feature);
    }

    #[test]
    fn an_unavailable_baseline_never_blocks() {
        let s = setup();
        let mut head = page("/orders");
        head.axe = Some(Ok(vec![contrast(&["#a"])]));
        let h = report(&[("Orders", "/orders", head)]);
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n2 = n.clone();
        let fake = Fake::new(move |_, _| {
            if n2.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Ok(h.clone())
            } else {
                Ok(json!({"serve": {"started": false, "tail": "Error: boom"}, "pages": []}))
            }
        });
        let r = verify(&s.verifier(a11y_project(), fake));
        assert!(r.passed, "{}", r.output);
        assert!(
            r.evidence["a11y"]["baseline"]
                .as_str()
                .unwrap()
                .starts_with("unavailable"),
            "{}",
            r.evidence
        );
        assert_eq!(r.evidence["a11y"]["new"], json!([]));

        // A tree the switch refuses (the agent left a file behind) is unavailable too.
        std::fs::write(s.work.join("stray.txt"), "x").unwrap();
        let mut head = page("/orders");
        head.axe = Some(Ok(vec![contrast(&["#a"])]));
        let h = report(&[("Orders", "/orders", head)]);
        let fake = Fake::new(move |_, _| Ok(h.clone()));
        let r = verify(&s.verifier(a11y_project(), fake.clone()));
        assert!(r.passed);
        assert_eq!(fake.calls().len(), 1);
        assert_eq!(
            r.evidence["a11y"]["baseline"],
            "unavailable (the worktree has uncommitted changes)"
        );
        assert!(s.work.join("stray.txt").exists());
    }

    #[test]
    fn a_redirected_page_is_skipped_not_failed() {
        let s = setup();
        let fake = Fake::new(|_, _| {
            let mut orders = page("/users/sign_in");
            orders.wait_for_found = Some(false);
            Ok(report(&[
                ("Orders", "/orders", orders),
                ("Settings", "/settings", page("/settings")),
            ]))
        });
        let r = verify(&s.verifier(project(), fake));
        assert!(r.passed, "{}", r.output);
        assert_eq!(r.evidence["pages"][0]["status"], "skipped");
        assert!(
            r.output
                .contains("Orders: skipped redirected to /users/sign_in (sign-in?)"),
            "{}",
            r.output
        );
    }

    #[test]
    fn the_branch_sha_and_status_are_unchanged_after_ui_check() {
        let s = setup();
        let before = state(&s.work);
        let fake = Fake::new(|_, cwd| {
            std::fs::write(cwd.join("page.html"), "rewritten by the dev server\n").unwrap();
            std::fs::write(cwd.join("next-env.d.ts"), "x").unwrap();
            std::fs::create_dir_all(cwd.join("node_modules/.cache")).unwrap();
            std::fs::write(cwd.join("node_modules/.cache/x"), "kept").unwrap();
            let mut p = page("/orders");
            p.axe = Some(Ok(vec![contrast(&["#a"])]));
            Ok(report(&[("Orders", "/orders", p)]))
        });
        let r = verify(&s.verifier(a11y_project(), fake.clone()));
        assert_eq!(fake.calls().len(), 2, "{}", r.output);
        assert_eq!(r.evidence["a11y"]["baseline"], "measured");
        assert!(r.evidence.get("restore_failed").is_none());
        assert_eq!(state(&s.work), before);
        assert!(s.work.join("node_modules/.cache/x").exists());
    }

    #[test]
    fn a_check_that_leaves_changes_is_an_error_and_a_panic_still_restores() {
        let s = setup();
        // The agent's own staged edit is dirty before; the dev server rewrites it.
        std::fs::write(s.work.join("page.html"), "staged\n").unwrap();
        git(&s.work, &["add", "page.html"]);
        let fake = Fake::new(|_, cwd| {
            std::fs::write(cwd.join("page.html"), "rewritten\n").unwrap();
            Ok(report(&[("Orders", "/orders", page("/orders"))]))
        });
        let r = verify(&s.verifier(project(), fake));
        assert!(!r.passed);
        assert_eq!(r.evidence["restore_failed"], json!(true));
        assert!(r.output.contains("left changes behind"), "{}", r.output);
        git(&s.work, &["reset", "-q", "--hard", "HEAD"]);

        let before = state(&s.work);
        let panicking = Fake::new(|_, cwd| {
            std::fs::write(cwd.join("next-env.d.ts"), "x").unwrap();
            panic!("runner blew up");
        });
        let v = s.verifier(project(), panicking);
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| verify(&v)));
        assert!(res.is_err());
        assert_eq!(state(&s.work), before);
    }

    #[test]
    fn mocks_and_session_files_over_one_megabyte_are_refused() {
        let screens = tempfile::tempdir().unwrap();
        std::fs::write(
            screens.path().join("big.json"),
            format!("[{}]", " ".repeat(1_100_000)),
        )
        .unwrap();
        let h = with(|u| u["mocks"] = json!("big.json"));
        let err = from_handoff(&h, screens.path()).unwrap_err();
        assert!(err.starts_with("mocks:") && err.contains("1 MB"), "{err}");
    }

    #[test]
    fn a_serve_holder_from_another_issue_is_left_alone() {
        let s = setup();
        let holder = s.root.path().join("loop/outputs/screens/.server");
        std::fs::create_dir_all(&holder).unwrap();
        struct Holder(std::process::Child);
        impl Drop for Holder {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Holder(
            std::process::Command::new("sh")
                .args(["-c", "while [ ! -f \"$1/stop\" ]; do sleep 0.1; done", "sh"])
                .arg(&holder)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        std::fs::write(holder.join("pid"), child.0.id().to_string()).unwrap();
        let fake = Fake::new(|_, _| Ok(report(&[("Orders", "/orders", page("/orders"))])));
        verify(&s.verifier(project(), fake.clone()));
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "another issue's holder was stopped"
        );
        assert!(holder.join("pid").exists() && !holder.join("stop").exists());
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn two_checks_on_one_origin_never_probe_at_the_same_time() {
        let s = setup();
        let inner = std::sync::Mutex::new(s.verifier(project(), never()));
        let seen: Arc<std::sync::Mutex<Option<VerificationResult>>> = Arc::default();
        let seen2 = seen.clone();
        let outer = Fake::new(move |_, _| {
            *seen2.lock().unwrap() = Some(verify(&inner.lock().unwrap()));
            Ok(report(&[("Orders", "/orders", page("/orders"))]))
        });
        let r = verify(&s.verifier(project(), outer.clone()));
        assert!(r.passed, "{}", r.output);
        assert_eq!(outer.calls().len(), 1);
        let nested = seen.lock().unwrap().take().unwrap();
        assert!(nested.passed);
        assert_eq!(
            nested.output,
            "another check or screenshot is using http://127.0.0.1:9 - browser check skipped"
        );
        // Released afterwards.
        let again = Fake::new(|_, _| Ok(report(&[("Orders", "/orders", page("/orders"))])));
        verify(&s.verifier(project(), again.clone()));
        assert_eq!(again.calls().len(), 1);
    }

    #[test]
    fn a_busy_port_is_vacuous() {
        let s = setup();
        let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = busy.local_addr().unwrap().port();
        s.handoff(Some(
            json!({"serve": "yarn dev", "origin": format!("http://127.0.0.1:{port}"),
                              "pages": [{"label": "Orders", "path": "/orders"}]}),
        ));
        let r = verify(&s.verifier(project(), never()));
        assert!(r.passed);
        assert_eq!(
            r.output,
            format!("another check or screenshot is using http://127.0.0.1:{port} - browser check skipped")
        );
    }

    #[test]
    fn a_stuck_check_gets_sigterm_then_sigkill_and_leaves_no_process() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("stuck");
        let marker = dir.path().join("got-term");
        let pids = dir.path().join("pids");
        std::fs::write(
            &exe,
            format!(
                "#!/bin/sh\ntrap 'echo term > {m}' TERM\nsleep 300 &\necho $! >> {p}\necho $$ >> {p}\n\
                 while true; do sleep 0.1; done\n",
                m = marker.display(),
                p = pids.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let runner = SubprocessRunner {
            exe: Some(exe),
            grace: Duration::from_secs(1),
            env: Vec::new(),
        };
        let started = Instant::now();
        let err = runner
            .run(
                &json!({}),
                dir.path(),
                &std::env::var("PATH").unwrap(),
                Duration::from_secs(1),
            )
            .unwrap_err();
        assert!(err.contains("did not finish"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(8));
        assert!(marker.exists(), "SIGTERM came first");
        let alive = |p: i32| unsafe { libc::kill(p, 0) == 0 };
        let ps: Vec<i32> = std::fs::read_to_string(&pids)
            .unwrap()
            .lines()
            .map(|l| l.parse().unwrap())
            .collect();
        assert_eq!(ps.len(), 2);
        let until = Instant::now() + Duration::from_secs(3);
        while ps.iter().any(|p| alive(*p)) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ps.iter().all(|p| !alive(*p)), "{ps:?}");
    }
}
