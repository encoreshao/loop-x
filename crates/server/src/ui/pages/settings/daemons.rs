//! Daemons view: launchd agents of this checkout plus the loops registry.

use super::super::loops_common::{custom_select, flash_html, relative_time};
use crate::ui::consts;
use crate::ui::pyfmt;
use crate::ui::routes::UiCtx;
use crate::ui::template::{Scope, Template};
use loopx_core::launchd::{self, Daemon};
use loopx_core::loops_config::{self, py_truthy};
use serde_json::Value;
use std::collections::BTreeSet;

pub const RETURN_TO_DEFAULT: &str = "/settings?view=daemons";
pub const RETURN_TO_ALLOWED: [&str; 2] = ["/loops", RETURN_TO_DEFAULT];

pub fn launchctl(ctx: &UiCtx) -> launchd::Launchctl {
    launchd::Launchctl {
        command: ctx.state.launchctl.clone(),
        list_file: Some(ctx.state.repo_root.join("outputs").join("launchctl_list.txt")),
    }
}

fn weekday_labels() -> Vec<(String, String)> {
    consts::list("dashboard_server", "_WEEKDAY_LABELS")
        .iter()
        .map(|p| {
            (
                p[0].as_str().unwrap_or("").to_string(),
                p[1].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

fn days_of_month() -> Vec<String> {
    (1..=31).map(|d| d.to_string()).collect()
}

/// `_schedule_form_html`: one daemon's schedule editor.
fn schedule_form(ctx: &UiCtx, d: &Daemon, csrf: &str) -> String {
    let entries: Vec<&Value> = match d.schedule.as_ref() {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(v) if py_truthy(v) => vec![v],
        _ => Vec::new(),
    };
    let objs = || entries.iter().filter_map(|e| e.as_object());
    let mut hours: Vec<&Value> = Vec::new();
    let mut minutes: Vec<&Value> = Vec::new();
    for e in objs() {
        for (key, set) in [("Hour", &mut hours), ("Minute", &mut minutes)] {
            let v = e.get(key).unwrap_or(&Value::Null);
            if !set.contains(&v) {
                set.push(v);
            }
        }
    }
    let hour = if hours.len() == 1 { hours[0].as_i64().unwrap_or(9) } else { 9 };
    let minute = if minutes.len() == 1 { minutes[0].as_i64().unwrap_or(0) } else { 0 };
    let day_of_month = objs().find_map(|e| e.get("Day").cloned());
    let mut selected: BTreeSet<i64> = objs().filter_map(|e| e.get("Weekday").and_then(Value::as_i64)).collect();
    let frequency = if day_of_month.as_ref().is_some_and(py_truthy) {
        "Monthly"
    } else if !selected.is_empty() {
        "Weekly"
    } else {
        "Daily"
    };
    if selected.is_empty() {
        selected = (0..7).collect();
    }
    let checkboxes: String = weekday_labels()
        .iter()
        .map(|(value, label)| {
            let on = value.parse::<i64>().is_ok_and(|v| selected.contains(&v));
            format!(
                "<label class='md-checkbox weekday-check'><input type='checkbox' name='weekday' value='{value}'{}> {}</label>",
                if on { " checked" } else { "" },
                pyfmt::escape(&ctx.t(label))
            )
        })
        .collect();
    let day = day_of_month
        .as_ref()
        .filter(|v| py_truthy(v))
        .map(loops_config::py_str)
        .unwrap_or_else(|| "1".to_string());
    let day_select = custom_select_plain_str("day_of_month", &days_of_month(), &day);
    let freq_opts: Vec<(String, String)> = ["Daily", "Weekly", "Monthly"]
        .iter()
        .map(|f| (f.to_string(), ctx.t(f)))
        .collect();
    let freq_select = custom_select("frequency", &freq_opts, Some(frequency), None);
    let weekly = if frequency == "Weekly" { "" } else { " style='display:none'" };
    let monthly = if frequency == "Monthly" { "" } else { " style='display:none'" };
    format!(
        "<form method='post' action='/daemons/{}/schedule' class='daemon-action-form schedule-form'>\
{csrf}\
<input type='time' name='time' value='{hour:02}:{minute:02}'>\
{freq_select}\
<span class='weekday-checks weekly-controls'{weekly}>{checkboxes}</span>\
<span class='monthly-controls'{monthly}>{}</span>\
<button type='submit' class='btn btn-neutral'>{}</button>\
</form>",
        pyfmt::escape(&d.file),
        ctx.tf("on day {day}", &[("day", &day_select)]),
        pyfmt::escape(&ctx.t("Save schedule"))
    )
}

fn custom_select_plain_str(name: &str, options: &[String], selected: &str) -> String {
    super::super::loops_common::custom_select_plain(name, options, Some(selected), None)
}

fn return_to_input(return_to: Option<&str>) -> String {
    match return_to {
        Some(r) if !r.is_empty() => format!(
            "<input type='hidden' name='return_to' value='{}'>",
            pyfmt::escape(r)
        ),
        _ => String::new(),
    }
}

/// `_loop_schedule_form_html`.
fn loop_schedule_form(ctx: &UiCtx, entry: &Value, csrf: &str) -> String {
    let empty = serde_json::Map::new();
    let schedule = entry.get("schedule").and_then(Value::as_object).unwrap_or(&empty);
    let freq_key = match schedule.get("frequency").and_then(Value::as_str) {
        Some(f @ ("daily" | "weekly" | "monthly" | "hourly")) => f.to_string(),
        _ => {
            if schedule.get("weekdays").and_then(Value::as_str) == Some("all") {
                "daily".to_string()
            } else {
                "weekly".to_string()
            }
        }
    };
    let mut frequency = freq_key.clone();
    frequency[..1].make_ascii_uppercase();
    let int_of = |key: &str, default: i64| -> Option<i64> {
        match schedule.get(key) {
            None => Some(default),
            Some(v) => v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())),
        }
    };
    let time_value = match (int_of("hour", 9), int_of("minute", 0)) {
        (Some(h), Some(m)) => format!("{h:02}:{m:02}"),
        _ => "09:00".to_string(),
    };
    let selected: BTreeSet<String> = match schedule.get("weekdays") {
        Some(Value::Array(a)) => a.iter().map(loops_config::py_str).collect(),
        _ => consts::list("dashboard_server", "_LOOP_WEEKDAY_LABELS")
            .iter()
            .map(|p| p[0].as_str().unwrap_or("").to_string())
            .collect(),
    };
    let checkboxes: String = consts::list("dashboard_server", "_LOOP_WEEKDAY_LABELS")
        .iter()
        .map(|p| {
            let (value, label) = (p[0].as_str().unwrap_or(""), p[1].as_str().unwrap_or(""));
            format!(
                "<label class='md-checkbox weekday-check'><input type='checkbox' name='weekday' value='{value}'{}> {label}</label>",
                if selected.contains(value) { " checked" } else { "" }
            )
        })
        .collect();
    let day = schedule
        .get("day")
        .filter(|v| py_truthy(v))
        .map(loops_config::py_str)
        .unwrap_or_else(|| "1".to_string());
    let day_select = custom_select_plain_str("day_of_month", &days_of_month(), &day);
    let interval_value = match schedule.get("interval_minutes").filter(|v| py_truthy(v)) {
        Some(m) => format!("{}m", loops_config::py_str(m)),
        None => schedule
            .get("interval_hours")
            .filter(|v| py_truthy(v))
            .map(loops_config::py_str)
            .unwrap_or_else(|| "4".to_string()),
    };
    let mut interval_options: Vec<(String, String)> = Vec::new();
    for m in consts::list("dashboard_server", "_LOOP_MINUTE_INTERVAL_CHOICES") {
        let m = m.as_str().unwrap_or("");
        interval_options.push((format!("{m}m"), ctx.tf("{n} minutes", &[("n", m)])));
    }
    for h in consts::list("dashboard_server", "_LOOP_HOURLY_INTERVAL_CHOICES") {
        let h = h.as_str().unwrap_or("");
        interval_options.push((h.to_string(), ctx.tf("{n} hour(s)", &[("n", h)])));
    }
    let interval_select = custom_select("interval_hours", &interval_options, Some(&interval_value), None);
    let freqs: Vec<String> = ["Daily", "Weekly", "Monthly", "Hourly"].iter().map(|s| s.to_string()).collect();
    let freq_select = super::super::loops_common::custom_select_plain("frequency", &freqs, Some(&frequency), None);
    let hide = |show: bool| if show { "" } else { " style='display:none'" };
    let name = pyfmt::escape(&entry.get("name").map_or("?".to_string(), loops_config::py_str));
    format!(
        "<form method='post' action='/daemons/loops/{name}/schedule' class='daemon-action-form schedule-form'>\
{csrf}{}\
<span class='time-control'{}><input type='time' name='time' value='{time_value}'></span>\
{freq_select}\
<span class='weekday-checks weekly-controls'{}>{checkboxes}</span>\
<span class='monthly-controls'{}>on day {day_select}</span>\
<span class='hourly-controls'{}>{} {interval_select}</span>\
<button type='submit' class='btn btn-neutral'>Save schedule</button>\
</form>",
        return_to_input(None),
        hide(frequency != "Hourly"),
        hide(frequency == "Weekly"),
        hide(frequency == "Monthly"),
        hide(frequency == "Hourly"),
        pyfmt::escape(&ctx.t("every")),
    )
}

/// `_loop_action_html`.
fn loop_action(entry: &Value, csrf: &str) -> String {
    let name = pyfmt::escape(&entry.get("name").map_or("?".to_string(), loops_config::py_str));
    let enabled = entry.get("enabled").is_none_or(py_truthy);
    if enabled {
        format!(
            "<form method='post' action='/daemons/loops/{name}/disable' class='daemon-action-form'>\
{csrf}{}\
<button type='submit' class='switch is-on' role='switch' aria-checked='true' \
aria-label='Disable {name}' title='Disable {name}'>\
<span class='switch-thumb'></span></button>\
</form>",
            return_to_input(None)
        )
    } else {
        format!(
            "<form method='post' action='/daemons/loops/{name}/enable' class='daemon-action-form'>\
{csrf}{}\
<button type='submit' class='switch is-off' role='switch' aria-checked='false' \
aria-label='Enable {name}' title='Enable {name}'>\
<span class='switch-thumb'></span></button>\
</form>",
            return_to_input(None)
        )
    }
}

/// `_render_registered_loops_section`.
fn registered_loops(ctx: &UiCtx) -> String {
    let empty = "<p>No loops registered yet - see <code>config/loops.json.template</code>.</p>".to_string();
    let Ok(loops) = loops_config::list_loops(
        &ctx.state.loops_config_path(),
        Some(&ctx.state.loops_template_path()),
    ) else {
        return empty;
    };
    if loops.is_empty() {
        return empty;
    }
    let csrf = ctx.csrf_input();
    let mut rows = String::new();
    for entry in &loops {
        let name = entry.get("name").map_or("?".to_string(), loops_config::py_str);
        let status = loopx_core::status::read_status(&ctx.state.status_path_for_loop(&name));
        let badge = crate::ui::badge::status_badge_markup(ctx.lang, &status);
        let updated = status.get("updated_at").and_then(Value::as_str).unwrap_or("");
        let last_run = if updated.is_empty() {
            "never".to_string()
        } else {
            pyfmt::escape(&relative_time(ctx.state.now(), updated))
        };
        rows.push_str(&format!(
            "<tr><td><code>{}</code></td><td>{}</td><td>{badge}</td><td>{last_run}</td><td>{}</td></tr>",
            pyfmt::escape(&name),
            loop_schedule_form(ctx, entry, &csrf),
            loop_action(entry, &csrf)
        ));
    }
    format!(
        "<div class='table-wrap'><table class='daemons'>\
<thead><tr><th>Loop</th><th>Schedule</th><th>Status</th><th>Last run</th><th>Action</th></tr></thead>\
<tbody>{rows}</tbody></table></div>"
    )
}

fn daemon_row(ctx: &UiCtx, d: &Daemon, csrf: &str) -> String {
    if let Some(err) = &d.error {
        return format!(
            "<tr><td>{}</td><td colspan='5'>{}</td></tr>",
            pyfmt::escape(&d.file),
            pyfmt::escape(&ctx.tf("error parsing plist: {error}", &[("error", err)]))
        );
    }
    let label = d.label.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| d.file.clone());
    let safe_file = pyfmt::escape(&d.file);
    let safe_label = pyfmt::escape(&label);
    let (pill, action) = if d.loaded {
        let text = match &d.pid {
            Some(pid) => ctx.tf("loaded (pid {pid})", &[("pid", &pyfmt::escape(pid))]),
            None => ctx.t("loaded"),
        };
        let disable = pyfmt::escape(&ctx.tf("Disable {name}", &[("name", &label)]));
        (
            format!(
                "<span class='pill pill-green'>{}{}</span>",
                consts::ds("_CHECK_ICON"),
                pyfmt::escape(&text)
            ),
            format!(
                "<form method='post' action='/daemons/{safe_file}/disable' class='daemon-action-form'>{csrf}\
<button type='submit' class='switch is-on' role='switch' aria-checked='true' aria-label='{disable}' title='{disable}'>\
<span class='switch-thumb'></span></button></form>"
            ),
        )
    } else {
        let enable = pyfmt::escape(&ctx.tf("Enable {name}", &[("name", &label)]));
        let confirm = pyfmt::escape(&ctx.tf(
            "Enable {name}? This will let it start running on its schedule.",
            &[("name", &label)],
        ));
        (
            format!("<span class='pill pill-grey'>{}</span>", pyfmt::escape(&ctx.t("not loaded"))),
            format!(
                "<form method='post' action='/daemons/{safe_file}/enable' class='daemon-action-form'>{csrf}\
<button type='submit' class='switch is-off' role='switch' aria-checked='false' aria-label='{enable}' title='{enable}' data-confirm=\"{confirm}\">\
<span class='switch-thumb'></span></button></form>"
            ),
        )
    };
    let runs: Vec<String> = d.program_arguments.iter().map(loops_config::py_str).collect();
    let schedule = if d.schedule.is_some() {
        schedule_form(ctx, d, csrf)
    } else {
        "—".to_string()
    };
    format!(
        "<tr><td>{safe_label}</td><td>{pill}</td><td>{}</td><td><code>{}</code></td><td>{schedule}</td><td>{action}</td></tr>",
        pyfmt::escape(&launchd::describe_trigger(d)),
        pyfmt::escape(&runs.join(" "))
    )
}

pub fn body(ctx: &UiCtx, flash: Option<&str>, flash_ok: bool) -> String {
    let ctl = launchctl(ctx);
    let daemons = launchd::get_daemons_status(&ctx.state.repo_root.join("launchd"), &ctl.list_output());
    let csrf = ctx.csrf_input();
    let rows: String = daemons.iter().map(|d| daemon_row(ctx, d, &csrf)).collect();
    let daemons_html = if rows.is_empty() {
        format!("<p>{}</p>", pyfmt::escape(&ctx.t("No launchd agents are installed. In the LoopX app, scheduled loops run from the app itself while it is open (LoopX Settings → Run scheduled loops while LoopX is open), checking every 15 minutes; the schedules are in Registered Loops below.")))
    } else {
        format!(
            "<div class='table-wrap'><table class='daemons'>\
<thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr></thead>\
<tbody>{rows}</tbody></table></div>",
            pyfmt::escape(&ctx.t("Label")),
            pyfmt::escape(&ctx.t("Status")),
            pyfmt::escape(&ctx.t("Trigger")),
            pyfmt::escape(&ctx.t("Runs")),
            pyfmt::escape(&ctx.t("Schedule")),
            pyfmt::escape(&ctx.t("Action"))
        )
    };
    let subtitle = ctx.tf(
        "Every loop the com.hermes.loop-engineering scheduler above runs, one entry per {path} registration - enabling or disabling that one daemon enables or disables all of these together.",
        &[("path", "<code>~/.loopx/loops.json</code>")],
    );
    let mut scope = Scope::new(ctx.lang);
    scope
        .var("flash_html", flash_html(flash, flash_ok))
        .var("daemons_html", daemons_html)
        .var("registered_loops_subtitle", subtitle)
        .var("registered_loops_html", registered_loops(ctx));
    Template::named("settings_daemons_body")
        .render(&scope)
        .unwrap_or_else(|e| panic!("{e}"))
}
