//! Local state for a Google Slides connector account, one file per account under
//! `<home>/slides/<account>.json`: the template list last fetched from Drive
//! (a cache, so the pages open without a network call), which templates the user
//! ticked as usable, and the decks created so far (what the review view lists).
//! It holds file ids, names and links only - never a token.

use crate::connectors_config::is_valid_id;
use crate::settings_store::{atomic_write_json, read_object};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Decks remembered per account; older ones drop off the list (they stay in Drive).
const MAX_DECKS: usize = 30;

pub fn default_dir() -> PathBuf {
    crate::config::home_dir().join("slides")
}

fn path(dir: &Path, account: &str) -> Option<PathBuf> {
    is_valid_id(account).then(|| dir.join(format!("{account}.json")))
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

fn load(dir: &Path, account: &str) -> Map<String, Value> {
    path(dir, account)
        .map(|p| read_object(&p))
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn save(dir: &Path, account: &str, state: Map<String, Value>) -> Result<(), String> {
    let p = path(dir, account).ok_or_else(|| "Invalid connector id".to_string())?;
    atomic_write_json(&Value::Object(state), &p)
        .map_err(|_| "Could not save slide settings".to_string())
}

/// The cached template rows (as `Connector::list_templates` returned them).
pub fn cached_templates(dir: &Path, account: &str) -> Vec<Value> {
    load(dir, account)
        .get("templates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// When the cache was last refreshed (RFC 3339), or empty.
pub fn fetched_at(dir: &Path, account: &str) -> String {
    load(dir, account)
        .get("fetched_at")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Replace the cache with a fresh Drive listing. The ticked templates are kept,
/// including ones missing from this listing: a hiccup must not untick them.
pub fn cache_templates(
    dir: &Path,
    account: &str,
    templates: &[Value],
    now: &str,
) -> Result<(), String> {
    let mut state = load(dir, account);
    state.insert("templates".into(), Value::Array(templates.to_vec()));
    state.insert("fetched_at".into(), json!(now));
    save(dir, account, state)
}

/// The ids the user ticked.
pub fn selected_ids(dir: &Path, account: &str) -> Vec<String> {
    strings(load(dir, account).get("selected"))
}

/// Store the ticked ids (distinct, in the given order).
pub fn set_selected(dir: &Path, account: &str, ids: &[String]) -> Result<(), String> {
    let mut seen: Vec<String> = Vec::new();
    for id in ids {
        let id = id.trim();
        if !id.is_empty() && !seen.iter().any(|s| s == id) {
            seen.push(id.to_string());
        }
    }
    let mut state = load(dir, account);
    state.insert("selected".into(), json!(seen));
    save(dir, account, state)
}

/// The ticked templates that are in the cache, in cache order.
pub fn selected_templates(dir: &Path, account: &str) -> Vec<Value> {
    let ids = selected_ids(dir, account);
    cached_templates(dir, account)
        .into_iter()
        .filter(|t| {
            t.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| ids.iter().any(|s| s == id))
        })
        .collect()
}

/// Remember a created deck (`{id, title, web_link, template_id, created}`), newest first.
pub fn record_deck(dir: &Path, account: &str, deck: Value) -> Result<(), String> {
    let mut state = load(dir, account);
    let mut decks: Vec<Value> = state
        .get("decks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let id = deck.get("id").cloned();
    decks.retain(|d| d.get("id") != id.as_ref());
    decks.insert(0, deck);
    decks.truncate(MAX_DECKS);
    state.insert("decks".into(), Value::Array(decks));
    save(dir, account, state)
}

pub fn decks(dir: &Path, account: &str) -> Vec<Value> {
    load(dir, account)
        .get("decks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Forget an account's slide state (its connector was deleted).
pub fn remove(dir: &Path, account: &str) {
    if let Some(p) = path(dir, account) {
        let _ = std::fs::remove_file(p);
    }
}

/// Move an account's slide state to its new id (the connector was renamed).
pub fn rename(dir: &Path, old: &str, new: &str) {
    if let (Some(from), Some(to)) = (path(dir, old), path(dir, new)) {
        if from.is_file() && !to.exists() {
            let _ = std::fs::rename(from, to);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> Value {
        json!({"id": id, "name": format!("T {id}")})
    }

    #[test]
    fn empty_until_something_is_saved() {
        let d = tempfile::tempdir().unwrap();
        assert!(cached_templates(d.path(), "s").is_empty());
        assert!(selected_ids(d.path(), "s").is_empty());
        assert_eq!(fetched_at(d.path(), "s"), "");
        assert!(decks(d.path(), "s").is_empty());
    }

    #[test]
    fn refreshing_the_cache_keeps_the_ticked_ids() {
        let d = tempfile::tempdir().unwrap();
        cache_templates(d.path(), "s", &[row("a"), row("b")], "2026-10-25T00:00:00Z").unwrap();
        set_selected(
            d.path(),
            "s",
            &["b".into(), "gone".into(), "b".into(), " ".into()],
        )
        .unwrap();
        cache_templates(d.path(), "s", &[row("b"), row("c")], "2026-10-26T00:00:00Z").unwrap();
        assert_eq!(selected_ids(d.path(), "s"), ["b", "gone"]);
        assert_eq!(selected_templates(d.path(), "s"), vec![row("b")]);
        assert_eq!(fetched_at(d.path(), "s"), "2026-10-26T00:00:00Z");
    }

    #[test]
    fn decks_are_newest_first_deduped_and_capped() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..(MAX_DECKS + 5) {
            record_deck(d.path(), "s", json!({"id": format!("d{i}")})).unwrap();
        }
        record_deck(d.path(), "s", json!({"id": "d10", "title": "again"})).unwrap();
        let all = decks(d.path(), "s");
        assert_eq!(all.len(), MAX_DECKS);
        assert_eq!(all[0]["title"], "again");
        assert_eq!(all.iter().filter(|x| x["id"] == "d10").count(), 1);
    }

    #[test]
    fn bad_account_ids_never_touch_the_disk() {
        let d = tempfile::tempdir().unwrap();
        assert!(set_selected(d.path(), "../x", &["a".into()]).is_err());
        assert!(selected_ids(d.path(), "../x").is_empty());
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
    }

    #[test]
    fn rename_moves_the_state_and_never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        set_selected(d.path(), "a", &["x".into()]).unwrap();
        set_selected(d.path(), "taken", &["keep".into()]).unwrap();
        rename(d.path(), "a", "taken");
        assert_eq!(selected_ids(d.path(), "taken"), ["keep"]);
        rename(d.path(), "a", "b");
        assert_eq!(selected_ids(d.path(), "b"), ["x"]);
        assert!(selected_ids(d.path(), "a").is_empty());
    }

    #[test]
    fn remove_deletes_the_file() {
        let d = tempfile::tempdir().unwrap();
        set_selected(d.path(), "s", &["a".into()]).unwrap();
        remove(d.path(), "s");
        assert!(selected_ids(d.path(), "s").is_empty());
    }

    #[test]
    fn the_file_is_private_and_holds_no_secret_fields() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        set_selected(d.path(), "s", &["a".into()]).unwrap();
        let mode = std::fs::metadata(d.path().join("s.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
