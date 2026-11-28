//! Standalone linters and SAST run on a merge request's changed files as *hints* for
//! the review model, which keeps or drops each one. Only allow-listed tools with fixed
//! arguments run, in a temp dir that holds just the changed files (no repo tool
//! config that could load code). A tool that is not installed is skipped.

use crate::dependency_audit::Outcome;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const RUN_BUDGET: Duration = Duration::from_secs(180);
pub const TOOL_TIMEOUT: Duration = Duration::from_secs(60);
pub const MAX_HINTS: usize = 60;
pub const MAX_FILES: usize = 60;
pub const MAX_FILE_BYTES: usize = 512 * 1024;
const MAX_MESSAGE: usize = 300;

#[derive(Debug, Clone, PartialEq)]
pub struct Hint {
    pub tool: &'static str,
    pub rule: String,
    pub path: String,
    pub line: i64,
    /// `error` | `warning` | `info`.
    pub severity: &'static str,
    pub message: String,
}

impl Hint {
    pub fn key(&self) -> String {
        format!("{}:{}", self.tool, self.rule)
    }
    pub fn to_json(&self) -> Value {
        json!({"tool": self.tool, "rule": self.rule, "path": self.path, "line": self.line,
               "severity": self.severity, "message": self.message})
    }
}

/// Runs `program args` in `dir`; `Outcome::Missing` when it is not installed.
pub trait LintRunner: Send + Sync {
    fn run(&self, program: &str, args: &[String], dir: &Path, timeout: Duration) -> Outcome;
    fn installed(&self, program: &str) -> bool;
}

pub struct SystemLintRunner;

impl LintRunner for SystemLintRunner {
    fn run(&self, program: &str, args: &[String], dir: &Path, timeout: Duration) -> Outcome {
        let path = std::env::var("PATH").unwrap_or_default();
        let Some(bin) = crate::doctor::find_on_path(program, &path) else {
            return Outcome::Missing;
        };
        let mut command = std::process::Command::new(bin);
        command
            .args(args)
            .current_dir(dir)
            .env("SEMGREP_SEND_METRICS", "off");
        match crate::proc::run_group(&mut command, None, Some(timeout)) {
            Err(e) => Outcome::Failed(e.to_string()),
            Ok(out) if out.timed_out => Outcome::TimedOut(timeout),
            Ok(out) => Outcome::Finished {
                code: out.code,
                stdout: out.stdout,
                stderr: out.stderr,
            },
        }
    }

    fn installed(&self, program: &str) -> bool {
        crate::doctor::find_on_path(program, &std::env::var("PATH").unwrap_or_default()).is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tool {
    Shellcheck,
    Hadolint,
    Actionlint,
    Ruff,
    Yamllint,
    Semgrep,
}

pub const ALL: [Tool; 6] = [
    Tool::Shellcheck,
    Tool::Hadolint,
    Tool::Actionlint,
    Tool::Ruff,
    Tool::Yamllint,
    Tool::Semgrep,
];

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Shellcheck => "shellcheck",
            Tool::Hadolint => "hadolint",
            Tool::Actionlint => "actionlint",
            Tool::Ruff => "ruff",
            Tool::Yamllint => "yamllint",
            Tool::Semgrep => "semgrep",
        }
    }

    pub fn wants(self, path: &str) -> bool {
        let b = base(path);
        match self {
            Tool::Shellcheck => b.ends_with(".sh") || b.ends_with(".bash"),
            Tool::Hadolint => {
                b == "Dockerfile" || b.starts_with("Dockerfile.") || b.ends_with(".dockerfile")
            }
            Tool::Actionlint => {
                path.starts_with(".github/workflows/")
                    && (b.ends_with(".yml") || b.ends_with(".yaml"))
            }
            Tool::Ruff => b.ends_with(".py"),
            Tool::Yamllint => b.ends_with(".yml") || b.ends_with(".yaml"),
            Tool::Semgrep => true,
        }
    }

    /// Fixed arguments; `files` are relative to the temp dir.
    fn args(self, files: &[String], semgrep_rules: &[PathBuf]) -> Vec<String> {
        let mut a: Vec<String> = match self {
            Tool::Shellcheck => vec!["-f".into(), "json1".into()],
            Tool::Hadolint => vec!["-f".into(), "json".into(), "--no-fail".into()],
            Tool::Actionlint => vec!["-format".into(), "{{json .}}".into(), "-no-color".into()],
            Tool::Ruff => [
                "check",
                "--output-format",
                "json",
                "--no-cache",
                "--isolated",
                "--exit-zero",
                "--select",
                "E9,F,B,S",
            ]
            .map(String::from)
            .to_vec(),
            Tool::Yamllint => ["-f", "parsable", "-d", "relaxed"]
                .map(String::from)
                .to_vec(),
            Tool::Semgrep => {
                let mut v: Vec<String> = [
                    "scan",
                    "--json",
                    "--metrics=off",
                    "--disable-version-check",
                    "--quiet",
                ]
                .map(String::from)
                .to_vec();
                for r in semgrep_rules {
                    v.push("--config".into());
                    v.push(r.display().to_string());
                }
                v
            }
        };
        a.push("--".into());
        if self == Tool::Actionlint || self == Tool::Hadolint {
            a.pop();
        }
        a.extend(files.iter().cloned());
        a
    }

    pub fn parse(self, stdout: &str, dir: &Path) -> Vec<Hint> {
        let rel = |p: &str| relative(p, dir);
        let msg = |s: &str| {
            s.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(MAX_MESSAGE)
                .collect::<String>()
        };
        let v: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
        let mut out = Vec::new();
        match self {
            Tool::Shellcheck => {
                for c in v["comments"].as_array().into_iter().flatten() {
                    out.push(Hint {
                        tool: "shellcheck",
                        rule: format!("SC{}", c["code"].as_i64().unwrap_or(0)),
                        path: rel(c["file"].as_str().unwrap_or("")),
                        line: c["line"].as_i64().unwrap_or(0),
                        severity: level(c["level"].as_str().unwrap_or("")),
                        message: msg(c["message"].as_str().unwrap_or("")),
                    });
                }
            }
            Tool::Hadolint => {
                for c in v.as_array().into_iter().flatten() {
                    out.push(Hint {
                        tool: "hadolint",
                        rule: c["code"].as_str().unwrap_or("").to_string(),
                        path: rel(c["file"].as_str().unwrap_or("")),
                        line: c["line"].as_i64().unwrap_or(0),
                        severity: level(c["level"].as_str().unwrap_or("")),
                        message: msg(c["message"].as_str().unwrap_or("")),
                    });
                }
            }
            Tool::Actionlint => {
                for c in v.as_array().into_iter().flatten() {
                    out.push(Hint {
                        tool: "actionlint",
                        rule: c["kind"].as_str().unwrap_or("check").to_string(),
                        path: rel(c["filepath"].as_str().unwrap_or("")),
                        line: c["line"].as_i64().unwrap_or(0),
                        severity: "error",
                        message: msg(c["message"].as_str().unwrap_or("")),
                    });
                }
            }
            Tool::Ruff => {
                for c in v.as_array().into_iter().flatten() {
                    let code = c["code"].as_str().unwrap_or("");
                    out.push(Hint {
                        tool: "ruff",
                        rule: code.to_string(),
                        path: rel(c["filename"].as_str().unwrap_or("")),
                        line: c["location"]["row"].as_i64().unwrap_or(0),
                        severity: if code.starts_with('S') || code.starts_with("E9") {
                            "error"
                        } else {
                            "warning"
                        },
                        message: msg(c["message"].as_str().unwrap_or("")),
                    });
                }
            }
            Tool::Yamllint => {
                // file:line:col: [level] message (rule)
                for l in stdout.lines() {
                    let mut parts = l.splitn(4, ':');
                    let (Some(f), Some(line), Some(_col), Some(rest)) =
                        (parts.next(), parts.next(), parts.next(), parts.next())
                    else {
                        continue;
                    };
                    let rest = rest.trim();
                    let sev = if rest.starts_with("[error]") {
                        "error"
                    } else {
                        "warning"
                    };
                    let rule = rest
                        .rsplit_once('(')
                        .map(|(_, r)| r.trim_end_matches(')').to_string())
                        .unwrap_or_else(|| "yaml".into());
                    let text = rest
                        .trim_start_matches("[error]")
                        .trim_start_matches("[warning]")
                        .rsplit_once('(')
                        .map_or(rest, |(m, _)| m);
                    out.push(Hint {
                        tool: "yamllint",
                        rule,
                        path: rel(f),
                        line: line.parse().unwrap_or(0),
                        severity: sev,
                        message: msg(text),
                    });
                }
            }
            Tool::Semgrep => {
                for r in v["results"].as_array().into_iter().flatten() {
                    let check = r["check_id"].as_str().unwrap_or("");
                    out.push(Hint {
                        tool: "semgrep",
                        rule: check.rsplit('.').next().unwrap_or(check).to_string(),
                        path: rel(r["path"].as_str().unwrap_or("")),
                        line: r["start"]["line"].as_i64().unwrap_or(0),
                        severity: level(r["extra"]["severity"].as_str().unwrap_or("")),
                        message: msg(r["extra"]["message"].as_str().unwrap_or("")),
                    });
                }
            }
        }
        out
    }
}

fn level(s: &str) -> &'static str {
    match s.to_ascii_lowercase().as_str() {
        "error" | "high" | "critical" => "error",
        "warning" | "medium" => "warning",
        _ => "info",
    }
}

/// Tool paths may be absolute (and macOS temp dirs resolve `/var` to `/private/var`).
fn relative(p: &str, dir: &Path) -> String {
    let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    for d in [dir.to_path_buf(), canon] {
        let prefix = format!("{}/", d.display());
        if let Some(r) = p.strip_prefix(&prefix) {
            return r.to_string();
        }
    }
    p.trim_start_matches("./").to_string()
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolReport {
    pub hints: Vec<Hint>,
    /// One `name: N hint(s)` / `name: not installed` / `name: timed out` per tool that applied.
    pub lines: Vec<String>,
}

/// Run every applicable, enabled tool on `files` (written under `dir`), keeping only
/// hints on `added` lines (`path -> new line numbers`).
pub fn run_all(
    runner: &dyn LintRunner,
    dir: &Path,
    files: &[String],
    added: &std::collections::BTreeMap<String, BTreeSet<i64>>,
    enabled: &dyn Fn(Tool) -> bool,
    semgrep_rules: &[PathBuf],
    budget: &mut Duration,
) -> ToolReport {
    let mut report = ToolReport::default();
    for tool in ALL {
        let mine: Vec<String> = files.iter().filter(|f| tool.wants(f)).cloned().collect();
        if mine.is_empty() || !enabled(tool) || !runner.installed(tool.name()) {
            continue;
        }
        if tool == Tool::Semgrep && semgrep_rules.is_empty() {
            report.lines.push("semgrep: rules not cached yet".into());
            continue;
        }
        if budget.is_zero() {
            report
                .lines
                .push(format!("{}: skipped (time budget spent)", tool.name()));
            continue;
        }
        let started = Instant::now();
        let outcome = runner.run(
            tool.name(),
            &tool.args(&mine, semgrep_rules),
            dir,
            TOOL_TIMEOUT.min(*budget),
        );
        *budget = budget.saturating_sub(started.elapsed());
        match outcome {
            Outcome::Missing => continue,
            Outcome::TimedOut(_) => report.lines.push(format!("{}: timed out", tool.name())),
            Outcome::Failed(_) => report.lines.push(format!("{}: could not run", tool.name())),
            Outcome::Finished { stdout, .. } => {
                let hints: Vec<Hint> = tool
                    .parse(&stdout, dir)
                    .into_iter()
                    .filter(|h| added.get(&h.path).is_some_and(|l| l.contains(&h.line)))
                    .collect();
                report.lines.push(format!(
                    "{}: {} hint(s) on added lines",
                    tool.name(),
                    hints.len()
                ));
                report.hints.extend(hints);
            }
        }
    }
    report.hints.sort_by_key(|h| match h.severity {
        "error" => 0,
        "warning" => 1,
        _ => 2,
    });
    report.hints.truncate(MAX_HINTS);
    report
}

/// `path -> new line numbers of added lines`, from the unified diff text.
pub fn added_lines(diff_text: &str) -> std::collections::BTreeMap<String, BTreeSet<i64>> {
    let mut out: std::collections::BTreeMap<String, BTreeSet<i64>> = Default::default();
    let (mut path, mut new): (Option<String>, i64) = (None, 0);
    for line in diff_text.lines() {
        if let Some(p) = line.strip_prefix("+++ b/") {
            path = Some(p.to_string());
            continue;
        }
        if line.starts_with("--- a/") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ -") {
            new = rest
                .split_once(" +")
                .and_then(|(_, r)| r.split([',', ' ']).next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            continue;
        }
        let Some(p) = &path else { continue };
        if line.starts_with('+') {
            out.entry(p.clone()).or_default().insert(new);
            new += 1;
        } else if line.starts_with(' ') {
            new += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        calls: Mutex<Vec<(String, Vec<String>)>>,
        reply: fn(&str) -> Outcome,
    }

    impl LintRunner for Fake {
        fn run(&self, program: &str, args: &[String], _dir: &Path, _t: Duration) -> Outcome {
            self.calls
                .lock()
                .unwrap()
                .push((program.to_string(), args.to_vec()));
            (self.reply)(program)
        }
        fn installed(&self, _program: &str) -> bool {
            true
        }
    }

    fn done(stdout: &str) -> Outcome {
        Outcome::Finished {
            code: Some(1),
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    #[test]
    fn parsers_read_each_tools_format() {
        let d = Path::new("/tmp/x");
        let sc = Tool::Shellcheck.parse(r#"{"comments":[{"file":"bin/a.sh","line":3,"level":"warning","code":2086,"message":"Double quote to prevent globbing."}]}"#, d);
        assert_eq!(
            (sc[0].rule.as_str(), sc[0].line, sc[0].severity),
            ("SC2086", 3, "warning")
        );
        let ruff = Tool::Ruff.parse(r#"[{"code":"S608","message":"Possible SQL injection","filename":"/tmp/x/app/q.py","location":{"row":7,"column":1}}]"#, d);
        assert_eq!(
            (ruff[0].path.as_str(), ruff[0].severity),
            ("app/q.py", "error")
        );
        let hl = Tool::Hadolint.parse(r#"[{"code":"DL3008","file":"Dockerfile","line":2,"level":"warning","message":"Pin versions"}]"#, d);
        assert_eq!(hl[0].rule, "DL3008");
        let al = Tool::Actionlint.parse(r#"[{"message":"bad","filepath":".github/workflows/ci.yml","line":9,"kind":"expression"}]"#, d);
        assert_eq!((al[0].rule.as_str(), al[0].line), ("expression", 9));
        let yl = Tool::Yamllint.parse(
            "config/a.yml:4:1: [error] duplication of key \"x\" in mapping (key-duplicates)\n",
            d,
        );
        assert_eq!(
            (yl[0].rule.as_str(), yl[0].severity, yl[0].line),
            ("key-duplicates", "error", 4)
        );
        assert_eq!(yl[0].message, "duplication of key \"x\" in mapping");
        let sg = Tool::Semgrep.parse(r#"{"results":[{"check_id":"python.lang.security.audit.eval-detected","path":"app/q.py","start":{"line":5},"extra":{"message":"eval","severity":"ERROR"}}]}"#, d);
        assert_eq!(
            (sg[0].rule.as_str(), sg[0].severity),
            ("eval-detected", "error")
        );
        assert!(Tool::Ruff.parse("not json", d).is_empty());
    }

    #[test]
    fn runs_applicable_tools_keeps_added_line_hints_and_skips_missing() {
        let fake = Fake {
            calls: Mutex::new(vec![]),
            reply: |p| match p {
                "ruff" => done(
                    r#"[{"code":"S307","message":"eval","filename":"app/q.py","location":{"row":2}},{"code":"F401","message":"unused","filename":"app/q.py","location":{"row":1}}]"#,
                ),
                _ => Outcome::Missing,
            },
        };
        let files = vec!["app/q.py".to_string(), "bin/a.sh".to_string()];
        let added =
            added_lines("--- a/app/q.py\n+++ b/app/q.py\n@@ -1,1 +1,2 @@\n import os\n+eval(x)\n");
        let mut budget = RUN_BUDGET;
        let r = run_all(
            &fake,
            Path::new("/tmp/x"),
            &files,
            &added,
            &|t| t != Tool::Semgrep,
            &[],
            &mut budget,
        );
        assert_eq!(r.hints.len(), 1);
        assert_eq!(r.hints[0].key(), "ruff:S307");
        assert_eq!(r.lines, ["ruff: 1 hint(s) on added lines"]);
        let calls = fake.calls.lock().unwrap();
        let programs: Vec<&str> = calls.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(programs, ["shellcheck", "ruff"]);
        assert!(calls[1].1.contains(&"--isolated".to_string()));
        assert_eq!(calls[1].1.last().unwrap(), "app/q.py");
    }

    #[test]
    fn semgrep_needs_cached_rules_and_the_budget_is_respected() {
        let fake = Fake {
            calls: Mutex::new(vec![]),
            reply: |_| done("{}"),
        };
        let files = vec!["a.go".to_string()];
        let mut budget = RUN_BUDGET;
        let r = run_all(
            &fake,
            Path::new("/tmp/x"),
            &files,
            &Default::default(),
            &|_| true,
            &[],
            &mut budget,
        );
        assert_eq!(r.lines, ["semgrep: rules not cached yet"]);
        let rules = vec![PathBuf::from("/c/p-default.yml")];
        let r = run_all(
            &fake,
            Path::new("/tmp/x"),
            &files,
            &Default::default(),
            &|_| true,
            &rules,
            &mut budget,
        );
        assert_eq!(r.lines, ["semgrep: 0 hint(s) on added lines"]);
        assert!(fake.calls.lock().unwrap()[0]
            .1
            .windows(2)
            .any(|w| w[0] == "--config" && w[1] == "/c/p-default.yml"));
        let mut none = Duration::ZERO;
        let r = run_all(
            &fake,
            Path::new("/tmp/x"),
            &files,
            &Default::default(),
            &|_| true,
            &rules,
            &mut none,
        );
        assert_eq!(r.lines, ["semgrep: skipped (time budget spent)"]);
    }

    #[test]
    fn added_lines_track_new_line_numbers() {
        let a = added_lines(
            "--- a/x\n+++ b/x\n@@ -1,3 +1,4 @@\n a\n-b\n+c\n+d\n e\n@@ -10 +11,2 @@\n z\n+y\n",
        );
        assert_eq!(a["x"].iter().copied().collect::<Vec<_>>(), [2, 3, 12]);
    }
}
