//! Lesson-reuse and lesson-effectiveness metrics from the event log, plus the
//! score feedback loop that down-/up-weights memory lessons by the outcome of
//! the issues that reused them (mirrors `bin/learning.py`).
//!
//! Events are `serde_json::Value` objects as in `outputs/events/*.jsonl`.

use crate::config::home_dir;
use crate::memory_store;
use crate::metrics::{self, field_key, Ordered};
use crate::pydumps;
use crate::slack::is_truthy;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

pub const FAILURES_PREVENTED_REASON: &str = "no counterfactual data exists - cannot know whether a specific issue would have failed without the lesson it cited, only whether it succeeded or failed with it";

pub const SCORE_CAP: i64 = 3;
pub const NEEDS_REVIEW_THRESHOLD: i64 = -2;

fn event_type(event: &Value) -> Option<&str> {
    event.get("event_type").and_then(Value::as_str)
}

fn data_of(event: &Value) -> Option<&Value> {
    event.get("data").filter(|d| d.is_object())
}

fn lesson_id_of(event: &Value) -> Option<&Value> {
    data_of(event)
        .and_then(|d| d.get("lesson_id"))
        .filter(|v| !v.is_null())
}

fn project_matches(event: &Value, project: Option<&str>) -> bool {
    match project {
        None => true,
        Some(p) => event.get("project").and_then(Value::as_str) == Some(p),
    }
}

/// `{issue_run_id: success?}` for every issue_run_id that reached a terminal
/// outcome (`issue.completed` = success, `issue.escalated`/`issue.failed` =
/// failure); later events win. Pending issues are absent.
fn issue_outcomes(events: &[Value]) -> Ordered<bool> {
    let mut outcomes = Ordered::new();
    for event in events {
        match event_type(event) {
            Some("issue.completed") => outcomes.insert(field_key(event, "issue_run_id"), true),
            Some("issue.escalated" | "issue.failed") => {
                outcomes.insert(field_key(event, "issue_run_id"), false)
            }
            _ => {}
        }
    }
    outcomes
}

/// One `("reused" | "fresh", succeeded)` per issue that reached a terminal
/// outcome, tagged `"reused"` when it had a `memory.reused` event.
pub fn memory_outcomes(events: &[Value]) -> Vec<(&'static str, bool)> {
    let outcomes = issue_outcomes(events);
    let reused: HashSet<String> = events
        .iter()
        .filter(|e| event_type(e) == Some("memory.reused"))
        .map(|e| field_key(e, "issue_run_id"))
        .collect();
    outcomes
        .iter()
        .map(|(id, ok)| {
            (
                if reused.contains(id) {
                    "reused"
                } else {
                    "fresh"
                },
                *ok,
            )
        })
        .collect()
}

/// `{lesson_id: category}` from `memory.created` events (category as created).
fn lesson_categories(events: &[Value], project: Option<&str>) -> Ordered<Value> {
    let mut categories = Ordered::new();
    for event in events {
        if !project_matches(event, project) || event_type(event) != Some("memory.created") {
            continue;
        }
        if let Some(id) = lesson_id_of(event) {
            let category = data_of(event)
                .and_then(|d| d.get("category"))
                .cloned()
                .unwrap_or(Value::Null);
            categories.insert(id.to_string(), category);
        }
    }
    categories
}

#[derive(Debug, Clone, PartialEq)]
pub struct LessonEffectiveness {
    pub lesson_id: Value,
    pub category: Value,
    pub times_reused: u64,
    pub successful_reuses: u64,
    pub failed_reuses: u64,
    pub effectiveness_rate: Option<f64>,
}

impl LessonEffectiveness {
    pub fn to_value(&self) -> Value {
        json!({
            "lesson_id": self.lesson_id,
            "category": self.category,
            "times_reused": self.times_reused,
            "successful_reuses": self.successful_reuses,
            "failed_reuses": self.failed_reuses,
            "effectiveness_rate": self.effectiveness_rate,
        })
    }
}

/// One entry per distinct lesson seen in a `memory.reused` event.
/// `effectiveness_rate = successful / (successful + failed)`, `None` if both are 0.
pub fn compute_lesson_effectiveness(
    events: &[Value],
    project: Option<&str>,
) -> Vec<LessonEffectiveness> {
    let outcomes = issue_outcomes(events);
    let categories = lesson_categories(events, project);
    let mut per_lesson: Ordered<(Value, u64, u64, u64)> = Ordered::new();
    for event in events {
        if !project_matches(event, project) || event_type(event) != Some("memory.reused") {
            continue;
        }
        let Some(id) = lesson_id_of(event) else {
            continue;
        };
        let key = id.to_string();
        if !per_lesson.contains_key(&key) {
            per_lesson.insert(key.clone(), (id.clone(), 0, 0, 0));
        }
        let stats = per_lesson.get_mut(&key).expect("just inserted");
        stats.1 += 1;
        match outcomes.get(&field_key(event, "issue_run_id")) {
            Some(true) => stats.2 += 1,
            Some(false) => stats.3 += 1,
            None => {}
        }
    }
    per_lesson
        .iter()
        .map(|(key, (id, times, ok, failed))| {
            let denom = ok + failed;
            LessonEffectiveness {
                lesson_id: id.clone(),
                category: categories.get(key).cloned().unwrap_or(Value::Null),
                times_reused: *times,
                successful_reuses: *ok,
                failed_reuses: *failed,
                effectiveness_rate: (denom != 0).then(|| *ok as f64 / denom as f64),
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReuseMetrics {
    pub lessons_created: u64,
    pub total_reuses: u64,
    pub memory_reuse_rate: Option<f64>,
    pub memory_success_rate: Option<f64>,
}

impl ReuseMetrics {
    pub fn to_value(&self) -> Value {
        json!({
            "lessons_created": self.lessons_created,
            "total_reuses": self.total_reuses,
            "memory_reuse_rate": self.memory_reuse_rate,
            "memory_success_rate": self.memory_success_rate,
            "failures_prevented": null,
            "failures_prevented_reason": FAILURES_PREVENTED_REASON,
        })
    }
}

/// Reuse arithmetic. `memory_reuse_rate` = issues with at least one reuse /
/// issues started (with a timestamp, deduped by id); `memory_success_rate` =
/// successful / (successful + failed) reuses over every lesson.
pub fn compute_reuse_metrics(events: &[Value], project: Option<&str>) -> ReuseMetrics {
    let outcomes = issue_outcomes(events);
    let mut lessons_created = 0;
    let mut total_reuses = 0;
    let mut processed: HashSet<String> = HashSet::new();
    let mut with_reuse: HashSet<String> = HashSet::new();
    let (mut ok, mut failed) = (0u64, 0u64);
    for event in events {
        if !project_matches(event, project) {
            continue;
        }
        match event_type(event) {
            Some("memory.created") => lessons_created += 1,
            Some("issue.started") => {
                if event.get("timestamp").is_some_and(|t| !t.is_null()) {
                    processed.insert(field_key(event, "issue_run_id"));
                }
            }
            Some("memory.reused") => {
                total_reuses += 1;
                let id = field_key(event, "issue_run_id");
                match outcomes.get(&id) {
                    Some(true) => ok += 1,
                    Some(false) => failed += 1,
                    None => {}
                }
                with_reuse.insert(id);
            }
            _ => {}
        }
    }
    let denom = ok + failed;
    ReuseMetrics {
        lessons_created,
        total_reuses,
        memory_reuse_rate: (!processed.is_empty())
            .then(|| with_reuse.len() as f64 / processed.len() as f64),
        memory_success_rate: (denom != 0).then(|| ok as f64 / denom as f64),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LearningReport {
    pub reuse: ReuseMetrics,
    pub lessons: Vec<LessonEffectiveness>,
    pub since_date: Option<String>,
    pub until_date: Option<String>,
    pub project: Option<String>,
}

impl LearningReport {
    /// `{"reuse", "lessons", "scope": {"since_date", "until_date", "project"}}`.
    pub fn to_value(&self) -> Value {
        json!({
            "reuse": self.reuse.to_value(),
            "lessons": self.lessons.iter().map(LessonEffectiveness::to_value).collect::<Vec<_>>(),
            "scope": {
                "since_date": self.since_date,
                "until_date": self.until_date,
                "project": self.project,
            },
        })
    }
}

/// Build the report from `all_events`, or read the event log when `None`.
pub fn build_learning_report(
    events_dir: Option<&Path>,
    since_date: Option<&str>,
    until_date: Option<&str>,
    project: Option<&str>,
    all_events: Option<&[Value]>,
) -> LearningReport {
    let owned;
    let events = match all_events {
        Some(e) => e,
        None => {
            owned = metrics::read_events(events_dir, since_date, until_date);
            &owned
        }
    };
    LearningReport {
        reuse: compute_reuse_metrics(events, project),
        lessons: compute_lesson_effectiveness(events, project),
        since_date: since_date.map(str::to_string),
        until_date: until_date.map(str::to_string),
        project: project.map(str::to_string),
    }
}

fn fmt_rate(value: Option<f64>) -> String {
    value.map_or_else(|| "N/A".to_string(), |v| format!("{:.1}%", v * 100.0))
}

/// The plain-text report the `learning` command prints.
pub fn format_learning_report(report: &LearningReport) -> String {
    let mut header = if report.since_date.is_none() && report.until_date.is_none() {
        "Loop Learning (all time)".to_string()
    } else {
        format!(
            "Loop Learning ({} to {})",
            report.since_date.as_deref().unwrap_or("None"),
            report.until_date.as_deref().unwrap_or("None")
        )
    };
    if let Some(p) = &report.project {
        header.push_str(&format!(" \u{2014} project: {p}"));
    }
    let r = &report.reuse;
    let mut lines = vec![
        header,
        String::new(),
        "Reuse".to_string(),
        format!("  Lessons created       {}", r.lessons_created),
        format!("  Total reuses          {}", r.total_reuses),
        format!("  Memory reuse rate     {}", fmt_rate(r.memory_reuse_rate)),
        format!(
            "  Memory success rate   {}",
            fmt_rate(r.memory_success_rate)
        ),
        format!("  Failures prevented    N/A ({FAILURES_PREVENTED_REASON})"),
        String::new(),
        "Lessons".to_string(),
    ];
    if report.lessons.is_empty() {
        lines.push("  N/A (no lessons reused in this window)".to_string());
    } else {
        let mut sorted: Vec<&LessonEffectiveness> = report.lessons.iter().collect();
        sorted.sort_by_key(|l| std::cmp::Reverse(l.times_reused));
        for l in sorted {
            let category = if is_truthy(&l.category) {
                pydumps::py_str(&l.category)
            } else {
                "uncategorized".to_string()
            };
            lines.push(format!(
                "  {}  ({category})  reused={} effectiveness={}",
                pydumps::py_str(&l.lesson_id),
                l.times_reused,
                fmt_rate(l.effectiveness_rate)
            ));
        }
    }
    lines.join("\n")
}

/// `<LOOP_ENGINEERING_HOME>/outputs/learning/applied.json`.
///
/// (Python keeps it under the repo checkout's `outputs/`; see
/// [`crate::events::default_events_dir`] for why this differs.)
pub fn default_applied_path() -> PathBuf {
    home_dir()
        .join("outputs")
        .join("learning")
        .join("applied.json")
}

fn load_applied(path: &Path) -> HashSet<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Array(items) => Some(
                items
                    .iter()
                    .map(pydumps::py_str)
                    .collect::<HashSet<String>>(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

/// `(score, flag, alias, already_flagged)` for `lesson_id`, if it still exists.
fn find_lesson(
    lesson_id: &str,
    alias: Option<&str>,
    root: Option<&Path>,
) -> Option<(i64, String, Option<String>, bool)> {
    let aliases: Vec<String> = match alias {
        Some(a) if !a.is_empty() => vec![a.to_string()],
        _ => memory_store::all_aliases(root),
    };
    for a in aliases {
        for entry in memory_store::list_task_memories(&a, root) {
            if entry.lesson_id.as_deref() == Some(lesson_id) {
                let flagged = entry.flag == "needs_review";
                return Some((entry.score, entry.flag, Some(a), flagged));
            }
        }
    }
    None
}

/// Down-weight (or up-weight) memory lessons by the outcome of the issues that
/// reused them: success adds 1 (capped at [`SCORE_CAP`]), failure subtracts 1,
/// and a score at or below [`NEEDS_REVIEW_THRESHOLD`] sets the sticky
/// `needs_review` flag. Each `(issue_run_id, lesson_id)` pair counts once ever
/// (persisted in `applied_path`). `loop.failed` events keyed by `run_id` count
/// as failures. Returns the lesson ids newly flagged by this call.
pub fn apply_outcomes(
    events: &[Value],
    root: Option<&Path>,
    applied_path: Option<&Path>,
) -> io::Result<Vec<String>> {
    let applied_path = applied_path
        .map(Path::to_path_buf)
        .unwrap_or_else(default_applied_path);
    let mut outcomes = issue_outcomes(events);
    for event in events {
        if event_type(event) == Some("loop.failed")
            && !event.get("issue_run_id").is_some_and(is_truthy)
        {
            if let Some(run_id) = event.get("run_id").filter(|r| is_truthy(r)) {
                outcomes.insert(run_id.to_string(), false);
            }
        }
    }
    let mut applied = load_applied(&applied_path);
    let mut scores: Ordered<(i64, String, Option<String>, bool)> = Ordered::new();
    let mut changed = false;
    for event in events {
        if event_type(event) != Some("memory.reused") {
            continue;
        }
        let Some(lesson_id) = lesson_id_of(event).filter(|v| is_truthy(v)) else {
            continue;
        };
        let lesson_key = pydumps::py_str(lesson_id);
        let issue_run_id = event.get("issue_run_id").unwrap_or(&Value::Null);
        let key = format!("{}|{}", pydumps::py_str(issue_run_id), lesson_key);
        let Some(&success) = outcomes.get(&issue_run_id.to_string()) else {
            continue;
        };
        if applied.contains(&key) {
            continue;
        }
        if !scores.contains_key(&lesson_key) {
            let alias = event.get("project").and_then(Value::as_str);
            let Some(current) = find_lesson(&lesson_key, alias, root) else {
                continue; // lesson gone; leave unapplied
            };
            scores.insert(lesson_key.clone(), current);
        }
        let entry = scores.get_mut(&lesson_key).expect("present");
        entry.0 = if success {
            SCORE_CAP.min(entry.0 + 1)
        } else {
            entry.0 - 1
        };
        if entry.0 <= NEEDS_REVIEW_THRESHOLD {
            entry.1 = "needs_review".to_string();
        }
        applied.insert(key);
        changed = true;
    }
    let mut newly_flagged = Vec::new();
    for (lesson_id, (score, flag, alias, was_flagged)) in scores.iter() {
        memory_store::set_score(lesson_id, *score, flag, alias.as_deref(), root)?;
        if flag == "needs_review" && !was_flagged {
            newly_flagged.push(lesson_id.clone());
        }
    }
    if changed {
        if let Some(parent) = applied_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut sorted: Vec<&String> = applied.iter().collect();
        sorted.sort();
        std::fs::write(&applied_path, pydumps::dumps(&sorted))?;
    }
    Ok(newly_flagged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn created(id: &str, cat: &str, project: &str) -> Value {
        json!({"event_type": "memory.created", "project": project,
               "timestamp": "2026-09-04T09:00:00.000Z",
               "data": {"lesson_id": id, "category": cat}})
    }
    fn reused(id: &str, run: &str, project: &str) -> Value {
        json!({"event_type": "memory.reused", "issue_run_id": run, "project": project,
               "timestamp": "2026-09-04T10:00:00.000Z", "data": {"lesson_id": id}})
    }
    fn ev(t: &str, run: &str, project: &str) -> Value {
        json!({"event_type": t, "issue_run_id": run, "project": project,
               "timestamp": "2026-09-04T10:00:00.000Z"})
    }

    #[test]
    fn effectiveness_counts_success_failure_and_pending() {
        let events = [
            created("l1", "testing", "k"),
            reused("l1", "a", "k"),
            ev("issue.completed", "a", "k"),
            reused("l1", "b", "k"),
            ev("issue.escalated", "b", "k"),
            reused("l1", "c", "k"), // pending
        ];
        let r = compute_lesson_effectiveness(&events, None);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].category, json!("testing"));
        assert_eq!(
            (
                r[0].times_reused,
                r[0].successful_reuses,
                r[0].failed_reuses
            ),
            (3, 1, 1)
        );
        assert_eq!(r[0].effectiveness_rate, Some(0.5));
        assert!(compute_lesson_effectiveness(&[], None).is_empty());
    }

    #[test]
    fn effectiveness_unknown_category_and_project_filter() {
        let events = [
            reused("l1", "a", "k"),
            ev("issue.completed", "a", "k"),
            reused("l2", "b", "other"),
            ev("issue.completed", "b", "other"),
        ];
        let all = compute_lesson_effectiveness(&events, None);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].category, Value::Null);
        let only = compute_lesson_effectiveness(&events, Some("k"));
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].lesson_id, json!("l1"));
    }

    #[test]
    fn pending_only_has_no_rate() {
        let r = compute_lesson_effectiveness(&[reused("l1", "a", "k")], None);
        assert_eq!(r[0].effectiveness_rate, None);
    }

    #[test]
    fn reuse_metrics_arithmetic() {
        let events = [
            created("l1", "c", "k"),
            ev("issue.started", "a", "k"),
            ev("issue.started", "b", "k"),
            ev("issue.started", "c", "k"),
            ev("issue.started", "d", "k"),
            reused("l1", "a", "k"),
            ev("issue.completed", "a", "k"),
            reused("l1", "b", "k"),
            ev("issue.escalated", "b", "k"),
        ];
        let m = compute_reuse_metrics(&events, None);
        assert_eq!((m.lessons_created, m.total_reuses), (1, 2));
        assert_eq!(m.memory_reuse_rate, Some(0.5));
        assert_eq!(m.memory_success_rate, Some(0.5));
        let z = compute_reuse_metrics(&[], None);
        assert_eq!((z.memory_reuse_rate, z.memory_success_rate), (None, None));
    }

    #[test]
    fn reuse_metrics_skip_untimestamped_starts_and_filter_projects() {
        let mut no_ts = ev("issue.started", "a", "k");
        no_ts.as_object_mut().unwrap().remove("timestamp");
        assert_eq!(
            compute_reuse_metrics(&[no_ts], None).memory_reuse_rate,
            None
        );
        let events = [
            ev("issue.started", "a", "k"),
            ev("issue.started", "b", "o"),
            reused("l", "a", "k"),
        ];
        assert_eq!(
            compute_reuse_metrics(&events, Some("k")).memory_reuse_rate,
            Some(1.0)
        );
    }

    #[test]
    fn memory_outcomes_pairs_reuse_with_outcome() {
        let events = [
            reused("l1", "a", "p"),
            reused("l2", "a", "p"),
            ev("issue.completed", "a", "p"),
            reused("l1", "b", "p"),
            ev("issue.escalated", "b", "p"),
            ev("issue.started", "c", "p"),
            ev("issue.completed", "c", "p"),
            ev("issue.started", "d", "p"),
            ev("issue.escalated", "d", "p"),
            reused("l1", "e", "p"),
            ev("issue.started", "f", "p"),
        ];
        let mut got = memory_outcomes(&events);
        got.sort();
        assert_eq!(
            got,
            [
                ("fresh", false),
                ("fresh", true),
                ("reused", false),
                ("reused", true)
            ]
        );
    }

    #[test]
    fn report_from_events_dir_and_text() {
        let d = tempfile::tempdir().unwrap();
        let lines: Vec<String> = [
            created("lesson_1", "testing", "k"),
            ev("issue.started", "a", "k"),
            reused("lesson_1", "a", "k"),
            ev("issue.completed", "a", "k"),
        ]
        .iter()
        .map(Value::to_string)
        .collect();
        std::fs::write(d.path().join("2026-09-04.jsonl"), lines.join("\n") + "\n").unwrap();
        let report = build_learning_report(Some(d.path()), None, None, None, None);
        let text = format_learning_report(&report);
        assert!(text.starts_with("Loop Learning (all time)\n\nReuse\n  Lessons created       1\n"));
        assert!(text.contains("  Memory reuse rate     100.0%"));
        assert!(text.contains("  Failures prevented    N/A (no counterfactual data"));
        assert!(text.ends_with("  lesson_1  (testing)  reused=1 effectiveness=100.0%"));
        let scoped = build_learning_report(
            Some(d.path()),
            Some("2026-09-01"),
            Some("2026-09-07"),
            Some("k"),
            None,
        );
        assert!(format_learning_report(&scoped)
            .starts_with("Loop Learning (2026-09-01 to 2026-09-07) \u{2014} project: k"));
        let empty = build_learning_report(None, None, None, None, Some(&[]));
        assert!(
            format_learning_report(&empty).ends_with("  N/A (no lessons reused in this window)")
        );
    }

    fn lesson(root: &Path, alias: &str, iid: i64) -> String {
        memory_store::add_task_memory(alias, iid, "lesson", &[], None, Some(root))
            .unwrap()
            .lesson_id
            .unwrap()
    }

    fn apply(root: &Path, events: &[Value]) -> Vec<String> {
        apply_outcomes(events, Some(root), Some(&root.join("applied.json"))).unwrap()
    }

    #[test]
    fn apply_flags_after_two_failures() {
        let d = tempfile::tempdir().unwrap();
        let lid = lesson(d.path(), "web", 7);
        let events = [
            reused(&lid, "r1", "web"),
            ev("issue.escalated", "r1", "web"),
            reused(&lid, "r2", "web"),
            ev("issue.escalated", "r2", "web"),
        ];
        assert_eq!(apply(d.path(), &events), vec![lid.clone()]);
        let m = memory_store::get_task_memory("web", 7, Some(d.path())).unwrap();
        assert_eq!((m.score, m.flag.as_str()), (-2, "needs_review"));
        let applied = std::fs::read_to_string(d.path().join("applied.json")).unwrap();
        assert!(applied.starts_with("[\"r1|lesson_") && applied.contains("\", \"r2|"));
    }

    #[test]
    fn apply_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let lid = lesson(d.path(), "web", 7);
        let events = [
            reused(&lid, "r1", "web"),
            ev("issue.escalated", "r1", "web"),
        ];
        apply(d.path(), &events);
        apply(d.path(), &events);
        assert_eq!(
            memory_store::get_task_memory("web", 7, Some(d.path()))
                .unwrap()
                .score,
            -1
        );
    }

    #[test]
    fn apply_caps_success_and_flag_is_sticky() {
        let d = tempfile::tempdir().unwrap();
        let lid = lesson(d.path(), "web", 7);
        let mut events = Vec::new();
        for i in 0..5 {
            events.push(reused(&lid, &format!("r{i}"), "web"));
            events.push(ev("issue.completed", &format!("r{i}"), "web"));
        }
        assert!(apply(d.path(), &events).is_empty());
        let get = || memory_store::get_task_memory("web", 7, Some(d.path())).unwrap();
        assert_eq!(get().score, 3);
        memory_store::set_score(&lid, -2, "needs_review", Some("web"), Some(d.path())).unwrap();
        let more = [
            reused(&lid, "r9", "web"),
            ev("issue.completed", "r9", "web"),
        ];
        assert!(apply(d.path(), &more).is_empty());
        assert_eq!((get().score, get().flag), (-1, "needs_review".to_string()));
    }

    #[test]
    fn apply_ignores_pending_and_missing_lessons() {
        let d = tempfile::tempdir().unwrap();
        let lid = lesson(d.path(), "web", 7);
        let events = [
            reused(&lid, "r1", "web"),
            reused("lesson_gone", "r2", "web"),
            ev("issue.escalated", "r2", "web"),
        ];
        assert!(apply(d.path(), &events).is_empty());
        assert_eq!(
            memory_store::get_task_memory("web", 7, Some(d.path()))
                .unwrap()
                .score,
            0
        );
        assert!(!d.path().join("applied.json").exists());
    }

    #[test]
    fn apply_treats_loop_failed_by_run_id_as_failure_and_searches_all_aliases() {
        let d = tempfile::tempdir().unwrap();
        let lid = lesson(d.path(), "web", 7);
        let events = [
            json!({"event_type": "memory.reused", "issue_run_id": "r1", "data": {"lesson_id": lid}}),
            json!({"event_type": "loop.failed", "run_id": "r1", "project": "web"}),
        ];
        apply(d.path(), &events);
        assert_eq!(
            memory_store::get_task_memory("web", 7, Some(d.path()))
                .unwrap()
                .score,
            -1
        );
    }
}
