//! General view: Appearance, Notifications, AI CLI and Instructions tabs.

use crate::ui::ai_cli;
use crate::ui::consts;
use crate::ui::i18n;
use crate::ui::pages::loops_common::{custom_select, flash_html, mask_secret};
use crate::ui::pyfmt;
use crate::ui::routes::UiCtx;
use crate::ui::template::{Scope, Template};
use loopx_core::settings_store as store;
use loopx_core::{ai_cli_config, ai_cli_safety};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn render(name: &str, scope: &Scope) -> String {
    Template::named(name)
        .render(scope)
        .unwrap_or_else(|e| panic!("{e}"))
}

const CLI_TTL_SECS: u64 = 3600;
static CLI_PROBING: AtomicBool = AtomicBool::new(false);

fn cli_flags_path(ctx: &UiCtx) -> PathBuf {
    ctx.state.home_dir.join("cli_status.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Is `name` installed, as last recorded in `cli_status.json`? Never runs a shell
/// on the page's path: `None` means not checked yet. A missing or stale flag starts
/// one background probe, so the next page view sees the refreshed answer.
pub fn cli_available(ctx: &UiCtx, names: &[&str]) -> HashMap<String, Option<bool>> {
    if let Some(pinned) = &ctx.state.cli_pinned {
        return names
            .iter()
            .map(|n| (n.to_string(), Some(pinned.iter().any(|p| p == n))))
            .collect();
    }
    let path = cli_flags_path(ctx);
    let saved = store::read_object(&path);
    let now = now_secs();
    let mut stale = false;
    let flags = names
        .iter()
        .map(|n| {
            let entry = saved.get(*n).and_then(Value::as_object);
            let found = entry.and_then(|e| e.get("found")).and_then(Value::as_bool);
            let at = entry.and_then(|e| e.get("at")).and_then(Value::as_u64).unwrap_or(0);
            stale |= found.is_none() || now.saturating_sub(at) > CLI_TTL_SECS;
            (n.to_string(), found)
        })
        .collect();
    if stale {
        refresh_cli_flags(path, names.iter().map(|n| n.to_string()).collect());
    }
    flags
}

/// Resolve `names` in one interactive login shell on a background thread and write
/// the flags to `path`. A zshrc that takes seconds to load costs the page nothing.
fn refresh_cli_flags(path: PathBuf, names: Vec<String>) {
    if CLI_PROBING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let script: String = names
            .iter()
            .map(|n| format!("command -v {n} >/dev/null 2>&1 && echo FOUND:{n}; "))
            .collect();
        let mut cmd = std::process::Command::new("zsh");
        cmd.args(["-i", "-l", "-c", &script]);
        let out = loopx_core::proc::run(&mut cmd, None, Some(Duration::from_secs(10))).ok();
        let at = now_secs();
        let mut flags = Map::new();
        for n in &names {
            let found = out
                .as_ref()
                .is_some_and(|o| o.stdout.lines().any(|l| l.trim() == format!("FOUND:{n}")));
            flags.insert(n.clone(), serde_json::json!({"found": found, "at": at}));
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, Value::Object(flags).to_string());
        CLI_PROBING.store(false, Ordering::SeqCst);
    });
}

fn notifications_panel(ctx: &UiCtx, csrf: &str) -> String {
    let slack = store::read_object(&store::slack_config_path(&ctx.state.user_home));
    let webhook = slack.get("webhook_url").map(loopx_core::loops_config::py_str).unwrap_or_default();
    let webhook_display = if webhook.is_empty() || slack.get("webhook_url").is_none_or(|v| !loopx_core::loops_config::py_truthy(v)) {
        pyfmt::escape(&ctx.t("(not set)"))
    } else {
        mask_secret(&store::display_secret(&webhook))
    };
    let saved = slack
        .get("block_templates")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let defaults = store::read_default_block_templates(&ctx.state.repo_root.join("docs").join("slack-templates"));
    let mut merged: Map<String, Value> = defaults.clone();
    for (k, v) in &saved {
        merged.insert(k.clone(), v.clone());
    }
    let default_only: Vec<Value> = defaults
        .keys()
        .filter(|k| !saved.contains_key(*k))
        .map(|k| Value::String(k.clone()))
        .collect();
    let js_safe = |v: &Value| loopx_core::pydumps::dumps(v).replace('<', "\\u003c");
    let key_options: String = store::BLOCK_TEMPLATE_NOTIFICATION_KEYS
        .iter()
        .map(|(key, label)| {
            format!(
                "<option value='{}'>{}</option>",
                pyfmt::escape(key),
                pyfmt::escape(&i18n::t(ctx.lang, label, &[]))
            )
        })
        .collect();
    let subtitle = ctx.tf(
        "Compose a Slack Block Kit template, optionally bind it to a real loop alert, and preview the JSON that will be sent. \
{token} in any text field is replaced with the real alert text when a bound template fires. \
Templates marked \"(default)\" ship with the app - pick one, preview or send a test message freely, \
and Save to make it your own (Delete is disabled until then).",
        &[("token", "<code>{{message}}</code>")],
    );
    let mut scope = Scope::new(ctx.lang);
    scope
        .var("webhook_display", webhook_display)
        .var("csrf_input", csrf)
        .var("bkb_subtitle", subtitle)
        .var("block_templates_json", js_safe(&Value::Object(merged)))
        .var("default_only_names_json", js_safe(&Value::Array(default_only)))
        .var("notification_key_options", key_options);
    render("settings_notifications_panel", &scope)
}

/// Whether `cli` is installed under any of its binary names; `None` until probed.
fn cli_found(flags: &HashMap<String, Option<bool>>, cli: &str) -> Option<bool> {
    let seen: Vec<Option<bool>> = ai_cli_config::binary_candidates(cli)
        .iter()
        .map(|b| flags.get(*b).copied().flatten())
        .collect();
    if seen.contains(&Some(true)) {
        Some(true)
    } else if seen.contains(&None) {
        None
    } else {
        Some(false)
    }
}

/// The inline warning for an unrestricted CLI, naming every feature it affects;
/// only the selected CLI's is visible, the picker's script reveals the others.
fn ai_cli_warning(ctx: &UiCtx, cli: &str, visible: bool) -> Option<String> {
    let mut features: Vec<String> = Vec::new();
    let mut texts: Vec<&str> = Vec::new();
    for site in ai_cli::SITES {
        if let Some(w) = ai_cli_safety::warning(cli, site) {
            features.push(ai_cli::feature_label(ctx.lang, site));
            if !texts.contains(&w) {
                texts.push(w);
            }
        }
    }
    if features.is_empty() {
        return None;
    }
    // Codex's chat text extends its general one: say it once.
    let longer = texts.clone();
    texts.retain(|w| !longer.iter().any(|o| o != w && o.contains(w)));
    let affected = ctx.tf(
        "{cli} runs without a tool allowlist in: {features}.",
        &[("cli", &ai_cli::display_name(cli)), ("features", &features.join(", "))],
    );
    let paragraphs: String = std::iter::once(affected)
        .chain(texts.iter().map(|w| ai_cli::warning_text(ctx.lang, w)))
        .map(|p| format!("<p>{}</p>", pyfmt::escape(&p)))
        .collect();
    Some(format!(
        "<div class='flash flash-warning' data-ai-cli-warning='{}'{}>{paragraphs}</div>",
        pyfmt::escape(cli),
        if visible { "" } else { " hidden" }
    ))
}

fn ai_cli_panel(ctx: &UiCtx, csrf: &str, current: &str) -> String {
    let binaries: Vec<&str> = ai_cli_config::VALID_CLIS
        .iter()
        .flat_map(|cli| ai_cli_config::binary_candidates(cli).iter().copied())
        .collect();
    let flags = cli_available(ctx, &binaries);
    let label = |cli: &str| {
        let mut text = ai_cli::display_name(cli);
        if ai_cli_safety::experimental(cli) {
            text.push_str(&format!(" ({})", ctx.t("experimental")));
        }
        match cli_found(&flags, cli) {
            Some(true) => text.push_str(&format!(" ({})", ctx.t("installed"))),
            Some(false) => text.push_str(&format!(" ({})", ctx.t("not found on PATH"))),
            None => {}
        }
        text
    };
    let labels: Vec<(String, String)> = ai_cli_config::VALID_CLIS
        .iter()
        .map(|cli| (cli.to_string(), label(cli)))
        .collect();
    let warnings: String = ai_cli_config::VALID_CLIS
        .iter()
        .filter_map(|cli| ai_cli_warning(ctx, cli, *cli == current))
        .collect();
    let select = custom_select("cli", &labels, Some(current), None);
    let current_label = scope_label(&labels, current);
    let mut scope = Scope::new(ctx.lang);
    scope
        .var("select_html", select)
        .var("csrf_input", csrf)
        .var("warnings_html", warnings);
    scope.set("html.escape(cli_labels[current_cli])", pyfmt::escape(&current_label));
    render("settings_ai_cli_panel", &scope)
}

fn scope_label(labels: &[(String, String)], current: &str) -> String {
    labels
        .iter()
        .find(|(c, _)| c == current)
        .map(|(_, l)| l.clone())
        .unwrap_or_default()
}

/// The Appearance tab's font buttons. The label goes through the translator
/// (catalog misses fall back to the English text); `system-ui` is the generic
/// keyword and must stay unquoted, a quoted name would be a font *named* so.
fn font_buttons(lang: i18n::Lang) -> String {
    let fallback = consts::ds("_FALLBACK_FONT_STACK");
    consts::list("dashboard_server", "_FONT_CHOICES")
        .iter()
        .map(|c| {
            let (key, label, name) = (
                c[0].as_str().unwrap_or(""),
                c[1].as_str().unwrap_or(""),
                c[2].as_str().unwrap_or(""),
            );
            let family = if name == "system-ui" {
                name.to_string()
            } else {
                format!("'{name}'")
            };
            format!(
                "<button type='button' class='pref-segmented-option' data-font-choice=\"{key}\" \
style=\"font-family: {family}, {fallback}\">{}</button>",
                pyfmt::escape(&i18n::t(lang, label, &[]))
            )
        })
        .collect()
}

fn appearance_panel(ctx: &UiCtx) -> String {
    let mode_buttons: String = [("light", "Light"), ("dark", "Dark"), ("auto", "Auto")]
        .iter()
        .map(|(mode, label)| {
            format!(
                "<button type='button' class='pref-segmented-option' data-color-mode-choice=\"{mode}\">{}</button>",
                pyfmt::escape(&ctx.t(label))
            )
        })
        .collect();
    let swatches: String = consts::list("dashboard_server", "_ACCENT_CHOICES")
        .iter()
        .map(|c| {
            let (key, label, nav) = (
                c[0].as_str().unwrap_or(""),
                c[1].as_str().unwrap_or(""),
                c[2].as_str().unwrap_or(""),
            );
            format!(
                "<button type='button' class='pref-swatch' data-accent-choice=\"{key}\">\
<span class='pref-swatch-preview'>\
<span class='pref-swatch-preview-nav' style='background:{nav}'></span>\
<span class='pref-swatch-preview-content'></span>\
</span>{}</button>",
                pyfmt::escape(&i18n::t(ctx.lang, label, &[]))
            )
        })
        .collect();
    let fonts = font_buttons(ctx.lang);
    let refresh: String = [
        ("5", ctx.tf("{n}s", &[("n", "5")])),
        ("11", ctx.tf("{n}s", &[("n", "11")])),
        ("30", ctx.tf("{n}s", &[("n", "30")])),
        ("60", ctx.tf("{n} min", &[("n", "1")])),
        ("300", ctx.tf("{n} min", &[("n", "5")])),
    ]
    .iter()
    .map(|(secs, label)| {
        format!(
            "<button type='button' class='pref-segmented-option' data-refresh-choice=\"{secs}\">{}</button>",
            pyfmt::escape(label)
        )
    })
    .collect();
    let mut scope = Scope::new(ctx.lang);
    scope
        .var("mode_buttons", mode_buttons)
        .var("swatch_buttons", swatches)
        .var("font_buttons", fonts)
        .var("refresh_buttons", refresh);
    render("settings_appearance_panel", &scope)
}

fn instructions_panel(ctx: &UiCtx, csrf: &str, current_cli: &str) -> String {
    let text = store::read_custom_instructions(&ctx.state.home_dir.join("instructions.md"));
    let subtitle = ctx.tf(
        "Include specific instructions in {cli}'s system prompt whenever the loop runs. \
Saved to {path} - read at the start of every run, on top of everything already in {spec}. \
For one loop only, add a file under {per_loop} (gitlab-issue.md, topic-monitor.md or inbox-triage.md).",
        &[
            ("cli", &pyfmt::escape(&ai_cli::display_name(current_cli))),
            ("path", "<code>~/.loopx/instructions.md</code>"),
            ("spec", "<code>instructions/</code>"),
            ("per_loop", "<code>~/.loopx/instructions/</code>"),
        ],
    );
    let mut scope = Scope::new(ctx.lang);
    scope
        .var("instructions_subtitle", subtitle)
        .var("csrf_input", csrf)
        .var("current_text", text);
    render("settings_instructions_panel", &scope)
}

pub fn body(ctx: &UiCtx, flash: Option<&str>, flash_ok: bool, tab: &str) -> String {
    let csrf = ctx.csrf_input();
    let current_cli = loopx_core::ai_cli_config::get_selected_cli(&ctx.state.ai_cli_config_path());
    let icon = |name: &str| consts::ds(name);
    let tabs: Vec<(&str, String, &str, String)> = vec![
        ("appearance", ctx.t("Appearance"), icon("_SECTION_ICON_PREFERENCES"), appearance_panel(ctx)),
        ("notifications", ctx.t("Notifications"), icon("_SECTION_ICON_SLACK"), notifications_panel(ctx, &csrf)),
        ("ai-cli", "AI CLI".to_string(), icon("_SECTION_ICON_AI_CLI"), ai_cli_panel(ctx, &csrf, &current_cli)),
        ("instructions", ctx.t("Instructions"), icon("_SECTION_ICON_INSTRUCTIONS"), instructions_panel(ctx, &csrf, &current_cli)),
    ];
    let active = if tabs.iter().any(|t| t.0 == tab) { tab } else { "notifications" };
    let buttons: String = tabs
        .iter()
        .map(|(key, label, icon, _)| {
            let on = *key == active;
            format!(
                "<button type='button' class='tab-button{}' data-tab-target='{key}' role='tab' aria-selected='{}'>{icon}{}</button>",
                if on { " is-active" } else { "" },
                if on { "true" } else { "false" },
                pyfmt::escape(label)
            )
        })
        .collect();
    let panels: String = tabs
        .iter()
        .map(|(key, _, _, panel)| {
            format!(
                "<div data-tab-panel='{key}'{}>{panel}</div>",
                if *key == active { "" } else { " hidden" }
            )
        })
        .collect();
    let mut scope = Scope::new(ctx.lang);
    scope
        .var("flash_html", flash_html(flash, flash_ok))
        .var("tab_buttons", buttons)
        .var("tab_panels", panels);
    render("settings_general_body", &scope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::i18n::Lang;

    #[test]
    fn font_labels_are_translated_and_system_previews_in_the_generic_stack() {
        let fr = font_buttons(Lang::Fr);
        assert!(fr.contains(">Système</button>"), "{fr}");
        assert!(fr.contains(">Roboto</button>"), "missing key falls back to English");
        assert!(fr.contains("font-family: system-ui, "), "{fr}");
        assert!(!fr.contains("'system-ui'"));
        assert!(font_buttons(Lang::En).contains(">System</button>"));
    }

    /// The AI CLI panel with `cli` selected and only `on_path` binaries installed.
    fn ai_panel_for(cli: &str, on_path: &[&str]) -> String {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ai_cli.json"), format!(r#"{{"cli": "{cli}"}}"#)).unwrap();
        let mut state = crate::ui::UiState::explicit(dir.path(), dir.path(), dir.path(), dir.path(), "tok", None);
        state.cli_pinned = Some(on_path.iter().map(|s| s.to_string()).collect());
        let req = crate::ui::UiRequest::get("/settings?tab=ai-cli");
        let ctx = UiCtx::new(&state, &req);
        let current = loopx_core::ai_cli_config::get_selected_cli(&state.ai_cli_config_path());
        ai_cli_panel(&ctx, "", &current)
    }

    fn option_label<'a>(html: &'a str, cli: &str) -> &'a str {
        let start = html.find(&format!("<option value='{cli}'")).unwrap_or_else(|| panic!("no {cli} option: {html}"));
        let rest = &html[start..];
        let open = rest.find('>').unwrap() + 1;
        &rest[open..rest.find("</option>").unwrap()]
    }

    #[test]
    fn the_picker_lists_all_five_and_tags_the_newer_ones_experimental() {
        let html = ai_panel_for("claude", &["claude", "codex", "gemini", "kimi", "agent"]);
        assert_eq!(html.matches("<option value=").count(), 5, "{html}");
        for (cli, name, experimental) in [
            ("claude", "Claude Code", false),
            ("codex", "Codex CLI", false),
            ("gemini", "Gemini CLI", true),
            ("kimi", "Kimi CLI", true),
            ("cursor", "Cursor CLI", true),
        ] {
            let label = option_label(&html, cli);
            assert!(label.starts_with(name), "{label}");
            assert_eq!(label.contains("experimental"), experimental, "{label}");
            assert!(label.contains("installed"), "{label}");
        }
    }

    #[test]
    fn the_picker_says_which_clis_are_not_on_path() {
        // Cursor counts as installed under either of its binary names.
        let html = ai_panel_for("claude", &["claude", "cursor-agent"]);
        for cli in ["codex", "gemini", "kimi"] {
            assert!(option_label(&html, cli).ends_with("(not found on PATH)"), "{html}");
        }
        assert!(option_label(&html, "cursor").ends_with("(installed)"), "{html}");
        assert!(option_label(&html, "claude").ends_with("(installed)"), "{html}");
    }

    #[test]
    fn an_unrestricted_selection_shows_its_warning_naming_the_affected_features() {
        let html = ai_panel_for("gemini", &["claude"]);
        let start = html
            .find("data-ai-cli-warning='gemini'>")
            .unwrap_or_else(|| panic!("no visible gemini warning: {html}"));
        let shown = &html[start..start + html[start..].find("</div>").unwrap()];
        assert!(shown.contains("full tool access"), "{shown}");
        for feature in ["GitLab issue loop", "Topic Monitor", "Inbox Triage", "Dashboard chat"] {
            assert!(shown.contains(feature), "{feature}: {shown}");
        }
        // The other unrestricted CLIs' warnings are present but hidden, for the
        // picker's script to reveal; claude has none.
        for cli in ["codex", "kimi", "cursor"] {
            assert!(html.contains(&format!("data-ai-cli-warning='{cli}' hidden>")), "{cli}: {html}");
        }
        assert!(!html.contains("data-ai-cli-warning='claude'"), "{html}");
    }

    #[test]
    fn claude_shows_no_warning() {
        let html = ai_panel_for("claude", &["claude"]);
        for cli in loopx_core::ai_cli_config::VALID_CLIS {
            assert!(!html.contains(&format!("data-ai-cli-warning='{cli}'>")), "{cli}: {html}");
        }
        assert!(html.contains("data-ai-cli-warning='codex' hidden>"), "{html}");
    }

    #[test]
    fn the_codex_warning_is_translated_and_names_its_session_log() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ai_cli.json"), r#"{"cli": "codex"}"#).unwrap();
        let state = crate::ui::UiState::explicit(dir.path(), dir.path(), dir.path(), dir.path(), "tok", None);
        let req = crate::ui::UiRequest::get("/settings").with_header("cookie", "loop_lang=fr");
        let ctx = UiCtx::new(&state, &req);
        let html = ai_cli_panel(&ctx, "", "codex");
        assert!(html.contains("~/.codex/sessions"), "{html}");
        assert!(!html.contains("Codex gives the model a shell"), "{html}");
        let en = ai_panel_for("codex", &["claude"]);
        assert_eq!(en.matches("Codex gives the model a shell").count(), 1, "{en}");
        assert!(en.contains("saving projects"), "{en}");
    }
}
