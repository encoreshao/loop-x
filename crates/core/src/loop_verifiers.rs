//! Verifiers: run after each agent call to decide whether the iteration
//! passed. Mirrors `bin/loop_verifiers.py` (`CommandVerifier`, `DiffVerifier`,
//! `ProjectCommandsVerifier`, `ObserveOnly`, `build_verifiers`).
//!
//! Difference from Python: a verifier never panics or errors out of
//! [`Verifier::verify`]. Python's `CommandVerifier` lets a missing binary or a
//! bad `shlex.split` raise (crashing the runtime); here that becomes a failed
//! result whose `output` starts with the Python exception name and whose
//! `evidence` carries `"error": true`, which is what
//! `ProjectCommandsVerifier` already did.

use crate::config::{default_config_path, Config};
use crate::loop_result::VerificationResult;
use crate::loop_runtime::LoopContext;
use crate::loops_config::{py_repr, py_repr_str};
use crate::proc;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// How much of a command's output `ProjectCommandsVerifier` keeps in evidence.
pub const OUTPUT_TAIL_CHARS: usize = 4000;

pub trait Verifier: Send {
    fn verify(&self, context: &LoopContext<'_>) -> VerificationResult;
}

fn elapsed_ms(start: Instant) -> i64 {
    i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX)
}

/// A Python-style exception that stopped a verifier from running.
#[derive(Debug, Clone, PartialEq)]
struct Raised {
    kind: &'static str,
    message: String,
}

impl Raised {
    fn text(&self) -> String {
        format!("{}: {}", self.kind, self.message)
    }
}

fn error_result(name: &str, raised: &Raised) -> VerificationResult {
    VerificationResult {
        name: name.to_string(),
        passed: false,
        exit_code: None,
        duration_ms: 0,
        output: raised.text(),
        evidence: json!({"error": true}),
    }
}

fn seconds_json(s: f64) -> Value {
    if s.fract() == 0.0 && s.abs() < 1e15 {
        json!(s as i64)
    } else {
        json!(s)
    }
}

/// Runs `command` (split like `shlex.split`, no shell) and passes on exit 0.
#[derive(Debug, Clone)]
pub struct CommandVerifier {
    pub name: String,
    pub command: String,
    pub cwd: Option<PathBuf>,
    pub timeout_seconds: Option<f64>,
}

impl CommandVerifier {
    pub fn new(name: &str, command: &str) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            cwd: None,
            timeout_seconds: None,
        }
    }

    pub fn with_cwd(mut self, cwd: &Path) -> Self {
        self.cwd = Some(cwd.to_path_buf());
        self
    }

    pub fn with_timeout(mut self, seconds: f64) -> Self {
        self.timeout_seconds = Some(seconds);
        self
    }

    fn run(&self) -> Result<VerificationResult, Raised> {
        let start = Instant::now();
        let mut evidence = Map::new();
        evidence.insert("command".into(), json!(self.command));
        evidence.insert(
            "cwd".into(),
            self.cwd
                .as_ref()
                .map_or(Value::Null, |c| json!(c.display().to_string())),
        );
        if let Some(t) = self.timeout_seconds {
            evidence.insert("timeout_seconds".into(), seconds_json(t));
        }
        let argv = shlex::split(&self.command)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| Raised {
                kind: "ValueError",
                message: format!("cannot parse command {}", py_repr_str(&self.command)),
            })?;
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        let timeout = self.timeout_seconds.map(Duration::from_secs_f64);
        let out = proc::run(&mut cmd, None, timeout).map_err(|e| Raised {
            kind: if e.kind() == std::io::ErrorKind::NotFound {
                "FileNotFoundError"
            } else {
                "OSError"
            },
            message: if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "[Errno 2] No such file or directory: {}",
                    py_repr_str(&argv[0])
                )
            } else {
                e.to_string()
            },
        })?;
        if out.timed_out {
            evidence.insert("timed_out".into(), json!(true));
            return Ok(VerificationResult {
                name: self.name.clone(),
                passed: false,
                exit_code: None,
                duration_ms: elapsed_ms(start),
                output: format!("{}{}", out.stdout, out.stderr),
                evidence: Value::Object(evidence),
            });
        }
        Ok(VerificationResult {
            name: self.name.clone(),
            passed: out.code == Some(0),
            exit_code: out.code.map(i64::from),
            duration_ms: elapsed_ms(start),
            output: format!("{}{}", out.stdout, out.stderr),
            evidence: Value::Object(evidence),
        })
    }
}

impl Verifier for CommandVerifier {
    fn verify(&self, _context: &LoopContext<'_>) -> VerificationResult {
        self.run()
            .unwrap_or_else(|raised| error_result(&self.name, &raised))
    }
}

fn is_allowed_path(path: &str, allowed_paths: &[String]) -> bool {
    allowed_paths.iter().any(|allowed| {
        let prefix = if allowed.ends_with('/') {
            allowed.clone()
        } else {
            format!("{allowed}/")
        };
        path == allowed || path.starts_with(&prefix)
    })
}

/// Fails if any changed file (tracked-modified or new untracked) falls outside
/// `allowed_paths`.
#[derive(Debug, Clone)]
pub struct DiffVerifier {
    pub name: String,
    pub allowed_paths: Vec<String>,
    pub cwd: Option<PathBuf>,
}

impl DiffVerifier {
    fn git_lines(&self, args: &[&str]) -> Vec<String> {
        let mut cmd = Command::new("git");
        cmd.args(args);
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        // A failing git (no HEAD, not a repo) prints nothing: no changes seen.
        proc::run(&mut cmd, None, None)
            .map(|o| {
                o.stdout
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Verifier for DiffVerifier {
    fn verify(&self, _context: &LoopContext<'_>) -> VerificationResult {
        let start = Instant::now();
        let mut changed = self.git_lines(&["diff", "--name-only", "HEAD"]);
        changed.extend(self.git_lines(&["ls-files", "--others", "--exclude-standard"]));
        let duration_ms = elapsed_ms(start);
        changed.sort();
        changed.dedup();
        let disallowed: Vec<String> = changed
            .iter()
            .filter(|f| !is_allowed_path(f, &self.allowed_paths))
            .cloned()
            .collect();
        let passed = disallowed.is_empty();
        VerificationResult {
            name: self.name.clone(),
            passed,
            exit_code: Some(if passed { 0 } else { 1 }),
            duration_ms,
            output: disallowed.join("\n"),
            evidence: json!({
                "changed_files": changed,
                "disallowed_files": disallowed,
                "allowed_paths": self.allowed_paths,
            }),
        }
    }
}

type ProjectFn = Box<dyn Fn(&str) -> Result<Value, String> + Send>;
type WorktreeRootFn = Box<dyn Fn() -> Result<PathBuf, String> + Send>;

/// Re-runs the project's own `test_cmd`/`lint_cmd` (from projects.json) in the
/// worktree the agent used for this issue. No worktree means no code change, so
/// a vacuous pass; with `handoff_path` (gate mode) anything but
/// `{"action": "fix"}` is vacuous too.
pub struct ProjectCommandsVerifier {
    pub name: String,
    pub alias: String,
    pub issue_iid: String,
    pub timeout_seconds: f64,
    pub handoff_path: Option<PathBuf>,
    /// projects.json to read when no `project_fn` is injected (default: the
    /// per-machine one, resolved at call time).
    pub config_path: Option<PathBuf>,
    project_fn: Option<ProjectFn>,
    worktree_root_fn: Option<WorktreeRootFn>,
}

impl ProjectCommandsVerifier {
    pub fn new(name: &str, alias: &str, issue_iid: impl ToString, timeout_seconds: f64) -> Self {
        Self {
            name: name.into(),
            alias: alias.into(),
            issue_iid: issue_iid.to_string(),
            timeout_seconds,
            handoff_path: None,
            config_path: None,
            project_fn: None,
            worktree_root_fn: None,
        }
    }

    pub fn with_handoff_path(mut self, path: Option<PathBuf>) -> Self {
        self.handoff_path = path;
        self
    }

    pub fn with_config_path(mut self, path: Option<PathBuf>) -> Self {
        self.config_path = path;
        self
    }

    fn config(&self) -> Result<Config, String> {
        let path = self.config_path.clone().unwrap_or_else(default_config_path);
        Config::load(&path).map_err(|e| e.to_string())
    }

    /// Replace the projects.json lookup (tests, or a caller with its own config).
    pub fn with_project_fn(
        mut self,
        f: impl Fn(&str) -> Result<Value, String> + Send + 'static,
    ) -> Self {
        self.project_fn = Some(Box::new(f));
        self
    }

    pub fn with_worktree_root_fn(
        mut self,
        f: impl Fn() -> Result<PathBuf, String> + Send + 'static,
    ) -> Self {
        self.worktree_root_fn = Some(Box::new(f));
        self
    }

    fn claims_fix(&self) -> bool {
        let Some(path) = &self.handoff_path else {
            return false;
        };
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .is_some_and(|v| v.get("action").and_then(Value::as_str) == Some("fix"))
    }

    fn project(&self) -> Result<Value, Raised> {
        let result = match &self.project_fn {
            Some(f) => f(&self.alias),
            None => self
                .config()
                .and_then(|c| c.project(&self.alias).map_err(|e| e.to_string())),
        };
        result.map_err(|message| Raised {
            kind: "KeyError",
            message: if message.starts_with("unknown project alias") {
                py_repr_str(&self.alias)
            } else {
                message
            },
        })
    }

    fn worktree_root(&self) -> Result<PathBuf, Raised> {
        let result = match &self.worktree_root_fn {
            Some(f) => f(),
            None => self
                .config()
                .and_then(|c| c.worktree_root().map_err(|e| e.to_string()))
                .map(PathBuf::from),
        };
        result.map_err(|message| Raised {
            kind: "KeyError",
            message,
        })
    }

    fn run(&self) -> Result<VerificationResult, Raised> {
        let start = Instant::now();
        let vacuous = |why: &str| VerificationResult {
            name: self.name.clone(),
            passed: true,
            exit_code: None,
            duration_ms: 0,
            output: why.into(),
            evidence: json!({"vacuous": true}),
        };
        if self.handoff_path.is_some() && !self.claims_fix() {
            return Ok(vacuous("no fix handoff - nothing to verify"));
        }
        let project = self.project()?;
        let root = self.worktree_root()?;
        let local_path = project
            .get("local_path")
            .and_then(Value::as_str)
            .ok_or_else(|| Raised {
                kind: "KeyError",
                message: "'local_path'".into(),
            })?;
        let repo = Path::new(local_path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let worktree = root.join(format!("{repo}-issue-{}", self.issue_iid));
        if !worktree.is_dir() {
            return Ok(vacuous("no worktree - nothing to verify"));
        }

        let (mut commands, mut lines, mut passed) = (Vec::new(), Vec::new(), true);
        for kind in ["test", "lint"] {
            let Some(command) = project
                .get(format!("{kind}_cmd"))
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
            else {
                continue;
            };
            let verifier = CommandVerifier::new(&format!("{}_{kind}", self.name), command)
                .with_cwd(&worktree)
                .with_timeout(self.timeout_seconds);
            let result = verifier.run()?;
            passed = passed && result.passed;
            commands.push(json!({
                "kind": kind,
                "command": command,
                "passed": result.passed,
                "exit_code": result.exit_code,
                "output": tail_chars(&result.output, OUTPUT_TAIL_CHARS),
            }));
            // The tail lives only in evidence; output is one line per command.
            let status = if result.passed {
                "passed".to_string()
            } else if let Some(code) = result.exit_code {
                format!("failed (exit {code})")
            } else {
                "failed (timed out)".to_string()
            };
            lines.push(format!("$ {command}: {status}"));
        }
        Ok(VerificationResult {
            name: self.name.clone(),
            passed,
            exit_code: Some(if passed { 0 } else { 1 }),
            duration_ms: elapsed_ms(start),
            output: lines.join("\n"),
            evidence: json!({"commands": commands}),
        })
    }
}

/// The last `n` characters (Python `s[-n:]`).
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        s.to_string()
    } else {
        s.chars().skip(count - n).collect()
    }
}

impl Verifier for ProjectCommandsVerifier {
    /// Never fails out: a misconfigured project or missing binary must not
    /// crash a batch.
    fn verify(&self, _context: &LoopContext<'_>) -> VerificationResult {
        self.run()
            .unwrap_or_else(|raised| error_result(&self.name, &raised))
    }
}

/// Records the inner verifier but never fails the loop; the real outcome is
/// kept in `evidence["observed_passed"]`.
pub struct ObserveOnly {
    pub inner: Box<dyn Verifier>,
}

impl Verifier for ObserveOnly {
    fn verify(&self, context: &LoopContext<'_>) -> VerificationResult {
        let mut result = self.inner.verify(context);
        let mut evidence = match std::mem::take(&mut result.evidence) {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        evidence.insert("observed_passed".into(), json!(result.passed));
        evidence.insert("mode".into(), json!("observe"));
        result.passed = true;
        result.evidence = Value::Object(evidence);
        result
    }
}

/// Per-issue context for `project_commands` verifiers.
#[derive(Debug, Clone)]
pub struct IssueContext {
    pub alias: String,
    pub issue_iid: String,
    pub timeout_seconds: f64,
    pub handoff_path: Option<PathBuf>,
    /// Override of the projects.json path (default: the per-machine one).
    pub config_path: Option<PathBuf>,
}

impl IssueContext {
    pub fn new(alias: &str, issue_iid: impl ToString, timeout_seconds: f64) -> Self {
        Self {
            alias: alias.into(),
            issue_iid: issue_iid.to_string(),
            timeout_seconds,
            handoff_path: None,
            config_path: None,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
#[error("{0}")]
pub struct VerifierBuildError(pub String);

fn build_err<T>(msg: String) -> Result<T, VerifierBuildError> {
    Err(VerifierBuildError(msg))
}

/// Build real verifiers from the raw `LoopDefinition.verifiers` specs. Fails
/// loud on an unknown type or a spec missing its required key rather than
/// silently skipping a misconfigured verifier.
pub fn build_verifiers(
    specs: &[Value],
    cwd: Option<&Path>,
    issue: Option<&IssueContext>,
    mode: &str,
) -> Result<Vec<Box<dyn Verifier>>, VerifierBuildError> {
    let mut verifiers: Vec<Box<dyn Verifier>> = Vec::new();
    for spec in specs {
        let name = spec.get("name").and_then(Value::as_str).ok_or_else(|| {
            VerifierBuildError(format!("verifier spec {} has no 'name'", py_repr(spec)))
        })?;
        let label = py_repr_str(name);
        let spec_type = spec.get("type");
        match spec_type.and_then(Value::as_str) {
            Some("command") => {
                let Some(command) = spec.get("command").and_then(Value::as_str) else {
                    return build_err(format!(
                        "verifier {label}: type 'command' requires a 'command' key"
                    ));
                };
                let mut v = CommandVerifier::new(name, command);
                v.cwd = cwd.map(Path::to_path_buf);
                verifiers.push(Box::new(v));
            }
            Some("git_diff") => {
                let Some(Value::Array(paths)) = spec.get("allowed_paths") else {
                    return build_err(format!(
                        "verifier {label}: type 'git_diff' requires an 'allowed_paths' key"
                    ));
                };
                verifiers.push(Box::new(DiffVerifier {
                    name: name.into(),
                    allowed_paths: paths
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                    cwd: cwd.map(Path::to_path_buf),
                }));
            }
            Some("project_commands") => {
                let Some(issue) = issue else {
                    return build_err(format!(
                        "verifier {label}: type 'project_commands' requires issue context"
                    ));
                };
                let verifier = ProjectCommandsVerifier::new(
                    name,
                    &issue.alias,
                    &issue.issue_iid,
                    issue.timeout_seconds,
                )
                .with_handoff_path(issue.handoff_path.clone())
                .with_config_path(issue.config_path.clone());
                if mode == "observe" {
                    verifiers.push(Box::new(ObserveOnly {
                        inner: Box::new(verifier),
                    }));
                } else {
                    verifiers.push(Box::new(verifier));
                }
            }
            _ => {
                let shown = spec_type.map_or("None".to_string(), py_repr);
                return build_err(format!("verifier {label}: unknown verifier type {shown}"));
            }
        }
    }
    Ok(verifiers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loop_definition::LoopDefinition;
    use std::fs;

    fn definition() -> LoopDefinition {
        LoopDefinition::from_value(&json!({
            "name": "t", "version": 1, "trigger": {"type": "manual"}, "goal": {"type": "g"}
        }))
        .unwrap()
    }

    fn with_ctx<R>(f: impl FnOnce(&LoopContext<'_>) -> R) -> R {
        let d = definition();
        f(&LoopContext {
            iteration: 1,
            definition: &d,
            previous: None,
        })
    }

    fn run(v: &dyn Verifier) -> VerificationResult {
        with_ctx(|c| v.verify(c))
    }

    #[test]
    fn passing_and_failing_commands() {
        let ok = run(&CommandVerifier::new("ok", "true"));
        assert!(ok.passed && ok.exit_code == Some(0) && ok.name == "ok");
        assert_eq!(ok.evidence, json!({"command": "true", "cwd": null}));
        let bad = run(&CommandVerifier::new("broken", "false"));
        assert!(!bad.passed && bad.exit_code == Some(1));
    }

    #[test]
    fn output_captures_both_streams() {
        let r = run(&CommandVerifier::new(
            "e",
            "sh -c 'echo hello; echo oops >&2'",
        ));
        assert!(r.output.contains("hello") && r.output.contains("oops"));
    }

    #[test]
    fn cwd_is_used_and_recorded() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("marker.txt"), "x").unwrap();
        let r = run(&CommandVerifier::new("c", "test -f marker.txt").with_cwd(d.path()));
        assert!(r.passed);
        assert_eq!(r.evidence["cwd"], d.path().display().to_string());
    }

    #[test]
    fn timeout_fails_with_partial_output_and_evidence() {
        let r = run(&CommandVerifier::new(
            "slow",
            "bash -c 'echo out; echo err >&2; exec sleep 5'",
        )
        .with_timeout(0.3));
        assert!(!r.passed && r.exit_code.is_none());
        assert_eq!(r.evidence["timed_out"], true);
        assert!(r.output.contains("out") && r.output.contains("err"));
        assert_eq!(r.evidence["timeout_seconds"], 0.3);
        let ok = run(&CommandVerifier::new("ok", "true").with_timeout(5.0));
        assert_eq!(ok.evidence["timeout_seconds"], 5);
    }

    #[test]
    fn missing_binary_is_a_failed_result_not_a_panic() {
        let r = run(&CommandVerifier::new("x", "no-such-binary-xyz --flag"));
        assert!(!r.passed && r.exit_code.is_none());
        assert!(r.output.starts_with("FileNotFoundError:"));
        assert_eq!(r.evidence, json!({"error": true}));
        assert!(!run(&CommandVerifier::new("x", "echo 'unterminated")).passed);
        assert!(!run(&CommandVerifier::new("x", "")).passed);
    }

    fn git(dir: &Path, args: &[&str]) {
        let s = Command::new("git")
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
            s.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&s.stderr)
        );
    }

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        git(d.path(), &["init", "-q"]);
        fs::create_dir_all(d.path().join("src")).unwrap();
        fs::write(d.path().join("src/a.txt"), "1").unwrap();
        fs::write(d.path().join("README"), "r").unwrap();
        git(d.path(), &["add", "."]);
        git(d.path(), &["commit", "-qm", "init"]);
        d
    }

    fn diff(d: &Path, allowed: &[&str]) -> VerificationResult {
        run(&DiffVerifier {
            name: "diff".into(),
            allowed_paths: allowed.iter().map(|s| s.to_string()).collect(),
            cwd: Some(d.to_path_buf()),
        })
    }

    #[test]
    fn diff_passes_with_no_changes_or_allowed_edits() {
        let d = repo();
        let r = diff(d.path(), &["src/"]);
        assert!(r.passed && r.exit_code == Some(0) && r.output.is_empty());
        fs::write(d.path().join("src/a.txt"), "2").unwrap();
        fs::write(d.path().join("src/new.txt"), "n").unwrap();
        let r = diff(d.path(), &["src"]);
        assert!(r.passed);
        assert_eq!(
            r.evidence["changed_files"],
            json!(["src/a.txt", "src/new.txt"])
        );
    }

    #[test]
    fn diff_fails_on_tracked_or_untracked_changes_outside_allowed() {
        let d = repo();
        fs::write(d.path().join("README"), "changed").unwrap();
        let r = diff(d.path(), &["src/"]);
        assert!(!r.passed && r.exit_code == Some(1));
        assert_eq!(r.output, "README");
        fs::write(d.path().join("README"), "r").unwrap();
        fs::write(d.path().join("stray.txt"), "s").unwrap();
        let r = diff(d.path(), &["src/"]);
        assert_eq!(r.evidence["disallowed_files"], json!(["stray.txt"]));
        // empty allowed_paths fails closed on any change
        assert!(!diff(d.path(), &[]).passed);
    }

    #[test]
    fn diff_allowed_path_is_a_directory_prefix_not_a_string_prefix() {
        assert!(is_allowed_path("src/a", &["src".into()]));
        assert!(!is_allowed_path("srcx/a", &["src".into()]));
        assert!(is_allowed_path("a.txt", &["a.txt".into()]));
    }

    fn pc_fixture(test_cmd: &str, lint_cmd: &str) -> (tempfile::TempDir, ProjectCommandsVerifier) {
        let d = tempfile::tempdir().unwrap();
        let project = json!({"local_path": d.path().join("repo").display().to_string(), "test_cmd": test_cmd, "lint_cmd": lint_cmd});
        let root = d.path().join("wt");
        let v = ProjectCommandsVerifier::new("pc", "web", 7, 30.0)
            .with_project_fn(move |_| Ok(project.clone()))
            .with_worktree_root_fn(move || Ok(root.clone()));
        (d, v)
    }

    fn make_worktree(d: &tempfile::TempDir) {
        fs::create_dir_all(d.path().join("wt/repo-issue-7")).unwrap();
    }

    #[test]
    fn project_commands_no_worktree_is_vacuous() {
        let (_d, v) = pc_fixture("false", "");
        let r = run(&v);
        assert!(r.passed);
        assert_eq!(r.evidence["vacuous"], true);
        assert_eq!(r.output, "no worktree - nothing to verify");
    }

    #[test]
    fn project_commands_runs_test_and_lint_in_the_worktree() {
        let (d, v) = pc_fixture("pwd", "false");
        make_worktree(&d);
        let r = run(&v);
        assert!(!r.passed);
        let kinds: Vec<_> = r.evidence["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["kind"].clone())
            .collect();
        assert_eq!(kinds, [json!("test"), json!("lint")]);
        assert!(r.evidence["commands"][0]["output"]
            .as_str()
            .unwrap()
            .contains("repo-issue-7"));
        assert_eq!(r.output, "$ pwd: passed\n$ false: failed (exit 1)");
    }

    #[test]
    fn project_commands_skips_empty_commands_and_bounds_the_tail() {
        let (d, v) = pc_fixture("true", "");
        make_worktree(&d);
        let r = run(&v);
        assert!(r.passed);
        assert_eq!(r.evidence["commands"].as_array().unwrap().len(), 1);
        let (d, v) = pc_fixture("python3 -c \"print('x'*100000); raise SystemExit(1)\"", "");
        make_worktree(&d);
        let r = run(&v);
        assert!(r.output.chars().count() < 4200);
        assert_eq!(
            r.evidence["commands"][0]["output"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            4000
        );
    }

    #[test]
    fn project_commands_tails_are_stored_once() {
        let (d, v) = pc_fixture("python3 -c \"print('TAIL' * 500)\"", "");
        make_worktree(&d);
        let r = run(&v);
        assert!(r.evidence["commands"][0]["output"]
            .as_str()
            .unwrap()
            .contains("TAIL"));
        assert!(!r.output.contains("TAILTAIL") && r.output.len() < 300);
    }

    #[test]
    fn project_commands_never_raises() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("wt");
        let v = ProjectCommandsVerifier::new("pc", "web", 7, 30.0)
            .with_project_fn(|alias| Err(format!("unknown project alias {alias:?}")))
            .with_worktree_root_fn(move || Ok(root.clone()));
        let r = run(&v);
        assert!(!r.passed && r.exit_code.is_none());
        assert!(r.output.starts_with("KeyError:"));
        assert_eq!(r.evidence, json!({"error": true}));

        let (d, v) = pc_fixture("no-such-binary-xyz --flag", "");
        make_worktree(&d);
        let r = run(&v);
        assert!(!r.passed && r.output.starts_with("FileNotFoundError:"));
        assert_eq!(r.evidence, json!({"error": true}));
    }

    #[test]
    fn handoff_without_fix_is_vacuous_even_with_a_worktree() {
        for handoff in [
            None,
            Some(r#"{"action": "answer"}"#),
            Some(r#"{"action": "escalate"}"#),
            Some("not json"),
        ] {
            let (d, v) = pc_fixture("false", "");
            make_worktree(&d);
            let hp = d.path().join("h.json");
            if let Some(text) = handoff {
                fs::write(&hp, text).unwrap();
            }
            let r = run(&v.with_handoff_path(Some(hp)));
            assert!(r.passed, "{handoff:?}");
            assert_eq!(r.evidence["vacuous"], true);
        }
    }

    #[test]
    fn fix_handoff_runs_the_checks() {
        let (d, v) = pc_fixture("false", "");
        make_worktree(&d);
        let hp = d.path().join("h.json");
        fs::write(&hp, r#"{"action": "fix"}"#).unwrap();
        assert!(!run(&v.with_handoff_path(Some(hp))).passed);
    }

    #[test]
    fn observe_only_never_fails_but_records_the_truth() {
        struct Bad;
        impl Verifier for Bad {
            fn verify(&self, _: &LoopContext<'_>) -> VerificationResult {
                VerificationResult {
                    name: "x".into(),
                    passed: false,
                    exit_code: Some(1),
                    duration_ms: 5,
                    output: "boom".into(),
                    evidence: json!({}),
                }
            }
        }
        let r = run(&ObserveOnly {
            inner: Box::new(Bad),
        });
        assert!(r.passed && r.output == "boom");
        assert_eq!(
            r.evidence,
            json!({"observed_passed": false, "mode": "observe"})
        );
        assert_eq!(r.exit_code, Some(1));
    }

    #[test]
    fn build_verifiers_builds_each_type_in_order() {
        let d = tempfile::tempdir().unwrap();
        let specs = vec![
            json!({"name": "tests", "type": "command", "command": "true"}),
            json!({"name": "diff", "type": "git_diff", "allowed_paths": ["src/"]}),
        ];
        let vs = build_verifiers(&specs, Some(d.path()), None, "observe").unwrap();
        assert_eq!(vs.len(), 2);
        let r = run(vs[0].as_ref());
        assert_eq!((r.name.as_str(), r.passed), ("tests", true));
        assert_eq!(r.evidence["cwd"], d.path().display().to_string());
        assert_eq!(run(vs[1].as_ref()).name, "diff");
        assert!(build_verifiers(&[], None, None, "observe")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn build_verifiers_fails_loud() {
        let e = |spec: Value, issue: Option<&IssueContext>| {
            build_verifiers(&[spec], None, issue, "observe")
                .err()
                .unwrap()
                .0
        };
        assert_eq!(
            e(json!({"name": "x", "type": "bogus"}), None),
            "verifier 'x': unknown verifier type 'bogus'"
        );
        assert_eq!(
            e(json!({"name": "x"}), None),
            "verifier 'x': unknown verifier type None"
        );
        assert_eq!(
            e(json!({"name": "x", "type": "command"}), None),
            "verifier 'x': type 'command' requires a 'command' key"
        );
        assert_eq!(
            e(json!({"name": "x", "type": "git_diff"}), None),
            "verifier 'x': type 'git_diff' requires an 'allowed_paths' key"
        );
        assert_eq!(
            e(json!({"name": "x", "type": "project_commands"}), None),
            "verifier 'x': type 'project_commands' requires issue context"
        );
        assert!(e(json!({"type": "command"}), None).contains("no 'name'"));
    }

    #[test]
    fn build_verifiers_wraps_project_commands_only_in_observe_mode() {
        let d = tempfile::tempdir().unwrap();
        let mut issue = IssueContext::new("web", 7, 30.0);
        issue.config_path = Some(d.path().join("no-projects.json"));
        let spec = [json!({"name": "pc", "type": "project_commands"})];
        // observe: a vacuous-by-config failure is hidden behind ObserveOnly
        let observe = build_verifiers(&spec, None, Some(&issue), "observe").unwrap();
        let r = run(observe[0].as_ref());
        assert!(r.passed);
        assert!(r.evidence.get("observed_passed").is_some());
        assert_eq!(r.evidence["mode"], "observe");
        // gate: unwrapped, so the real (errored) outcome shows through
        let gate = build_verifiers(&spec, None, Some(&issue), "gate").unwrap();
        assert!(run(gate[0].as_ref()).evidence.get("mode").is_none());
    }

    #[test]
    fn issue_context_passes_the_handoff_path_through() {
        let d = tempfile::tempdir().unwrap();
        let mut issue = IssueContext::new("web", 7, 30.0);
        issue.config_path = Some(d.path().join("no-projects.json"));
        issue.handoff_path = Some(d.path().join("h.json"));
        // Build in gate mode with a handoff that doesn't claim a fix: vacuous.
        let spec = [json!({"name": "pc", "type": "project_commands"})];
        let vs = build_verifiers(&spec, None, Some(&issue), "gate").unwrap();
        let r = run(vs[0].as_ref());
        assert!(r.passed);
        assert_eq!(r.output, "no fix handoff - nothing to verify");
        // Without a handoff path the project lookup is attempted (and errors here).
        issue.handoff_path = None;
        let vs = build_verifiers(&spec, None, Some(&issue), "gate").unwrap();
        let r = run(vs[0].as_ref());
        assert!(!r.passed && r.evidence == json!({"error": true}));
    }
}
