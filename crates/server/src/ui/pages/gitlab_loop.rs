//! The GitLab loop's page `/loops/gitlab-loop` (views Live, Tasks, Worktrees,
//! Projects), its lazily loaded fragments `GET /gitlab/live` and
//! `GET /settings/fragment`, the issue tracking switch
//! (`POST /gitlab/issues/<alias>/<iid>/enable|disable`), removing an issue's
//! branch and worktree (`POST /gitlab/worktrees/<alias>/<iid>/remove`) and
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
use crate::ui::icons::icon;
use crate::ui::{ai_cli, consts, gitlab as ui_gitlab};
use chrono::{DateTime, Local, Utc};
use loopx_core::config::use_worktree;
use loopx_core::loops_config::py_truthy;
use loopx_core::run_control::{self, Launcher, TriggerOutcome};
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
    r.post("/gitlab/worktrees/:alias/:iid/remove", remove_worktree);
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
    r.post("/settings/loop-projects/:alias/track", |c| {
        flash_post(c, |c, f| {
            track_project(
                c,
                c.param("alias"),
                f.first("local_path").unwrap_or(""),
                f.first("target_branch").unwrap_or(""),
                f.first("use_worktree").is_some(),
            )
        })
    });
    r.post("/settings/loop-projects/github", |c| {
        flash_post(c, |c, f| {
            add_github_project(
                c,
                &GithubProjectForm {
                    alias: f.first("alias").unwrap_or(""),
                    account: f.first("account").unwrap_or(""),
                    repo: f.first("repo").unwrap_or(""),
                    local_path: f.first("local_path").unwrap_or(""),
                    target_branch: f.first("target_branch").unwrap_or(""),
                    assignee: f.first("assignee_username").unwrap_or(""),
                    use_worktree: f.first("use_worktree").is_some(),
                },
            )
        })
    });
    r.post("/settings/loop-projects/:alias/worktree/:state", |c| {
        flash_post(c, |c, _| match c.param("state") {
            "on" => set_project_worktree(c, c.param("alias"), true),
            "off" => set_project_worktree(c, c.param("alias"), false),
            other => (false, name_msg(c, "Unknown setting: {name}", other)),
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
        "worktrees" => worktrees_body(ctx, flash.as_deref(), flash_ok),
        _ => projects_body(ctx, flash.as_deref(), flash_ok),
    };
    UiResponse::html(render_loop_page(ctx, &hub, "gitlab-loop", view, &body, None))
}

fn render(name: &str, scope: &Scope) -> String {
    Template::named(name)
        .render(scope)
        .unwrap_or_else(|e| panic!("{e}"))
}

/// The last fetched Tasks data ([`crate::ui::live_cache`]), painted while `/gitlab/live` loads.
const TASKS_CACHE: &str = "gitlab-tasks";

/// Paints the last fetched list at once (under a "refreshing" note) when there
/// is one, else a spinner; `/gitlab/live` then swaps in the fresh list.
fn tasks_body(ctx: &UiCtx) -> String {
    let cached = crate::ui::live_cache::read(ctx.state, TASKS_CACHE)
        .and_then(|(data, saved)| Some((live_fragment_html(ctx, &tasks_cache_from(&data)?), saved)));
    let mut scope = Scope::new(ctx.lang);
    scope.var(
        "lazy_inner",
        crate::ui::live_cache::placeholder(ctx, cached, &ctx.t("Loading live GitLab data")),
    );
    render("gitlab_tasks_body", &scope)
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
    /// What the AI CLI itself said when the call failed (`issue.agent_failed`).
    cli_message: Option<String>,
    mr_url: String,
    timestamp: String,
    title: String,
    web_url: String,
    /// The checks the agent chose (`verification.commands`), already HTML.
    checks: String,
}

struct Activity {
    updated_at: Option<String>,
    issues: Vec<IssueRow>,
    /// The run-start warning for a CLI without a tool allowlist.
    warning: Option<String>,
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
    let mut warnings: Vec<(Value, String)> = Vec::new();
    for event in &all {
        let etype = event.get("event_type").map(pydumps::py_str).unwrap_or_default();
        if etype == "run.started" {
            if let Some(w) = event.pointer("/data/warning").and_then(Value::as_str) {
                warnings.push((event.get("run_id").cloned().unwrap_or(Value::Null), w.to_string()));
            }
            continue;
        }
        let iid = event.get("issue_iid").cloned().unwrap_or(Value::Null);
        if !(etype.starts_with("issue.") || etype == "verification.commands") || iid.is_null() {
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
                    cli_message: None,
                    mr_url: String::new(),
                    timestamp: String::new(),
                    title: String::new(),
                    web_url: String::new(),
                    checks: String::new(),
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
            "verification.commands" => {
                let commands: Vec<String> = ["lint", "test"]
                    .iter()
                    .map(|k| str_or_empty(data.get(*k)))
                    .filter(|c| !c.trim().is_empty())
                    .map(|c| format!("<code>{}</code>", pyfmt::escape(&c)))
                    .collect();
                let summary = pyfmt::escape(&str_or_empty(data.get("summary")));
                row.checks = [summary, commands.join(" ")]
                    .into_iter()
                    .filter(|p| !p.is_empty())
                    .collect::<Vec<_>>()
                    .join(" \u{2014} ");
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
                row.cli_message = data
                    .get("cli")
                    .is_some_and(py_truthy)
                    .then(|| str_or_empty(data.get("stderr_excerpt")));
            }
            _ => continue,
        }
        let ts = str_or_empty(event.get("timestamp"));
        row.timestamp = ts.clone();
        updated = Some(ts);
    }
    let run_id = run_id?;
    let warning = warnings.into_iter().find(|(r, _)| *r == run_id).map(|(_, w)| w);
    Some(Activity { updated_at: updated, issues, warning })
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
            csrf,
            Some(&ctx.t(
                "No projects configured yet - <a href='/loops/gitlab-loop?view=projects'>add one on the GitLab page</a>.",
            )),
        )
    } else {
        run_now_action_html(
            ctx,
            "/run-now",
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
    let warning = activity
        .warning
        .as_deref()
        .map(|w| ai_cli::warning_notice(ctx.lang, w))
        .unwrap_or_default();
    format!("<section class='card'>{head}{warning}{body}{link}</section>")
}

fn is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// The merge request number in a GitLab MR URL (`…/-/merge_requests/<n>[/…]`)
/// or a GitHub pull request URL (`…/pull/<n>[/…]`).
fn mr_number(url: &str) -> Option<&str> {
    let rest = url.split("/merge_requests/").nth(1).or_else(|| url.split("/pull/").nth(1))?;
    let n = rest.split(['/', '?', '#']).next()?;
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then_some(n)
}

/// `Some("GitHub")` when the project's items came from GitHub.
fn provider_label(entry: &AliasState) -> Option<&'static str> {
    entry
        .issues
        .iter()
        .chain(&entry.mrs)
        .any(|i| i.get("provider").and_then(Value::as_str) == Some("github"))
        .then_some("GitHub")
}

/// GitLab numbers merge requests `!n`; GitHub numbers pull requests `#n`.
fn mr_prefix(item: &Value) -> &'static str {
    if item.get("provider").and_then(Value::as_str) == Some("github") {
        "#"
    } else {
        "!"
    }
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
            let label = match mr_number(&issue.mr_url) {
                Some(n) if issue.mr_url.contains("/pull/") => {
                    pyfmt::escape(&ctx.tf("Pull request #{number}", &[("number", n)]))
                }
                Some(n) => pyfmt::escape(&ctx.tf("Merge request !{number}", &[("number", n)])),
                None => esc_t(ctx, "Merge request"),
            };
            format!(
                "<a class='result-mr' href='{}' target='_blank' rel='noopener'>{}{label}{}</a>",
                pyfmt::escape(&issue.mr_url),
                icon("merge"),
                icon("open_in_new")
            )
        } else {
            String::new()
        };
        let mut detail = if issue.detail.is_empty() {
            String::new()
        } else {
            format!(
                "<p class='result-summary'>{}</p>",
                pyfmt::escape(&issue.detail.replace('_', " "))
            )
        };
        if let Some(message) = issue.cli_message.as_deref().filter(|_| issue.outcome == "failed") {
            if !message.is_empty() {
                detail.push_str(&format!("<p class='result-summary'>{}</p>", pyfmt::escape(message)));
            }
            detail.push_str(&format!("<p class='result-summary'>{}</p>", ai_cli::settings_link(ctx.lang)));
        }
        if !issue.checks.is_empty() {
            detail.push_str(&format!(
                "<p class='result-summary'>{}: {}</p>",
                esc_t(ctx, "Checks"),
                issue.checks
            ));
        }
        let ago = match super::loops_common::aware(&issue.timestamp) {
            Some(stamp) => format!(
                "<span class='result-time' title='{}'>{}{}</span>",
                pyfmt::escape(&local_minute(stamp)),
                icon("schedule"),
                pyfmt::escape(&relative_time_dt(ctx.state.now(), stamp))
            ),
            None => String::new(),
        };
        // Time and MR get their own line under the title, so they line up
        // from row to row however long the title is.
        let meta = if ago.is_empty() && mr.is_empty() {
            String::new()
        } else {
            format!("<div class='result-meta'>{ago}{mr}</div>")
        };
        rows.push_str(&format!(
            "<li class='result-row result-{kind}'><div class='result-head'><span class='result-title'>{title}</span><span class='pill history-pill history-{kind}'>{}</span></div>{meta}{detail}</li>",
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
// Worktrees view: every local `loop/issue-<iid>` branch of the tracked checkouts

const WORKTREES_URL: &str = "/loops/gitlab-loop?view=worktrees";

/// `(alias, local_path)` of every tracked project that has a checkout.
fn tracked_checkouts(ctx: &UiCtx) -> Vec<(String, String)> {
    read_json_or_empty(&projects_path(ctx))
        .get("projects")
        .and_then(Value::as_object)
        .map(|projects| {
            projects
                .iter()
                .filter_map(|(alias, p)| {
                    let path = p.get("local_path")?.as_str()?.trim();
                    (!path.is_empty()).then(|| (alias.clone(), path.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The title (from the cached Tasks list) and link of `alias #iid`; without a
/// cached row, the link is built from the alias's GitLab instance URL.
fn issue_ref(cached: &[(String, AliasState)], gitlab_config: &Value, alias: &str, iid: &str) -> (String, String) {
    let row = cached
        .iter()
        .filter(|(a, _)| a == alias)
        .flat_map(|(_, s)| &s.issues)
        .find(|i| item_str(i, "iid", "") == iid);
    let title = row.map(|i| item_str(i, "title", "")).unwrap_or_default();
    let url = row.map(|i| item_str(i, "web_url", "")).filter(|u| is_http(u));
    let url = url.unwrap_or_else(|| {
        let (project_id, instance, _) = gitlab::resolve_project_alias(Some(gitlab_config), alias, None);
        let instance = instance.unwrap_or_else(|| text(gitlab_config, "default"));
        let base = gitlab_config
            .pointer(&format!("/instances/{instance}/url"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_end_matches('/');
        // A numeric project id has no web path to build a link from.
        if is_http(base) && project_id.contains('/') {
            format!("{base}/{project_id}/-/issues/{iid}")
        } else {
            String::new()
        }
    });
    (title, url)
}

/// A bash/zsh helper that cds into the worktree checked out on a branch, run
/// from inside the project's checkout.
const WT_SNIPPET: &str = r#"wt() {
  local dir
  [[ -n $1 ]] || { echo "usage: wt <branch>" >&2; return 2; }
  dir=$(git worktree list --porcelain | awk -v b="refs/heads/$1" '
    /^worktree /{p=substr($0,10)} $0=="branch "b{print p; exit}')
  [[ -n $dir ]] || { echo "no worktree for branch: $1" >&2; return 1; }
  cd "$dir"
}"#;

fn wt_snippet_card(ctx: &UiCtx) -> String {
    format!(
        "<section class='card'><div class='section-header'><h2>{}</h2><button type='button' class='btn btn-neutral' style='margin-left: auto;' data-copy-target='wt-snippet' data-copied-label='{}'><svg class='icon' aria-hidden='true'><use href='#i-content_copy'/></svg> <span data-copy-label>{}</span></button></div><p class='section-subtitle'>{}</p><pre class='log' id='wt-snippet'>{}</pre></section>",
        esc_t(ctx, "Switch to a worktree"),
        esc_t(ctx, "Copied"),
        esc_t(ctx, "Copy"),
        esc_t(ctx, "Add this function to your ~/.zshrc or ~/.bashrc. Then, from inside the project's checkout, run wt with an issue branch (for example wt loop/issue-7) to jump into its worktree and check the change."),
        pyfmt::escape(WT_SNIPPET),
    )
}

fn worktrees_body(ctx: &UiCtx, flash: Option<&str>, flash_ok: bool) -> String {
    let csrf = ctx.csrf_input();
    let cached = crate::ui::live_cache::read(ctx.state, TASKS_CACHE)
        .and_then(|(data, _)| tasks_cache_from(&data))
        .unwrap_or_default();
    let gitlab_config = read_json_or_empty(&gitlab_config_path(ctx));
    let mut rows = Vec::new();
    let mut errors = String::new();
    for (alias, local_path) in tracked_checkouts(ctx) {
        let branches = match worktree::list_issue_branches(&local_path) {
            Ok(b) => b,
            Err(e) => {
                errors.push_str(&error_notice(ctx, &Some(format!("{alias}: {}", e.message))));
                continue;
            }
        };
        let safe_alias = pyfmt::escape(&alias);
        for b in branches {
            let (title, url) = issue_ref(&cached, &gitlab_config, &alias, &b.iid);
            let number = format!("#{}", b.iid);
            let number = if url.is_empty() {
                number
            } else {
                format!("<a href='{}' target='_blank' rel='noopener'>{number}</a>", pyfmt::escape(&url))
            };
            let issue = if title.is_empty() {
                number
            } else {
                format!("{number} {}", pyfmt::escape(&title))
            };
            let place = match (&b.worktree, b.in_checkout) {
                (Some(path), _) => format!("<code>{}</code>", pyfmt::escape(path)),
                (None, true) => esc_t(ctx, "In the checkout"),
                (None, false) => esc_t(ctx, "No worktree"),
            };
            let state = if b.dirty {
                format!("<span class='pill pill-red'>{}</span>", esc_t(ctx, "Uncommitted changes"))
            } else {
                format!("<span class='pill pill-grey'>{}</span>", esc_t(ctx, "Clean"))
            };
            let remove = if b.in_checkout {
                String::new()
            } else {
                let confirm = ctx.tf(
                    "Remove {branch} and its worktree for {alias} #{iid}? Uncommitted changes in it are lost; a branch already pushed to GitLab is kept.",
                    &[("branch", &b.branch), ("alias", &alias), ("iid", &b.iid)],
                );
                format!(
                    "<form method='post' action='/gitlab/worktrees/{}/{}/remove' class='daemon-action-form'>{csrf}<button type='submit' class='btn btn-warning' data-confirm=\"{}\"><svg class='icon' aria-hidden='true'><use href='#i-delete'/></svg> {}</button></form>",
                    pyfmt::quote(&alias, ""),
                    b.iid,
                    pyfmt::escape(&confirm),
                    esc_t(ctx, "Remove")
                )
            };
            rows.push(format!(
                "<tr><td>{issue}</td><td>{safe_alias}</td><td><code>{}</code></td><td>{place}</td><td>{state}</td><td>{}</td><td>{remove}</td></tr>",
                pyfmt::escape(&b.branch),
                pyfmt::escape(&relative_time(ctx.state.now(), &b.last_commit)),
            ));
        }
    }
    let list = if rows.is_empty() {
        format!(
            "<p style='color: var(--md-on-surface-variant);'>{}</p>",
            esc_t(ctx, "No issue branches in progress.")
        )
    } else {
        let headers = [
            esc_t(ctx, "Issue"),
            esc_t(ctx, "Project"),
            esc_t(ctx, "Branch"),
            esc_t(ctx, "Worktree"),
            esc_t(ctx, "State"),
            esc_t(ctx, "Last commit"),
            String::new(),
        ];
        table(&headers, &rows)
    };
    format!(
        "{}{}<section class='card'><div class='section-header'><h2>{}</h2></div><p class='section-subtitle'>{}</p>{errors}{list}</section>",
        flash_html(flash, flash_ok),
        wt_snippet_card(ctx),
        esc_t(ctx, "Worktrees"),
        esc_t(ctx, "The branches the loop started for issues, with their worktrees. Removing one deletes the worktree folder and the local branch."),
    )
}

/// Deletes one issue's local branch and worktree, refused while a run is going.
fn remove_worktree(ctx: &UiCtx) -> UiResponse {
    let alias = ctx.param("alias");
    let Some(iid) = py_int(ctx.param("iid")).filter(|n| *n > 0).map(|n| n.to_string()) else {
        return UiResponse::not_found(ctx.lang);
    };
    let (ok, message) = if status::read_status(&ctx.state.status_path())
        .get("state")
        .and_then(Value::as_str)
        == Some("running")
    {
        (false, ctx.t("The GitLab loop is running - wait for it to finish or stop it before removing a worktree"))
    } else if let Some((_, local_path)) = tracked_checkouts(ctx).into_iter().find(|(a, _)| a == alias) {
        match worktree::remove_issue_branch(&local_path, &iid) {
            Ok(()) => (
                true,
                ctx.tf(
                    "Removed {branch} for {alias} #{iid}",
                    &[("branch", &worktree::branch_name(&iid)), ("alias", alias), ("iid", &iid)],
                ),
            ),
            Err(e) => (false, e.message),
        }
    } else {
        (false, name_msg(ctx, "{name} is not a tracked project with a checkout", alias))
    };
    UiResponse::flash_redirect(ok, &message, WORKTREES_URL)
}

// ---------------------------------------------------------------------------
// GET /gitlab/live

/// One project alias's `{issues, mrs, issues_error, mrs_error}`.
#[derive(Debug, Clone)]
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

/// The dashboard's connector accounts (sandbox-aware paths and Keychain).
fn hosted_source(ctx: &UiCtx) -> Option<loopx_core::connectors_config::ConfigConnectors> {
    use super::connectors as conn;
    let store = conn::store(ctx.state).ok()?;
    Some(
        loopx_core::connectors_config::ConfigConnectors::real()
            .with_paths(conn::paths(ctx.state))
            .with_store(store)
            .with_http(conn::http(ctx.state)),
    )
}

/// A `code_host` project's issues and change requests (see
/// `loopx_core::hosted::dashboard_lists`).
fn fetch_hosted(source: Option<&loopx_core::connectors_config::ConfigConnectors>, project: &Value) -> AliasState {
    let failed = |message: String| AliasState {
        issues: vec![],
        mrs: vec![],
        issues_error: Some(message.clone()),
        mrs_error: Some(message),
    };
    let Some(source) = source else {
        return failed("the Keychain could not be opened".into());
    };
    match loopx_core::hosted::resolve_project(source, project) {
        Err(e) => failed(e.to_string()),
        Ok(r) => {
            let (issues, mrs) = loopx_core::hosted::dashboard_lists(&r, project);
            AliasState {
                issues_error: issues.as_ref().err().cloned(),
                mrs_error: mrs.as_ref().err().cloned(),
                issues: issues.unwrap_or_default(),
                mrs: mrs.unwrap_or_default(),
            }
        }
    }
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
    // `code_host` projects (GitHub, ...) go through the connector accounts.
    let hosted: Vec<Option<Value>> = aliases
        .iter()
        .map(|a| cfg.project(a).ok().filter(loopx_core::hosted::is_hosted))
        .collect();
    let source = hosted.iter().any(Option::is_some).then(|| hosted_source(ctx)).flatten();
    std::thread::scope(|scope| {
        let handles: Vec<_> = aliases
            .iter()
            .zip(&hosted)
            .map(|(alias, project)| {
                let (config, username, source) = (&gitlab_config, &username, &source);
                let handle = scope.spawn(move || match project {
                    Some(p) => fetch_hosted(source.as_ref(), p),
                    None => fetch_alias(config, alias, username),
                });
                (alias.clone(), handle)
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

/// "Run now" for one tracked issue: starts the loop on just this issue. Hidden by
/// CSS while the row's tracking switch is off (the route refuses it then too).
fn issue_run_html(ctx: &UiCtx, alias: &str, issue_iid: &Value) -> String {
    let safe_alias = pyfmt::quote(alias, "");
    let iid = pydumps::py_str(issue_iid);
    let label = pyfmt::escape(&ctx.tf("Run the GitLab loop on #{iid} now", &[("iid", &iid)]));
    format!(
        "<form method='post' action='/gitlab/issues/{safe_alias}/{iid}/run' class='daemon-action-form issue-run-now'>{}<button type='submit' class='btn btn-tonal' aria-label='{label}' title='{label}'><svg class='icon' aria-hidden='true'><use href='#i-bolt'/></svg> {}</button></form>",
        ctx.csrf_input(),
        esc_t(ctx, "Run now")
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
        (Some(a), Some(i)) => format!("{}{}", issue_run_html(ctx, a, i), tracking_toggle_html(ctx, a, i)),
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
            "<li class='gitlab-item'><div class='gitlab-item-row'>{link}<span class='gitlab-item-meta'>{}</span>{toggle}</div>{label_row}</li>",
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
            "<p class='inline-error'><svg class='icon' aria-hidden='true'><use href='#i-error'/></svg> {}</p>",
            ctx.tf("Couldn't check: {message}", &[("message", &pyfmt::escape(m))])
        ),
        _ => String::new(),
    }
}

fn is_mine(item: &Value) -> bool {
    item.get("_assigned_to_me").is_some_and(py_truthy)
}

fn live_fragment(ctx: &UiCtx) -> UiResponse {
    let live = live_state(ctx);
    let previous = crate::ui::live_cache::read(ctx.state, TASKS_CACHE)
        .and_then(|(d, _)| tasks_cache_from(&d))
        .unwrap_or_default();
    crate::ui::live_cache::write(
        ctx.state,
        TASKS_CACHE,
        tasks_cache_json(&merge_tasks_cache(&previous, live.clone())),
    );
    UiResponse::html(live_fragment_html(ctx, &live))
}

fn tasks_cache_json(live: &[(String, AliasState)]) -> Value {
    Value::Array(
        live.iter()
            .map(|(alias, s)| {
                json!({"alias": alias, "issues": s.issues, "mrs": s.mrs,
                       "issues_error": s.issues_error, "mrs_error": s.mrs_error})
            })
            .collect(),
    )
}

fn tasks_cache_from(data: &Value) -> Option<Vec<(String, AliasState)>> {
    let list = |v: &Value, k: &str| v.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    data.as_array()?
        .iter()
        .map(|v| {
            Some((
                text(v, "alias")?,
                AliasState {
                    issues: list(v, "issues"),
                    mrs: list(v, "mrs"),
                    issues_error: text(v, "issues_error"),
                    mrs_error: text(v, "mrs_error"),
                },
            ))
        })
        .collect()
}

/// What to cache after a fetch: the fresh data, except that a failed half
/// (issues or MRs) of a project keeps its last good rows (the page itself
/// still shows the error).
fn merge_tasks_cache(previous: &[(String, AliasState)], fresh: Vec<(String, AliasState)>) -> Vec<(String, AliasState)> {
    fresh
        .into_iter()
        .map(|(alias, mut s)| {
            if let Some((_, old)) = previous.iter().find(|(a, _)| *a == alias) {
                if s.issues_error.is_some() && old.issues_error.is_none() {
                    s.issues = old.issues.clone();
                    s.issues_error = None;
                }
                if s.mrs_error.is_some() && old.mrs_error.is_none() {
                    s.mrs = old.mrs.clone();
                    s.mrs_error = None;
                }
            }
            (alias, s)
        })
        .collect()
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
            .map(|m| gitlab_item(ctx, m, mr_prefix(m), None, false))
            .collect();
        let heading = match provider_label(entry) {
            Some(p) => format!("{} <span class='pill pill-grey'>{p}</span>", pyfmt::escape(alias)),
            None => pyfmt::escape(alias),
        };
        out.push_str(&format!(
            "<div class='project-block'><h3>{}</h3><div class='gitlab-subsection'><p class='gitlab-subhead'>{} <span class='badge-count'>{}</span></p>{}<ul class='plain gitlab-list'>{}</ul></div><div class='gitlab-subsection'><p class='gitlab-subhead'>MRs <span class='badge-count'>{}</span></p>{}<ul class='plain gitlab-list'>{}</ul></div></div>",
            heading,
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
// POST /gitlab/issues/<alias>/<iid>/enable|disable|run

fn issue_tracking_toggle(ctx: &UiCtx) -> UiResponse {
    let rest = ctx.param("rest").to_string();
    if let Some(middle) = rest.strip_suffix("/run") {
        return issue_run(ctx, middle);
    }
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

/// Starts the GitLab loop on one tracked issue, then shows the Live view.
fn issue_run(ctx: &UiCtx, middle: &str) -> UiResponse {
    let (alias, iid_part) = middle.rsplit_once('/').unwrap_or(("", middle));
    let Some(iid) = py_int(iid_part) else {
        return UiResponse::not_found(ctx.lang);
    };
    let back = "/loops/gitlab-loop?view=live";
    let path = ctx.state.home_dir.join("issue_tracking.json");
    if !issue_tracking_config::is_issue_enabled(alias, iid, &path).unwrap_or(true) {
        let message = ctx.tf("#{iid} is not tracked - turn its switch on first", &[("iid", &iid.to_string())]);
        return UiResponse::flash_redirect(false, &message, back);
    }
    let fallback = ctx.state.repo_root.join("run-loop-now.sh");
    let launcher = Launcher {
        cmd: &ctx.state.run_cmd,
        cwd: &ctx.state.repo_root,
        fallback_path: &fallback,
        queue: None,
    };
    let (ok, message) = match run_control::trigger_issue_run(&ctx.state.status_path(), alias, iid, &launcher) {
        TriggerOutcome::AlreadyRunning => (false, ctx.t("A run is already in progress")),
        TriggerOutcome::LauncherMissing(p) => (false, ctx.tf("run-loop-now.sh not found at {path}", &[("path", &p)])),
        TriggerOutcome::Started | TriggerOutcome::Queued(_) => (
            true,
            ctx.tf("Started work on {alias} #{iid}", &[("alias", alias), ("iid", &iid.to_string())]),
        ),
    };
    UiResponse::flash_redirect(ok, &message, back)
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
        "<form method='post' action='{action}' class='daemon-action-form'>{csrf}<button type='submit' class='btn btn-warning' data-confirm=\"{}\"><svg class='icon' aria-hidden='true'><use href='#i-delete'/></svg> {}</button></form>",
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

/// The Track-on / edit form for `alias`, prefilled from its projects.json entry.
/// Both checkout fields left empty tracks the project for issues only.
fn track_form(ctx: &UiCtx, alias: &str, project: Option<&Value>, csrf: &str) -> String {
    let empty = json!({});
    let label = esc_t(ctx, if project.is_some() { "Save" } else { "Track" });
    let project = project.unwrap_or(&empty);
    let checked = if use_worktree(project) { " checked" } else { "" };
    format!(
        "<form method='post' action='/settings/loop-projects/{}/track' class='daemon-action-form'>{csrf}{}{}<label><input type='checkbox' name='use_worktree' value='1'{checked}> {}</label><button type='submit' class='btn btn-neutral'>{}</button></form>",
        pyfmt::quote(alias, ""),
        input_text("local_path", &text(project, "local_path"), "/abs/path/to/checkout", ""),
        input_text("target_branch", &text(project, "target_branch"), &esc_t(ctx, "target branch"), ""),
        esc_t(ctx, "Use worktree"),
        label
    )
}

/// An on switch that stops tracking `alias` (removes its projects.json entry).
fn untrack_switch(ctx: &UiCtx, alias: &str, csrf: &str) -> String {
    let label = pyfmt::escape(&ctx.tf("Stop tracking {alias}", &[("alias", alias)]));
    let confirm = pyfmt::escape(&ctx.tf("Stop tracking project {alias}?", &[("alias", alias)]));
    format!(
        "<form method='post' action='/settings/loop-projects/{}/delete' class='daemon-action-form'>{csrf}<button type='submit' class='switch is-on' role='switch' aria-checked='true' aria-label='{label}' title='{label}' data-confirm=\"{confirm}\"><span class='switch-thumb'></span></button></form>",
        pyfmt::quote(alias, "")
    )
}

/// The Track column for one GitLab alias: a collapsed Track form when it is
/// not tracked; otherwise the untrack switch, where it lives, its worktree
/// switch and an edit form (an issues-only project has no checkout, so no
/// worktree switch either).
fn track_cell(ctx: &UiCtx, alias: &str, project: Option<&Value>, csrf: &str) -> String {
    let Some(project) = project else {
        return format!(
            "<details class='loop-settings'><summary><svg class='icon' aria-hidden='true'><use href='#i-add'/></svg> {}</summary><div class='loop-settings-body'>{}</div></details>",
            esc_t(ctx, "Track"),
            track_form(ctx, alias, None, csrf)
        );
    };
    if is_issues_only(project) {
        return format!(
            "<div class='loop-toggle'>{}<span class='loop-toggle-text'>{}</span></div><div class='loop-meta'><span class='pill'>{}</span></div><details class='loop-settings'><summary><svg class='icon' aria-hidden='true'><use href='#i-settings'/></svg> {}</summary><div class='loop-settings-body'>{}</div></details>",
            untrack_switch(ctx, alias, csrf),
            esc_t(ctx, "Tracked"),
            esc_t(ctx, "Issues only"),
            esc_t(ctx, "Edit"),
            track_form(ctx, alias, Some(project), csrf)
        );
    }
    let on = use_worktree(project);
    let (state, class, checked) = if on { ("off", "is-on", "true") } else { ("on", "is-off", "false") };
    let label = esc_t(ctx, "Use worktree");
    format!(
        "<div class='loop-toggle'>{}<span class='loop-toggle-text'>{}</span></div><div class='loop-meta'><span class='loop-meta-item'><code>{}</code></span><span class='loop-meta-item'><code>{}</code></span></div><div class='loop-toggle'><form method='post' action='/settings/loop-projects/{}/worktree/{state}' class='daemon-action-form'>{csrf}<button type='submit' class='switch {class}' role='switch' aria-checked='{checked}' aria-label='{label}' title='{label}'><span class='switch-thumb'></span></button></form><span class='loop-toggle-text'>{label}</span></div><details class='loop-settings'><summary><svg class='icon' aria-hidden='true'><use href='#i-settings'/></svg> {}</summary><div class='loop-settings-body'>{}</div></details>",
        untrack_switch(ctx, alias, csrf),
        esc_t(ctx, "Tracked"),
        pyfmt::escape(&text(project, "target_branch")),
        pyfmt::escape(&text(project, "local_path")),
        pyfmt::quote(alias, ""),
        esc_t(ctx, "Edit"),
        track_form(ctx, alias, Some(project), csrf)
    )
}

/// A tracked project with no local checkout: the loop reads, analyzes and
/// replies on its issues but never changes code.
fn is_issues_only(project: &Value) -> bool {
    text(project, "local_path").trim().is_empty()
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
    let no_projects = Map::new();
    let tracked = loop_config.get("projects").and_then(Value::as_object).unwrap_or(&no_projects);
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
        "\n<form method='post' action='/settings/gitlab/instances' class='daemon-action-form add-row-form'>\n{csrf}\n<input type='text' name='alias' placeholder='{}' required>\n<input type='text' name='url' placeholder='https://gitlab.example.com' required>\n<input type='password' name='token' placeholder='{}'>\n<button type='submit' class='btn btn-neutral'><svg class='icon' aria-hidden='true'><use href='#i-add'/></svg> {}</button>\n</form>\n",
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
            "<tr><td>{safe}</td><td><form method='post' action='/settings/gitlab/projects' class='daemon-action-form'>{csrf}<input type='hidden' name='alias' value='{safe}'><input type='text' name='project_id' value='{}' placeholder='namespace/project'>{}{}<button type='submit' class='btn btn-neutral'>{}</button></form></td><td>{}</td><td>{}</td></tr>",
            pyfmt::escape(&text(project, "project_id")),
            custom_select_plain("instance", &instance_names, Some(&text(project, "instance")), None),
            custom_select_plain("bundle", &bundle_names, Some(&text(project, "bundle")), Some(&t("(use instance default)"))),
            et("Save"),
            track_cell(ctx, alias, tracked.get(alias.as_str()), &csrf),
            delete_form(ctx, &format!("/settings/gitlab/projects/{url_safe}/delete"), &confirm, &csrf)
        ));
    }
    let mut projects_html = if rows.is_empty() {
        format!("<p>{}</p>", et("(no project aliases configured)"))
    } else {
        table(&[et("Alias"), et("Edit"), et("Track"), et("Delete")], &rows)
    };
    // Tracked in projects.json but no longer a GitLab alias: only untrack.
    // GitHub projects are listed in their own card.
    for alias in tracked
        .iter()
        .filter(|(a, p)| projects.get(a.as_str()).is_none() && !loopx_core::hosted::is_hosted(p))
        .map(|(a, _)| a)
    {
        projects_html.push_str(&format!(
            "<div class='loop-toggle'>{}<span class='loop-toggle-text'>{}</span></div>",
            untrack_switch(ctx, alias, &csrf),
            pyfmt::escape(&ctx.tf("{alias}: no matching GitLab alias", &[("alias", alias)]))
        ));
    }
    let add_project_form = format!(
        "\n<form method='post' action='/settings/gitlab/projects' class='daemon-action-form add-row-form'>\n{csrf}\n<input type='text' name='alias' placeholder='{}' required>\n<input type='text' name='project_id' placeholder='namespace/project' required>\n{}\n{}\n<button type='submit' class='btn btn-neutral'><svg class='icon' aria-hidden='true'><use href='#i-add'/></svg> {}</button>\n</form>\n",
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
        "\n<form method='post' action='/settings/access-bundles' class='daemon-action-form add-row-form'>\n{csrf}\n<input type='text' name='name' placeholder='{}' required>\n{}\n<input type='password' name='token' placeholder='{}'>\n<input type='password' name='webhook_url' placeholder='{}'>\n<button type='submit' class='btn btn-neutral'><svg class='icon' aria-hidden='true'><use href='#i-add'/></svg> {}</button>\n</form>\n",
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
        .var(
            "aliases_subtitle",
            pyfmt::escape_text(&t("Turn on Track to let the GitLab issue loop work on a project. Leave the local checkout and target branch empty to track issues only: the loop reads, analyzes and replies, but never changes code. Give both to let it fix issues too; the agent works out each project's install, lint and test commands itself and logs what it used.")),
        )
        .var("projects_html", projects_html)
        .var("add_project_form", add_project_form)
        .var(
            "bundles_subtitle",
            pyfmt::escape_text(&t("A project-specific GitLab token (and optional Slack webhook) for projects whose default instance token doesn't have full access.")),
        )
        .var("bundles_html", bundles_html)
        .var("add_bundle_form", add_bundle_form)
        .var("loop_settings_form", loop_settings_form)
        .var("github_html", github_section(ctx, tracked, &csrf));
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

/// Track the GitLab alias `alias` for the issue loop (or update how it is
/// tracked): its project id and instance come from the GitLab alias, and any
/// other keys already in its projects.json entry (command hints included) are
/// kept. With neither a checkout nor a target branch it is tracked for issues
/// only.
fn track_project(ctx: &UiCtx, alias: &str, local_path: &str, target_branch: &str, use_worktree: bool) -> (bool, String) {
    let (alias, target_branch) = (alias.trim(), target_branch.trim());
    let gitlab_config = read_json_or_empty(&gitlab_config_path(ctx));
    let Some(gitlab_alias) = gitlab_config.get("projects").and_then(|p| p.get(alias)) else {
        return (false, name_msg(ctx, "Unknown project: {name}", alias));
    };
    let project_id = text(gitlab_alias, "project_id");
    let instance = text(gitlab_alias, "instance");
    let local_path = local_path.trim();
    if local_path.is_empty() && target_branch.is_empty() {
        return track_issues_only(ctx, alias, &project_id, &instance);
    }
    if local_path.is_empty() || target_branch.is_empty() {
        return (
            false,
            ctx.t("Give both a local checkout and a target branch for code changes, or leave both empty to track issues only"),
        );
    }
    let local_path = worktree::expand_home(local_path, &ctx.state.user_home);
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
    save_tracked(ctx, alias, &path, &mut config, |entry| {
        entry["project_id"] = json!(project_id);
        entry["local_path"] = json!(local_path);
        entry["target_branch"] = json!(target_branch);
        entry["use_worktree"] = json!(use_worktree);
        set_instance(entry, &instance);
    })
}

/// Track `alias` without a checkout: the loop handles its issues but never
/// changes code, so the checkout keys go (command hints stay for later).
fn track_issues_only(ctx: &UiCtx, alias: &str, project_id: &str, instance: &str) -> (bool, String) {
    let path = projects_path(ctx);
    let mut config = read_json_or_empty(&path);
    save_tracked(ctx, alias, &path, &mut config, |entry| {
        entry["project_id"] = json!(project_id);
        if let Some(obj) = entry.as_object_mut() {
            for key in ["local_path", "target_branch", "use_worktree"] {
                obj.shift_remove(key);
            }
        }
        set_instance(entry, instance);
    })
}

fn set_instance(entry: &mut Value, instance: &str) {
    match entry.as_object_mut() {
        Some(obj) if instance.is_empty() => {
            obj.shift_remove("instance");
        }
        _ => entry["instance"] = json!(instance),
    }
}

/// Apply `update` to `alias`'s projects.json entry (created if missing) and save.
/// The GitHub connector accounts a project can be bound to: `(id, label)`.
fn github_accounts(ctx: &UiCtx) -> Vec<(String, String)> {
    let paths = super::connectors::paths(ctx.state);
    loopx_core::connectors_config::accounts_with_capability(loopx_core::connectors::ISSUES, &paths)
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.kind == "github")
        .map(|a| {
            let label = if a.label.is_empty() { a.id.clone() } else { format!("{} ({})", a.label, a.id) };
            (a.id, label)
        })
        .collect()
}

/// The "Add GitHub project" form's fields.
struct GithubProjectForm<'a> {
    alias: &'a str,
    account: &'a str,
    repo: &'a str,
    local_path: &'a str,
    target_branch: &'a str,
    assignee: &'a str,
    use_worktree: bool,
}

fn name_ok(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Track a GitHub repository in projects.json as `code_host` {account, repo}:
/// the issue loop works its assigned issues and opens pull requests. Both
/// checkout fields empty tracks it for issues only, as for GitLab.
fn add_github_project(ctx: &UiCtx, f: &GithubProjectForm<'_>) -> (bool, String) {
    let (alias, account, repo) = (f.alias.trim(), f.account.trim(), f.repo.trim());
    if !name_ok(alias) {
        return (false, ctx.t("A project alias may only use letters, digits, '.', '_' and '-'"));
    }
    if !github_accounts(ctx).iter().any(|(id, _)| id == account) {
        return (false, name_msg(ctx, "Unknown GitHub account: {name}", account));
    }
    if !repo.split_once('/').is_some_and(|(o, n)| name_ok(o) && name_ok(n)) {
        return (false, ctx.t("The repository must be owner/name"));
    }
    let path = projects_path(ctx);
    let mut config = read_json_or_empty(&path);
    if let Some(existing) = config.get("projects").and_then(|p| p.get(alias)) {
        if !loopx_core::hosted::is_hosted(existing) {
            return (false, name_msg(ctx, "{name} is already a GitLab project", alias));
        }
    }
    let (local_path, target_branch) = (f.local_path.trim(), f.target_branch.trim());
    if local_path.is_empty() != target_branch.is_empty() {
        return (
            false,
            ctx.t("Give both a local checkout and a target branch for code changes, or leave both empty to track issues only"),
        );
    }
    let local_path = if local_path.is_empty() {
        None
    } else {
        let expanded = worktree::expand_home(local_path, &ctx.state.user_home);
        if let Err(problem) = worktree::check_checkout(&expanded) {
            return (false, checkout_problem_message(ctx, problem, &expanded));
        }
        let root = text(&config, "worktree_root");
        if !root.is_empty() && Path::new(&worktree::expand_home(&root, &ctx.state.user_home)).starts_with(&expanded) {
            return (false, root_problem_message(ctx, RootProblem::InsideCheckout(expanded)));
        }
        Some(expanded)
    };
    let assignee = f.assignee.trim();
    save_tracked(ctx, alias, &path, &mut config, |entry| {
        entry["code_host"] = json!({"account": account, "repo": repo});
        if let Some(obj) = entry.as_object_mut() {
            for key in ["project_id", "instance", "local_path", "target_branch", "use_worktree", "assignee_username"] {
                obj.shift_remove(key);
            }
        }
        if let Some(local_path) = &local_path {
            entry["local_path"] = json!(local_path);
            entry["target_branch"] = json!(target_branch);
            entry["use_worktree"] = json!(f.use_worktree);
        }
        if !assignee.is_empty() {
            entry["assignee_username"] = json!(assignee);
        }
    })
}

/// The GitHub card on the Projects view: the GitHub projects tracked in
/// projects.json and the form that adds one.
fn github_section(ctx: &UiCtx, tracked: &Map<String, Value>, csrf: &str) -> String {
    let et = |s: &str| esc_t(ctx, s);
    let accounts = github_accounts(ctx);
    let mut rows = Vec::new();
    for (alias, project) in tracked.iter().filter(|(_, p)| loopx_core::hosted::is_hosted(p)) {
        let repo = text(&project["code_host"], "repo");
        let account = text(&project["code_host"], "account");
        let checkout = if is_issues_only(project) {
            format!("<span class='pill'>{}</span>", et("Issues only"))
        } else {
            format!(
                "<code>{}</code> &rarr; <code>{}</code>",
                pyfmt::escape(&text(project, "local_path")),
                pyfmt::escape(&text(project, "target_branch"))
            )
        };
        rows.push(format!(
            "<tr><td>{}</td><td><a href='https://github.com/{}' target='_blank' rel='noopener'>{}</a> <span class='pill pill-grey'>{}</span></td><td>{checkout}</td><td>{}</td></tr>",
            pyfmt::escape(alias),
            pyfmt::escape(&repo),
            pyfmt::escape(&repo),
            pyfmt::escape(&account),
            untrack_switch(ctx, alias, csrf)
        ));
    }
    let list = if rows.is_empty() {
        format!("<p>{}</p>", et("(no GitHub projects yet)"))
    } else {
        table(&[et("Alias"), et("Repository"), et("Checkout"), et("Tracked")], &rows)
    };
    let form = if accounts.is_empty() {
        format!(
            "<p>{} <a href='/connectors'>{}</a></p>",
            et("Add a GitHub account first to track GitHub projects."),
            et("Connectors")
        )
    } else {
        format!(
            "\n<form method='post' action='/settings/loop-projects/github' class='daemon-action-form add-row-form'>\n{csrf}\n<input type='text' name='alias' placeholder='{}' required>\n{}\n<input type='text' name='repo' placeholder='owner/name' required>\n<input type='text' name='local_path' placeholder='/abs/path/to/checkout'>\n<input type='text' name='target_branch' placeholder='{}'>\n<input type='text' name='assignee_username' placeholder='{}'>\n<label><input type='checkbox' name='use_worktree' value='1' checked> {}</label>\n<button type='submit' class='btn btn-neutral'><svg class='icon' aria-hidden='true'><use href='#i-add'/></svg> {}</button>\n</form>\n",
            et("project alias"),
            super::loops_common::custom_select("account", &accounts, None, None),
            et("target branch"),
            et("GitHub login (optional)"),
            et("Use worktree"),
            et("Add GitHub project")
        )
    };
    format!(
        "<p class='subtitle'>{}</p>{list}{form}",
        pyfmt::escape_text(&ctx.t("Repositories on GitHub. The loop works the issues assigned to you there and opens pull requests, the same way it works GitLab issues. The GitHub login defaults to the account's own; leave the checkout and target branch empty to track issues only."))
    )
}

fn save_tracked(ctx: &UiCtx, alias: &str, path: &Path, config: &mut Value, update: impl FnOnce(&mut Value)) -> (bool, String) {
    let projects = obj_mut(config, "projects");
    let is_new = !projects.contains_key(alias);
    let entry = projects.entry(alias.to_string()).or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    update(entry);
    tryw!(save(ctx, config, path));
    (
        true,
        name_msg(ctx, if is_new { "Tracking project {name}" } else { "Updated project {name}" }, alias),
    )
}

fn set_project_worktree(ctx: &UiCtx, alias: &str, on: bool) -> (bool, String) {
    let path = projects_path(ctx);
    let mut config = read_json_or_empty(&path);
    if !has_key(&config, "projects", alias) {
        return (false, name_msg(ctx, "Unknown project: {name}", alias));
    }
    if is_issues_only(&config["projects"][alias]) {
        return (false, name_msg(ctx, "{name} tracks issues only, so it has no checkout to work in", alias));
    }
    config["projects"][alias]["use_worktree"] = json!(on);
    tryw!(save(ctx, &config, &path));
    (
        true,
        name_msg(ctx, if on { "Worktree on for {name}" } else { "Worktree off for {name}" }, alias),
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
                r#"{"instances":{"g":{"url":"https://g.example","token":"t"}},"projects":{"app":{"project_id":"g/app","instance":"g"}}}"#,
            )
            .unwrap();
            let state = UiState::explicit(dir.path(), &dir.path().join("loopx"), &home, dir.path(), "tok", None);
            Self { dir, state, req: UiRequest::get("/") }
        }
        /// "Run now" writes its arguments to `marker` instead of starting a run.
        fn with_echo_launcher(self, marker: &Path) -> Self {
            let script = format!("echo \"$@\" > {}", marker.display());
            let Sandbox { dir, state, req } = self;
            Sandbox { dir, state: state.with_run_cmd(&["sh", "-c", &script, "sh"]), req }
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
            track_project(&self.ctx(), "app", local_path, branch, true)
        }
        fn seed_projects(&self, value: Value) {
            write_json_atomic(&value, &projects_path(&self.ctx())).unwrap();
        }
        fn saved(&self) -> Value {
            read_json_or_empty(&projects_path(&self.ctx()))
        }
    }

    #[test]
    fn a_project_needs_a_target_branch_and_a_real_checkout() {
        let sb = Sandbox::new();
        let repo = sb.repo("repo", true);
        let both = "Give both a local checkout and a target branch for code changes, or leave both empty to track issues only";
        assert_eq!(sb.project(&repo, "").1, both);
        assert_eq!(sb.project("", "main").1, both);
        assert_eq!(sb.project("repo", "main").1, "Local checkout path must be an absolute path");
        assert!(sb.project(&sb.path("missing"), "main").1.contains("is not a directory"));
        std::fs::create_dir(sb.path("plain")).unwrap();
        assert!(sb.project(&sb.path("plain"), "main").1.contains("is not a git repository"));
        let bare = sb.repo("local-only", false);
        assert!(sb.project(&bare, "main").1.contains("has no origin remote"));
        assert!(sb.saved().get("projects").is_none(), "nothing saved after rejections");
        assert_eq!(sb.project(&repo, "main"), (true, "Tracking project app".to_string()));
        assert_eq!(sb.saved()["projects"]["app"]["local_path"], repo);
    }

    #[test]
    fn track_on_takes_id_and_instance_from_the_gitlab_alias() {
        let sb = Sandbox::new();
        let repo = sb.repo("repo", true);
        assert!(sb.project(&repo, "main").0);
        let app = &sb.saved()["projects"]["app"];
        assert_eq!(app["project_id"], "g/app");
        assert_eq!(app["instance"], "g");
        assert_eq!(app["target_branch"], "main");
        assert_eq!(app["use_worktree"], true);
        assert!(app.get("test_cmd").is_none());
    }

    #[test]
    fn track_preserves_command_hints_and_unknown_keys() {
        let sb = Sandbox::new();
        let repo = sb.repo("repo", true);
        sb.seed_projects(json!({"projects": {"app": {"project_id": "old", "local_path": repo, "target_branch": "dev", "lint_cmd": "bundle exec rubocop .", "x": 1}}}));
        assert_eq!(sb.project(&repo, "main"), (true, "Updated project app".to_string()));
        let app = &sb.saved()["projects"]["app"];
        assert_eq!(app["lint_cmd"], "bundle exec rubocop .");
        assert_eq!(app["x"], 1);
        assert_eq!(app["target_branch"], "main");
        assert_eq!(app["project_id"], "g/app");
    }

    #[test]
    fn track_off_then_on_keeps_no_stale_entry() {
        let sb = Sandbox::new();
        let repo = sb.repo("repo", true);
        assert!(sb.project(&repo, "dev").0);
        assert!(delete_tracked_project(&sb.ctx(), "app").0);
        assert!(sb.saved()["projects"].get("app").is_none());
        assert!(sb.project(&repo, "main").0);
        assert_eq!(sb.saved()["projects"]["app"]["target_branch"], "main");
    }

    #[test]
    fn track_without_a_checkout_is_issues_only() {
        let sb = Sandbox::new();
        assert_eq!(sb.project("", " "), (true, "Tracking project app".to_string()));
        let app = &sb.saved()["projects"]["app"];
        assert_eq!(app["project_id"], "g/app");
        assert_eq!(app["instance"], "g");
        assert!(app.get("local_path").is_none() && app.get("target_branch").is_none(), "{app}");
        assert!(app.get("use_worktree").is_none(), "{app}");
        assert!(!set_project_worktree(&sb.ctx(), "app", false).0, "no worktree without a checkout");

        let repo = sb.repo("repo", true);
        assert!(sb.project(&repo, "main").0);
        assert_eq!(sb.saved()["projects"]["app"]["local_path"], repo);
        sb.seed_projects(json!({"projects": {"app": {"project_id": "g/app", "local_path": repo, "target_branch": "main", "use_worktree": false, "test_cmd": "make test"}}}));
        assert_eq!(sb.project("", ""), (true, "Updated project app".to_string()));
        let app = &sb.saved()["projects"]["app"];
        assert!(app.get("local_path").is_none() && app.get("target_branch").is_none(), "{app}");
        assert!(app.get("use_worktree").is_none(), "{app}");
        assert_eq!(app["test_cmd"], "make test", "hints kept for when code changes come back");
    }

    #[test]
    fn issues_only_project_renders_without_checkout_or_worktree_switch() {
        let sb = Sandbox::new();
        assert!(sb.project("", "").0);
        let html = settings_fragment_html(&sb.ctx());
        assert!(html.contains("<h3>Projects</h3>"), "{html}");
        assert!(!html.contains("Project aliases"), "{html}");
        assert!(html.contains("Issues only"), "{html}");
        assert!(html.contains("action='/settings/loop-projects/app/delete'"), "{html}");
        assert!(!html.contains("action='/settings/loop-projects/app/worktree/"), "{html}");
        assert!(!html.contains("name='local_path' value='' placeholder='/abs/path/to/checkout' required"), "{html}");
    }

    #[test]
    fn worktree_switch_sets_flag() {
        let sb = Sandbox::new();
        let repo = sb.repo("repo", true);
        assert!(!set_project_worktree(&sb.ctx(), "app", false).0, "untracked alias");
        assert!(sb.project(&repo, "main").0);
        assert_eq!(set_project_worktree(&sb.ctx(), "app", false), (true, "Worktree off for app".to_string()));
        assert_eq!(sb.saved()["projects"]["app"]["use_worktree"], false);
        assert!(set_project_worktree(&sb.ctx(), "app", true).0);
        assert_eq!(sb.saved()["projects"]["app"]["use_worktree"], true);
    }

    #[test]
    fn track_requires_gitlab_alias() {
        let sb = Sandbox::new();
        let repo = sb.repo("repo", true);
        assert_eq!(track_project(&sb.ctx(), "nope", &repo, "main", true), (false, "Unknown project: nope".to_string()));
    }

    #[test]
    fn settings_render_has_track_switch_and_no_tracked_card() {
        let sb = Sandbox::new();
        let html = settings_fragment_html(&sb.ctx());
        assert!(html.contains("action='/settings/loop-projects/app/track'"), "{html}");
        assert!(html.contains("name='use_worktree'"), "{html}");
        assert!(!html.contains("Tracked Projects"), "{html}");
        assert!(!html.contains("install command"), "{html}");
        assert!(!html.contains("name='lint_cmd'"), "{html}");
        assert!(!html.contains("action='/settings/loop-projects'"), "{html}");
        assert!(html.contains("action='/settings/loop-config'"), "loop settings kept: {html}");
        let repo = sb.repo("repo", true);
        assert!(sb.project(&repo, "main").0);
        let html = settings_fragment_html(&sb.ctx());
        assert!(html.contains("action='/settings/loop-projects/app/worktree/off'"), "{html}");
        assert!(html.contains("action='/settings/loop-projects/app/delete'"), "{html}");
    }

    #[test]
    fn orphan_alias_is_listed_with_track_off_only() {
        let sb = Sandbox::new();
        sb.seed_projects(json!({"projects": {"ghost": {"project_id": "x/ghost", "local_path": "/g", "target_branch": "main"}}}));
        let html = settings_fragment_html(&sb.ctx());
        assert!(html.contains("no matching GitLab alias"), "{html}");
        assert!(html.contains("action='/settings/loop-projects/ghost/delete'"), "{html}");
        assert!(!html.contains("action='/settings/loop-projects/ghost/track'"), "{html}");
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

    #[test]
    fn a_failed_cli_call_shows_the_cli_message_and_the_way_to_switch() {
        let sb = Sandbox::new();
        let dir = Some(sb.state.events_dir.as_path());
        let warning = loopx_core::ai_cli_safety::warning("gemini", loopx_core::ai_cli_safety::CallSite::GitlabLoop).unwrap();
        events::emit("run.started", "run_1", events::EventFields { data: Some(json!({"trigger": "manual", "warning": warning})), ..Default::default() }, dir).unwrap();
        events::emit(
            "issue.agent_failed",
            "run_1",
            events::EventFields {
                project: Some("app".into()),
                issue_iid: Some(7),
                data: Some(json!({"reason": "nonzero_exit", "cli": "gemini", "stderr_excerpt": "Error: quota <exceeded>"})),
                ..Default::default()
            },
            dir,
        )
        .unwrap();
        let html = live_body(&sb.ctx(), None, false);
        assert!(html.contains("nonzero exit"), "{html}");
        assert!(html.contains("Error: quota &lt;exceeded&gt;"), "{html}");
        assert!(html.contains("href='/settings?tab=ai-cli'"), "{html}");
        assert!(html.contains("<div class='flash flash-warning'>"), "{html}");
        assert!(html.contains("full tool access"), "{html}");
    }

    #[test]
    fn live_row_shows_logged_checks() {
        let sb = Sandbox::new();
        let dir = Some(sb.state.events_dir.as_path());
        let fields = |data: Value| events::EventFields { project: Some("app".into()), issue_iid: Some(7), data: Some(data), ..Default::default() };
        events::emit("issue.queued", "run_1", fields(json!({"title": "Fix it"})), dir).unwrap();
        events::emit(
            "verification.commands",
            "run_1",
            fields(json!({"summary": "Rails app <script>", "lint": "", "test": "bundle exec rspec"})),
            dir,
        )
        .unwrap();
        let html = live_body(&sb.ctx(), None, false);
        assert!(html.contains("Rails app &lt;script&gt;"), "{html}");
        assert!(html.contains("<code>bundle exec rspec</code>"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn last_run_row_keeps_title_apart_from_meta_and_numbers_the_mr() {
        let sb = Sandbox::new();
        let dir = Some(sb.state.events_dir.as_path());
        let fields = |data: Value| events::EventFields { project: Some("app".into()), issue_iid: Some(7), data: Some(data), ..Default::default() };
        events::emit("issue.queued", "run_1", fields(json!({"title": "A very long title"})), dir).unwrap();
        events::emit(
            "issue.completed",
            "run_1",
            fields(json!({"action": "fix", "mr_url": "https://g.example/g/app/-/merge_requests/1489"})),
            dir,
        )
        .unwrap();
        let html = live_body(&sb.ctx(), None, false);
        let row = &html[html.find("<li class='result-row result-done'>").expect("a result row")..];
        let head_end = row.find("</div>").unwrap();
        assert!(!row[..head_end].contains("stat-sub"), "time and MR stay out of the title line: {row}");
        assert!(row.contains("<div class='result-meta'>"), "{row}");
        assert!(row.contains("class='result-mr'"), "{row}");
        assert!(row.contains("Merge request !1489"), "{row}");
        assert!(row.contains("#i-merge"), "{row}");
    }

    fn github_account(sb: &Sandbox) {
        std::fs::create_dir_all(&sb.state.home_dir).unwrap();
        std::fs::write(
            sb.state.home_dir.join("connectors.json"),
            r#"[{"id": "gh-work", "type": "github", "label": "Work GitHub", "api_url": "https://api.github.com", "username": "u"}]"#,
        )
        .unwrap();
    }

    fn add_github(sb: &Sandbox, fields: &[(&str, &str)]) -> (bool, String) {
        let get = |k: &str| fields.iter().find(|(n, _)| *n == k).map_or("", |(_, v)| *v);
        add_github_project(
            &sb.ctx(),
            &GithubProjectForm {
                alias: get("alias"),
                account: get("account"),
                repo: get("repo"),
                local_path: get("local_path"),
                target_branch: get("target_branch"),
                assignee: get("assignee"),
                use_worktree: true,
            },
        )
    }

    #[test]
    fn a_github_project_is_saved_with_its_code_host_and_checkout() {
        let sb = Sandbox::new();
        github_account(&sb);
        let checkout = sb.repo("hub", true);
        let (ok, msg) = add_github(
            &sb,
            &[("alias", "hub"), ("account", "gh-work"), ("repo", "octo/web"), ("local_path", &checkout),
              ("target_branch", "main"), ("assignee", "octocat")],
        );
        assert!(ok, "{msg}");
        let saved = &sb.saved()["projects"]["hub"];
        assert_eq!(saved["code_host"], json!({"account": "gh-work", "repo": "octo/web"}));
        assert_eq!(saved["local_path"], checkout);
        assert_eq!(saved["target_branch"], "main");
        assert_eq!(saved["use_worktree"], true);
        assert_eq!(saved["assignee_username"], "octocat");
        assert!(saved.get("project_id").is_none());
        // issues only: no checkout
        let (ok, msg) = add_github(&sb, &[("alias", "docs"), ("account", "gh-work"), ("repo", "octo/docs")]);
        assert!(ok, "{msg}");
        assert!(sb.saved()["projects"]["docs"].get("local_path").is_none());
    }

    #[test]
    fn a_github_project_needs_a_github_account_a_repo_and_a_free_alias() {
        let sb = Sandbox::new();
        github_account(&sb);
        sb.seed_projects(json!({"projects": {"app": {"project_id": "g/app", "local_path": "/x"}}}));
        for (fields, why) in [
            (vec![("alias", "hub"), ("account", "nope"), ("repo", "octo/web")], "unknown account"),
            (vec![("alias", "hub"), ("account", "gh-work"), ("repo", "octo")], "bad repo"),
            (vec![("alias", "hub"), ("account", "gh-work"), ("repo", "../x")], "bad repo"),
            (vec![("alias", "a b"), ("account", "gh-work"), ("repo", "octo/web")], "bad alias"),
            (vec![("alias", "app"), ("account", "gh-work"), ("repo", "octo/web")], "GitLab alias taken"),
            (vec![("alias", "hub"), ("account", "gh-work"), ("repo", "octo/web"), ("local_path", "/x")], "half a checkout"),
        ] {
            let (ok, msg) = add_github(&sb, &fields);
            assert!(!ok, "{why}: {msg}");
        }
        assert!(sb.saved()["projects"].get("hub").is_none());
        assert_eq!(sb.saved()["projects"]["app"]["project_id"], "g/app");
    }

    #[test]
    fn the_settings_list_github_projects_under_github_not_as_missing_gitlab_aliases() {
        let sb = Sandbox::new();
        github_account(&sb);
        sb.seed_projects(json!({"projects": {"hub": {"code_host": {"account": "gh-work", "repo": "octo/web"},
                                                      "local_path": "/h", "target_branch": "main"}}}));
        let html = settings_fragment_html(&sb.ctx());
        assert!(!html.contains("hub: no matching GitLab alias"), "{html}");
        assert!(html.contains("https://github.com/octo/web"), "{html}");
        assert!(html.contains("action='/settings/loop-projects/github'"), "{html}");
        assert!(html.contains("<option value='gh-work'"), "{html}");
    }

    #[test]
    fn the_last_run_names_a_github_pull_request_as_one() {
        let sb = Sandbox::new();
        let dir = Some(sb.state.events_dir.as_path());
        let fields = |data: Value| events::EventFields { project: Some("hub".into()), issue_iid: Some(4), data: Some(data), ..Default::default() };
        events::emit("issue.queued", "run_1", fields(json!({"title": "Crash"})), dir).unwrap();
        events::emit(
            "issue.completed",
            "run_1",
            fields(json!({"outcome": "mr_opened", "action": "fix", "mr_url": "https://github.com/o/r/pull/7"})),
            dir,
        )
        .unwrap();
        let html = live_body(&sb.ctx(), None, false);
        assert!(html.contains("Pull request #7"), "{html}");
    }

    #[test]
    fn a_github_pull_request_is_numbered_from_its_url() {
        assert_eq!(mr_number("https://github.com/o/r/pull/7"), Some("7"));
        assert_eq!(mr_number("https://github.com/o/r/pull/7/files"), Some("7"));
        assert_eq!(mr_number("https://github.com/o/r/issues/7"), None);
    }

    #[test]
    fn github_projects_show_their_pull_requests_with_hash_numbers_and_a_github_pill() {
        let sb = Sandbox::new();
        let issue = json!({"id": 4, "iid": 4, "title": "Crash on save", "web_url": "https://github.com/o/r/issues/4",
                           "labels": [], "assignees": [], "updated_at": "2026-10-02T00:00:00Z",
                           "provider": "github", "_assigned_to_me": true});
        let pr = json!({"id": 5, "iid": 5, "title": "Fix crash", "web_url": "https://github.com/o/r/pull/5",
                        "labels": [], "assignees": [], "updated_at": "2026-10-02T00:00:00Z", "provider": "github"});
        let gitlab_mr = json!({"id": 9, "iid": 9, "title": "GitLab MR", "web_url": "https://g.example/g/app/-/merge_requests/9",
                               "labels": [], "assignees": [], "updated_at": "2026-10-01T00:00:00Z"});
        let state = |issues: Vec<Value>, mrs: Vec<Value>| AliasState { issues, mrs, issues_error: None, mrs_error: None };
        let html = live_fragment_html(
            &sb.ctx(),
            &[("hub".into(), state(vec![issue], vec![pr])), ("app".into(), state(vec![], vec![gitlab_mr]))],
        );
        assert!(html.contains(">#5 Fix crash</a>"), "{html}");
        assert!(html.contains(">!9 GitLab MR</a>"), "{html}");
        assert!(html.contains("<h3>hub <span class='pill pill-grey'>GitHub</span></h3>"), "{html}");
        assert!(html.contains("<h3>app</h3>"), "{html}");
        assert!(html.contains("Crash on save"), "{html}");
    }

    #[test]
    fn mr_number_comes_from_the_url() {
        assert_eq!(mr_number("https://g.example/g/app/-/merge_requests/1489"), Some("1489"));
        assert_eq!(mr_number("https://g.example/g/app/-/merge_requests/12/diffs"), Some("12"));
        assert_eq!(mr_number("https://g.example/g/app/-/issues/3"), None);
    }

    #[test]
    fn a_failure_without_a_cli_gets_no_cli_hint() {
        let sb = Sandbox::new();
        let dir = Some(sb.state.events_dir.as_path());
        events::emit("run.started", "run_1", events::EventFields { data: Some(json!({"trigger": "manual"})), ..Default::default() }, dir).unwrap();
        events::emit(
            "issue.failed",
            "run_1",
            events::EventFields { project: Some("app".into()), issue_iid: Some(7), data: Some(json!({"error": "worktree failed"})), ..Default::default() },
            dir,
        )
        .unwrap();
        let html = live_body(&sb.ctx(), None, false);
        assert!(html.contains("worktree failed"), "{html}");
        assert!(!html.contains("tab=ai-cli"), "{html}");
        assert!(!html.contains("flash-warning"), "{html}");
    }

    #[test]
    fn a_later_failure_without_a_cli_replaces_the_cli_message() {
        let sb = Sandbox::new();
        let dir = Some(sb.state.events_dir.as_path());
        let fields = |data: Value| events::EventFields { project: Some("app".into()), issue_iid: Some(7), data: Some(data), ..Default::default() };
        events::emit("issue.agent_failed", "run_1", fields(json!({"reason": "nonzero_exit", "cli": "gemini", "stderr_excerpt": "quota gone"})), dir).unwrap();
        events::emit("issue.failed", "run_1", fields(json!({"error": "push rejected"})), dir).unwrap();
        let html = live_body(&sb.ctx(), None, false);
        assert!(html.contains("push rejected"), "{html}");
        assert!(!html.contains("quota gone"), "{html}");
        assert!(!html.contains("tab=ai-cli"), "{html}");
    }
    fn post(state: &UiState, path: &str) -> crate::ui::routes::UiResponse {
        let req = UiRequest::new(axum::http::Method::POST, path)
            .with_header("content-type", "application/x-www-form-urlencoded")
            .with_body("csrf_token=tok");
        Registry::with_pages().dispatch(state, &req).expect("route")
    }

    fn wait_for(path: &Path) -> String {
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(path) {
                if !text.is_empty() {
                    return text;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("{} never written", path.display());
    }

    #[test]
    fn a_tracked_issue_gets_a_run_now_button_and_the_backlog_does_not() {
        let sb = Sandbox::new();
        let issue = json!({"iid": 7, "title": "Fix it", "web_url": "https://g.example/app/-/issues/7"});
        let row = gitlab_item(&sb.ctx(), &issue, "#", Some("app"), true);
        assert!(
            row.contains("<form method='post' action='/gitlab/issues/app/7/run' class='daemon-action-form issue-run-now'>"),
            "{row}"
        );
        assert!(row.contains("name='csrf_token'"), "{row}");
        assert!(row.contains("Run now"), "{row}");
        let backlog = gitlab_item(&sb.ctx(), &issue, "#", None, false);
        assert!(!backlog.contains("issue-run-now"), "{backlog}");
    }

    #[test]
    fn run_now_on_an_issue_starts_the_loop_scoped_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        let sb = Sandbox::new().with_echo_launcher(&marker);
        let state = &sb.state;
        let resp = post(state, "/gitlab/issues/app/7/run");
        assert_eq!(resp.status, 303);
        let location = resp.header("Location").unwrap();
        assert!(location.starts_with("/loops/gitlab-loop?view=live&flash="), "{location}");
        assert!(location.ends_with("&ok=1"), "{location}");
        assert_eq!(wait_for(&marker).trim(), "gitlab-loop app 7");
    }

    #[test]
    fn run_now_on_an_issue_is_refused_when_untracked_or_a_run_is_going() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        let sb = Sandbox::new().with_echo_launcher(&marker);
        let state = &sb.state;
        issue_tracking_config::set_issue_enabled("app", 7, false, &state.home_dir.join("issue_tracking.json")).unwrap();
        let resp = post(state, "/gitlab/issues/app/7/run");
        assert!(resp.header("Location").unwrap().ends_with("&ok=0"));
        status::write_status(&state.status_path(), "running", &[]).unwrap();
        let resp = post(state, "/gitlab/issues/app/8/run");
        assert!(resp.header("Location").unwrap().ends_with("&ok=0"));
        assert_eq!(post(state, "/gitlab/issues/app/x/run").status, 404);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!marker.exists());
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false", "-C"])
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// Tracks `app` with a clone of a bare origin and worktrees for issues 7 and 8.
    fn sandbox_with_worktrees() -> (Sandbox, String) {
        let sb = Sandbox::new();
        let origin = sb.dir.path().join("origin.git");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        git(sb.dir.path(), &["clone", "-q", &origin.display().to_string(), "app"]);
        let local = sb.dir.path().join("app");
        git(&local, &["checkout", "-q", "-b", "main"]);
        std::fs::write(local.join("a.txt"), "1").unwrap();
        git(&local, &["add", "."]);
        git(&local, &["commit", "-qm", "init"]);
        git(&local, &["push", "-q", "origin", "main"]);
        let local = local.display().to_string();
        let root = sb.path("wt");
        sb.seed_projects(json!({"gitlab_instance": "g", "assignee_username": "me", "worktree_root": root,
            "projects": {"app": {"project_id": "g/app", "local_path": local, "target_branch": "main"}}}));
        let wt7 = worktree::new_worktree(&local, "main", "7", &root).unwrap();
        worktree::new_worktree(&local, "main", "8", &root).unwrap();
        std::fs::write(Path::new(&wt7).join("wip.txt"), "x").unwrap();
        (sb, wt7)
    }

    #[test]
    fn the_worktrees_view_lists_each_issue_branch_with_a_remove_button() {
        let (sb, wt7) = sandbox_with_worktrees();
        let issue = json!({"iid": 7, "title": "Fix the <login>", "web_url": "https://g.example/g/app/-/issues/7"});
        crate::ui::live_cache::write(&sb.state, TASKS_CACHE, tasks_cache_json(&[("app".into(), state(vec![issue], None))]));
        let body = worktrees_body(&sb.ctx(), None, true);
        assert!(body.contains("Fix the &lt;login&gt;"), "{body}");
        assert!(body.contains("href='https://g.example/g/app/-/issues/7'"), "{body}");
        // Not in the cached Tasks list: linked from the instance URL instead.
        assert!(body.contains("href='https://g.example/g/app/-/issues/8'"), "{body}");
        assert!(body.contains("<code>loop/issue-7</code>") && body.contains("<code>loop/issue-8</code>"), "{body}");
        assert!(body.contains(&pyfmt::escape(&wt7)) || body.contains("app-issue-7"), "{body}");
        assert!(body.contains("Uncommitted changes"), "{body}");
        assert!(body.contains("<form method='post' action='/gitlab/worktrees/app/7/remove'"), "{body}");
        assert!(body.contains("name='csrf_token'") && body.contains("data-confirm="), "{body}");
    }

    #[test]
    fn the_worktrees_view_opens_with_the_wt_shell_snippet() {
        let sb = Sandbox::new();
        let body = worktrees_body(&sb.ctx(), None, true);
        let snippet = body.find("wt() {").expect(&body);
        assert!(snippet < body.find("No issue branches in progress.").unwrap(), "{body}");
        assert!(body.contains("$0==&quot;branch &quot;b"), "{body}");
        assert!(body.contains("return 1"), "{body}");
        assert!(body.contains("<pre class='log' id='wt-snippet'>"), "{body}");
        assert!(body.contains("data-copy-target='wt-snippet' data-copied-label='Copied'"), "{body}");
        assert!(body.contains("href='#i-content_copy'"), "{body}");
    }

    #[test]
    fn the_worktrees_view_says_when_there_is_nothing_in_progress() {
        let sb = Sandbox::new();
        let body = worktrees_body(&sb.ctx(), None, true);
        assert!(body.contains("No issue branches in progress."), "{body}");
        assert_eq!(builtin_loop_page("gitlab-loop").unwrap().views[2].key, "worktrees");
    }

    #[test]
    fn remove_deletes_the_worktree_and_branch_unless_a_run_is_going() {
        let (sb, wt7) = sandbox_with_worktrees();
        let state = &sb.state;
        status::write_status(&state.status_path(), "running", &[]).unwrap();
        let resp = post(state, "/gitlab/worktrees/app/7/remove");
        assert!(resp.header("Location").unwrap().ends_with("&ok=0"));
        assert!(Path::new(&wt7).exists());

        status::write_status(&state.status_path(), "idle", &[]).unwrap();
        let resp = post(state, "/gitlab/worktrees/app/7/remove");
        assert_eq!(resp.status, 303);
        let location = resp.header("Location").unwrap();
        assert!(location.starts_with("/loops/gitlab-loop?view=worktrees&flash="), "{location}");
        assert!(location.ends_with("&ok=1"), "{location}");
        assert!(!Path::new(&wt7).exists());
        let body = worktrees_body(&sb.ctx(), None, true);
        assert!(!body.contains("<code>loop/issue-7</code>") && body.contains("<code>loop/issue-8</code>"), "{body}");

        assert!(post(state, "/gitlab/worktrees/nope/8/remove").header("Location").unwrap().ends_with("&ok=0"));
        assert!(post(state, "/gitlab/worktrees/app/7/remove").header("Location").unwrap().ends_with("&ok=0"));
        assert_eq!(post(state, "/gitlab/worktrees/app/x/remove").status, 404);
    }

    fn state(issues: Vec<Value>, issues_error: Option<&str>) -> AliasState {
        AliasState { issues, mrs: vec![], issues_error: issues_error.map(str::to_string), mrs_error: None }
    }

    #[test]
    fn tasks_paint_the_cached_list_at_once_and_spin_only_without_one() {
        let sb = Sandbox::new();
        let body = tasks_body(&sb.ctx());
        assert!(body.contains("data-lazy-load='/gitlab/live'") && body.contains("Loading live GitLab data"));
        crate::ui::live_cache::write(&sb.state, TASKS_CACHE, tasks_cache_json(&[("app".into(), state(vec![], None))]));
        let body = tasks_body(&sb.ctx());
        assert!(body.contains("data-lazy-load='/gitlab/live'"));
        assert!(!body.contains("Loading live GitLab data"), "{body}");
        assert!(body.contains("refreshing") && body.contains("Nothing assigned to you right now."), "{body}");
    }

    #[test]
    fn the_tasks_cache_round_trips_and_a_failed_fetch_keeps_the_last_good_data() {
        let old = vec![("app".to_string(), state(vec![json!({"iid": 1})], None))];
        let back = tasks_cache_from(&tasks_cache_json(&old)).unwrap();
        assert_eq!(back[0].0, "app");
        assert_eq!(back[0].1.issues, vec![json!({"iid": 1})]);
        let fresh = vec![("app".to_string(), state(vec![], Some("timeout")))];
        let merged = merge_tasks_cache(&old, fresh);
        assert_eq!(merged[0].1.issues, vec![json!({"iid": 1})]);
        assert!(merged[0].1.issues_error.is_none());
        assert!(tasks_cache_from(&json!({"not": "a list"})).is_none());
    }
}
