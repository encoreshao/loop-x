//! Reader over the event ledger: one [`RunRecord`] per `run_id`, built from
//! `loop.result` events (last one wins). Mirrors `bin/ledger.py`, including
//! the legacy `result.json` backfill.

use crate::events::{self, fmt_timestamp};
use crate::loop_serialize;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub const BACKFILL_FILENAME: &str = "backfill-loop-result.jsonl";

#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    pub run_id: Option<String>,
    pub loop_id: Option<String>,
    pub definition: Option<String>,
    pub loop_name: Option<String>,
    pub final_state: Option<String>,
    pub stop_reason: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub total_cost_usd: Option<f64>,
    pub iterations: Vec<Value>,
    pub verified_success: bool,
    pub has_result: bool,
    pub budget_overall: Option<String>,
    pub complete: bool,
}

impl RunRecord {
    fn blank(run_id: Option<String>) -> Self {
        Self {
            run_id,
            loop_id: None,
            definition: None,
            loop_name: None,
            final_state: None,
            stop_reason: None,
            started_at: None,
            finished_at: None,
            duration_ms: None,
            total_cost_usd: None,
            iterations: Vec::new(),
            verified_success: false,
            has_result: false,
            budget_overall: None,
            complete: true,
        }
    }
}

/// Filters for [`iter_runs`]. `days` is a rolling window; `since_date` /
/// `until_date` are inclusive UTC `YYYY-MM-DD` calendar days.
#[derive(Debug, Default, Clone)]
pub struct RunQuery<'a> {
    pub days: Option<i64>,
    pub loop_name: Option<&'a str>,
    pub events_dir: Option<&'a Path>,
    pub since_date: Option<&'a str>,
    pub until_date: Option<&'a str>,
}

fn parse_ts(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn str_of(d: &Value, key: &str) -> Option<String> {
    d.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `event.get("data") or {}`.
fn data_of(event: &Value) -> Value {
    match event.get("data") {
        Some(v) if crate::loop_serialize::truthy(v) => v.clone(),
        _ => json!({}),
    }
}

/// Insertion-ordered map, like a Python dict.
struct Ordered<V> {
    keys: Vec<Option<String>>,
    vals: Vec<V>,
    idx: HashMap<Option<String>, usize>,
}

impl<V> Ordered<V> {
    fn new() -> Self {
        Self {
            keys: vec![],
            vals: vec![],
            idx: HashMap::new(),
        }
    }
    fn get(&self, k: &Option<String>) -> Option<&V> {
        self.idx.get(k).map(|&i| &self.vals[i])
    }
    fn contains(&self, k: &Option<String>) -> bool {
        self.idx.contains_key(k)
    }
    fn set(&mut self, k: Option<String>, v: V) {
        match self.idx.get(&k) {
            Some(&i) => self.vals[i] = v,
            None => {
                self.idx.insert(k.clone(), self.keys.len());
                self.keys.push(k);
                self.vals.push(v);
            }
        }
    }
    fn set_default(&mut self, k: Option<String>, v: V) {
        if !self.contains(&k) {
            self.set(k, v);
        }
    }
    fn into_iter(self) -> impl Iterator<Item = (Option<String>, V)> {
        self.keys.into_iter().zip(self.vals)
    }
}

fn terminal_state(event_type: &str) -> Option<&'static str> {
    match event_type {
        "loop.completed" => Some("completed"),
        "loop.failed" => Some("failed"),
        "loop.stopped" => Some("stopped"),
        _ => None,
    }
}

/// Fold an already-read event stream into run records (newest first).
/// `iter_runs` is this plus the file scan; tests inject events directly.
pub fn runs_from_events(
    events: &[Value],
    days: Option<i64>,
    loop_name: Option<&str>,
    since_date: Option<&str>,
    until_date: Option<&str>,
) -> Vec<RunRecord> {
    runs_from_events_at(events, days, loop_name, since_date, until_date, Utc::now())
}

/// [`runs_from_events`] with the rolling-window clock given (dashboard pages
/// and tests run against a fixed "now").
pub fn runs_from_events_at(
    events: &[Value],
    days: Option<i64>,
    loop_name: Option<&str>,
    since_date: Option<&str>,
    until_date: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<RunRecord> {
    let cutoff = days.map(|d| now - Duration::days(d));
    let mut results: Ordered<(String, Value)> = Ordered::new();
    let mut started: Ordered<(String, Value)> = Ordered::new();
    let mut terminal: Ordered<(String, String, Value)> = Ordered::new();

    for event in events {
        let run_id = str_of(event, "run_id");
        let ts = str_of(event, "timestamp").unwrap_or_default();
        if let Some(cutoff) = cutoff {
            match parse_ts(&ts) {
                Some(p) if p >= cutoff => {}
                _ => continue,
            }
        }
        if since_date.is_some() || until_date.is_some() {
            let Some(p) = parse_ts(&ts) else { continue };
            let day = p.format("%Y-%m-%d").to_string();
            if since_date.is_some_and(|s| day.as_str() < s)
                || until_date.is_some_and(|u| day.as_str() > u)
            {
                continue;
            }
        }
        let etype = str_of(event, "event_type").unwrap_or_default();
        if etype == "loop.result" {
            if results.get(&run_id).is_none_or(|(prev, _)| ts >= *prev) {
                results.set(run_id, (ts, data_of(event)));
            }
        } else if terminal_state(&etype).is_some() {
            terminal.set(run_id, (ts, etype, data_of(event)));
        } else if etype == "loop.started" {
            started.set_default(run_id, (ts, data_of(event)));
        }
    }

    let mut records: Vec<(String, RunRecord)> = Vec::new();
    let result_ids: HashSet<Option<String>> = results.idx.keys().cloned().collect();
    for (run_id, (ts, d)) in results.into_iter() {
        let mut r = RunRecord::blank(str_of(&d, "run_id").or(run_id));
        r.loop_id = str_of(&d, "loop_id");
        r.definition = str_of(&d, "definition");
        r.loop_name = r.definition.clone();
        r.final_state = str_of(&d, "final_state");
        r.stop_reason = str_of(&d, "stop_reason");
        r.started_at = str_of(&d, "started_at");
        r.finished_at = str_of(&d, "finished_at");
        r.duration_ms = d.get("duration_ms").and_then(as_i64_lenient);
        r.total_cost_usd = d.get("total_cost_usd").and_then(Value::as_f64);
        r.iterations = iterations_of(&d);
        r.verified_success = d
            .get("verified_success")
            .is_some_and(loop_serialize::truthy);
        r.budget_overall = str_of(&d, "budget_overall");
        r.has_result = true;
        records.push((ts, r));
    }
    let started_ref = &started;
    let mut terminal_ids = HashSet::new();
    for (run_id, (ts, etype, d)) in terminal.into_iter() {
        terminal_ids.insert(run_id.clone());
        if result_ids.contains(&run_id) {
            continue;
        }
        let (sts, sd) = match started_ref.get(&run_id) {
            Some((t, d)) => (Some(t.clone()), d.clone()),
            None => (None, json!({})),
        };
        let name = str_of(&d, "definition").or_else(|| str_of(&sd, "definition"));
        let mut r = RunRecord::blank(run_id);
        r.loop_id = str_of(&d, "loop_id").or_else(|| str_of(&sd, "loop_id"));
        r.definition = name.clone();
        r.loop_name = name;
        r.final_state =
            str_of(&d, "final_state").or_else(|| terminal_state(&etype).map(str::to_string));
        r.stop_reason = str_of(&d, "stop_reason");
        r.started_at = sts;
        r.finished_at = Some(ts.clone());
        r.duration_ms = d.get("duration_ms").and_then(as_i64_lenient);
        r.total_cost_usd = d.get("total_cost_usd").and_then(Value::as_f64);
        r.iterations = iterations_of(&d);
        records.push((ts, r));
    }
    for (run_id, (ts, d)) in started.into_iter() {
        if result_ids.contains(&run_id) || terminal_ids.contains(&run_id) {
            continue;
        }
        let mut r = RunRecord::blank(run_id);
        r.loop_id = str_of(&d, "loop_id");
        r.definition = str_of(&d, "definition");
        r.loop_name = r.definition.clone();
        r.final_state = Some("incomplete".into());
        r.started_at = Some(ts.clone());
        r.complete = false;
        records.push((ts, r));
    }
    if let Some(l) = loop_name {
        records.retain(|(_, r)| r.loop_name.as_deref() == Some(l));
    }
    // Stable, newest first (Python: sort(reverse=True) keeps equal elements in order).
    records.sort_by(|a, b| b.0.cmp(&a.0));
    records.into_iter().map(|(_, r)| r).collect()
}

fn iterations_of(d: &Value) -> Vec<Value> {
    d.get("iterations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn as_i64_lenient(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

/// One record per run in the events dir, newest first.
pub fn iter_runs(q: &RunQuery) -> Vec<RunRecord> {
    iter_runs_at(q, Utc::now())
}

/// [`iter_runs`] with the rolling-window clock given.
pub fn iter_runs_at(q: &RunQuery, now: DateTime<Utc>) -> Vec<RunRecord> {
    let mut since = q.since_date.map(str::to_string);
    if let Some(days) = q.days {
        let cutoff = now - Duration::days(days);
        // One day of slack; the non-date-named backfill file sorts after
        // every date stem so it is always included.
        let pushed = (cutoff - Duration::days(1)).format("%Y-%m-%d").to_string();
        since = Some(match since {
            Some(s) if s > pushed => s,
            _ => pushed,
        });
    }
    let events = events::iter_events(q.events_dir, since.as_deref(), None);
    runs_from_events_at(
        &events,
        q.days,
        q.loop_name,
        q.since_date,
        q.until_date,
        now,
    )
}

/// `run_<YYYYMMDD>_<HHMMSS>_...` -> the embedded UTC timestamp.
pub fn run_timestamp(run_id: &str) -> Option<DateTime<Utc>> {
    let rest = run_id.strip_prefix("run_")?;
    let b = rest.as_bytes();
    if b.len() < 16 || b[8] != b'_' || b[15] != b'_' {
        return None;
    }
    let (d, t) = (&rest[..8], &rest[9..15]);
    if !d.bytes().chain(t.bytes()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(
        d[..4].parse().ok()?,
        d[4..6].parse().ok()?,
        d[6..8].parse().ok()?,
    )?;
    let time = NaiveTime::from_hms_opt(
        t[..2].parse().ok()?,
        t[2..4].parse().ok()?,
        t[4..6].parse().ok()?,
    )?;
    Some(NaiveDateTime::new(date, time).and_utc())
}

/// run_ids that already have a `loop.result` anywhere in `events_dir`; only
/// lines mentioning "loop.result" are parsed.
fn loop_result_run_ids(events_dir: &Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    for path in events::jsonl_files(events_dir) {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        for line in String::from_utf8_lossy(&bytes).lines() {
            if !line.contains("loop.result") {
                continue;
            }
            if let Ok(ev) = serde_json::from_str::<Value>(line) {
                if ev.get("event_type").and_then(Value::as_str) == Some("loop.result") {
                    if let Some(id) = ev.get("run_id").and_then(Value::as_str) {
                        ids.insert(id.to_string());
                    }
                }
            }
        }
    }
    ids
}

/// The JSONL line for one `result.json`, or `None` to skip it.
fn backfill_line(path: &Path, seen: &mut HashSet<String>) -> Option<String> {
    let data = loop_serialize::read_result(path).ok()?;
    let status = data
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("finished");
    if status != "finished" {
        return None;
    }
    let run_id = str_of(&data, "run_id").filter(|s| !s.is_empty())?;
    if seen.contains(&run_id) {
        return None;
    }
    let mut summary = loop_serialize::result_summary(&data, Default::default()).ok()?;
    let has_iterations = summary["iterations"]
        .as_array()
        .is_some_and(|a| !a.is_empty());
    if has_iterations && !loop_serialize::truthy(&summary["total_cost_usd"]) {
        // Legacy result.json coerced an unknown cost to 0, and a run that
        // called a model never costs exactly $0: book it as unknown.
        summary["total_cost_usd"] = Value::Null;
        for it in summary["iterations"].as_array_mut()? {
            it["cost_usd"] = Value::Null;
        }
    }
    let duration_ms = summary["duration_ms"].as_i64()?;
    let (started, finished) = match run_timestamp(&run_id) {
        Some(s) => (s, s + Duration::milliseconds(duration_ms)),
        None => {
            let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
            let f: DateTime<Utc> = mtime.into();
            (f - Duration::milliseconds(duration_ms), f)
        }
    };
    summary["started_at"] = json!(fmt_timestamp(started));
    summary["finished_at"] = json!(fmt_timestamp(finished));
    seen.insert(run_id.clone());
    let event = json!({
        "schema_version": events::SCHEMA_VERSION,
        "event_id": format!("evt_{}", events::new_uuid_hex()),
        "timestamp": fmt_timestamp(finished),
        "event_type": "loop.result",
        "run_id": run_id,
        "issue_run_id": null,
        "project": null,
        "issue_iid": null,
        "data": summary,
    });
    Some(format!("{}\n", events::to_py_json(&event)))
}

/// Append one `loop.result` event per legacy finished `result.json` whose
/// run_id has none anywhere in the events dir; returns how many were
/// appended. Idempotent; events go to `<events_dir>/backfill-loop-result.jsonl`.
pub fn backfill_from_results(
    results_dir: Option<&Path>,
    events_dir: Option<&Path>,
) -> std::io::Result<usize> {
    let events_dir = events_dir.map_or_else(events::default_events_dir, Path::to_path_buf);
    let mut seen = loop_result_run_ids(&events_dir);
    let mut lines = Vec::new();
    for path in loop_serialize::list_results(results_dir) {
        // write_result names each run dir by its run_id: skip known runs
        // without parsing their result.json on every startup.
        let dir_name = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if seen.contains(&dir_name) {
            continue;
        }
        // One corrupt result.json must not abort the whole backfill.
        if let Some(line) = backfill_line(&path, &mut seen) {
            lines.push(line);
        }
    }
    if lines.is_empty() {
        return Ok(0);
    }
    std::fs::create_dir_all(&events_dir)?;
    events::append_line(&events_dir.join(BACKFILL_FILENAME), &lines.concat())?;
    Ok(lines.len())
}

/// Best-effort backfill for process start; `None` (logged) on failure.
pub fn run_startup_backfill(
    results_dir: Option<&Path>,
    events_dir: Option<&Path>,
) -> Option<usize> {
    match backfill_from_results(results_dir, events_dir) {
        Ok(n) => Some(n),
        Err(e) => {
            eprintln!("ledger: startup backfill failed: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventFields;
    use crate::loop_result::{IterationResult, LoopResult, VerificationResult};
    use crate::loop_serialize::{summarize_results, summarize_run_costs, write_result};
    use crate::loop_state::LoopState;

    fn ev(t: &str, run: &str, ts: &str, data: Value) -> Value {
        json!({"event_type": t, "run_id": run, "timestamp": ts, "data": data})
    }

    fn q() -> (Option<i64>, Option<&'static str>) {
        (None, None)
    }

    fn runs(events: &[Value]) -> Vec<RunRecord> {
        let (d, l) = q();
        runs_from_events(events, d, l, None, None)
    }

    #[test]
    fn one_record_per_run_last_result_wins() {
        let events = vec![
            ev(
                "loop.started",
                "r1",
                "2026-10-06T09:00:00Z",
                json!({"definition": "gitlab-issue-loop"}),
            ),
            ev(
                "loop.result",
                "r1",
                "2026-10-06T09:05:00Z",
                json!({"run_id": "r1", "definition": "gitlab-issue-loop", "final_state": "failed", "total_cost_usd": 0.1, "iterations": []}),
            ),
            ev(
                "loop.result",
                "r1",
                "2026-10-06T09:06:00Z",
                json!({"run_id": "r1", "definition": "gitlab-issue-loop", "final_state": "completed", "total_cost_usd": 0.2, "iterations": []}),
            ),
        ];
        let r = runs(&events);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].final_state.as_deref(), Some("completed"));
        assert!(r[0].complete && r[0].has_result);
        assert_eq!(r[0].loop_name.as_deref(), Some("gitlab-issue-loop"));
    }

    #[test]
    fn incomplete_run_reported() {
        let r = runs(&[ev(
            "loop.started",
            "r2",
            "2026-10-06T09:00:00Z",
            json!({"definition": "topic-monitor-loop"}),
        )]);
        assert!(!r[0].complete);
        assert_eq!(r[0].final_state.as_deref(), Some("incomplete"));
        assert!(!r[0].has_result);
    }

    fn res(run: &str, ts: &str, def: &str) -> Value {
        ev(
            "loop.result",
            run,
            ts,
            json!({"run_id": run, "definition": def, "final_state": "completed", "iterations": []}),
        )
    }

    #[test]
    fn filter_by_loop_and_newest_first() {
        let events = vec![
            res("old", "2026-10-01T09:00:00Z", "x"),
            res("new", "2026-10-06T09:00:00Z", "x"),
            res("b", "2026-10-03T09:00:00Z", "y"),
        ];
        let ids: Vec<_> = runs(&events)
            .into_iter()
            .map(|r| r.run_id.unwrap())
            .collect();
        assert_eq!(ids, ["new", "b", "old"]);
        let only_y = runs_from_events(&events, None, Some("y"), None, None);
        assert_eq!(only_y.len(), 1);
    }

    #[test]
    fn days_filters_on_event_timestamp() {
        let now = Utc::now();
        let events = vec![
            res("old", &fmt_timestamp(now - Duration::days(10)), "x"),
            res("new", &fmt_timestamp(now - Duration::days(1)), "x"),
        ];
        let r = runs_from_events(&events, Some(3), None, None, None);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].run_id.as_deref(), Some("new"));
    }

    #[test]
    fn legacy_terminal_event_is_complete_and_result_wins() {
        let events = vec![
            ev(
                "loop.started",
                "t1",
                "2026-10-06T09:00:00Z",
                json!({"definition": "x"}),
            ),
            ev(
                "loop.completed",
                "t1",
                "2026-10-06T09:01:00Z",
                json!({"final_state": "completed", "stop_reason": "completed"}),
            ),
            ev(
                "loop.started",
                "t2",
                "2026-10-06T09:00:00Z",
                json!({"definition": "x"}),
            ),
            ev("loop.stopped", "t2", "2026-10-06T09:02:00Z", json!({})),
            ev("loop.failed", "t3", "2026-10-06T09:03:00Z", json!({})),
            res("t3", "2026-10-06T09:04:00Z", "x"),
        ];
        let m: HashMap<_, _> = runs(&events)
            .into_iter()
            .map(|r| (r.run_id.clone().unwrap(), r))
            .collect();
        let t1 = &m["t1"];
        assert!(t1.complete && !t1.has_result);
        assert_eq!(t1.final_state.as_deref(), Some("completed"));
        assert_eq!(t1.loop_name.as_deref(), Some("x"));
        assert!(t1.total_cost_usd.is_none() && t1.iterations.is_empty());
        assert_eq!(m["t2"].final_state.as_deref(), Some("stopped"));
        assert!(m["t3"].has_result);
    }

    #[test]
    fn since_until_window_matches_calendar_days() {
        let events = vec![
            res("a", "2026-10-01T23:59:59.000Z", "x"),
            res("b", "2026-10-02T00:00:00.000Z", "x"),
            res("c", "2026-10-03T23:59:59.000Z", "x"),
            res("d", "2026-10-04T00:00:00.000Z", "x"),
        ];
        let r = runs_from_events(&events, None, None, Some("2026-10-02"), Some("2026-10-03"));
        let mut ids: Vec<_> = r.into_iter().map(|r| r.run_id.unwrap()).collect();
        ids.sort();
        assert_eq!(ids, ["b", "c"]);
    }

    #[test]
    fn reads_real_events_dir() {
        let d = tempfile::tempdir().unwrap();
        events::emit(
            "loop.result",
            "r9",
            EventFields {
                data: Some(json!({"run_id": "r9", "definition": "x", "final_state": "completed", "iterations": []})),
                ..Default::default()
            },
            Some(d.path()),
        )
        .unwrap();
        let r = iter_runs(&RunQuery {
            events_dir: Some(d.path()),
            ..Default::default()
        });
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].run_id.as_deref(), Some("r9"));
    }

    #[test]
    fn run_timestamp_parses_embedded_time() {
        let t = run_timestamp("run_20260901_100000_a").unwrap();
        assert_eq!(fmt_timestamp(t), "2026-09-01T10:00:00.000Z");
        assert!(run_timestamp("unparseable_id").is_none());
        assert!(run_timestamp("run_20261301_100000_a").is_none());
        assert!(run_timestamp("run_20260901_100000").is_none());
    }

    // ---- backfill + golden master ----

    #[allow(clippy::too_many_arguments)]
    fn write_sample(
        dir: &Path,
        run_id: &str,
        cost: f64,
        state: LoopState,
        name: &str,
        secs: f64,
        verified: bool,
        overall: &str,
        n: i64,
    ) {
        let its = (1..=n)
            .map(|i| IterationResult {
                iteration: i,
                state,
                verification_results: vec![VerificationResult {
                    name: "tests".into(),
                    passed: verified,
                    exit_code: Some(0),
                    duration_ms: 1,
                    output: "x".into(),
                    evidence: json!({}),
                }],
                budget: json!({"overall": overall, "cost": {"used_usd": cost}, "runtime": {"used_seconds": secs}}),
                progressed: true,
            })
            .collect();
        let r = LoopResult::new(
            &format!("l_{run_id}"),
            run_id,
            name,
            state,
            its,
            state.as_str(),
        );
        write_result(&r, Some(dir), None, Some(&|_, _, _, _| Ok(()))).unwrap();
    }

    fn sample(dir: &Path, run_id: &str) {
        write_sample(
            dir,
            run_id,
            0.0,
            LoopState::Completed,
            "test-loop",
            0.0,
            true,
            "ok",
            1,
        );
    }

    #[test]
    fn backfill_is_idempotent_and_skips_known_runs() {
        let d = tempfile::tempdir().unwrap();
        let (res_dir, ev_dir) = (d.path().join("results"), d.path().join("events"));
        sample(&res_dir, "old_1");
        assert_eq!(
            backfill_from_results(Some(&res_dir), Some(&ev_dir)).unwrap(),
            1
        );
        assert_eq!(
            backfill_from_results(Some(&res_dir), Some(&ev_dir)).unwrap(),
            0
        );
        let text = std::fs::read_to_string(ev_dir.join(BACKFILL_FILENAME)).unwrap();
        assert_eq!(text.matches('\n').count(), 1);
        // a loop.result in another file also suppresses backfill
        sample(&res_dir, "old_2");
        std::fs::write(
            ev_dir.join("2026-10-01.jsonl"),
            "{\"event_type\": \"loop.result\", \"run_id\": \"old_2\"}\n",
        )
        .unwrap();
        assert_eq!(
            backfill_from_results(Some(&res_dir), Some(&ev_dir)).unwrap(),
            0
        );
    }

    #[test]
    fn backfill_skips_running_corrupt_and_missing() {
        let d = tempfile::tempdir().unwrap();
        let (res_dir, ev_dir) = (d.path().join("results"), d.path().join("events"));
        assert_eq!(
            backfill_from_results(Some(&res_dir), Some(&ev_dir)).unwrap(),
            0
        );
        let mut r = LoopResult::new("l", "run_running", "d", LoopState::Executing, vec![], "x");
        r.status = "running".into();
        write_result(&r, Some(&res_dir), None, Some(&|_, _, _, _| Ok(()))).unwrap();
        std::fs::create_dir_all(res_dir.join("bad")).unwrap();
        std::fs::write(res_dir.join("bad").join("result.json"), "{not json").unwrap();
        sample(&res_dir, "good");
        assert_eq!(
            backfill_from_results(Some(&res_dir), Some(&ev_dir)).unwrap(),
            1
        );
    }

    #[test]
    fn backfill_timestamp_from_run_id_and_zero_cost_becomes_unknown() {
        let d = tempfile::tempdir().unwrap();
        let (res_dir, ev_dir) = (d.path().join("results"), d.path().join("events"));
        write_sample(
            &res_dir,
            "run_20260901_100000_a",
            0.0,
            LoopState::Completed,
            "n",
            3600.0,
            true,
            "ok",
            2,
        );
        write_sample(
            &res_dir,
            "run_20260902_100000_b",
            1.5,
            LoopState::Completed,
            "n",
            60.0,
            true,
            "ok",
            1,
        );
        assert_eq!(
            backfill_from_results(Some(&res_dir), Some(&ev_dir)).unwrap(),
            2
        );
        let m: HashMap<_, _> = iter_runs(&RunQuery {
            events_dir: Some(&ev_dir),
            ..Default::default()
        })
        .into_iter()
        .map(|r| (r.run_id.clone().unwrap(), r))
        .collect();
        let a = &m["run_20260901_100000_a"];
        assert_eq!(a.started_at.as_deref(), Some("2026-09-01T10:00:00.000Z"));
        assert_eq!(a.finished_at.as_deref(), Some("2026-09-01T11:00:00.000Z"));
        assert!(a.total_cost_usd.is_none());
        assert!(a.iterations.iter().all(|i| i["cost_usd"].is_null()));
        assert_eq!(m["run_20260902_100000_b"].total_cost_usd, Some(1.5));
    }

    #[test]
    fn golden_master_summaries() {
        let d = tempfile::tempdir().unwrap();
        let (res_dir, ev_dir) = (d.path().join("results"), d.path().join("events"));
        let spec = [
            (
                "run_20260901_100000_a",
                1.5,
                LoopState::Completed,
                "g",
                3600.0,
                true,
                "ok",
                2,
            ),
            (
                "run_20260902_100000_b",
                2.0,
                LoopState::Failed,
                "g",
                1800.0,
                false,
                "warning",
                1,
            ),
            (
                "run_20260902_180000_c",
                0.5,
                LoopState::Escalated,
                "t",
                600.0,
                true,
                "exceeded",
                3,
            ),
            (
                "run_20260915_100000_d",
                4.0,
                LoopState::Completed,
                "t",
                7200.0,
                true,
                "ok",
                1,
            ),
            (
                "run_20261002_100000_e",
                0.0,
                LoopState::Completed,
                "g",
                0.0,
                true,
                "ok",
                1,
            ),
        ];
        for (id, cost, st, name, secs, ver, overall, n) in spec {
            write_sample(&res_dir, id, cost, st, name, secs, ver, overall, n);
        }
        // result.json with no budget at all, and an unparseable id
        let nb = LoopResult::new(
            "l",
            "run_20261003_100000_nobudget",
            "g",
            LoopState::Completed,
            vec![IterationResult {
                iteration: 1,
                state: LoopState::Completed,
                verification_results: vec![],
                budget: json!({}),
                progressed: true,
            }],
            "completed",
        );
        write_result(&nb, Some(&res_dir), None, Some(&|_, _, _, _| Ok(()))).unwrap();
        write_sample(
            &res_dir,
            "unparseable_id",
            1.0,
            LoopState::Completed,
            "g",
            0.0,
            true,
            "ok",
            1,
        );
        backfill_from_results(Some(&res_dir), Some(&ev_dir)).unwrap();
        let runs = iter_runs(&RunQuery {
            events_dir: Some(&ev_dir),
            ..Default::default()
        });
        let s = summarize_results(Some(&runs));
        assert_eq!(s.total_runs, 7);
        assert!((s.success_rate.unwrap() - 5.0 / 7.0).abs() < 1e-9);
        assert!((s.escalation_rate.unwrap() - 1.0 / 7.0).abs() < 1e-9);
        assert!((s.average_cost_usd.unwrap() - 9.0 / 7.0).abs() < 1e-9);
        assert!((s.efficiency_score.unwrap() - 0.015151515151515152).abs() < 1e-12);
        let c = summarize_run_costs(Some(&runs));
        assert_eq!(c.total_runs, 7);
        assert!((c.total_cost_usd - 9.0).abs() < 1e-9);
    }

    #[test]
    fn startup_backfill_is_best_effort() {
        let d = tempfile::tempdir().unwrap();
        let (res_dir, ev_dir) = (d.path().join("results"), d.path().join("events"));
        sample(&res_dir, "s1");
        assert_eq!(run_startup_backfill(Some(&res_dir), Some(&ev_dir)), Some(1));
        // events dir is a file: append fails, reported as None not a panic
        let bad = d.path().join("file");
        std::fs::write(&bad, "x").unwrap();
        sample(&res_dir, "s2");
        assert_eq!(run_startup_backfill(Some(&res_dir), Some(&bad)), None);
    }
}
