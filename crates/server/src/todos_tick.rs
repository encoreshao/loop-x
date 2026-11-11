//! The background tick that keeps the cached GitLab todos fresh while sync is on.

use chrono::{DateTime, Duration, Utc};
use loopx_core::todos::{self};
use loopx_core::todos_sync::{apply_fetched, fetch_all, Fetched};
use std::path::Path;

/// One GitLab instance's source, or why it could not be built.
pub type Source = loopx_core::todos_sync::InstanceSource;

/// How often the thread looks at the settings (the fetch interval is separate).
const LOOK_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Default)]
pub struct TickState {
    last_run: Option<DateTime<Utc>>,
    was_enabled: bool,
}

/// Fetch if sync is on and the interval has passed (or sync was just turned on).
/// Returns whether a fetch ran. While off it reads at most and writes nothing. The
/// slow network part runs outside the store lock; the result is applied under it, and
/// dropped if the user switched sync off meanwhile.
pub fn tick(
    store_path: &Path,
    sources: impl FnOnce() -> Vec<Source>,
    now: DateTime<Utc>,
    state: &mut TickState,
) -> bool {
    let store = match todos::load(store_path) {
        Ok(s) => s,
        Err(_) => {
            // Keep the unreadable file for the user before anything could replace it.
            let _ = std::fs::copy(store_path, store_path.with_extension("json.bad"));
            return false;
        }
    };
    let enabled = store.settings.enabled;
    let just_enabled = enabled && !state.was_enabled;
    state.was_enabled = enabled;
    if !enabled {
        return false;
    }
    let interval = Duration::minutes(i64::from(store.settings.interval_minutes));
    let due = state.last_run.is_none_or(|last| now - last >= interval);
    if !due && !just_enabled {
        return false;
    }
    let fetched = fetch_sources(sources());
    let _ = todos::with_store(store_path, |s| {
        if s.settings.enabled {
            apply_fetched(s, fetched, now);
        }
    });
    state.last_run = Some(now);
    true
}

/// Fetch every source; one that failed to build reports why instead.
pub fn fetch_sources(sources: Vec<Source>) -> Vec<Fetched> {
    let mut built = Vec::new();
    let mut failed = Vec::new();
    for (name, source) in sources {
        match source {
            Ok(s) => built.push((name, s)),
            Err(e) => failed.push((name, Err(e))),
        }
    }
    let mut out = fetch_all(&built);
    out.extend(failed);
    out
}

/// The dashboard daemon's sync loop. Toggling the switch needs no restart.
pub fn start_todos_sync() {
    std::thread::spawn(move || {
        let mut state = TickState::default();
        loop {
            tick(
                &todos::path(),
                || {
                    loopx_core::todos_sync::sources_from_config(
                        loopx_core::gitlab::load_config().as_ref(),
                    )
                },
                Utc::now(),
                &mut state,
            );
            std::thread::sleep(LOOK_EVERY);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    use loopx_core::todos::{self, TodoStore};
    use loopx_core::todos_sync::TodoSource;
    use serde_json::{json, Value};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap()
    }

    struct One(Value);
    impl TodoSource for One {
        fn pending(&self) -> Result<Vec<Value>, String> {
            Ok(vec![self.0.clone()])
        }
        fn mark_done(&self, _: i64) -> Result<(), String> {
            Ok(())
        }
    }

    fn sources() -> Vec<Source> {
        let v = json!({"id": 1, "action_name": "mentioned", "target_type": "Issue",
            "target": {"iid": 1, "title": "t", "web_url": "u"}, "body": "b",
            "author": {"username": "a"}, "project": {"path_with_namespace": "g/p"},
            "state": "pending", "created_at": (now() - Duration::hours(1)).to_rfc3339()});
        vec![("work".into(), Ok(Box::new(One(v)) as Box<dyn TodoSource>))]
    }

    fn enable(path: &std::path::Path, minutes: u32) {
        let mut s = TodoStore::default();
        s.settings.enabled = true;
        s.settings.interval_minutes = minutes;
        todos::save(&s, path).unwrap();
    }

    #[test]
    fn tick_does_nothing_and_creates_no_file_while_off() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("todos.json");
        let mut t = TickState::default();
        assert!(!tick(&p, sources, now(), &mut t));
        assert!(!p.exists());
    }

    #[test]
    fn turning_sync_on_fetches_on_the_next_tick_then_waits_for_the_interval() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("todos.json");
        let mut t = TickState::default();
        enable(&p, 5);
        assert!(tick(&p, sources, now(), &mut t));
        assert!(todos::load(&p).unwrap().instances["work"]
            .todos
            .contains_key(&1));
        assert!(!tick(&p, sources, now() + Duration::minutes(4), &mut t));
        assert!(tick(&p, sources, now() + Duration::minutes(5), &mut t));
    }

    #[test]
    fn a_corrupt_store_is_copied_to_bad_and_the_tick_does_not_panic() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("todos.json");
        std::fs::write(&p, "{nope").unwrap();
        let mut t = TickState::default();
        assert!(!tick(&p, sources, now(), &mut t));
        assert_eq!(
            std::fs::read_to_string(d.path().join("todos.json.bad")).unwrap(),
            "{nope"
        );
    }

    #[test]
    fn a_source_that_could_not_be_built_records_its_error() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("todos.json");
        enable(&p, 5);
        let mut t = TickState::default();
        assert!(tick(
            &p,
            || vec![("work".into(), Err("no token".into()))],
            now(),
            &mut t
        ));
        assert_eq!(
            todos::load(&p).unwrap().instances["work"]
                .last_error
                .as_deref(),
            Some("no token")
        );
    }

    #[test]
    fn a_switch_turned_off_during_the_fetch_stays_off_and_discards_the_result() {
        struct Disables(std::path::PathBuf);
        impl TodoSource for Disables {
            fn pending(&self) -> Result<Vec<Value>, String> {
                todos::with_store(&self.0, |s| s.settings.enabled = false).unwrap();
                Ok(vec![])
            }
            fn mark_done(&self, _: i64) -> Result<(), String> {
                Ok(())
            }
        }
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("todos.json");
        enable(&p, 5);
        let mut t = TickState::default();
        let path = p.clone();
        assert!(tick(
            &p,
            move || vec![(
                "work".into(),
                Ok(Box::new(Disables(path)) as Box<dyn TodoSource>)
            )],
            now(),
            &mut t
        ));
        let s = todos::load(&p).unwrap();
        assert!(!s.settings.enabled);
        assert!(s.instances.is_empty());
    }
}
