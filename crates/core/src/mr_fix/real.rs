//! The production [`MrApi`] (a GitLab client on one merge request) and
//! [`Workspace`] (the MR's git worktree, its checks and the push).

use super::run::{CheckResult, Commit, MrApi, MrInfo, Verification, Workspace};
use crate::gitlab::Client;
use crate::loop_verifiers::CommandVerifier;
use crate::verify_commands::check_verify_command;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// GitLab on one merge request, with the user's own token.
pub struct GitlabMr {
    pub client: Client,
    pub project_id: String,
    pub iid: i64,
}

/// The fields the runner needs from `Client::get_merge_request`.
pub fn mr_info(v: &Value) -> MrInfo {
    let text = |k: &str| match v.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    MrInfo {
        iid: text("iid"),
        state: text("state"),
        title: text("title"),
        description: text("description"),
        source_branch: text("source_branch"),
        project_path: v
            .pointer("/references/full")
            .and_then(Value::as_str)
            .and_then(|r| r.rsplit_once('!'))
            .map(|(p, _)| p.to_string())
            .unwrap_or_default(),
        changes: v
            .get("changes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    }
}

impl MrApi for GitlabMr {
    fn me(&self) -> Result<String, String> {
        let u = self.client.current_user().map_err(|e| e.to_string())?;
        u.get("username")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| "the GitLab token has no username".into())
    }

    fn mr(&self) -> Result<MrInfo, String> {
        self.client
            .get_merge_request(&self.project_id, self.iid)
            .map(|v| mr_info(&v))
            .map_err(|e| e.to_string())
    }

    fn discussions(&self) -> Result<Vec<Value>, String> {
        self.client
            .list_mr_discussions(&self.project_id, self.iid)
            .map_err(|e| e.to_string())
    }

    fn post_note(&self, body: &str) -> Result<i64, String> {
        let v = self
            .client
            .post_mr_comment(&self.project_id, self.iid, body)
            .map_err(|e| e.to_string())?;
        v.get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| "GitLab returned no note id".into())
    }

    fn reply(&self, discussion_id: &str, body: &str) -> Result<(), String> {
        self.client
            .reply_to_mr_discussion(&self.project_id, self.iid, discussion_id, body)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn upload(&self, path: &Path) -> Result<String, String> {
        let v = self
            .client
            .upload_file(&self.project_id, path)
            .map_err(|e| e.to_string())?;
        v.get("url")
            .and_then(Value::as_str)
            .filter(|u| u.starts_with("/uploads/"))
            .map(str::to_owned)
            .ok_or_else(|| "GitLab returned no upload url".into())
    }
}

/// What the project's own config tells the agent: its `custom_instructions`
/// and the `screenshot-recipe` lessons recorded for it. Both come from the
/// user's own files (trusted), unlike the merge request text.
pub fn render_project_notes(custom_instructions: &str, recipes: &[String]) -> String {
    let mut out = String::new();
    let custom = custom_instructions.trim();
    if !custom.is_empty() {
        out.push_str("### Project instructions (from the user's LoopX settings)\n\n");
        out.push_str(custom);
        out.push_str("\n\n");
    }
    let recipes: Vec<&str> = recipes
        .iter()
        .map(|r| r.trim())
        .filter(|r| !r.is_empty())
        .collect();
    if !recipes.is_empty() {
        out.push_str("### Screenshot recipes recorded for this project\n\n");
        for r in recipes {
            out.push_str(&format!("- {}\n", r.replace('\n', " ")));
        }
    }
    let out = out.trim().to_string();
    if out.is_empty() {
        return "(none recorded)".into();
    }
    crate::runner_util::head(&out, 8000)
}

/// The MR's worktree under `worktree_root` (`<repo>-mr-<iid>`, local branch
/// `loop/mr-<iid>` tracking `origin/<source_branch>`).
pub struct GitWorkspace {
    pub repo_path: String,
    pub worktree_root: String,
    pub mr_iid: String,
    /// Per check command.
    pub check_timeout: Duration,
    /// For git itself (the push may talk to the network).
    pub git_timeout: Duration,
}

impl GitWorkspace {
    pub fn new(repo_path: &str, worktree_root: &str, mr_iid: &str) -> Self {
        GitWorkspace {
            repo_path: repo_path.to_string(),
            worktree_root: worktree_root.to_string(),
            mr_iid: mr_iid.to_string(),
            check_timeout: Duration::from_secs(30 * 60),
            git_timeout: Duration::from_secs(5 * 60),
        }
    }

    /// `git <args>` in `dir`, no shell, never prompting for credentials.
    fn git(&self, dir: &Path, args: &[&str]) -> Result<String, String> {
        let mut cmd = git_command(dir, args);
        let out = crate::proc::run(&mut cmd, None, Some(self.git_timeout))
            .map_err(|e| format!("cannot run git: {e}"))?;
        if out.timed_out {
            return Err(format!("git {} timed out", args.join(" ")));
        }
        if !out.success() {
            return Err(format!(
                "git {} failed: {}",
                args.first().unwrap_or(&""),
                out.stderr.trim()
            ));
        }
        Ok(out.stdout)
    }
}

/// The runner's own git: no shell, no prompts, no repo hooks, no credentials env.
fn git_command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "core.hooksPath=/dev/null", "-C"])
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    for k in SCRUBBED_ENV {
        cmd.env_remove(k);
    }
    cmd
}

/// Credentials the agent and the check commands never inherit.
pub const SCRUBBED_ENV: [&str; 2] = ["GITLAB_TOKEN", "GITLAB_URL"];

/// Per-developer files that must never be pushed.
fn always_private(path: &str) -> bool {
    path == "config/master.key"
        || (path.starts_with("config/credentials/") && path.ends_with(".key"))
}

/// The command that installs a JS project's dependencies, when its worktree
/// has a lockfile but no `node_modules` (a fresh worktree never has one, so
/// `yarn test` would die with "jest: command not found", exit 127).
fn install_command(dir: &Path) -> Option<&'static str> {
    if !dir.join("package.json").is_file() {
        return None;
    }
    // `SHARP_IGNORE_GLOBAL_LIBVIPS`: with Homebrew's libvips on the daemon's PATH,
    // sharp skips its prebuilt binary and tries to compile from source.
    // The marker is written only when an install finishes, so a tree left by a
    // failed install is installed again.
    [
        (
            "yarn.lock",
            "SHARP_IGNORE_GLOBAL_LIBVIPS=1 yarn install --frozen-lockfile",
            ".yarn-integrity",
        ),
        (
            "pnpm-lock.yaml",
            "SHARP_IGNORE_GLOBAL_LIBVIPS=1 pnpm install --frozen-lockfile",
            ".modules.yaml",
        ),
        (
            "package-lock.json",
            "SHARP_IGNORE_GLOBAL_LIBVIPS=1 npm ci",
            ".package-lock.json",
        ),
    ]
    .into_iter()
    .find(|(lock, _, _)| dir.join(lock).is_file())
    .filter(|(_, _, marker)| !dir.join("node_modules").join(marker).exists())
    .map(|(_, cmd, _)| cmd)
}

/// Files the worktree setup copies in when the branch does not track them.
const COPIED_IF_UNTRACKED: [&str; 2] = [".ruby-version", "config/database.yml"];

impl GitWorkspace {
    /// [`Workspace::checks`] with `scrub` removed from the commands' environment.
    pub(crate) fn checks_scrubbing(
        &self,
        dir: &Path,
        v: &Verification,
        scrub: &[&str],
    ) -> Result<CheckResult, String> {
        let mut chosen = Vec::new();
        for (kind, cmd) in [("test", &v.test), ("lint", &v.lint)] {
            let cmd = cmd.trim();
            if cmd.is_empty() {
                if kind == "test" {
                    return Err("no test command".into());
                }
                continue;
            }
            check_verify_command(cmd).map_err(|e| format!("refused {kind} command: {e}"))?;
            chosen.push((kind, cmd));
        }
        let (mut passed, mut lines, mut details) = (true, Vec::new(), Vec::<String>::new());
        for (kind, cmd) in chosen {
            let r = CommandVerifier::new(&format!("mr_fix_{kind}"), cmd)
                .with_cwd(dir)
                .with_timeout(self.check_timeout.as_secs_f64())
                .with_env_removed(scrub)
                .run_once();
            passed &= r.passed;
            let status = match (r.passed, r.exit_code) {
                (true, _) => "passed".to_string(),
                (false, Some(c)) => format!("failed (exit {c})"),
                (false, None) => "failed (timed out or did not start)".to_string(),
            };
            lines.push(format!("`{cmd}` {status}"));
            if !r.passed {
                let out = crate::runner_util::tail(r.output.trim(), 1500);
                if !out.is_empty() {
                    details.push(format!("$ {cmd}\n{out}"));
                }
            }
        }
        let mut summary = lines.join("; ");
        if !details.is_empty() {
            summary = format!("{summary}\n{}", details.join("\n\n"));
        }
        Ok(CheckResult { passed, summary })
    }

    /// `git <args>` in `dir`: exit code and stdout (no error on a non-zero exit).
    fn git_status(&self, dir: &Path, args: &[&str]) -> Result<(Option<i32>, String), String> {
        self.git_input(dir, args, None)
    }

    /// [`Self::git_status`] with `input` on stdin.
    fn git_input(
        &self,
        dir: &Path,
        args: &[&str],
        input: Option<&str>,
    ) -> Result<(Option<i32>, String), String> {
        let mut cmd = git_command(dir, args);
        let out = crate::proc::run(&mut cmd, input, Some(self.git_timeout))
            .map_err(|e| format!("cannot run git: {e}"))?;
        if out.timed_out {
            return Err(format!("git {} timed out", args.join(" ")));
        }
        Ok((out.code, out.stdout))
    }

    /// Which of `paths` the `.gitignore` files of commit `base` ignore. The
    /// rules are read from `base`, not from the agent's HEAD, so a push that
    /// deletes a rule and adds the file it covered is still caught: base's
    /// `.gitignore` files are written into a scratch repository and
    /// `check-ignore --no-index` runs there, with no global excludes file.
    fn ignored_at(&self, dir: &Path, base: &str, paths: &[&str]) -> Result<Vec<String>, String> {
        if paths.is_empty() {
            return Ok(vec![]);
        }
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let tmp = Scratch(std::env::temp_dir().join(format!(
            "loopx-mr-fix-ignore-{}",
            crate::events::new_uuid_hex()
        )));
        std::fs::create_dir_all(&tmp.0).map_err(|e| format!("cannot create a scratch dir: {e}"))?;
        let (code, _) = self.git_status(&tmp.0, &["init", "-q"])?;
        if code != Some(0) {
            return Err("cannot create a scratch repository".into());
        }
        let tree = self.git(dir, &["ls-tree", "-r", "-z", "--name-only", base])?;
        for f in tree
            .split('\0')
            .filter(|f| *f == ".gitignore" || f.ends_with("/.gitignore"))
            .filter(|f| scratch_ignore_path_ok(f))
        {
            let text = self.git(dir, &["cat-file", "blob", &format!("{base}:{f}")])?;
            let dest = tmp.0.join(f);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&dest, text).map_err(|e| e.to_string())?;
        }
        let input = paths.join("\0");
        let (code, out) = self.git_input(
            &tmp.0,
            &[
                "-c",
                "core.excludesFile=/dev/null",
                "check-ignore",
                "--no-index",
                "-z",
                "--stdin",
            ],
            Some(&input),
        )?;
        if !matches!(code, Some(0) | Some(1)) {
            return Err("git check-ignore failed".into());
        }
        Ok(out
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// A worktree whose branch holds commits origin does not have is an earlier
    /// run's fix that was never pushed (its checks failed, say). The goal is a
    /// fix on the latest code of the MR branch, so those commits are saved on a
    /// `loopx-backup/mr-<iid>-<sha>` branch and the worktree is reset to
    /// `origin/<source_branch>`; the agent redoes the pending threads on top of it.
    /// Uncommitted changes are never discarded: they are refused.
    fn shelve_unpushed(&self, wt: &Path, source_branch: &str) -> Result<(), String> {
        if !wt.join(".git").exists() {
            return Ok(());
        }
        let origin_ref = format!("refs/remotes/origin/{source_branch}");
        // Reset to the latest branch, not to what the last run saw.
        self.git(wt, &["fetch", "origin", "--", source_branch])?;
        let range = format!("{origin_ref}..HEAD");
        let Ok((Some(0), out)) = self.git_status(wt, &["rev-list", "--count", &range]) else {
            return Ok(());
        };
        let n: usize = out.trim().parse().unwrap_or(0);
        if n == 0 {
            return Ok(());
        }
        if self.dirty(wt)? {
            return Err(format!(
                "{}: an earlier unpushed fix ({n} commit(s)) sits next to uncommitted changes, \
                 which are never discarded; commit or discard them by hand",
                wt.display()
            ));
        }
        let head = self.head(wt)?;
        let backup = format!(
            "loopx-backup/mr-{}-{}",
            self.mr_iid,
            head.chars().take(10).collect::<String>()
        );
        self.git(wt, &["branch", "-f", &backup, &head])?;
        self.git(wt, &["reset", "--hard", &origin_ref])?;
        eprintln!(
            "discarded {n} unpushed commit(s) from an earlier run of this MR's fix and reset \
             to origin/{source_branch}; they are kept on branch {backup}"
        );
        Ok(())
    }
}

/// A `.gitignore` path from the base tree that may be copied into the scratch repo.
fn scratch_ignore_path_ok(f: &str) -> bool {
    !f.starts_with('/') && !f.split('/').any(|c| c == "..")
}

impl Workspace for GitWorkspace {
    fn prepare(&self, source_branch: &str) -> Result<PathBuf, String> {
        crate::worktree::check_source_branch(source_branch).map_err(|f| f.message)?;
        let wt = PathBuf::from(crate::worktree::mr_worktree_path(
            &self.repo_path,
            &self.mr_iid,
            &self.worktree_root,
        ));
        // Before syncing, so a diverged branch is reset, not "cannot fast-forward".
        self.shelve_unpushed(&wt, source_branch)?;
        let dir = crate::worktree::new_mr_worktree(
            &self.repo_path,
            &self.mr_iid,
            source_branch,
            &self.worktree_root,
        )
        .map(PathBuf::from)
        .map_err(|f| f.message)?;
        self.shelve_unpushed(&dir, source_branch)?;
        if let Some(cmd) = install_command(&dir) {
            let r = CommandVerifier::new("mr_fix_install", cmd)
                .with_cwd(&dir)
                .with_timeout(self.check_timeout.as_secs_f64())
                .with_env_removed(&SCRUBBED_ENV)
                .run_once();
            if !r.passed {
                let out = crate::runner_util::tail(r.output.trim(), 1500);
                return Err(format!("`{cmd}` failed in the new worktree\n{out}"));
            }
        }
        Ok(dir)
    }

    fn head(&self, dir: &Path) -> Result<String, String> {
        Ok(self.git(dir, &["rev-parse", "HEAD"])?.trim().to_string())
    }

    fn dirty(&self, dir: &Path) -> Result<bool, String> {
        Ok(!self
            .git(dir, &["status", "--porcelain", "--untracked-files=all"])?
            .trim()
            .is_empty())
    }

    fn commits_since(&self, dir: &Path, base: &str) -> Result<Vec<Commit>, String> {
        if base.is_empty() || !base.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("not a commit id: {base:?}"));
        }
        self.git(dir, &["merge-base", "--is-ancestor", base, "HEAD"])
            .map_err(|_| "the branch history was rewritten since the run started".to_string())?;
        let range = format!("{base}..HEAD");
        let log = self.git(dir, &["log", "--reverse", "--format=%H %ct", &range])?;
        Ok(log
            .lines()
            .filter_map(|l| {
                let (sha, t) = l.split_once(' ')?;
                Some(Commit {
                    sha: sha.to_string(),
                    time: t.trim().parse().ok()?,
                })
            })
            .collect())
    }

    fn checks(&self, dir: &Path, v: &Verification) -> Result<CheckResult, String> {
        self.checks_scrubbing(dir, v, &SCRUBBED_ENV)
    }

    fn forbidden_changes(&self, dir: &Path, base: &str) -> Result<Vec<String>, String> {
        if base.is_empty() || !base.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("not a commit id: {base:?}"));
        }
        // NUL-separated: no quoting of non-ASCII or unusual names.
        let names = self.git(
            dir,
            &["diff", "--name-only", "-z", "--no-renames", base, "HEAD"],
        )?;
        let paths: Vec<&str> = names.split('\0').filter(|p| !p.is_empty()).collect();
        let mut bad = Vec::new();
        for p in &paths {
            let copied = COPIED_IF_UNTRACKED.contains(p)
                && self
                    .git_status(dir, &["cat-file", "-e", &format!("{base}:{p}")])?
                    .0
                    != Some(0);
            if always_private(p) || copied {
                bad.push(p.to_string());
            }
        }
        for p in self.ignored_at(dir, base, &paths)? {
            if !bad.contains(&p) {
                bad.push(p);
            }
        }
        Ok(bad)
    }

    fn push(&self, dir: &Path, source_branch: &str) -> Result<(), String> {
        // The branch name comes from the untrusted MR: checked again here.
        crate::worktree::check_source_branch(source_branch).map_err(|f| f.message)?;
        let refspec = format!("HEAD:refs/heads/{source_branch}");
        self.git(dir, &["push", "--porcelain", "origin", &refspec])
            .map(|_| ())
    }

    fn remove(&self) -> Result<(), String> {
        crate::worktree::remove_mr_worktree(&self.repo_path, &self.mr_iid, &self.worktree_root)
            .map_err(|f| f.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                // Hermetic: the developer's global ignore file plays no part.
                "-c",
                "core.excludesFile=/dev/null",
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

    /// A bare origin with `main` and an MR branch `feat/x`, a local clone,
    /// and a [`GitWorkspace`] for MR 7 whose worktree is prepared.
    fn setup(d: &Path) -> (GitWorkspace, PathBuf, PathBuf) {
        let origin = d.join("origin.git");
        std::fs::create_dir(&origin).unwrap();
        sh(&origin, &["init", "-q", "--bare", "-b", "main"]);
        sh(d, &["clone", "-q", &origin.display().to_string(), "local"]);
        let local = d.join("local");
        sh(&local, &["checkout", "-q", "-b", "main"]);
        std::fs::write(local.join("a.txt"), "1").unwrap();
        std::fs::write(local.join("Makefile"), "test:\n\ttrue\nlint:\n\tfalse\n").unwrap();
        sh(&local, &["add", "."]);
        sh(&local, &["commit", "-qm", "init"]);
        sh(&local, &["push", "-q", "origin", "main"]);
        sh(&local, &["checkout", "-q", "-b", "feat/x"]);
        sh(&local, &["push", "-q", "origin", "feat/x"]);
        sh(&local, &["checkout", "-q", "main"]);
        let ws = GitWorkspace::new(
            &local.display().to_string(),
            &d.join("wt").display().to_string(),
            "7",
        );
        let dir = ws.prepare("feat/x").unwrap();
        (ws, dir, origin)
    }

    fn commit(dir: &Path, file: &str) {
        std::fs::write(dir.join(file), file).unwrap();
        sh(dir, &["add", file]);
        sh(dir, &["commit", "-qm", file]);
    }

    #[test]
    fn scratch_ignore_copy_skips_escaping_paths() {
        assert!(scratch_ignore_path_ok(".gitignore"));
        assert!(scratch_ignore_path_ok("a/b/.gitignore"));
        assert!(!scratch_ignore_path_ok("../.gitignore"));
        assert!(!scratch_ignore_path_ok("a/../../.gitignore"));
        assert!(!scratch_ignore_path_ok("/etc/.gitignore"));
        assert!(scratch_ignore_path_ok("a/..b/.gitignore"));
    }

    #[test]
    fn mr_info_reads_the_fields_the_runner_needs() {
        let i = mr_info(
            &json!({"iid": 7, "state": "opened", "title": "T", "description": null,
            "source_branch": "feat/x", "changes": [{"new_path": "a"}]}),
        );
        assert_eq!(i.iid, "7");
        assert_eq!(i.state, "opened");
        assert_eq!(i.description, "");
        assert_eq!(i.source_branch, "feat/x");
        assert_eq!(i.changes.len(), 1);
        assert_eq!(i.project_path, "");
        let j = mr_info(&json!({"iid": 7, "references": {"full": "grp/sub/web!7"}}));
        assert_eq!(j.project_path, "grp/sub/web");
    }

    #[test]
    fn the_runners_git_ignores_repo_hooks_and_credentials() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, origin) = setup(d.path());
        // A hook the MR's repo controls must not run with the runner's token.
        let hooks = d.path().join("local/.git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-push");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        commit(&dir, "fix.txt");
        ws.push(&dir, "feat/x").unwrap();
        assert_eq!(
            sh(&origin, &["rev-parse", "refs/heads/feat/x"]),
            ws.head(&dir).unwrap()
        );
        let cmd = git_command(&dir, &["status"]);
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|w| w == ["-c", "core.hooksPath=/dev/null"]),
            "{args:?}"
        );
        let envs: Vec<_> = cmd.get_envs().collect();
        for k in SCRUBBED_ENV {
            assert!(
                envs.contains(&(std::ffi::OsStr::new(k), None)),
                "{k}: {envs:?}"
            );
        }
    }

    /// Rewrite the MR branch on origin from another clone (a rebase + force push).
    fn force_push_rewrite(d: &Path, origin: &Path) -> String {
        let other = d.join("other-force");
        sh(
            d,
            &[
                "clone",
                "-q",
                "-b",
                "feat/x",
                &origin.display().to_string(),
                "other-force",
            ],
        );
        commit(&other, "rebased.txt");
        sh(&other, &["commit", "-q", "--amend", "-m", "rewritten"]);
        sh(&other, &["reset", "-q", "--soft", "HEAD~1"]);
        sh(&other, &["commit", "-qm", "rewritten history"]);
        sh(&other, &["push", "-q", "-f", "origin", "feat/x"]);
        sh(&other, &["rev-parse", "HEAD"])
    }

    #[test]
    fn a_force_pushed_source_branch_is_followed_when_nothing_is_unpushed() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, origin) = setup(d.path());
        // A first fix was pushed, then the author rebased the branch.
        commit(&dir, "fix.txt");
        ws.push(&dir, "feat/x").unwrap();
        sh(&dir, &["fetch", "-q", "origin"]);
        let other = d.path().join("other-force");
        sh(
            d.path(),
            &[
                "clone",
                "-q",
                "-b",
                "main",
                &origin.display().to_string(),
                "other-force",
            ],
        );
        sh(&other, &["checkout", "-q", "-b", "rebased"]);
        commit(&other, "rebased.txt");
        sh(&other, &["push", "-q", "-f", "origin", "rebased:feat/x"]);
        let new_head = sh(&other, &["rev-parse", "HEAD"]);
        let again = ws.prepare("feat/x").unwrap();
        assert_eq!(again, dir);
        assert_eq!(ws.head(&dir).unwrap(), new_head);
        assert!(dir.join("rebased.txt").is_file());
    }

    fn backups(repo: &Path) -> Vec<String> {
        sh(
            repo,
            &[
                "branch",
                "--list",
                "loopx-backup/*",
                "--format=%(refname:short)",
            ],
        )
        .lines()
        .map(str::to_string)
        .collect()
    }

    #[test]
    fn a_force_push_with_an_unpushed_local_fix_follows_origin_and_keeps_a_backup() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, origin) = setup(d.path());
        commit(&dir, "mine.txt");
        let ours = ws.head(&dir).unwrap();
        force_push_rewrite(d.path(), &origin);
        let again = ws.prepare("feat/x").unwrap();
        assert_eq!(again, dir);
        assert!(
            !dir.join("mine.txt").exists(),
            "the worktree follows origin"
        );
        assert!(dir.join("rebased.txt").is_file());
        let kept = backups(&dir);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(sh(&dir, &["rev-parse", &kept[0]]), ours);
    }

    #[test]
    fn an_earlier_unpushed_fix_is_backed_up_and_the_worktree_follows_origin() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, origin) = setup(d.path());
        commit(&dir, "fix.txt");
        let ours = ws.head(&dir).unwrap();
        // The branch also moved on origin meanwhile (a new commit by the author).
        sh(
            d.path(),
            &[
                "clone",
                "-q",
                "-b",
                "feat/x",
                &origin.display().to_string(),
                "other",
            ],
        );
        commit(&d.path().join("other"), "theirs.txt");
        sh(&d.path().join("other"), &["push", "-q", "origin", "feat/x"]);
        let theirs = sh(&d.path().join("other"), &["rev-parse", "HEAD"]);
        assert_eq!(ws.prepare("feat/x").unwrap(), dir);
        assert_eq!(ws.head(&dir).unwrap(), theirs, "latest code on the branch");
        assert!(!dir.join("fix.txt").exists());
        let kept = backups(&dir);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(sh(&dir, &["rev-parse", &kept[0]]), ours, "nothing is lost");
        // A second prepare has nothing left to shelve.
        assert!(ws.prepare("feat/x").is_ok());
        assert_eq!(backups(&dir).len(), 1);
    }

    #[test]
    fn an_unpushed_fix_with_uncommitted_changes_is_still_refused() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, _) = setup(d.path());
        commit(&dir, "fix.txt");
        std::fs::write(dir.join("wip.txt"), "x").unwrap();
        let e = ws.prepare("feat/x").unwrap_err();
        assert!(e.contains("uncommitted changes"), "{e}");
        assert!(dir.join("fix.txt").is_file() && dir.join("wip.txt").is_file());
        assert!(backups(&dir).is_empty());
    }

    #[test]
    fn workspace_reports_head_dirt_and_new_commits_in_order() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, _) = setup(d.path());
        assert!(dir.ends_with("local-mr-7"));
        let base = ws.head(&dir).unwrap();
        assert!(!ws.dirty(&dir).unwrap());
        assert!(ws.commits_since(&dir, &base).unwrap().is_empty());
        std::fs::write(dir.join("new.txt"), "n").unwrap();
        assert!(ws.dirty(&dir).unwrap(), "untracked files count");
        std::fs::remove_file(dir.join("new.txt")).unwrap();
        commit(&dir, "one.txt");
        commit(&dir, "two.txt");
        let c = ws.commits_since(&dir, &base).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].sha, ws.head(&dir).unwrap());
        assert!(c[0].time > 1_600_000_000);
        // Rewritten history (base no longer an ancestor) is an error.
        sh(&dir, &["reset", "-q", "--hard", &format!("{base}~0")]);
        sh(
            &dir,
            &[
                "commit",
                "-q",
                "--amend",
                "-m",
                "rewritten",
                "--allow-empty",
            ],
        );
        assert!(ws.commits_since(&dir, &base).is_err());
        assert!(ws.commits_since(&dir, "HEAD; rm").is_err());
    }

    #[test]
    fn push_updates_the_source_branch() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, origin) = setup(d.path());
        commit(&dir, "fix.txt");
        ws.push(&dir, "feat/x").unwrap();
        assert_eq!(
            sh(&origin, &["rev-parse", "refs/heads/feat/x"]),
            ws.head(&dir).unwrap()
        );
    }

    #[test]
    fn a_rejected_push_fails_and_keeps_the_commits() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, origin) = setup(d.path());
        // Someone else pushes to the MR branch while the agent works.
        sh(
            d.path(),
            &[
                "clone",
                "-q",
                "-b",
                "feat/x",
                &origin.display().to_string(),
                "other",
            ],
        );
        commit(&d.path().join("other"), "theirs.txt");
        sh(&d.path().join("other"), &["push", "-q", "origin", "feat/x"]);
        commit(&dir, "fix.txt");
        let ours = ws.head(&dir).unwrap();
        let e = ws.push(&dir, "feat/x").unwrap_err();
        assert!(e.contains("push failed"), "{e}");
        assert_eq!(ws.head(&dir).unwrap(), ours);
        assert!(dir.join("fix.txt").is_file());
        assert_ne!(sh(&origin, &["rev-parse", "refs/heads/feat/x"]), ours);
    }

    #[test]
    fn a_failed_check_reports_the_tail_of_its_output() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, _) = setup(d.path());
        std::fs::write(
            dir.join("Makefile"),
            "test:\n\ttrue\nlint:\n\techo first-line; echo 'next: command not found'; exit 127\n",
        )
        .unwrap();
        let v = Verification {
            test: "make test".into(),
            lint: "make lint".into(),
        };
        let r = ws.checks(&dir, &v).unwrap();
        assert!(!r.passed);
        let (head, detail) = r.summary.split_once('\n').expect(&r.summary);
        assert!(head.contains("`make lint` failed (exit 2)"), "{head}");
        assert!(detail.contains("next: command not found"), "{detail}");
        // A passing command adds no detail.
        let ok = ws
            .checks(
                &dir,
                &Verification {
                    test: "make test".into(),
                    lint: String::new(),
                },
            )
            .unwrap();
        assert!(!ok.summary.contains('\n'), "{}", ok.summary);
    }

    #[test]
    fn install_command_follows_the_lockfile_and_skips_only_complete_installs() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        assert_eq!(install_command(p), None);
        std::fs::write(p.join("package.json"), "{}").unwrap();
        assert_eq!(install_command(p), None, "no lockfile, no guess");
        std::fs::write(p.join("package-lock.json"), "{}").unwrap();
        assert_eq!(
            install_command(p),
            Some("SHARP_IGNORE_GLOBAL_LIBVIPS=1 npm ci")
        );
        std::fs::write(p.join("yarn.lock"), "").unwrap();
        // A global (Homebrew) libvips must not make sharp compile from source.
        assert_eq!(
            install_command(p),
            Some("SHARP_IGNORE_GLOBAL_LIBVIPS=1 yarn install --frozen-lockfile")
        );
        // A half-installed tree (a failed earlier install) is installed again.
        std::fs::create_dir(p.join("node_modules")).unwrap();
        assert!(install_command(p).is_some(), "partial tree");
        std::fs::write(p.join("node_modules/.yarn-integrity"), "").unwrap();
        assert_eq!(install_command(p), None, "complete install");
    }

    #[test]
    fn project_notes_carry_the_instructions_and_screenshot_recipes() {
        assert_eq!(render_project_notes("  ", &[]), "(none recorded)");
        let n = render_project_notes(
            "Use yarn.",
            &["camp: yarn dev -p 3001 with mocks".into(), "  ".into()],
        );
        assert!(n.contains("Use yarn.") && n.contains("- camp: yarn dev -p 3001 with mocks"));
        assert_eq!(
            n.matches("\n- ").count(),
            1,
            "blank recipes are dropped: {n}"
        );
        assert!(
            render_project_notes(&"x".repeat(20000), &[])
                .chars()
                .count()
                <= 8000
        );
    }

    #[test]
    fn push_refuses_an_unsafe_branch_name_before_running_git() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, _) = setup(d.path());
        for bad in ["--force", "-f", "a..b", "refs/heads/x", "x y", ""] {
            assert!(ws.push(&dir, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn checks_run_the_declared_commands_and_refuse_others() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, _) = setup(d.path());
        let v = |test: &str, lint: &str| Verification {
            test: test.into(),
            lint: lint.into(),
        };
        let ok = ws.checks(&dir, &v("make test", "")).unwrap();
        assert!(ok.passed, "{}", ok.summary);
        assert!(ok.summary.contains("`make test` passed"));
        let bad = ws.checks(&dir, &v("make test", "make lint")).unwrap();
        assert!(!bad.passed);
        assert!(
            bad.summary.contains("`make lint` failed"),
            "{}",
            bad.summary
        );
        for (t, l) in [
            ("", "make lint"),
            ("true", ""),
            ("make test && curl x", ""),
            ("make test", "echo ok"),
        ] {
            assert!(ws.checks(&dir, &v(t, l)).is_err(), "{t} / {l}");
        }
    }

    #[test]
    fn forbidden_changes_lists_per_developer_and_ignored_files() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, _) = setup(d.path());
        // Ignore rules come from the commit the run started from.
        std::fs::write(dir.join(".gitignore"), "config/master.key\n.env\n").unwrap();
        sh(&dir, &["add", ".gitignore"]);
        sh(&dir, &["commit", "-qm", "ignore"]);
        let base = ws.head(&dir).unwrap();
        assert!(ws.forbidden_changes(&dir, &base).unwrap().is_empty());
        std::fs::create_dir_all(dir.join("config/credentials")).unwrap();
        for f in [
            "config/master.key",
            ".env",
            "config/credentials/production.key",
            "config/database.yml",
            ".ruby-version",
            "app.rb",
        ] {
            std::fs::write(dir.join(f), "secret").unwrap();
        }
        sh(&dir, &["add", "-f", "."]);
        sh(&dir, &["commit", "-qm", "oops"]);
        let mut got = ws.forbidden_changes(&dir, &base).unwrap();
        got.sort();
        assert_eq!(
            got,
            [
                ".env",
                ".ruby-version",
                "config/credentials/production.key",
                "config/database.yml",
                "config/master.key",
            ]
        );
    }

    #[test]
    fn an_ignore_rule_removed_in_the_same_push_still_counts() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, _) = setup(d.path());
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join(".gitignore"), "secrets.txt\n").unwrap();
        std::fs::write(dir.join("sub/.gitignore"), "données/\n").unwrap();
        sh(&dir, &["add", ".gitignore"]);
        sh(&dir, &["commit", "-qm", "ignore"]);
        sh(&dir, &["add", "sub/.gitignore"]);
        sh(&dir, &["commit", "-qm", "sub ignore"]);
        let base = ws.head(&dir).unwrap();
        // One commit drops both rules and adds the files they covered.
        std::fs::write(dir.join(".gitignore"), "").unwrap();
        std::fs::remove_file(dir.join("sub/.gitignore")).unwrap();
        std::fs::create_dir_all(dir.join("sub/données")).unwrap();
        std::fs::write(dir.join("secrets.txt"), "s").unwrap();
        std::fs::write(dir.join("sub/données/clé.txt"), "s").unwrap();
        std::fs::write(dir.join("app.rb"), "ok").unwrap();
        sh(&dir, &["add", "-A"]);
        sh(&dir, &["commit", "-qm", "sneaky"]);
        let mut got = ws.forbidden_changes(&dir, &base).unwrap();
        got.sort();
        assert_eq!(got, ["secrets.txt", "sub/données/clé.txt"]);
    }

    #[test]
    fn an_ignored_non_ascii_file_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, _) = setup(d.path());
        std::fs::write(dir.join(".gitignore"), "données/\n").unwrap();
        sh(&dir, &["add", ".gitignore"]);
        sh(&dir, &["commit", "-qm", "ignore"]);
        let base = ws.head(&dir).unwrap();
        std::fs::create_dir_all(dir.join("données")).unwrap();
        std::fs::write(dir.join("données/clé \"x\".txt"), "s").unwrap();
        sh(&dir, &["add", "-f", "données"]);
        sh(&dir, &["commit", "-qm", "oops"]);
        assert_eq!(
            ws.forbidden_changes(&dir, &base).unwrap(),
            ["données/clé \"x\".txt"]
        );
    }

    #[test]
    fn an_ordinary_change_is_not_forbidden() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, _) = setup(d.path());
        std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        sh(&dir, &["add", ".gitignore"]);
        sh(&dir, &["commit", "-qm", "ignore"]);
        let base = ws.head(&dir).unwrap();
        commit(&dir, "app.rb");
        commit(&dir, "naïve file.rb");
        assert!(ws.forbidden_changes(&dir, &base).unwrap().is_empty());
    }

    #[test]
    fn copied_files_tracked_at_base_may_change() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, _) = setup(d.path());
        std::fs::write(dir.join(".ruby-version"), "3.2").unwrap();
        commit(&dir, ".ruby-version");
        let base = ws.head(&dir).unwrap();
        std::fs::write(dir.join(".ruby-version"), "3.3").unwrap();
        sh(&dir, &["commit", "-qam", "bump"]);
        assert!(ws.forbidden_changes(&dir, &base).unwrap().is_empty());
    }

    #[test]
    fn checks_run_without_gitlab_credentials_in_the_environment() {
        assert!(SCRUBBED_ENV.contains(&"GITLAB_TOKEN"));
        assert!(SCRUBBED_ENV.contains(&"GITLAB_URL"));
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, _) = setup(d.path());
        // The runner strips SCRUBBED_ENV; prove the mechanism on a unique name.
        std::env::set_var("LOOPX_MR_FIX_SCRUB_PROBE", "secret");
        std::fs::write(
            dir.join("Makefile"),
            "test:\n\ttest -z \"$$LOOPX_MR_FIX_SCRUB_PROBE\"\n",
        )
        .unwrap();
        let v = Verification {
            test: "make test".into(),
            lint: String::new(),
        };
        let r = ws
            .checks_scrubbing(&dir, &v, &["LOOPX_MR_FIX_SCRUB_PROBE"])
            .unwrap();
        assert!(r.passed, "{}", r.summary);
        let r = ws.checks_scrubbing(&dir, &v, &[]).unwrap();
        assert!(!r.passed, "the probe is visible without scrubbing");
    }

    #[test]
    fn remove_deletes_the_worktree() {
        let d = tempfile::tempdir().unwrap();
        let (ws, dir, _) = setup(d.path());
        ws.remove().unwrap();
        assert!(!dir.exists());
    }
}
