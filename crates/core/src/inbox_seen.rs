//! Per-inbox triage memory for the Inbox Triage loop: a high-water mark (the
//! newest labelled message's date) plus the ids already triaged in the last 14
//! days, so no message is labelled or drafted twice. Mirrors `bin/inbox_seen.py`;
//! stored under `<root>/outputs/inbox-triage/state/<inbox>.json` (per-checkout
//! run state, like the topic monitor's).

use crate::mail_time::{isoformat_utc, parse_iso};
use crate::pyjson;
use crate::runner_util::repo_root;
use chrono::{DateTime, Duration, FixedOffset, Utc};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const RETENTION_DAYS: i64 = 14;
pub const FIRST_RUN_HOURS: i64 = 48;

/// `<root>/outputs/inbox-triage` for `root`.
pub fn output_dir_for(root: &Path) -> PathBuf {
    root.join("outputs").join("inbox-triage")
}

pub fn default_state_dir() -> PathBuf {
    output_dir_for(&repo_root()).join("state")
}

#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub high_water: Option<String>,
    /// message id -> the ISO time it was recorded.
    pub seen: Map<String, Value>,
}

impl State {
    fn empty() -> Self {
        Self {
            high_water: None,
            seen: Map::new(),
        }
    }

    pub fn contains(&self, id: &str) -> bool {
        self.seen.contains_key(id)
    }

    fn to_value(&self) -> Value {
        json!({"high_water": self.high_water, "seen": self.seen})
    }
}

fn path(name: &str, state_dir: &Path) -> PathBuf {
    state_dir.join(format!("{name}.json"))
}

/// The state, or an empty first-run state when missing, corrupt or malformed.
pub fn load(name: &str, state_dir: &Path) -> State {
    let Some(Value::Object(data)) = std::fs::read_to_string(path(name, state_dir))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return State::empty();
    };
    match data.get("seen") {
        Some(Value::Object(seen)) => State {
            high_water: data
                .get("high_water")
                .and_then(Value::as_str)
                .map(str::to_owned),
            seen: seen.clone(),
        },
        _ => State::empty(),
    }
}

/// Where to start fetching: the high-water mark, else 48 hours back.
pub fn since(state: &State, now: DateTime<Utc>) -> DateTime<FixedOffset> {
    state
        .high_water
        .as_deref()
        .and_then(parse_iso)
        .unwrap_or_else(|| (now - Duration::hours(FIRST_RUN_HOURS)).fixed_offset())
}

/// Record `(id, date)` pairs as triaged at `now`; the high-water mark only moves
/// forward; entries older than the retention window are dropped.
pub fn record(
    name: &str,
    messages: &[(String, String)],
    now: DateTime<Utc>,
    state_dir: &Path,
) -> std::io::Result<State> {
    let mut state = load(name, state_dir);
    let now_iso = isoformat_utc(now);
    for (id, date) in messages {
        state
            .seen
            .insert(id.clone(), Value::String(now_iso.clone()));
        let newer = match (
            state.high_water.as_deref().and_then(parse_iso),
            parse_iso(date),
        ) {
            (None, _) => true,
            (Some(hw), Some(d)) => d > hw,
            (Some(_), None) => false,
        };
        if newer {
            state.high_water = Some(date.clone());
        }
    }
    let cutoff = now - Duration::days(RETENTION_DAYS);
    state
        .seen
        .retain(|_, v| v.as_str().and_then(parse_iso).is_some_and(|t| t >= cutoff));
    let file = path(name, state_dir);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_file_name(format!(".{name}.json.tmp"));
    std::fs::write(&tmp, pyjson::to_string_pretty(&state.to_value()))?;
    std::fs::rename(&tmp, &file)?;
    Ok(state)
}

/// Delete a deleted inbox's memory, so a re-added inbox of the same name starts
/// fresh. True if there was one.
pub fn forget(name: &str, state_dir: &Path) -> bool {
    std::fs::remove_file(path(name, state_dir)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::tempdir;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 27, 9, 0, 0).unwrap()
    }

    fn msg(id: &str, date: &str) -> (String, String) {
        (id.into(), date.into())
    }

    #[test]
    fn first_run_since_is_48_hours_back() {
        let d = tempdir().unwrap();
        let state = load("w", d.path());
        assert_eq!(state, State::empty());
        assert_eq!(
            since(&state, now()),
            (now() - Duration::hours(48)).fixed_offset()
        );
    }

    #[test]
    fn record_advances_high_water_to_the_newest_message() {
        let d = tempdir().unwrap();
        record(
            "w",
            &[
                msg("a", "2026-09-27T07:00:00+00:00"),
                msg("b", "2026-09-27T08:30:00+00:00"),
            ],
            now(),
            d.path(),
        )
        .unwrap();
        let state = load("w", d.path());
        assert_eq!(
            state.high_water.as_deref(),
            Some("2026-09-27T08:30:00+00:00")
        );
        assert_eq!(state.seen.keys().collect::<Vec<_>>(), vec!["a", "b"]);
        assert_eq!(
            since(&state, now()),
            Utc.with_ymd_and_hms(2026, 9, 27, 8, 30, 0)
                .unwrap()
                .fixed_offset()
        );
    }

    #[test]
    fn record_never_moves_high_water_backwards() {
        let d = tempdir().unwrap();
        record(
            "w",
            &[msg("b", "2026-09-27T08:30:00+00:00")],
            now(),
            d.path(),
        )
        .unwrap();
        record(
            "w",
            &[msg("a", "2026-09-27T07:00:00+00:00")],
            now(),
            d.path(),
        )
        .unwrap();
        assert_eq!(
            load("w", d.path()).high_water.as_deref(),
            Some("2026-09-27T08:30:00+00:00")
        );
    }

    #[test]
    fn recording_nothing_keeps_an_empty_state() {
        let d = tempdir().unwrap();
        record("w", &[], now(), d.path()).unwrap();
        assert_eq!(load("w", d.path()), State::empty());
    }

    #[test]
    fn record_prunes_seen_older_than_14_days() {
        let d = tempdir().unwrap();
        std::fs::write(
            d.path().join("w.json"),
            json!({"high_water": null, "seen": {
                "old": isoformat_utc(now() - Duration::days(15)),
                "new": isoformat_utc(now())}})
            .to_string(),
        )
        .unwrap();
        record("w", &[], now(), d.path()).unwrap();
        assert_eq!(
            load("w", d.path()).seen.keys().collect::<Vec<_>>(),
            vec!["new"]
        );
    }

    #[test]
    fn corrupt_or_malformed_state_is_a_first_run() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("w.json"), "{nope").unwrap();
        assert_eq!(load("w", d.path()), State::empty());
        std::fs::write(
            d.path().join("w.json"),
            r#"{"high_water": "x", "seen": []}"#,
        )
        .unwrap();
        assert_eq!(load("w", d.path()), State::empty());
    }

    #[test]
    fn file_format_is_python_pretty_json() {
        let d = tempdir().unwrap();
        record(
            "w",
            &[msg("a", "2026-09-27T07:00:00+00:00")],
            now(),
            d.path(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("w.json")).unwrap(),
            "{\n  \"high_water\": \"2026-09-27T07:00:00+00:00\",\n  \"seen\": {\n    \"a\": \"2026-09-27T09:00:00+00:00\"\n  }\n}"
        );
    }

    #[test]
    fn forget_deletes_only_that_inboxes_state() {
        let d = tempdir().unwrap();
        record(
            "w",
            &[msg("m1", "2026-09-27T00:00:00+00:00")],
            now(),
            d.path(),
        )
        .unwrap();
        record(
            "h",
            &[msg("m2", "2026-09-27T00:00:00+00:00")],
            now(),
            d.path(),
        )
        .unwrap();
        assert!(forget("w", d.path()));
        assert!(!d.path().join("w.json").exists() && d.path().join("h.json").exists());
        assert!(!forget("w", d.path()));
    }
}
