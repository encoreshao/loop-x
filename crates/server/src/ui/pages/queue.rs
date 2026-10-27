//! Mission Control: `/queue` shows every loop that is running, waiting for its
//! turn or just finished, with Stop / Move to front / Remove; the topbar pill
//! (`pill_html`) links here and polls `/queue/pill`. The queue itself lives in
//! `loopx_core::run_queue`; this page only reads it and presses its buttons.

use crate::ui::badge::default_badge;
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::shell::{render_shell, ShellOpts};
use crate::ui::state::UiState;
use crate::ui::{icons, pyfmt};
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

// ----- the topbar pill -----

/// `(css class, label)` for the topbar pill from the queue's counts.
fn pill_parts(ctx: &UiCtx, snap: &Snapshot) -> (&'static str, String) {
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
        return ("pill pill-blue", label);
    }
    if queued > 0 {
        return ("pill pill-blue", ctx.tf("{queued} queued", &[("queued", &queued.to_string())]));
    }
    if snap.recent.first().is_some_and(|e| e.state == "failed") {
        return ("pill pill-red", ctx.t("Last run failed"));
    }
    ("pill pill-grey", ctx.t("Mission Control"))
}

/// The topbar link to Mission Control; `JS in shell.tpl` keeps it current.
pub fn pill_html(ctx: &UiCtx) -> String {
    let snap = queue_for(ctx.state).snapshot(1);
    let (class, label) = pill_parts(ctx, &snap);
    let title = pyfmt::escape(&ctx.t("Mission Control"));
    format!(
        "<a class='{class}' id='queue-pill' href='/queue' title='{title}' aria-label='{title}'>{}<span>{}</span></a>",
        icons::icon("speed"),
        pyfmt::escape(&label),
    )
}

fn pill(ctx: &UiCtx) -> UiResponse {
    let snap = queue_for(ctx.state).snapshot(1);
    let (class, label) = pill_parts(ctx, &snap);
    UiResponse::json(200, &json!({"class": class, "label": label}))
}

// ----- the page -----

fn reason_label(ctx: &UiCtx, reason: &str) -> String {
    match reason {
        "manual" => ctx.t("Manual"),
        "catch-up" => ctx.t("Catch-up"),
        _ => ctx.t("Scheduled"),
    }
}

fn action_button(ctx: &UiCtx, action: &str, id: &str, label: &str, icon: &str) -> String {
    let _ = ctx;
    format!(
        "<button type='button' class='btn btn-neutral' data-queue-action='{action}' data-id='{}'>{}{}</button>",
        pyfmt::escape(id),
        icons::icon(icon),
        pyfmt::escape(label),
    )
}

fn row(ctx: &UiCtx, e: &Entry, detail: &str, actions: &str) -> String {
    let (pill_class, state_label) = match e.state.as_str() {
        "running" => ("pill-blue", ctx.t("Running")),
        "queued" => ("pill-grey", ctx.t("Waiting")),
        "done" => ("pill-green", ctx.t("Done")),
        "failed" => ("pill-red", ctx.t("Failed")),
        "cancelled" => ("pill-grey", ctx.t("Cancelled")),
        _ => ("pill-grey", ctx.t("Skipped")),
    };
    format!(
        "<div class='history-entry queue-row'><div class='history-entry-header'><strong>{}</strong>\
         <div class='pill-row'><span class='pill {pill_class}'>{}</span><span class='pill pill-grey'>{}</span></div></div>\
         <p class='history-entry-overview'>{}</p><div class='queue-actions'>{actions}</div></div>",
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
    let running = snap
        .running
        .iter()
        .map(|e| {
            let step = loopx_core::status::read_status(&ctx.state.status_path_for_loop(&e.loop_name))
                .get("current_step")
                .and_then(|v| v.as_str())
                .map(String::from);
            let elapsed = ctx.tf(
                "running for {time}",
                &[("time", &short_age(age_secs(e.started_at.as_deref())))],
            );
            let detail = match step {
                Some(s) if !s.is_empty() => format!("{s} · {elapsed}"),
                _ => elapsed,
            };
            row(ctx, e, &detail, &action_button(ctx, "stop", &e.id, &ctx.t("Stop"), "close"))
        })
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
                actions.push_str(&action_button(ctx, "front", &e.id, &ctx.t("Move to front"), "arrow_upward"));
            }
            actions.push_str(&action_button(ctx, "stop", &e.id, &ctx.t("Remove"), "close"));
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
        pyfmt::escape(&ctx.t("Mission Control")),
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
        "Mission Control · Loop X Engineering",
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
        assert!(pill.body_text().contains("Mission Control"));
    }

    #[test]
    fn running_and_waiting_loops_show_on_the_page_and_in_the_pill() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let q = RunQueue::new(d.path().join("run_queue.json"));
        q.enqueue("gitlab-loop", Reason::Scheduled, "2026-10-27T10:00:00").unwrap();
        q.enqueue("topic-loop", Reason::Scheduled, "2026-10-27T10:00:01").unwrap();
        q.enqueue("inbox-triage-loop", Reason::Scheduled, "2026-10-27T10:00:02").unwrap();
        q.claim_next("2026-10-27T10:00:05", 1).unwrap();
        let html = st.registry.dispatch(&st, &UiRequest::get("/queue/fragment")).unwrap().body_text();
        assert!(html.contains("gitlab-loop") && html.contains("topic-loop") && html.contains("inbox-triage-loop"));
        assert!(html.contains("#2 in line"), "{html}");
        let pill = st.registry.dispatch(&st, &UiRequest::get("/queue/pill")).unwrap().body_text();
        assert!(pill.contains("1 running") && pill.contains("2 queued"), "{pill}");
        let shell = st.registry.dispatch(&st, &UiRequest::get("/queue")).unwrap().body_text();
        assert!(shell.contains("id='queue-pill'") && shell.contains("1 running · 2 queued"));
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
