//! Hub pages: one sidebar entry, several tabbed views (`bin/web/hub.py` plus the
//! hub tables of `dashboard_server._hubs()`/`_loop_pages()`). The view is chosen
//! with `?view=`, never `?tab=` (which tabs *inside* a body use).
//!
//! This module only knows the navigation metadata. A page module supplies the
//! body for the view it owns and calls [`render_hub_page`].

use super::i18n;
use super::routes::UiCtx;
use super::shell::{render_shell, ShellOpts};
use super::{consts, pyfmt};

#[derive(Debug, Clone, PartialEq)]
pub struct HubView {
    pub key: String,
    /// English label, translated when rendered.
    pub label: String,
    pub refresh: bool,
    pub lazy_refresh: bool,
}

impl HubView {
    fn new(key: &str, label: &str) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
            refresh: false,
            lazy_refresh: false,
        }
    }

    fn refreshing(mut self) -> Self {
        self.refresh = true;
        self
    }

    fn lazy(mut self) -> Self {
        self.lazy_refresh = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hub {
    /// The sidebar/nav key (`overview`, `runs`, ... or `loops` for loop pages).
    pub key: String,
    pub path: String,
    pub label: String,
    /// Icon markup.
    pub icon: String,
    pub views: Vec<HubView>,
}

fn hub(key: &str, path: &str, label: &str, icon_const: &str, views: Vec<HubView>) -> Hub {
    Hub {
        key: key.to_string(),
        path: path.to_string(),
        label: label.to_string(),
        icon: consts::ds(icon_const).to_string(),
        views,
    }
}

/// The six top-level hubs (`_hubs()`), keyed `overview`, `runs`, `insights`,
/// `harness`, `connectors`, `settings`.
pub fn hubs() -> Vec<Hub> {
    let v = HubView::new;
    vec![
        hub(
            "overview",
            "/",
            "Dashboard",
            "_SECTION_ICON_OVERVIEW",
            vec![
                v("overview", "Overview"),
                v("activity", "Activity").refreshing(),
            ],
        ),
        hub(
            "runs",
            "/runs",
            "Runs",
            "_SECTION_ICON_LOOP_RUNS",
            vec![
                v("loop-runs", "Loop Runs"),
                v("history", "Run History"),
                v("logs", "Logs").refreshing(),
            ],
        ),
        hub(
            "insights",
            "/insights",
            "Insights",
            "_SECTION_ICON_ANALYTICS",
            vec![
                v("analytics", "Analytics"),
                v("cost", "Cost"),
                v("budget", "Budget"),
                v("memory", "Memory"),
            ],
        ),
        hub(
            "harness",
            "/harness",
            "Harness",
            "_SECTION_ICON_AUDIT",
            vec![
                v("gates", "Gates"),
                v("evals", "Evals"),
                v("audit", "Audit"),
            ],
        ),
        hub(
            "connectors",
            "/connectors",
            "Connectors",
            "_SECTION_ICON_CONNECTORS",
            vec![v("accounts", "Accounts"), v("add", "Add")],
        ),
        hub(
            "settings",
            "/settings",
            "Settings",
            "_SECTION_ICON_GENERAL_SETTINGS",
            vec![
                v("general", "General"),
                v("daemons", "Daemons"),
                v("skills", "Skills"),
                v("chat", "Chat tools"),
            ],
        ),
    ]
}

/// One top-level hub by key.
pub fn hub_by_key(key: &str) -> Option<Hub> {
    hubs().into_iter().find(|h| h.key == key)
}

/// `_loop_pages()` for the three built-in loops, `None` for any other name.
pub fn builtin_loop_page(name: &str) -> Option<Hub> {
    let v = HubView::new;
    Some(match name {
        "gitlab-loop" => hub(
            "loops",
            "/loops/gitlab-loop",
            "GitLab",
            "_SECTION_ICON_GITLAB",
            vec![
                v("live", "Live").refreshing(),
                v("tasks", "Tasks").refreshing().lazy(),
                v("projects", "Projects"),
            ],
        ),
        "topic-loop" => hub(
            "loops",
            "/loops/topic-loop",
            "Topic Monitor",
            "_SECTION_ICON_TOPIC_MONITOR",
            vec![v("live", "Live").refreshing(), v("topics", "Topics")],
        ),
        "inbox-triage-loop" => hub(
            "loops",
            "/loops/inbox-triage-loop",
            "Inbox Triage",
            "_SECTION_ICON_INBOX",
            vec![
                v("live", "Live").refreshing(),
                v("drafts", "Drafts"),
                v("setup", "Setup"),
            ],
        ),
        _ => return None,
    })
}

/// `_generic_loop_page`: the page of a registered plugin loop. `loop_entry` is
/// its registry entry (for the `label`); `has_settings` says whether the plugin
/// declares settings fields (adds the Settings view).
pub fn generic_loop_page(
    name: &str,
    loop_entry: Option<&serde_json::Value>,
    has_settings: bool,
) -> Hub {
    let label = loop_entry
        .and_then(|l| l.get("label"))
        .filter(|l| loopx_core::loops_config::py_truthy(l))
        .map_or_else(|| name.to_string(), loopx_core::pydumps::py_str);
    let v = HubView::new;
    let mut views = vec![
        v("live", "Live").refreshing(),
        v("content", "Content"),
        v("history", "History"),
    ];
    if name == "linkedin-autopilot-loop" {
        views.insert(2, v("people", "People"));
    }
    if has_settings {
        views.push(v("settings", "Settings"));
    }
    let icon = match nav_icon_glyph(name) {
        Some(glyph) => {
            format!("<span class='material-symbols-outlined' aria-hidden='true'>{glyph}</span>")
        }
        None => consts::ds("_SECTION_ICON_LOOPS").to_string(),
    };
    let icon = match brand_icon_key(name) {
        Some(brand) => super::logos::brand_nav_icon(&brand),
        None => icon,
    };
    Hub {
        key: "loops".to_string(),
        path: format!("/loops/{}", pyfmt::quote(name, "/")),
        label,
        icon,
        views,
    }
}

fn nav_icon_glyph(name: &str) -> Option<String> {
    consts::value("dashboard_server", "_LOOP_NAV_ICONS")?
        .get(name)?
        .as_str()
        .map(str::to_string)
}

fn brand_icon_key(name: &str) -> Option<String> {
    consts::value("dashboard_server", "_LOOP_BRAND_ICONS")?
        .get(name)?
        .as_str()
        .map(str::to_string)
}

/// `resolve_view`: the requested view, or the first when it is unknown.
pub fn resolve_view<'a>(hub: &'a Hub, requested: Option<&str>) -> &'a HubView {
    hub.views
        .iter()
        .find(|v| Some(v.key.as_str()) == requested)
        .unwrap_or(&hub.views[0])
}

/// `hub_tab_strip_html`: empty for a single-view hub.
pub fn tab_strip_html(
    lang: i18n::Lang,
    hub_path: &str,
    views: &[HubView],
    active_key: &str,
    extra_query: &str,
) -> String {
    if views.len() < 2 {
        return String::new();
    }
    let links: String = views
        .iter()
        .map(|view| {
            let active = view.key == active_key;
            let current = if active { " aria-current='page'" } else { "" };
            let cls = if active { "hub-tab active" } else { "hub-tab" };
            let href = format!("{hub_path}?view={}{extra_query}", view.key);
            format!(
                "<a class='{cls}' href='{}'{current}>{}</a>",
                pyfmt::escape(&href),
                pyfmt::escape(&i18n::t(lang, &view.label, &[]))
            )
        })
        .collect();
    format!(
        "<nav class='hub-tabs' aria-label='{}'>{links}</nav>",
        pyfmt::escape(&i18n::t(lang, "Page sections", &[]))
    )
}

/// `render_hub_page`: the tab strip plus the view's body inside the shell, with
/// that view's own refresh behaviour. `body` is only the view body; the badge is
/// the status pill the page wants in the topbar.
pub fn render_hub_page(
    ctx: &UiCtx,
    hub: &Hub,
    requested_view: Option<&str>,
    body: &str,
    badge_html: &str,
) -> String {
    let current = resolve_view(hub, requested_view);
    let tabs = tab_strip_html(ctx.lang, &hub.path, &hub.views, &current.key, "");
    let title = format!(
        "{} · Loop X Engineering",
        i18n::t(ctx.lang, &hub.label, &[])
    );
    render_shell(
        ctx,
        &title,
        &hub.key,
        badge_html,
        &format!("{tabs}{body}"),
        ShellOpts {
            refresh: current.refresh,
            refresh_note: current.refresh,
            lazy_refresh: current.lazy_refresh,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_view_falls_back_to_the_first() {
        let h = hub_by_key("runs").unwrap();
        assert_eq!(resolve_view(&h, Some("logs")).key, "logs");
        assert_eq!(resolve_view(&h, Some("nope")).key, "loop-runs");
        assert_eq!(resolve_view(&h, None).key, "loop-runs");
    }

    #[test]
    fn tab_strip_marks_the_active_view_and_skips_single_view_hubs() {
        let h = hub_by_key("insights").unwrap();
        let html = tab_strip_html(i18n::Lang::En, &h.path, &h.views, "cost", "");
        assert!(html.contains(
            "<a class='hub-tab active' href='/insights?view=cost' aria-current='page'>Cost</a>"
        ));
        assert!(html.starts_with("<nav class='hub-tabs' aria-label='Page sections'>"));
        assert_eq!(
            tab_strip_html(i18n::Lang::En, "/x", &h.views[..1], "analytics", ""),
            ""
        );
    }

    #[test]
    fn generic_loop_pages() {
        let entry = serde_json::json!({"name": "rss-watch-loop", "label": "RSS Watch"});
        let page = generic_loop_page("rss-watch-loop", Some(&entry), true);
        assert_eq!(page.label, "RSS Watch");
        assert!(page.icon.contains("rss_feed"));
        assert_eq!(page.views.last().unwrap().key, "settings");
        let li = generic_loop_page("linkedin-autopilot-loop", None, false);
        assert_eq!(li.views[2].key, "people");
        assert!(li.icon.contains("brand-nav-mark"));
        assert_eq!(generic_loop_page("x y", None, false).path, "/loops/x%20y");
    }
}
