//! The persistent left sidebar: `_NAV_ITEMS`/`_NAV_GROUPS` (read from the
//! generated constants), one child link per visible registered loop, and the AI
//! panel's page-to-hub mapping.

use super::i18n::{self, Lang};
use super::state::UiState;
use super::template::{Scope, Template};
use super::{consts, pyfmt};
use serde_json::Value;

/// `_nav_key`: the `_NAV_ITEMS` key a page key belongs to.
pub fn nav_key(page: &str) -> String {
    if page.starts_with("loop:") {
        return "loops".to_string();
    }
    consts::value("dashboard_server", "_LEGACY_PAGE_NAV_KEY")
        .and_then(|m| m.get(page))
        .and_then(Value::as_str)
        .unwrap_or(page)
        .to_string()
}

/// `_nav_link`.
#[allow(clippy::too_many_arguments)]
pub fn nav_link(
    lang: Lang,
    key: &str,
    href: &str,
    label: &str,
    icon: &str,
    active_page: &str,
    extra_class: &str,
    after: &str,
) -> String {
    let classes: Vec<&str> = [extra_class, if key == active_page { "active" } else { "" }]
        .into_iter()
        .filter(|c| !c.is_empty())
        .collect();
    let cls = if classes.is_empty() {
        String::new()
    } else {
        format!(" class='{}'", classes.join(" "))
    };
    let label = pyfmt::escape(&i18n::t(lang, label, &[]));
    let mut scope = Scope::new(lang);
    scope
        .var("href", href)
        .var("label", label)
        .var("cls", cls)
        .var("icon", icon)
        .var("after", after);
    Template::named("nav_link")
        .render(&scope)
        .expect("nav_link template")
}

/// `loop_is_visible`: only an enabled loop is listed. A loop the user turned
/// off disappears from the sidebar and Activity even if it has run before; its
/// history stays on disk and comes back when it is enabled again.
pub fn loop_is_visible(_state: &UiState, entry: &Value) -> bool {
    entry
        .get("enabled")
        .is_none_or(loopx_core::loops_config::py_truthy)
}

/// The registry's loops, or none when the file is missing or malformed.
pub fn registered_loops(state: &UiState) -> Vec<Value> {
    loopx_core::loops_config::list_loops(
        &state.loops_config_path(),
        Some(&state.loops_template_path()),
    )
    .unwrap_or_default()
}

fn loop_children(state: &UiState, lang: Lang, active_page: &str) -> String {
    let dot = format!(
        "<span class='nav-dot' role='img' aria-label='{}'></span>",
        pyfmt::escape(&i18n::t(lang, "Needs attention", &[]))
    );
    super::sidebar_loops::for_state(state)
        .iter()
        .map(|l| {
            nav_link(
                lang,
                &format!("loop:{}", l.name),
                &l.path,
                &l.label,
                &l.icon,
                active_page,
                "nav-child",
                if l.attention { &dot } else { "" },
            )
        })
        .collect()
}

/// `_sidebar_html(active_page)`.
pub fn sidebar_html(state: &UiState, lang: Lang, active_page: &str) -> String {
    let nav_active = nav_key(active_page);
    let items = consts::list("dashboard_server", "_NAV_ITEMS");
    let mut nav_html = String::new();
    for group in consts::list("dashboard_server", "_NAV_GROUPS") {
        let group = group.as_array().expect("nav group");
        if let Some(label) = group[0].as_str() {
            nav_html.push_str(&format!(
                "<p class='sidebar-group-label'>{}</p>",
                pyfmt::escape(&i18n::t(lang, label, &[]))
            ));
        }
        for key in group[1].as_array().expect("nav group keys") {
            let key = key.as_str().expect("nav key");
            let item = items
                .iter()
                .find(|i| i[0].as_str() == Some(key))
                .expect("nav item");
            nav_html.push_str(&nav_link(
                lang,
                key,
                item[1].as_str().unwrap_or("/"),
                item[2].as_str().unwrap_or(""),
                item[3].as_str().unwrap_or(""),
                &nav_active,
                "",
                "",
            ));
            if key == "loops" {
                nav_html.push_str(&loop_children(state, lang, active_page));
            }
        }
    }
    let mut scope = Scope::new(lang);
    scope.var("nav_html", nav_html);
    Template::named("sidebar")
        .render(&scope)
        .expect("sidebar template")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_page_keys_map_to_their_hub() {
        assert_eq!(nav_key("loop:rss"), "loops");
        assert_eq!(nav_key("history"), "runs");
        assert_eq!(nav_key("overview"), "overview");
        assert_eq!(nav_key("update"), "update");
    }

    #[test]
    fn only_enabled_loops_are_visible_even_when_a_disabled_one_has_run_before() {
        let dir = tempfile::tempdir().unwrap();
        let state = UiState::explicit(dir.path(), dir.path(), dir.path(), dir.path(), "tok", None);
        let status = state.status_path_for_loop("rss-watch-loop");
        std::fs::create_dir_all(status.parent().unwrap()).unwrap();
        std::fs::write(&status, r#"{"state": "idle"}"#).unwrap();
        assert!(!loop_is_visible(
            &state,
            &serde_json::json!({"name": "rss-watch-loop", "enabled": false})
        ));
        assert!(loop_is_visible(
            &state,
            &serde_json::json!({"name": "rss-watch-loop", "enabled": true})
        ));
        assert!(loop_is_visible(
            &state,
            &serde_json::json!({"name": "no-flag-loop"})
        ));
    }

    fn sidebar_fixture(pins: Option<&str>) -> (tempfile::TempDir, UiState) {
        let dir = tempfile::tempdir().unwrap();
        let (repo, home) = (dir.path().join("repo"), dir.path().join("home"));
        std::fs::create_dir_all(repo.join("outputs/status")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("loops.json"),
            r#"[{"name": "issue-loop", "enabled": true},
                {"name": "topic-loop", "enabled": true},
                {"name": "rss-watch-loop", "label": "RSS Watch", "enabled": true}]"#,
        )
        .unwrap();
        if let Some(pins) = pins {
            std::fs::write(home.join("sidebar_pins.json"), pins).unwrap();
        }
        let state = UiState::explicit(&repo, &home, dir.path(), dir.path(), "tok", None);
        (dir, state)
    }

    #[test]
    fn the_sidebar_lists_pinned_loops_then_loops_needing_attention() {
        let (_d, state) = sidebar_fixture(Some(r#"{"pinned": ["topic-loop"]}"#));
        std::fs::write(
            state.status_path_for_loop("rss-watch-loop"),
            r#"{"state": "failed", "updated_at": "2026-11-24T08:00:00+00:00"}"#,
        )
        .unwrap();
        let html = sidebar_html(&state, Lang::En, "overview");
        let topic = html
            .find("href='/loops/topic-loop'")
            .expect("pinned loop listed");
        let rss = html
            .find("href='/loops/rss-watch-loop'")
            .expect("failing loop listed");
        assert!(topic < rss);
        assert!(
            !html.contains("href='/loops/issue-loop'"),
            "unpinned, healthy loop hidden"
        );
        let rss_link = &html[rss..rss + html[rss..].find("</a>").unwrap()];
        assert!(rss_link.contains("nav-dot"), "{rss_link}");
        let topic_link = &html[topic..topic + html[topic..].find("</a>").unwrap()];
        assert!(!topic_link.contains("nav-dot"), "{topic_link}");
    }

    #[test]
    fn a_get_never_writes_the_pins_file() {
        let (_d, state) = sidebar_fixture(None);
        let html = sidebar_html(&state, Lang::En, "overview");
        assert!(
            html.contains("href='/loops/rss-watch-loop'"),
            "seeded from enabled loops"
        );
        assert!(!state.home_dir.join("sidebar_pins.json").exists());
    }

    #[test]
    fn the_loops_link_reads_all_loops() {
        let (_d, state) = sidebar_fixture(None);
        let html = sidebar_html(&state, Lang::En, "overview");
        assert!(
            html.contains("<a href='/loops' title='All loops'"),
            "{html}"
        );
    }

    #[test]
    fn link_markup() {
        assert_eq!(
            nav_link(Lang::Fr, "runs", "/runs", "Runs", "<i/>", "runs", "", ""),
            "<a href='/runs' title='Exécutions'><span class='nav-icon'><i/></span><span class='nav-label'>Exécutions</span></a>"
                .replace("<a href='/runs' title='Exécutions'>", "<a href='/runs' title='Exécutions' class='active'>")
        );
    }
}
