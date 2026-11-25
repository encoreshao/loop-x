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
    let criteria: Vec<Value> = acceptance_criteria(handoff)
        .iter()
        .map(|c| json!({"id": c.id, "statement": c.statement, "test": c.test, "reason": c.reason}))
        .collect();
    let mut item = json!({
        "title": text("title"),
        "description": text("description"),
        "acceptance_criteria": criteria,
        "diff": cap_diff(diff),
    });
    if let Some(failures) = handoff
        .pointer("/tester/failures")
        .and_then(Value::as_array)
        .filter(|f| !f.is_empty())
    {
        item["unresolved_test_failures"] = json!(failures);
    }
    PROMPT.replace("{{item_json}}", &item.to_string())
}

/// Model text on one line: a newline in a finding would let it start a line of
/// the MR description, where GitLab runs `/command` quick actions.
pub(crate) fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn clip(s: &str, n: usize) -> String {
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

pub(crate) fn security_line(r: &VerificationResult) -> String {
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

const MAX_UI_ITEMS: usize = 5;

/// The browser check's line and, for projects that opted in, the
/// accessibility line. Every value is flattened, so no line can start a
/// quick action.
pub(crate) fn ui_lines(r: &VerificationResult) -> Vec<String> {
    let ev = &r.evidence;
    if ev.get("error").is_some_and(crate::loop_serialize::truthy) {
        return vec![clip(
            &format!("- Browser check: could not run ({})", first_line(&r.output)),
            MAX_LINE_CHARS,
        )];
    }
    let text = |v: &Value, k: &str| one_line(v.get(k).and_then(Value::as_str).unwrap_or(""));
    let pages = ev
        .get("pages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let problems = |p: &Value| {
        p.get("problems")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let mut blocking = Vec::new();
    for p in &pages {
        for x in problems(p).iter().filter(|x| x["blocking"] == json!(true)) {
            blocking.push(format!(
                "{}: {} {}",
                text(p, "label"),
                text(x, "rule"),
                text(x, "detail")
            ));
        }
    }
    let browser = if blocking.is_empty() {
        let clean = pages.iter().filter(|p| p["status"] == json!("ok")).count();
        let mut parts = vec![format!("{clean} page(s) loaded clean")];
        for p in pages.iter().filter(|p| p["status"] == json!("skipped")) {
            let why = problems(p)
                .first()
                .map(|x| text(x, "detail"))
                .unwrap_or_default();
            parts.push(format!("{} skipped: {why}", text(p, "label")));
        }
        parts.join("; ")
    } else {
        blocking.truncate(MAX_UI_ITEMS);
        blocking.join("; ")
    };
    let mut lines = vec![clip(
        &format!("- Browser check: {browser}"),
        MAX_LINE_CHARS * 2,
    )];
    let a11y = ev.get("a11y").cloned().unwrap_or(Value::Null);
    if a11y.get("enabled") == Some(&json!(true)) {
        let unavailable = text(&a11y, "baseline").starts_with("unavailable");
        let note = if unavailable {
            " (baseline unavailable)"
        } else {
            ""
        };
        let new = a11y
            .get("new")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let count = a11y
            .get("new_count")
            .and_then(Value::as_u64)
            .unwrap_or(new.len() as u64);
        let line = if count == 0 {
            let existing = a11y.get("existing").and_then(Value::as_u64).unwrap_or(0);
            if unavailable {
                format!("- Accessibility (WCAG 2.2 AA): {existing} found; baseline unavailable, none judged new")
            } else {
                format!("- Accessibility (WCAG 2.2 AA): no new violations ({existing} existing)")
            }
        } else {
            let items: Vec<String> = new
                .iter()
                .take(MAX_UI_ITEMS)
                .map(|v| {
                    format!(
                        "{} ({}) on {}",
                        text(v, "id"),
                        text(v, "impact"),
                        text(v, "page")
                    )
                })
                .collect();
            format!("- Accessibility: {count} new: {}{note}", items.join("; "))
        };
        lines.push(clip(&line, MAX_LINE_CHARS * 2));
    }
    lines
}

const MAX_DEPENDENCY_ITEMS: usize = 10;

/// Built from evidence only: a tool's own error text can carry local paths.
pub(crate) fn dependency_line(r: &VerificationResult) -> String {
    use crate::dependency_audit::ecosystem_label;
    let ev = &r.evidence;
    let list = |k: &str| {
        ev.get(k)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let text = |v: &Value, k: &str| one_line(v.get(k).and_then(Value::as_str).unwrap_or(""));
    let mut parts = Vec::new();
    let found: Vec<Value> = list("blocking")
        .into_iter()
        .chain(list("advisory"))
        .collect();
    let ran: Vec<&str> = ev
        .get("tools")
        .and_then(Value::as_object)
        .map(|t| {
            t.iter()
                .filter(|(_, s)| s.as_str() == Some("ran"))
                .map(|(k, _)| ecosystem_label(k))
                .collect()
        })
        .unwrap_or_default();
    if ev.get("error").is_some_and(crate::loop_serialize::truthy) {
        let errors: Vec<String> = list("errors")
            .iter()
            .map(|e| format!("{}: {}", text(e, "tool"), text(e, "reason")))
            .collect();
        parts.push(if errors.is_empty() {
            "could not run".to_string()
        } else {
            format!("could not run ({})", errors.join(", "))
        });
    } else if !found.is_empty() {
        let mut items: Vec<String> = found
            .iter()
            .take(MAX_DEPENDENCY_ITEMS)
            .map(|a| one_line(&format!("{} in {}", text(a, "id"), text(a, "package"))))
            .collect();
        if found.len() > MAX_DEPENDENCY_ITEMS {
            items.push("…".into());
        }
        parts.push(format!(
            "{} advisory(ies): {}",
            found.len(),
            items.join(", ")
        ));
    } else if !ran.is_empty() {
        parts.push(format!("clean ({})", ran.join(", ")));
    }
    let skipped: Vec<String> = list("not_audited")
        .iter()
        .map(|n| {
            format!(
                "{} ({})",
                ecosystem_label(&text(n, "ecosystem")),
                text(n, "reason")
            )
        })
        .collect();
    if !skipped.is_empty() {
        parts.push(format!("not audited: {}", skipped.join(", ")));
    }
    let config: Vec<String> = list("config_changed")
        .iter()
        .filter_map(|p| p.as_str().map(one_line))
        .collect();
    if !config.is_empty() {
        parts.push(format!(
            "the fix changed audit configuration: {}",
            config.join(", ")
        ));
    }
    if parts.is_empty() {
        parts.push("clean".into());
    }
    format!("- Dependency audit: {}", parts.join("; "))
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
    let (review, dependency, security) = (gate("review"), gate("dependency"), gate("security"));
    let ui = gate("ui");
    if review.is_none() && dependency.is_none() && security.is_none() && ui.is_none() {
        return None;
    }
    let advisory = [review, ui, dependency, security]
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
    if let Some(r) = ui {
        lines.extend(ui_lines(r));
    }
    if let Some(r) = dependency {
        lines.push(dependency_line(r));
    }
    if let Some(r) = security {
        lines.push(security_line(r));
    }
    Some(lines.join("\n"))
}

const MAX_CRITERIA: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct Criterion {
    pub id: String,
    pub statement: String,
    pub test: Option<String>,
    pub reason: String,
}

/// The fix handoff's `acceptance_criteria`, read leniently: a missing or
/// malformed list never makes the handoff invalid, it only yields fewer criteria.
pub fn acceptance_criteria(handoff: &Value) -> Vec<Criterion> {
    let Some(list) = handoff.get("acceptance_criteria").and_then(Value::as_array) else {
        return Vec::new();
    };
    let text = |c: &Value, k: &str| {
        clip(
            c.get(k).and_then(Value::as_str).unwrap_or(""),
            MAX_LINE_CHARS,
        )
    };
    let mut taken: std::collections::HashSet<String> = list
        .iter()
        .map(|c| text(c, "id"))
        .filter(|id| !id.is_empty())
        .collect();
    let mut fallback_id = |i: usize| {
        let id = (i + 1..)
            .map(|n| format!("AC{n}"))
            .find(|id| !taken.contains(id))
            .expect("an unbounded range");
        taken.insert(id.clone());
        id
    };
    list.iter()
        .enumerate()
        .filter(|(_, c)| c.is_object())
        .map(|(i, c)| Criterion {
            id: Some(text(c, "id"))
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| fallback_id(i)),
            statement: text(c, "statement"),
            test: Some(text(c, "test").replace('`', "'")).filter(|t| {
                !matches!(
                    t.trim().to_ascii_lowercase().as_str(),
                    "" | "null" | "none" | "n/a" | "na" | "-"
                )
            }),
            reason: text(c, "reason"),
        })
        .filter(|c| !c.id.is_empty() && !c.statement.is_empty())
        .take(MAX_CRITERIA)
        .collect()
}

/// A criterion the agent left without a test and without saying why.
fn uncovered_criteria(handoff: &Value) -> Vec<Finding> {
    acceptance_criteria(handoff)
        .into_iter()
        .filter(|c| c.test.is_none() && c.reason.is_empty())
        .map(|c| Finding {
            severity: "major".into(),
            category: "tests".into(),
            title: format!("acceptance criterion {} has no test", c.id),
            body: c.statement,
            path: String::new(),
            line: None,
            suggestion: "add a test that proves it, or say why none is possible".into(),
        })
        .collect()
}

pub fn acceptance_section(handoff: &Value) -> Option<String> {
    let criteria = acceptance_criteria(handoff);
    if criteria.is_empty() {
        return None;
    }
    let lines: Vec<String> = criteria
        .iter()
        .map(|c| match &c.test {
            Some(test) => format!("- [x] {}: {} (test: `{test}`)", c.id, c.statement),
            None if c.reason.is_empty() => format!("- [ ] {}: {} (no test)", c.id, c.statement),
            None => format!("- [ ] {}: {} (no test: {})", c.id, c.statement, c.reason),
        })
        .collect();
    Some(format!("## Acceptance criteria\n\n{}", lines.join("\n")))
}

pub struct AgentReviewVerifier {
    pub name: String,
    pub work: IssueWork,
    pub model: Option<String>,
    pub max_cost_usd: f64,
    pub issue_budget_usd: f64,
    pub cli: String,
    /// A review that could not run passes (recorded as observe-mode), while
    /// blocking findings still fail.
    pub errors_advisory: bool,
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
            errors_advisory: false,
            invoker,
            costs,
        }
    }

    pub fn with_errors_advisory(mut self, advisory: bool) -> Self {
        self.errors_advisory = advisory;
        self
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
        let mut evidence = json!({"error": true, "gate": "review", "cost_usd": cost});
        if self.errors_advisory {
            evidence["observed_passed"] = json!(false);
            evidence["mode"] = json!("observe");
        }
        self.result(self.errors_advisory, why, evidence, start)
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
        let mut review = match parse_review(&text) {
            Ok(r) => r,
            Err(e) => return self.error(format!("review output unusable: {e}"), cost, start),
        };
        review.findings.splice(0..0, uncovered_criteria(&handoff));
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
    fn an_advisory_review_error_passes_but_blocking_findings_still_fail() {
        let f = fixture("new\n");
        let costs = CostLedger::default();
        let r = run(&verifier(
            &f,
            invoker(Err("timed out"), Some(0.2), &Calls::default()),
            &costs,
            "claude",
        )
        .with_errors_advisory(true));
        assert!(r.passed, "{}", r.output);
        assert_eq!(r.evidence["observed_passed"], json!(false));
        assert_eq!(r.evidence["mode"], "observe");

        let text = reply(vec![finding("blocker", "correctness")], "request_changes");
        let r = run(&verifier(
            &f,
            invoker(Ok(&text), Some(0.1), &Calls::default()),
            &costs,
            "claude",
        )
        .with_errors_advisory(true));
        assert!(!r.passed, "{}", r.output);
    }

    #[test]
    fn review_prompt_shows_the_testers_unresolved_failures_only_when_there_are_some() {
        let mut handoff = fix_with(json!([]));
        assert!(!review_prompt(&handoff, "+x\n").contains("\"unresolved_test_failures\":"));
        handoff["tester"] = json!({"verdict": "fail", "failures": [
            {"criterion": "AC2", "problem": "empty list raises", "evidence": "NoMethodError"}]});
        let prompt = review_prompt(&handoff, "+x\n");
        assert!(prompt.contains("\"unresolved_test_failures\":"), "{prompt}");
        assert!(prompt.contains("empty list raises"), "{prompt}");
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

    fn ui_result(evidence: serde_json::Value) -> VerificationResult {
        gate_result("ui_check", "", evidence)
    }

    #[test]
    fn pre_review_section_has_browser_and_accessibility_lines() {
        let clean = ui_result(json!({"gate": "ui", "mode": "observe", "blocking": [],
            "pages": [{"label": "Orders", "path": "/orders", "status": "ok", "problems": []},
                      {"label": "Settings", "path": "/settings", "status": "skipped",
                       "problems": [{"rule": "skipped", "detail": "redirected to /users/sign_in (sign-in?)", "blocking": false}]}],
            "a11y": {"enabled": true, "baseline": "measured", "new": [], "new_count": 0, "existing": 2}}));
        assert_eq!(
            pre_review_section(&loop_result(vec![clean])).unwrap(),
            "## Pre-review (advisory)\n\n\
             - Browser check: 1 page(s) loaded clean; Settings skipped: redirected to /users/sign_in (sign-in?)\n\
             - Accessibility (WCAG 2.2 AA): no new violations (2 existing)"
        );
        let problems = ui_result(
            json!({"gate": "ui", "blocking": [{"rule": "page-error", "page": "Orders", "kind": "smoke"}],
            "pages": [{"label": "Orders", "path": "/orders", "status": "problems",
                       "problems": [{"rule": "page-error", "detail": "TypeError: x", "blocking": true},
                                    {"rule": "console-error", "detail": "noise", "blocking": false}]}],
            "a11y": {"enabled": true, "baseline": "unavailable (the worktree has uncommitted changes)",
                     "new": [{"page": "Orders", "id": "label", "impact": "critical", "targets": ["#q"]},
                             {"page": "Orders", "id": "region", "impact": "moderate", "targets": ["main"]}],
                     "new_count": 2, "existing": 0}}),
        );
        assert_eq!(
            pre_review_section(&loop_result(vec![problems])).unwrap(),
            "## Pre-review\n\n\
             - Browser check: Orders: page-error TypeError: x\n\
             - Accessibility: 2 new: label (critical) on Orders; region (moderate) on Orders (baseline unavailable)"
        );
        let no_a11y = ui_result(
            json!({"gate": "ui", "blocking": [], "a11y": {"enabled": false},
            "pages": [{"label": "A", "path": "/", "status": "ok", "problems": []}]}),
        );
        assert_eq!(
            pre_review_section(&loop_result(vec![no_a11y])).unwrap(),
            "## Pre-review\n\n- Browser check: 1 page(s) loaded clean"
        );
        let vacuous = ui_result(json!({"gate": "ui", "vacuous": true, "undeclared_ui": true}));
        assert!(pre_review_section(&loop_result(vec![vacuous])).is_none());
        let error = VerificationResult {
            passed: false,
            exit_code: None,
            ..ui_result(json!({"gate": "ui", "error": true}))
        };
        let error = VerificationResult {
            output: "dev server did not start: boom".into(),
            ..error
        };
        assert_eq!(
            pre_review_section(&loop_result(vec![error])).unwrap(),
            "## Pre-review\n\n- Browser check: could not run (dev server did not start: boom)"
        );
    }

    #[test]
    fn an_unavailable_baseline_says_nothing_was_judged_new() {
        let r = ui_result(json!({"gate": "ui", "blocking": [],
            "pages": [{"label": "A", "path": "/", "status": "ok", "problems": []}],
            "a11y": {"enabled": true, "baseline": "unavailable (the worktree has uncommitted changes)",
                     "new": [], "new_count": 0, "existing": 3}}));
        assert_eq!(
            pre_review_section(&loop_result(vec![r])).unwrap(),
            "## Pre-review\n\n- Browser check: 1 page(s) loaded clean\n\
             - Accessibility (WCAG 2.2 AA): 3 found; baseline unavailable, none judged new"
        );
    }

    #[test]
    fn pre_review_lines_never_start_a_quick_action() {
        let hostile = ui_result(
            json!({"gate": "ui", "blocking": [{"rule": "page-error", "page": "x", "kind": "smoke"}],
            "pages": [{"label": "\n/close", "path": "/", "status": "problems",
                       "problems": [{"rule": "page-error", "detail": "a\n/merge\r\n/approve", "blocking": true}]},
                      {"label": "/label ~x\n", "path": "/b", "status": "skipped",
                       "problems": [{"rule": "skipped", "detail": "\n/unassign", "blocking": false}]}],
            "a11y": {"enabled": true, "baseline": "unavailable (\n/close)",
                     "new": [{"page": "\n/merge", "id": "x\n/close", "impact": "serious\n/approve", "targets": []}],
                     "new_count": 1, "existing": 0}}),
        );
        let section = pre_review_section(&loop_result(vec![hostile])).unwrap();
        for line in section.lines().skip(1) {
            assert!(
                line.is_empty() || line.starts_with("- "),
                "{line:?} in {section}"
            );
        }
        assert_eq!(section.lines().count(), 4, "{section}");
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
    fn pre_review_section_has_a_dependency_audit_line() {
        let deps = gate_result(
            "dependency_audit",
            "",
            json!({"gate": "dependency", "mode": "observe", "tools": {"npm": "ran"},
                   "blocking": [{"package": "left-pad", "version": "1.0.1",
                                 "id": "GHSA-aaaa-test-0001", "severity": "high"}],
                   "advisory": [{"package": "is\nodd", "version": "2.0.0",
                                 "id": "GHSA-bbbb-test-0002\n/close", "severity": "moderate"}]}),
        );
        let scan = gate_result(
            "security_scan",
            "",
            json!({"gate": "security", "blocking": [], "advisory": []}),
        );
        assert_eq!(
            pre_review_section(&loop_result(vec![deps, scan])).unwrap(),
            "## Pre-review (advisory)\n\n\
             - Dependency audit: 2 advisory(ies): GHSA-aaaa-test-0001 in left-pad, \
             GHSA-bbbb-test-0002 /close in is odd\n\
             - Security scan: clean"
        );
        let clean = gate_result(
            "dependency_audit",
            "",
            json!({"gate": "dependency", "blocking": [], "advisory": []}),
        );
        assert_eq!(
            pre_review_section(&loop_result(vec![clean])).unwrap(),
            "## Pre-review\n\n- Dependency audit: clean"
        );
        let error = gate_result(
            "dependency_audit",
            "npm timed out after 120 s\nmore",
            json!({"gate": "dependency", "error": true, "blocking": [], "advisory": [],
                   "errors": [{"tool": "npm", "reason": "timed out", "detail": "npm timed out after 120 s"}]}),
        );
        assert_eq!(
            pre_review_section(&loop_result(vec![error])).unwrap(),
            "## Pre-review\n\n- Dependency audit: could not run (npm: timed out)"
        );
        let missing = gate_result(
            "dependency_audit",
            "npm is not installed - dependency audit skipped",
            json!({"gate": "dependency", "vacuous": true}),
        );
        assert_eq!(pre_review_section(&loop_result(vec![missing])), None);
    }

    #[test]
    fn dependency_error_line_never_quotes_tool_stderr() {
        let error = gate_result(
            "dependency_audit",
            "could not read npm output (exit 1): /Users/someone/app/x.js boom",
            json!({"gate": "dependency", "error": true, "blocking": [], "advisory": [],
                   "errors": [{"tool": "npm", "reason": "unreadable output",
                               "detail": "could not read npm output (exit 1): ~/app/x.js boom"}]}),
        );
        assert_eq!(
            pre_review_section(&loop_result(vec![error])).unwrap(),
            "## Pre-review\n\n- Dependency audit: could not run (npm: unreadable output)"
        );
        let legacy = gate_result(
            "dependency_audit",
            "/Users/someone/secret path",
            json!({"gate": "dependency", "error": true}),
        );
        assert_eq!(
            pre_review_section(&loop_result(vec![legacy])).unwrap(),
            "## Pre-review\n\n- Dependency audit: could not run"
        );
    }

    #[test]
    fn dependency_line_names_what_was_and_was_not_audited() {
        let partial = gate_result(
            "dependency_audit",
            "",
            json!({"gate": "dependency", "blocking": [], "advisory": [],
                   "tools": {"npm": "ran", "ruby": "missing"},
                   "not_audited": [{"ecosystem": "ruby", "reason": "bundle-audit not installed"}],
                   "config_changed": [".bundler-audit.yml"]}),
        );
        assert_eq!(
            pre_review_section(&loop_result(vec![partial])).unwrap(),
            "## Pre-review\n\n- Dependency audit: clean (npm); not audited: Ruby (bundle-audit not installed); \
             the fix changed audit configuration: .bundler-audit.yml"
        );
        let config_only = gate_result(
            "dependency_audit",
            "",
            json!({"gate": "dependency", "blocking": [], "advisory": [], "tools": {},
                   "config_changed": ["web/.npmrc\n/close"]}),
        );
        assert_eq!(
            pre_review_section(&loop_result(vec![config_only])).unwrap(),
            "## Pre-review\n\n- Dependency audit: the fix changed audit configuration: web/.npmrc /close"
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

    fn fix_with(criteria: serde_json::Value) -> serde_json::Value {
        json!({"action": "fix", "branch": "feature#7-x", "target_branch": "main",
               "title": "fix: #7 x", "acceptance_criteria": criteria})
    }

    fn with_criteria(f: &Fixture, criteria: serde_json::Value) {
        let mut h: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&f.handoff).unwrap()).unwrap();
        h["acceptance_criteria"] = criteria;
        fs::write(&f.handoff, h.to_string()).unwrap();
    }

    fn approve() -> String {
        reply(vec![], "approve")
    }

    #[test]
    fn uncovered_criterion_is_a_major_tests_finding() {
        let f = fixture("new\n");
        with_criteria(
            &f,
            json!([{"id": "AC1", "statement": "WHEN x the system shall y", "test": null}]),
        );
        let r = run(&verifier(
            &f,
            invoker(Ok(&approve()), None, &Calls::default()),
            &CostLedger::default(),
            "claude",
        ));
        assert!(r.passed, "advisory, never blocking: {}", r.output);
        assert_eq!(r.evidence["counts"]["major"], json!(1));
        assert_eq!(
            r.evidence["findings"][0]["title"],
            "acceptance criterion AC1 has no test"
        );
        assert_eq!(r.evidence["findings"][0]["category"], "tests");
    }

    #[test]
    fn criterion_with_reason_is_not_a_finding() {
        let f = fixture("new\n");
        with_criteria(
            &f,
            json!([{"id": "AC1", "statement": "s", "test": null, "reason": "visual only"}]),
        );
        let r = run(&verifier(
            &f,
            invoker(Ok(&approve()), None, &Calls::default()),
            &CostLedger::default(),
            "claude",
        ));
        assert_eq!(r.evidence["counts"]["major"], json!(0));
    }

    #[test]
    fn no_criteria_no_synthetic_findings() {
        let f = fixture("new\n");
        let r = run(&verifier(
            &f,
            invoker(Ok(&approve()), None, &Calls::default()),
            &CostLedger::default(),
            "claude",
        ));
        assert_eq!(r.evidence["findings"], json!([]));
    }

    #[test]
    fn review_prompt_includes_criteria() {
        let handoff = fix_with(
            json!([{"id": "AC1", "statement": "WHEN a the system shall b", "test": "t1"}]),
        );
        let prompt = review_prompt(&handoff, "+x\n");
        assert!(prompt.contains("WHEN a the system shall b"), "{prompt}");
        assert!(prompt.contains("\"acceptance_criteria\""), "{prompt}");
    }

    #[test]
    fn placeholder_test_strings_count_as_no_test() {
        let handoff = fix_with(json!([
            {"id": "AC1", "statement": "s1", "test": "null"},
            {"id": "AC2", "statement": "s2", "test": "None"},
            {"id": "AC3", "statement": "s3", "test": "n/a"},
            {"id": "AC4", "statement": "s4", "test": " - "}
        ]));
        let criteria = acceptance_criteria(&handoff);
        assert_eq!(criteria.len(), 4);
        assert!(criteria.iter().all(|c| c.test.is_none()), "{criteria:?}");
        assert_eq!(uncovered_criteria(&handoff).len(), 4);
    }

    #[test]
    fn criteria_render_as_a_checklist() {
        let handoff = fix_with(json!([
            {"id": "AC1", "statement": "WHEN 25 items are paged by 10 the system shall show 3 pages",
             "test": "tests/test_pages.py::test_partial_last_page_counts"},
            {"id": "AC2", "statement": "The system shall keep the pager accessible", "test": null,
             "reason": "visual only, checked by hand"}
        ]));
        assert_eq!(
            acceptance_section(&handoff).unwrap(),
            "## Acceptance criteria\n\n\
             - [x] AC1: WHEN 25 items are paged by 10 the system shall show 3 pages (test: `tests/test_pages.py::test_partial_last_page_counts`)\n\
             - [ ] AC2: The system shall keep the pager accessible (no test: visual only, checked by hand)"
        );
    }

    #[test]
    fn missing_criteria_render_nothing_and_never_invalidate_the_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.json");
        let mut plain = fix_with(json!(null));
        plain.as_object_mut().unwrap().remove("acceptance_criteria");
        for handoff in [plain, fix_with(json!("garbage")), fix_with(json!([1, 2]))] {
            assert_eq!(acceptance_section(&handoff), None, "{handoff}");
            fs::write(&path, handoff.to_string()).unwrap();
            assert!(
                crate::issue_runner::read_handoff(&path, "7").is_some(),
                "{handoff}"
            );
        }
    }

    #[test]
    fn criteria_text_is_flattened() {
        let handoff = fix_with(json!([
            {"id": "AC1\n/merge", "statement": "WHEN x\n/approve the system shall y",
             "test": "t\n/close"}
        ]));
        let section = acceptance_section(&handoff).unwrap();
        for line in section.lines().skip(1) {
            assert!(line.is_empty() || line.starts_with("- ["), "{line:?}");
        }
    }

    #[test]
    fn a_missing_id_never_reuses_one_the_agent_gave() {
        let handoff = fix_with(json!([
            {"id": "AC2", "statement": "s1", "test": "t"},
            {"statement": "s2", "test": "t"},
            {"statement": "s3", "test": "t"}
        ]));
        let ids: Vec<String> = acceptance_criteria(&handoff)
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, ["AC2", "AC3", "AC4"]);
    }

    #[test]
    fn malformed_entries_are_dropped_and_list_is_capped() {
        let mut list = vec![
            json!("a string"),
            json!({"id": "", "statement": "s", "test": "t"}),
            json!({"id": "X", "statement": "", "test": "t"}),
            json!({"id": "N", "statement": "numeric test", "test": 5}),
        ];
        for i in 1..=10 {
            list.push(json!({"id": format!("AC{i}"), "statement": "s", "test": "t"}));
        }
        let criteria = acceptance_criteria(&fix_with(json!(list)));
        assert_eq!(criteria.len(), 8);
        assert_eq!(criteria[0].id, "AC11");
        assert_eq!(criteria[1].id, "N");
        assert_eq!(criteria[1].test, None);
        assert_eq!(criteria[2].id, "AC1");
    }

    #[test]
    fn a_non_string_id_falls_back_to_its_position() {
        let criteria = acceptance_criteria(&fix_with(json!([
            {"id": "AC1", "statement": "s1", "test": "t"},
            {"id": 1, "statement": "s2", "test": "t"},
            {"statement": "s3", "test": "t"}
        ])));
        let ids: Vec<&str> = criteria.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["AC1", "AC2", "AC3"]);
    }

    #[test]
    fn uncovered_criteria_survive_the_evidence_cap() {
        let f = fixture("new\n");
        with_criteria(&f, json!([{"id": "AC1", "statement": "s", "test": null}]));
        let many = (0..MAX_EVIDENCE_FINDINGS)
            .map(|_| finding("nit", "maintainability"))
            .collect();
        let r = run(&verifier(
            &f,
            invoker(Ok(&reply(many, "comment")), None, &Calls::default()),
            &CostLedger::default(),
            "claude",
        ));
        let titles: Vec<&str> = r.evidence["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f["title"].as_str())
            .collect();
        assert!(
            titles.contains(&"acceptance criterion AC1 has no test"),
            "{titles:?}"
        );
    }
}
