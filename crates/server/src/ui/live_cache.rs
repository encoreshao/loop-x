//! The last good result of a slow live fetch (GitLab → Tasks, MR Review's Merge
//! requests), kept under `outputs/cache/<name>.json` so the page can paint it at
//! once while the shell's `data-lazy-load` handler fetches fresh data. The data
//! is cached, never the HTML: a render carries the per-process CSRF token, and a
//! cached render would show stale choices (e.g. a switch just flipped).

use super::pages::loops_common::relative_time_dt;
use super::pyfmt::escape as h;
use super::routes::UiCtx;
use super::state::UiState;
use chrono::{DateTime, FixedOffset, SecondsFormat};
use serde_json::{json, Value};
use std::path::PathBuf;

pub fn path(state: &UiState, name: &str) -> PathBuf {
    state
        .repo_root
        .join("outputs")
        .join("cache")
        .join(format!("{name}.json"))
}

/// `(data, saved_at)`, or `None` when missing or unreadable.
pub fn read(state: &UiState, name: &str) -> Option<(Value, DateTime<FixedOffset>)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path(state, name)).ok()?).ok()?;
    let saved = DateTime::parse_from_rfc3339(v.get("saved_at")?.as_str()?).ok()?;
    Some((v.get("data")?.clone(), saved))
}

/// Best effort: a cache that cannot be written only costs the next page a spinner.
pub fn write(state: &UiState, name: &str, data: Value) {
    let p = path(state, name);
    let Some(dir) = p.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let body =
        json!({"saved_at": state.now().to_rfc3339_opts(SecondsFormat::Secs, true), "data": data});
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, body.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

/// What goes inside a `data-lazy-load` container before the fresh fragment
/// arrives: the cached render under an "Updated … · refreshing" note, or the
/// full loading spinner when nothing is cached yet.
pub fn placeholder(
    ctx: &UiCtx,
    cached: Option<(String, DateTime<FixedOffset>)>,
    loading_text: &str,
) -> String {
    match cached {
        Some((html, saved)) => format!(
            "<p class='lazy-stale' role='status'><span class='md-spinner md-spinner-sm' aria-hidden='true'></span>{}</p>{html}",
            h(&ctx.tf(
                "Updated {when} \u{b7} refreshing\u{2026}",
                &[("when", &relative_time_dt(ctx.state.now(), saved))]
            ))
        ),
        None => format!(
            "<div class=\"lazy-loading\"><div class=\"md-spinner\"></div><p class=\"loading-text\">{}\
<span class=\"loading-dots\"><span>.</span><span>.</span><span>.</span></span></p></div>",
            h(loading_text)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::routes::UiRequest;

    #[test]
    fn round_trips_and_tolerates_a_missing_or_broken_file() {
        let d = tempfile::tempdir().unwrap();
        let st = UiState::explicit(
            d.path(),
            d.path(),
            d.path(),
            &d.path().join("u"),
            "tok",
            None,
        );
        assert!(read(&st, "x").is_none());
        write(&st, "x", json!([1, 2]));
        let (data, saved) = read(&st, "x").unwrap();
        assert_eq!(data, json!([1, 2]));
        assert_eq!(saved.timestamp(), st.now().timestamp());
        std::fs::write(path(&st, "x"), "{nope").unwrap();
        assert!(read(&st, "x").is_none());
    }

    #[test]
    fn placeholder_is_the_spinner_only_without_a_cache() {
        let d = tempfile::tempdir().unwrap();
        let st = UiState::explicit(
            d.path(),
            d.path(),
            d.path(),
            &d.path().join("u"),
            "tok",
            None,
        );
        let req = UiRequest::get("/");
        let ctx = UiCtx::new(&st, &req);
        let spinner = placeholder(&ctx, None, "Loading <stuff>");
        assert!(
            spinner.contains("class=\"md-spinner\"") && spinner.contains("Loading &lt;stuff&gt;")
        );
        let saved = st.now().fixed_offset() - chrono::Duration::minutes(5);
        let stale = placeholder(&ctx, Some(("<ul>cached</ul>".into(), saved)), "Loading");
        assert!(
            stale.ends_with("<ul>cached</ul>") && stale.contains("Updated 5m ago"),
            "{stale}"
        );
        assert!(!stale.contains("lazy-loading"));
    }
}
