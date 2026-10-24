//! The scripted evaluation harness (`loop eval`). Mirrors `bin/loop_eval.py`:
//! drives the real, unmodified [`LoopRuntime`] with a scripted agent and
//! scripted verifiers, so a passing case is evidence about the runtime's
//! stop/verify/escalate/cost behaviour, not about this harness.

use crate::loop_definition::LoopDefinition;
use crate::loop_result::VerificationResult;
use crate::loop_runtime::{
    AgentFailure, AgentFn, AgentSuccess, LoopContext, LoopRuntime, RuntimeError,
};
use crate::loop_verifiers::Verifier;
use crate::loops_config::py_repr;
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A case file is broken (not "the case failed"): a script ran out.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct ScriptExhausted(pub String);

/// Shared slot a scripted agent/verifier writes to when its script runs out;
/// the runtime has no way to propagate a Python-style raise.
pub type ExhaustedSlot = Arc<Mutex<Option<ScriptExhausted>>>;

fn truthy(v: &Value) -> bool {
    crate::loop_serialize::truthy(v)
}

/// `(changed, cost_usd)` of one scripted agent call.
pub type ScriptedCall = (bool, Option<f64>);

/// A scripted agent: pops the next `{changed, cost_usd}` entry per call.
pub struct ScriptedAgent {
    script: VecDeque<Value>,
    calls: usize,
    total: usize,
    exhausted: ExhaustedSlot,
}

impl ScriptedAgent {
    pub fn new(script: Vec<Value>, exhausted: ExhaustedSlot) -> Self {
        let total = script.len();
        Self {
            script: script.into(),
            calls: 0,
            total,
            exhausted,
        }
    }

    /// Next entry as `(changed, cost_usd)`.
    pub fn next_call(&mut self) -> Result<ScriptedCall, ScriptExhausted> {
        let Some(entry) = self.script.pop_front() else {
            return Err(ScriptExhausted(format!(
                "ScriptedAgent has no entry for call {} - only {} scripted",
                self.calls + 1,
                self.total
            )));
        };
        self.calls += 1;
        let changed = entry.get("changed").is_none_or(truthy);
        let cost = entry.get("cost_usd").and_then(Value::as_f64);
        Ok((changed, cost))
    }

    pub fn into_agent_fn(mut self) -> AgentFn {
        Box::new(move |_ctx: &LoopContext<'_>| match self.next_call() {
            Ok((_, cost_usd)) => Ok(AgentSuccess { cost_usd }),
            Err(e) => {
                let message = e.to_string();
                *self.exhausted.lock().unwrap() = Some(e);
                Err(AgentFailure {
                    message,
                    cost_usd: None,
                })
            }
        })
    }
}

/// A scripted verifier: pops the next pass/fail per call, for one name.
pub struct ScriptedVerifier {
    name: String,
    script: Mutex<VecDeque<bool>>,
    calls: Mutex<usize>,
    total: usize,
    exhausted: ExhaustedSlot,
}

impl ScriptedVerifier {
    pub fn new(name: &str, script: Vec<bool>, exhausted: ExhaustedSlot) -> Self {
        Self {
            name: name.to_string(),
            total: script.len(),
            script: Mutex::new(script.into()),
            calls: Mutex::new(0),
            exhausted,
        }
    }
}

impl Verifier for ScriptedVerifier {
    fn verify(&self, _context: &LoopContext<'_>) -> VerificationResult {
        let next = self.script.lock().unwrap().pop_front();
        let mut calls = self.calls.lock().unwrap();
        let passed = match next {
            Some(p) => {
                *calls += 1;
                p
            }
            None => {
                *self.exhausted.lock().unwrap() = Some(ScriptExhausted(format!(
                    "ScriptedVerifier {} has no entry for call {} - only {} scripted",
                    crate::pydumps::py_repr_str(&self.name),
                    *calls + 1,
                    self.total
                )));
                false
            }
        };
        VerificationResult {
            name: self.name.clone(),
            passed,
            exit_code: Some(if passed { 0 } else { 1 }),
            duration_ms: 0,
            output: String::new(),
            evidence: json!({}),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EvalCase {
    pub name: String,
    pub description: String,
    pub definition: LoopDefinition,
    pub agent_script: Vec<Value>,
    /// Verifier name -> scripted outcomes, in file order.
    pub verifier_scripts: Vec<(String, Vec<Value>)>,
    pub expect: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvalOutcome {
    pub case_name: String,
    pub passed: bool,
    pub expected: Map<String, Value>,
    pub actual: Map<String, Value>,
    pub detail: String,
}

const REQUIRED_CASE_FIELDS: [&str; 5] = ["name", "description", "definition", "script", "expect"];

#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("{0}")]
    Case(String),
    #[error(transparent)]
    Exhausted(#[from] ScriptExhausted),
}

/// Parse one case from YAML text; `origin` names it in error messages.
pub fn parse_case(text: &str, origin: &str) -> Result<EvalCase, EvalError> {
    let data: Value = serde_norway::from_str(text)
        .map_err(|e| EvalError::Case(format!("{origin}: invalid YAML: {e}")))?;
    let Some(obj) = data.as_object() else {
        return Err(EvalError::Case(format!(
            "eval case {origin}: top level must be a mapping"
        )));
    };
    for key in REQUIRED_CASE_FIELDS {
        if !obj.contains_key(key) {
            return Err(EvalError::Case(format!(
                "eval case {origin}: missing required field '{key}'"
            )));
        }
    }
    let script = obj["script"].as_object().cloned().unwrap_or_default();
    let definition = LoopDefinition::from_value(&obj["definition"])
        .map_err(|e| EvalError::Case(e.to_string()))?;
    let verifier_scripts = script
        .get("verifiers")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_array().cloned().unwrap_or_default()))
                .collect()
        })
        .unwrap_or_default();
    Ok(EvalCase {
        name: py_text(&obj["name"]),
        description: py_text(&obj["description"]),
        definition,
        agent_script: script
            .get("agent")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        verifier_scripts,
        expect: obj["expect"].as_object().cloned().unwrap_or_default(),
    })
}

fn py_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => crate::pydumps::py_str(other),
    }
}

pub fn load_case(path: &Path) -> Result<EvalCase, EvalError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| EvalError::Case(format!("cannot read {}: {e}", path.display())))?;
    parse_case(&text, &path.display().to_string())
}

/// Every `*.yaml` in `dir`, sorted by path.
pub fn load_cases(dir: &Path) -> Result<Vec<EvalCase>, EvalError> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
        .collect();
    paths.sort();
    paths.iter().map(|p| load_case(p)).collect()
}

/// The cases compiled into the binary (the repo's `evals/cases`).
pub fn builtin_cases() -> Result<Vec<EvalCase>, EvalError> {
    crate::loop_templates::EVAL_CASES
        .iter()
        .map(|(name, text)| parse_case(text, name))
        .collect()
}

/// `actual.get(key) == value`, with Python's `2 == 2.0`.
fn same(actual: Option<&Value>, expected: &Value) -> bool {
    match (actual, expected) {
        (Some(Value::Number(a)), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Some(a), b) => a == b,
        (None, _) => false,
    }
}

pub fn run_case(case: &EvalCase, events_dir: &Path) -> Result<EvalOutcome, EvalError> {
    let exhausted: ExhaustedSlot = Arc::new(Mutex::new(None));
    let verifiers: Vec<Box<dyn Verifier>> = case
        .verifier_scripts
        .iter()
        .map(|(name, script)| {
            Box::new(ScriptedVerifier::new(
                name,
                script.iter().map(truthy).collect(),
                exhausted.clone(),
            )) as Box<dyn Verifier>
        })
        .collect();
    let agent = ScriptedAgent::new(case.agent_script.clone(), exhausted.clone());
    let mut runtime = LoopRuntime::new(agent.into_agent_fn(), verifiers)
        .with_events_dir(events_dir.to_path_buf());

    let run_id = format!("eval-{}", case.name);
    let started = runtime.start(&case.definition, &run_id, None);
    // A script that ran out means the case file is broken: propagate, don't grade.
    if let Some(e) = exhausted.lock().unwrap().take() {
        return Err(e.into());
    }
    let mut actual = Map::new();
    match started {
        Err(RuntimeError::Policy(_)) => {
            actual.insert("final_state".into(), json!("blocked"));
            actual.insert("stop_reason".into(), json!("policy_denied"));
        }
        Err(e @ RuntimeError::Transition(_)) => return Err(EvalError::Case(e.to_string())),
        Ok(result) => {
            actual.insert("final_state".into(), json!(result.final_state.as_str()));
            actual.insert("stop_reason".into(), json!(result.stop_reason));
            actual.insert("iterations".into(), json!(result.iterations.len()));
        }
    }
    let passed = case.expect.iter().all(|(k, v)| same(actual.get(k), v));
    let detail = if passed {
        String::new()
    } else {
        format!(
            "expected {}, got {}",
            py_repr(&Value::Object(case.expect.clone())),
            py_repr(&Value::Object(actual.clone()))
        )
    };
    Ok(EvalOutcome {
        case_name: case.name.clone(),
        passed,
        expected: case.expect.clone(),
        actual,
        detail,
    })
}

/// Run `cases` against a throwaway events dir.
pub fn run_cases(cases: &[EvalCase]) -> Result<Vec<EvalOutcome>, EvalError> {
    let dir = std::env::temp_dir().join(format!("loopx-eval-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).map_err(|e| EvalError::Case(e.to_string()))?;
    let outcomes: Result<Vec<_>, _> = cases.iter().map(|c| run_case(c, &dir)).collect();
    let _ = std::fs::remove_dir_all(&dir);
    outcomes
}

/// `run_all(cases_dir)`.
pub fn run_all(dir: &Path) -> Result<Vec<EvalOutcome>, EvalError> {
    run_cases(&load_cases(dir)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot() -> ExhaustedSlot {
        Arc::new(Mutex::new(None))
    }

    fn case_yaml(expect: &str, script: &str) -> String {
        format!(
            "name: t\ndescription: d\ndefinition:\n  name: eval-t\n  version: 1\n  trigger: {{type: manual}}\n  goal: {{type: fix}}\nscript:\n{script}\nexpect:\n{expect}\n"
        )
    }

    fn run(text: &str) -> Result<EvalOutcome, EvalError> {
        let case = parse_case(text, "t.yaml")?;
        let dir =
            std::env::temp_dir().join(format!("loopx-eval-t-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = run_case(&case, &dir);
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn scripted_agent_returns_entries_in_order_then_runs_out() {
        let mut agent = ScriptedAgent::new(
            vec![
                json!({"changed": true, "cost_usd": 0.10}),
                json!({"changed": false, "cost_usd": 0.0}),
            ],
            slot(),
        );
        assert_eq!(agent.next_call().unwrap(), (true, Some(0.10)));
        assert_eq!(agent.next_call().unwrap(), (false, Some(0.0)));
        let err = agent.next_call().unwrap_err();
        assert_eq!(
            err.to_string(),
            "ScriptedAgent has no entry for call 3 - only 2 scripted"
        );
    }

    #[test]
    fn scripted_verifier_yields_exit_codes_and_runs_out() {
        let ex = slot();
        let v = ScriptedVerifier::new("tests", vec![true, false], ex.clone());
        let def = LoopDefinition::from_value(
            &json!({"name":"x","version":1,"trigger":{"type":"manual"},"goal":{"type":"fix"}}),
        )
        .unwrap();
        let ctx = LoopContext {
            iteration: 1,
            definition: &def,
            previous: None,
        };
        let a = v.verify(&ctx);
        assert!(a.passed);
        assert_eq!(a.exit_code, Some(0));
        let b = v.verify(&ctx);
        assert!(!b.passed);
        assert_eq!(b.exit_code, Some(1));
        assert!(ex.lock().unwrap().is_none());
        v.verify(&ctx);
        assert_eq!(
            ex.lock().unwrap().as_ref().unwrap().to_string(),
            "ScriptedVerifier 'tests' has no entry for call 3 - only 2 scripted"
        );
    }

    #[test]
    fn parse_requires_every_field() {
        let err = parse_case("name: a\n", "x.yaml").unwrap_err();
        assert_eq!(
            err.to_string(),
            "eval case x.yaml: missing required field 'description'"
        );
    }

    #[test]
    fn passing_case_reports_actual_state() {
        let text = case_yaml(
            "  final_state: completed\n  iterations: 1",
            "  agent:\n    - {changed: true, cost_usd: 0.05}\n  verifiers:\n    tests: [true]",
        );
        let out = run(&text).unwrap();
        assert!(out.passed, "{}", out.detail);
        assert_eq!(out.actual["iterations"], json!(1));
        assert_eq!(out.detail, "");
    }

    #[test]
    fn wrong_expectation_fails_with_python_style_detail() {
        let text = case_yaml(
            "  final_state: escalated\n  stop_reason: no_progress",
            "  agent:\n    - {changed: true, cost_usd: 0.0}\n  verifiers:\n    tests: [true]",
        );
        let out = run(&text).unwrap();
        assert!(!out.passed);
        assert_eq!(
            out.detail,
            "expected {'final_state': 'escalated', 'stop_reason': 'no_progress'}, got {'final_state': 'completed', 'stop_reason': 'completed', 'iterations': 1}"
        );
    }

    #[test]
    fn policy_denial_is_a_blocked_outcome() {
        let text = "name: t\ndescription: d\ndefinition:\n  name: e\n  version: 1\n  trigger: {type: manual}\n  goal: {type: fix}\n  actions: [merge]\nscript:\n  agent: []\nexpect:\n  final_state: blocked\n  stop_reason: policy_denied\n";
        assert!(run(text).unwrap().passed);
    }

    #[test]
    fn an_exhausted_script_is_an_error_not_a_failed_case() {
        let text = case_yaml(
            "  final_state: completed",
            "  agent: []\n  verifiers:\n    tests: [true]",
        );
        match run(&text) {
            Err(EvalError::Exhausted(e)) => assert!(e.0.contains("ScriptedAgent has no entry")),
            other => panic!("expected ScriptExhausted, got {other:?}"),
        }
    }

    #[test]
    fn every_builtin_case_passes() {
        let cases = builtin_cases().unwrap();
        assert_eq!(cases.len(), 8);
        for o in run_cases(&cases).unwrap() {
            assert!(o.passed, "{}: {}", o.case_name, o.detail);
        }
    }
}
