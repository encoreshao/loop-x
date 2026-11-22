//! Needs you: `/needs-you` lists everything waiting on a person
//! (`loopx_core::needs_you`) with what to do about each: Allow / Deny /
//! Request changes for an approval, a link for the rest, Dismiss for an
//! escalation, a failed loop or a dead sign-in, and for a merge-request review
//! thread the MR-feedback round handed over: Approve the plan, Post my reply,
//! Plan again with my instructions, or Dismiss (`POST /needs-you/mr-thread`,
//! `loopx_core::mr_feedback::decide_thread`), and for a spec the issue loop
//! posted: Approve, Request changes or Dismiss (`POST /needs-you/spec`,
//! `loopx_core::spec_gate::decide` / `dismiss`). The topbar bell (`bell_html`)
//! counts them and polls `/needs-you/count`.

use crate::snapshot::{self, Paths};
use crate::ui::badge::default_badge;
use crate::ui::pages::loops_common::{flash_html, relative_time, relative_time_dt};
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::shell::{render_shell, ShellOpts};
use crate::ui::state::UiState;
use crate::ui::{icons, pyfmt};
use chrono::Utc;
use loopx_core::issue_runner::RunCtx;
use loopx_core::loop_definition::LoopDefinition;
use loopx_core::mr_feedback::{decide_thread, RoundDeps, UserChoice};
use loopx_core::mr_threads::{IssueThreads, ThreadStatus};
use serde_json::{json, Value};
use std::path::PathBuf;

const PATH: &str = "/needs-you";

pub fn register(r: &mut Registry) {
    r.get(PATH, page);
    r.get("/needs-you/count", count);
    r.post("/needs-you/approval", post_approval);
    r.post("/needs-you/dismiss", post_dismiss);
    r.post("/needs-you/mr-thread", post_mr_thread);
    r.post("/needs-you/spec", post_spec);
}

fn memory_root(state: &UiState) -> PathBuf {
    state.home_dir.join("memory")
}

pub(crate) fn paths(state: &UiState) -> Paths {
    let outputs = state.repo_root.join("outputs");
    Paths {
        events_dir: state.events_dir.clone(),
        approvals_dir: outputs.join("approvals"),
        queue_path: state.home_dir.join("run_queue.json"),
        auth_state: state.home_dir.join("auth_health.json"),
        dismissed: state.home_dir.join("needs_you_dismissed.json"),
        memory_root: memory_root(state),
        creator_jobs_dir: loopx_core::creator::jobs::jobs_dir(&state.home_dir),
        release_jobs_dir: loopx_core::release_mr::job::jobs_dir(&state.home_dir),
        mr_fix_jobs_dir: loopx_core::mr_fix::jobs::jobs_dir(&state.home_dir),
        loops_config: state.loops_config_path(),
        user_home: state.user_home.clone(),
        outputs,
    }
}

pub(crate) fn items(state: &UiState) -> Vec<Value> {
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

/// `ny-<id>` with every byte outside `[A-Za-z0-9-]` as `_XX`, so distinct ids
/// never share an anchor and the result is safe in an attribute and a URL.
pub(crate) fn anchor_id(id: &str) -> String {
    let mut out = String::from("ny-");
    for b in id.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' {
            out.push(b as char);
        } else {
            out.push_str(&format!("_{b:02X}"));
        }
    }
    out
}

/// The item's line, translated.
pub(crate) fn headline(ctx: &UiCtx, item: &Value) -> String {
    let f = |k: &str| text(item, k);
    match item["kind"].as_str() {
        Some("approval") if item["approval_kind"] == "profile-section" => ctx.tf(
            "Review the proposed {section} change to the {project} profile",
            &[("section", &f("section")), ("project", &f("project"))],
        ),
        Some("approval") if item["approval_kind"] == loopx_core::guide_proposal::KIND => ctx.tf(
            "Review the proposed change to the {slug} guide page of {project}",
            &[("slug", &f("slug")), ("project", &f("project"))],
        ),
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
        Some("profile_sync_failed") => {
            let error = crate::ui::profile_jobs::failure_text(ctx, &f("message"));
            ctx.tf("Profile sync failed for {alias}: {error}", &[("alias", &f("project")), ("error", &error)])
        }
        Some("slack_listener") => ctx.tf("The Slack listener needs attention: {error}", &[("error", &f("error"))]),
        Some("mr_thread") => ctx.tf(
            "{project} #{iid}: a review thread on the merge request needs you ({reason})",
            &[("project", &f("project")), ("iid", &f("issue_iid")), ("reason", &f("reason"))],
        ),
        Some("spec") => ctx.tf(
            "Approve the spec for {project} #{iid}",
            &[("project", &f("project")), ("iid", &f("issue_iid"))],
        ),
        Some("mr_autofix") => ctx.tf(
            "Automatic fix stopped on {mr}: {reason}",
            &[("mr", &f("mr_ref")), ("reason", &autofix_reason(ctx, &f("reason")))],
        ),
        _ => f("title"),
    }
}

fn autofix_reason(ctx: &UiCtx, reason: &str) -> String {
    match reason {
        "already_attempted" => ctx.t("it already had its one automatic attempt"),
        "fix_failed" => ctx.t("the automatic fix failed"),
        "fix_nothing" => ctx.t("the automatic fix changed nothing"),
        "branch_moved" => ctx.t("the branch moved after the failure was diagnosed"),
        "already_handled" => ctx.t("an automatic CI fix was already made on it"),
        "daily_cap" => ctx.t("the daily limit of automatic fixes was reached"),
        "low_confidence" => ctx.t("the diagnosis was not confident enough"),
        "job_busy" => ctx.t("a Fix now run was already in progress"),
        "verify_failed" => ctx.t("GitLab could not be checked"),
        "ledger_corrupt" => ctx.t("its attempt ledger is unreadable"),
        _ => ctx.t("it needs a human"),
    }
}

fn icon_for(kind: &str) -> &'static str {
    match kind {
        "approval" => "task_alt",
        "escalated" => "warning",
        "sign_in" => "login",
        "mr_thread" | "slack_listener" => "forum",
        "spec" => "fact_check",
        "mr_autofix" => "git_pull_request",
        _ => "error",
    }
}

/// Where to go to deal with an item that has no button of its own.
fn link(ctx: &UiCtx, item: &Value) -> String {
    let (href, label) = match item["kind"].as_str() {
        Some("escalated") => ("/loops/issue-loop?view=tasks".to_string(), ctx.t("Open in the issue loop's tasks")),
        Some("sign_in") if text(item, "probe").starts_with("mailbox:") => {
            ("/loops/inbox-triage-loop?view=setup".to_string(), ctx.t("Reconnect"))
        }
        Some("sign_in") if text(item, "probe") == "ai_cli" => (crate::ui::ai_cli::SETTINGS_HREF.to_string(), ctx.t("Open AI CLI settings")),
        Some("sign_in") => ("/connectors".to_string(), ctx.t("Reconnect")),
        Some("loop_failed") => (sheet_href(&text(item, "loop")), ctx.t("See the log")),
        Some("slack_listener") => (
            crate::ui::pages::slack_agent::listener_href(ctx.state),
            ctx.t("Open Slack settings"),
        ),
        Some("profile_sync_failed") => (
            crate::ui::pages::memory::project_url(&text(item, "project"), None),
            ctx.t("Open the project's memory"),
        ),
        Some("mr_autofix") => {
            let url = text(item, "mr_web_url");
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return String::new();
            }
            return format!(
                "<a class='btn btn-neutral' href='{}' target='_blank' rel='noopener'>{}</a>",
                pyfmt::escape(&url),
                pyfmt::escape(&ctx.t("Open the merge request")),
            );
        }
        _ => return String::new(),
    };
    format!("<a class='btn btn-neutral' href='{}'>{}</a>", pyfmt::escape(&href), pyfmt::escape(&label))
}

/// Whether `item` is a guide page proposal that can no longer be applied: the page
/// changed (or went) after it was raised. It offers Dismiss and the project's
/// proposals, not an Approve button that can only fail.
fn stale_guide_proposal(ctx: &UiCtx, item: &Value) -> bool {
    if item["kind"] != "approval" || item["approval_kind"] != loopx_core::guide_proposal::KIND {
        return false;
    }
    let dir = paths(ctx.state).approvals_dir;
    loopx_core::approvals::get(&dir, &text(item, "approval_id"))
        .is_some_and(|a| !loopx_core::guide_proposal::still_applies(&a, Some(&memory_root(ctx.state))))
}

fn stale_note(ctx: &UiCtx) -> String {
    format!(
        "<p class='muted needs-you-note'>{}</p>",
        pyfmt::escape(&ctx.t(
            "The page changed after this was proposed, so it can no longer be applied. Dismiss it and rebuild the page for a new proposal."
        ))
    )
}

fn actions(ctx: &UiCtx, item: &Value, stale: bool) -> String {
    let csrf = ctx.csrf_input();
    if stale {
        return format!(
            "<form method='post' action='/needs-you/approval' class='needs-you-approval'>{csrf}\
<input type='hidden' name='id' value='{}'>\
<button type='submit' name='decision' value='deny' class='btn btn-neutral'>{}</button></form>\
<a class='btn btn-neutral' href='{}'>{}</a>",
            pyfmt::escape(&text(item, "approval_id")),
            pyfmt::escape(&ctx.t("Dismiss")),
            pyfmt::escape(&crate::ui::pages::memory::project_url(&text(item, "project"), Some("proposals"))),
            pyfmt::escape(&ctx.t("Open in Memory")),
        );
    }
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

/// What the user needs to decide a review thread: links, the reviewer's
/// comment, the plan and why it stopped. Everything but the labels comes
/// from GitLab or a model and is escaped.
fn mr_thread_details(ctx: &UiCtx, item: &Value, prefixes: &crate::ui::markdown::GitlabPrefixes) -> String {
    let f = |k: &str| text(item, k);
    let mut links = String::new();
    if let Some(base) = prefixes.get(&f("project")) {
        let base = pyfmt::escape(base);
        links.push_str(&format!(
            "<a href='{base}/-/issues/{iid}' target='_blank' rel='noopener'>{}</a>",
            pyfmt::escape(&ctx.tf("Issue #{iid}", &[("iid", &f("issue_iid"))])),
            iid = pyfmt::escape(&f("issue_iid")),
        ));
        if !f("mr_iid").is_empty() {
            links.push_str(&format!(
                " · <a href='{base}/-/merge_requests/{mr}#note_{note}' target='_blank' rel='noopener'>{}</a>",
                pyfmt::escape(&ctx.tf("Thread on merge request !{mr}", &[("mr", &f("mr_iid"))])),
                mr = pyfmt::escape(&f("mr_iid")),
                note = pyfmt::escape(&f("last_note_id")),
            ));
        }
        links = format!("<p class='needs-you-links'>{links}</p>");
    }
    let comment = if f("comment_body").is_empty() {
        String::new()
    } else {
        format!(
            "<blockquote class='needs-you-comment'><span class='muted'>{}</span>\n{}</blockquote>",
            pyfmt::escape(&ctx.tf("{author} wrote:", &[("author", &f("comment_author"))])),
            pyfmt::escape(&f("comment_body")),
        )
    };
    let confidence = if f("confidence").is_empty() { "-".to_string() } else { format!("{}%", f("confidence")) };
    let fact = |label: &str, value: &str| {
        format!("<dt>{}</dt><dd>{}</dd>", pyfmt::escape(label), pyfmt::escape(value))
    };
    format!(
        "<div class='needs-you-thread'>{links}{comment}<dl class='needs-you-facts'>{}{}{}</dl></div>",
        fact(&ctx.t("Plan"), &f("plan_summary")),
        fact(&ctx.t("Confidence"), &confidence),
        fact(&ctx.t("Why it stopped"), &f("reason")),
    )
}

/// The four decisions on a review thread, in one form.
fn mr_thread_actions(ctx: &UiCtx, item: &Value) -> String {
    let approve = if text(item, "action").is_empty() {
        // No usable plan: there is nothing to approve.
        String::new()
    } else {
        format!(
            "<button type='submit' name='decision' value='approve' class='btn btn-primary'>{}</button>",
            pyfmt::escape(&ctx.t("Approve the plan")),
        )
    };
    format!(
        "<form method='post' action='/needs-you/mr-thread' class='daemon-action-form needs-you-thread-form'>{csrf}\
<input type='hidden' name='id' value='{id}'>\
<label>{reply_label}<textarea name='reply' rows='4' maxlength='10000'>{reply}</textarea></label>\
<label>{instructions_label}<textarea name='instructions' rows='3' maxlength='4000' placeholder='{placeholder}'></textarea></label>\
<p class='muted'>{hint}</p>\
<div class='needs-you-buttons'>{approve}\
<button type='submit' name='decision' value='edit_reply' class='btn btn-neutral'>{post}</button>\
<button type='submit' name='decision' value='instructions' class='btn btn-neutral'>{again}</button>\
<button type='submit' name='decision' value='dismiss' class='btn btn-neutral'>{dismiss}</button></div></form>",
        csrf = ctx.csrf_input(),
        id = pyfmt::escape(&text(item, "id")),
        reply_label = pyfmt::escape(&ctx.t("Draft reply")),
        reply = pyfmt::escape(&text(item, "reply")),
        instructions_label = pyfmt::escape(&ctx.t("Your instructions")),
        placeholder = pyfmt::escape(&ctx.t("What should the loop do instead?")),
        hint = pyfmt::escape(&ctx.t(
            "Approve carries out the plan as drafted. Post my reply sends the text above instead. Plan again hands your instructions to the loop's next run. Dismiss posts nothing."
        )),
        post = pyfmt::escape(&ctx.t("Post my reply")),
        again = pyfmt::escape(&ctx.t("Plan again with my instructions")),
        dismiss = pyfmt::escape(&ctx.t("Dismiss")),
    )
}

/// A posted spec: its opening text (escaped, it came from the agent), a link to
/// the issue where the whole spec is, and Approve, Request changes or Dismiss.
fn spec_details(ctx: &UiCtx, item: &Value, prefixes: &crate::ui::markdown::GitlabPrefixes) -> String {
    let f = |k: &str| text(item, k);
    let more = if item["body_truncated"] == true { "\u{2026}" } else { "" };
    let link = match prefixes.get(&f("project")) {
        Some(base) => format!(
            "<p class='needs-you-links'><a href='{}/-/issues/{}' target='_blank' rel='noopener'>{}</a></p>",
            pyfmt::escape(base),
            pyfmt::escape(&f("issue_iid")),
            pyfmt::escape(&ctx.t("Read the whole spec on the issue")),
        ),
        None => String::new(),
    };
    format!(
        "<pre class='needs-you-spec-text'>{}{more}</pre>{link}",
        pyfmt::escape(&f("body"))
    )
}

fn spec_actions(ctx: &UiCtx, item: &Value) -> String {
    format!(
        "<form method='post' action='/needs-you/spec' class='needs-you-spec-form'>{csrf}\
<input type='hidden' name='id' value='{id}'>\
<label>{notes_label}<textarea name='notes' rows='3' maxlength='4000' required placeholder='{placeholder}'></textarea></label>\
<div class='needs-you-buttons'>\
<button type='submit' name='decision' value='approve' class='btn btn-primary' formnovalidate>{approve}</button>\
<button type='submit' name='decision' value='request_changes' class='btn btn-neutral'>{changes}</button>\
<button type='submit' name='decision' value='dismiss' class='btn btn-neutral' formnovalidate>{dismiss}</button></div></form>",
        csrf = ctx.csrf_input(),
        id = pyfmt::escape(&text(item, "id")),
        notes_label = pyfmt::escape(&ctx.t("What should change (for Request changes)")),
        placeholder = pyfmt::escape(&ctx.t("What the next spec should do differently")),
        approve = pyfmt::escape(&ctx.t("Approve")),
        changes = pyfmt::escape(&ctx.t("Request changes")),
        dismiss = pyfmt::escape(&ctx.t("Dismiss")),
    )
}

/// How long ago the item started, or "" when it has no time.
fn ago(item: &Value) -> String {
    let since = text(item, "since");
    if since.is_empty() {
        return since;
    }
    // Sign-in and run times are naive local; read them as such, not as UTC.
    match loopx_core::needs_you::parse_time(&since) {
        Some(t) => relative_time_dt(Utc::now(), t.fixed_offset()),
        None => relative_time(Utc::now(), &since),
    }
}

fn sheet_href(loop_name: &str) -> String {
    format!("{PATH}?log={}", pyfmt::quote(loop_name, ""))
}

/// The end of a failed run's own output: from where the run started writing in
/// the day's shared history log, at most the last 64 KB. `None` when the status
/// names no log, or one that is not a plain file name in `outputs/history`.
fn failed_run_output(state: &UiState, status: &Value) -> Option<(String, String)> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 64 * 1024;
    let name = status["last_log"].as_str()?;
    let plain = std::path::Path::new(name).file_name().is_some_and(|f| f == name);
    if !plain || !name.ends_with(".log") {
        return None;
    }
    let mut f = std::fs::File::open(state.repo_root.join("outputs/history").join(name)).ok()?;
    let len = f.metadata().ok()?.len();
    let start = status["last_log_offset"].as_u64().unwrap_or(0).min(len);
    f.seek(SeekFrom::Start(start.max(len.saturating_sub(TAIL)))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some((format!("outputs/history/{name}"), String::from_utf8_lossy(&buf).trim_end().to_string()))
}

/// The right-hand sheet behind a failed loop's "See the log": what failed, its
/// recent lines in the unified log and the run's own output.
fn loop_log_sheet(ctx: &UiCtx, items: &[Value]) -> String {
    let Some(name) = ctx.query().first("log").map(str::to_string) else {
        return String::new();
    };
    let Some(item) = items.iter().find(|i| i["kind"] == "loop_failed" && text(i, "loop") == name) else {
        return String::new();
    };
    let status = snapshot::loop_statuses(&paths(ctx.state).outputs).remove(&name).unwrap_or(Value::Null);
    let mut facts = vec![ago(item)];
    if let Some(code) = item["exit_code"].as_i64() {
        facts.push(ctx.tf("Exit code {code}", &[("code", &code.to_string())]));
    }
    if let Some(n) = item["consecutive_failures"].as_i64().filter(|n| *n > 1) {
        facts.push(ctx.tf("Failed {n} runs in a row", &[("n", &n.to_string())]));
    }
    facts.retain(|f| !f.is_empty());
    let facts = pyfmt::escape(&facts.join(" · "));

    let log_path = loopx_core::run_history::unified_log_path(&ctx.state.repo_root);
    let tail = loopx_core::run_history::read_unified_log_tail(2000, &log_path).unwrap_or_default();
    let prefixes = crate::ui::gitlab::issue_url_prefixes(ctx.state);
    let mut entries: Vec<_> = loopx_core::run_history::parse_unified_log_entries(&tail)
        .into_iter()
        .filter(|e| e.source.as_deref() == Some(name.as_str()))
        .collect();
    entries.reverse();
    entries.truncate(20);
    let entries = if entries.is_empty() {
        format!("<p class='muted'>{}</p>", pyfmt::escape(&ctx.t("No log entries yet.")))
    } else {
        let inner: String = entries.iter().map(|e| crate::ui::pages::runs::log_entry_html(e, &prefixes)).collect();
        format!("<div class='log-entries'>{inner}</div>")
    };
    let output = match failed_run_output(ctx.state, &status) {
        Some((path, out)) if !out.is_empty() => format!(
            "<p class='ny-sheet-path'><code>{}</code></p><pre class='ny-sheet-output'>{}</pre>",
            pyfmt::escape(&path),
            pyfmt::escape(&out),
        ),
        _ => format!("<p class='muted'>{}</p>", pyfmt::escape(&ctx.t("The output of this run was not recorded."))),
    };
    let close = pyfmt::escape(&ctx.t("Close"));
    format!(
        "<a class='ny-sheet-backdrop' href='{PATH}' aria-label='{close}' tabindex='-1'></a>\
<aside class='ny-sheet' role='dialog' aria-modal='true' aria-labelledby='ny-sheet-title'>\
<header class='ny-sheet-head'><div><h2 id='ny-sheet-title'>{title}</h2><p class='muted'>{facts}</p></div>\
<a class='ny-sheet-close' href='{PATH}' aria-label='{close}' title='{close}'>{x}</a></header>\
<section class='ny-sheet-section'><h3>{output_label}</h3>{output}</section>\
<section class='ny-sheet-section'><h3>{entries_label}</h3>{entries}</section>\
<footer class='ny-sheet-foot'>\
<form method='post' action='/needs-you/dismiss'>{csrf}<input type='hidden' name='id' value='{id}'><button type='submit' class='btn btn-neutral'>{dismiss}</button></form>\
<a class='btn btn-neutral' href='/runs?view=logs'>{all}</a></footer></aside>\
<script>(function(){{var o=document.querySelector('.ny-sheet-output');if(o)o.scrollTop=o.scrollHeight;\
document.addEventListener('keydown',function(e){{if(e.key==='Escape')location.href='{PATH}';}});}})();</script>",
        title = pyfmt::escape(&headline(ctx, item)),
        x = icons::icon("close"),
        output_label = pyfmt::escape(&ctx.t("Run output")),
        entries_label = pyfmt::escape(&ctx.t("Log entries")),
        csrf = ctx.csrf_input(),
        id = pyfmt::escape(&text(item, "id")),
        dismiss = pyfmt::escape(&ctx.t("Dismiss")),
        all = pyfmt::escape(&ctx.t("Open all logs")),
    )
}

fn row(ctx: &UiCtx, item: &Value, prefixes: &crate::ui::markdown::GitlabPrefixes) -> String {
    let since = ago(item);
    let when = if since.is_empty() {
        String::new()
    } else {
        format!(" <span class='muted'>· {}</span>", pyfmt::escape(&since))
    };
    let stale = stale_guide_proposal(ctx, item);
    let (details, actions) = if item["kind"] == "mr_thread" {
        (mr_thread_details(ctx, item, prefixes), mr_thread_actions(ctx, item))
    } else if item["kind"] == "spec" {
        (spec_details(ctx, item, prefixes), spec_actions(ctx, item))
    } else if stale {
        (stale_note(ctx), actions(ctx, item, true))
    } else {
        (String::new(), actions(ctx, item, false))
    };
    format!(
        "<li id='{anchor}' class='needs-you-item needs-you-{kind}'>{}<div class='needs-you-text'>{}{when}{details}</div><div class='needs-you-actions'>{actions}</div></li>",
        icons::icon(icon_for(item["kind"].as_str().unwrap_or(""))),
        pyfmt::escape(&headline(ctx, item)),
        anchor = anchor_id(&text(item, "id")),
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
        let prefixes = if items.iter().any(|i| i["kind"] == "mr_thread" || i["kind"] == "spec") {
            crate::ui::gitlab::issue_url_prefixes(ctx.state)
        } else {
            Default::default()
        };
        let rows: String = items.iter().map(|i| row(ctx, i, &prefixes)).collect();
        format!("<div class='card'><ul class='needs-you-list'>{rows}</ul></div>")
    };
    let sheet = loop_log_sheet(ctx, &items);
    format!(
        "<h1>{}</h1><p>{}</p>{}{list}{sheet}",
        pyfmt::escape(&ctx.t("Needs you")),
        pyfmt::escape(&ctx.t(
            "What is waiting on you: approvals, escalated issues, review threads on the loop's merge requests, broken sign-ins and failed loops. Each one alerted you once; it stays here until it is dealt with."
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
    // A profile or guide page proposal is only worth approving if it is applied: the
    // shared helper approves it (by any approve verb) only while it still applies.
    let kind = loopx_core::approvals::get(&dir, id).map(|a| a["kind"].clone());
    let applies = kind.as_ref().is_some_and(|k| loopx_core::proposals::applies(k, decision));
    let done = if !applies {
        ctx.t("Decision recorded; the loop picks it up within seconds.")
    } else if kind.as_ref().is_some_and(|k| k == loopx_core::guide_proposal::KIND) {
        ctx.t("Proposal approved and applied to the guide page.")
    } else {
        ctx.t("Proposal approved and applied to the profile.")
    };
    let root = memory_root(ctx.state);
    match loopx_core::proposals::decide(id, decision, notes, "dashboard", Some(&root), &dir, Utc::now()) {
        Ok(_) => UiResponse::flash_redirect(true, &done, PATH),
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

/// `mr_thread:<alias>:<iid>:<discussion id>`.
fn parse_thread_id(id: &str) -> Option<(String, i64, String)> {
    let rest = id.strip_prefix("mr_thread:")?;
    let mut parts = rest.rsplitn(3, ':');
    let discussion = parts.next().filter(|d| !d.is_empty())?;
    let iid = parts.next()?.parse().ok()?;
    let alias = parts.next().filter(|a| !a.is_empty())?;
    Some((alias.to_string(), iid, discussion.to_string()))
}

/// `spec:<alias>:<iid>`, with an alias that names a folder and a numeric iid.
fn parse_spec_id(id: &str) -> Option<(String, String)> {
    let (alias, iid) = id.strip_prefix("spec:")?.rsplit_once(':')?;
    let alias_ok = !alias.is_empty() && alias != "." && alias != ".." && !alias.contains(['/', '\\']);
    let iid_ok = !iid.is_empty() && iid.bytes().all(|b| b.is_ascii_digit());
    (alias_ok && iid_ok).then(|| (alias.to_string(), iid.to_string()))
}

/// Approve a posted spec, or ask for a new one with notes. The issue loop's
/// next run implements it, or writes the new spec.
fn post_spec(ctx: &UiCtx) -> UiResponse {
    use loopx_core::spec_gate::{self, SpecStatus};
    let form = ctx.form();
    let fail = |message: &str| UiResponse::flash_redirect(false, message, PATH);
    let root = memory_root(ctx.state);
    let waiting = parse_spec_id(form.first("id").unwrap_or(""))
        .filter(|(alias, iid)| spec_gate::load(&root, alias, iid).is_some_and(|s| s.status == SpecStatus::Awaiting));
    let Some((alias, iid)) = waiting else {
        return fail(&ctx.t("That spec is not waiting on you."));
    };
    if form.first("decision") == Some("dismiss") {
        return match spec_gate::dismiss(&root, &alias, &iid, "dashboard", Utc::now()) {
            Ok(_) => UiResponse::flash_redirect(true, &ctx.t("Spec dismissed \u{2014} the issue runs as usual."), PATH),
            Err(e) => fail(&e),
        };
    }
    let approve = match form.first("decision") {
        Some("approve") => true,
        Some("request_changes") => false,
        _ => return fail(&ctx.t("Choose what to do with the spec.")),
    };
    let notes = form.first("notes").unwrap_or("").trim().to_string();
    if !approve && notes.is_empty() {
        return fail(&ctx.t("Say what should change in the spec."));
    }
    match spec_gate::decide(&root, &alias, &iid, approve, &notes, "dashboard", Utc::now()) {
        Ok(_) if approve => UiResponse::flash_redirect(true, &ctx.t("Spec approved \u{2014} the next run implements it."), PATH),
        Ok(_) => UiResponse::flash_redirect(true, &ctx.t("Changes requested \u{2014} the next run writes a new spec."), PATH),
        Err(e) => fail(&e),
    }
}

/// The run context a decision works in: this dashboard's checkout, config,
/// task memory and event ledger.
fn run_ctx(state: &UiState) -> RunCtx {
    let mut ctx = RunCtx::new(state.repo_root.clone());
    ctx.events_dir = Some(state.events_dir.clone());
    ctx.config_path = Some(state.home_dir.join("projects.json"));
    ctx.memory_root = Some(memory_root(state));
    ctx
}

fn thread_deps(
    state: &UiState,
    ctx: &RunCtx,
    definition: &LoopDefinition,
    alias: &str,
    iid: i64,
) -> Result<RoundDeps, String> {
    let iid = iid.to_string();
    let deps = match &state.mr_feedback_deps {
        Some(f) => f(ctx, definition, alias, &iid),
        None => RoundDeps::real(ctx, alias, &iid, definition),
    };
    deps?.ok_or_else(|| format!("{alias} has no GitLab merge request to act on"))
}

/// Approve, Post my reply, Plan again with my instructions, or Dismiss, for a
/// review thread waiting on the user. Approving can run the agent for
/// minutes, so it runs off the request: the thread leaves the list when it
/// is done, or comes back with the reason it stopped.
fn post_mr_thread(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let field = |k: &str| form.first(k).unwrap_or("").to_string();
    let fail = |message: &str| UiResponse::flash_redirect(false, message, PATH);
    let Some((alias, iid, discussion)) = parse_thread_id(&field("id")) else {
        return fail(&ctx.t("That review thread is not waiting on you."));
    };
    let choice = match field("decision").as_str() {
        "approve" => UserChoice::Approve,
        "edit_reply" => UserChoice::EditReply(field("reply")),
        "dismiss" => UserChoice::Dismiss,
        "instructions" => UserChoice::Instructions(field("instructions")),
        _ => return fail(&ctx.t("Choose what to do with the thread.")),
    };
    let state = ctx.state;
    let stored = IssueThreads::load(&alias, iid, Some(&memory_root(state)));
    let Some(thread) = stored.threads.get(&discussion).filter(|t| t.status == ThreadStatus::NeedsYou) else {
        return fail(&ctx.t("That review thread is not waiting on you."));
    };
    if choice == UserChoice::Approve {
        if let Err(e) = loopx_core::mr_feedback::approvable(thread) {
            return fail(&e);
        }
        // Approve posts the stored draft: text edited in the box would be
        // silently dropped, so it has to go through Post my reply.
        let normalize = |s: &str| s.replace("\r\n", "\n").trim().to_string();
        let draft = thread
            .plan
            .as_ref()
            .and_then(|p| p.get("reply"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if form.first("reply").is_some_and(|r| normalize(r) != normalize(draft)) {
            return fail(&ctx.t("You changed the draft reply. Approve posts the draft as written; to send your text, use Post my reply."));
        }
    }
    let run = run_ctx(state);
    let definition = match LoopDefinition::from_yaml(&run.definition_path()) {
        Ok(d) => d,
        Err(e) => return fail(&e.to_string()),
    };
    if choice == UserChoice::Approve {
        let lock = loopx_core::file_lock::issue_lock_path(&run.repo_root, &alias, &iid.to_string());
        if loopx_core::file_lock::is_held(&lock) {
            return fail(&ctx.t("A run is working on this issue right now; try again when it finishes."));
        }
        let factory = state.mr_feedback_deps.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("mr-decision-{alias}-{iid}"))
            .spawn(move || {
                let iid_text = iid.to_string();
                let deps = match &factory {
                    Some(f) => f(&run, &definition, &alias, &iid_text),
                    None => RoundDeps::real(&run, &alias, &iid_text, &definition),
                };
                // decide_thread logs why a decision was not carried out.
                if let Ok(Some(deps)) = deps {
                    let _ = decide_thread(&run, &definition, &alias, iid, &discussion, UserChoice::Approve, &deps);
                }
            });
        return match spawned {
            Ok(_) => UiResponse::flash_redirect(
                true,
                &ctx.t("Approved. The loop is carrying out the plan now; the thread leaves this list when it is done, or comes back with the reason if it stops."),
                PATH,
            ),
            Err(e) => fail(&e.to_string()),
        };
    }
    let message = match &choice {
        UserChoice::EditReply(_) => ctx.t("Your reply was posted in the thread."),
        UserChoice::Instructions(_) => ctx.t("Instructions saved; the loop plans the thread again on its next run."),
        _ => ctx.t("Dismissed. The thread comes back only if someone writes in it again."),
    };
    let result = thread_deps(state, &run, &definition, &alias, iid)
        .and_then(|deps| decide_thread(&run, &definition, &alias, iid, &discussion, choice, &deps));
    match result {
        Ok(()) => UiResponse::flash_redirect(true, &message, PATH),
        Err(e) => fail(&e),
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
            target: None,
        };
        let a = approvals::create(&dir.join("outputs/approvals"), &new, chrono::Utc::now()).unwrap();
        a["id"].as_str().unwrap().to_string()
    }

    /// A profile with a Stack section and a pending proposal to change it.
    fn profile_proposal(dir: &std::path::Path, memory: &std::path::Path) -> String {
        use loopx_core::project_profile::{self, Origin, Profile, Section, SectionKey, Status};
        let mut sections = std::collections::BTreeMap::new();
        sections.insert(
            SectionKey::Stack,
            Section { summary: "s".into(), body: "Old.".into(), last_verified: None, sources: vec![], origin: Origin::Agent },
        );
        let p = Profile { alias: "ny-profile-web".into(), generated_at: None, head_sha: None, status: Status::Approved, links: vec![], sections };
        project_profile::save(&p, Some(memory)).unwrap();
        let a = approvals::create(
            dir,
            &NewApproval {
                kind: loopx_core::profile_sync::KIND.into(),
                project: "ny-profile-web".into(),
                title: "Update the stack section of the web profile".into(),
                timeout_seconds: 3600,
                target: Some(serde_json::json!({"section": "stack", "alias": "ny-profile-web", "old": "Old.", "old_summary": "s",
                    "new": "New.", "new_summary": "n", "sources": []})),
                ..Default::default()
            },
            chrono::Utc::now(),
        )
        .unwrap();
        a["id"].as_str().unwrap().to_string()
    }

    /// An approved `feature-orders` page of `alias` and a pending `guide-page`
    /// proposal rewriting its `what` part.
    fn guide_proposal(alias: &str, apr: &std::path::Path, memory: &std::path::Path) -> String {
        use loopx_core::project_guide::{self, Kind, Page, Part};
        use loopx_core::project_profile::{Origin, Status};
        let page = Page {
            alias: alias.into(),
            slug: "feature-orders".into(),
            kind: Kind::Feature,
            title: "Orders".into(),
            status: Status::Approved,
            generated_at: None,
            head_sha: Some("abc123".into()),
            summary: "Taking orders.".into(),
            related: vec![],
            parts: vec![Part { key: "what".into(), body: "Old what.".into(), origin: Origin::Agent }],
            sources: vec![],
        };
        project_guide::save(&page, Some(memory)).unwrap();
        let mut held = page;
        held.status = Status::Draft;
        held.parts[0].body = "New what.".into();
        loopx_core::guide_proposal::raise(alias, &held, apr, Some(memory), chrono::Utc::now()).unwrap().expect("a proposal")
    }

    fn guide_what(alias: &str, memory: &std::path::Path) -> String {
        loopx_core::project_guide::load(alias, "feature-orders", Some(memory)).unwrap().unwrap().parts[0].body.clone()
    }

    #[test]
    fn a_guide_page_proposal_reads_as_a_page_review_in_every_language() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        guide_proposal("ny-guide-read", &d.path().join("outputs/approvals"), &d.path().join("memory"));
        let html = get(&st, "/needs-you");
        assert!(html.contains("Review the proposed change to the feature-orders guide page of ny-guide-read"), "{html}");
        assert!(!html.contains("Approve the fix for"), "{html}");
        let req = crate::ui::routes::UiRequest::get("/needs-you").with_header("cookie", "loop_lang=fr");
        let fr = st.registry.dispatch(&st, &req).unwrap().body_text();
        assert!(
            fr.contains("Examiner la modification propos\u{e9}e de la page de guide feature-orders de ny-guide-read"),
            "{fr}"
        );
    }

    #[test]
    fn needs_you_shows_no_approve_button_for_a_stale_guide_proposal() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let apr = d.path().join("outputs/approvals");
        let memory = d.path().join("memory");
        let fresh = guide_proposal("ny-guide-fresh", &apr, &memory);
        let stale = guide_proposal("ny-guide-stale", &apr, &memory);
        // The page is changed after the proposal was raised.
        let file = loopx_core::project_guide::guide_dir("ny-guide-stale", Some(&memory)).join("feature-orders.md");
        let text = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, text.replace("Old what.", "Hand what.")).unwrap();
        let html = get(&st, "/needs-you");
        let item = |id: &str| -> String {
            let at = html.find(&format!("value='{id}'")).unwrap_or_else(|| panic!("no item {id}: {html}"));
            let start = html[..at].rfind("<li").unwrap();
            let end = at + html[at..].find("</li>").unwrap();
            html[start..end].to_string()
        };
        let f = item(&fresh);
        assert!(f.contains("name='decision' value='approve'"), "{f}");
        assert!(!f.contains("can no longer be applied"), "{f}");
        let s = item(&stale);
        assert!(!s.contains("value='approve'") && !s.contains("value='request_changes'"), "{s}");
        assert!(s.contains("The page changed after this was proposed, so it can no longer be applied. Dismiss it and rebuild the page for a new proposal."), "{s}");
        assert!(s.contains("name='decision' value='deny'") && s.contains(">Dismiss<"), "{s}");
        assert!(s.contains("href='/insights?view=memory&amp;project=ny-guide-stale&amp;page=proposals'"), "{s}");
        // Dismissing it here records a denial and writes nothing.
        let r = post(&st, "/needs-you/approval", &format!("csrf_token=tok&id={stale}&decision=deny"));
        assert!(r.header("location").unwrap_or_default().contains("ok=1"));
        assert_eq!(approvals::get(&apr, &stale).unwrap()["status"], "denied");
        assert_eq!(guide_what("ny-guide-stale", &memory), "Hand what.");
        // Translated.
        let req = crate::ui::routes::UiRequest::get("/needs-you").with_header("cookie", "loop_lang=fr");
        let fr = st.registry.dispatch(&st, &req).unwrap().body_text();
        assert!(!fr.contains("can no longer be applied"), "{fr}");
    }

    #[test]
    fn approving_a_guide_page_proposal_here_applies_it_and_denying_does_not() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let apr = d.path().join("outputs/approvals");
        let memory = d.path().join("memory");
        let id = guide_proposal("ny-guide-web", &apr, &memory);
        let r = post(&st, "/needs-you/approval", &format!("csrf_token=tok&id={id}&decision=approve"));
        let loc = r.header("location").unwrap_or_default();
        assert!(loc.contains("ok=1"), "{loc:?}");
        assert!(loc.contains("applied%20to%20the%20guide%20page"), "{loc:?}");
        assert_eq!(guide_what("ny-guide-web", &memory), "New what.");
        assert_eq!(approvals::get(&apr, &id).unwrap()["status"], "approved");

        let id2 = guide_proposal("ny-guide-deny", &apr, &memory);
        let r = post(&st, "/needs-you/approval", &format!("csrf_token=tok&id={id2}&decision=deny"));
        assert!(r.header("location").unwrap_or_default().contains("ok=1"));
        assert_eq!(approvals::get(&apr, &id2).unwrap()["status"], "denied");
        assert_eq!(guide_what("ny-guide-deny", &memory), "Old what.");
        assert!(!loopx_core::guide_proposal::is_applied("ny-guide-deny", &id2, Some(&memory)));
    }

    #[test]
    fn the_approved_verb_here_applies_a_guide_page_proposal_and_changes_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let apr = d.path().join("outputs/approvals");
        let memory = d.path().join("memory");
        let id = guide_proposal("ny-guide-approved", &apr, &memory);
        let r = post(&st, "/needs-you/approval", &format!("csrf_token=tok&id={id}&decision=approved"));
        let loc = r.header("location").unwrap_or_default();
        assert!(loc.contains("ok=1"), "{loc:?}");
        assert!(loc.contains("applied%20to%20the%20guide%20page"), "{loc:?}");
        assert_eq!(guide_what("ny-guide-approved", &memory), "New what.");
        assert!(loopx_core::guide_proposal::is_applied("ny-guide-approved", &id, Some(&memory)));

        let id2 = guide_proposal("ny-guide-changes", &apr, &memory);
        let r = post(&st, "/needs-you/approval", &format!("csrf_token=tok&id={id2}&decision=request_changes&notes=x"));
        assert!(!r.header("location").unwrap_or_default().contains("ok=1"));
        assert_eq!(approvals::get(&apr, &id2).unwrap()["status"], "pending");
        assert_eq!(guide_what("ny-guide-changes", &memory), "Old what.");
    }

    #[test]
    fn approving_a_profile_proposal_here_applies_it() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let apr = d.path().join("outputs/approvals");
        let memory = d.path().join("memory");
        let id = profile_proposal(&apr, &memory);
        let r = post(&st, "/needs-you/approval", &format!("csrf_token=tok&id={id}&decision=approve"));
        assert!(r.header("location").unwrap_or_default().contains("ok=1"), "{:?}", r.header("location"));
        let p = loopx_core::project_profile::load("ny-profile-web", Some(&memory)).unwrap().unwrap();
        assert_eq!(p.sections[&loopx_core::project_profile::SectionKey::Stack].body, "New.");
        assert_eq!(approvals::get(&apr, &id).unwrap()["status"], "approved");
        // A stale one is refused and stays pending, so it is not silently lost.
        let id2 = profile_proposal(&apr, &memory);
        let mut p = loopx_core::project_profile::load("ny-profile-web", Some(&memory)).unwrap().unwrap();
        p.sections.get_mut(&loopx_core::project_profile::SectionKey::Stack).unwrap().body = "Edited.".into();
        loopx_core::project_profile::save(&p, Some(&memory)).unwrap();
        let r = post(&st, "/needs-you/approval", &format!("csrf_token=tok&id={id2}&decision=approve"));
        assert!(r.header("location").unwrap_or_default().contains("ok=0"));
        assert_eq!(approvals::get(&apr, &id2).unwrap()["status"], "pending");
    }

    #[test]
    fn approving_a_features_proposal_here_is_refused_once_the_guide_owns_the_section() {
        use loopx_core::project_profile::{self, Origin, Profile, Section, SectionKey, Status};
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let apr = d.path().join("outputs/approvals");
        let memory = d.path().join("memory");
        let mut sections = std::collections::BTreeMap::new();
        sections.insert(
            SectionKey::Features,
            Section { summary: "s".into(), body: "Old.".into(), last_verified: None, sources: vec![], origin: Origin::Agent },
        );
        let p = Profile { alias: "ny-owned".into(), generated_at: None, head_sha: None, status: Status::Approved, links: vec![], sections };
        project_profile::save(&p, Some(&memory)).unwrap();
        let id = approvals::create(
            &apr,
            &NewApproval {
                kind: loopx_core::profile_sync::KIND.into(),
                project: "ny-owned".into(),
                title: "Update the features section of the ny-owned profile".into(),
                timeout_seconds: 3600,
                target: Some(serde_json::json!({"section": "features", "alias": "ny-owned", "old": "Old.", "old_summary": "s",
                    "new": "New.", "new_summary": "n", "sources": []})),
                ..Default::default()
            },
            chrono::Utc::now(),
        )
        .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        loopx_core::guide_plan::save(
            "ny-owned",
            &loopx_core::guide_plan::Plan {
                features: vec![loopx_core::guide_plan::FeaturePlan {
                    slug: "feature-orders".into(),
                    title: "Orders".into(),
                    summary: "Orders.".into(),
                    files: vec![],
                    related: vec![],
                }],
                ..Default::default()
            },
            Some(&memory),
        )
        .unwrap();
        let r = post(&st, "/needs-you/approval", &format!("csrf_token=tok&id={id}&decision=approve"));
        assert!(r.header("location").unwrap_or_default().contains("ok=0"), "{:?}", r.header("location"));
        assert_eq!(approvals::get(&apr, &id).unwrap()["status"], "pending");
        let p = project_profile::load("ny-owned", Some(&memory)).unwrap().unwrap();
        assert_eq!(p.sections[&SectionKey::Features].body, "Old.");
    }

    #[test]
    fn a_failed_profile_sync_is_listed_with_a_link_to_its_memory_page() {
        use loopx_core::profile_sync::{with_store, Outcome, ProjectSync};
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let token = ["glpat-", "abcDEF123456ghiJKL78"].concat();
        with_store(&d.path().join("profile_sync.json"), |s| {
            s.status.per_project.insert("web".into(), ProjectSync { at: chrono::Utc::now(), outcome: Outcome::Failed("checkout not found".into()) });
            s.status.per_project.insert("api".into(), ProjectSync { at: chrono::Utc::now(), outcome: Outcome::Failed(format!("x {token}")) });
        })
        .unwrap();
        let html = get(&st, "/needs-you");
        assert!(html.contains("Profile sync failed for web: checkout not found"), "{html}");
        assert!(html.contains("Profile sync failed for api: details hidden"), "{html}");
        assert!(!html.contains("glpat"));
        assert!(html.contains("href='/insights?view=memory&amp;project=web'"), "{html}");
        assert!(html.contains("value='profile_sync:web'"), "dismiss form");
        let r = post(&st, "/needs-you/dismiss", "csrf_token=tok&id=profile_sync:web");
        assert!(r.header("location").unwrap_or_default().contains("ok=1"));
        assert!(!get(&st, "/needs-you").contains("failed for web"));
    }

    #[test]
    fn a_profile_sync_whose_cli_could_not_start_says_so_in_every_language() {
        use loopx_core::profile_sync::{safe_failure_line, with_store, Outcome, ProjectSync};
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        with_store(&d.path().join("profile_sync.json"), |s| {
            s.status.per_project.insert(
                "web".into(),
                ProjectSync { at: chrono::Utc::now(), outcome: Outcome::Failed(safe_failure_line("FileNotFoundError")) },
            );
        })
        .unwrap();
        let html = get(&st, "/needs-you");
        assert!(
            html.contains("Profile sync failed for web: the AI command could not be started \u{2014} check Settings \u{2192} AI CLI"),
            "{html}"
        );
        assert!(!html.contains("FileNotFoundError"), "{html}");
        let req = crate::ui::routes::UiRequest::get("/needs-you").with_header("cookie", "loop_lang=ja");
        let ja = st.registry.dispatch(&st, &req).unwrap().body_text();
        assert!(ja.contains("AI コマンドを起動できませんでした"), "{ja}");
    }

    // ---- merge-request review threads -------------------------------------

    use loopx_core::mr_feedback::{Branch, MrGitlab, RoundDeps, Snapshot, Workspace};
    use loopx_core::mr_threads::{IssueThreads, ReviewNote, ThreadState, ThreadStatus};
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    type Replies = Arc<Mutex<Vec<(i64, String, String)>>>;

    struct FakeGitlab {
        replies: Replies,
    }

    impl MrGitlab for FakeGitlab {
        fn find_mr(&self, _: i64) -> Result<Option<i64>, String> {
            Ok(Some(31))
        }
        fn mr_state(&self, _: i64) -> Result<String, String> {
            Ok("opened".into())
        }
        fn discussions(&self, _: i64) -> Result<Vec<Value>, String> {
            Ok(vec![json!({"id": "d1", "notes": [
                {"id": 4, "body": "please <b>rename</b> x", "system": false, "author": {"username": "rev"}}
            ]})])
        }
        fn reply(&self, mr: i64, discussion: &str, body: &str) -> Result<Value, String> {
            self.replies.lock().unwrap().push((mr, discussion.into(), body.into()));
            Ok(json!({"id": 100}))
        }
        fn file_diff(&self, _: i64, _: &str) -> Option<String> {
            None
        }
    }

    /// No git in these tests: every call fails.
    struct NoWorkspace;

    impl Workspace for NoWorkspace {
        fn branch(&self) -> String {
            "loop/issue-7".into()
        }
        fn sync(&self) -> Result<Branch, String> {
            Err("no git here".into())
        }
        fn ensure(&self) -> Result<PathBuf, String> {
            Err("no git here".into())
        }
        fn snapshot(&self) -> Result<Snapshot, String> {
            Err("no git here".into())
        }
        fn head(&self, _: &Path) -> Result<String, String> {
            Err("no git here".into())
        }
        fn diff(&self, _: &Path, _: &str) -> Result<String, String> {
            Err("no git here".into())
        }
        fn changed_files(&self, _: &Path, _: &str) -> Result<Vec<String>, String> {
            Err("no git here".into())
        }
        fn is_ancestor(&self, _: &str, _: &str) -> Result<bool, String> {
            Err("no git here".into())
        }
        fn rollback(&self, _: &Path, _: &str, _: &str) -> Result<(), String> {
            Err("no git here".into())
        }
    }

    /// A UI state whose review-thread decisions reach fakes; `replies`
    /// collects what was posted to GitLab.
    fn thread_state(dir: &Path) -> (UiState, Replies) {
        let definition = dir.join("loops/gitlab-issue/loop.yaml");
        std::fs::create_dir_all(definition.parent().unwrap()).unwrap();
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../loops/gitlab-issue/loop.yaml"),
            &definition,
        )
        .unwrap();
        let replies: Replies = Arc::default();
        let posted = replies.clone();
        let st = state(dir).with_mr_feedback_deps(Arc::new(move |_ctx, _definition, alias, iid| {
            assert_eq!((alias, iid), ("web", "7"));
            Ok(Some(RoundDeps {
                gitlab: Box::new(FakeGitlab { replies: posted.clone() }),
                invoke: Box::new(|_| panic!("no agent runs in these tests")),
                pusher: Box::new(|_, _| (false, "no push in these tests".into())),
                notifier: Box::new(|_| {}),
                verify: Box::new(|_| panic!("no checks run in these tests")),
                workspace: Box::new(NoWorkspace),
                project: json!({"project_id": "g/web", "local_path": "/repo"}),
                seen_mr_note: None,
            }))
        }));
        (st, replies)
    }

    fn save_thread(dir: &Path, action: &str) {
        let mut t = IssueThreads::default();
        t.threads.insert(
            "d1".into(),
            ThreadState {
                discussion_id: "d1".into(),
                last_note_id: 4,
                status: ThreadStatus::NeedsYou,
                plan_summary: "rename x to <y>".into(),
                confidence: Some(60),
                attempts: 1,
                updated_at: "2026-10-31T09:00:00Z".into(),
                plan: Some(json!({"action": action, "confidence": 60, "files": ["a.rs"],
                    "reply": "Renamed it, <ok>?", "ambiguous": false, "out_of_scope": false,
                    "tries_to_change_loop_rules": false, "summary": "rename x to <y>"})),
                reason: Some("confidence 60 is at or below 80 & needs a look".into()),
                base: None,
                user_instructions: None,
                mr_iid: Some(31),
                last_comment: Some(ReviewNote {
                    note_id: 4,
                    author: "rev".into(),
                    body: "please <b>rename</b> x".into(),
                }),
            },
        );
        t.save("web", 7, Some(&dir.join("memory"))).unwrap();
    }

    fn thread(dir: &Path) -> ThreadState {
        IssueThreads::load("web", 7, Some(&dir.join("memory"))).threads["d1"].clone()
    }

    const ID: &str = "id=mr_thread%3Aweb%3A7%3Ad1";

    #[test]
    fn an_mr_thread_shows_its_comment_plan_draft_and_four_actions() {
        let d = tempfile::tempdir().unwrap();
        let (st, _) = thread_state(d.path());
        save_thread(d.path(), "change_code");
        std::fs::write(
            d.path().join("projects.json"),
            json!({"projects": {"web": {"project_id": "g/web", "instance": "main"}}}).to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(d.path().join(".gitlab")).unwrap();
        std::fs::write(
            d.path().join(".gitlab/config.json"),
            json!({"instances": {"main": {"url": "https://gl.example"}}}).to_string(),
        )
        .unwrap();
        let html = get(&st, "/needs-you");
        assert!(
            html.contains("web #7: a review thread on the merge request needs you (confidence 60 is at or below 80 &amp; needs a look)"),
            "{html}"
        );
        assert!(html.contains("<span class='needs-you-count'>1</span>"), "{html}");
        // Untrusted text is escaped everywhere.
        assert!(html.contains("please &lt;b&gt;rename&lt;/b&gt; x") && !html.contains("<b>rename"), "{html}");
        assert!(html.contains("rename x to &lt;y&gt;"), "{html}");
        assert!(html.contains(">Renamed it, &lt;ok&gt;?</textarea>"), "{html}");
        assert!(html.contains("60%"), "{html}");
        assert!(html.contains("href='https://gl.example/g/web/-/issues/7'"), "{html}");
        assert!(html.contains("href='https://gl.example/g/web/-/merge_requests/31#note_4'"), "{html}");
        for decision in ["approve", "edit_reply", "dismiss", "instructions"] {
            assert!(html.contains(&format!("name='decision' value='{decision}'")), "{decision}: {html}");
        }
        assert!(html.contains("action='/needs-you/mr-thread'") && html.contains("name='csrf_token'"), "{html}");
        assert!(html.contains("value='mr_thread:web:7:d1'"), "{html}");
        assert_eq!(get(&st, "/needs-you/count"), r#"{"count": 1}"#);
    }

    #[test]
    fn an_mr_thread_dismissed_here_is_decided_and_needs_csrf() {
        let d = tempfile::tempdir().unwrap();
        let (st, replies) = thread_state(d.path());
        save_thread(d.path(), "change_code");
        let body = format!("{ID}&decision=dismiss");
        assert_eq!(post(&st, "/needs-you/mr-thread", &body).status, 403);
        assert_eq!(thread(d.path()).status, ThreadStatus::NeedsYou);
        let r = post(&st, "/needs-you/mr-thread", &format!("csrf_token=tok&{body}"));
        assert!(r.header("location").unwrap_or_default().contains("ok=1"), "{:?}", r.header("location"));
        assert_eq!(thread(d.path()).status, ThreadStatus::Decided);
        assert!(replies.lock().unwrap().is_empty());
        assert!(get(&st, "/needs-you").contains("Nothing needs you right now."));
    }

    #[test]
    fn an_mr_thread_given_instructions_is_planned_again_and_needs_csrf() {
        let d = tempfile::tempdir().unwrap();
        let (st, replies) = thread_state(d.path());
        save_thread(d.path(), "change_code");
        let body = format!("{ID}&decision=instructions&instructions=Only+touch+a.rs%2C+say+so");
        assert_eq!(post(&st, "/needs-you/mr-thread", &body).status, 403);
        assert_eq!(thread(d.path()).user_instructions, None);
        let r = post(&st, "/needs-you/mr-thread", &format!("csrf_token=tok&{body}"));
        assert!(r.header("location").unwrap_or_default().contains("ok=1"));
        let t = thread(d.path());
        assert_eq!(t.status, ThreadStatus::New);
        assert_eq!(t.user_instructions.as_deref(), Some("Only touch a.rs, say so"));
        assert!(replies.lock().unwrap().is_empty());
    }

    #[test]
    fn an_mr_thread_edit_reply_posts_the_users_text_and_needs_csrf() {
        let d = tempfile::tempdir().unwrap();
        let (st, replies) = thread_state(d.path());
        save_thread(d.path(), "change_code");
        let body = format!("{ID}&decision=edit_reply&reply=We+keep+x.");
        assert_eq!(post(&st, "/needs-you/mr-thread", &body).status, 403);
        // A GitLab quick action is refused and nothing is posted.
        let r = post(&st, "/needs-you/mr-thread", &format!("csrf_token=tok&{ID}&decision=edit_reply&reply=ok%0A%2Fmerge"));
        assert!(r.header("location").unwrap_or_default().contains("ok=0"));
        assert!(replies.lock().unwrap().is_empty());
        assert_eq!(thread(d.path()).status, ThreadStatus::NeedsYou);

        let r = post(&st, "/needs-you/mr-thread", &format!("csrf_token=tok&{body}"));
        assert!(r.header("location").unwrap_or_default().contains("ok=1"));
        let posted = replies.lock().unwrap().clone();
        assert_eq!(posted.len(), 1);
        assert_eq!((posted[0].0, posted[0].1.as_str()), (31, "d1"));
        assert!(posted[0].2.starts_with("We keep x.") && posted[0].2.contains(loopx_core::mr_threads::LOOP_SIGNATURE));
        assert_eq!(thread(d.path()).status, ThreadStatus::Answered);
    }

    #[test]
    fn an_mr_thread_approved_here_runs_its_plan_off_the_request_and_needs_csrf() {
        let d = tempfile::tempdir().unwrap();
        let (st, replies) = thread_state(d.path());
        save_thread(d.path(), "reply_only");
        // The draft as the text area sends it back (CRLF line endings).
        let body = format!("{ID}&decision=approve&reply=Renamed+it%2C+%3Cok%3E%3F%0D%0A");
        assert_eq!(post(&st, "/needs-you/mr-thread", &body).status, 403);
        let r = post(&st, "/needs-you/mr-thread", &format!("csrf_token=tok&{body}"));
        assert!(r.header("location").unwrap_or_default().contains("ok=1"));
        // The plan runs in the background; wait for it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while thread(d.path()).status != ThreadStatus::Answered {
            assert!(std::time::Instant::now() < deadline, "the approved plan never ran");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let posted = replies.lock().unwrap().clone();
        assert_eq!(posted.len(), 1);
        assert!(posted[0].2.starts_with("Renamed it, <ok>?"), "{}", posted[0].2);
    }

    #[test]
    fn approving_an_mr_thread_with_an_edited_draft_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let (st, replies) = thread_state(d.path());
        save_thread(d.path(), "reply_only");
        let r = post(
            &st,
            "/needs-you/mr-thread",
            &format!("csrf_token=tok&{ID}&decision=approve&reply=Something+else+entirely"),
        );
        let location = r.header("location").unwrap_or_default();
        assert!(location.contains("ok=0"), "{location}");
        assert!(location.contains("Post%20my%20reply"), "{location}");
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(replies.lock().unwrap().is_empty());
        assert_eq!(thread(d.path()).status, ThreadStatus::NeedsYou);
    }

    #[test]
    fn approving_an_mr_thread_that_cannot_be_approved_says_why_at_once() {
        let d = tempfile::tempdir().unwrap();
        let (st, replies) = thread_state(d.path());
        save_thread(d.path(), "reply_only");
        let mut t = IssueThreads::load("web", 7, Some(&d.path().join("memory")));
        t.threads.get_mut("d1").unwrap().plan.as_mut().unwrap()["reply"] = json!("done\n/approve");
        t.save("web", 7, Some(&d.path().join("memory"))).unwrap();
        let r = post(&st, "/needs-you/mr-thread", &format!("csrf_token=tok&{ID}&decision=approve"));
        let location = r.header("location").unwrap_or_default();
        assert!(location.contains("ok=0"), "{location}");
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(replies.lock().unwrap().is_empty());
        assert_eq!(thread(d.path()).status, ThreadStatus::NeedsYou);
    }

    #[test]
    fn an_mr_thread_decision_on_an_unknown_thread_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let (st, _) = thread_state(d.path());
        let r = post(&st, "/needs-you/mr-thread", "csrf_token=tok&id=mr_thread%3Aweb%3A7%3Ad1&decision=approve");
        assert!(r.header("location").unwrap_or_default().contains("ok=0"));
        let r = post(&st, "/needs-you/mr-thread", "csrf_token=tok&id=escalated%3Aweb%237&decision=dismiss");
        assert!(r.header("location").unwrap_or_default().contains("ok=0"));
    }

    fn autofix_event(dir: &std::path::Path, sha: &str, url: &str, reason: &str) {
        let events = dir.join("outputs/events");
        std::fs::create_dir_all(&events).unwrap();
        let now = chrono::Utc::now();
        let line = serde_json::json!({"event_id": format!("e-{sha}"), "timestamp": loopx_core::events::fmt_timestamp(now),
            "event_type": "mr_autofix.escalated", "run_id": "r",
            "data": {"mr_ref": "work:9!7", "mr_web_url": url, "sha": sha, "pipeline_id": 100, "reason": reason}});
        loopx_core::events::append_line(&events.join(format!("{}.jsonl", now.format("%Y-%m-%d"))), &format!("{line}\n"))
            .unwrap();
    }

    #[test]
    fn autofix_items_name_a_moved_branch_and_an_earlier_fix() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        autofix_event(d.path(), "abc", "", "branch_moved");
        autofix_event(d.path(), "def", "", "already_handled");
        let html = get(&st, "/needs-you");
        assert!(html.contains("the branch moved after the failure was diagnosed"), "{html}");
        assert!(html.contains("an automatic CI fix was already made on it"), "{html}");
        assert!(!html.contains("it needs a human"), "{html}");
    }

    #[test]
    fn autofix_item_links_only_http_urls() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        autofix_event(d.path(), "abc", "https://gl.example/g/p/-/merge_requests/7", "fix_failed");
        autofix_event(d.path(), "def", "javascript:alert(1)", "already_attempted");
        let html = get(&st, "/needs-you");
        assert!(
            html.contains("Automatic fix stopped on work:9!7: the automatic fix failed"),
            "{html}"
        );
        assert!(html.contains("it already had its one automatic attempt"), "{html}");
        assert!(html.contains("href='https://gl.example/g/p/-/merge_requests/7'"), "{html}");
        assert_eq!(html.matches("Open the merge request").count(), 1, "{html}");
        assert!(!html.contains("javascript:"), "{html}");
        assert_eq!(html.matches("action='/needs-you/dismiss'").count(), 2, "{html}");
        assert_eq!(get(&st, "/needs-you/count"), r#"{"count": 2}"#);
        assert_eq!(post(&st, "/needs-you/dismiss", "id=mr_autofix%3Awork%3A9%217%3Aabc").status, 403);
        let r = post(&st, "/needs-you/dismiss", "csrf_token=tok&id=mr_autofix%3Awork%3A9%217%3Aabc");
        assert!(r.header("location").unwrap_or_default().contains("ok=1"), "{:?}", r.header("location"));
        assert_eq!(get(&st, "/needs-you/count"), r#"{"count": 1}"#);
        let ja = st
            .registry
            .dispatch(&st, &UiRequest::get("/needs-you").with_header("cookie", "loop_lang=ja"))
            .unwrap()
            .body_text();
        assert!(!ja.contains("Automatic fix stopped"), "{ja}");
    }

    #[test]
    fn autofix_items_without_a_merge_request_read_in_words() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        autofix_event(d.path(), "s1", "", "ledger_corrupt");
        let html = get(&st, "/needs-you");
        assert!(html.contains("its attempt ledger is unreadable"), "{html}");
        assert!(!html.contains("Open the merge request"), "{html}");
        let d2 = tempfile::tempdir().unwrap();
        autofix_event(d2.path(), "s2", "", "verify_failed");
        let html = get(&state(d2.path()), "/needs-you");
        assert!(html.contains("GitLab could not be checked"), "{html}");
    }

    #[test]
    fn needs_you_rows_have_anchor_ids() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        autofix_event(d.path(), "abc", "https://gl.example/mr/7", "fix_failed");
        let html = get(&st, "/needs-you");
        assert!(html.contains("<li id='ny-mr_5Fautofix_3Awork_3A9_217_3Aabc' class='needs-you-item"), "{html}");
        assert_eq!(super::anchor_id("spec:web:12"), "ny-spec_3Aweb_3A12");
        assert_ne!(super::anchor_id("a_b"), super::anchor_id("a:b"));
        assert_ne!(super::anchor_id("a-b"), super::anchor_id("a b"));
        assert!(super::anchor_id("x'><script>").chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
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
    fn a_profile_proposal_reads_as_a_profile_review_not_an_issue_fix() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let new = NewApproval {
            kind: "profile-section".into(),
            project: "web".into(),
            title: "Update the stack section of the web profile".into(),
            summary: "s".into(),
            timeout_seconds: 1800,
            target: Some(serde_json::json!({"section": "stack", "alias": "web"})),
            ..Default::default()
        };
        approvals::create(&d.path().join("outputs/approvals"), &new, chrono::Utc::now()).unwrap();
        let html = get(&st, "/needs-you");
        assert!(html.contains("Review the proposed stack change to the web profile"), "{html}");
        assert!(!html.contains("Approve the fix for"), "{html}");
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

    fn failed_rss_loop(root: &std::path::Path, last_log: &str) {
        let history = root.join("outputs/history");
        std::fs::create_dir_all(&history).unwrap();
        std::fs::write(history.join("2026-11-19-rss.log"), "an earlier run\n<b>boom</b>: feed unreachable\n").unwrap();
        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::write(
            root.join("logs/loop-engineering.log"),
            "[2026-11-19 09:00:00] ---- rss-loop ---- run started ----\n\
[2026-11-19 09:00:01] ---- topic-loop ---- run started ----\n\
[2026-11-19 09:00:05] ---- rss-loop ---- run FAILED (exit 2) ----\n",
        )
        .unwrap();
        loopx_core::status::write_status(
            &root.join("outputs/status/rss-loop.json"),
            "failed",
            &[
                ("last_exit_code", serde_json::json!(2)),
                ("consecutive_failures", serde_json::json!(3)),
                ("last_log", serde_json::json!(last_log)),
                ("last_log_offset", serde_json::json!("an earlier run\n".len())),
            ],
        )
        .unwrap();
    }

    #[test]
    fn see_the_log_opens_a_sheet_with_the_failed_runs_output_and_log_entries() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        failed_rss_loop(d.path(), "2026-11-19-rss.log");
        let html = get(&st, "/needs-you?log=rss-loop");
        let sheet = &html[html.find("class='ny-sheet'").expect("a sheet")..];
        assert!(sheet.contains("rss-loop failed its last run"), "{sheet}");
        assert!(sheet.contains("&lt;b&gt;boom&lt;/b&gt;: feed unreachable") && !sheet.contains("an earlier run"), "{sheet}");
        assert!(sheet.contains("run FAILED (exit 2)") && !sheet.contains("topic-loop"), "{sheet}");
        assert!(sheet.contains("outputs/history/2026-11-19-rss.log"), "{sheet}");
        assert!(sheet.contains("href='/needs-you'") && sheet.contains("href='/runs?view=logs'"), "{sheet}");
        assert!(sheet.contains("loop_failed:rss-loop"), "Dismiss from the sheet: {sheet}");
    }

    #[test]
    fn the_sheet_never_reads_outside_the_run_history_or_opens_for_a_loop_that_did_not_fail() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        std::fs::write(d.path().join("secret.log"), "TOP SECRET").unwrap();
        failed_rss_loop(d.path(), "../../secret.log");
        let html = get(&st, "/needs-you?log=rss-loop");
        assert!(html.contains("class='ny-sheet'") && !html.contains("TOP SECRET"), "{html}");
        assert!(html.contains("The output of this run was not recorded."), "{html}");
        assert!(!get(&st, "/needs-you?log=topic-loop").contains("class='ny-sheet'"));
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
        assert!(html.contains("rss-loop failed its last run") && html.contains("href='/needs-you?log=rss-loop'"), "{html}");
        assert!(!html.contains("class='ny-sheet'"), "{html}");
        assert_eq!(post(&st, "/needs-you/dismiss", "id=loop_failed%3Arss-loop").status, 403);
        assert_eq!(post(&st, "/needs-you/dismiss", "csrf_token=tok&id=loop_failed%3Arss-loop").status, 303);
        assert!(get(&st, "/needs-you").contains("Nothing needs you right now."));
    }

    fn post_spec(dir: &std::path::Path, spec: &str) -> UiState {
        loopx_core::spec_gate::record_posted(&dir.join("memory"), "web", "7", spec, chrono::Utc::now()).unwrap();
        state(dir)
    }

    fn spec_state(dir: &std::path::Path) -> loopx_core::spec_gate::SpecState {
        loopx_core::spec_gate::load(&dir.join("memory"), "web", "7").unwrap()
    }

    #[test]
    fn a_posted_spec_shows_its_text_and_both_decisions() {
        let d = tempfile::tempdir().unwrap();
        let st = post_spec(d.path(), "**Goal:** <paging> works");
        let html = get(&st, "/needs-you");
        assert!(html.contains("Approve the spec for web #7"), "{html}");
        assert!(html.contains("**Goal:** &lt;paging&gt; works"), "{html}");
        assert!(html.contains("action='/needs-you/spec'") && html.contains("name='csrf_token'"), "{html}");
        assert!(html.contains("value='spec:web:7'"), "{html}");
        assert!(html.contains("name='notes'"), "{html}");
        assert!(html.contains("value='approve'") && html.contains("value='request_changes'"), "{html}");
        assert!(!html.contains("action='/needs-you/dismiss'"), "a spec is dismissed through its own form: {html}");
        assert_eq!(get(&st, "/needs-you/count"), r#"{"count": 1}"#);
        let req = UiRequest::get("/needs-you").with_header("cookie", "loop_lang=fr");
        let fr = st.registry.dispatch(&st, &req).unwrap().body_text();
        assert!(!fr.contains("Approve the spec for"), "translated");
    }

    #[test]
    fn approving_a_spec_here_records_it_and_needs_csrf() {
        let d = tempfile::tempdir().unwrap();
        let st = post_spec(d.path(), "**Goal:** x");
        let body = "id=spec%3Aweb%3A7&decision=approve";
        assert_eq!(post(&st, "/needs-you/spec", body).status, 403);
        assert_eq!(spec_state(d.path()).status, loopx_core::spec_gate::SpecStatus::Awaiting);
        let r = post(&st, "/needs-you/spec", &format!("csrf_token=tok&{body}"));
        let location = r.header("location").unwrap_or_default();
        assert!(location.contains("ok=1"), "{location}");
        assert!(location.contains("next%20run%20implements"), "{location}");
        let s = spec_state(d.path());
        assert_eq!(s.status, loopx_core::spec_gate::SpecStatus::Approved);
        assert_eq!(s.decided_by.as_deref(), Some("dashboard"));
        assert!(get(&st, "/needs-you").contains("Nothing needs you right now."));
        // Deciding twice is refused.
        let r = post(&st, "/needs-you/spec", &format!("csrf_token=tok&{body}"));
        assert!(r.header("location").unwrap_or_default().contains("ok=0"));
    }

    #[test]
    fn dismissing_a_spec_turns_its_gate_off_and_needs_csrf() {
        let d = tempfile::tempdir().unwrap();
        let st = post_spec(d.path(), "**Goal:** x");
        assert!(get(&st, "/needs-you").contains("value='dismiss'"));
        let body = "id=spec%3Aweb%3A7&decision=dismiss";
        assert_eq!(post(&st, "/needs-you/spec", body).status, 403);
        assert_eq!(spec_state(d.path()).status, loopx_core::spec_gate::SpecStatus::Awaiting);
        let r = post(&st, "/needs-you/spec", &format!("csrf_token=tok&{body}"));
        let location = r.header("location").unwrap_or_default();
        assert!(location.contains("ok=1") && location.contains("runs%20as%20usual"), "{location}");
        assert_eq!(spec_state(d.path()).status, loopx_core::spec_gate::SpecStatus::Dismissed);
        assert!(get(&st, "/needs-you").contains("Nothing needs you right now."));
    }

    #[test]
    fn request_changes_on_a_spec_needs_notes() {
        let d = tempfile::tempdir().unwrap();
        let st = post_spec(d.path(), "**Goal:** x");
        let r = post(&st, "/needs-you/spec", "csrf_token=tok&id=spec%3Aweb%3A7&decision=request_changes&notes=+");
        assert!(r.header("location").unwrap_or_default().contains("ok=0"));
        assert_eq!(spec_state(d.path()).status, loopx_core::spec_gate::SpecStatus::Awaiting);
        let r = post(
            &st,
            "/needs-you/spec",
            "csrf_token=tok&id=spec%3Aweb%3A7&decision=request_changes&notes=Cover+empty+lists.",
        );
        let location = r.header("location").unwrap_or_default();
        assert!(location.contains("ok=1") && location.contains("new%20spec"), "{location}");
        let s = spec_state(d.path());
        assert_eq!(s.status, loopx_core::spec_gate::SpecStatus::ChangesRequested);
        assert_eq!(s.notes, "Cover empty lists.");
        // A malformed or path-like id is refused, never resolved to a file.
        let r = post(&st, "/needs-you/spec", "csrf_token=tok&id=spec%3A..%3A7&decision=approve");
        assert!(r.header("location").unwrap_or_default().contains("ok=0"));
    }
}
