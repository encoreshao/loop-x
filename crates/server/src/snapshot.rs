//! The state document the Mac app and the web UI watch: every loop's run status,
//! the topic monitor's per-topic status, pending approvals and recent outcomes,
//! plus one derived `app_state` for a glanceable indicator.

use chrono::{DateTime, Duration, Utc};
use loopx_core::{approvals, events, status};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// How long an outcome (MR opened, escalation, failure) stays "fresh" in
/// `attention`, i.e. worth surfacing in the notch.
pub const ATTENTION_WINDOW_MINUTES: i64 = 15;
const RECENT_EVENTS: usize = 15;
const INTERESTING: [&str; 9] = [
    "run.started",
    "run.completed",
    "run.failed",
    "issue.queued",
    "issue.escalated",
    "issue.completed",
    "approval.requested",
    "approval.decided",
    "loop.failed",
];

/// Where a root keeps its per-checkout run state.
#[derive(Debug, Clone)]
pub struct Paths {
    pub outputs: PathBuf,
    pub events_dir: PathBuf,
    pub approvals_dir: PathBuf,
}

impl Paths {
    pub fn for_root(root: &Path) -> Self {
        let outputs = root.join("outputs");
        Self {
            events_dir: events::default_events_dir(),
            approvals_dir: outputs.join("approvals"),
            outputs,
        }
    }
}

#[cfg(unix)]
fn pid_alive(pid: u64) -> bool {
    u32::try_from(pid).is_ok_and(crate::lock::pid_alive)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u64) -> bool {
    true
}

/// A `running` status whose pid is gone is a run that died without cleaning up.
fn effective_state(doc: &Value) -> String {
    let state = doc["state"].as_str().unwrap_or("unknown").to_string();
    if state == "running" {
        if let Some(pid) = doc.get("pid").and_then(Value::as_u64) {
            if !pid_alive(pid) {
                return "stale".to_string();
            }
        }
    }
    state
}

fn loop_statuses(outputs: &Path) -> Map<String, Value> {
    let mut loops = Map::new();
    let legacy = outputs.join("status.json");
    if legacy.exists() {
        loops.insert("gitlab-loop".into(), status::read_status(&legacy));
    }
    if let Ok(entries) = std::fs::read_dir(outputs.join("status")) {
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        files.sort();
        for p in files {
            if let Some(name) = p.file_stem().map(|n| n.to_string_lossy().into_owned()) {
                loops.insert(name, status::read_status(&p));
            }
        }
    }
    for doc in loops.values_mut() {
        let eff = effective_state(doc);
        if eff != doc["state"].as_str().unwrap_or("") {
            doc["state"] = json!(eff);
        }
    }
    loops
}

fn compact_event(e: &Value) -> Value {
    json!({
        "type": e["event_type"],
        "at": e["timestamp"],
        "run_id": e["run_id"],
        "project": e["project"],
        "issue_iid": e["issue_iid"],
        "data": e["data"],
    })
}

fn parse_time(e: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(e["timestamp"].as_str()?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// The newest fresh outcome worth showing: an MR opened, an escalation, a failure.
fn attention(recent: &[Value], now: DateTime<Utc>) -> Value {
    let cutoff = now - Duration::minutes(ATTENTION_WINDOW_MINUTES);
    for e in recent.iter().rev() {
        if parse_time(e).is_none_or(|t| t < cutoff) {
            continue;
        }
        let data = &e["data"];
        let kind = match e["event_type"].as_str() {
            Some("issue.escalated") => "escalated",
            Some("issue.completed")
                if data["action"] == "fix" || data["outcome"] == "mr_opened" =>
            {
                "mr_opened"
            }
            Some("run.failed") | Some("loop.failed") => "failed",
            _ => continue,
        };
        return json!({
            "kind": kind,
            "at": e["timestamp"],
            "project": e["project"],
            "issue_iid": e["issue_iid"],
            "reason": data["reason"],
            "mr_url": data["mr_url"],
        });
    }
    Value::Null
}

/// One glanceable state: `awaiting_approval` > `running` > `failed` > `idle`.
fn app_state(loops: &Map<String, Value>, pending: usize) -> &'static str {
    if pending > 0 {
        return "awaiting_approval";
    }
    let any = |s: &str| loops.values().any(|d| d["state"] == s);
    if any("running") {
        "running"
    } else if any("failed") {
        "failed"
    } else {
        "idle"
    }
}

pub fn build_snapshot(paths: &Paths, now: DateTime<Utc>) -> Value {
    let loops = loop_statuses(&paths.outputs);
    let topics =
        status::read_topic_status(&paths.outputs.join("topic-monitor").join("status.json"));
    approvals::expire_overdue(&paths.approvals_dir, now);
    let pending = approvals::list(&paths.approvals_dir, Some("pending"), now);

    let since = (now - Duration::days(1)).format("%Y-%m-%d").to_string();
    let all = events::iter_events(Some(&paths.events_dir), Some(&since), None);
    let recent: Vec<Value> = all
        .iter()
        .filter(|e| {
            e["event_type"]
                .as_str()
                .is_some_and(|t| INTERESTING.contains(&t))
        })
        .cloned()
        .collect();
    let tail: Vec<Value> = recent
        .iter()
        .rev()
        .take(RECENT_EVENTS)
        .rev()
        .map(compact_event)
        .collect();
    let state = app_state(&loops, pending.len());

    json!({
        "version": 1,
        "app_state": state,
        "loops": loops,
        "topics": topics["topics"],
        "approvals": {"pending": pending},
        "attention": attention(&recent, now),
        "recent_events": tail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 22, 12, 0, 0).unwrap()
    }

    fn paths(root: &Path) -> Paths {
        Paths {
            outputs: root.join("outputs"),
            events_dir: root.join("outputs/events"),
            approvals_dir: root.join("outputs/approvals"),
        }
    }

    fn event(root: &Path, kind: &str, at: &str, data: Value) {
        let dir = root.join("outputs/events");
        std::fs::create_dir_all(&dir).unwrap();
        let day = &at[..10];
        let line = json!({
            "schema_version": 1, "event_id": "evt_x", "timestamp": at, "event_type": kind,
            "run_id": "run_1", "issue_run_id": null, "project": "web", "issue_iid": 7, "data": data
        });
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{day}.jsonl")))
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    #[test]
    fn an_empty_root_is_idle() {
        let d = tempfile::tempdir().unwrap();
        let s = build_snapshot(&paths(d.path()), now());
        assert_eq!(s["app_state"], "idle");
        assert_eq!(s["loops"], json!({}));
        assert_eq!(s["attention"], Value::Null);
        assert_eq!(s["recent_events"], json!([]));
    }

    #[test]
    fn statuses_from_every_loop_file_are_collected() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        status::write_status(&p.outputs.join("status.json"), "idle", &[]).unwrap();
        status::write_status(&p.outputs.join("status/topic-loop.json"), "failed", &[]).unwrap();
        status::write_topic_status(
            &p.outputs.join("topic-monitor/status.json"),
            "ai",
            "running",
            &[],
        )
        .unwrap();
        let s = build_snapshot(&p, now());
        assert_eq!(s["loops"]["gitlab-loop"]["state"], "idle");
        assert_eq!(s["loops"]["topic-loop"]["state"], "failed");
        assert_eq!(s["topics"]["ai"]["state"], "running");
        assert_eq!(s["app_state"], "failed");
    }

    #[test]
    fn running_with_a_live_pid_is_running_and_with_a_dead_pid_is_stale() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        let me = std::process::id();
        status::write_status(
            &p.outputs.join("status.json"),
            "running",
            &[("pid", json!(me))],
        )
        .unwrap();
        assert_eq!(build_snapshot(&p, now())["app_state"], "running");
        status::write_status(
            &p.outputs.join("status.json"),
            "running",
            &[("pid", json!(0x3fff_fffeu32))],
        )
        .unwrap();
        let s = build_snapshot(&p, now());
        assert_eq!(s["loops"]["gitlab-loop"]["state"], "stale");
        assert_eq!(s["app_state"], "idle");
    }

    #[test]
    fn a_pending_approval_outranks_everything() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        status::write_status(&p.outputs.join("status.json"), "failed", &[]).unwrap();
        approvals::create(
            &p.approvals_dir,
            &approvals::NewApproval {
                kind: "open_mr".into(),
                project: "web".into(),
                title: "t".into(),
                timeout_seconds: 3600,
                ..Default::default()
            },
            now(),
        )
        .unwrap();
        let s = build_snapshot(&p, now());
        assert_eq!(s["app_state"], "awaiting_approval");
        assert_eq!(s["approvals"]["pending"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn expired_approvals_do_not_count_as_pending() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        approvals::create(
            &p.approvals_dir,
            &approvals::NewApproval {
                timeout_seconds: 60,
                ..Default::default()
            },
            now() - Duration::hours(1),
        )
        .unwrap();
        let s = build_snapshot(&p, now());
        assert_eq!(s["approvals"]["pending"], json!([]));
        assert_eq!(s["app_state"], "idle");
    }

    #[test]
    fn fresh_outcomes_surface_as_attention_and_old_ones_do_not() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        event(
            d.path(),
            "issue.completed",
            "2026-10-22T11:50:00.000Z",
            json!({"action": "fix", "outcome": "mr_opened", "mr_url": "http://mr/1"}),
        );
        let s = build_snapshot(&p, now());
        assert_eq!(s["attention"]["kind"], "mr_opened");
        assert_eq!(s["attention"]["mr_url"], "http://mr/1");
        assert_eq!(s["attention"]["project"], "web");

        event(
            d.path(),
            "issue.escalated",
            "2026-10-22T11:55:00.000Z",
            json!({"reason": "verification_failed"}),
        );
        let s = build_snapshot(&p, now());
        assert_eq!(
            s["attention"]["kind"], "escalated",
            "the newest outcome wins"
        );
        assert_eq!(s["attention"]["reason"], "verification_failed");

        let later = now() + Duration::minutes(30);
        assert_eq!(build_snapshot(&p, later)["attention"], Value::Null);
    }

    #[test]
    fn recent_events_keep_only_interesting_types_and_are_bounded() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        event(
            d.path(),
            "iteration.started",
            "2026-10-22T11:00:00.000Z",
            json!({}),
        );
        for i in 0..20 {
            event(
                d.path(),
                "issue.queued",
                &format!("2026-10-22T11:{i:02}:00.000Z"),
                json!({"n": i}),
            );
        }
        let s = build_snapshot(&p, now());
        let recent = s["recent_events"].as_array().unwrap();
        assert_eq!(recent.len(), RECENT_EVENTS);
        assert!(recent.iter().all(|e| e["type"] == "issue.queued"));
        assert_eq!(recent.last().unwrap()["data"]["n"], 19, "newest last");
    }
}
