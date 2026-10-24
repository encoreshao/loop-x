//! The GitLab loop's page `/loops/gitlab-loop` (views Live, Tasks, Projects), its
//! lazily loaded fragments `GET /gitlab/live` and `GET /settings/fragment`, the
//! issue tracking switch (`POST /gitlab/issues/<alias>/<iid>/enable|disable`) and
//! the GitLab configuration POSTs (`/settings/gitlab/*`, `/settings/access-bundles*`,
//! `/settings/loop-config`, `/settings/loop-projects*`).

use super::loops_common::{
    custom_select_plain, empty_state_html, esc_t, flash_html, mask_secret, relative_time,
    relative_time_dt, render_loop_page, run_now_action_html, stop_action_html,
};
use crate::ui::badge::status_badge_markup;
use crate::ui::hub::{builtin_loop_page, resolve_view};
use crate::ui::pyfmt::{self, Form};
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::template::{Scope, Template};
use crate::ui::{consts, gitlab as ui_gitlab};
use chrono::{DateTime, Local, Utc};
use loopx_core::loops_config::py_truthy;
use loopx_core::worktree::{self, CheckoutProblem, RootProblem};
use loopx_core::{events, gitlab, issue_tracking_config, pydumps, pyjson, status};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

const PROJECTS_URL: &str = "/loops/gitlab-loop?view=projects";

pub fn register(r: &mut Registry) {
    r.get("/loops/gitlab-loop", page);
    r.get("/gitlab/live", live_fragment);
    r.get("/settings/fragment", settings_fragment);
    r.post("/gitlab/issues/*rest", issue_tracking_toggle);
    r.post("/settings/gitlab/default", |c| {
        flash_post(c, |c, f| set_default_instance(c, f.first("instance").unwrap_or("")))
    });
    r.post("/settings/gitlab/instances", |c| {
        flash_post(c, |c, f| {
            upsert_instance(
                c,
                f.first("alias").unwrap_or(""),
                f.first("url").unwrap_or(""),
                f.first("token").unwrap_or(""),
            )
        })
    });
    r.post("/settings/gitlab/instances/:alias/delete", |c| {
        flash_post(c, |c, _| delete_instance(c, c.param("alias")))
    });
    r.post("/settings/gitlab/projects", |c| {
        flash_post(c, |c, f| {
            upsert_project(
                c,
                f.first("alias").unwrap_or(""),
                f.first("project_id").unwrap_or(""),
                f.first("instance").unwrap_or(""),
                f.first("bundle").unwrap_or(""),
            )
        })
    });
    r.post("/settings/gitlab/projects/:alias/delete", |c| {
        flash_post(c, |c, _| delete_project(c, c.param("alias")))
    });
    r.post("/settings/access-bundles", |c| {
        flash_post(c, |c, f| {
            upsert_bundle(
                c,
                f.first("name").unwrap_or(""),
                f.first("instance").unwrap_or(""),
                f.first("token").unwrap_or(""),
                f.first("webhook_url").unwrap_or(""),
            )
        })
    });
    r.post("/settings/access-bundles/:name/delete", |c| {
        flash_post(c, |c, _| delete_bundle(c, c.param("name")))
    });
    r.post("/settings/access-bundles/:name/clear-webhook", |c| {
        flash_post(c, |c, _| clear_bundle_webhook(c, c.param("name")))
    });
    r.post("/settings/loop-config", |c| {
        flash_post(c, |c, f| {
            update_loop_settings(
                c,
                f.first("assignee_username").unwrap_or(""),
                f.first("worktree_root").unwrap_or(""),
                f.first("gitlab_instance").unwrap_or(""),
            )
        })
    });
    r.post("/settings/loop-projects", |c| {
        flash_post(c, |c, f| {
            let g = |k: &str| f.first(k).unwrap_or("").to_string();
            upsert_tracked_project(
                c,
                &[
                    g("alias"),
                    g("project_id"),
                    g("local_path"),
                    g("target_branch"),
                    g("install_cmd"),
                    g("lint_cmd"),
                    g("test_cmd"),
                    g("instance"),
                    g("original_alias"),
                ],
            )
        })
    });
    r.post("/settings/loop-projects/:alias/delete", |c| {
        flash_post(c, |c, _| delete_tracked_project(c, c.param("alias")))
    });
}

// ---------------------------------------------------------------------------
// paths and config files

fn gitlab_config_path(ctx: &UiCtx) -> PathBuf {
    ctx.state.user_home.join(".gitlab").join("config.json")
}

fn slack_config_path(ctx: &UiCtx) -> PathBuf {
    ctx.state.user_home.join(".slack").join("config.json")
}

fn projects_path(ctx: &UiCtx) -> PathBuf {
    ctx.state.home_dir.join("projects.json")
}

/// `read_gitlab_config` & co: `{}` when the file is missing or malformed.
fn read_json_or_empty(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or_else(|| json!({}))
}

/// `_atomic_write_json`: temp file in the same directory, mode 0600, rename.
fn write_json_atomic(value: &Value, path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, pyjson::to_string_pretty(value)).map_err(|e| e.to_string())?;
    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

fn obj_mut<'a>(v: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
    if !v.get(key).is_some_and(Value::is_object) {
        v[key] = json!({});
    }
    v[key].as_object_mut().expect("object")
}

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .map(|x| if x.is_null() { String::new() } else { pydumps::py_str(x) })
        .unwrap_or_default()
}

fn keys_of(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_object)
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// page

fn page(ctx: &UiCtx) -> UiResponse {
    let (flash, flash_ok) = ctx.flash();
    let hub = builtin_loop_page("gitlab-loop").expect("gitlab hub");
    let q = ctx.query();
    let view = q.first("view");
    let body = match resolve_view(&hub, view).key.as_str() {
        "live" => live_body(ctx, flash.as_deref(), flash_ok),
        "tasks" => tasks_body(ctx),
        _ => projects_body(ctx, flash.as_deref(), flash_ok),
    };
    UiResponse::html(render_loop_page(ctx, &hub, "gitlab-loop", view, &body, None))
}

fn render(name: &str, scope: &Scope) -> String {
    Template::named(name)
        .render(scope)
        .unwrap_or_else(|e| panic!("{e}"))
}

fn tasks_body(ctx: &UiCtx) -> String {
    render("gitlab_tasks_body", &Scope::new(ctx.lang))
}

fn projects_body(ctx: &UiCtx, flash: Option<&str>, flash_ok: bool) -> String {
    let mut scope = Scope::new(ctx.lang);
    scope.var("flash_html", flash_html(flash, flash_ok));
    render("gitlab_projects_body", &scope)
}

// ---------------------------------------------------------------------------
// Live view: status card and the last run's results from the events ledger

const RECENT_EVENT_DAYS: i64 = 14;

struct IssueRow {
    project: Value,
    iid: Value,
    outcome: &'static str,
    detail: String,
    mr_url: String,
    timestamp: String,
    title: String,
    web_url: String,
}

struct Activity {
    updated_at: Option<String>,
    issues: Vec<IssueRow>,
}

fn str_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(x) if py_truthy(x) => pydumps::py_str(x),
        _ => String::new(),
    }
}

/// `gitlab_last_run_activity`: the newest run in the events ledger.
fn last_run_activity(ctx: &UiCtx) -> Option<Activity> {
    let since = (ctx.state.now() - chrono::Duration::days(RECENT_EVENT_DAYS))
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    let all = events::iter_events(Some(&ctx.state.events_dir), Some(&since), None);
    let mut run_id: Option<Value> = None;
    let mut keys: Vec<String> = Vec::new();
    let mut issues: Vec<IssueRow> = Vec::new();
    let mut updated: Option<String> = None;
    for event in &all {
        let etype = event.get("event_type").map(pydumps::py_str).unwrap_or_default();
        let iid = event.get("issue_iid").cloned().unwrap_or(Value::Null);
        if !etype.starts_with("issue.") || iid.is_null() {
            continue;
        }
        let this_run = event.get("run_id").cloned().unwrap_or(Value::Null);
        if run_id.as_ref() != Some(&this_run) {
            run_id = Some(this_run);
            keys.clear();
            issues.clear();
        }
        let project = event.get("project").cloned().unwrap_or(Value::Null);
        let key = pydumps::dumps(&json!([project, iid]));
        let idx = match keys.iter().position(|k| *k == key) {
            Some(i) => i,
            None => {
                keys.push(key);
                issues.push(IssueRow {
                    project,
                    iid,
                    outcome: "in_progress",
                    detail: String::new(),
                    mr_url: String::new(),
                    timestamp: String::new(),
                    title: String::new(),
                    web_url: String::new(),
                });
                issues.len() - 1
            }
        };
        let row = &mut issues[idx];
        let empty = Map::new();
        let data = event.get("data").and_then(Value::as_object).unwrap_or(&empty);
        match etype.as_str() {
            "issue.queued" => {
                row.title = str_or_empty(data.get("title"));
                row.web_url = str_or_empty(data.get("web_url"));
                continue;
            }
            "issue.completed" => {
                row.outcome = match data.get("action").and_then(Value::as_str) {
                    Some("fix") => "fix",
                    Some("answer") => "answered",
                    _ => "waiting",
                };
                row.mr_url = str_or_empty(data.get("mr_url"));
            }
            "issue.escalated" => {
                row.outcome = "escalated";
                row.detail = str_or_empty(data.get("reason"));
            }
            "issue.failed" | "issue.agent_failed" => {
                row.outcome = "failed";
                let reason = str_or_empty(data.get("reason"));
                row.detail = if reason.is_empty() {
                    str_or_empty(data.get("error"))
                } else {
                    reason
                };
            }
            _ => continue,
        }
        let ts = str_or_empty(event.get("timestamp"));
        row.timestamp = ts.clone();
        updated = Some(ts);
    }
    run_id?;
    Some(Activity { updated_at: updated, issues })
}

fn outcome_pill(outcome: &str) -> (&'static str, &'static str) {
    match outcome {
        "fix" => ("done", "MR opened"),
        "answered" => ("done", "Answered"),
        "waiting" => ("skipped", "Waiting"),
        "escalated" => ("failed", "Escalated"),
        "failed" => ("failed", "Failed"),
        _ => ("skipped", "In progress"),
    }
}

fn local_minute(dt: DateTime<chrono::FixedOffset>) -> String {
    dt.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()
}

fn status_card(ctx: &UiCtx, status: &Value, activity: &Option<Activity>, csrf: &str) -> String {
    let action = if status.get("state").and_then(Value::as_str) == Some("running") {
        stop_action_html(
            ctx,
            "/gitlab/stop",
            &ctx.t("Stop the running GitLab loop? The in-progress issue's work will be abandoned."),
            csrf,
        )
    } else if !read_json_or_empty(&projects_path(ctx))
        .get("projects")
        .is_some_and(py_truthy)
    {
        run_now_action_html(
            ctx,
            "/run-now",
            "",
            csrf,
            Some(&ctx.t(
                "No projects configured yet - <a href='/loops/gitlab-loop?view=projects'>add one on the GitLab page</a>.",
            )),
        )
    } else {
        run_now_action_html(
            ctx,
            "/run-now",
            &ctx.t("Run the GitLab loop now? This starts a real automated run outside its normal schedule."),
            csrf,
            None,
        )
    };
    let head = format!(
        "<div class='status-head'><div class='section-header'><h2>{}</h2>{}</div>{action}</div>",
        esc_t(ctx, "Status"),
        status_badge_markup(ctx.lang, status)
    );
    let Some(activity) = activity else {
        return format!(
            "<section class='card'>{head}<p class='section-subtitle'>{}</p></section>",
            esc_t(ctx, "No runs recorded yet.")
        );
    };
    let finished = activity.updated_at.as_deref().and_then(super::loops_common::aware);
    let when = finished.map(local_minute).unwrap_or_default();
    let ago = finished
        .map(|f| relative_time_dt(ctx.state.now(), f))
        .unwrap_or_default();
    let count = |kinds: &[&str]| {
        activity
            .issues
            .iter()
            .filter(|i| kinds.contains(&i.outcome))
            .count()
            .to_string()
    };
    let tiles = [
        ("", ctx.t("Last run"), when, ago),
        ("done", ctx.t("Done"), count(&["fix", "answered"]), String::new()),
        ("", ctx.t("Waiting"), count(&["waiting"]), String::new()),
        ("failed", ctx.t("Escalated"), count(&["escalated"]), String::new()),
        ("failed", ctx.t("Failed"), count(&["failed"]), String::new()),
    ];
    let mut body = String::from("<div class='stat-tiles'>");
    for (kind, label, value, sub) in &tiles {
        let kind_class = if kind.is_empty() {
            String::new()
        } else {
            format!(" stat-{kind}")
        };
        body.push_str(&format!(
            "<div class='stat-tile{kind_class}'><span class='stat-label'>{}</span><span class='stat-value'>{}</span>",
            pyfmt::escape(label),
            pyfmt::escape(value)
        ));
        if !sub.is_empty() {
            body.push_str(&format!("<span class='stat-sub'>{}</span>", pyfmt::escape(sub)));
        }
        body.push_str("</div>");
    }
    body.push_str("</div>");
    let link = format!(
        "<p class='status-link'><a href='/runs?view=history'>{}</a></p>",
        esc_t(ctx, "View run history")
    );
    format!("<section class='card'>{head}{body}{link}</section>")
}

fn is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn last_run_card(ctx: &UiCtx, activity: &Option<Activity>) -> String {
    let head = format!(
        "<div class='section-header'><h2>{}</h2></div>",
        esc_t(ctx, "Last run results")
    );
    let Some(activity) = activity.as_ref().filter(|a| !a.issues.is_empty()) else {
        return format!(
            "<section class='card'>{head}<p class='section-subtitle'>{}</p></section>",
            esc_t(ctx, "Nothing saved yet. Results appear here after the next run.")
        );
    };
    let prefixes = ui_gitlab::issue_url_prefixes(ctx.state);
    let mut rows = String::new();
    for issue in &activity.issues {
        let (kind, label) = outcome_pill(issue.outcome);
        let mut url = issue.web_url.clone();
        let project = issue.project.as_str().unwrap_or("");
        let iid = pydumps::py_str(&issue.iid);
        if url.is_empty() {
            if let Some(prefix) = issue.project.as_str().and_then(|p| prefixes.get(p)) {
                url = format!("{prefix}/-/issues/{iid}");
            }
        }
        let text = if issue.title.is_empty() {
            format!("#{iid} ({})", pydumps::py_str(&issue.project).replace("None", if project.is_empty() { "None" } else { project }))
        } else {
            format!("#{iid} {}", issue.title)
        };
        let mut title = pyfmt::escape(&text);
        if is_http(&url) {
            title = format!(
                "<a href='{}' target='_blank' rel='noopener'>{title}</a>",
                pyfmt::escape(&url)
            );
        }
        let mr = if is_http(&issue.mr_url) {
            format!(
                " <a class='stat-sub' href='{}' target='_blank' rel='noopener'>{}</a>",
                pyfmt::escape(&issue.mr_url),
                esc_t(ctx, "Merge request")
            )
        } else {
            String::new()
        };
        let detail = if issue.detail.is_empty() {
            String::new()
        } else {
            format!(
                "<p class='result-summary'>{}</p>",
                pyfmt::escape(&issue.detail.replace('_', " "))
            )
        };
        let ago = match super::loops_common::aware(&issue.timestamp) {
            Some(stamp) => format!(
                "<span class='stat-sub'>{}</span>",
                pyfmt::escape(&relative_time_dt(ctx.state.now(), stamp))
            ),
            None => String::new(),
        };
        rows.push_str(&format!(
            "<li class='result-row'><div class='result-head'><span class='result-title'>{title}</span>{mr}{ago}<span class='pill history-pill history-{kind}'>{}</span></div>{detail}</li>",
            esc_t(ctx, label)
        ));
    }
    format!("<section class='card'>{head}<ul class='result-list'>{rows}</ul></section>")
}

fn live_body(ctx: &UiCtx, flash: Option<&str>, flash_ok: bool) -> String {
    let csrf = ctx.csrf_input();
    let status = status::read_status(&ctx.state.status_path());
    let activity = last_run_activity(ctx);
    let has_projects = read_json_or_empty(&projects_path(ctx))
        .get("projects")
        .is_some_and(py_truthy);
    if !has_projects && activity.is_none() {
        return format!(
            "{}<section class='card'><div class='section-header'><h2>{}</h2>{}</div>{}</section>",
            flash_html(flash, flash_ok),
            esc_t(ctx, "Status"),
            status_badge_markup(ctx.lang, &status),
            empty_state_html(
                &pyfmt::escape_text(&ctx.t("No projects configured yet, so there's nothing to check for issues or MRs.")),
                PROJECTS_URL,
                &esc_t(ctx, "Set up a project"),
            )
        );
    }
    format!(
        "{}{}{}",
        flash_html(flash, flash_ok),
        status_card(ctx, &status, &activity, &csrf),
        last_run_card(ctx, &activity)
    )
}

// ---------------------------------------------------------------------------
// GET /gitlab/live

/// One project alias's `{issues, mrs, issues_error, mrs_error}`.
pub struct AliasState {
    pub issues: Vec<Value>,
    pub mrs: Vec<Value>,
    pub issues_error: Option<String>,
    pub mrs_error: Option<String>,
}

/// `_describe_gitlab_api_error`: the first meaningful line of the failure.
fn describe_error(message: &str) -> String {
    for line in message.trim().lines() {
        let line = line.trim();
        if !line.is_empty() && !line.contains("Warning:") && !line.starts_with("warnings.warn(") {
            return line.to_string();
        }
    }
    message.to_string()
}

fn api_error_text(e: gitlab::GitlabError) -> String {
    match e {
        gitlab::GitlabError::Http { status, url, body } => {
            format!("HTTP Error: {status} for url: {url}\nResponse: {body}")
        }
        other => other.to_string(),
    }
}

/// `_merge_gitlab_items`: de-duplicated by `id`.
fn merge_items(lists: Vec<Vec<Value>>) -> Vec<Value> {
    let mut seen: Vec<Value> = Vec::new();
    let mut out = Vec::new();
    for item in lists.into_iter().flatten() {
        let id = item.get("id").cloned().unwrap_or(Value::Null);
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        out.push(item);
    }
    out
}

fn fetch_alias(config: &Option<Value>, alias: &str, username: &str) -> AliasState {
    let (project_id, instance, bundle) = gitlab::resolve_project_alias(config.as_ref(), alias, None);
    let client = gitlab::Client::from_config(config.as_ref(), instance.as_deref(), bundle.as_deref());
    let client = match client {
        Ok(c) => c,
        Err(e) => {
            let msg = describe_error(&api_error_text(e));
            return AliasState {
                issues: vec![],
                mrs: vec![],
                issues_error: Some(msg.clone()),
                mrs_error: Some(msg),
            };
        }
    };
    let mut state = AliasState {
        issues: vec![],
        mrs: vec![],
        issues_error: None,
        mrs_error: None,
    };
    let issues = (|| -> Result<Vec<Value>, gitlab::GitlabError> {
        let mut assigned = client.list_issues_filtered(&project_id, "opened", &[], Some(username), None)?;
        let mut authored = client.list_issues_filtered(&project_id, "opened", &[], None, Some(username))?;
        for item in &mut assigned {
            item["_assigned_to_me"] = Value::Bool(true);
        }
        for item in &mut authored {
            item["_assigned_to_me"] = Value::Bool(false);
        }
        Ok(merge_items(vec![assigned, authored]))
    })();
    match issues {
        Ok(items) => state.issues = items,
        Err(e) => state.issues_error = Some(describe_error(&api_error_text(e))),
    }
    let mrs = (|| -> Result<Vec<Value>, gitlab::GitlabError> {
        let assigned = client.list_merge_requests(&project_id, "opened", Some(username), None)?;
        let authored = client.list_merge_requests(&project_id, "opened", None, Some(username))?;
        Ok(merge_items(vec![assigned, authored]))
    })();
    match mrs {
        Ok(items) => state.mrs = items,
        Err(e) => state.mrs_error = Some(describe_error(&api_error_text(e))),
    }
    state
}

/// `get_live_gitlab_state`: `[(alias, state)]` for every tracked project, each
/// fetched on its own thread.
pub fn live_state(ctx: &UiCtx) -> Vec<(String, AliasState)> {
    let Ok(cfg) = loopx_core::config::Config::load(&projects_path(ctx)) else {
        return Vec::new();
    };
    let (Ok(username), Ok(aliases)) = (cfg.assignee_username(), cfg.aliases()) else {
        return Vec::new();
    };
    if aliases.is_empty() {
        return Vec::new();
    }
    let gitlab_config: Option<Value> = std::fs::read_to_string(gitlab_config_path(ctx))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    std::thread::scope(|scope| {
        let handles: Vec<_> = aliases
            .iter()
            .map(|alias| {
                let (config, username) = (&gitlab_config, &username);
                (alias.clone(), scope.spawn(move || fetch_alias(config, alias, username)))
            })
            .collect();
        handles
            .into_iter()
            .map(|(alias, h)| {
                let state = h.join().unwrap_or(AliasState {
                    issues: vec![],
                    mrs: vec![],
                    issues_error: Some("fetch failed".into()),
                    mrs_error: Some("fetch failed".into()),
                });
                (alias, state)
            })
            .collect()
    })
}

fn tracking_toggle_html(ctx: &UiCtx, alias: &str, issue_iid: &Value) -> String {
    let safe_alias = pyfmt::quote(alias, "");
    let iid = pydumps::py_str(issue_iid);
    let path = ctx.state.home_dir.join("issue_tracking.json");
    let enabled = issue_tracking_config::is_issue_enabled(alias, &iid, &path).unwrap_or(true);
    let (action, class, checked, label) = if enabled {
        ("disable", "is-on", "true", ctx.tf("Stop tracking #{iid}", &[("iid", &iid)]))
    } else {
        ("enable", "is-off", "false", ctx.tf("Track #{iid} again", &[("iid", &iid)]))
    };
    let safe_label = pyfmt::escape(&label);
    format!(
        "<form method='post' action='/gitlab/issues/{safe_alias}/{iid}/{action}' data-issue-iid='{iid}' class='daemon-action-form issue-tracking-toggle'>{}<button type='submit' class='switch {class}' role='switch' aria-checked='{checked}' aria-label='{safe_label}' title='{safe_label}'><span class='switch-thumb'></span></button></form>",
        ctx.csrf_input()
    )
}

fn item_str(item: &Value, key: &str, default: &str) -> String {
    match item.get(key) {
        Some(Value::Null) | None => default.to_string(),
        Some(v) => pydumps::py_str(v),
    }
}

fn gitlab_item(ctx: &UiCtx, item: &Value, prefix: &str, alias: Option<&str>, compact: bool) -> String {
    let updated = relative_time(ctx.state.now(), &item_str(item, "updated_at", ""));
    let labels = item.get("labels").and_then(Value::as_array).cloned().unwrap_or_default();
    let pills: String = labels
        .iter()
        .map(|l| format!("<span class='pill pill-grey'>{}</span>", pyfmt::escape(&pydumps::py_str(l))))
        .collect();
    let label_row = if pills.is_empty() {
        String::new()
    } else {
        format!("<div class='pill-row'>{pills}</div>")
    };
    let iid = item.get("iid").filter(|v| !v.is_null());
    let toggle = match (alias, iid) {
        (Some(a), Some(i)) => tracking_toggle_html(ctx, a, i),
        _ => String::new(),
    };
    let link = format!(
        "<a class='gitlab-item-title' href='{}' target='_blank' rel='noopener'>{prefix}{} {}</a>",
        pyfmt::escape(&item_str(item, "web_url", "#")),
        pyfmt::escape(&item_str(item, "iid", "?")),
        pyfmt::escape(&item_str(item, "title", ""))
    );
    if compact {
        return format!(
            "<li class='gitlab-item'><div class='gitlab-item-row'>{link}{toggle}</div>{label_row}<div class='gitlab-item-meta gitlab-item-meta-standalone'>{}</div></li>",
            pyfmt::escape(&updated)
        );
    }
    let assignees = item.get("assignees").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut names: Vec<String> = Vec::new();
    for a in &assignees {
        let n = a.get("name").filter(|v| py_truthy(v));
        names.push(match n {
            Some(n) => pydumps::py_str(n),
            None => item_str(a, "username", ""),
        });
    }
    let assignee_names = if names.is_empty() {
        ctx.t("Unassigned")
    } else {
        names.join(", ")
    };
    let alias_pill = match alias {
        Some(a) => format!("<span class='pill pill-blue'>{}</span> ", pyfmt::escape(a)),
        None => String::new(),
    };
    format!(
        "<li class='gitlab-item'><div class='gitlab-item-row'>{link}<span class='gitlab-item-meta'>{alias_pill}{} &middot; {}</span>{toggle}</div>{label_row}</li>",
        pyfmt::escape(&assignee_names),
        pyfmt::escape(&updated)
    )
}

fn error_notice(ctx: &UiCtx, message: &Option<String>) -> String {
    match message {
        Some(m) if !m.is_empty() => format!(
            "<p class='inline-error'><span class='material-symbols-outlined' aria-hidden='true'>error</span> {}</p>",
            ctx.tf("Couldn't check: {message}", &[("message", &pyfmt::escape(m))])
        ),
        _ => String::new(),
    }
}

fn is_mine(item: &Value) -> bool {
    item.get("_assigned_to_me").is_some_and(py_truthy)
}

fn live_fragment(ctx: &UiCtx) -> UiResponse {
    UiResponse::html(live_fragment_html(ctx, &live_state(ctx)))
}

pub fn live_fragment_html(ctx: &UiCtx, live: &[(String, AliasState)]) -> String {
    if live.is_empty() {
        return empty_state_html(
            &pyfmt::escape_text(&ctx.t("No projects configured yet, so there's nothing to check for issues or MRs.")),
            PROJECTS_URL,
            &esc_t(ctx, "Set up a project"),
        );
    }
    let mut groups: Vec<(String, Vec<&Value>)> = Vec::new();
    for (alias, entry) in live {
        let mut assigned: Vec<&Value> = entry.issues.iter().filter(|i| is_mine(i)).collect();
        if !assigned.is_empty() {
            assigned.sort_by_key(|b| std::cmp::Reverse(item_str(b, "updated_at", "")));
            groups.push((alias.clone(), assigned));
        }
    }
    let total: usize = groups.iter().map(|(_, items)| items.len()).sum();
    let mut ordered: Vec<usize> = (0..groups.len()).collect();
    ordered.sort_by(|&a, &b| {
        item_str(groups[b].1[0], "updated_at", "").cmp(&item_str(groups[a].1[0], "updated_at", ""))
    });
    let (priority_body, priority_subtitle);
    if groups.is_empty() {
        priority_body = format!(
            "<p style='color: var(--md-on-surface-variant);'>{}</p>",
            esc_t(ctx, "Nothing assigned to you right now.")
        );
        priority_subtitle = String::new();
    } else if groups.len() == 1 {
        let (alias, items) = &groups[ordered[0]];
        let rows: String = items
            .iter()
            .map(|i| gitlab_item(ctx, i, "#", Some(alias), true))
            .collect();
        priority_body = format!("<ul class='plain gitlab-list'>{rows}</ul>");
        let n = total.to_string();
        priority_subtitle = format!(
            "<p>{}</p>",
            pyfmt::escape(&if total == 1 {
                ctx.tf("{count} issue", &[("count", &n)])
            } else {
                ctx.tf("{count} issues", &[("count", &n)])
            })
        );
    } else {
        let mut blocks = String::new();
        for &i in &ordered {
            let (alias, items) = &groups[i];
            let rows: String = items
                .iter()
                .map(|it| gitlab_item(ctx, it, "#", Some(alias), true))
                .collect();
            blocks.push_str(&format!(
                "<div class='attn-group'><h4 class='attn-group-title'>{} <span class='badge-count'>{}</span></h4><ul class='plain gitlab-list'>{rows}</ul></div>",
                pyfmt::escape(alias),
                items.len()
            ));
        }
        priority_body = blocks;
        priority_subtitle = format!(
            "<p>{}</p>",
            pyfmt::escape(&ctx.tf(
                "{count} issues across {projects} projects",
                &[("count", &total.to_string()), ("projects", &groups.len().to_string())]
            ))
        );
    }
    let mut out = format!(
        "<div class='project-block'><h3>{}</h3>{priority_subtitle}{priority_body}</div>",
        esc_t(ctx, "My Queue")
    );
    for (alias, entry) in live {
        let backlog: Vec<&Value> = entry.issues.iter().filter(|i| !is_mine(i)).collect();
        let none_item = format!("<li>{}</li>", esc_t(ctx, "(none)"));
        let issue_items: String = backlog
            .iter()
            .map(|i| gitlab_item(ctx, i, "#", None, false))
            .collect();
        let mr_items: String = entry
            .mrs
            .iter()
            .map(|m| gitlab_item(ctx, m, "!", None, false))
            .collect();
        out.push_str(&format!(
            "<div class='project-block'><h3>{}</h3><p>{} <span class='badge-count'>{}</span></p>{}<ul class='plain gitlab-list'>{}</ul><p>MRs <span class='badge-count'>{}</span></p>{}<ul class='plain gitlab-list'>{}</ul></div>",
            pyfmt::escape(alias),
            esc_t(ctx, "Backlog"),
            backlog.len(),
            error_notice(ctx, &entry.issues_error),
            if issue_items.is_empty() { &none_item } else { &issue_items },
            entry.mrs.len(),
            error_notice(ctx, &entry.mrs_error),
            if mr_items.is_empty() { &none_item } else { &mr_items },
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// POST /gitlab/issues/<alias>/<iid>/enable|disable

fn issue_tracking_toggle(ctx: &UiCtx) -> UiResponse {
    let rest = ctx.param("rest").to_string();
    let (enabled, middle) = if let Some(m) = rest.strip_suffix("/enable") {
        (true, m)
    } else if let Some(m) = rest.strip_suffix("/disable") {
        (false, m)
    } else {
        return UiResponse::not_found(ctx.lang);
    };
    let (alias, iid_part) = middle.rsplit_once('/').unwrap_or(("", middle));
    let Some(iid) = py_int(iid_part) else {
        return UiResponse::not_found(ctx.lang);
    };
    let path = ctx.state.home_dir.join("issue_tracking.json");
    let (ok, message) = match issue_tracking_config::set_issue_enabled(alias, iid, enabled, &path) {
        Ok(m) => (true, m),
        Err(e) => (false, e),
    };
    UiResponse::json(200, &json!({"ok": ok, "enabled": enabled, "message": message}))
}

/// Python's `int(text)`: optional sign and underscores between digits, surrounding spaces.
fn py_int(s: &str) -> Option<i64> {
    let t = s.trim();
    let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    t.replace('_', "").parse().ok()
}

// ---------------------------------------------------------------------------
// the GitLab configuration forms (GET /settings/fragment)

fn settings_fragment(ctx: &UiCtx) -> UiResponse {
    UiResponse::html(settings_fragment_html(ctx))
}

fn input_text(name: &str, value: &str, placeholder: &str, extra: &str) -> String {
    format!(
        "<input type='text' name='{name}' value='{}' placeholder='{placeholder}'{extra}>",
        pyfmt::escape(value)
    )
}

fn delete_form(ctx: &UiCtx, action: &str, confirm: &str, csrf: &str) -> String {
    format!(
        "<form method='post' action='{action}' class='daemon-action-form'>{csrf}<button type='submit' class='btn btn-warning' data-confirm=\"{}\"><span class='material-symbols-outlined' aria-hidden='true'>delete</span> {}</button></form>",
        pyfmt::escape(confirm),
        esc_t(ctx, "Delete")
    )
}

fn table(headers: &[String], rows: &[String]) -> String {
    let heads: String = headers.iter().map(|h| format!("<th>{h}</th>")).collect();
    format!(
        "<div class='table-wrap'><table class='daemons'><thead><tr>{heads}</tr></thead><tbody>{}</tbody></table></div>",
        rows.concat()
    )
}

fn settings_fragment_html(ctx: &UiCtx) -> String {
    let gitlab_config = read_json_or_empty(&gitlab_config_path(ctx));
    let slack_config = read_json_or_empty(&slack_config_path(ctx));
    let loop_config = read_json_or_empty(&projects_path(ctx));
    let csrf = ctx.csrf_input();
    let empty = json!({});
    let instances = gitlab_config.get("instances").unwrap_or(&empty);
    let projects = gitlab_config.get("projects").unwrap_or(&empty);
    let bundles = gitlab_config.get("bundles").unwrap_or(&empty);
    let bundle_webhooks = slack_config.get("bundle_webhooks").unwrap_or(&empty);
    let default_instance = text(&gitlab_config, "default");
    let instance_names = keys_of(&gitlab_config, "instances");
    let bundle_names = keys_of(&gitlab_config, "bundles");
    let t = |s: &str| ctx.t(s);
    let et = |s: &str| esc_t(ctx, s);

    let default_form = format!(
        "\n<form method='post' action='/settings/gitlab/default' class='daemon-action-form single-field'>\n{csrf}\n{}\n<button type='submit' class='btn btn-neutral'>{}</button>\n</form>\n",
        custom_select_plain("instance", &instance_names, Some(&default_instance), None),
        et("Set default")
    );

    let mut rows = Vec::new();
    for (name, inst) in instances.as_object().into_iter().flatten() {
        let safe = pyfmt::escape(name);
        let url_safe = pyfmt::quote(name, "");
        let badge = if *name == default_instance {
            format!(" <span class='pill pill-blue'>{}</span>", et("default"))
        } else {
            String::new()
        };
        let confirm = ctx.tf(
            "Delete GitLab instance {name}? Any project alias using it will need to be reassigned first.",
            &[("name", name)],
        );
        rows.push(format!(
            "<tr><td>{safe}{badge}</td><td>{}</td><td><form method='post' action='/settings/gitlab/instances' class='daemon-action-form'>{csrf}<input type='hidden' name='alias' value='{safe}'><input type='text' name='url' value='{}' placeholder='https://gitlab.example.com'><input type='password' name='token' placeholder='{}'><button type='submit' class='btn btn-neutral'>{}</button></form></td><td>{}</td></tr>",
            pyfmt::escape(&mask_secret(&text(inst, "token"))),
            pyfmt::escape(&text(inst, "url")),
            et("leave blank to keep current"),
            et("Save"),
            delete_form(ctx, &format!("/settings/gitlab/instances/{url_safe}/delete"), &confirm, &csrf)
        ));
    }
    let instances_html = if rows.is_empty() {
        format!("<p>{}</p>", et("(no GitLab instances configured)"))
    } else {
        table(&[et("Instance"), et("Token"), et("Edit"), et("Delete")], &rows)
    };
    let add_instance_form = format!(
        "\n<form method='post' action='/settings/gitlab/instances' class='daemon-action-form add-row-form'>\n{csrf}\n<input type='text' name='alias' placeholder='{}' required>\n<input type='text' name='url' placeholder='https://gitlab.example.com' required>\n<input type='password' name='token' placeholder='{}'>\n<button type='submit' class='btn btn-neutral'><span class='material-symbols-outlined' aria-hidden='true'>add</span> {}</button>\n</form>\n",
        et("instance name"),
        et("required"),
        et("Add instance")
    );

    let mut rows = Vec::new();
    for (alias, project) in projects.as_object().into_iter().flatten() {
        let safe = pyfmt::escape(alias);
        let url_safe = pyfmt::quote(alias, "");
        let confirm = ctx.tf("Delete project alias {alias}?", &[("alias", alias)]);
        rows.push(format!(
            "<tr><td>{safe}</td><td><form method='post' action='/settings/gitlab/projects' class='daemon-action-form'>{csrf}<input type='hidden' name='alias' value='{safe}'><input type='text' name='project_id' value='{}' placeholder='namespace/project'>{}{}<button type='submit' class='btn btn-neutral'>{}</button></form></td><td>{}</td></tr>",
            pyfmt::escape(&text(project, "project_id")),
            custom_select_plain("instance", &instance_names, Some(&text(project, "instance")), None),
            custom_select_plain("bundle", &bundle_names, Some(&text(project, "bundle")), Some(&t("(use instance default)"))),
            et("Save"),
            delete_form(ctx, &format!("/settings/gitlab/projects/{url_safe}/delete"), &confirm, &csrf)
        ));
    }
    let projects_html = if rows.is_empty() {
        format!("<p>{}</p>", et("(no project aliases configured)"))
    } else {
        table(&[et("Alias"), et("Edit"), et("Delete")], &rows)
    };
    let add_project_form = format!(
        "\n<form method='post' action='/settings/gitlab/projects' class='daemon-action-form add-row-form'>\n{csrf}\n<input type='text' name='alias' placeholder='{}' required>\n<input type='text' name='project_id' placeholder='namespace/project' required>\n{}\n{}\n<button type='submit' class='btn btn-neutral'><span class='material-symbols-outlined' aria-hidden='true'>add</span> {}</button>\n</form>\n",
        et("project alias"),
        custom_select_plain("instance", &instance_names, None, None),
        custom_select_plain("bundle", &bundle_names, None, Some(&t("(use instance default)"))),
        et("Add project")
    );

    let loop_settings_form = format!(
        "\n<form method='post' action='/settings/loop-config' class='daemon-action-form'>\n{csrf}\n<input type='text' name='assignee_username' value='{}' placeholder='{}'>\n<input type='text' name='worktree_root' value='{}' placeholder='/absolute/path/to/worktrees'>\n{}\n<button type='submit' class='btn btn-neutral'>{}</button>\n</form>\n",
        pyfmt::escape(&text(&loop_config, "assignee_username")),
        et("GitLab username"),
        pyfmt::escape(&text(&loop_config, "worktree_root")),
        custom_select_plain("gitlab_instance", &instance_names, Some(&text(&loop_config, "gitlab_instance")), None),
        et("Save")
    );

    let mut rows = Vec::new();
    for (alias, project) in loop_config.get("projects").and_then(Value::as_object).into_iter().flatten() {
        let safe = pyfmt::escape(alias);
        let url_safe = pyfmt::quote(alias, "");
        let confirm = ctx.tf("Stop tracking project {alias}?", &[("alias", alias)]);
        rows.push(format!(
            "<tr><td><form method='post' action='/settings/loop-projects' class='daemon-action-form'>{csrf}<input type='hidden' name='original_alias' value='{safe}'><input type='text' name='alias' value='{safe}' placeholder='{}' required>{}{}{}{}{}{}{}<button type='submit' class='btn btn-neutral'>{}</button></form></td><td>{}</td></tr>",
            et("project alias"),
            input_text("project_id", &text(project, "project_id"), "namespace/project", ""),
            input_text("local_path", &text(project, "local_path"), "/abs/path/to/checkout", ""),
            input_text("target_branch", &text(project, "target_branch"), &et("target branch"), ""),
            input_text("install_cmd", &text(project, "install_cmd"), &et("install command"), ""),
            input_text("lint_cmd", &text(project, "lint_cmd"), &et("lint command"), ""),
            input_text("test_cmd", &text(project, "test_cmd"), &et("test command"), ""),
            custom_select_plain("instance", &instance_names, Some(&text(project, "instance")), Some(&t("(use default)"))),
            et("Save"),
            delete_form(ctx, &format!("/settings/loop-projects/{url_safe}/delete"), &confirm, &csrf)
        ));
    }
    let tracked_projects_html = if rows.is_empty() {
        format!("<p>{}</p>", et("(no tracked projects configured)"))
    } else {
        table(&[et("Project"), et("Delete")], &rows)
    };
    let add_tracked_project_form = format!(
        "\n<form method='post' action='/settings/loop-projects' class='daemon-action-form add-row-form'>\n{csrf}\n<input type='text' name='alias' placeholder='{}' required>\n<input type='text' name='project_id' placeholder='namespace/project' required>\n<input type='text' name='local_path' placeholder='/abs/path/to/checkout'>\n<input type='text' name='target_branch' placeholder='{}'>\n<input type='text' name='install_cmd' placeholder='{}'>\n<input type='text' name='lint_cmd' placeholder='{}'>\n<input type='text' name='test_cmd' placeholder='{}'>\n{}\n<button type='submit' class='btn btn-neutral'><span class='material-symbols-outlined' aria-hidden='true'>add</span> {}</button>\n</form>\n",
        et("project alias"),
        et("target branch"),
        et("install command"),
        et("lint command"),
        et("test command"),
        custom_select_plain("instance", &instance_names, None, Some(&t("(use default)"))),
        et("Add project")
    );

    let mut rows = Vec::new();
    for (name, bundle) in bundles.as_object().into_iter().flatten() {
        let safe = pyfmt::escape(name);
        let url_safe = pyfmt::quote(name, "");
        let webhook = bundle_webhooks.get(name.as_str()).map(pydumps::py_str).unwrap_or_default();
        let webhook_cell = if webhook.is_empty() {
            et("(not set)")
        } else {
            pyfmt::escape(&mask_secret(&webhook))
        };
        let clear_form = if webhook.is_empty() {
            String::new()
        } else {
            let confirm = pyfmt::escape(&ctx.tf("Clear the Slack webhook override for bundle {name}?", &[("name", name)]));
            format!(
                " <form method='post' action='/settings/access-bundles/{url_safe}/clear-webhook' class='daemon-action-form' style='display:inline'>{csrf}<button type='submit' class='btn btn-neutral' data-confirm=\"{confirm}\">{}</button></form>",
                et("Clear")
            )
        };
        let confirm = ctx.tf(
            "Delete access bundle {name}? Any project alias using it will need to be reassigned first.",
            &[("name", name)],
        );
        rows.push(format!(
            "<tr><td>{safe}</td><td>{}</td><td>{}</td><td>{webhook_cell}{clear_form}</td><td><form method='post' action='/settings/access-bundles' class='daemon-action-form'>{csrf}<input type='hidden' name='name' value='{safe}'>{}<input type='password' name='token' placeholder='{}'><input type='password' name='webhook_url' placeholder='{}'><button type='submit' class='btn btn-neutral'>{}</button></form></td><td>{}</td></tr>",
            pyfmt::escape(&text(bundle, "instance")),
            pyfmt::escape(&mask_secret(&text(bundle, "token"))),
            custom_select_plain("instance", &instance_names, Some(&text(bundle, "instance")), None),
            et("leave blank to keep current token"),
            et("leave blank to keep current webhook"),
            et("Save"),
            delete_form(ctx, &format!("/settings/access-bundles/{url_safe}/delete"), &confirm, &csrf)
        ));
    }
    let bundles_html = if rows.is_empty() {
        format!("<p>{}</p>", et("(no access bundles configured)"))
    } else {
        table(
            &[et("Bundle"), et("Instance"), et("Token"), et("Slack webhook"), et("Edit"), et("Delete")],
            &rows,
        )
    };
    let add_bundle_form = format!(
        "\n<form method='post' action='/settings/access-bundles' class='daemon-action-form add-row-form'>\n{csrf}\n<input type='text' name='name' placeholder='{}' required>\n{}\n<input type='password' name='token' placeholder='{}'>\n<input type='password' name='webhook_url' placeholder='{}'>\n<button type='submit' class='btn btn-neutral'><span class='material-symbols-outlined' aria-hidden='true'>add</span> {}</button>\n</form>\n",
        et("bundle name"),
        custom_select_plain("instance", &instance_names, None, None),
        et("GitLab access token"),
        et("Slack webhook URL (optional)"),
        et("Add bundle")
    );

    let mut scope = Scope::new(ctx.lang);
    scope
        .var("default_form", default_form)
        .var("instances_html", instances_html)
        .var("add_instance_form", add_instance_form)
        .var("projects_html", projects_html)
        .var("add_project_form", add_project_form)
        .var(
            "bundles_subtitle",
            pyfmt::escape_text(&t("A project-specific GitLab token (and optional Slack webhook) for projects whose default instance token doesn't have full access.")),
        )
        .var("bundles_html", bundles_html)
        .var("add_bundle_form", add_bundle_form)
        .var(
            "tracked_subtitle",
            pyfmt::escape_text(&t("This loop's own ~/.loopx/projects.json - where each project lives locally, its target branch and install/lint/test commands, and (if it differs from the default instance above) which GitLab instance it's on. Different from \"Project aliases\" above, which is only about GitLab API auth routing.")),
        )
        .var("loop_settings_form", loop_settings_form)
        .var("tracked_projects_html", tracked_projects_html)
        .var("add_tracked_project_form", add_tracked_project_form);
    let _ = consts::ds("_SECTION_ICON_SETTINGS");
    render("gitlab_settings_fragment", &scope)
}

// ---------------------------------------------------------------------------
// configuration mutations (`set_default_gitlab_instance` & co); every message is translated

fn flash_post(ctx: &UiCtx, f: impl Fn(&UiCtx, &Form) -> (bool, String)) -> UiResponse {
    let (ok, message) = f(ctx, &ctx.form());
    UiResponse::flash_redirect(ok, &message, PROJECTS_URL)
}

fn name_msg(ctx: &UiCtx, template: &str, name: &str) -> String {
    ctx.tf(template, &[("name", name)])
}

fn save(ctx: &UiCtx, value: &Value, path: &Path) -> Result<(), (bool, String)> {
    write_json_atomic(value, path)
        .map_err(|e| (false, format!("{}: {e}", ctx.t("Could not save"))))
}

macro_rules! tryw {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(r) => return r,
        }
    };
}

fn has_key(v: &Value, section: &str, key: &str) -> bool {
    v.get(section).and_then(Value::as_object).is_some_and(|o| o.contains_key(key))
}

fn set_default_instance(ctx: &UiCtx, instance: &str) -> (bool, String) {
    let path = gitlab_config_path(ctx);
    let mut config = read_json_or_empty(&path);
    if !has_key(&config, "instances", instance) {
        return (false, name_msg(ctx, "Unknown instance: {name}", instance));
    }
    config["default"] = json!(instance);
    tryw!(save(ctx, &config, &path));
    (true, name_msg(ctx, "Default instance set to {name}", instance))
}

fn upsert_instance(ctx: &UiCtx, alias: &str, url: &str, token: &str) -> (bool, String) {
    let (alias, url, token) = (alias.trim(), url.trim(), token.trim());
    if alias.is_empty() {
        return (false, ctx.t("Instance name is required"));
    }
    if url.is_empty() {
        return (false, ctx.t("URL is required"));
    }
    let path = gitlab_config_path(ctx);
    let mut config = read_json_or_empty(&path);
    let instances = obj_mut(&mut config, "instances");
    let is_new = !instances.contains_key(alias);
    if is_new && token.is_empty() {
        return (false, ctx.t("Token is required for a new instance"));
    }
    let mut entry = instances.get(alias).cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
    entry["url"] = json!(url);
    let old_token = entry.get("token").cloned().unwrap_or(json!(""));
    entry["token"] = if token.is_empty() { old_token } else { json!(token) };
    instances.insert(alias.to_string(), entry);
    tryw!(save(ctx, &config, &path));
    (
        true,
        name_msg(ctx, if is_new { "Added instance {name}" } else { "Updated instance {name}" }, alias),
    )
}

fn delete_instance(ctx: &UiCtx, alias: &str) -> (bool, String) {
    let path = gitlab_config_path(ctx);
    let mut config = read_json_or_empty(&path);
    if !has_key(&config, "instances", alias) {
        return (false, name_msg(ctx, "Unknown instance: {name}", alias));
    }
    if config.get("default").and_then(Value::as_str) == Some(alias) {
        return (false, name_msg(ctx, "Cannot delete {name}: it is the default instance", alias));
    }
    let referencing = |section: &str| -> Vec<String> {
        config
            .get(section)
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter(|(_, v)| v.get("instance").and_then(Value::as_str) == Some(alias))
            .map(|(k, _)| k.clone())
            .collect()
    };
    let (projects, bundles) = (referencing("projects").join(", "), referencing("bundles").join(", "));
    if !projects.is_empty() || !bundles.is_empty() {
        let msg = if !projects.is_empty() && !bundles.is_empty() {
            ctx.tf(
                "Cannot delete {name}: still used by project(s) {projects} and bundle(s) {bundles}",
                &[("name", alias), ("projects", &projects), ("bundles", &bundles)],
            )
        } else if !projects.is_empty() {
            ctx.tf("Cannot delete {name}: still used by project(s) {projects}", &[("name", alias), ("projects", &projects)])
        } else {
            ctx.tf("Cannot delete {name}: still used by bundle(s) {bundles}", &[("name", alias), ("bundles", &bundles)])
        };
        return (false, msg);
    }
    obj_mut(&mut config, "instances").shift_remove(alias);
    tryw!(save(ctx, &config, &path));
    (true, name_msg(ctx, "Deleted instance {name}", alias))
}

fn upsert_project(ctx: &UiCtx, alias: &str, project_id: &str, instance: &str, bundle: &str) -> (bool, String) {
    let (alias, project_id, instance, bundle) = (alias.trim(), project_id.trim(), instance.trim(), bundle.trim());
    if alias.is_empty() {
        return (false, ctx.t("Project alias is required"));
    }
    if project_id.is_empty() {
        return (false, ctx.t("Project ID is required"));
    }
    let path = gitlab_config_path(ctx);
    let mut config = read_json_or_empty(&path);
    if !has_key(&config, "instances", instance) {
        return (false, name_msg(ctx, "Unknown instance: {name}", instance));
    }
    if !bundle.is_empty() {
        let Some(entry) = config.get("bundles").and_then(|b| b.get(bundle)) else {
            return (false, name_msg(ctx, "Unknown bundle: {name}", bundle));
        };
        if entry.get("instance").and_then(Value::as_str) != Some(instance) {
            let bi = entry.get("instance").map(pydumps::py_str).unwrap_or_else(|| "None".into());
            return (
                false,
                ctx.tf(
                    "Bundle {name} is for instance {bundle_instance}, not {instance}",
                    &[("name", bundle), ("bundle_instance", &bi), ("instance", instance)],
                ),
            );
        }
    }
    let projects = obj_mut(&mut config, "projects");
    let is_new = !projects.contains_key(alias);
    let mut entry = projects.get(alias).cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
    entry["project_id"] = json!(project_id);
    entry["instance"] = json!(instance);
    if bundle.is_empty() {
        entry.as_object_mut().expect("object").shift_remove("bundle");
    } else {
        entry["bundle"] = json!(bundle);
    }
    projects.insert(alias.to_string(), entry);
    tryw!(save(ctx, &config, &path));
    (
        true,
        name_msg(ctx, if is_new { "Added project {name}" } else { "Updated project {name}" }, alias),
    )
}

fn delete_project(ctx: &UiCtx, alias: &str) -> (bool, String) {
    let path = gitlab_config_path(ctx);
    let mut config = read_json_or_empty(&path);
    if !has_key(&config, "projects", alias) {
        return (false, name_msg(ctx, "Unknown project: {name}", alias));
    }
    obj_mut(&mut config, "projects").shift_remove(alias);
    tryw!(save(ctx, &config, &path));
    (true, name_msg(ctx, "Deleted project {name}", alias))
}

fn bundle_users(config: &Value, name: &str) -> Vec<String> {
    config
        .get("projects")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, p)| p.get("bundle").and_then(Value::as_str) == Some(name))
        .map(|(k, _)| k.clone())
        .collect()
}

fn upsert_bundle(ctx: &UiCtx, name: &str, instance: &str, token: &str, webhook_url: &str) -> (bool, String) {
    let (name, instance, token, webhook_url) = (name.trim(), instance.trim(), token.trim(), webhook_url.trim());
    if name.is_empty() {
        return (false, ctx.t("Bundle name is required"));
    }
    let gpath = gitlab_config_path(ctx);
    let mut config = read_json_or_empty(&gpath);
    if !has_key(&config, "instances", instance) {
        return (false, name_msg(ctx, "Unknown instance: {name}", instance));
    }
    let is_new = !has_key(&config, "bundles", name);
    if is_new && token.is_empty() {
        return (false, ctx.t("Token is required for a new bundle"));
    }
    if !is_new
        && config["bundles"][name].get("instance").and_then(Value::as_str) != Some(instance)
    {
        let users = bundle_users(&config, name);
        if !users.is_empty() {
            return (
                false,
                ctx.tf(
                    "Cannot change instance for bundle {name}: still used by project(s) {projects}",
                    &[("name", name), ("projects", &users.join(", "))],
                ),
            );
        }
    }
    let bundles = obj_mut(&mut config, "bundles");
    let mut entry = bundles.get(name).cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
    entry["instance"] = json!(instance);
    let old = entry.get("token").cloned().unwrap_or(json!(""));
    entry["token"] = if token.is_empty() { old } else { json!(token) };
    bundles.insert(name.to_string(), entry);
    tryw!(save(ctx, &config, &gpath));
    if !webhook_url.is_empty() {
        let spath = slack_config_path(ctx);
        let mut slack = read_json_or_empty(&spath);
        obj_mut(&mut slack, "bundle_webhooks").insert(name.to_string(), json!(webhook_url));
        tryw!(save(ctx, &slack, &spath));
    }
    (
        true,
        name_msg(ctx, if is_new { "Added bundle {name}" } else { "Updated bundle {name}" }, name),
    )
}

fn delete_bundle(ctx: &UiCtx, name: &str) -> (bool, String) {
    let gpath = gitlab_config_path(ctx);
    let mut config = read_json_or_empty(&gpath);
    if !has_key(&config, "bundles", name) {
        return (false, name_msg(ctx, "Unknown bundle: {name}", name));
    }
    let users = bundle_users(&config, name);
    if !users.is_empty() {
        return (
            false,
            ctx.tf(
                "Cannot delete {name}: still used by project(s) {projects}",
                &[("name", name), ("projects", &users.join(", "))],
            ),
        );
    }
    obj_mut(&mut config, "bundles").shift_remove(name);
    tryw!(save(ctx, &config, &gpath));
    let spath = slack_config_path(ctx);
    let mut slack = read_json_or_empty(&spath);
    if has_key(&slack, "bundle_webhooks", name) {
        obj_mut(&mut slack, "bundle_webhooks").shift_remove(name);
        tryw!(save(ctx, &slack, &spath));
    }
    (true, name_msg(ctx, "Deleted bundle {name}", name))
}

fn clear_bundle_webhook(ctx: &UiCtx, name: &str) -> (bool, String) {
    let spath = slack_config_path(ctx);
    let mut slack = read_json_or_empty(&spath);
    if !has_key(&slack, "bundle_webhooks", name) {
        return (false, name_msg(ctx, "No Slack webhook override set for bundle {name}", name));
    }
    obj_mut(&mut slack, "bundle_webhooks").shift_remove(name);
    tryw!(save(ctx, &slack, &spath));
    (true, name_msg(ctx, "Cleared Slack webhook override for bundle {name}", name))
}

fn update_loop_settings(ctx: &UiCtx, assignee: &str, worktree_root: &str, instance: &str) -> (bool, String) {
    let (assignee, worktree_root, instance) = (assignee.trim(), worktree_root.trim(), instance.trim());
    if assignee.is_empty() {
        return (false, ctx.t("GitLab username is required"));
    }
    if worktree_root.is_empty() {
        return (false, ctx.t("Worktree root is required"));
    }
    let gitlab_config = read_json_or_empty(&gitlab_config_path(ctx));
    if !has_key(&gitlab_config, "instances", instance) {
        return (false, name_msg(ctx, "Unknown instance: {name}", instance));
    }
    let worktree_root = worktree::expand_home(worktree_root, &ctx.state.user_home);
    let path = projects_path(ctx);
    let mut config = read_json_or_empty(&path);
    let checkouts: Vec<String> = config
        .get("projects")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(_, p)| text(p, "local_path"))
        .collect();
    if let Err(problem) = worktree::check_worktree_root(&worktree_root, &checkouts) {
        return (false, root_problem_message(ctx, problem));
    }
    config["assignee_username"] = json!(assignee);
    config["worktree_root"] = json!(&worktree_root);
    config["gitlab_instance"] = json!(instance);
    if config.get("projects").is_none() {
        config["projects"] = json!({});
    }
    tryw!(save(ctx, &config, &path));
    (true, ctx.t("Updated project settings"))
}

fn root_problem_message(ctx: &UiCtx, problem: RootProblem) -> String {
    match problem {
        RootProblem::Empty => ctx.t("Worktree root is required"),
        RootProblem::NotAbsolute => ctx.t("Worktree root must be an absolute path"),
        RootProblem::InsideCheckout(repo) => ctx.tf(
            "Worktree root must be outside the project checkout {path}",
            &[("path", &repo)],
        ),
        RootProblem::CannotCreate(e) => ctx.tf(
            "Could not create the worktree root: {error}",
            &[("error", &e)],
        ),
    }
}

fn checkout_problem_message(ctx: &UiCtx, problem: CheckoutProblem, path: &str) -> String {
    let args = [("path", path)];
    match problem {
        CheckoutProblem::Empty => ctx.t("Local checkout path is required"),
        CheckoutProblem::NotAbsolute => ctx.t("Local checkout path must be an absolute path"),
        CheckoutProblem::NotADirectory => ctx.tf("Local checkout path is not a directory: {path}", &args),
        CheckoutProblem::NotAGitRepo => ctx.tf("Local checkout path is not a git repository: {path}", &args),
        CheckoutProblem::NoOrigin => ctx.tf(
            "The checkout at {path} has no origin remote to fetch from",
            &args,
        ),
    }
}

/// `fields`: alias, project_id, local_path, target_branch, install_cmd, lint_cmd,
/// test_cmd, instance, original_alias.
fn upsert_tracked_project(ctx: &UiCtx, fields: &[String; 9]) -> (bool, String) {
    let f: Vec<&str> = fields.iter().map(|s| s.trim()).collect();
    let (alias, project_id, instance, original) = (f[0], f[1], f[7], f[8]);
    if alias.is_empty() {
        return (false, ctx.t("Project alias is required"));
    }
    if project_id.is_empty() {
        return (false, ctx.t("Project ID is required"));
    }
    if f[3].is_empty() {
        return (false, ctx.t("Target branch is required"));
    }
    if f[2].is_empty() {
        return (false, ctx.t("Local checkout path is required"));
    }
    let local_path = worktree::expand_home(f[2], &ctx.state.user_home);
    if let Err(problem) = worktree::check_checkout(&local_path) {
        return (false, checkout_problem_message(ctx, problem, &local_path));
    }
    let path = projects_path(ctx);
    let mut config = read_json_or_empty(&path);
    let root = text(&config, "worktree_root");
    if !root.is_empty() {
        let root = worktree::expand_home(&root, &ctx.state.user_home);
        if Path::new(&root).starts_with(&local_path) {
            return (
                false,
                root_problem_message(ctx, RootProblem::InsideCheckout(local_path)),
            );
        }
    }
    let projects = obj_mut(&mut config, "projects");
    let renaming = !original.is_empty() && original != alias;
    if renaming {
        if !projects.contains_key(original) {
            return (false, name_msg(ctx, "Unknown project: {name}", original));
        }
        if projects.contains_key(alias) {
            return (false, name_msg(ctx, "Project alias already in use: {name}", alias));
        }
        projects.shift_remove(original);
    }
    let is_new = !projects.contains_key(alias);
    let mut entry = json!({
        "project_id": project_id,
        "local_path": local_path,
        "target_branch": f[3],
        "install_cmd": f[4],
        "lint_cmd": f[5],
        "test_cmd": f[6],
    });
    if !instance.is_empty() {
        entry["instance"] = json!(instance);
    }
    projects.insert(alias.to_string(), entry);
    tryw!(save(ctx, &config, &path));
    if renaming {
        return (true, ctx.tf("Renamed project {old} to {new}", &[("old", original), ("new", alias)]));
    }
    (
        true,
        name_msg(ctx, if is_new { "Added project {name}" } else { "Updated project {name}" }, alias),
    )
}

fn delete_tracked_project(ctx: &UiCtx, alias: &str) -> (bool, String) {
    let path = projects_path(ctx);
    let mut config = read_json_or_empty(&path);
    if !has_key(&config, "projects", alias) {
        return (false, name_msg(ctx, "Unknown project: {name}", alias));
    }
    obj_mut(&mut config, "projects").shift_remove(alias);
    tryw!(save(ctx, &config, &path));
    (true, name_msg(ctx, "Deleted project {name}", alias))
}

#[allow(dead_code)]
fn unused(_: DateTime<Utc>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::routes::UiRequest;
    use crate::ui::state::UiState;

    struct Sandbox {
        dir: tempfile::TempDir,
        state: UiState,
        req: UiRequest,
    }

    impl Sandbox {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            std::fs::create_dir_all(home.join(".gitlab")).unwrap();
            std::fs::write(
                home.join(".gitlab/config.json"),
                r#"{"instances":{"g":{"url":"https://g.example","token":"t"}}}"#,
            )
            .unwrap();
            let state = UiState::explicit(dir.path(), &dir.path().join("loopx"), &home, dir.path(), "tok", None);
            Self { dir, state, req: UiRequest::get("/") }
        }
        fn ctx(&self) -> UiCtx<'_> {
            UiCtx::new(&self.state, &self.req)
        }
        fn path(&self, rel: &str) -> String {
            self.dir.path().join(rel).display().to_string()
        }
        fn repo(&self, name: &str, origin: bool) -> String {
            let p = self.dir.path().join(name);
            std::fs::create_dir_all(&p).unwrap();
            let git = |a: &[&str]| {
                assert!(std::process::Command::new("git").arg("-C").arg(&p).args(a).output().unwrap().status.success());
            };
            git(&["init", "-q"]);
            if origin {
                git(&["remote", "add", "origin", "https://example.invalid/x.git"]);
            }
            p.display().to_string()
        }
        fn project(&self, local_path: &str, branch: &str) -> (bool, String) {
            let f = |s: &str| s.to_string();
            upsert_tracked_project(
                &self.ctx(),
                &[f("app"), f("g/app"), f(local_path), f(branch), f(""), f(""), f(""), f(""), f("")],
            )
        }
        fn saved(&self) -> Value {
            read_json_or_empty(&projects_path(&self.ctx()))
        }
    }

    #[test]
    fn a_project_needs_a_target_branch_and_a_real_checkout() {
        let sb = Sandbox::new();
        let repo = sb.repo("repo", true);
        assert_eq!(sb.project(&repo, "").1, "Target branch is required");
        assert_eq!(sb.project("", "main").1, "Local checkout path is required");
        assert_eq!(sb.project("repo", "main").1, "Local checkout path must be an absolute path");
        assert!(sb.project(&sb.path("missing"), "main").1.contains("is not a directory"));
        std::fs::create_dir(sb.path("plain")).unwrap();
        assert!(sb.project(&sb.path("plain"), "main").1.contains("is not a git repository"));
        let bare = sb.repo("local-only", false);
        assert!(sb.project(&bare, "main").1.contains("has no origin remote"));
        assert!(sb.saved().get("projects").is_none(), "nothing saved after rejections");
        assert_eq!(sb.project(&repo, "main"), (true, "Added project app".to_string()));
        assert_eq!(sb.saved()["projects"]["app"]["local_path"], repo);
    }

    #[test]
    fn a_tilde_checkout_path_is_stored_absolute() {
        let sb = Sandbox::new();
        sb.repo("home/code/app", true);
        assert!(sb.project("~/code/app", "main").0);
        assert_eq!(sb.saved()["projects"]["app"]["local_path"], sb.path("home/code/app"));
    }

    #[test]
    fn the_worktree_root_is_absolute_created_and_outside_checkouts() {
        let sb = Sandbox::new();
        let save = |root: &str| update_loop_settings(&sb.ctx(), "dev", root, "g");
        assert_eq!(save("worktrees").1, "Worktree root must be an absolute path");
        let root = sb.path("wt/nested");
        assert!(save(&root).0);
        assert!(Path::new(&root).is_dir());
        assert_eq!(sb.saved()["worktree_root"], root);
        assert!(save("~/wt").0);
        assert_eq!(sb.saved()["worktree_root"], sb.path("home/wt"));
        // A root inside a tracked checkout is refused from both directions.
        let repo = sb.repo("repo", true);
        assert!(sb.project(&repo, "main").0);
        let inside = format!("{repo}/wt");
        assert!(save(&inside).1.contains("must be outside the project checkout"));
        assert!(sb.saved()["worktree_root"] != inside.as_str());
        assert!(save(&sb.path("wt2")).0);
        let on_root = sb.repo("wt2", true);
        assert!(sb.project(&on_root, "main").1.contains("must be outside the project checkout"));
    }
}
