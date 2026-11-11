//! GitLab todos mirrored locally (`<LOOP_ENGINEERING_HOME>/todos.json`): opt-in
//! settings, a per-instance cache and the pure merge rules the sync uses. No
//! network here; see `todos_sync`.

use crate::config::home_dir;
use crate::pyjson;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A first enable imports only todos created this recently.
const FIRST_IMPORT_DAYS: i64 = 2;
/// Clock-skew margin on the later import cutoff.
const SKEW_MINUTES: i64 = 10;
/// Done todos are dropped from the cache after this long.
const PRUNE_DONE_DAYS: i64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TodoSettings {
    pub enabled: bool,
    pub interval_minutes: u32,
}

impl Default for TodoSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_minutes: 5,
        }
    }
}

impl TodoSettings {
    /// Only 5 and 10 minutes are offered; anything else is 5.
    pub fn normalized_interval(minutes: u32) -> u32 {
        if minutes == 10 {
            10
        } else {
            5
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Todo {
    pub id: i64,
    pub instance: String,
    pub action: String,
    pub target_type: String,
    pub target_iid: Option<i64>,
    pub title: String,
    pub web_url: String,
    pub project: String,
    pub author: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
    /// `pending` or `done`.
    pub state: String,
    pub done_at: Option<DateTime<Utc>>,
    pub draft_approval_id: Option<String>,
}

impl Todo {
    /// One entry of GitLab's `GET /todos`; `None` when it lacks an id or a valid time.
    pub fn from_gitlab(instance: &str, v: &serde_json::Value) -> Option<Todo> {
        let text =
            |x: Option<&serde_json::Value>| x.and_then(|x| x.as_str()).unwrap_or("").to_string();
        let target = v.get("target");
        let created_at = DateTime::parse_from_rfc3339(v.get("created_at")?.as_str()?)
            .ok()?
            .with_timezone(&Utc);
        let web_url = match text(target.and_then(|t| t.get("web_url"))) {
            u if u.is_empty() => text(v.get("target_url")),
            u => u,
        };
        Some(Todo {
            id: v.get("id")?.as_i64()?,
            instance: instance.to_string(),
            action: text(v.get("action_name")),
            target_type: text(v.get("target_type")),
            target_iid: target.and_then(|t| t.get("iid")).and_then(|i| i.as_i64()),
            title: text(target.and_then(|t| t.get("title"))),
            web_url,
            project: text(v.get("project").and_then(|p| p.get("path_with_namespace"))),
            author: text(v.get("author").and_then(|a| a.get("username"))),
            body: text(v.get("body")),
            created_at,
            state: if text(v.get("state")) == "done" {
                "done"
            } else {
                "pending"
            }
            .to_string(),
            done_at: None,
            draft_approval_id: None,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstanceState {
    pub last_capture: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub todos: BTreeMap<i64, Todo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TodoStore {
    pub settings: TodoSettings,
    pub instances: BTreeMap<String, InstanceState>,
}

pub fn path() -> PathBuf {
    home_dir().join("todos.json")
}

/// A missing file is the default (sync off); an unreadable one is an `Err` the
/// caller must not overwrite without keeping a copy.
pub fn load(path: &Path) -> Result<TodoStore, String> {
    if !path.exists() {
        return Ok(TodoStore::default());
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("{} is not valid: {e}", path.display()))
}

static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Load, change and save `todos.json` under one process-wide lock, so the background
/// tick and the request handlers never overwrite each other's changes. A corrupt
/// file is an `Err` and is left untouched.
pub fn with_store<R>(path: &Path, f: impl FnOnce(&mut TodoStore) -> R) -> Result<R, String> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = load(path)?;
    let out = f(&mut store);
    save(&store, path)?;
    Ok(out)
}

/// Same-directory temp file then rename, so a reader never sees a torn file.
pub fn save(store: &TodoStore, path: &Path) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, pyjson::to_string_pretty(store)).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// The earliest `created_at` a pending todo may have to be imported: two days
/// back on a first enable, else `min(last_capture, now)` less a skew margin.
pub fn import_cutoff(last_capture: Option<DateTime<Utc>>, now: DateTime<Utc>) -> DateTime<Utc> {
    match last_capture {
        None => now - Duration::days(FIRST_IMPORT_DAYS),
        Some(last) => last.min(now) - Duration::minutes(SKEW_MINUTES),
    }
}

/// Fold one successful fetch of an instance's pending todos into its state.
pub fn merge_fetch(state: &mut InstanceState, fetched_pending: Vec<Todo>, now: DateTime<Utc>) {
    let cutoff = import_cutoff(state.last_capture, now);
    let fetched_ids: std::collections::BTreeSet<i64> =
        fetched_pending.iter().map(|t| t.id).collect();
    for t in fetched_pending {
        match state.todos.get_mut(&t.id) {
            Some(old) => {
                // Pending in GitLab again (reopened): it is pending here too.
                old.state = "pending".into();
                old.done_at = None;
                old.action = t.action;
                old.title = t.title;
                old.body = t.body;
                old.web_url = t.web_url;
            }
            None if t.created_at >= cutoff => {
                state.todos.insert(t.id, t);
            }
            None => {}
        }
    }
    for t in state.todos.values_mut() {
        if t.state == "pending" && !fetched_ids.contains(&t.id) {
            t.state = "done".into();
            t.done_at = Some(now);
        }
    }
    let keep_after = now - Duration::days(PRUNE_DONE_DAYS);
    state
        .todos
        .retain(|_, t| t.state != "done" || t.done_at.is_none_or(|d| d >= keep_after));
    state.last_capture = Some(now);
    state.last_error = None;
}

/// A failed fetch changes nothing but the shown error.
pub fn record_error(state: &mut InstanceState, message: &str) {
    state.last_error = Some(message.to_string());
}

/// Todo kinds a reply can sensibly be drafted for.
pub fn is_reply_worthy(action: &str) -> bool {
    matches!(
        action,
        "mentioned" | "directly_addressed" | "assigned" | "review_requested" | "marked"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap()
    }

    fn todo(id: i64, age: Duration) -> Todo {
        Todo {
            id,
            instance: "work".into(),
            action: "mentioned".into(),
            target_type: "Issue".into(),
            target_iid: Some(7),
            title: "t".into(),
            web_url: "https://gl/x".into(),
            project: "g/p".into(),
            author: "ann".into(),
            body: "hi".into(),
            created_at: now() - age,
            state: "pending".into(),
            done_at: None,
            draft_approval_id: None,
        }
    }

    #[test]
    fn first_enable_imports_only_the_last_two_days() {
        let mut st = InstanceState::default();
        merge_fetch(
            &mut st,
            vec![todo(1, Duration::days(1)), todo(2, Duration::days(3))],
            now(),
        );
        assert!(st.todos.contains_key(&1));
        assert!(!st.todos.contains_key(&2));
        assert_eq!(st.last_capture, Some(now()));
    }

    #[test]
    fn cutoff_is_min_of_last_capture_and_now() {
        let skew = Duration::minutes(10);
        assert_eq!(
            import_cutoff(Some(now() + Duration::hours(1)), now()),
            now() - skew
        );
        assert_eq!(
            import_cutoff(Some(now() - Duration::hours(1)), now()),
            now() - Duration::hours(1) - skew
        );
        assert_eq!(import_cutoff(None, now()), now() - Duration::days(2));
    }

    #[test]
    fn later_fetch_imports_new_ids_and_skips_cached_ones() {
        let mut st = InstanceState::default();
        merge_fetch(&mut st, vec![todo(1, Duration::hours(2))], now());
        st.todos.get_mut(&1).unwrap().draft_approval_id = Some("apr_1".into());
        let later = now() + Duration::minutes(5);
        merge_fetch(
            &mut st,
            vec![todo(1, Duration::hours(2)), todo(2, Duration::minutes(1))],
            later,
        );
        assert_eq!(st.todos.len(), 2);
        // A cached todo keeps its local draft link.
        assert_eq!(st.todos[&1].draft_approval_id.as_deref(), Some("apr_1"));
    }

    #[test]
    fn a_pending_todo_missing_from_the_fetch_becomes_done() {
        let mut st = InstanceState::default();
        merge_fetch(&mut st, vec![todo(1, Duration::hours(1))], now());
        let later = now() + Duration::minutes(5);
        merge_fetch(&mut st, vec![], later);
        assert_eq!(st.todos[&1].state, "done");
        assert_eq!(st.todos[&1].done_at, Some(later));
    }

    #[test]
    fn done_todos_older_than_thirty_days_are_pruned() {
        let mut st = InstanceState::default();
        let mut old = todo(1, Duration::days(40));
        old.state = "done".into();
        old.done_at = Some(now() - Duration::days(31));
        st.todos.insert(1, old);
        merge_fetch(&mut st, vec![], now());
        assert!(st.todos.is_empty());
    }

    #[test]
    fn a_failed_instance_keeps_last_capture() {
        let mut st = InstanceState::default();
        merge_fetch(&mut st, vec![todo(1, Duration::hours(1))], now());
        record_error(&mut st, "boom");
        assert_eq!(st.last_capture, Some(now()));
        assert_eq!(st.last_error.as_deref(), Some("boom"));
        assert_eq!(st.todos.len(), 1);
    }

    #[test]
    fn load_of_a_missing_file_is_default_and_off() {
        let d = tempfile::tempdir().unwrap();
        let s = load(&d.path().join("todos.json")).unwrap();
        assert!(!s.settings.enabled);
        assert_eq!(s.settings.interval_minutes, 5);
    }

    #[test]
    fn load_of_corrupt_json_is_err_and_save_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("todos.json");
        std::fs::write(&p, "{nope").unwrap();
        assert!(load(&p).is_err());
        let mut s = TodoStore::default();
        s.settings.enabled = true;
        s.instances
            .entry("work".into())
            .or_default()
            .todos
            .insert(1, todo(1, Duration::hours(1)));
        save(&s, &p).unwrap();
        let back = load(&p).unwrap();
        assert!(back.settings.enabled);
        assert_eq!(back.instances["work"].todos[&1].title, "t");
    }

    #[test]
    fn interval_other_than_5_or_10_becomes_5() {
        assert_eq!(TodoSettings::normalized_interval(10), 10);
        assert_eq!(TodoSettings::normalized_interval(7), 5);
        assert_eq!(TodoSettings::normalized_interval(0), 5);
    }

    #[test]
    fn reply_worthy_matches_the_spec_list() {
        for a in [
            "mentioned",
            "directly_addressed",
            "assigned",
            "review_requested",
            "marked",
        ] {
            assert!(is_reply_worthy(a), "{a}");
        }
        for a in ["build_failed", "unmergeable", "approval_required"] {
            assert!(!is_reply_worthy(a), "{a}");
        }
    }

    #[test]
    fn from_gitlab_reads_the_todo_payload() {
        let v = json!({"id": 9, "action_name": "directly_addressed", "target_type": "MergeRequest",
            "target": {"iid": 3, "title": "Fix", "web_url": "https://gl/mr/3"},
            "body": "ping", "author": {"username": "bob"},
            "project": {"path_with_namespace": "g/p"}, "state": "pending",
            "created_at": "2026-11-11T10:00:00.000Z"});
        let t = Todo::from_gitlab("work", &v).unwrap();
        assert_eq!((t.id, t.target_iid, t.author.as_str()), (9, Some(3), "bob"));
        assert_eq!(t.web_url, "https://gl/mr/3");
        assert!(Todo::from_gitlab("work", &json!({"id": 1})).is_none());
    }

    #[test]
    fn a_done_todo_that_comes_back_pending_is_reopened() {
        let mut st = InstanceState::default();
        merge_fetch(&mut st, vec![todo(1, Duration::hours(1))], now());
        merge_fetch(&mut st, vec![], now() + Duration::minutes(5));
        assert_eq!(st.todos[&1].state, "done");
        merge_fetch(
            &mut st,
            vec![todo(1, Duration::hours(1))],
            now() + Duration::minutes(10),
        );
        assert_eq!(st.todos[&1].state, "pending");
        assert_eq!(st.todos[&1].done_at, None);
    }

    #[test]
    fn with_store_loads_applies_and_saves() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("todos.json");
        with_store(&p, |s| s.settings.enabled = true).unwrap();
        assert!(load(&p).unwrap().settings.enabled);
        std::fs::write(&p, "{nope").unwrap();
        assert!(with_store(&p, |s| s.settings.enabled = false).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{nope");
    }
}
