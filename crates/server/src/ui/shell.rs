//! `render_shell`: the `<!doctype html>` skeleton every page shares, a port of
//! `dashboard_server._render_shell` (head with the stylesheet and scripts, the
//! sidebar, the topbar with its AI button, status pill, language menu and help
//! link, the update banner, the AI side panel, and the closing scripts). The
//! literal markup and JavaScript come from the templates the extractor writes, so
//! only the data that varies per request is assembled here.

use super::i18n::{self, Lang};
use super::routes::UiCtx;
use super::template::{Scope, Template};
use super::{assets, badge, consts, csrf, icons, nav, pyfmt, update};
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
        ("web.search", "Searching the web"),
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
        .replace("__STOP_TEXT__", &json_t(lang, "Stop"))
        .replace("__STOPPED_TEXT__", &json_t(lang, "(reply stopped)"))
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

/// The toast every action result shows (bottom left, the Office view's card style). A POST
/// redirects back with `flash=<message>&ok=1|0`; this reads those once, shows the message,
/// hides the matching banner and cleans the address bar. Failures stay until dismissed.
fn flash_toast_script(lang: Lang) -> String {
    r#"(function() {
  var stack = null;
  var timers = new Map();
  var STICKY = { failed: 1, waiting: 1 };
  function live() { return stack ? Array.prototype.filter.call(stack.children, function(li) { return !li.classList.contains('leaving'); }) : []; }
  function leave(li) {
    if (li.classList.contains('leaving')) return;
    clearTimeout(timers.get(li));
    timers.delete(li);
    li.classList.add('leaving');
    var gone = function() { li.remove(); };
    li.addEventListener('animationend', gone, { once: true });
    setTimeout(gone, 400);
  }
  // `window.loopxToast(message, ok)` or `(message, {kind, key, onOpen, ttlMs, max})`: a card at
  // the bottom left, newest at the bottom. `kind` is done, failed, waiting, started or step;
  // failed and waiting stay until dismissed, the rest fade after `ttlMs`. A card with a `key`
  // replaces the earlier card of that key, except that an unread failure stays up when a
  // started or step card follows it. A card with `onOpen` is clickable.
  function toast(message, opts) {
    if (!message) return;
    if (typeof opts !== 'object' || !opts) opts = { kind: opts === false ? 'failed' : 'done' };
    var kind = opts.kind || 'done';
    var ttl = opts.ttlMs || 12000;
    var max = opts.max || 5;
    var onOpen = opts.onOpen;
    if (!stack || !stack.isConnected) {
      stack = document.createElement('ol');
      stack.className = 'app-toasts';
      stack.setAttribute('role', 'log');
      stack.setAttribute('aria-live', 'polite');
      document.body.append(stack);
    }
    if (opts.key) {
      live().forEach(function(li) {
        if (li.dataset.key === opts.key && !(li.dataset.kind === 'failed' && (kind === 'started' || kind === 'step'))) {
          clearTimeout(timers.get(li));
          timers.delete(li);
          li.remove();
        }
      });
    }
    var card = document.createElement('li');
    card.className = 'app-toast';
    card.dataset.kind = kind;
    if (opts.key) card.dataset.key = opts.key;
    card.tabIndex = 0;
    if (onOpen) card.classList.add('is-link');
    var dot = document.createElement('span');
    dot.className = 'app-toast-dot';
    dot.setAttribute('aria-hidden', 'true');
    var body = document.createElement('span');
    body.className = 'app-toast-body';
    var title = document.createElement('span');
    title.className = 'app-toast-title';
    title.textContent = message;
    var time = document.createElement('time');
    time.className = 'app-toast-time';
    var now = new Date();
    time.dateTime = now.toISOString();
    time.textContent = now.toLocaleTimeString(document.documentElement.lang || undefined, { hour: '2-digit', minute: '2-digit' });
    body.append(title, time);
    var close = document.createElement('button');
    close.type = 'button';
    close.className = 'app-toast-close';
    close.setAttribute('aria-label', __DISMISS__);
    close.title = __DISMISS__;
    close.textContent = '\u00d7';
    card.append(dot, body, close);
    function arm() {
      clearTimeout(timers.get(card));
      if (STICKY[kind]) return;
      timers.set(card, setTimeout(function() { leave(card); }, ttl));
    }
    close.addEventListener('click', function(e) { e.stopPropagation(); leave(card); });
    if (onOpen) card.addEventListener('click', function() { onOpen(opts.key); });
    card.addEventListener('keydown', function(e) {
      if (e.target !== card) return;
      if (onOpen && (e.key === 'Enter' || e.key === ' ')) { e.preventDefault(); onOpen(opts.key); }
      else if (e.key === 'Escape' || e.key === 'Delete' || e.key === 'Backspace') { e.preventDefault(); leave(card); }
    });
    // Hovering or focusing a card holds it; leaving it starts its full time again.
    card.addEventListener('pointerenter', function() { clearTimeout(timers.get(card)); });
    card.addEventListener('pointerleave', arm);
    card.addEventListener('focusin', function() { clearTimeout(timers.get(card)); });
    card.addEventListener('focusout', function(e) { if (!card.contains(e.relatedTarget)) arm(); });
    stack.append(card);
    arm();
    var shown = live();
    for (var i = 0; i < shown.length - max; i++) leave(shown[i]);
  }
  /// What is on screen, oldest first: `[{key, kind, text}]`.
  toast.items = function() {
    return live().map(function(li) { return { key: li.dataset.key || '', kind: li.dataset.kind, text: li.querySelector('.app-toast-title').textContent }; });
  };
  /// Removes the cards of `key` (all cards without one).
  toast.clear = function(key) {
    live().forEach(function(li) { if (key === undefined || li.dataset.key === key) { clearTimeout(timers.get(li)); timers.delete(li); li.remove(); } });
  };
  window.loopxToast = toast;
  // A POST redirects back with ?flash=<message>&ok=1|0: show it once and tidy the address bar.
  var params = new URLSearchParams(location.search);
  var message = params.get('flash');
  if (!message) return;
  var ok = params.get('ok') !== '0';
  ['flash', 'ok'].forEach(function(k) { params.delete(k); });
  var qs = params.toString();
  try { history.replaceState(null, '', location.pathname + (qs ? '?' + qs : '') + location.hash); } catch (e) {}
  document.querySelectorAll('.flash').forEach(function(b) {
    if (b.textContent.trim() === message) b.hidden = true;
  });
  toast(message, ok);
})();"#
    .replace("__DISMISS__", &json_t(lang, "Dismiss"))
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
            .var("icon", icons::icon(p[0].as_str().unwrap_or("")))
            .var("label", p[1].as_str().unwrap_or(""));
        prompts_html.push_str(&render("ai_panel_prompt", &scope));
    }
    let mut scope = Scope::new(lang);
    scope
        .set(
            "html.escape(active_page, quote=True)",
            pyfmt::escape(active_page),
        )
        .var("panel_label", escaped_t(lang, "LoopX assistant"))
        .var("history", escaped_t(lang, "Chat history"))
        .var("new_chat", escaped_t(lang, "New chat"))
        .var("close", escaped_t(lang, "Close assistant"))
        .var("context_html", context_html)
        .var("prompts_html", prompts_html)
        .var(
            "ai_cli_notes_html",
            super::ai_cli::chat_notes_html(ctx.state, lang),
        )
        .set(
            "html.escape(_CSRF_TOKEN)",
            pyfmt::escape(&ctx.state.csrf_token),
        );
    render("ai_panel", &scope)
}

pub fn lang_switch_html(lang: Lang) -> String {
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
/// `status_badge_html` is the page's loop status; it only lights the topbar's progress
/// sliver now, since the Agents pill (`queue::pill_html`) carries the progress text.
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
    scope
        .var("ai_label", escaped_t(lang, "Ask LoopX"))
        .var("chat_icon", icons::icon("chat"));
    let office_label = escaped_t(lang, "Switch to Office");
    let ai_trigger_html = format!(
        "{}{}<a class='topbar-icon' href='/office' aria-label='{office_label}' title='{office_label}'>{}</a>{}",
        super::pages::queue::pill_html(ctx),
        super::pages::needs_you::bell_html(ctx),
        icons::icon(super::components::ICON_OFFICE),
        render("shell_ai_trigger", &scope)
    );

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
        .var("refresh_schedule_script", refresh_schedule_script)
        .var("refresh_note_script", refresh_note_script)
        .var("ai_panel_script", ai_panel_script(lang))
        .var("flash_toast_script", flash_toast_script(lang))
        .var("icon_sprite", icons::SPRITE)
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
            "Overview · LoopX Engineering",
            "overview",
            "<b>badge</b>",
            "<p>body</p>",
            ShellOpts::default(),
        );
        assert!(html.starts_with("<!doctype html>\n<html lang=\"en\">"));
        assert!(html.contains("<title>Overview · LoopX Engineering</title>"));
        assert!(html.contains("<p>body</p>"));
        assert!(
            !html.contains("<b>badge</b>"),
            "a running loop's progress shows in the Agents pill instead"
        );
        assert!(!html.contains('\u{27e6}'));
        assert!(html.contains("name='csrf_token' value=\"test-csrf-token\""));
        assert!(
            html.contains("ev.target.closest('[data-copy-target]')"),
            "every page can opt in to a copy button"
        );
    }

    fn panel_for(cli: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let state = UiState::explicit(dir.path(), dir.path(), dir.path(), dir.path(), "tok", None);
        std::fs::write(state.ai_cli_config_path(), format!(r#"{{"cli": "{cli}"}}"#)).unwrap();
        let req = UiRequest::get("/settings");
        ai_panel_html(&UiCtx::new(&state, &req), "settings")
    }

    #[test]
    fn the_side_panel_carries_the_chat_warning_in_its_footer() {
        let gemini = panel_for("gemini");
        let footer = &gemini[gemini.find("<div class='ai-panel-footer'>").unwrap()..];
        assert!(
            footer.starts_with("<div class='ai-panel-footer'>\n<div class='flash flash-warning'>"),
            "{footer}"
        );
        assert!(footer.contains("full tool access"), "{footer}");
        assert!(
            footer.contains("Gemini CLI replies in one piece"),
            "{footer}"
        );
        assert!(footer.contains("href='/settings?tab=ai-cli'"), "{footer}");
        assert!(!panel_for("claude").contains("flash-warning"));
    }

    #[test]
    fn the_shell_turns_a_flash_redirect_into_a_bottom_left_toast() {
        let state = UiState::for_tests();
        let req = UiRequest::get("/");
        let ctx = UiCtx::new(&state, &req);
        let html = render_shell(
            &ctx,
            "Overview",
            "overview",
            "",
            "<p>body</p>",
            ShellOpts::default(),
        );
        assert!(
            html.contains("params.get('flash')"),
            "reads the flash message"
        );
        assert!(
            html.contains("opts === false ? 'failed' : 'done'"),
            "failures get their own card"
        );
        assert!(
            html.contains("history.replaceState"),
            "clears the one-shot params"
        );
        assert!(html.contains("app-toast"), "builds the toast card");
        let css = consts::ds("_STYLE");
        let at = css.find(".app-toasts{").expect("toast stack styled");
        assert!(
            css[at..].contains("left:calc(var(--shell-left,0px) + 12px)")
                && css[at..].contains("bottom:12px")
        );
        assert!(css.contains(".app-toast[data-kind='done']"));
    }

    #[test]
    fn command_o_toggles_the_chat_and_the_button_announces_it() {
        let state = UiState::for_tests();
        let req = UiRequest::get("/");
        let ctx = UiCtx::new(&state, &req);
        let html = render_shell(
            &ctx,
            "Overview",
            "overview",
            "",
            "<p>body</p>",
            ShellOpts::default(),
        );
        let start = html.find("id='ai-panel-trigger'").expect("chat button");
        let trigger = &html[start..start + html[start..].find("</button>").unwrap()];
        assert!(trigger.contains("aria-keyshortcuts='Meta+O'"), "{trigger}");
        assert!(trigger.contains("title='Ask LoopX (⌘O)'"), "{trigger}");
        let script = ai_panel_script(Lang::En);
        assert!(script.contains("ev.metaKey && !ev.ctrlKey && !ev.altKey && !ev.shiftKey && ev.key.toLowerCase() === 'o'"), "{script}");
    }

    #[test]
    fn the_chat_button_uses_the_chat_glyph_and_the_panel_header_the_brand_logo() {
        let state = UiState::for_tests();
        let req = UiRequest::get("/");
        let ctx = UiCtx::new(&state, &req);
        let html = render_shell(
            &ctx,
            "Overview",
            "overview",
            "",
            "<p>body</p>",
            ShellOpts::default(),
        );
        let start = html.find("id='ai-panel-trigger'").expect("chat button");
        let trigger = &html[start..start + html[start..].find("</button>").unwrap()];
        assert!(
            trigger.contains(&icons::icon("chat")) && !trigger.contains("<img"),
            "{trigger}"
        );
        let panel = panel_for("claude");
        let start = panel
            .find("<span class='ai-panel-title'>")
            .expect("panel title");
        let title = &panel[start
            ..start
                + panel[start..]
                    .find("<div class='ai-panel-header-actions'>")
                    .unwrap()];
        assert!(
            title.contains(&format!("src='{}'", assets::brand_url("mark.png")))
                && title.contains(&format!("src='{}'", assets::brand_url("name.png"))),
            "{title}"
        );
        assert!(!title.contains("#i-auto_awesome"), "{title}");
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
        let reload = &on[on.find("__loopScheduleRefresh() {").unwrap()..];
        let reload = &reload[..reload.find("location.reload();").unwrap()];
        assert!(
            reload.contains("classList.contains('ai-panel-open')"),
            "a reload while the chat panel is open would wipe the draft: {reload}"
        );
        let off = render_shell(&ctx, "Logs · X", "logs", "", "", ShellOpts::default());
        assert!(!off.contains("location.reload();"));
    }
}
