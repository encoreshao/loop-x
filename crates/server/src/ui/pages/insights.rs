//! The Insights hub (`/insights`: analytics, cost, budget, memory) and the
//! Harness hub (`/harness`: gates, evals, audit): `_analytics_body`,
//! `_cost_body`, `_budget_body`, `_memory_body`, `_gates_body`, `_evals_body`
//! and `_audit_body` of `bin/web/dashboard_server.py`. None of these views has a
//! POST route, and the flash query parameter is ignored by every one of them.

use crate::ui::badge::default_badge;
use crate::ui::consts;
use crate::ui::hub::{hub_by_key, render_hub_page};
use crate::ui::markdown::{self, GitlabPrefixes};
use crate::ui::pyfmt::{self, escape, escape_text};
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use loopx_core::{
    budget, cost, events, gate_stats, health, learning, ledger, loop_audit, loop_serialize,
    memory_store, metrics, pydumps,
};
use serde_json::Value;
use std::path::Path;

pub fn register(r: &mut Registry) {
    r.get("/insights", insights);
    r.get("/harness", harness);
}

fn query_view(ctx: &UiCtx) -> Option<String> {
    ctx.query().first("view").map(str::to_string)
}

/// `int(query["days"][0])`, with a non-int becoming 7, then the page's own
/// `days not in (7, 30, 90)` fallback.
fn query_days(ctx: &UiCtx) -> i64 {
    let days = ctx
        .query()
        .first("days")
        .and_then(|raw| {
            let t = raw.trim();
            let t = t.strip_prefix('+').unwrap_or(t);
            if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
                return None;
            }
            t.parse::<i64>().ok()
        })
        .unwrap_or(7);
    if matches!(days, 7 | 30 | 90) {
        days
    } else {
        7
    }
}

fn insights(ctx: &UiCtx) -> UiResponse {
    let hub = hub_by_key("insights").expect("insights hub");
    let view = query_view(ctx);
    let key = crate::ui::hub::resolve_view(&hub, view.as_deref())
        .key
        .clone();
    let body = match key.as_str() {
        "cost" => cost_body(ctx),
        "budget" => budget_body(ctx),
        "memory" => memory_body(ctx),
        _ => analytics_body(ctx),
    };
    UiResponse::html(render_hub_page(
        ctx,
        &hub,
        view.as_deref(),
        &body,
        &default_badge(ctx),
    ))
}

fn harness(ctx: &UiCtx) -> UiResponse {
    let hub = hub_by_key("harness").expect("harness hub");
    let view = query_view(ctx);
    let key = crate::ui::hub::resolve_view(&hub, view.as_deref())
        .key
        .clone();
    let body = match key.as_str() {
        "evals" => evals_body(ctx),
        "audit" => audit_body(ctx),
        _ => gates_body(ctx),
    };
    UiResponse::html(render_hub_page(
        ctx,
        &hub,
        view.as_deref(),
        &body,
        &default_badge(ctx),
    ))
}

// ---------------------------------------------------------------- shared bits

fn icon(name: &str) -> &'static str {
    consts::ds(name)
}

fn n_of(v: &Value) -> String {
    pydumps::py_str(v)
}

fn f_of(v: &Value) -> Option<f64> {
    v.as_f64()
}

/// `f"{x:.Nf}"`.
fn fixed(x: f64, places: usize) -> String {
    format!("{x:.places$}")
}

fn py_capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect(),
        None => String::new(),
    }
}

/// `_three_state_pill_class`.
fn three_state_pill_class(status: &str) -> &'static str {
    match status.to_lowercase().as_str() {
        "pass" | "ok" => "pill-green",
        "fail" | "exceeded" => "pill-red",
        _ => "pill-grey",
    }
}

/// `_stat_tile_html`: `value` is already-rendered text (not escaped).
fn stat_tile(icon: &str, label: &str, value: &str, tooltip: Option<&str>, detail: Option<&str>) -> String {
    let tooltip_attr = tooltip
        .map(|t| format!(" title=\"{}\"", escape(t)))
        .unwrap_or_default();
    let detail_html = detail
        .filter(|d| !d.is_empty())
        .map(|d| format!("<span class='dash-stat-detail'>{}</span>", escape(d)))
        .unwrap_or_default();
    format!(
        "<div class='dash-stat-tile'{tooltip_attr}>\
         <span class='material-symbols-outlined dash-stat-icon' aria-hidden='true'>{icon}</span>\
         <span class='dash-stat-value'>{value}</span>\
         <span class='dash-stat-label'>{}</span>\
         {detail_html}</div>",
        escape(label)
    )
}

fn na_stat_tile(ctx: &UiCtx, icon: &str, label: &str, reason: &str) -> String {
    format!(
        "<div class='dash-stat-tile' title=\"{}\">\
         <span class='material-symbols-outlined dash-stat-icon' aria-hidden='true'>{icon}</span>\
         <span class='dash-stat-value'>{}</span>\
         <span class='dash-stat-label'>{}</span></div>",
        escape(reason),
        escape(&ctx.t("N/A")),
        escape(label)
    )
}

fn page_title(ctx: &UiCtx, title: &str, subtitle: &str) -> String {
    format!(
        "<div class=\"page-title\">\n<h1>{}</h1>\n<p class=\"subtitle\">{}</p>\n</div>\n",
        escape(&ctx.t(title)),
        escape(&ctx.t(subtitle))
    )
}

fn days_selector(ctx: &UiCtx, prefix: &str, days: i64) -> String {
    [7, 30, 90]
        .iter()
        .map(|n| {
            format!(
                "<a href='/insights?{prefix}days={n}' class=\"{}\">{}</a>",
                if *n == days { "active" } else { "" },
                escape(&ctx.tf("{n}d", &[("n", &n.to_string())]))
            )
        })
        .collect()
}

fn utc_window(ctx: &UiCtx, days: i64) -> (chrono::NaiveDate, String, String) {
    let until = ctx.state.now().date_naive();
    let since = until - chrono::Duration::days(days - 1);
    (until, since.to_string(), until.to_string())
}

// ------------------------------------------------------------------ analytics

const AUTONOMY_PLACEHOLDER_TOOLTIP: &str = "placeholder: currently identical to resolution rate";
const ESCALATION_TOOLTIP: &str = "1 - (escalated / processed) - higher is healthier";
const FIRST_PASS_VERIFICATION_TOOLTIP: &str =
    "currently identical to Verification \u{2014} the loop has no retry behavior yet";

fn health_label(name: &str) -> String {
    match name {
        "escalation" => "Non-escalation".to_string(),
        "cost_efficiency" => "Cost efficiency".to_string(),
        "retry_rate" => "Retry rate".to_string(),
        "learning_effectiveness" => "Learning effectiveness".to_string(),
        other => py_capitalize(other),
    }
}

fn health_detail(ctx: &UiCtx, name: &str, details: &Value) -> Option<String> {
    let value = details.get(name)?.get("value")?.as_f64()?;
    match name {
        "cost_efficiency" => Some(ctx.tf("${usd} per verified issue", &[("usd", &fixed(value, 2))])),
        "retry_rate" => Some(ctx.tf("{pct}% of runs retried", &[("pct", &fixed(value * 100.0, 0))])),
        "learning_effectiveness" => Some(ctx.tf(
            "{points} pts with memory",
            &[("points", &format!("{:+.0}", value * 100.0))],
        )),
        _ => None,
    }
}

fn health_section(ctx: &UiCtx, health: &Value, metrics_report: &Value) -> String {
    let score = f_of(&health["score"]);
    let score_text = score.map_or_else(|| ctx.t("N/A"), |s| format!("{}/100", fixed(s, 0)));
    let mut partial_note = String::new();
    if health["is_partial"].as_bool().unwrap_or(false) {
        let missing: Vec<&str> = health["missing_components"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let text = ctx.tf(
            "Partial score \u{2014} not yet tracked: {missing}",
            &[("missing", &missing.join(", "))],
        );
        partial_note = format!(
            "<p class='analytics-health-note' title=\"{}\">{}</p>",
            escape(health["missing_reason"].as_str().unwrap_or("")),
            escape(&text)
        );
    }
    let placeholder = metrics_report["quality_and_autonomy"]["autonomy_rate_is_placeholder"]
        .as_bool()
        .unwrap_or(false);
    let tiles: String = health["components"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(name, value)| {
                    let tooltip = if name == "escalation" {
                        Some(ctx.t(ESCALATION_TOOLTIP))
                    } else if name == "autonomy" && placeholder {
                        Some(ctx.t(AUTONOMY_PLACEHOLDER_TOOLTIP))
                    } else {
                        None
                    };
                    let value_text = f_of(value).map_or_else(|| ctx.t("N/A"), |v| fixed(v, 0));
                    let detail = health_detail(ctx, name, &health["details"]);
                    stat_tile(
                        "check_circle",
                        &ctx.t(&health_label(name)),
                        &value_text,
                        tooltip.as_deref(),
                        detail.as_deref(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    format!(
        "\n<section class=\"card\">\n<div class=\"section-header\">{}<h2>{}</h2></div>\n\
         <p class=\"analytics-health-score\">{score_text}</p>\n{partial_note}\n\
         <div class=\"dash-stats-grid\">{tiles}</div>\n</section>\n",
        icon("_SECTION_ICON_ANALYTICS"),
        escape(&ctx.t("Loop Health"))
    )
}

fn pct1(rate: &Value) -> Option<String> {
    f_of(rate).map(|r| format!("{}%", fixed(r * 100.0, 1)))
}

fn outcomes_section(ctx: &UiCtx, m: &Value) -> String {
    let issue = &m["issue"];
    let qa = &m["quality_and_autonomy"];
    let autonomy_text = pct1(&qa["autonomy_rate"]).unwrap_or_else(|| ctx.t("N/A"));
    let autonomy_tooltip = qa["autonomy_rate_is_placeholder"]
        .as_bool()
        .unwrap_or(false)
        .then(|| ctx.t(AUTONOMY_PLACEHOLDER_TOOLTIP));
    let tiles = [
        stat_tile("history", &ctx.t("Processed"), &n_of(&issue["issues_processed"]), None, None),
        stat_tile("check_circle", &ctx.t("Completed"), &n_of(&issue["issues_completed"]), None, None),
        stat_tile("warning", &ctx.t("Escalated"), &n_of(&issue["issues_escalated"]), None, None),
        stat_tile("error", &ctx.t("Failed"), &n_of(&issue["issues_failed"]), None, None),
        stat_tile("bolt", &ctx.t("Autonomy"), &autonomy_text, autonomy_tooltip.as_deref(), None),
    ]
    .concat();
    format!(
        "\n<section class=\"card\">\n<div class=\"section-header\">{}<h2>{}</h2></div>\n\
         <div class=\"dash-stats-grid\">{tiles}</div>\n</section>\n",
        icon("_SECTION_ICON_ACTIVITY"),
        escape(&ctx.t("Outcomes"))
    )
}

fn quality_section(ctx: &UiCtx, m: &Value) -> String {
    let verification = &m["verification"];
    let qa = &m["quality_and_autonomy"];
    let verification_text = pct1(&verification["verification_pass_rate"]).unwrap_or_else(|| ctx.t("N/A"));
    let first_pass_text =
        pct1(&verification["first_pass_verification_rate"]).unwrap_or_else(|| ctx.t("N/A"));
    let tiles = [
        stat_tile("check_circle", &ctx.t("Verification"), &verification_text, None, None),
        stat_tile(
            "check_circle",
            &ctx.t("First-pass verification"),
            &first_pass_text,
            Some(&ctx.t(FIRST_PASS_VERIFICATION_TOOLTIP)),
            None,
        ),
        na_stat_tile(
            ctx,
            "merge",
            &ctx.t("First-pass MR"),
            &ctx.t("needs Phase 10 human-review data, not built yet"),
        ),
        na_stat_tile(
            ctx,
            "history",
            &ctx.t("Retry rate"),
            qa["retry_rate_unavailable_reason"].as_str().unwrap_or(""),
        ),
        na_stat_tile(
            ctx,
            "error",
            &ctx.t("Regression"),
            &ctx.t("not defined by any sprint built so far"),
        ),
    ]
    .concat();
    format!(
        "\n<section class=\"card\">\n<div class=\"section-header\">{}<h2>{}</h2></div>\n\
         <div class=\"dash-stats-grid\">{tiles}</div>\n</section>\n",
        icon("_SECTION_ICON_LOGS"),
        escape(&ctx.t("Quality"))
    )
}

fn sorted_count_rows(ctx: &UiCtx, counts: &Value) -> String {
    let mut items: Vec<(&String, &Value)> = counts.as_object().map(|m| m.iter().collect()).unwrap_or_default();
    items.sort_by(|a, b| a.0.cmp(b.0));
    let rows: String = items
        .iter()
        .map(|(k, v)| format!("<li><span class='k'>{}</span><span>{}</span></li>", escape(k), n_of(v)))
        .collect();
    if rows.is_empty() {
        format!(
            "<li><span class='k'>{}</span><span>-</span></li>",
            escape(&ctx.t("No data"))
        )
    } else {
        rows
    }
}

fn risk_section(ctx: &UiCtx, m: &Value) -> String {
    let c = &m["classification"];
    let mut tiles = stat_tile("check_circle", &ctx.t("Classified"), &n_of(&c["classified_total"]), None, None);
    for level in ["LOW", "MEDIUM", "HIGH", "CRITICAL"] {
        let count = c["by_risk_level"].get(level).map_or_else(|| "0".to_string(), n_of);
        tiles.push_str(&stat_tile("warning", &ctx.t(&py_capitalize(level)), &count, None, None));
    }
    format!(
        "\n<section class=\"card\">\n<div class=\"section-header\">{}<h2>{}</h2></div>\n\
         <div class=\"dash-stats-grid\">{tiles}</div>\n<div class=\"analytics-breakdown-columns\">\n\
         <div><h3>{}</h3><ul class=\"field-list\">{}</ul></div>\n\
         <div><h3>{}</h3><ul class=\"field-list\">{}</ul></div>\n</div>\n</section>\n",
        icon("_SECTION_ICON_RISK"),
        escape(&ctx.t("Risk & Classification")),
        escape(&ctx.t("By type")),
        sorted_count_rows(ctx, &c["by_type"]),
        escape(&ctx.t("By complexity")),
        sorted_count_rows(ctx, &c["by_complexity"])
    )
}

fn failure_section(ctx: &UiCtx, m: &Value) -> String {
    let ft = &m["failure_taxonomy"];
    let tiles = if ft["total"].as_i64().unwrap_or(0) == 0 {
        na_stat_tile(
            ctx,
            "error",
            &ctx.t("Failure breakdown"),
            &ctx.t("no escalations in this window"),
        )
    } else {
        let mut pcts: Vec<(&String, f64)> = ft["by_category_pct"]
            .as_object()
            .map(|o| o.iter().map(|(k, v)| (k, v.as_f64().unwrap_or(0.0))).collect())
            .unwrap_or_default();
        pcts.sort_by(|a, b| (-a.1).partial_cmp(&(-b.1)).unwrap_or(std::cmp::Ordering::Equal));
        let mut out = stat_tile("error", &ctx.t("Escalations"), &n_of(&ft["total"]), None, None);
        for (category, pct) in pcts {
            out.push_str(&stat_tile(
                "error",
                &ctx.t(&py_capitalize(category)),
                &format!("{}%", fixed(pct * 100.0, 1)),
                None,
                None,
            ));
        }
        out
    };
    format!(
        "\n<section class=\"card\">\n<div class=\"section-header\">{}<h2>{}</h2></div>\n\
         <div class=\"dash-stats-grid\">{tiles}</div>\n</section>\n",
        icon("_SECTION_ICON_FAILURE"),
        escape(&ctx.t("Failure Breakdown"))
    )
}

fn learning_section(ctx: &UiCtx, report: &learning::LearningReport) -> String {
    let reuse = report.reuse.to_value();
    let rate = |v: &Value| pct1(v).unwrap_or_else(|| ctx.t("N/A"));
    let tiles = [
        stat_tile("lightbulb", &ctx.t("Lessons created"), &n_of(&reuse["lessons_created"]), None, None),
        stat_tile("lightbulb", &ctx.t("Total reuses"), &n_of(&reuse["total_reuses"]), None, None),
        stat_tile("lightbulb", &ctx.t("Reuse rate"), &rate(&reuse["memory_reuse_rate"]), None, None),
        stat_tile("lightbulb", &ctx.t("Success rate"), &rate(&reuse["memory_success_rate"]), None, None),
        na_stat_tile(
            ctx,
            "lightbulb",
            &ctx.t("Failures prevented"),
            reuse["failures_prevented_reason"].as_str().unwrap_or(""),
        ),
    ]
    .concat();
    format!(
        "\n<section class=\"card\">\n<div class=\"section-header\">{}<h2>{}</h2></div>\n\
         <div class=\"dash-stats-grid\">{tiles}</div>\n</section>\n",
        icon("_SECTION_ICON_MEMORY"),
        escape(&ctx.t("Learning"))
    )
}

fn fmt_trend_value(value: f64, unit: &str) -> String {
    if unit == "$" {
        cost::fmt_money_value(value)
    } else {
        format!("{}%", fixed(value, 1))
    }
}

/// `_trend_line_chart_svg` (default 520x140 size).
fn trend_chart(ctx: &UiCtx, label: &str, points: &[(String, Option<f64>)], unit: &str, note: Option<&str>) -> String {
    let (width, height) = (520.0_f64, 140.0_f64);
    let note_html = note
        .filter(|n| !n.is_empty())
        .map(|n| format!("<p class='trend-chart-note'>{}</p>", escape(n)))
        .unwrap_or_default();
    let values: Vec<f64> = points.iter().filter_map(|(_, v)| *v).collect();
    if values.is_empty() {
        return format!(
            "<div class='trend-chart trend-chart-empty'><p class='trend-chart-title'>{}</p>{note_html}<p>{}</p></div>",
            escape(label),
            escape(&ctx.t("no data in this window"))
        );
    }
    let (pad_left, pad_right, pad_top, pad_bottom) = (8.0, 8.0, 12.0, 12.0);
    let plot_w = width - pad_left - pad_right;
    let plot_h = height - pad_top - pad_bottom;
    let max_value = values.iter().cloned().fold(f64::MIN, f64::max);
    let (mut v_min, mut v_max) = if unit == "%" { (0.0, 100.0) } else { (0.0, max_value) };
    if v_min == v_max {
        v_min -= 1.0;
        v_max += 1.0;
    }
    let n = points.len();
    let x_at = |i: usize| {
        pad_left + if n > 1 { plot_w * i as f64 / (n - 1) as f64 } else { plot_w / 2.0 }
    };
    let y_at = |v: f64| pad_top + plot_h - ((v - v_min) / (v_max - v_min) * plot_h);

    let mut segments: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut current: Vec<(f64, f64)> = Vec::new();
    for (i, (_, v)) in points.iter().enumerate() {
        match v {
            None => {
                if !current.is_empty() {
                    segments.push(std::mem::take(&mut current));
                }
            }
            Some(v) => current.push((x_at(i), y_at(*v))),
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    let polylines: String = segments
        .iter()
        .map(|seg| {
            format!(
                "<polyline points='{}' fill='none' stroke='var(--md-primary)' stroke-width='2' \
                 stroke-linecap='round' stroke-linejoin='round' />",
                seg.iter()
                    .map(|(x, y)| format!("{x:.1},{y:.1}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        })
        .collect();
    let dots: String = points
        .iter()
        .enumerate()
        .filter_map(|(i, (date_label, v))| {
            v.map(|v| {
                format!(
                    "<circle cx='{:.1}' cy='{:.1}' r='3' fill='var(--md-primary)'><title>{}: {}</title></circle>",
                    x_at(i),
                    y_at(v),
                    escape(date_label),
                    fmt_trend_value(v, unit)
                )
            })
        })
        .collect();
    let baseline_y = pad_top + plot_h;
    let axis = format!(
        "<line x1='{}' y1='{}' x2='{}' y2='{}' stroke='var(--md-outline-variant)' stroke-width='1' />",
        pad_left as i64,
        baseline_y as i64,
        (width - pad_right) as i64,
        baseline_y as i64
    );
    let max_label = if unit == "$" {
        format!(
            "<p class='trend-chart-note'>{}</p>",
            escape(&ctx.tf("max: {value}", &[("value", &fmt_trend_value(max_value, unit))]))
        )
    } else {
        String::new()
    };
    let aria = ctx.tf("{label} trend", &[("label", label)]);
    format!(
        "<div class='trend-chart'><p class='trend-chart-title'>{}</p>{note_html}{max_label}\
         <svg viewBox='0 0 {} {}' width='100%' height='{}' role='img' aria-label='{}'>{axis}{polylines}{dots}</svg></div>",
        escape(label),
        width as i64,
        height as i64,
        height as i64,
        escape(&aria)
    )
}

fn trend_section(ctx: &UiCtx, days: i64, today: chrono::NaiveDate, window: &[Value]) -> String {
    let bucket_days = if days <= 7 { 1 } else { 7 };
    let reports = metrics::bucketed_reports(
        Some(&ctx.state.events_dir),
        days,
        bucket_days,
        Some(window),
        today,
    );
    let label_of = |r: &metrics::Report| {
        let (s, u) = (
            r.since_date.clone().unwrap_or_default(),
            r.until_date.clone().unwrap_or_default(),
        );
        if s == u {
            s
        } else {
            format!("{s}\u{2013}{u}")
        }
    };
    let pct = |rate: Option<f64>| rate.map(|r| r * 100.0);
    let autonomy: Vec<_> = reports
        .iter()
        .map(|r| (label_of(r), pct(r.quality_and_autonomy.autonomy_rate)))
        .collect();
    let resolution: Vec<_> = reports
        .iter()
        .map(|r| (label_of(r), pct(r.quality_and_autonomy.resolution_rate)))
        .collect();
    let verification: Vec<_> = reports
        .iter()
        .map(|r| (label_of(r), pct(r.verification.verification_pass_rate)))
        .collect();
    let cost_points: Vec<_> = reports
        .iter()
        .map(|r| {
            let (s, u) = (r.since_date.as_deref(), r.until_date.as_deref());
            let slice = events::filter_by_date(window.to_vec(), s, u);
            let report = cost::build_cost_report(Some(&ctx.state.events_dir), s, u, Some(&slice));
            (label_of(r), report.cost.cost_per_resolution)
        })
        .collect();
    let note = (!reports.is_empty()).then(|| ctx.t(AUTONOMY_PLACEHOLDER_TOOLTIP));
    let charts = [
        trend_chart(ctx, &ctx.t("Autonomy rate"), &autonomy, "%", note.as_deref()),
        trend_chart(ctx, &ctx.t("Resolution rate"), &resolution, "%", None),
        trend_chart(ctx, &ctx.t("Verification pass rate"), &verification, "%", None),
        trend_chart(ctx, &ctx.t("Cost per resolution"), &cost_points, "$", None),
        format!(
            "<div class='trend-chart trend-chart-empty'><p class='trend-chart-title'>{}</p><p>{}</p></div>",
            escape(&ctx.t("MR acceptance")),
            escape(&ctx.t("Not yet tracked \u{2014} needs Phase 10 human-review data."))
        ),
    ]
    .concat();
    format!(
        "\n<section class=\"card\">\n<div class=\"section-header\">{}<h2>{}</h2></div>\n\
         <div class=\"trend-charts-grid\">{charts}</div>\n</section>\n",
        icon("_SECTION_ICON_ANALYTICS"),
        escape(&ctx.t("Trend"))
    )
}

fn analytics_body(ctx: &UiCtx) -> String {
    let days = query_days(ctx);
    let (today, since, until) = utc_window(ctx, days);
    let dir = &ctx.state.events_dir;
    let window = events::iter_events(Some(dir), Some(&since), Some(&until));
    let metrics_report = metrics::build_report(Some(dir), Some(&since), Some(&until), None, Some(&window));
    let learning_report =
        learning::build_learning_report(Some(dir), Some(&since), Some(&until), None, Some(&window));
    let runs = ledger::iter_runs(&ledger::RunQuery {
        days: None,
        loop_name: None,
        events_dir: Some(dir),
        since_date: Some(&since),
        until_date: Some(&until),
    });
    let outcomes = learning::memory_outcomes(&window);
    let health_report =
        health::compute_health_score(&metrics_report, Some(&runs), Some(&outcomes)).to_value();
    let metrics_value = metrics_report.to_value();
    format!(
        "\n{}\n<div class=\"analytics-days-selector\">{}</div>\n\n<div class=\"analytics-sections\">\n\
         {}\n{}\n{}\n{}\n{}\n{}\n{}\n</div>\n",
        page_title(ctx, "Analytics", "How the loop is performing - no logs required."),
        days_selector(ctx, "", days),
        health_section(ctx, &health_report, &metrics_value),
        outcomes_section(ctx, &metrics_value),
        quality_section(ctx, &metrics_value),
        risk_section(ctx, &metrics_value),
        failure_section(ctx, &metrics_value),
        learning_section(ctx, &learning_report),
        trend_section(ctx, days, today, &window)
    )
}

// ----------------------------------------------------------------------- cost

fn cost_body(ctx: &UiCtx) -> String {
    let days = query_days(ctx);
    let (_, since, until) = utc_window(ctx, days);
    let dir = &ctx.state.events_dir;
    let report = cost::build_cost_report(Some(dir), Some(&since), Some(&until), None).cost;
    let runs = ledger::iter_runs(&ledger::RunQuery {
        days: None,
        loop_name: None,
        events_dir: Some(dir),
        since_date: None,
        until_date: None,
    });
    let summary = loop_serialize::summarize_run_costs(Some(&runs));

    let money = |v: Option<f64>| v.map_or_else(|| ctx.t("N/A"), cost::fmt_money_value);
    let tiles = [
        stat_tile("smart_toy", &ctx.t("AI cost"), &cost::fmt_money_value(report.total_cost_usd), None, None),
        stat_tile("smart_toy", &ctx.t("Cost / issue"), &money(report.cost_per_issue), None, None),
        stat_tile("smart_toy", &ctx.t("Cost / resolution"), &money(report.cost_per_resolution), None, None),
    ]
    .concat();
    let cost_section = format!(
        "\n<section class=\"card\">\n<div class=\"section-header\">{}<h2>{}</h2></div>\n\
         <div class=\"dash-stats-grid\">{tiles}</div>\n</section>\n",
        icon("_SECTION_ICON_AI_CLI"),
        escape(&ctx.t("Cost"))
    );
    let rt_tiles = [
        stat_tile("loop", &ctx.t("Runs"), &summary.total_runs.to_string(), None, None),
        stat_tile(
            "payments",
            &ctx.t("Estimated cost"),
            &cost::fmt_money_value(summary.total_cost_usd),
            None,
            None,
        ),
        stat_tile("payments", &ctx.t("Cost / run"), &money(summary.cost_per_run_usd), None, None),
    ]
    .concat();
    let runtime_section = format!(
        "\n<section class=\"card\">\n<div class=\"section-header\">{}<h2>{}</h2></div>\n\
         <div class=\"dash-stats-grid\">{rt_tiles}</div>\n</section>\n",
        icon("_SECTION_ICON_COST"),
        escape(&ctx.t("Loop Runtime Cost"))
    );
    format!(
        "\n{}\n<div class=\"analytics-days-selector\">{}</div>\n\n<div class=\"analytics-sections\">\n{cost_section}\n{runtime_section}\n</div>\n",
        page_title(ctx, "Cost", "What the loop is spending, from both cost sources it tracks."),
        days_selector(ctx, "view=cost&", days)
    )
}

// --------------------------------------------------------------------- budget

fn budget_dimension_tile(ctx: &UiCtx, icon: &str, label: &str, dimension: &Value) -> String {
    let empty = match dimension {
        Value::Null => true,
        Value::Object(o) => o.is_empty(),
        _ => false,
    };
    if empty {
        return String::new();
    }
    let pick = |names: [&str; 3]| -> Value {
        names
            .iter()
            .find_map(|n| dimension.get(*n).cloned())
            .unwrap_or(Value::Null)
    };
    let used = pick(["used", "used_seconds", "used_usd"]);
    let limit = pick(["limit", "limit_seconds", "limit_usd"]);
    let status = dimension.get("status").cloned().unwrap_or_else(|| Value::String(String::new()));
    let status_text = n_of(&status);
    let value = if limit.is_null() {
        ctx.tf("{used} / \u{2014} (no limit configured)", &[("used", &n_of(&used))])
    } else {
        format!("{} / {}", n_of(&used), n_of(&limit))
    };
    format!(
        "<div class='dash-stat-tile'>\
         <span class='material-symbols-outlined dash-stat-icon' aria-hidden='true'>{icon}</span>\
         <span class='dash-stat-value'>{}</span>\
         <span class='dash-stat-label'>{}</span>\
         <span class='pill {}'>{}</span></div>",
        escape(&value),
        escape(label),
        three_state_pill_class(&status_text),
        escape(&status_text)
    )
}

fn rollup_section(ctx: &UiCtx, title: &str, key_header: &str, rows: &[(String, budget::BudgetGroup)]) -> String {
    if rows.is_empty() {
        return format!(
            "\n<section class=\"card\">\n<div class=\"section-header\"><h2>{}</h2></div>\n<p>{}</p>\n</section>\n",
            escape(title),
            escape(&ctx.t("No data yet for this breakdown."))
        );
    }
    let body: String = rows
        .iter()
        .map(|(key, g)| {
            let pills: String = [("ok", g.status_counts.ok), ("warning", g.status_counts.warning), ("exceeded", g.status_counts.exceeded)]
                .iter()
                .filter(|(_, c)| *c > 0)
                .map(|(s, c)| format!("<span class='pill {}'>{s} {c}</span>", three_state_pill_class(s)))
                .collect();
            format!(
                "<tr><td>{}</td><td>{}</td><td>${}</td><td class='pill-row'>{pills}</td></tr>",
                escape(key),
                g.runs,
                fixed(g.cost_used_usd, 2)
            )
        })
        .collect();
    format!(
        "\n<section class=\"card\">\n<div class=\"section-header\"><h2>{}</h2></div>\n\
         <div class='table-wrap'><table class='daemons'>\n\
         <thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr></thead>\n\
         <tbody>{body}</tbody>\n</table></div>\n</section>\n",
        escape(title),
        escape(key_header),
        escape(&ctx.t("Runs")),
        escape(&ctx.t("Cost")),
        escape(&ctx.t("Status"))
    )
}

fn budget_body(ctx: &UiCtx) -> String {
    let runs_dir = ctx.state.repo_root.join("outputs").join("loop-runs");
    let mut with_budget: Vec<(Value, Value)> = Vec::new();
    for path in loop_serialize::list_results(Some(&runs_dir)).into_iter().rev() {
        let Ok(data) = loop_serialize::read_result(&path) else {
            continue;
        };
        let Some(last) = data.get("iterations").and_then(Value::as_array).and_then(|a| a.last()) else {
            continue;
        };
        let budget = last.get("budget").cloned().unwrap_or(Value::Null);
        let empty = match &budget {
            Value::Null => true,
            Value::Object(o) => o.is_empty(),
            _ => false,
        };
        if empty {
            continue;
        }
        with_budget.push((data.clone(), budget));
    }
    let heading = page_title(ctx, "Budget", "Budget usage for every recorded LoopRuntime run, most recent first.");
    if with_budget.is_empty() {
        return format!(
            "\n{heading}<div class=\"grid\"><section class=\"card\">\n<p>{}</p>\n</section></div>\n",
            ctx.tf(
                "No runs yet - run {command} to produce one.",
                &[("command", "<code>loopx loop run &lt;loop.yaml&gt;</code>")]
            )
        );
    }

    let ledger_runs = ledger::iter_runs(&ledger::RunQuery {
        days: None,
        loop_name: None,
        events_dir: Some(&ctx.state.events_dir),
        since_date: None,
        until_date: None,
    });
    let budget_runs: Vec<budget::BudgetRun> = ledger_runs
        .iter()
        .filter_map(|r| {
            Some(budget::BudgetRun {
                loop_name: r.loop_name.clone()?,
                run_id: r.run_id.clone()?,
                has_result: r.has_result,
                has_iterations: !r.iterations.is_empty(),
                budget_overall: r.budget_overall.as_deref().and_then(budget::BudgetStatus::parse),
                total_cost_usd: r.total_cost_usd,
            })
        })
        .collect();
    let by_loop: Vec<_> = budget::summarize_by_loop(&budget_runs)
        .into_iter()
        .map(|s| (s.definition_name, s.group))
        .collect();
    let by_time = |granularity: &str, limit: usize| -> Vec<(String, budget::BudgetGroup)> {
        budget::summarize_by_time(&budget_runs, granularity, Some(limit))
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.bucket, s.group))
            .collect()
    };
    let rollups = [
        rollup_section(ctx, &ctx.t("By loop"), &ctx.t("Loop"), &by_loop),
        rollup_section(ctx, &ctx.t("By day"), &ctx.t("Day"), &by_time("day", 14)),
        rollup_section(ctx, &ctx.t("By week"), &ctx.t("Week"), &by_time("week", 8)),
        rollup_section(ctx, &ctx.t("By month"), &ctx.t("Month"), &by_time("month", 6)),
    ]
    .concat();

    let rows: String = with_budget
        .iter()
        .map(|(data, budget)| {
            let run_id = data["run_id"].as_str().unwrap_or("");
            let overall = budget.get("overall").map_or_else(String::new, n_of);
            let tiles = [
                budget_dimension_tile(ctx, "loop", &ctx.t("Iterations"), &budget["iterations"]),
                budget_dimension_tile(ctx, "history", &ctx.t("Runtime (s)"), &budget["runtime"]),
                budget_dimension_tile(ctx, "payments", &ctx.t("Cost ($)"), &budget["cost"]),
            ]
            .concat();
            format!(
                "\n<section class=\"card\">\n<div class=\"history-entry-header\">\n\
                 <a href='/loop-runs/{}'><strong>{}</strong></a>\n\
                 <div class='pill-row'><span class='pill {}'>{}</span></div>\n</div>\n\
                 <p class='history-entry-overview'>run_id: {}</p>\n\
                 <div class=\"dash-stats-grid\">{tiles}</div>\n</section>\n",
                pyfmt::quote(run_id, "/"),
                escape(data["definition_name"].as_str().unwrap_or("")),
                three_state_pill_class(&overall),
                escape(&overall),
                escape(run_id)
            )
        })
        .collect();
    format!(
        "\n{heading}\n<div class=\"grid\">\n{rollups}\n</div>\n\n<div class=\"page-title\">\n<h2>{}</h2>\n</div>\n\n<div class=\"grid\">\n{rows}\n</div>\n",
        escape(&ctx.t("Runs"))
    )
}

// --------------------------------------------------------------------- memory

fn legacy_learnings(ctx: &UiCtx, instance: &str, project_id: &str) -> Vec<Value> {
    let path = ctx
        .state
        .user_home
        .join(".gitlab")
        .join("cache")
        .join(instance)
        .join("projects")
        .join(project_id.replace('/', "__"))
        .join("project.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("_memory")?.get("fix_learnings")?.as_array().cloned())
        .unwrap_or_default()
}

fn issue_pill(prefixes: &GitlabPrefixes, alias: &str, iid: &Value) -> String {
    let safe = escape(&n_of(iid));
    match prefixes.get(alias).filter(|b| !b.is_empty()) {
        Some(base) => format!(
            "<a class='pill pill-link' href='{base}/-/issues/{}' rel='noopener' target='_blank'>\
             <span class='material-symbols-outlined' aria-hidden='true'>open_in_new</span>#{safe}</a>",
            n_of(iid)
        ),
        None => format!("<span class='pill pill-grey'>#{safe}</span>"),
    }
}

fn tag_pills(tags: &Value) -> String {
    tags.as_array()
        .map(|a| {
            a.iter()
                .map(|t| format!("<span class='pill pill-grey'>{}</span>", escape(&n_of(t))))
                .collect()
        })
        .unwrap_or_default()
}

fn truthy(v: &Value) -> bool {
    loopx_core::loops_config::py_truthy(v)
}

fn memory_body(ctx: &UiCtx) -> String {
    let st = ctx.state;
    let root = st.home_dir.join("memory");
    let projects: Value = std::fs::read_to_string(st.home_dir.join("projects.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let default_instance = projects["gitlab_instance"].as_str().unwrap_or("").to_string();
    // {alias: (legacy entries, task memories)}, in projects.json order.
    let mut memory: Vec<(String, Vec<Value>, Vec<Value>)> = Vec::new();
    if let Some(map) = projects.get("projects").and_then(Value::as_object) {
        for (alias, project) in map {
            let instance = project["instance"].as_str().unwrap_or(&default_instance);
            let legacy = legacy_learnings(ctx, instance, &n_of(&project["project_id"]));
            let tasks = memory_store::list_task_memories(alias, Some(&root))
                .iter()
                .map(memory_store::TaskMemory::to_value)
                .collect();
            memory.push((alias.clone(), legacy, tasks));
        }
    }
    let report = learning::build_learning_report(Some(&st.events_dir), None, None, None, None);
    let stats: std::collections::HashMap<String, Value> = report
        .lessons
        .iter()
        .filter_map(|l| Some((l.lesson_id.as_str()?.to_string(), l.to_value())))
        .collect();
    let prefixes = crate::ui::gitlab::issue_url_prefixes(st);

    let reuse_stats = |lesson_id: &Value| -> String {
        if !truthy(lesson_id) {
            return String::new();
        }
        let s = lesson_id.as_str().and_then(|id| stats.get(id));
        match s.filter(|s| s["times_reused"].as_i64().unwrap_or(0) != 0) {
            None => format!(
                "<p class='learning-reuse-stats'>{}</p>",
                escape(&ctx.t("Not yet reused"))
            ),
            Some(s) => {
                let eff = f_of(&s["effectiveness_rate"])
                    .map_or_else(|| ctx.t("pending"), |e| format!("{}%", fixed(e * 100.0, 0)));
                let text = ctx.tf(
                    "Reused {times}\u{d7} \u{b7} {successful} successful, {failed} failed (effectiveness: {effectiveness})",
                    &[
                        ("times", &n_of(&s["times_reused"])),
                        ("successful", &n_of(&s["successful_reuses"])),
                        ("failed", &n_of(&s["failed_reuses"])),
                        ("effectiveness", &eff),
                    ],
                );
                format!("<p class='learning-reuse-stats'>{}</p>", escape_text(&text))
            }
        }
    };

    let task_item = |alias: &str, e: &Value| -> String {
        let category = if truthy(&e["category"]) {
            format!("<span class='pill pill-grey'>{}</span>", escape(&n_of(&e["category"])))
        } else {
            String::new()
        };
        let meta = format!(
            "<div class='pill-row'>{}{category}{}</div>",
            issue_pill(&prefixes, alias, &e["issue_iid"]),
            tag_pills(&e["tags"])
        );
        let description = &e["description"];
        let description_html = if truthy(description) {
            format!("<p class='history-entry-overview'>{}</p>", escape(&n_of(description)))
        } else {
            String::new()
        };
        let needs_review = e["flag"].as_str() == Some("needs_review");
        let badge = if needs_review {
            format!(
                "<span class='pill pill-red' title='{}'><span class='material-symbols-outlined' aria-hidden='true'>warning</span>{}</span>",
                escape(&ctx.t("Reuse of this lesson preceded repeated failures; check it is still correct.")),
                escape(&ctx.t("Needs review"))
            )
        } else {
            String::new()
        };
        let data_attr = if needs_review { " data-needs-review='1'" } else { "" };
        format!(
            "<li class='learning-item'{data_attr}>{badge}{description_html}<div class='markdown'>{}</div>{meta}{}</li>",
            markdown::render_markdown(e["body"].as_str().unwrap_or(""), &prefixes),
            reuse_stats(&e["lesson_id"])
        )
    };
    let legacy_item = |alias: &str, e: &Value| -> String {
        let pill = match e.get("issue_iid") {
            Some(iid) if !iid.is_null() => issue_pill(&prefixes, alias, iid),
            _ => String::new(),
        };
        let meta = format!("{pill}{}", tag_pills(&e["tags"]));
        let meta_html = if meta.is_empty() {
            String::new()
        } else {
            format!("<div class='pill-row'>{meta}</div>")
        };
        format!(
            "<li class='learning-item'><div class='markdown'>{}</div>{meta_html}</li>",
            markdown::render_markdown(e["lesson"].as_str().unwrap_or(""), &prefixes)
        )
    };

    let needs_review_count: usize = memory
        .iter()
        .map(|(_, _, tasks)| tasks.iter().filter(|e| e["flag"].as_str() == Some("needs_review")).count())
        .sum();
    let sections: String = memory
        .iter()
        .map(|(alias, legacy, tasks)| {
            let tasks_html = if tasks.is_empty() {
                format!("<p>{}</p>", escape(&ctx.t("(no task memory recorded yet)")))
            } else {
                format!(
                    "<ul class='plain'>{}</ul>",
                    tasks.iter().map(|e| task_item(alias, e)).collect::<String>()
                )
            };
            let legacy_html = if legacy.is_empty() {
                String::new()
            } else {
                format!(
                    "<h4>{}</h4><ul class='plain'>{}</ul>",
                    escape(&ctx.t("Legacy learnings")),
                    legacy.iter().map(|e| legacy_item(alias, e)).collect::<String>()
                )
            };
            format!(
                "<div class='project-block'><h3>{}</h3>{tasks_html}{legacy_html}</div>",
                escape(alias)
            )
        })
        .collect();
    let memory_html = if sections.is_empty() {
        format!(
            "\n<div class='empty-state'>\n<div class='empty-state-icon'><span class='material-symbols-outlined' aria-hidden='true'>folder_off</span></div>\n\
             <p class='empty-state-message'>{}</p>\n\
             <a class='btn btn-primary empty-state-action' href='/loops/gitlab-loop?view=projects'>\n\
             <span class='material-symbols-outlined' aria-hidden='true'>settings</span> {}\n</a>\n</div>\n",
            escape_text(&ctx.t("No projects configured yet, so there is no memory recorded.")),
            escape_text(&ctx.t("Set up a project"))
        )
    } else {
        sections
    };

    let style = "\n<style>\n\
#memory-filter-needs-review:checked ~ .memory-list li.learning-item:not([data-needs-review]) { display: none; }\n\
#memory-filter-needs-review { position: absolute; opacity: 0; pointer-events: none; }\n\
#memory-filter-needs-review:checked + label.pill { outline: 2px solid var(--md-error, currentColor); }\n\
#memory-filter-needs-review:focus-visible + label.pill { outline: 2px solid currentColor; }\n\
label.memory-filter-chip { cursor: pointer; }\n\
</style>\n";
    format!(
        "\n{}{style}<div class=\"grid\">\n<section class=\"card\">\n<div class=\"section-header\">{}<h2>{}</h2></div>\n\
         <input type=\"checkbox\" id=\"memory-filter-needs-review\">\n\
         <label class=\"pill pill-red memory-filter-chip\" for=\"memory-filter-needs-review\">{} ({needs_review_count})</label>\n\
         <div class=\"memory-list\">\n{memory_html}\n</div>\n</section>\n</div>\n",
        page_title(ctx, "Project Memory", "Task memory recorded per project by the automated review loop."),
        icon("_SECTION_ICON_MEMORY"),
        escape(&ctx.t("Project Memory")),
        escape(&ctx.t("Needs review"))
    )
}

// --------------------------------------------------------------------- gates

fn loop_yamls(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("loop.yaml"))
        .filter(|p| p.is_file())
        .collect();
    out.sort();
    out
}

fn gate_rate_text(ctx: &UiCtx, rate: Option<f64>) -> String {
    rate.map_or_else(|| ctx.t("N/A"), |r| format!("{}%", fixed(r * 100.0, 0)))
}

fn gate_tile(icon: &str, value: &str, label: &str, note: &str) -> String {
    let note_html = if note.is_empty() {
        String::new()
    } else {
        format!("<span class='subtitle'>{}</span>", escape(note))
    };
    format!(
        "<div class='dash-stat-tile'>\
         <span class='material-symbols-outlined dash-stat-icon' aria-hidden='true'>{icon}</span>\
         <span class='dash-stat-value'>{}</span>\
         <span class='dash-stat-label'>{}</span>{note_html}</div>",
        escape(value),
        escape(label)
    )
}

fn gates_body(ctx: &UiCtx) -> String {
    let since = (ctx.state.now().date_naive() - chrono::Duration::days(6)).to_string();
    let evs = events::iter_events(Some(&ctx.state.events_dir), Some(&since), None);
    let stats = gate_stats::gate_stats(&evs);
    let total = &stats.total;
    let threshold = (gate_stats::AGREEMENT_THRESHOLD * 100.0) as i64;
    let rate_note = ctx.tf(
        "Flip a loop to gate once this holds at {threshold}% or more over a week.",
        &[("threshold", &threshold.to_string())],
    );
    let retried_label = ctx.t("Retried \u{2192} passed");
    let tiles = [
        gate_tile("fact_check", &gate_rate_text(ctx, total.agreement_rate), &ctx.t("Agreement rate"), &rate_note),
        gate_tile("error", &total.gated_blocks.to_string(), &ctx.t("Blocked by gate"), ""),
        gate_tile("autorenew", &total.retried_then_passed.to_string(), &retried_label, ""),
        gate_tile("warning", &total.escalated_verification.to_string(), &ctx.t("Escalated"), ""),
    ]
    .concat();
    let rows: String = stats
        .by_project
        .iter()
        .map(|(project, row)| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape(project),
                row.issues,
                escape(&gate_rate_text(ctx, row.agreement_rate)),
                row.gated_blocks,
                row.retried_then_passed,
                row.escalated_verification
            )
        })
        .collect();
    let table = if rows.is_empty() {
        format!("<p>{}</p>", escape(&ctx.t("No data yet for this breakdown.")))
    } else {
        format!(
            "<div class='table-wrap'><table class='daemons'><thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>{rows}</tbody></table></div>",
            escape(&ctx.t("Project")),
            escape(&ctx.t("Issues")),
            escape(&ctx.t("Agreement rate")),
            escape(&ctx.t("Blocked by gate")),
            escape(&retried_label),
            escape(&ctx.t("Escalated"))
        )
    };
    let chips: String = loop_yamls(&ctx.state.repo_root.join("loops"))
        .iter()
        .filter_map(|p| loop_audit::load(p).ok())
        .map(|(def, _)| {
            let gate = def.verification.mode == "gate";
            format!(
                "<li><strong>{}</strong> <span class='pill {}'>{}</span></li>",
                escape(&def.name),
                if gate { "pill-green" } else { "pill-grey" },
                escape(&ctx.t(if gate { "gate" } else { "observe" }))
            )
        })
        .collect();
    format!(
        "\n{}<section class=\"card\">\n<div class=\"section-header\"><h2>{}</h2></div>\n<div class=\"dash-stats-grid\">{tiles}</div>\n</section>\n\
         <section class=\"card\">\n<div class=\"section-header\"><h2>{}</h2></div>\n{table}\n</section>\n\
         <section class=\"card\">\n<div class=\"section-header\"><h2>{}</h2></div>\n<ul class='plain'>{chips}</ul>\n</section>\n",
        page_title(ctx, "Gates", "Does the external verifier agree with the agent, and what the gate did."),
        escape(&ctx.t("Last 7 days")),
        escape(&ctx.t("By project")),
        escape(&ctx.t("Verification mode"))
    )
}

// --------------------------------------------------------------------- evals

fn read_eval_json(ctx: &UiCtx, name: &str) -> Option<Value> {
    let data: Value = serde_json::from_str(&std::fs::read_to_string(ctx.state.evals_dir.join(name)).ok()?).ok()?;
    data.is_object().then_some(data)
}

fn eval_card(ctx: &UiCtx, title: &str, data: &Option<Value>, empty_hint: &str, extra: &str) -> String {
    let head = format!("<div class='section-header'><h2>{}</h2></div>", escape(title));
    let data = match data {
        Some(d) if d.as_object().is_some_and(|o| !o.is_empty()) => d,
        _ => return format!("<section class=\"card\">{head}<p>{}</p></section>", escape(empty_hint)),
    };
    let results: Vec<Value> = data
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let passed = results
        .iter()
        .filter(|r| r.is_object() && truthy(&r["passed"]))
        .count();
    let summary = ctx.tf(
        "{passed}/{total} cases passed",
        &[("passed", &passed.to_string()), ("total", &results.len().to_string())],
    );
    let finished = ctx.tf(
        "Finished {when}",
        &[("when", &data.get("finished_at").map_or_else(String::new, n_of))],
    );
    let table = if results.is_empty() {
        format!("<p>{}</p>", escape(&ctx.t("No cases recorded.")))
    } else {
        let rows: String = results
            .iter()
            .filter(|r| r.is_object())
            .map(|r| {
                let ok = truthy(&r["passed"]);
                let cost_text = match r.get("cost_usd") {
                    Some(Value::Number(n)) => format!("${}", fixed(n.as_f64().unwrap_or(0.0), 2)),
                    _ => "\u{2014}".to_string(),
                };
                let reasons = r
                    .get("reasons")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(n_of).collect::<Vec<_>>().join("; "))
                    .unwrap_or_default();
                format!(
                    "<tr><td><span class='material-symbols-outlined' aria-hidden='true'>{}</span> {}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    if ok { "check_circle" } else { "cancel" },
                    escape(&ctx.t(if ok { "Passed" } else { "Failed" })),
                    escape(&r.get("name").map_or_else(String::new, n_of)),
                    escape(&cost_text),
                    escape(&reasons)
                )
            })
            .collect();
        format!(
            "<div class='table-wrap'><table class='daemons'><thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>{rows}</tbody></table></div>",
            escape(&ctx.t("Result")),
            escape(&ctx.t("Case")),
            escape(&ctx.t("Cost")),
            escape(&ctx.t("Reasons"))
        )
    };
    format!(
        "<section class=\"card\">{head}<p>{} \u{b7} {}</p>{extra}{table}</section>",
        escape(&summary),
        escape(&finished)
    )
}

fn evals_body(ctx: &UiCtx) -> String {
    let scripted = read_eval_json(ctx, "last.json");
    let golden = read_eval_json(ctx, "golden-last.json");
    let mut extra = String::new();
    if let Some(g) = golden.as_ref().filter(|g| g.as_object().is_some_and(|o| !o.is_empty())) {
        if let Some(spent) = g.get("spent_usd").and_then(|v| v.as_number()).and_then(|n| n.as_f64()) {
            let budget = g
                .get("budget_usd")
                .and_then(|v| v.as_number())
                .and_then(|n| n.as_f64())
                .map_or_else(|| "?".to_string(), |b| fixed(b, 2));
            let line = ctx.tf(
                "Spent ${spent} of ${budget} budget",
                &[("spent", &fixed(spent, 2)), ("budget", &budget)],
            );
            extra.push_str(&format!("<p>{}</p>", escape(&line)));
        }
        let not_run: Vec<String> = g
            .get("not_run")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(n_of).collect())
            .unwrap_or_default();
        if !not_run.is_empty() {
            extra.push_str(&format!(
                "<p>{}: {}</p>",
                escape(&ctx.t("Not run (budget reached)")),
                escape(&not_run.join(", "))
            ));
        }
    }
    format!(
        "\n{}{}\n{}\n",
        page_title(ctx, "Evals", "Last scripted and golden evaluation runs."),
        eval_card(
            ctx,
            &ctx.t("Scripted evals"),
            &scripted,
            &ctx.t("No scripted run yet. Run `loop eval` to record one."),
            ""
        ),
        eval_card(
            ctx,
            &ctx.t("Golden evals"),
            &golden,
            &ctx.t("No golden run yet. Run `loop eval --golden` to record one."),
            &extra
        )
    )
}

// --------------------------------------------------------------------- audit

fn audit_body(ctx: &UiCtx) -> String {
    let heading = page_title(ctx, "Audit", "Loop Ready Score for every loop definition under loops/.");
    let paths = loop_yamls(&ctx.state.repo_root.join("loops"));
    if paths.is_empty() {
        return format!(
            "\n{heading}<div class=\"grid\"><section class=\"card\">\n<p>{}</p>\n</section></div>\n",
            escape(&ctx.t("No loop definitions found under loops/."))
        );
    }
    let cards: String = paths
        .iter()
        .filter_map(|path| {
            let (definition, nulls) = loop_audit::load(path).ok()?;
            let report = loop_audit::audit_with(&definition, nulls, &loopx_core::policy::PolicyEngine::new());
            let check_items: String = report
                .checks
                .iter()
                .map(|c| {
                    format!(
                        "<li><span class='pill {}'>{}</span> <strong>{}</strong> &mdash; {}</li>",
                        three_state_pill_class(c.status.as_str()),
                        escape(c.status.as_str()),
                        escape(c.name),
                        escape(&c.detail)
                    )
                })
                .collect();
            let score_text = report
                .score
                .map_or_else(|| ctx.t("N/A"), |s| format!("{} / 100", fixed(s, 0)));
            let partial = if report.is_partial {
                format!(
                    "<p class='subtitle'>{}</p>",
                    escape(&ctx.tf(
                        "Partial - missing: {components}",
                        &[("components", &report.missing_components.join(", "))]
                    ))
                )
            } else {
                String::new()
            };
            Some(format!(
                "\n<section class=\"card\">\n<div class=\"section-header\">{}<h2>{}</h2></div>\n<p>{}</p>\n{partial}\n<ul class='plain'>{check_items}</ul>\n</section>\n",
                icon("_SECTION_ICON_AUDIT"),
                escape(&definition.name),
                ctx.tf(
                    "Loop Ready Score: {score}",
                    &[("score", &format!("<strong>{}</strong>", escape(&score_text)))]
                )
            ))
        })
        .collect();
    format!("\n{heading}\n<div class=\"grid\">\n{cards}\n</div>\n")
}
