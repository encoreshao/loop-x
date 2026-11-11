//! The Todos view of the Issues page (`/loops/issue-loop?view=todos`): the user's
//! GitLab todos mirrored in `todos.json`, plus `POST /todos/settings`,
//! `POST /todos/refresh` and `POST /todos/<instance>/<id>/done`.

use super::loops_common::flash_html;
use crate::ui::icons::icon;
use crate::ui::pyfmt;
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use chrono::{DateTime, Duration, Utc};
use loopx_core::todos::{self, Todo, TodoSettings, TodoStore};
use loopx_core::todos_draft;
use loopx_core::todos_sync;
use serde_json::Value;

const URL: &str = "/loops/issue-loop?view=todos";

pub fn register(r: &mut Registry) {
    r.post("/todos/settings", save_settings);
    r.post("/todos/refresh", refresh);
    r.post("/todos/bulk-done", bulk_done);
    r.post("/todos/:instance/:id/done", mark_done);
    r.post("/todos/:instance/:id/draft", start_draft);
    r.post("/todos/:instance/:id/approve", approve_draft);
    r.post("/todos/:instance/:id/discard", discard_draft);
}

/// Drafts live apart from `outputs/approvals`: the generic approval screens (Needs you,
/// the API) would mark one approved without posting it, and the issue loop would count it.
fn approvals_dir(ctx: &UiCtx) -> std::path::PathBuf {
    ctx.state.repo_root.join("outputs").join("todo-replies")
}

fn todo_id(ctx: &UiCtx) -> i64 {
    ctx.param("id").parse().unwrap_or(-1)
}

fn client(ctx: &UiCtx, instance: &str) -> Result<loopx_core::gitlab::Client, String> {
    loopx_core::gitlab::Client::from_config(gitlab_config(ctx).as_ref(), Some(instance), None).map_err(|e| e.to_string())
}

/// The description and comments of the todo's issue or merge request, capped.
fn thread_text(client: &loopx_core::gitlab::Client, todo: &Todo) -> Result<String, String> {
    let iid = todo.target_iid.ok_or("the todo has no issue or merge request")?;
    let v = if todo.target_type == "MergeRequest" {
        client.get_merge_request(&todo.project, iid)
    } else {
        client.get_issue(&todo.project, iid)
    }
    .map_err(|e| e.to_string())?;
    let mut text = v.get("description").and_then(Value::as_str).unwrap_or("").to_string();
    for n in v.get("notes").and_then(Value::as_array).into_iter().flatten().filter(|n| n.get("system") != Some(&Value::Bool(true))) {
        let who = n.pointer("/author/username").and_then(Value::as_str).unwrap_or("?");
        text.push_str(&format!("\n\n@{who}: {}", n.get("body").and_then(Value::as_str).unwrap_or("")));
    }
    Ok(text.chars().take(8000).collect())
}

fn start_draft(ctx: &UiCtx) -> UiResponse {
    let (instance, id, now, dir) = (ctx.param("instance").to_string(), todo_id(ctx), ctx.state.now(), approvals_dir(ctx));
    let todo = match locked(ctx, |s| {
        todos_draft::clear_dead_draft(s, &dir, &instance, id, now);
        todos_draft::begin(s, &instance, id, now).map(|()| s.instances[&instance].todos[&id].clone())
    }) {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => return back(ctx, false, &e),
        Err(r) => return r,
    };
    if ctx.state.spawn_processes {
        let (path, config) = (store_path(ctx), gitlab_config(ctx));
        std::thread::spawn(move || run_draft(&path, &dir, config, &instance, todo));
    }
    back(ctx, true, &ctx.t("Drafting a reply. It will appear here for you to review."))
}

/// The draft job: read the thread, ask the agent, store the result for review.
fn run_draft(path: &std::path::Path, dir: &std::path::Path, config: Option<Value>, instance: &str, todo: Todo) {
    let result = (|| {
        let _slot = todos_draft::try_acquire().ok_or("two drafts are already running, try again shortly")?;
        let client = loopx_core::gitlab::Client::from_config(config.as_ref(), Some(instance), None).map_err(|e| e.to_string())?;
        let req = todos_draft::DraftRequest {
            project: todo.project.clone(),
            target_type: todo.target_type.clone(),
            iid: todo.target_iid.unwrap_or(0),
            title: todo.title.clone(),
            issue_text: thread_text(&client, &todo)?,
            todo_body: todo.body.clone(),
            author: todo.author.clone(),
        };
        todos_draft::run_agent(&todos_draft::build_prompt(&req))
    })();
    // Stamped when the draft is ready, not when the click happened.
    let _ = todos::with_store(path, |s| todos_draft::complete(s, dir, instance, todo.id, result.map_err(|e: String| e), Utc::now()));
}

fn approve_draft(ctx: &UiCtx) -> UiResponse {
    let instance = ctx.param("instance").to_string();
    let client = match client(ctx, &instance) {
        Ok(c) => c,
        Err(e) => return back(ctx, false, &e),
    };
    let text = ctx.form().first("text").unwrap_or("").to_string();
    let (id, now, dir) = (todo_id(ctx), ctx.state.now(), approvals_dir(ctx));
    // Under the store lock, so a double click cannot post the reply twice.
    match locked(ctx, |s| todos_draft::approve(s, &dir, &client, &instance, id, &text, now)) {
        Ok(Ok(())) => back(ctx, true, &ctx.t("Reply posted to GitLab.")),
        Ok(Err(e)) => back(ctx, false, &ctx.tf("The reply was not posted: {error}", &[("error", &e)])),
        Err(r) => r,
    }
}

fn discard_draft(ctx: &UiCtx) -> UiResponse {
    let (instance, id, now, dir) = (ctx.param("instance").to_string(), todo_id(ctx), ctx.state.now(), approvals_dir(ctx));
    match locked(ctx, |s| todos_draft::discard(s, &dir, &instance, id, now)) {
        Ok(Ok(())) => back(ctx, true, &ctx.t("Draft discarded.")),
        Ok(Err(e)) => back(ctx, false, &e),
        Err(r) => r,
    }
}

fn store_path(ctx: &UiCtx) -> std::path::PathBuf {
    ctx.state.home_dir.join("todos.json")
}

/// The GitLab config under the dashboard's own `user_home`, so a sandboxed run
/// never reads the real `~/.gitlab`.
fn gitlab_config(ctx: &UiCtx) -> Option<Value> {
    let text = std::fs::read_to_string(ctx.state.user_home.join(".gitlab").join("config.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// The view a form was submitted from: only the known filters are echoed back.
fn view_filter(ctx: &UiCtx) -> &'static str {
    match ctx.form().first("filter") {
        Some("pending") => "pending",
        Some("done") => "done",
        _ => "all",
    }
}

/// Back to the list the user was on, so marking one todo done keeps them in Pending.
fn back(ctx: &UiCtx, ok: bool, message: &str) -> UiResponse {
    let location = match view_filter(ctx) {
        "all" => URL.to_string(),
        f => format!("{URL}&filter={f}"),
    };
    UiResponse::flash_redirect(ok, message, &location)
}

/// The hidden field that tells a form's handler which view to return to.
fn filter_input(filter: &str) -> String {
    format!("<input type='hidden' name='filter' value='{}'>", pyfmt::escape(filter))
}

/// Run `f` on the store under the process-wide lock; a corrupt or unwritable file is a flash.
fn locked<R>(ctx: &UiCtx, f: impl FnOnce(&mut TodoStore) -> R) -> Result<R, UiResponse> {
    todos::with_store(&store_path(ctx), f).map_err(|_| back(ctx, false, &ctx.t("The saved todos could not be read.")))
}

fn save_settings(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let enabled = form.first("enabled").is_some_and(|v| v == "on" || v == "1");
    let minutes = form.first("interval_minutes").and_then(|v| v.parse().ok()).unwrap_or(5);
    if let Err(r) = locked(ctx, |s| {
        s.settings.enabled = enabled;
        s.settings.interval_minutes = TodoSettings::normalized_interval(minutes);
    }) {
        return r;
    }
    if enabled {
        back(ctx, true, &ctx.t("Saved. The first fetch starts within a minute."))
    } else {
        back(ctx, true, &ctx.t("Saved. Todo sync is off."))
    }
}

/// One fetch now, whether or not automatic sync is on: the user asked for it.
fn refresh(ctx: &UiCtx) -> UiResponse {
    if todos::load(&store_path(ctx)).is_err() {
        return back(ctx, false, &ctx.t("The saved todos could not be read."));
    }
    let fetched = crate::todos_tick::fetch_sources(todos_sync::sources_from_config(gitlab_config(ctx).as_ref()));
    let now = ctx.state.now();
    match locked(ctx, |s| todos_sync::apply_fetched(s, fetched, now)) {
        Ok(()) => back(ctx, true, &ctx.t("Todos refreshed.")),
        Err(r) => r,
    }
}

fn mark_done(ctx: &UiCtx) -> UiResponse {
    let (instance, id, now) = (ctx.param("instance").to_string(), todo_id(ctx), ctx.state.now());
    let client = match client(ctx, &instance) {
        Ok(c) => c,
        Err(e) => return back(ctx, false, &e),
    };
    match locked(ctx, |s| todos_sync::mark_done(s, &client, &instance, id, now)) {
        Ok(Ok(())) => back(ctx, true, &ctx.t("Marked done in GitLab.")),
        Ok(Err(e)) => back(ctx, false, &ctx.tf("GitLab did not accept it: {error}", &[("error", &e)])),
        Err(r) => r,
    }
}

/// Mark every ticked todo done in GitLab and here; the failures are listed, not fatal.
fn bulk_done(ctx: &UiCtx) -> UiResponse {
    let now = ctx.state.now();
    let mut by_instance: std::collections::BTreeMap<String, Vec<i64>> = Default::default();
    for sel in ctx.form().all("sel") {
        if let Some((instance, id)) = sel.rsplit_once(':').and_then(|(i, n)| Some((i, n.parse::<i64>().ok()?))) {
            by_instance.entry(instance.to_string()).or_default().push(id);
        }
    }
    if by_instance.is_empty() {
        return back(ctx, false, &ctx.t("Select at least one todo first."));
    }
    let (mut done, mut failed) = (0, Vec::new());
    for (instance, ids) in by_instance {
        let client = match client(ctx, &instance) {
            Ok(c) => c,
            Err(e) => {
                failed.push(e);
                continue;
            }
        };
        match locked(ctx, |s| todos_sync::mark_many(s, &client, &instance, &ids, now)) {
            Ok((n, errs)) => {
                done += n;
                failed.extend(errs);
            }
            Err(r) => return r,
        }
    }
    if failed.is_empty() {
        back(ctx, true, &ctx.tf("Marked {n} done in GitLab.", &[("n", &done.to_string())]))
    } else {
        back(
            ctx,
            false,
            &ctx.tf(
                "Marked {n} done; {failed} could not be: {error}",
                &[("n", &done.to_string()), ("failed", &failed.len().to_string()), ("error", &failed[0])],
            ),
        )
    }
}

const STYLE: &str = "<style>
.todo-bar{display:flex;flex-wrap:wrap;align-items:center;gap:.5rem 1rem;margin-bottom:1rem}
.card.todo-settings{padding:.6rem 1rem}
.todo-settings .todo-bar{margin-bottom:0}
.todo-btn{display:inline-flex;align-items:center;gap:.4rem;height:32px;box-sizing:border-box;padding:0 .9rem;border:1px solid var(--md-outline-variant,rgba(128,128,128,.35));border-radius:999px;background:var(--md-surface,transparent);color:inherit;font:inherit;font-size:.85rem;cursor:pointer;transition:background .2s cubic-bezier(.32,.72,0,1)}
.todo-btn:hover{background:var(--md-surface-variant,rgba(128,128,128,.15))}
.todo-btn .icon{width:16px;height:16px}
.todo-form{display:flex;flex-wrap:wrap;align-items:center;gap:.75rem}
.todo-field{height:34px;box-sizing:border-box;padding:0 .75rem;border:1px solid var(--md-outline-variant,rgba(128,128,128,.3));border-radius:10px;background:var(--md-surface,transparent);color:inherit;font:inherit}
.todo-check{display:inline-flex;align-items:center;gap:.5rem;height:34px}
.todo-seg{display:inline-flex;padding:2px;border-radius:999px;background:var(--md-surface-variant,rgba(128,128,128,.12))}
.todo-seg a,.todo-seg button{border:0;background:transparent;font:inherit;cursor:pointer;height:30px;line-height:30px;padding:0 .9rem;border-radius:999px;text-decoration:none;color:inherit;font-size:.85rem}
.todo-seg a.is-active,.todo-seg button.is-active{background:var(--md-surface,#fff);box-shadow:0 1px 2px rgba(0,0,0,.18);color:var(--md-primary,#0a84ff)}
.todo-list{display:flex;flex-direction:column}
.todo-row{display:flex;gap:1rem;align-items:flex-start;justify-content:space-between;padding:.9rem 0;border-top:1px solid var(--md-outline-variant,rgba(128,128,128,.25))}
.todo-row:first-child{border-top:0}
.todo-main{min-width:0;flex:1 1 auto}
.todo-title{font-weight:600;text-decoration:none;color:inherit}
.todo-meta{display:flex;flex-wrap:wrap;gap:.5rem;align-items:center;margin-top:.25rem;font-size:.8rem;opacity:.75}
.todo-chip{display:inline-flex;align-items:center;height:20px;padding:0 .55rem;border-radius:999px;background:var(--md-surface-variant,rgba(128,128,128,.15));font-size:.72rem}
.todo-chip.is-new{background:var(--md-primary,#0a84ff);color:#fff}
.todo-body{margin:.4rem 0 0;font-size:.9rem;opacity:.9;overflow-wrap:anywhere}
.todo-actions{display:flex;flex-wrap:wrap;gap:.5rem;flex:0 0 auto}
.todo-actions form{margin:0}
.todo-row.is-done{opacity:.6}
.todo-error{margin-bottom:.75rem}
.todo-draft-text{display:block;width:100%;height:auto;min-height:200px;padding:.6rem .75rem;box-sizing:border-box;resize:vertical;line-height:1.45}
.todo-dialog{width:min(640px,calc(100vw - 2rem));max-width:none;padding:1.25rem 1.4rem;border:1px solid var(--md-outline-variant,rgba(128,128,128,.3));border-radius:16px;background:var(--md-surface,#fff);color:inherit;box-shadow:0 24px 60px rgba(0,0,0,.28)}
.todo-dialog::backdrop{background:rgba(0,0,0,.35);backdrop-filter:blur(4px)}
.todo-dialog h2{margin:0 0 .25rem;font-size:1.05rem}
.todo-dialog .todo-meta{margin:0 0 .75rem}
.todo-dialog .todo-actions{margin-top:.9rem;justify-content:flex-end}
.todo-pick{display:flex;align-items:flex-start;padding-top:.2rem}
.todo-pick input{width:16px;height:16px;margin:0}
.todo-bulkbar{display:flex;align-items:center;gap:.75rem;padding:.4rem 0 .6rem;font-size:.85rem}
.todo-bulkbar label{display:inline-flex;align-items:center;gap:.4rem}
.todo-bulkbar input[type=checkbox]{width:16px;height:16px;margin:0}
</style>";

fn reason_label(ctx: &UiCtx, action: &str) -> String {
    match action {
        "mentioned" => ctx.t("Mentioned you"),
        "directly_addressed" => ctx.t("Addressed you"),
        "assigned" => ctx.t("Assigned to you"),
        "review_requested" => ctx.t("Review requested"),
        "marked" => ctx.t("Marked as to-do"),
        "build_failed" => ctx.t("Pipeline failed"),
        "unmergeable" => ctx.t("Not mergeable"),
        "approval_required" => ctx.t("Approval required"),
        _ => ctx.t("Other"),
    }
}

fn ago(ctx: &UiCtx, now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let d = now - then;
    if d < Duration::minutes(1) {
        ctx.t("just now")
    } else if d < Duration::hours(1) {
        ctx.tf("{n}m ago", &[("n", &d.num_minutes().to_string())])
    } else if d < Duration::days(1) {
        ctx.tf("{n}h ago", &[("n", &d.num_hours().to_string())])
    } else {
        ctx.tf("{n}d ago", &[("n", &d.num_days().to_string())])
    }
}

fn settings_card(ctx: &UiCtx, store: &TodoStore, filter: &str) -> String {
    let csrf = format!("{}{}", ctx.csrf_input(), filter_input(filter));
    let enabled = store.settings.enabled;
    let interval = store.settings.interval_minutes;
    let last = store
        .instances
        .values()
        .filter_map(|i| i.last_capture)
        .max()
        .map(|t| ctx.tf("Last fetched {when}", &[("when", &ago(ctx, ctx.state.now(), t))]))
        .unwrap_or_else(|| ctx.t("Not fetched yet"));
    let label = pyfmt::escape(&ctx.t("Sync my GitLab todos"));
    // The switch posts the opposite of what it shows; the interval buttons keep it as it is.
    let (switch_class, checked, next) = if enabled { ("is-on", "true", "0") } else { ("", "false", "on") };
    let keep = if enabled { "on" } else { "0" };
    let seg = |m: u32, text: String| {
        let active = if interval == m { " class='is-active'" } else { "" };
        format!("<button type='submit' name='interval_minutes' value='{m}'{active}>{}</button>", pyfmt::escape(&text))
    };
    format!(
        "<section class='card todo-settings'><div class='todo-bar'>\
<div class='loop-toggle'><form method='post' action='/todos/settings' class='daemon-action-form'>{csrf}\
<input type='hidden' name='enabled' value='{next}'><input type='hidden' name='interval_minutes' value='{interval}'>\
<button type='submit' class='switch {switch_class}' role='switch' aria-checked='{checked}' aria-label='{label}' title='{label}'><span class='switch-thumb'></span></button></form>\
<span class='loop-toggle-text'>{label}</span></div>\
<form method='post' action='/todos/settings' class='todo-seg' title=\"{every}\">{csrf}<input type='hidden' name='enabled' value='{keep}'>{s5}{s10}</form>\
<form method='post' action='/todos/refresh' class='todo-form'>{csrf}<button class='todo-btn' type='submit'>{ricon} {refresh}</button></form>\
<span class='todo-meta'>{last}</span></div></section>",
        every = pyfmt::escape(&ctx.t("Fetch every")),
        s5 = seg(5, ctx.t("5 minutes")),
        s10 = seg(10, ctx.t("10 minutes")),
        ricon = icon("refresh"),
        refresh = pyfmt::escape(&ctx.t("Refresh now")),
        last = pyfmt::escape(&last),
    )
}

fn row(ctx: &UiCtx, t: &Todo, now: DateTime<Utc>, filter: &str) -> String {
    let csrf = format!("{}{}", ctx.csrf_input(), filter_input(filter));
    let done = t.state == "done";
    let is_new = !done && now - t.created_at < Duration::hours(24);
    let new_chip = if is_new { format!("<span class='todo-chip is-new'>{}</span>", pyfmt::escape(&ctx.t("New"))) } else { String::new() };
    let done_btn = if done {
        format!("<span class='todo-chip'>{}</span>", pyfmt::escape(&ctx.t("Done")))
    } else {
        format!(
            "<form method='post' action='/todos/{}/{}/done'>{csrf}<button class='btn btn-primary' type='submit' style='height:34px'>{} {}</button></form>",
            pyfmt::escape(&t.instance),
            t.id,
            icon("check"),
            pyfmt::escape(&ctx.t("Mark done"))
        )
    };
    let draft = draft_html(ctx, t, filter);
    format!(
        "<div class='todo-row{cls}'>{pick}<div class='todo-main'>\
<a class='todo-title' href=\"{url}\" target='_blank' rel='noopener'>{title}</a>\
<div class='todo-meta'>{new_chip}<span class='todo-chip'>{reason}</span><span>{project}</span><span>@{author}</span><span>{when}</span></div>\
<p class='todo-body'>{body}</p>{draft}</div>\
<div class='todo-actions'>{done_btn}{draft_btn}<a class='btn' style='height:34px' href=\"{url}\" target='_blank' rel='noopener'>{link} {open}</a></div></div>",
        cls = if done { " is-done" } else { "" },
        pick = if done {
            String::new()
        } else {
            format!(
                "<div class='todo-pick'><input type='checkbox' form='todo-bulk' name='sel' value='{}:{}' aria-label=\"{}\"></div>",
                pyfmt::escape(&t.instance),
                t.id,
                pyfmt::escape(&ctx.t("Select"))
            )
        },
        url = pyfmt::escape(t.link()),
        title = pyfmt::escape(&t.title),
        reason = pyfmt::escape(&reason_label(ctx, &t.action)),
        project = pyfmt::escape(&t.project),
        author = pyfmt::escape(&t.author),
        when = pyfmt::escape(&ago(ctx, now, t.created_at)),
        body = pyfmt::escape(&snippet(&t.body)),
        link = icon("open_in_new"),
        open = pyfmt::escape(&ctx.t("Open")),
        draft_btn = draft_button(ctx, t, filter),
    )
}

/// "Analyze & draft reply" for a pending todo a reply makes sense for and has no draft yet.
fn draft_button(ctx: &UiCtx, t: &Todo, filter: &str) -> String {
    if t.state == "done" || !todos::is_reply_worthy(&t.action) || has_live_draft(ctx, t) {
        return String::new();
    }
    format!(
        "<form method='post' action='/todos/{}/{}/draft'>{}<button class='btn' type='submit' style='height:34px'>{}</button></form>",
        pyfmt::escape(&t.instance),
        t.id,
        ctx.csrf_input() + &filter_input(filter),
        pyfmt::escape(&ctx.t("Analyze & draft reply"))
    )
}

/// Whether the todo has a draft running, awaiting review or already posted. A stale run
/// marker, an expired or discarded approval do not count: the todo can be drafted again.
fn has_live_draft(ctx: &UiCtx, t: &Todo) -> bool {
    let Some(id) = t.draft_approval_id.as_deref() else { return false };
    if id.starts_with(todos_draft::RUNNING) {
        return todos_draft::is_running(Some(id), ctx.state.now());
    }
    loopx_core::approvals::get(&approvals_dir(ctx), id)
        .is_some_and(|a| matches!(a["status"].as_str(), Some("pending" | "approved")))
}

/// The draft under a row: a spinner note while running, a Review button (and the dialog it
/// opens) once the draft is ready.
fn draft_html(ctx: &UiCtx, t: &Todo, filter: &str) -> String {
    let Some(id) = t.draft_approval_id.as_deref() else { return String::new() };
    if id.starts_with(todos_draft::RUNNING) {
        if !todos_draft::is_running(Some(id), ctx.state.now()) {
            return String::new();
        }
        return format!("<p class='todo-meta'>{}</p>", pyfmt::escape(&ctx.t("Drafting a reply…")));
    }
    let Some(approval) = loopx_core::approvals::get(&approvals_dir(ctx), id) else { return String::new() };
    let text = approval["summary"].as_str().unwrap_or("");
    match approval["status"].as_str() {
        Some("pending") => {
            let slug: String = t.instance.chars().filter(char::is_ascii_alphanumeric).collect();
            let dialog = format!("todo-review-{slug}-{}", t.id);
            format!(
                "<p class='todo-meta'>{ready} <button class='todo-btn' type='button' onclick=\"document.getElementById('{dialog}').showModal()\">{eye} {review}</button></p>\
<dialog id='{dialog}' class='todo-dialog'><form method='post' action='/todos/{inst}/{id}/approve'>{csrf}{filter}\
<h2>{heading}</h2><div class='todo-meta'>{about}</div>\
<textarea class='todo-field todo-draft-text' name='text' rows='9'>{text}</textarea>\
<div class='todo-actions'><button class='btn' type='button' onclick=\"this.closest('dialog').close()\" style='height:34px'>{cancel}</button>\
<button class='btn' type='submit' formaction='/todos/{inst}/{id}/discard' style='height:34px'>{discard}</button>\
<button class='btn btn-primary' type='submit' style='height:34px'>{post}</button></div></form></dialog>",
                inst = pyfmt::escape(&t.instance),
                id = t.id,
                csrf = ctx.csrf_input(),
                filter = filter_input(filter),
                text = pyfmt::escape(text),
                ready = pyfmt::escape(&ctx.t("Draft ready.")),
                eye = icon("visibility"),
                review = pyfmt::escape(&ctx.t("Review")),
                heading = pyfmt::escape(&ctx.t("Review the reply")),
                about = pyfmt::escape(&ctx.tf("{title} · posted to GitLab only when you confirm", &[("title", &t.title)])),
                cancel = pyfmt::escape(&ctx.t("Cancel")),
                post = pyfmt::escape(&ctx.t("Confirm & post reply")),
                discard = pyfmt::escape(&ctx.t("Discard")),
            )
        }
        Some("approved") => format!("<p class='todo-meta'>{}</p>", pyfmt::escape(&ctx.t("Reply posted."))),
        _ => String::new(),
    }
}

/// The first 280 characters of a todo's text.
fn snippet(body: &str) -> String {
    let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 280 {
        format!("{}…", flat.chars().take(280).collect::<String>())
    } else {
        flat
    }
}

fn filter_bar(ctx: &UiCtx, current: &str) -> String {
    let link = |key: &str, label: &str| {
        let active = if key == current { " is-active" } else { "" };
        format!("<a class='{}' href='{URL}&amp;filter={key}'>{}</a>", active.trim(), pyfmt::escape(&ctx.t(label)))
    };
    format!("<div class='todo-seg'>{}{}{}</div>", link("all", "All"), link("pending", "Pending"), link("done", "Done"))
}

pub fn body(ctx: &UiCtx, flash: Option<&str>, flash_ok: bool) -> String {
    let title = format!(
        "<div class='page-title'><h1>{}</h1><p class='subtitle'>{}</p></div>",
        pyfmt::escape(&ctx.t("Todos")),
        pyfmt::escape(&ctx.t("Your GitLab to-do list: mentions, assignments and review requests, including issues you were only tagged on."))
    );
    let store = match todos::load(&store_path(ctx)) {
        Ok(s) => s,
        Err(_) => {
            return format!(
                "{STYLE}{title}{}<div class='flash flash-danger'>{}</div>",
                flash_html(flash, flash_ok),
                pyfmt::escape(&ctx.t("The saved todos could not be read."))
            )
        }
    };
    let view = match ctx.query().first("filter") {
        Some("pending") => "pending",
        Some("done") => "done",
        _ => "all",
    };
    let mut out = format!("{STYLE}{title}{}{}", flash_html(flash, flash_ok), settings_card(ctx, &store, view));
    let cached = store.instances.values().any(|i| !i.todos.is_empty());
    if !store.settings.enabled && !cached {
        out.push_str(&format!(
            "<section class='card'><p>{}</p></section>",
            pyfmt::escape(&ctx.t("Sync is off. Turn it on above to fetch your GitLab todos every few minutes, or use Refresh now for a single fetch. Nothing is fetched automatically while it is off."))
        ));
        return out;
    }
    let filter = view.to_string();
    let now = ctx.state.now();
    let mut rows: Vec<&Todo> = store
        .instances
        .values()
        .flat_map(|i| i.todos.values())
        .filter(|t| match filter.as_str() {
            "done" => t.state == "done",
            "all" => true,
            _ => t.state == "pending",
        })
        .collect();
    // Pending before done, newest first within each.
    rows.sort_by(|a, b| (a.state == "done", std::cmp::Reverse(a.created_at)).cmp(&(b.state == "done", std::cmp::Reverse(b.created_at))));
    let errors: String = store
        .instances
        .iter()
        .filter_map(|(name, i)| {
            i.last_error.as_ref().map(|e| {
                format!(
                    "<div class='flash flash-danger todo-error'>{}</div>",
                    pyfmt::escape(&ctx.tf("{instance}: {error}", &[("instance", name), ("error", e)]))
                )
            })
        })
        .collect();
    out.push_str(&errors);
    out.push_str(&format!("<section class='card'><div class='todo-bar'>{}</div>", filter_bar(ctx, &filter)));
    let any_pending = rows.iter().any(|t| t.state == "pending");
    if rows.is_empty() {
        out.push_str(&format!("<p>{}</p></section>", pyfmt::escape(&ctx.t("Nothing here yet."))));
    } else {
        if any_pending {
            out.push_str(&format!(
            "<form id='todo-bulk' method='post' action='/todos/bulk-done' class='todo-bulkbar'>{}{}\
<label><input type='checkbox' aria-label=\"{all}\" onclick=\"document.querySelectorAll('input[form=todo-bulk][name=sel]').forEach(function(c){{c.checked=this.checked}},this)\"> {all}</label>\
<button class='todo-btn' type='submit'>{check} {done}</button></form>",
            ctx.csrf_input(),
            filter_input(&filter),
            all = pyfmt::escape(&ctx.t("Select all")),
            check = icon("check"),
            done = pyfmt::escape(&ctx.t("Mark selected done")),
        ));
        }
        out.push_str("<div class='todo-list'>");
        for t in rows {
            out.push_str(&row(ctx, t, now, &filter));
        }
        out.push_str("</div></section>");
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::ui::hub::builtin_loop_page;
    use crate::ui::routes::Registry;
    use crate::ui::state::UiState;
    use axum::http::Method;
    use chrono::{Duration, TimeZone, Utc};
    use loopx_core::todos::{self, InstanceState, Todo, TodoStore};
    use loopx_core::todos_draft;

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap()
    }

    struct Sb {
        _dir: tempfile::TempDir,
        state: UiState,
    }

    impl Sb {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            std::fs::create_dir_all(home.join(".gitlab")).unwrap();
            // Port 1 refuses connections, so GitLab calls fail fast.
            std::fs::write(
                home.join(".gitlab/config.json"),
                r#"{"default":"work","instances":{"work":{"url":"http://127.0.0.1:1","token":"t"}}}"#,
            )
            .unwrap();
            let state = UiState::explicit(dir.path(), &dir.path().join("loopx"), &home, dir.path(), "tok", Some(now()));
            Self { _dir: dir, state }
        }
        fn store_path(&self) -> std::path::PathBuf {
            self.state.home_dir.join("todos.json")
        }
        fn seed(&self, enabled: bool, todos_in: Vec<Todo>) {
            let mut s = TodoStore::default();
            s.settings.enabled = enabled;
            let st = s.instances.entry("work".into()).or_insert_with(InstanceState::default);
            st.last_capture = Some(now());
            for t in todos_in {
                st.todos.insert(t.id, t);
            }
            todos::save(&s, &self.store_path()).unwrap();
        }
        fn get(&self, url: &str) -> String {
            let r = self.state.registry.dispatch(&self.state, &crate::ui::routes::UiRequest::get(url)).unwrap();
            String::from_utf8(r.body).unwrap()
        }
        fn post(&self, url: &str, body: &str) -> crate::ui::routes::UiResponse {
            let req = crate::ui::routes::UiRequest::new(Method::POST, url)
                .with_header("content-type", "application/x-www-form-urlencoded")
                .with_body(format!("csrf_token=tok&{body}"));
            self.state.registry.dispatch(&self.state, &req).unwrap()
        }
    }

    fn todo(id: i64, action: &str, age: Duration, state: &str) -> Todo {
        Todo {
            id,
            instance: "work".into(),
            action: action.into(),
            target_type: "Issue".into(),
            target_iid: Some(id),
            title: format!("Title {id}"),
            web_url: format!("https://gl/i/{id}"),
            project: "g/p".into(),
            author: "ann".into(),
            body: "hello".into(),
            created_at: now() - age,
            state: state.into(),
            done_at: None,
            target_url: String::new(),
            draft_approval_id: None,
        }
    }

    #[test]
    fn the_todos_view_is_listed_after_tasks_in_the_issue_hub() {
        let hub = builtin_loop_page("issue-loop").unwrap();
        let keys: Vec<&str> = hub.views.iter().map(|v| v.key.as_str()).collect();
        let tasks = keys.iter().position(|k| *k == "tasks").unwrap();
        assert_eq!(keys[tasks + 1], "todos");
    }

    #[test]
    fn sync_off_page_explains_and_fetches_nothing() {
        let sb = Sb::new();
        let html = sb.get("/loops/issue-loop?view=todos");
        assert!(html.contains("Sync is off"), "{html}");
        assert!(html.contains("action='/todos/settings'"), "{html}");
        assert!(!sb.store_path().exists());
    }

    #[test]
    fn rows_are_sorted_newest_first_pending_before_done() {
        let sb = Sb::new();
        sb.seed(
            true,
            vec![
                todo(1, "mentioned", Duration::hours(5), "pending"),
                todo(2, "mentioned", Duration::hours(1), "pending"),
                todo(3, "mentioned", Duration::minutes(1), "done"),
            ],
        );
        let html = sb.get("/loops/issue-loop?view=todos");
        let at = |n: i64| html.find(&format!("Title {n}")).unwrap();
        assert!(at(2) < at(1) && at(1) < at(3), "{html}");
        let only_done = sb.get("/loops/issue-loop?view=todos&filter=done");
        assert!(only_done.contains("Title 3") && !only_done.contains("Title 1"));
    }

    #[test]
    fn body_text_is_html_escaped_and_done_button_only_on_pending_rows() {
        let sb = Sb::new();
        let mut t = todo(1, "mentioned", Duration::hours(1), "pending");
        t.body = "<script>alert(1)</script>".into();
        sb.seed(true, vec![t, todo(2, "mentioned", Duration::hours(2), "done")]);
        let html = sb.get("/loops/issue-loop?view=todos");
        assert!(!html.contains("<script>alert(1)"), "{html}");
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("action='/todos/work/1/done'"));
        assert!(!html.contains("action='/todos/work/2/done'"));
    }

    #[test]
    fn every_post_route_requires_csrf() {
        let sb = Sb::new();
        for url in ["/todos/settings", "/todos/refresh", "/todos/bulk-done", "/todos/work/1/done", "/todos/work/1/draft", "/todos/work/1/approve", "/todos/work/1/discard"] {
            let req = crate::ui::routes::UiRequest::new(Method::POST, url);
            let r = sb.state.registry.dispatch(&sb.state, &req).unwrap();
            assert_eq!(r.status, 403, "{url}");
        }
    }

    #[test]
    fn settings_post_persists_enabled_and_interval() {
        let sb = Sb::new();
        let r = sb.post("/todos/settings", "enabled=on&interval_minutes=10");
        assert_eq!(r.status, 303);
        let s = todos::load(&sb.store_path()).unwrap();
        assert!(s.settings.enabled);
        assert_eq!(s.settings.interval_minutes, 10);
        sb.post("/todos/settings", "interval_minutes=7");
        let s = todos::load(&sb.store_path()).unwrap();
        assert!(!s.settings.enabled);
        assert_eq!(s.settings.interval_minutes, 5);
    }

    #[test]
    fn done_post_leaves_row_pending_and_flashes_error_when_gitlab_fails() {
        let sb = Sb::new();
        sb.seed(true, vec![todo(1, "mentioned", Duration::hours(1), "pending")]);
        let r = sb.post("/todos/work/1/done", "");
        assert_eq!(r.status, 303);
        let loc = r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("location")).unwrap().1.clone();
        assert!(loc.contains("ok=0"), "{loc}");
        let s = todos::load(&sb.store_path()).unwrap();
        assert_eq!(s.instances["work"].todos[&1].state, "pending");
    }

    #[test]
    fn a_corrupt_store_shows_an_error_instead_of_panicking() {
        let sb = Sb::new();
        std::fs::create_dir_all(&sb.state.home_dir).unwrap();
        std::fs::write(sb.store_path(), "{nope").unwrap();
        let html = sb.get("/loops/issue-loop?view=todos");
        assert!(html.contains("could not be read"), "{html}");
        let _ = Registry::with_pages();
    }

    fn approvals_dir(sb: &Sb) -> std::path::PathBuf {
        sb.state.repo_root.join("outputs").join("todo-replies")
    }

    #[test]
    fn only_reply_worthy_pending_rows_have_a_draft_button() {
        let sb = Sb::new();
        sb.seed(
            true,
            vec![
                todo(1, "mentioned", Duration::hours(1), "pending"),
                todo(2, "build_failed", Duration::hours(2), "pending"),
                todo(3, "mentioned", Duration::hours(3), "done"),
            ],
        );
        let html = sb.get("/loops/issue-loop?view=todos");
        assert!(html.contains("action='/todos/work/1/draft'"), "{html}");
        assert!(!html.contains("action='/todos/work/2/draft'"));
        assert!(!html.contains("action='/todos/work/3/draft'"));
    }

    #[test]
    fn draft_post_marks_the_todo_running_and_refuses_other_kinds() {
        let sb = Sb::new();
        sb.seed(
            true,
            vec![todo(1, "mentioned", Duration::hours(1), "pending"), todo(2, "build_failed", Duration::hours(1), "pending")],
        );
        let ok = sb.post("/todos/work/1/draft", "");
        assert_eq!(ok.status, 303);
        let s = todos::load(&sb.store_path()).unwrap();
        assert!(todos_draft::is_running(s.instances["work"].todos[&1].draft_approval_id.as_deref(), now()));
        assert!(sb.get("/loops/issue-loop?view=todos").contains("Drafting"));
        let bad = sb.post("/todos/work/2/draft", "");
        let loc = bad.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("location")).unwrap().1.clone();
        assert!(loc.contains("ok=0"), "{loc}");
    }

    fn with_draft(sb: &Sb) -> String {
        sb.seed(true, vec![todo(1, "mentioned", Duration::hours(1), "pending")]);
        let mut s = todos::load(&sb.store_path()).unwrap();
        todos_draft::begin(&mut s, "work", 1, now()).unwrap();
        let id = todos_draft::complete(&mut s, &approvals_dir(sb), "work", 1, Ok("Thanks <b>ann</b>".into()), now()).unwrap();
        todos::save(&s, &sb.store_path()).unwrap();
        id
    }

    fn location(r: &crate::ui::routes::UiResponse) -> String {
        r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("location")).unwrap().1.clone()
    }

    #[test]
    fn open_goes_to_the_comment_that_raised_the_todo_and_says_just_open() {
        let sb = Sb::new();
        let mut t = todo(1, "mentioned", Duration::hours(1), "pending");
        t.target_url = "https://gl/i/1#note_77".into();
        sb.seed(true, vec![t, todo(2, "mentioned", Duration::hours(2), "pending")]);
        let html = sb.get("/loops/issue-loop?view=todos");
        assert!(html.contains("href=\"https://gl/i/1#note_77\""), "{html}");
        assert!(html.contains("href=\"https://gl/i/2\""), "falls back to the issue: {html}");
        assert!(!html.contains("Open in GitLab"), "{html}");
        assert!(html.contains("#i-open_in_new"), "{html}");
    }

    #[test]
    fn every_action_returns_to_the_view_it_came_from() {
        let sb = Sb::new();
        sb.seed(true, vec![todo(1, "mentioned", Duration::hours(1), "pending")]);
        for (url, body) in [
            ("/todos/work/1/done", "filter=pending"),
            ("/todos/work/1/draft", "filter=pending"),
            ("/todos/work/1/discard", "filter=pending"),
            ("/todos/work/1/approve", "filter=pending&text=x"),
            ("/todos/refresh", "filter=pending"),
            ("/todos/bulk-done", "filter=pending&sel=work:1"),
        ] {
            let loc = location(&sb.post(url, body));
            assert!(loc.contains("filter=pending"), "{url}: {loc}");
        }
        // An unknown filter value is not echoed back.
        let loc = location(&sb.post("/todos/work/1/done", "filter=%22%3E%3Cb%3E"));
        assert!(!loc.contains("filter=%22") && !loc.contains("<b>"), "{loc}");
        // The forms carry the current view.
        let html = sb.get("/loops/issue-loop?view=todos&filter=pending");
        assert!(html.contains("name='filter' value='pending'"), "{html}");
    }

    #[test]
    fn pending_rows_have_a_checkbox_for_the_bulk_mark_done_form() {
        let sb = Sb::new();
        sb.seed(true, vec![todo(1, "mentioned", Duration::hours(1), "pending"), todo(2, "mentioned", Duration::hours(2), "done")]);
        let html = sb.get("/loops/issue-loop?view=todos");
        assert!(html.contains("id='todo-bulk'") && html.contains("action='/todos/bulk-done'"), "{html}");
        assert!(html.contains("form='todo-bulk' name='sel' value='work:1'"), "{html}");
        assert!(!html.contains("value='work:2'"), "{html}");
    }

    #[test]
    fn bulk_done_needs_a_selection_and_reports_gitlab_failures() {
        let sb = Sb::new();
        sb.seed(true, vec![todo(1, "mentioned", Duration::hours(1), "pending")]);
        let none = location(&sb.post("/todos/bulk-done", "filter=pending"));
        assert!(none.contains("ok=0"), "{none}");
        // GitLab is unreachable here: nothing flips, and the user is told.
        let failed = location(&sb.post("/todos/bulk-done", "sel=work:1"));
        assert!(failed.contains("ok=0"), "{failed}");
        let s = todos::load(&sb.store_path()).unwrap();
        assert_eq!(s.instances["work"].todos[&1].state, "pending");
    }

    #[test]
    fn a_pending_draft_shows_a_review_button_that_opens_a_dialog_with_the_text() {
        let sb = Sb::new();
        with_draft(&sb);
        let html = sb.get("/loops/issue-loop?view=todos");
        assert!(html.contains("<dialog"), "{html}");
        assert!(html.contains("showModal()"), "{html}");
        assert!(html.contains("Review"), "{html}");
    }

    #[test]
    fn a_pending_draft_shows_an_editable_card_with_approve_and_discard() {
        let sb = Sb::new();
        with_draft(&sb);
        let html = sb.get("/loops/issue-loop?view=todos");
        assert!(html.contains("action='/todos/work/1/approve'") && html.contains("action='/todos/work/1/discard'"), "{html}");
        assert!(html.contains("Thanks &lt;b&gt;ann&lt;/b&gt;"), "{html}");
        assert!(!html.contains("action='/todos/work/1/draft'"));
    }

    #[test]
    fn approve_with_a_gitlab_failure_keeps_the_draft_and_flashes_the_error() {
        let sb = Sb::new();
        let id = with_draft(&sb);
        let r = sb.post("/todos/work/1/approve", "text=hello");
        let loc = r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("location")).unwrap().1.clone();
        assert!(loc.contains("ok=0"), "{loc}");
        assert_eq!(loopx_core::approvals::get(&approvals_dir(&sb), &id).unwrap()["status"], "pending");
    }

    #[test]
    fn discard_drops_the_draft_and_the_button_returns() {
        let sb = Sb::new();
        let id = with_draft(&sb);
        let r = sb.post("/todos/work/1/discard", "");
        assert_eq!(r.status, 303);
        assert_eq!(loopx_core::approvals::get(&approvals_dir(&sb), &id).unwrap()["status"], "denied");
        assert!(sb.get("/loops/issue-loop?view=todos").contains("action='/todos/work/1/draft'"));
    }

    #[test]
    fn sync_is_a_switch_with_no_save_button_and_the_interval_saves_on_click() {
        let sb = Sb::new();
        let off = sb.get("/loops/issue-loop?view=todos");
        assert!(off.contains("role='switch'") && off.contains("aria-checked='false'"), "{off}");
        let card = &off[off.find("todo-settings").unwrap()..];
        let card = &card[..card.find("</section>").unwrap()];
        assert!(!card.contains("type='checkbox'") && !card.contains("Save"), "{card}");
        // The switch posts the opposite of its state.
        assert!(off.contains("name='enabled' value='on'"), "{off}");
        sb.seed(true, vec![]);
        let on = sb.get("/loops/issue-loop?view=todos");
        assert!(on.contains("aria-checked='true'") && on.contains("name='enabled' value='0'"), "{on}");
        // Each interval button posts itself and keeps the switch as it is.
        assert!(on.contains("name='interval_minutes' value='10'"), "{on}");
        assert!(on.contains("name='enabled' value='on'"), "{on}");
    }

    #[test]
    fn refresh_now_fetches_once_even_while_sync_is_off_and_last_fetched_follows_it() {
        let sb = Sb::new();
        let r = sb.post("/todos/refresh", "");
        let loc = r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("location")).unwrap().1.clone();
        assert!(loc.contains("ok=1"), "{loc}");
        let s = todos::load(&sb.store_path()).unwrap();
        assert!(!s.settings.enabled);
        assert!(s.instances["work"].last_error.is_some(), "the unreachable GitLab is recorded");
        let html = sb.get("/loops/issue-loop?view=todos");
        let refresh = html.find("action='/todos/refresh'").unwrap();
        let last = html.find("Not fetched yet").or_else(|| html.find("Last fetched")).unwrap();
        assert!(refresh < last, "{html}");
    }
}
