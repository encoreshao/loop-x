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
use loopx_core::loop_plugins::mr_review::{self, AccountMrs, MrPrefs, MyMr, ReviewState};
use loopx_core::pydumps::py_str;
use loopx_core::seen_store::SeenStore;
use serde_json::{Map, Value};
use std::path::PathBuf;

pub const LOOP: &str = "mr-review-loop";
/// The last fetched list ([`crate::ui::live_cache`]), painted while the fresh one loads.
pub const MRS_CACHE: &str = "mr-review-mrs";
const BACK: &str = "/loops/mr-review-loop?view=mrs";

pub fn register(r: &mut Registry) {
    r.get("/loops/mr-review-loop/mrs/fragment", mrs_fragment);
    r.post("/loops/mr-review-loop/mrs/auto", set_auto);
    r.post("/loops/mr-review-loop/mrs/review", review_now);
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

fn state_pill(ctx: &UiCtx, state: ReviewState) -> String {
    let (kind, label) = match state {
        ReviewState::Reviewed => ("done", "Reviewed"),
        ReviewState::Requested => ("done", "Queued for review"),
        ReviewState::Pending => ("skipped", "Waiting for next run"),
        ReviewState::Skipped => ("skipped", "Skipped by you"),
        ReviewState::Draft => ("skipped", "Draft \u{b7} not reviewed"),
        ReviewState::Own => ("skipped", "Your MR \u{b7} not reviewed"),
    };
    format!("<span class='pill history-pill history-{kind}'>{}</span>", h(&ctx.t(label)))
}

fn hidden_mr(mr: &MyMr) -> String {
    format!("<input type='hidden' name='mr' value='{}'>", h(&mr.mr_ref()))
}

fn actions_html(ctx: &UiCtx, mr: &MyMr, state: ReviewState) -> String {
    if !mr.reviewable() || mr.draft {
        return String::new();
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
    format!("<div class='mr-actions'>{toggle}{review}</div>")
}

fn mr_row(ctx: &UiCtx, mr: &MyMr, prefs: &MrPrefs, seen: &dyn Fn(&str) -> bool, account: Option<&str>) -> String {
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
        _ => "skipped",
    };
    format!(
        "<li class='result-row result-{kind}'><div class='result-head'><span class='result-title'>{title}</span>{}</div>{}{}</li>",
        state_pill(ctx, state),
        meta_line(ctx, &mr.reference, &mr.web_url, &mr.author, &mr.created_at, &extra),
        actions_html(ctx, mr, state)
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
            h(&ctx.t("Connect a GitLab account to see your merge requests.")),
            h(&ctx.t("Open Connectors"))
        );
    }
    let root = state_root(ctx);
    let prefs = MrPrefs::load(&root);
    let store = SeenStore::open(LOOP, Some(&root), None).ok();
    let seen = |k: &str| store.as_ref().is_some_and(|s| s.has(k));
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
            let row = mr_row(ctx, mr, &prefs, &seen, label);
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
        "{}<div data-lazy-load='/loops/mr-review-loop/mrs/fragment'>{}</div>",
        flash_html(flash, ok),
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
    let id_ok = !account.is_empty()
        && account.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    (id_ok && digits(pid) && digits(iid)).then_some(r)
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
        assert!(render(&st, Ok(vec![])).contains("Connect a GitLab account"));
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
