//! The Dashboard hub (`/`): the chat page (view `overview`), the Activity view,
//! their fragments, every `/activity/*` POST, `/chat-docs/<name>` and the chat
//! engine glue (`POST /activity/chat`, `GET /activity/chat-stream`).
//!
//! Python: `_overview_body`, `_activity_body`, `render_activity_messages_fragment`,
//! `render_chat_history_fragment`, `chat_tool_chips_html`, `chat_pending_card_html`
//! and the `/activity/*` branches of `DashboardHandler`. The engine itself
//! (prompts, command, stream parsing, jobs) lives in `loopx_core::chat`.

use crate::ui::badge::{self, default_badge};
use crate::ui::hub;
use crate::ui::i18n::{self, Lang};
use crate::ui::markdown;
use crate::ui::pages::loops_common::{
    aware, flash_html, relative_time, run_now_action_html, stop_action_html,
};
use crate::ui::pyfmt;
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::state::UiState;
use crate::ui::template::{Scope, Template};
use crate::ui::{consts, csrf};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use loopx_core::chat::{self, ChatCommand, ChatJobs, ChatRuntime};
use loopx_core::chat_tools::{self, Env as ToolsEnv};
use loopx_core::messages::MessageStore;
use loopx_core::{pydumps, run_history, status as core_status, topics};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

pub fn register(r: &mut Registry) {
    r.get("/", dashboard);
    r.get("/activity/messages/fragment", messages_fragment_route);
    r.get("/activity/sessions/fragment", sessions_fragment_route);
    r.get("/chat-docs/:name", chat_doc);
    r.post("/activity/messages", post_message);
    r.post("/activity/messages/:timestamp/delete", post_delete_message);
    r.post("/activity/sessions/:id/delete", post_delete_session);
    r.post("/activity/new-chat", post_new_chat);
    r.post("/activity/chat/approve", post_approve);
    r.post("/activity/chat/reject", post_reject);
    r.post("/activity/chat", post_chat);
}

// ----- shared state -----

/// The in-memory reply jobs (`_CHAT_JOBS`), shared by every request.
pub fn jobs() -> &'static ChatJobs {
    static JOBS: OnceLock<ChatJobs> = OnceLock::new();
    JOBS.get_or_init(ChatJobs::new)
}

fn store(state: &UiState) -> MessageStore {
    MessageStore::for_root(&state.repo_root)
}

fn tools_env(state: &UiState) -> ToolsEnv {
    ToolsEnv::new(&state.repo_root, &state.home_dir)
}

fn epoch(state: &UiState) -> f64 {
    state.now().timestamp_micros() as f64 / 1_000_000.0
}

fn t(lang: Lang, text: &str) -> String {
    i18n::t(lang, text, &[])
}

fn esc_t(lang: Lang, text: &str) -> String {
    pyfmt::escape(&t(lang, text))
}

fn csrf_input(state: &UiState) -> String {
    csrf::input_html(&state.csrf_token)
}

fn prefixes(state: &UiState) -> markdown::GitlabPrefixes {
    crate::ui::gitlab::issue_url_prefixes(state)
}

// ----- tool chips and approval cards -----

/// `tool_label`.
pub fn tool_label(lang: Lang, name: &str) -> String {
    let label = match name {
        "gitlab.read_issue" | "github.read_issue" => "Reading issue",
        "gitlab.list_issues" => "Listing issues",
        "gitlab.comment" => "Commenting on issue",
        "github.read_pr" => "Reading pull request",
        "github.comment" => "Commenting",
        "rss.read_feed" => "Reading feed",
        "calendar.list_events" => "Reading calendar",
        "mail.search" => "Searching mail",
        "mail.read" => "Reading email",
        "mail.send" => "Sending email",
        "calendar.create_event" => "Creating event",
        "docs.save" => "Saving document",
        _ => return name.to_string(),
    };
    t(lang, label)
}

/// `chat_pending_card_html`.
pub fn pending_card_html(lang: Lang, pending_id: &str, session: &str, record: &Value) -> String {
    let tool = pyfmt::escape(&text_of(record.get("tool")));
    let summary = match record.get("summary") {
        Some(v) if loopx_core::loops_config::py_truthy(v) => pydumps::py_str(v),
        _ => String::new(),
    };
    let summary_html = if summary.is_empty() {
        String::new()
    } else {
        format!("<div class='pending-card-summary'>{}</div>", pyfmt::escape(&summary))
    };
    let args = pyfmt::escape(
        &serde_json::to_string_pretty(record.get("args").unwrap_or(&json!({})))
            .unwrap_or_default(),
    );
    format!(
        "<div class='pending-card' data-pending-id=\"{}\" data-pending-session=\"{}\">\
         <div class='pending-card-head'><span class='material-symbols-outlined' aria-hidden='true'>schedule</span>\
         <span>{}</span></div>\
         <div class='pending-card-tool'>{tool}</div>{summary_html}\
         <pre class='pending-card-args'>{args}</pre>\
         <div class='pending-card-actions'>\
         <button type='button' class='btn btn-primary' data-pending-approve>\
         <span class='material-symbols-outlined' aria-hidden='true'>check</span>{}</button>\
         <button type='button' class='btn btn-neutral' data-pending-reject>\
         <span class='material-symbols-outlined' aria-hidden='true'>close</span>{}</button>\
         </div>\
         <div class='pending-card-status' aria-live='polite'></div>\
         </div>",
        pyfmt::escape(pending_id),
        pyfmt::escape(session),
        esc_t(lang, "Waiting for your approval"),
        esc_t(lang, "Approve"),
        esc_t(lang, "Reject"),
    )
}

fn text_of(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(v) => pydumps::py_str(v),
    }
}

/// `chat_tool_chips_html`: chips for a stored loop message's `tools` list.
pub fn tool_chips_html(
    state: &UiState,
    lang: Lang,
    tools: Option<&Value>,
    session: Option<&str>,
) -> String {
    let Some(Value::Array(tools)) = tools.filter(|t| t.as_array().is_some_and(|a| !a.is_empty()))
    else {
        return String::new();
    };
    let (mut chips, mut cards) = (String::new(), String::new());
    for tool in tools {
        let Value::Object(tool) = tool else { continue };
        let kind = match tool.get("ok") {
            Some(Value::Bool(true)) => "ok",
            Some(Value::Bool(false)) => "err",
            _ => "run",
        };
        let summary = match tool.get("summary") {
            Some(v) if loopx_core::loops_config::py_truthy(v) => format!(
                " \u{b7} {}",
                pyfmt::escape(&chat::short_tool_ref(&pydumps::py_str(v)))
            ),
            _ => String::new(),
        };
        let icon = match kind {
            "ok" => "check_circle",
            "err" => "error",
            _ => "autorenew",
        };
        chips.push_str(&format!(
            "<span class=\"tool-chip tool-chip-{kind}\">\
             <span class=\"material-symbols-outlined tool-chip-icon\" aria-hidden=\"true\">{icon}</span>\
             <span class=\"tool-chip-text\">{}{summary}</span></span>",
            pyfmt::escape(&tool_label(lang, &text_of(tool.get("name"))))
        ));
        if let (Some(session), Some(Value::Object(stored))) =
            (session.filter(|s| !s.is_empty()), tool.get("pending"))
        {
            if let Some(Value::String(pid)) = stored.get("pending_id") {
                if let Some(record) =
                    chat_tools::pending::lookup(&tools_env(state), pid, session, Some(epoch(state)))
                {
                    cards.push_str(&pending_card_html(lang, pid, session, &record.to_value()));
                }
            }
        }
    }
    if chips.is_empty() {
        return String::new();
    }
    format!("<div class=\"tool-chips\">{chips}</div>{cards}")
}

// ----- messages fragment -----

fn message_date(ts: &str) -> Option<NaiveDate> {
    if ts.is_empty() {
        return None;
    }
    aware(ts).map(|d| d.date_naive())
}

/// `_day_separator_label`.
fn day_separator_label(day: NaiveDate, today: NaiveDate) -> String {
    let delta = (today - day).num_days();
    if delta == 0 {
        "Today".to_string()
    } else if delta == 1 {
        "Yesterday".to_string()
    } else if day.year() == today.year() {
        day.format("%b %-d").to_string()
    } else {
        day.format("%b %-d, %Y").to_string()
    }
}

/// `render_activity_messages_fragment`. `session_id` `None` is the current
/// session; `Some("")` a new chat with no messages yet.
pub fn messages_fragment(state: &UiState, lang: Lang, session_id: Option<&str>) -> String {
    let store = store(state);
    let session = match session_id {
        Some(s) => s.to_string(),
        None => store.current_chat_session_id().unwrap_or_default(),
    };
    let messages = store.chat_session_messages(&session);
    let csrf_input = csrf_input(state);
    let now: DateTime<Utc> = state.now();
    let today = now.date_naive();
    let prefixes = prefixes(state);
    let brand = consts::ds("_MESSAGE_BRAND_ICON");

    let mut rows = String::new();
    let mut last_day: Option<NaiveDate> = None;
    let mut last_sender: Option<bool> = None;
    for m in &messages {
        let is_user = m.get("from").and_then(Value::as_str) == Some("user");
        let text = text_of_default(m.get("text"));
        let timestamp = text_of_default(m.get("timestamp"));
        let relative = relative_time(now, &timestamp);
        let ts_quoted = pyfmt::quote(&timestamp, "");
        let delete_confirm = pyfmt::escape(&t(lang, "Delete this message?"));

        let day = message_date(&timestamp);
        if let Some(d) = day {
            if Some(d) != last_day {
                rows.push_str(&format!(
                    "<li class='message-day-sep'><span>{}</span></li>",
                    pyfmt::escape(&day_separator_label(d, today))
                ));
                last_day = Some(d);
                last_sender = None;
            }
        }
        let mut row_classes = format!(
            "message-row {}",
            if is_user { "message-row-user" } else { "message-row-loop" }
        );
        if last_sender == Some(is_user) {
            row_classes.push_str(" message-row-consecutive");
        }
        last_sender = Some(is_user);

        let who = if is_user {
            format!("<span class='k'>{}</span>", esc_t(lang, "You"))
        } else {
            format!("<span class='k' aria-label='Loop X'>{brand}</span>")
        };
        let edit = if is_user {
            format!(
                "<button type='button' class='message-action-btn' data-edit-message aria-label='{}' title='{}'>\
                 <span class='material-symbols-outlined' aria-hidden='true'>edit</span></button>",
                esc_t(lang, "Edit message"),
                esc_t(lang, "Edit")
            )
        } else {
            String::new()
        };
        let chips = if is_user {
            String::new()
        } else {
            tool_chips_html(state, lang, m.get("tools"), Some(&session))
        };
        rows.push_str(&format!(
            "<li class='{row_classes}'>\
             <div class='message-body'>\
             <div class='message-bubble {}'>\
             <div class='message-meta'>{who}<span class='message-time'>{}</span></div>\
             {chips}\
             <div class='message-text markdown' data-raw=\"{}\">{}</div>\
             </div>\
             <div class='message-actions'>\
             <button type='button' class='message-action-btn' data-copy-message aria-label='{}' title='{}'>\
             <span class='material-symbols-outlined' aria-hidden='true'>content_copy</span></button>\
             {edit}\
             <form method='post' action='/activity/messages/{ts_quoted}/delete' class='message-delete-form'>\
             {csrf_input}\
             <button type='submit' class='message-action-btn' aria-label='{}' title='{}' data-confirm=\"{delete_confirm}\">\
             <span class='material-symbols-outlined' aria-hidden='true'>delete</span></button>\
             </form>\
             </div>\
             </div>\
             </li>",
            if is_user { "message-bubble-user" } else { "message-bubble-loop" },
            pyfmt::escape(&relative),
            pyfmt::escape(&text),
            markdown::render_markdown(&text, &prefixes),
            esc_t(lang, "Copy message"),
            esc_t(lang, "Copy"),
            esc_t(lang, "Delete message"),
            esc_t(lang, "Delete"),
        ));
    }
    if rows.is_empty() {
        format!("<p>{}</p>", esc_t(lang, "(no messages yet)"))
    } else {
        format!("<ul class='message-list'>{rows}</ul>")
    }
}

/// `str(m.get(key, ""))`.
fn text_of_default(v: Option<&Value>) -> String {
    text_of(v)
}

// ----- history drawer -----

fn history_group(last_at: &str, today: NaiveDate) -> &'static str {
    let Some(day) = message_date(last_at) else {
        return "Older";
    };
    let age = (today - day).num_days();
    match age {
        a if a <= 0 => "Today",
        1 => "Yesterday",
        2..=7 => "Previous 7 days",
        8..=30 => "Previous 30 days",
        _ => "Older",
    }
}

/// `render_chat_history_fragment`.
pub fn history_fragment(state: &UiState, lang: Lang, active: Option<&str>) -> String {
    let sessions = store(state).list_chat_sessions();
    if sessions.is_empty() {
        return format!("<p class='chat-history-empty'>{}</p>", esc_t(lang, "No chats yet"));
    }
    let now = state.now();
    let today = now.date_naive();
    let csrf_input = csrf_input(state);
    let viewing_input = format!(
        "<input type='hidden' name='viewing' value='{}'>",
        pyfmt::escape(active.unwrap_or(""))
    );
    let mut parts = String::new();
    for group in ["Today", "Yesterday", "Previous 7 days", "Previous 30 days", "Older"] {
        let members: Vec<_> = sessions
            .iter()
            .filter(|s| history_group(&s.last_at, today) == group)
            .collect();
        if members.is_empty() {
            continue;
        }
        parts.push_str(&format!("<h3 class='chat-history-group'>{}</h3>", esc_t(lang, group)));
        for s in members {
            let is_active = Some(s.id.as_str()) == active;
            let active_cls = if is_active { " is-active" } else { "" };
            let current = if is_active { " aria-current='page'" } else { "" };
            let quoted = pyfmt::quote(&s.id, "");
            let href = format!("/?session={quoted}");
            let confirm = pyfmt::escape(&i18n::t(
                lang,
                "Delete the chat \u{201c}{title}\u{201d}? Its messages will be removed.",
                &[("title", &s.title)],
            ));
            parts.push_str(&format!(
                "<div class='chat-history-row'>\
                 <a class='chat-history-item{active_cls}' href='{}'{current}>\
                 <span class='chat-history-title'>{}</span>\
                 <span class='chat-history-time'>{}</span>\
                 </a>\
                 <form method='post' action='/activity/sessions/{}/delete' class='chat-history-delete-form'>\
                 {csrf_input}{viewing_input}\
                 <button type='submit' class='message-action-btn' aria-label='{}'\
                 \u{20}title='{}' data-confirm=\"{confirm}\">\
                 <span class='material-symbols-outlined' aria-hidden='true'>delete</span></button>\
                 </form>\
                 </div>",
                pyfmt::escape(&href),
                pyfmt::escape(&s.title),
                pyfmt::escape(&relative_time(now, &s.last_at)),
                pyfmt::escape(&quoted),
                pyfmt::escape(&i18n::t(lang, "Delete chat {title}", &[("title", &s.title)])),
                esc_t(lang, "Delete chat"),
            ));
        }
    }
    parts
}

// ----- the overview (chat) body -----

const SUGGESTIONS: [(&str, &str, &str); 4] = [
    ("monitoring", "Loop status", "What is the loop doing right now?"),
    ("history", "Latest run", "Summarize the latest GitLab run review."),
    ("bolt", "Run an issue", "Run this GitLab issue now: "),
    (
        "email",
        "Inbox triage",
        "Summarize my latest inbox triage - anything urgent?",
    ),
];

const LIVE_LOOPS: [(&str, &str, &str); 2] = [
    ("meeting-prep-loop", "Meeting Prep", "calendar_month"),
    ("inbox-triage-loop", "Inbox Triage", "mail"),
];

/// `_overview_live_links_html`.
fn live_links_html(state: &UiState, lang: Lang) -> String {
    let Ok(loops) = loopx_core::loops_config::list_loops(
        &state.loops_config_path(),
        Some(&state.loops_template_path()),
    ) else {
        return String::new();
    };
    let enabled: Vec<&str> = loops
        .iter()
        .filter(|l| l.is_object() && l.get("enabled").is_some_and(loopx_core::loops_config::py_truthy))
        .filter_map(|l| l.get("name").and_then(Value::as_str))
        .collect();
    LIVE_LOOPS
        .iter()
        .filter(|(name, _, _)| enabled.contains(name))
        .map(|(name, label, icon)| {
            format!(
                "<a class='btn btn-neutral chat-link-pill' href='/loops/{name}'>\
                 <span class='material-symbols-outlined' aria-hidden='true'>{icon}</span>\
                 {}</a>",
                pyfmt::escape(&i18n::t(lang, "Live {loop}", &[("loop", &t(lang, label))]))
            )
        })
        .collect()
}

/// `_overview_body`.
fn overview_body(ctx: &UiCtx, flash: Option<&str>, flash_ok: bool, session: Option<&str>) -> String {
    let state = ctx.state;
    let lang = ctx.lang;
    let store = store(state);
    let status = core_status::read_status(&state.status_path());
    let mut viewing = match session {
        Some(s) => {
            if store.chat_session_exists(s) {
                s.to_string()
            } else {
                String::new()
            }
        }
        None => store.current_chat_session_id().unwrap_or_default(),
    };
    let is_empty = store.chat_session_messages(&viewing).is_empty();
    if is_empty {
        viewing = String::new();
    }
    let messages_html = format!(
        "<div id='activity-message-list'>{}</div>",
        messages_fragment(state, lang, Some(&viewing))
    );
    let history_html = history_fragment(state, lang, (!viewing.is_empty()).then_some(viewing.as_str()));
    let csrf = csrf_input(state);
    let session_input = format!(
        "<input type='hidden' name='session' value='{}'>",
        pyfmt::escape(&viewing)
    );

    let mut announce = i18n::t(
        lang,
        "GitLab loop: {state}",
        &[("state", &badge::state_label(lang, &status))],
    );
    if let Some(updated) = status.get("updated_at").filter(|v| loopx_core::loops_config::py_truthy(v)) {
        announce.push_str(" \u{b7} ");
        announce.push_str(&i18n::t(
            lang,
            "updated {when}",
            &[("when", &relative_time(state.now(), &pydumps::py_str(updated)))],
        ));
    }
    let suggestions: String = SUGGESTIONS
        .iter()
        .map(|(icon, label, prompt)| {
            format!(
                "<button type='button' class='btn btn-neutral chat-chip' data-chat-suggestion=\"{}\">\
                 <span class='material-symbols-outlined' aria-hidden='true'>{icon}</span>{}</button>",
                pyfmt::escape(prompt),
                pyfmt::escape(&t(lang, label))
            )
        })
        .collect();
    let hero_title = i18n::t(
        lang,
        "Into the {loop}",
        &[("loop", "<span class='chat-hero-accent'>Loop</span>")],
    );

    let mut scope = Scope::new(lang);
    scope
        .set("' is-empty' if is_empty else ''", if is_empty { " is-empty" } else { "" })
        .var("csrf_input", csrf)
        .var("history_html", history_html)
        .var("flash_html", flash_html(flash, flash_ok))
        .var("announce_text", announce)
        .var("hero_title_html", hero_title)
        .var("messages_html", messages_html)
        .var("session_input", session_input)
        .var("suggestions_html", suggestions)
        .set("_overview_live_links_html()", live_links_html(state, lang));
    Template::named("chat_overview_body")
        .render(&scope)
        .expect("chat overview template")
}

// ----- the activity body -----

/// `str.capitalize()`.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect(),
        None => String::new(),
    }
}

fn read_json(path: &std::path::Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// `get_configured_topics`: `[]` when topics.json is missing or malformed.
fn configured_topics(state: &UiState) -> Vec<Value> {
    topics::load(&state.home_dir.join("topics.json")).unwrap_or_default()
}

/// `_topic_monitor_progress_text`.
fn topic_progress_text(lang: Lang, topic_status: &serde_json::Map<String, Value>, topics: &[Value]) -> String {
    for (name, entry) in topic_status {
        if entry.get("state").and_then(Value::as_str) == Some("running") {
            let label = topics
                .iter()
                .find(|t| t.get("name").and_then(Value::as_str) == Some(name))
                .map_or_else(|| name.clone(), |t| match t.get("label") {
                    Some(l) => pydumps::py_str(l),
                    None => name.clone(),
                });
            let step_label = match entry.get("current_step").filter(|v| loopx_core::loops_config::py_truthy(v)) {
                Some(step) => {
                    let step = pydumps::py_str(step);
                    let mapped = consts::value("dashboard_server", "_STEP_LABELS")
                        .and_then(|m| m.get(&step))
                        .and_then(Value::as_str)
                        .unwrap_or(&step)
                        .to_string();
                    t(lang, &mapped)
                }
                None => t(lang, "Starting up"),
            };
            return format!("{step_label} \u{2014} {label}");
        }
    }
    t(lang, "Idle")
}

/// `_topic_latest_data_html`.
pub fn topic_latest_data_html(state: &UiState, lang: Lang, topics: &[Value]) -> String {
    if topics.is_empty() {
        let link = format!(
            "<a href='/loops/topic-loop?view=topics'>{}</a>",
            esc_t(lang, "Topic Settings")
        );
        return format!(
            "<p>{}</p>",
            i18n::t(lang, "No topics configured yet. Add one on the {link} page.", &[("link", &link)])
        );
    }
    let history_dir = state.repo_root.join("outputs").join("topic-monitor").join("history");
    let prefixes = prefixes(state);
    let expand = consts::ds("_EXPAND_ICON");
    let mut blocks = String::new();
    for topic in topics {
        let name = topic.get("name").map(pydumps::py_str).unwrap_or_default();
        let label = topic.get("label").map_or_else(|| name.clone(), pydumps::py_str);
        let names = run_history::list_topic_history(Some(&name), &history_dir);
        let Some(latest) = names.first() else {
            blocks.push_str(&format!(
                "<div class='topic-latest-item'><h3>{}</h3>\
                 <p class='topic-latest-overview'>{}</p></div>",
                pyfmt::escape(&label),
                esc_t(lang, "(no data yet)")
            ));
            continue;
        };
        let content = run_history::read_history_file(latest, &history_dir).unwrap_or_default();
        let overview = run_history::extract_history_overview(&content, 240);
        let tags: String = run_history::topic_history_tags(latest, &content)
            .iter()
            .map(|tag| format!("<span class='pill pill-grey'>{}</span>", pyfmt::escape(tag)))
            .collect();
        let when = relative_time(state.now(), &format!("{}T00:00:00Z", &latest[..10]));
        blocks.push_str(&format!(
            "<div class='topic-latest-item'>\
             <div class='topic-latest-summary' tabindex='0' role='button' aria-expanded='false' \
             onclick=\"this.classList.toggle('is-expanded'); \
             this.setAttribute('aria-expanded', this.classList.contains('is-expanded'))\" \
             onkeydown=\"if (event.key === 'Enter' || event.key === ' ') {{ \
             event.preventDefault(); this.click(); }}\">\
             <h3>{} {expand}\
             <span class='topic-last-run'>{}</span></h3>\
             <p class='topic-latest-overview'>{}</p>\
             <span class='pill-row'>{tags}</span>\
             </div>\
             <div class='topic-latest-detail'>\
             <div class='markdown'>{}</div>\
             </div>\
             </div>",
            pyfmt::escape(&label),
            pyfmt::escape(&i18n::t(lang, "latest {when}", &[("when", &when)])),
            pyfmt::escape(&overview),
            markdown::render_markdown(&content, &prefixes),
        ));
    }
    blocks
}

/// `_today_log_tail`.
fn today_log_tail(state: &UiState, lines: usize) -> Option<String> {
    let today = match state.fixed_now {
        Some(n) => n.format("%Y-%m-%d").to_string(),
        None => chrono::Local::now().format("%Y-%m-%d").to_string(),
    };
    let path = state.repo_root.join("outputs").join("history").join(format!("{today}.log"));
    let content = std::fs::read(&path).ok()?;
    let content = String::from_utf8_lossy(&content).into_owned();
    let all = pyfmt::splitlines(&content);
    let start = all.len().saturating_sub(lines);
    Some(all[start..].join("\n"))
}

/// `_activity_body`.
fn activity_body(ctx: &UiCtx, flash: Option<&str>, flash_ok: bool) -> String {
    let state = ctx.state;
    let lang = ctx.lang;
    let status = core_status::read_status(&state.status_path());
    let review = std::fs::read(state.repo_root.join("outputs").join("daily-review.md"))
        .map_or_else(|_| "No run yet.".to_string(), |b| String::from_utf8_lossy(&b).into_owned());
    let run_state = status
        .get("state")
        .map_or_else(|| "unknown".to_string(), pydumps::py_str);
    let csrf = csrf_input(state);
    let prefixes = prefixes(state);

    let (badge_class, badge_icon) = badge::status_badge(&run_state);
    let state_hero = format!(
        "<div class='status-hero'><span class='pill pill-lg {badge_class}'>{badge_icon}{}</span></div>",
        pyfmt::escape(&badge::state_label(lang, &status))
    );
    let mut status_lines = String::new();
    if let Value::Object(map) = &status {
        for (key, value) in map {
            if key == "state" {
                continue;
            }
            let label = t(lang, &capitalize(&key.replace('_', " ")));
            let (display, title_attr) = if key == "updated_at" && loopx_core::loops_config::py_truthy(value) {
                (
                    relative_time(state.now(), &pydumps::py_str(value)),
                    format!(" title='{}'", pyfmt::escape(&pydumps::py_str(value))),
                )
            } else {
                (pydumps::py_str(value), String::new())
            };
            status_lines.push_str(&format!(
                "<li><span class='k'>{}</span><span{title_attr}>{}</span></li>",
                pyfmt::escape(&label),
                pyfmt::escape(&display)
            ));
        }
    }
    let log_html = if run_state == "running" {
        match today_log_tail(state, 30).filter(|s| !s.is_empty()) {
            Some(tail) => format!(
                "<p><strong>{}</strong></p><pre class='log'>{}</pre>",
                pyfmt::escape_with(&t(lang, "Today's log (tail):"), false),
                pyfmt::escape(&tail)
            ),
            None => String::new(),
        }
    } else {
        String::new()
    };

    let has_projects = read_json(&state.home_dir.join("projects.json"))
        .and_then(|c| c.get("projects").cloned())
        .is_some_and(|p| loopx_core::loops_config::py_truthy(&p));
    let gitlab_run_now = if run_state == "running" {
        stop_action_html(
            ctx,
            "/gitlab/stop",
            &t(lang, "Stop the running GitLab loop? The in-progress issue's work will be abandoned."),
            &csrf,
        )
    } else if !has_projects {
        run_now_action_html(
            ctx,
            "/run-now",
            "",
            &csrf,
            Some(&t(
                lang,
                "No projects configured yet - <a href='/loops/gitlab-loop?view=projects'>add one on the GitLab page</a>.",
            )),
        )
    } else {
        run_now_action_html(
            ctx,
            "/run-now",
            &t(lang, "Run the GitLab loop now? This starts a real automated run outside its normal schedule."),
            &csrf,
            None,
        )
    };

    let topics = configured_topics(state);
    let topic_status = core_status::read_topic_status(
        &state
            .repo_root
            .join("outputs")
            .join("topic-monitor")
            .join("status.json"),
    );
    let empty = serde_json::Map::new();
    let topic_status = topic_status
        .get("topics")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let any_running = topic_status
        .values()
        .any(|e| e.get("state").and_then(Value::as_str) == Some("running"));
    let (tb_class, tb_icon, tb_label) = if any_running {
        let (c, i) = badge::status_badge("running");
        (c, i, topic_progress_text(lang, topic_status, &topics))
    } else if topics.is_empty() {
        let (c, i) = badge::status_badge("never_run");
        (c, i, t(lang, "Not configured"))
    } else {
        let (c, i) = badge::status_badge("idle");
        (c, i, t(lang, "Idle"))
    };
    let topic_hero = format!(
        "<div class='status-hero'><span class='pill pill-lg {tb_class}'>{tb_icon}{}</span></div>",
        pyfmt::escape(&tb_label)
    );
    let mut topic_fields = format!(
        "<li><span class='k'>{}</span><span>{}</span></li>",
        esc_t(lang, "Configured topics"),
        topics.len()
    );
    let most_recent = topic_status
        .values()
        .filter_map(|e| e.get("updated_at"))
        .filter(|v| loopx_core::loops_config::py_truthy(v))
        .map(pydumps::py_str)
        .max();
    if let Some(recent) = most_recent {
        topic_fields.push_str(&format!(
            "<li><span class='k'>{}</span><span title='{}'>{}</span></li>",
            esc_t(lang, "Last run"),
            pyfmt::escape(&recent),
            pyfmt::escape(&relative_time(state.now(), &recent))
        ));
    }
    let topic_run_now = if any_running {
        stop_action_html(
            ctx,
            "/topic-monitor/stop",
            &t(lang, "Stop the running topic loop? The in-progress topic's work will be abandoned."),
            &csrf,
        )
    } else if topics.is_empty() {
        run_now_action_html(
            ctx,
            "/topic-monitor/run-now",
            "",
            &csrf,
            Some(&t(
                lang,
                "No topics configured yet - <a href='/loops/topic-loop?view=topics'>add one on the Topic Settings page</a>.",
            )),
        )
    } else {
        run_now_action_html(
            ctx,
            "/topic-monitor/run-now",
            &t(lang, "Run the topic monitor loop now? This starts a real automated run outside its normal schedule."),
            &csrf,
            None,
        )
    };

    let mut scope = Scope::new(lang);
    scope
        .var(
            "activity_subtitle",
            pyfmt::escape_with(
                &t(lang, "What each automated loop is doing right now, plus the GitLab loop's most recent report."),
                false,
            ),
        )
        .var("flash_html", flash_html(flash, flash_ok))
        .var("state_hero_html", state_hero)
        .set("''.join(status_lines)", status_lines)
        .var("gitlab_run_now_html", gitlab_run_now)
        .var("log_html", log_html)
        .var("topic_state_hero_html", topic_hero)
        .set("''.join(topic_fields)", topic_fields)
        .var("topic_run_now_html", topic_run_now)
        .set("render_markdown(review)", markdown::render_markdown(&review, &prefixes))
        .set(
            "_topic_latest_data_html(topics)",
            topic_latest_data_html(state, lang, &topics),
        );
    Template::named("chat_activity_body")
        .render(&scope)
        .expect("chat activity template")
}

// ----- routes: pages and fragments -----

fn dashboard(ctx: &UiCtx) -> UiResponse {
    let q = ctx.query();
    let (flash, ok) = ctx.flash();
    let hub = hub::hub_by_key("overview").expect("overview hub");
    let view = hub::resolve_view(&hub, q.first("view")).key.clone();
    let body = if view == "activity" {
        activity_body(ctx, flash.as_deref(), ok)
    } else {
        overview_body(ctx, flash.as_deref(), ok, q.first("session"))
    };
    UiResponse::html(hub::render_hub_page(
        ctx,
        &hub,
        Some(&view),
        &body,
        &default_badge(ctx),
    ))
}

fn messages_fragment_route(ctx: &UiCtx) -> UiResponse {
    let q = ctx.query();
    UiResponse::html(messages_fragment(ctx.state, ctx.lang, q.first("session")))
}

fn sessions_fragment_route(ctx: &UiCtx) -> UiResponse {
    let q = ctx.query();
    UiResponse::html(history_fragment(ctx.state, ctx.lang, q.first("session")))
}

fn chat_doc(ctx: &UiCtx) -> UiResponse {
    let name = ctx.param("name");
    let env = tools_env(ctx.state);
    let Some(content) = chat_tools::documents::read_doc(&env, name) else {
        return UiResponse::not_found(ctx.lang);
    };
    if ctx.query().all("fragment").iter().any(|v| v == "1")
        || ctx.req.query.split('&').any(|p| p == "fragment=1")
    {
        return UiResponse::html(format!(
            "<div class='markdown'>{}</div>",
            markdown::render_markdown(&content, &prefixes(ctx.state))
        ));
    }
    let mut resp = UiResponse::text(200, content);
    resp.headers = vec![
        ("Content-Type".into(), "text/markdown; charset=utf-8".into()),
        (
            "Content-Disposition".into(),
            format!("attachment; filename=\"{name}\""),
        ),
        ("X-Content-Type-Options".into(), "nosniff".into()),
    ];
    resp
}

// ----- routes: POST -----

/// `urllib.parse.parse_qs(body, keep_blank_values=True)`: first value per key.
fn form_keep_blank(body: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(body).into_owned();
    let mut out: Vec<(String, String)> = Vec::new();
    for pair in text.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let k = pyfmt::unquote(&k.replace('+', " "));
        let v = pyfmt::unquote(&v.replace('+', " "));
        if !out.iter().any(|(n, _)| *n == k) {
            out.push((k, v));
        }
    }
    out
}

fn field<'a>(form: &'a [(String, String)], name: &str) -> Option<&'a str> {
    form.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn post_message(ctx: &UiCtx) -> UiResponse {
    let form = form_keep_blank(&ctx.req.body);
    let text = field(&form, "text").unwrap_or("");
    match store(ctx.state).send_chat_message(text, field(&form, "session")) {
        Some(session) => UiResponse::flash_redirect(
            true,
            &ctx.t("Message sent"),
            &format!("/?session={}", pyfmt::quote(&session, "")),
        ),
        None => UiResponse::flash_redirect(false, &ctx.t("Message is required"), "/"),
    }
}

fn post_delete_message(ctx: &UiCtx) -> UiResponse {
    let timestamp = ctx.param("timestamp");
    let store = store(ctx.state);
    let target = store
        .read_messages()
        .into_iter()
        .find(|m| m.get("timestamp").and_then(Value::as_str) == Some(timestamp));
    let ok = store.delete_message(timestamp);
    let message = if ok { ctx.t("Message deleted") } else { ctx.t("Message not found") };
    let location = match target {
        Some(m) => {
            let sid = m
                .get("session")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(loopx_core::messages::LEGACY_CHAT_SESSION_ID);
            format!("/?session={}", pyfmt::quote(sid, ""))
        }
        None => "/".to_string(),
    };
    UiResponse::flash_redirect(ok, &message, &location)
}

fn post_delete_session(ctx: &UiCtx) -> UiResponse {
    let session_id = ctx.param("id");
    let form = form_keep_blank(&ctx.req.body);
    let viewing = field(&form, "viewing").unwrap_or("");
    // parse_qs without keep_blank in Python: a blank `viewing` is "".
    let ok = store(ctx.state).delete_chat_session(session_id);
    let message = if ok { ctx.t("Chat deleted") } else { ctx.t("Chat not found") };
    let location = if !viewing.is_empty() && viewing != session_id {
        format!("/?session={}&history=1", pyfmt::quote(viewing, ""))
    } else {
        "/?history=1".to_string()
    };
    UiResponse::flash_redirect(ok, &message, &location)
}

fn post_new_chat(ctx: &UiCtx) -> UiResponse {
    store(ctx.state).start_new_chat_session();
    UiResponse::redirect(303, "/")
}

fn pending_form(ctx: &UiCtx) -> (String, String) {
    let form = form_keep_blank(&ctx.req.body);
    (
        field(&form, "id").unwrap_or("").to_string(),
        field(&form, "session").unwrap_or("").to_string(),
    )
}

/// `_chat_approve`.
pub fn chat_approve(state: &UiState, lang: Lang, pending_id: &str, session: &str) -> Value {
    let env = tools_env(state);
    let expired = || json!({"ok": false, "error": t(lang, "This action expired or was already handled.")});
    let Some(record) = chat_tools::pending::claim(&env, pending_id, session, Some(epoch(state))) else {
        return expired();
    };
    let is_write = chat_tools::tool_mode(&record.tool) == Some(chat_tools::Mode::Write);
    if !is_write || !record.args.is_object() {
        return json!({"ok": false, "error": t(lang, "Unknown action.")});
    }
    let result = chat_tools::execute_tool(&env, &record.tool, &record.args);
    let failed = result.get("error").is_some();
    let text = if failed {
        i18n::t(
            lang,
            "{tool} failed: {error}",
            &[("tool", &record.tool), ("error", &pydumps::py_str(&result["error"]))],
        )
    } else {
        i18n::t(
            lang,
            "Approved and done: {tool} {summary}",
            &[("tool", &record.tool), ("summary", &record.summary)],
        )
    };
    let body = if failed {
        format!("error: {}", pydumps::py_str(&result["error"]))
    } else {
        "ok".to_string()
    };
    chat::append_unified_log(
        &state.repo_root,
        "chat-tool",
        &format!("approve {}", record.tool),
        Some(&body),
    );
    store(state).append_message("loop", &text, Some(session), None);
    if failed {
        json!({"ok": false, "error": text, "result": result})
    } else {
        json!({"ok": true, "result": result})
    }
}

fn post_approve(ctx: &UiCtx) -> UiResponse {
    let (id, session) = pending_form(ctx);
    UiResponse::json(200, &chat_approve(ctx.state, ctx.lang, &id, &session))
}

fn post_reject(ctx: &UiCtx) -> UiResponse {
    let (id, session) = pending_form(ctx);
    let result = if chat_tools::pending::discard(&tools_env(ctx.state), &id, &session) {
        json!({"ok": true})
    } else {
        json!({"ok": false, "error": ctx.t("This action expired or was already handled.")})
    };
    UiResponse::json(200, &result)
}

/// The runtime a reply thread needs, wired from the server state.
pub fn chat_runtime(state: &UiState) -> ChatRuntime {
    let env = tools_env(state);
    let now_fixed = state.fixed_now;
    ChatRuntime {
        repo_root: state.repo_root.clone(),
        store: store(state),
        command: ChatCommand::from_env(),
        jobs: jobs().clone(),
        tools_description: Arc::new(chat_tools::describe_all),
        pending_lookup: Arc::new(move |id, session| {
            let now = now_fixed.map(|n| n.timestamp_micros() as f64 / 1_000_000.0);
            chat_tools::pending::lookup(&env, id, session, now).map(|p| p.to_value())
        }),
        env: Vec::new(),
    }
}

/// The page-context pair `(label, path)` for a known nav key (`build_chat_prompt`).
fn page_context(page: Option<&str>) -> Option<(String, String)> {
    let key = crate::ui::nav::nav_key(page?);
    consts::list("dashboard_server", "_NAV_ITEMS").iter().find_map(|item| {
        let a = item.as_array()?;
        (a.first()?.as_str()? == key).then(|| {
            (
                a.get(2).and_then(Value::as_str).unwrap_or("").to_string(),
                a.get(1).and_then(Value::as_str).unwrap_or("").to_string(),
            )
        })
    })
}

fn post_chat(ctx: &UiCtx) -> UiResponse {
    let form = form_keep_blank(&ctx.req.body);
    let text = field(&form, "text").unwrap_or("");
    let store = store(ctx.state);
    let Some(session_id) = store.send_chat_message(text, field(&form, "session")) else {
        return UiResponse::json(400, &json!({"error": ctx.t("Message is required")}));
    };
    let all = store.chat_session_messages(&session_id);
    let upto = all.len().saturating_sub(1);
    let start = upto.saturating_sub(chat::CHAT_MESSAGE_HISTORY_LIMIT);
    let recent = &all[start..upto];
    chat::append_unified_log(&ctx.state.repo_root, "chat-assistant", "question", Some(text.trim()));
    let rt = chat_runtime(ctx.state);
    let reply_key = rt.jobs.create();
    let context = page_context(field(&form, "page").filter(|p| !p.is_empty()));
    let prompt = chat::build_chat_prompt(
        text.trim(),
        recent,
        context.as_ref().map(|(a, b)| (a.as_str(), b.as_str())),
    );
    let (key, sid) = (reply_key.clone(), session_id.clone());
    let spawned = std::thread::Builder::new()
        .name("chat-reply".into())
        .spawn(move || chat::run_chat_job(&rt, &key, &prompt, &sid));
    if let Err(e) = spawned {
        jobs().finish(
            &reply_key,
            Some(i18n::t(
                ctx.lang,
                "Could not start assistant thread: {error}",
                &[("error", &e.to_string())],
            )),
            None,
        );
    }
    UiResponse::json(200, &json!({"reply_key": reply_key, "session": session_id}))
}

// ----- the SSE stream -----

/// `GET /activity/chat-stream?reply_key=K` as a streaming response; the caller
/// (`glue::try_serve`) hands the raw query string. Frames are produced by a
/// blocking thread and forwarded as they appear, so text reaches the browser
/// while the reply is still being written.
pub fn chat_stream_response(query: &str) -> axum::http::Response<axum::body::Body> {
    use axum::body::Body;
    use bytes::Bytes;
    let key = pyfmt::Form::parse(query)
        .first("reply_key")
        .unwrap_or("")
        .to_string();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Bytes, std::io::Error>>();
    std::thread::spawn(move || {
        let Some(mut stream) = jobs().stream(&key) else {
            let _ = tx.send(Ok(Bytes::from(chat::sse_frame(
                "error",
                &json!("Unknown or expired reply_key"),
            ))));
            return;
        };
        while let Some(batch) = stream.next_batch(std::time::Duration::from_secs(
            chat::STREAM_IDLE_TIMEOUT_SECONDS,
        )) {
            for item in &batch {
                let frame = match item {
                    chat::StreamItem::Done { error: Some(e), .. } if e.is_empty() => chat::sse_frame(
                        "done",
                        &json!(match item {
                            chat::StreamItem::Done { final_text, .. } =>
                                final_text.clone().unwrap_or_default(),
                            _ => String::new(),
                        }),
                    ),
                    other => chat::stream_item_frame(other),
                };
                if tx.send(Ok(Bytes::from(frame))).is_err() {
                    return; // the client went away; the job keeps running
                }
            }
        }
    });
    let body = Body::from_stream(tokio_stream::wrappers::UnboundedReceiverStream::new(rx));
    axum::http::Response::builder()
        .status(200)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-store")
        .header("X-Accel-Buffering", "no")
        .body(body)
        .expect("valid response")
}

/// Unused import guard for helpers other modules may want.
#[allow(dead_code)]
fn _paths(_: PathBuf) {}
