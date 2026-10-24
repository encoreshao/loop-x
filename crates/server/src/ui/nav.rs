//! The persistent left sidebar: `_NAV_ITEMS`/`_NAV_GROUPS` (read from the
//! generated constants), one child link per visible registered loop, and the AI
//! panel's page-to-hub mapping.

use super::i18n::{self, Lang};
use super::state::UiState;
use super::template::{Scope, Template};
use super::{consts, hub, pyfmt};
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
pub fn nav_link(
    lang: Lang,
    key: &str,
    href: &str,
    label: &str,
    icon: &str,
    active_page: &str,
    extra_class: &str,
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
        .var("icon", icon);
    Template::named("nav_link")
        .render(&scope)
        .expect("nav_link template")
}

/// `loop_is_visible`: enabled, or has ever run (its status file exists).
pub fn loop_is_visible(state: &UiState, entry: &Value) -> bool {
    match entry.get("enabled") {
        None => true,
        Some(v) if loopx_core::loops_config::py_truthy(v) => true,
        _ => {
            let name = entry.get("name").and_then(Value::as_str).unwrap_or("?");
            state.status_path_for_loop(name).exists()
        }
    }
}

/// The registry's loops, or none when the file is missing or malformed.
pub fn registered_loops(state: &UiState) -> Vec<Value> {
    loopx_core::loops_config::list_loops(
        &state.loops_config_path(),
        Some(&state.loops_template_path()),
    )
    .unwrap_or_default()
}

fn loop_children(state: &UiState, lang: Lang, active_page: &str, loops: &[Value]) -> String {
    let mut links = String::new();
    for entry in loops {
        if !entry.is_object() {
            continue;
        }
        let name = match entry.get("name") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => loopx_core::pydumps::py_str(other),
            None => String::new(),
        };
        if name.is_empty() || !loop_is_visible(state, entry) {
            continue;
        }
        let page = hub::builtin_loop_page(&name)
            .unwrap_or_else(|| hub::generic_loop_page(&name, Some(entry), false));
        links.push_str(&nav_link(
            lang,
            &format!("loop:{name}"),
            &format!("/loops/{}", pyfmt::quote(&name, "/")),
            &page.label,
            &page.icon,
            active_page,
            "nav-child",
        ));
    }
    links
}

/// `_sidebar_html(active_page)`.
pub fn sidebar_html(state: &UiState, lang: Lang, active_page: &str) -> String {
    let loops = registered_loops(state);
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
            ));
            if key == "loops" {
                nav_html.push_str(&loop_children(state, lang, active_page, &loops));
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
    fn link_markup() {
        assert_eq!(
            nav_link(Lang::Fr, "runs", "/runs", "Runs", "<i/>", "runs", ""),
            "<a href='/runs' title='Exécutions'><span class='nav-icon'><i/></span><span class='nav-label'>Exécutions</span></a>"
                .replace("<a href='/runs' title='Exécutions'>", "<a href='/runs' title='Exécutions' class='active'>")
        );
    }
}
