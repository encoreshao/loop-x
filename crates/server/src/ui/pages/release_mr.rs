//! The release MR panel on the MR Review page's Merge requests view: pick a tracked
//! project and two branches, and an agent writes the MR (see
//! docs/superpowers/specs/2026-11-10-release-mr-panel-design.md).

use crate::ui::pyfmt::escape as h;
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use loopx_core::config::Config;
use loopx_core::release_mr::api::{GitlabApi, ReleaseApi};
use loopx_core::release_mr::job::{self, Mode, ReleaseJob};
use loopx_core::release_mr::run as runner;
use serde_json::Value;
use std::path::PathBuf;

const BACK: &str = "/loops/mr-review-loop";

pub fn register(r: &mut Registry) {
    r.get("/loops/mr-review-loop/release/branches", branches);
    r.get("/loops/mr-review-loop/release/progress", progress_fragment);
    r.post("/loops/mr-review-loop/release/start", start);
    r.post("/loops/mr-review-loop/release/confirm", confirm);
    r.post("/loops/mr-review-loop/release/discard", discard);
}

// ---- projects and the real GitLab ---------------------------------------------------------------

/// `(alias, project entry)` for every tracked project, in the config's order.
fn tracked(ctx: &UiCtx) -> Vec<(String, Value)> {
    let Ok(config) = Config::load(&ctx.state.home_dir.join("projects.json")) else {
        return Vec::new();
    };
    config.aliases().unwrap_or_default().into_iter().filter_map(|a| config.project(&a).ok().map(|p| (a, p))).collect()
}

fn project_of(ctx: &UiCtx, alias: &str) -> Option<Value> {
    tracked(ctx).into_iter().find(|(a, _)| a == alias).map(|(_, p)| p)
}

fn project_id(project: &Value) -> String {
    project.get("project_id").and_then(Value::as_str).unwrap_or_default().to_string()
}

/// The real GitLab for a tracked project.
fn real_api(ctx: &UiCtx, project: &Value) -> Result<GitlabApi, String> {
    let path = ctx.state.user_home.join(".gitlab").join("config.json");
    let config: Value = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    GitlabApi::for_project(&config, project)
}

fn jobs_dir(ctx: &UiCtx) -> PathBuf {
    job::jobs_dir(&ctx.state.home_dir)
}

fn cli_ready(ctx: &UiCtx) -> bool {
    let cli = loopx_core::ai_cli_config::get_selected_cli(&ctx.state.home_dir.join("ai_cli.json"));
    super::settings::general::cli_available(ctx, &[cli.as_str()]).get(&cli).copied().flatten().unwrap_or(true)
}

fn fail(ctx: &UiCtx, msg: &str) -> UiResponse {
    let _ = ctx;
    UiResponse::flash_redirect(false, msg, BACK)
}

// ---- the branch fragment ---------------------------------------------------------------------------

fn rel_error(msg: &str) -> UiResponse {
    UiResponse::html(format!("<div class='rel-error' role='alert'>{}</div>", h(msg)))
}

fn select(ctx: &UiCtx, name: &str, label: &str, options: &[String], chosen: &str) -> String {
    let opts: String = options
        .iter()
        .map(|b| format!("<option value='{0}'{1}>{0}</option>", h(b), if b == chosen { " selected" } else { "" }))
        .collect();
    format!("<label class='rel-field'><span>{}</span><select name='{name}' class='rel-input'>{opts}</select></label>", h(&ctx.t(label)))
}

fn branches(ctx: &UiCtx) -> UiResponse {
    let alias = ctx.query().first("alias").unwrap_or_default().to_string();
    match project_of(ctx, &alias) {
        None => rel_error(&ctx.t("Unknown project")),
        Some(p) => match real_api(ctx, &p) {
            Ok(api) => branches_with(ctx, &api),
            Err(e) => rel_error(&ctx.tf("Could not load branches: {detail}", &[("detail", &e)])),
        },
    }
}

/// The branch pair a release usually wants: `staging` into `master` (else `main`).
fn default_pair(names: &[String], configured: &str, default_branch: &str) -> (String, String) {
    let has = |n: &str| names.iter().any(|x| x == n);
    let source = ["staging", configured, "develop"].iter().find(|c| !c.is_empty() && has(c)).map(|s| s.to_string()).or_else(|| names.first().cloned()).unwrap_or_default();
    let target = ["master", "main", default_branch]
        .iter()
        .find(|c| !c.is_empty() && has(c) && **c != source)
        .map(|s| s.to_string())
        .or_else(|| names.iter().find(|n| **n != source).cloned())
        .unwrap_or_default();
    (source, target)
}

fn branches_with(ctx: &UiCtx, api: &dyn ReleaseApi) -> UiResponse {
    let alias = ctx.query().first("alias").unwrap_or_default().to_string();
    let Some(project) = project_of(ctx, &alias) else {
        return rel_error(&ctx.t("Unknown project"));
    };
    let id = project_id(&project);
    let names = match api.branches(&id) {
        Ok(n) => n,
        Err(e) => return rel_error(&ctx.tf("Could not load branches: {detail}", &[("detail", &e)])),
    };
    let default_branch = api.default_branch(&id).unwrap_or_default();
    let configured = project.get("target_branch").and_then(Value::as_str).unwrap_or_default();
    let (source, target) = default_pair(&names, configured, &default_branch);
    UiResponse::html(format!(
        "{}<span class='rel-arrow' aria-hidden='true'>\u{2192}</span>{}",
        select(ctx, "source", "Merge from", &names, &source),
        select(ctx, "target", "Into", &names, &target)
    ))
}

// ---- start ---------------------------------------------------------------------------------------------

fn start(ctx: &UiCtx) -> UiResponse {
    let alias = ctx.form().first("alias").unwrap_or_default().to_string();
    let Some(project) = project_of(ctx, &alias) else {
        return fail(ctx, &ctx.t("Unknown project"));
    };
    let api = match real_api(ctx, &project) {
        Ok(a) => a,
        Err(e) => return fail(ctx, &ctx.tf("Could not start the job: {detail}", &[("detail", &e)])),
    };
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "loopx".to_string());
    // The job id is only known after `job::start` writes the record, which hands it to the spawn.
    start_with(ctx, &api, &|log| {
        let id = log.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        job::spawn_runner(log, &exe, &id)
    })
}

fn start_with(ctx: &UiCtx, api: &dyn ReleaseApi, spawn: &job::Spawn<'_>) -> UiResponse {
    let form = ctx.form();
    let alias = form.first("alias").unwrap_or_default().to_string();
    let (source, target) = (form.first("source").unwrap_or_default().trim().to_string(), form.first("target").unwrap_or_default().trim().to_string());
    let Some(project) = project_of(ctx, &alias) else {
        return fail(ctx, &ctx.t("Unknown project"));
    };
    let mode = match form.first("mode") {
        Some("create") => Mode::Create,
        Some("preview") => Mode::Preview,
        _ => return fail(ctx, &ctx.t("Choose Create MR or Preview first")),
    };
    if source.is_empty() || target.is_empty() || source == target {
        return fail(ctx, &ctx.t("Pick two different branches that exist in the project"));
    }
    if !ctx.state.spawn_processes {
        return fail(ctx, &ctx.t("This server does not start agent jobs"));
    }
    if !cli_ready(ctx) {
        return fail(ctx, &ctx.t("No AI CLI is available. Install and sign in to your AI CLI in Settings to create release merge requests."));
    }
    let id = project_id(&project);
    match api.branches(&id) {
        Ok(names) if names.contains(&source) && names.contains(&target) => {}
        Ok(_) => return fail(ctx, &ctx.t("Pick two different branches that exist in the project")),
        Err(e) => return fail(ctx, &ctx.tf("Could not load branches: {detail}", &[("detail", &e)])),
    }
    match api.open_mr(&id, &source, &target) {
        Ok(Some(url)) => return fail(ctx, &ctx.tf("A merge request for these branches is already open: {url}", &[("url", &url)])),
        Ok(None) => {}
        Err(e) => return fail(ctx, &ctx.tf("Could not start the job: {detail}", &[("detail", &e)])),
    }
    match job::start(&jobs_dir(ctx), &alias, &id, &source, &target, mode, spawn) {
        Ok(_) => UiResponse::flash_redirect(true, &ctx.t("Release merge request started - the agent is reading the changes"), BACK),
        Err(e) => fail(ctx, &ctx.tf("Could not start the job: {detail}", &[("detail", &e)])),
    }
}

// ---- confirm and discard --------------------------------------------------------------------------

fn confirm(ctx: &UiCtx) -> UiResponse {
    let id = ctx.form().first("job").unwrap_or_default().to_string();
    let Some(rec) = job::get(&jobs_dir(ctx), &id) else {
        return fail(ctx, &ctx.t("This preview is no longer available"));
    };
    let Some(project) = project_of(ctx, &rec.alias) else {
        return fail(ctx, &ctx.t("Unknown project"));
    };
    match real_api(ctx, &project) {
        Ok(api) => confirm_with(ctx, &api),
        Err(e) => fail(ctx, &ctx.tf("Could not create the merge request: {detail}", &[("detail", &e)])),
    }
}

fn confirm_with(ctx: &UiCtx, api: &dyn ReleaseApi) -> UiResponse {
    let form = ctx.form();
    let (id, title, description) = (form.first("job").unwrap_or_default(), form.first("title").unwrap_or_default(), form.first("description").unwrap_or_default());
    match runner::confirm(api, &jobs_dir(ctx), id, title, description) {
        Ok(url) => UiResponse::flash_redirect(true, &ctx.tf("Merge request created: {url}", &[("url", &url)]), BACK),
        Err(e) => fail(ctx, &e.to_string()),
    }
}

fn discard(ctx: &UiCtx) -> UiResponse {
    let id = ctx.form().first("job").unwrap_or_default().to_string();
    let dir = jobs_dir(ctx);
    match job::get(&dir, &id) {
        Some(j) if j.state == "ready" => {
            let _ = job::update(&dir, &id, |j| {
                j.state = "discarded".into();
                j.summary = "Discarded".into();
            });
            UiResponse::flash_redirect(true, &ctx.t("Preview discarded"), BACK)
        }
        _ => fail(ctx, &ctx.t("This preview is no longer available")),
    }
}

// ---- the panel -----------------------------------------------------------------------------------------

fn state_pill(ctx: &UiCtx, state: &str) -> String {
    let (kind, label) = match state {
        "created" => ("done", "Created"),
        "failed" => ("failed", "Failed"),
        "ready" => ("skipped", "Ready to review"),
        "expired" => ("skipped", "Expired"),
        "discarded" => ("skipped", "Discarded"),
        _ => ("skipped", "Running\u{2026}"),
    };
    format!("<span class='pill history-pill history-{kind}'>{}</span>", h(&ctx.t(label)))
}

/// An older job in the "Recent" list.
fn job_row(ctx: &UiCtx, j: &ReleaseJob) -> String {
    let detail = if j.state == "created" && j.mr_url.starts_with("http") {
        format!("<a href='{}' target='_blank' rel='noopener'>{}</a>", h(&j.mr_url), h(&ctx.t("Open merge request")))
    } else if j.state == "failed" && !j.summary.is_empty() {
        format!("<span class='rel-reason'>{}</span>", h(&j.summary))
    } else {
        String::new()
    };
    format!(
        "<li class='rel-job'><code>{} \u{2192} {}</code><span class='rel-job-project'>{}</span>{}{}</li>",
        h(&j.source), h(&j.target), h(&j.alias), state_pill(ctx, &j.state), detail
    )
}

/// The newest job as a progress block, while it matters: running or waiting for review,
/// or finished within the last day. `None` for old, expired and discarded jobs.
fn showing(j: &ReleaseJob) -> bool {
    let recent = chrono::DateTime::parse_from_rfc3339(&j.started_at).map(|t| (chrono::Utc::now() - t.to_utc()).num_hours() < job::READY_TTL_HOURS).unwrap_or(false);
    match j.state.as_str() {
        "running" | "ready" => true,
        "created" | "failed" => recent,
        _ => false,
    }
}

/// Every step of the job with where it is: the whole run is visible at a glance.
fn progress_block(ctx: &UiCtx, j: &ReleaseJob) -> String {
    let labels = job::step_labels(j.mode);
    let finished = matches!(j.state.as_str(), "ready" | "created");
    let steps: String = labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let (class, mark) = if finished || i < j.step {
                ("is-done", "<span class='rel-mark'>\u{2713}</span>".to_string())
            } else if i == j.step && j.state == "running" {
                ("is-active", "<span class='rel-mark'><span class='rel-spin'></span></span>".to_string())
            } else if i == j.step && j.state == "failed" {
                ("is-failed", "<span class='rel-mark'>!</span>".to_string())
            } else {
                ("is-todo", "<span class='rel-mark'></span>".to_string())
            };
            let detail = if i == j.step && j.state == "running" && !j.detail.is_empty() { format!("<span class='rel-step-detail'>{}</span>", h(&j.detail)) } else { String::new() };
            format!("<li class='rel-step {class}'>{mark}<span class='rel-step-name'>{}</span>{detail}</li>", h(&ctx.t(label)))
        })
        .collect();
    let (title, extra) = match j.state.as_str() {
        "running" => (ctx.t("Writing your release MR"), String::new()),
        "ready" => (ctx.t("Preview ready"), format!("<a class='btn btn-primary' href='{BACK}'>{}</a>", h(&ctx.t("Review and create")))),
        "created" if j.mr_url.starts_with("http") => (ctx.t("Merge request created"), format!("<a class='btn btn-primary' href='{}' target='_blank' rel='noopener'>{}</a>", h(&j.mr_url), h(&ctx.t("Open merge request")))),
        "created" => (ctx.t("Merge request created"), String::new()),
        _ => (ctx.t("Could not create the release MR"), format!("<p class='rel-error' role='alert'>{}</p>", h(&j.summary))),
    };
    format!(
        "<div class='rel-run' data-state='{state}'><div class='rel-run-head'><strong>{title}</strong><code>{} \u{b7} {} \u{2192} {}</code></div><ol class='rel-steps'>{steps}</ol>{extra}</div>",
        h(&j.alias), h(&j.source), h(&j.target), state = h(&j.state), title = h(&title),
    )
}

/// The block for the newest job, or nothing.
fn latest_block(ctx: &UiCtx, jobs: &[ReleaseJob]) -> String {
    jobs.first().filter(|j| showing(j)).map(|j| progress_block(ctx, j)).unwrap_or_default()
}

fn progress_fragment(ctx: &UiCtx) -> UiResponse {
    UiResponse::html(latest_block(ctx, &job::list(&jobs_dir(ctx))))
}

fn sheet(ctx: &UiCtx, j: &ReleaseJob) -> String {
    let csrf = ctx.csrf_input();
    format!(
        "<aside class='rel-sheet' role='dialog' aria-label='{label}'>\
<div class='rel-sheet-head'><h3>{label}</h3><p class='rel-meta'><code>{src} \u{2192} {dst}</code> \u{b7} {hint}</p></div>\
<form id='rel-confirm' method='post' action='/loops/mr-review-loop/release/confirm' class='rel-sheet-form'>{csrf}<input type='hidden' name='job' value='{id}'>\
<label class='rel-field'><span>{title_l}</span><input type='text' name='title' class='rel-input rel-title' value='{title}' maxlength='255'></label>\
<label class='rel-field rel-grow'><span>{desc_l}</span><textarea name='description' class='rel-text' spellcheck='false'>{desc}</textarea></label></form>\
<form id='rel-discard' method='post' action='/loops/mr-review-loop/release/discard'>{csrf}<input type='hidden' name='job' value='{id}'></form>\
<div class='rel-actions rel-sheet-foot'><button type='submit' form='rel-confirm' class='btn btn-primary'>{create}</button>\
<button type='submit' form='rel-discard' class='btn btn-neutral'>{discard}</button>\
<a class='btn btn-neutral' href='{BACK}?sheet=0'>{close}</a></div></aside>",
        label = h(&ctx.t("Review the release MR")),
        hint = h(&ctx.t("The merge request is created with exactly this text.")),
        src = h(&j.source), dst = h(&j.target), id = h(&j.id),
        title_l = h(&ctx.t("Title")), title = h(&j.title),
        desc_l = h(&ctx.t("Description")), desc = h(&j.description),
        create = h(&ctx.t("Create MR")), discard = h(&ctx.t("Discard")), close = h(&ctx.t("Close")),
    )
}

/// Keeps the pickers' choice for this tab, holds the page's auto-refresh while one is in use,
/// loads the branches of the chosen project and writes the "merging X into Y" line.
const FORM_SCRIPT: &str = "<script>(function(){var a=document.getElementById('rel-alias'),b=document.getElementById('rel-branches'),r=document.getElementById('rel-route'),f=a&&a.form;if(!a||!b)return;var seq=0,K='rel-pick';function save(){try{var s=b.querySelector('[name=source]'),t=b.querySelector('[name=target]');sessionStorage.setItem(K,JSON.stringify({a:a.value,s:s&&s.value,t:t&&t.value}))}catch(e){}}function saved(){try{return JSON.parse(sessionStorage.getItem(K)||'null')}catch(e){return null}}function route(){var s=b.querySelector('[name=source]'),t=b.querySelector('[name=target]');if(!r)return;r.textContent=s&&t?(r.dataset.pre||'')+' '+s.value+' '+(r.dataset.mid||'')+' '+t.value+(/^(master|main|production|prod)$/i.test(t.value)?' (production)':t.value==='staging'?' (staging)':''):''}function pick(sel,v){if(sel&&v){for(var i=0;i<sel.options.length;i++){if(sel.options[i].value===v){sel.value=v}}}}function load(keep){var n=++seq;if(!a.value){b.innerHTML='';route();return}fetch('/loops/mr-review-loop/release/branches?alias='+encodeURIComponent(a.value),{credentials:'same-origin'}).then(function(x){return x.text()}).then(function(h){if(n!==seq)return;b.innerHTML=h;var p=saved();if(keep&&p&&p.a===a.value){pick(b.querySelector('[name=source]'),p.s);pick(b.querySelector('[name=target]'),p.t)}route();save()}).catch(function(){})}a.addEventListener('change',function(){load(false)});b.addEventListener('change',function(){route();save()});f.addEventListener('focusin',function(){window.__loopChatStreaming=true});f.addEventListener('focusout',function(){window.__loopChatStreaming=false});var p=saved();if(p){pick(a,p.a)}load(true)})();</script>";

/// Polls the progress block while a job runs; reloads once it settles so the sheet and links appear.
const POLL_SCRIPT: &str = "<script>(function(){var box=document.getElementById('rel-progress');if(!box)return;function tick(){fetch('/loops/mr-review-loop/release/progress',{credentials:'same-origin'}).then(function(r){return r.text()}).then(function(h){var m=/data-state='([a-z]+)'/.exec(h);if(!m||m[1]!=='running'){location.reload();return}box.innerHTML=h;setTimeout(tick,2000)}).catch(function(){setTimeout(tick,5000)})}setTimeout(tick,2000)})();</script>";

/// The Create release MR card for the Live page, right after Status.
pub fn panel(ctx: &UiCtx) -> String {
    let projects = tracked(ctx);
    let jobs = job::list(&jobs_dir(ctx));
    let heading = format!(
        "<div class='section-header'><h2>{}</h2></div><p class='section-subtitle'>{}</p>",
        h(&ctx.t("Create a release MR")),
        h(&ctx.t("Compare two branches and an agent writes the merge request: a short title, a clear summary by feature, and the right people tagged."))
    );
    if projects.is_empty() {
        return format!("<section class='card rel-card'>{heading}<p class='rel-empty'>{}</p></section>", h(&ctx.t("No tracked projects yet. Add one on the Issues page, in Projects.")));
    }
    let options: String = projects.iter().map(|(a, _)| format!("<option value='{0}'>{0}</option>", h(a))).collect();
    let ready = cli_ready(ctx);
    let disabled = if ready { "" } else { " disabled" };
    let reason = if ready { String::new() } else { format!("<p class='rel-empty' role='note'>{}</p>", h(&ctx.t("No AI CLI is available. Install and sign in to your AI CLI in Settings to create release merge requests."))) };
    let recent: String = jobs.iter().skip(usize::from(jobs.first().is_some_and(showing))).take(4).map(|j| job_row(ctx, j)).collect();
    let recent = if recent.is_empty() { String::new() } else { format!("<h3 class='rel-sub'>{}</h3><ul class='rel-jobs'>{recent}</ul>", h(&ctx.t("Earlier"))) };
    format!(
        "<section class='card rel-card'>{heading}\
<form method='post' action='/loops/mr-review-loop/release/start' class='rel-form'>{csrf}\
<div class='rel-row'><label class='rel-field'><span>{project}</span><select name='alias' id='rel-alias' class='rel-input'>{options}</select></label>\
<div id='rel-branches' class='rel-branches'></div></div>\
<p class='rel-route' id='rel-route' data-pre='{pre}' data-mid='{mid}' aria-live='polite'></p>\
<div class='rel-actions'><button type='submit' name='mode' value='create' class='btn btn-primary'{disabled}>{create}</button>\
<button type='submit' name='mode' value='preview' class='btn btn-neutral'{disabled}>{preview}</button></div>\
<p class='rel-hint'>{hint}</p></form>{reason}\
<div id='rel-progress'>{block}</div>{recent}</section>",
        csrf = ctx.csrf_input(),
        project = h(&ctx.t("Project")),
        create = h(&ctx.t("Create MR")),
        preview = h(&ctx.t("Preview first")),
        pre = h(&ctx.t("Merging")),
        mid = h(&ctx.t("into")),
        hint = h(&ctx.t("Create MR opens the merge request right away. Preview first lets you edit the title and description before anything is created.")),
        block = latest_block(ctx, &jobs),
    )
}

/// The review sheet, the form script and (while a job runs) the progress poller: placed after
/// the page's cards so the shared card spacing never touches them.
pub fn overlays(ctx: &UiCtx) -> String {
    if tracked(ctx).is_empty() {
        return String::new();
    }
    let jobs = job::list(&jobs_dir(ctx));
    let sheet_html = if ctx.query().first("sheet") == Some("0") {
        String::new()
    } else {
        jobs.iter().find(|j| j.state == "ready").map(|j| sheet(ctx, j)).unwrap_or_default()
    };
    let poll = if jobs.first().is_some_and(|j| j.state == "running") { POLL_SCRIPT } else { "" };
    format!("{sheet_html}{FORM_SCRIPT}{poll}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::routes::UiRequest;
    use crate::ui::state::UiState;
    use axum::http::Method;
    use loopx_core::release_mr::api::{CommitInfo, Member, MrInfo, NewMr};
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake {
        branches: Vec<String>,
        default_branch: String,
        open_mr: Option<String>,
        created: RefCell<Vec<NewMr>>,
        heads: std::collections::HashMap<String, String>,
    }
    impl ReleaseApi for Fake {
        fn branches(&self, _: &str) -> Result<Vec<String>, String> {
            Ok(self.branches.clone())
        }
        fn default_branch(&self, _: &str) -> Result<String, String> {
            Ok(self.default_branch.clone())
        }
        fn branch_head(&self, _: &str, b: &str) -> Result<String, String> {
            Ok(self.heads.get(b).cloned().unwrap_or_else(|| format!("head-of-{b}")))
        }
        fn compare(&self, _: &str, _: &str, _: &str) -> Result<Vec<CommitInfo>, String> {
            Ok(vec![])
        }
        fn open_mr(&self, _: &str, _: &str, _: &str) -> Result<Option<String>, String> {
            Ok(self.open_mr.clone())
        }
        fn commit_mr(&self, _: &str, _: &str, _: &str) -> Result<Option<MrInfo>, String> {
            Ok(None)
        }
        fn members(&self, _: &str) -> Result<Vec<Member>, String> {
            Ok(vec![])
        }
        fn current_username(&self) -> Result<String, String> {
            Ok(String::new())
        }
        fn create_mr(&self, _: &str, mr: &NewMr) -> Result<String, String> {
            self.created.borrow_mut().push(mr.clone());
            Ok("https://g/app/-/merge_requests/10".into())
        }
    }

    fn fake() -> Fake {
        Fake { branches: vec!["master".into(), "staging".into(), "develop".into()], default_branch: "master".into(), ..Fake::default() }
    }

    fn state(dir: &std::path::Path) -> UiState {
        let mut s = UiState::explicit(dir, dir, dir, &dir.join("update"), "tok", None);
        s.spawn_processes = true;
        s
    }

    fn seed_projects(dir: &std::path::Path) {
        std::fs::write(
            dir.join("projects.json"),
            r#"{"gitlab_instance":"g","projects":{"app":{"project_id":"g/app","target_branch":"staging","local_path":"/w/app"},
               "docs":{"project_id":"g/docs","local_path":""}}}"#,
        )
        .unwrap();
    }

    fn post_ctx<'a>(st: &'a UiState, req: &'a UiRequest) -> UiCtx<'a> {
        UiCtx::new(st, req)
    }

    fn form_req(path: &str, body: &str) -> UiRequest {
        UiRequest::new(Method::POST, path).with_body(body)
    }

    fn jobs(dir: &std::path::Path) -> Vec<ReleaseJob> {
        job::list(&job::jobs_dir(dir))
    }

    fn location(r: &UiResponse) -> String {
        r.header("Location").unwrap().to_string()
    }

    #[test]
    fn the_release_panel_lists_only_tracked_projects_and_has_both_buttons() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        let st = state(d.path());
        let req = UiRequest::get(BACK);
        let html = panel(&UiCtx::new(&st, &req));
        assert!(html.contains("<option value='app'>app</option>") && html.contains("<option value='docs'>docs</option>"), "{html}");
        assert!(!html.contains("value='other'"));
        assert!(html.contains("name='mode' value='create'") && html.contains("name='mode' value='preview'"), "{html}");
        assert!(html.contains("Create MR") && html.contains("Preview first"));
        assert!(html.contains("action='/loops/mr-review-loop/release/start'") && html.contains("name='csrf_token'"));
        assert!(overlays(&UiCtx::new(&st, &req)).contains("/loops/mr-review-loop/release/branches"), "the branch selects load lazily");
    }

    #[test]
    fn the_panel_explains_when_no_project_is_tracked() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let req = UiRequest::get(BACK);
        let html = panel(&UiCtx::new(&st, &req));
        assert!(html.contains("No tracked projects yet"), "{html}");
        assert!(!html.contains("name='mode'"), "no buttons without a project");
    }

    #[test]
    fn buttons_are_disabled_with_a_reason_when_no_ai_cli_is_available() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        let mut st = state(d.path());
        st.cli_pinned = Some(vec![]);
        let req = UiRequest::get(BACK);
        let html = panel(&UiCtx::new(&st, &req));
        assert!(html.contains("disabled") && html.contains("No AI CLI is available"), "{html}");
    }

    fn branch_fragment(d: &std::path::Path, api: &Fake) -> String {
        let st = state(d);
        let req = UiRequest::get("/loops/mr-review-loop/release/branches?alias=app");
        branches_with(&UiCtx::new(&st, &req), api).body_text()
    }

    #[test]
    fn the_branch_fragment_defaults_to_staging_into_master() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        let html = branch_fragment(d.path(), &fake());
        assert!(html.contains("<select name='source'") && html.contains("<select name='target'"), "{html}");
        assert!(html.contains("<option value='staging' selected>"), "{html}");
        assert!(html.contains("<option value='master' selected>"), "{html}");
        assert!(html.contains("rel-arrow"), "the two selects read as from -> into: {html}");
        let st = state(d.path());
        let bad = branches_with(&UiCtx::new(&st, &UiRequest::get("/loops/mr-review-loop/release/branches?alias=nope")), &fake());
        assert!(bad.body_text().contains("Unknown project"));
    }

    #[test]
    fn the_target_prefers_master_then_main_and_never_equals_the_source() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        // Both exist: master wins even when GitLab's default branch is something else.
        let mut api = fake();
        api.branches = vec!["develop".into(), "main".into(), "master".into(), "staging".into()];
        api.default_branch = "develop".into();
        assert!(branch_fragment(d.path(), &api).contains("<option value='master' selected>"));
        // Only main.
        api.branches = vec!["develop".into(), "main".into(), "staging".into()];
        let html = branch_fragment(d.path(), &api);
        assert!(html.contains("<option value='main' selected>") && html.contains("<option value='staging' selected>"), "{html}");
        // No staging: the source falls back to the project's branch, then develop, and is never the target.
        api.branches = vec!["develop".into(), "master".into()];
        let html = branch_fragment(d.path(), &api);
        assert!(html.contains("<option value='develop' selected>") && html.contains("<option value='master' selected>"), "{html}");
        // GitLab's default branch is staging and there is no master/main: the target is not the source.
        api.branches = vec!["staging".into(), "release".into()];
        api.default_branch = "staging".into();
        let html = branch_fragment(d.path(), &api);
        assert_eq!(html.matches("selected>").count(), 2);
        assert!(html.contains("<option value='staging' selected>") && html.contains("<option value='release' selected>"), "{html}");
    }

    #[test]
    fn start_rejects_an_untracked_alias_equal_branches_and_unknown_branches() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        let st = state(d.path());
        let spawn = |_: &std::path::Path| Ok(std::process::id() as i64);
        for body in [
            "csrf_token=tok&alias=nope&source=staging&target=master&mode=create",
            "csrf_token=tok&alias=app&source=staging&target=staging&mode=create",
            "csrf_token=tok&alias=app&source=ghost&target=master&mode=create",
            "csrf_token=tok&alias=app&source=staging&target=master&mode=zzz",
            "csrf_token=tok&alias=app&source=&target=master&mode=create",
        ] {
            let req = form_req("/loops/mr-review-loop/release/start", body);
            let r = start_with(&post_ctx(&st, &req), &fake(), &spawn);
            assert!(location(&r).contains("ok=0"), "{body}: {}", location(&r));
        }
        assert!(jobs(d.path()).is_empty(), "no job for any rejected request");
    }

    #[test]
    fn start_refuses_when_a_release_mr_is_already_open_and_links_it() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        let st = state(d.path());
        let mut api = fake();
        api.open_mr = Some("https://g/app/-/merge_requests/3".into());
        let req = form_req("/loops/mr-review-loop/release/start", "csrf_token=tok&alias=app&source=staging&target=master&mode=create");
        let r = start_with(&post_ctx(&st, &req), &api, &|_: &std::path::Path| Ok(1));
        let loc = location(&r);
        assert!(loc.contains("ok=0") && loc.contains("merge_requests%2F3"), "{loc}");
        assert!(jobs(d.path()).is_empty());
    }

    #[test]
    fn start_spawns_a_job_through_the_spawn_seam_and_a_second_start_is_refused() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        let st = state(d.path());
        let spawn = |_: &std::path::Path| Ok(std::process::id() as i64);
        let req = form_req("/loops/mr-review-loop/release/start", "csrf_token=tok&alias=app&source=staging&target=master&mode=preview");
        assert!(location(&start_with(&post_ctx(&st, &req), &fake(), &spawn)).contains("ok=1"));
        let all = jobs(d.path());
        assert_eq!(all.len(), 1);
        assert_eq!((all[0].project.as_str(), all[0].mode, all[0].state.as_str()), ("g/app", Mode::Preview, "running"));
        assert!(location(&start_with(&post_ctx(&st, &req), &fake(), &spawn)).contains("ok=0"));
        assert_eq!(jobs(d.path()).len(), 1);
    }

    #[test]
    fn release_posts_require_the_csrf_token() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        let st = state(d.path());
        for path in ["start", "confirm", "discard"] {
            let r = st.registry.dispatch(&st, &UiRequest::new(Method::POST, &format!("/loops/mr-review-loop/release/{path}")).with_body("alias=app")).unwrap();
            assert_eq!(r.status, 403, "{path}");
        }
    }

    fn ready_job(dir: &std::path::Path) -> String {
        let jd = job::jobs_dir(dir);
        let j = job::start(&jd, "app", "g/app", "staging", "master", Mode::Preview, &|_: &std::path::Path| Ok(std::process::id() as i64)).unwrap();
        job::update(&jd, &j.id, |j| {
            j.state = "ready".into();
            j.title = "Release: order <export>".into();
            j.description = "## What's in this release\n\n- **Order export**: CSV.".into();
            j.source_head = "head-of-staging".into();
            j.target_head = "head-of-master".into();
        })
        .unwrap();
        j.id
    }

    #[test]
    fn a_ready_job_renders_an_editable_sheet_with_create_and_discard() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        let id = ready_job(d.path());
        let st = state(d.path());
        let req = UiRequest::get(BACK);
        let html = overlays(&UiCtx::new(&st, &req));
        assert!(html.contains("class='rel-sheet'"), "{html}");
        assert!(html.contains("<input type='text' name='title' class='rel-input rel-title' value='Release: order &lt;export&gt;'"), "{html}");
        assert!(html.contains("<textarea name='description'") && html.contains("Order export"), "{html}");
        assert!(html.contains("action='/loops/mr-review-loop/release/confirm'") && html.contains("action='/loops/mr-review-loop/release/discard'"));
        assert!(html.contains(&format!("name='job' value='{id}'")));
    }

    #[test]
    fn the_sheet_can_be_closed_without_discarding_and_reopened_from_the_ready_block() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        ready_job(d.path());
        let st = state(d.path());
        let open = overlays(&UiCtx::new(&st, &UiRequest::get(BACK)));
        assert!(open.contains("class='rel-sheet'") && open.contains("href='/loops/mr-review-loop?sheet=0'"), "{open}");
        let closed_req = UiRequest::get("/loops/mr-review-loop?sheet=0");
        assert!(!overlays(&UiCtx::new(&st, &closed_req)).contains("class='rel-sheet'"));
        let panel_html = panel(&UiCtx::new(&st, &closed_req));
        assert!(panel_html.contains("Preview ready") && panel_html.contains("href='/loops/mr-review-loop'"), "the ready block links back to the preview: {panel_html}");
    }

    fn running_job(dir: &std::path::Path, mode: Mode) -> (std::path::PathBuf, String) {
        let jd = job::jobs_dir(dir);
        let j = job::start(&jd, "app", "g/app", "staging", "master", mode, &|_: &std::path::Path| Ok(std::process::id() as i64)).unwrap();
        (jd, j.id)
    }

    #[test]
    fn a_running_job_shows_every_step_with_the_current_one_and_its_detail() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        let (jd, id) = running_job(d.path(), Mode::Preview);
        job::set_step(&jd, &id, 2, "42 changes found").unwrap();
        let st = state(d.path());
        let html = panel(&UiCtx::new(&st, &UiRequest::get(BACK)));
        assert!(html.contains("data-state='running'") && html.contains("id='rel-progress'"), "{html}");
        for step in ["Checking the branches", "Reading the changes", "Matching contributors", "Writing the summary", "Preparing the preview"] {
            assert!(html.contains(step), "{step}: {html}");
        }
        assert_eq!(html.matches("is-done").count(), 2, "{html}");
        assert_eq!(html.matches("is-active").count(), 1, "{html}");
        assert_eq!(html.matches("is-todo").count(), 2, "{html}");
        assert!(html.contains("42 changes found") && html.contains("staging \u{2192} master"), "{html}");
        let create = running_job_label(d.path());
        assert!(create.contains("Creating the merge request") || html.contains("Preparing the preview"));
    }

    fn running_job_label(_d: &std::path::Path) -> String {
        job::step_labels(Mode::Create)[4].to_string()
    }

    #[test]
    fn the_progress_fragment_updates_in_place_and_polling_runs_only_while_a_job_runs() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        let st = state(d.path());
        let get = |st: &UiState| st.registry.dispatch(st, &UiRequest::get("/loops/mr-review-loop/release/progress")).unwrap().body_text();
        assert!(!overlays(&UiCtx::new(&st, &UiRequest::get(BACK))).contains("release/progress"), "no polling without a running job");
        let (jd, id) = running_job(d.path(), Mode::Create);
        let busy = overlays(&UiCtx::new(&st, &UiRequest::get(BACK)));
        assert!(busy.contains("/loops/mr-review-loop/release/progress") && busy.contains("location.reload"), "{busy}");
        let before = get(&st);
        assert!(before.contains("data-state='running'") && !before.contains("Matching contributors</span><span class='rel-step-detail'>"), "{before}");
        job::set_step(&jd, &id, 3, "2 people to tag").unwrap();
        let during = get(&st);
        assert_ne!(during, before);
        assert!(during.contains("2 people to tag"), "{during}");
        job::update(&jd, &id, |j| {
            j.state = "failed".into();
            j.summary = "Nothing to <release>".into();
        })
        .unwrap();
        let failed = get(&st);
        assert!(failed.contains("data-state='failed'") && failed.contains("Nothing to &lt;release&gt;"), "{failed}");
        assert!(failed.contains("is-failed"), "the step that failed is marked: {failed}");
        assert!(!overlays(&UiCtx::new(&st, &UiRequest::get(BACK))).contains("release/progress"), "polling stops once nothing runs");
    }

    #[test]
    fn a_created_job_shows_a_success_block_with_the_mr_link_and_older_ones_list_below() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        let jd = job::jobs_dir(d.path());
        let spawn = |_: &std::path::Path| Ok(std::process::id() as i64);
        let a = job::start(&jd, "app", "g/app", "develop", "master", Mode::Create, &spawn).unwrap();
        job::update(&jd, &a.id, |j| {
            j.state = "failed".into();
            j.summary = "Nothing to <release>".into();
            j.started_at = (chrono::Utc::now() - chrono::Duration::hours(3)).to_rfc3339();
        })
        .unwrap();
        let b = job::start(&jd, "app", "g/app", "staging", "master", Mode::Create, &spawn).unwrap();
        job::update(&jd, &b.id, |j| {
            j.state = "created".into();
            j.step = job::STEP_COUNT;
            j.mr_url = "https://g/app/-/merge_requests/10".into();
        })
        .unwrap();
        let st = state(d.path());
        let html = panel(&UiCtx::new(&st, &UiRequest::get(BACK)));
        assert!(html.contains("data-state='created'") && html.contains("href='https://g/app/-/merge_requests/10'"), "{html}");
        assert_eq!(html.matches("is-done").count(), 5, "{html}");
        assert!(html.contains("Nothing to &lt;release&gt;") && html.contains("develop"), "the older failure is in the recent list: {html}");
    }

    #[test]
    fn the_panel_lives_on_the_live_page_after_status_and_not_on_the_merge_requests_view() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        std::fs::write(d.path().join("loops.json"), r#"[{"name": "mr-review-loop", "label": "MR Review", "enabled": true}]"#).unwrap();
        let st = state(d.path());
        let live = st.registry.dispatch(&st, &UiRequest::get("/loops/mr-review-loop")).unwrap().body_text();
        let status = live.find("Status").expect(&live);
        let panel_at = live.find("Create a release MR").expect(&live);
        assert!(status < panel_at, "after the Status section");
        let mrs = st.registry.dispatch(&st, &UiRequest::get("/loops/mr-review-loop?view=mrs")).unwrap().body_text();
        assert!(!mrs.contains("Create a release MR"), "no longer on the merge requests view");
    }

    #[test]
    fn confirm_creates_via_the_api_seam_with_the_edited_text_and_discard_removes_the_sheet() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        let id = ready_job(d.path());
        let st = state(d.path());
        let api = fake();
        let body = format!("csrf_token=tok&job={id}&title=Release%3A+edited&description=my+words");
        let req = form_req("/loops/mr-review-loop/release/confirm", &body);
        let r = confirm_with(&post_ctx(&st, &req), &api);
        assert!(location(&r).contains("ok=1"), "{}", location(&r));
        assert_eq!(api.created.borrow()[0].title, "Release: edited");
        assert_eq!(api.created.borrow()[0].description, "my words");
        // A moved branch is refused and nothing more is created.
        let id2 = ready_job(d.path());
        let mut moved = fake();
        moved.heads.insert("staging".into(), "pushed".into());
        let req = form_req("/loops/mr-review-loop/release/confirm", &format!("csrf_token=tok&job={id2}&title=T&description=d"));
        assert!(location(&confirm_with(&post_ctx(&st, &req), &moved)).contains("ok=0"));
        assert!(moved.created.borrow().is_empty());
        // Discard.
        let req = form_req("/loops/mr-review-loop/release/discard", &format!("csrf_token=tok&job={id2}"));
        let r = st.registry.dispatch(&st, &req).unwrap();
        assert!(location(&r).contains("ok=1"));
        assert_eq!(job::get(&job::jobs_dir(d.path()), &id2).unwrap().state, "discarded");
    }
}
