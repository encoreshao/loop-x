//! `IterationResult` / `LoopResult` (mirrors `bin/loop_result.py`). Field order
//! is the dataclass field order, so `serde_json::to_value` matches
//! `dataclasses.asdict` and the `result.json` layout byte for byte.

use crate::loop_state::LoopState;
use serde::Serialize;
use serde_json::Value;

/// One verifier's outcome (`loop_verifiers.VerificationResult`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VerificationResult {
    pub name: String,
    pub passed: bool,
    pub exit_code: Option<i64>,
    pub duration_ms: i64,
    pub output: String,
    pub evidence: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IterationResult {
    pub iteration: i64,
    pub state: LoopState,
    pub verification_results: Vec<VerificationResult>,
    /// The budget snapshot as plain JSON (`overall`, `cost`, `runtime`, ...).
    pub budget: Value,
    pub progressed: bool,
}

/// How a runner reported cost, beyond what `budget` records. Not serialized
/// into `result.json` (same as the Python plain attributes).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum AgentCost {
    /// Attribute never set (hand-built or legacy result).
    #[default]
    Unset,
    /// No agent call reported a cost.
    Unknown,
    Usd(f64),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LoopResult {
    pub loop_id: String,
    pub run_id: String,
    pub definition_name: String,
    pub final_state: LoopState,
    pub iterations: Vec<IterationResult>,
    pub stop_reason: String,
    pub prompt: Option<String>,
    pub definition_path: Option<String>,
    /// `"finished"` (terminal) or `"running"` (in-progress snapshot).
    pub status: String,
    #[serde(skip)]
    pub agent_cost_usd: AgentCost,
    /// `LoopRuntime`'s "did any agent call report a cost" flag; `None` = unset.
    #[serde(skip)]
    pub cost_reported: Option<bool>,
}

impl LoopResult {
    pub fn new(
        loop_id: &str,
        run_id: &str,
        definition_name: &str,
        final_state: LoopState,
        iterations: Vec<IterationResult>,
        stop_reason: &str,
    ) -> Self {
        Self {
            loop_id: loop_id.into(),
            run_id: run_id.into(),
            definition_name: definition_name.into(),
            final_state,
            iterations,
            stop_reason: stop_reason.into(),
            prompt: None,
            definition_path: None,
            status: "finished".into(),
            agent_cost_usd: AgentCost::Unset,
            cost_reported: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> LoopResult {
        let it = IterationResult {
            iteration: 1,
            state: LoopState::Completed,
            verification_results: vec![VerificationResult {
                name: "tests".into(),
                passed: true,
                exit_code: Some(0),
                duration_ms: 12,
                output: "ok".into(),
                evidence: json!({"command": "true"}),
            }],
            budget: json!({"overall": "ok"}),
            progressed: true,
        };
        LoopResult::new("l", "r", "d", LoopState::Completed, vec![it], "completed")
    }

    #[test]
    fn defaults_match_python() {
        let r = sample();
        assert_eq!(r.status, "finished");
        assert!(r.prompt.is_none() && r.definition_path.is_none());
    }

    #[test]
    fn serializes_in_dataclass_field_order_without_cost_attrs() {
        let v = serde_json::to_value(sample()).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "loop_id",
                "run_id",
                "definition_name",
                "final_state",
                "iterations",
                "stop_reason",
                "prompt",
                "definition_path",
                "status"
            ]
        );
        assert_eq!(v["final_state"], "completed");
        let it: Vec<&str> = v["iterations"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            it,
            [
                "iteration",
                "state",
                "verification_results",
                "budget",
                "progressed"
            ]
        );
        let vr: Vec<&str> = v["iterations"][0]["verification_results"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            vr,
            [
                "name",
                "passed",
                "exit_code",
                "duration_ms",
                "output",
                "evidence"
            ]
        );
    }
}
