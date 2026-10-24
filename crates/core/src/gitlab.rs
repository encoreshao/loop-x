//! Minimal GitLab REST client, replacing the external gitlab-config skill's
//! `gitlab_api.py`. Reads `~/.gitlab/config.json` (see docs/contract/json-files.md).

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::Value;
use std::path::PathBuf;

/// Python's `urllib.parse.quote(s, safe='')` leaves only these unreserved chars.
const QUOTE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

const PER_PAGE: usize = 100;
const MAX_PAGES: usize = 50;

#[derive(Debug, thiserror::Error)]
pub enum GitlabError {
    #[error("Error: Instance '{0}' not found in config\nAvailable instances: {1}")]
    UnknownInstance(String, String),
    #[error("Error: Bundle '{0}' not found in config")]
    UnknownBundle(String),
    #[error("Error: Bundle '{bundle}' is for instance '{actual}', not '{wanted}'")]
    BundleInstanceMismatch {
        bundle: String,
        actual: String,
        wanted: String,
    },
    #[error("Error: Bundle '{0}' has no token configured")]
    BundleNoToken(String),
    #[error(
        "GitLab configuration not found.\n\nOption 1 - Config file:\n  Create \
         ~/.gitlab/config.json with your GitLab instances\n\nOption 2 - Environment variables:\n  \
         export GITLAB_URL='https://gitlab.com'\n  export GITLAB_TOKEN='glpat-xxxxxxxxxxxx'"
    )]
    NotConfigured,
    #[error("HTTP Error: {status} for url ({url})\nResponse: {body}")]
    Http {
        status: u16,
        url: String,
        body: String,
    },
    #[error("Error: {0}")]
    Transport(String),
    #[error("Error: invalid JSON from GitLab: {0}")]
    BadJson(String),
    #[error("Error: cannot read the token from the secret store: {0}")]
    Secret(String),
}

fn config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        paths.push(cwd.join("gitlab_config.json"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".gitlab").join("config.json"));
    }
    paths
}

/// The first readable, parseable GitLab config file, if any.
pub fn load_config() -> Option<Value> {
    config_paths()
        .into_iter()
        .filter(|p| p.exists())
        .find_map(|p| serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok())
}

/// Resolve a project alias to `(project_id, instance, bundle)`; an unknown
/// alias is treated as a literal project id.
pub fn resolve_project_alias(
    config: Option<&Value>,
    alias: &str,
    instance: Option<&str>,
) -> (String, Option<String>, Option<String>) {
    let entry = config
        .and_then(|c| c.get("projects"))
        .and_then(|p| p.get(alias));
    let Some(entry) = entry else {
        return (alias.to_string(), instance.map(str::to_owned), None);
    };
    let text = |k: &str| entry.get(k).and_then(Value::as_str).map(str::to_owned);
    (
        text("project_id").unwrap_or_else(|| alias.to_string()),
        instance.map(str::to_owned).or_else(|| text("instance")),
        text("bundle"),
    )
}

pub struct Client {
    api_url: String,
    token: String,
}

/// A token as written in the config: plain, or a `keychain:` reference (see `secrets migrate`).
fn resolve_secret(value: &str) -> Result<String, GitlabError> {
    crate::keychain::resolve_value(value).map_err(|e| GitlabError::Secret(e.to_string()))
}

impl Client {
    /// Build a client for `instance` (config default if `None`), optionally
    /// overriding the token with an access bundle's.
    pub fn from_config(
        config: Option<&Value>,
        instance: Option<&str>,
        bundle: Option<&str>,
    ) -> Result<Self, GitlabError> {
        if let Some(config) = config {
            let name = instance
                .map(str::to_owned)
                .or_else(|| config.get("default")?.as_str().map(str::to_owned));
            if let Some(name) = name.filter(|n| !n.is_empty()) {
                let instances = config.get("instances").and_then(Value::as_object);
                let Some(inst) = instances.and_then(|i| i.get(&name)) else {
                    let available = instances
                        .map(|i| i.keys().cloned().collect::<Vec<_>>().join(", "))
                        .unwrap_or_default();
                    return Err(GitlabError::UnknownInstance(name, available));
                };
                let text = |k: &str| inst.get(k).and_then(Value::as_str).unwrap_or("");
                let url = text("url").trim_end_matches('/').to_string();
                let mut token = resolve_secret(text("token"))?;
                if let Some(b) = bundle {
                    let entry = config
                        .get("bundles")
                        .and_then(|bs| bs.get(b))
                        .ok_or_else(|| GitlabError::UnknownBundle(b.to_string()))?;
                    let actual = entry.get("instance").and_then(Value::as_str).unwrap_or("");
                    if actual != name {
                        return Err(GitlabError::BundleInstanceMismatch {
                            bundle: b.to_string(),
                            actual: actual.to_string(),
                            wanted: name,
                        });
                    }
                    token =
                        resolve_secret(entry.get("token").and_then(Value::as_str).unwrap_or(""))?;
                    if token.is_empty() {
                        return Err(GitlabError::BundleNoToken(b.to_string()));
                    }
                }
                if !url.is_empty() && !token.is_empty() {
                    return Ok(Self::new(&url, token));
                }
            }
        }
        let url = std::env::var("GITLAB_URL").unwrap_or_default();
        let token = std::env::var("GITLAB_TOKEN").unwrap_or_default();
        if url.is_empty() || token.is_empty() {
            return Err(GitlabError::NotConfigured);
        }
        Ok(Self::new(url.trim_end_matches('/'), token))
    }

    fn new(base_url: &str, token: String) -> Self {
        Self {
            api_url: format!("{base_url}/api/v4"),
            token,
        }
    }

    fn get(&self, endpoint: &str, params: &[(String, String)]) -> Result<Value, GitlabError> {
        let url = format!("{}/{endpoint}", self.api_url);
        let mut req = ureq::get(&url).set("PRIVATE-TOKEN", &self.token);
        for (k, v) in params {
            req = req.query(k, v);
        }
        match req.call() {
            Ok(resp) => {
                let text = resp
                    .into_string()
                    .map_err(|e| GitlabError::Transport(e.to_string()))?;
                if text.is_empty() {
                    return Ok(Value::Null);
                }
                serde_json::from_str(&text).map_err(|e| GitlabError::BadJson(e.to_string()))
            }
            Err(ureq::Error::Status(status, resp)) => Err(GitlabError::Http {
                status,
                url,
                body: resp.into_string().unwrap_or_default(),
            }),
            Err(e) => Err(GitlabError::Transport(e.to_string())),
        }
    }

    /// GET every page of a list endpoint (GitLab silently caps one page at 20).
    pub fn get_all(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
    ) -> Result<Vec<Value>, GitlabError> {
        let mut results = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut query: Vec<(String, String)> = params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            query.push(("per_page".into(), PER_PAGE.to_string()));
            query.push(("page".into(), page.to_string()));
            let batch = match self.get(endpoint, &query)? {
                Value::Array(items) if !items.is_empty() => items,
                _ => break,
            };
            let short = batch.len() < PER_PAGE;
            results.extend(batch);
            if short {
                break;
            }
        }
        Ok(results)
    }

    /// Open (or `state`) issues of a project, optionally filtered server-side.
    pub fn list_issues(&self, project_id: &str, state: &str) -> Result<Vec<Value>, GitlabError> {
        self.list_issues_filtered(project_id, state, &[], None, None)
    }

    pub fn list_issues_filtered(
        &self,
        project_id: &str,
        state: &str,
        labels: &[String],
        assignee: Option<&str>,
        author: Option<&str>,
    ) -> Result<Vec<Value>, GitlabError> {
        let endpoint = format!("projects/{}/issues", quote(project_id));
        let joined = labels.join(",");
        let mut params = vec![("state", state)];
        if !labels.is_empty() {
            params.push(("labels", &joined));
        }
        if let Some(a) = assignee.filter(|a| !a.is_empty()) {
            params.push(("assignee_username", a));
        }
        if let Some(a) = author.filter(|a| !a.is_empty()) {
            params.push(("author_username", a));
        }
        self.get_all(&endpoint, &params)
    }

    pub fn list_merge_requests(
        &self,
        project_id: &str,
        state: &str,
        assignee: Option<&str>,
        author: Option<&str>,
    ) -> Result<Vec<Value>, GitlabError> {
        let endpoint = format!("projects/{}/merge_requests", quote(project_id));
        let mut params = vec![("state", state)];
        if let Some(a) = assignee.filter(|a| !a.is_empty()) {
            params.push(("assignee_username", a));
        }
        if let Some(a) = author.filter(|a| !a.is_empty()) {
            params.push(("author_username", a));
        }
        self.get_all(&endpoint, &params)
    }

    /// GET a single resource (no pagination).
    pub fn get_json(&self, endpoint: &str) -> Result<Value, GitlabError> {
        self.get(endpoint, &[])
    }

    /// POST a JSON body and return the parsed response (`null` when empty).
    pub fn post_json(&self, endpoint: &str, body: &Value) -> Result<Value, GitlabError> {
        let url = format!("{}/{endpoint}", self.api_url);
        let result = ureq::post(&url)
            .set("PRIVATE-TOKEN", &self.token)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        match result {
            Ok(resp) => {
                let text = resp
                    .into_string()
                    .map_err(|e| GitlabError::Transport(e.to_string()))?;
                if text.is_empty() {
                    return Ok(Value::Null);
                }
                serde_json::from_str(&text).map_err(|e| GitlabError::BadJson(e.to_string()))
            }
            Err(ureq::Error::Status(status, resp)) => Err(GitlabError::Http {
                status,
                url,
                body: resp.into_string().unwrap_or_default(),
            }),
            Err(e) => Err(GitlabError::Transport(e.to_string())),
        }
    }

    /// PUT a JSON body (e.g. `{"add_labels": "..."}` on an issue).
    pub fn put_json(&self, endpoint: &str, body: &Value) -> Result<Value, GitlabError> {
        let url = format!("{}/{endpoint}", self.api_url);
        let result = ureq::put(&url)
            .set("PRIVATE-TOKEN", &self.token)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        match result {
            Ok(resp) => {
                let text = resp
                    .into_string()
                    .map_err(|e| GitlabError::Transport(e.to_string()))?;
                if text.is_empty() {
                    return Ok(Value::Null);
                }
                serde_json::from_str(&text).map_err(|e| GitlabError::BadJson(e.to_string()))
            }
            Err(ureq::Error::Status(status, resp)) => Err(GitlabError::Http {
                status,
                url,
                body: resp.into_string().unwrap_or_default(),
            }),
            Err(e) => Err(GitlabError::Transport(e.to_string())),
        }
    }

    pub fn current_user(&self) -> Result<Value, GitlabError> {
        self.get_json("user")
    }

    pub fn get_project(&self, project_id: &str) -> Result<Value, GitlabError> {
        self.get_json(&format!("projects/{}", quote(project_id)))
    }

    pub fn get_project_members(&self, project_id: &str) -> Result<Value, GitlabError> {
        self.get_json(&format!("projects/{}/members/all", quote(project_id)))
    }

    pub fn get_group(&self, group_id: &str) -> Result<Value, GitlabError> {
        self.get_json(&format!("groups/{}", quote(group_id)))
    }

    pub fn get_group_members(&self, group_id: &str) -> Result<Value, GitlabError> {
        self.get_json(&format!("groups/{}/members/all", quote(group_id)))
    }

    /// The issue plus all of its notes (as `notes`).
    pub fn get_issue(&self, project_id: &str, iid: i64) -> Result<Value, GitlabError> {
        let base = format!("projects/{}/issues/{iid}", quote(project_id));
        let mut issue = self.get_json(&base)?;
        let notes = self.get_all(&format!("{base}/notes"), &[])?;
        if let Some(obj) = issue.as_object_mut() {
            obj.insert("notes".into(), Value::Array(notes));
        }
        Ok(issue)
    }

    /// The merge request plus its `changes` and `notes`.
    pub fn get_merge_request(&self, project_id: &str, iid: i64) -> Result<Value, GitlabError> {
        let base = format!("projects/{}/merge_requests/{iid}", quote(project_id));
        let mut mr = self.get_json(&base)?;
        let changes = self.get_json(&format!("{base}/changes"))?;
        let notes = self.get_all(&format!("{base}/notes"), &[])?;
        if let Some(obj) = mr.as_object_mut() {
            obj.insert(
                "changes".into(),
                changes
                    .get("changes")
                    .cloned()
                    .unwrap_or(Value::Array(vec![])),
            );
            obj.insert("notes".into(), Value::Array(notes));
        }
        Ok(mr)
    }

    pub fn post_issue_comment(
        &self,
        project_id: &str,
        iid: i64,
        body: &str,
    ) -> Result<Value, GitlabError> {
        self.post_json(
            &format!("projects/{}/issues/{iid}/notes", quote(project_id)),
            &serde_json::json!({ "body": body }),
        )
    }

    pub fn post_mr_comment(
        &self,
        project_id: &str,
        iid: i64,
        body: &str,
    ) -> Result<Value, GitlabError> {
        self.post_json(
            &format!("projects/{}/merge_requests/{iid}/notes", quote(project_id)),
            &serde_json::json!({ "body": body }),
        )
    }

    /// Unified-diff-ish text for a merge request, like `gitlab_api.py get-diff`.
    pub fn get_diff_content(&self, project_id: &str, iid: i64) -> Result<String, GitlabError> {
        let changes = self.get_json(&format!(
            "projects/{}/merge_requests/{iid}/changes",
            quote(project_id)
        ))?;
        let mut parts: Vec<String> = Vec::new();
        for c in changes
            .get("changes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let field = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            parts.push(format!(
                "File: {} -> {}",
                field("old_path"),
                field("new_path")
            ));
            parts.push(field("diff"));
            parts.push("\n".to_string());
        }
        Ok(parts.join("\n"))
    }

    /// Issue statistics from the first page (up to 100) of a project's issues.
    pub fn aggregate_issues(&self, project_id: &str) -> Result<Value, GitlabError> {
        let all = self.get(
            &format!("projects/{}/issues", quote(project_id)),
            &[("per_page".into(), "100".into())],
        )?;
        let all = all.as_array().cloned().unwrap_or_default();
        let count = |state: &str| {
            all.iter()
                .filter(|i| i.get("state").and_then(Value::as_str) == Some(state))
                .count()
        };
        let mut by_label = serde_json::Map::new();
        for issue in &all {
            for label in issue
                .get("labels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(l) = label.as_str() {
                    let n = by_label.get(l).and_then(Value::as_i64).unwrap_or(0);
                    by_label.insert(l.to_string(), Value::from(n + 1));
                }
            }
        }
        let mut sorted = all.clone();
        sorted.sort_by(|a, b| updated(b).cmp(updated(a)));
        let recent: Vec<Value> = sorted.into_iter().take(10).collect();
        Ok(serde_json::json!({
            "total": all.len(),
            "opened": count("opened"),
            "closed": count("closed"),
            "by_label": by_label,
            "recent": recent,
        }))
    }
}

fn updated(issue: &Value) -> &str {
    issue
        .get("updated_at")
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// `{alias: [issue, ...]}` of open issues assigned to `username`, in alias order.
pub fn list_assigned_issues(
    config: Option<&Value>,
    aliases: &[String],
    username: &str,
) -> Result<Vec<(String, Vec<Value>)>, GitlabError> {
    let mut out = Vec::new();
    for alias in aliases {
        let (project_id, instance, bundle) = resolve_project_alias(config, alias, None);
        let client = Client::from_config(config, instance.as_deref(), bundle.as_deref())?;
        let issues = client.list_issues(&project_id, "opened")?;
        out.push((alias.clone(), filter_assigned_to(issues, username)));
    }
    Ok(out)
}

/// Keep only issues where `username` appears in the `assignees` list.
pub fn filter_assigned_to(issues: Vec<Value>, username: &str) -> Vec<Value> {
    issues
        .into_iter()
        .filter(|issue| {
            issue
                .get("assignees")
                .and_then(Value::as_array)
                .is_some_and(|a| {
                    a.iter()
                        .any(|u| u.get("username").and_then(Value::as_str) == Some(username))
                })
        })
        .collect()
}

/// `urllib.parse.quote(s, safe='')`.
pub fn quote(s: &str) -> String {
    utf8_percent_encode(s, QUOTE).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn quote_matches_python_safe_empty() {
        assert_eq!(quote("grp/sub/proj"), "grp%2Fsub%2Fproj");
        assert_eq!(quote("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(quote("é 1"), "%C3%A9%201");
    }

    #[test]
    fn alias_resolves_id_instance_and_bundle() {
        let cfg =
            json!({"projects": {"web": {"project_id": "g/web", "instance": "i", "bundle": "b"}}});
        assert_eq!(
            resolve_project_alias(Some(&cfg), "web", None),
            ("g/web".into(), Some("i".into()), Some("b".into()))
        );
    }

    #[test]
    fn explicit_instance_overrides_alias_instance() {
        let cfg = json!({"projects": {"web": {"project_id": "g/web", "instance": "i"}}});
        let (_, inst, _) = resolve_project_alias(Some(&cfg), "web", Some("x"));
        assert_eq!(inst.as_deref(), Some("x"));
    }

    #[test]
    fn unknown_alias_is_a_literal_project_id() {
        assert_eq!(
            resolve_project_alias(None, "g/raw", None),
            ("g/raw".into(), None, None)
        );
    }

    #[test]
    fn unknown_instance_lists_available() {
        let cfg = json!({"default": "nope", "instances": {"a": {"url": "http://x", "token": "t"}}});
        let err = Client::from_config(Some(&cfg), None, None).err().unwrap();
        assert!(err.to_string().contains("Available instances: a"));
    }

    #[test]
    fn a_malformed_token_reference_is_a_secret_error_not_a_token() {
        let cfg = json!({"default": "a", "instances": {"a": {"url": "http://x", "token": "keychain:oops"}}});
        let err = Client::from_config(Some(&cfg), None, None).err().unwrap();
        assert!(matches!(err, GitlabError::Secret(_)), "{err}");
    }

    #[test]
    fn bundle_must_match_instance() {
        let cfg = json!({"default": "a",
            "instances": {"a": {"url": "http://x", "token": "t"}},
            "bundles": {"bd": {"instance": "other", "token": "bt"}}});
        let err = Client::from_config(Some(&cfg), None, Some("bd"))
            .err()
            .unwrap();
        assert!(matches!(err, GitlabError::BundleInstanceMismatch { .. }));
    }
}
