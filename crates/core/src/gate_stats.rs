//! Harness > Gates numbers from the event log (`gate_stats` in
//! `bin/web/dashboard_server.py`): observe-mode agreement (does the external
//! verifier agree with the agent's "fixed" claim) and what the gate did.

use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

pub const ESCALATION_REASONS: [&str; 6] = [
    "verification_failed",
    "handoff_invalid",
    "mr_open_failed",
    "project_config_error",
    "run_incomplete",
    "guard_violation",
];
pub const AGREEMENT_THRESHOLD: f64 = 0.9;

/// One bucket of counters (the total, or one project).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GateBucket {
    pub issues: i64,
    pub agreement_rate: Option<f64>,
    pub gated_blocks: i64,
    pub retried_then_passed: i64,
    pub escalated_verification: i64,
    pub policy_denied: i64,
    /// Issues whose last pre-MR review found a blocking problem (advisory gate).
    pub review_would_block: i64,
    pub review_errors: i64,
    /// Issues whose last secret scan found a blocking hit (advisory gate).
    pub security_would_block: i64,
    /// Issues whose last dependency audit found a high/critical advisory (advisory gate).
    pub dependency_would_block: i64,
    agreed: i64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GateStats {
    pub total: GateBucket,
    /// Sorted by project name.
    pub by_project: BTreeMap<String, GateBucket>,
}

fn truthy_project(ev: &Value) -> Option<String> {
    match ev.get("project") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn key_of(ev: &Value) -> String {
    let part = |k: &str| ev.get(k).cloned().unwrap_or(Value::Null).to_string();
    format!(
        "{}\u{0}{}\u{0}{}",
        part("run_id"),
        part("project"),
        part("issue_iid")
    )
}

fn is_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Bool(true)) => true,
    }
}

/// `gate_stats`: over the issues whose `issue.completed` says `action == "fix"`,
/// the share whose last non-error `verification.external_completed` (observe
/// mode) observed a pass. Verifier config errors (`data.error`) are excluded.
pub fn gate_stats(events: &[Value]) -> GateStats {
    let mut stats = GateStats::default();
    let mut last_verdict: Vec<(String, Option<String>, bool)> = Vec::new();
    let mut verdict_index: HashMap<String, usize> = HashMap::new();
    let mut fixed: HashSet<String> = HashSet::new();
    let mut retried: Vec<(String, Option<String>)> = Vec::new();
    let mut retried_seen: HashSet<String> = HashSet::new();
    // (gate, issue key) -> (project, would block, errored), last event wins.
    let mut gate_last: BTreeMap<(&str, String), (Option<String>, bool, bool)> = BTreeMap::new();
    let null = Value::Null;

    for ev in events {
        let kind = ev.get("event_type").and_then(Value::as_str).unwrap_or("");
        let data = ev.get("data").filter(|d| d.is_object()).unwrap_or(&null);
        let project = truthy_project(ev);
        let key = key_of(ev);
        match kind {
            "verification.external_completed" => {
                if is_truthy(data.get("error")) {
                    continue;
                }
                let passed = is_truthy(data.get("observed_passed"));
                if data.get("mode").and_then(Value::as_str) != Some("gate") {
                    match verdict_index.get(&key) {
                        Some(&i) => last_verdict[i].2 = passed,
                        None => {
                            verdict_index.insert(key.clone(), last_verdict.len());
                            last_verdict.push((key.clone(), project.clone(), passed));
                        }
                    }
                }
                let iteration = data.get("iteration").filter(|v| is_truthy(Some(v)));
                let iteration = iteration.and_then(Value::as_f64).unwrap_or(1.0);
                if passed && iteration >= 2.0 && retried_seen.insert(key.clone()) {
                    retried.push((key.clone(), project.clone()));
                }
            }
            "verification.review_completed"
            | "verification.security_completed"
            | "verification.dependency_completed" => {
                let gate = match kind {
                    "verification.review_completed" => "review",
                    "verification.security_completed" => "security",
                    _ => "dependency",
                };
                let errored = is_truthy(data.get("error"));
                let blocks = !errored && data.get("observed_passed") == Some(&Value::Bool(false));
                gate_last.insert((gate, key), (project, blocks, errored));
            }
            "issue.completed" => {
                if data.get("action").and_then(Value::as_str) == Some("fix") {
                    fixed.insert(key);
                }
            }
            "issue.escalated" => {
                let reason = data.get("reason").and_then(Value::as_str).unwrap_or("");
                if ESCALATION_REASONS.contains(&reason) && is_truthy(data.get("gated")) {
                    let apply = |b: &mut GateBucket| {
                        b.escalated_verification += 1;
                        if reason == "verification_failed" {
                            b.gated_blocks += 1;
                        }
                    };
                    apply(&mut stats.total);
                    if let Some(p) = project {
                        apply(stats.by_project.entry(p).or_default());
                    }
                }
            }
            "policy.denied" => {
                stats.total.policy_denied += 1;
                if let Some(p) = project {
                    stats.by_project.entry(p).or_default().policy_denied += 1;
                }
            }
            _ => {}
        }
    }

    for (key, project, verdict) in &last_verdict {
        if !fixed.contains(key) {
            continue;
        }
        let apply = |b: &mut GateBucket| {
            b.issues += 1;
            b.agreed += i64::from(*verdict);
        };
        apply(&mut stats.total);
        if let Some(p) = project {
            apply(stats.by_project.entry(p.clone()).or_default());
        }
    }
    for (_, project) in &retried {
        stats.total.retried_then_passed += 1;
        if let Some(p) = project {
            stats
                .by_project
                .entry(p.clone())
                .or_default()
                .retried_then_passed += 1;
        }
    }
    for ((gate, _), (project, blocks, errored)) in &gate_last {
        let apply = |b: &mut GateBucket| match (*gate, blocks, errored) {
            ("review", _, true) => b.review_errors += 1,
            ("review", true, _) => b.review_would_block += 1,
            ("security", true, _) => b.security_would_block += 1,
            ("dependency", true, _) => b.dependency_would_block += 1,
            _ => {}
        };
        apply(&mut stats.total);
        if let Some(p) = project {
            apply(stats.by_project.entry(p.clone()).or_default());
        }
    }
    for b in std::iter::once(&mut stats.total).chain(stats.by_project.values_mut()) {
        if b.issues > 0 {
            b.agreement_rate = Some(b.agreed as f64 / b.issues as f64);
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(kind: &str, iid: Option<i64>, data: Value, project: &str) -> Value {
        json!({"event_type": kind, "project": project, "issue_iid": iid, "data": data, "run_id": "r"})
    }

    #[test]
    fn agreement_rate() {
        let events = vec![
            ev("issue.completed", Some(1), json!({"action": "fix"}), "web"),
            ev(
                "verification.external_completed",
                Some(1),
                json!({"observed_passed": true}),
                "web",
            ),
            ev("issue.completed", Some(2), json!({"action": "fix"}), "web"),
            ev(
                "verification.external_completed",
                Some(2),
                json!({"observed_passed": false}),
                "web",
            ),
        ];
        let s = gate_stats(&events);
        assert_eq!((s.total.issues, s.total.agreement_rate), (2, Some(0.5)));
        assert_eq!(gate_stats(&[]).total.agreement_rate, None);
    }

    #[test]
    fn last_iteration_wins_and_errors_are_excluded() {
        let events = vec![
            ev(
                "verification.external_completed",
                Some(1),
                json!({"observed_passed": false, "iteration": 1}),
                "web",
            ),
            ev(
                "verification.external_completed",
                Some(1),
                json!({"observed_passed": true, "iteration": 2}),
                "web",
            ),
            ev("issue.completed", Some(1), json!({"action": "fix"}), "web"),
            ev(
                "verification.external_completed",
                Some(2),
                json!({"observed_passed": false, "error": true}),
                "web",
            ),
            ev("issue.completed", Some(2), json!({"action": "fix"}), "web"),
        ];
        let s = gate_stats(&events);
        assert_eq!((s.total.issues, s.total.agreement_rate), (1, Some(1.0)));
        assert_eq!(s.total.retried_then_passed, 1);
    }

    #[test]
    fn blocks_escalations_and_policy_denials() {
        let events = vec![
            ev(
                "issue.escalated",
                Some(1),
                json!({"reason": "verification_failed", "gated": true}),
                "web",
            ),
            ev(
                "issue.escalated",
                Some(2),
                json!({"reason": "handoff_invalid", "gated": true}),
                "api",
            ),
            ev(
                "issue.escalated",
                Some(3),
                json!({"reason": "something_else", "gated": true}),
                "web",
            ),
            ev(
                "issue.escalated",
                Some(4),
                json!({"reason": "verification_failed"}),
                "web",
            ),
            ev("policy.denied", None, json!({}), "web"),
        ];
        let s = gate_stats(&events);
        assert_eq!(s.total.gated_blocks, 1);
        assert_eq!(s.total.escalated_verification, 2);
        assert_eq!(s.total.policy_denied, 1);
        assert_eq!(s.by_project["api"].escalated_verification, 1);
    }

    #[test]
    fn gate_mode_verdicts_do_not_count_for_agreement_but_retries_do() {
        let events = vec![
            ev(
                "verification.external_completed",
                Some(1),
                json!({"observed_passed": false, "mode": "gate", "iteration": 1}),
                "web",
            ),
            ev(
                "verification.external_completed",
                Some(1),
                json!({"observed_passed": true, "mode": "gate", "iteration": 2}),
                "web",
            ),
            ev("issue.completed", Some(1), json!({"action": "fix"}), "web"),
            ev(
                "verification.external_completed",
                Some(2),
                json!({"observed_passed": true, "mode": "observe"}),
                "web",
            ),
            ev("issue.completed", Some(2), json!({"action": "fix"}), "web"),
            ev(
                "verification.external_completed",
                Some(3),
                json!({"observed_passed": false}),
                "web",
            ),
            ev(
                "issue.completed",
                Some(3),
                json!({"action": "answer"}),
                "web",
            ),
        ];
        let s = gate_stats(&events);
        assert_eq!((s.total.issues, s.total.agreement_rate), (1, Some(1.0)));
        assert_eq!(s.total.retried_then_passed, 1);
    }

    fn review(iid: i64, observed_passed: bool, project: &str) -> Value {
        ev(
            "verification.review_completed",
            Some(iid),
            json!({"observed_passed": observed_passed, "mode": "observe"}),
            project,
        )
    }

    fn security(iid: i64, observed_passed: bool, project: &str) -> Value {
        ev(
            "verification.security_completed",
            Some(iid),
            json!({"observed_passed": observed_passed, "mode": "observe"}),
            project,
        )
    }

    #[test]
    fn review_and_security_would_block_counted_per_project() {
        let events = vec![
            review(1, false, "web"),
            security(1, false, "web"),
            review(2, true, "web"),
            review(3, false, "api"),
            security(3, true, "api"),
        ];
        let s = gate_stats(&events);
        assert_eq!(s.total.review_would_block, 2);
        assert_eq!(s.total.security_would_block, 1);
        assert_eq!(s.by_project["web"].review_would_block, 1);
        assert_eq!(s.by_project["web"].security_would_block, 1);
        assert_eq!(s.by_project["api"].review_would_block, 1);
        assert_eq!(s.by_project["api"].security_would_block, 0);
    }

    fn deps(iid: i64, data: Value, project: &str) -> Value {
        ev(
            "verification.dependency_completed",
            Some(iid),
            data,
            project,
        )
    }

    #[test]
    fn dependency_would_block_counts_last_event_per_issue_and_skips_errors() {
        let events = vec![
            deps(1, json!({"observed_passed": false}), "web"),
            deps(2, json!({"observed_passed": false}), "web"),
            deps(2, json!({"observed_passed": true}), "web"),
            deps(3, json!({"observed_passed": false, "error": true}), "api"),
            deps(4, json!({"observed_passed": false}), "api"),
        ];
        let s = gate_stats(&events);
        assert_eq!(s.total.dependency_would_block, 2);
        assert_eq!(s.by_project["web"].dependency_would_block, 1);
        assert_eq!(s.by_project["api"].dependency_would_block, 1);
        assert_eq!(s.total.security_would_block, 0);
        assert_eq!(s.total.review_would_block, 0);
    }

    #[test]
    fn guard_violation_counts_as_an_escalation() {
        let events = vec![ev(
            "issue.escalated",
            Some(1),
            json!({"reason": "guard_violation", "gated": true}),
            "web",
        )];
        let s = gate_stats(&events);
        assert_eq!(s.total.escalated_verification, 1);
        assert_eq!(s.by_project["web"].escalated_verification, 1);
        assert_eq!(s.total.gated_blocks, 0);
    }

    #[test]
    fn review_error_is_not_a_block() {
        let events = vec![ev(
            "verification.review_completed",
            Some(1),
            json!({"observed_passed": false, "error": true}),
            "web",
        )];
        let s = gate_stats(&events);
        assert_eq!(s.total.review_would_block, 0);
        assert_eq!(s.total.review_errors, 1);
    }

    #[test]
    fn retry_that_passed_review_counts_once() {
        let events = vec![review(1, false, "web"), review(1, true, "web")];
        assert_eq!(gate_stats(&events).total.review_would_block, 0);
    }

    #[test]
    fn review_events_leave_agreement_rate_unchanged() {
        let events = vec![
            ev(
                "verification.external_completed",
                Some(1),
                json!({"observed_passed": true, "mode": "observe"}),
                "web",
            ),
            review(1, false, "web"),
            ev("issue.completed", Some(1), json!({"action": "fix"}), "web"),
        ];
        let s = gate_stats(&events);
        assert_eq!((s.total.issues, s.total.agreement_rate), (1, Some(1.0)));
    }
}
