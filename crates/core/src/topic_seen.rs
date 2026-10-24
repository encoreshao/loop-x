//! Rolling window of already-reported items per topic, so the daily briefing
//! doesn't repeat itself. Mirrors `bin/topic_seen.py`: state lives in
//! `<state_dir>/<topic>.json` as `{url, title, seen_at}` entries pruned to the
//! last [`SEEN_WINDOW_DAYS`] days.

use crate::config::home_dir;
use crate::pyjson;
use chrono::{DateTime, Duration, FixedOffset, SecondsFormat, Timelike, Utc};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const SEEN_WINDOW_DAYS: i64 = 7;

/// `LOOP_TOPIC_STATE_DIR`, else `<LOOP_ENGINEERING_HOME>/outputs/topic-monitor/state`
/// (the checkout root on an installed machine).
pub fn default_state_dir() -> PathBuf {
    match std::env::var_os("LOOP_TOPIC_STATE_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home_dir()
            .join("outputs")
            .join("topic-monitor")
            .join("state"),
    }
}

fn state_path(state_dir: &Path, topic: &str) -> PathBuf {
    // Only the final path component, so a topic name can't escape the state dir.
    let safe = Path::new(topic)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    state_dir.join(format!("{safe}.json"))
}

fn load(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Array(items) => Some(items),
            _ => None,
        })
        .unwrap_or_default()
}

fn save(path: &Path, entries: &[Value]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, pyjson::to_string_pretty(&entries))
}

/// Python's `datetime.isoformat()` for an aware UTC time: fractional seconds
/// are only present when non-zero.
fn isoformat(now: DateTime<Utc>) -> String {
    if now.nanosecond() / 1000 == 0 {
        now.to_rfc3339_opts(SecondsFormat::Secs, false)
    } else {
        now.to_rfc3339_opts(SecondsFormat::Micros, false)
    }
}

fn parse_seen_at(entry: &Value) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(entry.get("seen_at")?.as_str()?).ok()
}

fn prune(entries: Vec<Value>, now: DateTime<Utc>) -> Vec<Value> {
    let cutoff = now - Duration::days(SEEN_WINDOW_DAYS);
    entries
        .into_iter()
        .filter(|e| parse_seen_at(e).is_some_and(|t| t >= cutoff))
        .collect()
}

/// Every `{url, title}` still in the window; prunes the file as a side effect.
pub fn get_seen(state_dir: &Path, topic: &str, now: DateTime<Utc>) -> Result<Vec<Value>, String> {
    let path = state_path(state_dir, topic);
    let entries = prune(load(&path), now);
    save(&path, &entries).map_err(|e| e.to_string())?;
    entries
        .iter()
        .map(|e| match (e.get("url"), e.get("title")) {
            (Some(url), Some(title)) => Ok(json!({ "url": url, "title": title })),
            _ => Err(format!(
                "seen entry missing url/title in {}",
                path.display()
            )),
        })
        .collect()
}

/// Record one item as reported at `now`.
pub fn add_seen(
    state_dir: &Path,
    topic: &str,
    url: &str,
    title: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let path = state_path(state_dir, topic);
    let mut entries = prune(load(&path), now);
    entries.push(json!({ "url": url, "title": title, "seen_at": isoformat(now) }));
    save(&path, &entries).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, day, 12, 0, 0).unwrap()
    }

    #[test]
    fn isoformat_omits_zero_fraction_like_python() {
        assert_eq!(isoformat(at(1)), "2026-10-01T12:00:00+00:00");
        let frac = at(1) + Duration::microseconds(5);
        assert_eq!(isoformat(frac), "2026-10-01T12:00:00.000005+00:00");
    }

    #[test]
    fn window_keeps_seven_days_and_drops_older() {
        let d = tempfile::tempdir().unwrap();
        add_seen(d.path(), "t", "old", "O", at(1)).unwrap();
        add_seen(d.path(), "t", "new", "N", at(9)).unwrap(); // prunes day 1 (8d old)
        let seen = get_seen(d.path(), "t", at(10)).unwrap();
        assert_eq!(seen, vec![json!({"url": "new", "title": "N"})]);
    }

    #[test]
    fn topic_name_cannot_escape_state_dir() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(state_path(d.path(), "../x"), d.path().join("x.json"));
    }
}
