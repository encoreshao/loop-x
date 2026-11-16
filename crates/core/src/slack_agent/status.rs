//! The listener's `status.json`: connected, offline or needing attention,
//! since when, the last event and the last error. Read by `loopx slack
//! status` and the dashboard. Never holds a token or a message body.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Connected,
    Offline,
    NeedsAttention,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub state: State,
    /// RFC 3339: when `state` last changed.
    pub since: String,
    /// RFC 3339: when the last Slack event was judged.
    pub last_event: Option<String>,
    /// A short, token-free reason.
    pub last_error: Option<String>,
}

/// Best effort (atomic replace); a status that cannot be written is skipped.
pub fn write_status(path: &Path, s: &Status) {
    if let Ok(text) = serde_json::to_string_pretty(s) {
        let _ = crate::file_lock::write_atomic(path, text.as_bytes());
    }
}

/// `None` when the file is missing or not a status.
pub fn read_status(path: &Path) -> Option<Status> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// What the dashboard and `loopx slack status` show: the status file read
/// against whether the agent is installed and whether a listener really holds
/// the state dir's lock (the kernel drops it when the process dies, so a
/// crashed listener can never keep saying "connected").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effective {
    NotInstalled,
    /// Installed, but no listener is running and none reported a problem.
    Stopped,
    /// Running but not connected. `since` is `None` right after a launch.
    Offline {
        since: Option<String>,
    },
    Connected {
        since: String,
        last_event: Option<String>,
    },
    /// A token was rejected (or similar): the user must act. Shown even when
    /// the listener has already exited.
    NeedsAttention {
        error: Option<String>,
    },
    /// Installed and the file claims it was running, but nobody holds the lock.
    Crashed,
}

/// Pure: `status` is the file's content, `lock_held` whether a listener holds
/// `listener.lock`, `installed` whether the launchd agent is in place.
pub fn effective_state(status: Option<&Status>, lock_held: bool, installed: bool) -> Effective {
    if !installed && !lock_held {
        return Effective::NotInstalled;
    }
    let needs_attention = |s: &Status| Effective::NeedsAttention {
        error: s.last_error.clone(),
    };
    match (status, lock_held) {
        (None, true) => Effective::Offline { since: None },
        (None, false) => Effective::Stopped,
        (Some(s), true) => match s.state {
            State::Connected => Effective::Connected {
                since: s.since.clone(),
                last_event: s.last_event.clone(),
            },
            State::Offline => Effective::Offline {
                since: Some(s.since.clone()),
            },
            State::NeedsAttention => needs_attention(s),
        },
        (Some(s), false) => match s.state {
            State::NeedsAttention => needs_attention(s),
            State::Connected | State::Offline => Effective::Crashed,
        },
    }
}

/// Whether a listener currently holds `<state_dir>/listener.lock`. Creates
/// nothing: a state dir that was never used reads as "not held".
pub fn listener_lock_held(state_dir: &Path) -> bool {
    crate::file_lock::is_held(&state_dir.join("listener.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(state: State) -> Status {
        Status {
            state,
            since: "2026-11-15T10:00:00Z".into(),
            last_event: Some("2026-11-15T10:05:00Z".into()),
            last_error: Some("Slack rejected the token".into()),
        }
    }

    #[test]
    fn nothing_is_installed_without_the_agent_whatever_a_stale_file_says() {
        for s in [
            None,
            Some(st(State::Connected)),
            Some(st(State::NeedsAttention)),
        ] {
            assert_eq!(
                effective_state(s.as_ref(), false, false),
                Effective::NotInstalled
            );
        }
    }

    #[test]
    fn an_installed_agent_with_a_live_lock_shows_what_it_reports() {
        assert_eq!(
            effective_state(Some(&st(State::Connected)), true, true),
            Effective::Connected {
                since: "2026-11-15T10:00:00Z".into(),
                last_event: Some("2026-11-15T10:05:00Z".into())
            }
        );
        assert_eq!(
            effective_state(Some(&st(State::Offline)), true, true),
            Effective::Offline {
                since: Some("2026-11-15T10:00:00Z".into())
            }
        );
        assert_eq!(
            effective_state(Some(&st(State::NeedsAttention)), true, true),
            Effective::NeedsAttention {
                error: Some("Slack rejected the token".into())
            }
        );
        // Just launched: running, nothing written yet.
        assert_eq!(
            effective_state(None, true, true),
            Effective::Offline { since: None }
        );
    }

    #[test]
    fn a_status_that_says_running_with_no_one_holding_the_lock_is_a_crash() {
        assert_eq!(
            effective_state(Some(&st(State::Connected)), false, true),
            Effective::Crashed
        );
        assert_eq!(
            effective_state(Some(&st(State::Offline)), false, true),
            Effective::Crashed
        );
    }

    #[test]
    fn a_stopped_listener_is_stopped_or_needs_attention_never_a_crash() {
        assert_eq!(effective_state(None, false, true), Effective::Stopped);
        // It exited on a rejected token: the reason stays visible.
        assert_eq!(
            effective_state(Some(&st(State::NeedsAttention)), false, true),
            Effective::NeedsAttention {
                error: Some("Slack rejected the token".into())
            }
        );
    }

    #[test]
    fn a_listener_run_by_hand_is_shown_even_when_not_installed() {
        assert_eq!(
            effective_state(Some(&st(State::Connected)), true, false),
            Effective::Connected {
                since: "2026-11-15T10:00:00Z".into(),
                last_event: Some("2026-11-15T10:05:00Z".into())
            }
        );
    }

    #[test]
    fn the_lock_probe_sees_a_holder_and_creates_nothing() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("slack");
        assert!(!listener_lock_held(&state));
        assert!(!state.exists(), "probing must not create the state dir");
        let held = crate::file_lock::FileLock::try_exclusive(&state.join("listener.lock"))
            .unwrap()
            .unwrap();
        assert!(listener_lock_held(&state));
        drop(held);
        assert!(!listener_lock_held(&state));
    }

    fn sample() -> Status {
        Status {
            state: State::NeedsAttention,
            since: "2026-11-15T10:00:00Z".into(),
            last_event: Some("2026-11-15T09:59:00Z".into()),
            last_error: Some("Slack rejected the token".into()),
        }
    }

    #[test]
    fn a_written_status_reads_back() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/status.json");
        write_status(&p, &sample());
        assert_eq!(read_status(&p), Some(sample()));
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(raw["state"], "needs_attention");
    }

    #[test]
    fn a_rewrite_replaces_the_old_status() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("status.json");
        write_status(&p, &sample());
        let next = Status {
            state: State::Connected,
            since: "2026-11-15T11:00:00Z".into(),
            last_event: None,
            last_error: None,
        };
        write_status(&p, &next);
        assert_eq!(read_status(&p), Some(next));
    }

    #[test]
    fn missing_or_garbage_files_read_as_none() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("status.json");
        assert_eq!(read_status(&p), None);
        std::fs::write(&p, "not json").unwrap();
        assert_eq!(read_status(&p), None);
        std::fs::write(&p, r#"{"state":"sleepy","since":"x"}"#).unwrap();
        assert_eq!(read_status(&p), None);
    }
}
