//! Session-scoped grants for Google files the user pasted into the chat.
//!
//! A read tool may open a Google Doc or Sheet by id only if that id appeared
//! in one of the *user's own* messages in the same chat session. Grants are
//! recorded from the text the user posted through the chat UI, before the
//! agent runs, and from nothing else: never from tool output, the agent's
//! reply or any stored document text, so a hostile document cannot make the
//! chat read another file.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Value};

use crate::settings_store::{atomic_write_json, read_object};

/// The newest this many ids are kept per session.
const MAX_IDS: usize = 200;
const MIN_BARE_LEN: usize = 25;
const MAX_BARE_LEN: usize = 100;

fn url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?:docs\.google\.com/(?:document|spreadsheets|presentation)|drive\.google\.com/file)/d/([A-Za-z0-9_-]+)",
        )
        .expect("valid regex")
    })
}

fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z0-9_-]+").expect("valid regex"))
}

/// File ids in `text`: Docs/Sheets/Slides/Drive URLs, plus bare ids that are
/// whole tokens of 25+ id characters. First-seen order, no duplicates.
pub fn extract_file_ids(text: &str) -> Vec<String> {
    let mut found: Vec<(usize, String)> = url_re()
        .captures_iter(text)
        .filter_map(|c| c.get(1))
        .map(|m| (m.start(), m.as_str().to_string()))
        .collect();
    found.extend(
        token_re()
            .find_iter(text)
            .filter(|m| (MIN_BARE_LEN..=MAX_BARE_LEN).contains(&m.as_str().len()))
            .map(|m| (m.start(), m.as_str().to_string())),
    );
    found.sort_by_key(|(pos, _)| *pos);
    let mut out: Vec<String> = Vec::new();
    for (_, id) in found {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// `^[A-Za-z0-9_-]{1,64}$`: a session name that is safe as a file name.
fn valid_session(session: &str) -> bool {
    (1..=64).contains(&session.len())
        && session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn read_ids(path: &Path) -> Vec<String> {
    read_object(path)
        .get("ids")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Grants `ids` to `session`, keeping only the newest [`MAX_IDS`].
pub fn record(dir: &Path, session: &str, ids: &[String]) -> Result<(), String> {
    if !valid_session(session) {
        return Err("invalid session".into());
    }
    if ids.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{session}.json"));
    let mut all = read_ids(&path);
    for id in ids {
        all.retain(|x| x != id);
        all.push(id.clone());
    }
    if all.len() > MAX_IDS {
        all.drain(..all.len() - MAX_IDS);
    }
    atomic_write_json(&json!({"ids": all}), &path).map_err(|e| e.to_string())
}

/// True only when `id` was recorded for exactly this session.
pub fn granted(dir: &Path, session: &str, id: &str) -> bool {
    valid_session(session)
        && read_ids(&dir.join(format!("{session}.json")))
            .iter()
            .any(|x| x == id)
}

/// Records the ids in a message the user posted. Call only with the user's
/// own text.
pub fn record_user_message(dir: &Path, session: &str, text: &str) -> Result<(), String> {
    record(dir, session, &extract_file_ids(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789";

    #[test]
    fn extracts_url_forms_and_bare_ids() {
        for url in [
            format!("https://docs.google.com/document/d/{ID}/edit"),
            format!("https://docs.google.com/spreadsheets/d/{ID}/edit#gid=0"),
            format!("https://docs.google.com/presentation/d/{ID}/edit"),
            format!("https://drive.google.com/file/d/{ID}/view"),
        ] {
            assert_eq!(extract_file_ids(&format!("see {url} please")), vec![ID]);
        }
        assert_eq!(extract_file_ids(&format!("read {ID}.")), vec![ID]);
    }

    #[test]
    fn short_tokens_and_embedded_runs_are_not_ids() {
        assert!(extract_file_ids("short-token_123 and https://example.com/").is_empty());
        let long = "a".repeat(300);
        assert!(extract_file_ids(&long).is_empty());
        // A 25+ run glued to other id characters is one longer token, not an id.
        assert!(extract_file_ids(&format!("{ID}{}", "x".repeat(100))).is_empty());
    }

    #[test]
    fn dedupes_preserving_order() {
        let other = "9ZyXwVuTsRqPoNmLkJiHgFeDcBa987654321";
        let text = format!(
            "{other} then https://docs.google.com/document/d/{ID}/edit and {other} and {ID}"
        );
        assert_eq!(extract_file_ids(&text), vec![other, ID]);
    }

    #[test]
    fn granted_only_for_recorded_ids_in_that_session() {
        let d = tempfile::tempdir().unwrap();
        record(d.path(), "sess-a", &[ID.to_string()]).unwrap();
        assert!(granted(d.path(), "sess-a", ID));
        assert!(!granted(d.path(), "sess-a", "other"));
        assert!(!granted(d.path(), "sess-b", ID));
        assert!(d.path().join("sess-a.json").exists());
    }

    #[test]
    fn invalid_session_names_are_rejected() {
        let d = tempfile::tempdir().unwrap();
        for bad in ["", "../x", "a/b", "a.b", " ", &"s".repeat(65)] {
            assert!(record(d.path(), bad, &[ID.to_string()]).is_err(), "{bad:?}");
            assert!(!granted(d.path(), bad, ID));
        }
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
    }

    #[test]
    fn store_is_bounded_to_the_newest_ids() {
        let d = tempfile::tempdir().unwrap();
        let ids: Vec<String> = (0..250).map(|i| format!("id{i:030}")).collect();
        record(d.path(), "s", &ids).unwrap();
        assert!(!granted(d.path(), "s", &ids[0]));
        assert!(!granted(d.path(), "s", &ids[49]));
        assert!(granted(d.path(), "s", &ids[50]));
        assert!(granted(d.path(), "s", &ids[249]));
    }
}
