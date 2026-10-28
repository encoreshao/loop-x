//! Where the GitLab agent starts when choosing a project's install/lint/test
//! commands: the stored ones, what it logged on earlier runs of this project,
//! and what the other tracked projects last used (`verification.commands`).

use crate::config::Config;
use crate::events;
use serde_json::{json, Value};
use std::path::Path;

/// The event the agent emits with the commands it chose for an issue.
pub const EVENT_TYPE: &str = "verification.commands";

/// How far back the ledger is read for logged commands.
const LOOKBACK_DAYS: i64 = 90;

/// `{"stored": {...}, "recent": [...], "other_projects": [...]}` for `alias`:
/// its non-empty stored commands, its last three logged ones (newest first),
/// and every other tracked alias's newest logged ones.
pub fn command_hints(alias: &str, config: &Config, events_dir: Option<&Path>) -> Value {
    let project = config.project(alias).unwrap_or(Value::Null);
    let mut stored = serde_json::Map::new();
    for kind in ["install", "lint", "test"] {
        if let Some(c) = project
            .get(format!("{kind}_cmd"))
            .and_then(Value::as_str)
            .filter(|c| !c.trim().is_empty())
        {
            stored.insert(kind.into(), json!(c));
        }
    }
    let since = (chrono::Utc::now() - chrono::Duration::days(LOOKBACK_DAYS))
        .format("%Y-%m-%d")
        .to_string();
    let mut logged: Vec<Value> = events::iter_events(events_dir, Some(&since), None)
        .into_iter()
        .filter(|e| e.get("event_type").and_then(Value::as_str) == Some(EVENT_TYPE))
        .collect();
    logged.reverse();
    let event_alias = |e: &Value| {
        e.get("project")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let recent: Vec<Value> = logged
        .iter()
        .filter(|e| event_alias(e) == alias)
        .take(3)
        .map(|e| e.get("data").cloned().unwrap_or(Value::Null))
        .collect();
    let mut other_projects = Vec::new();
    for other in config.aliases().unwrap_or_default() {
        if other == alias {
            continue;
        }
        if let Some(e) = logged.iter().find(|e| event_alias(e) == other) {
            let data = e.get("data").cloned().unwrap_or(Value::Null);
            let field = |k: &str| data.get(k).cloned().unwrap_or(json!(""));
            other_projects.push(json!({
                "alias": other,
                "stack": field("stack"),
                "install": field("install"),
                "lint": field("lint"),
                "test": field("test"),
            }));
        }
    }
    json!({"stored": stored, "recent": recent, "other_projects": other_projects})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventFields;

    fn setup() -> (tempfile::TempDir, Config) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("projects.json");
        std::fs::write(
            &p,
            json!({"gitlab_instance": "g", "projects": {
                "web": {"local_path": "/w", "install_cmd": "bundle install", "lint_cmd": "", "test_cmd": "bundle exec rspec"},
                "api": {"local_path": "/a"},
                "cli": {"local_path": "/c"}
            }})
            .to_string(),
        )
        .unwrap();
        let c = Config::load(&p).unwrap();
        (d, c)
    }

    fn log(dir: &Path, alias: &str, test: &str) {
        events::emit(
            EVENT_TYPE,
            "run",
            EventFields {
                project: Some(alias.into()),
                issue_iid: Some(1),
                data: Some(json!({"stack": "rails", "test": test, "lint": "", "install": ""})),
                ..Default::default()
            },
            Some(dir),
        )
        .unwrap();
    }

    #[test]
    fn stored_hints_skip_empty() {
        let (d, c) = setup();
        let h = command_hints("web", &c, Some(&d.path().join("ev")));
        assert_eq!(
            h["stored"],
            json!({"install": "bundle install", "test": "bundle exec rspec"})
        );
        assert_eq!(h["recent"], json!([]));
    }

    #[test]
    fn recent_is_newest_first_and_capped_at_three() {
        let (d, c) = setup();
        let ev = d.path().join("ev");
        for t in ["t1", "t2", "t3", "t4"] {
            log(&ev, "web", t);
        }
        let h = command_hints("web", &c, Some(&ev));
        let tests: Vec<_> = h["recent"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["test"].clone())
            .collect();
        assert_eq!(tests, [json!("t4"), json!("t3"), json!("t2")]);
    }

    #[test]
    fn other_projects_take_each_aliases_newest() {
        let (d, c) = setup();
        let ev = d.path().join("ev");
        log(&ev, "web", "mine");
        log(&ev, "api", "old");
        log(&ev, "api", "new");
        let h = command_hints("web", &c, Some(&ev));
        assert_eq!(
            h["other_projects"],
            json!([{"alias": "api", "stack": "rails", "install": "", "lint": "", "test": "new"}])
        );
    }
}
