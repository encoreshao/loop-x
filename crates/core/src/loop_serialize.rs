//! `result.json` persistence and the ledger-backed run summaries. Mirrors
//! `bin/loop_serialize.py`: `<results_dir>/<run_id>/result.json`, one plain
//! JSON file per run, written atomically (temp file + rename).

use crate::config::home_dir;
use crate::events::{self, fmt_timestamp, EventFields};
use crate::ledger::{self, RunRecord};
use crate::loop_result::{AgentCost, LoopResult};
use crate::pyjson;
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// `<LOOP_ENGINEERING_HOME>/outputs/loop-runs`.
pub fn default_results_dir() -> PathBuf {
    home_dir().join("outputs").join("loop-runs")
}

/// Python truthiness of a JSON value.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `x or {}` for a field expected to be an object.
fn obj_or_empty(v: Option<&Value>) -> Value {
    match v {
        Some(x) if truthy(x) => x.clone(),
        _ => json!({}),
    }
}

/// `a or b` for JSON values.
fn or_else(v: Option<&Value>, fallback: Value) -> Value {
    match v {
        Some(x) if truthy(x) => x.clone(),
        _ => fallback,
    }
}

/// The JSON dict for a result (`dataclasses.asdict` shape).
pub fn to_json_dict(result: &LoopResult) -> Value {
    serde_json::to_value(result).expect("LoopResult always serializes")
}

/// Callback used to emit the terminal `loop.result` event:
/// `(event_type, run_id, data, events_dir)`.
pub type EmitFn<'a> = dyn Fn(&str, &str, Value, Option<&Path>) -> Result<(), String> + 'a;

/// Write `result.json` for a run; for finished results also emit a
/// `loop.result` event (best-effort: an emit failure never fails the write).
pub fn write_result(
    result: &LoopResult,
    results_dir: Option<&Path>,
    events_dir: Option<&Path>,
    emit: Option<&EmitFn>,
) -> std::io::Result<PathBuf> {
    let results_dir = results_dir.map_or_else(default_results_dir, Path::to_path_buf);
    let run_dir = results_dir.join(&result.run_id);
    std::fs::create_dir_all(&run_dir)?;
    let path = run_dir.join("result.json");
    let tmp = run_dir.join("result.json.tmp");
    let data = to_json_dict(result);
    std::fs::write(&tmp, pyjson::to_string_pretty(&data))?;
    std::fs::rename(&tmp, &path)?;
    if result.status == "finished" {
        emit_loop_result(result, &data, events_dir, emit);
    }
    Ok(path)
}

fn emit_loop_result(
    result: &LoopResult,
    data: &Value,
    events_dir: Option<&Path>,
    emit: Option<&EmitFn>,
) {
    let Ok(summary) = result_summary(
        data,
        CostHint {
            agent_cost: result.agent_cost_usd,
            cost_reported: result.cost_reported,
        },
    ) else {
        return;
    };
    let _ = match emit {
        Some(f) => f("loop.result", &result.run_id, summary, events_dir),
        None => events::emit(
            "loop.result",
            &result.run_id,
            EventFields {
                data: Some(summary),
                ..Default::default()
            },
            events_dir,
        )
        .map(|_| ())
        .map_err(|e| e.to_string()),
    };
}

/// The runner-side cost attributes a live result may carry (absent on
/// results read back from `result.json`).
#[derive(Debug, Clone, Copy, Default)]
pub struct CostHint {
    pub agent_cost: AgentCost,
    pub cost_reported: Option<bool>,
}

/// A serialized verification result's real outcome: observe mode forces
/// `passed` true and keeps the underlying result in `evidence.observed_passed`.
pub fn effective_passed(verification: &Value) -> Value {
    let evidence = obj_or_empty(verification.get("evidence"));
    match evidence.get("observed_passed") {
        Some(v) => v.clone(),
        None => verification.get("passed").cloned().unwrap_or(Value::Null),
    }
}

fn all_effective_passed(verification: &[Value]) -> bool {
    verification.iter().all(|v| truthy(&effective_passed(v)))
}

fn iter_verification(it: &Value) -> Vec<Value> {
    match it.get("verification_results") {
        Some(Value::Array(a)) => a.clone(),
        _ => vec![],
    }
}

fn is_verified_successful(data: &Value, iterations: &[Value]) -> bool {
    if data["final_state"] != "completed" || iterations.is_empty() {
        return false;
    }
    all_effective_passed(&iter_verification(&iterations[iterations.len() - 1]))
}

fn get<'a>(data: &'a Value, key: &str) -> Result<&'a Value, String> {
    data.get(key).ok_or_else(|| format!("missing key {key:?}"))
}

/// Compact `loop.result` event payload, mapped field by field. `data` is the
/// serialized result (live or read back); errors on a missing required key.
pub fn result_summary(data: &Value, hint: CostHint) -> Result<Value, String> {
    let cost_unknown = match hint.agent_cost {
        AgentCost::Unknown => true,
        AgentCost::Unset => hint.cost_reported == Some(false),
        AgentCost::Usd(_) => false,
    };
    let iterations = get(data, "iterations")?
        .as_array()
        .ok_or("iterations is not a list")?
        .clone();
    let mut out_iters = Vec::new();
    for it in &iterations {
        let budget = obj_or_empty(it.get("budget"));
        let cost_usd = if cost_unknown {
            Value::Null
        } else {
            let cost = obj_or_empty(budget.get("cost"));
            or_else(cost.get("used_usd"), json!(0))
        };
        out_iters.push(json!({
            "n": it.get("iteration").cloned().unwrap_or(Value::Null),
            "state": it.get("state").cloned().unwrap_or(Value::Null),
            "verifiers_passed": all_effective_passed(&iter_verification(it)),
            "cost_usd": cost_usd,
        }));
    }
    let last_budget = match iterations.last() {
        Some(it) => obj_or_empty(it.get("budget")),
        None => json!({}),
    };
    let used_seconds = or_else(
        obj_or_empty(last_budget.get("runtime")).get("used_seconds"),
        json!(0),
    )
    .as_f64()
    .unwrap_or(0.0);
    let duration_ms = (used_seconds * 1000.0).round_ties_even() as i64;
    let total_cost = if cost_unknown {
        Value::Null
    } else if let AgentCost::Usd(c) = hint.agent_cost {
        json!(c)
    } else {
        or_else(
            obj_or_empty(last_budget.get("cost")).get("used_usd"),
            json!(0),
        )
    };
    let finished = Utc::now();
    let started = finished - Duration::milliseconds(duration_ms);
    let budget_overall = if truthy(&last_budget) {
        match last_budget.get("overall") {
            None => json!("ok"),
            Some(Value::String(s)) => json!(s),
            Some(Value::Null) => json!("None"),
            Some(other) => json!(other.to_string()),
        }
    } else {
        Value::Null
    };
    Ok(json!({
        "run_id": get(data, "run_id")?,
        "loop_id": get(data, "loop_id")?,
        "definition": get(data, "definition_name")?,
        "final_state": get(data, "final_state")?,
        "stop_reason": get(data, "stop_reason")?,
        "started_at": fmt_timestamp(started),
        "finished_at": fmt_timestamp(finished),
        "duration_ms": duration_ms,
        "total_cost_usd": total_cost,
        "iterations": out_iters,
        "verified_success": is_verified_successful(data, &iterations),
        "budget_overall": budget_overall,
    }))
}

pub fn read_result(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// Every `<results_dir>/*/result.json`, sorted by path.
pub fn list_results(results_dir: Option<&Path>) -> Vec<PathBuf> {
    let dir = results_dir.map_or_else(default_results_dir, Path::to_path_buf);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .map(|e| e.path().join("result.json"))
        .filter(|p| p.exists())
        .collect();
    out.sort();
    out
}

/// The most recently modified `result.json`, if any.
pub fn find_latest_result(results_dir: Option<&Path>) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for p in list_results(results_dir) {
        let Ok(m) = std::fs::metadata(&p).and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(bm, _)| m > *bm) {
            best = Some((m, p));
        }
    }
    best.map(|(_, p)| p)
}

/// Only runs backed by a `loop.result` event count; default is the ledger.
fn complete_runs(runs: Option<&[RunRecord]>) -> Vec<RunRecord> {
    let all = match runs {
        Some(r) => r.to_vec(),
        None => ledger::iter_runs(&Default::default()),
    };
    all.into_iter()
        .filter(|r| r.complete && r.has_result)
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResultsSummary {
    pub total_runs: usize,
    pub success_rate: Option<f64>,
    pub escalation_rate: Option<f64>,
    pub average_cost_usd: Option<f64>,
    pub efficiency_score: Option<f64>,
}

/// Overview across ledger runs plus the experimental Loop Efficiency Score:
/// verified-successful runs / (total cost * total hours * total iterations).
/// Everything is `None` (not 0) when there is nothing to divide by.
pub fn summarize_results(runs: Option<&[RunRecord]>) -> ResultsSummary {
    let runs = complete_runs(runs);
    let total = runs.len();
    if total == 0 {
        return ResultsSummary {
            total_runs: 0,
            success_rate: None,
            escalation_rate: None,
            average_cost_usd: None,
            efficiency_score: None,
        };
    }
    let count = |s: &str| {
        runs.iter()
            .filter(|r| r.final_state.as_deref() == Some(s))
            .count()
    };
    let total_cost: f64 = runs.iter().map(|r| r.total_cost_usd.unwrap_or(0.0)).sum();
    let total_seconds: f64 = runs
        .iter()
        .map(|r| r.duration_ms.unwrap_or(0) as f64 / 1000.0)
        .sum();
    let total_iterations: usize = runs.iter().map(|r| r.iterations.len()).sum();
    let verified = runs.iter().filter(|r| r.verified_success).count();
    let hours = total_seconds / 3600.0;
    let efficiency_score = if total_cost != 0.0 && hours != 0.0 && total_iterations != 0 {
        Some(verified as f64 / (total_cost * hours * total_iterations as f64))
    } else {
        None
    };
    ResultsSummary {
        total_runs: total,
        success_rate: Some(count("completed") as f64 / total as f64),
        escalation_rate: Some(count("escalated") as f64 / total as f64),
        average_cost_usd: Some(total_cost / total as f64),
        efficiency_score,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunCosts {
    pub total_runs: usize,
    pub total_cost_usd: f64,
    pub cost_per_run_usd: Option<f64>,
}

pub fn summarize_run_costs(runs: Option<&[RunRecord]>) -> RunCosts {
    let runs = complete_runs(runs);
    let total: f64 = runs.iter().map(|r| r.total_cost_usd.unwrap_or(0.0)).sum();
    RunCosts {
        total_runs: runs.len(),
        total_cost_usd: total,
        cost_per_run_usd: (!runs.is_empty()).then(|| total / runs.len() as f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loop_result::{IterationResult, VerificationResult};
    use crate::loop_state::LoopState;

    fn sample(run_id: &str, state: LoopState, cost: f64, secs: f64, verified: bool) -> LoopResult {
        let it = IterationResult {
            iteration: 1,
            state,
            verification_results: vec![VerificationResult {
                name: "tests".into(),
                passed: verified,
                exit_code: Some(if verified { 0 } else { 1 }),
                duration_ms: 12,
                output: "ok".into(),
                evidence: json!({"command": "true"}),
            }],
            budget: json!({"overall": "ok", "cost": {"used_usd": cost}, "runtime": {"used_seconds": secs}}),
            progressed: true,
        };
        LoopResult::new(
            "loop_1",
            run_id,
            "test-loop",
            state,
            vec![it],
            state.as_str(),
        )
    }

    fn quiet() -> Box<EmitFn<'static>> {
        Box::new(|_, _, _, _| Ok(()))
    }

    #[test]
    fn write_and_read_round_trip_with_python_json_layout() {
        let d = tempfile::tempdir().unwrap();
        let r = sample("run_1", LoopState::Completed, 0.0, 0.0, true);
        let p = write_result(&r, Some(d.path()), None, Some(&*quiet())).unwrap();
        assert_eq!(p, d.path().join("run_1").join("result.json"));
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("{\n  \"loop_id\": \"loop_1\","));
        assert!(!d.path().join("run_1").join("result.json.tmp").exists());
        assert_eq!(read_result(&p).unwrap(), to_json_dict(&r));
    }

    #[test]
    fn list_and_latest() {
        let d = tempfile::tempdir().unwrap();
        assert!(list_results(Some(d.path())).is_empty());
        assert!(find_latest_result(Some(d.path())).is_none());
        for id in ["b", "a"] {
            write_result(
                &sample(id, LoopState::Completed, 0.0, 0.0, true),
                Some(d.path()),
                None,
                Some(&*quiet()),
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let names: Vec<_> = list_results(Some(d.path()))
            .iter()
            .map(|p| {
                p.parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, ["a", "b"]);
        let latest = find_latest_result(Some(d.path())).unwrap();
        assert!(latest.ends_with("a/result.json"));
    }

    #[test]
    fn write_result_emits_compact_loop_result_only_when_finished() {
        let d = tempfile::tempdir().unwrap();
        let got = std::cell::RefCell::new(Vec::new());
        let cb = |t: &str, run: &str, data: Value, _: Option<&Path>| {
            got.borrow_mut()
                .push((t.to_string(), run.to_string(), data));
            Ok(())
        };
        let r = sample("run_e", LoopState::Completed, 0.5, 30.0, true);
        write_result(&r, Some(d.path()), None, Some(&cb)).unwrap();
        let (t, run, data) = got.borrow()[0].clone();
        assert_eq!((t.as_str(), run.as_str()), ("loop.result", "run_e"));
        assert_eq!(data["definition"], "test-loop");
        assert_eq!(data["final_state"], "completed");
        assert_eq!(data["total_cost_usd"], 0.5);
        assert_eq!(data["duration_ms"], 30000);
        assert_eq!(data["verified_success"], true);
        assert_eq!(data["budget_overall"], "ok");
        assert_eq!(
            data["iterations"],
            json!([{"n": 1, "state": "completed", "verifiers_passed": true, "cost_usd": 0.5}])
        );

        got.borrow_mut().clear();
        let mut running = sample("run_r", LoopState::Executing, 0.0, 0.0, true);
        running.status = "running".into();
        write_result(&running, Some(d.path()), None, Some(&cb)).unwrap();
        assert!(got.borrow().is_empty());
    }

    #[test]
    fn write_result_emits_real_event_by_default_and_survives_failure() {
        let d = tempfile::tempdir().unwrap();
        let ev = d.path().join("events");
        let r = sample("run_d", LoopState::Completed, 0.25, 1.0, true);
        write_result(&r, Some(&d.path().join("res")), Some(&ev), None).unwrap();
        let runs = ledger::iter_runs(&ledger::RunQuery {
            events_dir: Some(&ev),
            ..Default::default()
        });
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].total_cost_usd, Some(0.25));
        let boom = |_: &str, _: &str, _: Value, _: Option<&Path>| Err("x".to_string());
        assert!(write_result(&r, Some(&d.path().join("res2")), None, Some(&boom)).is_ok());
    }

    #[test]
    fn summary_unverified_and_cost_hints() {
        let bad = to_json_dict(&sample("r", LoopState::Completed, 0.0, 0.0, false));
        let s = result_summary(&bad, CostHint::default()).unwrap();
        assert_eq!(s["verified_success"], false);
        assert_eq!(s["iterations"][0]["verifiers_passed"], false);

        let d = to_json_dict(&sample("r", LoopState::Completed, 0.0, 0.0, true));
        let unknown = CostHint {
            agent_cost: AgentCost::Unknown,
            cost_reported: None,
        };
        let s = result_summary(&d, unknown).unwrap();
        assert!(s["total_cost_usd"].is_null() && s["iterations"][0]["cost_usd"].is_null());
        let usd = CostHint {
            agent_cost: AgentCost::Usd(1.25),
            cost_reported: None,
        };
        assert_eq!(result_summary(&d, usd).unwrap()["total_cost_usd"], 1.25);
        let unreported = CostHint {
            agent_cost: AgentCost::Unset,
            cost_reported: Some(false),
        };
        assert!(result_summary(&d, unreported).unwrap()["total_cost_usd"].is_null());
        let hand = to_json_dict(&sample("r", LoopState::Completed, 0.75, 0.0, true));
        assert_eq!(
            result_summary(&hand, CostHint::default()).unwrap()["total_cost_usd"],
            0.75
        );
        // Python: `0.0 or 0` is the integer 0
        assert_eq!(
            result_summary(&d, CostHint::default()).unwrap()["total_cost_usd"].to_string(),
            "0"
        );
    }

    #[test]
    fn observed_failure_is_not_verified_successful() {
        let mut r = sample("r", LoopState::Completed, 1.0, 3600.0, true);
        r.iterations[0].verification_results[0].evidence =
            json!({"observed_passed": false, "mode": "observe"});
        let s = result_summary(&to_json_dict(&r), CostHint::default()).unwrap();
        assert_eq!(s["verified_success"], false);
        let d = tempfile::tempdir().unwrap();
        let ev = d.path();
        write_result(&r, Some(&d.path().join("res")), Some(ev), None).unwrap();
        let runs = ledger::iter_runs(&ledger::RunQuery {
            events_dir: Some(ev),
            ..Default::default()
        });
        assert_eq!(summarize_results(Some(&runs)).efficiency_score, Some(0.0));
    }

    #[test]
    fn summaries_empty_and_populated() {
        let e = summarize_results(Some(&[]));
        assert_eq!(e.total_runs, 0);
        assert!(e.success_rate.is_none() && e.efficiency_score.is_none());
        let c = summarize_run_costs(Some(&[]));
        assert_eq!(
            (c.total_runs, c.total_cost_usd, c.cost_per_run_usd),
            (0, 0.0, None)
        );

        let d = tempfile::tempdir().unwrap();
        let ev = d.path();
        for (id, st, cost) in [
            ("a", LoopState::Completed, 1.0),
            ("b", LoopState::Escalated, 3.0),
        ] {
            write_result(
                &sample(id, st, cost, 3600.0, true),
                Some(&d.path().join("res")),
                Some(ev),
                None,
            )
            .unwrap();
        }
        let runs = ledger::iter_runs(&ledger::RunQuery {
            events_dir: Some(ev),
            ..Default::default()
        });
        let s = summarize_results(Some(&runs));
        assert_eq!(s.total_runs, 2);
        assert_eq!(s.success_rate, Some(0.5));
        assert_eq!(s.escalation_rate, Some(0.5));
        assert_eq!(s.average_cost_usd, Some(2.0));
        // 1 verified / (4.0 usd * 2.0 h * 2 iterations)
        assert_eq!(s.efficiency_score, Some(1.0 / 16.0));
        let c = summarize_run_costs(Some(&runs));
        assert_eq!(
            (c.total_runs, c.total_cost_usd, c.cost_per_run_usd),
            (2, 4.0, Some(2.0))
        );
    }

    #[test]
    fn efficiency_none_without_cost_or_duration() {
        let d = tempfile::tempdir().unwrap();
        write_result(
            &sample("a", LoopState::Completed, 0.0, 0.0, true),
            Some(&d.path().join("r")),
            Some(d.path()),
            None,
        )
        .unwrap();
        let runs = ledger::iter_runs(&ledger::RunQuery {
            events_dir: Some(d.path()),
            ..Default::default()
        });
        assert!(summarize_results(Some(&runs)).efficiency_score.is_none());
    }
}
