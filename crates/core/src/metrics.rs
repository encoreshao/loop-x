//! The slice of `bin/metrics.py` that cost/learning reporting depends on
//! (run and issue counts from the event log), plus a minimal event-log reader.
//!
//! Events are plain `serde_json::Value` objects, exactly as they sit in
//! `outputs/events/YYYY-MM-DD.jsonl`.

use crate::events;
use chrono::{DateTime, FixedOffset, NaiveDateTime};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;

/// Every event in `<dir>/*.jsonl` within `[since_date, until_date]` (inclusive
/// `YYYY-MM-DD` strings, either may be `None`), in filename order; `None` dir
/// means [`events::default_events_dir`]. Thin wrapper over [`events::iter_events`].
pub fn read_events(
    events_dir: Option<&Path>,
    since_date: Option<&str>,
    until_date: Option<&str>,
) -> Vec<Value> {
    events::iter_events(events_dir, since_date, until_date)
}

/// A string-keyed map that remembers first-insertion order, like a Python
/// `dict` (re-inserting a key keeps its original position).
#[derive(Debug, Clone)]
pub struct Ordered<V> {
    index: HashMap<String, usize>,
    items: Vec<(String, V)>,
}

impl<V> Default for Ordered<V> {
    fn default() -> Self {
        Self {
            index: HashMap::new(),
            items: Vec::new(),
        }
    }
}

impl<V> Ordered<V> {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert(&mut self, key: String, value: V) {
        match self.index.get(&key) {
            Some(&i) => self.items[i].1 = value,
            None => {
                self.index.insert(key.clone(), self.items.len());
                self.items.push((key, value));
            }
        }
    }
    pub fn get(&self, key: &str) -> Option<&V> {
        self.index.get(key).map(|&i| &self.items[i].1)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        match self.index.get(key) {
            Some(&i) => Some(&mut self.items[i].1),
            None => None,
        }
    }
    pub fn contains_key(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.items.iter().map(|(k, v)| (k, v))
    }
}

/// A dict key for event field `name`: the JSON text of the value, so absent
/// and `null` fields share one key (Python's `None`).
pub fn field_key(event: &Value, name: &str) -> String {
    event.get(name).unwrap_or(&Value::Null).to_string()
}

/// `event["data"] or {}` as an object lookup.
pub fn data_field<'a>(event: &'a Value, name: &str) -> Option<&'a Value> {
    event.get("data").and_then(|d| d.get(name))
}

/// `datetime.fromisoformat(ts.replace("Z", "+00:00"))` in microseconds since epoch.
fn parse_timestamp_us(ts: &str) -> Option<i64> {
    let ts = ts.replace('Z', "+00:00");
    if let Ok(dt) = DateTime::<FixedOffset>::parse_from_rfc3339(&ts) {
        return Some(dt.timestamp_micros());
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(&ts, fmt) {
            return Some(n.and_utc().timestamp_micros());
        }
    }
    None
}

/// Milliseconds between two ISO timestamps (`None` if either is unparseable).
pub fn duration_ms(start_iso: &str, end_iso: &str) -> Option<f64> {
    let (s, e) = (parse_timestamp_us(start_iso)?, parse_timestamp_us(end_iso)?);
    Some((e - s) as f64 / 1e6 * 1000.0)
}

fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty())
        .then(|| crate::pyfmt_lite::py_sum(values.iter().copied()) / values.len() as f64)
}

fn timestamp_of(event: &Value) -> Option<&str> {
    event.get("timestamp").and_then(Value::as_str)
}

fn durations(started: &Ordered<String>, ended: &Ordered<String>) -> Vec<f64> {
    started
        .iter()
        .filter_map(|(id, start)| duration_ms(start, ended.get(id)?))
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunMetrics {
    pub runs_total: usize,
    pub runs_success: usize,
    pub runs_failed: usize,
    pub average_run_duration_ms: Option<f64>,
}

impl RunMetrics {
    pub fn to_value(&self) -> Value {
        json!({
            "runs_total": self.runs_total,
            "runs_success": self.runs_success,
            "runs_failed": self.runs_failed,
            "average_run_duration_ms": self.average_run_duration_ms,
        })
    }
}

/// Runs from `run.started` / `run.completed` / `run.failed`, paired by `run_id`.
pub fn compute_run_metrics(events: &[Value]) -> RunMetrics {
    let mut started = Ordered::new();
    let mut completed = Ordered::new();
    let mut failed = std::collections::HashSet::new();
    for event in events {
        let run_id = field_key(event, "run_id");
        let ts = timestamp_of(event);
        match event.get("event_type").and_then(Value::as_str) {
            Some("run.started") => {
                if let Some(ts) = ts {
                    started.insert(run_id, ts.to_string());
                }
            }
            Some("run.completed") => {
                if let Some(ts) = ts {
                    completed.insert(run_id, ts.to_string());
                }
            }
            Some("run.failed") => {
                failed.insert(run_id);
            }
            _ => {}
        }
    }
    RunMetrics {
        runs_total: started.len(),
        runs_success: completed.len(),
        runs_failed: failed.len(),
        average_run_duration_ms: mean(&durations(&started, &completed)),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IssueMetrics {
    pub issues_processed: usize,
    pub issues_completed: usize,
    pub issues_escalated: usize,
    pub issues_failed: usize,
    pub average_issue_duration_ms: Option<f64>,
}

impl IssueMetrics {
    pub fn to_value(&self) -> Value {
        json!({
            "issues_processed": self.issues_processed,
            "issues_completed": self.issues_completed,
            "issues_escalated": self.issues_escalated,
            "issues_failed": self.issues_failed,
            "average_issue_duration_ms": self.average_issue_duration_ms,
        })
    }
}

/// Issue counts from `issue.started` / `.completed` / `.escalated` / `.failed`,
/// paired by `issue_run_id`; with `project`, only that project's events count.
pub fn compute_issue_metrics(events: &[Value], project: Option<&str>) -> IssueMetrics {
    let mut started = Ordered::new();
    let mut ended = Ordered::new();
    let mut completed = std::collections::HashSet::new();
    let mut escalated = std::collections::HashSet::new();
    let mut failed = std::collections::HashSet::new();
    for event in events {
        if let Some(p) = project {
            if event.get("project").and_then(Value::as_str) != Some(p) {
                continue;
            }
        }
        let id = field_key(event, "issue_run_id");
        let ts = timestamp_of(event);
        let mut end = |ids: Option<&mut std::collections::HashSet<String>>| {
            if let Some(ids) = ids {
                ids.insert(id.clone());
            }
            if let Some(ts) = ts {
                ended.insert(id.clone(), ts.to_string());
            }
        };
        match event.get("event_type").and_then(Value::as_str) {
            Some("issue.started") => {
                if let Some(ts) = ts {
                    started.insert(id.clone(), ts.to_string());
                }
            }
            Some("issue.completed") => end(Some(&mut completed)),
            Some("issue.escalated") => end(Some(&mut escalated)),
            Some("issue.failed") => end(Some(&mut failed)),
            _ => {}
        }
    }
    IssueMetrics {
        issues_processed: started.len(),
        issues_completed: completed.len(),
        issues_escalated: escalated.len(),
        issues_failed: failed.len(),
        average_issue_duration_ms: mean(&durations(&started, &ended)),
    }
}

// ---- the rest of bin/metrics.py ---------------------------------------------------

pub const RETRY_RATE_UNAVAILABLE_REASON: &str =
    "not computed here; gate-mode retries are counted on Harness > Gates (Retried \u{2192} passed)";
pub const FAILURE_RATE_UNAVAILABLE_REASON: &str =
    "no issue-level \"failed\" outcome exists yet, only fix/answer/escalate";

/// `issue.escalated` / `issue.failed` reasons mapped to a failure category;
/// anything else is `"unknown"`.
pub const FAILURE_REASON_CATEGORY_MAP: [(&str, &str); 3] = [
    ("verification_failed", "verification"),
    ("needs_clarification", "requirement"),
    ("worktree_creation_failed", "environment"),
];

fn project_matches(event: &Value, project: Option<&str>) -> bool {
    project.is_none_or(|p| event.get("project").and_then(Value::as_str) == Some(p))
}

fn event_type(event: &Value) -> Option<&str> {
    event.get("event_type").and_then(Value::as_str)
}

/// `event["data"] or {}` as a lookup of one key (`None` when absent).
fn data_get<'a>(event: &'a Value, key: &str) -> Option<&'a Value> {
    event.get("data").and_then(|d| d.get(key))
}

#[derive(Debug, Clone, PartialEq)]
pub struct VerificationMetrics {
    pub verification_total: usize,
    pub verification_passed: usize,
    pub verification_failed: usize,
    pub verification_pass_rate: Option<f64>,
    pub average_verification_duration_ms: Option<f64>,
    /// Filled in by [`build_report`] from [`compute_first_pass_verification_metrics`].
    pub first_pass: Option<FirstPassMetrics>,
}

impl VerificationMetrics {
    pub fn to_value(&self) -> Value {
        let mut v = json!({
            "verification_total": self.verification_total,
            "verification_passed": self.verification_passed,
            "verification_failed": self.verification_failed,
            "verification_pass_rate": self.verification_pass_rate,
            "average_verification_duration_ms": self.average_verification_duration_ms,
        });
        if let (Some(fp), Some(obj)) = (&self.first_pass, v.as_object_mut()) {
            for (k, val) in fp.to_value().as_object().into_iter().flatten() {
                obj.insert(k.clone(), val.clone());
            }
        }
        v
    }
}

/// Verification outcomes from `verification.started` / `.passed` / `.failed`,
/// paired by `issue_run_id`; same project filtering as the issue metrics.
pub fn compute_verification_metrics(
    events: &[Value],
    project: Option<&str>,
) -> VerificationMetrics {
    let mut started = Ordered::new();
    let mut ended = Ordered::new();
    let mut passed = std::collections::HashSet::new();
    let mut failed = std::collections::HashSet::new();
    for event in events {
        if !project_matches(event, project) {
            continue;
        }
        let id = field_key(event, "issue_run_id");
        let ts = timestamp_of(event);
        match event_type(event) {
            Some("verification.started") => {
                if let Some(ts) = ts {
                    started.insert(id, ts.to_string());
                }
            }
            Some("verification.passed") => {
                passed.insert(id.clone());
                if let Some(ts) = ts {
                    ended.insert(id, ts.to_string());
                }
            }
            Some("verification.failed") => {
                failed.insert(id.clone());
                if let Some(ts) = ts {
                    ended.insert(id, ts.to_string());
                }
            }
            _ => {}
        }
    }
    let denom = passed.len() + failed.len();
    VerificationMetrics {
        verification_total: denom,
        verification_passed: passed.len(),
        verification_failed: failed.len(),
        verification_pass_rate: (denom != 0).then(|| passed.len() as f64 / denom as f64),
        average_verification_duration_ms: mean(&durations(&started, &ended)),
        first_pass: None,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClassificationMetrics {
    pub classified_total: usize,
    pub by_type: Ordered<i64>,
    pub by_complexity: Ordered<i64>,
    pub by_risk_level: Ordered<i64>,
}

fn counts_value(counts: &Ordered<i64>) -> Value {
    Value::Object(counts.iter().map(|(k, v)| (k.clone(), json!(v))).collect())
}

impl<V: PartialEq> PartialEq for Ordered<V> {
    fn eq(&self, other: &Self) -> bool {
        self.items == other.items
    }
}

impl ClassificationMetrics {
    pub fn to_value(&self) -> Value {
        json!({
            "classified_total": self.classified_total,
            "by_type": counts_value(&self.by_type),
            "by_complexity": counts_value(&self.by_complexity),
            "by_risk_level": counts_value(&self.by_risk_level),
        })
    }
}

/// A count key for a classification value: strings as-is, anything else as JSON.
fn count_key(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn bump(counts: &mut Ordered<i64>, key: String) {
    let n = counts.get(&key).copied().unwrap_or(0);
    counts.insert(key, n + 1);
}

/// Counts by type / complexity / risk level from `issue.classified` data (the
/// last event per `issue_run_id` wins); a classification missing a field is
/// simply not counted under it.
pub fn compute_classification_metrics(
    events: &[Value],
    project: Option<&str>,
) -> ClassificationMetrics {
    let mut latest: Ordered<Value> = Ordered::new();
    for event in events {
        if !project_matches(event, project) || event_type(event) != Some("issue.classified") {
            continue;
        }
        let data = match event.get("data") {
            Some(d) if crate::loop_serialize::truthy(d) => d.clone(),
            _ => json!({}),
        };
        latest.insert(field_key(event, "issue_run_id"), data);
    }
    let (mut by_type, mut by_complexity, mut by_risk_level) =
        (Ordered::new(), Ordered::new(), Ordered::new());
    for (_, data) in latest.iter() {
        for (key, counts) in [
            ("type", &mut by_type),
            ("complexity", &mut by_complexity),
            ("risk_level", &mut by_risk_level),
        ] {
            if let Some(v) = data.get(key).filter(|v| !v.is_null()) {
                bump(counts, count_key(v));
            }
        }
    }
    ClassificationMetrics {
        classified_total: latest.len(),
        by_type,
        by_complexity,
        by_risk_level,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FailureTaxonomy {
    pub total: i64,
    pub by_category: Ordered<i64>,
    pub by_category_pct: Vec<(String, f64)>,
}

impl FailureTaxonomy {
    pub fn to_value(&self) -> Value {
        json!({
            "total": self.total,
            "by_category": counts_value(&self.by_category),
            "by_category_pct": Value::Object(
                self.by_category_pct.iter().map(|(k, v)| (k.clone(), json!(v))).collect()
            ),
        })
    }
}

/// Failure categories from `issue.escalated` / `issue.failed` `data.reason`.
/// An unmapped or missing reason counts under `"unknown"` rather than being
/// dropped; `by_category_pct` is empty (not zeros) when there are none.
pub fn compute_failure_taxonomy(events: &[Value], project: Option<&str>) -> FailureTaxonomy {
    let mut by_category: Ordered<i64> = Ordered::new();
    for event in events {
        if !project_matches(event, project) {
            continue;
        }
        if !matches!(event_type(event), Some("issue.escalated" | "issue.failed")) {
            continue;
        }
        let reason = data_get(event, "reason").and_then(Value::as_str);
        let category = FAILURE_REASON_CATEGORY_MAP
            .iter()
            .find(|(r, _)| Some(*r) == reason)
            .map_or("unknown", |(_, c)| c);
        bump(&mut by_category, category.to_string());
    }
    let total: i64 = by_category.iter().map(|(_, n)| *n).sum();
    let by_category_pct = if total != 0 {
        by_category
            .iter()
            .map(|(k, n)| (k.clone(), *n as f64 / total as f64))
            .collect()
    } else {
        Vec::new()
    };
    FailureTaxonomy {
        total,
        by_category,
        by_category_pct,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FirstPassMetrics {
    pub first_pass_verification_total: usize,
    pub first_pass_verification_passed: usize,
    pub first_pass_verification_rate: Option<f64>,
}

impl FirstPassMetrics {
    pub fn to_value(&self) -> Value {
        json!({
            "first_pass_verification_total": self.first_pass_verification_total,
            "first_pass_verification_passed": self.first_pass_verification_passed,
            "first_pass_verification_rate": self.first_pass_verification_rate,
        })
    }
}

/// For each `issue_run_id` the EARLIEST-timestamped `verification.passed` /
/// `.failed` event decides whether the first pass succeeded (ISO timestamps
/// compare chronologically as strings). Events without a timestamp are ignored.
pub fn compute_first_pass_verification_metrics(
    events: &[Value],
    project: Option<&str>,
) -> FirstPassMetrics {
    let mut earliest: Ordered<(String, bool)> = Ordered::new();
    for event in events {
        if !project_matches(event, project) {
            continue;
        }
        let passed = match event_type(event) {
            Some("verification.passed") => true,
            Some("verification.failed") => false,
            _ => continue,
        };
        let Some(ts) = timestamp_of(event) else {
            continue;
        };
        let id = field_key(event, "issue_run_id");
        if earliest.get(&id).is_none_or(|(cur, _)| ts < cur.as_str()) {
            earliest.insert(id, (ts.to_string(), passed));
        }
    }
    let total = earliest.len();
    let passed = earliest.iter().filter(|(_, (_, p))| *p).count();
    FirstPassMetrics {
        first_pass_verification_total: total,
        first_pass_verification_passed: passed,
        first_pass_verification_rate: (total != 0).then(|| passed as f64 / total as f64),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct QualityAndAutonomy {
    pub resolution_rate: Option<f64>,
    pub verification_pass_rate: Option<f64>,
    pub autonomy_rate: Option<f64>,
    pub human_intervention_rate: Option<f64>,
}

impl QualityAndAutonomy {
    pub fn to_value(&self) -> Value {
        json!({
            "resolution_rate": self.resolution_rate,
            "verification_pass_rate": self.verification_pass_rate,
            "retry_rate": Value::Null,
            "retry_rate_unavailable_reason": RETRY_RATE_UNAVAILABLE_REASON,
            "failure_rate": Value::Null,
            "failure_rate_unavailable_reason": FAILURE_RATE_UNAVAILABLE_REASON,
            "autonomy_rate": self.autonomy_rate,
            "autonomy_rate_is_placeholder": true,
            "human_intervention_rate": self.human_intervention_rate,
        })
    }
}

/// Pure arithmetic over the issue and verification metrics. `retry_rate` and
/// `failure_rate` are always unavailable; `autonomy_rate` is a placeholder
/// equal to `resolution_rate`.
pub fn compute_quality_and_autonomy_metrics(
    issue: &IssueMetrics,
    verification: &VerificationMetrics,
) -> QualityAndAutonomy {
    let resolution_rate = (issue.issues_processed != 0)
        .then(|| issue.issues_completed as f64 / issue.issues_processed as f64);
    QualityAndAutonomy {
        resolution_rate,
        verification_pass_rate: verification.verification_pass_rate,
        autonomy_rate: resolution_rate,
        human_intervention_rate: resolution_rate.map(|r| 1.0 - r),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunSection {
    Runs(RunMetrics),
    /// Present instead of a tally when a project filter makes runs meaningless.
    NotApplicable(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub run: RunSection,
    pub issue: IssueMetrics,
    pub verification: VerificationMetrics,
    pub quality_and_autonomy: QualityAndAutonomy,
    pub classification: ClassificationMetrics,
    pub failure_taxonomy: FailureTaxonomy,
    pub since_date: Option<String>,
    pub until_date: Option<String>,
    pub project: Option<String>,
}

impl Report {
    pub fn to_value(&self) -> Value {
        json!({
            "run": match &self.run {
                RunSection::Runs(r) => r.to_value(),
                RunSection::NotApplicable(reason) => json!({"not_applicable_reason": reason}),
            },
            "issue": self.issue.to_value(),
            "verification": self.verification.to_value(),
            "quality_and_autonomy": self.quality_and_autonomy.to_value(),
            "classification": self.classification.to_value(),
            "failure_taxonomy": self.failure_taxonomy.to_value(),
            "scope": {
                "since_date": self.since_date,
                "until_date": self.until_date,
                "project": self.project,
            },
        })
    }
}

/// Read the event log once (or use `all_events`) and assemble every section.
/// With a `project`, `run` becomes a not-applicable note: a run spans every
/// project touched in it.
pub fn build_report(
    events_dir: Option<&Path>,
    since_date: Option<&str>,
    until_date: Option<&str>,
    project: Option<&str>,
    all_events: Option<&[Value]>,
) -> Report {
    let owned;
    let events = match all_events {
        Some(e) => e,
        None => {
            owned = read_events(events_dir, since_date, until_date);
            &owned
        }
    };
    let issue = compute_issue_metrics(events, project);
    let mut verification = compute_verification_metrics(events, project);
    verification.first_pass = Some(compute_first_pass_verification_metrics(events, project));
    let classification = compute_classification_metrics(events, project);
    let failure_taxonomy = compute_failure_taxonomy(events, project);
    let quality_and_autonomy = compute_quality_and_autonomy_metrics(&issue, &verification);
    let run = match project {
        Some(_) => RunSection::NotApplicable("a run is not scoped to a single project".into()),
        None => RunSection::Runs(compute_run_metrics(events)),
    };
    Report {
        run,
        issue,
        verification,
        quality_and_autonomy,
        classification,
        failure_taxonomy,
        since_date: since_date.map(str::to_string),
        until_date: until_date.map(str::to_string),
        project: project.map(str::to_string),
    }
}

/// One report per sequential UTC-day bucket covering the last `days` days,
/// oldest first, each spanning `bucket_days` days. Buckets are built from
/// today backwards so a remainder lands on the OLDEST bucket. `all_events`
/// (covering the whole window) is sliced per bucket instead of re-reading.
pub fn bucketed_reports(
    events_dir: Option<&Path>,
    days: i64,
    bucket_days: i64,
    all_events: Option<&[Value]>,
    today: chrono::NaiveDate,
) -> Vec<Report> {
    let mut buckets = Vec::new();
    let mut bucket_until = today;
    let mut remaining = days;
    while remaining > 0 {
        let span = bucket_days.min(remaining);
        let since = bucket_until - chrono::Duration::days(span - 1);
        buckets.push((since, bucket_until));
        bucket_until = since - chrono::Duration::days(1);
        remaining -= span;
    }
    buckets.reverse();
    buckets
        .into_iter()
        .map(|(since, until)| {
            let (s, u) = (since.to_string(), until.to_string());
            let slice = all_events
                .map(|events| events::filter_by_date(events.to_vec(), Some(&s), Some(&u)));
            build_report(events_dir, Some(&s), Some(&u), None, slice.as_deref())
        })
        .collect()
}

fn fmt_rate(value: Option<f64>) -> String {
    value.map_or("N/A".to_string(), |v| format!("{:.1}%", v * 100.0))
}

fn fmt_seconds(ms: Option<f64>) -> String {
    ms.map_or("N/A".to_string(), |v| format!("{:.1}s", v / 1000.0))
}

fn fmt_counts(counts: &Ordered<i64>) -> String {
    if counts.is_empty() {
        return "none".to_string();
    }
    let mut items: Vec<(&String, &i64)> = counts.iter().collect();
    items.sort();
    items
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The report as plain text for the CLI (no trailing newline).
pub fn format_report(report: &Report) -> String {
    let header = match (&report.since_date, &report.until_date) {
        (None, None) => "Loop Metrics (all time)".to_string(),
        (since, until) => format!(
            "Loop Metrics ({} to {})",
            since.as_deref().unwrap_or("None"),
            until.as_deref().unwrap_or("None")
        ),
    };
    let header = match &report.project {
        Some(p) => format!("{header} \u{2014} project: {p}"),
        None => header,
    };
    let mut lines: Vec<String> = vec![header, String::new(), "Runs".into()];
    match &report.run {
        RunSection::NotApplicable(reason) => lines.push(format!("  N/A ({reason})")),
        RunSection::Runs(run) => {
            lines.push(format!("  Total          {}", run.runs_total));
            lines.push(format!("  Success        {}", run.runs_success));
            lines.push(format!("  Failed         {}", run.runs_failed));
            lines.push(format!(
                "  Avg duration   {}",
                fmt_seconds(run.average_run_duration_ms)
            ));
        }
    }
    lines.push(String::new());

    let issue = &report.issue;
    lines.push("Issues".into());
    lines.push(format!("  Processed      {}", issue.issues_processed));
    lines.push(format!("  Completed      {}", issue.issues_completed));
    lines.push(format!("  Escalated      {}", issue.issues_escalated));
    lines.push(format!("  Failed         {}", issue.issues_failed));
    lines.push(String::new());

    let (qa, verification) = (&report.quality_and_autonomy, &report.verification);
    lines.push("Quality".into());
    lines.push(format!(
        "  Resolution rate         {}",
        fmt_rate(qa.resolution_rate)
    ));
    lines.push(format!(
        "  Verification pass rate  {}",
        fmt_rate(verification.verification_pass_rate)
    ));
    lines.push(format!(
        "  First-pass verification {}",
        fmt_rate(
            verification
                .first_pass
                .as_ref()
                .and_then(|f| f.first_pass_verification_rate)
        )
    ));
    lines.push(format!(
        "  Retry rate              N/A ({RETRY_RATE_UNAVAILABLE_REASON})"
    ));
    lines.push(format!(
        "  Failure rate            N/A ({FAILURE_RATE_UNAVAILABLE_REASON})"
    ));
    lines.push(String::new());

    let c = &report.classification;
    lines.push("Classification".into());
    if c.classified_total == 0 {
        lines.push("  N/A (no issues classified in this window)".into());
    } else {
        lines.push(format!("  Classified      {}", c.classified_total));
        lines.push(format!("  By type         {}", fmt_counts(&c.by_type)));
        lines.push(format!(
            "  By complexity   {}",
            fmt_counts(&c.by_complexity)
        ));
        lines.push(format!(
            "  By risk level   {}",
            fmt_counts(&c.by_risk_level)
        ));
    }
    lines.push(String::new());

    let ft = &report.failure_taxonomy;
    lines.push("Failure breakdown".into());
    if ft.total == 0 {
        lines.push("  N/A (no escalations in this window)".into());
    } else {
        let mut pcts: Vec<&(String, f64)> = ft.by_category_pct.iter().collect();
        pcts.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (category, pct) in pcts {
            lines.push(format!("  {category:<15} {:.1}%", pct * 100.0));
        }
    }
    lines.push(String::new());

    lines.push("Autonomy".into());
    lines.push(format!(
        "  Autonomy rate            {} (placeholder: currently identical to resolution rate)",
        fmt_rate(qa.autonomy_rate)
    ));
    lines.push(format!(
        "  Human intervention rate  {}",
        fmt_rate(qa.human_intervention_rate)
    ));
    lines.push(String::new());

    lines.push("Duration".into());
    lines.push(format!(
        "  Avg issue duration          {}",
        fmt_seconds(issue.average_issue_duration_ms)
    ));
    lines.push(format!(
        "  Avg verification duration   {}",
        fmt_seconds(verification.average_verification_duration_ms)
    ));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: &str, run: &str, ts: &str) -> Value {
        json!({"event_type": t, "run_id": run, "issue_run_id": run, "timestamp": ts})
    }

    #[test]
    fn run_metrics_pair_by_run_id() {
        let events = [
            ev("run.started", "r1", "2026-09-04T10:00:00Z"),
            ev("run.completed", "r1", "2026-09-04T10:00:02Z"),
            ev("run.started", "r2", "2026-09-04T11:00:00Z"),
            ev("run.failed", "r2", "2026-09-04T11:00:01Z"),
        ];
        let m = compute_run_metrics(&events);
        assert_eq!((m.runs_total, m.runs_success, m.runs_failed), (2, 1, 1));
        assert_eq!(m.average_run_duration_ms, Some(2000.0));
        assert_eq!(compute_run_metrics(&[]).average_run_duration_ms, None);
    }

    #[test]
    fn issue_metrics_count_outcomes_and_durations() {
        let events = [
            ev("issue.started", "a", "2026-09-04T10:00:00Z"),
            ev("issue.completed", "a", "2026-09-04T10:00:10Z"),
            ev("issue.started", "b", "2026-09-04T10:00:00Z"),
            ev("issue.escalated", "b", "2026-09-04T10:00:20Z"),
            ev("issue.started", "c", "2026-09-04T10:00:00Z"),
            ev("issue.failed", "c", "2026-09-04T10:00:30Z"),
        ];
        let m = compute_issue_metrics(&events, None);
        assert_eq!(
            (
                m.issues_processed,
                m.issues_completed,
                m.issues_escalated,
                m.issues_failed
            ),
            (3, 1, 1, 1)
        );
        assert_eq!(m.average_issue_duration_ms, Some(20000.0));
    }

    #[test]
    fn issue_metrics_filter_by_project() {
        let mut a = ev("issue.started", "a", "2026-09-04T10:00:00Z");
        a["project"] = json!("p1");
        let mut b = ev("issue.started", "b", "2026-09-04T10:00:00Z");
        b["project"] = json!("p2");
        assert_eq!(
            compute_issue_metrics(&[a, b], Some("p1")).issues_processed,
            1
        );
    }

    #[test]
    fn started_without_timestamp_is_not_counted() {
        let e = json!({"event_type": "issue.started", "issue_run_id": "a"});
        assert_eq!(compute_issue_metrics(&[e], None).issues_processed, 0);
    }

    #[test]
    fn reads_event_files_in_range_and_skips_bad_lines() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("2026-09-01.jsonl"),
            "{\"a\": 1}\nnot json\n\n",
        )
        .unwrap();
        std::fs::write(d.path().join("2026-09-02.jsonl"), "{\"a\": 2}\n").unwrap();
        std::fs::write(d.path().join("2026-09-03.jsonl"), "{\"a\": 3}\n").unwrap();
        std::fs::write(d.path().join("notes.txt"), "{\"a\": 9}\n").unwrap();
        let all = read_events(Some(d.path()), None, None);
        assert_eq!(all.len(), 3);
        let some = read_events(Some(d.path()), Some("2026-09-02"), Some("2026-09-02"));
        assert_eq!(some, vec![json!({"a": 2})]);
        assert!(read_events(Some(&d.path().join("nope")), None, None).is_empty());
    }

    #[test]
    fn ordered_keeps_first_position_on_reinsert() {
        let mut o = Ordered::new();
        o.insert("a".into(), 1);
        o.insert("b".into(), 2);
        o.insert("a".into(), 3);
        let keys: Vec<_> = o.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        assert_eq!(keys, [("a", 3), ("b", 2)]);
    }

    fn at(t: &str, id: &str, ts: &str) -> Value {
        json!({"event_type": t, "issue_run_id": id, "timestamp": ts})
    }

    #[test]
    fn verification_metrics_rate_duration_and_duplicates() {
        let events = [
            at("verification.started", "a", "2026-09-04T10:00:00Z"),
            at("verification.passed", "a", "2026-09-04T10:00:04Z"),
            at("verification.passed", "a", "2026-09-04T10:00:05Z"),
            at("verification.started", "b", "2026-09-04T10:00:00Z"),
            at("verification.failed", "b", "2026-09-04T10:00:02Z"),
        ];
        let m = compute_verification_metrics(&events, None);
        assert_eq!(
            (
                m.verification_total,
                m.verification_passed,
                m.verification_failed
            ),
            (2, 1, 1)
        );
        assert_eq!(m.verification_pass_rate, Some(0.5));
        // `a` ends at its LAST terminal event (5s), `b` at 2s.
        assert_eq!(m.average_verification_duration_ms, Some(3500.0));
        let none = compute_verification_metrics(&[], None);
        assert_eq!(none.verification_pass_rate, None);
        assert_eq!(none.average_verification_duration_ms, None);
    }

    #[test]
    fn first_pass_takes_the_earliest_outcome_not_the_latest() {
        let events = [
            at("verification.passed", "a", "2026-09-04T10:00:09Z"),
            at("verification.failed", "a", "2026-09-04T10:00:01Z"),
            json!({"event_type": "verification.passed", "issue_run_id": "z"}),
        ];
        let m = compute_first_pass_verification_metrics(&events, None);
        assert_eq!(
            (
                m.first_pass_verification_total,
                m.first_pass_verification_passed
            ),
            (1, 0)
        );
        assert_eq!(m.first_pass_verification_rate, Some(0.0));
        assert_eq!(
            compute_first_pass_verification_metrics(&[], None).first_pass_verification_rate,
            None
        );
    }

    #[test]
    fn classification_keeps_the_latest_per_issue_and_skips_missing_fields() {
        let c = |id: &str, data: Value| json!({"event_type": "issue.classified", "issue_run_id": id, "project": "p", "data": data});
        let events = [
            c(
                "a",
                json!({"type": "bug", "complexity": "M", "risk_level": "LOW"}),
            ),
            c("a", json!({"type": "feature"})),
            c("b", json!({"type": "bug", "risk_level": "HIGH"})),
        ];
        let m = compute_classification_metrics(&events, None);
        assert_eq!(m.classified_total, 2);
        assert_eq!(m.to_value()["by_type"], json!({"feature": 1, "bug": 1}));
        assert_eq!(m.to_value()["by_complexity"], json!({}));
        assert_eq!(m.to_value()["by_risk_level"], json!({"HIGH": 1}));
        assert_eq!(
            compute_classification_metrics(&events, Some("other")).classified_total,
            0
        );
    }

    #[test]
    fn failure_taxonomy_maps_reasons_and_buckets_the_rest_as_unknown() {
        let f = |t: &str, reason: Option<&str>| json!({"event_type": t, "project": "p", "data": reason.map_or(json!({}), |r| json!({"reason": r}))});
        let events = [
            f("issue.escalated", Some("verification_failed")),
            f("issue.failed", Some("needs_clarification")),
            f("issue.escalated", Some("worktree_creation_failed")),
            f("issue.escalated", Some("brand_new_reason")),
            f("issue.escalated", None),
            f("issue.completed", None),
        ];
        let t = compute_failure_taxonomy(&events, None);
        assert_eq!(t.total, 5);
        assert_eq!(
            t.to_value()["by_category"],
            json!({"verification": 1, "requirement": 1, "environment": 1, "unknown": 2})
        );
        assert_eq!(t.to_value()["by_category_pct"]["unknown"], json!(0.4));
        let empty = compute_failure_taxonomy(&[], None);
        assert_eq!(empty.total, 0);
        assert_eq!(empty.to_value()["by_category_pct"], json!({}));
    }

    #[test]
    fn quality_and_autonomy_arithmetic() {
        let issue = |p, c| IssueMetrics {
            issues_processed: p,
            issues_completed: c,
            issues_escalated: 0,
            issues_failed: 0,
            average_issue_duration_ms: None,
        };
        let v = compute_verification_metrics(&[], None);
        let q = compute_quality_and_autonomy_metrics(&issue(4, 3), &v);
        assert_eq!(q.resolution_rate, Some(0.75));
        assert_eq!(q.autonomy_rate, Some(0.75));
        assert_eq!(q.human_intervention_rate, Some(0.25));
        let q = compute_quality_and_autonomy_metrics(&issue(0, 0), &v);
        assert_eq!((q.resolution_rate, q.human_intervention_rate), (None, None));
        let value = q.to_value();
        assert!(value["retry_rate"].is_null() && value["failure_rate"].is_null());
        assert_eq!(value["autonomy_rate_is_placeholder"], json!(true));
        assert_eq!(
            value["retry_rate_unavailable_reason"],
            json!(RETRY_RATE_UNAVAILABLE_REASON)
        );
    }

    #[test]
    fn build_report_project_filter_replaces_the_run_section() {
        let evs = [
            json!({"event_type": "run.started", "run_id": "r", "timestamp": "2026-09-04T10:00:00Z"}),
            json!({"event_type": "issue.started", "issue_run_id": "i", "project": "alpha", "timestamp": "2026-09-04T10:00:00Z"}),
        ];
        let all = build_report(None, None, None, None, Some(&evs));
        assert_eq!(all.to_value()["run"]["runs_total"], json!(1));
        let one = build_report(None, None, None, Some("alpha"), Some(&evs));
        assert_eq!(
            one.to_value()["run"],
            json!({"not_applicable_reason": "a run is not scoped to a single project"})
        );
        assert_eq!(one.to_value()["issue"]["issues_processed"], json!(1));
        assert_eq!(one.to_value()["scope"]["project"], json!("alpha"));
        let keys: Vec<String> = all
            .to_value()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            keys,
            [
                "run",
                "issue",
                "verification",
                "quality_and_autonomy",
                "classification",
                "failure_taxonomy",
                "scope"
            ]
        );
    }

    #[test]
    fn format_report_headers_and_na_lines() {
        let empty = build_report(None, None, None, None, Some(&[]));
        let text = format_report(&empty);
        assert!(text.starts_with("Loop Metrics (all time)\n\nRuns\n  Total          0\n"));
        assert!(text.contains("  Avg duration   N/A\n"));
        assert!(text.contains(RETRY_RATE_UNAVAILABLE_REASON));
        assert!(text.contains("no issues classified in this window"));
        assert!(text.contains("no escalations in this window"));
        assert!(text.ends_with("  Avg verification duration   N/A"));
        let scoped = build_report(
            None,
            Some("2026-09-01"),
            Some("2026-09-04"),
            Some("kurrant"),
            Some(&[]),
        );
        let text = format_report(&scoped);
        assert!(
            text.starts_with("Loop Metrics (2026-09-01 to 2026-09-04) \u{2014} project: kurrant\n")
        );
        assert!(text.contains("  N/A (a run is not scoped to a single project)"));
    }

    #[test]
    fn format_report_failure_breakdown_sorts_by_share() {
        let f = |reason: &str| json!({"event_type": "issue.escalated", "data": {"reason": reason}});
        let evs = [
            f("needs_clarification"),
            f("verification_failed"),
            f("verification_failed"),
        ];
        let text = format_report(&build_report(None, None, None, None, Some(&evs)));
        let at_v = text.find("  verification    66.7%").unwrap();
        let at_r = text.find("  requirement     33.3%").unwrap();
        assert!(at_v < at_r);
    }

    #[test]
    fn bucketed_reports_put_the_remainder_on_the_oldest_bucket() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let reports = bucketed_reports(None, 90, 7, Some(&[]), today);
        assert_eq!(reports.len(), 13);
        let first = &reports[0];
        assert_eq!(first.until_date.as_deref(), Some("2026-06-18"));
        assert_eq!(first.since_date.as_deref(), Some("2026-06-13")); // 6-day span
        assert_eq!(reports[12].until_date.as_deref(), Some("2026-09-10"));
        assert_eq!(reports[12].since_date.as_deref(), Some("2026-09-04"));
        let daily = bucketed_reports(None, 3, 1, Some(&[]), today);
        let days: Vec<&str> = daily
            .iter()
            .map(|r| r.since_date.as_deref().unwrap())
            .collect();
        assert_eq!(days, ["2026-09-08", "2026-09-09", "2026-09-10"]);
        assert!(daily.iter().all(|r| r.since_date == r.until_date));
    }

    #[test]
    fn bucketed_reports_slice_preloaded_events_by_utc_day() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let evs = [
            json!({"event_type": "issue.started", "issue_run_id": "a", "timestamp": "2026-09-09T23:59:59Z"}),
            json!({"event_type": "issue.started", "issue_run_id": "b", "timestamp": "2026-09-10T00:00:01Z"}),
            json!({"event_type": "issue.started", "issue_run_id": "c", "timestamp": "2026-09-10T05:00:00Z"}),
        ];
        let reports = bucketed_reports(None, 2, 1, Some(&evs), today);
        let counts: Vec<usize> = reports.iter().map(|r| r.issue.issues_processed).collect();
        assert_eq!(counts, [1, 2]);
    }
}
