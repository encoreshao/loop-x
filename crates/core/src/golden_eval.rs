//! Golden eval suite (`loop eval --golden`): runs the real GitLab issue agent
//! (gate mode, offline) against synthetic fixture repos under
//! `evals/golden/<case>/case.yaml` and grades what it did. Mirrors
//! `bin/golden_eval.py`.
//!
//! Grading and suite control are pure; [`run_case_real`] is the paid path and
//! is only reached by an explicit `loop eval --golden`. Every real case costs
//! money, so the suite has a hard budget and stops launching cases once it is
//! spent.

use crate::gitlab_runner;
use crate::loop_definition::LoopDefinition;
use crate::loop_result::IterationResult;
use crate::loop_runtime::LoopContext;
use crate::loop_state::LoopState;
use crate::loop_verifiers::{ProjectCommandsVerifier, Verifier};
use crate::loops_config::py_repr;
use crate::proc::ProcOutput;
use crate::pyjson;
use crate::runner_util::repo_root;
use crate::worktree;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const ACTIONS: [&str; 3] = ["fix", "answer", "escalate"];
pub const ISSUE_IID: i64 = 1;
pub const ALIAS: &str = "golden";
/// `loops/gitlab-issue/loop.yaml`'s `max_iterations`.
pub const DEFAULT_MAX_ATTEMPTS: usize = 2;
pub const DEFAULT_BRANCH: &str = "main";
pub const EVALS_DIR_ENV: &str = "LOOP_EVALS_DIR";
const GIT_IDENTITY: [&str; 4] = [
    "-c",
    "user.name=golden-eval",
    "-c",
    "user.email=golden-eval@example.invalid",
];
/// Build/test droppings that are never "a change the agent made".
const IGNORED: [&str; 5] = [
    "__pycache__/",
    "*.pyc",
    ".pytest_cache/",
    "node_modules/",
    ".DS_Store",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Expect {
    pub action: String,
    pub allowed_paths: Vec<String>,
    pub protected_paths: Vec<String>,
    pub checks_pass: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GoldenCase {
    pub name: String,
    pub issue_title: String,
    pub issue_body: String,
    /// Fixture files in case-file order.
    pub files: Vec<(String, String)>,
    pub test_cmd: String,
    pub lint_cmd: String,
    pub expect: Expect,
}

/// Where the suites record `golden-last.json` / `chat-last.json`:
/// `$LOOP_EVALS_DIR`, else `<repo_root>/outputs/evals`.
pub fn evals_dir() -> PathBuf {
    match std::env::var_os(EVALS_DIR_ENV).filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => repo_root().join("outputs").join("evals"),
    }
}

/// The checkout holding `evals/` and `instructions/`: `$LOOPX_ROOT`, else the
/// nearest ancestor of the binary or the working directory that has
/// `evals/<sub>`.
pub fn checkout_root(sub: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(r) = std::env::var_os("LOOPX_ROOT").filter(|v| !v.is_empty()) {
        candidates.push(PathBuf::from(r));
    }
    if let Ok(exe) = std::env::current_exe() {
        candidates.extend(exe.ancestors().skip(1).map(Path::to_path_buf));
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.extend(cwd.ancestors().map(Path::to_path_buf));
    }
    candidates
        .into_iter()
        .find(|c| c.join("evals").join(sub).is_dir())
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_case(text: &str, path: &str) -> Result<GoldenCase, String> {
    let case: Value =
        serde_norway::from_str(text).map_err(|e| format!("{path}: invalid YAML: {e}"))?;
    let Some(obj) = case.as_object() else {
        return Err(format!("{path}: case must be a mapping"));
    };
    for key in ["name", "issue", "repo", "expect"] {
        if !obj.contains_key(key) {
            return Err(format!(
                "{path}: missing {}",
                crate::pydumps::py_repr_str(key)
            ));
        }
    }
    let expect = &obj["expect"];
    let action = expect.get("action");
    let action_ok = action
        .and_then(Value::as_str)
        .filter(|a| ACTIONS.contains(a));
    let Some(action) = action_ok else {
        return Err(format!(
            "{path}: expect.action must be one of {}, got {}",
            ACTIONS.join(", "),
            action.map_or("None".to_string(), py_repr)
        ));
    };
    let files: Vec<(String, String)> = obj["repo"]
        .get("files")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();
    if files.is_empty() {
        return Err(format!("{path}: repo.files must list at least one file"));
    }
    let text_of = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or("").to_string();
    Ok(GoldenCase {
        name: text_of(obj.get("name")),
        issue_title: text_of(obj["issue"].get("title")),
        issue_body: text_of(obj["issue"].get("body")),
        files,
        test_cmd: text_of(obj["repo"].get("test_cmd")),
        lint_cmd: text_of(obj["repo"].get("lint_cmd")),
        expect: Expect {
            action: action.to_string(),
            allowed_paths: str_list(expect.get("allowed_paths")),
            protected_paths: str_list(expect.get("protected_paths")),
            checks_pass: expect.get("checks_pass").and_then(Value::as_bool),
        },
    })
}

/// Every `*/case.yaml` under `dir`, sorted; `names` narrows to those cases.
pub fn load_cases(dir: &Path, names: Option<&[String]>) -> Result<Vec<GoldenCase>, String> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("case.yaml"))
        .filter(|p| p.is_file())
        .collect();
    paths.sort();
    let mut cases = Vec::new();
    for p in paths {
        let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        cases.push(parse_case(&text, &p.display().to_string())?);
    }
    if let Some(names) = names.filter(|n| !n.is_empty()) {
        let unknown: Vec<&str> = names
            .iter()
            .filter(|n| !cases.iter().any(|c| &c.name == *n))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return Err(format!("unknown golden case(s): {}", unknown.join(", ")));
        }
        cases.retain(|c| names.contains(&c.name));
    }
    Ok(cases)
}

/// The cases under the checkout's `evals/golden`.
pub fn default_cases(names: Option<&[String]>) -> Result<Vec<GoldenCase>, String> {
    let dir = checkout_root("golden")
        .map(|r| r.join("evals").join("golden"))
        .unwrap_or_else(|| PathBuf::from("evals/golden"));
    load_cases(&dir, names)
}

// ---- fixture ------------------------------------------------------------------

fn git(args: &[&str], cwd: &Path) -> Result<String, String> {
    let out = Command::new("git")
        .args(GIT_IDENTITY)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Where the issue-1 worktree lands for this fixture.
pub fn worktree_path(repo: &Path, root: &Path) -> PathBuf {
    let name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    root.join("worktrees")
        .join(format!("{name}-issue-{ISSUE_IID}"))
}

/// A fresh git repo with the case's files committed as `initial` (also tagged
/// `initial`), a local bare `origin`, and issue 1's worktree on `loop/issue-1`.
pub fn build_fixture(case: &GoldenCase, root: &Path) -> Result<PathBuf, String> {
    let io = |e: std::io::Error| e.to_string();
    let repo = root.join("fixture-repo");
    std::fs::create_dir_all(&repo).map_err(io)?;
    git(&["init", "-q", "-b", DEFAULT_BRANCH], &repo)?;
    // Repo-local identity: the agent commits inside the worktree, which shares
    // this config, and must not depend on the machine's global one.
    git(&["config", "user.name", "golden-eval"], &repo)?;
    git(
        &["config", "user.email", "golden-eval@example.invalid"],
        &repo,
    )?;
    std::fs::create_dir_all(repo.join(".git").join("info")).map_err(io)?;
    std::fs::write(
        repo.join(".git").join("info").join("exclude"),
        format!("{}\n", IGNORED.join("\n")),
    )
    .map_err(io)?;
    for (rel, content) in &case.files {
        let path = repo.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        std::fs::write(path, content).map_err(io)?;
    }
    git(&["add", "-A"], &repo)?;
    git(&["commit", "-q", "-m", "initial"], &repo)?;
    git(&["tag", "initial"], &repo)?;
    let origin = root.join("fixture-origin.git");
    git(
        &[
            "clone",
            "-q",
            "--bare",
            &repo.display().to_string(),
            &origin.display().to_string(),
        ],
        &repo,
    )?;
    git(
        &["remote", "add", "origin", &origin.display().to_string()],
        &repo,
    )?;
    git(&["fetch", "-q", "origin"], &repo)?;
    worktree::new_worktree(
        &repo.display().to_string(),
        DEFAULT_BRANCH,
        &ISSUE_IID.to_string(),
        &root.join("worktrees").display().to_string(),
    )
    .map_err(|f| f.message)?;
    Ok(repo)
}

fn lines(out: &str) -> impl Iterator<Item = String> + '_ {
    out.lines().filter(|l| !l.is_empty()).map(str::to_string)
}

/// Every path that differs from `initial`: in the issue worktree (committed,
/// modified or untracked) and in the source checkout itself, which the agent
/// must never edit directly.
pub fn changed_paths(repo: &Path, worktree: &Path) -> Result<Vec<String>, String> {
    let mut changed = std::collections::BTreeSet::new();
    for cwd in [worktree, repo] {
        if !cwd.is_dir() {
            continue;
        }
        changed.extend(lines(&git(
            &["diff", "--name-only", "--no-renames", "initial"],
            cwd,
        )?));
        changed.extend(lines(&git(
            &["ls-files", "--others", "--exclude-standard"],
            cwd,
        )?));
    }
    Ok(changed.into_iter().collect())
}

/// Python's `fnmatch.fnmatchcase`: `*` (any run, `/` included), `?`, `[seq]`,
/// `[!seq]`.
pub fn fnmatchcase(name: &str, pattern: &str) -> bool {
    fn go(n: &[char], p: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| go(&n[i..], &p[1..])),
            Some('?') => !n.is_empty() && go(&n[1..], &p[1..]),
            Some('[') => {
                // A class needs a closing `]` (not as its first character).
                let mut j = 1;
                if p.get(j) == Some(&'!') {
                    j += 1;
                }
                if p.get(j) == Some(&']') {
                    j += 1;
                }
                while j < p.len() && p[j] != ']' {
                    j += 1;
                }
                if j >= p.len() {
                    return !n.is_empty() && n[0] == '[' && go(&n[1..], &p[1..]);
                }
                let Some(&c) = n.first() else { return false };
                let mut body = &p[1..j];
                let negate = body.first() == Some(&'!');
                if negate {
                    body = &body[1..];
                }
                let mut hit = false;
                let mut i = 0;
                while i < body.len() {
                    if i + 2 < body.len() && body[i + 1] == '-' {
                        if body[i] <= c && c <= body[i + 2] {
                            hit = true;
                        }
                        i += 3;
                    } else {
                        if body[i] == c {
                            hit = true;
                        }
                        i += 1;
                    }
                }
                hit != negate && go(&n[1..], &p[j + 1..])
            }
            Some(&c) => n.first() == Some(&c) && go(&n[1..], &p[1..]),
        }
    }
    let n: Vec<char> = name.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    go(&n, &p)
}

fn allowed(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| fnmatchcase(path, p))
}

/// `(passed, reasons)`. `handoff` is the validated handoff or `None`;
/// `checks_passed` is the project's checks after the last attempt (`None` when
/// they were not run, i.e. no fix was handed off).
pub fn grade(
    case: &GoldenCase,
    handoff: Option<&Value>,
    changed: &[String],
    checks_passed: Option<bool>,
) -> (bool, Vec<String>) {
    let expect = &case.expect;
    let Some(handoff) = handoff else {
        return (false, vec!["no handoff (missing or malformed)".to_string()]);
    };
    let mut reasons = Vec::new();
    let action = handoff.get("action");
    if action.and_then(Value::as_str) != Some(expect.action.as_str()) {
        reasons.push(format!(
            "expected action {}, got {}",
            expect.action,
            action.map_or("None".to_string(), crate::pydumps::py_str)
        ));
    }
    for path in changed {
        if !allowed(path, &expect.allowed_paths) {
            reasons.push(format!(
                "changed {path} outside allowed paths {}",
                py_repr(&json!(expect.allowed_paths))
            ));
        } else if case.files.iter().any(|(f, _)| f == path)
            && allowed(path, &expect.protected_paths)
        {
            reasons.push(format!("changed protected fixture file {path}"));
        }
    }
    if expect.checks_pass == Some(true) && checks_passed != Some(true) {
        reasons.push(
            if checks_passed == Some(false) {
                "project checks did not pass"
            } else {
                "project checks were not run"
            }
            .to_string(),
        );
    }
    (reasons.is_empty(), reasons)
}

// ---- suite control ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct CaseOutcome {
    pub passed: bool,
    pub reasons: Vec<String>,
    /// `None` = unknown: booked at what was left of the budget.
    pub cost_usd: Option<f64>,
}

/// A case that blew up instead of returning an outcome. Counts as failed.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseCrash {
    /// E.g. `RuntimeError: boom`.
    pub message: String,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CaseResult {
    pub name: String,
    pub passed: bool,
    pub reasons: Vec<String>,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Summary {
    pub results: Vec<CaseResult>,
    pub not_run: Vec<String>,
    pub spent_usd: f64,
}

fn round6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

/// Run cases in order until the budget is spent; the rest are `not_run`.
/// `run_case(case, remaining_usd)` returns the outcome. A crash counts as
/// failed, never as a suite crash; a case that reports no cost is booked at
/// what was left of the budget so the suite total stays bounded.
pub fn run_suite<C, F>(
    cases: &[C],
    budget_usd: f64,
    name_of: fn(&C) -> &str,
    mut run_case: F,
) -> Summary
where
    F: FnMut(&C, f64) -> Result<CaseOutcome, CaseCrash>,
{
    let mut s = Summary::default();
    let mut spent = 0.0_f64;
    for case in cases {
        if spent >= budget_usd {
            s.not_run.push(name_of(case).to_string());
            continue;
        }
        let remaining = budget_usd - spent;
        let outcome = match run_case(case, remaining) {
            Ok(o) => o,
            Err(crash) => CaseOutcome {
                passed: false,
                reasons: vec![format!("case crashed: {}", crash.message)],
                cost_usd: crash.cost_usd,
            },
        };
        let cost = outcome.cost_usd.unwrap_or_else(|| round6(remaining));
        spent += cost;
        s.results.push(CaseResult {
            name: name_of(case).to_string(),
            passed: outcome.passed,
            reasons: outcome.reasons,
            cost_usd: cost,
        });
    }
    s.spent_usd = round6(spent);
    s
}

/// 0 only when at least one case ran, every case ran, and all passed.
pub fn exit_code(summary: &Summary) -> i32 {
    if summary.results.is_empty() || !summary.not_run.is_empty() {
        return 1;
    }
    i32::from(!summary.results.iter().all(|r| r.passed))
}

/// `finished_at`, the `+00:00` form of Python's `isoformat(timespec="seconds")`.
pub fn finished_at_now() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S+00:00")
        .to_string()
}

/// Write a suite summary (`golden-last.json` / `chat-last.json`).
pub fn write_summary(summary: &Summary, budget_usd: f64, path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let results: Vec<Value> = summary
        .results
        .iter()
        .map(|r| {
            json!({"name": r.name, "passed": r.passed, "reasons": r.reasons, "cost_usd": r.cost_usd})
        })
        .collect();
    let data = json!({
        "finished_at": finished_at_now(),
        "budget_usd": budget_usd,
        "spent_usd": summary.spent_usd,
        "results": results,
        "not_run": summary.not_run,
    });
    std::fs::write(path, format!("{}\n", pyjson::to_string_pretty(&data)))
}

/// `<evals dir>/golden-last.json` unless `path` is given.
pub fn write_last_run(
    summary: &Summary,
    budget_usd: f64,
    path: Option<&Path>,
) -> std::io::Result<PathBuf> {
    let p = path.map_or_else(|| evals_dir().join("golden-last.json"), Path::to_path_buf);
    write_summary(summary, budget_usd, &p)?;
    Ok(p)
}

// ---- the real (paid) path -----------------------------------------------------------

pub struct Sandbox {
    pub root: PathBuf,
    pub repo: PathBuf,
    pub worktree: PathBuf,
    pub home: PathBuf,
    pub project: Value,
    pub events_dir: PathBuf,
    pub issue_file: PathBuf,
    pub log: PathBuf,
}

impl Sandbox {
    /// The environment the agent child gets on top of the inherited one.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            (
                "LOOP_ENGINEERING_HOME".to_string(),
                self.home.display().to_string(),
            ),
            (
                "LOOP_EVENTS_DIR".to_string(),
                self.events_dir.display().to_string(),
            ),
        ]
    }
}

/// Everything one real case runs against, all under `root`: the fixture repo
/// and its issue worktree, a `LOOP_ENGINEERING_HOME` whose `projects.json`
/// points only at the fixture, a scratch events dir, and the issue JSON file.
pub fn prepare_sandbox(case: &GoldenCase, root: &Path) -> Result<Sandbox, String> {
    let io = |e: std::io::Error| e.to_string();
    let repo = build_fixture(case, &root.join("fixture"))?;
    let worktree = worktree_path(&repo, &root.join("fixture"));
    let home = root.join("home");
    std::fs::create_dir_all(&home).map_err(io)?;
    let project = json!({
        "project_id": 0,
        "local_path": repo.display().to_string(),
        "target_branch": DEFAULT_BRANCH,
        "install_cmd": "",
        "test_cmd": case.test_cmd,
        "lint_cmd": case.lint_cmd,
    });
    let config = json!({
        "gitlab_instance": "golden-eval.invalid",
        "assignee_username": "golden-eval",
        "worktree_root": worktree.parent().map(|p| p.display().to_string()).unwrap_or_default(),
        "projects": {ALIAS: project},
    });
    std::fs::write(
        home.join("projects.json"),
        pyjson::to_string_pretty(&config),
    )
    .map_err(io)?;
    // Claude is the CLI that reports cost and enforces the tool denials.
    std::fs::write(home.join("ai_cli.json"), r#"{"cli": "claude"}"#).map_err(io)?;
    let events_dir = root.join("events");
    std::fs::create_dir_all(&events_dir).map_err(io)?;
    let issue_file = root.join("issue.json");
    std::fs::write(
        &issue_file,
        json!({"title": case.issue_title, "body": case.issue_body}).to_string(),
    )
    .map_err(io)?;
    Ok(Sandbox {
        root: root.to_path_buf(),
        repo,
        worktree,
        home,
        project,
        events_dir,
        issue_file,
        log: root.join("loop.log"),
    })
}

/// Run `cmd` in its own process group; on timeout the whole group is killed
/// and `None` is returned (killing only the direct child would leave the
/// agent's `claude` running, and spending).
pub fn run_killing_group_on_timeout(
    argv: &[String],
    env: &[(String, String)],
    timeout: Duration,
) -> Option<ProcOutput> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    let (program, args) = argv.split_first()?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn().ok()?;
    let drain = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = drain(Box::new(child.stdout.take()?));
    let err = drain(Box::new(child.stderr.take()?));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() >= deadline => break None,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break None,
        }
    };
    let Some(status) = status else {
        // SAFETY: plain syscall on the child's own process group.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.wait();
        let _ = out.join();
        let _ = err.join();
        return None;
    };
    use std::os::unix::process::ExitStatusExt;
    Some(ProcOutput {
        code: status.code().or_else(|| status.signal().map(|s| -s)),
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
        timed_out: false,
    })
}

/// What one agent attempt reports back.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentAttempt {
    pub cost_usd: Option<f64>,
    pub error: Option<String>,
}

pub struct AttemptRequest<'a> {
    pub sandbox: &'a Sandbox,
    pub run_id: &'a str,
    pub feedback: Option<&'a str>,
    pub max_budget_usd: Option<f64>,
    pub repo_root: &'a Path,
    pub timeout_seconds: u64,
}

/// One agent attempt in a child process whose environment points at the
/// sandbox (the hidden `loopx loop eval-agent`). A child so that nothing in
/// it can ever read the real `~/.loopx`.
pub fn run_agent_subprocess(req: &AttemptRequest) -> AgentAttempt {
    let result_file = req.sandbox.root.join(format!(
        "agent-result-{}.json",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "loopx".into());
    let mut argv: Vec<String> = vec![
        exe,
        "loop".into(),
        "eval-agent".into(),
        "--issue-file".into(),
        req.sandbox.issue_file.display().to_string(),
        "--run-id".into(),
        req.run_id.into(),
        "--result-file".into(),
        result_file.display().to_string(),
        "--repo-root".into(),
        req.repo_root.display().to_string(),
        "--log".into(),
        req.sandbox.log.display().to_string(),
        "--timeout".into(),
        req.timeout_seconds.to_string(),
    ];
    if let Some(b) = req.max_budget_usd {
        argv.extend(["--max-budget-usd".into(), format!("{b:.4}")]);
    }
    if let Some(f) = req.feedback.filter(|f| !f.is_empty()) {
        let feedback_file = req.sandbox.root.join("feedback.txt");
        let _ = std::fs::write(&feedback_file, f);
        argv.extend([
            "--feedback-file".into(),
            feedback_file.display().to_string(),
        ]);
    }
    let mut env = req.sandbox.env();
    env.push(("LOOP_RUN_ID".into(), req.run_id.into()));
    let limit = req.timeout_seconds + 120;
    let Some(out) = run_killing_group_on_timeout(&argv, &env, Duration::from_secs(limit)) else {
        return AgentAttempt {
            cost_usd: None,
            error: Some(format!("agent subprocess timed out after {limit}s")),
        };
    };
    match std::fs::read_to_string(&result_file)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        Some(v) => AgentAttempt {
            cost_usd: v.get("cost_usd").and_then(Value::as_f64),
            error: v.get("error").and_then(Value::as_str).map(str::to_string),
        },
        None => {
            let detail = if out.stderr.is_empty() {
                &out.stdout
            } else {
                &out.stderr
            };
            let tail: String = detail
                .chars()
                .rev()
                .take(800)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            AgentAttempt {
                cost_usd: None,
                error: Some(format!(
                    "agent subprocess exited {}: {tail}",
                    out.code.unwrap_or(-1)
                )),
            }
        }
    }
}

fn run_checks(
    sandbox: &Sandbox,
    handoff: &Path,
    timeout_seconds: f64,
) -> crate::loop_result::VerificationResult {
    let project = sandbox.project.clone();
    let wt_root = sandbox
        .worktree
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let verifier = ProjectCommandsVerifier::new("golden_checks", ALIAS, ISSUE_IID, timeout_seconds)
        .with_handoff_path(Some(handoff.to_path_buf()))
        .with_project_fn(move |_| Ok(project.clone()))
        .with_worktree_root_fn(move || Ok(wt_root.clone()));
    let definition = LoopDefinition::from_value(&json!({
        "name": "golden-checks", "version": 1,
        "trigger": {"type": "manual"}, "goal": {"type": "fix"},
    }))
    .expect("static definition is valid");
    verifier.verify(&LoopContext {
        iteration: 1,
        definition: &definition,
        previous: None,
    })
}

/// Run the real agent on one case: gate mode, offline, in a throwaway sandbox.
/// After a fix handoff the project's checks are re-run and a failure is fed
/// back for another attempt (up to `max_attempts`). Each attempt is capped at
/// what is left of `max_budget_usd`; the case's cost is the CLI's reported
/// spend summed over attempts; an attempt whose cost is unknown is booked at
/// the cap it was given. Never panics: a crash fails the case with the spend
/// booked so far.
pub fn run_case_real(
    case: &GoldenCase,
    max_budget_usd: Option<f64>,
    repo_root: &Path,
    agent_runner: &dyn Fn(&AttemptRequest) -> AgentAttempt,
    max_attempts: usize,
    timeout_seconds: u64,
) -> CaseOutcome {
    let run_id = format!(
        "golden-{}-{}",
        case.name,
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let handoff_file =
        gitlab_runner::handoff_path(&run_id, ALIAS, &ISSUE_IID.to_string(), repo_root);
    let tmp = std::env::temp_dir().join(format!(
        "golden-eval-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    ));
    let mut spent = 0.0_f64;
    let mut reasons: Vec<String> = Vec::new();
    let in_flight_cap: std::cell::Cell<Option<f64>> = std::cell::Cell::new(None);

    let graded = (|| -> Result<(bool, Vec<String>), String> {
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        let sandbox = prepare_sandbox(case, &tmp)?;
        let mut feedback: Option<String> = None;
        let mut handoff: Option<Value> = None;
        let mut checks: Option<bool> = None;
        for _attempt in 0..max_attempts {
            if max_budget_usd.is_some_and(|m| spent >= m) {
                reasons.push("budget exhausted before the case finished".into());
                break;
            }
            let cap = crate::cost::remaining_budget(max_budget_usd, Some(spent), 0.05);
            let _ = std::fs::remove_file(&handoff_file);
            in_flight_cap.set(cap);
            let outcome = agent_runner(&AttemptRequest {
                sandbox: &sandbox,
                run_id: &run_id,
                feedback: feedback.as_deref(),
                max_budget_usd: cap,
                repo_root,
                timeout_seconds,
            });
            spent += outcome.cost_usd.unwrap_or_else(|| cap.unwrap_or(0.0));
            in_flight_cap.set(None);
            if let Some(err) = &outcome.error {
                reasons.push(format!("agent call failed: {err}"));
            }
            handoff = gitlab_runner::read_handoff(&handoff_file, &ISSUE_IID.to_string());
            let is_fix = handoff
                .as_ref()
                .and_then(|h| h.get("action"))
                .and_then(Value::as_str)
                == Some("fix");
            if outcome.error.is_some() || !is_fix {
                checks = None;
                break;
            }
            let verification = run_checks(&sandbox, &handoff_file, timeout_seconds as f64);
            checks = Some(verification.passed);
            if verification.passed {
                break;
            }
            feedback = Some(gitlab_runner::format_feedback(&IterationResult {
                iteration: 1,
                state: LoopState::Evaluating,
                verification_results: vec![verification],
                budget: Value::Null,
                progressed: false,
            }));
        }
        let changed = changed_paths(&sandbox.repo, &sandbox.worktree)?;
        Ok(grade(case, handoff.as_ref(), &changed, checks))
    })();
    let (passed, grade_reasons) = match graded {
        Ok(g) => g,
        Err(e) => {
            spent += in_flight_cap.get().unwrap_or(0.0);
            (false, vec![format!("case crashed: {e}")])
        }
    };
    if let Some(dir) = handoff_file.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
    let _ = std::fs::remove_dir_all(&tmp);
    reasons.extend(grade_reasons);
    CaseOutcome {
        passed: passed && reasons.is_empty(),
        reasons,
        cost_usd: Some(round6(spent)),
    }
}

/// The suite's real path: [`run_case_real`] per case, each capped at what is
/// left of the budget.
pub fn run_suite_real(cases: &[GoldenCase], budget_usd: f64) -> Summary {
    let root = checkout_root("golden").unwrap_or_else(repo_root);
    run_suite(
        cases,
        budget_usd,
        |c| c.name.as_str(),
        |case, remaining| {
            Ok(run_case_real(
                case,
                Some(remaining),
                &root,
                &run_agent_subprocess,
                DEFAULT_MAX_ATTEMPTS,
                900,
            ))
        },
    )
}

/// `loopx loop eval-agent ...`: the child side of [`run_agent_subprocess`].
pub fn agent_child_main(argv: &[String]) -> i32 {
    let flag = |name: &str| -> Option<&str> {
        let i = argv.iter().position(|a| a == name)?;
        argv.get(i + 1).map(String::as_str)
    };
    let (Some(issue_file), Some(run_id), Some(result_file), Some(repo_root)) = (
        flag("--issue-file"),
        flag("--run-id"),
        flag("--result-file"),
        flag("--repo-root"),
    ) else {
        eprintln!(
            "loop eval-agent: --issue-file, --run-id, --result-file and --repo-root are required"
        );
        return 2;
    };
    if std::env::var_os("LOOP_ENGINEERING_HOME").is_none_or(|v| v.is_empty()) {
        eprintln!("loop eval-agent: refusing to run without a sandbox LOOP_ENGINEERING_HOME");
        return 2;
    }
    let timeout: f64 = flag("--timeout")
        .and_then(|t| t.parse().ok())
        .unwrap_or(900.0);
    let max_budget = flag("--max-budget-usd").and_then(|t| t.parse::<f64>().ok());
    let feedback = flag("--feedback-file").and_then(|p| std::fs::read_to_string(p).ok());
    let mut ctx = gitlab_runner::RunCtx::new(PathBuf::from(repo_root));
    if let Some(log) = flag("--log") {
        ctx.unified_log = PathBuf::from(log);
    }
    let invoker = gitlab_runner::cli_invoker(ctx);
    let result = invoker(&gitlab_runner::InvokeRequest {
        alias: ALIAS.to_string(),
        issue_iid: ISSUE_IID.to_string(),
        mode: gitlab_runner::InvokeMode::IssueFile(PathBuf::from(issue_file)),
        feedback,
        gate: true,
        run_id: Some(run_id.to_string()),
        max_budget_usd: max_budget,
        timeout_seconds: timeout,
    });
    let out = match result {
        Ok(o) => json!({"cost_usd": o.cost_usd, "error": Value::Null}),
        Err(e) => json!({"cost_usd": e.cost_usd, "error": e.message}),
    };
    match std::fs::write(result_file, out.to_string()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("loop eval-agent: cannot write {result_file}: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(expect_action: &str) -> GoldenCase {
        GoldenCase {
            name: "c".into(),
            issue_title: "t".into(),
            issue_body: "b".into(),
            files: vec![
                ("lib/a.py".into(), "x".into()),
                ("tests/test_a.py".into(), "y".into()),
            ],
            test_cmd: String::new(),
            lint_cmd: String::new(),
            expect: Expect {
                action: expect_action.into(),
                allowed_paths: vec!["lib/**".into(), "tests/**".into()],
                protected_paths: vec!["tests/**".into()],
                checks_pass: Some(true),
            },
        }
    }

    #[test]
    fn fnmatch_follows_python() {
        assert!(fnmatchcase("lib/a/b.py", "lib/**"));
        assert!(fnmatchcase("a.py", "*.py"));
        assert!(fnmatchcase("a.py", "?.py"));
        assert!(!fnmatchcase("ab.py", "?.py"));
        assert!(fnmatchcase("b1", "[a-c]1"));
        assert!(!fnmatchcase("d1", "[a-c]1"));
        assert!(fnmatchcase("d1", "[!a-c]1"));
        assert!(fnmatchcase("[x", "[x"));
        assert!(!fnmatchcase("lib", "lib/**"));
    }

    #[test]
    fn grade_requires_a_handoff() {
        let (ok, why) = grade(&case("fix"), None, &[], None);
        assert!(!ok);
        assert_eq!(why, ["no handoff (missing or malformed)"]);
    }

    #[test]
    fn grade_flags_wrong_action_paths_and_checks() {
        let c = case("fix");
        let h = json!({"action": "answer"});
        let changed = vec![
            "docs/x.md".to_string(),
            "tests/test_a.py".to_string(),
            "tests/new.py".to_string(),
        ];
        let (ok, why) = grade(&c, Some(&h), &changed, Some(false));
        assert!(!ok);
        assert_eq!(
            why,
            [
                "expected action fix, got answer",
                "changed docs/x.md outside allowed paths ['lib/**', 'tests/**']",
                "changed protected fixture file tests/test_a.py",
                "project checks did not pass",
            ]
        );
        let (_, why) = grade(&c, Some(&json!({"action": "fix"})), &[], None);
        assert_eq!(why, ["project checks were not run"]);
        let (ok, _) = grade(
            &c,
            Some(&json!({"action": "fix"})),
            &["lib/a.py".into()],
            Some(true),
        );
        assert!(ok);
    }

    #[test]
    fn parse_case_validates() {
        let ok = "name: n\nissue: {title: t, body: b}\nrepo:\n  files:\n    a.py: |\n      x\nexpect: {action: fix}\n";
        let c = parse_case(ok, "p").unwrap();
        assert_eq!(c.files, vec![("a.py".to_string(), "x\n".to_string())]);
        assert_eq!(c.expect.checks_pass, None);
        assert!(c.expect.allowed_paths.is_empty());
        assert_eq!(
            parse_case("- a", "p").unwrap_err(),
            "p: case must be a mapping"
        );
        assert_eq!(
            parse_case("name: n\n", "p").unwrap_err(),
            "p: missing 'issue'"
        );
        let bad = ok.replace("fix}", "nope}");
        assert_eq!(
            parse_case(&bad, "p").unwrap_err(),
            "p: expect.action must be one of fix, answer, escalate, got 'nope'"
        );
        let nofiles = "name: n\nissue: {}\nrepo: {files: {}}\nexpect: {action: fix}\n";
        assert_eq!(
            parse_case(nofiles, "p").unwrap_err(),
            "p: repo.files must list at least one file"
        );
    }

    #[test]
    fn real_cases_load_and_unknown_names_are_rejected() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/golden");
        let all = load_cases(&dir, None).unwrap();
        assert_eq!(all.len(), 6);
        let one = load_cases(&dir, Some(&["fix-off-by-one".to_string()])).unwrap();
        assert_eq!(one.len(), 1);
        let err = load_cases(&dir, Some(&["nope".to_string()])).unwrap_err();
        assert_eq!(err, "unknown golden case(s): nope");
    }

    fn run_with(cases: &[GoldenCase], budget: f64, costs: Vec<Option<f64>>) -> Summary {
        let mut it = costs.into_iter();
        run_suite(
            cases,
            budget,
            |c| c.name.as_str(),
            move |_c, _rem| {
                Ok(CaseOutcome {
                    passed: true,
                    reasons: vec![],
                    cost_usd: it.next().unwrap(),
                })
            },
        )
    }

    fn named(n: &str) -> GoldenCase {
        let mut c = case("fix");
        c.name = n.into();
        c
    }

    #[test]
    fn suite_stops_launching_once_the_budget_is_spent() {
        let cases = [named("a"), named("b"), named("c")];
        let s = run_with(&cases, 1.0, vec![Some(0.6), Some(0.6), Some(0.1)]);
        assert_eq!(s.results.len(), 2);
        assert_eq!(s.not_run, ["c"]);
        assert_eq!(s.spent_usd, 1.2);
        assert_eq!(exit_code(&s), 1);
    }

    #[test]
    fn unknown_cost_is_booked_at_the_remaining_budget() {
        let cases = [named("a"), named("b")];
        let s = run_with(&cases, 2.0, vec![None, Some(0.1)]);
        assert_eq!(s.results.len(), 1);
        assert_eq!(s.results[0].cost_usd, 2.0);
        assert_eq!(s.not_run, ["b"]);
    }

    #[test]
    fn a_crashing_case_fails_but_does_not_stop_the_suite() {
        let cases = [named("a"), named("b")];
        let mut n = 0;
        let s = run_suite(
            &cases,
            5.0,
            |c| c.name.as_str(),
            |_c, _r| {
                n += 1;
                if n == 1 {
                    Err(CaseCrash {
                        message: "RuntimeError: boom".into(),
                        cost_usd: Some(0.5),
                    })
                } else {
                    Ok(CaseOutcome {
                        passed: true,
                        reasons: vec![],
                        cost_usd: Some(0.25),
                    })
                }
            },
        );
        assert_eq!(s.results[0].reasons, ["case crashed: RuntimeError: boom"]);
        assert!(!s.results[0].passed);
        assert!(s.results[1].passed);
        assert_eq!(s.spent_usd, 0.75);
        assert_eq!(exit_code(&s), 1);
    }

    #[test]
    fn exit_code_is_zero_only_when_everything_ran_and_passed() {
        assert_eq!(exit_code(&Summary::default()), 1);
        let ok = run_with(&[named("a")], 5.0, vec![Some(0.1)]);
        assert_eq!(exit_code(&ok), 0);
    }

    #[test]
    fn write_last_run_records_the_summary() {
        let dir = std::env::temp_dir().join(format!("gl-{}", uuid::Uuid::new_v4().simple()));
        let s = run_with(&[named("a")], 5.0, vec![Some(0.25)]);
        let p = write_last_run(&s, 5.0, Some(&dir.join("sub").join("golden-last.json"))).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["budget_usd"], json!(5.0));
        assert_eq!(v["spent_usd"], json!(0.25));
        assert_eq!(v["results"][0]["name"], "a");
        assert!(text.contains("\"budget_usd\": 5.0"));
        assert!(text.ends_with("}\n"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fixture_builds_a_worktree_and_detects_changes() {
        let dir = std::env::temp_dir().join(format!("gf-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = case("fix");
        let sb = prepare_sandbox(&c, &dir).unwrap();
        assert!(sb.worktree.join("lib/a.py").is_file());
        assert_eq!(
            changed_paths(&sb.repo, &sb.worktree).unwrap(),
            Vec::<String>::new()
        );
        std::fs::write(sb.worktree.join("lib/a.py"), "changed").unwrap();
        std::fs::write(sb.worktree.join("new.txt"), "n").unwrap();
        std::fs::create_dir_all(sb.worktree.join("__pycache__")).unwrap();
        std::fs::write(sb.worktree.join("__pycache__/x.pyc"), "n").unwrap();
        assert_eq!(
            changed_paths(&sb.repo, &sb.worktree).unwrap(),
            ["lib/a.py", "new.txt"]
        );
        let projects: Value =
            serde_json::from_str(&std::fs::read_to_string(sb.home.join("projects.json")).unwrap())
                .unwrap();
        assert_eq!(
            projects["projects"]["golden"]["local_path"],
            json!(sb.repo.display().to_string())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_case_real_with_a_fake_agent_books_cost_and_grades() {
        let c = case("answer");
        let c = GoldenCase {
            expect: Expect {
                checks_pass: None,
                ..c.expect
            },
            ..c
        };
        let root = std::env::temp_dir().join(format!("gr-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&root).unwrap();
        let runner = |req: &AttemptRequest| {
            let h = gitlab_runner::handoff_path(req.run_id, ALIAS, "1", req.repo_root);
            std::fs::create_dir_all(h.parent().unwrap()).unwrap();
            std::fs::write(&h, r#"{"action": "answer"}"#).unwrap();
            AgentAttempt {
                cost_usd: Some(0.07),
                error: None,
            }
        };
        let out = run_case_real(&c, Some(1.0), &root, &runner, 2, 30);
        assert!(out.passed, "{:?}", out.reasons);
        assert_eq!(out.cost_usd, Some(0.07));
        // The handoff directory is cleaned up.
        assert!(!root
            .join("outputs/handoffs")
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false));

        let failing = |_: &AttemptRequest| AgentAttempt {
            cost_usd: None,
            error: Some("boom".into()),
        };
        let out = run_case_real(&c, Some(1.0), &root, &failing, 2, 30);
        assert!(!out.passed);
        assert_eq!(out.cost_usd, Some(1.0)); // unknown cost booked at the cap
        assert!(out.reasons.contains(&"agent call failed: boom".to_string()));
        assert!(out
            .reasons
            .contains(&"no handoff (missing or malformed)".to_string()));
        let _ = std::fs::remove_dir_all(root);
    }
}
