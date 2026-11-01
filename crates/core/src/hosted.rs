//! Loop projects bound through a connector account (`code_host` in
//! `projects.json`): GitHub repositories, or GitLab ones reached through a
//! connector rather than `~/.gitlab/config.json`.
//!
//! The GitLab issue loop, its agent helpers (`loopx issues`, `loopx gitlab`)
//! and the dashboard all read issues and merge requests as GitLab-shaped
//! JSON. This module serves a hosted project's issues, comments and change
//! requests in that same shape (`iid`, `description`, `notes`,
//! `author.username`, `source_branch`, ...), so every consumer works
//! unchanged whichever host the project lives on. Each value also carries
//! `provider` (`github` / `gitlab`).

use crate::config::{default_config_path, Config};
use crate::connectors::ConnectorSource;
use crate::providers::binding::binding_of;
use crate::providers::{
    resolve, ChangeRequest, CodeHost, Comment, CrFilter, CrState, Issue, IssueState, IssueTracker,
    PResult, Provider, Resolved, DEFAULT_CAP,
};
use serde_json::{json, Value};

/// Does this project go through a connector account (`code_host`) instead
/// of the legacy `instance` + `project_id` GitLab config?
pub fn is_hosted(project: &Value) -> bool {
    project.get("code_host").is_some_and(Value::is_object)
}

/// The tracker and code host of a hosted project.
pub fn resolve_project(src: &dyn ConnectorSource, project: &Value) -> PResult<Resolved> {
    resolve(src, &binding_of(project)?)
}

/// The alias's project from `projects.json` when it is hosted, else `None`
/// (unknown alias, unreadable config, or a legacy GitLab project).
pub fn hosted_project(alias: &str) -> Option<Value> {
    hosted_project_in(&default_config_path(), alias)
}

pub fn hosted_project_in(config_path: &std::path::Path, alias: &str) -> Option<Value> {
    Config::load(config_path)
        .ok()?
        .project(alias)
        .ok()
        .filter(is_hosted)
}

/// The alias whose `code_host` is `account` + `repo`: the `<instance>
/// <project_id>` pair the agent passes to cache-keyed helpers.
pub fn alias_for(cfg: &Config, account: &str, repo: &str) -> Option<String> {
    cfg.aliases().ok()?.into_iter().find(|a| {
        cfg.project(a).ok().is_some_and(|p| {
            is_hosted(&p)
                && p["code_host"]["account"].as_str() == Some(account)
                && p["code_host"]["repo"].as_str() == Some(repo)
        })
    })
}

/// The key a hosted project's local data (issue cache, notes watermarks,
/// project memory) is stored under: `<provider>/<account>`, which
/// `gitlab_cache` keeps in `<LOOP_ENGINEERING_HOME>/data/<provider>/<account>`.
/// An account that no longer exists keeps its plain id.
pub fn data_instance(project: &Value, paths: &crate::connectors_config::Paths) -> String {
    let account = project["code_host"]["account"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    crate::connectors_config::get_account(&account, paths)
        .ok()
        .flatten()
        .and_then(|a| Provider::from_kind(&a.kind))
        .filter(|p| p.connected())
        .map_or(account.clone(), |p| format!("{}/{account}", p.kind()))
}

/// Whose issues the loop works: the project's own `assignee_username`, else
/// the account the token belongs to. A GitHub login rarely matches the
/// GitLab username the top-level `assignee_username` names.
pub fn assignee(project: &Value, tracker: &dyn IssueTracker) -> PResult<String> {
    match project
        .get("assignee_username")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(u) => Ok(u.to_string()),
        None => tracker.me(),
    }
}

fn user(name: &str) -> Value {
    json!({"username": name, "name": name})
}

fn users(names: &[String]) -> Value {
    Value::Array(names.iter().map(|n| user(n)).collect())
}

/// An issue as GitLab's issue JSON.
pub fn issue_json(provider: Provider, i: &Issue) -> Value {
    json!({
        "id": i.number,
        "iid": i.number,
        "project_id": i.repo,
        "title": i.title,
        "description": i.body,
        "state": match i.state { IssueState::Open => "opened", IssueState::Closed => "closed" },
        "labels": i.labels,
        "assignees": users(&i.assignees),
        "author": user(&i.author),
        "web_url": i.web_url,
        "created_at": i.created_at,
        "updated_at": i.updated_at,
        "provider": provider.kind(),
    })
}

/// A change request as GitLab's merge request JSON.
pub fn cr_json(provider: Provider, c: &ChangeRequest, reference: &str) -> Value {
    json!({
        "id": c.number,
        "iid": c.number,
        "project_id": c.repo,
        "title": c.title,
        "description": c.description,
        "source_branch": c.source_branch,
        "target_branch": c.target_branch,
        "sha": c.head_sha,
        "state": match c.state {
            CrState::Open => "opened",
            CrState::Merged => "merged",
            CrState::Closed => "closed",
        },
        "draft": c.draft,
        "author": user(&c.author),
        "assignees": users(&c.assignees),
        "reviewers": users(&c.reviewers),
        "web_url": c.web_url,
        "updated_at": c.updated_at,
        "references": {"full": reference},
        "provider": provider.kind(),
    })
}

/// The comment's numeric note id. Issue comments keep the provider's own
/// (one increasing sequence). Change request comments mix sequences on
/// GitHub (issue-style, diff and review summaries), so they are ordered by
/// time instead: creation second times 1000, plus the id's last three
/// digits to keep two comments of the same second apart. Stable across
/// syncs, so the agent's "last seen" watermark keeps working.
fn note_id(c: &Comment, by_time: bool) -> i64 {
    let digits: String = c.id.chars().filter(char::is_ascii_digit).collect();
    let raw: i64 = digits.parse().unwrap_or(0);
    if !by_time {
        return raw;
    }
    let secs = chrono::DateTime::parse_from_rfc3339(&c.created_at)
        .map(|t| t.timestamp())
        .unwrap_or(0);
    secs * 1000 + raw % 1000
}

/// A comment as a GitLab note; diff comments keep `position.new_path` /
/// `new_line` like GitLab's.
pub fn comment_json(c: &Comment, by_time: bool) -> Value {
    let mut note = json!({
        "id": note_id(c, by_time),
        "body": c.body,
        "author": user(&c.author),
        "created_at": c.created_at,
        "system": false,
    });
    if let Some(p) = &c.position {
        note["position"] = json!({"new_path": p.path, "new_line": p.line});
    }
    note
}

/// Open issues assigned to `user`, as GitLab issue JSON.
pub fn assigned_issues(tracker: &dyn IssueTracker, user: &str) -> PResult<Vec<Value>> {
    let p = tracker.provider();
    Ok(tracker
        .list_assigned_open(user, DEFAULT_CAP)?
        .iter()
        .map(|i| issue_json(p, i))
        .collect())
}

/// The open change request from the loop's `loop/issue-<n>` branch, as
/// `{iid, web_url, notes}`, or `None` when there is none.
pub fn loop_mr(code: &dyn CodeHost, n: u64) -> PResult<Option<Value>> {
    let branch = crate::worktree::branch_name(&n.to_string());
    let Some(cr) = code.find_by_source_branch(&branch, Some(CrState::Open))? else {
        return Ok(None);
    };
    let mut notes: Vec<Value> = code
        .cr_comments(cr.number)?
        .iter()
        .map(|c| comment_json(c, true))
        .collect();
    notes.sort_by_key(|n| n["id"].as_i64().unwrap_or(0));
    Ok(Some(json!({
        "iid": cr.number,
        "web_url": cr.web_url,
        "notes": notes,
    })))
}

/// The issue with its `notes` (what `loopx gitlab get-issue` returns).
pub fn issue_with_notes(tracker: &dyn IssueTracker, n: u64) -> PResult<Value> {
    let mut issue = issue_json(tracker.provider(), &tracker.issue(n)?);
    issue["notes"] = Value::Array(
        tracker
            .comments(n)?
            .iter()
            .map(|c| comment_json(c, false))
            .collect(),
    );
    Ok(issue)
}

/// Is the loop done with this issue for good: closed, or the change request
/// from its `loop/issue-<n>` branch merged? Decides when that branch is deleted.
pub fn issue_finished(r: &Resolved, n: u64) -> PResult<bool> {
    if r.tracker.issue(n)?.state == IssueState::Closed {
        return Ok(true);
    }
    let branch = crate::worktree::branch_name(&n.to_string());
    Ok(r.code
        .find_by_source_branch(&branch, Some(CrState::Merged))?
        .is_some())
}

/// Open change requests the user authored, is assigned to or reviews,
/// de-duplicated, as GitLab merge request JSON.
pub fn my_change_requests(code: &dyn CodeHost, user: &str) -> PResult<Vec<Value>> {
    let p = code.provider();
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for filter in [
        CrFilter::Assigned,
        CrFilter::ReviewRequested,
        CrFilter::Mine,
    ] {
        for cr in code.change_requests(filter, user, DEFAULT_CAP)? {
            if seen.insert(cr.number) {
                out.push(cr_json(p, &cr, &code.cr_ref(cr.number)));
            }
        }
    }
    Ok(out)
}

/// What the dashboard's Tasks view lists for a hosted project: the issues
/// assigned to its assignee (marked `_assigned_to_me`, so they land in "My
/// Queue") and their open change requests. Each list fails on its own.
pub fn dashboard_lists(
    r: &Resolved,
    project: &Value,
) -> (Result<Vec<Value>, String>, Result<Vec<Value>, String>) {
    let user = match assignee(project, r.tracker.as_ref()) {
        Ok(u) => u,
        Err(e) => return (Err(e.to_string()), Err(e.to_string())),
    };
    let issues = assigned_issues(r.tracker.as_ref(), &user)
        .map(|mut list| {
            for i in &mut list {
                i["_assigned_to_me"] = Value::Bool(true);
            }
            list
        })
        .map_err(|e| e.to_string());
    let mrs = my_change_requests(r.code.as_ref(), &user).map_err(|e| e.to_string());
    (issues, mrs)
}

fn number(args: &[String], i: usize, command: &str) -> Result<u64, String> {
    args.get(i)
        .ok_or_else(|| format!("Invalid arguments for command: {command}"))?
        .trim()
        .parse()
        .map_err(|_| format!("Invalid arguments for command: {command}"))
}

/// `loopx gitlab <command> <alias> <args...>` for a hosted alias, in the
/// GitLab JSON shape the agent already reads. `args` are what follows the
/// alias. `sync-issue` returns the fresh issue (with `notes` and `loop_mr`);
/// the caller merges it into the local cache.
pub fn helper(
    r: &Resolved,
    project: &Value,
    command: &str,
    args: &[String],
) -> Result<Value, String> {
    let err = |e: crate::providers::ProviderError| e.to_string();
    let user = || assignee(project, r.tracker.as_ref()).map_err(err);
    match command {
        "project-info" => Ok(json!({
            "project_id": project["code_host"]["repo"],
            "instance": project["code_host"]["account"],
            "bundle": project.get("bundle").cloned().unwrap_or(Value::Null),
            "provider": r.code.provider().kind(),
        })),
        "get-issue" | "sync-issue" => {
            let n = number(args, 0, command)?;
            let mut issue = issue_with_notes(r.tracker.as_ref(), n).map_err(err)?;
            if command == "sync-issue" {
                // Review comments on the loop's own change request reach the
                // agent with the issue's; a failed lookup never fails the sync.
                issue["loop_mr"] = match loop_mr(r.code.as_ref(), n) {
                    Ok(mr) => mr.unwrap_or(Value::Null),
                    Err(e) => {
                        eprintln!("sync-issue: change request lookup failed: {e}");
                        Value::Null
                    }
                };
            }
            Ok(issue)
        }
        "list-issues" => Ok(Value::Array(
            assigned_issues(r.tracker.as_ref(), &user()?).map_err(err)?,
        )),
        "list-mrs" => Ok(Value::Array(
            my_change_requests(r.code.as_ref(), &user()?).map_err(err)?,
        )),
        "post-issue-comment" | "post-mr-comment" => {
            let n = number(args, 0, command)?;
            let body = args
                .get(1)
                .ok_or_else(|| format!("Invalid arguments for command: {command}"))?;
            // A GitHub pull request takes comments through its issue.
            r.tracker.post_comment(n, body).map_err(err)?;
            Ok(json!({"posted": true, "iid": n}))
        }
        "whoami" => Ok(json!({"username": r.tracker.me().map_err(err)?})),
        other => Err(format!(
            "{other} is not supported for a {} project",
            r.code.provider().kind()
        )),
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! An in-memory tracker + code host for tests.
    use super::*;
    use crate::providers::{
        DraftNote, DraftOutcome, FileDiff, Job, PipelineRun, ProviderError, RepoInfo, Tag,
    };
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub struct Host {
        pub issues: Vec<Issue>,
        pub comments: Vec<Comment>,
        pub crs: Vec<ChangeRequest>,
        pub cr_comments: Vec<Comment>,
        pub me: String,
        /// Every write, as `"verb args"`.
        pub writes: Mutex<Vec<String>>,
        pub fail_open: bool,
    }

    impl Host {
        pub fn log(&self, s: String) {
            self.writes.lock().unwrap().push(s);
        }
        pub fn writes(&self) -> Vec<String> {
            self.writes.lock().unwrap().clone()
        }
    }

    pub struct Shared(pub Arc<Host>);

    impl IssueTracker for Shared {
        fn provider(&self) -> Provider {
            Provider::Github
        }
        fn me(&self) -> PResult<String> {
            Ok(self.0.me.clone())
        }
        fn list_assigned_open(&self, user: &str, _cap: usize) -> PResult<Vec<Issue>> {
            Ok(self
                .0
                .issues
                .iter()
                .filter(|i| i.state == IssueState::Open && i.assignees.iter().any(|a| a == user))
                .cloned()
                .collect())
        }
        fn issue(&self, n: u64) -> PResult<Issue> {
            self.0
                .issues
                .iter()
                .find(|i| i.number == n)
                .cloned()
                .ok_or_else(|| ProviderError::Malformed(format!("no #{n}")))
        }
        fn comments(&self, _n: u64) -> PResult<Vec<Comment>> {
            Ok(self.0.comments.clone())
        }
        fn post_comment(&self, n: u64, body: &str) -> PResult<()> {
            self.0.log(format!("comment #{n} {body}"));
            Ok(())
        }
        fn add_label(&self, n: u64, label: &str) -> PResult<()> {
            self.0.log(format!("label #{n} {label}"));
            Ok(())
        }
        fn issue_ref(&self, n: u64) -> String {
            format!("o/r#{n}")
        }
        fn issue_url(&self, n: u64) -> String {
            format!("https://github.com/o/r/issues/{n}")
        }
        fn closes_keyword(&self, n: u64) -> String {
            format!("Closes #{n}")
        }
    }

    impl CodeHost for Shared {
        fn provider(&self) -> Provider {
            Provider::Github
        }
        fn me(&self) -> PResult<String> {
            Ok(self.0.me.clone())
        }
        fn repo_info(&self) -> PResult<RepoInfo> {
            Ok(RepoInfo {
                path: "o/r".into(),
                default_branch: "main".into(),
                web_url: "https://github.com/o/r".into(),
            })
        }
        fn change_requests(
            &self,
            filter: CrFilter,
            user: &str,
            _cap: usize,
        ) -> PResult<Vec<ChangeRequest>> {
            Ok(self
                .0
                .crs
                .iter()
                .filter(|c| c.state == CrState::Open)
                .filter(|c| match filter {
                    CrFilter::Mine => c.author == user,
                    CrFilter::ReviewRequested => c.reviewers.iter().any(|r| r == user),
                    CrFilter::Assigned => c.assignees.iter().any(|a| a == user),
                })
                .cloned()
                .collect())
        }
        fn change_request(&self, n: u64) -> PResult<ChangeRequest> {
            self.0
                .crs
                .iter()
                .find(|c| c.number == n)
                .cloned()
                .ok_or_else(|| ProviderError::Malformed(format!("no !{n}")))
        }
        fn find_by_source_branch(
            &self,
            branch: &str,
            state: Option<CrState>,
        ) -> PResult<Option<ChangeRequest>> {
            Ok(self
                .0
                .crs
                .iter()
                .find(|c| c.source_branch == branch && state.is_none_or(|s| s == c.state))
                .cloned())
        }
        fn diff(&self, _n: u64) -> PResult<Vec<FileDiff>> {
            Ok(vec![FileDiff {
                old_path: "src/a.rs".into(),
                new_path: "src/a.rs".into(),
                diff: "@@ -1,1 +1,2 @@\n a\n+b\n".into(),
                new_file: false,
                deleted_file: false,
                renamed_file: false,
            }])
        }
        fn cr_comments(&self, _n: u64) -> PResult<Vec<Comment>> {
            Ok(self.0.cr_comments.clone())
        }
        fn draft_review(
            &self,
            cr: &ChangeRequest,
            notes: &[DraftNote],
            _signature: &str,
        ) -> PResult<DraftOutcome> {
            self.0
                .log(format!("draft_review #{} {}", cr.number, notes.len()));
            Ok(DraftOutcome {
                posted: notes.len(),
                fallback: 0,
            })
        }
        fn open_change_request(
            &self,
            branch: &str,
            target: &str,
            title: &str,
            body: &str,
        ) -> PResult<ChangeRequest> {
            if self.0.fail_open {
                return Err(ProviderError::Malformed("refused".into()));
            }
            self.0
                .log(format!("open {branch}->{target} {title} | {body}"));
            Ok(cr(7, branch, CrState::Open))
        }
        fn update_description(&self, n: u64, body: &str) -> PResult<()> {
            self.0.log(format!("describe !{n} {body}"));
            Ok(())
        }
        fn failed_pipelines(&self, _: &str, _: &str, _: usize) -> PResult<Vec<PipelineRun>> {
            Ok(vec![])
        }
        fn cr_failed_pipelines(&self, _: &ChangeRequest) -> PResult<Vec<PipelineRun>> {
            Ok(vec![])
        }
        fn failed_jobs(&self, _: &PipelineRun) -> PResult<Vec<Job>> {
            Ok(vec![])
        }
        fn job_log_tail(&self, _: &Job) -> PResult<String> {
            Ok(String::new())
        }
        fn tags(&self, _: usize) -> PResult<Vec<Tag>> {
            Ok(vec![])
        }
        fn merged_between(
            &self,
            _: Option<&Tag>,
            _: &Tag,
            _: &str,
            _: usize,
        ) -> PResult<Vec<ChangeRequest>> {
            Ok(vec![])
        }
        fn cr_ref(&self, n: u64) -> String {
            format!("o/r#{n}")
        }
        fn cr_noun(&self) -> &'static str {
            "pull request"
        }
    }

    pub fn resolved(host: Arc<Host>) -> Resolved {
        Resolved {
            tracker: Box::new(Shared(host.clone())),
            code: Box::new(Shared(host)),
        }
    }

    pub fn issue(n: u64, assignee: &str, state: IssueState) -> Issue {
        Issue {
            number: n,
            repo: "o/r".into(),
            title: format!("Issue {n}"),
            body: "body".into(),
            state,
            labels: vec!["bug".into()],
            assignees: vec![assignee.into()],
            author: "pm".into(),
            web_url: format!("https://github.com/o/r/issues/{n}"),
            created_at: "2026-10-01T00:00:00Z".into(),
            updated_at: "2026-10-02T00:00:00Z".into(),
            raw: Value::Null,
        }
    }

    pub fn cr(n: u64, branch: &str, state: CrState) -> ChangeRequest {
        ChangeRequest {
            number: n,
            repo: "o/r".into(),
            title: format!("PR {n}"),
            description: String::new(),
            source_branch: branch.into(),
            target_branch: "main".into(),
            head_sha: "abc".into(),
            state,
            draft: false,
            author: "me".into(),
            reviewers: vec![],
            assignees: vec![],
            web_url: format!("https://github.com/o/r/pull/{n}"),
            updated_at: "2026-10-02T00:00:00Z".into(),
            merged_at: None,
            raw: Value::Null,
        }
    }

    pub fn comment(id: &str, author: &str, at: &str) -> Comment {
        Comment {
            id: id.into(),
            author: author.into(),
            body: format!("comment {id}"),
            created_at: at.into(),
            source: crate::providers::CommentSource::Issue,
            position: None,
            raw: Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;
    use std::sync::Arc;

    #[test]
    fn only_a_code_host_object_makes_a_project_hosted() {
        assert!(is_hosted(
            &json!({"code_host": {"account": "gh", "repo": "o/r"}})
        ));
        assert!(!is_hosted(&json!({"project_id": "1", "instance": "g"})));
        assert!(!is_hosted(&json!({"code_host": "gh"})));
    }

    #[test]
    fn issues_read_as_gitlab_issue_json() {
        let v = issue_json(Provider::Github, &issue(12, "me", IssueState::Open));
        assert_eq!(v["iid"], 12);
        assert_eq!(v["state"], "opened");
        assert_eq!(v["description"], "body");
        assert_eq!(v["assignees"][0]["username"], "me");
        assert_eq!(v["author"]["username"], "pm");
        assert_eq!(v["web_url"], "https://github.com/o/r/issues/12");
        assert_eq!(v["provider"], "github");
        assert_eq!(v["project_id"], "o/r");
    }

    #[test]
    fn assigned_issues_are_the_open_ones_of_that_user() {
        let host = Arc::new(Host {
            issues: vec![
                issue(1, "me", IssueState::Open),
                issue(2, "else", IssueState::Open),
                issue(3, "me", IssueState::Closed),
            ],
            ..Default::default()
        });
        let r = resolved(host);
        let got = assigned_issues(r.tracker.as_ref(), "me").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["iid"], 1);
    }

    #[test]
    fn the_assignee_is_the_projects_own_else_the_token_owner() {
        let host = Arc::new(Host {
            me: "octo".into(),
            ..Default::default()
        });
        let r = resolved(host);
        assert_eq!(assignee(&json!({}), r.tracker.as_ref()).unwrap(), "octo");
        assert_eq!(
            assignee(&json!({"assignee_username": "dev"}), r.tracker.as_ref()).unwrap(),
            "dev"
        );
    }

    #[test]
    fn an_issue_comes_with_its_notes_keyed_by_numeric_id() {
        let host = Arc::new(Host {
            issues: vec![issue(4, "me", IssueState::Open)],
            comments: vec![
                comment("101", "pm", "2026-10-01T00:00:00Z"),
                comment("102", "me", "2026-10-01T01:00:00Z"),
            ],
            ..Default::default()
        });
        let v = issue_with_notes(resolved(host).tracker.as_ref(), 4).unwrap();
        let notes = v["notes"].as_array().unwrap();
        assert_eq!(notes[0]["id"], 101);
        assert_eq!(notes[1]["author"]["username"], "me");
        assert_eq!(notes[0]["system"], false);
    }

    #[test]
    fn the_loop_mr_orders_mixed_comment_kinds_by_time() {
        let mut review = comment("r900", "rev", "2026-10-03T00:00:00Z");
        review.position = Some(crate::providers::DiffPosition {
            path: "lib/a.rs".into(),
            line: Some(3),
            ..Default::default()
        });
        let host = Arc::new(Host {
            crs: vec![cr(5, "loop/issue-4", CrState::Open)],
            cr_comments: vec![review, comment("v1", "rev", "2026-10-02T00:00:00Z")],
            ..Default::default()
        });
        let mr = loop_mr(resolved(host).code.as_ref(), 4).unwrap().unwrap();
        assert_eq!(mr["iid"], 5);
        assert_eq!(mr["web_url"], "https://github.com/o/r/pull/5");
        let notes = mr["notes"].as_array().unwrap();
        assert_eq!(notes[0]["body"], "comment v1");
        assert_eq!(notes[1]["position"]["new_path"], "lib/a.rs");
        assert_eq!(notes[1]["position"]["new_line"], 3);
        assert!(notes[0]["id"].as_i64() < notes[1]["id"].as_i64());
        // the same comment always gets the same id
        let again = note_id(&comment("r900", "rev", "2026-10-03T00:00:00Z"), true);
        assert_eq!(notes[1]["id"].as_i64().unwrap(), again);
    }

    #[test]
    fn no_open_loop_branch_means_no_loop_mr() {
        let host = Arc::new(Host {
            crs: vec![cr(5, "loop/issue-4", CrState::Merged)],
            ..Default::default()
        });
        assert!(loop_mr(resolved(host).code.as_ref(), 4).unwrap().is_none());
    }

    #[test]
    fn an_issue_is_finished_when_closed_or_its_branch_merged() {
        let host = Arc::new(Host {
            issues: vec![
                issue(1, "me", IssueState::Closed),
                issue(2, "me", IssueState::Open),
                issue(3, "me", IssueState::Open),
            ],
            crs: vec![cr(9, "loop/issue-2", CrState::Merged)],
            ..Default::default()
        });
        let r = resolved(host);
        assert!(issue_finished(&r, 1).unwrap());
        assert!(issue_finished(&r, 2).unwrap());
        assert!(!issue_finished(&r, 3).unwrap());
    }

    #[test]
    fn my_change_requests_are_deduplicated_across_roles() {
        let mut both = cr(1, "a", CrState::Open);
        both.assignees = vec!["me".into()];
        both.reviewers = vec!["me".into()];
        both.author = "x".into();
        let mine = cr(2, "b", CrState::Open);
        let mut other = cr(3, "c", CrState::Open);
        other.author = "x".into();
        let host = Arc::new(Host {
            crs: vec![both, mine, other],
            ..Default::default()
        });
        let got = my_change_requests(resolved(host).code.as_ref(), "me").unwrap();
        let iids: Vec<i64> = got.iter().map(|v| v["iid"].as_i64().unwrap()).collect();
        assert_eq!(iids, [1, 2]);
        assert_eq!(got[0]["references"]["full"], "o/r#1");
        assert_eq!(got[0]["state"], "opened");
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn helper_host() -> Arc<Host> {
        Arc::new(Host {
            me: "me".into(),
            issues: vec![issue(4, "me", IssueState::Open)],
            comments: vec![comment("11", "pm", "2026-10-01T00:00:00Z")],
            crs: vec![cr(5, "loop/issue-4", CrState::Open)],
            ..Default::default()
        })
    }

    const PROJECT: &str = r#"{"code_host":{"account":"hub","repo":"o/r"},"bundle":"team"}"#;

    #[test]
    fn helper_project_info_reports_account_repo_and_bundle() {
        let r = resolved(helper_host());
        let p: Value = serde_json::from_str(PROJECT).unwrap();
        let v = helper(&r, &p, "project-info", &[]).unwrap();
        assert_eq!(
            v,
            json!({"project_id": "o/r", "instance": "hub", "bundle": "team", "provider": "github"})
        );
    }

    #[test]
    fn helper_sync_issue_returns_the_issue_its_notes_and_the_loop_mr() {
        let r = resolved(helper_host());
        let p: Value = serde_json::from_str(PROJECT).unwrap();
        let v = helper(&r, &p, "sync-issue", &args(&["4"])).unwrap();
        assert_eq!(v["iid"], 4);
        assert_eq!(v["notes"][0]["id"], 11);
        assert_eq!(v["loop_mr"]["iid"], 5);
        let v = helper(&r, &p, "get-issue", &args(&["4"])).unwrap();
        assert!(v.get("loop_mr").is_none());
    }

    #[test]
    fn helper_posts_issue_comments_and_lists_open_change_requests() {
        let host = helper_host();
        let r = resolved(host.clone());
        let p: Value = serde_json::from_str(PROJECT).unwrap();
        helper(&r, &p, "post-issue-comment", &args(&["4", "On it."])).unwrap();
        assert_eq!(host.writes(), ["comment #4 On it."]);
        let mrs = helper(&r, &p, "list-mrs", &args(&["opened"])).unwrap();
        assert_eq!(mrs[0]["source_branch"], "loop/issue-4");
        let issues = helper(&r, &p, "list-issues", &[]).unwrap();
        assert_eq!(issues[0]["iid"], 4);
    }

    #[test]
    fn helper_refuses_what_it_cannot_do_and_bad_numbers() {
        let r = resolved(helper_host());
        let p: Value = serde_json::from_str(PROJECT).unwrap();
        assert!(helper(&r, &p, "get-issue", &args(&["x"])).is_err());
        assert!(helper(&r, &p, "get-issue", &[]).is_err());
        let e = helper(&r, &p, "sync-group", &args(&["g"])).unwrap_err();
        assert!(e.contains("not supported for a github project"), "{e}");
    }

    #[test]
    fn the_dashboard_sees_assigned_issues_as_mine_and_the_users_pull_requests() {
        let host = helper_host();
        let r = resolved(host);
        let p: Value = serde_json::from_str(PROJECT).unwrap();
        let (issues, mrs) = dashboard_lists(&r, &p);
        let issues = issues.unwrap();
        assert_eq!(issues[0]["iid"], 4);
        assert_eq!(issues[0]["_assigned_to_me"], true);
        assert_eq!(mrs.unwrap()[0]["web_url"], "https://github.com/o/r/pull/5");
    }

    #[test]
    fn a_hosted_projects_data_is_keyed_by_provider_and_account() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("connectors.json");
        std::fs::write(
            &config,
            r#"[{"id": "gh-work", "type": "github", "settings": {}}]"#,
        )
        .unwrap();
        let paths = crate::connectors_config::Paths {
            config_path: Some(config),
            gitlab_config_path: Some(dir.path().join("none")),
            slack_config_path: Some(dir.path().join("none")),
            inbox_config_path: Some(dir.path().join("none")),
        };
        let p = |account: &str| json!({"code_host": {"account": account, "repo": "o/r"}});
        assert_eq!(data_instance(&p("gh-work"), &paths), "github/gh-work");
        // An account that is gone keeps its plain id (nothing to route).
        assert_eq!(data_instance(&p("gone"), &paths), "gone");
    }

    #[test]
    fn alias_for_finds_the_project_by_account_and_repo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("projects.json");
        std::fs::write(
            &path,
            r#"{"gitlab_instance":"g","assignee_username":"dev","worktree_root":"/w","projects":{
                "web":{"local_path":"/w","project_id":"1"},
                "gh":{"local_path":"/g","code_host":{"account":"hub","repo":"o/r"}}}}"#,
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(alias_for(&cfg, "hub", "o/r").as_deref(), Some("gh"));
        assert_eq!(alias_for(&cfg, "hub", "o/x"), None);
        assert!(hosted_project_in(&path, "gh").is_some());
        assert!(hosted_project_in(&path, "web").is_none());
    }
}
