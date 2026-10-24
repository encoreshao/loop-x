//! Action risk classification + `PolicyEngine` (mirrors `bin/loop_policy.py`).
//! Scores *action types* (merge, modify_code, ...) for whether they need a human
//! gate. An unclassified action defaults to L3: default-deny.

use std::collections::{HashMap, HashSet};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskLevel {
    L0ReadOnly,
    L1LocalMutation,
    L2ExternalChange,
    L3Irreversible,
}

impl RiskLevel {
    /// `"L0"`..`"L3"`, the value Python's enum carries.
    pub fn as_str(self) -> &'static str {
        match self {
            RiskLevel::L0ReadOnly => "L0",
            RiskLevel::L1LocalMutation => "L1",
            RiskLevel::L2ExternalChange => "L2",
            RiskLevel::L3Irreversible => "L3",
        }
    }
}

use RiskLevel::*;

/// Built-in action classification (same table as `ACTION_RISK_LEVELS`).
pub const ACTION_RISK_LEVELS: &[(&str, RiskLevel)] = &[
    ("inspect_issue", L0ReadOnly),
    ("inspect_repository", L0ReadOnly),
    ("read_issue", L0ReadOnly),
    ("fetch_connector_data", L0ReadOnly),
    ("summarize", L0ReadOnly),
    ("review_diff", L0ReadOnly),
    ("diagnose_failure", L0ReadOnly),
    ("diagnose_pipeline", L0ReadOnly),
    ("rank_entries", L0ReadOnly),
    ("send_notification", L2ExternalChange),
    ("create_draft_review_note", L2ExternalChange),
    ("modify_worktree", L1LocalMutation),
    ("modify_code", L1LocalMutation),
    ("run_tests", L1LocalMutation),
    ("research_topic", L1LocalMutation),
    ("write_briefing", L1LocalMutation),
    ("read_mailbox", L0ReadOnly),
    ("classify_messages", L0ReadOnly),
    ("label_messages", L2ExternalChange),
    ("create_reply_drafts", L2ExternalChange),
    ("send_email", L3Irreversible),
    ("create_merge_request", L2ExternalChange),
    ("create_mr", L2ExternalChange),
    ("post_public_comment", L2ExternalChange),
    ("merge", L3Irreversible),
    ("production_deploy", L3Irreversible),
    ("delete_production_data", L3Irreversible),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyViolation {
    pub action: String,
    pub risk_level: RiskLevel,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyViolationError {
    pub violations: Vec<PolicyViolation>,
}

impl fmt::Display for PolicyViolationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let summary: Vec<String> = self
            .violations
            .iter()
            .map(|v| format!("{} ({}): {}", v.action, v.risk_level.as_str(), v.reason))
            .collect();
        write!(f, "LoopDefinition policy violation: {}", summary.join("; "))
    }
}

impl std::error::Error for PolicyViolationError {}

#[derive(Debug, Clone)]
pub struct PolicyEngine {
    action_risk_levels: HashMap<String, RiskLevel>,
}

impl Default for PolicyEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PolicyEngine {
    /// Engine over the built-in [`ACTION_RISK_LEVELS`].
    pub fn new() -> Self {
        Self::with_levels(
            ACTION_RISK_LEVELS
                .iter()
                .map(|(a, l)| (a.to_string(), *l))
                .collect(),
        )
    }

    pub fn with_levels(action_risk_levels: HashMap<String, RiskLevel>) -> Self {
        Self { action_risk_levels }
    }

    /// Unknown actions are L3.
    pub fn risk_level_for(&self, action: &str) -> RiskLevel {
        self.action_risk_levels
            .get(action)
            .copied()
            .unwrap_or(L3Irreversible)
    }

    pub fn requires_human_gate(&self, action: &str, human_gates: &[String]) -> bool {
        match self.risk_level_for(action) {
            L3Irreversible => true,
            L2ExternalChange => human_gates.iter().any(|g| g == action),
            _ => false,
        }
    }

    /// Violations for a definition's `actions` / `human_gates`: every L3 action
    /// must be listed as a human gate.
    pub fn validate_definition(
        &self,
        actions: &[String],
        human_gates: &[String],
    ) -> Vec<PolicyViolation> {
        let gates: HashSet<&str> = human_gates.iter().map(String::as_str).collect();
        actions
            .iter()
            .filter_map(|action| {
                let risk = self.risk_level_for(action);
                (risk == L3Irreversible && !gates.contains(action.as_str())).then(|| {
                    PolicyViolation {
                        action: action.clone(),
                        risk_level: risk,
                        reason: "L3 (irreversible) action must be listed in human_gates"
                            .to_string(),
                    }
                })
            })
            .collect()
    }

    /// Like [`validate_definition`](Self::validate_definition) but as a `Result`.
    pub fn check_definition(
        &self,
        actions: &[String],
        human_gates: &[String],
    ) -> Result<(), PolicyViolationError> {
        let violations = self.validate_definition(actions, human_gates);
        if violations.is_empty() {
            Ok(())
        } else {
            Err(PolicyViolationError { violations })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn classification_and_default_deny() {
        let e = PolicyEngine::new();
        assert_eq!(e.risk_level_for("inspect_issue"), L0ReadOnly);
        assert_eq!(e.risk_level_for("modify_code"), L1LocalMutation);
        assert_eq!(e.risk_level_for("create_mr"), L2ExternalChange);
        assert_eq!(e.risk_level_for("merge"), L3Irreversible);
        assert_eq!(e.risk_level_for("never_heard_of_it"), L3Irreversible);
        assert_eq!(L2ExternalChange.as_str(), "L2");
    }

    #[test]
    fn human_gate_rules() {
        let e = PolicyEngine::new();
        assert!(e.requires_human_gate("merge", &[]));
        assert!(e.requires_human_gate("mystery", &[]));
        assert!(!e.requires_human_gate("create_mr", &[]));
        assert!(e.requires_human_gate("create_mr", &v(&["create_mr"])));
        assert!(!e.requires_human_gate("modify_code", &v(&["modify_code"])));
        assert!(!e.requires_human_gate("summarize", &[]));
    }

    #[test]
    fn validate_flags_ungated_l3_only() {
        let e = PolicyEngine::new();
        let out = e.validate_definition(
            &v(&["inspect_issue", "merge", "create_mr", "weird"]),
            &v(&["weird"]),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].action, "merge");
        assert_eq!(out[0].risk_level, L3Irreversible);
        assert!(e
            .validate_definition(&v(&["merge"]), &v(&["merge"]))
            .is_empty());
    }

    #[test]
    fn error_message_lists_every_violation() {
        let e = PolicyEngine::new();
        let err = e
            .check_definition(&v(&["merge", "production_deploy"]), &[])
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "LoopDefinition policy violation: \
             merge (L3): L3 (irreversible) action must be listed in human_gates; \
             production_deploy (L3): L3 (irreversible) action must be listed in human_gates"
        );
    }

    #[test]
    fn custom_levels_override_the_table() {
        let e = PolicyEngine::with_levels(HashMap::from([("merge".to_string(), L0ReadOnly)]));
        assert_eq!(e.risk_level_for("merge"), L0ReadOnly);
        assert_eq!(e.risk_level_for("create_mr"), L3Irreversible);
    }
}
