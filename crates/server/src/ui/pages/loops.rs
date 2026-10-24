//! The loops catalog (`/loops`), the generic per-loop page (`/loops/<name>`: views
//! live, content, people, history, settings) for every registered loop other than the
//! three built-in hub loops, a run's report (`/loops/<name>/history/<file>`) and the
//! POSTs that belong to those pages (run-now, settings, notify).

use super::connectors as conn;
use super::loops_common::{aware, flash_html, relative_time, render_loop_page, run_now_action_html, custom_select};
use crate::ui::badge::{default_badge, status_badge_markup};
use crate::ui::consts;
use crate::ui::hub::{self, Hub};
use crate::ui::i18n::Lang;
use crate::ui::markdown::{render_markdown, GitlabPrefixes};
use crate::ui::pyfmt::{self, escape as h};
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::shell::{render_shell, ShellOpts};
use crate::ui::state::UiState;
use chrono::{DateTime, Datelike, Duration, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use loopx_core::connectors::{self as connectors, Account, Field};
use loopx_core::connectors_config as cc;
use loopx_core::loops_config::{self, py_truthy};
use loopx_core::pydumps::py_str;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub fn register(r: &mut Registry) {
    r.get("/loops", catalog_page);
    r.get("/loops/:name", loop_page);
    r.get("/loops/:name/history/:file", history_file);
    r.post("/loops/:name/settings", settings);
    r.post("/loops/:name/notify", notify);
}

const BUILTIN: [&str; 3] = ["gitlab-loop", "topic-loop", "inbox-triage-loop"];

fn is_builtin(name: &str) -> bool {
    BUILTIN.contains(&name)
}

// ---- registry access -------------------------------------------------------------------

fn list_loops(state: &UiState) -> Option<Vec<Value>> {
    loops_config::list_loops(&state.loops_config_path(), Some(&state.loops_template_path())).ok()
}

fn get_loop(state: &UiState, name: &str) -> Option<Value> {
    loops_config::get_loop(name, &state.loops_config_path(), Some(&state.loops_template_path())).ok()
}

fn field_str(v: &Value, key: &str) -> String {
    v.get(key).map(py_str).unwrap_or_default()
}

fn truthy_key(v: &Value, key: &str) -> bool {
    v.get(key).is_some_and(py_truthy)
}

/// `_plugin_settings_fields`: only `bin.loop_plugins.<module>` entry points count.
fn plugin_settings_fields(entry_point: Option<&Value>) -> &'static [Field] {
    use loopx_core::loop_plugins as lp;
    let Some(Value::String(ep)) = entry_point else {
        return &[];
    };
    let Some(module) = ep.strip_prefix("bin.loop_plugins.") else {
        return &[];
    };
    if module.is_empty() || !module.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
        return &[];
    }
    match module {
        "calendar_prep" => &lp::calendar_prep::SETTINGS_FIELDS,
        "linkedin_autopilot" => &lp::linkedin_autopilot::SETTINGS_FIELDS,
        "release_notes" => &lp::release_notes::SETTINGS_FIELDS,
        "rss_watch" => &lp::rss_watch::SETTINGS_FIELDS,
        "stale_sweeper" => &lp::stale_sweeper::SETTINGS_FIELDS,
        _ => &[],
    }
}

fn loop_settings_fields(loop_: Option<&Value>) -> &'static [Field] {
    plugin_settings_fields(loop_.and_then(|l| l.get("entry_point")))
}

fn generic_page(name: &str, loop_: &Value) -> Hub {
    hub::generic_loop_page(name, Some(loop_), !loop_settings_fields(Some(loop_)).is_empty())
}

fn output_dir(state: &UiState, name: &str) -> PathBuf {
    state.repo_root.join("outputs").join("loops").join(name)
}

fn read_json_obj(path: &Path) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(&std::fs::read_to_string(path).ok()?).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

fn read_status(state: &UiState, name: &str) -> Value {
    loopx_core::status::read_status(&state.status_path_for_loop(name))
}

fn local_fmt(dt: &DateTime<FixedOffset>, fmt: &str) -> String {
    dt.with_timezone(&Local).format(fmt).to_string()
}

fn local_today(state: &UiState) -> NaiveDate {
    state.now().with_timezone(&Local).date_naive()
}

fn gs(v: &Value, key: &str, default: &str) -> String {
    v.get(key).map_or_else(|| default.to_string(), py_str)
}

fn obj_list(v: Option<&Value>) -> Vec<&Value> {
    match v {
        Some(Value::Array(a)) => a.iter().collect(),
        _ => Vec::new(),
    }
}

fn is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
        None => String::new(),
    }
}

// ---- the Live view -----------------------------------------------------------------------

fn status_card(ctx: &UiCtx, name: &str, status: &Value, last: Option<&Map<String, Value>>) -> String {
    let safe_name = h(name);
    let quoted = pyfmt::quote(name, "/");
    let running = status.get("state").and_then(Value::as_str) == Some("running");
    let run_now = if running {
        String::new()
    } else {
        run_now_action_html(ctx, &format!("/loops/{safe_name}/run-now"), "", &ctx.csrf_input(), None)
    };
    let head = format!(
        "<div class='status-head'><div class='section-header'><h2>{}</h2>{}</div>{run_now}</div>",
        h(&ctx.t("Status")),
        status_badge_markup(ctx.lang, status)
    );
    let body = match last {
        None => format!("<p class='section-subtitle'>{}</p>", h(&ctx.t("No runs recorded yet."))),
        Some(last) => {
            let empty = Map::new();
            let counts = match last.get("counts") {
                Some(Value::Object(m)) => m,
                _ => &empty,
            };
            let finished_raw = last.get("finished_at").map(py_str).unwrap_or_default();
            let finished = aware(&finished_raw);
            let when = match &finished {
                Some(f) => local_fmt(f, "%Y-%m-%d %H:%M"),
                None => finished_raw.clone(),
            };
            let ago = finished
                .as_ref()
                .map(|f| relative_time(ctx.state.now(), &f.to_rfc3339()))
                .unwrap_or_default();
            let mut tiles: Vec<(String, String, String, String)> =
                vec![(String::new(), ctx.t("Last run"), when, ago)];
            for (key, label) in [("done", "Done"), ("skipped", "Skipped"), ("failed", "Failed")] {
                let value = counts.get(key).map_or_else(|| "0".to_string(), py_str);
                tiles.push((key.to_string(), ctx.t(label), value, String::new()));
            }
            let inner: String = tiles
                .iter()
                .map(|(kind, label, value, sub)| {
                    format!(
                        "<div class='stat-tile{}'><span class='stat-label'>{}</span><span class='stat-value'>{}</span>{}</div>",
                        if kind.is_empty() { String::new() } else { format!(" stat-{kind}") },
                        h(label),
                        h(value),
                        if sub.is_empty() { String::new() } else { format!("<span class='stat-sub'>{}</span>", h(sub)) }
                    )
                })
                .collect();
            format!("<div class='stat-tiles'>{inner}</div>")
        }
    };
    format!(
        "<section class='card'>{head}{body}<p class='status-link'><a href='/loops/{quoted}?view=history'>{}</a></p></section>",
        h(&ctx.t("View run history"))
    )
}

fn latest_results_card(ctx: &UiCtx, last: Option<&Map<String, Value>>) -> String {
    let outcomes: Vec<&Value> = last
        .map(|l| obj_list(l.get("outcomes")).into_iter().filter(|o| o.is_object()).collect())
        .unwrap_or_default();
    let head = format!("<div class='section-header'><h2>{}</h2></div>", h(&ctx.t("Latest results")));
    if outcomes.is_empty() {
        return format!(
            "<section class='card'>{head}<p class='section-subtitle'>{}</p></section>",
            h(&ctx.t("Nothing saved yet. Results appear here after the next run."))
        );
    }
    let mut rows = String::new();
    for o in outcomes.iter().take(3) {
        let status = field_str(o, "status");
        let url = if truthy_key(o, "url") { field_str(o, "url") } else { String::new() };
        let label = if truthy_key(o, "label") { field_str(o, "label") } else { gs(o, "item_key", "") };
        let mut title = h(&label);
        if is_http(&url) {
            title = format!("<a href='{}' target='_blank' rel='noopener'>{title}</a>", h(&url));
        }
        let summary = if truthy_key(o, "summary") {
            format!("<p class='result-summary'>{}</p>", h(&field_str(o, "summary")))
        } else {
            String::new()
        };
        let kind = if matches!(status.as_str(), "done" | "skipped" | "failed") { status.as_str() } else { "skipped" };
        let shown = if status.is_empty() { String::new() } else { ctx.t(&capitalize(&status)) };
        rows.push_str(&format!(
            "<li class='result-row'><div class='result-head'><span class='result-title'>{title}</span>\
<span class='pill history-pill history-{kind}'>{}</span></div>{summary}</li>",
            h(&shown)
        ));
    }
    format!("<section class='card'>{head}<ul class='result-list'>{rows}</ul></section>")
}

fn duration_label(ctx: &UiCtx, minutes: i64) -> String {
    let minutes = minutes.max(0);
    if minutes < 60 {
        return ctx.tf("{minutes} min", &[("minutes", &minutes.to_string())]);
    }
    ctx.tf(
        "{hours} h {minutes} min",
        &[("hours", &(minutes / 60).to_string()), ("minutes", &(minutes % 60).to_string())],
    )
}

fn meetings_today_card(ctx: &UiCtx, name: &str) -> String {
    let now = ctx.state.now();
    let dir = output_dir(ctx.state, name);
    let heading = ctx.t("Today's meetings");
    let head = format!("<div class='section-header'><h2>{}</h2></div>", h(&heading));
    let today = local_today(ctx.state).format("%Y-%m-%d").to_string();
    let data = read_json_obj(&dir.join("today.json"));
    let mut meetings: Vec<(&Value, DateTime<FixedOffset>)> = Vec::new();
    let mut updated: Option<DateTime<FixedOffset>> = None;
    if let Some(d) = &data {
        if d.get("date").and_then(Value::as_str) == Some(today.as_str()) {
            for m in obj_list(d.get("meetings")) {
                if m.is_object() {
                    if let Some(s) = aware(&gs(m, "start", "None")) {
                        meetings.push((m, s));
                    }
                }
            }
            meetings.sort_by_key(|(_, s)| *s);
            updated = d.get("generated_at").and_then(|g| aware(&py_str(g)));
        }
    }
    if meetings.is_empty() {
        return format!(
            "<section class='card'>{head}<p>{}</p></section>",
            h(&ctx.t("No meetings recorded for today yet."))
        );
    }
    let briefs = read_json_obj(&dir.join("briefs.json")).unwrap_or_default();
    let mut rows = String::new();
    for (m, start) in &meetings {
        let end = m.get("end").and_then(|e| aware(&py_str(e)));
        let end_or_start = end.unwrap_or(*start);
        let state = if now >= end_or_start.with_timezone(&Utc) {
            "past"
        } else if now >= start.with_timezone(&Utc) {
            "ongoing"
        } else {
            "upcoming"
        };
        let span = format!(
            "{}{}",
            local_fmt(start, "%H:%M"),
            end.map(|e| format!("\u{2013}{}", local_fmt(&e, "%H:%M"))).unwrap_or_default()
        );
        let when = if state == "upcoming" {
            let mins = ((start.with_timezone(&Utc) - now).num_milliseconds() as f64 / 60000.0).round_ties_even() as i64;
            ctx.tf("in {duration}", &[("duration", &duration_label(ctx, mins))])
        } else if state == "ongoing" && end.is_some() {
            let mins = ((end_or_start.with_timezone(&Utc) - now).num_milliseconds() as f64 / 60000.0).round_ties_even() as i64;
            ctx.tf("ends in {duration}", &[("duration", &duration_label(ctx, mins))])
        } else {
            String::new()
        };
        let mut title = h(&if truthy_key(m, "title") { field_str(m, "title") } else { ctx.t("(no title)") });
        let join = if truthy_key(m, "join_url") { field_str(m, "join_url") } else { String::new() };
        let href = if join.is_empty() {
            if truthy_key(m, "html_link") { field_str(m, "html_link") } else { String::new() }
        } else {
            join.clone()
        };
        if is_http(&href) {
            let icon = if join.is_empty() {
                ""
            } else {
                "<span class='material-symbols-outlined' aria-hidden='true'>videocam</span>"
            };
            title = format!("<a href='{}' target='_blank' rel='noopener'>{icon}{title}</a>", h(&href));
        }
        let key = if truthy_key(m, "key") { field_str(m, "key") } else { String::new() };
        let brief = briefs.get(&key).filter(|b| b.is_object());
        let brief_present = brief.is_some_and(py_truthy);
        let (brief_state, brief_label) = if brief_present {
            ("ready", ctx.t("Brief ready"))
        } else if state == "upcoming" {
            ("pending", ctx.t("Brief pending"))
        } else {
            ("none", ctx.t("No brief"))
        };
        let summary = match brief {
            Some(b) if brief_present && truthy_key(b, "summary") => {
                format!("<p class='meeting-summary'>{}</p>", h(&field_str(b, "summary")))
            }
            _ => String::new(),
        };
        let label = ctx.t(&capitalize(state));
        rows.push_str(&format!(
            "<li class='meeting-row meeting-{state}'><span class='meeting-time'>{}</span>\
<span class='meeting-title'>{title}</span><span class='meeting-when'>{}</span>\
<span class='meeting-pills'><span class='pill meeting-brief meeting-brief-{brief_state}'>{}</span>\
<span class='pill meeting-pill'>{}</span></span>{summary}</li>",
            h(&span),
            h(&when),
            h(&brief_label),
            h(&label)
        ));
    }
    let note = match updated {
        Some(u) => format!(
            "<p class='section-subtitle'>{}</p>",
            h(&ctx.tf("Updated {when}", &[("when", &relative_time(now, &u.to_rfc3339()))]))
        ),
        None => String::new(),
    };
    format!("<section class='card'>{head}{note}<ul class='meeting-list'>{rows}</ul></section>")
}

fn live_body(ctx: &UiCtx, name: &str, flash: Option<&str>, ok: bool) -> String {
    let status = read_status(ctx.state, name);
    let last = read_json_obj(&output_dir(ctx.state, name).join("last-run.json"));
    let mut out = flash_html(flash, ok);
    out.push_str(&status_card(ctx, name, &status, last.as_ref()));
    if name == "meeting-prep-loop" {
        out.push_str(&meetings_today_card(ctx, name));
    } else {
        out.push_str(&latest_results_card(ctx, last.as_ref()));
    }
    out
}

// ---- Content -------------------------------------------------------------------------------

type Entry = (String, String);

fn content_list(heading: &str, entries: &[Entry]) -> String {
    let items: String = entries
        .iter()
        .map(|(text, url)| {
            let mut t = h(text);
            if is_http(url) {
                t = format!("<a href='{}' target='_blank' rel='noopener'>{t}</a>", h(url));
            }
            format!("<li>{t}</li>")
        })
        .collect();
    if items.is_empty() {
        return String::new();
    }
    format!("<h3 class='content-heading'>{}</h3><ul class='content-list'>{items}</ul>", h(heading))
}

fn plain(items: Option<&Value>) -> Vec<Entry> {
    obj_list(items).into_iter().map(|v| (py_str(v), String::new())).collect()
}

fn linked(items: Option<&Value>, text_key: &str, url_key: &str) -> Vec<Entry> {
    obj_list(items)
        .into_iter()
        .filter(|o| o.is_object())
        .map(|o| (gs(o, text_key, ""), gs(o, url_key, "")))
        .collect()
}

fn content_record_html(ctx: &UiCtx, record: &Value) -> String {
    let empty = Value::Object(Map::new());
    let data = match record.get("data") {
        Some(d) if d.is_object() => d,
        _ => &empty,
    };
    let brief = data.get("brief").filter(|b| b.is_object());
    let label = if truthy_key(record, "label") { field_str(record, "label") } else { gs(record, "item_key", "") };
    let mut title = h(&label);
    let url = if truthy_key(record, "url") { field_str(record, "url") } else { String::new() };
    if is_http(&url) {
        title = format!("<a href='{}' target='_blank' rel='noopener'>{title}</a>", h(&url));
    }
    let saved = record.get("saved_at").and_then(|s| aware(&py_str(s)));
    let mut when = saved.map(|s| local_fmt(&s, "%Y-%m-%d %H:%M")).unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    let text_p = |s: String| format!("<p class='content-text'>{}</p>", h(&s));
    if let Some(brief) = brief {
        let ev = data.get("event").filter(|e| e.is_object());
        if let Some(start) = ev.and_then(|e| e.get("start")).and_then(|s| aware(&py_str(s))) {
            when = local_fmt(&start, "%Y-%m-%d %H:%M");
        }
        if truthy_key(brief, "summary") {
            parts.push(text_p(field_str(brief, "summary")));
        }
        parts.push(content_list(&ctx.t("Agenda"), &plain(brief.get("agenda"))));
        parts.push(content_list(&ctx.t("Open items"), &linked(brief.get("open_items"), "text", "link")));
        parts.push(content_list(&ctx.t("Raise"), &plain(brief.get("talking_points"))));
        parts.push(content_list(&ctx.t("From last time"), &plain(brief.get("follow_ups"))));
    } else if matches!(data.get("needs_you"), Some(Value::Array(_))) {
        if truthy_key(data, "summary") {
            parts.push(text_p(field_str(data, "summary")));
        }
        for (heading, key) in [
            ("Needs you", "needs_you"),
            ("Waiting on others", "waiting_on_others"),
            ("Today's meetings", "meetings"),
            ("What Loop X did", "loop_x_did"),
            ("FYI", "fyi"),
        ] {
            parts.push(content_list(&ctx.t(heading), &linked(data.get(key), "text", "url")));
        }
    } else if matches!(data.get("worth_reading"), Some(Value::Array(_))) {
        if truthy_key(data, "summary") {
            parts.push(text_p(field_str(data, "summary")));
        }
        if let Some(counts) = data.get("counts").filter(|c| c.as_object().is_some_and(|m| !m.is_empty())) {
            let c = |k: &str| gs(counts, k, "0");
            parts.push(format!(
                "<p class='section-subtitle'>{}</p>",
                h(&ctx.tf(
                    "{notifications} notifications, {messages} messages, {feed} feed posts",
                    &[("notifications", &c("notifications")), ("messages", &c("messages")), ("feed", &c("feed"))]
                ))
            ));
        }
        parts.push(content_list(&ctx.t("Topics"), &plain(data.get("topics"))));
        let worth: Vec<Entry> = obj_list(data.get("worth_reading"))
            .into_iter()
            .filter(|w| w.is_object())
            .map(|w| (format!("{} - {}", gs(w, "title", ""), gs(w, "why", "")), gs(w, "url", "")))
            .collect();
        parts.push(content_list(&ctx.t("Worth reading"), &worth));
        let people: Vec<Entry> = obj_list(data.get("people"))
            .into_iter()
            .filter(|w| w.is_object())
            .map(|w| (format!("{} - {}", gs(w, "title", ""), gs(w, "why", "")), gs(w, "profile", "")))
            .collect();
        parts.push(content_list(&ctx.t("People to know"), &people));
    } else if matches!(data.get("highlights"), Some(Value::Array(_))) {
        let hl: Vec<Entry> = obj_list(data.get("highlights"))
            .into_iter()
            .filter(|w| w.is_object())
            .map(|w| {
                (
                    format!("{} - {} ({}/5)", gs(w, "title", ""), gs(w, "why", ""), gs(w, "score", "0")),
                    gs(w, "link", ""),
                )
            })
            .collect();
        parts.push(content_list(&ctx.t("Highlights"), &hl));
    } else if truthy_key(record, "summary") {
        parts.push(text_p(field_str(record, "summary")));
    }
    format!(
        "<section class='card'><div class='section-header'><h2>{title}</h2></div>\
<p class='section-subtitle'>{}</p>{}</section>",
        h(&when),
        parts.join("")
    )
}

fn content_body(ctx: &UiCtx, name: &str, flash: Option<&str>, ok: bool) -> String {
    let records = loopx_core::content_archive::load(&output_dir(ctx.state, name).join("content.json"));
    let inner = if records.is_empty() {
        format!(
            "<section class='card'><div class='section-header'><h2>{}</h2></div>\
<p class='section-subtitle'>{}</p></section>",
            h(&ctx.t("Content")),
            h(&ctx.t("Nothing saved yet. Results appear here after the next run."))
        )
    } else {
        records.iter().map(|r| content_record_html(ctx, r)).collect()
    };
    format!("{}{inner}", flash_html(flash, ok))
}

// ---- People (LinkedIn) -----------------------------------------------------------------------

fn people_body(ctx: &UiCtx, name: &str, flash: Option<&str>, ok: bool) -> String {
    let people: Vec<Value> = std::fs::read_to_string(output_dir(ctx.state, name).join("people.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Array(a) => Some(
                a.into_iter()
                    .filter(|r| r.is_object() && truthy_key(r, "url"))
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .unwrap_or_default();
    let head = format!("<div class='section-header'><h2>{}</h2></div>", h(&ctx.t("People")));
    let inner = if people.is_empty() {
        format!(
            "<p class='section-subtitle'>{}</p>",
            h(&ctx.t("No people collected yet. They appear here after the next run."))
        )
    } else {
        let mut cards = String::new();
        for p in &people {
            let person = if truthy_key(p, "name") { field_str(p, "name") } else { String::new() };
            let url = if truthy_key(p, "url") { field_str(p, "url") } else { String::new() };
            let avatar = if truthy_key(p, "avatar") { field_str(p, "avatar") } else { String::new() };
            let pic = if avatar.starts_with("https://") {
                format!(
                    "<img class='person-avatar' src='{}' alt='' loading='lazy' referrerpolicy='no-referrer'>",
                    h(&avatar)
                )
            } else {
                format!(
                    "<span class='person-avatar' aria-hidden='true'>{}</span>",
                    h(&person.chars().take(1).collect::<String>().to_uppercase())
                )
            };
            let mut label = h(&person);
            if url.starts_with("https://") {
                label = format!("<a href='{}' target='_blank' rel='noopener'>{label}</a>", h(&url));
            }
            let meta = ["position", "company"]
                .iter()
                .filter(|k| truthy_key(p, k))
                .map(|k| h(&field_str(p, k)))
                .collect::<Vec<_>>()
                .join(" · ");
            let seen = if truthy_key(p, "first_seen") { field_str(p, "first_seen") } else { String::new() };
            let seen: String = seen.chars().take(10).collect();
            cards.push_str(&format!(
                "<li class='person'>{pic}<div class='person-body'>{label}<span class='person-meta'>{meta}</span>\
<span class='person-meta'>{}</span></div></li>",
                h(&seen)
            ));
        }
        format!(
            "<p class='section-subtitle'>{}</p><ul class='people-list'>{cards}</ul>",
            h(&ctx.tf("{count} people", &[("count", &people.len().to_string())]))
        )
    };
    format!("{}<section class='card'>{head}{inner}</section>", flash_html(flash, ok))
}

// ---- History -----------------------------------------------------------------------------------

/// `^\d{4}-\d{2}-\d{2}_\d{6}\.md$`
pub fn valid_history_name(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 20
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b'_',
            16 => *c == b'.',
            17 => *c == b'm',
            18 => *c == b'd',
            _ => c.is_ascii_digit(),
        })
        && s.ends_with(".md")
}

fn history_files(state: &UiState, name: &str) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(output_dir(state, name).join("history")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| valid_history_name(n))
        .collect();
    names.sort();
    names.reverse();
    names
}

struct Run {
    file: String,
    ran: DateTime<Local>,
    counts: [usize; 3],
    items: Vec<(String, String)>,
}

fn history_run(state: &UiState, name: &str, filename: &str) -> Option<Run> {
    let stem = &filename[..filename.len() - 3];
    let ran = NaiveDateTime::parse_from_str(stem, "%Y-%m-%d_%H%M%S").ok()?;
    let ran = Utc.from_utc_datetime(&ran).with_timezone(&Local);
    let text = std::fs::read(output_dir(state, name).join("history").join(filename))
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let text: String = text.chars().take(20000).collect();
    let mut counts = [0usize; 3];
    let mut items = Vec::new();
    for line in pyfmt::splitlines(&text) {
        for (i, status) in ["done", "skipped", "failed"].iter().enumerate() {
            if let Some(rest) = line.strip_prefix(&format!("- [{status}] ")) {
                counts[i] += 1;
                items.push((status.to_string(), rest.to_string()));
            }
        }
    }
    Some(Run { file: filename.to_string(), ran, counts, items })
}

/// `^(.*) \((https?://[^\s()]+)\)$`
fn split_link(text: &str) -> Option<(&str, &str)> {
    let inner = text.strip_suffix(')')?;
    let pos = inner.rfind(" (")?;
    let url = &inner[pos + 2..];
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    if rest.is_empty() || url.chars().any(|c| c.is_whitespace() || c == '(' || c == ')') {
        return None;
    }
    Some((&inner[..pos], url))
}

fn day_heading(ctx: &UiCtx, day: NaiveDate, today: NaiveDate) -> String {
    let prefix = if day == today {
        ctx.t("Today")
    } else if day == today - Duration::days(1) {
        ctx.t("Yesterday")
    } else {
        day.format("%a").to_string()
    };
    format!("{prefix} \u{b7} {}", day.format("%Y-%m-%d"))
}

fn history_run_html(ctx: &UiCtx, name: &str, run: &Run) -> String {
    let quoted = pyfmt::quote(name, "/");
    let mut pills = String::new();
    for (i, (key, label)) in [("done", "Done"), ("skipped", "Skipped"), ("failed", "Failed")].iter().enumerate() {
        if run.counts[i] > 0 {
            pills.push_str(&format!(
                "<span class='pill history-pill history-{key}'>{} {}</span>",
                h(&ctx.t(label)),
                run.counts[i]
            ));
        }
    }
    let mut items = String::new();
    for (status, text) in run.items.iter().take(4) {
        let shown = match split_link(text) {
            Some((t, url)) => format!("<a href='{}' target='_blank' rel='noopener'>{}</a>", h(url), h(t)),
            None => h(text),
        };
        items.push_str(&format!("<li class='history-item history-item-{status}'>{shown}</li>"));
    }
    if run.items.len() > 4 {
        items.push_str(&format!(
            "<li class='history-more'>{}</li>",
            h(&ctx.tf("+{count} more", &[("count", &(run.items.len() - 4).to_string())]))
        ));
    }
    let body = if items.is_empty() { String::new() } else { format!("<ul class='history-items'>{items}</ul>") };
    let f = h(&run.file);
    format!(
        "<li class='history-run'><div class='history-run-head'>\
<a class='history-time' href='/loops/{quoted}/history/{f}'>{}<span class='history-file'> {}</span></a>{pills}</div>{body}</li>",
        run.ran.format("%H:%M"),
        &f[..f.len() - 3]
    )
}

fn history_body(ctx: &UiCtx, name: &str, flash: Option<&str>, ok: bool) -> String {
    let mut runs: Vec<Run> = history_files(ctx.state, name)
        .iter()
        .filter_map(|f| history_run(ctx.state, name, f))
        .collect();
    let inner = if runs.is_empty() {
        format!("<p>{}</p>", h(&ctx.t("No history yet.")))
    } else {
        runs.sort_by(|a, b| b.ran.cmp(&a.ran));
        let today = local_today(ctx.state);
        let mut days: Vec<(NaiveDate, Vec<&Run>)> = Vec::new();
        for r in &runs {
            let d = r.ran.date_naive();
            match days.iter_mut().find(|(day, _)| *day == d) {
                Some((_, v)) => v.push(r),
                None => days.push((d, vec![r])),
            }
        }
        let mut groups = String::new();
        for (i, (day, day_runs)) in days.iter().enumerate() {
            let total = |k: usize| day_runs.iter().map(|r| r.counts[k]).sum::<usize>();
            let mut summary = vec![h(&ctx.tf("{count} runs", &[("count", &day_runs.len().to_string())]))];
            for (k, label) in [(0usize, "Done"), (2usize, "Failed")] {
                if total(k) > 0 {
                    summary.push(format!("{} {}", h(&ctx.t(label)), total(k)));
                }
            }
            groups.push_str(&format!(
                "<details class='day-group'{}><summary><span class='day-title'>{}</span>\
<span class='day-summary'>{}</span></summary><ul class='history-runs'>{}</ul></details>",
                if i == 0 { " open" } else { "" },
                h(&day_heading(ctx, *day, today)),
                summary.join(" \u{b7} "),
                day_runs.iter().map(|r| history_run_html(ctx, name, r)).collect::<String>()
            ));
        }
        groups
    };
    format!(
        "{}<section class='card'><div class='section-header'><h2>{}</h2></div>{inner}</section>",
        flash_html(flash, ok),
        h(&ctx.t("History"))
    )
}

// ---- Settings ----------------------------------------------------------------------------------

fn settings_body(ctx: &UiCtx, name: &str, flash: Option<&str>, ok: bool) -> String {
    let loop_ = get_loop(ctx.state, name);
    let fields = loop_settings_fields(loop_.as_ref());
    let current = loop_
        .as_ref()
        .and_then(|l| l.get("settings"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let csrf = ctx.csrf_input();
    let mut rows = String::new();
    for field in fields {
        let value = match current.get(field.key) {
            Some(Value::String(s)) => s.clone(),
            None => field.default.to_string(),
            Some(_) => String::new(),
        };
        let field_id = format!(
            "ls-{}",
            field
                .key
                .to_lowercase()
                .chars()
                .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' { c } else { '-' })
                .collect::<String>()
        );
        let help = if field.help.is_empty() { String::new() } else { ctx.t(field.help) };
        let placeholder = if field.placeholder.contains(' ') { ctx.t(field.placeholder) } else { field.placeholder.to_string() };
        let ph = if placeholder.is_empty() { String::new() } else { format!(" placeholder='{}'", h(&placeholder)) };
        let described = if help.is_empty() { String::new() } else { format!(" aria-describedby='{field_id}-help'") };
        let req = if field.required { " required" } else { "" };
        let attrs = format!("name='{}' id='{field_id}'{described}{req}", h(field.key));
        let wide = field.kind == "textarea";
        let control = if wide {
            format!("<textarea {attrs} rows='4' maxlength='1000'{ph}>{}</textarea>", h(&value))
        } else {
            format!("<input type='text' {attrs} value='{}' maxlength='1000'{ph}>", h(&value))
        };
        let marker = if field.required {
            " <span class='field-required' aria-hidden='true'>*</span>".to_string()
        } else {
            format!(" <span class='field-optional'>{}</span>", h(&ctx.t("(optional)")))
        };
        let help_html = if help.is_empty() {
            String::new()
        } else {
            format!("<span class='field-help section-subtitle' id='{field_id}-help'>{}</span>", h(&help))
        };
        let cls = if wide { "connector-field connector-field-wide" } else { "connector-field" };
        rows.push_str(&format!(
            "<div class='{cls}'><label for='{field_id}'>{}{marker}</label>{control}{help_html}</div>",
            h(&ctx.t(field.label))
        ));
    }
    let safe_name = h(&pyfmt::quote(name, ""));
    let form = if rows.is_empty() {
        format!("<p>{}</p>", h(&ctx.t("This loop has no settings.")))
    } else {
        format!(
            "<form method='post' action='/loops/{safe_name}/settings' class='stack-form'>{csrf}\
<div class='connector-fields'>{rows}</div>\
<button type='submit' class='btn btn-neutral'>{}</button></form>",
            h(&ctx.t("Save"))
        )
    };
    format!(
        "{}<section class='card'><div class='section-header'><h2>{}</h2></div>{form}</section>",
        flash_html(flash, ok),
        h(&ctx.t("Settings"))
    )
}

// ---- /loops/<name> --------------------------------------------------------------------------------

fn loop_page(ctx: &UiCtx) -> UiResponse {
    let name = ctx.param("name");
    if is_builtin(name) {
        // Rendered by the built-in loop pages (their own literal routes win); only
        // reachable when none is registered.
        return UiResponse::not_found(ctx.lang);
    }
    let Some(loop_) = get_loop(ctx.state, name) else {
        return UiResponse::not_found(ctx.lang);
    };
    let page = generic_page(name, &loop_);
    let q = ctx.query();
    let (flash, ok) = ctx.flash();
    let view = q.first("view");
    let current = hub::resolve_view(&page, view).key.clone();
    let f = flash.as_deref();
    let body = match current.as_str() {
        "content" => content_body(ctx, name, f, ok),
        "history" => history_body(ctx, name, f, ok),
        "people" => people_body(ctx, name, f, ok),
        "settings" => settings_body(ctx, name, f, ok),
        _ => live_body(ctx, name, f, ok),
    };
    let badge = status_badge_markup(ctx.lang, &read_status(ctx.state, name));
    UiResponse::html(render_loop_page(ctx, &page, name, view, &body, Some(&badge)))
}

fn history_file(ctx: &UiCtx) -> UiResponse {
    let (name, fname) = (ctx.param("name"), ctx.param("file"));
    if is_builtin(name) || !valid_history_name(fname) || get_loop(ctx.state, name).is_none() {
        return UiResponse::not_found(ctx.lang);
    }
    let Ok(dir) = output_dir(ctx.state, name).join("history").canonicalize() else {
        return UiResponse::not_found(ctx.lang);
    };
    let Ok(target) = dir.join(fname).canonicalize() else {
        return UiResponse::not_found(ctx.lang);
    };
    if target.parent() != Some(dir.as_path()) || !target.is_file() {
        return UiResponse::not_found(ctx.lang);
    }
    let text = std::fs::read(&target)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let stem = &fname[..fname.len() - 3];
    let body = format!(
        "<div class='page-title'><h1>{}</h1></div><p><a href='/loops/{}?view=history'>{}</a></p>\
<section class='card'>{}</section>",
        h(stem),
        pyfmt::quote(name, "/"),
        h(&ctx.t("Back to history")),
        render_markdown(&text, &GitlabPrefixes::new())
    );
    let badge = status_badge_markup(ctx.lang, &read_status(ctx.state, name));
    UiResponse::html(render_shell(
        ctx,
        &format!("{stem} · Loop X Engineering"),
        &format!("loop:{name}"),
        &badge,
        &body,
        ShellOpts::default(),
    ))
}

// ---- /loops catalog ---------------------------------------------------------------------------------

fn to_int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::Bool(b) => Some(i64::from(*b)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn schedule_summary(ctx: &UiCtx, loop_: &Value) -> String {
    let Some(schedule) = loop_.get("schedule").and_then(Value::as_object).filter(|s| !s.is_empty()) else {
        return ctx.t("No schedule");
    };
    let frequency = match schedule.get("frequency").and_then(Value::as_str) {
        Some(f @ ("daily" | "weekly" | "monthly" | "hourly")) => f,
        _ => {
            if schedule.get("weekdays").and_then(Value::as_str) == Some("all") {
                "daily"
            } else {
                "weekly"
            }
        }
    };
    let time_value = match (
        to_int(Some(schedule.get("hour").unwrap_or(&json!(9)))),
        to_int(Some(schedule.get("minute").unwrap_or(&json!(0)))),
    ) {
        (Some(hh), Some(mm)) => format!("{hh:02}:{mm:02}"),
        _ => "09:00".to_string(),
    };
    let truthy = |k: &str| schedule.get(k).is_some_and(py_truthy);
    if frequency == "hourly" {
        if truthy("interval_minutes") {
            return ctx.tf("Every {minutes} minutes", &[("minutes", &py_str(&schedule["interval_minutes"]))]);
        }
        let hours = if truthy("interval_hours") { py_str(&schedule["interval_hours"]) } else { "4".to_string() };
        if hours == "1" {
            return ctx.t("Every hour");
        }
        return ctx.tf("Every {hours} hours", &[("hours", &hours)]);
    }
    if frequency == "monthly" {
        let day = if truthy("day") { py_str(&schedule["day"]) } else { "1".to_string() };
        return ctx.tf("Monthly on day {day} at {time}", &[("day", &day), ("time", &time_value)]);
    }
    let daily = || ctx.tf("Daily at {time}", &[("time", &time_value)]);
    let Some(Value::Array(weekdays)) = schedule.get("weekdays") else {
        return daily();
    };
    if frequency == "daily" {
        return daily();
    }
    let mut days: Vec<i64> = Vec::new();
    for d in weekdays {
        match to_int(Some(d)) {
            Some(n) => days.push(n),
            None => return daily(),
        }
    }
    days.sort_unstable();
    days.dedup();
    if days == [1, 2, 3, 4, 5, 6, 7] {
        return daily();
    }
    if days == [1, 2, 3, 4, 5] {
        return ctx.tf("Weekdays at {time}", &[("time", &time_value)]);
    }
    if days.is_empty() {
        return ctx.t("No days selected");
    }
    let names = ["", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let labels = days
        .iter()
        .filter(|d| (1..=7).contains(*d))
        .map(|d| ctx.t(names[*d as usize]))
        .collect::<Vec<_>>()
        .join(", ");
    ctx.tf("{days} at {time}", &[("days", &labels), ("time", &time_value)])
}

fn return_to_input(return_to: &str) -> String {
    if return_to.is_empty() {
        String::new()
    } else {
        format!("<input type='hidden' name='return_to' value='{}'>", h(return_to))
    }
}

fn schedule_form_html(ctx: &UiCtx, loop_: &Value, csrf: &str, return_to: &str) -> String {
    let empty = Map::new();
    let schedule = loop_.get("schedule").and_then(Value::as_object).filter(|s| !s.is_empty()).unwrap_or(&empty);
    let frequency_key = match schedule.get("frequency").and_then(Value::as_str) {
        Some(f @ ("daily" | "weekly" | "monthly" | "hourly")) => f,
        _ => {
            if schedule.get("weekdays").and_then(Value::as_str) == Some("all") {
                "daily"
            } else {
                "weekly"
            }
        }
    };
    let frequency = capitalize(frequency_key);
    let time_value = match (
        to_int(Some(schedule.get("hour").unwrap_or(&json!(9)))),
        to_int(Some(schedule.get("minute").unwrap_or(&json!(0)))),
    ) {
        (Some(hh), Some(mm)) => format!("{hh:02}:{mm:02}"),
        _ => "09:00".to_string(),
    };
    let labels: Vec<(String, String)> = consts::list("dashboard_server", "_LOOP_WEEKDAY_LABELS")
        .iter()
        .map(|p| (p[0].as_str().unwrap_or("").to_string(), p[1].as_str().unwrap_or("").to_string()))
        .collect();
    let selected: Vec<String> = match schedule.get("weekdays") {
        Some(Value::Array(a)) => a.iter().map(py_str).collect(),
        _ => labels.iter().map(|(v, _)| v.clone()).collect(),
    };
    let checkboxes: String = labels
        .iter()
        .map(|(value, label)| {
            format!(
                "<label class='md-checkbox weekday-check'><input type='checkbox' name='weekday' value='{value}'{}> {label}</label>",
                if selected.contains(value) { " checked" } else { "" }
            )
        })
        .collect();
    let day_of_month = if schedule.get("day").is_some_and(py_truthy) { py_str(&schedule["day"]) } else { "1".to_string() };
    let days: Vec<(String, String)> = (1..=31).map(|d| (d.to_string(), d.to_string())).collect();
    let day_select = custom_select("day_of_month", &days, Some(&day_of_month), None);
    let interval_value = if schedule.get("interval_minutes").is_some_and(py_truthy) {
        format!("{}m", py_str(&schedule["interval_minutes"]))
    } else if schedule.get("interval_hours").is_some_and(py_truthy) {
        py_str(&schedule["interval_hours"])
    } else {
        "4".to_string()
    };
    let mut interval_options: Vec<(String, String)> = Vec::new();
    for m in consts::list("dashboard_server", "_LOOP_MINUTE_INTERVAL_CHOICES") {
        let m = m.as_str().unwrap_or("");
        interval_options.push((format!("{m}m"), ctx.tf("{n} minutes", &[("n", m)])));
    }
    for hh in consts::list("dashboard_server", "_LOOP_HOURLY_INTERVAL_CHOICES") {
        let hh = hh.as_str().unwrap_or("");
        interval_options.push((hh.to_string(), ctx.tf("{n} hour(s)", &[("n", hh)])));
    }
    let interval_select = custom_select("interval_hours", &interval_options, Some(&interval_value), None);
    let freqs: Vec<(String, String)> = consts::list("dashboard_server", "_LOOP_SCHEDULE_FREQUENCIES")
        .iter()
        .map(|f| (f.as_str().unwrap_or("").to_string(), f.as_str().unwrap_or("").to_string()))
        .collect();
    let freq_select = custom_select("frequency", &freqs, Some(&frequency), None);
    let style = |show: bool| if show { "" } else { " style='display:none'" };
    let safe_name = h(&loop_.get("name").map_or_else(|| "?".to_string(), py_str));
    format!(
        "<form method='post' action='/daemons/loops/{safe_name}/schedule' class='daemon-action-form schedule-form'>\
{csrf}{}<span class='time-control'{}><input type='time' name='time' value='{time_value}'></span>\
{freq_select}<span class='weekday-checks weekly-controls'{}>{checkboxes}</span>\
<span class='monthly-controls'{}>on day {day_select}</span>\
<span class='hourly-controls'{}>{} {interval_select}</span>\
<button type='submit' class='btn btn-neutral'>Save schedule</button></form>",
        return_to_input(return_to),
        style(frequency != "Hourly"),
        style(frequency == "Weekly"),
        style(frequency == "Monthly"),
        style(frequency == "Hourly"),
        h(&ctx.t("every"))
    )
}

fn loop_action_html(loop_: &Value, csrf: &str, return_to: &str, requirements_met: bool) -> String {
    let name = loop_.get("name").map_or_else(|| "?".to_string(), py_str);
    let safe = h(&name);
    let enabled = loop_.get("enabled").is_none_or(py_truthy);
    if enabled {
        return format!(
            "<form method='post' action='/daemons/loops/{safe}/disable' class='daemon-action-form'>\
{csrf}{}<button type='submit' class='switch is-on' role='switch' aria-checked='true' \
aria-label='Disable {safe}' title='Disable {safe}'><span class='switch-thumb'></span></button></form>",
            return_to_input(return_to)
        );
    }
    format!(
        "<form method='post' action='/daemons/loops/{safe}/enable' class='daemon-action-form'>\
{csrf}{}<button type='submit' class='switch is-off' role='switch' aria-checked='false'{} \
aria-label='Enable {safe}' title='Enable {safe}'><span class='switch-thumb'></span></button></form>",
        return_to_input(return_to),
        if requirements_met { "" } else { " disabled" }
    )
}

fn notify_form_html(ctx: &UiCtx, loop_: &Value, csrf: &str, notify_accounts: &[Account]) -> String {
    let selected_list: Vec<String> = match loop_.get("notify") {
        Some(Value::Array(a)) => a.iter().map(py_str).collect(),
        _ => Vec::new(),
    };
    let name = loop_.get("name").map_or_else(|| "?".to_string(), py_str);
    let safe_name = h(&pyfmt::quote(&name, ""));
    if loop_.get("routes_notifications") != Some(&Value::Bool(true)) || notify_accounts.is_empty() {
        if selected_list.is_empty() {
            return String::new();
        }
        let ids = selected_list.iter().map(|i| h(i)).collect::<Vec<_>>().join(", ");
        return format!(
            "<form method='post' action='/loops/{safe_name}/notify' class='daemon-action-form'>{csrf}\
<span>{}: <code>{ids}</code></span> <button type='submit' class='btn btn-neutral'>{}</button></form>",
            h(&ctx.t("Notify via")),
            h(&ctx.t("Clear"))
        );
    }
    let options: String = notify_accounts
        .iter()
        .map(|a| {
            format!(
                "<option value='{}'{}>{} ({})</option>",
                h(&a.id),
                if selected_list.contains(&a.id) { " selected" } else { "" },
                h(&a.label),
                h(&a.id)
            )
        })
        .collect();
    format!(
        "<form method='post' action='/loops/{safe_name}/notify' class='daemon-action-form'>{csrf}\
<label class='notify-via'>{} <select multiple name='notify' size='{}'>{options}</select></label> \
<button type='submit' class='btn btn-neutral'>{}</button></form>",
        h(&ctx.t("Notify via")),
        notify_accounts.len().clamp(2, 5),
        h(&ctx.t("Save"))
    )
}

struct Accounts(Vec<Account>);

impl Accounts {
    fn with(&self, cap: &str) -> Vec<Account> {
        self.0
            .iter()
            .filter(|a| a.enabled && connectors::get_type(&a.kind).is_some_and(|t| t.has_capability(cap)))
            .cloned()
            .collect()
    }
}

fn requirements_missing(loop_: &Value, accounts: &Accounts) -> Vec<String> {
    let Some(Value::Array(requires)) = loop_.get("requires") else {
        return Vec::new();
    };
    requires
        .iter()
        .filter(|c| accounts.with(&py_str(c)).is_empty())
        .map(py_str)
        .collect()
}

fn type_for_capability(cap: &str) -> Option<&'static str> {
    let mut types: Vec<_> = connectors::all_types().to_vec();
    types.sort_by_key(|t| t.name);
    types.into_iter().find(|t| !t.external && t.has_capability(cap)).map(|t| t.name)
}

fn requirement_chips(ctx: &UiCtx, missing: &[String]) -> String {
    let icon = consts::str("dashboard_server", "_LOOP_NEED_ICON");
    missing
        .iter()
        .map(|cap| {
            let label = consts::value("dashboard_server", "_CAPABILITY_LABELS")
                .and_then(|v| v.get(cap.as_str()))
                .and_then(Value::as_str)
                .unwrap_or(cap);
            let text = h(&ctx.tf("Needs: {capability}", &[("capability", &ctx.t(label))]));
            match type_for_capability(cap) {
                Some(t) => format!(
                    "<a class='chip' href='/connectors?view=add&amp;type={}'>{icon}{text}</a>",
                    h(&pyfmt::quote(t, ""))
                ),
                None => format!("<span class='chip'>{icon}{text}</span>"),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_visible(state: &UiState, loop_: &Value) -> bool {
    if loop_.get("enabled").is_none_or(py_truthy) {
        return true;
    }
    let name = loop_.get("name").map_or_else(|| "?".to_string(), py_str);
    state.status_path_for_loop(&name).exists()
}

struct Parts {
    name: String,
    safe_name: String,
    icon: String,
    title: String,
    description_html: String,
    missing_html: String,
    schedule_html: String,
    settings_html: String,
    toggle_html: String,
    open_html: String,
}

fn catalog_parts(ctx: &UiCtx, loop_: &Value, csrf: &str, notify_accounts: &[Account], accounts: &Accounts) -> Parts {
    let name = loop_.get("name").map_or_else(|| "?".to_string(), py_str);
    let bespoke = hub::builtin_loop_page(&name);
    let page = bespoke.clone().unwrap_or_else(|| generic_page(&name, loop_));
    let label = h(&ctx.t(&match &bespoke {
        Some(b) => b.label.clone(),
        None => {
            if truthy_key(loop_, "label") {
                field_str(loop_, "label")
            } else {
                name.clone()
            }
        }
    }));
    let description = if truthy_key(loop_, "description") { field_str(loop_, "description") } else { String::new() };
    let missing = requirements_missing(loop_, accounts);
    let met = missing.is_empty();
    let href = format!("/loops/{}", pyfmt::quote(&name, "/"));
    let settings = format!(
        "<details class='loop-settings'><summary><span class='material-symbols-outlined' aria-hidden='true'>tune</span>{}</summary>\
<div class='loop-settings-body'>{}{}</div></details>",
        h(&ctx.t("Edit schedule")),
        schedule_form_html(ctx, loop_, csrf, "/loops"),
        notify_form_html(ctx, loop_, csrf, notify_accounts)
    );
    let enabled = loop_.get("enabled").is_none_or(py_truthy);
    let toggle = format!(
        "<div class='loop-toggle'>{}<span class='loop-toggle-text'>{}</span></div>",
        loop_action_html(loop_, csrf, "/loops", met),
        h(&if enabled { ctx.t("On") } else { ctx.t("Off") })
    );
    Parts {
        safe_name: h(&name),
        icon: page.icon.clone(),
        title: format!("<a class='loop-title' href='{}'><span class='loop-label'>{label}</span></a>", h(&href)),
        description_html: if description.is_empty() {
            String::new()
        } else {
            format!("<p class='loop-description'>{}</p>", h(&ctx.t(&description)))
        },
        missing_html: if missing.is_empty() { String::new() } else { requirement_chips(ctx, &missing) },
        schedule_html: format!(
            "<span class='loop-meta-item'><span class='material-symbols-outlined' aria-hidden='true'>schedule</span>{}</span>",
            h(&schedule_summary(ctx, loop_))
        ),
        settings_html: settings,
        toggle_html: toggle,
        open_html: format!("<a class='btn btn-neutral loop-open' href='{}'>{}</a>", h(&href), h(&ctx.t("Open"))),
        name,
    }
}

fn catalog_row(ctx: &UiCtx, loop_: &Value, csrf: &str, notify: &[Account], accounts: &Accounts) -> String {
    let p = catalog_parts(ctx, loop_, csrf, notify, accounts);
    let status = read_status(ctx.state, &p.name);
    let last_run = match status.get("updated_at").filter(|v| py_truthy(v)) {
        Some(u) => format!(
            "<span class='loop-meta-item'><span class='material-symbols-outlined' aria-hidden='true'>history</span>{}</span>",
            h(&ctx.tf("Last run {when}", &[("when", &relative_time(ctx.state.now(), &py_str(u)))]))
        ),
        None => String::new(),
    };
    format!(
        "<li class='loop-row' data-loop='{}'><div class='loop-icon'>{}</div><div class='loop-main'>\
<div class='loop-heading'>{}{}</div>{}<div class='loop-meta'>{}{last_run}{}</div>{}</div>\
<div class='loop-actions'>{}{}</div></li>",
        p.safe_name,
        p.icon,
        p.title,
        p.missing_html,
        p.description_html,
        status_badge_markup(ctx.lang, &status),
        p.schedule_html,
        p.settings_html,
        p.toggle_html,
        p.open_html
    )
}

fn catalog_tile(ctx: &UiCtx, loop_: &Value, csrf: &str, notify: &[Account], accounts: &Accounts) -> String {
    let p = catalog_parts(ctx, loop_, csrf, notify, accounts);
    let needs = if p.missing_html.is_empty() {
        String::new()
    } else {
        format!(
            "<div class='loop-needs'>{}<span class='loop-needs-hint'>{}</span></div>",
            p.missing_html,
            h(&ctx.t("Add the connector to enable this loop."))
        )
    };
    format!(
        "<li class='loop-tile' data-loop='{}'><div class='loop-tile-head'><div class='loop-icon'>{}</div>{}</div>\
{}{needs}<div class='loop-meta'>{}</div>{}<div class='loop-tile-foot'>{}{}</div></li>",
        p.safe_name, p.icon, p.title, p.description_html, p.schedule_html, p.settings_html, p.toggle_html, p.open_html
    )
}

fn catalog_body(ctx: &UiCtx, flash: Option<&str>, ok: bool) -> String {
    let loops: Vec<Value> = list_loops(ctx.state)
        .unwrap_or_default()
        .into_iter()
        .filter(Value::is_object)
        .collect();
    let csrf = ctx.csrf_input();
    let (active, available): (Vec<&Value>, Vec<&Value>) = loops.iter().partition(|l| is_visible(ctx.state, l));
    let accounts = Accounts(cc::list_accounts(&conn::paths(ctx.state)).unwrap_or_default());
    let notify: Vec<Account> = accounts.with("notify").into_iter().filter(|a| cc::is_valid_id(&a.id)).collect();
    let section = |title: &str, hint: &str, rows: &[&Value], render: fn(&UiCtx, &Value, &str, &[Account], &Accounts) -> String, list_class: &str, attrs: &str| {
        let inner = if rows.is_empty() {
            format!("<p class='loop-empty'>{}</p>", h(&ctx.t("No loops here.")))
        } else {
            format!(
                "<ul class='{list_class}'>{}</ul>",
                rows.iter().map(|l| render(ctx, l, &csrf, &notify, &accounts)).collect::<String>()
            )
        };
        format!(
            "<section class='card loop-section'{attrs}><div class='section-header'><h2>{}</h2>\
<span class='badge-count'>{}</span></div><p class='loop-section-hint'>{}</p>{inner}</section>",
            h(&ctx.t(title)),
            rows.len(),
            h(&ctx.t(hint))
        )
    };
    format!(
        "<div class='page-title'><h1>{}</h1></div>{}{}{}",
        h(&ctx.t("Loops")),
        flash_html(flash, ok),
        section(
            "Active loops",
            "Loops that are on or have run. Turn them off, change when they run, or open one for details.",
            &active,
            catalog_row,
            "loop-list",
            ""
        ),
        section(
            "Available loops",
            "Loops you can add. Turn one on to start it on its schedule.",
            &available,
            catalog_tile,
            "loop-gallery",
            " data-section='available'"
        )
    )
}

fn catalog_page(ctx: &UiCtx) -> UiResponse {
    let (flash, ok) = ctx.flash();
    let body = catalog_body(ctx, flash.as_deref(), ok);
    UiResponse::html(render_shell(
        ctx,
        "Loops · Loop X Engineering",
        "loops",
        &default_badge(ctx),
        &body,
        ShellOpts::default(),
    ))
}

// ---- POSTs ---------------------------------------------------------------------------------------------

fn loop_location(name: &str) -> String {
    format!("/loops/{}", pyfmt::quote(name, "/"))
}

fn settings(ctx: &UiCtx) -> UiResponse {
    let name = ctx.param("name").to_string();
    let form = ctx.form();
    let loop_ = list_loops(ctx.state).and_then(|l| {
        l.into_iter()
            .find(|l| l.is_object() && l.get("name").and_then(Value::as_str) == Some(name.as_str()))
    });
    let fields = loop_settings_fields(loop_.as_ref());
    let (ok, message) = if loop_.is_none() || is_builtin(&name) {
        (false, ctx.tf("Unknown loop {name}", &[("name", &name)]))
    } else if fields.is_empty() {
        (false, ctx.tf("{name} has no settings", &[("name", &name)]))
    } else {
        let keys: Vec<&str> = fields.iter().map(|f| f.key).collect();
        let values: Map<String, Value> = keys
            .iter()
            .filter_map(|k| form.first(k).map(|v| (k.to_string(), Value::String(v.to_string()))))
            .collect();
        match loops_config::set_settings(
            &name,
            &Value::Object(values),
            &keys,
            &ctx.state.loops_config_path(),
            Some(&ctx.state.loops_template_path()),
        ) {
            Ok(m) => (true, m),
            Err(m) => (false, m),
        }
    };
    UiResponse::flash_redirect(ok, &message, &format!("{}?view=settings", loop_location(&name)))
}

fn notify(ctx: &UiCtx) -> UiResponse {
    let name = ctx.param("name").to_string();
    let form = ctx.form();
    let ids: Vec<String> = form.all("notify").iter().filter(|i| !i.is_empty()).cloned().collect();
    let loop_ = list_loops(ctx.state).and_then(|l| {
        l.into_iter()
            .find(|l| l.is_object() && l.get("name").and_then(Value::as_str) == Some(name.as_str()))
    });
    let mut allowed: Vec<String> = Vec::new();
    let mut config_error: Option<String> = None;
    if loop_.is_some() && !ids.is_empty() {
        match cc::list_accounts(&conn::paths(ctx.state)) {
            Ok(a) => {
                allowed = Accounts(a)
                    .with("notify")
                    .into_iter()
                    .filter(|a| cc::is_valid_id(&a.id))
                    .map(|a| a.id)
                    .collect()
            }
            Err(e) => config_error = Some(e.0),
        }
    }
    let (ok, message) = match &loop_ {
        None => (false, ctx.tf("Unknown loop {name}", &[("name", &name)])),
        Some(l) if !ids.is_empty() && l.get("routes_notifications") != Some(&Value::Bool(true)) => (
            false,
            ctx.tf("{name} does not route notifications through connectors", &[("name", &name)]),
        ),
        _ if config_error.is_some() => (
            false,
            ctx.tf("Could not read connectors: {detail}", &[("detail", config_error.as_deref().unwrap_or(""))]),
        ),
        _ if ids.iter().any(|i| !allowed.contains(i)) => {
            (false, ctx.t("Choose only enabled notification connectors"))
        }
        _ => match loops_config::set_notify(
            &name,
            &json!(ids),
            &ctx.state.loops_config_path(),
            Some(&ctx.state.loops_template_path()),
        ) {
            Ok(m) => (true, m),
            Err(m) => (false, m),
        },
    };
    UiResponse::flash_redirect(ok, &message, "/loops")
}

#[allow(dead_code)]
fn _lang(_: Lang) {}
#[allow(dead_code)]
fn _wd(d: NaiveDate) -> u32 {
    d.day()
}
