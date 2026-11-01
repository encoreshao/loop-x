//! Agents: `/queue` shows every loop that is running, waiting for its
//! turn or just finished, with Stop / Move to front / Remove; the topbar pill
//! (`pill_html`) links here and polls `/queue/pill`. The queue itself lives in
//! `loopx_core::run_queue`; this page only reads it and presses its buttons.

use crate::ui::badge::{default_badge, progress_text};
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::shell::{render_shell, ShellOpts};
use crate::ui::state::UiState;
use crate::ui::{consts, icons, pyfmt};
use chrono::{Local, NaiveDateTime};
use loopx_core::run_queue::{self, Entry, RunQueue, Snapshot};
use serde_json::json;

const RECENT: usize = 10;

pub fn register(r: &mut Registry) {
    r.get("/queue", page);
    r.get("/queue/fragment", fragment);
    r.get("/queue/pill", pill);
    r.post("/queue/stop", post_stop);
    r.post("/queue/front", post_front);
}

fn queue_for(state: &UiState) -> RunQueue {
    RunQueue::new(state.home_dir.join("run_queue.json"))
}

fn now_iso() -> String {
    loopx_core::loop_scheduler::isoformat(Local::now().naive_local())
}

/// Whole seconds since a naive local ISO timestamp, with or without fractional
/// seconds (0 when unreadable).
fn age_secs(iso: Option<&str>) -> i64 {
    let parse = |t: &str| {
        NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f")
            .or_else(|_| NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S"))
            .ok()
    };
    iso.and_then(parse)
        .map_or(0, |t| (Local::now().naive_local() - t).num_seconds().max(0))
}

#[cfg(test)]
mod age_tests {
    use super::*;

    #[test]
    fn age_reads_timestamps_with_and_without_fractions() {
        let now = Local::now().naive_local();
        let past = now - chrono::Duration::seconds(125);
        let plain = past.format("%Y-%m-%dT%H:%M:%S").to_string();
        let frac = past.format("%Y-%m-%dT%H:%M:%S%.6f").to_string();
        assert!((age_secs(Some(&plain)) - 125).abs() <= 2);
        assert!((age_secs(Some(&frac)) - 125).abs() <= 2);
        assert_eq!(age_secs(Some("nonsense")), 0);
    }
}

fn short_age(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}

/// What each of the GitLab loop's agents last did, from the hooks' activity
/// files (one per issue, since issues run in parallel):
/// "web #7: Agent is running Bash: cargo test · 1m ago".
fn agent_activity(ctx: &UiCtx) -> Option<String> {
    let base = loopx_core::agent_activity::beside(&ctx.state.status_path_for_loop("issue-loop"));
    let lines: Vec<String> = loopx_core::agent_activity::read_all(&base)
        .iter()
        .filter_map(|a| one_activity(ctx, a))
        .collect();
    (!lines.is_empty()).then(|| lines.join(" · "))
}

fn one_activity(ctx: &UiCtx, a: &serde_json::Value) -> Option<String> {
    let text = |k: &str| a.get(k).and_then(serde_json::Value::as_str).filter(|v| !v.is_empty());
    let doing = match (text("event")?, text("tool"), text("detail")) {
        ("PreToolUse", Some(tool), Some(detail)) => {
            ctx.tf("Agent is running {tool}: {detail}", &[("tool", tool), ("detail", detail)])
        }
        ("PreToolUse", Some(tool), None) => ctx.tf("Agent is running {tool}", &[("tool", tool)]),
        ("Notification", _, Some(message)) => ctx.tf("Agent is waiting: {message}", &[("message", message)]),
        ("Stop", ..) => ctx.t("Agent finished its turn"),
        ("Start", ..) => ctx.t("Agent is starting"),
        _ => ctx.t("Agent is thinking"),
    };
    let ago = text("at")
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds().max(0));
    let doing = match ago {
        Some(secs) => ctx.tf("{doing} · {time} ago", &[("doing", &doing), ("time", &short_age(secs))]),
        None => doing,
    };
    Some(match text("label") {
        Some(label) => ctx.tf("{task}: {activity}", &[("task", label), ("activity", &doing)]),
        None => doing,
    })
}

/// `progress` plus, for the GitLab loop, what its agent is doing.
fn with_agent_activity(ctx: &UiCtx, loop_name: &str, progress: String) -> String {
    match (loop_name == "issue-loop").then(|| agent_activity(ctx)).flatten() {
        Some(activity) => format!("{progress} · {activity}"),
        None => progress,
    }
}

// ----- the topbar pill -----

/// A loop's status file when it says the loop is running and its process is alive.
fn live_status(ctx: &UiCtx, name: &str) -> Option<serde_json::Value> {
    let status = loopx_core::status::read_status(&ctx.state.status_path_for_loop(name));
    (crate::snapshot::effective_state(&status) == "running").then_some(status)
}

/// A GitLab loop run started outside the queue (launchd, Run now): the pill and
/// the page both show it alongside the queue's own running entries.
fn outside_gitlab_run(ctx: &UiCtx, snap: &Snapshot) -> Option<serde_json::Value> {
    if snap.running.iter().any(|e| e.loop_name == "issue-loop") {
        return None;
    }
    live_status(ctx, "issue-loop")
}

/// What a running loop is doing right now, from its status file: the GitLab loop
/// first (it also runs outside the queue), then each running queue entry.
fn running_progress(ctx: &UiCtx, snap: &Snapshot) -> Option<String> {
    std::iter::once("issue-loop")
        .chain(snap.running.iter().map(|e| e.loop_name.as_str()))
        .find_map(|name| live_status(ctx, name))
        .map(|status| progress_text(ctx.lang, &status))
}

/// `(css class, label, busy)` for the topbar pill: a running loop's progress,
/// else the queue's counts.
fn pill_parts(ctx: &UiCtx, snap: &Snapshot) -> (&'static str, String, bool) {
    if let Some(progress) = running_progress(ctx, snap) {
        return ("pill pill-blue", progress, true);
    }
    let (running, queued) = (snap.running.len(), snap.queued.len());
    if running > 0 {
        let label = if queued > 0 {
            ctx.tf(
                "{running} running · {queued} queued",
                &[("running", &running.to_string()), ("queued", &queued.to_string())],
            )
        } else {
            ctx.tf("{running} running", &[("running", &running.to_string())])
        };
        return ("pill pill-blue", label, true);
    }
    if queued > 0 {
        return ("pill pill-blue", ctx.tf("{queued} queued", &[("queued", &queued.to_string())]), false);
    }
    if snap.recent.first().is_some_and(|e| e.state == "failed") {
        return ("pill pill-red", ctx.t("Last run failed"), false);
    }
    ("pill pill-outline", ctx.t("Agents"), false)
}

fn pill_icon(busy: bool) -> String {
    if busy {
        consts::ds("_SPINNER_ICON").to_string()
    } else {
        icons::icon("speed")
    }
}

/// The topbar link to Agents, which also says what a running loop is doing;
/// `JS in shell.tpl` keeps it current.
pub fn pill_html(ctx: &UiCtx) -> String {
    let snap = queue_for(ctx.state).snapshot(1);
    let (class, label, busy) = pill_parts(ctx, &snap);
    let label = pyfmt::escape(&label);
    let active = if ctx.req.path == "/queue" { " is-active" } else { "" };
    format!(
        "<a class='topbar-agents {class}{active}' id='queue-pill' href='/queue' title='{label}'>{}<span>{label}</span></a>",
        pill_icon(busy),
    )
}

fn pill(ctx: &UiCtx) -> UiResponse {
    let snap = queue_for(ctx.state).snapshot(1);
    let (class, label, busy) = pill_parts(ctx, &snap);
    UiResponse::json(200, &json!({"class": class, "label": label, "icon": pill_icon(busy)}))
}

// ----- the page -----

fn reason_label(ctx: &UiCtx, reason: &str) -> String {
    match reason {
        "manual" => ctx.t("Manual"),
        "catch-up" => ctx.t("Catch-up"),
        "outside" => ctx.t("Outside the queue"),
        _ => ctx.t("Scheduled"),
    }
}

fn action_button(action: &str, style: &str, id: &str, label: &str, icon: &str) -> String {
    format!(
        "<button type='button' class='btn {style}' data-queue-action='{action}' data-id='{}'>{}{}</button>",
        pyfmt::escape(id),
        icons::icon(icon),
        pyfmt::escape(label),
    )
}

fn row(ctx: &UiCtx, e: &Entry, detail: &str, actions: &str) -> String {
    let (pill_class, state_label) = match e.state.as_str() {
        "running" => ("pill-blue", ctx.t("Running")),
        "queued" => ("pill-outline", ctx.t("Waiting")),
        "done" => ("pill-green", ctx.t("Done")),
        "failed" => ("pill-red", ctx.t("Failed")),
        "cancelled" => ("pill-outline", ctx.t("Cancelled")),
        _ => ("pill-outline", ctx.t("Skipped")),
    };
    format!(
        "<div class='history-entry queue-row'><div class='history-entry-header'><strong>{}</strong>\
         <div class='pill-row'><span class='pill {pill_class}'>{}</span><span class='pill pill-outline'>{}</span></div>\
         <div class='queue-actions'>{actions}</div></div>\
         <p class='history-entry-overview'>{}</p></div>",
        pyfmt::escape(&e.loop_name),
        pyfmt::escape(&state_label),
        pyfmt::escape(&reason_label(ctx, &e.reason)),
        pyfmt::escape(detail),
    )
}

fn section(ctx: &UiCtx, title: &str, empty: &str, rows: Vec<String>) -> String {
    let inner = if rows.is_empty() {
        format!("<p>{}</p>", pyfmt::escape(&ctx.t(empty)))
    } else {
        rows.concat()
    };
    format!(
        "<section class='card'><div class='section-header'><h2>{}</h2></div>{inner}</section>",
        pyfmt::escape(&ctx.t(title)),
    )
}

fn sections_html(ctx: &UiCtx, snap: &Snapshot) -> String {
    let outside = outside_gitlab_run(ctx, snap).map(|status| {
        let e = Entry {
            id: String::new(),
            loop_name: "issue-loop".into(),
            reason: "outside".into(),
            priority: 0,
            enqueued_at: String::new(),
            state: "running".into(),
            started_at: None,
            finished_at: None,
            pid: None,
            exit_code: None,
            note: None,
            cancel_requested: false,
        };
        let progress = with_agent_activity(ctx, "issue-loop", progress_text(ctx.lang, &status));
        row(ctx, &e, &progress, "")
    });
    let running = outside
        .into_iter()
        .chain(snap.running.iter().map(|e| {
            let progress = live_status(ctx, &e.loop_name)
                .map(|s| with_agent_activity(ctx, &e.loop_name, progress_text(ctx.lang, &s)));
            let elapsed = ctx.tf(
                "running for {time}",
                &[("time", &short_age(age_secs(e.started_at.as_deref())))],
            );
            let detail = match progress {
                Some(p) => format!("{p} · {elapsed}"),
                None => elapsed,
            };
            row(ctx, e, &detail, &action_button("stop", "btn-danger", &e.id, &ctx.t("Stop"), "close"))
        }))
        .collect();
    let queued = snap
        .queued
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let detail = ctx.tf(
                "#{position} in line · waiting {time}",
                &[
                    ("position", &(i + 1).to_string()),
                    ("time", &short_age(age_secs(Some(&e.enqueued_at)))),
                ],
            );
            let mut actions = String::new();
            if i > 0 {
                actions.push_str(&action_button("front", "btn-tonal", &e.id, &ctx.t("Move to front"), "arrow_upward"));
            }
            actions.push_str(&action_button("stop", "btn-warning", &e.id, &ctx.t("Remove"), "close"));
            row(ctx, e, &detail, &actions)
        })
        .collect();
    let recent = snap
        .recent
        .iter()
        .map(|e| {
            let ago = short_age(age_secs(e.finished_at.as_deref()));
            let detail = match &e.note {
                Some(n) => format!("{} · {n}", ctx.tf("finished {time} ago", &[("time", &ago)])),
                None => ctx.tf("finished {time} ago", &[("time", &ago)]),
            };
            row(ctx, e, &detail, "")
        })
        .collect();
    [
        section(ctx, "Running", "Nothing is running", running),
        section(ctx, "Up next", "Nothing is waiting", queued),
        section(ctx, "Recent", "No runs yet", recent),
    ]
    .concat()
}

fn fragment_html(ctx: &UiCtx) -> String {
    sections_html(ctx, &queue_for(ctx.state).snapshot(RECENT))
}

fn body_html(ctx: &UiCtx) -> String {
    format!(
        "<h1>{}</h1><p>{}</p>{}<div id='queue-root' class='queue-root'>{}</div>{SCRIPT}",
        pyfmt::escape(&ctx.t("Agents")),
        pyfmt::escape(&ctx.t(
            "Loops that are due wait here and run one at a time; a run you start by hand goes first."
        )),
        ctx.csrf_input(),
        fragment_html(ctx),
    )
}

/// Refreshes the page every 3 s and presses the row buttons.
const SCRIPT: &str = "<script>(function(){\
var root=document.getElementById('queue-root');if(!root)return;\
var busy=false;\
function refresh(){if(busy||document.hidden)return;\
fetch('/queue/fragment').then(function(r){return r.text()}).then(function(h){if(!busy)root.innerHTML=h}).catch(function(){});}\
setInterval(refresh,3000);\
root.addEventListener('click',function(ev){\
var b=ev.target.closest('[data-queue-action]');if(!b)return;\
var token=document.querySelector(\"input[name='csrf_token']\");\
var p=new URLSearchParams();p.set('id',b.getAttribute('data-id'));p.set('csrf_token',token?token.value:'');\
busy=true;b.disabled=true;\
fetch('/queue/'+b.getAttribute('data-queue-action'),{method:'POST',body:p}).catch(function(){}).then(function(){busy=false;refresh()});\
});})();</script>";

fn page(ctx: &UiCtx) -> UiResponse {
    UiResponse::html(render_shell(
        ctx,
        "Agents · LoopX Engineering",
        "queue",
        &default_badge(ctx),
        &body_html(ctx),
        ShellOpts::default(),
    ))
}

fn fragment(ctx: &UiCtx) -> UiResponse {
    UiResponse::html(fragment_html(ctx))
}

// ----- the buttons -----

fn form_id(ctx: &UiCtx) -> String {
    pyfmt::Form::parse(&String::from_utf8_lossy(&ctx.req.body))
        .first("id")
        .unwrap_or("")
        .to_string()
}

fn post_stop(ctx: &UiCtx) -> UiResponse {
    let outcome = run_queue::stop_entry(&queue_for(ctx.state), &form_id(ctx), &now_iso());
    let ok = !matches!(outcome, Ok(run_queue::CancelOutcome::NotFound) | Err(_));
    UiResponse::json(200, &json!({"ok": ok}))
}

fn post_front(ctx: &UiCtx) -> UiResponse {
    let ok = queue_for(ctx.state).move_to_front(&form_id(ctx)).unwrap_or(false);
    UiResponse::json(200, &json!({"ok": ok}))
}

#[cfg(test)]
mod tests {
    use crate::ui::routes::UiRequest;
    use crate::ui::state::UiState;
    use axum::http::Method;
    use loopx_core::run_queue::{Reason, RunQueue};

    fn state(dir: &std::path::Path) -> UiState {
        UiState::explicit(dir, dir, dir, &dir.join("update"), "tok", None)
    }

    fn post(st: &UiState, path: &str, body: &str) -> crate::ui::routes::UiResponse {
        st.registry
            .dispatch(st, &UiRequest::new(Method::POST, path).with_body(body.to_string()))
            .unwrap()
    }

    #[test]
    fn an_empty_queue_says_so_and_the_pill_is_idle() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let page = st.registry.dispatch(&st, &UiRequest::get("/queue")).unwrap();
        assert_eq!(page.status, 200);
        let html = page.body_text();
        assert!(html.contains("Nothing is running") && html.contains("Nothing is waiting"), "{html}");
        let pill = st.registry.dispatch(&st, &UiRequest::get("/queue/pill")).unwrap();
        assert!(pill.body_text().contains("Agents"));
    }

    #[test]
    fn the_topbar_pill_is_selected_only_on_the_agents_page() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let on = st.registry.dispatch(&st, &UiRequest::get("/queue")).unwrap().body_text();
        assert!(on.contains("topbar-agents pill pill-outline is-active"), "{on}");
        let off = st.registry.dispatch(&st, &UiRequest::get("/runs")).unwrap().body_text();
        assert!(off.contains("topbar-agents pill pill-outline'"), "{off}");
    }

    #[test]
    fn running_and_waiting_loops_show_on_the_page_and_in_the_pill() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let q = RunQueue::new(d.path().join("run_queue.json"));
        q.enqueue("issue-loop", Reason::Scheduled, "2026-10-27T10:00:00").unwrap();
        q.enqueue("topic-loop", Reason::Scheduled, "2026-10-27T10:00:01").unwrap();
        q.enqueue("inbox-triage-loop", Reason::Scheduled, "2026-10-27T10:00:02").unwrap();
        q.claim_next("2026-10-27T10:00:05", 1).unwrap();
        let html = st.registry.dispatch(&st, &UiRequest::get("/queue/fragment")).unwrap().body_text();
        assert!(html.contains("issue-loop") && html.contains("topic-loop") && html.contains("inbox-triage-loop"));
        assert!(html.contains("#2 in line"), "{html}");
        let pill = st.registry.dispatch(&st, &UiRequest::get("/queue/pill")).unwrap().body_text();
        assert!(pill.contains("1 running") && pill.contains("2 queued"), "{pill}");
        let shell = st.registry.dispatch(&st, &UiRequest::get("/queue")).unwrap().body_text();
        assert!(shell.contains("id='queue-pill'") && shell.contains("1 running · 2 queued"));
    }

    fn write_status(dir: &std::path::Path, rel: &str, body: &str) {
        let path = dir.join("outputs").join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn the_pill_carries_a_running_loops_progress_with_a_spinner() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        write_status(
            d.path(),
            "status.json",
            r#"{"state": "running", "current_issue": "web #1362", "current_step": "analyzing"}"#,
        );
        let pill = st.registry.dispatch(&st, &UiRequest::get("/queue/pill")).unwrap().body_text();
        let pill: serde_json::Value = serde_json::from_str(&pill).unwrap();
        assert_eq!(pill["label"], "Processing web #1362 — Analyzing");
        assert_eq!(pill["class"], "pill pill-blue");
        assert!(pill["icon"].as_str().unwrap().contains("md-spinner"), "{pill}");
        let shell = st.registry.dispatch(&st, &UiRequest::get("/runs")).unwrap().body_text();
        let start = shell.find("id='queue-pill'").unwrap();
        let tag = &shell[start..start + shell[start..].find("</a>").unwrap()];
        assert!(tag.contains("md-spinner") && tag.contains("Processing web #1362 — Analyzing"), "{tag}");
        assert_eq!(shell.matches("Processing web #1362").count(), 2, "only the pill and its title");
    }

    #[test]
    fn a_gitlab_run_outside_the_queue_shows_under_running_like_the_pill() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        write_status(
            d.path(),
            "status.json",
            r#"{"state": "running", "current_issue": "web #10", "current_step": "running_verification"}"#,
        );
        let html = st.registry.dispatch(&st, &UiRequest::get("/queue/fragment")).unwrap().body_text();
        assert!(!html.contains("Nothing is running"), "{html}");
        assert!(html.contains("issue-loop") && html.contains("Processing web #10"), "{html}");
    }

    #[test]
    fn a_gitlab_run_says_what_its_agent_is_doing() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        write_status(d.path(), "status.json", r#"{"state": "running", "current_issue": "web #10", "current_step": "analyzing"}"#);
        let at = (chrono::Utc::now() - chrono::Duration::seconds(75)).to_rfc3339();
        write_status(
            d.path(),
            "agent-activity.json",
            &serde_json::json!({"at": at, "label": "web #10", "event": "PreToolUse", "tool": "Bash", "detail": "cargo test <x>"}).to_string(),
        );
        let html = st.registry.dispatch(&st, &UiRequest::get("/queue/fragment")).unwrap().body_text();
        assert!(html.contains("Agent is running Bash: cargo test &lt;x&gt; · 1m ago"), "{html}");
        write_status(
            d.path(),
            "agent-activity.json",
            &serde_json::json!({"at": chrono::Utc::now().to_rfc3339(), "label": "web #10", "event": "Notification", "tool": null, "detail": "needs permission"}).to_string(),
        );
        let html = st.registry.dispatch(&st, &UiRequest::get("/queue/fragment")).unwrap().body_text();
        assert!(html.contains("Agent is waiting: needs permission"), "{html}");
    }

    #[test]
    fn parallel_issues_each_say_what_their_agent_is_doing() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        write_status(d.path(), "status.json", r#"{"state": "running", "current_step": "analyzing"}"#);
        let now = chrono::Utc::now().to_rfc3339();
        write_status(
            d.path(),
            "agent-activity/web-10.json",
            &serde_json::json!({"at": now, "label": "web #10", "event": "PreToolUse", "tool": "Bash", "detail": "cargo test"}).to_string(),
        );
        write_status(
            d.path(),
            "agent-activity/api-2.json",
            &serde_json::json!({"at": now, "label": "api #2", "event": "Start", "tool": null, "detail": null}).to_string(),
        );
        let html = st.registry.dispatch(&st, &UiRequest::get("/queue/fragment")).unwrap().body_text();
        assert!(html.contains("web #10: Agent is running Bash: cargo test"), "{html}");
        assert!(html.contains("api #2: Agent is starting"), "{html}");
    }

    #[test]
    fn an_idle_gitlab_loop_shows_no_stale_agent_activity() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        write_status(d.path(), "status.json", r#"{"state": "idle"}"#);
        write_status(
            d.path(),
            "agent-activity.json",
            r#"{"at": "2026-10-31T09:00:00+00:00", "label": "web #10", "event": "Stop", "tool": null, "detail": null}"#,
        );
        let html = st.registry.dispatch(&st, &UiRequest::get("/queue/fragment")).unwrap().body_text();
        assert!(!html.contains("Agent "), "{html}");
    }

    #[test]
    fn a_queued_gitlab_run_is_not_listed_twice() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let q = RunQueue::new(d.path().join("run_queue.json"));
        q.enqueue("issue-loop", Reason::Scheduled, "2026-10-27T10:00:00").unwrap();
        q.claim_next("2026-10-27T10:00:05", 1).unwrap();
        write_status(d.path(), "status.json", r#"{"state": "running", "current_step": "analyzing"}"#);
        let html = st.registry.dispatch(&st, &UiRequest::get("/queue/fragment")).unwrap().body_text();
        assert_eq!(html.matches("<strong>issue-loop</strong>").count(), 1, "{html}");
        assert!(html.contains("Analyzing"), "{html}");
    }

    #[test]
    fn a_queued_loops_own_progress_wins_over_the_run_count() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let q = RunQueue::new(d.path().join("run_queue.json"));
        q.enqueue("topic-loop", Reason::Scheduled, "2026-10-27T10:00:00").unwrap();
        q.claim_next("2026-10-27T10:00:05", 1).unwrap();
        write_status(
            d.path(),
            "status/topic-loop.json",
            r#"{"state": "running", "current_step": "analyzing"}"#,
        );
        let pill = st.registry.dispatch(&st, &UiRequest::get("/queue/pill")).unwrap().body_text();
        assert!(pill.contains(r#""label": "Analyzing""#), "{pill}");
    }

    #[test]
    fn the_buttons_need_csrf_and_move_or_remove_waiting_runs() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let q = RunQueue::new(d.path().join("run_queue.json"));
        q.enqueue("a", Reason::Scheduled, "t0").unwrap();
        q.enqueue("b", Reason::Scheduled, "t1").unwrap();
        let b = q.snapshot(0).queued[1].id.clone();
        assert_eq!(post(&st, "/queue/front", &format!("id={b}")).status, 403);
        assert!(post(&st, "/queue/front", &format!("id={b}&csrf_token=tok")).body_text().contains("true"));
        assert_eq!(q.snapshot(0).queued[0].loop_name, "b");
        assert!(post(&st, "/queue/stop", &format!("id={b}&csrf_token=tok")).body_text().contains("true"));
        assert_eq!(q.snapshot(0).queued.len(), 1);
        assert!(post(&st, "/queue/stop", "id=nope&csrf_token=tok").body_text().contains("false"));
    }
}
