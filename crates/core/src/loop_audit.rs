//! `loop audit`: score a `loop.yaml` against the Loop Ready checklist.
//! Mirrors `bin/loop_audit.py` check for check, weight for weight.
//!
//! Every check has real backing (a `LoopDefinition` field or the
//! `PolicyEngine`), so `is_partial` is always false; the field is kept for the
//! same report shape the Python had.

use crate::loop_definition::{DefinitionError, LoopDefinition};
use crate::policy::PolicyEngine;
use crate::pydumps::py_repr_str;
use serde_json::Value;
use std::path::Path;

/// Actions that actually change repository content.
const CODE_MUTATING_ACTIONS: [&str; 2] = ["modify_code", "modify_worktree"];
/// The two persistent memory tiers.
const MEMORY_SOURCES: [&str; 2] = ["project_memory", "task_memory"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

impl CheckStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckStatus::Pass => "PASS",
            CheckStatus::Warn => "WARN",
            CheckStatus::Fail => "FAIL",
        }
    }

    fn weight(self) -> f64 {
        match self {
            CheckStatus::Pass => 1.0,
            CheckStatus::Warn => 0.5,
            CheckStatus::Fail => 0.0,
        }
    }
}

const CHECK_WEIGHTS: [(&str, f64); 12] = [
    ("goal", 10.0),
    ("trigger", 5.0),
    ("verification", 15.0),
    ("stop_conditions", 15.0),
    ("retry", 10.0),
    ("budget", 10.0),
    ("no_progress_detection", 10.0),
    ("human_gates", 15.0),
    ("context_strategy", 5.0),
    ("memory_strategy", 5.0),
    ("credential_boundary", 5.0),
    ("observability", 5.0),
];

#[derive(Debug, Clone, PartialEq)]
pub struct AuditCheck {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuditReport {
    pub checks: Vec<AuditCheck>,
    pub score: Option<f64>,
    pub is_partial: bool,
    pub missing_components: Vec<String>,
    pub missing_reason: String,
}

impl AuditReport {
    pub fn check(&self, name: &str) -> Option<&AuditCheck> {
        self.checks.iter().find(|c| c.name == name)
    }

    pub fn has_fail(&self) -> bool {
        self.checks.iter().any(|c| c.status == CheckStatus::Fail)
    }

    /// The score as Python prints it (`87.5`, `100.0`).
    pub fn score_text(&self) -> String {
        match self.score {
            Some(s) => format!("{s:?}"),
            None => "None".to_string(),
        }
    }
}

/// Which `stop_conditions` fields the YAML set to an explicit `null`.
/// Python stores that as `None` ("unbounded"); `LoopDefinition` falls back to
/// the default instead, so the audit needs to be told.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NullStops {
    pub max_iterations: bool,
    pub max_runtime_minutes: bool,
    pub max_cost_usd: bool,
    pub no_progress_iterations: bool,
}

impl NullStops {
    pub fn from_raw(raw: &Value) -> Self {
        let is_null = |key: &str| {
            raw.get("stop_conditions")
                .and_then(|s| s.get(key))
                .is_some_and(Value::is_null)
        };
        Self {
            max_iterations: is_null("max_iterations"),
            max_runtime_minutes: is_null("max_runtime_minutes"),
            max_cost_usd: is_null("max_cost_usd"),
            no_progress_iterations: is_null("no_progress_iterations"),
        }
    }
}

fn check(name: &'static str, status: CheckStatus, detail: impl Into<String>) -> AuditCheck {
    AuditCheck {
        name,
        status,
        detail: detail.into(),
    }
}

/// Python's `round(x, 1)` for the non-tie values this score can take.
fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

pub fn audit_definition(definition: &LoopDefinition) -> AuditReport {
    audit_with(definition, NullStops::default(), &PolicyEngine::new())
}

pub fn audit_with(
    definition: &LoopDefinition,
    nulls: NullStops,
    policy: &PolicyEngine,
) -> AuditReport {
    use CheckStatus::*;
    let mut checks = Vec::new();

    checks.push(check(
        "goal",
        if definition.goal.kind.is_empty() {
            Fail
        } else {
            Pass
        },
        format!("goal.type={}", py_repr_str(&definition.goal.kind)),
    ));
    checks.push(check(
        "trigger",
        if definition.trigger.kind.is_empty() {
            Fail
        } else {
            Pass
        },
        format!("trigger.type={}", py_repr_str(&definition.trigger.kind)),
    ));

    let project_commands = definition
        .verifiers
        .iter()
        .any(|v| v.get("type").and_then(Value::as_str) == Some("project_commands"));
    if project_commands {
        let mode = &definition.verification.mode;
        let detail =
            format!("project_commands verifier (per-project test_cmd/lint_cmd, mode={mode})");
        if mode == "observe" {
            checks.push(check(
                "verification",
                Warn,
                format!("{detail} - verification recorded but not enforced"),
            ));
        } else {
            checks.push(check("verification", Pass, detail));
        }
    } else if !definition.verification.required.is_empty() {
        checks.push(check(
            "verification",
            Pass,
            format!(
                "{} verifier(s) required",
                definition.verification.required.len()
            ),
        ));
    } else if !definition
        .actions
        .iter()
        .any(|a| CODE_MUTATING_ACTIONS.contains(&a.as_str()))
    {
        checks.push(check(
            "verification",
            Pass,
            "no code-mutating actions declared - nothing to verify",
        ));
    } else {
        checks.push(check(
            "verification",
            Fail,
            "no verifiers required - success cannot be machine-checked",
        ));
    }

    let sc = &definition.stop_conditions;
    let max_iterations = (!nulls.max_iterations).then_some(sc.max_iterations);
    match max_iterations {
        Some(n) if n > 0 => checks.push(check(
            "stop_conditions",
            Pass,
            format!("max_iterations={n}"),
        )),
        other => checks.push(check(
            "stop_conditions",
            Fail,
            format!(
                "max_iterations={} - loop cannot make progress",
                other.map_or("None".to_string(), |n| n.to_string())
            ),
        )),
    }

    let retry = &definition.retry;
    if !retry.enabled {
        checks.push(check(
            "retry",
            Warn,
            "retry disabled - a single failure escalates immediately",
        ));
    } else if retry.max_attempts >= 1 {
        checks.push(check(
            "retry",
            Pass,
            format!("max_attempts={}", retry.max_attempts),
        ));
    } else {
        checks.push(check(
            "retry",
            Fail,
            format!("enabled with max_attempts={}", retry.max_attempts),
        ));
    }

    let unbounded: Vec<&str> = [
        ("max_iterations", nulls.max_iterations),
        ("max_runtime_minutes", nulls.max_runtime_minutes),
        ("max_cost_usd", nulls.max_cost_usd),
    ]
    .iter()
    .filter(|(_, is_none)| *is_none)
    .map(|(dim, _)| *dim)
    .collect();
    if unbounded.is_empty() {
        checks.push(check("budget", Pass, "all three budget dimensions bounded"));
    } else {
        checks.push(check(
            "budget",
            Warn,
            format!("unbounded: {}", unbounded.join(", ")),
        ));
    }

    let no_progress = (!nulls.no_progress_iterations).then_some(sc.no_progress_iterations);
    match no_progress {
        Some(n) if n > 0 => checks.push(check(
            "no_progress_detection",
            Pass,
            format!("no_progress_iterations={n}"),
        )),
        _ => checks.push(check(
            "no_progress_detection",
            Fail,
            "no_progress_iterations not set - a stuck loop only stops via budget",
        )),
    }

    let violations = policy.validate_definition(&definition.actions, &definition.human_gates);
    if violations.is_empty() {
        checks.push(check("human_gates", Pass, "every L3 action is gated"));
    } else {
        let detail = violations
            .iter()
            .map(|v| format!("{} ({})", v.action, v.risk_level.as_str()))
            .collect::<Vec<_>>()
            .join("; ");
        checks.push(check("human_gates", Fail, detail));
    }

    if definition.context.sources.is_empty() {
        checks.push(check(
            "context_strategy",
            Warn,
            "no context sources declared - the agent may receive unbounded context",
        ));
    } else {
        checks.push(check(
            "context_strategy",
            Pass,
            format!(
                "{} context source(s) declared",
                definition.context.sources.len()
            ),
        ));
    }

    if definition
        .context
        .sources
        .iter()
        .any(|s| MEMORY_SOURCES.contains(&s.as_str()))
    {
        checks.push(check(
            "memory_strategy",
            Pass,
            "persistent memory source declared",
        ));
    } else {
        checks.push(check(
            "memory_strategy",
            Warn,
            "no persistent memory source declared - loop starts stateless each run",
        ));
    }

    if definition.permissions.credentials_read {
        checks.push(check(
            "credential_boundary",
            Fail,
            "permissions.credentials_read=True - agent must never read credentials",
        ));
    } else {
        checks.push(check(
            "credential_boundary",
            Pass,
            "permissions.credentials_read=False",
        ));
    }

    if definition.observability_enabled {
        checks.push(check("observability", Pass, "observability_enabled=True"));
    } else {
        checks.push(check(
            "observability",
            Fail,
            "observability_enabled=False - runs will not produce structured events",
        ));
    }

    let weight_of = |name: &str| {
        CHECK_WEIGHTS
            .iter()
            .find(|(n, _)| *n == name)
            .map_or(0.0, |(_, w)| *w)
    };
    let weight_sum: f64 = checks.iter().map(|c| weight_of(c.name)).sum();
    let earned: f64 = checks
        .iter()
        .map(|c| weight_of(c.name) * c.status.weight())
        .sum();
    let score = (weight_sum != 0.0).then(|| round1(100.0 * earned / weight_sum));

    AuditReport {
        checks,
        score,
        is_partial: false,
        missing_components: Vec::new(),
        missing_reason: String::new(),
    }
}

/// Parse `path` into a definition plus the stop-condition fields the YAML set
/// to an explicit `null`.
pub fn load(path: &Path) -> Result<(LoopDefinition, NullStops), DefinitionError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| DefinitionError::Io(path.display().to_string(), e))?;
    let raw: Value = serde_norway::from_str(&text)
        .map_err(|e| DefinitionError::Yaml(path.display().to_string(), e.to_string()))?;
    let definition = LoopDefinition::from_value(&raw)?;
    Ok((definition, NullStops::from_raw(&raw)))
}

/// Load `path` and audit it; explicit `null` stop-condition fields count as
/// unbounded, like the Python's `None`.
pub fn audit_path(path: &Path) -> Result<AuditReport, DefinitionError> {
    let (definition, nulls) = load(path)?;
    Ok(audit_with(&definition, nulls, &PolicyEngine::new()))
}

/// The report's lines exactly as `loop audit` prints them (without the
/// `== path` header).
pub fn format_report(report: &AuditReport) -> String {
    let mut out = String::new();
    for c in &report.checks {
        out.push_str(&format!(
            "{:<4}  {:<22} {}\n",
            c.status.as_str(),
            c.name,
            c.detail
        ));
    }
    out.push('\n');
    if report.is_partial {
        out.push_str(&format!(
            "Loop Ready Score: {} / 100 (partial - missing: {})\n",
            report.score_text(),
            report.missing_components.join(", ")
        ));
    } else {
        out.push_str(&format!(
            "Loop Ready Score: {} / 100\n",
            report.score_text()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn compliant() -> Value {
        json!({
            "name": "compliant-loop",
            "version": 1,
            "trigger": {"type": "schedule", "schedule": "0 10 * * 1-5"},
            "goal": {"type": "issue_resolution"},
            "context": {"sources": ["issue", "repository", "project_memory", "task_memory"]},
            "actions": ["inspect_issue", "modify_code", "merge"],
            "verification": {"required": ["tests", "lint"]},
            "stop_conditions": {"max_iterations": 3, "max_runtime_minutes": 30, "max_cost_usd": 5, "no_progress_iterations": 2},
            "human_gates": ["merge"],
            "retry": {"enabled": true, "max_attempts": 2},
        })
    }

    fn audit(v: &Value) -> AuditReport {
        let d = LoopDefinition::from_value(v).unwrap();
        audit_with(&d, NullStops::from_raw(v), &PolicyEngine::new())
    }

    fn status(r: &AuditReport, name: &str) -> CheckStatus {
        r.check(name).unwrap().status
    }

    #[test]
    fn fully_compliant_definition_scores_100() {
        let r = audit(&compliant());
        assert_eq!(r.score, Some(100.0));
        assert!(r.checks.iter().all(|c| c.status == CheckStatus::Pass));
        assert!(!r.is_partial);
        assert!(r.missing_components.is_empty());
        assert_eq!(r.missing_reason, "");
        assert_eq!(r.score_text(), "100.0");
    }

    #[test]
    fn verification_fails_without_required_verifiers_for_code_changes() {
        let mut v = compliant();
        v["verification"] = json!({"required": []});
        let r = audit(&v);
        assert_eq!(status(&r, "verification"), CheckStatus::Fail);
        assert!(r.score.unwrap() < 100.0);
    }

    #[test]
    fn verification_passes_when_nothing_mutates_code() {
        let mut v = compliant();
        v["verification"] = json!({"required": []});
        v["actions"] = json!(["inspect_issue", "post_comment"]);
        let r = audit(&v);
        assert_eq!(status(&r, "verification"), CheckStatus::Pass);
        assert!(r
            .check("verification")
            .unwrap()
            .detail
            .contains("nothing to verify"));
    }

    #[test]
    fn project_commands_verifier_warns_in_observe_mode_and_passes_in_gate() {
        let mut v = compliant();
        v["verification"] = json!({"required": [], "mode": "observe"});
        v["verifiers"] = json!([{"name": "checks", "type": "project_commands"}]);
        let r = audit(&v);
        let c = r.check("verification").unwrap();
        assert_eq!(c.status, CheckStatus::Warn);
        assert_eq!(
            c.detail,
            "project_commands verifier (per-project test_cmd/lint_cmd, mode=observe) - verification recorded but not enforced"
        );
        v["verification"]["mode"] = json!("gate");
        assert_eq!(status(&audit(&v), "verification"), CheckStatus::Pass);
    }

    #[test]
    fn retry_disabled_warns_and_zero_attempts_fails() {
        let mut v = compliant();
        v["retry"] = json!({"enabled": false, "max_attempts": 1});
        assert_eq!(status(&audit(&v), "retry"), CheckStatus::Warn);
        v["retry"] = json!({"enabled": true, "max_attempts": 0});
        let r = audit(&v);
        assert_eq!(status(&r, "retry"), CheckStatus::Fail);
        assert_eq!(
            r.check("retry").unwrap().detail,
            "enabled with max_attempts=0"
        );
    }

    #[test]
    fn zero_iterations_or_no_progress_fail() {
        let mut v = compliant();
        v["stop_conditions"]["max_iterations"] = json!(0);
        v["stop_conditions"]["no_progress_iterations"] = json!(0);
        let r = audit(&v);
        assert_eq!(status(&r, "stop_conditions"), CheckStatus::Fail);
        assert_eq!(status(&r, "no_progress_detection"), CheckStatus::Fail);
    }

    #[test]
    fn explicit_null_limits_are_unbounded() {
        let mut v = compliant();
        v["stop_conditions"]["max_cost_usd"] = Value::Null;
        v["stop_conditions"]["max_runtime_minutes"] = Value::Null;
        let r = audit(&v);
        let c = r.check("budget").unwrap();
        assert_eq!(c.status, CheckStatus::Warn);
        assert_eq!(c.detail, "unbounded: max_runtime_minutes, max_cost_usd");
        v["stop_conditions"]["max_iterations"] = Value::Null;
        let r = audit(&v);
        assert_eq!(
            r.check("stop_conditions").unwrap().detail,
            "max_iterations=None - loop cannot make progress"
        );
    }

    #[test]
    fn ungated_l3_action_fails_human_gates() {
        let mut v = compliant();
        v["human_gates"] = json!([]);
        let r = audit(&v);
        let c = r.check("human_gates").unwrap();
        assert_eq!(c.status, CheckStatus::Fail);
        assert_eq!(c.detail, "merge (L3)");
    }

    #[test]
    fn context_and_memory_warn_when_missing() {
        let mut v = compliant();
        v["context"] = json!({"sources": ["issue"]});
        let r = audit(&v);
        assert_eq!(status(&r, "context_strategy"), CheckStatus::Pass);
        assert_eq!(status(&r, "memory_strategy"), CheckStatus::Warn);
        v["context"] = json!({"sources": []});
        assert_eq!(status(&audit(&v), "context_strategy"), CheckStatus::Warn);
    }

    #[test]
    fn credentials_and_observability_fail() {
        let mut v = compliant();
        v["permissions"] = json!({"credentials_read": true});
        v["observability_enabled"] = json!(false);
        let r = audit(&v);
        assert_eq!(status(&r, "credential_boundary"), CheckStatus::Fail);
        assert_eq!(status(&r, "observability"), CheckStatus::Fail);
        assert_eq!(
            r.check("credential_boundary").unwrap().detail,
            "permissions.credentials_read=True - agent must never read credentials"
        );
    }

    #[test]
    fn score_weights_warn_as_half() {
        // retry disabled: WARN on a weight-10 check -> 100 * (110 - 5) / 110.
        let mut v = compliant();
        v["retry"] = json!({"enabled": false});
        assert_eq!(audit(&v).score, Some(95.5));
    }

    #[test]
    fn format_matches_the_cli_layout() {
        let r = audit(&compliant());
        let text = format_report(&r);
        assert!(text.starts_with("PASS  goal                   goal.type='issue_resolution'\n"));
        assert!(text.ends_with("\nLoop Ready Score: 100.0 / 100\n"));
    }

    #[test]
    fn every_real_loop_and_template_has_no_fail() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut seen = 0;
        for dir in ["loops", "templates"] {
            for entry in std::fs::read_dir(root.join(dir)).unwrap().flatten() {
                let path = entry.path().join("loop.yaml");
                if path.exists() {
                    let r = audit_path(&path).unwrap();
                    assert!(!r.has_fail(), "{} has a FAIL", path.display());
                    seen += 1;
                }
            }
        }
        assert!(seen >= 10);
    }
}
