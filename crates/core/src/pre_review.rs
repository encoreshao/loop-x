//! The gate-mode review of a fix before its merge request opens: one read-only
//! model call over the branch's diff (`agent_review`), and the "Pre-review"
//! section the MR description gets from it and from `security_scan`.

use crate::loop_result::{LoopResult, VerificationResult};
use crate::loop_runtime::LoopContext;
pub use crate::loop_verifiers::IssueWork;
use crate::loop_verifiers::{branch_diff, Verifier, WorkDir};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const DIFF_CAP_CHARS: usize = 60_000;
const MAX_BLOCKING_LINES: usize = 8;
const MAX_LINE_CHARS: usize = 300;
const MAX_EVIDENCE_FINDINGS: usize = 20;
const MIN_BUDGET_USD: f64 = 0.05;
const SKIP_NON_CLAUDE: &str = "review skipped: read-only review needs the claude CLI";
const PROMPT: &str = include_str!("../../../loops/mr-review/prompt.md");

pub struct ReviewReply {
    pub text: String,
    pub cost_usd: Option<f64>,
}

/// (prompt, model, budget) → the model's reply, or why it failed and what it cost.
pub type ReviewInvoker = Arc<
    dyn Fn(&str, Option<&str>, Option<f64>) -> Result<ReviewReply, (String, Option<f64>)>
        + Send
        + Sync,
>;
pub type CostLedger = Arc<Mutex<Vec<Option<f64>>>>;

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub severity: String,
    pub category: String,
    pub title: String,
    pub body: String,
    pub path: String,
    pub line: Option<i64>,
    pub suggestion: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Review {
    pub verdict: String,
    pub summary: String,
    pub findings: Vec<Finding>,
}

pub fn is_blocking(f: &Finding) -> bool {
    f.severity == "blocker"
        || (f.severity == "major" && matches!(f.category.as_str(), "correctness" | "security"))
}

pub fn parse_review(raw: &str) -> Result<Review, String> {
    let (start, end) = match (raw.find('{'), raw.rfind('}')) {
        (Some(s), Some(e)) if e > s => (s, e),
        _ => return Err("no JSON object in the review output".into()),
    };
    let v: Value =
        serde_json::from_str(&raw[start..=end]).map_err(|_| "invalid review JSON".to_string())?;
    let verdict = v
        .get("verdict")
        .and_then(Value::as_str)
        .ok_or("`verdict` is missing")?
        .to_string();
    let findings = v
        .get("findings")
        .and_then(Value::as_array)
        .ok_or("`findings` is not a list")?;
    let text = |f: &Value, k: &str| f.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Ok(Review {
        verdict,
        summary: text(&v, "summary"),
        findings: findings
            .iter()
            .map(|f| Finding {
                severity: text(f, "severity"),
                category: text(f, "category"),
                title: text(f, "title"),
                body: text(f, "body"),
                path: text(f, "path"),
                line: f.get("line").and_then(Value::as_i64),
                suggestion: text(f, "suggestion"),
            })
            .collect(),
    })
}

fn cap_diff(diff: &str) -> String {
    if diff.len() <= DIFF_CAP_CHARS {
        return diff.to_string();
    }
    let mut cut = DIFF_CAP_CHARS;
    while !diff.is_char_boundary(cut) {
        cut -= 1;
    }
    let cut = diff[..cut].rfind('\n').map_or(cut, |i| i + 1);
    format!(
        "{}\n[diff truncated: {} more chars]",
        &diff[..cut],
        diff.len() - cut
    )
}

pub fn review_prompt(handoff: &Value, diff: &str) -> String {
    let text = |k: &str| handoff.get(k).and_then(Value::as_str).unwrap_or("");
    let item = json!({
        "title": text("title"),
        "description": text("description"),
        "diff": cap_diff(diff),
    });
    PROMPT.replace("{{item_json}}", &item.to_string())
}

/// Model text on one line: a newline in a finding would let it start a line of
/// the MR description, where GitLab runs `/command` quick actions.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(s: &str, n: usize) -> String {
    let s = one_line(s);
    if s.chars().count() <= n {
        s
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

fn finding_line(f: &Finding) -> String {
    let at = match f.line {
        Some(l) => format!("{}:{l}", f.path),
        None => f.path.clone(),
    };
    let mut line = format!("[{}/{}] {at} {}", f.severity, f.category, f.title);
    if !f.suggestion.is_empty() {
        line.push_str(" — ");
        line.push_str(&f.suggestion);
    }
    clip(&line, MAX_LINE_CHARS)
}

fn finding_json(f: &Finding) -> Value {
    json!({"severity": f.severity, "category": f.category, "title": f.title,
           "path": f.path, "line": f.line, "suggestion": clip(&f.suggestion, MAX_LINE_CHARS)})
}

fn severity_rank(severity: &str) -> usize {
    ["blocker", "major", "minor", "nit"]
        .iter()
        .position(|s| *s == severity)
        .unwrap_or(4)
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

fn finding_from_json(f: &Value) -> Finding {
    let text = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Finding {
        severity: text("severity"),
        category: text("category"),
        title: text("title"),
        body: String::new(),
        path: text("path"),
        line: f.get("line").and_then(Value::as_i64),
        suggestion: text("suggestion"),
    }
}

fn review_lines(r: &VerificationResult) -> Vec<String> {
    let ev = &r.evidence;
    if ev.get("error").is_some() {
        return vec![format!(
            "- Review: could not run ({})",
            first_line(&r.output)
        )];
    }
    let verdict = one_line(ev.get("verdict").and_then(Value::as_str).unwrap_or("?"));
    let summary = clip(
        ev.get("summary").and_then(Value::as_str).unwrap_or(""),
        MAX_LINE_CHARS,
    );
    let mut lines = vec![if summary.is_empty() {
        format!("- Review: {verdict}")
    } else {
        format!("- Review: {verdict} — {summary}")
    }];
    let mut findings: Vec<Finding> = ev
        .get("findings")
        .and_then(Value::as_array)
        .map(|fs| fs.iter().map(finding_from_json).collect())
        .unwrap_or_default();
    findings.sort_by_key(|f| severity_rank(&f.severity));
    lines.extend(
        findings
            .iter()
            .take(MAX_BLOCKING_LINES)
            .map(|f| format!("- {}", finding_line(f))),
    );
    lines
}

fn security_line(r: &VerificationResult) -> String {
    let ev = &r.evidence;
    if ev.get("error").is_some() {
        return format!("- Security scan: could not run ({})", first_line(&r.output));
    }
    let blocking = ev
        .get("blocking")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if blocking.is_empty() {
        return "- Security scan: clean".into();
    }
    let hits: Vec<String> = blocking
        .iter()
        .map(|h| {
            let s = |k: &str| h.get(k).and_then(Value::as_str).unwrap_or("");
            match h.get("line").and_then(Value::as_i64) {
                Some(l) => one_line(&format!("{} at {}:{l}", s("rule"), s("path"))),
                None => one_line(&format!("{} at {}", s("rule"), s("path"))),
            }
        })
        .collect();
    format!(
        "- Security scan: {} finding(s): {}",
        hits.len(),
        hits.join(", ")
    )
}

/// The MR description's "Pre-review" section from the last verified
/// iteration, or `None` when neither gate looked at the fix.
pub fn pre_review_section(result: &LoopResult) -> Option<String> {
    let last = result
        .iterations
        .iter()
        .rev()
        .find(|i| !i.verification_results.is_empty())?;
    let gate = |name: &str| {
        last.verification_results.iter().find(|r| {
            r.evidence.get("gate").and_then(Value::as_str) == Some(name)
                && r.evidence.get("vacuous").is_none()
        })
    };
    let (review, security) = (gate("review"), gate("security"));
    if review.is_none() && security.is_none() {
        return None;
    }
    let advisory = [review, security]
        .iter()
        .flatten()
        .any(|r| r.evidence.get("mode").and_then(Value::as_str) == Some("observe"));
    let mut lines = vec![if advisory {
        "## Pre-review (advisory)".to_string()
    } else {
        "## Pre-review".to_string()
    }];
    lines.push(String::new());
    if let Some(r) = review {
        lines.extend(review_lines(r));
    }
    if let Some(r) = security {
        lines.push(security_line(r));
    }
    Some(lines.join("\n"))
}

pub struct AgentReviewVerifier {
    pub name: String,
    pub work: IssueWork,
    pub model: Option<String>,
    pub max_cost_usd: f64,
    pub issue_budget_usd: f64,
    pub cli: String,
    invoker: ReviewInvoker,
    costs: CostLedger,
}

impl AgentReviewVerifier {
    pub fn new(name: &str, work: IssueWork, invoker: ReviewInvoker, costs: CostLedger) -> Self {
        Self {
            name: name.into(),
            work,
            model: Some("opus".into()),
            max_cost_usd: 0.5,
            issue_budget_usd: f64::INFINITY,
            cli: "claude".into(),
            invoker,
            costs,
        }
    }

    pub fn with_cli(mut self, cli: &str) -> Self {
        self.cli = cli.into();
        self
    }

    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    pub fn with_budget(mut self, max_cost_usd: f64, issue_budget_usd: f64) -> Self {
        self.max_cost_usd = max_cost_usd;
        self.issue_budget_usd = issue_budget_usd;
        self
    }

    fn result(
        &self,
        passed: bool,
        output: String,
        evidence: Value,
        start: Instant,
    ) -> VerificationResult {
        VerificationResult {
            name: self.name.clone(),
            passed,
            exit_code: if evidence.get("vacuous").is_some() || evidence.get("error").is_some() {
                None
            } else {
                Some(if passed { 0 } else { 1 })
            },
            duration_ms: i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX),
            output,
            evidence,
        }
    }

    fn vacuous(&self, why: &str, start: Instant) -> VerificationResult {
        self.result(
            true,
            why.into(),
            json!({"vacuous": true, "gate": "review"}),
            start,
        )
    }

    fn error(&self, why: String, cost: Option<f64>, start: Instant) -> VerificationResult {
        self.result(
            false,
            why,
            json!({"error": true, "gate": "review", "cost_usd": cost}),
            start,
        )
    }
}

impl Verifier for AgentReviewVerifier {
    fn verify(&self, _context: &LoopContext<'_>) -> VerificationResult {
        let start = Instant::now();
        let (dir, handoff) = match self.work.resolve() {
            Ok(WorkDir::Vacuous(why)) => return self.vacuous(why, start),
            Ok(WorkDir::Ready { dir, handoff, .. }) => (dir, handoff.unwrap_or(Value::Null)),
            Err(e) => return self.error(e, None, start),
        };
        if self.cli != "claude" {
            return self.vacuous(SKIP_NON_CLAUDE, start);
        }
        let target = handoff
            .get("target_branch")
            .and_then(Value::as_str)
            .unwrap_or("main");
        let diff = match branch_diff(&dir, target) {
            Ok(d) => d,
            Err(e) => return self.error(e, None, start),
        };
        if diff.trim().is_empty() {
            return self.vacuous("empty diff - nothing to review", start);
        }
        let spent: f64 = self.costs.lock().unwrap().iter().flatten().sum();
        let budget = self.max_cost_usd.min(self.issue_budget_usd - spent);
        if budget <= MIN_BUDGET_USD {
            return self.error("no budget left for review".into(), None, start);
        }
        let reply = (self.invoker)(
            &review_prompt(&handoff, &diff),
            self.model.as_deref(),
            Some(budget),
        );
        let cost = match &reply {
            Ok(r) => r.cost_usd,
            Err((_, c)) => *c,
        };
        self.costs.lock().unwrap().push(cost);
        let text = match reply {
            Ok(r) => r.text,
            Err((e, _)) => return self.error(format!("review call failed: {e}"), cost, start),
        };
        let review = match parse_review(&text) {
            Ok(r) => r,
            Err(e) => return self.error(format!("review output unusable: {e}"), cost, start),
        };
        let mut counts = json!({"blocker": 0, "major": 0, "minor": 0, "nit": 0});
        for f in &review.findings {
            if let Some(n) = counts.get_mut(f.severity.as_str()) {
                *n = json!(n.as_i64().unwrap_or(0) + 1);
            }
        }
        let blocking: Vec<&Finding> = review.findings.iter().filter(|f| is_blocking(f)).collect();
        let output = blocking
            .iter()
            .take(MAX_BLOCKING_LINES)
            .map(|f| finding_line(f))
            .collect::<Vec<_>>()
            .join("\n");
        let evidence = json!({
            "gate": "review",
            "verdict": review.verdict,
            "summary": clip(&review.summary, MAX_LINE_CHARS),
            "counts": counts,
            "findings": review.findings.iter().take(MAX_EVIDENCE_FINDINGS).map(finding_json).collect::<Vec<_>>(),
            "cost_usd": cost,
        });
        self.result(blocking.is_empty(), output, evidence, start)
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
    use crate::loop_runtime::LoopContext;
    use crate::loop_verifiers::{build_verifiers, IssueContext, Verifier};
    use serde_json::json;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    type Calls = Arc<Mutex<Vec<(String, Option<String>, Option<f64>)>>>;

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

    struct Fixture {
        _repo: tempfile::TempDir,
        _dir: tempfile::TempDir,
        repo: std::path::PathBuf,
        handoff: std::path::PathBuf,
    }

    fn fixture(new_content: &str) -> Fixture {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        fs::write(repo.path().join("a.rb"), "old\n").unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-qm", "init"]);
        git(repo.path(), &["checkout", "-qb", "feature#7-x"]);
        fs::write(repo.path().join("a.rb"), new_content).unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-qm", "change"]);
        let dir = tempfile::tempdir().unwrap();
        let handoff = dir.path().join("h.json");
        fs::write(
            &handoff,
            json!({"action": "fix", "branch": "feature#7-x", "target_branch": "main",
                   "title": "fix: #7 the title", "description": "the description"})
            .to_string(),
        )
        .unwrap();
        Fixture {
            repo: repo.path().to_path_buf(),
            _repo: repo,
            _dir: dir,
            handoff,
        }
    }

    fn work(f: &Fixture) -> IssueWork {
        let project = json!({"local_path": f.repo.display().to_string(), "use_worktree": false});
        let mut w = IssueWork::new("web", 7)
            .with_project_fn(move |_| Ok(project.clone()))
            .with_worktree_root_fn(|| Ok(std::path::PathBuf::from("/nonexistent-worktree-root")));
        w.handoff_path = Some(f.handoff.clone());
        w
    }

    fn invoker(reply: Result<&str, &str>, cost: Option<f64>, calls: &Calls) -> ReviewInvoker {
        let calls = calls.clone();
        let reply = reply.map(str::to_string).map_err(str::to_string);
        Arc::new(
            move |prompt: &str, model: Option<&str>, budget: Option<f64>| {
                calls
                    .lock()
                    .unwrap()
                    .push((prompt.to_string(), model.map(str::to_string), budget));
                match &reply {
                    Ok(text) => Ok(ReviewReply {
                        text: text.clone(),
                        cost_usd: cost,
                    }),
                    Err(e) => Err((e.clone(), cost)),
                }
            },
        )
    }

    fn verifier(
        f: &Fixture,
        inv: ReviewInvoker,
        costs: &CostLedger,
        cli: &str,
    ) -> AgentReviewVerifier {
        AgentReviewVerifier::new("agent_review", work(f), inv, costs.clone())
            .with_cli(cli)
            .with_budget(0.5, 3.0)
    }

    fn run(v: &dyn Verifier) -> crate::loop_result::VerificationResult {
        let d = LoopDefinition::from_value(
            &json!({"name": "t", "version": 1, "trigger": {"type": "manual"}, "goal": {"type": "g"}}),
        )
        .unwrap();
        v.verify(&LoopContext {
            iteration: 1,
            definition: &d,
            previous: None,
        })
    }

    fn finding(severity: &str, category: &str) -> serde_json::Value {
        json!({"path": "a.rb", "line": 1, "severity": severity, "category": category,
               "title": format!("{severity} {category} problem"), "body": "b", "suggestion": "do x"})
    }

    fn reply(findings: Vec<serde_json::Value>, verdict: &str) -> String {
        json!({"summary": "s", "verdict": verdict, "overview": [], "risks": [], "findings": findings}).to_string()
    }

    #[test]
    fn blocker_finding_fails_with_one_line_per_blocking_finding() {
        let f = fixture("new\n");
        let calls = Calls::default();
        let text = reply(
            vec![
                finding("blocker", "correctness"),
                finding("nit", "maintainability"),
            ],
            "request_changes",
        );
        let r = run(&verifier(
            &f,
            invoker(Ok(&text), Some(0.1), &calls),
            &CostLedger::default(),
            "claude",
        ));
        assert!(!r.passed);
        assert_eq!(r.output.lines().count(), 1, "{}", r.output);
        assert!(
            r.output
                .starts_with("[blocker/correctness] a.rb:1 blocker correctness problem"),
            "{}",
            r.output
        );
        assert_eq!(r.evidence["gate"], "review");
        assert_eq!(
            r.evidence["counts"],
            json!({"blocker": 1, "major": 0, "minor": 0, "nit": 1})
        );
        let (prompt, model, _) = calls.lock().unwrap()[0].clone();
        assert!(
            prompt.contains("+new") && prompt.contains("fix: #7 the title"),
            "{prompt}"
        );
        assert!(!prompt.contains("{{item_json}}"));
        assert_eq!(model.as_deref(), Some("opus"));
    }

    #[test]
    fn major_style_finding_is_advisory() {
        let f = fixture("new\n");
        let text = reply(vec![finding("major", "maintainability")], "comment");
        let r = run(&verifier(
            &f,
            invoker(Ok(&text), None, &Calls::default()),
            &CostLedger::default(),
            "claude",
        ));
        assert!(r.passed, "{}", r.output);
    }

    #[test]
    fn major_security_finding_blocks() {
        let f = fixture("new\n");
        let text = reply(vec![finding("major", "security")], "request_changes");
        let r = run(&verifier(
            &f,
            invoker(Ok(&text), None, &Calls::default()),
            &CostLedger::default(),
            "claude",
        ));
        assert!(!r.passed);
    }

    #[test]
    fn malformed_review_output_is_an_error_result() {
        let f = fixture("new\n");
        let r = run(&verifier(
            &f,
            invoker(Ok("looks good!"), None, &Calls::default()),
            &CostLedger::default(),
            "claude",
        ));
        assert!(!r.passed);
        assert_eq!(r.evidence["error"], json!(true));
        assert_eq!(r.evidence["gate"], "review");
    }

    #[test]
    fn invoker_error_is_an_error_result_and_its_cost_is_recorded() {
        let f = fixture("new\n");
        let costs = CostLedger::default();
        let r = run(&verifier(
            &f,
            invoker(Err("timed out"), Some(0.2), &Calls::default()),
            &costs,
            "claude",
        ));
        assert!(!r.passed && r.evidence["error"] == json!(true));
        assert!(r.output.contains("timed out"));
        assert_eq!(*costs.lock().unwrap(), vec![Some(0.2)]);
    }

    #[test]
    fn non_claude_cli_is_vacuous() {
        let f = fixture("new\n");
        let calls = Calls::default();
        let r = run(&verifier(
            &f,
            invoker(Ok("{}"), None, &calls),
            &CostLedger::default(),
            "codex",
        ));
        assert!(r.passed);
        assert_eq!(
            r.output,
            "review skipped: read-only review needs the claude CLI"
        );
        assert_eq!(r.evidence["vacuous"], json!(true));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn review_budget_is_capped_by_remaining_issue_budget() {
        let f = fixture("new\n");
        let calls = Calls::default();
        let costs: CostLedger = Arc::new(Mutex::new(vec![Some(2.8)]));
        let text = reply(vec![], "approve");
        let r = run(&verifier(
            &f,
            invoker(Ok(&text), Some(0.1), &calls),
            &costs,
            "claude",
        ));
        assert!(r.passed, "{}", r.output);
        let budget = calls.lock().unwrap()[0].2.unwrap();
        assert!((budget - 0.2).abs() < 1e-9, "{budget}");
        assert_eq!(costs.lock().unwrap().len(), 2);
    }

    #[test]
    fn exhausted_budget_is_an_error_without_a_call() {
        let f = fixture("new\n");
        let calls = Calls::default();
        let costs: CostLedger = Arc::new(Mutex::new(vec![Some(2.99)]));
        let r = run(&verifier(
            &f,
            invoker(Ok("{}"), None, &calls),
            &costs,
            "claude",
        ));
        assert!(!r.passed && r.output.contains("no budget left for review"));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn diff_over_cap_is_truncated_with_marker() {
        let handoff = json!({"title": "t", "description": "d"});
        let diff = "x\n".repeat(DIFF_CAP_CHARS);
        let prompt = review_prompt(&handoff, &diff);
        assert!(prompt.contains("[diff truncated: "), "marker missing");
        let capped = cap_diff(&diff);
        assert!(capped.len() < DIFF_CAP_CHARS + 50, "{}", capped.len());
        assert!(capped.ends_with(&format!(
            "[diff truncated: {} more chars]",
            diff.len() - DIFF_CAP_CHARS
        )));
    }

    #[test]
    fn build_verifiers_requires_an_invoker_for_agent_review() {
        let spec = [json!({"name": "agent_review", "type": "agent_review"})];
        let issue = IssueContext::new("web", 7, 30.0);
        let err = build_verifiers(&spec, None, Some(&issue), "gate")
            .err()
            .unwrap();
        assert!(err.0.contains("agent_review"), "{}", err.0);
    }

    fn gate_result(name: &str, output: &str, evidence: serde_json::Value) -> VerificationResult {
        VerificationResult {
            name: name.into(),
            passed: true,
            exit_code: Some(0),
            duration_ms: 1,
            output: output.into(),
            evidence,
        }
    }

    fn loop_result(results: Vec<VerificationResult>) -> crate::loop_result::LoopResult {
        crate::loop_result::LoopResult::new(
            "l",
            "r",
            "gitlab-issue",
            crate::loop_state::LoopState::Completed,
            vec![crate::loop_result::IterationResult {
                iteration: 1,
                state: crate::loop_state::LoopState::Completed,
                verification_results: results,
                budget: json!({}),
                progressed: true,
            }],
            "completed",
        )
    }

    #[test]
    fn pre_review_section_lists_verdict_findings_and_scan() {
        let review = gate_result(
            "agent_review",
            "",
            json!({"gate": "review", "verdict": "request_changes", "summary": "one real bug",
            "mode": "observe", "observed_passed": false,
            "findings": [
                {"severity": "nit", "category": "maintainability", "title": "naming", "path": "b.rb", "line": 9, "suggestion": ""},
                {"severity": "blocker", "category": "correctness", "title": "nil crash", "path": "a.rb", "line": 1, "suggestion": "guard it"}
            ]}),
        );
        let scan = gate_result(
            "security_scan",
            "",
            json!({"gate": "security", "mode": "observe", "advisory": [],
                   "blocking": [{"rule": "aws_access_key", "path": "a.rb", "line": 2}]}),
        );
        let section = pre_review_section(&loop_result(vec![review, scan])).unwrap();
        assert_eq!(
            section,
            "## Pre-review (advisory)\n\n\
             - Review: request_changes — one real bug\n\
             - [blocker/correctness] a.rb:1 nil crash — guard it\n\
             - [nit/maintainability] b.rb:9 naming\n\
             - Security scan: 1 finding(s): aws_access_key at a.rb:2"
        );
    }

    #[test]
    fn model_text_cannot_start_a_line_in_the_mr_description() {
        let review = gate_result(
            "agent_review",
            "",
            json!({"gate": "review", "verdict": "comment", "summary": "fine\n/approve",
                   "findings": [{"severity": "minor", "category": "tests", "title": "t\n/merge",
                                 "path": "a.rb", "line": 1, "suggestion": "x\r\n/close"}]}),
        );
        let section = pre_review_section(&loop_result(vec![review])).unwrap();
        for line in section.lines().skip(1) {
            assert!(
                line.is_empty() || line.starts_with("- "),
                "{line:?} in {section}"
            );
        }
        assert!(
            section.contains("- Review: comment — fine /approve"),
            "{section}"
        );
    }

    #[test]
    fn pre_review_section_says_clean_scan() {
        let scan = gate_result(
            "security_scan",
            "",
            json!({"gate": "security", "blocking": [], "advisory": []}),
        );
        assert_eq!(
            pre_review_section(&loop_result(vec![scan])).unwrap(),
            "## Pre-review\n\n- Security scan: clean"
        );
    }

    #[test]
    fn pre_review_section_is_none_when_all_vacuous() {
        let skipped = gate_result(
            "agent_review",
            "skipped: an earlier check failed",
            json!({"vacuous": true, "skipped": true}),
        );
        let vacuous = gate_result(
            "security_scan",
            "no fix handoff - nothing to verify",
            json!({"vacuous": true, "gate": "security"}),
        );
        assert_eq!(
            pre_review_section(&loop_result(vec![skipped, vacuous])),
            None
        );
        assert_eq!(pre_review_section(&loop_result(vec![])), None);
    }
}
