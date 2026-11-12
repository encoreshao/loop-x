//! Needs you: `/needs-you` lists everything waiting on a person
//! (`loopx_core::needs_you`) with what to do about each: Allow / Deny /
//! Request changes for an approval, a link for the rest, Dismiss for an
//! escalation, a failed loop or a dead sign-in, and for a merge-request review
//! thread the MR-feedback round handed over: Approve the plan, Post my reply,
//! Plan again with my instructions, or Dismiss (`POST /needs-you/mr-thread`,
//! `loopx_core::mr_feedback::decide_thread`). The topbar bell (`bell_html`)
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
}

fn memory_root(state: &UiState) -> PathBuf {
    state.home_dir.join("memory")
}

fn paths(state: &UiState) -> Paths {
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
        Some("approval") if item["approval_kind"] == "profile-section" => ctx.tf(
            "Review the proposed {section} change to the {project} profile",
            &[("section", &f("section")), ("project", &f("project"))],
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
        Some("mr_thread") => ctx.tf(
            "{project} #{iid}: a review thread on the merge request needs you ({reason})",
            &[("project", &f("project")), ("iid", &f("issue_iid")), ("reason", &f("reason"))],
        ),
        _ => f("title"),
    }
}

fn icon_for(kind: &str) -> &'static str {
    match kind {
        "approval" => "task_alt",
        "escalated" => "warning",
        "sign_in" => "login",
        "mr_thread" => "forum",
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
        Some("loop_failed") => ("/runs?view=logs".to_string(), ctx.t("See the log")),
        Some("profile_sync_failed") => (
            crate::ui::pages::memory::project_url(&text(item, "project"), None),
            ctx.t("Open the project's memory"),
        ),
        _ => return String::new(),
    };
    format!("<a class='btn btn-neutral' href='{}'>{}</a>", pyfmt::escape(&href), pyfmt::escape(&label))
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

fn row(ctx: &UiCtx, item: &Value, prefixes: &crate::ui::markdown::GitlabPrefixes) -> String {
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
    let (details, actions) = if item["kind"] == "mr_thread" {
        (mr_thread_details(ctx, item, prefixes), mr_thread_actions(ctx, item))
    } else {
        (String::new(), actions(ctx, item))
    };
    format!(
        "<li class='needs-you-item needs-you-{kind}'>{}<div class='needs-you-text'>{}{when}{details}</div><div class='needs-you-actions'>{actions}</div></li>",
        icons::icon(icon_for(item["kind"].as_str().unwrap_or(""))),
        pyfmt::escape(&headline(ctx, item)),
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
        let prefixes = if items.iter().any(|i| i["kind"] == "mr_thread") {
            crate::ui::gitlab::issue_url_prefixes(ctx.state)
        } else {
            Default::default()
        };
        let rows: String = items.iter().map(|i| row(ctx, i, &prefixes)).collect();
        format!("<div class='card'><ul class='needs-you-list'>{rows}</ul></div>")
    };
    format!(
        "<h1>{}</h1><p>{}</p>{}{list}",
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
    // A profile proposal is only worth approving if it is applied: the shared
    // helper approves it only while it still applies, then writes it.
    let profile = decision == "approve"
        && loopx_core::approvals::get(&dir, id).is_some_and(|a| a["kind"] == loopx_core::profile_sync::KIND);
    if profile {
        let root = memory_root(ctx.state);
        return match loopx_core::profile_sync::approve_and_apply(id, "dashboard", Some(&root), &dir, Utc::now()) {
            Ok(_) => UiResponse::flash_redirect(true, &ctx.t("Proposal approved and applied to the profile."), PATH),
            Err(e) => UiResponse::flash_redirect(false, &e, PATH),
        };
    }
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

/// `mr_thread:<alias>:<iid>:<discussion id>`.
fn parse_thread_id(id: &str) -> Option<(String, i64, String)> {
    let rest = id.strip_prefix("mr_thread:")?;
    let mut parts = rest.rsplitn(3, ':');
    let discussion = parts.next().filter(|d| !d.is_empty())?;
    let iid = parts.next()?.parse().ok()?;
    let alias = parts.next().filter(|a| !a.is_empty())?;
    Some((alias.to_string(), iid, discussion.to_string()))
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
