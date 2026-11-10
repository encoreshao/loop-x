//! The GitLab calls the release MR needs, behind a trait so the rest can be tested
//! without a network.

#[derive(Debug, Clone, PartialEq)]
pub struct CommitInfo {
    pub sha: String,
    pub title: String,
    pub author_name: String,
    pub author_email: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub username: String,
    pub name: String,
    pub email: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MrInfo {
    pub title: String,
    pub author_username: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewMr {
    pub source: String,
    pub target: String,
    pub title: String,
    pub description: String,
}

pub trait ReleaseApi {
    fn branches(&self, project: &str) -> Result<Vec<String>, String>;
    fn default_branch(&self, project: &str) -> Result<String, String>;
    fn branch_head(&self, project: &str, branch: &str) -> Result<String, String>;
    /// Commits in `to` that `from` lacks.
    fn compare(&self, project: &str, from: &str, to: &str) -> Result<Vec<CommitInfo>, String>;
    /// The web URL of an open MR for this pair, if any.
    fn open_mr(&self, project: &str, source: &str, target: &str) -> Result<Option<String>, String>;
    /// The merged MR that brought `sha` into `into_branch`, if any.
    fn commit_mr(
        &self,
        project: &str,
        sha: &str,
        into_branch: &str,
    ) -> Result<Option<MrInfo>, String>;
    fn members(&self, project: &str) -> Result<Vec<Member>, String>;
    fn current_username(&self) -> Result<String, String>;
    /// Creates the MR and returns its web URL.
    fn create_mr(&self, project: &str, mr: &NewMr) -> Result<String, String>;
}

use crate::gitlab::{quote, Client};
use serde_json::{json, Value};

/// The real GitLab behind [`ReleaseApi`].
pub struct GitlabApi {
    client: Client,
}

impl GitlabApi {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// A client for a tracked project (its `projects.json` entry: `instance`, optional
    /// `bundle`), from the `~/.gitlab/config.json` document.
    pub fn for_project(gitlab_config: &Value, project: &Value) -> Result<Self, String> {
        let get = |k: &str| {
            project
                .get(k)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
        };
        let client = Client::from_config(Some(gitlab_config), get("instance"), get("bundle"))
            .map_err(|e| e.to_string())?;
        Ok(Self::new(client))
    }
}

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

impl ReleaseApi for GitlabApi {
    fn branches(&self, project: &str) -> Result<Vec<String>, String> {
        let all = self
            .client
            .get_all(
                &format!("projects/{}/repository/branches", quote(project)),
                &[],
            )
            .map_err(|e| e.to_string())?;
        Ok(all
            .iter()
            .map(|b| text(b, "name"))
            .filter(|n| !n.is_empty())
            .collect())
    }

    fn default_branch(&self, project: &str) -> Result<String, String> {
        let p = self
            .client
            .get_project(project)
            .map_err(|e| e.to_string())?;
        Ok(text(&p, "default_branch"))
    }

    fn branch_head(&self, project: &str, branch: &str) -> Result<String, String> {
        let b = self
            .client
            .get_json(&format!(
                "projects/{}/repository/branches/{}",
                quote(project),
                quote(branch)
            ))
            .map_err(|e| e.to_string())?;
        Ok(b.pointer("/commit/id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    fn compare(&self, project: &str, from: &str, to: &str) -> Result<Vec<CommitInfo>, String> {
        let doc = self
            .client
            .get_json(&format!(
                "projects/{}/repository/compare?from={}&to={}",
                quote(project),
                quote(from),
                quote(to)
            ))
            .map_err(|e| e.to_string())?;
        let commits = doc
            .get("commits")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(commits
            .iter()
            .map(|c| CommitInfo {
                sha: text(c, "id"),
                title: text(c, "title"),
                author_name: text(c, "author_name"),
                author_email: text(c, "author_email"),
            })
            .collect())
    }

    fn open_mr(&self, project: &str, source: &str, target: &str) -> Result<Option<String>, String> {
        let found = self
            .client
            .get_all(
                &format!("projects/{}/merge_requests", quote(project)),
                &[
                    ("state", "opened"),
                    ("source_branch", source),
                    ("target_branch", target),
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(found.first().map(|m| text(m, "web_url")))
    }

    fn commit_mr(
        &self,
        project: &str,
        sha: &str,
        into_branch: &str,
    ) -> Result<Option<MrInfo>, String> {
        let found = self
            .client
            .get_all(
                &format!(
                    "projects/{}/repository/commits/{}/merge_requests",
                    quote(project),
                    quote(sha)
                ),
                &[],
            )
            .map_err(|e| e.to_string())?;
        // A commit belongs to every MR that contains it (a promotion, a closed release MR):
        // only the merged one that landed it on `into_branch` says what the work was.
        let landed = found
            .iter()
            .find(|m| text(m, "state") == "merged" && text(m, "target_branch") == into_branch);
        Ok(landed.map(|m| MrInfo {
            title: text(m, "title"),
            author_username: m
                .pointer("/author/username")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }))
    }

    fn members(&self, project: &str) -> Result<Vec<Member>, String> {
        let all = self
            .client
            .get_all(&format!("projects/{}/members/all", quote(project)), &[])
            .map_err(|e| e.to_string())?;
        Ok(all
            .iter()
            .map(|m| Member {
                username: text(m, "username"),
                name: text(m, "name"),
                email: m
                    .get("email")
                    .and_then(Value::as_str)
                    .filter(|e| !e.is_empty())
                    .map(str::to_string),
            })
            .collect())
    }

    fn current_username(&self) -> Result<String, String> {
        let me = self.client.current_user().map_err(|e| e.to_string())?;
        Ok(text(&me, "username"))
    }

    fn create_mr(&self, project: &str, mr: &NewMr) -> Result<String, String> {
        let made = self
            .client
            .post_json(
                &format!("projects/{}/merge_requests", quote(project)),
                &json!({
                    "source_branch": mr.source,
                    "target_branch": mr.target,
                    "title": mr.title,
                    "description": mr.description,
                    "remove_source_branch": false,
                    "squash": false,
                }),
            )
            .map_err(|e| e.to_string())?;
        Ok(text(&made, "web_url"))
    }
}

/// A scriptable [`ReleaseApi`] for tests in this module tree.
#[cfg(test)]
#[derive(Default)]
pub struct FakeApi {
    pub branches: Vec<String>,
    pub default_branch: String,
    pub heads: std::collections::HashMap<String, String>,
    pub commits: Vec<CommitInfo>,
    pub open_mr: Option<String>,
    pub commit_mrs: std::collections::HashMap<String, MrInfo>,
    pub members: Vec<Member>,
    pub me: String,
    pub create_url: String,
    pub create_error: Option<String>,
    pub created: std::cell::RefCell<Vec<NewMr>>,
    pub commit_mr_calls: std::cell::Cell<usize>,
}

#[cfg(test)]
impl ReleaseApi for FakeApi {
    fn branches(&self, _: &str) -> Result<Vec<String>, String> {
        Ok(self.branches.clone())
    }
    fn default_branch(&self, _: &str) -> Result<String, String> {
        Ok(self.default_branch.clone())
    }
    fn branch_head(&self, _: &str, branch: &str) -> Result<String, String> {
        Ok(self
            .heads
            .get(branch)
            .cloned()
            .unwrap_or_else(|| format!("head-of-{branch}")))
    }
    fn compare(&self, _: &str, _: &str, _: &str) -> Result<Vec<CommitInfo>, String> {
        Ok(self.commits.clone())
    }
    fn open_mr(&self, _: &str, _: &str, _: &str) -> Result<Option<String>, String> {
        Ok(self.open_mr.clone())
    }
    fn commit_mr(&self, _: &str, sha: &str, _into: &str) -> Result<Option<MrInfo>, String> {
        self.commit_mr_calls.set(self.commit_mr_calls.get() + 1);
        Ok(self.commit_mrs.get(sha).cloned())
    }
    fn members(&self, _: &str) -> Result<Vec<Member>, String> {
        Ok(self.members.clone())
    }
    fn current_username(&self) -> Result<String, String> {
        Ok(self.me.clone())
    }
    fn create_mr(&self, _: &str, mr: &NewMr) -> Result<String, String> {
        if let Some(e) = &self.create_error {
            return Err(e.clone());
        }
        self.created.borrow_mut().push(mr.clone());
        Ok(self.create_url.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitlab::Client;
    use std::sync::{Arc, Mutex};

    type Seen = Arc<Mutex<Vec<(String, String)>>>;

    /// Answers each request from `reply(method, path_and_query)`; records `(path, body)`.
    fn stub(reply: fn(&str, &str) -> &'static str) -> (String, Seen) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let seen: Seen = Arc::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Read, Write};
            for s in l.incoming().flatten() {
                let mut r = BufReader::new(&s);
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                let mut parts = line.split(' ');
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                log.lock()
                    .unwrap()
                    .push((path.clone(), String::from_utf8_lossy(&body).into()));
                let out = reply(&method, &path);
                let _ = write!(&s, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}", out.len());
            }
        });
        (url, seen)
    }

    fn api(url: &str) -> GitlabApi {
        GitlabApi::new(Client::with_token(url, "t".into()))
    }

    #[test]
    fn compare_maps_commits() {
        let (url, seen) = stub(|_, p| {
            if p.contains("/repository/compare") {
                r#"{"commits":[{"id":"abc1234","title":"Add export","author_name":"Ann","author_email":"ann@x.io"}]}"#
            } else {
                "[]"
            }
        });
        let got = api(&url).compare("g/app", "master", "staging").unwrap();
        assert_eq!(
            got,
            vec![CommitInfo {
                sha: "abc1234".into(),
                title: "Add export".into(),
                author_name: "Ann".into(),
                author_email: "ann@x.io".into()
            }]
        );
        let path = &seen.lock().unwrap()[0].0;
        assert!(
            path.contains("/projects/g%2Fapp/repository/compare?from=master&to=staging"),
            "{path}"
        );
    }

    #[test]
    fn open_mr_returns_web_url_for_matching_branch_pair() {
        let (url, seen) = stub(|_, p| {
            if p.contains("source_branch=staging") {
                r#"[{"web_url":"https://g/app/-/merge_requests/9"}]"#
            } else {
                "[]"
            }
        });
        let a = api(&url);
        assert_eq!(
            a.open_mr("g/app", "staging", "master").unwrap().as_deref(),
            Some("https://g/app/-/merge_requests/9")
        );
        assert_eq!(a.open_mr("g/app", "other", "master").unwrap(), None);
        let first = &seen.lock().unwrap()[0].0;
        assert!(
            first.contains("state=opened") && first.contains("target_branch=master"),
            "{first}"
        );
    }

    #[test]
    fn create_mr_sends_remove_source_branch_false_and_squash_false() {
        let (url, seen) = stub(|m, _| {
            if m == "POST" {
                r#"{"web_url":"https://g/app/-/merge_requests/10"}"#
            } else {
                "[]"
            }
        });
        let made = api(&url)
            .create_mr(
                "g/app",
                &NewMr {
                    source: "staging".into(),
                    target: "master".into(),
                    title: "Release: x".into(),
                    description: "d".into(),
                },
            )
            .unwrap();
        assert_eq!(made, "https://g/app/-/merge_requests/10");
        let (path, body) = seen.lock().unwrap().last().unwrap().clone();
        assert!(path.ends_with("/projects/g%2Fapp/merge_requests"), "{path}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["source_branch"], "staging");
        assert_eq!(v["target_branch"], "master");
        assert_eq!(v["title"], "Release: x");
        assert_eq!(v["remove_source_branch"], false);
        assert_eq!(v["squash"], false);
    }

    #[test]
    fn members_commit_mr_head_and_default_branch_are_mapped() {
        let (url, _) = stub(|_, p| {
            if p.contains("/members/all") {
                r#"[{"username":"ann","name":"Ann Lee","email":"ann@x.io"},{"username":"bo","name":"Bo"}]"#
            } else if p.contains("/commits/abc/merge_requests") {
                r#"[{"title":"Merge develop into staging","state":"closed","target_branch":"master","author":{"username":"zed"}},
                    {"title":"Promote staging","state":"merged","target_branch":"master","author":{"username":"zed"}},
                    {"title":"Export","state":"merged","target_branch":"staging","author":{"username":"ann"}}]"#
            } else if p.contains("/repository/branches/staging") {
                r#"{"name":"staging","commit":{"id":"h1"}}"#
            } else if p.contains("/repository/branches") {
                r#"[{"name":"master"},{"name":"staging"}]"#
            } else if p.ends_with("/user") {
                r#"{"username":"loopx-bot"}"#
            } else {
                r#"{"default_branch":"master"}"#
            }
        });
        let a = api(&url);
        let m = a.members("g/app").unwrap();
        assert_eq!(m[0].email.as_deref(), Some("ann@x.io"));
        assert_eq!(m[1].email, None);
        assert_eq!(
            a.commit_mr("g/app", "abc", "staging").unwrap(),
            Some(MrInfo {
                title: "Export".into(),
                author_username: "ann".into()
            })
        );
        assert_eq!(
            a.commit_mr("g/app", "abc", "develop").unwrap(),
            None,
            "no merged MR landed it on develop"
        );
        assert_eq!(a.branch_head("g/app", "staging").unwrap(), "h1");
        assert_eq!(a.branches("g/app").unwrap(), vec!["master", "staging"]);
        assert_eq!(a.current_username().unwrap(), "loopx-bot");
        assert_eq!(a.default_branch("g/app").unwrap(), "master");
    }
}
