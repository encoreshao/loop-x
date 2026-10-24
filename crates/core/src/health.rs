//! The Loop Health score. Mirrors `bin/health.py`: computed from the metrics
//! report plus, optionally, run-ledger records and memory outcomes. The
//! plan's seven weighted components are Resolution 30, Autonomy 25,
//! Verification 15, Cost Efficiency 10, Retry Rate 10, Escalation 5 and
//! Learning Effectiveness 5. A component with no data is `None` and excluded;
//! the rest renormalize. `is_partial` is false only when all seven have data.

use crate::ledger::RunRecord;
use crate::metrics::{IssueMetrics, Report};
use serde_json::{json, Value};

const GITLAB_ISSUE_LOOP: &str = "gitlab-issue-loop";
const MIN_LEARNING_SAMPLE: usize = 5;
/// `(upper bound USD per verified-successful issue, score)`.
const COST_BANDS: [(f64, f64); 4] = [(0.50, 100.0), (1.0, 80.0), (2.0, 60.0), (4.0, 40.0)];

/// The component names in report order, with their weights.
pub const COMPONENT_WEIGHTS: [(&str, f64); 7] = [
    ("resolution", 30.0),
    ("autonomy", 25.0),
    ("verification", 15.0),
    ("cost_efficiency", 10.0),
    ("retry_rate", 10.0),
    ("escalation", 5.0),
    ("learning_effectiveness", 5.0),
];

pub const MISSING_REASON: &str = "components with no data yet (e.g. no ledger runs, no cost-reporting runs with a verified success, or fewer than 5 issues with and without memory reuse) are excluded and the rest renormalize";

#[derive(Debug, Clone, PartialEq)]
pub struct HealthScore {
    pub score: Option<f64>,
    pub is_partial: bool,
    /// In `COMPONENT_WEIGHTS`-independent report order: resolution, autonomy,
    /// verification, escalation, cost_efficiency, retry_rate,
    /// learning_effectiveness.
    pub components: Vec<(&'static str, Option<f64>)>,
    pub cost_efficiency_value: Option<f64>,
    pub retry_rate_value: Option<f64>,
    pub learning_effectiveness_value: Option<f64>,
    pub missing_components: Vec<&'static str>,
    pub missing_reason: &'static str,
}

impl HealthScore {
    pub fn component(&self, name: &str) -> Option<f64> {
        self.components
            .iter()
            .find(|(n, _)| *n == name)
            .and_then(|(_, v)| *v)
    }

    pub fn to_value(&self) -> Value {
        json!({
            "score": self.score,
            "is_partial": self.is_partial,
            "components": Value::Object(
                self.components.iter().map(|(n, v)| (n.to_string(), json!(v))).collect()
            ),
            "details": {
                "cost_efficiency": {"value": self.cost_efficiency_value},
                "retry_rate": {"value": self.retry_rate_value},
                "learning_effectiveness": {"value": self.learning_effectiveness_value},
            },
            "missing_components": self.missing_components,
            "missing_reason": self.missing_reason,
        })
    }
}

fn to_pct(rate: Option<f64>) -> Option<f64> {
    rate.map(|r| r * 100.0)
}

fn escalation_rate(issue: &IssueMetrics) -> Option<f64> {
    (issue.issues_processed != 0)
        .then(|| 1.0 - issue.issues_escalated as f64 / issue.issues_processed as f64)
}

/// GitLab-issue runs backed by a `loop.result` event.
fn gitlab_issue_runs(runs: &[RunRecord]) -> Vec<&RunRecord> {
    runs.iter()
        .filter(|r| {
            r.has_result
                && (r.definition.as_deref() == Some(GITLAB_ISSUE_LOOP)
                    || r.loop_name.as_deref() == Some(GITLAB_ISSUE_LOOP))
        })
        .collect()
}

/// `(score, rate)` over completed GitLab-issue runs backed by a `loop.result`
/// event; `(None, None)` when there are none.
fn retry_rate(runs: &[RunRecord]) -> (Option<f64>, Option<f64>) {
    let done: Vec<&RunRecord> = gitlab_issue_runs(runs)
        .into_iter()
        .filter(|r| r.final_state.as_deref() == Some("completed"))
        .collect();
    if done.is_empty() {
        return (None, None);
    }
    let rate = done.iter().filter(|r| r.iterations.len() > 1).count() as f64 / done.len() as f64;
    (Some(100.0 * (1.0 - rate)), Some(rate))
}

/// `(score, usd_per_verified_issue)` over GitLab-issue runs with a known cost.
/// Other loops are excluded (a topic or inbox run has no verifiers, so it is
/// "verified" at ~$0 and would make the score meaningless); unknown-cost
/// (Codex) runs leave both numerator and denominator.
fn cost_efficiency(runs: &[RunRecord]) -> (Option<f64>, Option<f64>) {
    let known: Vec<&RunRecord> = gitlab_issue_runs(runs)
        .into_iter()
        .filter(|r| r.total_cost_usd.is_some())
        .collect();
    let verified = known.iter().filter(|r| r.verified_success).count();
    if verified == 0 {
        return (None, None);
    }
    let value =
        crate::pyfmt_lite::py_sum(known.iter().filter_map(|r| r.total_cost_usd)) / verified as f64;
    for (bound, score) in COST_BANDS {
        if value <= bound {
            return (Some(score), Some(value));
        }
    }
    (Some(20.0), Some(value))
}

fn learning_effectiveness(outcomes: &[(&str, bool)]) -> (Option<f64>, Option<f64>) {
    let rate = |kind: &str| -> (usize, f64) {
        let xs: Vec<bool> = outcomes
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, ok)| *ok)
            .collect();
        (
            xs.len(),
            xs.iter().filter(|b| **b).count() as f64 / xs.len().max(1) as f64,
        )
    };
    let ((n_reused, reused), (n_fresh, fresh)) = (rate("reused"), rate("fresh"));
    if n_reused < MIN_LEARNING_SAMPLE || n_fresh < MIN_LEARNING_SAMPLE {
        return (None, None);
    }
    let delta = (reused - fresh).clamp(-0.5, 0.5);
    (Some(50.0 + 100.0 * delta), Some(delta))
}

/// The health score from `metrics::build_report`'s report. `runs` is
/// `ledger::iter_runs` and `memory_outcomes` is `learning::memory_outcomes`;
/// `None` means "no data" for the components derived from them. The cost
/// report is not read (cost efficiency derives from the runs' own cost).
pub fn compute_health_score(
    metrics_report: &Report,
    runs: Option<&[RunRecord]>,
    memory_outcomes: Option<&[(&str, bool)]>,
) -> HealthScore {
    let qa = &metrics_report.quality_and_autonomy;
    let (retry_score, retry_value) = retry_rate(runs.unwrap_or(&[]));
    let (cost_score, cost_value) = cost_efficiency(runs.unwrap_or(&[]));
    let (learn_score, learn_delta) = learning_effectiveness(memory_outcomes.unwrap_or(&[]));

    let components: Vec<(&'static str, Option<f64>)> = vec![
        ("resolution", to_pct(qa.resolution_rate)),
        ("autonomy", to_pct(qa.autonomy_rate)),
        (
            "verification",
            to_pct(metrics_report.verification.verification_pass_rate),
        ),
        ("escalation", to_pct(escalation_rate(&metrics_report.issue))),
        ("cost_efficiency", cost_score),
        ("retry_rate", retry_score),
        ("learning_effectiveness", learn_score),
    ];
    let weight = |name: &str| {
        COMPONENT_WEIGHTS
            .iter()
            .find(|(n, _)| *n == name)
            .map_or(0.0, |(_, w)| *w)
    };
    let missing: Vec<&'static str> = components
        .iter()
        .filter(|(_, v)| v.is_none())
        .map(|(n, _)| *n)
        .collect();
    let available: Vec<(&str, f64)> = components
        .iter()
        .filter_map(|(n, v)| v.map(|v| (*n, v)))
        .collect();
    let score = (!available.is_empty()).then(|| {
        let weight_sum: f64 = available.iter().map(|(n, _)| weight(n)).sum();
        crate::pyfmt_lite::py_sum(available.iter().map(|(n, v)| v * weight(n))) / weight_sum
    });
    HealthScore {
        score,
        is_partial: !missing.is_empty(),
        components,
        cost_efficiency_value: cost_value,
        retry_rate_value: retry_value,
        learning_effectiveness_value: learn_delta,
        missing_components: missing,
        missing_reason: MISSING_REASON,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics;

    fn events(n_completed: usize, n_escalated: usize, passed: usize, failed: usize) -> Vec<Value> {
        let mut out = Vec::new();
        let mut push = |t: &str, id: String| {
            out.push(
                json!({"event_type": t, "issue_run_id": id, "timestamp": "2026-09-04T10:00:00Z"}),
            );
        };
        for i in 0..n_completed {
            push("issue.started", format!("c{i}"));
            push("issue.completed", format!("c{i}"));
        }
        for i in 0..n_escalated {
            push("issue.started", format!("e{i}"));
            push("issue.escalated", format!("e{i}"));
        }
        for i in 0..passed {
            push("verification.passed", format!("p{i}"));
        }
        for i in 0..failed {
            push("verification.failed", format!("f{i}"));
        }
        out
    }

    fn report(evs: &[Value]) -> Report {
        metrics::build_report(None, None, None, None, Some(evs))
    }

    fn run(def: &str, state: &str, iters: usize, cost: Option<f64>, verified: bool) -> RunRecord {
        RunRecord {
            run_id: Some("x".into()),
            loop_id: None,
            definition: Some(def.to_string()),
            loop_name: None,
            final_state: Some(state.to_string()),
            stop_reason: None,
            started_at: None,
            finished_at: None,
            duration_ms: None,
            total_cost_usd: cost,
            iterations: vec![json!({}); iters],
            verified_success: verified,
            has_result: true,
            budget_overall: None,
            complete: true,
        }
    }

    #[test]
    fn no_data_gives_no_score_and_everything_missing() {
        let h = compute_health_score(&report(&[]), None, None);
        assert_eq!(h.score, None);
        assert!(h.is_partial);
        assert_eq!(h.missing_components.len(), 7);
        assert_eq!(h.missing_reason, MISSING_REASON);
    }

    #[test]
    fn available_components_renormalize() {
        // 3 completed + 1 escalated: resolution = autonomy = 75, escalation = 75;
        // verification 2 passed / 1 failed = 66.67.
        let evs = events(3, 1, 2, 1);
        let h = compute_health_score(&report(&evs), None, None);
        assert_eq!(h.component("resolution"), Some(75.0));
        assert_eq!(h.component("escalation"), Some(75.0));
        let v = 200.0 / 3.0;
        assert!((h.component("verification").unwrap() - v).abs() < 1e-9);
        let want = (75.0 * 30.0 + 75.0 * 25.0 + v * 15.0 + 75.0 * 5.0) / 75.0;
        assert!((h.score.unwrap() - want).abs() < 1e-9);
        assert_eq!(
            h.missing_components,
            ["cost_efficiency", "retry_rate", "learning_effectiveness"]
        );
    }

    #[test]
    fn retry_rate_counts_completed_gitlab_runs_with_more_than_one_iteration() {
        let runs = vec![
            run("gitlab-issue-loop", "completed", 1, Some(0.1), true),
            run("gitlab-issue-loop", "completed", 2, Some(0.1), true),
            run("gitlab-issue-loop", "escalated", 3, Some(0.1), false),
            run("topic-loop", "completed", 3, Some(0.0), true),
        ];
        let h = compute_health_score(&report(&[]), Some(&runs), None);
        assert_eq!(h.component("retry_rate"), Some(50.0));
        assert_eq!(h.retry_rate_value, Some(0.5));
    }

    #[test]
    fn cost_efficiency_uses_bands_and_ignores_other_loops() {
        let mk = |cost: f64| vec![run("gitlab-issue-loop", "completed", 1, Some(cost), true)];
        for (cost, want) in [
            (0.4, 100.0),
            (0.5, 100.0),
            (0.9, 80.0),
            (1.5, 60.0),
            (3.0, 40.0),
            (9.0, 20.0),
        ] {
            let h = compute_health_score(&report(&[]), Some(&mk(cost)), None);
            assert_eq!(h.component("cost_efficiency"), Some(want), "cost {cost}");
        }
        let none_verified = vec![run("gitlab-issue-loop", "escalated", 1, Some(1.0), false)];
        assert_eq!(
            compute_health_score(&report(&[]), Some(&none_verified), None)
                .component("cost_efficiency"),
            None
        );
        let other = vec![run("topic-loop", "completed", 1, Some(0.0), true)];
        assert_eq!(
            compute_health_score(&report(&[]), Some(&other), None).cost_efficiency_value,
            None
        );
        // Two known runs, one verified: total cost / verified.
        let mixed = vec![
            run("gitlab-issue-loop", "completed", 1, Some(0.3), true),
            run("gitlab-issue-loop", "escalated", 1, Some(0.5), false),
        ];
        let h = compute_health_score(&report(&[]), Some(&mixed), None);
        assert_eq!(h.cost_efficiency_value, Some(0.8));
        assert_eq!(h.component("cost_efficiency"), Some(80.0));
    }

    #[test]
    fn learning_effectiveness_needs_five_of_each_and_clamps() {
        let mut o: Vec<(&str, bool)> = vec![("reused", true); 5];
        o.extend(vec![("fresh", false); 4]);
        assert_eq!(
            compute_health_score(&report(&[]), None, Some(&o)).component("learning_effectiveness"),
            None
        );
        o.push(("fresh", false));
        let h = compute_health_score(&report(&[]), None, Some(&o));
        assert_eq!(h.component("learning_effectiveness"), Some(100.0));
        assert_eq!(h.learning_effectiveness_value, Some(0.5));
        let mut worse: Vec<(&str, bool)> = vec![("reused", false); 5];
        worse.extend(vec![("fresh", true); 5]);
        let h = compute_health_score(&report(&[]), None, Some(&worse));
        assert_eq!(h.component("learning_effectiveness"), Some(0.0));
    }

    #[test]
    fn to_value_has_the_report_shape() {
        let h = compute_health_score(&report(&events(1, 0, 1, 0)), None, None);
        let v = h.to_value();
        assert_eq!(v["is_partial"], json!(true));
        assert_eq!(v["components"]["resolution"], json!(100.0));
        assert!(v["components"]["retry_rate"].is_null());
        assert!(v["details"]["cost_efficiency"]["value"].is_null());
        let keys: Vec<&String> = v["components"].as_object().unwrap().keys().collect();
        assert_eq!(keys[0], "resolution");
        assert_eq!(keys[3], "escalation");
    }
}
