//! The MR Review loop's own parts of `/loops/mr-review-loop`: the Latest results
//! card (each MR with its title, author and age) and the Merge requests view (the
//! user's open MRs, live from GitLab, with per-MR Auto review and Review now),
//! plus the two POSTs behind that view.

use super::connectors as conn;
use super::loops_common::{aware, flash_html, relative_time_dt, run_now_action_html};
use crate::ui::icons::icon;
use crate::ui::pyfmt::escape as h;
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use chrono::Local;
use loopx_core::connectors_config::ConfigConnectors;
use loopx_core::config::Config;
use loopx_core::loop_plugins::mr_review::{self, AccountMrs, MrPrefs, MyMr, ReviewState};
use loopx_core::mr_fix::eligibility::{self, FixTarget, Ineligible};
use loopx_core::mr_fix::jobs::{self, FixJob};
use loopx_core::pydumps::py_str;
use loopx_core::seen_store::SeenStore;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;

pub const LOOP: &str = "mr-review-loop";
/// The last fetched list ([`crate::ui::live_cache`]), painted while the fresh one loads.
pub const MRS_CACHE: &str = "mr-review-mrs";
const BACK: &str = "/loops/mr-review-loop?view=mrs";

pub fn register(r: &mut Registry) {
    r.get("/loops/mr-review-loop/mrs/fragment", mrs_fragment);
    r.post("/loops/mr-review-loop/mrs/auto", set_auto);
    r.post("/loops/mr-review-loop/mrs/review", review_now);
    r.post("/loops/mr-review-loop/mrs/fix", fix_now);
}

fn state_root(ctx: &UiCtx) -> PathBuf {
    ctx.state.repo_root.join("outputs").join("loops")
}

fn is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).filter(|x| !x.is_null()).map(py_str).unwrap_or_default()
}

/// `!7` from an item key `mr:<account>:<project>!<iid>@<sha>`.
fn iid_from_key(key: &str) -> String {
    key.rsplit_once('!')
        .map(|(_, rest)| format!("!{}", rest.split('@').next().unwrap_or("")))
        .unwrap_or_default()
}

/// The line under an MR's title: its reference (linked), author and age.
fn meta_line(ctx: &UiCtx, reference: &str, url: &str, author: &str, created_at: &str, extra: &str) -> String {
    let mut parts = String::new();
    if !reference.is_empty() {
        parts.push_str(&if is_http(url) {
            format!(
                "<a class='result-mr' href='{}' target='_blank' rel='noopener'>{}{}{}</a>",
                h(url),
                icon("git_pull_request"),
                h(reference),
                icon("open_in_new")
            )
        } else {
            format!("<span class='result-time'>{}{}</span>", icon("git_pull_request"), h(reference))
        });
    }
    if !author.is_empty() {
        parts.push_str(&format!("<span class='result-time'>{}{}</span>", icon("group"), h(author)));
    }
    if let Some(at) = aware(created_at) {
        let ago = relative_time_dt(ctx.state.now(), at);
        parts.push_str(&format!(
            "<span class='result-time' title='{}'>{}{}</span>",
            h(&at.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()),
            icon("schedule"),
            h(&ctx.tf("Opened {when}", &[("when", &ago)]))
        ));
    }
    parts.push_str(extra);
    if parts.is_empty() {
        return String::new();
    }
    format!("<div class='result-meta'>{parts}</div>")
}

// ---- Live: Latest results -----------------------------------------------------------------

/// Every MR of the last run: title, `group/project!iid`, author, age, what the
/// review concluded, and a status pill.
pub fn latest_results_card(ctx: &UiCtx, last: Option<&Map<String, Value>>) -> String {
    let outcomes: Vec<&Value> = last
        .and_then(|l| l.get("outcomes"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(|o| o.is_object()).collect())
        .unwrap_or_default();
    let head = format!("<div class='section-header'><h2>{}</h2></div>", h(&ctx.t("Latest results")));
    if outcomes.is_empty() {
        return format!(
            "<section class='card'>{head}<p class='section-subtitle'>{}</p></section>",
            h(&ctx.t("No merge requests were reviewed in the last run."))
        );
    }
    let mut rows = String::new();
    for o in outcomes {
        let empty = Value::Object(Map::new());
        let meta = o.get("meta").filter(|m| m.is_object()).unwrap_or(&empty);
        let key = text(o, "item_key");
        let title = [text(meta, "title"), text(o, "label")]
            .into_iter()
            .find(|t| !t.trim().is_empty())
            .unwrap_or_else(|| key.clone());
        let url = text(o, "url");
        let title = if is_http(&url) {
            format!("<a href='{}' target='_blank' rel='noopener'>{}</a>", h(&url), h(&title))
        } else {
            h(&title)
        };
        let reference = Some(text(meta, "ref")).filter(|r| !r.is_empty()).unwrap_or_else(|| iid_from_key(&key));
        let status = text(o, "status");
        let (kind, label) = match status.as_str() {
            "done" => ("done", "Reviewed"),
            "failed" => ("failed", "Failed"),
            _ => ("skipped", "Skipped"),
        };
        let summary = text(o, "summary");
        let summary = if summary.is_empty() {
            String::new()
        } else {
            format!("<p class='result-summary'>{}</p>", h(&summary))
        };
        rows.push_str(&format!(
            "<li class='result-row result-{kind}'><div class='result-head'><span class='result-title'>{title}</span>\
<span class='pill history-pill history-{kind}'>{}</span></div>{}{summary}</li>",
            h(&ctx.t(label)),
            meta_line(ctx, &reference, &url, &text(meta, "author"), &text(meta, "created_at"), "")
        ));
    }
    format!("<section class='card'>{head}<ul class='result-list'>{rows}</ul></section>")
}

// ---- The Merge requests view ----------------------------------------------------------------

fn list_mrs(ctx: &UiCtx) -> Result<Vec<AccountMrs>, String> {
    let store = conn::store(ctx.state).map_err(|_| ctx.t("Could not open the Keychain"))?;
    let source = ConfigConnectors::real()
        .with_paths(conn::paths(ctx.state))
        .with_store(store)
        .with_http(conn::http(ctx.state));
    Ok(mr_review::list_my_mrs(&source))
}

fn state_pill(ctx: &UiCtx, state: ReviewState, failure: Option<&str>) -> String {
    let (kind, label) = match state {
        ReviewState::Reviewed => ("done", "Reviewed"),
        ReviewState::Requested => ("done", "Queued for review"),
        ReviewState::Pending => ("skipped", "Waiting for next run"),
        ReviewState::Skipped => ("skipped", "Skipped by you"),
        ReviewState::Failed => ("failed", "Review failed \u{b7} skipped until Review now"),
        ReviewState::Draft => ("skipped", "Draft \u{b7} not reviewed"),
        ReviewState::Own => ("skipped", "Your MR \u{b7} not reviewed"),
    };
    // Why it failed, on hover: the full error would crowd the row.
    let title = failure.map_or(String::new(), |f| format!(" title='{}'", h(f)));
    format!("<span class='pill history-pill history-{kind}'{title}>{}</span>", h(&ctx.t(label)))
}

fn hidden_mr(mr: &MyMr) -> String {
    format!("<input type='hidden' name='mr' value='{}'>", h(&mr.mr_ref()))
}

/// What the Fix now button needs, loaded once per render: the projects config, each
/// account's kind and every fix job.
struct FixEnv {
    config: Option<Config>,
    kinds: HashMap<String, String>,
    jobs: Vec<FixJob>,
}

impl FixEnv {
    fn load(ctx: &UiCtx) -> Self {
        let kinds = loopx_core::connectors_config::list_accounts(&conn::paths(ctx.state))
            .unwrap_or_default()
            .into_iter()
            .map(|a| (a.id, a.kind))
            .collect();
        FixEnv {
            config: Config::load(&ctx.state.home_dir.join("projects.json")).ok(),
            kinds,
            jobs: jobs::list(&jobs::jobs_dir(&ctx.state.home_dir)),
        }
    }

    /// Whether Fix now may touch `mr`, and where its checkout lives.
    fn check(&self, mr: &MyMr) -> Result<FixTarget, Ineligible> {
        let kind = self.kinds.get(&mr.account).map_or("", String::as_str);
        match &self.config {
            Some(config) => eligibility::resolve(mr, kind, config),
            None if kind != "gitlab" => Err(Ineligible::NotGitlab),
            None if mr.draft => Err(Ineligible::Draft),
            None => Err(Ineligible::ProjectNotConfigured),
        }
    }
}

/// The reason text, translated; literal arguments so the catalog test sees them.
fn ineligible_reason(ctx: &UiCtx, why: Ineligible) -> String {
    match why {
        Ineligible::Fork => ctx.t("This merge request comes from a fork, so its branch cannot be pushed to."),
        Ineligible::ProjectNotConfigured => ctx.t("This merge request's project is not configured in Projects."),
        Ineligible::NotGitlab => ctx.t("Fix now only works on GitLab merge requests."),
        Ineligible::Draft => ctx.t("This merge request is a draft."),
    }
}

/// `("5", "7", rest)` from a done summary shaped `Fixed 5 of 7[ ...]`, as the runner writes it.
fn fixed_counts(summary: &str) -> Option<(&str, &str, &str)> {
    let rest = summary.strip_prefix("Fixed ")?;
    let (a, rest) = rest.split_once(" of ")?;
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    let (b, tail) = rest.split_at(end);
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    (digits(a) && digits(b)).then_some((a, b, tail))
}

/// The Fix now button (or its disabled state) plus the last job's result pill.
fn fix_html(ctx: &UiCtx, mr: &MyMr, fix: &FixEnv) -> String {
    // A draft shows the button disabled, with the reason on hover.
    let mr_ref = mr.mr_ref();
    let label = h(&ctx.t("Fix now"));
    let button = if jobs::running(&fix.jobs, &mr_ref).is_some() {
        format!(
            "<button type='button' class='btn btn-tonal' disabled>{}{}</button>",
            icon("bolt"),
            h(&ctx.t("Fixing\u{2026}"))
        )
    } else if let Err(why) = fix.check(mr) {
        format!(
            "<button type='button' class='btn btn-tonal' disabled title='{}'>{}{label}</button>",
            h(&ineligible_reason(ctx, why)),
            icon("bolt")
        )
    } else {
        format!(
            "<form method='post' action='/loops/mr-review-loop/mrs/fix' class='daemon-action-form'>{}{}\
<button type='submit' class='btn btn-tonal'>{}{label}</button></form>",
            ctx.csrf_input(),
            hidden_mr(mr),
            icon("bolt")
        )
    };
    let result = jobs::latest_for(&fix.jobs, &mr_ref).and_then(|j| {
        let (kind, text, title) = match j.state.as_str() {
            "done" => match fixed_counts(&j.summary) {
                Some((a, b, rest)) => (
                    "done",
                    ctx.tf("Fixed {fixed} of {total}", &[("fixed", a), ("total", b)]),
                    rest.trim().to_string(),
                ),
                None => ("done", ctx.t("Fixed"), j.summary.clone()),
            },
            "nothing" => ("skipped", ctx.t("Nothing to fix"), j.summary.clone()),
            "failed" => ("failed", ctx.t("Fix failed"), j.summary.clone()),
            "stopped" => ("skipped", ctx.t("Fix stopped"), String::new()),
            _ => return None,
        };
        // A failure's reason is shown in full under the buttons, not only on hover:
        // the first line as the headline, any command output below it in a block.
        let why = if kind == "failed" && !title.trim().is_empty() {
            let (head, detail) = title.trim().split_once('\n').unwrap_or((title.trim(), ""));
            let detail = detail.trim();
            let pre = if detail.is_empty() { String::new() } else { format!("<pre>{}</pre>", h(detail)) };
            format!("<div class='mr-fix-error' role='alert'><p>{}</p>{pre}</div>", h(head))
        } else {
            String::new()
        };
        let title = if title.is_empty() { String::new() } else { format!(" title='{}'", h(&title)) };
        Some(format!("<span class='pill history-pill history-{kind}'{title}>{}</span>{why}", h(&text)))
    });
    format!("{button}{}", result.unwrap_or_default())
}

fn actions_html(ctx: &UiCtx, mr: &MyMr, state: ReviewState, fix: &FixEnv) -> String {
    let fix = fix_html(ctx, mr, fix);
    if !mr.reviewable() || mr.draft {
        return if fix.is_empty() { String::new() } else { format!("<div class='mr-actions'>{fix}</div>") };
    }
    let csrf = ctx.csrf_input();
    let auto = state != ReviewState::Skipped;
    let label = h(&ctx.t("Auto review"));
    let (cls, next) = if auto { ("is-on", "off") } else { ("is-off", "on") };
    let toggle = format!(
        "<form method='post' action='/loops/mr-review-loop/mrs/auto' class='daemon-action-form mr-auto'>{csrf}{}\
<input type='hidden' name='auto' value='{next}'>\
<button type='submit' class='switch {cls}' role='switch' aria-checked='{auto}' aria-label='{label}' title='{label}'>\
<span class='switch-thumb'></span></button><span class='mr-auto-label'>{label}</span></form>",
        hidden_mr(mr)
    );
    let review = format!(
        "<form method='post' action='/loops/mr-review-loop/mrs/review' class='daemon-action-form'>{csrf}{}\
<button type='submit' class='btn btn-tonal'>{}{}</button></form>",
        hidden_mr(mr),
        icon("bolt"),
        h(&ctx.t("Review now"))
    );
    format!("<div class='mr-actions'>{toggle}{review}{fix}</div>")
}

fn mr_row(ctx: &UiCtx, mr: &MyMr, prefs: &MrPrefs, seen: &dyn Fn(&str) -> bool, account: Option<&str>, fix: &FixEnv) -> String {
    let state = mr_review::review_state(mr, prefs, seen);
    let title = if is_http(&mr.web_url) {
        format!("<a href='{}' target='_blank' rel='noopener'>{}</a>", h(&mr.web_url), h(&mr.title))
    } else {
        h(&mr.title)
    };
    let mut roles: Vec<String> = Vec::new();
    if mr.reviewer {
        roles.push(ctx.t("Reviewer"));
    }
    if mr.assignee {
        roles.push(ctx.t("Assignee"));
    }
    let mut extra = String::new();
    if mr.reviewable() && !roles.is_empty() {
        extra.push_str(&format!("<span class='result-time'>{}</span>", h(&ctx.tf("You: {roles}", &[("roles", &roles.join(", "))]))));
    }
    if let Some(a) = account {
        extra.push_str(&format!("<span class='result-time'>{}</span>", h(a)));
    }
    let kind = match state {
        ReviewState::Reviewed | ReviewState::Requested => "done",
        ReviewState::Failed => "failed",
        _ => "skipped",
    };
    let failure = (state == ReviewState::Failed)
        .then(|| prefs.failed.get(&mr.mr_ref()).map(String::as_str))
        .flatten();
    format!(
        "<li class='result-row result-{kind}'><div class='result-head'><span class='result-title'>{title}</span>{}</div>{}{}</li>",
        state_pill(ctx, state, failure),
        meta_line(ctx, &mr.reference, &mr.web_url, &mr.author, &mr.created_at, &extra),
        actions_html(ctx, mr, state, fix)
    )
}

fn mr_card(heading: &str, subtitle: &str, run_now: &str, rows: &[String], empty: &str) -> String {
    let list = if rows.is_empty() {
        format!("<p class='section-subtitle'>{}</p>", h(empty))
    } else {
        format!("<ul class='result-list'>{}</ul>", rows.concat())
    };
    format!(
        "<section class='card'><div class='status-head'><div class='section-header'><h2>{} <span class='mr-count'>{}</span></h2></div>{run_now}</div>\
<p class='section-subtitle'>{}</p>{list}</section>",
        h(heading),
        rows.len(),
        h(subtitle)
    )
}

/// The view body, from an already fetched list (tests pass their own).
pub fn mrs_html(ctx: &UiCtx, listed: Result<Vec<AccountMrs>, String>) -> String {
    let accounts = match listed {
        Ok(a) => a,
        Err(m) => return format!("<section class='card'><p class='section-subtitle'>{}</p></section>", h(&m)),
    };
    if accounts.is_empty() {
        return format!(
            "<section class='card'><div class='section-header'><h2>{}</h2></div><p class='section-subtitle'>{} <a href='/connectors'>{}</a></p></section>",
            h(&ctx.t("Merge requests")),
            h(&ctx.t("Connect a GitLab or GitHub account to see your merge requests.")),
            h(&ctx.t("Open Connectors"))
        );
    }
    let root = state_root(ctx);
    let prefs = MrPrefs::load(&root);
    let store = SeenStore::open(LOOP, Some(&root), None).ok();
    let seen = |k: &str| store.as_ref().is_some_and(|s| s.has(k));
    let fix = FixEnv::load(ctx);
    let several = accounts.len() > 1;
    let mut errors = String::new();
    let (mut assigned, mut created) = (Vec::new(), Vec::new());
    for a in &accounts {
        if let Some(e) = &a.error {
            errors.push_str(&format!(
                "<div class='flash flash-error'>{}</div>",
                h(&ctx.tf("Could not load merge requests from {account} ({error})", &[("account", &a.account_label), ("error", e)]))
            ));
        }
        let label = several.then_some(a.account_label.as_str());
        for mr in &a.mrs {
            let row = mr_row(ctx, mr, &prefs, &seen, label, &fix);
            if mr.reviewable() {
                assigned.push(row);
            } else {
                created.push(row);
            }
        }
    }
    let run_now = run_now_action_html(ctx, "/loops/mr-review-loop/run-now", &ctx.csrf_input(), None);
    format!(
        "{errors}<div class='grid mr-view'>{}{}</div>",
        mr_card(
            &ctx.t("Assigned to you"),
            &ctx.t("Open merge requests where you are the assignee or a reviewer. These are the only ones the loop reviews; drafts are always skipped."),
            &run_now,
            &assigned,
            &ctx.t("Nothing is assigned to you right now."),
        ),
        mr_card(
            &ctx.t("Created by you"),
            &ctx.t("Your own open merge requests. They are listed for reference and never reviewed."),
            "",
            &created,
            &ctx.t("You have no open merge requests."),
        )
    )
}

/// The view paints at once: the last fetched list when there is one (under a
/// "refreshing" note), else a spinner. The shell's `data-lazy-load` handler then
/// swaps in the fresh list from [`mrs_fragment`], as GitLab → Tasks does. Review
/// states and Auto review always come from the current prefs, not the cache.
pub fn mrs_body(ctx: &UiCtx, flash: Option<&str>, ok: bool) -> String {
    let cached = crate::ui::live_cache::read(ctx.state, MRS_CACHE).and_then(|(data, saved)| {
        let accounts: Vec<AccountMrs> = serde_json::from_value(data).ok()?;
        Some((mrs_html(ctx, Ok(accounts)), saved))
    });
    format!(
        "{}{}<div data-lazy-load='/loops/mr-review-loop/mrs/fragment'>{}</div>",
        flash_html(flash, ok),
        super::release_mr::panel(ctx),
        crate::ui::live_cache::placeholder(ctx, cached, &ctx.t("Loading your merge requests"))
    )
}

/// What to cache after a fetch: the fresh list, except that an account whose
/// fetch failed keeps its last good MRs (the page itself still shows the error).
fn merge_cache(previous: &Value, fresh: &[AccountMrs]) -> Vec<AccountMrs> {
    let previous: Vec<AccountMrs> = serde_json::from_value(previous.clone()).unwrap_or_default();
    fresh
        .iter()
        .map(|a| match (&a.error, previous.iter().find(|p| p.account_id == a.account_id)) {
            (Some(_), Some(p)) => AccountMrs { error: None, ..p.clone() },
            _ => a.clone(),
        })
        .collect()
}

fn mrs_fragment(ctx: &UiCtx) -> UiResponse {
    let listed = list_mrs(ctx);
    if let Ok(fresh) = &listed {
        let previous = crate::ui::live_cache::read(ctx.state, MRS_CACHE).map_or(Value::Null, |(d, _)| d);
        if let Ok(data) = serde_json::to_value(merge_cache(&previous, fresh)) {
            crate::ui::live_cache::write(ctx.state, MRS_CACHE, data);
        }
    }
    UiResponse::html(mrs_html(ctx, listed))
}

// ---- POSTs ------------------------------------------------------------------------------------

/// A well-formed `account:project!iid` from the form, or `None`.
fn form_ref(ctx: &UiCtx) -> Option<String> {
    let r = ctx.form().first("mr")?.to_string();
    let (account, rest) = r.split_once(':')?;
    let (pid, iid) = rest.split_once('!')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let name = |s: &str| {
        !s.is_empty() && s != ".." && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    // GitLab's numeric project id, or a GitHub `owner/name`.
    let project_ok = digits(pid) || pid.split_once('/').is_some_and(|(o, n)| name(o) && name(n));
    (name(account) && project_ok && digits(iid)).then_some(r)
}

fn set_auto(ctx: &UiCtx) -> UiResponse {
    let Some(r) = form_ref(ctx) else {
        return UiResponse::flash_redirect(false, &ctx.t("Unknown merge request"), BACK);
    };
    let on = ctx.form().first("auto") == Some("on");
    let saved = MrPrefs::update(&state_root(ctx), |p| {
        if on {
            p.skip.remove(&r);
        } else {
            p.skip.insert(r.clone());
            p.requested.remove(&r);
        }
    });
    match saved {
        Ok(_) if on => UiResponse::flash_redirect(true, &ctx.t("Auto review turned on for this merge request"), BACK),
        Ok(_) => UiResponse::flash_redirect(true, &ctx.t("This merge request will be skipped"), BACK),
        Err(e) => UiResponse::flash_redirect(false, &ctx.tf("Could not save: {detail}", &[("detail", &e.to_string())]), BACK),
    }
}

fn review_now(ctx: &UiCtx) -> UiResponse {
    let Some(r) = form_ref(ctx) else {
        return UiResponse::flash_redirect(false, &ctx.t("Unknown merge request"), BACK);
    };
    if let Err(e) = MrPrefs::update(&state_root(ctx), |p| {
        p.requested.insert(r.clone());
    }) {
        return UiResponse::flash_redirect(false, &ctx.tf("Could not save: {detail}", &[("detail", &e.to_string())]), BACK);
    }
    let (ok, message) = super::runs::trigger_plugin_loop(ctx, LOOP);
    if ok {
        return UiResponse::flash_redirect(true, &ctx.t("Review started - the draft notes appear on the merge request when it finishes"), BACK);
    }
    UiResponse::flash_redirect(
        false,
        &ctx.tf("{message}. This merge request stays queued for the next run.", &[("message", &message)]),
        BACK,
    )
}

/// `POST /loops/mr-review-loop/mrs/fix`: the form's `mr` is only a key into the live
/// list. Eligibility is re-resolved here, never taken from what the page showed.
fn fix_now(ctx: &UiCtx) -> UiResponse {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "loopx".to_string());
    // Only fetch the live list for a well-formed ref.
    let listed = if form_ref(ctx).is_some() { list_mrs(ctx) } else { Ok(Vec::new()) };
    let r = form_ref(ctx).unwrap_or_default();
    fix_now_with(ctx, listed, &|log| jobs::spawn_runner(log, &exe, &r))
}

fn fix_now_with(ctx: &UiCtx, listed: Result<Vec<AccountMrs>, String>, spawn: &jobs::Spawn<'_>) -> UiResponse {
    let fail = |m: String| UiResponse::flash_redirect(false, &m, BACK);
    let Some(r) = ctx.form().first("mr").map(str::to_string).filter(|r| loopx_core::mr_fix::valid_mr_ref(r)) else {
        return fail(ctx.t("Unknown merge request"));
    };
    let accounts = match listed {
        Ok(a) => a,
        Err(m) => return fail(m),
    };
    let Some(mr) = accounts.iter().flat_map(|a| a.mrs.iter()).find(|m| m.mr_ref() == r) else {
        return fail(ctx.t("Unknown merge request"));
    };
    let fix = FixEnv::load(ctx);
    if jobs::running(&fix.jobs, &r).is_some() {
        return fail(ctx.t("A fix for this merge request is already running"));
    }
    if let Err(why) = fix.check(mr) {
        return fail(ineligible_reason(ctx, why));
    }
    if !ctx.state.spawn_processes {
        return fail(ctx.t("This server does not start agent jobs"));
    }
    match jobs::start(&jobs::jobs_dir(&ctx.state.home_dir), &r, &mr.title, spawn) {
        Ok(_) => UiResponse::flash_redirect(true, &ctx.t("Fix started - the agent posts its plan on the merge request first"), BACK),
        Err(e) => fail(ctx.tf("Could not start the job: {detail}", &[("detail", &e)])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::routes::UiRequest;
    use crate::ui::state::UiState;
    use axum::http::Method;
    use serde_json::json;

    fn state(dir: &std::path::Path) -> UiState {
        UiState::explicit(dir, dir, dir, &dir.join("update"), "tok", None)
    }

    fn post(st: &UiState, path: &str, body: &str) -> UiResponse {
        st.registry.dispatch(st, &UiRequest::new(Method::POST, path).with_body(body)).unwrap()
    }

    fn mr(iid: &str, f: impl FnOnce(&mut MyMr)) -> MyMr {
        let mut m = MyMr {
            account: "work".into(),
            project_id: "9".into(),
            iid: iid.into(),
            title: format!("MR <{iid}>"),
            web_url: format!("https://gl/g/p/-/merge_requests/{iid}"),
            reference: format!("g/p!{iid}"),
            author: "Ana Lee".into(),
            created_at: "2026-10-01T09:00:00Z".into(),
            sha: "h".into(),
            reviewer: true,
            source_project_id: "9".into(),
            ..MyMr::default()
        };
        f(&mut m);
        m
    }

    fn render(st: &UiState, listed: Result<Vec<AccountMrs>, String>) -> String {
        let req = UiRequest::get("/loops/mr-review-loop?view=mrs");
        let ctx = UiCtx::new(st, &req);
        mrs_html(&ctx, listed)
    }

    fn account(mrs: Vec<MyMr>) -> AccountMrs {
        AccountMrs { account_id: "work".into(), account_label: "Work".into(), mrs, error: None }
    }

    #[test]
    fn assigned_and_created_mrs_are_split_with_state_and_controls() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        MrPrefs::update(&d.path().join("outputs/loops"), |p| {
            p.skip.insert("work:9!2".into());
        })
        .unwrap();
        let html = render(
            &st,
            Ok(vec![account(vec![
                mr("1", |_| {}),
                mr("2", |_| {}),
                mr("3", |m| m.draft = true),
                mr("4", |m| {
                    m.authored = true;
                    m.reviewer = false;
                }),
            ])]),
        );
        let (assigned, created) = html.split_once("Created by you").unwrap();
        assert!(assigned.contains("MR &lt;1&gt;") && !assigned.contains("MR <1>"), "{assigned}");
        assert!(assigned.contains("Waiting for next run") && assigned.contains("Skipped by you"));
        assert!(assigned.contains("Draft \u{b7} not reviewed"));
        assert!(assigned.contains("g/p!1") && assigned.contains("Ana Lee") && assigned.contains("Opened "));
        assert!(assigned.contains("value='work:9!1'") && assigned.contains("Review now"));
        assert!(assigned.contains("class='switch is-off'"), "skipped MR shows Auto review off");
        assert!(!assigned.contains("value='work:9!3'"), "no controls on a draft");
        assert!(assigned.contains("/loops/mr-review-loop/run-now"));
        assert!(html.contains("<div class='grid mr-view'>"), "hover styles are scoped to this view");
        assert!(assigned.contains("class='btn btn-tonal'"), "Review now is accent-tinted like GitLab's run button");
        assert!(created.contains("MR &lt;4&gt;") && created.contains("Your MR \u{b7} not reviewed"));
        assert!(!created.contains("Review now"));
    }

    #[test]
    fn a_failed_mr_shows_why_and_offers_review_now() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        MrPrefs::update(&d.path().join("outputs/loops"), |p| {
            p.failed.insert("work:9!1".into(), "model <timed> out".into());
        })
        .unwrap();
        let html = render(&st, Ok(vec![account(vec![mr("1", |_| {})])]));
        assert!(html.contains("result-row result-failed"), "{html}");
        assert!(html.contains("history-failed'") && html.contains("Review failed \u{b7} skipped until Review now"));
        assert!(html.contains("title='model &lt;timed&gt; out'"));
        assert!(html.contains("value='work:9!1'") && html.contains("Review now"));
    }

    #[test]
    fn the_view_paints_a_spinner_and_loads_the_list_lazily() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let req = UiRequest::get("/loops/mr-review-loop?view=mrs");
        let body = mrs_body(&UiCtx::new(&st, &req), Some("Saved"), true);
        assert!(body.contains("data-lazy-load='/loops/mr-review-loop/mrs/fragment'"), "{body}");
        assert!(body.contains("class=\"md-spinner\"") && body.contains("Loading your merge requests"));
        assert!(body.contains("Saved"), "the flash shows at once");
        assert!(!body.contains("Assigned to you"), "no GitLab call while painting");
        let frag = st
            .registry
            .dispatch(&st, &UiRequest::get("/loops/mr-review-loop/mrs/fragment"))
            .unwrap();
        assert_eq!(frag.status, 200);
        assert!(!frag.body_text().contains("<html"), "a fragment, not a page");
    }

    #[test]
    fn a_cached_list_paints_at_once_and_the_spinner_only_shows_without_one() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let req = UiRequest::get("/loops/mr-review-loop?view=mrs");
        let ctx = UiCtx::new(&st, &req);
        assert!(mrs_body(&ctx, None, true).contains("Loading your merge requests"));
        crate::ui::live_cache::write(&st, MRS_CACHE, serde_json::to_value(vec![account(vec![mr("1", |_| {})])]).unwrap());
        // Auto review reflects the current choice, not the one when cached.
        MrPrefs::update(&d.path().join("outputs/loops"), |p| {
            p.skip.insert("work:9!1".into());
        })
        .unwrap();
        let body = mrs_body(&ctx, None, true);
        assert!(body.contains("data-lazy-load='/loops/mr-review-loop/mrs/fragment'"));
        assert!(body.contains("MR &lt;1&gt;") && body.contains("refreshing"), "{body}");
        assert!(!body.contains("Loading your merge requests"));
        assert!(body.contains("class='switch is-off'"));
    }

    #[test]
    fn the_fragment_saves_what_it_fetched() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        assert!(crate::ui::live_cache::read(&st, MRS_CACHE).is_none());
        st.registry.dispatch(&st, &UiRequest::get("/loops/mr-review-loop/mrs/fragment")).unwrap();
        assert_eq!(crate::ui::live_cache::read(&st, MRS_CACHE).unwrap().0, json!([]));
    }

    #[test]
    fn a_failing_account_keeps_its_last_good_list_in_the_cache() {
        let old = vec![account(vec![mr("1", |_| {})]), AccountMrs { account_id: "gone".into(), ..account(vec![]) }];
        let mut bad = account(vec![]);
        bad.error = Some("MailHTTPError".into());
        let merged = merge_cache(&serde_json::to_value(&old).unwrap(), &[bad]);
        assert_eq!(merged.len(), 1, "an account no longer configured is dropped");
        assert_eq!(merged[0].mrs.len(), 1);
        assert!(merged[0].error.is_none());
        let fresh = vec![account(vec![mr("2", |_| {})])];
        assert_eq!(merge_cache(&serde_json::to_value(&old).unwrap(), &fresh), fresh);
    }

    #[test]
    fn an_account_error_and_no_accounts_are_explained() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let mut bad = account(vec![]);
        bad.error = Some("MailHTTPError".into());
        let html = render(&st, Ok(vec![bad]));
        assert!(html.contains("Could not load merge requests from Work (MailHTTPError)"));
        assert!(render(&st, Ok(vec![])).contains("Connect a GitLab or GitHub account"));
    }

    #[test]
    fn auto_toggle_writes_the_skip_list() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let root = d.path().join("outputs/loops");
        let r = post(&st, "/loops/mr-review-loop/mrs/auto", "csrf_token=tok&mr=work%3A9%217&auto=off");
        assert_eq!(r.status, 303);
        assert!(r.header("Location").unwrap().starts_with("/loops/mr-review-loop?view=mrs&flash="));
        assert!(MrPrefs::load(&root).skip.contains("work:9!7"));
        post(&st, "/loops/mr-review-loop/mrs/auto", "csrf_token=tok&mr=work%3A9%217&auto=on");
        assert!(MrPrefs::load(&root).skip.is_empty());
    }

    #[test]
    fn review_now_queues_the_mr_even_when_the_run_cannot_start() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let r = post(&st, "/loops/mr-review-loop/mrs/review", "csrf_token=tok&mr=work%3A9%217");
        assert_eq!(r.status, 303);
        assert!(MrPrefs::load(&d.path().join("outputs/loops")).requested.contains("work:9!7"));
    }

    #[test]
    fn a_github_pull_request_can_be_queued_by_owner_and_repo() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        post(&st, "/loops/mr-review-loop/mrs/review", "csrf_token=tok&mr=gh%3Aocto-org%2Fweb.app%211");
        assert!(MrPrefs::load(&d.path().join("outputs/loops")).requested.contains("gh:octo-org/web.app!1"));
        for bad in ["gh%3A..%2Fx%211", "gh%3Ao%2Fr%2Fz%211", "gh%3Ao%2F%211", "gh%3Ao%20x%2Fr%211"] {
            let r = post(&st, "/loops/mr-review-loop/mrs/review", &format!("csrf_token=tok&mr={bad}"));
            assert!(r.header("Location").unwrap().contains("ok=0"), "{bad}");
        }
    }

    #[test]
    fn malformed_refs_are_refused_and_posts_need_csrf() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        for bad in ["../x%3A9%217", "work%3Aa%217", "work%3A9", ""] {
            let r = post(&st, "/loops/mr-review-loop/mrs/review", &format!("csrf_token=tok&mr={bad}"));
            assert!(r.header("Location").unwrap().contains("ok=0"), "{bad}");
        }
        assert!(MrPrefs::load(&d.path().join("outputs/loops")).requested.is_empty());
        for path in ["/loops/mr-review-loop/mrs/auto", "/loops/mr-review-loop/mrs/review"] {
            assert_eq!(st.registry.dispatch(&st, &UiRequest::new(Method::POST, path)).unwrap().status, 403);
        }
    }

    fn fix_setup(d: &std::path::Path) {
        std::fs::write(
            d.join("connectors.json"),
            r#"[{"id":"work","type":"gitlab"},{"id":"gh","type":"github"}]"#,
        )
        .unwrap();
        std::fs::write(
            d.join("projects.json"),
            r#"{"gitlab_instance":"g.example.com","worktree_root":"/wt","projects":{
                "web":{"local_path":"/code/web","project_id":"9"}}}"#,
        )
        .unwrap();
    }

    fn jobs_in(d: &std::path::Path) -> Vec<loopx_core::mr_fix::jobs::FixJob> {
        loopx_core::mr_fix::jobs::list(&loopx_core::mr_fix::jobs::jobs_dir(d))
    }

    fn fix_post(
        st: &UiState,
        body: &str,
        listed: Vec<AccountMrs>,
    ) -> UiResponse {
        let req = UiRequest::new(Method::POST, "/loops/mr-review-loop/mrs/fix").with_body(body);
        let ctx = UiCtx::new(st, &req);
        fix_now_with(&ctx, Ok(listed), &|_| Ok(i64::from(std::process::id())))
    }

    #[test]
    fn fix_now_button_shows_on_assigned_and_created_mrs() {
        let d = tempfile::tempdir().unwrap();
        fix_setup(d.path());
        let st = state(d.path());
        let html = render(
            &st,
            Ok(vec![account(vec![
                mr("1", |_| {}),
                mr("3", |m| m.draft = true),
                mr("4", |m| {
                    m.authored = true;
                    m.reviewer = false;
                }),
            ])]),
        );
        let (assigned, created) = html.split_once("Created by you").unwrap();
        assert!(assigned.contains("action='/loops/mr-review-loop/mrs/fix'"), "{assigned}");
        assert!(assigned.contains("Fix now") && assigned.contains("Review now"));
        assert!(created.contains("Fix now") && created.contains("value='work:9!4'"), "{created}");
        assert!(!created.contains("Review now"));
        // A draft gets the button too, disabled, with the reason on hover.
        assert_eq!(html.matches("Fix now").count(), 3, "{html}");
        assert!(!html.contains("value='work:9!3'"), "no live form on the draft");
        assert!(
            html.contains("disabled title='This merge request is a draft.'"),
            "{html}"
        );
    }

    #[test]
    fn fix_now_is_disabled_with_a_reason_for_a_fork_mr() {
        let d = tempfile::tempdir().unwrap();
        fix_setup(d.path());
        let st = state(d.path());
        let html = render(
            &st,
            Ok(vec![account(vec![
                mr("1", |m| m.source_project_id = "10".into()),
                mr("2", |m| {
                    m.project_id = "77".into();
                    m.source_project_id = "77".into();
                }),
                mr("3", |m| m.account = "gh".into()),
            ])]),
        );
        assert!(html.contains("cannot be pushed to"), "fork reason on hover: {html}");
        assert!(html.contains("is not configured in Projects"), "{html}");
        assert!(html.contains("only works on GitLab"), "{html}");
        assert!(!html.contains("action='/loops/mr-review-loop/mrs/fix'"), "no live form: {html}");
        assert!(html.contains("disabled"));
    }

    #[test]
    fn fix_now_post_starts_a_job_and_needs_csrf() {
        let d = tempfile::tempdir().unwrap();
        fix_setup(d.path());
        let st = state(d.path());
        let r = st
            .registry
            .dispatch(&st, &UiRequest::new(Method::POST, "/loops/mr-review-loop/mrs/fix"))
            .unwrap();
        assert_eq!(r.status, 403);
        let mut st = state(d.path());
        st.spawn_processes = false;
        let r = fix_post(&st, "csrf_token=tok&mr=work%3A9%211", vec![account(vec![mr("1", |_| {})])]);
        assert!(r.header("Location").unwrap().contains("ok=0"), "no spawn when the server forbids it");
        assert!(jobs_in(d.path()).is_empty());
        st.spawn_processes = true;
        let r = fix_post(&st, "csrf_token=tok&mr=work%3A9%211", vec![account(vec![mr("1", |_| {})])]);
        assert_eq!(r.status, 303);
        let loc = r.header("Location").unwrap();
        assert!(loc.starts_with("/loops/mr-review-loop?view=mrs&flash=") && loc.contains("ok=1"), "{loc}");
        let jobs = jobs_in(d.path());
        assert_eq!(jobs.len(), 1);
        assert_eq!((jobs[0].mr_ref.as_str(), jobs[0].state.as_str()), ("work:9!1", "running"));
        assert_eq!(jobs[0].title, "MR <1>");
    }

    #[test]
    fn fix_now_post_refuses_an_ineligible_or_running_mr_even_with_a_forged_form() {
        let d = tempfile::tempdir().unwrap();
        fix_setup(d.path());
        let mut st = state(d.path());
        st.spawn_processes = true;
        let listed = || {
            vec![account(vec![
                mr("2", |m| m.source_project_id = "10".into()),
                mr("3", |m| m.draft = true),
                mr("5", |m| {
                    m.project_id = "77".into();
                    m.source_project_id = "77".into();
                }),
                mr("6", |m| m.account = "gh".into()),
            ])]
        };
        for bad in [
            "work%3A9%212",   // fork
            "work%3A9%213",   // draft
            "work%3A77%215",  // project not configured
            "gh%3A9%216",     // not GitLab
            "work%3A9%2199",  // not in the live list
            "-x%3A1%212",     // malformed
            "",
        ] {
            let r = fix_post(&st, &format!("csrf_token=tok&mr={bad}"), listed());
            assert!(r.header("Location").unwrap().contains("ok=0"), "{bad}");
        }
        assert!(jobs_in(d.path()).is_empty());
        let ok = vec![account(vec![mr("1", |_| {})])];
        assert!(fix_post(&st, "csrf_token=tok&mr=work%3A9%211", ok.clone()).header("Location").unwrap().contains("ok=1"));
        let again = fix_post(&st, "csrf_token=tok&mr=work%3A9%211", ok);
        assert!(again.header("Location").unwrap().contains("ok=0"), "already running");
        assert_eq!(jobs_in(d.path()).len(), 1);
    }

    #[test]
    fn a_running_fix_shows_fixing_and_disables_the_button() {
        let d = tempfile::tempdir().unwrap();
        fix_setup(d.path());
        let st = state(d.path());
        let dir = loopx_core::mr_fix::jobs::jobs_dir(d.path());
        loopx_core::mr_fix::jobs::start(&dir, "work:9!1", "t", &|_| Ok(i64::from(std::process::id()))).unwrap();
        let html = render(&st, Ok(vec![account(vec![mr("1", |_| {})])]));
        assert!(html.contains("Fixing\u{2026}"), "{html}");
        assert!(!html.contains("action='/loops/mr-review-loop/mrs/fix'"), "{html}");
    }

    #[test]
    fn a_finished_fix_shows_its_result_pill() {
        let d = tempfile::tempdir().unwrap();
        fix_setup(d.path());
        let st = state(d.path());
        let dir = loopx_core::mr_fix::jobs::jobs_dir(d.path());
        let job = loopx_core::mr_fix::jobs::start(&dir, "work:9!1", "t", &|_| Ok(i64::from(std::process::id()))).unwrap();
        loopx_core::mr_fix::jobs::finish(&dir, &job.id, "done", "Fixed 5 of 7").unwrap();
        let html = render(&st, Ok(vec![account(vec![mr("1", |_| {})])]));
        assert!(html.contains("Fixed 5 of 7") && html.contains("action='/loops/mr-review-loop/mrs/fix'"), "{html}");
        let job = loopx_core::mr_fix::jobs::start(&dir, "work:9!1", "t", &|_| Ok(i64::from(std::process::id()))).unwrap();
        loopx_core::mr_fix::jobs::finish(&dir, &job.id, "failed", "build <broke>").unwrap();
        let html = render(&st, Ok(vec![account(vec![mr("1", |_| {})])]));
        assert!(html.contains("Fix failed") && html.contains("title='build &lt;broke&gt;'"), "{html}");
        assert!(html.contains("<div class='mr-fix-error' role='alert'><p>build &lt;broke&gt;</p></div>"), "{html}");
        let job = loopx_core::mr_fix::jobs::start(&dir, "work:9!1", "t", &|_| Ok(i64::from(std::process::id()))).unwrap();
        loopx_core::mr_fix::jobs::finish(&dir, &job.id, "failed", "checks failed: `yarn lint` failed (exit 127)\n$ yarn lint\nsh: next: command not found").unwrap();
        let html = render(&st, Ok(vec![account(vec![mr("1", |_| {})])]));
        assert!(html.contains("<p>checks failed: `yarn lint` failed (exit 127)</p><pre>$ yarn lint\nsh: next: command not found</pre>"), "{html}");
    }

    fn render_in(st: &UiState, lang: &str, listed: Vec<AccountMrs>) -> String {
        let req = UiRequest::get("/loops/mr-review-loop?view=mrs")
            .with_header("cookie", &format!("loop_lang={lang}"));
        mrs_html(&UiCtx::new(st, &req), Ok(listed))
    }

    #[test]
    fn a_done_fix_is_translated_with_its_counts_and_odd_summaries_are_escaped() {
        let d = tempfile::tempdir().unwrap();
        fix_setup(d.path());
        let st = state(d.path());
        let dir = loopx_core::mr_fix::jobs::jobs_dir(d.path());
        let job = loopx_core::mr_fix::jobs::start(&dir, "work:9!1", "t", &|_| Ok(i64::from(std::process::id()))).unwrap();
        loopx_core::mr_fix::jobs::finish(&dir, &job.id, "done", "Fixed 5 of 7").unwrap();
        let listed = || vec![account(vec![mr("1", |_| {})])];
        let en = render_in(&st, "en", listed());
        assert!(en.contains("Fixed 5 of 7"));
        for lang in ["fr", "ja", "zh"] {
            let html = render_in(&st, lang, listed());
            assert!(!html.contains("Fixed 5 of 7"), "{lang}: {html}");
            assert!(html.contains('5') && html.contains('7'));
        }
        assert!(render_in(&st, "fr", listed()).contains("5 corrig"));
        let job = loopx_core::mr_fix::jobs::start(&dir, "work:9!1", "t", &|_| Ok(i64::from(std::process::id()))).unwrap();
        loopx_core::mr_fix::jobs::finish(&dir, &job.id, "done", "Fixed 2 of 3 (finish note not posted)").unwrap();
        let html = render_in(&st, "en", listed());
        assert!(html.contains("Fixed 2 of 3") && html.contains("title='(finish note not posted)'"), "{html}");
        let job = loopx_core::mr_fix::jobs::start(&dir, "work:9!1", "t", &|_| Ok(i64::from(std::process::id()))).unwrap();
        loopx_core::mr_fix::jobs::finish(&dir, &job.id, "done", "weird <b>'x'").unwrap();
        let html = render_in(&st, "fr", listed());
        assert!(html.contains("title='weird &lt;b&gt;&#x27;x&#x27;'") || html.contains("title='weird &lt;b&gt;&#39;x&#39;'"), "{html}");
    }

    #[test]
    fn the_fix_controls_render_translated_and_attribute_safe_in_fr_and_ja() {
        let d = tempfile::tempdir().unwrap();
        fix_setup(d.path());
        let st = state(d.path());
        let listed = || vec![account(vec![mr("1", |_| {}), mr("2", |m| m.source_project_id = "10".into())])];
        let fr = render_in(&st, "fr", listed());
        assert!(fr.contains("Corriger maintenant") && !fr.contains(">Fix now<"), "{fr}");
        assert!(fr.contains("n\u{2019}est pas") || fr.contains("peut pas recevoir"), "{fr}");
        // A French apostrophe (U+2019) is fine; a raw ' would end the title attribute early.
        let title = fr.split("disabled title='").nth(1).unwrap().split('\'').next().unwrap();
        assert!(title.contains("fork") && !title.contains('<'), "{title}");
        let ja = render_in(&st, "ja", listed());
        assert!(ja.contains("今すぐ修正") && ja.contains("フォーク由来"), "{ja}");
        let dir = loopx_core::mr_fix::jobs::jobs_dir(d.path());
        loopx_core::mr_fix::jobs::start(&dir, "work:9!1", "t", &|_| Ok(i64::from(std::process::id()))).unwrap();
        assert!(render_in(&st, "fr", listed()).contains("Correction en cours\u{2026}"));
        assert!(render_in(&st, "ja", listed()).contains("修正中\u{2026}"));
    }

    #[test]
    fn latest_results_show_title_ref_author_age_and_status() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let req = UiRequest::get("/loops/mr-review-loop");
        let ctx = UiCtx::new(&st, &req);
        let last = json!({"outcomes": [
            {"item_key": "mr:work:9!7@h", "label": "", "status": "done", "url": "https://gl/7",
             "summary": "Changes requested: 2 findings (1 blocker), 1 risks",
             "meta": {"title": "Fix <login>", "ref": "web/app!7", "author": "Ana Lee", "created_at": "2026-10-01T09:00:00Z"}},
            {"item_key": "mr:work:9!8@h", "label": "", "status": "failed", "url": "", "summary": "x"}
        ]});
        let html = latest_results_card(&ctx, last.as_object());
        assert!(html.contains("Fix &lt;login&gt;") && html.contains("web/app!7") && html.contains("Ana Lee"));
        assert!(html.contains("Opened ") && html.contains(">Reviewed<") && html.contains(">Failed<"));
        assert!(html.contains("!8"), "an old outcome without meta still names its MR");
        assert!(latest_results_card(&ctx, None).contains("No merge requests were reviewed"));
    }
}
