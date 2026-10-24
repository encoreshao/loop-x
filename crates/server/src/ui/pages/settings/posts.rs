//! POST routes behind the Settings hub and the topic/history actions of the
//! Python `do_POST` that no other page module owns: launchd daemons, the loops
//! registry's enable/disable/schedule, skills install, chat tools, Slack
//! notifications, AI CLI, custom instructions and topic CRUD (history delete lives in `runs.rs`).
//! Every handler is CSRF-checked by the registry (`Registry::post`).

use super::super::connectors as conn;
use super::daemons::{launchctl, RETURN_TO_ALLOWED, RETURN_TO_DEFAULT};
use super::skills::{chat_tools_path, install_status_path};
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::pyfmt::Form;
use loopx_core::msg::Msg;
use loopx_core::{
    ai_cli_config, connectors, connectors_config as cc, launchd, loops_config,
    settings_store as store, status, topics,
};
use serde_json::{json, Value};
use std::path::PathBuf;

const TOPIC_LOOP: &str = "/loops/topic-loop?view=topics";

pub fn register(r: &mut Registry) {
    r.post("/skills/install", skills_install);
    r.post("/daemons/loops/:name/enable", loop_enable);
    r.post("/daemons/loops/:name/disable", loop_disable);
    r.post("/daemons/loops/:name/schedule", loop_schedule);
    r.post("/daemons/:file/enable", daemon_enable);
    r.post("/daemons/:file/disable", daemon_disable);
    r.post("/daemons/:file/schedule", daemon_schedule);
    r.post("/settings/chat-tools", chat_tools);
    r.post("/notifications/webhook", webhook);
    r.post("/notifications/block-templates", block_template_save);
    r.post("/notifications/block-templates/:name/delete", block_template_delete);
    r.post("/notifications/block-templates/:name/test", block_template_test);
    r.post("/ai-cli", ai_cli);
    r.post("/instructions", instructions);
    r.post("/topic-monitor/topics", topic_save);
    r.post("/topic-monitor/topics/:name/delete", topic_delete);
    r.post("/topic-monitor/topics/:name/enable", topic_enable);
    r.post("/topic-monitor/topics/:name/disable", topic_disable);
}

fn msg(ctx: &UiCtx, m: &Msg) -> String {
    match m {
        Msg::Tr(text, args) => {
            let refs: Vec<(&str, &str)> =
                args.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            ctx.tf(text, &refs)
        }
        Msg::Raw(s) => s.clone(),
    }
}

fn outcome(ctx: &UiCtx, (ok, m): (bool, Msg), location: &str) -> UiResponse {
    UiResponse::flash_redirect(ok, &msg(ctx, &m), location)
}

fn plain(ok: bool, message: String, location: &str) -> UiResponse {
    UiResponse::flash_redirect(ok, &message, location)
}

fn result(r: Result<String, String>, location: &str) -> UiResponse {
    match r {
        Ok(m) => plain(true, m, location),
        Err(m) => plain(false, m, location),
    }
}

fn loops_paths(ctx: &UiCtx) -> (PathBuf, PathBuf) {
    (ctx.state.loops_config_path(), ctx.state.loops_template_path())
}

fn topics_path(ctx: &UiCtx) -> PathBuf {
    ctx.state.home_dir.join("topics.json")
}

/// The form's `return_to` only when it is exactly one of the allowlisted
/// targets (never a prefix/URL check, so it cannot be an open redirect).
fn return_to(form: &Form) -> String {
    let v = form.first("return_to").unwrap_or("");
    if RETURN_TO_ALLOWED.contains(&v) {
        v.to_string()
    } else {
        RETURN_TO_DEFAULT.to_string()
    }
}

// ---- skills ---------------------------------------------------------------------------

fn skills_install(ctx: &UiCtx) -> UiResponse {
    let loc = "/settings?view=skills";
    let status_path = install_status_path(ctx);
    if status::read_status(&status_path).get("state").and_then(Value::as_str) == Some("installing")
    {
        return plain(false, ctx.t("A setup is already in progress"), loc);
    }
    let script = ctx.state.repo_root.join("bin").join("scripts").join("setup.sh");
    if !script.exists() {
        return plain(
            false,
            ctx.tf("setup.sh not found at {path}", &[("path", &script.display().to_string())]),
            loc,
        );
    }
    let log = ctx.state.repo_root.join("outputs").join("skills_install.log");
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = status::write_status(&status_path, "installing", &[]);
    if ctx.state.spawn_processes {
        use std::os::unix::process::CommandExt;
        let q = loops_config::shell_quote;
        let sp = status_path.display().to_string();
        let write = |state: &str| {
            format!(
                "printf '{{\"state\": \"%s\", \"updated_at\": \"%s\"}}' {state} \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\" > {}",
                q(&sp)
            )
        };
        let command = format!(
            "{} >>{} 2>&1; ec=$?; if [ $ec -eq 0 ]; then {}; else {}; fi",
            q(&script.display().to_string()),
            q(&log.display().to_string()),
            write("done"),
            write("failed"),
        );
        let mut cmd = std::process::Command::new("bash");
        cmd.args(["-c", &command])
            .current_dir(&ctx.state.repo_root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // Detach so the restart step at the end cannot kill its own shell.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let _ = cmd.spawn();
    }
    plain(true, ctx.t("Setup started in the background - the dashboard will restart automatically when it's done"), loc)
}

// ---- loops registry ---------------------------------------------------------------------

fn loop_requirements_missing(ctx: &UiCtx, name: &str) -> Vec<String> {
    let (cfg, tpl) = loops_paths(ctx);
    let Ok(l) = loops_config::get_loop(name, &cfg, Some(&tpl)) else {
        return Vec::new();
    };
    let Some(Value::Array(requires)) = l.get("requires") else {
        return Vec::new();
    };
    // A malformed connectors.json counts as "no accounts".
    let accounts = cc::list_accounts(&conn::paths(ctx.state)).unwrap_or_default();
    requires
        .iter()
        .map(loops_config::py_str)
        .filter(|cap| {
            !accounts.iter().any(|a| {
                a.enabled && connectors::get_type(&a.kind).is_some_and(|t| t.has_capability(cap))
            })
        })
        .collect()
}

fn loop_enable(ctx: &UiCtx) -> UiResponse {
    let name = ctx.param("name").to_string();
    let loc = return_to(&ctx.form());
    let missing = loop_requirements_missing(ctx, &name);
    if !missing.is_empty() {
        return plain(
            false,
            ctx.tf(
                "Cannot enable {name}: missing connector for {capabilities}",
                &[("name", &name), ("capabilities", &missing.join(", "))],
            ),
            &loc,
        );
    }
    let (cfg, tpl) = loops_paths(ctx);
    result(loops_config::set_enabled(&name, true, &cfg, Some(&tpl)), &loc)
}

fn loop_disable(ctx: &UiCtx) -> UiResponse {
    let name = ctx.param("name").to_string();
    let loc = return_to(&ctx.form());
    let (cfg, tpl) = loops_paths(ctx);
    result(loops_config::set_enabled(&name, false, &cfg, Some(&tpl)), &loc)
}

fn int(s: &str) -> Result<i64, ()> {
    s.trim().parse::<i64>().map_err(|_| ())
}

fn hour_minute(time: &str) -> Result<(i64, i64), ()> {
    let (h, m) = time.split_once(':').ok_or(())?;
    if m.contains(':') {
        return Err(());
    }
    Ok((int(h)?, int(m)?))
}

fn py_repr(s: &str) -> String {
    loops_config::py_repr_str(s)
}

fn loop_schedule(ctx: &UiCtx) -> UiResponse {
    let name = ctx.param("name").to_string();
    let form = ctx.form();
    let loc = return_to(&form);
    let time = form.first("time").unwrap_or("");
    let frequency = form.first("frequency").unwrap_or("Daily");
    let build = || -> Result<Value, ()> {
        let (hour, minute) = hour_minute(time)?;
        Ok(match frequency {
            "Monthly" => json!({"frequency": "monthly",
                "day": int(form.first("day_of_month").unwrap_or(""))?,
                "hour": hour, "minute": minute}),
            "Weekly" => {
                let mut days = form.all("weekday").iter().map(|v| int(v)).collect::<Result<Vec<_>, _>>()?;
                days.sort_unstable();
                json!({"frequency": "weekly", "weekdays": days, "hour": hour, "minute": minute})
            }
            "Hourly" => {
                let interval = form.first("interval_hours").unwrap_or("");
                match interval.strip_suffix('m') {
                    Some(m) => json!({"frequency": "hourly", "interval_minutes": int(m)?}),
                    None => json!({"frequency": "hourly", "interval_hours": int(interval)?}),
                }
            }
            _ => json!({"frequency": "daily", "hour": hour, "minute": minute}),
        })
    };
    match build() {
        Err(()) => plain(
            false,
            ctx.tf("Invalid schedule value: {value}", &[("value", &py_repr(time))]),
            &loc,
        ),
        Ok(schedule) => {
            let (cfg, tpl) = loops_paths(ctx);
            result(loops_config::set_schedule(&name, &schedule, &cfg, Some(&tpl)), &loc)
        }
    }
}

// ---- launchd daemons --------------------------------------------------------------------

fn dirs(ctx: &UiCtx) -> (PathBuf, PathBuf) {
    (
        ctx.state.repo_root.join("launchd"),
        launchd::launch_agents_dir(&ctx.state.user_home),
    )
}

fn daemon_enable(ctx: &UiCtx) -> UiResponse {
    let (dir, agents) = dirs(ctx);
    outcome(
        ctx,
        launchd::enable_daemon(ctx.param("file"), &dir, &agents, &launchctl(ctx)),
        RETURN_TO_DEFAULT,
    )
}

fn daemon_disable(ctx: &UiCtx) -> UiResponse {
    let (dir, agents) = dirs(ctx);
    outcome(
        ctx,
        launchd::disable_daemon(ctx.param("file"), &dir, &agents, &launchctl(ctx)),
        RETURN_TO_DEFAULT,
    )
}

fn daemon_schedule(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let time = form.first("time").unwrap_or("");
    let frequency = form.first("frequency").unwrap_or("Daily");
    let parsed = (|| -> Result<(i64, i64, Vec<i64>, Option<i64>), ()> {
        let (hour, minute) = hour_minute(time)?;
        if frequency == "Monthly" {
            Ok((hour, minute, Vec::new(), Some(int(form.first("day_of_month").unwrap_or(""))?)))
        } else {
            let days = form.all("weekday").iter().map(|v| int(v)).collect::<Result<Vec<_>, _>>()?;
            Ok((hour, minute, days, None))
        }
    })();
    match parsed {
        Err(()) => plain(
            false,
            ctx.tf(
                "Invalid time, weekday, or day-of-month value: {value}",
                &[("value", &py_repr(time))],
            ),
            RETURN_TO_DEFAULT,
        ),
        Ok((hour, minute, weekdays, day)) => {
            let (dir, agents) = dirs(ctx);
            outcome(
                ctx,
                launchd::update_daemon_schedule(
                    ctx.param("file"),
                    hour,
                    minute,
                    &weekdays,
                    day,
                    &dir,
                    &agents,
                    &launchctl(ctx),
                ),
                RETURN_TO_DEFAULT,
            )
        }
    }
}

// ---- chat tools, notifications, AI CLI, instructions ---------------------------------------

fn chat_tools(ctx: &UiCtx) -> UiResponse {
    let names: Vec<String> = ctx.form().all("auto_approve").to_vec();
    let loc = "/settings?view=chat";
    match store::save_auto_approve(&names, &chat_tools_path(ctx)) {
        Ok(()) => plain(true, ctx.t("Saved"), loc),
        Err(e) => plain(false, e, loc),
    }
}

const NOTIFICATIONS: &str = "/settings?tab=notifications";

fn slack_config(ctx: &UiCtx) -> PathBuf {
    store::slack_config_path(&ctx.state.user_home)
}

fn webhook(ctx: &UiCtx) -> UiResponse {
    let url = ctx.form().first("webhook_url").unwrap_or("").to_string();
    outcome(ctx, store::update_slack_webhook(&url, &slack_config(ctx)), NOTIFICATIONS)
}

fn block_template_save(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let get = |k: &str| form.first(k).unwrap_or("").to_string();
    let blocks = form.first("blocks_json").unwrap_or("[]").to_string();
    outcome(
        ctx,
        store::upsert_block_template(
            &get("name"),
            &blocks,
            &get("notification_key"),
            &get("original_name"),
            &slack_config(ctx),
        ),
        NOTIFICATIONS,
    )
}

fn block_template_delete(ctx: &UiCtx) -> UiResponse {
    outcome(
        ctx,
        store::delete_block_template(ctx.param("name"), &slack_config(ctx)),
        NOTIFICATIONS,
    )
}

fn block_template_test(ctx: &UiCtx) -> UiResponse {
    let defaults = ctx.state.repo_root.join("docs").join("slack-templates");
    outcome(
        ctx,
        store::send_test_block_template(
            ctx.param("name"),
            &slack_config(ctx),
            &defaults,
            &store::post_to_slack,
        ),
        NOTIFICATIONS,
    )
}

fn ai_cli(ctx: &UiCtx) -> UiResponse {
    let cli = ctx.form().first("cli").unwrap_or("").to_string();
    result(
        ai_cli_config::set_selected_cli(&cli, &ctx.state.ai_cli_config_path()),
        "/settings?tab=ai-cli",
    )
}

fn instructions(ctx: &UiCtx) -> UiResponse {
    let text = ctx.form().first("instructions").unwrap_or("").to_string();
    outcome(
        ctx,
        store::write_custom_instructions(&text, &ctx.state.home_dir.join("instructions.md")),
        "/settings?tab=instructions",
    )
}

fn topic_history_dir(ctx: &UiCtx) -> PathBuf {
    ctx.state.repo_root.join("outputs").join("topic-monitor").join("history")
}

// ---- topics -----------------------------------------------------------------------------

fn topic_msg(ctx: &UiCtx, (ok, message): (bool, String)) -> UiResponse {
    plain(ok, ctx.t(&message), TOPIC_LOOP)
}

fn topic_save(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let get = |k: &str| form.first(k).unwrap_or("").to_string();
    let (name, label, brief, bundle) = (get("name"), get("label"), get("brief"), get("slack_bundle"));
    let original = get("original_name").trim().to_string();
    let path = topics_path(ctx);
    if !original.is_empty() && original != name.trim() {
        let r = topics::rename_topic(&original, &name, &path);
        if !r.0 {
            return topic_msg(ctx, r);
        }
        let root = ctx.state.repo_root.join("outputs").join("topic-monitor");
        topics::migrate_topic_rename(
            &original,
            name.trim(),
            &root.join("status.json"),
            &topic_history_dir(ctx),
            &loopx_core::topic_seen::default_state_dir(),
        );
    }
    topic_msg(ctx, topics::upsert_topic(&name, &label, &brief, &bundle, &path))
}

fn topic_delete(ctx: &UiCtx) -> UiResponse {
    topic_msg(ctx, topics::delete_topic(ctx.param("name"), &topics_path(ctx)))
}

fn topic_enable(ctx: &UiCtx) -> UiResponse {
    topic_msg(ctx, topics::set_enabled(ctx.param("name"), true, &topics_path(ctx)))
}

fn topic_disable(ctx: &UiCtx) -> UiResponse {
    topic_msg(ctx, topics::set_enabled(ctx.param("name"), false, &topics_path(ctx)))
}
