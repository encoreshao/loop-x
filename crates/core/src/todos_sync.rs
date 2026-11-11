//! Fetching GitLab todos into the local store and marking them done. GitLab is
//! reached through [`TodoSource`] so the rules are testable without a network.

use crate::gitlab::Client;
use crate::todos::{merge_fetch, record_error, Todo, TodoStore};
use chrono::{DateTime, Utc};
use serde_json::Value;

pub trait TodoSource: Send + Sync {
    /// The user's pending todos, raw GitLab JSON.
    fn pending(&self) -> Result<Vec<Value>, String>;
    fn mark_done(&self, id: i64) -> Result<(), String>;
}

impl TodoSource for Client {
    fn pending(&self) -> Result<Vec<Value>, String> {
        self.list_todos("pending").map_err(|e| e.to_string())
    }
    fn mark_done(&self, id: i64) -> Result<(), String> {
        self.mark_todo_done(id)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// One instance's fetched pending todos, or why the fetch failed.
pub type Fetched = (String, Result<Vec<Todo>, String>);

/// Ask every source for its pending todos. No store is touched, so this can run
/// without holding the store lock while the network is slow.
pub fn fetch_all(sources: &[(String, Box<dyn TodoSource>)]) -> Vec<Fetched> {
    sources
        .iter()
        .map(|(name, source)| {
            let result = source.pending().map(|raw| {
                raw.iter()
                    .filter_map(|v| Todo::from_gitlab(name, v))
                    .collect()
            });
            (name.clone(), result)
        })
        .collect()
}

/// Fold fetched results into the store. One failing instance records its error and
/// keeps its cache and `last_capture`. With any result at all, instances no longer
/// configured are dropped; with none (config unreadable) nothing is dropped.
pub fn apply_fetched(store: &mut TodoStore, fetched: Vec<Fetched>, now: DateTime<Utc>) {
    if !fetched.is_empty() {
        let names: std::collections::BTreeSet<&str> =
            fetched.iter().map(|(n, _)| n.as_str()).collect();
        store.instances.retain(|k, _| names.contains(k.as_str()));
    }
    for (name, result) in fetched {
        let state = store.instances.entry(name).or_default();
        match result {
            Ok(todos) => merge_fetch(state, todos, now),
            Err(e) => record_error(state, &e),
        }
    }
}

/// [`fetch_all`] then [`apply_fetched`].
pub fn sync_all(
    store: &mut TodoStore,
    sources: &[(String, Box<dyn TodoSource>)],
    now: DateTime<Utc>,
) {
    apply_fetched(store, fetch_all(sources), now);
}

/// Mark done in GitLab first; the local copy flips only when GitLab accepted it.
pub fn mark_done(
    store: &mut TodoStore,
    source: &dyn TodoSource,
    instance: &str,
    id: i64,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let todo = store
        .instances
        .get_mut(instance)
        .and_then(|s| s.todos.get_mut(&id))
        .ok_or_else(|| format!("no todo {id} for {instance}"))?;
    source.mark_done(id)?;
    todo.state = "done".into();
    todo.done_at = Some(now);
    Ok(())
}

/// One GitLab instance's source, or why its client could not be built.
pub type InstanceSource = (String, Result<Box<dyn TodoSource>, String>);

/// One source per GitLab instance in the config. An instance whose client cannot
/// be built (missing token) is returned as `Err` so the caller can show why.
pub fn sources_from_config(config: Option<&Value>) -> Vec<InstanceSource> {
    let Some(instances) = config
        .and_then(|c| c.get("instances"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    instances
        .keys()
        .map(|name| {
            let source = Client::from_config(config, Some(name), None)
                .map(|c| Box::new(c) as Box<dyn TodoSource>)
                .map_err(|e| e.to_string());
            (name.clone(), source)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todos::{InstanceState, TodoStore};
    use chrono::{Duration, TimeZone, Utc};
    use serde_json::{json, Value};
    use std::sync::Mutex;

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap()
    }

    struct Fake {
        pending: Result<Vec<Value>, String>,
        done_result: Result<(), String>,
        done_calls: Mutex<Vec<i64>>,
    }

    impl Fake {
        fn ok(pending: Vec<Value>) -> Self {
            Fake {
                pending: Ok(pending),
                done_result: Ok(()),
                done_calls: Mutex::new(vec![]),
            }
        }
    }

    impl TodoSource for Fake {
        fn pending(&self) -> Result<Vec<Value>, String> {
            self.pending.clone()
        }
        fn mark_done(&self, id: i64) -> Result<(), String> {
            self.done_calls.lock().unwrap().push(id);
            self.done_result.clone()
        }
    }

    fn payload(id: i64) -> Value {
        json!({"id": id, "action_name": "mentioned", "target_type": "Issue",
            "target": {"iid": 1, "title": "t", "web_url": "https://gl/1"}, "body": "b",
            "author": {"username": "a"}, "project": {"path_with_namespace": "g/p"},
            "state": "pending", "created_at": (now() - Duration::hours(1)).to_rfc3339()})
    }

    #[test]
    fn sync_isolates_a_failing_instance() {
        let mut store = TodoStore::default();
        store.instances.insert(
            "bad".into(),
            InstanceState {
                last_capture: Some(now() - Duration::hours(1)),
                ..Default::default()
            },
        );
        let sources: Vec<(String, Box<dyn TodoSource>)> = vec![
            (
                "bad".into(),
                Box::new(Fake {
                    pending: Err("401".into()),
                    ..Fake::ok(vec![])
                }),
            ),
            ("good".into(), Box::new(Fake::ok(vec![payload(5)]))),
        ];
        sync_all(&mut store, &sources, now());
        assert_eq!(store.instances["bad"].last_error.as_deref(), Some("401"));
        assert_eq!(
            store.instances["bad"].last_capture,
            Some(now() - Duration::hours(1))
        );
        assert!(store.instances["good"].todos.contains_key(&5));
        assert_eq!(store.instances["good"].last_capture, Some(now()));
    }

    fn store_with_pending(id: i64) -> TodoStore {
        let mut store = TodoStore::default();
        let sources: Vec<(String, Box<dyn TodoSource>)> =
            vec![("work".into(), Box::new(Fake::ok(vec![payload(id)])))];
        sync_all(&mut store, &sources, now());
        store
    }

    #[test]
    fn mark_done_does_not_flip_local_state_when_gitlab_errors() {
        let mut store = store_with_pending(3);
        let src = Fake {
            done_result: Err("500".into()),
            ..Fake::ok(vec![])
        };
        assert!(mark_done(&mut store, &src, "work", 3, now()).is_err());
        assert_eq!(store.instances["work"].todos[&3].state, "pending");
    }

    #[test]
    fn mark_done_flips_local_state_on_success() {
        let mut store = store_with_pending(3);
        let src = Fake::ok(vec![]);
        mark_done(&mut store, &src, "work", 3, now()).unwrap();
        assert_eq!(store.instances["work"].todos[&3].state, "done");
        assert_eq!(*src.done_calls.lock().unwrap(), vec![3]);
        assert!(mark_done(&mut store, &src, "work", 99, now()).is_err());
    }

    #[test]
    fn instances_no_longer_in_the_sources_are_dropped_but_none_means_keep_all() {
        let mut store = store_with_pending(3);
        store
            .instances
            .insert("old".into(), InstanceState::default());
        let sources: Vec<(String, Box<dyn TodoSource>)> =
            vec![("work".into(), Box::new(Fake::ok(vec![payload(3)])))];
        sync_all(&mut store, &sources, now());
        assert!(!store.instances.contains_key("old"));
        sync_all(&mut store, &[], now());
        assert!(store.instances.contains_key("work"));
    }
}
