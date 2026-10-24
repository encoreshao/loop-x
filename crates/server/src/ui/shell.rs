//! `render_shell`: the `<!doctype html>` skeleton every page shares, a port of
//! `dashboard_server._render_shell` (head with the stylesheet and scripts, the
//! sidebar, the topbar with its AI button, status pill, language menu and help
//! link, the update banner, the AI side panel, and the closing scripts). The
//! literal markup and JavaScript come from the templates the extractor writes, so
//! only the data that varies per request is assembled here.

use super::i18n::{self, Lang};
use super::routes::UiCtx;
use super::template::{Scope, Template};
use super::{assets, badge, consts, csrf, nav, pyfmt, update};
use serde_json::{json, Value};

/// The auto-refresh switches of `_render_shell(refresh=, refresh_note=, lazy_refresh=)`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShellOpts {
    pub refresh: bool,
    pub refresh_note: bool,
    pub lazy_refresh: bool,
}

fn render(name: &str, scope: &Scope) -> String {
    Template::named(name)
        .render(scope)
        .unwrap_or_else(|e| panic!("{e}"))
}

fn escaped_t(lang: Lang, text: &str) -> String {
    pyfmt::escape(&i18n::t(lang, text, &[]))
}

fn json_t(lang: Lang, text: &str) -> String {
    pyfmt::json_str(&i18n::t(lang, text, &[]))
}

/// `_chat_tool_chips_js()`.
fn chat_tool_chips_js(lang: Lang) -> String {
    let mut labels = serde_json::Map::new();
    let names = [
        ("gitlab.read_issue", "Reading issue"),
        ("gitlab.list_issues", "Listing issues"),
        ("gitlab.comment", "Commenting on issue"),
        ("github.read_issue", "Reading issue"),
        ("github.read_pr", "Reading pull request"),
        ("github.comment", "Commenting"),
        ("rss.read_feed", "Reading feed"),
        ("calendar.list_events", "Reading calendar"),
        ("mail.search", "Searching mail"),
        ("mail.read", "Reading email"),
        ("mail.send", "Sending email"),
        ("calendar.create_event", "Creating event"),
        ("docs.save", "Saving document"),
    ];
    for (name, label) in names {
        labels.insert(name.to_string(), json!(i18n::t(lang, label, &[])));
    }
    consts::ds("_CHAT_TOOL_CHIPS_JS")
        .replace(
            "__TOOL_LABELS__",
            &loopx_core::pydumps::dumps(&Value::Object(labels)),
        )
        .replace("__STATUS_TEXT__", &json_t(lang, "Analyzing results…"))
}

/// `_chat_pending_card_js()`.
fn chat_pending_card_js(lang: Lang) -> String {
    consts::ds("_CHAT_PENDING_CARD_JS")
        .replace("__WAITING__", &json_t(lang, "Waiting for your approval"))
        .replace("__APPROVE__", &json_t(lang, "Approve"))
        .replace("__REJECT__", &json_t(lang, "Reject"))
        .replace("__APPROVED__", &json_t(lang, "Approved."))
        .replace("__REJECTED__", &json_t(lang, "Rejected."))
        .replace(
            "__ERROR__",
            &json_t(lang, "Something went wrong - try again."),
        )
}

fn ai_panel_script(lang: Lang) -> String {
    consts::ds("_AI_PANEL_SCRIPT")
        .replace(
            "__ERROR_TEXT__",
            &json_t(lang, "Something went wrong - try again."),
        )
        .replace("__INTERRUPTED_TEXT__", &json_t(lang, "(reply interrupted)"))
        .replace("__THINKING_TEXT__", &json_t(lang, "Thinking…"))
        .replace(
            "__THINKING_HTML__",
            &pyfmt::json_str(consts::ds("_AI_THINKING_HTML")),
        )
        .replace("__YOU__", &json_t(lang, "You"))
        .replace(
            "__BRAND_ICON__",
            &pyfmt::json_str(consts::ds("_MESSAGE_BRAND_ICON")),
        )
}

/// `_ai_panel_html(active_page)`.
pub fn ai_panel_html(ctx: &UiCtx, active_page: &str) -> String {
    let lang = ctx.lang;
    let key = nav::nav_key(active_page);
    let prompts_table = consts::value("dashboard_server", "_AI_PANEL_PROMPTS");
    let prompts = prompts_table
        .and_then(|t| t.get(&key))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_else(|| consts::list("dashboard_server", "_AI_PANEL_DEFAULT_PROMPTS"));
    let page_label = consts::list("dashboard_server", "_NAV_ITEMS")
        .iter()
        .find(|item| item[0].as_str() == Some(&key))
        .and_then(|item| item[2].as_str());
    let context_html = match page_label {
        Some(label) => {
            let mut scope = Scope::new(lang);
            scope.var("page_label", label);
            render("ai_panel_context", &scope)
        }
        None => String::new(),
    };
    let mut prompts_html = String::new();
    for p in prompts {
        let mut scope = Scope::new(lang);
        scope
            .var("prompt", p[2].as_str().unwrap_or(""))
            .var(
                "send",
                if p[3].as_bool().unwrap_or(false) {
                    "1"
                } else {
                    ""
                },
            )
            .var("icon", p[0].as_str().unwrap_or(""))
            .var("label", p[1].as_str().unwrap_or(""));
        prompts_html.push_str(&render("ai_panel_prompt", &scope));
    }
    let mut scope = Scope::new(lang);
    scope
        .set(
            "html.escape(active_page, quote=True)",
            pyfmt::escape(active_page),
        )
        .var("panel_label", escaped_t(lang, "Loop X assistant"))
        .var("history", escaped_t(lang, "Chat history"))
        .var("new_chat", escaped_t(lang, "New chat"))
        .var("close", escaped_t(lang, "Close assistant"))
        .var("context_html", context_html)
        .var("prompts_html", prompts_html)
        .set(
            "html.escape(_CSRF_TOKEN)",
            pyfmt::escape(&ctx.state.csrf_token),
        );
    render("ai_panel", &scope)
}

fn lang_switch_html(lang: Lang) -> String {
    let mut options = String::new();
    for code in Lang::ALL {
        let mut scope = Scope::new(lang);
        scope
            .var("code", code.code())
            .var("current_lang", lang.code())
            .var("name", code.display_name());
        options.push_str(&render("shell_lang_option", &scope));
    }
    let mut scope = Scope::new(lang);
    scope
        .var("language_label", escaped_t(lang, "Language"))
        .var("current_lang", lang.code())
        .var("lang_options_html", options);
    render("shell_lang_switch", &scope)
}

/// `_render_shell(title, active_page, status_badge_html, body_html, ...)`.
pub fn render_shell(
    ctx: &UiCtx,
    title: &str,
    active_page: &str,
    status_badge_html: &str,
    body_html: &str,
    opts: ShellOpts,
) -> String {
    let lang = ctx.lang;
    let state = ctx.state;

    let (page_title, sep, product) = match title.split_once(" · ") {
        Some((page, product)) => (page, " · ", product),
        None => (title, "", ""),
    };
    let title = format!("{}{sep}{product}", i18n::t(lang, page_title, &[]));

    let ai_cli_badge_html = badge::ai_cli_badge(state, lang);
    let refresh_html = if opts.refresh_note {
        render("shell_refresh_html", &Scope::new(lang))
    } else {
        String::new()
    };

    let (update_status, update_progress) = update::read_both(&state.update_dir);
    let update_chip_html = update::render_chip(lang, &update_status);
    let update_banner_html = if active_page == "update" {
        String::new()
    } else {
        update::render_banner(
            lang,
            &update_status,
            &update_progress,
            &csrf::input_html(&state.csrf_token),
        )
    };

    let mut scope = Scope::new(lang);
    scope.var("ai_label", escaped_t(lang, "Ask Loop X"));
    let ai_trigger_html = render("shell_ai_trigger", &scope);
    let mut scope = Scope::new(lang);
    scope.var("help_label", escaped_t(lang, "Help"));
    let help_link_html = render("shell_help_link", &scope);

    let refresh_schedule_script = if opts.refresh {
        let action = Template::named(if opts.lazy_refresh {
            "shell_refresh_action_lazy"
        } else {
            "shell_refresh_action_reload"
        })
        .render(&Scope::new(lang))
        .expect("refresh action");
        let mut scope = Scope::new(lang);
        scope.var("refresh_action", action);
        render("shell_refresh_schedule", &scope)
    } else {
        String::new()
    };
    let refresh_note_script = if opts.refresh_note {
        Template::named("shell_refresh_note")
            .render(&Scope::new(lang))
            .expect("refresh note")
            .replace(
                "__REFRESH_TEMPLATE__",
                &json_t(lang, "auto-refreshes every {interval}"),
            )
    } else {
        String::new()
    };

    let mut scope = Scope::new(lang);
    scope
        .var("current_lang", lang.code())
        .var("title", title)
        .var("status_badge_html", status_badge_html)
        .var("body_html", body_html)
        .var("update_chip_html", update_chip_html)
        .var("update_banner_html", update_banner_html)
        .var("ai_trigger_html", ai_trigger_html)
        .var("ai_cli_badge_html", ai_cli_badge_html)
        .var("refresh_html", refresh_html)
        .var("lang_switch_html", lang_switch_html(lang))
        .var("help_link_html", help_link_html)
        .var("refresh_schedule_script", refresh_schedule_script)
        .var("refresh_note_script", refresh_note_script)
        .var("ai_panel_script", ai_panel_script(lang))
        .set("_favicon_version()", assets::favicon_version())
        .set("_chat_tool_chips_js()", chat_tool_chips_js(lang))
        .set("_chat_pending_card_js()", chat_pending_card_js(lang))
        .set(
            "_sidebar_html(active_page)",
            nav::sidebar_html(state, lang, active_page),
        )
        .set(
            "_ai_panel_html(active_page)",
            ai_panel_html(ctx, active_page),
        );
    render("shell", &scope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::routes::UiRequest;
    use crate::ui::state::UiState;

    #[test]
    fn the_shell_renders_with_every_slot_resolved() {
        let state = UiState::for_tests();
        let req = UiRequest::get("/");
        let ctx = UiCtx::new(&state, &req);
        let html = render_shell(
            &ctx,
            "README · Loop X Engineering",
            "readme",
            "<b>badge</b>",
            "<p>body</p>",
            ShellOpts::default(),
        );
        assert!(html.starts_with("<!doctype html>\n<html lang=\"en\">"));
        assert!(html.contains("<title>README · Loop X Engineering</title>"));
        assert!(html.contains("<p>body</p>"));
        assert!(html.contains("<b>badge</b>"));
        assert!(!html.contains('\u{27e6}'));
        assert!(html.contains("name='csrf_token' value=\"test-csrf-token\""));
    }

    #[test]
    fn refresh_options_add_the_timer_scripts() {
        let state = UiState::for_tests();
        let req = UiRequest::get("/");
        let ctx = UiCtx::new(&state, &req);
        let on = render_shell(
            &ctx,
            "Logs · X",
            "logs",
            "",
            "",
            ShellOpts {
                refresh: true,
                refresh_note: true,
                lazy_refresh: false,
            },
        );
        assert!(on.contains("location.reload();"));
        assert!(on.contains("id='refresh-note-text'"));
        let off = render_shell(&ctx, "Logs · X", "logs", "", "", ShellOpts::default());
        assert!(!off.contains("location.reload();"));
    }
}
