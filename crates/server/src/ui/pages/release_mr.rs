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

const BACK: &str = "/loops/mr-review-loop?view=mrs";

pub fn register(r: &mut Registry) {
    r.get("/loops/mr-review-loop/release/branches", branches);
    r.get("/loops/mr-review-loop/release/state", state_signature);
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
    let default_target = api.default_branch(&id).unwrap_or_default();
    let configured = project.get("target_branch").and_then(Value::as_str).unwrap_or("staging");
    let source = [configured, "staging", "develop"].iter().find(|c| names.iter().any(|n| n == *c)).map(|s| s.to_string()).or_else(|| names.first().cloned()).unwrap_or_default();
    let target = if names.contains(&default_target) { default_target } else { names.first().cloned().unwrap_or_default() };
    UiResponse::html(format!(
        "{}{}",
        select(ctx, "source", "From (source branch)", &names, &source),
        select(ctx, "target", "Into (target branch)", &names, &target)
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

fn job_row(ctx: &UiCtx, j: &ReleaseJob) -> String {
    let detail = if j.state == "created" && j.mr_url.starts_with("http") {
        format!("<a href='{}' target='_blank' rel='noopener'>{}</a>", h(&j.mr_url), h(&ctx.t("Open merge request")))
    } else if j.state == "ready" {
        format!("<a href='{BACK}'>{}</a>", h(&ctx.t("Review")))
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

fn sheet(ctx: &UiCtx, j: &ReleaseJob) -> String {
    let csrf = ctx.csrf_input();
    format!(
        "<aside class='rel-sheet' role='dialog' aria-label='{label}'>\
<h3>{label}</h3><p class='rel-meta'><code>{src} \u{2192} {dst}</code></p>\
<form method='post' action='/loops/mr-review-loop/release/confirm' class='rel-sheet-form'>{csrf}<input type='hidden' name='job' value='{id}'>\
<label class='rel-field'><span>{title_l}</span><input type='text' name='title' class='rel-input' value='{title}' maxlength='255'></label>\
<label class='rel-field'><span>{desc_l}</span><textarea name='description' class='rel-text' rows='14'>{desc}</textarea></label>\
<div class='rel-actions'><button type='submit' class='btn btn-primary'>{create}</button></div></form>\
<form method='post' action='/loops/mr-review-loop/release/discard' class='rel-sheet-form'>{csrf}<input type='hidden' name='job' value='{id}'>\
<div class='rel-actions'><button type='submit' class='btn btn-neutral'>{discard}</button>\
<a class='btn btn-neutral' href='{BACK}&amp;sheet=0'>{close}</a></div></form></aside>",
        label = h(&ctx.t("Review the release MR")),
        src = h(&j.source), dst = h(&j.target), id = h(&j.id),
        title_l = h(&ctx.t("Title")), title = h(&j.title),
        desc_l = h(&ctx.t("Description")), desc = h(&j.description),
        create = h(&ctx.t("Create MR")), discard = h(&ctx.t("Discard")), close = h(&ctx.t("Close")),
    )
}

/// Every job and its state, as one string: it changes whenever a job settles, which is the
/// panel's cue to reload.
fn state_signature(ctx: &UiCtx) -> UiResponse {
    let sig: Vec<String> = job::list(&jobs_dir(ctx)).iter().map(|j| format!("{}:{}", j.id, j.state)).collect();
    UiResponse::html(sig.join("|"))
}

/// Polls while a job runs and reloads the page once any job changes state.
const POLL_SCRIPT: &str = "<script>(function(){var sig=null;function tick(){fetch('/loops/mr-review-loop/release/state',{credentials:'same-origin'}).then(function(r){return r.text()}).then(function(t){if(sig===null){sig=t}else if(t!==sig){location.reload();return}setTimeout(tick,4000)}).catch(function(){setTimeout(tick,8000)})}tick()})();</script>";

const SCRIPT: &str = "<script>(function(){var a=document.getElementById('rel-alias'),b=document.getElementById('rel-branches');if(!a||!b)return;var seq=0;function load(){var n=++seq;if(!a.value){b.innerHTML='';return}fetch('/loops/mr-review-loop/release/branches?alias='+encodeURIComponent(a.value),{credentials:'same-origin'}).then(function(r){return r.text()}).then(function(h){if(n===seq)b.innerHTML=h}).catch(function(){})}a.addEventListener('change',load);load()})();</script>";

/// The panel (and the preview sheet when a preview is waiting).
pub fn panel(ctx: &UiCtx) -> String {
    let projects = tracked(ctx);
    let jobs = job::list(&jobs_dir(ctx));
    let heading = format!("<div class='section-header'><h2>{}</h2></div><p class='section-subtitle'>{}</p>", h(&ctx.t("Create release MR")), h(&ctx.t("Pick a project and two branches. An agent reads what changed between them and writes the title and description; the merge request is created for you.")));
    if projects.is_empty() {
        return format!("<section class='card rel-card'>{heading}<p class='rel-empty'>{}</p></section>", h(&ctx.t("No tracked projects yet. Add one on the Issues page, in Projects.")));
    }
    let options: String = projects.iter().map(|(a, _)| format!("<option value='{0}'>{0}</option>", h(a))).collect();
    let ready = cli_ready(ctx);
    let disabled = if ready { "" } else { " disabled" };
    let reason = if ready { String::new() } else { format!("<p class='rel-empty' role='note'>{}</p>", h(&ctx.t("No AI CLI is available. Install and sign in to your AI CLI in Settings to create release merge requests."))) };
    let recent: String = jobs.iter().take(5).map(|j| job_row(ctx, j)).collect();
    let recent = if recent.is_empty() { String::new() } else { format!("<h3 class='rel-sub'>{}</h3><ul class='rel-jobs'>{recent}</ul>", h(&ctx.t("Recent release merge requests"))) };
    let sheet_html = if ctx.query().first("sheet") == Some("0") {
        String::new()
    } else {
        jobs.iter().find(|j| j.state == "ready").map(|j| sheet(ctx, j)).unwrap_or_default()
    };
    format!(
        "<section class='card rel-card'>{heading}\
<form method='post' action='/loops/mr-review-loop/release/start' class='rel-form'>{csrf}\
<label class='rel-field'><span>{project}</span><select name='alias' id='rel-alias' class='rel-input'>{options}</select></label>\
<div id='rel-branches' class='rel-branches'></div>\
<div class='rel-actions'><button type='submit' name='mode' value='create' class='btn btn-primary'{disabled}>{create}</button>\
<button type='submit' name='mode' value='preview' class='btn btn-neutral'{disabled}>{preview}</button></div></form>{reason}{recent}</section>{sheet_html}{SCRIPT}{poll}",
        csrf = ctx.csrf_input(),
        poll = if jobs.iter().any(|j| j.state == "running") { POLL_SCRIPT } else { "" },
        project = h(&ctx.t("Project")),
        create = h(&ctx.t("Create MR")),
        preview = h(&ctx.t("Preview first")),
    )
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
        assert!(html.contains("/loops/mr-review-loop/release/branches"), "the branch selects load lazily");
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

    #[test]
    fn the_branch_fragment_defaults_source_to_the_project_branch_and_target_to_the_default_branch() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        let st = state(d.path());
        let req = UiRequest::get("/loops/mr-review-loop/release/branches?alias=app");
        let r = branches_with(&UiCtx::new(&st, &req), &fake());
        let html = r.body_text();
        assert!(html.contains("<select name='source'") && html.contains("<select name='target'"), "{html}");
        assert!(html.contains("<option value='staging' selected>"), "{html}");
        assert!(html.contains("<option value='master' selected>"), "{html}");
        let bad = branches_with(&UiCtx::new(&st, &UiRequest::get("/loops/mr-review-loop/release/branches?alias=nope")), &fake());
        assert!(bad.body_text().contains("Unknown project"));
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
        let html = panel(&UiCtx::new(&st, &req));
        assert!(html.contains("class='rel-sheet'"), "{html}");
        assert!(html.contains("<input type='text' name='title' class='rel-input' value='Release: order &lt;export&gt;'"), "{html}");
        assert!(html.contains("<textarea name='description'") && html.contains("Order export"), "{html}");
        assert!(html.contains("action='/loops/mr-review-loop/release/confirm'") && html.contains("action='/loops/mr-review-loop/release/discard'"));
        assert!(html.contains(&format!("name='job' value='{id}'")));
    }

    #[test]
    fn a_running_job_makes_the_panel_poll_and_the_state_changes_when_it_settles() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        let st = state(d.path());
        let jd = job::jobs_dir(d.path());
        let quiet = panel(&UiCtx::new(&st, &UiRequest::get(BACK)));
        assert!(!quiet.contains("release/state"), "no polling without a running job: {quiet}");
        let j = job::start(&jd, "app", "g/app", "staging", "master", Mode::Preview, &|_: &std::path::Path| Ok(std::process::id() as i64)).unwrap();
        let busy = panel(&UiCtx::new(&st, &UiRequest::get(BACK)));
        assert!(busy.contains("/loops/mr-review-loop/release/state") && busy.contains("location.reload"), "{busy}");
        let sig = |st: &UiState| st.registry.dispatch(st, &UiRequest::get("/loops/mr-review-loop/release/state")).unwrap().body_text();
        let before = sig(&st);
        job::update(&jd, &j.id, |j| j.state = "ready".into()).unwrap();
        assert_ne!(sig(&st), before, "a settled job changes the signature so the page reloads");
        assert!(!panel(&UiCtx::new(&st, &UiRequest::get(BACK))).contains("release/state"), "polling stops once nothing runs");
    }

    #[test]
    fn the_sheet_can_be_closed_without_discarding_and_reopened_from_the_ready_row() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        ready_job(d.path());
        let st = state(d.path());
        let open = panel(&UiCtx::new(&st, &UiRequest::get(BACK)));
        assert!(open.contains("class='rel-sheet'") && open.contains("href='/loops/mr-review-loop?view=mrs&amp;sheet=0'"), "{open}");
        let closed = panel(&UiCtx::new(&st, &UiRequest::get("/loops/mr-review-loop?view=mrs&sheet=0")));
        assert!(!closed.contains("class='rel-sheet'"), "{closed}");
        assert!(closed.contains("href='/loops/mr-review-loop?view=mrs'") && closed.contains("Review"), "the ready row links back to the preview: {closed}");
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

    #[test]
    fn recent_jobs_show_their_state_and_the_created_mr_link_and_failures_show_the_reason() {
        let d = tempfile::tempdir().unwrap();
        seed_projects(d.path());
        let jd = job::jobs_dir(d.path());
        let spawn = |_: &std::path::Path| Ok(std::process::id() as i64);
        let a = job::start(&jd, "app", "g/app", "staging", "master", Mode::Create, &spawn).unwrap();
        job::update(&jd, &a.id, |j| {
            j.state = "created".into();
            j.mr_url = "https://g/app/-/merge_requests/10".into();
        })
        .unwrap();
        let b = job::start(&jd, "app", "g/app", "develop", "master", Mode::Create, &spawn).unwrap();
        job::update(&jd, &b.id, |j| {
            j.state = "failed".into();
            j.summary = "Nothing to <release>".into();
        })
        .unwrap();
        let st = state(d.path());
        let req = UiRequest::get(BACK);
        let html = panel(&UiCtx::new(&st, &req));
        assert!(html.contains("href='https://g/app/-/merge_requests/10'"), "{html}");
        assert!(html.contains("Nothing to &lt;release&gt;"), "{html}");
    }
}
