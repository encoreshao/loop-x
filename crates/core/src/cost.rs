//! AI usage/cost extraction and cost metrics (mirrors `bin/cost.py`).
//!
//! Claude only: `claude -p --output-format json` returns a precise
//! `total_cost_usd` and token breakdown; other CLIs are skipped.

use crate::metrics::{self, compute_issue_metrics, field_key};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::Path;

pub const RETRY_WASTE_UNAVAILABLE_REASON: &str =
    "no retry tracking exists yet - wasted tokens can't be distinguished from productive ones";

/// Pull `{provider, model, input_tokens, output_tokens, cache_read_tokens,
/// cache_write_tokens, duration_ms, cost_usd}` out of a parsed
/// `claude -p --output-format json` result, or `None` when it doesn't look
/// like one (no `total_cost_usd` or no `usage`).
pub fn extract_claude_usage(parsed: &Value) -> Option<Value> {
    let obj = parsed.as_object()?;
    if !obj.contains_key("total_cost_usd") || !obj.contains_key("usage") {
        return None;
    }
    let usage = &obj["usage"];
    let model = match obj.get("modelUsage").and_then(Value::as_object) {
        Some(m) if m.len() == 1 => json!(m.keys().next()),
        _ => Value::Null,
    };
    let field = |v: &Value, k: &str| v.get(k).cloned().unwrap_or(Value::Null);
    Some(json!({
        "provider": "anthropic",
        "model": model,
        "input_tokens": field(usage, "input_tokens"),
        "output_tokens": field(usage, "output_tokens"),
        "cache_read_tokens": field(usage, "cache_read_input_tokens"),
        "cache_write_tokens": field(usage, "cache_creation_input_tokens"),
        "duration_ms": field(parsed, "duration_ms"),
        "cost_usd": field(parsed, "total_cost_usd"),
    }))
}

/// `parsed["result"]` when it is a string, else a fixed placeholder.
pub fn extract_result_text(parsed: &Value) -> String {
    match parsed.get("result") {
        Some(Value::String(s)) => s.clone(),
        _ => "(no result text in CLI output)".to_string(),
    }
}

/// Read `path` and try to parse it as a JSON object. `(None, raw)` when it
/// isn't one, `(None, "")` when the file can't be read.
pub fn read_cli_output_file(path: &Path) -> (Option<Value>, String) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return (None, String::new());
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) if v.is_object() => (Some(v), raw),
        _ => (None, raw),
    }
}

/// `claude --max-budget-usd` argv for a per-call cost cap; empty without a cap.
pub fn budget_args(max_budget_usd: Option<f64>) -> Vec<String> {
    match max_budget_usd {
        None => Vec::new(),
        Some(v) => vec!["--max-budget-usd".to_string(), format!("{v:.2}")],
    }
}

/// Cap for the next call: what is left of the run's budget, never below
/// `floor` (Python default 0.05). `None` when the run has no budget.
pub fn remaining_budget(
    max_cost_usd: Option<f64>,
    spent_usd: Option<f64>,
    floor: f64,
) -> Option<f64> {
    max_cost_usd.map(|max| floor.max(max - spent_usd.unwrap_or(0.0)))
}

#[derive(Debug, Clone, PartialEq)]
pub struct CostMetrics {
    pub total_cost_usd: f64,
    pub total_tokens: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cache_write_tokens: i64,
    pub cache_hit_rate: Option<f64>,
    pub cost_per_issue: Option<f64>,
    pub cost_per_resolution: Option<f64>,
}

impl CostMetrics {
    /// The Python dict, including the always-`None` `wasted_cost`.
    pub fn to_value(&self) -> Value {
        json!({
            "total_cost_usd": self.total_cost_usd,
            "total_tokens": self.total_tokens,
            "total_input_tokens": self.total_input_tokens,
            "total_output_tokens": self.total_output_tokens,
            "total_cache_read_tokens": self.total_cache_read_tokens,
            "total_cache_write_tokens": self.total_cache_write_tokens,
            "cache_hit_rate": self.cache_hit_rate,
            "cost_per_issue": self.cost_per_issue,
            "cost_per_resolution": self.cost_per_resolution,
            "wasted_cost": null,
            "wasted_cost_unavailable_reason": RETRY_WASTE_UNAVAILABLE_REASON,
        })
    }
}

fn tokens(data: &Value, key: &str) -> i64 {
    match data.get(key) {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        _ => 0,
    }
}

fn run_completed_cost(event: &Value) -> Option<(&Value, f64)> {
    if event.get("event_type").and_then(Value::as_str) != Some("run.completed") {
        return None;
    }
    let data = event.get("data").filter(|d| d.is_object())?;
    match data.get("cost_usd") {
        None | Some(Value::Null) => None,
        Some(c) => Some((data, c.as_f64().unwrap_or(0.0))),
    }
}

/// Totals from `run.completed` events carrying a `cost_usd`, plus per-issue and
/// per-resolution cost over only those priced runs' issue events.
pub fn compute_cost_metrics(events: &[Value]) -> CostMetrics {
    let mut total_cost_usd = 0.0;
    let (mut input, mut output, mut cache_read, mut cache_write) = (0i64, 0i64, 0i64, 0i64);
    let mut priced: HashSet<String> = HashSet::new();
    for event in events {
        let Some((data, cost)) = run_completed_cost(event) else {
            continue;
        };
        priced.insert(field_key(event, "run_id"));
        total_cost_usd += cost;
        input += tokens(data, "input_tokens");
        output += tokens(data, "output_tokens");
        cache_read += tokens(data, "cache_read_tokens");
        cache_write += tokens(data, "cache_write_tokens");
    }
    let priced_events: Vec<Value> = events
        .iter()
        .filter(|e| priced.contains(&field_key(e, "run_id")))
        .cloned()
        .collect();
    let issues = compute_issue_metrics(&priced_events, None);
    let eligible = cache_read + input;
    CostMetrics {
        total_cost_usd,
        total_tokens: input + output + cache_read + cache_write,
        total_input_tokens: input,
        total_output_tokens: output,
        total_cache_read_tokens: cache_read,
        total_cache_write_tokens: cache_write,
        cache_hit_rate: (eligible != 0).then(|| cache_read as f64 / eligible as f64),
        cost_per_issue: (issues.issues_processed != 0)
            .then(|| total_cost_usd / issues.issues_processed as f64),
        cost_per_resolution: (issues.issues_completed != 0)
            .then(|| total_cost_usd / issues.issues_completed as f64),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CostReport {
    pub cost: CostMetrics,
    pub since_date: Option<String>,
    pub until_date: Option<String>,
}

impl CostReport {
    /// `{"cost": {...}, "scope": {"since_date", "until_date"}}`.
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("cost".into(), self.cost.to_value());
        m.insert(
            "scope".into(),
            json!({"since_date": self.since_date, "until_date": self.until_date}),
        );
        Value::Object(m)
    }
}

/// Read the event log once (or use `all_events`) and compute the report.
pub fn build_cost_report(
    events_dir: Option<&Path>,
    since_date: Option<&str>,
    until_date: Option<&str>,
    all_events: Option<&[Value]>,
) -> CostReport {
    let owned;
    let events = match all_events {
        Some(e) => e,
        None => {
            owned = metrics::read_events(events_dir, since_date, until_date);
            &owned
        }
    };
    CostReport {
        cost: compute_cost_metrics(events),
        since_date: since_date.map(str::to_string),
        until_date: until_date.map(str::to_string),
    }
}

/// `{:,}` for an integer.
pub fn fmt_count(value: i64) -> String {
    group_thousands(&value.to_string())
}

/// `${:,.2f}`.
pub fn fmt_money_value(value: f64) -> String {
    let s = format!("{:.2}", value.abs());
    let (int, frac) = s.split_once('.').unwrap_or((&s, "00"));
    let sign = if value.is_sign_negative() && s != "0.00" {
        "-"
    } else {
        ""
    };
    format!("${sign}{}.{frac}", group_thousands(int))
}

fn group_thousands(digits: &str) -> String {
    let (sign, digits) = match digits.strip_prefix('-') {
        Some(d) => ("-", d),
        None => ("", digits),
    };
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    format!("{sign}{out}")
}

fn fmt_money(value: Option<f64>) -> String {
    value.map_or_else(|| "N/A".to_string(), fmt_money_value)
}

fn fmt_percent(value: Option<f64>) -> String {
    value.map_or_else(|| "N/A".to_string(), |v| format!("{:.1}%", v * 100.0))
}

/// The plain-text report the `cost report` command prints.
pub fn format_cost_report(report: &CostReport) -> String {
    let header = match (&report.since_date, &report.until_date) {
        (None, None) => "Loop Cost (all time)".to_string(),
        (s, u) => format!(
            "Loop Cost ({} to {})",
            s.as_deref().unwrap_or("None"),
            u.as_deref().unwrap_or("None")
        ),
    };
    let c = &report.cost;
    [
        header,
        String::new(),
        format!(
            "Total AI cost         {}",
            fmt_money(Some(c.total_cost_usd))
        ),
        format!("Total tokens          {}", fmt_count(c.total_tokens)),
        format!(
            "Cache read tokens     {}",
            fmt_count(c.total_cache_read_tokens)
        ),
        format!(
            "Cache write tokens    {}",
            fmt_count(c.total_cache_write_tokens)
        ),
        format!("Cache hit rate        {}", fmt_percent(c.cache_hit_rate)),
        String::new(),
        format!("Cost per issue         {}", fmt_money(c.cost_per_issue)),
        format!(
            "Cost per resolution    {}",
            fmt_money(c.cost_per_resolution)
        ),
        format!("Wasted cost            N/A ({RETRY_WASTE_UNAVAILABLE_REASON})"),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real_claude_json() -> Value {
        json!({
            "duration_api_ms": 2792,
            "total_cost_usd": 0.14211259999999998,
            "usage": {"input_tokens": 2, "cache_creation_input_tokens": 34536,
                      "cache_read_input_tokens": 19123, "output_tokens": 14},
            "modelUsage": {"claude-sonnet-5": {"inputTokens": 2}},
            "result": "Hi! What can I help you with today?",
            "duration_ms": 3109
        })
    }

    #[test]
    fn extracts_usage_from_real_shape() {
        assert_eq!(
            extract_claude_usage(&real_claude_json()).unwrap(),
            json!({
                "provider": "anthropic", "model": "claude-sonnet-5",
                "input_tokens": 2, "output_tokens": 14,
                "cache_read_tokens": 19123, "cache_write_tokens": 34536,
                "duration_ms": 3109, "cost_usd": 0.14211259999999998
            })
        );
    }

    #[test]
    fn usage_none_on_missing_fields_or_other_shapes() {
        for k in ["total_cost_usd", "usage"] {
            let mut v = real_claude_json();
            v.as_object_mut().unwrap().remove(k);
            assert!(extract_claude_usage(&v).is_none());
        }
        assert!(extract_claude_usage(&json!({"type": "thread.started"})).is_none());
    }

    #[test]
    fn model_is_null_without_exactly_one_model_key() {
        for mu in [json!({}), json!({"a": {}, "b": {}})] {
            let mut v = real_claude_json();
            v["modelUsage"] = mu;
            assert_eq!(extract_claude_usage(&v).unwrap()["model"], Value::Null);
        }
        let mut v = real_claude_json();
        v.as_object_mut().unwrap().remove("modelUsage");
        assert_eq!(extract_claude_usage(&v).unwrap()["model"], Value::Null);
    }

    #[test]
    fn result_text_or_placeholder() {
        assert_eq!(
            extract_result_text(&real_claude_json()),
            "Hi! What can I help you with today?"
        );
        assert_eq!(
            extract_result_text(&json!({})),
            "(no result text in CLI output)"
        );
        assert_eq!(
            extract_result_text(&json!({"result": 3})),
            "(no result text in CLI output)"
        );
    }

    #[test]
    fn cli_output_file_reading() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("o.json");
        std::fs::write(&p, "[1, 2]").unwrap();
        assert_eq!(read_cli_output_file(&p), (None, "[1, 2]".to_string()));
        std::fs::write(&p, "{\"a\": 1}").unwrap();
        assert!(read_cli_output_file(&p).0.is_some());
        assert_eq!(
            read_cli_output_file(&d.path().join("missing")),
            (None, String::new())
        );
    }

    fn run_completed(run_id: &str, data: Value) -> Value {
        json!({"event_type": "run.completed", "run_id": run_id, "data": data})
    }
    fn issue(t: &str, id: &str, run: &str) -> Value {
        let ts = if t == "issue.started" {
            "2026-09-04T10:00:00.000Z"
        } else {
            "2026-09-04T10:05:00.000Z"
        };
        json!({"event_type": t, "run_id": run, "issue_run_id": id, "timestamp": ts})
    }
    fn priced(cost: f64, i: i64, o: i64, cr: i64, cw: i64) -> Value {
        json!({"cost_usd": cost, "input_tokens": i, "output_tokens": o,
               "cache_read_tokens": cr, "cache_write_tokens": cw})
    }

    #[test]
    fn sums_and_divides() {
        let events = [
            run_completed("run_1", priced(1.5, 100, 50, 10, 5)),
            run_completed("run_2", priced(2.5, 200, 60, 20, 15)),
            issue("issue.started", "a", "run_1"),
            issue("issue.completed", "a", "run_1"),
            issue("issue.started", "b", "run_2"),
        ];
        let m = compute_cost_metrics(&events);
        assert_eq!(m.total_cost_usd, 4.0);
        assert_eq!(m.total_tokens, 460);
        assert_eq!(m.cost_per_issue, Some(2.0));
        assert_eq!(m.cost_per_resolution, Some(4.0));
    }

    #[test]
    fn cache_breakdown_and_hit_rate() {
        let events = [
            run_completed("r1", priced(1.0, 100, 50, 300, 5)),
            run_completed("r2", priced(1.0, 50, 20, 150, 0)),
        ];
        let m = compute_cost_metrics(&events);
        assert_eq!(m.total_input_tokens, 150);
        assert_eq!(m.total_output_tokens, 70);
        assert_eq!(m.total_cache_read_tokens, 450);
        assert_eq!(m.total_cache_write_tokens, 5);
        assert_eq!(m.cache_hit_rate, Some(450.0 / 600.0));
        assert_eq!(compute_cost_metrics(&[]).cache_hit_rate, None);
    }

    #[test]
    fn unpriced_runs_do_not_dilute() {
        let events = [
            run_completed("claude", priced(10.0, 1, 1, 0, 0)),
            issue("issue.started", "c1", "claude"),
            issue("issue.completed", "c1", "claude"),
            run_completed("codex", json!({})),
            issue("issue.started", "x1", "codex"),
            issue("issue.completed", "x1", "codex"),
            issue("issue.started", "x2", "codex"),
            issue("issue.completed", "x2", "codex"),
        ];
        let m = compute_cost_metrics(&events);
        assert_eq!(m.total_cost_usd, 10.0);
        assert_eq!(m.total_tokens, 2);
        assert_eq!(m.cost_per_issue, Some(10.0));
        assert_eq!(m.cost_per_resolution, Some(10.0));
    }

    #[test]
    fn zero_denominators_are_none_and_wasted_is_fixed() {
        let m = compute_cost_metrics(&[]);
        assert_eq!((m.total_cost_usd, m.total_tokens), (0.0, 0));
        assert_eq!((m.cost_per_issue, m.cost_per_resolution), (None, None));
        let v = m.to_value();
        assert_eq!(v["wasted_cost"], Value::Null);
        assert_eq!(
            v["wasted_cost_unavailable_reason"],
            RETRY_WASTE_UNAVAILABLE_REASON
        );
    }

    #[test]
    fn report_reads_events_dir_and_formats() {
        let d = tempfile::tempdir().unwrap();
        let line = run_completed("r1", priced(1234.5, 1000, 1, 3000, 0)).to_string();
        std::fs::write(d.path().join("2026-09-04.jsonl"), format!("{line}\n")).unwrap();
        let report = build_cost_report(Some(d.path()), None, None, None);
        assert_eq!(report.to_value()["cost"]["total_cost_usd"], 1234.5);
        let text = format_cost_report(&report);
        assert!(text.starts_with("Loop Cost (all time)\n\nTotal AI cost         $1,234.50\n"));
        assert!(text.contains("Total tokens          4,001"));
        assert!(text.contains("Cache hit rate        75.0%"));
        assert!(text.contains("Cost per issue         N/A"));
        let windowed =
            build_cost_report(Some(d.path()), Some("2026-08-29"), Some("2026-09-04"), None);
        assert!(format_cost_report(&windowed).starts_with("Loop Cost (2026-08-29 to 2026-09-04)"));
    }

    #[test]
    fn money_and_count_formatting() {
        assert_eq!(fmt_money_value(0.0), "$0.00");
        assert_eq!(fmt_money_value(1234567.891), "$1,234,567.89");
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(1000), "1,000");
        assert_eq!(fmt_count(123456789), "123,456,789");
    }

    #[test]
    fn budget_helpers() {
        assert!(budget_args(None).is_empty());
        assert_eq!(budget_args(Some(2.0)), ["--max-budget-usd", "2.00"]);
        assert_eq!(remaining_budget(None, Some(1.0), 0.05), None);
        assert_eq!(remaining_budget(Some(5.0), Some(1.5), 0.05), Some(3.5));
        assert_eq!(remaining_budget(Some(5.0), Some(9.0), 0.05), Some(0.05));
        assert_eq!(remaining_budget(Some(5.0), None, 0.05), Some(5.0));
    }
}
