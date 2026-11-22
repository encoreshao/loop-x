//! Event-triggered loops: a loop whose `loop.yaml` says `trigger.type: event`
//! is queued by the scheduler tick when a matching event lands in LoopX's own
//! event log. Matches go to a per-loop inbox file the run drains, because a
//! queue entry carries only the loop name.
//!
//! Files under `<home>/event_triggers/`: `<loop>.json` (the inbox,
//! `{"armed_at", "pending"}`) and `<loop>-events/seen.json` (de-dup by
//! `event_id`).

use crate::file_lock;
use crate::loop_definition::LoopDefinition;
use crate::seen_store::{parse_iso, SeenStore};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub const MAX_PENDING: usize = 20;
const SEEN_WINDOW_DAYS: i64 = 30;
/// How far back a tick reads the event log after a long pause (under the seen window).
const MAX_LOOKBACK_DAYS: i64 = 7;

#[derive(Debug, Clone, PartialEq)]
pub struct Subscription {
    pub loop_name: String,
    pub on: String,
    pub filter: Map<String, Value>,
}

/// `on` is an exact `event_type`; every `where` key must match `data[key]`
/// (scalar: JSON equality, array: one of); a missing key never matches.
pub fn matches(sub: &Subscription, event: &Value) -> bool {
    if event.get("event_type").and_then(Value::as_str) != Some(sub.on.as_str()) {
        return false;
    }
    let data = event.get("data");
    sub.filter.iter().all(|(key, want)| {
        let Some(have) = data.and_then(|d| d.get(key)) else {
            return false;
        };
        match want {
            Value::Array(options) => options.contains(have),
            scalar => scalar == have,
        }
    })
}

/// The loop's `loop.yaml` under `repo_root`, or the binary's bundled copy when the
/// root has not been seeded yet (the scheduler tick never seeds it).
fn definition_for(loop_name: &str, repo_root: &Path) -> Option<LoopDefinition> {
    let dir = crate::loop_plugins::definition_dir_for(loop_name)?;
    let rel = format!("loops/{dir}/loop.yaml");
    let path = repo_root.join(&rel);
    let parsed = match (path.exists(), crate::bundled::text(&rel)) {
        (false, Some(text)) => LoopDefinition::from_yaml_str(text, &rel),
        _ => LoopDefinition::from_yaml(&path),
    };
    match parsed {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("event_triggers: skipping {loop_name}: {e}");
            None
        }
    }
}

/// Enabled registry entries whose definition has an event trigger.
pub fn subscriptions(loops: &[Value], repo_root: &Path) -> Vec<Subscription> {
    loops
        .iter()
        .filter(|entry| entry.get("enabled").and_then(Value::as_bool) == Some(true))
        .filter_map(|entry| {
            let name = entry.get("name").and_then(Value::as_str)?;
            let definition = definition_for(name, repo_root)?;
            subscription_of(name, &definition)
        })
        .collect()
}

pub fn subscription_of(loop_name: &str, definition: &LoopDefinition) -> Option<Subscription> {
    if definition.trigger.kind != "event" {
        return None;
    }
    Some(Subscription {
        loop_name: loop_name.to_string(),
        on: definition.trigger.on.clone()?,
        filter: definition.trigger.filter.clone(),
    })
}

/// Forgets the inbox (arming and pending events) of every switched-off event loop,
/// so switching it back on arms it afresh instead of replaying what it missed.
pub fn disarm_disabled(loops: &[Value], repo_root: &Path, state_dir: &Path) {
    for entry in loops {
        if entry.get("enabled").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(name) = entry.get("name").and_then(Value::as_str) else {
            continue;
        };
        let path = inbox_path(state_dir, name);
        if !path.exists() || subscription_for(name, repo_root).is_none() {
            continue;
        }
        let removed = file_lock::with_lock(&path, || std::fs::remove_file(&path));
        if let Err(e) = removed.and_then(|r| r) {
            eprintln!("event_triggers: cannot disarm {name}: {e}");
        }
    }
}

/// The subscription of one loop by name, regardless of its `enabled` flag.
pub fn subscription_for(loop_name: &str, repo_root: &Path) -> Option<Subscription> {
    subscription_of(loop_name, &definition_for(loop_name, repo_root)?)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Inbox {
    pub armed_at: String,
    #[serde(default)]
    pub pending: Vec<Value>,
    /// The last tick that read the event log for this loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collected_at: Option<String>,
}

pub fn inbox_path(state_dir: &Path, loop_name: &str) -> PathBuf {
    state_dir.join(format!("{loop_name}.json"))
}

pub fn load_inbox(path: &Path) -> Option<Inbox> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_inbox(path: &Path, inbox: &Inbox) -> std::io::Result<()> {
    let text = crate::pyjson::to_string_pretty(&serde_json::to_value(inbox)?);
    file_lock::write_atomic(path, format!("{text}\n").as_bytes())
}

/// Empties the inbox and returns what was pending; `armed_at` is kept.
pub fn take_pending(state_dir: &Path, loop_name: &str) -> Vec<Value> {
    let path = inbox_path(state_dir, loop_name);
    let taken = file_lock::with_lock(&path, || {
        let mut inbox = load_inbox(&path)?;
        if inbox.pending.is_empty() {
            return Some(Vec::new());
        }
        let pending = std::mem::take(&mut inbox.pending);
        match save_inbox(&path, &inbox) {
            Ok(()) => Some(pending),
            Err(e) => {
                eprintln!("event_triggers: cannot empty {}: {e}", path.display());
                None
            }
        }
    });
    taken.ok().flatten().unwrap_or_default()
}

/// Set on an event the run put back to retry: not taken again before this time.
pub const RETRY_AFTER: &str = "_retry_after";

fn ready(event: &Value, now: DateTime<Utc>) -> bool {
    event
        .get(RETRY_AFTER)
        .and_then(Value::as_str)
        .and_then(parse_iso)
        .is_none_or(|t| t <= now)
}

pub fn ready_count(state_dir: &Path, loop_name: &str, now: DateTime<Utc>) -> usize {
    load_inbox(&inbox_path(state_dir, loop_name))
        .map_or(0, |i| i.pending.iter().filter(|e| ready(e, now)).count())
}

/// Removes and returns the oldest pending event that is due; the rest stay.
pub fn take_ready_one(state_dir: &Path, loop_name: &str, now: DateTime<Utc>) -> Option<Value> {
    let path = inbox_path(state_dir, loop_name);
    file_lock::with_lock(&path, || {
        let mut inbox = load_inbox(&path)?;
        let at = inbox.pending.iter().position(|e| ready(e, now))?;
        let event = inbox.pending.remove(at);
        match save_inbox(&path, &inbox) {
            Ok(()) => Some(event),
            Err(e) => {
                eprintln!("event_triggers: cannot update {}: {e}", path.display());
                None
            }
        }
    })
    .ok()
    .flatten()
}

/// Returns an event to the front of the inbox (creating it, armed now, if it is gone).
pub fn put_back(state_dir: &Path, loop_name: &str, event: Value) {
    let path = inbox_path(state_dir, loop_name);
    let saved = file_lock::with_lock(&path, || {
        let mut inbox = load_inbox(&path).unwrap_or_else(|| Inbox {
            armed_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            pending: Vec::new(),
            collected_at: None,
        });
        inbox.pending.insert(0, event);
        save_inbox(&path, &inbox)
    });
    if let Err(e) = saved.and_then(|r| r) {
        eprintln!("event_triggers: cannot put an event back for {loop_name}: {e}");
    }
}

pub fn pending_count(state_dir: &Path, loop_name: &str) -> usize {
    load_inbox(&inbox_path(state_dir, loop_name)).map_or(0, |i| i.pending.len())
}

fn timestamp_of(event: &Value) -> Option<DateTime<Utc>> {
    event
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_iso)
}

/// Adds the new matching events to the loop's inbox and returns how many were
/// added. The first call arms the subscription: events older than `armed_at`
/// never match, so enabling a loop does not replay history.
pub fn collect(
    sub: &Subscription,
    events_dir: &Path,
    state_dir: &Path,
    now: DateTime<Utc>,
) -> usize {
    let path = inbox_path(state_dir, &sub.loop_name);
    let added = file_lock::with_lock(&path, || {
        collect_locked(sub, events_dir, state_dir, &path, now)
    });
    match added {
        Ok(n) => n,
        Err(e) => {
            eprintln!("event_triggers: {}: {e}", sub.loop_name);
            0
        }
    }
}

fn collect_locked(
    sub: &Subscription,
    events_dir: &Path,
    state_dir: &Path,
    path: &Path,
    now: DateTime<Utc>,
) -> usize {
    let mut seen = match SeenStore::new(
        &format!("{}-events", sub.loop_name),
        Some(state_dir),
        SEEN_WINDOW_DAYS,
        Some(Box::new(move || now)),
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("event_triggers: {e}");
            return 0;
        }
    };
    let mut inbox = match load_inbox(path) {
        Some(inbox) => inbox,
        None => {
            let inbox = Inbox {
                armed_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
                pending: Vec::new(),
                collected_at: None,
            };
            if let Err(e) = save_inbox(path, &inbox) {
                eprintln!("event_triggers: cannot arm {}: {e}", sub.loop_name);
            }
            return 0;
        }
    };
    let Some(armed_at) = parse_iso(&inbox.armed_at) else {
        eprintln!("event_triggers: {}: unreadable armed_at", sub.loop_name);
        return 0;
    };
    let last = inbox.collected_at.as_deref().and_then(parse_iso);
    let from =
        (last.unwrap_or(armed_at) - Duration::days(1)).max(now - Duration::days(MAX_LOOKBACK_DAYS));
    let from_day = from.format("%Y-%m-%d").to_string();
    let today = now.format("%Y-%m-%d").to_string();
    let mut added = 0;
    for event in crate::events::iter_events(Some(events_dir), Some(&from_day), Some(&today)) {
        let Some(id) = event.get("event_id").and_then(Value::as_str) else {
            continue;
        };
        if seen.has(id) || timestamp_of(&event).is_none_or(|t| t < armed_at) {
            continue;
        }
        if !matches(sub, &event) {
            continue;
        }
        seen.add(id);
        inbox.pending.push(event);
        added += 1;
    }
    let new_day = last.is_none_or(|t| t.date_naive() != now.date_naive());
    if added == 0 && !new_day {
        return 0;
    }
    inbox.collected_at = Some(now.to_rfc3339_opts(SecondsFormat::Millis, true));
    if inbox.pending.len() > MAX_PENDING {
        let dropped = inbox.pending.len() - MAX_PENDING;
        inbox.pending.drain(..dropped);
        eprintln!(
            "event_triggers: {}: inbox full, dropped the {dropped} oldest event(s)",
            sub.loop_name
        );
    }
    if let Err(e) = save_inbox(path, &inbox) {
        eprintln!("event_triggers: cannot save {}: {e}", path.display());
        return 0;
    }
    if let Err(e) = seen.save() {
        eprintln!(
            "event_triggers: cannot save seen for {}: {e}",
            sub.loop_name
        );
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sub() -> Subscription {
        let filter = json!({"mr_owned_by_loop": true, "category": ["test_failure", "lint"]});
        Subscription {
            loop_name: "mr-autofix-loop".into(),
            on: "pipeline_doctor.diagnosed".into(),
            filter: filter.as_object().unwrap().clone(),
        }
    }

    fn event(id: &str, at: DateTime<Utc>, data: Value) -> Value {
        json!({
            "event_id": id,
            "timestamp": crate::events::fmt_timestamp(at),
            "event_type": "pipeline_doctor.diagnosed",
            "run_id": "r",
            "data": data,
        })
    }

    fn good() -> Value {
        json!({"mr_owned_by_loop": true, "category": "lint"})
    }

    fn write(dir: &Path, events: &[Value]) {
        std::fs::create_dir_all(dir).unwrap();
        for e in events {
            let ts = e["timestamp"].as_str().unwrap();
            let path = dir.join(format!("{}.jsonl", &ts[..10]));
            crate::events::append_line(&path, &format!("{e}\n")).unwrap();
        }
    }

    #[test]
    fn where_scalar_array_bool_and_missing_key() {
        let s = sub();
        let now = Utc::now();
        assert!(matches(&s, &event("a", now, good())));
        assert!(matches(
            &s,
            &event(
                "a",
                now,
                json!({"mr_owned_by_loop": true, "category": "test_failure", "x": 1})
            )
        ));
        assert!(!matches(
            &s,
            &event(
                "a",
                now,
                json!({"mr_owned_by_loop": true, "category": "infra"})
            )
        ));
        assert!(!matches(
            &s,
            &event(
                "a",
                now,
                json!({"mr_owned_by_loop": "true", "category": "lint"})
            )
        ));
        assert!(!matches(&s, &event("a", now, json!({"category": "lint"}))));
        assert!(!matches(
            &s,
            &event(
                "a",
                now,
                json!({"mr_owned_by_loop": null, "category": "lint"})
            )
        ));
        let mut other = event("a", now, good());
        other["event_type"] = json!("pipeline_doctor.diagnosed.x");
        assert!(!matches(&s, &other));
        let open = Subscription {
            filter: Map::new(),
            ..sub()
        };
        assert!(matches(&open, &event("a", now, json!({}))));
    }

    #[test]
    fn arming_ignores_older_events_and_each_event_matches_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (events, state) = (tmp.path().join("events"), tmp.path().join("state"));
        let t0 = Utc::now() - Duration::minutes(10);
        write(&events, &[event("old", t0 - Duration::minutes(1), good())]);
        assert_eq!(collect(&sub(), &events, &state, t0), 0);
        let armed = load_inbox(&inbox_path(&state, "mr-autofix-loop")).unwrap();
        assert!(armed.pending.is_empty());

        write(&events, &[event("new", t0 + Duration::minutes(1), good())]);
        let t1 = t0 + Duration::minutes(2);
        assert_eq!(collect(&sub(), &events, &state, t1), 1);
        assert_eq!(collect(&sub(), &events, &state, t1), 0);
        let inbox = load_inbox(&inbox_path(&state, "mr-autofix-loop")).unwrap();
        let ids: Vec<_> = inbox
            .pending
            .iter()
            .map(|e| e["event_id"].clone())
            .collect();
        assert_eq!(ids, vec![json!("new")]);
        assert_eq!(inbox.armed_at, armed.armed_at);

        assert_eq!(take_pending(&state, "mr-autofix-loop").len(), 1);
        assert_eq!(
            collect(&sub(), &events, &state, t1),
            0,
            "seen after draining"
        );
    }

    #[test]
    fn events_from_a_long_pause_still_queue_up_to_a_week_back() {
        let tmp = tempfile::tempdir().unwrap();
        let (events, state) = (tmp.path().join("events"), tmp.path().join("state"));
        let t0 = Utc::now() - Duration::days(20);
        collect(&sub(), &events, &state, t0);
        let tick = t0 + Duration::days(10);
        assert_eq!(collect(&sub(), &events, &state, tick), 0);
        write(
            &events,
            &[
                event("day_before_last_tick", tick - Duration::days(1), good()),
                event("three_days", tick + Duration::hours(1), good()),
            ],
        );
        let after_pause = tick + Duration::days(3);
        assert_eq!(collect(&sub(), &events, &state, after_pause), 2);
        let resumed = after_pause + Duration::days(9);
        write(
            &events,
            &[
                event("eight_days", resumed - Duration::days(8), good()),
                event("six_days", resumed - Duration::days(6), good()),
            ],
        );
        assert_eq!(
            collect(&sub(), &events, &state, resumed),
            1,
            "capped at 7 days"
        );
    }

    #[test]
    fn concurrent_collects_add_each_event_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (events, state) = (tmp.path().join("events"), tmp.path().join("state"));
        let t0 = Utc::now() - Duration::minutes(10);
        collect(&sub(), &events, &state, t0);
        write(&events, &[event("e1", t0 + Duration::minutes(1), good())]);
        let t1 = t0 + Duration::minutes(2);
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let (events, state) = (events.clone(), state.clone());
                std::thread::spawn(move || collect(&sub(), &events, &state, t1))
            })
            .collect();
        let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(total, 1);
        assert_eq!(pending_count(&state, "mr-autofix-loop"), 1);
    }

    #[test]
    fn pending_is_capped_oldest_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let (events, state) = (tmp.path().join("events"), tmp.path().join("state"));
        let t0 = Utc::now() - Duration::hours(1);
        collect(&sub(), &events, &state, t0);
        let batch: Vec<Value> = (0..25)
            .map(|i| event(&format!("e{i:02}"), t0 + Duration::seconds(i + 1), good()))
            .collect();
        write(&events, &batch);
        assert_eq!(
            collect(&sub(), &events, &state, t0 + Duration::minutes(1)),
            25
        );
        let inbox = load_inbox(&inbox_path(&state, "mr-autofix-loop")).unwrap();
        assert_eq!(inbox.pending.len(), MAX_PENDING);
        assert_eq!(inbox.pending[0]["event_id"], "e05");
        assert_eq!(inbox.pending[19]["event_id"], "e24");
    }

    #[test]
    fn take_pending_empties_the_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let (events, state) = (tmp.path().join("events"), tmp.path().join("state"));
        assert!(take_pending(&state, "mr-autofix-loop").is_empty());
        let t0 = Utc::now() - Duration::minutes(10);
        collect(&sub(), &events, &state, t0);
        write(&events, &[event("e1", t0 + Duration::minutes(1), good())]);
        collect(&sub(), &events, &state, t0 + Duration::minutes(2));
        let taken = take_pending(&state, "mr-autofix-loop");
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0]["event_id"], "e1");
        assert!(take_pending(&state, "mr-autofix-loop").is_empty());
        let inbox = load_inbox(&inbox_path(&state, "mr-autofix-loop")).unwrap();
        assert!(!inbox.armed_at.is_empty() && inbox.pending.is_empty());
    }

    #[test]
    fn one_ready_event_at_a_time_and_retries_wait_their_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path();
        let now = Utc::now();
        let later = crate::events::fmt_timestamp(now + Duration::minutes(5));
        let mut waiting = event("e0", now, good());
        waiting[RETRY_AFTER] = json!(later);
        let inbox = Inbox {
            armed_at: "2026-01-01T00:00:00.000Z".into(),
            pending: vec![waiting, event("e1", now, good()), event("e2", now, good())],
            collected_at: None,
        };
        save_inbox(&inbox_path(state, "l"), &inbox).unwrap();
        assert_eq!(ready_count(state, "l", now), 2);
        assert_eq!(take_ready_one(state, "l", now).unwrap()["event_id"], "e1");
        assert_eq!(pending_count(state, "l"), 2, "the rest stay pending");
        assert_eq!(take_ready_one(state, "l", now).unwrap()["event_id"], "e2");
        assert!(
            take_ready_one(state, "l", now).is_none(),
            "e0 is not due yet"
        );
        assert_eq!(ready_count(state, "l", now + Duration::minutes(6)), 1);

        put_back(state, "l", event("e3", now, good()));
        assert_eq!(take_ready_one(state, "l", now).unwrap()["event_id"], "e3");
        let empty = tempfile::tempdir().unwrap();
        put_back(empty.path(), "l", event("e4", now, good()));
        assert_eq!(
            pending_count(empty.path(), "l"),
            1,
            "put back into a missing inbox"
        );
    }

    fn write_def(root: &Path, dir: &str, trigger: &str) {
        let d = root.join("loops").join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("loop.yaml"),
            format!("name: x\nversion: 1\ntrigger:\n{trigger}\ngoal:\n  type: g\n"),
        )
        .unwrap();
    }

    #[test]
    fn disabled_or_schedule_loops_are_not_subscriptions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_def(
            root,
            "mr-autofix",
            "  type: event\n  on: pipeline_doctor.diagnosed\n  where:\n    category: [lint]",
        );
        write_def(
            root,
            "rss-watch",
            "  type: schedule\n  schedule: '0 * * * *'",
        );
        let loops = vec![
            json!({"name": "mr-autofix-loop", "enabled": true}),
            json!({"name": "rss-watch-loop", "enabled": true}),
            json!({"name": "issue-loop", "enabled": true}),
            json!({"name": "stale-sweeper-loop", "enabled": true}),
        ];
        let subs = subscriptions(&loops, root);
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].loop_name, "mr-autofix-loop");
        assert_eq!(subs[0].on, "pipeline_doctor.diagnosed");
        assert_eq!(subs[0].filter["category"], json!(["lint"]));

        let unseeded = tempfile::tempdir().unwrap();
        let subs = subscriptions(&loops[..1], unseeded.path());
        assert_eq!(
            subs.len(),
            1,
            "an install root not yet seeded falls back to the bundled definition"
        );
        assert_eq!(subs[0].filter["category"], json!(["test_failure", "lint"]));

        let disabled = vec![json!({"name": "mr-autofix-loop", "enabled": false})];
        assert!(subscriptions(&disabled, root).is_empty());
        assert!(subscription_for("mr-autofix-loop", root).is_some());
    }
}
