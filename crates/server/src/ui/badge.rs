//! Status pills and the AI-CLI badge: `_status_badge`, `_progress_text`,
//! `_state_label`, `_status_badge_markup` and the topbar's AI CLI pill.

use super::i18n::{self, Lang};
use super::pyfmt;
use super::template::{Scope, Template};
use super::{consts, state::UiState};
use loopx_core::{ai_cli_config, loops_config::py_truthy, pydumps};
use serde_json::Value;

/// `_status_badge`: the pill class and its icon for a status `state`.
pub fn status_badge(state: &str) -> (&'static str, String) {
    let dot = consts::ds("_DOT_ICON_TEMPLATE").replace("{cls}", "");
    match state {
        "running" => ("pill-blue", consts::ds("_SPINNER_ICON").to_string()),
        "idle" => ("pill-green", consts::ds("_CHECK_ICON").to_string()),
        "failed" => ("pill-red", dot),
        _ => ("pill-grey", dot),
    }
}

/// `str.title()`.
pub fn py_title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_cased = false;
    for ch in s.chars() {
        if ch.is_alphabetic() {
            if prev_cased {
                out.extend(ch.to_lowercase());
            } else {
                out.extend(ch.to_uppercase());
            }
            prev_cased = true;
        } else {
            out.push(ch);
            prev_cased = false;
        }
    }
    out
}

fn step_label(lang: Lang, step: &str) -> String {
    let label = consts::value("dashboard_server", "_STEP_LABELS")
        .and_then(|v| v.get(step))
        .and_then(Value::as_str)
        .unwrap_or(step);
    i18n::t(lang, label, &[])
}

/// `_progress_text`: one line saying what the loop is doing right now.
pub fn progress_text(lang: Lang, status: &Value) -> String {
    let state = status
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let current_issue = status.get("current_issue").filter(|v| py_truthy(v));
    let current_step = status.get("current_step").filter(|v| py_truthy(v));
    if state == "running" {
        if let Some(step) = current_step {
            let step_text = step_label(lang, &pydumps::py_str(step));
            return match current_issue {
                Some(issue) => i18n::t(
                    lang,
                    "Processing {issue} — {step}",
                    &[("issue", &pydumps::py_str(issue)), ("step", &step_text)],
                ),
                None => step_text,
            };
        }
        return i18n::t(lang, "Starting up", &[]);
    }
    i18n::t(lang, "Idle", &[])
}

/// `_state_label`.
pub fn state_label(lang: Lang, status: &Value) -> String {
    match status.get("state") {
        Some(Value::String(s)) if s == "running" => progress_text(lang, status),
        Some(Value::String(s)) => i18n::t(lang, &py_title(&s.replace('_', " ")), &[]),
        Some(other) => pydumps::py_str(other),
        None => i18n::t(lang, "Unknown", &[]),
    }
}

/// `_status_badge_markup`: the small state pill in the topbar.
pub fn status_badge_markup(lang: Lang, status: &Value) -> String {
    let state = pydumps::py_str(
        status
            .get("state")
            .unwrap_or(&Value::String("unknown".into())),
    );
    let (class, icon) = status_badge(&state);
    format!(
        "<span class='pill {class}'>{icon}{}</span>",
        pyfmt::escape(&state_label(lang, status))
    )
}

/// `_default_badge()`: the GitLab loop's status pill, for pages with no loop of their own.
pub fn default_badge(ctx: &super::routes::UiCtx) -> String {
    status_badge_markup(
        ctx.lang,
        &loopx_core::status::read_status(&ctx.state.status_path()),
    )
}

/// The always-visible AI CLI pill (logo + display name) linking to its settings.
pub fn ai_cli_badge(state: &UiState, lang: Lang) -> String {
    let selected = ai_cli_config::get_selected_cli(&state.ai_cli_config_path());
    let name = consts::value("dashboard_server", "_AI_CLI_DISPLAY_NAMES")
        .and_then(|v| v.get(&selected))
        .and_then(Value::as_str)
        .unwrap_or("Claude Code");
    let logo = consts::value("dashboard_server", "_AI_CLI_LOGOS")
        .and_then(|v| v.get(&selected))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut scope = Scope::new(lang);
    scope
        .set("_AI_CLI_LOGOS[selected_cli]", logo)
        .var("ai_cli_name", name);
    Template::named("shell_ai_cli_badge")
        .render(&scope)
        .expect("ai cli badge template")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn title_matches_python() {
        assert_eq!(py_title("never run"), "Never Run");
        assert_eq!(py_title("it's a x-ray"), "It'S A X-Ray");
    }

    #[test]
    fn badge_for_each_state() {
        let idle = status_badge_markup(Lang::En, &json!({"state": "idle"}));
        assert!(idle.starts_with("<span class='pill pill-green'>"));
        assert!(idle.ends_with("Idle</span>"));
        let running = status_badge_markup(
            Lang::En,
            &json!({"state": "running", "current_issue": "a #1", "current_step": "analyzing"}),
        );
        assert!(
            running.contains("md-spinner")
                && running.ends_with("Processing a #1 — Analyzing</span>")
        );
        assert!(
            status_badge_markup(Lang::En, &json!({"state": "never_run"})).contains("pill-grey")
        );
        assert!(status_badge_markup(Lang::Fr, &json!({"state": "failed"})).contains("pill-red"));
    }
}
