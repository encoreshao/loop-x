//! The fix agent: its fenced `claude -p` argv, the real [`Agent`] that runs it,
//! and the plan-file handshake behind `loopx mr-fix announce <plan-file>`.
//!
//! Handshake: the agent writes its plan draft and runs the announce command,
//! which validates the draft ([`parse_plan`]) and writes it to the plan path
//! the runner gave it in `$LOOPX_MR_FIX_PLAN` ([`announce_request`]). The
//! runner, polling while the session runs, takes the plan file, posts the start
//! note, and answers in `<plan>.ack` (`ok` or `error: <why>`). The runner trusts
//! nothing here: it re-validates the plan and gates the push on its own checks.

use super::run::{parse_handoff, parse_plan, Agent, AgentHandoff, Announce};
use crate::creator::spawn::{claude_argv, common_denied, Fence, JobFence};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Spend cap of one fix session (`claude --max-budget-usd`).
pub const FIX_BUDGET_USD: f64 = 10.0;
/// The env var naming the plan file the runner reads.
pub const PLAN_ENV: &str = "LOOPX_MR_FIX_PLAN";
/// The only built-in tools the session has; permission rules narrow them.
const FIX_TOOLS: &str = "Bash,Read,Write,Edit,Grep,Glob";

/// Read-only and committing git commands the agent may run.
const GIT_ALLOWED: [&str; 6] = [
    "Bash(git status*)",
    "Bash(git diff*)",
    "Bash(git log*)",
    "Bash(git show*)",
    "Bash(git add *)",
    "Bash(git commit *)",
];

/// Git that pushes, fetches, rewrites, switches, configures or writes files
/// outside the tree; a deny wins over the allows above.
const GIT_DENIED: [&str; 29] = [
    "Bash(git push*)",
    // `git -C x push`, `git --no-pager push`: an option before the subcommand.
    "Bash(git -* push*)",
    "Bash(git remote*)",
    "Bash(git config*)",
    "Bash(git -c*)",
    "Bash(git -C*)",
    "Bash(git --git-dir*)",
    "Bash(git --work-tree*)",
    "Bash(git fetch*)",
    "Bash(git pull*)",
    "Bash(git merge*)",
    "Bash(git rebase*)",
    "Bash(git reset*)",
    "Bash(git checkout*)",
    "Bash(git switch*)",
    "Bash(git clean*)",
    "Bash(git worktree*)",
    "Bash(git branch*)",
    "Bash(git tag*)",
    "Bash(git commit*--amend*)",
    "Bash(git *--output*)",
    "Bash(git *--ext-diff*)",
    // Force-adding ignored files, and reading arbitrary files into the repo
    // or a commit message. A leading space keeps `git add my-feature.rb` usable.
    "Bash(git add* -f*)",
    "Bash(git add* --force*)",
    "Bash(git * --no-index*)",
    "Bash(git commit* -F*)",
    "Bash(git commit* --file*)",
    "Bash(git commit* --template*)",
    // `git add` / `git commit` reading their paths from an arbitrary file.
    "Bash(git *--pathspec-from-file*)",
];

/// Credential files outside the worktree, under the user's home (on top of
/// the Creator list, which already has `.ssh`, `.aws`, `.config/gh`, ...).
const HOME_SECRETS: [&str; 9] = [
    ".git-credentials",
    ".config/git/credentials",
    ".config/glab-cli/**",
    ".config/gh/**",
    ".kube/**",
    ".gnupg/**",
    ".pgpass",
    ".aws/**",
    ".ssh/**",
];

/// loopx subcommands denied by name (the announce command is the only one
/// allowed; these are listed so a broad future allow cannot reopen them).
const LOOPX_DENIED: [&str; 14] = [
    "gitlab",
    "mr ",
    "mr-fix run",
    "mr-fix stop",
    "issues",
    "notify",
    "slack",
    "status",
    "serve",
    "chat-tool",
    "config",
    "secrets",
    "worktree",
    "events",
];

fn strs(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// The plan draft the agent writes before announcing.
pub fn plan_draft_path(scratch: &Path) -> PathBuf {
    scratch.join("plan-draft.json")
}

/// Where `announce` puts the validated plan for the runner.
pub fn plan_path(scratch: &Path) -> PathBuf {
    scratch.join("plan.json")
}

/// The runner's answer to a plan.
pub fn ack_path(plan: &Path) -> PathBuf {
    let mut s = plan.as_os_str().to_owned();
    s.push(".ack");
    PathBuf::from(s)
}

/// Where the agent saves screenshots (`loopx screenshot --out <dir>/1-after.png`).
pub fn screens_path(scratch: &Path) -> PathBuf {
    scratch.join("screens")
}

/// The agent's final report.
pub fn handoff_path(scratch: &Path) -> PathBuf {
    scratch.join("handoff.json")
}

/// The announce command exactly as the agent must run it (and as it is allowed).
pub fn announce_command(exe: &str, scratch: &Path) -> String {
    format!(
        "{exe} mr-fix announce {}",
        plan_draft_path(scratch).display()
    )
}

/// `claude -p` for one fix session, sealed like the Creator jobs (no MCP, no
/// user settings, hooks off, spend cap), with:
/// - reads and edits only in the worktree (`f.cwd`) and the job's `scratch` dir;
/// - git limited to status/diff/log/show/add/commit, `git push` and every
///   history, remote or config command denied (the runner pushes);
/// - the project's stack lint/test commands (`verify_commands`);
/// - one loopx command: `<exe> mr-fix announce <scratch>/plan-draft.json`,
///   exactly; every GitLab, Slack, dashboard and network tool denied.
pub fn fix_argv(
    cli: &str,
    prompt: &str,
    f: &Fence<'_>,
    scratch: &Path,
) -> Result<Vec<String>, String> {
    if cli != "claude" {
        return Err(format!(
            "Fix now needs the Claude CLI: the selected AI CLI ({cli}) cannot be \
             limited to a fixed set of tools. Switch CLI in Settings -> AI CLI."
        ));
    }
    // Rules are matched against resolved paths (macOS: /var -> /private/var).
    let resolve = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (cwd, scratch) = (resolve(f.cwd), resolve(scratch));
    let scratch = scratch.as_path();
    let f = &Fence {
        cwd: &cwd,
        user_home: f.user_home,
        loopx_home: f.loopx_home,
        exe: f.exe,
    };
    let c = f.cwd.display();
    let sc = scratch.display();
    let mut allowed = strs(&GIT_ALLOWED);
    allowed.extend(crate::verify_commands::claude_patterns());
    allowed.push("Bash(RAILS_ENV=test bundle exec rake db:test:prepare*)".into());
    let draft = plan_draft_path(scratch);
    allowed.push(format!("Bash(loopx mr-fix announce {})", draft.display()));
    allowed.push(format!("Bash({})", announce_command(f.exe, scratch)));
    // Screenshots of the page the fix changes: `loopx screenshot` only visits
    // localhost and starts only a known dev-server command, so it is safe to
    // allow as a whole (every other loopx command stays denied).
    allowed.push("Bash(loopx screenshot *)".into());
    allowed.push(format!("Bash({} screenshot *)", f.exe));
    for root in [&c as &dyn std::fmt::Display, &sc] {
        allowed.push(format!("Read(/{root}/**)"));
        allowed.push(format!("Edit(/{root}/**)"));
    }
    let mut denied = narrow_home_denies(common_denied(f), &[f.cwd, scratch]);
    let h = f.user_home.display();
    for p in HOME_SECRETS {
        let rule = format!("Read(/{h}/{p})");
        if !denied.contains(&rule) {
            denied.push(rule);
        }
    }
    denied.extend(strs(&GIT_DENIED));
    for sub in LOOPX_DENIED {
        denied.push(format!("Bash(loopx {sub}*)"));
        denied.push(format!("Bash({} {sub}*)", f.exe));
    }
    denied.extend(strs(&["WebFetch", "WebSearch"]));
    // CI config runs on the forge with the project's secrets.
    // `.git` is a file in a worktree: repointing it would move the commits.
    for p in [".gitlab-ci.yml", ".github/**", ".git", ".git/**"] {
        denied.push(format!("Edit(/{c}/{p})"));
    }
    Ok(claude_argv(
        prompt,
        f.cwd,
        JobFence {
            tools: FIX_TOOLS,
            allowed,
            denied,
            budget_usd: FIX_BUDGET_USD,
            extra_dirs: vec![scratch.to_path_buf()],
        },
    ))
}

/// A blanket `Read(/<dir>/**)` deny over a directory that holds the worktree
/// or the scratch dir (the default `worktree_root` is `~/.loopx/worktrees`)
/// would hide the agent's own files, and a deny beats any allow. Each such
/// rule becomes the narrow set the setup doctor uses: the directory's
/// top-level config files plus the Fix now job records. Other rules are kept.
fn narrow_home_denies(denied: Vec<String>, work: &[&Path]) -> Vec<String> {
    let mut out = Vec::new();
    for rule in denied {
        let dir = rule
            .strip_prefix("Read(/")
            .and_then(|r| r.strip_suffix("/**)"))
            .filter(|d| !d.contains('*'))
            .map(PathBuf::from);
        match dir {
            Some(d) if work.iter().any(|w| w.starts_with(&d)) => {
                for p in ["*.json", "*.yaml", "*.yml", "mr-fix/**"] {
                    let narrow = format!("Read(/{}/{p})", d.display());
                    if !out.contains(&narrow) {
                        out.push(narrow);
                    }
                }
            }
            _ => out.push(rule),
        }
    }
    out
}

/// Write `text` to `path` atomically (temp file + rename).
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = {
        let mut s = path.as_os_str().to_owned();
        s.push(".tmp");
        PathBuf::from(s)
    };
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// The announce command's side of the handshake: validate the draft, hand it
/// to the runner at `plan`, and wait for its answer. `Ok(n)` once the start
/// note listing `n` changes is posted.
pub fn announce_request(
    draft: &Path,
    plan: &Path,
    timeout: Duration,
    poll: Duration,
) -> Result<usize, String> {
    let text = std::fs::read_to_string(draft)
        .map_err(|e| format!("cannot read {}: {e}", draft.display()))?;
    let n = parse_plan(&text)?.len();
    let ack = ack_path(plan);
    let _ = std::fs::remove_file(&ack);
    write_atomic(plan, &text)?;
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(answer) = std::fs::read_to_string(&ack) {
            let _ = std::fs::remove_file(&ack);
            let answer = answer.trim();
            return match answer.strip_prefix("error: ") {
                Some(e) => Err(e.to_string()),
                None if answer == "ok" => Ok(n),
                None => Err(format!("unexpected answer from LoopX: {answer}")),
            };
        }
        std::thread::sleep(poll);
    }
    Err("LoopX did not confirm the start note in time; do not edit anything".into())
}

/// The session's process: in `dir`, with the plan path, without credentials.
fn agent_command(argv: &[String], dir: &Path, plan: &Path) -> Result<Command, String> {
    let (program, args) = argv.split_first().ok_or("empty agent command")?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .env(PLAN_ENV, plan)
        .stdin(Stdio::null());
    for k in super::real::SCRUBBED_ENV {
        cmd.env_remove(k);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    Ok(cmd)
}

/// Builds the session argv from the final prompt.
pub type ArgvFn = dyn Fn(&str) -> Result<Vec<String>, String>;

/// The real agent: one `claude -p` session (see [`fix_argv`]) in the worktree.
pub struct ClaudeAgent {
    pub argv: Box<ArgvFn>,
    /// The job's private folder for the plan and handoff files.
    pub scratch: PathBuf,
    /// [`announce_command`] for this job.
    pub announce_cmd: String,
    /// The loopx binary, for the screenshot command in the prompt.
    pub exe: String,
    pub timeout: Duration,
    pub poll: Duration,
}

impl ClaudeAgent {
    pub fn new(argv: Box<ArgvFn>, scratch: PathBuf, exe: &str) -> Self {
        // The permission rules hold resolved paths (macOS: /var -> /private/var),
        // so the paths the prompt hands the agent must be resolved too, or its
        // writes and the announce command are denied.
        let scratch = std::fs::create_dir_all(&scratch)
            .and_then(|_| std::fs::canonicalize(&scratch))
            .unwrap_or(scratch);
        ClaudeAgent {
            announce_cmd: announce_command(exe, &scratch),
            exe: exe.to_string(),
            argv,
            scratch,
            timeout: Duration::from_secs(60 * 60),
            poll: Duration::from_millis(500),
        }
    }

    /// Take a plan file the session left, post it, and answer. `Err` is fatal
    /// (the start note could not be posted): the session must be stopped.
    fn take_plan(&self, announce: &Announce<'_>, announced: &mut bool) -> Result<(), String> {
        let plan = plan_path(&self.scratch);
        let Ok(text) = std::fs::read_to_string(&plan) else {
            return Ok(());
        };
        let _ = std::fs::remove_file(&plan);
        let ack = ack_path(&plan);
        if *announced {
            return write_atomic(&ack, "error: the plan was already announced");
        }
        let changes = match parse_plan(&text) {
            Ok(c) => c,
            // The agent may fix its plan and try again.
            Err(e) => return write_atomic(&ack, &format!("error: {e}")),
        };
        match announce(&changes) {
            Ok(()) => {
                *announced = true;
                write_atomic(&ack, "ok")
            }
            Err(e) => {
                let _ = write_atomic(&ack, &format!("error: {e}"));
                Err(format!("the start note was not posted: {e}"))
            }
        }
    }
}

fn stop(child: &mut std::process::Child) {
    crate::run_control::kill_process_group(
        i64::from(child.id()),
        Duration::from_secs(5),
        Duration::from_millis(100),
    );
    let _ = child.kill();
    let _ = child.wait();
}

impl Agent for ClaudeAgent {
    fn run(
        &self,
        dir: &Path,
        prompt: &str,
        announce: &Announce<'_>,
    ) -> Result<AgentHandoff, String> {
        std::fs::create_dir_all(&self.scratch)
            .map_err(|e| format!("cannot create {}: {e}", self.scratch.display()))?;
        let screens = screens_path(&self.scratch);
        std::fs::create_dir_all(&screens)
            .map_err(|e| format!("cannot create {}: {e}", screens.display()))?;
        let plan = plan_path(&self.scratch);
        let handoff = handoff_path(&self.scratch);
        for p in [
            plan.clone(),
            ack_path(&plan),
            plan_draft_path(&self.scratch),
            handoff.clone(),
        ] {
            let _ = std::fs::remove_file(p);
        }
        let prompt = prompt
            .replace(
                "{{PLAN_PATH}}",
                &plan_draft_path(&self.scratch).display().to_string(),
            )
            .replace("{{HANDOFF_PATH}}", &handoff.display().to_string())
            .replace("{{SCREENS_DIR}}", &screens.display().to_string())
            .replace("{{LOOPX_EXE}}", &self.exe)
            .replace("{{ANNOUNCE}}", &self.announce_cmd);
        let argv = (self.argv)(&prompt)?;
        let mut cmd = agent_command(&argv, dir, &plan)?;
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", argv[0]))?;
        let start = Instant::now();
        let mut announced = false;
        let status = loop {
            if let Err(e) = self.take_plan(announce, &mut announced) {
                stop(&mut child);
                return Err(e);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => {
                    stop(&mut child);
                    return Err(format!("lost the agent process: {e}"));
                }
            }
            if start.elapsed() > self.timeout {
                stop(&mut child);
                return Err(format!(
                    "the agent ran over {} minutes and was stopped",
                    self.timeout.as_secs() / 60
                ));
            }
            std::thread::sleep(self.poll);
        };
        // Anything the session left running in the background (its process
        // group) must not keep editing while the runner checks and pushes.
        crate::run_control::kill_process_group(
            i64::from(child.id()),
            Duration::from_secs(5),
            Duration::from_millis(100),
        );
        let text = std::fs::read_to_string(&handoff)
            .map_err(|_| format!("the agent wrote no handoff (exit status {status})"))?;
        let mut parsed = parse_handoff(&text)?;
        // The images the agent saved: only PNGs inside the screens dir.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            let (pairs, dropped) = crate::mr_screenshots::screenshot_pairs(&v, &screens, &screens);
            parsed.screenshots = pairs;
            if !dropped.is_empty() && parsed.screenshots_note.is_empty() {
                parsed.screenshots_note = dropped.join(" ");
            }
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mr_fix::notes::PlannedChange;
    use std::cell::RefCell;

    const HOME: &str = "/Users/u";
    const LOOPX_HOME: &str = "/sandbox/lx";
    const EXE: &str = "/opt/bin/loopx";

    fn fence(cwd: &Path) -> Fence<'_> {
        Fence {
            cwd,
            user_home: Path::new(HOME),
            loopx_home: Path::new(LOOPX_HOME),
            exe: EXE,
        }
    }

    fn flag<'a>(argv: &'a [String], name: &str) -> &'a str {
        let i = argv.iter().position(|a| a == name).unwrap();
        &argv[i + 1]
    }

    /// Space-separated rules; a rule's own spaces sit inside parentheses.
    fn rules(s: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let (mut depth, mut start) = (0, 0);
        for (i, ch) in s.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                ' ' if depth == 0 => {
                    if i > start {
                        out.push(&s[start..i]);
                    }
                    start = i + 1;
                }
                _ => {}
            }
        }
        if start < s.len() {
            out.push(&s[start..]);
        }
        out
    }

    fn argv() -> Vec<String> {
        fix_argv(
            "claude",
            "PROMPT",
            &fence(Path::new("/wt/repo-mr-7")),
            Path::new("/tmp/job1"),
        )
        .unwrap()
    }

    #[test]
    fn fix_argv_lets_the_agent_take_localhost_screenshots_and_nothing_else_of_loopx() {
        let a = argv();
        let allowed = rules(flag(&a, "--allowedTools"));
        assert!(allowed.contains(&"Bash(loopx screenshot *)"), "{allowed:?}");
        assert!(allowed
            .iter()
            .any(|r| r.ends_with("screenshot *)") && r.contains(EXE)));
        let denied = rules(flag(&a, "--disallowedTools"));
        // Everything else of loopx stays denied, screenshots are not a wildcard.
        assert!(denied.contains(&"Bash(loopx gitlab*)"));
        assert!(!allowed.contains(&"Bash(loopx *)"));
    }

    #[test]
    fn fix_argv_denies_git_push_and_gitlab_tools() {
        let a = argv();
        let denied = rules(flag(&a, "--disallowedTools"));
        for want in [
            "Bash(git push*)",
            "Bash(git remote*)",
            "Bash(git config*)",
            "Bash(git reset*)",
            "Bash(git checkout*)",
            "Bash(git commit*--amend*)",
            "Bash(loopx gitlab*)",
            "Bash(/opt/bin/loopx gitlab*)",
            "Bash(loopx mr *)",
            "Bash(loopx notify*)",
            "Bash(loopx slack*)",
            "Bash(loopx serve*)",
            "Bash(glab *)",
            "Bash(gh *)",
            "Bash(curl *)",
            "WebFetch",
            "WebSearch",
            "Read(//Users/u/.gitlab/**)",
            "Read(//Users/u/.slack/**)",
            "Read(//sandbox/lx/**)",
            "Edit(//wt/repo-mr-7/.claude/**)",
            "Edit(//wt/repo-mr-7/.gitlab-ci.yml)",
            "Edit(//wt/repo-mr-7/.git)",
            "Edit(//wt/repo-mr-7/.git/**)",
            "Bash(git add* -f*)",
            "Bash(git add* --force*)",
            "Bash(git * --no-index*)",
            "Bash(git commit* -F*)",
            "Bash(git commit* --file*)",
            "Bash(git commit* --template*)",
        ] {
            assert!(denied.contains(&want), "{want} not denied: {denied:?}");
        }
        let allowed = rules(flag(&a, "--allowedTools"));
        for rule in &allowed {
            assert!(!rule.contains("push"), "{rule}");
            assert!(!rule.starts_with("mcp__"), "{rule}");
            assert_ne!(*rule, "Bash");
            assert_ne!(*rule, "Bash(*)");
            assert_ne!(*rule, "Edit");
            assert_ne!(*rule, "Read");
            assert!(!rule.contains("gitlab") && !rule.contains("curl"), "{rule}");
        }
        assert_eq!(a[0], "claude");
        assert_eq!(flag(&a, "--permission-mode"), "default");
        assert_eq!(flag(&a, "--setting-sources"), "project,local");
        assert_eq!(flag(&a, "--mcp-config"), r#"{"mcpServers": {}}"#);
        assert_eq!(flag(&a, "--tools"), "Bash,Read,Write,Edit,Grep,Glob");
        assert_eq!(flag(&a, "--max-budget-usd"), "10.00");
        assert_eq!(a.last().unwrap(), "PROMPT");
    }

    #[test]
    fn fix_argv_scopes_edits_and_allows_only_the_exact_announce_command() {
        let a = argv();
        let allowed = rules(flag(&a, "--allowedTools"));
        for want in [
            "Read(//wt/repo-mr-7/**)",
            "Edit(//wt/repo-mr-7/**)",
            "Edit(//tmp/job1/**)",
            "Bash(git commit *)",
            "Bash(bundle exec rspec*)",
            "Bash(loopx mr-fix announce /tmp/job1/plan-draft.json)",
            "Bash(/opt/bin/loopx mr-fix announce /tmp/job1/plan-draft.json)",
        ] {
            assert!(allowed.contains(&want), "{want} not allowed: {allowed:?}");
        }
        // No deny rule (a deny wins) swallows the announce command.
        let denied = rules(flag(&a, "--disallowedTools"));
        for cmd in [
            "loopx mr-fix announce /tmp/job1/plan-draft.json".to_string(),
            announce_command(EXE, Path::new("/tmp/job1")),
        ] {
            for rule in &denied {
                let Some(body) = rule.strip_prefix("Bash(").and_then(|r| r.strip_suffix(')'))
                else {
                    continue;
                };
                let hit = match body.strip_suffix('*') {
                    Some(prefix) if !prefix.contains('*') => cmd.starts_with(prefix),
                    _ => body == cmd,
                };
                assert!(!hit, "{rule} denies {cmd}");
            }
        }
        // The exact announce command is the only loopx command without a wildcard,
        // and `screenshot` the only one with one (localhost pages only).
        let loopx: Vec<_> = allowed.iter().filter(|r| r.contains("loopx")).collect();
        let exact: Vec<_> = loopx.iter().filter(|r| !r.contains('*')).collect();
        assert_eq!(exact.len(), 2, "{loopx:?}");
        assert!(exact.iter().all(|r| r.contains("mr-fix announce")));
        let wild: Vec<_> = loopx.iter().filter(|r| r.contains('*')).collect();
        assert!(wild.iter().all(|r| r.contains("screenshot *)")), "{wild:?}");
        // Both the worktree and the scratch dir are working directories.
        let dirs: Vec<_> = a
            .windows(2)
            .filter(|w| w[0] == "--add-dir")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(dirs, ["/wt/repo-mr-7", "/tmp/job1"]);
    }

    /// Claude-style rule matching: `*` matches any run of characters.
    fn glob(pat: &str, s: &str) -> bool {
        let parts: Vec<&str> = pat.split('*').collect();
        let mut rest = s;
        for (i, p) in parts.iter().enumerate() {
            if i == 0 {
                let Some(r) = rest.strip_prefix(p) else {
                    return false;
                };
                rest = r;
            } else if i == parts.len() - 1 {
                return p.is_empty() || rest.ends_with(p);
            } else {
                let Some(at) = rest.find(p) else { return false };
                rest = &rest[at + p.len()..];
            }
        }
        rest.is_empty()
    }

    fn denied_cmd(a: &[String], cmd: &str) -> bool {
        rules(flag(a, "--disallowedTools")).iter().any(|r| {
            r.strip_prefix("Bash(")
                .and_then(|r| r.strip_suffix(')'))
                .is_some_and(|body| glob(body, cmd))
        })
    }

    #[test]
    fn fix_argv_denies_every_push_form_but_not_the_word_in_a_message() {
        let a = argv();
        for cmd in [
            "git push origin HEAD:main",
            "git push",
            "git -C /x push origin y",
            "git --no-pager push",
            "git -c x=y push",
            "git add -f config/master.key",
            "git add . --force",
            "git commit -F /etc/passwd",
            "git commit --file=/x",
            "git diff --no-index /etc/passwd x",
        ] {
            assert!(denied_cmd(&a, cmd), "{cmd} is not denied");
        }
        for cmd in [
            "git commit -m \"Retry the push when it fails\"",
            "git add app/my-feature.rb",
            "git commit -m \"Fix-For review\"",
            "git log --oneline",
            "git diff",
        ] {
            assert!(!denied_cmd(&a, cmd), "{cmd} is denied");
        }
    }

    #[test]
    fn the_agent_process_never_inherits_gitlab_credentials() {
        let cmd = agent_command(
            &["claude".to_string(), "-p".to_string()],
            Path::new("/wt"),
            Path::new("/s/plan.json"),
        )
        .unwrap();
        let envs: Vec<_> = cmd.get_envs().collect();
        for k in ["GITLAB_TOKEN", "GITLAB_URL"] {
            assert!(
                envs.contains(&(std::ffi::OsStr::new(k), None)),
                "{k} not removed: {envs:?}"
            );
        }
        assert!(envs.contains(&(
            std::ffi::OsStr::new(PLAN_ENV),
            Some(std::ffi::OsStr::new("/s/plan.json"))
        )));
    }

    /// Claude-style path globs: `**` crosses `/`, `*` does not.
    fn path_glob(pat: &str, path: &str) -> bool {
        fn go(p: &[u8], s: &[u8]) -> bool {
            if p.is_empty() {
                return s.is_empty();
            }
            if p.starts_with(b"**") {
                return (0..=s.len()).any(|i| go(&p[2..], &s[i..]));
            }
            if p[0] == b'*' {
                return (0..=s.len())
                    .take_while(|&i| i == 0 || s[i - 1] != b'/')
                    .any(|i| go(&p[1..], &s[i..]));
            }
            !s.is_empty() && p[0] == s[0] && go(&p[1..], &s[1..])
        }
        go(pat.as_bytes(), path.as_bytes())
    }

    /// Absolute-path rules of `kind` (`Read`/`Edit`) that cover `path`.
    fn rules_covering<'a>(a: &'a [String], list: &str, kind: &str, path: &str) -> Vec<&'a str> {
        rules(flag(a, list))
            .into_iter()
            .filter(|r| {
                r.strip_prefix(&format!("{kind}(/"))
                    .and_then(|r| r.strip_suffix(')'))
                    .is_some_and(|g| g.starts_with('/') && path_glob(g, path))
            })
            .collect()
    }

    #[test]
    fn a_worktree_under_the_loopx_home_stays_readable_and_editable() {
        let cwd = Path::new("/Users/u/.loopx/worktrees/web-mr-7");
        let a = fix_argv(
            "claude",
            "P",
            &Fence {
                cwd,
                user_home: Path::new("/Users/u"),
                loopx_home: Path::new("/Users/u/.loopx"),
                exe: EXE,
            },
            Path::new("/tmp/job1"),
        )
        .unwrap();
        for file in [
            "/Users/u/.loopx/worktrees/web-mr-7/Gemfile",
            "/Users/u/.loopx/worktrees/web-mr-7/app/models/user.rb",
        ] {
            let read = rules_covering(&a, "--disallowedTools", "Read", file);
            assert!(read.is_empty(), "{read:?} hide {file}");
            let edit = rules_covering(&a, "--disallowedTools", "Edit", file);
            assert!(edit.is_empty(), "{edit:?} block edits to {file}");
            assert!(!rules_covering(&a, "--allowedTools", "Read", file).is_empty());
        }
        // The loopx home's own config and job records stay hidden.
        for secret in [
            "/Users/u/.loopx/projects.json",
            "/Users/u/.loopx/loops.yaml",
            "/Users/u/.loopx/mr-fix/jobs/x.json",
        ] {
            assert!(
                !rules_covering(&a, "--disallowedTools", "Read", secret).is_empty(),
                "{secret} is readable"
            );
        }
        // Creator keeps its blanket deny.
        let creator = crate::creator::spawn::scout_argv(
            "claude",
            "P",
            &Fence {
                cwd: Path::new("/Users/u/.video-scout"),
                user_home: Path::new("/Users/u"),
                loopx_home: Path::new("/Users/u/.loopx"),
                exe: EXE,
            },
            Path::new("/Users/u/.claude/skills/video-scout"),
        )
        .unwrap();
        assert!(rules(flag(&creator, "--disallowedTools")).contains(&"Read(//Users/u/.loopx/**)"));
    }

    #[test]
    fn fix_argv_denies_credential_files_and_pathspec_from_file() {
        let a = argv();
        let denied = rules(flag(&a, "--disallowedTools"));
        for p in [
            ".git-credentials",
            ".config/git/credentials",
            ".config/glab-cli/**",
            ".config/gh/**",
            ".kube/**",
            ".gnupg/**",
            ".pgpass",
            ".aws/**",
            ".ssh/**",
        ] {
            let want = format!("Read(//Users/u/{p})");
            assert!(denied.contains(&want.as_str()), "{want} not denied");
        }
        assert!(denied.contains(&"Bash(git *--pathspec-from-file*)"));
        for cmd in [
            "git add --pathspec-from-file=/etc/passwd",
            "git commit -m x --pathspec-from-file /x",
        ] {
            assert!(denied_cmd(&a, cmd), "{cmd} is not denied");
        }
    }

    #[cfg(unix)]
    #[test]
    fn fix_argv_rules_use_the_resolved_paths() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().canonicalize().unwrap().join("real");
        let scratch_real = d.path().canonicalize().unwrap().join("scratch");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&scratch_real).unwrap();
        let link = d.path().join("link");
        let scratch_link = d.path().join("scratch-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        std::os::unix::fs::symlink(&scratch_real, &scratch_link).unwrap();
        let a = fix_argv("claude", "P", &fence(&link), &scratch_link).unwrap();
        let allowed = rules(flag(&a, "--allowedTools"));
        assert!(
            allowed.contains(&format!("Edit(/{}/**)", real.display()).as_str()),
            "{allowed:?}"
        );
        assert!(allowed.contains(&format!("Edit(/{}/**)", scratch_real.display()).as_str()));
        let dirs: Vec<_> = a
            .windows(2)
            .filter(|w| w[0] == "--add-dir")
            .map(|w| w[1].clone())
            .collect();
        assert_eq!(
            dirs,
            [
                real.display().to_string(),
                scratch_real.display().to_string()
            ]
        );
    }

    #[test]
    fn fix_argv_refuses_a_non_claude_cli() {
        for cli in ["codex", "cursor", "gemini", ""] {
            let e = fix_argv(cli, "p", &fence(Path::new("/wt")), Path::new("/s")).unwrap_err();
            assert!(e.contains("Claude CLI"), "{e}");
        }
    }

    #[test]
    fn announce_request_validates_hands_over_and_waits_for_the_answer() {
        let d = tempfile::tempdir().unwrap();
        let draft = plan_draft_path(d.path());
        let plan = plan_path(d.path());
        std::fs::write(&draft, r#"{"changes": [{"severity": "low", "text": "x"}]}"#).unwrap();
        let p = plan.clone();
        let runner = std::thread::spawn(move || {
            for _ in 0..200 {
                if p.exists() {
                    std::fs::remove_file(&p).unwrap();
                    std::fs::write(ack_path(&p), "ok").unwrap();
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        });
        let got = announce_request(
            &draft,
            &plan,
            Duration::from_secs(5),
            Duration::from_millis(10),
        );
        assert!(runner.join().unwrap());
        assert_eq!(got, Ok(1));
        // An invalid draft never reaches the runner.
        std::fs::write(&draft, r#"{"changes": []}"#).unwrap();
        assert!(announce_request(
            &draft,
            &plan,
            Duration::from_millis(50),
            Duration::from_millis(10)
        )
        .is_err());
        assert!(!plan.exists());
        // No answer: a timeout error telling the agent not to edit.
        std::fs::write(&draft, r#"{"changes": [{"severity": "low", "text": "x"}]}"#).unwrap();
        let e = announce_request(
            &draft,
            &plan,
            Duration::from_millis(50),
            Duration::from_millis(10),
        )
        .unwrap_err();
        assert!(e.contains("do not edit"), "{e}");
    }

    /// A stand-in session: a shell script in place of `claude -p`.
    fn script_agent(
        scratch: &Path,
        script: &str,
        seen: std::rc::Rc<RefCell<String>>,
    ) -> ClaudeAgent {
        let script = script.to_string();
        let mut a = ClaudeAgent::new(
            Box::new(move |prompt: &str| {
                *seen.borrow_mut() = prompt.to_string();
                Ok(vec!["/bin/sh".into(), "-c".into(), script.clone()])
            }),
            scratch.to_path_buf(),
            EXE,
        );
        a.poll = Duration::from_millis(20);
        a
    }

    #[test]
    fn claude_agent_posts_the_plan_then_reads_the_handoff() {
        let d = tempfile::tempdir().unwrap();
        let scratch = d.path().join("job");
        let wt = d.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        let h = handoff_path(&scratch).display().to_string();
        // Hand the plan over, wait for the ack, record it, then hand off.
        let script = format!(
            r#"printf '{{"changes":[{{"severity":"major","text":"Fix it"}}]}}' > "$LOOPX_MR_FIX_PLAN"
               while [ ! -f "$LOOPX_MR_FIX_PLAN.ack" ]; do sleep 0.02; done
               cp "$LOOPX_MR_FIX_PLAN.ack" ack-seen
               printf '{{"items":[{{"discussion_id":"d1","fixed":true,"text":"t"}}],"verification":{{"test":"make test"}}}}' > '{h}'"#
        );
        let seen = std::rc::Rc::new(RefCell::new(String::new()));
        let agent = script_agent(&scratch, &script, seen.clone());
        let posted: RefCell<Vec<String>> = RefCell::new(vec![]);
        let announce = |p: &[PlannedChange]| -> Result<(), String> {
            posted.borrow_mut().push(p[0].text.clone());
            Ok(())
        };
        let got = agent
            .run(
                &wt,
                "plan at {{PLAN_PATH}}, run {{ANNOUNCE}}, handoff {{HANDOFF_PATH}}",
                &announce,
            )
            .unwrap();
        assert_eq!(*posted.borrow(), ["Fix it"]);
        assert_eq!(std::fs::read_to_string(wt.join("ack-seen")).unwrap(), "ok");
        assert_eq!(got.items[0].discussion_id, "d1");
        assert_eq!(got.verification.test, "make test");
        let p = seen.borrow();
        let scratch = std::fs::canonicalize(&scratch).unwrap();
        assert!(p.contains(&format!("plan at {}", plan_draft_path(&scratch).display())));
        assert!(p.contains(&format!("run {}", announce_command(EXE, &scratch))));
        assert!(p.contains(&format!("handoff {}", handoff_path(&scratch).display())));
    }

    #[cfg(unix)]
    #[test]
    fn claude_agent_hands_the_session_resolved_scratch_paths() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let a = ClaudeAgent::new(Box::new(|_| Ok(vec![])), link.join("job"), EXE);
        let resolved = std::fs::canonicalize(&real).unwrap().join("job");
        assert_eq!(a.scratch, resolved);
        assert!(
            a.announce_cmd.contains(&resolved.display().to_string()),
            "{}",
            a.announce_cmd
        );
    }

    #[test]
    fn claude_agent_resolves_the_handoffs_screenshots_inside_the_screens_dir() {
        let d = tempfile::tempdir().unwrap();
        let scratch = d.path().join("job");
        let wt = d.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        let h = handoff_path(&scratch).display().to_string();
        // A real PNG header; the third entry escapes the screens directory.
        let script = format!(
            r#"mkdir -p "$LOOPX_SCREENS"
               printf '\211PNG\r\n\032\n' > "$LOOPX_SCREENS/1-after.png"
               printf '\211PNG\r\n\032\n' > ../secret.png
               printf '{{"items":[],"verification":{{"test":"t"}},"screenshots_note":"dark mode skipped","screenshots":[{{"label":"popup","after":"1-after.png"}},{{"label":"bad","after":"../secret.png"}}]}}' > '{h}'"#
        );
        let mut agent = ClaudeAgent::new(
            Box::new(move |p| {
                Ok(vec![
                    "sh".into(),
                    "-c".into(),
                    format!("export LOOPX_SCREENS=\"{}\"; {script}", screens_marker(p)),
                ])
            }),
            scratch.clone(),
            EXE,
        );
        agent.poll = Duration::from_millis(20);
        let got = agent.run(&wt, "{{SCREENS_DIR}}", &|_| Ok(())).unwrap();
        assert_eq!(got.screenshots.len(), 1, "{:?}", got.screenshots);
        assert_eq!(got.screenshots[0].label, "popup");
        assert!(got.screenshots[0].after.ends_with("screens/1-after.png"));
        assert_eq!(got.screenshots_note, "dark mode skipped");
    }

    /// The prompt is the screens dir (the test template is just the placeholder).
    fn screens_marker(prompt: &str) -> String {
        prompt.to_string()
    }

    #[test]
    fn claude_agent_stops_the_session_when_the_start_note_fails() {
        let d = tempfile::tempdir().unwrap();
        let scratch = d.path().join("job");
        let script = r#"printf '{"changes":[{"severity":"low","text":"x"}]}' > "$LOOPX_MR_FIX_PLAN"; sleep 30"#;
        let agent = script_agent(&scratch, script, Default::default());
        let announce = |_: &[PlannedChange]| -> Result<(), String> { Err("403 Forbidden".into()) };
        let t = Instant::now();
        let e = agent.run(d.path(), "p", &announce).unwrap_err();
        assert!(e.contains("403 Forbidden"), "{e}");
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "the session was not stopped"
        );
    }

    #[test]
    fn claude_agent_rejects_a_bad_plan_and_reports_a_missing_handoff() {
        let d = tempfile::tempdir().unwrap();
        let scratch = d.path().join("job");
        let script = r#"printf '{"changes":[]}' > "$LOOPX_MR_FIX_PLAN"
            while [ ! -f "$LOOPX_MR_FIX_PLAN.ack" ]; do sleep 0.02; done
            cp "$LOOPX_MR_FIX_PLAN.ack" ack-seen"#;
        let agent = script_agent(&scratch, script, Default::default());
        let announce = |_: &[PlannedChange]| -> Result<(), String> { panic!("must not post") };
        let e = agent.run(d.path(), "p", &announce).unwrap_err();
        assert!(e.contains("no handoff"), "{e}");
        let ack = std::fs::read_to_string(d.path().join("ack-seen")).unwrap();
        assert!(ack.starts_with("error: "), "{ack}");
    }

    #[test]
    fn processes_the_session_left_behind_are_stopped_before_the_runner_checks() {
        let d = tempfile::tempdir().unwrap();
        let scratch = d.path().join("job");
        let h = handoff_path(&scratch).display().to_string();
        let late = d.path().join("late");
        let script = format!(
            r#"(sleep 1; touch '{}') &
               printf '{{"items":[],"verification":{{}}}}' > '{h}'"#,
            late.display()
        );
        let agent = script_agent(&scratch, &script, Default::default());
        let announce = |_: &[PlannedChange]| -> Result<(), String> { Ok(()) };
        agent.run(d.path(), "p", &announce).unwrap();
        std::thread::sleep(Duration::from_millis(1600));
        assert!(
            !late.exists(),
            "a background process kept editing after the session"
        );
    }

    #[test]
    fn claude_agent_times_out() {
        let d = tempfile::tempdir().unwrap();
        let mut agent = script_agent(&d.path().join("job"), "sleep 30", Default::default());
        agent.timeout = Duration::from_millis(200);
        let announce = |_: &[PlannedChange]| -> Result<(), String> { Ok(()) };
        let t = Instant::now();
        assert!(agent.run(d.path(), "p", &announce).is_err());
        assert!(t.elapsed() < Duration::from_secs(20));
    }
}
