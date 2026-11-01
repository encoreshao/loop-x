//! Which connector accounts and repository a project uses.
//!
//! New shape, per project in `projects.json`:
//! `"code_host": {"account": "github-work", "repo": "owner/name"}` and an
//! optional `"tracker": {"account": ..., "project": ...}` whose missing fields
//! default to the code host's. Legacy projects (`instance`, injected from the
//! top-level `gitlab_instance` by `Config::project`, plus `project_id`) read
//! as a GitLab code host. `code_host` wins when both shapes are present.
//! Nothing is rewritten on read.

use super::{s, ProviderError};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub account: String,
    /// `group/project`, a numeric GitLab id as text, or `owner/name`.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    pub code: Endpoint,
    pub tracker: Endpoint,
}

fn malformed(what: &str) -> ProviderError {
    ProviderError::Malformed(format!("project binding: {what}"))
}

pub fn binding_of(project: &Value) -> Result<Binding, ProviderError> {
    let code = match project.get("code_host") {
        Some(Value::Object(_)) => {
            let ch = &project["code_host"];
            let account = s(ch, "account");
            let path = s(ch, "repo");
            if account.is_empty() || path.is_empty() {
                return Err(malformed("code_host needs account and repo"));
            }
            Endpoint { account, path }
        }
        Some(_) => return Err(malformed("code_host must be an object")),
        None => {
            let account = s(project, "instance");
            let path = s(project, "project_id");
            if account.is_empty() || path.is_empty() {
                return Err(malformed("no code_host, or instance and project_id"));
            }
            Endpoint { account, path }
        }
    };
    let tracker = match project.get("tracker") {
        Some(t @ Value::Object(_)) => Endpoint {
            account: Some(s(t, "account"))
                .filter(|a| !a.is_empty())
                .unwrap_or_else(|| code.account.clone()),
            path: Some(s(t, "project"))
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| code.path.clone()),
        },
        Some(_) => return Err(malformed("tracker must be an object")),
        None => code.clone(),
    };
    Ok(Binding { code, tracker })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ep(a: &str, p: &str) -> Endpoint {
        Endpoint {
            account: a.into(),
            path: p.into(),
        }
    }

    #[test]
    fn new_shape_with_default_tracker() {
        let b = binding_of(&json!({"code_host": {"account": "gh", "repo": "o/r"}})).unwrap();
        assert_eq!(b.code, ep("gh", "o/r"));
        assert_eq!(b.tracker, ep("gh", "o/r"));
    }

    #[test]
    fn tracker_fields_default_one_by_one() {
        let b = binding_of(&json!({
            "code_host": {"account": "gh", "repo": "o/r"},
            "tracker": {"account": "jira"}
        }))
        .unwrap();
        assert_eq!(b.tracker, ep("jira", "o/r"));
        let b = binding_of(&json!({
            "code_host": {"account": "gh", "repo": "o/r"},
            "tracker": {"project": "o/issues"}
        }))
        .unwrap();
        assert_eq!(b.tracker, ep("gh", "o/issues"));
    }

    #[test]
    fn legacy_instance_and_numeric_or_path_project_id() {
        let b = binding_of(&json!({"instance": "work", "project_id": "g/sub/p"})).unwrap();
        assert_eq!(b.code, ep("work", "g/sub/p"));
        assert_eq!(b.tracker, b.code);
        let b = binding_of(&json!({"instance": "work", "project_id": 123})).unwrap();
        assert_eq!(b.code, ep("work", "123"));
    }

    #[test]
    fn code_host_wins_over_legacy_keys() {
        let b = binding_of(&json!({
            "instance": "work", "project_id": "g/p",
            "code_host": {"account": "gh", "repo": "o/r"}
        }))
        .unwrap();
        assert_eq!(b.code, ep("gh", "o/r"));
    }

    #[test]
    fn incomplete_bindings_are_errors_not_panics() {
        for p in [
            json!({}),
            json!({"project_id": "g/p"}),
            json!({"instance": "work"}),
            json!({"code_host": {"account": "gh"}}),
            json!({"code_host": {"repo": "o/r"}}),
            json!({"code_host": "gh"}),
            json!({"code_host": {"account": "", "repo": "o/r"}}),
        ] {
            assert!(
                matches!(binding_of(&p), Err(ProviderError::Malformed(_))),
                "{p}"
            );
        }
    }
}
