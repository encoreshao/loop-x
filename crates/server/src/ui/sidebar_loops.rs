//! The loops the sidebar lists: pinned loops in pin order, then any other
//! enabled loop with something in Needs you. The web sidebar and the Mac app's
//! Loops menu (through the snapshot) both use this list.

use super::state::UiState;
use super::{hub, nav, office, pyfmt};
use loopx_core::loops_config::py_truthy;
use loopx_core::sidebar_pins;
use serde_json::{json, Value};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq)]
pub struct SidebarLoop {
    pub name: String,
    pub label: String,
    pub icon: String,
    pub path: String,
    pub pinned: bool,
    pub attention: bool,
}

impl SidebarLoop {
    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "label": self.label, "path": self.path, "attention": self.attention})
    }
}

pub fn attention_loops(items: &[Value]) -> BTreeSet<String> {
    items.iter().filter_map(office::owner_of).collect()
}

fn entry_name(entry: &Value) -> Option<&str> {
    entry
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
}

fn is_enabled(entry: &Value) -> bool {
    entry.get("enabled").is_none_or(py_truthy)
}

pub fn enabled_names(registry: &[Value]) -> Vec<String> {
    registry
        .iter()
        .filter(|e| is_enabled(e))
        .filter_map(|e| entry_name(e).map(str::to_string))
        .collect()
}

fn sidebar_loop(entry: &Value, name: &str, pinned: bool, attention: bool) -> SidebarLoop {
    let page = hub::builtin_loop_page(name)
        .unwrap_or_else(|| hub::generic_loop_page(name, Some(entry), false));
    SidebarLoop {
        name: name.to_string(),
        label: page.label,
        icon: page.icon,
        path: format!("/loops/{}", pyfmt::quote(name, "/")),
        pinned,
        attention,
    }
}

pub fn build(
    registry: &[Value],
    pins: &[String],
    attention: &BTreeSet<String>,
) -> Vec<SidebarLoop> {
    let enabled = |name: &str| {
        registry
            .iter()
            .find(|e| entry_name(e) == Some(name))
            .filter(|e| is_enabled(e))
    };
    let mut out: Vec<SidebarLoop> = pins
        .iter()
        .filter_map(|name| {
            enabled(name).map(|e| sidebar_loop(e, name, true, attention.contains(name)))
        })
        .collect();
    for entry in registry.iter().filter(|e| is_enabled(e)) {
        let Some(name) = entry_name(entry) else {
            continue;
        };
        if attention.contains(name) && !pins.iter().any(|p| p == name) {
            out.push(sidebar_loop(entry, name, false, true));
        }
    }
    out
}

pub fn pins_for(state: &UiState, registry: &[Value]) -> Vec<String> {
    let path = sidebar_pins::pins_path(&state.loops_config_path());
    sidebar_pins::effective(&sidebar_pins::read(&path), &enabled_names(registry))
}

pub fn for_state(state: &UiState) -> Vec<SidebarLoop> {
    let registry = nav::registered_loops(state);
    let items = super::pages::needs_you::items(state);
    build(
        &registry,
        &pins_for(state, &registry),
        &attention_loops(&items),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(list: &[SidebarLoop]) -> Vec<&str> {
        list.iter().map(|l| l.name.as_str()).collect()
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn registry() -> Vec<Value> {
        vec![
            json!({"name": "issue-loop", "enabled": true}),
            json!({"name": "topic-loop", "enabled": true}),
            json!({"name": "rss-watch-loop", "label": "RSS Watch", "enabled": true}),
            json!({"name": "mr-review-loop", "label": "MR Review", "enabled": false}),
        ]
    }

    #[test]
    fn pinned_loops_come_first_in_pin_order() {
        let list = build(
            &registry(),
            &s(&["rss-watch-loop", "issue-loop"]),
            &BTreeSet::new(),
        );
        assert_eq!(names(&list), ["rss-watch-loop", "issue-loop"]);
        assert!(list.iter().all(|l| l.pinned && !l.attention));
        assert_eq!(list[0].label, "RSS Watch");
        assert_eq!(list[0].path, "/loops/rss-watch-loop");
    }

    #[test]
    fn an_unpinned_loop_needing_attention_follows_the_pins() {
        let attention = BTreeSet::from(["topic-loop".to_string(), "issue-loop".to_string()]);
        let list = build(&registry(), &s(&["issue-loop"]), &attention);
        assert_eq!(names(&list), ["issue-loop", "topic-loop"]);
        assert!(list[0].pinned && list[0].attention);
        assert!(!list[1].pinned && list[1].attention);
    }

    #[test]
    fn disabled_loops_are_never_listed() {
        let attention = BTreeSet::from(["mr-review-loop".to_string()]);
        let list = build(&registry(), &s(&["mr-review-loop"]), &attention);
        assert!(list.is_empty());
    }

    #[test]
    fn unknown_pinned_names_are_skipped() {
        let list = build(&registry(), &s(&["gone", "topic-loop"]), &BTreeSet::new());
        assert_eq!(names(&list), ["topic-loop"]);
    }

    #[test]
    fn attention_comes_from_the_owner_of_each_item() {
        let items = vec![
            json!({"kind": "loop_failed", "loop": "rss-watch-loop"}),
            json!({"kind": "escalated"}),
            json!({"kind": "sign_in"}),
        ];
        assert_eq!(
            attention_loops(&items),
            BTreeSet::from(["issue-loop".to_string(), "rss-watch-loop".to_string()])
        );
    }

    #[test]
    fn a_dismissed_failure_lights_nothing() {
        use chrono::{TimeZone, Utc};
        let d = tempfile::tempdir().unwrap();
        let dismissed = d.path().join("needs_you_dismissed.json");
        let failed = json!({"state": "failed", "updated_at": "2026-11-24T08:00:00+00:00"});
        let loops = serde_json::Map::from_iter([("rss-watch-loop".to_string(), failed)]);
        let now = Utc.with_ymd_and_hms(2026, 11, 24, 12, 0, 0).unwrap();
        let src = loopx_core::needs_you::Sources {
            approvals_dir: &d.path().join("approvals"),
            events_dir: &d.path().join("events"),
            auth_state: &d.path().join("auth_health.json"),
            loops: &loops,
            dismissed: &dismissed,
            memory_root: &d.path().join("memory"),
            slack_status: None,
        };
        let before = loopx_core::needs_you::collect(&src, now);
        assert_eq!(
            attention_loops(&before),
            BTreeSet::from(["rss-watch-loop".to_string()])
        );
        loopx_core::needs_you::dismiss(&dismissed, "loop_failed:rss-watch-loop", now).unwrap();
        assert!(attention_loops(&loopx_core::needs_you::collect(&src, now)).is_empty());
    }

    #[test]
    fn the_json_form_carries_name_label_path_and_attention() {
        let list = build(&registry(), &s(&["rss-watch-loop"]), &BTreeSet::new());
        assert_eq!(
            list[0].to_json(),
            json!({"name": "rss-watch-loop", "label": "RSS Watch", "path": "/loops/rss-watch-loop", "attention": false})
        );
    }
}
