//! The Runs hub: `/runs` (views `loop-runs`, `history`, `logs`), the run detail
//! and history file pages, and the run-control POST routes (Run now, Stop,
//! delete a history file). Ports `_loop_runs_body`, `_history_body`,
//! `_logs_body`, `render_loop_run_detail_page`, the `/history/<name>` and
//! `/topic-monitor/history/<name>` routes and section 6.2 of
//! `docs/contract/http-api.md`.

use crate::ui::badge::default_badge;
use crate::ui::hub::{hub_by_key, render_hub_page, Hub};
use crate::ui::markdown::render_markdown;
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::shell::{render_shell, ShellOpts};
use crate::ui::template::{Scope, Template};
use crate::ui::{consts, pyfmt};
use loopx_core::ledger::{self, RunQuery, RunRecord};
use loopx_core::loops_config;
use loopx_core::run_control::{self, Launcher, StopOutcome, TriggerOutcome};
use loopx_core::run_history::{self, HistoryDelete, LogEntry};
use loopx_core::{ai_cli_config, loop_serialize};
use serde_json::Value;
use std::path::PathBuf;

pub fn register(r: &mut Registry) {
    r.get("/runs", runs_hub);
    r.get("/loop-runs/*rest", loop_run_detail);
    r.get("/history/*rest", history_page);
    r.get("/topic-monitor/history/*rest", topic_history_page);

    r.post("/run-now", run_now);
    r.post("/gitlab/stop", stop_gitlab);
    r.post("/topic-monitor/stop", stop_topic);
    r.post("/topic-monitor/run-now", run_topic);
    r.post("/inbox/run-now", run_inbox);
    r.post("/loops/:name/run-now", run_loop);
    // Python only checks the token once the path ends in `/delete`; anything
    // else under these prefixes is a plain 404.
    r.post_unchecked("/history/*rest", delete_history);
    r.post_unchecked("/topic-monitor/history/*rest", delete_topic_history);
}

// ---- paths ----------------------------------------------------------------

fn loop_runs_dir(ctx: &UiCtx) -> PathBuf {
    ctx.state.repo_root.join("outputs").join("loop-runs")
}

fn history_dir(ctx: &UiCtx) -> PathBuf {
    ctx.state.repo_root.join("outputs").join("history")
}

fn topic_history_dir(ctx: &UiCtx) -> PathBuf {
    ctx.state
        .repo_root
        .join("outputs")
        .join("topic-monitor")
        .join("history")
}

fn topic_status_path(ctx: &UiCtx) -> PathBuf {
    ctx.state
        .repo_root
        .join("outputs")
        .join("topic-monitor")
        .join("status.json")
}

// ---- small helpers --------------------------------------------------------

fn s_of(v: &Value, key: &str) -> String {
    v.get(key).map(loopx_core::pydumps::py_str).unwrap_or_default()
}

fn render(name: &str, scope: &Scope) -> String {
    Template::named(name)
        .render(scope)
        .unwrap_or_else(|e| panic!("{e}"))
}

fn esc_t(ctx: &UiCtx, text: &str) -> String {
    pyfmt::escape(&ctx.t(text))
}

/// `_loop_run_state_pill_class`.
fn state_pill_class(final_state: &str) -> &'static str {
    match final_state {
        "completed" => "pill-green",
        "running" => "pill-blue",
        "failed" | "escalated" => "pill-red",
        _ => "pill-grey",
    }
}

/// Python's `f"{value:.{prec}g}"`.
fn fmt_g(value: f64, prec: usize) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    if !value.is_finite() {
        return if value.is_nan() {
            "nan".into()
        } else if value > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        };
    }
    let sci = format!("{:.*e}", prec - 1, value);
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let strip = |s: &str| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s.to_string()
        }
    };
    if exp < -4 || exp >= prec as i32 {
        format!(
            "{}e{}{:02}",
            strip(mantissa),
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        )
    } else {
        let decimals = (prec as i32 - 1 - exp).max(0) as usize;
        strip(&format!("{value:.decimals$}"))
    }
}

// ---- ledger-backed pieces -------------------------------------------------

fn ledger_runs(ctx: &UiCtx, days: Option<i64>) -> Vec<RunRecord> {
    ledger::iter_runs_at(
        &RunQuery {
            days,
            events_dir: Some(&ctx.state.events_dir),
            ..Default::default()
        },
        ctx.state.now(),
    )
}

/// `_incomplete_runs_html`: runs that started in the last 30 days but never
/// recorded a result or a terminal event.
fn incomplete_runs_html(ctx: &UiCtx) -> String {
    const DAYS: i64 = 30;
    let runs: Vec<RunRecord> = ledger_runs(ctx, Some(DAYS))
        .into_iter()
        .filter(|r| !r.complete)
        .collect();
    if runs.is_empty() {
        return String::new();
    }
    let rows: String = runs
        .iter()
        .map(|r| {
            let name = r
                .loop_name
                .as_deref()
                .filter(|n| !n.is_empty())
                .map_or_else(|| ctx.t("Unknown loop"), str::to_string);
            let run_id = r.run_id.clone().unwrap_or_default();
            let when = r
                .started_at
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("?");
            format!(
                "<div class='history-entry'><div class='history-entry-header'>\
                 <strong>{}</strong>\
                 <div class='pill-row'><span class='pill pill-grey'>{}</span></div>\
                 </div>\
                 <p class='history-entry-overview'>run_id: {} &middot; {}</p></div>",
                pyfmt::escape(&name),
                esc_t(ctx, "incomplete"),
                pyfmt::escape(&run_id),
                pyfmt::escape(&ctx.tf("started {when}", &[("when", when)])),
            )
        })
        .collect();
    let note = ctx.tf(
        "Started but never recorded a result in the last {days} days: crashed, killed, or still running.",
        &[("days", &DAYS.to_string())],
    );
    format!(
        "<section class=\"card\"><div class=\"section-header\">{}\
         <h2>{}</h2></div><p>{}</p>{rows}</section>",
        consts::ds("_SECTION_ICON_LOOP_RUNS"),
        esc_t(ctx, "Incomplete runs"),
        pyfmt::escape(&note),
    )
}

/// `_loop_runs_overview_html`.
fn loop_runs_overview_html(ctx: &UiCtx) -> String {
    let summary = loop_serialize::summarize_results(Some(&ledger_runs(ctx, None)));
    let pct = |rate: Option<f64>| rate.map_or_else(|| "\u{2014}".to_string(), |r| format!("{:.0}%", r * 100.0));
    let cost = |v: Option<f64>| v.map_or_else(|| "\u{2014}".to_string(), |v| format!("${v:.2}"));
    let score = |v: Option<f64>| v.map_or_else(|| "\u{2014}".to_string(), |v| fmt_g(v, 4));
    let tiles = [
        ("history", "Total Runs", summary.total_runs.to_string()),
        ("check_circle", "Success Rate", pct(summary.success_rate)),
        ("warning", "Escalation Rate", pct(summary.escalation_rate)),
        ("bolt", "Average Cost", cost(summary.average_cost_usd)),
        ("speed", "Loop Efficiency Score", score(summary.efficiency_score)),
    ];
    let tiles_html: String = tiles
        .iter()
        .map(|(icon, label, value)| {
            let mut scope = Scope::new(ctx.lang);
            scope
                .var("icon", *icon)
                .var("value", value.as_str())
                .set("html.escape(label)", esc_t(ctx, label));
            render("runs_overview_tile", &scope)
        })
        .collect();
    let mut scope = Scope::new(ctx.lang);
    scope.var("tiles_html", tiles_html);
    render("runs_overview", &scope)
}

// ---- Loop Runs ------------------------------------------------------------

fn read_runs(ctx: &UiCtx) -> Vec<Value> {
    let mut paths = loop_serialize::list_results(Some(&loop_runs_dir(ctx)));
    paths.reverse();
    paths
        .iter()
        .filter_map(|p| loop_serialize::read_result(p).ok())
        .collect()
}

fn loop_runs_body(ctx: &UiCtx) -> String {
    let runs = read_runs(ctx);
    let mut scope = Scope::new(ctx.lang);
    if runs.is_empty() {
        scope.set("_incomplete_runs_html()", incomplete_runs_html(ctx));
        return render("runs_loop_runs_empty", &scope);
    }
    let rows: String = runs
        .iter()
        .map(|data| {
            let final_state = s_of(data, "final_state");
            let count = data
                .get("iterations")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let mut row = Scope::new(ctx.lang);
            row.var("run_href", pyfmt::quote(&s_of(data, "run_id"), "/"))
                .var("pill_class", state_pill_class(&final_state))
                .set("html.escape(data['definition_name'])", pyfmt::escape(&s_of(data, "definition_name")))
                .set("html.escape(data['final_state'])", pyfmt::escape(&final_state))
                .set("html.escape(data['run_id'])", pyfmt::escape(&s_of(data, "run_id")))
                .set(
                    "html.escape(_t('{count} iteration(s)', count=len(data['iterations'])))",
                    pyfmt::escape(&ctx.tf("{count} iteration(s)", &[("count", &count.to_string())])),
                )
                .set("html.escape(data['stop_reason'])", pyfmt::escape(&s_of(data, "stop_reason")));
            render("runs_loop_run_row", &row)
        })
        .collect();
    scope
        .set("_loop_runs_overview_html(summary)", loop_runs_overview_html(ctx))
        .set("''.join(rows)", rows)
        .set("_incomplete_runs_html()", incomplete_runs_html(ctx));
    render("runs_loop_runs_body", &scope)
}

// ---- Run History ----------------------------------------------------------

fn history_entry_html(
    ctx: &UiCtx,
    name: &str,
    detail_href: &str,
    delete_href: &str,
    overview: &str,
    tags: &[String],
) -> String {
    let tags_html: String = tags
        .iter()
        .map(|t| format!("<span class='pill pill-grey'>{}</span>", pyfmt::escape(t)))
        .collect();
    let mut scope = Scope::new(ctx.lang);
    scope
        .var("detail_href", detail_href)
        .var("safe_name", pyfmt::escape(name))
        .var("tags_html", tags_html)
        .var("delete_href", delete_href)
        .var("csrf_input", ctx.csrf_input())
        .var(
            "delete_confirm",
            pyfmt::escape(&ctx.tf("Delete {name}? This can't be undone.", &[("name", name)])),
        )
        .var("name", name)
        .var("overview", overview);
    render("runs_history_entry", &scope)
}

fn history_body(ctx: &UiCtx) -> String {
    let none_yet = format!("<p>{}</p>", esc_t(ctx, "(none yet)"));

    let hist = history_dir(ctx);
    let gitlab: String = run_history::list_run_history(&hist)
        .iter()
        .map(|name| {
            let content = run_history::read_history_file(name, &hist).unwrap_or_default();
            let safe = pyfmt::quote(name, "/");
            history_entry_html(
                ctx,
                name,
                &format!("/history/{safe}"),
                &format!("/history/{safe}/delete"),
                &run_history::extract_history_overview(&content, 240),
                &run_history::gitlab_history_tags(&content),
            )
        })
        .collect();

    let topic_dir = topic_history_dir(ctx);
    let topic: String = run_history::list_topic_history(None, &topic_dir)
        .iter()
        .map(|name| {
            let content = run_history::read_history_file(name, &topic_dir).unwrap_or_default();
            let safe = pyfmt::quote(name, "/");
            history_entry_html(
                ctx,
                name,
                &format!("/topic-monitor/history/{safe}"),
                &format!("/topic-monitor/history/{safe}/delete"),
                &run_history::extract_history_overview(&content, 240),
                &run_history::topic_history_tags(name, &content),
            )
        })
        .collect();

    let mut scope = Scope::new(ctx.lang);
    scope
        .var("gitlab_items", if gitlab.is_empty() { none_yet.clone() } else { gitlab })
        .var("topic_items", if topic.is_empty() { none_yet } else { topic });
    render("runs_history_body", &scope)
}

// ---- Logs -----------------------------------------------------------------

fn logs_body(ctx: &UiCtx) -> String {
    let log_path = run_history::unified_log_path(&ctx.state.repo_root);
    let tail = run_history::read_unified_log_tail(500, &log_path).filter(|t| !t.is_empty());
    let log_html = match tail {
        Some(tail) => {
            let prefixes = crate::ui::gitlab::issue_url_prefixes(ctx.state);
            let mut entries = run_history::parse_unified_log_entries(&tail);
            entries.reverse();
            let inner: String = entries
                .iter()
                .map(|e| log_entry_html(e, &prefixes))
                .collect();
            format!("<div class='log-entries'>{inner}</div>")
        }
        None => format!("<p>{}</p>", esc_t(ctx, "No log entries yet.")),
    };
    let selected = ai_cli_config::get_selected_cli(&ctx.state.ai_cli_config_path());
    let cli_name = consts::value("dashboard_server", "_AI_CLI_DISPLAY_NAMES")
        .and_then(|v| v.get(&selected))
        .and_then(Value::as_str)
        .unwrap_or("Claude Code");
    let mut scope = Scope::new(ctx.lang);
    scope
        .var("log_html", log_html)
        .var("ai_cli_name", cli_name);
    render("runs_logs_body", &scope)
}

/// `_log_entry_html`, with the GitLab prefixes already computed once per page
/// (Python recomputes them per entry from the same files; the result is equal).
fn log_entry_html(entry: &LogEntry, prefixes: &crate::ui::markdown::GitlabPrefixes) -> String {
    let header = match (&entry.source, &entry.detail, &entry.timestamp) {
        (Some(source), Some(detail), Some(ts)) => format!(
            "<span class='log-entry-source'>{}</span>\
             <span class='log-entry-detail'>{}</span>\
             <span class='log-entry-time'>{}</span>",
            pyfmt::escape(source),
            pyfmt::escape(detail),
            pyfmt::escape(ts),
        ),
        _ => "<span class='log-entry-meta'>(continued from an earlier entry)</span>".to_string(),
    };
    let body = if entry.body.is_empty() {
        String::new()
    } else {
        format!(
            "<div class='log-entry-body markdown'>{}</div>",
            render_markdown(&entry.body, prefixes)
        )
    };
    format!("<div class='log-entry'><div class='log-entry-header'>{header}</div>{body}</div>")
}

// ---- GET routes -----------------------------------------------------------

fn runs_hub(ctx: &UiCtx) -> UiResponse {
    let hub: Hub = hub_by_key("runs").expect("runs hub");
    let view = ctx.query().first("view").map(str::to_string);
    let current = crate::ui::hub::resolve_view(&hub, view.as_deref()).key.clone();
    let body = match current.as_str() {
        "history" => history_body(ctx),
        "logs" => logs_body(ctx),
        _ => loop_runs_body(ctx),
    };
    UiResponse::html(render_hub_page(
        ctx,
        &hub,
        view.as_deref(),
        &body,
        &default_badge(ctx),
    ))
}

/// The raw (still percent-encoded) remainder of the request path after `prefix`,
/// as Python's `split.path[len(prefix):]` sees it.
fn raw_rest<'a>(ctx: &'a UiCtx, prefix: &str) -> &'a str {
    ctx.req.path.strip_prefix(prefix).unwrap_or("")
}

fn loop_run_detail(ctx: &UiCtx) -> UiResponse {
    let run_id = raw_rest(ctx, "/loop-runs/");
    let data = loop_serialize::list_results(Some(&loop_runs_dir(ctx)))
        .iter()
        .filter_map(|p| loop_serialize::read_result(p).ok())
        .find(|d| s_of(d, "run_id") == run_id);
    let Some(data) = data else {
        return UiResponse::not_found(ctx.lang);
    };
    let final_state = s_of(&data, "final_state");
    let iterations: Vec<Value> = data
        .get("iterations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let blocks: String = iterations
        .iter()
        .map(|iteration| {
            let verifiers: Vec<Value> = iteration
                .get("verification_results")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut items: String = verifiers
                .iter()
                .map(|v| {
                    let passed = loopx_core::loop_serialize::truthy(&loop_serialize::effective_passed(v));
                    format!(
                        "<li>{} {}</li>",
                        if passed { "\u{2713}" } else { "\u{2717}" },
                        pyfmt::escape(&s_of(v, "name"))
                    )
                })
                .collect();
            if items.is_empty() {
                items = format!("<li>{}</li>", esc_t(ctx, "(no verifiers configured)"));
            }
            let mut scope = Scope::new(ctx.lang);
            scope
                .set(
                    "html.escape(_t('Iteration {number}', number=iteration['iteration']))",
                    pyfmt::escape(&ctx.tf("Iteration {number}", &[("number", &s_of(iteration, "iteration"))])),
                )
                .set("html.escape(iteration['state'])", pyfmt::escape(&s_of(iteration, "state")))
                .var("verifier_items", items);
            render("runs_detail_iteration", &scope)
        })
        .collect();
    let mut scope = Scope::new(ctx.lang);
    scope
        .set("html.escape(data['definition_name'])", pyfmt::escape(&s_of(&data, "definition_name")))
        .set("html.escape(data['run_id'])", pyfmt::escape(&s_of(&data, "run_id")))
        .var("pill_class", state_pill_class(&final_state))
        .set("html.escape(data['final_state'])", pyfmt::escape(&final_state))
        .set(
            "html.escape(_t('Stop reason: {reason}', reason=data['stop_reason']))",
            pyfmt::escape(&ctx.tf("Stop reason: {reason}", &[("reason", &s_of(&data, "stop_reason"))])),
        )
        .set("''.join(iteration_blocks)", blocks);
    let body = render("runs_detail_body", &scope);
    UiResponse::html(render_shell(
        ctx,
        &format!("{} \u{b7} Loop X Engineering", s_of(&data, "definition_name")),
        "loop_runs",
        &default_badge(ctx),
        &body,
        ShellOpts::default(),
    ))
}

fn history_file_page(
    ctx: &UiCtx,
    prefix: &str,
    dir: PathBuf,
    active_page: &str,
) -> UiResponse {
    let name = raw_rest(ctx, prefix);
    let Some(content) = run_history::read_history_file(name, &dir) else {
        return UiResponse::not_found(ctx.lang);
    };
    let title = run_history::path_name(name);
    let prefixes = crate::ui::gitlab::issue_url_prefixes(ctx.state);
    let body = format!(
        "<h1>{}</h1><div class='grid'><div class='card'><div class='markdown'>{}</div></div></div>",
        pyfmt::escape(&title),
        render_markdown(&content, &prefixes)
    );
    UiResponse::html(render_shell(
        ctx,
        &title,
        active_page,
        &default_badge(ctx),
        &body,
        ShellOpts::default(),
    ))
}

fn history_page(ctx: &UiCtx) -> UiResponse {
    history_file_page(ctx, "/history/", history_dir(ctx), "history")
}

fn topic_history_page(ctx: &UiCtx) -> UiResponse {
    history_file_page(ctx, "/topic-monitor/history/", topic_history_dir(ctx), "topic_monitor")
}

// ---- POST routes ----------------------------------------------------------

fn launcher<'a>(ctx: &'a UiCtx, fallback: &'a PathBuf) -> Launcher<'a> {
    Launcher {
        cmd: &ctx.state.run_cmd,
        cwd: &ctx.state.repo_root,
        fallback_path: fallback,
    }
}

fn trigger_message(ctx: &UiCtx, outcome: TriggerOutcome, started: &str) -> (bool, String) {
    match outcome {
        TriggerOutcome::AlreadyRunning => (false, ctx.t("A run is already in progress")),
        TriggerOutcome::LauncherMissing(path) => (
            false,
            ctx.tf("run-loop-now.sh not found at {path}", &[("path", &path)]),
        ),
        TriggerOutcome::Started => (true, ctx.t(started)),
    }
}

fn stop_message(ctx: &UiCtx, outcome: StopOutcome) -> (bool, String) {
    match outcome {
        StopOutcome::NoRun => (false, ctx.t("No run is currently in progress")),
        StopOutcome::ClearedStale => (
            true,
            ctx.t("Cleared a stale running state (no active process found)"),
        ),
        StopOutcome::Stopped => (true, ctx.t("Run stopped")),
    }
}

const STARTED: &str = "Run started - check back here for progress";

fn run_script_path(ctx: &UiCtx) -> PathBuf {
    ctx.state.repo_root.join("run-loop-now.sh")
}

fn run_now(ctx: &UiCtx) -> UiResponse {
    let fallback = run_script_path(ctx);
    let outcome = run_control::trigger_manual_run(
        &ctx.state.status_path(),
        "gitlab-loop",
        &launcher(ctx, &fallback),
    );
    let (ok, message) = trigger_message(ctx, outcome, STARTED);
    UiResponse::flash_redirect(ok, &message, "/")
}

fn stop_gitlab(ctx: &UiCtx) -> UiResponse {
    let (ok, message) = stop_message(ctx, run_control::stop_gitlab_loop(&ctx.state.status_path()));
    UiResponse::flash_redirect(ok, &message, "/?view=activity")
}

fn stop_topic(ctx: &UiCtx) -> UiResponse {
    let outcome = run_control::stop_topic_loop(
        &ctx.state.status_path_for_loop("topic-loop"),
        &topic_status_path(ctx),
    );
    let (ok, message) = stop_message(ctx, outcome);
    UiResponse::flash_redirect(ok, &message, "/?view=activity")
}

fn run_topic(ctx: &UiCtx) -> UiResponse {
    let fallback = run_script_path(ctx);
    let outcome = run_control::trigger_topic_monitor_run(
        &topic_status_path(ctx),
        "topic-loop",
        &launcher(ctx, &fallback),
    );
    let (ok, message) = trigger_message(ctx, outcome, STARTED);
    UiResponse::flash_redirect(ok, &message, "/loops/topic-loop")
}

fn run_inbox(ctx: &UiCtx) -> UiResponse {
    let fallback = run_script_path(ctx);
    let outcome = run_control::trigger_inbox_triage_run(
        &ctx.state.status_path_for_loop("inbox-triage-loop"),
        "inbox-triage-loop",
        &launcher(ctx, &fallback),
    );
    let (ok, message) = trigger_message(ctx, outcome, "Inbox triage started - check back here for results");
    UiResponse::flash_redirect(ok, &message, "/loops/inbox-triage-loop")
}

/// `POST /loops/<name>/run-now`: 404 for the built-in hub loops and for any
/// name that is not registered.
fn run_loop(ctx: &UiCtx) -> UiResponse {
    let name = ctx.param("name");
    let builtin = crate::ui::hub::builtin_loop_page(name).is_some();
    let registered = loops_config::get_loop(
        name,
        &ctx.state.loops_config_path(),
        Some(&ctx.state.loops_template_path()),
    )
    .is_ok();
    if builtin || !registered {
        return UiResponse::not_found(ctx.lang);
    }
    let fallback = run_script_path(ctx);
    let outcome = run_control::trigger_manual_run(
        &ctx.state.status_path_for_loop(name),
        name,
        &launcher(ctx, &fallback),
    );
    let (ok, message) = trigger_message(ctx, outcome, STARTED);
    UiResponse::flash_redirect(ok, &message, &format!("/loops/{}", pyfmt::quote(name, "/")))
}

fn delete_in(ctx: &UiCtx, prefix: &str, dir: PathBuf) -> UiResponse {
    let Some(inner) = ctx
        .req
        .path
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix("/delete"))
    else {
        return UiResponse::not_found(ctx.lang);
    };
    if !ctx.csrf_ok() {
        return UiResponse::forbidden();
    }
    let name = pyfmt::unquote(inner);
    let (ok, message) = match run_history::delete_history_file(&name, &dir) {
        HistoryDelete::Invalid(n) => (
            false,
            ctx.tf(
                "Invalid history filename: {name}",
                &[("name", &loops_config::py_repr_str(&n))],
            ),
        ),
        HistoryDelete::NotFound(n) => (false, ctx.tf("{name} not found", &[("name", &n)])),
        HistoryDelete::Deleted(n) => (true, ctx.tf("Deleted {name}", &[("name", &n)])),
    };
    UiResponse::flash_redirect(ok, &message, "/runs?view=history")
}

fn delete_history(ctx: &UiCtx) -> UiResponse {
    delete_in(ctx, "/history/", history_dir(ctx))
}

fn delete_topic_history(ctx: &UiCtx) -> UiResponse {
    delete_in(ctx, "/topic-monitor/history/", topic_history_dir(ctx))
}
