//! Read-only GitLab config lookups the pages share.

use super::markdown::GitlabPrefixes;
use super::state::UiState;
use serde_json::Value;

fn read_json(path: &std::path::Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// `gitlab_issue_url_prefixes()`: `{alias: "https://host/namespace/project"}` for
/// every project in `projects.json` whose instance has a URL in
/// `~/.gitlab/config.json` (only `url`, never the token). Empty when either file
/// is missing or malformed.
pub fn issue_url_prefixes(state: &UiState) -> GitlabPrefixes {
    let mut out = GitlabPrefixes::new();
    let Some(loop_cfg) = read_json(&state.home_dir.join("projects.json")) else {
        return out;
    };
    let Some(gitlab_cfg) = read_json(&state.user_home.join(".gitlab").join("config.json")) else {
        return out;
    };
    let default_instance = loop_cfg.get("gitlab_instance").and_then(Value::as_str);
    let instances = gitlab_cfg.get("instances").and_then(Value::as_object);
    let Some(projects) = loop_cfg.get("projects").and_then(Value::as_object) else {
        return out;
    };
    for (alias, project) in projects {
        let Some(project_id) = project
            .get("project_id")
            .filter(|v| loopx_core::loops_config::py_truthy(v))
        else {
            continue;
        };
        let instance = project
            .get("instance")
            .and_then(Value::as_str)
            .or(default_instance);
        let base = instance
            .and_then(|i| instances?.get(i))
            .and_then(|inst| inst.get("url"))
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty());
        if let Some(base) = base {
            out.insert(
                alias.clone(),
                format!(
                    "{}/{}",
                    base.trim_end_matches('/'),
                    loopx_core::pydumps::py_str(project_id)
                ),
            );
        }
    }
    out
}
