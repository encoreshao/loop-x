//! The selected AI CLI as the dashboard shows it: display names, logos, and
//! core's safety warnings (`loopx_core::ai_cli_safety`) in the reader's language.

use super::i18n::{t, Lang};
use super::{consts, icons, pyfmt};
use loopx_core::ai_cli_safety::CallSite;
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

#[cfg(test)]
mod tests {
    use super::*;
    use loopx_core::ai_cli_config::VALID_CLIS;
    use loopx_core::ai_cli_safety::warning;

    #[test]
    fn every_core_warning_is_translated_in_every_language() {
        let mut seen = 0;
        for cli in VALID_CLIS.iter().chain(["unknown-cli"].iter()) {
            for site in SITES {
                let Some(english) = warning(cli, site) else {
                    continue;
                };
                seen += 1;
                assert_eq!(warning_text(Lang::En, english), english);
                for lang in [Lang::Ja, Lang::Zh, Lang::Fr] {
                    let shown = warning_text(lang, english);
                    assert!(
                        !shown.is_empty() && shown != english,
                        "{cli}/{site:?}: no {} translation for {english:?}",
                        lang.code()
                    );
                }
            }
        }
        assert!(seen >= 24, "only {seen} warnings checked");
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
