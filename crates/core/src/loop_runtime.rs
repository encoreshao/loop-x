//! The iteration / stop-condition engine. Mirrors `bin/loop_runtime.py`.
//!
//! `agent_fn` and the verifiers are injected, so this module has no
//! GitLab/Claude/Codex/Slack dependency. Every event emit and `on_iteration`
//! call is best-effort: observability must never crash the runtime.

use crate::budget::{BudgetController, BudgetStatus, StopConditions as BudgetStopConditions};
use crate::events::{self, EventFields};
use crate::loop_definition::LoopDefinition;
use crate::loop_progress::ProgressDetector;
use crate::loop_result::{IterationResult, LoopResult};
use crate::loop_state::{transition, InvalidTransition, LoopState};
use crate::loop_verifiers::Verifier;
use crate::policy::{PolicyEngine, PolicyViolationError};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Instant;

/// What an agent call (and each verifier) is told about the iteration.
pub struct LoopContext<'a> {
    pub iteration: i64,
    pub definition: &'a LoopDefinition,
    pub previous: Option<&'a IterationResult>,
}

/// A successful agent call. `cost_usd` is `None` when the CLI reported none
/// (Codex, topic runs): the ledger then says "unknown" rather than "$0".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentSuccess {
    pub cost_usd: Option<f64>,
}

/// A failed agent call; it may still have spent money (`error_max_budget_usd`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentFailure {
    pub message: String,
    pub cost_usd: Option<f64>,
}

pub type AgentFn = Box<dyn FnMut(&LoopContext<'_>) -> Result<AgentSuccess, AgentFailure> + Send>;
pub type OnIteration = Box<dyn FnMut(&str, &str, &str, &[IterationResult]) + Send>;

/// Where lifecycle events go. Failures are ignored by the runtime.
pub trait EventSink: Send {
    fn emit(&self, event_type: &str, run_id: &str, data: Value) -> Result<(), String>;
}

/// The default sink: the JSONL event log (`events::emit`).
#[derive(Debug, Clone, Default)]
pub struct FileEvents {
    pub events_dir: Option<PathBuf>,
}

impl EventSink for FileEvents {
    fn emit(&self, event_type: &str, run_id: &str, data: Value) -> Result<(), String> {
        events::emit(
            event_type,
            run_id,
            EventFields {
                data: Some(data),
                ..Default::default()
            },
            self.events_dir.as_deref(),
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("{0}")]
    Policy(PolicyViolationError),
    #[error("{0}")]
    Transition(#[from] InvalidTransition),
}

pub struct LoopRuntime {
    pub agent_fn: AgentFn,
    pub verifiers: Vec<Box<dyn Verifier>>,
    pub progress_detector: ProgressDetector,
    pub policy_engine: PolicyEngine,
    pub events: Box<dyn EventSink>,
    pub on_iteration: Option<OnIteration>,
}

impl LoopRuntime {
    pub fn new(agent_fn: AgentFn, verifiers: Vec<Box<dyn Verifier>>) -> Self {
        Self {
            agent_fn,
            verifiers,
            progress_detector: ProgressDetector,
            policy_engine: PolicyEngine::new(),
            events: Box::new(FileEvents::default()),
            on_iteration: None,
        }
    }

    pub fn with_events_dir(mut self, dir: PathBuf) -> Self {
        self.events = Box::new(FileEvents {
            events_dir: Some(dir),
        });
        self
    }

    pub fn with_events(mut self, sink: Box<dyn EventSink>) -> Self {
        self.events = sink;
        self
    }

    pub fn with_on_iteration(mut self, f: OnIteration) -> Self {
        self.on_iteration = Some(f);
        self
    }

    fn emit(&self, event_type: &str, run_id: &str, data: Value) {
        let _ = self.events.emit(event_type, run_id, data);
    }

    fn notify(&mut self, run_id: &str, loop_id: &str, name: &str, iterations: &[IterationResult]) {
        if let Some(f) = self.on_iteration.as_mut() {
            f(run_id, loop_id, name, iterations);
        }
    }

    /// Run `definition` to a terminal state. `loop_id` defaults to
    /// `loop_<uuid4 hex>`.
    pub fn start(
        &mut self,
        definition: &LoopDefinition,
        run_id: &str,
        loop_id: Option<&str>,
    ) -> Result<LoopResult, RuntimeError> {
        let loop_id = loop_id
            .map(str::to_string)
            .unwrap_or_else(|| format!("loop_{}", uuid::Uuid::new_v4().simple()));
        let violations = self
            .policy_engine
            .validate_definition(&definition.actions, &definition.human_gates);
        if !violations.is_empty() {
            let actions: Vec<&str> = violations.iter().map(|v| v.action.as_str()).collect();
            self.emit(
                "policy.denied",
                run_id,
                json!({"loop_id": loop_id, "violations": actions}),
            );
            return Err(RuntimeError::Policy(PolicyViolationError { violations }));
        }
        self.emit("policy.allowed", run_id, json!({"loop_id": loop_id}));

        let sc = &definition.stop_conditions;
        let budget = BudgetController::new(BudgetStopConditions {
            max_iterations: Some(sc.max_iterations),
            max_runtime_minutes: Some(sc.max_runtime_minutes),
            max_cost_usd: Some(sc.max_cost_usd),
        });
        self.emit(
            "loop.started",
            run_id,
            json!({"definition": definition.name, "loop_id": loop_id}),
        );

        let mut state = LoopState::Pending;
        state = transition(state, LoopState::Discovering)?;
        state = transition(state, LoopState::Planning)?;

        let mut iterations: Vec<IterationResult> = Vec::new();
        let mut completed = 0_i64;
        let mut total_cost = 0.0_f64;
        // The budget needs a number, so an unreported cost counts as 0 there;
        // this flag remembers whether ANY call reported one.
        let mut cost_reported = false;
        let started = Instant::now();
        let mut iteration_number = 1_i64;
        let mut same_failure_streak = 0_i64;
        let (final_state, stop_reason);

        loop {
            let check = budget.check(completed, started.elapsed().as_secs_f64(), total_cost);
            state = transition(state, LoopState::Executing)?;

            if check.overall == BudgetStatus::Exceeded {
                transition(
                    transition(
                        transition(state, LoopState::Verifying)?,
                        LoopState::Evaluating,
                    )?,
                    LoopState::Stopped,
                )?;
                iterations.push(IterationResult {
                    iteration: iteration_number,
                    state: LoopState::Stopped,
                    verification_results: vec![],
                    budget: serde_json::to_value(&check).unwrap_or(Value::Null),
                    progressed: false,
                });
                self.notify(run_id, &loop_id, &definition.name, &iterations);
                final_state = LoopState::Stopped;
                stop_reason = "budget_exceeded";
                break;
            }

            self.emit(
                "iteration.started",
                run_id,
                json!({"iteration": iteration_number}),
            );
            let outcome = {
                let ctx = LoopContext {
                    iteration: iteration_number,
                    definition,
                    previous: iterations_prev(&iterations, iteration_number),
                };
                (self.agent_fn)(&ctx)
            };
            let (agent_failed, call_cost) = match &outcome {
                Ok(ok) => (false, ok.cost_usd),
                Err(err) => (true, err.cost_usd),
            };
            if let Some(cost) = call_cost {
                cost_reported = true;
                total_cost += cost;
            }

            state = transition(state, LoopState::Verifying)?;
            let mut verification_results = Vec::new();
            let mut all_passed = true;
            if !agent_failed {
                self.emit(
                    "verification.started",
                    run_id,
                    json!({"iteration": iteration_number}),
                );
                let ctx = LoopContext {
                    iteration: iteration_number,
                    definition,
                    previous: iterations_prev(&iterations, iteration_number),
                };
                verification_results = self.verifiers.iter().map(|v| v.verify(&ctx)).collect();
                all_passed = verification_results.iter().all(|r| r.passed);
                self.emit(
                    if all_passed {
                        "verification.passed"
                    } else {
                        "verification.failed"
                    },
                    run_id,
                    json!({"iteration": iteration_number}),
                );
            }
            state = transition(state, LoopState::Evaluating)?;

            completed += 1;
            let iteration_budget = serde_json::to_value(budget.check(
                completed,
                started.elapsed().as_secs_f64(),
                total_cost,
            ))
            .unwrap_or(Value::Null);
            let mut result = IterationResult {
                iteration: iteration_number,
                state: LoopState::Evaluating,
                verification_results,
                budget: iteration_budget,
                progressed: false,
            };

            if agent_failed {
                transition(state, LoopState::Failed)?;
                result.state = LoopState::Failed;
                iterations.push(result);
                self.notify(run_id, &loop_id, &definition.name, &iterations);
                self.emit(
                    "iteration.completed",
                    run_id,
                    json!({"iteration": iteration_number, "state": "failed"}),
                );
                final_state = LoopState::Failed;
                stop_reason = "agent_failed";
                break;
            }

            if all_passed {
                transition(state, LoopState::Completed)?;
                result.state = LoopState::Completed;
                result.progressed = true;
                iterations.push(result);
                self.notify(run_id, &loop_id, &definition.name, &iterations);
                self.emit(
                    "iteration.completed",
                    run_id,
                    json!({"iteration": iteration_number, "state": "completed"}),
                );
                final_state = LoopState::Completed;
                stop_reason = "completed";
                break;
            }

            let progress = self
                .progress_detector
                .compare(iterations_prev(&iterations, iteration_number), &result);
            result.progressed = progress.progressed;
            same_failure_streak = if progress.progressed {
                1
            } else {
                same_failure_streak + 1
            };

            if same_failure_streak >= definition.stop_conditions.no_progress_iterations {
                transition(state, LoopState::Escalated)?;
                result.state = LoopState::Escalated;
                iterations.push(result);
                self.notify(run_id, &loop_id, &definition.name, &iterations);
                self.emit(
                    "iteration.completed",
                    run_id,
                    json!({"iteration": iteration_number, "state": "escalated", "reason": "no_progress"}),
                );
                final_state = LoopState::Escalated;
                stop_reason = "no_progress";
                break;
            }

            if definition.retry.enabled && iteration_number < definition.retry.max_attempts {
                iterations.push(result);
                self.notify(run_id, &loop_id, &definition.name, &iterations);
                self.emit(
                    "iteration.completed",
                    run_id,
                    json!({"iteration": iteration_number, "state": "retry"}),
                );
                iteration_number += 1;
                // The next pass re-enters EXECUTING from EVALUATING.
                continue;
            }

            transition(state, LoopState::Escalated)?;
            result.state = LoopState::Escalated;
            iterations.push(result);
            self.notify(run_id, &loop_id, &definition.name, &iterations);
            self.emit(
                "iteration.completed",
                run_id,
                json!({"iteration": iteration_number, "state": "escalated", "reason": "retries_exhausted"}),
            );
            final_state = LoopState::Escalated;
            stop_reason = "escalated";
            break;
        }

        let terminal = match final_state {
            LoopState::Completed => "loop.completed",
            LoopState::Failed => "loop.failed",
            _ => "loop.stopped",
        };
        self.emit(
            terminal,
            run_id,
            json!({"final_state": final_state.as_str(), "stop_reason": stop_reason}),
        );
        let mut result = LoopResult::new(
            &loop_id,
            run_id,
            &definition.name,
            final_state,
            iterations,
            stop_reason,
        );
        result.cost_reported = Some(cost_reported);
        Ok(result)
    }
}

/// The iteration before `current` (iterations are pushed in order, so it is the
/// last one recorded), mirroring Python's `previous`.
fn iterations_prev(iterations: &[IterationResult], current: i64) -> Option<&IterationResult> {
    iterations.last().filter(|i| i.iteration < current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loop_result::VerificationResult;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    type Call = (String, String, String, usize);
    type Seen = Vec<(i64, Option<i64>)>;

    fn definition(
        max_iterations: i64,
        max_attempts: i64,
        actions: &[&str],
        gates: &[&str],
    ) -> LoopDefinition {
        LoopDefinition::from_value(&json!({
            "name": "test-loop", "version": 1,
            "trigger": {"type": "manual"}, "goal": {"type": "issue_resolution"},
            "stop_conditions": {"max_iterations": max_iterations, "max_runtime_minutes": 30, "max_cost_usd": 5, "no_progress_iterations": 2},
            "retry": {"enabled": true, "max_attempts": max_attempts},
            "actions": actions, "human_gates": gates,
        }))
        .unwrap()
    }

    fn vr(name: &str, passed: bool) -> VerificationResult {
        VerificationResult {
            name: name.into(),
            passed,
            exit_code: Some(if passed { 0 } else { 1 }),
            duration_ms: 1,
            output: String::new(),
            evidence: json!({}),
        }
    }

    #[derive(Clone)]
    struct Fake {
        results: Arc<Mutex<VecDeque<VerificationResult>>>,
        calls: Arc<Mutex<usize>>,
    }

    impl Fake {
        fn new(results: Vec<VerificationResult>) -> Self {
            Self {
                results: Arc::new(Mutex::new(results.into())),
                calls: Arc::new(Mutex::new(0)),
            }
        }
        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    impl Verifier for Fake {
        fn verify(&self, _: &LoopContext<'_>) -> VerificationResult {
            *self.calls.lock().unwrap() += 1;
            self.results.lock().unwrap().pop_front().unwrap()
        }
    }

    struct NoEvents;
    impl EventSink for NoEvents {
        fn emit(&self, _: &str, _: &str, _: Value) -> Result<(), String> {
            Err("ignored".into())
        }
    }

    fn ok_agent() -> AgentFn {
        Box::new(|_| Ok(AgentSuccess::default()))
    }

    fn runtime(agent: AgentFn, fake: &Fake) -> LoopRuntime {
        LoopRuntime::new(agent, vec![Box::new(fake.clone())]).with_events(Box::new(NoEvents))
    }

    #[test]
    fn success_path_completes() {
        let f = Fake::new(vec![vr("tests", true)]);
        let r = runtime(ok_agent(), &f)
            .start(&definition(10, 2, &[], &[]), "run_1", None)
            .unwrap();
        assert_eq!(r.final_state, LoopState::Completed);
        assert_eq!(r.stop_reason, "completed");
        assert_eq!(r.iterations.len(), 1);
        assert!(r.iterations[0].progressed);
        assert_eq!(f.calls(), 1);
        assert!(r.loop_id.starts_with("loop_") && r.loop_id.len() == 5 + 32);
    }

    #[test]
    fn retry_path_fails_once_then_passes() {
        let f = Fake::new(vec![vr("tests", false), vr("tests", true)]);
        let r = runtime(ok_agent(), &f)
            .start(&definition(10, 2, &[], &[]), "run_2", None)
            .unwrap();
        assert_eq!(r.final_state, LoopState::Completed);
        assert_eq!(r.iterations.len(), 2);
        assert_eq!(r.iterations[0].state, LoopState::Evaluating);
    }

    #[test]
    fn escalates_after_exhausting_retries() {
        let f = Fake::new(vec![vr("tests", false), vr("lint", false)]);
        let r = runtime(ok_agent(), &f)
            .start(&definition(10, 2, &[], &[]), "run_3", None)
            .unwrap();
        assert_eq!(r.final_state, LoopState::Escalated);
        assert_eq!(r.stop_reason, "escalated");
        assert_eq!(r.iterations.len(), 2);
    }

    #[test]
    fn no_progress_escalates_before_retries_are_exhausted() {
        let f = Fake::new(vec![vr("tests", false), vr("tests", false)]);
        let r = runtime(ok_agent(), &f)
            .start(&definition(10, 5, &[], &[]), "run_7", None)
            .unwrap();
        assert_eq!(r.final_state, LoopState::Escalated);
        assert_eq!(r.stop_reason, "no_progress");
        assert_eq!(r.iterations.len(), 2);
        assert_eq!(f.calls(), 2);
    }

    #[test]
    fn budget_exceeded_stops_without_calling_verifiers() {
        let f = Fake::new(vec![vr("tests", true)]);
        let r = runtime(ok_agent(), &f)
            .start(&definition(0, 2, &[], &[]), "run_4", None)
            .unwrap();
        assert_eq!(r.final_state, LoopState::Stopped);
        assert_eq!(r.stop_reason, "budget_exceeded");
        assert_eq!(f.calls(), 0);
        assert_eq!(r.iterations[0].budget["overall"], "exceeded");
    }

    #[test]
    fn agent_failure_results_in_failed_state() {
        let f = Fake::new(vec![]);
        let agent: AgentFn = Box::new(|_| {
            Err(AgentFailure {
                message: "boom".into(),
                cost_usd: None,
            })
        });
        let r = runtime(agent, &f)
            .start(&definition(10, 2, &[], &[]), "run_5", None)
            .unwrap();
        assert_eq!(r.final_state, LoopState::Failed);
        assert_eq!(r.stop_reason, "agent_failed");
        assert_eq!(f.calls(), 0);
    }

    #[test]
    fn policy_violation_prevents_start() {
        let f = Fake::new(vec![vr("tests", true)]);
        let called = Arc::new(Mutex::new(0));
        let c = called.clone();
        let agent: AgentFn = Box::new(move |_| {
            *c.lock().unwrap() += 1;
            Ok(AgentSuccess::default())
        });
        let err = runtime(agent, &f)
            .start(&definition(10, 2, &["merge"], &[]), "run_p1", None)
            .unwrap_err();
        assert!(matches!(err, RuntimeError::Policy(_)));
        assert!(err.to_string().contains("merge"));
        assert_eq!(*called.lock().unwrap(), 0);
        assert_eq!(f.calls(), 0);
    }

    #[test]
    fn gated_l3_action_allows_start() {
        let f = Fake::new(vec![vr("tests", true)]);
        let r = runtime(ok_agent(), &f)
            .start(&definition(10, 2, &["merge"], &["merge"]), "run_p2", None)
            .unwrap();
        assert_eq!(r.final_state, LoopState::Completed);
    }

    #[test]
    fn events_are_written_to_the_injected_dir() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(vec![vr("tests", true)]);
        LoopRuntime::new(ok_agent(), vec![Box::new(f)])
            .with_events_dir(d.path().to_path_buf())
            .start(&definition(10, 2, &[], &[]), "run_6", None)
            .unwrap();
        let files: Vec<_> = std::fs::read_dir(d.path()).unwrap().collect();
        assert_eq!(files.len(), 1);
        let text = std::fs::read_to_string(files[0].as_ref().unwrap().path()).unwrap();
        let events: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let types: Vec<&str> = events
            .iter()
            .map(|e| e["event_type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            [
                "policy.allowed",
                "loop.started",
                "iteration.started",
                "verification.started",
                "verification.passed",
                "iteration.completed",
                "loop.completed"
            ]
        );
        assert!(events.iter().all(|e| e["run_id"] == "run_6"));
    }

    #[test]
    fn on_iteration_fires_once_per_iteration_with_a_snapshot() {
        let f = Fake::new(vec![vr("tests", false), vr("lint", true)]);
        let calls: Arc<Mutex<Vec<Call>>> = Arc::default();
        let c = calls.clone();
        let mut rt = runtime(ok_agent(), &f).with_on_iteration(Box::new(move |r, l, n, its| {
            c.lock()
                .unwrap()
                .push((r.into(), l.into(), n.into(), its.len()));
        }));
        let result = rt
            .start(&definition(10, 3, &[], &[]), "run_oi", Some("loop_fixed"))
            .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(result.loop_id, "loop_fixed");
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            ("run_oi".into(), "loop_fixed".into(), "test-loop".into(), 1)
        );
        assert_eq!(calls[1].3, 2);
    }

    #[test]
    fn unreported_cost_is_distinguished_from_zero() {
        let none = LoopRuntime::new(ok_agent(), vec![])
            .with_events(Box::new(NoEvents))
            .start(&definition(10, 2, &[], &[]), "run_nocost", None)
            .unwrap();
        assert_eq!(none.cost_reported, Some(false));
        let known: AgentFn = Box::new(|_| {
            Ok(AgentSuccess {
                cost_usd: Some(0.5),
            })
        });
        let r = LoopRuntime::new(known, vec![])
            .with_events(Box::new(NoEvents))
            .start(&definition(10, 2, &[], &[]), "run_known", None)
            .unwrap();
        assert_eq!(r.cost_reported, Some(true));
        assert_eq!(r.iterations[0].budget["cost"]["used_usd"], 0.5);
    }

    #[test]
    fn failed_call_cost_is_counted() {
        let agent: AgentFn = Box::new(|_| {
            Err(AgentFailure {
                message: "budget stop".into(),
                cost_usd: Some(3.0),
            })
        });
        let r = LoopRuntime::new(agent, vec![])
            .with_events(Box::new(NoEvents))
            .start(&definition(10, 2, &[], &[]), "run_failcost", None)
            .unwrap();
        assert_eq!(r.final_state, LoopState::Failed);
        assert_eq!(r.iterations.last().unwrap().budget["cost"]["used_usd"], 3.0);
        assert_eq!(r.cost_reported, Some(true));
    }

    #[test]
    fn agent_sees_the_previous_iteration_on_retry() {
        let f = Fake::new(vec![vr("tests", false), vr("lint", true)]);
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let s = seen.clone();
        let agent: AgentFn = Box::new(move |ctx| {
            s.lock()
                .unwrap()
                .push((ctx.iteration, ctx.previous.map(|p| p.iteration)));
            Ok(AgentSuccess::default())
        });
        runtime(agent, &f)
            .start(&definition(10, 3, &[], &[]), "run_ctx", None)
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), [(1, None), (2, Some(1))]);
    }
}
