//! Needs you: `/needs-you` lists everything waiting on a person
//! (`loopx_core::needs_you`) with what to do about each: Allow / Deny /
//! Request changes for an approval, a link for the rest, Dismiss for an
//! escalation, a failed loop or a dead sign-in. The topbar bell (`bell_html`) counts them and
//! polls `/needs-you/count`.

use crate::snapshot::{self, Paths};
use crate::ui::badge::default_badge;
use crate::ui::pages::loops_common::{flash_html, relative_time, relative_time_dt};
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::shell::{render_shell, ShellOpts};
use crate::ui::state::UiState;
use crate::ui::{icons, pyfmt};
use chrono::Utc;
use serde_json::{json, Value};

const PATH: &str = "/needs-you";

pub fn register(r: &mut Registry) {
    r.get(PATH, page);
    r.get("/needs-you/count", count);
    r.post("/needs-you/approval", post_approval);
    r.post("/needs-you/dismiss", post_dismiss);
}

fn paths(state: &UiState) -> Paths {
    let outputs = state.repo_root.join("outputs");
    Paths {
        events_dir: state.events_dir.clone(),
        approvals_dir: outputs.join("approvals"),
        queue_path: state.home_dir.join("run_queue.json"),
        auth_state: state.home_dir.join("auth_health.json"),
        dismissed: state.home_dir.join("needs_you_dismissed.json"),
        outputs,
    }
}

fn items(state: &UiState) -> Vec<Value> {
    let p = paths(state);
    let loops = snapshot::loop_statuses(&p.outputs);
    snapshot::needs_you(&p, &loops, Utc::now())
}

fn text(item: &Value, key: &str) -> String {
    match &item[key] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The item's line, translated.
fn headline(ctx: &UiCtx, item: &Value) -> String {
    let f = |k: &str| text(item, k);
    match item["kind"].as_str() {
        Some("approval") => ctx.tf(
            "Approve the fix for {project} #{iid}: {title}",
            &[("project", &f("project")), ("iid", &f("issue_iid")), ("title", &f("issue_title"))],
        ),
        Some("escalated") => ctx.tf(
            "{project} #{iid} was escalated to you ({reason})",
            &[("project", &f("project")), ("iid", &f("issue_iid")), ("reason", &f("reason"))],
        ),
        Some("sign_in") => ctx.tf(
            "Sign-in needs attention: {label} - {message}",
            &[("label", &f("label")), ("message", &f("message"))],
        ),
        Some("loop_failed") => ctx.tf("{loop} failed its last run", &[("loop", &f("loop"))]),
        _ => f("title"),
    }
}

fn icon_for(kind: &str) -> &'static str {
    match kind {
        "approval" => "task_alt",
        "escalated" => "warning",
        "sign_in" => "login",
        _ => "error",
    }
}

/// Where to go to deal with an item that has no button of its own.
fn link(ctx: &UiCtx, item: &Value) -> String {
    let (href, label) = match item["kind"].as_str() {
        Some("escalated") => ("/loops/gitlab-loop?view=tasks".to_string(), ctx.t("Open in the issue loop's tasks")),
        Some("sign_in") if text(item, "probe").starts_with("mailbox:") => {
            ("/loops/inbox-triage-loop?view=setup".to_string(), ctx.t("Reconnect"))
        }
        Some("sign_in") if text(item, "probe") == "ai_cli" => (crate::ui::ai_cli::SETTINGS_HREF.to_string(), ctx.t("Open AI CLI settings")),
        Some("sign_in") => ("/connectors".to_string(), ctx.t("Reconnect")),
        Some("loop_failed") => ("/runs?view=logs".to_string(), ctx.t("See the log")),
        _ => return String::new(),
    };
    format!("<a class='btn btn-neutral' href='{href}'>{}</a>", pyfmt::escape(&label))
}

fn actions(ctx: &UiCtx, item: &Value) -> String {
    let csrf = ctx.csrf_input();
    if item["kind"] == "approval" {
        return format!(
            "<form method='post' action='/needs-you/approval' class='daemon-action-form needs-you-approval'>{csrf}\
<input type='hidden' name='id' value='{}'>\
<input type='text' name='notes' maxlength='2000' placeholder='{}'>\
<button type='submit' name='decision' value='approve' class='btn btn-primary'>{}</button>\
<button type='submit' name='decision' value='request_changes' class='btn btn-neutral'>{}</button>\
<button type='submit' name='decision' value='deny' class='btn btn-danger'>{}</button></form>",
            pyfmt::escape(&text(item, "approval_id")),
            pyfmt::escape(&ctx.t("What to change (for Request changes)")),
            pyfmt::escape(&ctx.t("Allow")),
            pyfmt::escape(&ctx.t("Request changes")),
            pyfmt::escape(&ctx.t("Deny")),
        );
    }
    let dismiss = if item["dismissable"] == true {
        format!(
            "<form method='post' action='/needs-you/dismiss' class='daemon-action-form needs-you-dismiss'>{csrf}\
<input type='hidden' name='id' value='{}'><button type='submit' class='btn btn-neutral'>{}</button></form>",
            pyfmt::escape(&text(item, "id")),
            pyfmt::escape(&ctx.t("Dismiss")),
        )
    } else {
        String::new()
    };
    format!("{}{dismiss}", link(ctx, item))
}

fn row(ctx: &UiCtx, item: &Value) -> String {
    let since = text(item, "since");
    let when = if since.is_empty() {
        String::new()
    } else {
        // Sign-in and run times are naive local; read them as such, not as UTC.
        let ago = match loopx_core::needs_you::parse_time(&since) {
            Some(t) => relative_time_dt(Utc::now(), t.fixed_offset()),
            None => relative_time(Utc::now(), &since),
        };
        format!(" <span class='muted'>· {}</span>", pyfmt::escape(&ago))
    };
    format!(
        "<li class='needs-you-item needs-you-{kind}'>{}<div class='needs-you-text'>{}{when}</div><div class='needs-you-actions'>{}</div></li>",
        icons::icon(icon_for(item["kind"].as_str().unwrap_or(""))),
        pyfmt::escape(&headline(ctx, item)),
        actions(ctx, item),
        kind = pyfmt::escape(&text(item, "kind")),
    )
}

fn body_html(ctx: &UiCtx) -> String {
    let (flash, ok) = ctx.flash();
    let items = items(ctx.state);
    let list = if items.is_empty() {
        format!(
            "<div class='card'><div class='empty-state'><div class='empty-state-icon'>{}</div><p class='empty-state-message'>{}</p></div></div>",
            icons::icon("check_circle"),
            pyfmt::escape(&ctx.t("Nothing needs you right now.")),
        )
    } else {
        let rows: String = items.iter().map(|i| row(ctx, i)).collect();
        format!("<div class='card'><ul class='needs-you-list'>{rows}</ul></div>")
    };
    format!(
        "<h1>{}</h1><p>{}</p>{}{list}",
        pyfmt::escape(&ctx.t("Needs you")),
        pyfmt::escape(&ctx.t(
            "What is waiting on you: approvals, escalated issues, broken sign-ins and failed loops. Each one alerted you once; it stays here until it is dealt with."
        )),
        flash_html(flash.as_deref(), ok),
    )
}

fn page(ctx: &UiCtx) -> UiResponse {
    UiResponse::html(render_shell(
        ctx,
        "Needs you · LoopX Engineering",
        "needs-you",
        &default_badge(ctx),
        &body_html(ctx),
        ShellOpts::default(),
    ))
}

/// The topbar bell: a count badge when anything is waiting.
pub fn bell_html(ctx: &UiCtx) -> String {
    let n = items(ctx.state).len();
    let label = pyfmt::escape(&ctx.tf("Needs you: {n}", &[("n", &n.to_string())]));
    let badge = if n == 0 {
        String::new()
    } else {
        format!("<span class='needs-you-count'>{}</span>", if n > 9 { "9+".to_string() } else { n.to_string() })
    };
    format!(
        "<a class='topbar-icon needs-you-bell' id='needs-you-bell' href='{PATH}' aria-label='{label}' title='{label}'>{}{badge}</a>",
        icons::icon("bell"),
    )
}

fn count(ctx: &UiCtx) -> UiResponse {
    UiResponse::json(200, &json!({"count": items(ctx.state).len()}))
}

fn post_approval(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let id = form.first("id").unwrap_or("");
    let decision = form.first("decision").unwrap_or("");
    let notes = form.first("notes").map(str::trim).filter(|n| !n.is_empty());
    let dir = paths(ctx.state).approvals_dir;
    match loopx_core::approvals::decide_with_notes(&dir, id, decision, notes, "dashboard", Utc::now()) {
        Ok(_) => UiResponse::flash_redirect(true, &ctx.t("Decision recorded; the loop picks it up within seconds."), PATH),
        Err(e) => UiResponse::flash_redirect(false, &e.to_string(), PATH),
    }
}

fn post_dismiss(ctx: &UiCtx) -> UiResponse {
    let id = ctx.form().first("id").unwrap_or("").to_string();
    let path = paths(ctx.state).dismissed;
    match loopx_core::needs_you::dismiss(&path, &id, Utc::now()) {
        Ok(()) => UiResponse::flash_redirect(true, &ctx.t("Dismissed until it happens again."), PATH),
        Err(e) => UiResponse::flash_redirect(false, &e, PATH),
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::routes::UiRequest;
    use crate::ui::state::UiState;
    use axum::http::Method;
    use loopx_core::approvals::{self, NewApproval};

    fn state(dir: &std::path::Path) -> UiState {
        UiState::explicit(dir, dir, dir, &dir.join("update"), "tok", None)
    }

    fn get(st: &UiState, path: &str) -> String {
        st.registry.dispatch(st, &UiRequest::get(path)).unwrap().body_text()
    }

    fn post(st: &UiState, path: &str, body: &str) -> crate::ui::routes::UiResponse {
        st.registry
            .dispatch(st, &UiRequest::new(Method::POST, path).with_body(body.to_string()))
            .unwrap()
    }

    fn approval(dir: &std::path::Path) -> String {
        let new = NewApproval {
            kind: "merge_request".into(),
            run_id: "r".into(),
            project: "web".into(),
            issue_iid: Some(7),
            title: "Fix <login>".into(),
            branch: "loop/issue-7".into(),
            target_branch: "main".into(),
            summary: "s".into(),
            timeout_seconds: 1800,
        };
        let a = approvals::create(&dir.join("outputs/approvals"), &new, chrono::Utc::now()).unwrap();
        a["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn nothing_waiting_says_so_and_the_bell_has_no_count() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let html = get(&st, "/needs-you");
        assert!(html.contains("Nothing needs you right now."), "{html}");
        assert!(html.contains("id='needs-you-bell'") && !html.contains("<span class='needs-you-count'>"), "{html}");
        assert_eq!(get(&st, "/needs-you/count"), r#"{"count": 0}"#);
    }

    #[test]
    fn a_pending_approval_is_listed_counted_and_decided_here() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let id = approval(d.path());
        let html = get(&st, "/needs-you");
        assert!(html.contains("Approve the fix for web #7: Fix &lt;login&gt;"), "{html}");
        assert!(html.contains("<span class='needs-you-count'>1</span>"), "{html}");
        assert!(html.contains("value='request_changes'"), "{html}");

        let body = format!("id={id}&decision=approve");
        assert_eq!(post(&st, "/needs-you/approval", &body).status, 403, "needs CSRF");
        // Request changes needs notes.
        let r = post(&st, "/needs-you/approval", &format!("csrf_token=tok&id={id}&decision=request_changes&notes=+"));
        assert!(r.header("location").unwrap_or_default().contains("ok=0"));
        let r = post(&st, "/needs-you/approval", &format!("csrf_token=tok&{body}"));
        assert_eq!(r.status, 303);
        assert!(r.header("location").unwrap_or_default().contains("ok=1"));
        let a = approvals::get(&d.path().join("outputs/approvals"), &id).unwrap();
        assert_eq!((a["status"].as_str(), a["decided_by"].as_str()), (Some("approved"), Some("dashboard")));
        assert_eq!(get(&st, "/needs-you/count"), r#"{"count": 0}"#);
    }

    #[test]
    fn a_signed_out_ai_cli_links_to_its_settings() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        std::fs::write(
            d.path().join("auth_health.json"),
            serde_json::json!({"checks": {"ai_cli": {"ok": false, "confirmed": true, "label": "AI CLI (claude)",
                "message": "claude is not signed in"}}})
            .to_string(),
        )
        .unwrap();
        let html = get(&st, "/needs-you");
        let button = format!("<a class='btn btn-neutral' href='{}'>", crate::ui::ai_cli::SETTINGS_HREF);
        assert!(html.contains(&button), "{html}");
    }

    #[test]
    fn a_sign_in_failing_since_a_local_time_says_how_long_ago() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let since = (chrono::Local::now() - chrono::Duration::hours(3)).naive_local().format("%Y-%m-%dT%H:%M:%S%.f");
        std::fs::write(
            d.path().join("auth_health.json"),
            serde_json::json!({"checks": {"mailbox:work": {"ok": false, "confirmed": true, "label": "work (gmail)",
                "message": "expired", "failing_since": since.to_string()}}})
            .to_string(),
        )
        .unwrap();
        let html = get(&st, "/needs-you");
        assert!(html.contains("· 3h ago"), "{html}");
    }

    #[test]
    fn a_dead_sign_in_can_be_dismissed() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        std::fs::write(
            d.path().join("auth_health.json"),
            serde_json::json!({"checks": {"mailbox:work": {"ok": false, "confirmed": true, "label": "work (gmail)",
                "message": "Google refresh failed (invalid_grant)", "failing_since": "2026-10-31T06:00:00"}}})
            .to_string(),
        )
        .unwrap();
        let html = get(&st, "/needs-you");
        assert!(html.contains("Sign-in needs attention: work (gmail)") && html.contains("needs-you-dismiss"), "{html}");
        assert_eq!(post(&st, "/needs-you/dismiss", "csrf_token=tok&id=sign_in%3Amailbox%3Awork").status, 303);
        assert!(get(&st, "/needs-you").contains("Nothing needs you right now."));
    }

    #[test]
    fn a_failed_loop_links_to_its_log_and_can_be_dismissed() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        loopx_core::status::write_status(
            &d.path().join("outputs/status/rss-loop.json"),
            "failed",
            &[("last_exit_code", serde_json::json!(1))],
        )
        .unwrap();
        let html = get(&st, "/needs-you");
        assert!(html.contains("rss-loop failed its last run") && html.contains("href='/runs?view=logs'"), "{html}");
        assert_eq!(post(&st, "/needs-you/dismiss", "id=loop_failed%3Arss-loop").status, 403);
        assert_eq!(post(&st, "/needs-you/dismiss", "csrf_token=tok&id=loop_failed%3Arss-loop").status, 303);
        assert!(get(&st, "/needs-you").contains("Nothing needs you right now."));
    }
}
