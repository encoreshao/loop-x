//! `BudgetController` and the Budget page rollups (mirrors `bin/loop_budget.py`).
//! 80% of a limit is WARNING, reaching it is EXCEEDED, for every dimension.
//!
//! The ledger's run records live in another module, so the rollups take the
//! small [`BudgetRun`] projection of a `RunRecord` instead of depending on it.

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use serde::Serialize;
use serde_json::{json, Value};

const WARNING_THRESHOLD: f64 = 0.8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetStatus {
    Ok,
    Warning,
    Exceeded,
}

impl BudgetStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            BudgetStatus::Ok => "ok",
            BudgetStatus::Warning => "warning",
            BudgetStatus::Exceeded => "exceeded",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ok" => Some(Self::Ok),
            "warning" => Some(Self::Warning),
            "exceeded" => Some(Self::Exceeded),
            _ => None,
        }
    }
}

fn status_for(used: f64, limit: Option<f64>) -> BudgetStatus {
    match limit {
        None => BudgetStatus::Ok,
        Some(l) if used >= l => BudgetStatus::Exceeded,
        Some(l) if used >= l * WARNING_THRESHOLD => BudgetStatus::Warning,
        Some(_) => BudgetStatus::Ok,
    }
}

fn worst(statuses: &[BudgetStatus]) -> BudgetStatus {
    if statuses.contains(&BudgetStatus::Exceeded) {
        BudgetStatus::Exceeded
    } else if statuses.contains(&BudgetStatus::Warning) {
        BudgetStatus::Warning
    } else {
        BudgetStatus::Ok
    }
}

/// The stop conditions a controller enforces (`None` = unlimited).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StopConditions {
    pub max_iterations: Option<i64>,
    pub max_runtime_minutes: Option<f64>,
    pub max_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IterationsBudget {
    pub status: BudgetStatus,
    pub used: i64,
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuntimeBudget {
    pub status: BudgetStatus,
    pub used_seconds: f64,
    pub limit_seconds: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CostBudget {
    pub status: BudgetStatus,
    pub used_usd: f64,
    pub limit_usd: Option<f64>,
}

/// Result of [`BudgetController::check`]; serialises to the same shape as the
/// Python dict (`iterations`, `runtime`, `cost`, `overall`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BudgetCheck {
    pub iterations: IterationsBudget,
    pub runtime: RuntimeBudget,
    pub cost: CostBudget,
    pub overall: BudgetStatus,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BudgetController {
    pub stop_conditions: StopConditions,
}

impl BudgetController {
    pub fn new(stop_conditions: StopConditions) -> Self {
        Self { stop_conditions }
    }

    pub fn check(&self, iterations: i64, runtime_seconds: f64, cost_usd: f64) -> BudgetCheck {
        let sc = &self.stop_conditions;
        let max_runtime_seconds = sc.max_runtime_minutes.map(|m| m * 60.0);
        let iterations_status = status_for(iterations as f64, sc.max_iterations.map(|i| i as f64));
        let runtime_status = status_for(runtime_seconds, max_runtime_seconds);
        let cost_status = status_for(cost_usd, sc.max_cost_usd);
        BudgetCheck {
            iterations: IterationsBudget {
                status: iterations_status,
                used: iterations,
                limit: sc.max_iterations,
            },
            runtime: RuntimeBudget {
                status: runtime_status,
                used_seconds: runtime_seconds,
                limit_seconds: max_runtime_seconds,
            },
            cost: CostBudget {
                status: cost_status,
                used_usd: cost_usd,
                limit_usd: sc.max_cost_usd,
            },
            overall: worst(&[iterations_status, runtime_status, cost_status]),
        }
    }
}

/// Parse the UTC timestamp out of a `run_<YYYYMMDD>_<HHMMSS>_...` run id.
pub fn run_timestamp(run_id: &str) -> Option<DateTime<Utc>> {
    let rest = run_id.strip_prefix("run_")?;
    let b = rest.as_bytes();
    if b.len() < 16 || b[8] != b'_' || b[15] != b'_' {
        return None;
    }
    let digits = |s: &[u8]| -> Option<u32> {
        if s.iter().all(u8::is_ascii_digit) {
            std::str::from_utf8(s).ok()?.parse().ok()
        } else {
            None
        }
    };
    let date = NaiveDate::from_ymd_opt(
        digits(&b[0..4])? as i32,
        digits(&b[4..6])?,
        digits(&b[6..8])?,
    )?;
    let naive = date.and_hms_opt(digits(&b[9..11])?, digits(&b[11..13])?, digits(&b[13..15])?)?;
    Some(Utc.from_utc_datetime(&naive))
}

/// The projection of a ledger `RunRecord` the rollups need.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetRun {
    /// The definition (loop) name.
    pub loop_name: String,
    pub run_id: String,
    pub has_result: bool,
    pub has_iterations: bool,
    pub budget_overall: Option<BudgetStatus>,
    pub total_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct StatusCounts {
    pub ok: u64,
    pub warning: u64,
    pub exceeded: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct BudgetGroup {
    pub runs: u64,
    pub cost_used_usd: f64,
    pub status_counts: StatusCounts,
}

impl BudgetGroup {
    fn add(&mut self, cost: f64, overall: BudgetStatus) {
        self.runs += 1;
        self.cost_used_usd += cost;
        match overall {
            BudgetStatus::Ok => self.status_counts.ok += 1,
            BudgetStatus::Warning => self.status_counts.warning += 1,
            BudgetStatus::Exceeded => self.status_counts.exceeded += 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoopSummary {
    pub definition_name: String,
    pub group: BudgetGroup,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimeSummary {
    pub bucket: String,
    pub group: BudgetGroup,
}

impl LoopSummary {
    /// `{"definition_name", "runs", "cost_used_usd", "status_counts"}`.
    pub fn to_value(&self) -> Value {
        group_value("definition_name", &self.definition_name, &self.group)
    }
}

impl TimeSummary {
    /// `{"bucket", "runs", "cost_used_usd", "status_counts"}`.
    pub fn to_value(&self) -> Value {
        group_value("bucket", &self.bucket, &self.group)
    }
}

fn group_value(key: &str, name: &str, g: &BudgetGroup) -> Value {
    let mut v = json!({ key: name });
    v["runs"] = json!(g.runs);
    v["cost_used_usd"] = json!(g.cost_used_usd);
    v["status_counts"] = json!(g.status_counts);
    v
}

/// (loop name, timestamp, cost, overall) for one run, or None when it has
/// nothing to aggregate (incomplete, no iterations, unparseable run id).
fn budget_row(run: &BudgetRun) -> Option<(&str, DateTime<Utc>, f64, BudgetStatus)> {
    if !run.has_result || !run.has_iterations {
        return None;
    }
    let overall = run.budget_overall?;
    let ts = run_timestamp(&run.run_id)?;
    Some((
        &run.loop_name,
        ts,
        run.total_cost_usd.unwrap_or(0.0),
        overall,
    ))
}

/// Group runs by definition name; rows sorted by name.
pub fn summarize_by_loop(runs: &[BudgetRun]) -> Vec<LoopSummary> {
    let mut groups: Vec<(String, BudgetGroup)> = Vec::new();
    for (name, _ts, cost, overall) in runs.iter().filter_map(budget_row) {
        match groups.iter_mut().find(|(n, _)| n == name) {
            Some((_, g)) => g.add(cost, overall),
            None => {
                let mut g = BudgetGroup::default();
                g.add(cost, overall);
                groups.push((name.to_string(), g));
            }
        }
    }
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    groups
        .into_iter()
        .map(|(definition_name, group)| LoopSummary {
            definition_name,
            group,
        })
        .collect()
}

/// Bucket key for `granularity` ("day", "week" ISO `%G-W%V`, "month").
pub fn bucket_key(ts: &DateTime<Utc>, granularity: &str) -> Result<String, String> {
    match granularity {
        "day" => Ok(ts.format("%Y-%m-%d").to_string()),
        "week" => {
            let w = ts.iso_week();
            Ok(format!("{:04}-W{:02}", w.year(), w.week()))
        }
        "month" => Ok(ts.format("%Y-%m").to_string()),
        other => Err(format!("unknown granularity: {other:?}")),
    }
}

/// Group runs into day/week/month buckets, most recent bucket first, capped to
/// `limit` buckets when given.
pub fn summarize_by_time(
    runs: &[BudgetRun],
    granularity: &str,
    limit: Option<usize>,
) -> Result<Vec<TimeSummary>, String> {
    let mut groups: Vec<(String, BudgetGroup)> = Vec::new();
    for (_name, ts, cost, overall) in runs.iter().filter_map(budget_row) {
        let key = bucket_key(&ts, granularity)?;
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, g)) => g.add(cost, overall),
            None => {
                let mut g = BudgetGroup::default();
                g.add(cost, overall);
                groups.push((key, g));
            }
        }
    }
    groups.sort_by(|a, b| b.0.cmp(&a.0));
    if let Some(n) = limit {
        groups.truncate(n);
    }
    Ok(groups
        .into_iter()
        .map(|(bucket, group)| TimeSummary { bucket, group })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctl(i: Option<i64>, m: Option<f64>, c: Option<f64>) -> BudgetController {
        BudgetController::new(StopConditions {
            max_iterations: i,
            max_runtime_minutes: m,
            max_cost_usd: c,
        })
    }

    #[test]
    fn under_80_percent_is_ok() {
        let r = ctl(Some(10), Some(5.0), Some(10.0)).check(3, 60.0, 2.0);
        assert_eq!(r.overall, BudgetStatus::Ok);
        assert_eq!(r.iterations.status, BudgetStatus::Ok);
    }

    #[test]
    fn at_80_percent_is_warning_and_at_limit_exceeded() {
        let c = ctl(Some(10), Some(5.0), Some(10.0));
        let r = c.check(8, 0.0, 0.0);
        assert_eq!(r.iterations.status, BudgetStatus::Warning);
        assert_eq!(r.overall, BudgetStatus::Warning);
        let r = c.check(1, 0.0, 10.0);
        assert_eq!(r.cost.status, BudgetStatus::Exceeded);
        assert_eq!(r.overall, BudgetStatus::Exceeded);
        assert_eq!(
            c.check(10, 0.0, 0.0).iterations.status,
            BudgetStatus::Exceeded
        );
    }

    #[test]
    fn unset_limit_is_always_ok() {
        let r = ctl(None, None, None).check(1_000_000, 1e9, 1e9);
        assert_eq!(r.overall, BudgetStatus::Ok);
        assert_eq!(r.runtime.limit_seconds, None);
    }

    #[test]
    fn reports_used_and_limits_and_serialises_like_python() {
        let r = ctl(Some(10), Some(5.0), Some(10.0)).check(3, 60.0, 2.0);
        assert_eq!(r.runtime.limit_seconds, Some(300.0));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["iterations"]["status"], "ok");
        assert_eq!(v["iterations"]["used"], 3);
        assert_eq!(v["overall"], "ok");
        assert_eq!(v["cost"]["limit_usd"], 10.0);
    }

    #[test]
    fn run_timestamp_parses_and_rejects() {
        let ts = run_timestamp("run_20260909_082338_ai-news").unwrap();
        assert_eq!(ts, Utc.with_ymd_and_hms(2026, 9, 9, 8, 23, 38).unwrap());
        assert!(run_timestamp("not-a-run-id").is_none());
        assert!(run_timestamp("run_20260230_000000_x").is_none());
        assert!(run_timestamp("run_20260909_082338").is_none());
    }

    fn run(name: &str, id: &str, cost: f64, st: BudgetStatus) -> BudgetRun {
        BudgetRun {
            loop_name: name.into(),
            run_id: id.into(),
            has_result: true,
            has_iterations: true,
            budget_overall: Some(st),
            total_cost_usd: Some(cost),
        }
    }

    #[test]
    fn by_loop_groups_sorts_and_skips() {
        let mut incomplete = run("zzz", "run_20260901_000000_a", 9.0, BudgetStatus::Ok);
        incomplete.has_result = false;
        let rows = summarize_by_loop(&[
            run("b", "run_20260901_000000_a", 1.5, BudgetStatus::Ok),
            run("b", "run_20260901_000001_a", 2.0, BudgetStatus::Warning),
            run("a", "run_20260901_000002_a", 1.0, BudgetStatus::Exceeded),
            run("c", "bad-id", 1.0, BudgetStatus::Ok),
            incomplete,
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].definition_name, "a");
        assert_eq!(rows[1].group.runs, 2);
        assert_eq!(rows[1].group.cost_used_usd, 3.5);
        assert_eq!(
            rows[1].group.status_counts,
            StatusCounts {
                ok: 1,
                warning: 1,
                exceeded: 0
            }
        );
        assert_eq!(summarize_by_loop(&[]).len(), 0);
        assert_eq!(
            rows[0].to_value()["status_counts"],
            json!({"ok": 0, "warning": 0, "exceeded": 1})
        );
    }

    #[test]
    fn by_time_buckets_orders_and_limits() {
        let runs = [
            run("a", "run_20260901_000000_a", 1.0, BudgetStatus::Ok),
            run("a", "run_20260901_100000_a", 1.0, BudgetStatus::Ok),
            run("a", "run_20260902_000000_a", 2.0, BudgetStatus::Ok),
            run("a", "run_20260903_000000_a", 2.0, BudgetStatus::Ok),
        ];
        let day = summarize_by_time(&runs, "day", None).unwrap();
        let keys: Vec<_> = day.iter().map(|r| r.bucket.as_str()).collect();
        assert_eq!(keys, ["2026-09-03", "2026-09-02", "2026-09-01"]);
        assert_eq!(day[2].group.runs, 2);
        let limited = summarize_by_time(&runs, "day", Some(2)).unwrap();
        assert_eq!(limited.len(), 2);
        let month = summarize_by_time(&runs, "month", None).unwrap();
        assert_eq!(month[0].bucket, "2026-09");
        // 2026-09-06 is a Sunday (ISO week 36), 2026-09-07 starts week 37.
        let wk = summarize_by_time(
            &[
                run("a", "run_20260906_000000_a", 0.0, BudgetStatus::Ok),
                run("a", "run_20260907_000000_a", 0.0, BudgetStatus::Ok),
            ],
            "week",
            None,
        )
        .unwrap();
        let wk_keys: Vec<_> = wk.iter().map(|r| r.bucket.as_str()).collect();
        assert_eq!(wk_keys, ["2026-W37", "2026-W36"]);
        assert!(summarize_by_time(&runs, "year", None).is_err());
    }
}
