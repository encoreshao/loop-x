//! The selected AI CLI as the dashboard shows it: display names, logos, and
//! core's safety warnings (`loopx_core::ai_cli_safety`) in the reader's language.

use super::i18n::{t, Lang};
use super::state::UiState;
use super::{consts, icons, pyfmt};
use loopx_core::ai_cli_safety::CallSite;
use loopx_core::{ai_cli_config, chat};
use serde_json::Value;

/// Every place loopx calls an AI CLI, in the order the Settings warning names them.
pub const SITES: [CallSite; 6] = [
    CallSite::GitlabLoop,
    CallSite::TopicMonitor,
    CallSite::CalendarPrep,
    CallSite::InboxTriage,
    CallSite::Chat,
    CallSite::Plugin,
];

/// Where the user changes the CLI.
pub const SETTINGS_HREF: &str = "/settings?tab=ai-cli";

/// `_AI_CLI_DISPLAY_NAMES[cli]`, or the id itself for an unknown one.
pub fn display_name(cli: &str) -> String {
    consts::value("dashboard_server", "_AI_CLI_DISPLAY_NAMES")
        .and_then(|v| v.get(cli))
        .and_then(Value::as_str)
        .unwrap_or(cli)
        .to_string()
}

/// `_AI_CLI_LOGOS[cli]`, or the circle glyph for an unknown one.
pub fn logo(cli: &str) -> String {
    consts::value("dashboard_server", "_AI_CLI_LOGOS")
        .and_then(|v| v.get(cli))
        .and_then(Value::as_str)
        .map_or_else(|| icons::icon_with("circle", "ai-cli-logo"), str::to_string)
}

/// A warning `ai_cli_safety::warning` returned, translated. Each arm repeats
/// core's English as the literal the catalogs key on; text this table does not
/// know (a newer core) is shown in English rather than hidden.
pub fn warning_text(lang: Lang, english: &str) -> String {
    match english {
        "Codex gives the model a shell and records prompts, including email text, under ~/.codex/sessions. In chat, its sandbox also blocks actions that change loopx settings directly (enabling or disabling loops, saving projects): use the Settings pages or switch CLI for those." => t(lang, "Codex gives the model a shell and records prompts, including email text, under ~/.codex/sessions. In chat, its sandbox also blocks actions that change loopx settings directly (enabling or disabling loops, saving projects): use the Settings pages or switch CLI for those.", &[]),
        "Codex gives the model a shell and records prompts, including email text, under ~/.codex/sessions." => t(lang, "Codex gives the model a shell and records prompts, including email text, under ~/.codex/sessions.", &[]),
        "Gemini CLI cannot be limited to a fixed set of tools here, so the model runs with its full tool access." => t(lang, "Gemini CLI cannot be limited to a fixed set of tools here, so the model runs with its full tool access.", &[]),
        "Kimi CLI cannot be limited to a fixed set of tools here, so the model runs with its full tool access." => t(lang, "Kimi CLI cannot be limited to a fixed set of tools here, so the model runs with its full tool access.", &[]),
        "Cursor Agent cannot be limited to a fixed set of tools here, so the model runs with its full tool access." => t(lang, "Cursor Agent cannot be limited to a fixed set of tools here, so the model runs with its full tool access.", &[]),
        "Grok CLI cannot be limited to a fixed set of tools here, so the model runs with its full tool access." => t(lang, "Grok CLI cannot be limited to a fixed set of tools here, so the model runs with its full tool access.", &[]),
        "This AI CLI cannot be limited to a fixed set of tools here." => t(lang, "This AI CLI cannot be limited to a fixed set of tools here.", &[]),
        other => other.to_string(),
    }
}

/// The feature a call site stands for, as the Settings warning names it.
pub fn feature_label(lang: Lang, site: CallSite) -> String {
    match site {
        CallSite::GitlabLoop => t(lang, "GitLab issue loop", &[]),
        CallSite::TopicMonitor => t(lang, "Topic Monitor", &[]),
        CallSite::CalendarPrep => t(lang, "Calendar prep", &[]),
        CallSite::InboxTriage => t(lang, "Inbox Triage", &[]),
        CallSite::Chat => t(lang, "Dashboard chat", &[]),
        CallSite::Plugin => t(lang, "LoopKit plugins and LinkedIn extraction", &[]),
    }
}

/// The link to Settings -> AI CLI that every warning and CLI error carries.
pub fn settings_link(lang: Lang) -> String {
    format!(
        "<a href='{SETTINGS_HREF}'>{}</a>",
        pyfmt::escape(&t(lang, "Switch AI CLI in Settings", &[]))
    )
}

/// A warning box; each paragraph is already HTML.
pub fn warning_box(paragraphs: &[String]) -> String {
    let inner: String = paragraphs.iter().map(|p| format!("<p>{p}</p>")).collect();
    format!("<div class='flash flash-warning'>{inner}</div>")
}

/// A stored or live core warning as a box with the way out.
pub fn warning_notice(lang: Lang, english: &str) -> String {
    warning_box(&[
        pyfmt::escape(&warning_text(lang, english)),
        settings_link(lang),
    ])
}

/// What the selected CLI means for chat (the Chat page and the side panel):
/// core's warning when it runs without a tool allowlist, and a note when its
/// reply arrives in one piece. Empty for a restricted, streaming CLI.
pub fn chat_notes_html(state: &UiState, lang: Lang) -> String {
    let cli = chat::chat_cli(&ai_cli_config::get_selected_cli(
        &state.ai_cli_config_path(),
    ));
    let record = chat::ChatJobRecord::for_cli(cli);
    let mut paragraphs: Vec<String> = Vec::new();
    if let Some(w) = &record.warning {
        paragraphs.push(pyfmt::escape(&warning_text(lang, w)));
    }
    if record.chat_mode == chat::ChatMode::SingleReply {
        paragraphs.push(pyfmt::escape(&t(
            lang,
            "{cli} replies in one piece: there is no live typing while it writes.",
            &[("cli", &display_name(cli))],
        )));
    }
    if paragraphs.is_empty() {
        return String::new();
    }
    paragraphs.push(settings_link(lang));
    warning_box(&paragraphs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use loopx_core::ai_cli_config::VALID_CLIS;
    use loopx_core::ai_cli_safety::warning;

    /// The English patterns of `warning_text`'s arms, read from this file, each
    /// checked to translate its own pattern.
    fn mapped_warnings() -> std::collections::BTreeSet<String> {
        let src = include_str!("ai_cli.rs");
        let body = &src[src.find("pub fn warning_text(").unwrap()..];
        let body = &body[..body.find("other => other.to_string()").unwrap()];
        body.lines()
            .filter_map(|l| l.trim().strip_prefix('"'))
            .map(|l| {
                let (pattern, rest) = l.split_once("\" => t(lang, \"").expect("arm shape");
                assert_eq!(
                    rest,
                    format!("{pattern}\", &[]),"),
                    "arm translates another text"
                );
                pattern.to_string()
            })
            .collect()
    }

    #[test]
    fn every_core_warning_is_mapped_and_translated_in_every_language() {
        let mut seen = std::collections::BTreeSet::new();
        for cli in VALID_CLIS.iter().chain(["unknown-cli"].iter()) {
            for site in SITES {
                if let Some(english) = warning(cli, site) {
                    seen.insert(english.to_string());
                }
            }
        }
        // A core text change, a new core text or a dead arm all fail here.
        assert_eq!(seen, mapped_warnings());
        for english in &seen {
            assert_eq!(&warning_text(Lang::En, english), english);
            for lang in [Lang::Ja, Lang::Zh, Lang::Fr] {
                let shown = warning_text(lang, english);
                assert!(
                    !shown.is_empty() && &shown != english,
                    "no {} translation for {english:?}",
                    lang.code()
                );
            }
        }
    }

    #[test]
    fn unknown_warning_text_is_shown_as_is() {
        assert_eq!(warning_text(Lang::Fr, "Something new"), "Something new");
    }

    #[test]
    fn every_call_site_has_a_translated_feature_name() {
        for site in SITES {
            let en = feature_label(Lang::En, site);
            assert!(!en.is_empty(), "{site:?}");
            for lang in [Lang::Ja, Lang::Zh, Lang::Fr] {
                assert_ne!(feature_label(lang, site), en, "{site:?} in {}", lang.code());
            }
        }
    }

    #[test]
    fn every_cli_has_a_name_and_logo_and_unknown_ones_get_the_circle() {
        for (cli, name) in [
            ("claude", "Claude Code"),
            ("codex", "Codex CLI"),
            ("gemini", "Gemini CLI"),
            ("kimi", "Kimi CLI"),
            ("cursor", "Cursor CLI"),
        ] {
            assert_eq!(display_name(cli), name);
            let svg = logo(cli);
            assert!(svg.starts_with("<svg class='ai-cli-logo'"), "{cli}: {svg}");
            assert!(svg.contains("aria-hidden='true'"), "{cli}");
        }
        assert_eq!(display_name("mystery"), "mystery");
        assert!(logo("mystery").contains("#i-circle"), "{}", logo("mystery"));
    }
}
