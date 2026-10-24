//! The Topic Monitor loop's page `/loops/topic-loop` (views Live and Topics): every
//! enabled topic's status and latest briefing, and the topic list editor. The POSTs
//! behind the Topics view live in `settings/posts.rs` and `runs.rs`.

use super::chat::topic_latest_data_html;
use super::loops_common::{
    custom_select_plain, empty_state_html, esc_t, flash_html, relative_time, render_loop_page,
    run_now_action_html,
};
use crate::ui::badge::status_badge_markup;
use crate::ui::hub::{builtin_loop_page, resolve_view};
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::{consts, pyfmt};
use loopx_core::{pydumps, status, topics};
use serde_json::{Map, Value};

pub fn register(r: &mut Registry) {
    r.get("/loops/topic-loop", page);
}

fn page(ctx: &UiCtx) -> UiResponse {
    let (flash, flash_ok) = ctx.flash();
    let hub = builtin_loop_page("topic-loop").expect("topic hub");
    let view = ctx.query().first("view").map(str::to_string);
    let body = match resolve_view(&hub, view.as_deref()).key.as_str() {
        "topics" => topics_body(ctx, flash.as_deref(), flash_ok),
        _ => live_body(ctx, flash.as_deref(), flash_ok),
    };
    UiResponse::html(render_loop_page(ctx, &hub, "topic-loop", view.as_deref(), &body, None))
}

fn configured_topics(ctx: &UiCtx) -> Vec<Value> {
    topics::load(&ctx.state.home_dir.join("topics.json")).unwrap_or_default()
}

fn is_enabled(topic: &Value) -> bool {
    topic.get("enabled").and_then(Value::as_bool).unwrap_or(true)
}

fn text(topic: &Value, key: &str) -> String {
    topic.get(key).map(pydumps::py_str).unwrap_or_default()
}

/// `_topic_monitor_body`.
fn live_body(ctx: &UiCtx, flash: Option<&str>, flash_ok: bool) -> String {
    let topics: Vec<Value> = configured_topics(ctx).into_iter().filter(is_enabled).collect();
    let topic_status = status::read_topic_status(
        &ctx.state.repo_root.join("outputs").join("topic-monitor").join("status.json"),
    );
    let empty = Map::new();
    let topic_status = topic_status.get("topics").and_then(Value::as_object).unwrap_or(&empty);
    let any_running = topic_status
        .values()
        .any(|e| e.get("state").and_then(Value::as_str) == Some("running"));

    let run_now_form = if any_running || topics.is_empty() {
        String::new()
    } else {
        run_now_action_html(
            ctx,
            "/topic-monitor/run-now",
            &ctx.t("Run the topic monitor loop now? This starts a real automated run outside its normal schedule."),
            &ctx.csrf_input(),
            None,
        )
    };

    let icon = consts::ds("_SECTION_ICON_TOPIC_MONITOR");
    let (topics_html, latest_section) = if topics.is_empty() {
        (
            empty_state_html(
                &esc_t(ctx, "No enabled topics, so there's nothing to monitor."),
                "/loops/topic-loop?view=topics",
                &esc_t(ctx, "Manage topics"),
            ),
            String::new(),
        )
    } else {
        let blocks: String = topics
            .iter()
            .map(|topic| {
                let name = text(topic, "name");
                let label = topic.get("label").map_or_else(|| name.clone(), pydumps::py_str);
                let mut entry = topic_status.get(&name).cloned().unwrap_or_else(|| Value::Object(Map::new()));
                if let Some(o) = entry.as_object_mut() {
                    o.entry("state").or_insert_with(|| Value::String("never_run".into()));
                }
                let badge = status_badge_markup(ctx.lang, &entry);
                let updated = entry.get("updated_at").map(pydumps::py_str).unwrap_or_default();
                let last_run = relative_time(ctx.state.now(), &updated);
                let last_run_html = if last_run.is_empty() {
                    String::new()
                } else {
                    format!(
                        " <span class='topic-last-run'>{}</span>",
                        pyfmt::escape(&ctx.tf("last run {when}", &[("when", &last_run)]))
                    )
                };
                format!(
                    "<div class='project-block'><h3>{} {badge}{last_run_html}</h3></div>",
                    pyfmt::escape(&label)
                )
            })
            .collect();
        let latest = format!(
            "\n<div class=\"grid\">\n<section class=\"card\">\n<div class=\"section-header\">{icon}<h2>{}</h2></div>\n{}\n</section>\n</div>\n",
            esc_t(ctx, "Latest Data"),
            topic_latest_data_html(ctx.state, ctx.lang, &topics)
        );
        (blocks, latest)
    };

    let history_link = format!("<a href=\"/runs?view=history\">{}</a>", esc_t(ctx, "Run History"));
    let subtitle = ctx.tf(
        "Status for every configured topic. Saved briefings are on the {link} page.",
        &[("link", &history_link)],
    );
    format!(
        "\n<div class=\"page-title\">\n<h1>{}</h1>\n<p class=\"subtitle\">{subtitle}</p>\n</div>\n\n{}\n\n<div class=\"grid\">\n<section class=\"card\">\n<div class=\"section-header\">{icon}<h2>{}</h2></div>\n{topics_html}\n{run_now_form}\n</section>\n</div>\n{latest_section}\n",
        esc_t(ctx, "Topic Monitor"),
        flash_html(flash, flash_ok),
        esc_t(ctx, "Topics"),
    )
}

/// `_topic_action_html`: the enable/disable switch of one topic row.
fn action_html(ctx: &UiCtx, topic: &Value, csrf: &str) -> String {
    let name = text(topic, "name");
    let url_name = pyfmt::quote(&name, "");
    let (verb, class, checked, label) = if is_enabled(topic) {
        ("disable", "is-on", "true", ctx.tf("Disable {name}", &[("name", &name)]))
    } else {
        ("enable", "is-off", "false", ctx.tf("Enable {name}", &[("name", &name)]))
    };
    let label = pyfmt::escape(&label);
    format!(
        "<form method='post' action='/topic-monitor/topics/{url_name}/{verb}' class='daemon-action-form topic-row-switch'>{csrf}<button type='submit' class='switch {class}' role='switch' aria-checked='{checked}' aria-label='{label}' title='{label}'><span class='switch-thumb'></span></button></form>"
    )
}

/// `_topic_settings_body`.
fn topics_body(ctx: &UiCtx, flash: Option<&str>, flash_ok: bool) -> String {
    let topics = configured_topics(ctx);
    let gitlab: Value = std::fs::read_to_string(ctx.state.user_home.join(".gitlab").join("config.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let bundles: Vec<String> = gitlab
        .get("bundles")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let csrf = ctx.csrf_input();
    let empty_label = ctx.t("(use default webhook)");
    let e = |s: &str| pyfmt::escape(&ctx.t(s));

    let rows: String = topics
        .iter()
        .enumerate()
        .map(|(index, topic)| {
            let name = text(topic, "name");
            let safe_name = pyfmt::escape(&name);
            let url_name = pyfmt::quote(&name, "");
            let delete_confirm = pyfmt::escape(&ctx.tf(
                "Delete topic {name}? This does not delete its saved briefings.",
                &[("name", &name)],
            ));
            let disabled = if is_enabled(topic) { "" } else { " is-disabled" };
            let bundle = text(topic, "slack_bundle");
            let select = custom_select_plain("slack_bundle", &bundles, Some(&bundle), Some(&empty_label));
            format!(
                "\n<div class='project-block topic-settings-row{disabled}'>\n<div class='topic-row'>\n{action}\n<form method='post' action='/topic-monitor/topics' class='daemon-action-form topic-row-fields' id='topic-edit-form-{index}'>\n{csrf}\n<input type='hidden' name='original_name' value='{safe_name}'>\n<div class='topic-row-line1'>\n<input type='text' name='label' value='{label}' placeholder='{ph_label}' required>\n<input type='text' name='name' value='{safe_name}' placeholder='{ph_name}' class='topic-row-name-input' required>\n{select}\n</div>\n<textarea name='brief' rows='2' placeholder='{ph_brief}' class='topic-row-brief' required>{brief}</textarea>\n</form>\n<div class='topic-row-actions'>\n<button type='submit' form='topic-edit-form-{index}' class='btn btn-neutral'>\n<span class='material-symbols-outlined' aria-hidden='true'>save</span> {save}</button>\n<form method='post' action='/topic-monitor/topics/{url_name}/delete' id='topic-delete-form-{index}'>{csrf}</form>\n<button type='submit' form='topic-delete-form-{index}' class='btn btn-warning' data-confirm=\"{delete_confirm}\">\n<span class='material-symbols-outlined' aria-hidden='true'>delete</span> {delete}</button>\n</div>\n</div>\n</div>\n",
                action = action_html(ctx, topic, &csrf),
                label = pyfmt::escape(&text(topic, "label")),
                brief = pyfmt::escape(&text(topic, "brief")),
                ph_label = e("label"),
                ph_name = e("topic name"),
                ph_brief = e("what counts as notable"),
                save = e("Save"),
                delete = e("Delete"),
            )
        })
        .collect();

    let add_select = custom_select_plain("slack_bundle", &bundles, None, Some(&empty_label));
    let add_form = format!(
        "\n<div class='project-block topic-settings-row'>\n<div class='topic-row'>\n<div class='topic-row-switch-spacer'></div>\n<form method='post' action='/topic-monitor/topics' class='daemon-action-form topic-row-fields' id='topic-add-form'>\n{csrf}\n<div class='topic-row-line1'>\n<input type='text' name='name' placeholder='{ph_name}' required>\n<input type='text' name='label' placeholder='{ph_label}' required>\n{add_select}\n</div>\n<textarea name='brief' rows='2' placeholder='{ph_brief}' class='topic-row-brief' required></textarea>\n</form>\n<div class='topic-row-actions'>\n<button type='submit' form='topic-add-form' class='btn btn-neutral'><span class='material-symbols-outlined' aria-hidden='true'>add</span> {add}</button>\n</div>\n</div>\n</div>\n",
        ph_name = e("topic name"),
        ph_label = e("label"),
        ph_brief = e("what counts as notable"),
        add = e("Add topic"),
    );

    let monitor_link = format!("<a href=\"/loops/topic-loop\">{}</a>", e("Topic Monitor"));
    let subtitle = ctx.tf(
        "Add, edit, or delete topics. Live status is on the {link} page.",
        &[("link", &monitor_link)],
    );
    format!(
        "\n<div class=\"page-title\">\n<h1>{title}</h1>\n<p class=\"subtitle\">{subtitle}</p>\n</div>\n\n{flash}\n\n<div class=\"grid\">\n<section class=\"card\">\n<div class=\"section-header\">{icon}<h2>{title}</h2></div>\n{rows}\n{add_form}\n</section>\n</div>\n",
        title = e("Topic Settings"),
        flash = flash_html(flash, flash_ok),
        icon = consts::ds("_SECTION_ICON_SETTINGS"),
    )
}
