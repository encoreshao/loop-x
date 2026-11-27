//! What LoopX already told you, shared by every notifier. An alert carries an
//! `about` key naming what it is about (`issue:web#7:escalated:needs_clarification`);
//! the outbox records each one it sends here and skips the same `about` from any
//! loop for [`REPEAT_HOURS`], and the daily digest gets the list so it does not
//! report those alerts again. Kept in `<home>/told.json`.

use crate::file_lock::FileLock;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The same alert is not sent again for this long.
pub const REPEAT_HOURS: i64 = 24;
/// Records are kept this long.
pub const KEEP_DAYS: i64 = 14;
/// Characters of an alert's first line kept for the digest.
const MAX_TEXT: usize = 160;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Told {
    pub about: String,
    pub text: String,
    pub at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
}

pub fn default_path() -> PathBuf {
    crate::config::home_dir().join("told.json")
}

fn read(path: &Path) -> Vec<Told> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// When `about` was last told, if within [`REPEAT_HOURS`] of `now`.
pub fn recently(path: &Path, about: &str, now: DateTime<Utc>) -> Option<Told> {
    read(path)
        .into_iter()
        .filter(|t| t.about == about && now - t.at < Duration::hours(REPEAT_HOURS))
        .max_by_key(|t| t.at)
}

/// Record that `about` was told with `text` (its first line is kept).
pub fn record(path: &Path, about: &str, text: &str, via: Option<&str>, now: DateTime<Utc>) {
    let Ok(_lock) = FileLock::exclusive(&path.with_extension("lock")) else {
        return;
    };
    let mut all = read(path);
    all.retain(|t| now - t.at < Duration::days(KEEP_DAYS) && t.about != about);
    all.push(Told {
        about: about.to_string(),
        text: text
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(MAX_TEXT)
            .collect(),
        at: now,
        via: via.map(str::to_string),
    });
    if let Ok(bytes) = serde_json::to_vec_pretty(&all) {
        let _ = crate::settings_store::atomic_write(path, &bytes, Some(0o600));
    }
}

/// Everything told since `since`, oldest first.
pub fn since(path: &Path, since: DateTime<Utc>) -> Vec<Told> {
    let mut all: Vec<Told> = read(path).into_iter().filter(|t| t.at >= since).collect();
    all.sort_by_key(|t| t.at);
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-27T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn an_alert_counts_as_told_for_a_day_and_is_kept_two_weeks() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("told.json");
        assert!(recently(&p, "issue:web#7:escalated", now()).is_none());
        record(
            &p,
            "issue:web#7:escalated",
            "*Escalated* web #7\nmore",
            Some("issue-loop"),
            now(),
        );
        let t = recently(&p, "issue:web#7:escalated", now() + Duration::hours(23)).unwrap();
        assert_eq!(t.text, "*Escalated* web #7");
        assert!(recently(&p, "issue:web#7:escalated", now() + Duration::hours(25)).is_none());
        assert!(recently(&p, "issue:web#8:escalated", now()).is_none());
        record(&p, "other", "x", None, now() + Duration::days(15));
        assert_eq!(
            since(&p, now() - Duration::days(1)).len(),
            1,
            "the old one was pruned"
        );
    }
}
