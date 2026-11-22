//! The last gate before a fix's merge request opens: a model-free scan of the
//! lines the branch adds for secrets and risky calls (`security_scan`).
//! A hit names its rule, path and line only, never the matched text.

use crate::loop_result::VerificationResult;
use crate::loop_runtime::LoopContext;
use crate::loop_verifiers::{branch_changes, IssueWork, Verifier, WorkDir};
use regex::Regex;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::LazyLock;
use std::time::Instant;

const MAX_BLOCKING_LINES: usize = 10;
const MAX_ADVISORY: usize = 20;

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub rule: &'static str,
    pub path: String,
    pub line: Option<i64>,
    pub blocking: bool,
}

struct Rule {
    id: &'static str,
    re: Regex,
    blocking: bool,
}

fn rule(id: &'static str, pattern: &str, blocking: bool) -> Rule {
    Rule {
        id,
        re: Regex::new(pattern).expect("static pattern"),
        blocking,
    }
}

static LINE_RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        rule("aws_access_key", r"AKIA[0-9A-Z]{16}", true),
        rule(
            "private_key",
            r"-----BEGIN ([A-Z]+ )?PRIVATE KEY-----",
            true,
        ),
        rule("github_token", r"gh[pousr]_[A-Za-z0-9]{36,}", true),
        rule("gitlab_token", r"glpat-[A-Za-z0-9_-]{20,}", true),
        rule("slack_token", r"xox[baprs]-[A-Za-z0-9-]{10,}", true),
        rule(
            "slack_webhook",
            r"https://hooks\.slack\.com/services/[A-Za-z0-9/]+",
            true,
        ),
        rule("dynamic_eval", r"\beval\s*\(", false),
        rule(
            "html_unescaped",
            r"\.html_safe\b|\braw\s*\(|dangerouslySetInnerHTML",
            false,
        ),
        rule(
            "shell_interpolation",
            r"`[^`]*#\{|\b(system|exec|spawn)\s*\([^)]*#\{",
            false,
        ),
    ]
});

static GENERIC_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b(password|passwd|secret|api_?key|access_?token|auth_?token)\b\s*[:=]\s*["']([^"'\s]{8,})["']"#,
    )
    .expect("static pattern")
});

static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[xX*]+|<[^>]*>|\$\{[^}]*\}|ENV\[[^\]]*\])$").expect("static pattern")
});

fn is_test_path(path: &str) -> bool {
    let p = Path::new(path);
    let in_test_dir = p.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some("spec" | "test" | "tests" | "__tests__" | "fixtures" | "fixture")
        )
    });
    let file = p.file_name().and_then(|f| f.to_str()).unwrap_or("");
    let stem = file.split('.').next().unwrap_or("");
    in_test_dir
        || stem.ends_with("_test")
        || stem.ends_with("_spec")
        || stem.starts_with("test_")
        || file.contains(".spec.")
        || file.contains(".test.")
}

fn is_sensitive_file(path: &str) -> bool {
    let file = Path::new(path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("");
    if file == ".env" {
        return true;
    }
    if let Some(rest) = file.strip_prefix(".env.") {
        return !matches!(rest, "example" | "sample" | "template");
    }
    if [".pem", ".key", ".p12", ".pfx"]
        .iter()
        .any(|ext| file.ends_with(ext))
    {
        return true;
    }
    (file.starts_with("id_rsa") || file.starts_with("id_ed25519")) && !file.ends_with(".pub")
}

fn hunk_new_start(header: &str) -> Option<i64> {
    let plus = header.split_whitespace().find(|t| t.starts_with('+'))?;
    plus[1..].split(',').next()?.parse().ok()
}

pub fn scan(diff: &str, added: &[String]) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut path: Option<String> = None;
    let mut line: i64 = 0;
    let mut in_hunk = false;
    for text in diff.lines() {
        if text.starts_with("diff --git ") {
            in_hunk = false;
            path = None;
            continue;
        }
        if !in_hunk {
            if let Some(p) = text.strip_prefix("+++ ") {
                path = p
                    .strip_prefix("b/")
                    .map(|p| p.trim_end_matches('\t').to_string());
            } else if text.starts_with("@@") {
                in_hunk = true;
                line = hunk_new_start(text).unwrap_or(1);
            }
            continue;
        }
        if text.starts_with("@@") {
            line = hunk_new_start(text).unwrap_or(line);
            continue;
        }
        match text.chars().next() {
            Some('+') => {
                if let Some(p) = &path {
                    scan_line(&text[1..], p, line, &mut hits);
                }
                line += 1;
            }
            Some(' ') => line += 1,
            _ => {}
        }
    }
    for file in added.iter().filter(|f| is_sensitive_file(f)) {
        hits.push(Hit {
            rule: "sensitive_file",
            path: file.clone(),
            line: None,
            blocking: true,
        });
    }
    hits
}

fn scan_line(text: &str, path: &str, line: i64, hits: &mut Vec<Hit>) {
    let mut hit = |rule: &'static str, blocking: bool| {
        hits.push(Hit {
            rule,
            path: path.to_string(),
            line: Some(line),
            blocking,
        })
    };
    for r in LINE_RULES.iter() {
        if r.re.is_match(text) {
            hit(r.id, r.blocking);
        }
    }
    if !is_test_path(path)
        && GENERIC_SECRET
            .captures_iter(text)
            .any(|c| !PLACEHOLDER.is_match(&c[2]))
    {
        hit("generic_secret", true);
    }
}

fn hit_json(h: &Hit) -> Value {
    json!({"rule": h.rule, "path": h.path, "line": h.line})
}

fn hit_line(h: &Hit) -> String {
    match h.line {
        Some(l) => format!("{} at {}:{l}", h.rule, h.path),
        None => format!("{} at {}", h.rule, h.path),
    }
}

pub fn scan_result(name: &str, hits: &[Hit], start: Instant) -> VerificationResult {
    let (blocking, advisory): (Vec<&Hit>, Vec<&Hit>) = hits.iter().partition(|h| h.blocking);
    let mut lines: Vec<String> = blocking
        .iter()
        .take(MAX_BLOCKING_LINES)
        .map(|h| hit_line(h))
        .collect();
    if !advisory.is_empty() {
        lines.push(format!("(+{} advisory notes)", advisory.len()));
    }
    let passed = blocking.is_empty();
    VerificationResult {
        name: name.to_string(),
        passed,
        exit_code: Some(if passed { 0 } else { 1 }),
        duration_ms: i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX),
        output: lines.join("\n"),
        evidence: json!({
            "gate": "security",
            "blocking": blocking.iter().map(|h| hit_json(h)).collect::<Vec<_>>(),
            "advisory": advisory.iter().take(MAX_ADVISORY).map(|h| hit_json(h)).collect::<Vec<_>>(),
        }),
    }
}

pub struct SecurityScanVerifier {
    pub name: String,
    pub work: IssueWork,
}

impl SecurityScanVerifier {
    pub fn new(name: &str, work: IssueWork) -> Self {
        Self {
            name: name.into(),
            work,
        }
    }

    fn special(&self, passed: bool, output: String, evidence: Value) -> VerificationResult {
        VerificationResult {
            name: self.name.clone(),
            passed,
            exit_code: None,
            duration_ms: 0,
            output,
            evidence,
        }
    }
}

impl Verifier for SecurityScanVerifier {
    fn verify(&self, _context: &LoopContext<'_>) -> VerificationResult {
        let start = Instant::now();
        let error = |e: String| self.special(false, e, json!({"error": true, "gate": "security"}));
        let (dir, handoff) = match self.work.resolve() {
            Ok(WorkDir::Vacuous(why)) => {
                return self.special(
                    true,
                    why.into(),
                    json!({"vacuous": true, "gate": "security"}),
                )
            }
            Ok(WorkDir::Ready { dir, handoff, .. }) => (dir, handoff.unwrap_or(Value::Null)),
            Err(e) => return error(e),
        };
        let target = handoff
            .get("target_branch")
            .and_then(Value::as_str)
            .unwrap_or("main");
        let (diff, added) = match branch_changes(&dir, target) {
            Ok(changes) => changes,
            Err(e) => return error(e),
        };
        scan_result(&self.name, &scan(&diff, &added), start)
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
    use crate::loop_definition::LoopDefinition;
    use serde_json::json;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    fn aws_key() -> String {
        format!("AKIA{}", "Z".repeat(16))
    }

    fn file_diff(path: &str, added: &[&str]) -> String {
        let mut d = format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,1 +1,{} @@\n context\n",
            added.len() + 1
        );
        for line in added {
            d.push_str(&format!("+{line}\n"));
        }
        d
    }

    #[test]
    fn aws_key_in_added_line_blocks_with_path_and_line() {
        let diff = file_diff(
            "app/config.rb",
            &["ok = 1", &format!("KEY = \"{}\"", aws_key())],
        );
        let hits = scan(&diff, &[]);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].rule, "aws_access_key");
        assert_eq!(hits[0].path, "app/config.rb");
        assert_eq!(hits[0].line, Some(3));
        assert!(hits[0].blocking);
    }

    #[test]
    fn removed_lines_are_ignored() {
        let diff = format!(
            "diff --git a/a.rb b/a.rb\n--- a/a.rb\n+++ b/a.rb\n@@ -1,2 +1,1 @@\n-KEY = \"{}\"\n keep\n",
            aws_key()
        );
        assert!(scan(&diff, &[]).is_empty());
    }

    #[test]
    fn matched_secret_text_never_appears_in_output_or_evidence() {
        let key = aws_key();
        let diff = file_diff("a.rb", &[&format!("k = '{key}'")]);
        let hits = scan(&diff, &[]);
        assert!(!format!("{hits:?}").contains(&key));
        let r = scan_result("security_scan", &hits, std::time::Instant::now());
        assert!(!r.output.contains(&key) && !r.evidence.to_string().contains(&key));
        assert!(!r.passed);
        assert_eq!(r.output.lines().next(), Some("aws_access_key at a.rb:2"));
    }

    #[test]
    fn generic_assignment_in_test_paths_is_ignored() {
        for path in [
            "spec/models/user_spec.rb",
            "test/x_test.py",
            "src/__tests__/a.ts",
            "fixtures/users.yml",
        ] {
            let diff = file_diff(path, &["password: \"password123\""]);
            assert!(scan(&diff, &[]).is_empty(), "{path}");
        }
    }

    #[test]
    fn generic_assignment_in_app_code_blocks() {
        let diff = file_diff(
            "app/services/client.rb",
            &["api_key = \"live-9f8e7d6c5b4a\""],
        );
        let hits = scan(&diff, &[]);
        assert_eq!(
            hits.iter().map(|h| h.rule).collect::<Vec<_>>(),
            ["generic_secret"]
        );
        assert!(hits[0].blocking);
    }

    #[test]
    fn placeholder_values_are_ignored() {
        let diff = file_diff(
            "config/app.yml",
            &[
                "password: \"${DB_PASSWORD}\"",
                "token = \"<your-token>\"",
                "secret: \"********\"",
                "api_key = \"xxxxxxxxxx\"",
            ],
        );
        assert!(scan(&diff, &[]).is_empty(), "{:?}", scan(&diff, &[]));
    }

    #[test]
    fn env_file_added_blocks_but_env_example_does_not() {
        let hits = scan("", &[".env".into(), "config/.env.production".into()]);
        assert_eq!(hits.len(), 2);
        assert!(hits
            .iter()
            .all(|h| h.rule == "sensitive_file" && h.blocking));
        assert!(scan(
            "",
            &[
                ".env.example".into(),
                ".env.sample".into(),
                ".env.template".into()
            ]
        )
        .is_empty());
    }

    #[test]
    fn public_key_file_does_not_block() {
        assert!(scan("", &["keys/id_rsa.pub".into()]).is_empty());
        assert_eq!(scan("", &["keys/id_ed25519".into()]).len(), 1);
        assert_eq!(scan("", &["certs/server.pem".into()]).len(), 1);
    }

    #[test]
    fn eval_is_advisory_only() {
        let diff = file_diff("app/a.rb", &["eval(params[:code])"]);
        let hits = scan(&diff, &[]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rule, "dynamic_eval");
        assert!(!hits[0].blocking);
        let r = scan_result("security_scan", &hits, std::time::Instant::now());
        assert!(r.passed);
        assert_eq!(r.output, "(+1 advisory notes)");
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

    #[test]
    fn verifier_on_real_repo_reports_hit() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        fs::write(repo.path().join("a.rb"), "x\n").unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-qm", "init"]);
        git(repo.path(), &["checkout", "-qb", "feature#7-x"]);
        fs::write(
            repo.path().join("a.rb"),
            format!("x\nk = '{}'\n", aws_key()),
        )
        .unwrap();
        fs::write(repo.path().join(".env"), "A=1\n").unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-qm", "c"]);
        let dir = tempfile::tempdir().unwrap();
        let handoff = dir.path().join("h.json");
        fs::write(
            &handoff,
            json!({"action": "fix", "target_branch": "main"}).to_string(),
        )
        .unwrap();
        let project =
            json!({"local_path": repo.path().display().to_string(), "use_worktree": false});
        let mut work = IssueWork::new("web", 7)
            .with_project_fn(move |_| Ok(project.clone()))
            .with_worktree_root_fn(|| Ok(std::path::PathBuf::from("/nonexistent-worktree-root")));
        work.handoff_path = Some(handoff);
        let v = SecurityScanVerifier::new("security_scan", work);
        let d = LoopDefinition::from_value(
            &json!({"name": "t", "version": 1, "trigger": {"type": "manual"}, "goal": {"type": "g"}}),
        )
        .unwrap();
        let r = v.verify(&LoopContext {
            iteration: 1,
            definition: &d,
            previous: None,
        });
        assert!(!r.passed, "{}", r.output);
        assert_eq!(r.evidence["gate"], "security");
        let rules: Vec<&str> = r.evidence["blocking"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["rule"].as_str().unwrap())
            .collect();
        assert_eq!(rules, ["aws_access_key", "sensitive_file"]);
        assert!(v.skip_after_failure());
    }

    fn scan_repo(repo: &Path) -> crate::loop_result::VerificationResult {
        let dir = tempfile::tempdir().unwrap();
        let handoff = dir.path().join("h.json");
        fs::write(
            &handoff,
            json!({"action": "fix", "target_branch": "main"}).to_string(),
        )
        .unwrap();
        let project = json!({"local_path": repo.display().to_string(), "use_worktree": false});
        let mut work = IssueWork::new("web", 7)
            .with_project_fn(move |_| Ok(project.clone()))
            .with_worktree_root_fn(|| Ok(std::path::PathBuf::from("/nonexistent-worktree-root")));
        work.handoff_path = Some(handoff);
        let d = LoopDefinition::from_value(
            &json!({"name": "t", "version": 1, "trigger": {"type": "manual"}, "goal": {"type": "g"}}),
        )
        .unwrap();
        SecurityScanVerifier::new("security_scan", work).verify(&LoopContext {
            iteration: 1,
            definition: &d,
            previous: None,
        })
    }

    fn base_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        fs::write(repo.path().join(".env.example"), "A=\n").unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-qm", "init"]);
        git(repo.path(), &["checkout", "-qb", "feature#7-x"]);
        repo
    }

    #[test]
    fn secrets_are_found_under_noprefix_and_quoted_paths() {
        let repo = base_repo();
        git(repo.path(), &["config", "diff.noprefix", "true"]);
        let sub = repo.path().join("dir with space");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("\u{fc}ber.rb"), format!("k = '{}'\n", aws_key())).unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-qm", "c"]);
        let r = scan_repo(repo.path());
        assert!(!r.passed, "{}", r.output);
        assert_eq!(r.evidence["blocking"][0]["rule"], "aws_access_key");
        assert_eq!(
            r.evidence["blocking"][0]["path"],
            "dir with space/\u{fc}ber.rb"
        );
    }

    #[test]
    fn renamed_env_file_blocks() {
        let repo = base_repo();
        git(repo.path(), &["mv", ".env.example", ".env"]);
        git(repo.path(), &["commit", "-qm", "c"]);
        let r = scan_repo(repo.path());
        assert!(!r.passed, "{}", r.output);
        assert_eq!(r.evidence["blocking"][0]["rule"], "sensitive_file");
        assert_eq!(r.evidence["blocking"][0]["path"], ".env");
    }
}
