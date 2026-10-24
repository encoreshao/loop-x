//! No-progress detector (mirrors `bin/loop_progress.py`): does the current
//! iteration repeat the failing-verifier signature of the previous one?
//! Stateless; the runtime accumulates the streak.

use crate::loop_result::IterationResult;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressResult {
    pub progressed: bool,
    pub reason: &'static str,
}

fn failure_signature(it: &IterationResult) -> BTreeSet<(String, Option<i64>)> {
    it.verification_results
        .iter()
        .filter(|r| !r.passed)
        .map(|r| (r.name.clone(), r.exit_code))
        .collect()
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ProgressDetector;

impl ProgressDetector {
    pub fn compare(
        &self,
        previous: Option<&IterationResult>,
        current: &IterationResult,
    ) -> ProgressResult {
        let Some(previous) = previous else {
            return ProgressResult {
                progressed: true,
                reason: "first_iteration",
            };
        };
        let prev = failure_signature(previous);
        let cur = failure_signature(current);
        if !cur.is_empty() && cur == prev {
            ProgressResult {
                progressed: false,
                reason: "same_verification_failure",
            }
        } else {
            ProgressResult {
                progressed: true,
                reason: "different_result",
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loop_result::VerificationResult;
    use crate::loop_state::LoopState;
    use serde_json::json;

    fn vr(name: &str, passed: bool, code: i64) -> VerificationResult {
        VerificationResult {
            name: name.into(),
            passed,
            exit_code: Some(code),
            duration_ms: 1,
            output: String::new(),
            evidence: json!({}),
        }
    }

    fn it(results: Vec<VerificationResult>) -> IterationResult {
        IterationResult {
            iteration: 1,
            state: LoopState::Evaluating,
            verification_results: results,
            budget: json!({}),
            progressed: false,
        }
    }

    #[test]
    fn first_iteration_always_progressed() {
        let r = ProgressDetector.compare(None, &it(vec![vr("tests", false, 1)]));
        assert!(r.progressed);
        assert_eq!(r.reason, "first_iteration");
    }

    #[test]
    fn same_failure_signature_is_not_progressed() {
        let a = it(vec![vr("tests", false, 1)]);
        let b = it(vec![vr("tests", false, 1)]);
        let r = ProgressDetector.compare(Some(&a), &b);
        assert!(!r.progressed);
        assert_eq!(r.reason, "same_verification_failure");
    }

    #[test]
    fn different_name_or_exit_code_is_progressed() {
        let a = it(vec![vr("tests", false, 1)]);
        for b in [
            it(vec![vr("lint", false, 1)]),
            it(vec![vr("tests", false, 2)]),
        ] {
            let r = ProgressDetector.compare(Some(&a), &b);
            assert!(r.progressed);
            assert_eq!(r.reason, "different_result");
        }
    }

    #[test]
    fn order_independent_and_passing_current_progresses() {
        let a = it(vec![vr("a", false, 1), vr("b", false, 2)]);
        let b = it(vec![vr("b", false, 2), vr("a", false, 1)]);
        assert!(!ProgressDetector.compare(Some(&a), &b).progressed);
        let ok = it(vec![vr("a", true, 0)]);
        assert!(ProgressDetector.compare(Some(&a), &ok).progressed);
    }
}
