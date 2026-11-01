//! GitLab as an issue tracker, code host and account host, over the GitLab
//! connector (`/api/v4` is added by the connector).

use super::*;
use crate::connectors::http::{quote, urlencode};
use crate::connectors::{ApiOpts, Connector};
use serde_json::{json, Value};

pub struct GitLab {
    conn: Box<dyn Connector>,
    project: String,
    /// `group/project` for web links, looked up once when `project` is a
    /// numeric id (a link built from the id is dead).
    web_path: std::sync::OnceLock<String>,
}

impl GitLab {
    pub fn new(conn: Box<dyn Connector>, project: &str) -> Self {
        Self {
            conn,
            project: project.to_string(),
            web_path: std::sync::OnceLock::new(),
        }
    }

    fn p(&self) -> String {
        format!("/projects/{}", quote(&self.project))
    }

    fn get(&self, path: &str) -> PResult<Value> {
        Ok(self.conn.api("GET", path, &ApiOpts::default())?)
    }

    fn send(&self, method: &str, path: &str, body: Value) -> PResult<Value> {
        Ok(self.conn.api(method, path, &ApiOpts::json(body))?)
    }

    fn web_base(&self) -> String {
        self.conn
            .base_url()
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string()
    }

    /// Newest `COMMENT_CAP` notes of `kind` (`issues`/`merge_requests`),
    /// system notes dropped, oldest first.
    fn notes(&self, kind: &str, n: u64, source: CommentSource) -> PResult<Vec<Comment>> {
        let rows = paged(
            self.conn.as_ref(),
            &format!(
                "{}/{kind}/{n}/notes?sort=desc&order_by=created_at",
                self.p()
            ),
            COMMENT_CAP,
            None,
        )?;
        let mut out: Vec<Comment> = rows
            .iter()
            .filter(|r| r.get("system").and_then(Value::as_bool) != Some(true))
            .map(|r| note_from(r, source))
            .collect();
        out.reverse();
        Ok(out)
    }
}

pub(crate) fn me_of(conn: &dyn Connector) -> PResult<String> {
    let v = conn.api("GET", "/user", &ApiOpts::default())?;
    Ok(s(&v, "username"))
}

/// `group/project` from `references.full` (`g/p#1`, `g/p!2`), else "".
fn repo_of(v: &Value) -> String {
    let full = v
        .get("references")
        .map(|r| s(r, "full"))
        .unwrap_or_default();
    full.split(['#', '!']).next().unwrap_or("").to_string()
}

pub(crate) fn issue_from(v: &Value) -> Issue {
    Issue {
        number: num(v, "iid"),
        repo: repo_of(v),
        title: s(v, "title"),
        body: s(v, "description"),
        state: if s(v, "state") == "opened" {
            IssueState::Open
        } else {
            IssueState::Closed
        },
        labels: v
            .get("labels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|l| l.as_str().map(str::to_string))
            .collect(),
        assignees: logins(v, "assignees", "username"),
        author: v
            .get("author")
            .map(|a| s(a, "username"))
            .unwrap_or_default(),
        web_url: s(v, "web_url"),
        created_at: s(v, "created_at"),
        updated_at: s(v, "updated_at"),
        raw: v.clone(),
    }
}

fn position_from(p: &Value) -> Option<DiffPosition> {
    if !p.is_object() {
        return None;
    }
    let path = Some(s(p, "new_path"))
        .filter(|x| !x.is_empty())
        .unwrap_or_else(|| s(p, "old_path"));
    let old_path = opt_s(p, "old_path").filter(|o| *o != path);
    Some(DiffPosition {
        path,
        old_path,
        line: p.get("new_line").and_then(Value::as_u64),
        old_line: p.get("old_line").and_then(Value::as_u64),
        base_sha: opt_s(p, "base_sha"),
        start_sha: opt_s(p, "start_sha"),
        head_sha: opt_s(p, "head_sha"),
    })
}

fn note_from(v: &Value, source: CommentSource) -> Comment {
    Comment {
        id: s(v, "id"),
        author: v
            .get("author")
            .map(|a| s(a, "username"))
            .unwrap_or_default(),
        body: s(v, "body"),
        created_at: s(v, "created_at"),
        source,
        position: v.get("position").and_then(position_from),
        raw: v.clone(),
    }
}

impl IssueTracker for GitLab {
    fn provider(&self) -> Provider {
        Provider::Gitlab
    }
    fn me(&self) -> PResult<String> {
        me_of(self.conn.as_ref())
    }
    fn list_assigned_open(&self, user: &str, cap: usize) -> PResult<Vec<Issue>> {
        let q = urlencode(&[
            ("state", "opened".into()),
            ("assignee_username", user.into()),
        ]);
        let rows = paged(
            self.conn.as_ref(),
            &format!("{}/issues?{q}", self.p()),
            cap,
            None,
        )?;
        Ok(rows.iter().map(issue_from).collect())
    }
    fn issue(&self, n: u64) -> PResult<Issue> {
        Ok(issue_from(&self.get(&format!("{}/issues/{n}", self.p()))?))
    }
    fn comments(&self, n: u64) -> PResult<Vec<Comment>> {
        self.notes("issues", n, CommentSource::Issue)
    }
    fn post_comment(&self, n: u64, body: &str) -> PResult<()> {
        self.send(
            "POST",
            &format!("{}/issues/{n}/notes", self.p()),
            json!({"body": body}),
        )?;
        Ok(())
    }
    fn add_label(&self, n: u64, label: &str) -> PResult<()> {
        self.send(
            "PUT",
            &format!("{}/issues/{n}", self.p()),
            json!({"add_labels": label}),
        )?;
        Ok(())
    }
    fn issue_ref(&self, n: u64) -> String {
        format!("#{n}")
    }
    fn issue_url(&self, n: u64) -> String {
        let path = self.web_path.get_or_init(|| {
            if !self.project.chars().all(|c| c.is_ascii_digit()) {
                return self.project.clone();
            }
            match CodeHost::repo_info(self) {
                Ok(info) if !info.path.is_empty() => info.path,
                _ => self.project.clone(),
            }
        });
        format!("{}/{path}/-/issues/{n}", self.web_base())
    }
    fn closes_keyword(&self, n: u64) -> String {
        format!("Closes #{n}")
    }
}

fn role(filter: CrFilter) -> &'static str {
    match filter {
        CrFilter::Mine => "author_username",
        CrFilter::ReviewRequested => "reviewer_username",
        CrFilter::Assigned => "assignee_username",
    }
}

pub(crate) fn cr_from(v: &Value) -> ChangeRequest {
    let head_sha = v
        .get("diff_refs")
        .and_then(|d| opt_s(d, "head_sha"))
        .unwrap_or_else(|| s(v, "sha"));
    ChangeRequest {
        number: num(v, "iid"),
        repo: repo_of(v),
        title: s(v, "title"),
        description: s(v, "description"),
        source_branch: s(v, "source_branch"),
        target_branch: s(v, "target_branch"),
        head_sha,
        state: match s(v, "state").as_str() {
            "opened" => CrState::Open,
            "merged" => CrState::Merged,
            _ => CrState::Closed,
        },
        draft: v.get("draft").and_then(Value::as_bool).unwrap_or(false)
            || v.get("work_in_progress")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        author: v
            .get("author")
            .map(|a| s(a, "username"))
            .unwrap_or_default(),
        reviewers: logins(v, "reviewers", "username"),
        assignees: logins(v, "assignees", "username"),
        web_url: s(v, "web_url"),
        updated_at: s(v, "updated_at"),
        merged_at: opt_s(v, "merged_at"),
        raw: v.clone(),
    }
}

fn run_from(v: &Value) -> PipelineRun {
    PipelineRun {
        id: s(v, "id"),
        git_ref: s(v, "ref"),
        sha: s(v, "sha"),
        status: s(v, "status"),
        web_url: s(v, "web_url"),
        updated_at: s(v, "updated_at"),
        raw: v.clone(),
    }
}

fn parse_time(t: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(t).ok()
}

impl CodeHost for GitLab {
    fn provider(&self) -> Provider {
        Provider::Gitlab
    }
    fn me(&self) -> PResult<String> {
        me_of(self.conn.as_ref())
    }
    fn repo_info(&self) -> PResult<RepoInfo> {
        let v = self.get(&self.p())?;
        Ok(RepoInfo {
            path: s(&v, "path_with_namespace"),
            default_branch: opt_s(&v, "default_branch").unwrap_or_else(|| "main".into()),
            web_url: s(&v, "web_url"),
        })
    }
    fn change_requests(
        &self,
        filter: CrFilter,
        user: &str,
        cap: usize,
    ) -> PResult<Vec<ChangeRequest>> {
        let q = urlencode(&[("state", "opened".into()), (role(filter), user.into())]);
        let rows = paged(
            self.conn.as_ref(),
            &format!("{}/merge_requests?{q}", self.p()),
            cap,
            None,
        )?;
        Ok(rows.iter().map(cr_from).collect())
    }
    fn change_request(&self, n: u64) -> PResult<ChangeRequest> {
        Ok(cr_from(
            &self.get(&format!("{}/merge_requests/{n}", self.p()))?,
        ))
    }
    fn find_by_source_branch(
        &self,
        branch: &str,
        state: Option<CrState>,
    ) -> PResult<Option<ChangeRequest>> {
        let mut q = vec![("source_branch", branch.to_string())];
        if let Some(st) = state {
            let name = match st {
                CrState::Open => "opened",
                CrState::Merged => "merged",
                CrState::Closed => "closed",
            };
            q.push(("state", name.into()));
        }
        let rows = paged(
            self.conn.as_ref(),
            &format!("{}/merge_requests?{}", self.p(), urlencode(&q)),
            1,
            None,
        )?;
        Ok(rows.first().map(cr_from))
    }
    fn diff(&self, n: u64) -> PResult<Vec<FileDiff>> {
        let rows = paged(
            self.conn.as_ref(),
            &format!("{}/merge_requests/{n}/diffs", self.p()),
            300,
            None,
        )?;
        Ok(rows
            .iter()
            .map(|d| FileDiff {
                old_path: s(d, "old_path"),
                new_path: s(d, "new_path"),
                diff: s(d, "diff"),
                new_file: d.get("new_file").and_then(Value::as_bool).unwrap_or(false),
                deleted_file: d
                    .get("deleted_file")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                renamed_file: d
                    .get("renamed_file")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
            .collect())
    }
    fn cr_comments(&self, n: u64) -> PResult<Vec<Comment>> {
        self.notes("merge_requests", n, CommentSource::ChangeRequest)
    }
    fn draft_review(
        &self,
        cr: &ChangeRequest,
        notes: &[DraftNote],
        signature: &str,
    ) -> PResult<DraftOutcome> {
        let has_refs = cr.raw.get("diff_refs").is_some_and(Value::is_object);
        let fetched;
        let cr = if !has_refs && notes.iter().any(|n| n.position.is_some()) {
            fetched = self.change_request(cr.number)?;
            &fetched
        } else {
            cr
        };
        let refs = cr.raw.get("diff_refs").cloned().unwrap_or(Value::Null);
        let path = format!("{}/merge_requests/{}/draft_notes", self.p(), cr.number);
        if !signature.trim().is_empty() {
            // Every page is listed before deleting, so a deletion never
            // shifts a later page.
            let ours: Vec<String> = paged(self.conn.as_ref(), &path, COMMENT_CAP, None)?
                .iter()
                .filter(|d| signed(&s(d, "note"), signature))
                .map(|d| s(d, "id"))
                .filter(|id| !id.is_empty())
                .collect();
            for id in ours {
                self.conn.api(
                    "DELETE",
                    &format!("{path}/{id}"),
                    &ApiOpts::default().once(),
                )?;
            }
        }
        let mut out = DraftOutcome::default();
        for note in notes {
            let mut body = serde_json::Map::new();
            body.insert("note".into(), json!(note.body));
            if let Some(p) = &note.position {
                let sha =
                    |own: &Option<String>, key: &str| own.clone().unwrap_or_else(|| s(&refs, key));
                let mut pos = serde_json::Map::new();
                pos.insert("position_type".into(), json!("text"));
                pos.insert("base_sha".into(), json!(sha(&p.base_sha, "base_sha")));
                pos.insert("start_sha".into(), json!(sha(&p.start_sha, "start_sha")));
                pos.insert("head_sha".into(), json!(sha(&p.head_sha, "head_sha")));
                pos.insert("new_path".into(), json!(p.path));
                pos.insert(
                    "old_path".into(),
                    json!(p.old_path.clone().unwrap_or_else(|| p.path.clone())),
                );
                if let Some(l) = p.line {
                    pos.insert("new_line".into(), json!(l));
                }
                if let Some(l) = p.old_line {
                    pos.insert("old_line".into(), json!(l));
                }
                body.insert("position".into(), Value::Object(pos));
            }
            match self.send("POST", &path, Value::Object(body)) {
                Ok(_) => {}
                Err(e) if note.position.is_some() && is_rejected(&e) => {
                    let p = note.position.as_ref().expect("checked above");
                    self.send("POST", &path, json!({"note": located(p, &note.body)}))?;
                    out.fallback += 1;
                }
                Err(e) => return Err(e),
            }
            out.posted += 1;
        }
        Ok(out)
    }
    fn open_change_request(
        &self,
        branch: &str,
        target: &str,
        title: &str,
        body: &str,
    ) -> PResult<ChangeRequest> {
        let v = self.send(
            "POST",
            &format!("{}/merge_requests", self.p()),
            json!({"source_branch": branch, "target_branch": target, "title": title,
                   "description": body, "remove_source_branch": true}),
        )?;
        Ok(cr_from(&v))
    }
    fn update_description(&self, n: u64, body: &str) -> PResult<()> {
        self.send(
            "PUT",
            &format!("{}/merge_requests/{n}", self.p()),
            json!({"description": body}),
        )?;
        Ok(())
    }
    fn failed_pipelines(
        &self,
        git_ref: &str,
        since: &str,
        cap: usize,
    ) -> PResult<Vec<PipelineRun>> {
        let q = urlencode(&[
            ("status", "failed".into()),
            ("ref", git_ref.into()),
            ("updated_after", since.into()),
        ]);
        let rows = paged(
            self.conn.as_ref(),
            &format!("{}/pipelines?{q}", self.p()),
            cap,
            None,
        )?;
        Ok(rows.iter().map(run_from).collect())
    }
    fn cr_failed_pipelines(&self, cr: &ChangeRequest) -> PResult<Vec<PipelineRun>> {
        let head = match cr.raw.get("head_pipeline") {
            Some(h) if h.is_object() => h.clone(),
            _ => self
                .change_request(cr.number)?
                .raw
                .get("head_pipeline")
                .cloned()
                .unwrap_or(Value::Null),
        };
        Ok(if head.is_object() && s(&head, "status") == "failed" {
            vec![run_from(&head)]
        } else {
            vec![]
        })
    }
    fn failed_jobs(&self, run: &PipelineRun) -> PResult<Vec<Job>> {
        let rows = paged(
            self.conn.as_ref(),
            &format!("{}/pipelines/{}/jobs?scope%5B%5D=failed", self.p(), run.id),
            DEFAULT_CAP,
            None,
        )?;
        Ok(rows
            .iter()
            .map(|j| Job {
                id: s(j, "id"),
                name: s(j, "name"),
                stage: opt_s(j, "stage"),
                status: s(j, "status"),
                web_url: s(j, "web_url"),
            })
            .collect())
    }
    fn job_log_tail(&self, job: &Job) -> PResult<String> {
        Ok(self
            .conn
            .api_text(&format!("{}/jobs/{}/trace", self.p(), job.id))?)
    }
    fn tags(&self, limit: usize) -> PResult<Vec<Tag>> {
        let v = self.get(&format!(
            "{}/repository/tags?per_page={}",
            self.p(),
            limit.clamp(1, 100)
        ))?;
        Ok(v.as_array()
            .into_iter()
            .flatten()
            .filter(|t| !s(t, "name").is_empty())
            .map(|t| {
                let commit = t.get("commit").cloned().unwrap_or(Value::Null);
                Tag {
                    name: s(t, "name"),
                    sha: s(&commit, "id"),
                    created_at: opt_s(&commit, "committed_date"),
                }
            })
            .collect())
    }
    fn merged_between(
        &self,
        from: Option<&Tag>,
        to: &Tag,
        branch: &str,
        cap: usize,
    ) -> PResult<Vec<ChangeRequest>> {
        let mut q = vec![
            ("state", "merged".to_string()),
            ("target_branch", branch.to_string()),
        ];
        if let Some(since) = from.and_then(|f| f.created_at.clone()) {
            q.push(("updated_after", since));
        }
        let rows = paged(
            self.conn.as_ref(),
            &format!("{}/merge_requests?{}", self.p(), urlencode(&q)),
            cap,
            None,
        )?;
        let upper = to.created_at.as_deref().and_then(parse_time);
        // `updated_after` also returns MRs merged before `from` and touched
        // since; those belong to the previous release.
        let lower = from
            .and_then(|f| f.created_at.as_deref())
            .and_then(parse_time);
        Ok(rows
            .iter()
            .filter(|r| match parse_time(&s(r, "merged_at")) {
                None => false,
                Some(m) => upper.is_none_or(|u| m <= u) && lower.is_none_or(|l| m > l),
            })
            .map(cr_from)
            .collect())
    }
    fn cr_ref(&self, n: u64) -> String {
        format!("!{n}")
    }
    fn cr_noun(&self) -> &'static str {
        "merge request"
    }
}

pub struct GitLabAccount {
    conn: Box<dyn Connector>,
}

impl GitLabAccount {
    pub fn new(conn: Box<dyn Connector>) -> Self {
        Self { conn }
    }
}

fn idle_query(q: &mut Vec<(&'static str, String)>, idle_before: &str) {
    q.push(("updated_before", idle_before.to_string()));
    q.push(("order_by", "updated_at".into()));
    q.push(("sort", "asc".into()));
}

impl AccountHost for GitLabAccount {
    fn provider(&self) -> Provider {
        Provider::Gitlab
    }
    fn me(&self) -> PResult<String> {
        me_of(self.conn.as_ref())
    }
    fn change_requests_anywhere(
        &self,
        filter: CrFilter,
        user: &str,
        idle_before: Option<&str>,
        cap: usize,
    ) -> PResult<Vec<ChangeRequest>> {
        let mut q: Vec<(&'static str, String)> = vec![
            (role(filter), user.into()),
            ("state", "opened".into()),
            ("scope", "all".into()),
            ("non_archived", "true".into()),
        ];
        if let Some(t) = idle_before {
            idle_query(&mut q, t);
        }
        let rows = paged(
            self.conn.as_ref(),
            &format!("/merge_requests?{}", urlencode(&q)),
            cap,
            None,
        )?;
        Ok(rows.iter().map(cr_from).collect())
    }
    fn stale_issues_anywhere(
        &self,
        user: &str,
        idle_before: &str,
        cap: usize,
    ) -> PResult<Vec<Issue>> {
        let mut q: Vec<(&'static str, String)> = vec![
            ("assignee_username", user.into()),
            ("state", "opened".into()),
            ("scope", "all".into()),
        ];
        idle_query(&mut q, idle_before);
        let rows = paged(
            self.conn.as_ref(),
            &format!("/issues?{}", urlencode(&q)),
            cap,
            None,
        )?;
        Ok(rows.iter().map(issue_from).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::gitlab::GitLabConnector;
    use crate::connectors::http::HttpRequest;
    use crate::connectors::testutil::FakeHttp;
    use crate::connectors::Account;
    use serde_json::json;
    use std::sync::Arc;

    pub(crate) fn host(http: Arc<FakeHttp>, project: &str) -> GitLab {
        let acct = Account::new("work", "gitlab").with_setting("url", "https://gl.example.com/");
        GitLab::new(
            Box::new(GitLabConnector::new(acct, Some("tok".into()), http)),
            project,
        )
    }

    pub(crate) fn urls(http: &FakeHttp) -> Vec<String> {
        http.calls
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.url.clone())
            .collect()
    }

    fn issue_json(iid: u64) -> serde_json::Value {
        json!({"iid": iid, "title": "T", "description": null, "state": "opened",
               "labels": ["bug"], "assignees": [{"username": "me"}],
               "author": {"username": "pm"}, "web_url": "https://gl/x/-/issues/1",
               "created_at": "c", "updated_at": "u",
               "references": {"full": "g/sub/p#1"}})
    }

    #[test]
    fn nested_project_paths_are_one_encoded_segment() {
        let http = FakeHttp::new(|_| Ok(json!([])));
        host(http.clone(), "g/sub/p")
            .list_assigned_open("me", 100)
            .unwrap();
        assert_eq!(
            urls(&http)[0],
            "https://gl.example.com/api/v4/projects/g%2Fsub%2Fp/issues?state=opened&assignee_username=me&per_page=100&page=1"
        );
    }

    #[test]
    fn issue_maps_every_field() {
        let http = FakeHttp::ok(issue_json(7));
        let i = host(http, "g/p").issue(7).unwrap();
        assert_eq!(i.number, 7);
        assert_eq!(i.repo, "g/sub/p");
        assert_eq!(i.body, "");
        assert_eq!(i.state, IssueState::Open);
        assert_eq!(i.labels, vec!["bug"]);
        assert_eq!(i.assignees, vec!["me"]);
        assert_eq!(i.author, "pm");
        assert_eq!((i.created_at.as_str(), i.updated_at.as_str()), ("c", "u"));
        let closed = issue_from(&json!({"iid": 1, "state": "closed"}));
        assert_eq!(closed.state, IssueState::Closed);
        assert_eq!(closed.repo, "");
    }

    #[test]
    fn comments_keep_newest_500_humans_oldest_first_with_positions() {
        let http = FakeHttp::new(|r: &HttpRequest| {
            let page: u64 = r.url.split("page=").last().unwrap().parse().unwrap();
            // 6 pages of 100 newest-first notes: ids 600..1
            Ok(json!((0..100).map(|i| {
                let id = 600 - ((page - 1) * 100 + i);
                json!({"id": id, "body": format!("n{id}"), "author": {"username": "a"},
                       "created_at": format!("t{id:04}"), "system": id == 590,
                       "position": if id == 600 {json!({"new_path": "a.rb", "new_line": 3, "old_line": null,
                                     "base_sha": "b", "start_sha": "s", "head_sha": "h"})} else {json!(null)}})
            }).collect::<Vec<_>>()))
        });
        let cs = host(http.clone(), "g/p").comments(9).unwrap();
        assert_eq!(urls(&http).len(), 5);
        assert!(urls(&http)[0]
            .contains("/issues/9/notes?sort=desc&order_by=created_at&per_page=100&page=1"));
        assert_eq!(cs.len(), 499); // 500 fetched, one system note dropped
        assert_eq!(cs.first().unwrap().id, "101");
        assert_eq!(cs.last().unwrap().id, "600");
        assert!(cs.iter().all(|c| c.id != "590"));
        let p = cs.last().unwrap().position.clone().unwrap();
        assert_eq!(
            (p.path.as_str(), p.line, p.old_line),
            ("a.rb", Some(3), None)
        );
        assert_eq!(p.head_sha.as_deref(), Some("h"));
        assert_eq!(cs[0].source, CommentSource::Issue);
    }

    #[test]
    fn writes_use_the_documented_bodies() {
        let http = FakeHttp::ok(json!({}));
        let h = host(http.clone(), "g/p");
        h.post_comment(4, "hello").unwrap();
        h.add_label(4, "loop:needs-human").unwrap();
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls[0].method, "POST");
        assert!(calls[0].url.ends_with("/projects/g%2Fp/issues/4/notes"));
        assert_eq!(calls[0].json_body, Some(json!({"body": "hello"})));
        assert_eq!(calls[1].method, "PUT");
        assert!(calls[1].url.ends_with("/projects/g%2Fp/issues/4"));
        assert_eq!(
            calls[1].json_body,
            Some(json!({"add_labels": "loop:needs-human"}))
        );
    }

    #[test]
    fn refs_and_urls() {
        let h = host(FakeHttp::ok(json!({"username": "me"})), "g/p");
        assert_eq!(IssueTracker::me(&h).unwrap(), "me");
        assert_eq!(h.issue_ref(3), "#3");
        assert_eq!(h.closes_keyword(3), "Closes #3");
        assert_eq!(h.issue_url(3), "https://gl.example.com/g/p/-/issues/3");
        assert_eq!(IssueTracker::provider(&h), Provider::Gitlab);
    }
    fn mr_json(iid: u64) -> serde_json::Value {
        json!({"iid": iid, "title": "M", "description": "d", "source_branch": "feat/issue-1",
               "target_branch": "main", "state": "opened", "work_in_progress": true,
               "author": {"username": "a"}, "reviewers": [{"username": "r"}],
               "assignees": [], "web_url": "w", "updated_at": "u", "merged_at": null,
               "sha": "s0", "diff_refs": {"base_sha": "b", "start_sha": "st", "head_sha": "h"},
               "references": {"full": "g/p!3"},
               "head_pipeline": {"id": 9, "status": "failed", "ref": "feat/issue-1", "sha": "h",
                                 "web_url": "pw", "updated_at": "pu"}})
    }

    #[test]
    fn cr_maps_and_roles_pick_the_filter() {
        let http = FakeHttp::ok(json!([mr_json(3)]));
        let h = host(http.clone(), "g/p");
        let crs = h
            .change_requests(CrFilter::ReviewRequested, "me", 100)
            .unwrap();
        assert!(urls(&http)[0].contains(
            "/projects/g%2Fp/merge_requests?state=opened&reviewer_username=me&per_page=100&page=1"
        ));
        let c = &crs[0];
        assert_eq!(
            (c.number, c.repo.as_str(), c.head_sha.as_str()),
            (3, "g/p", "h")
        );
        assert!(c.draft);
        assert_eq!(c.state, CrState::Open);
        assert_eq!(c.reviewers, vec!["r"]);
        assert_eq!(c.merged_at, None);
        h.change_requests(CrFilter::Mine, "me", 100).unwrap();
        h.change_requests(CrFilter::Assigned, "me", 100).unwrap();
        assert!(urls(&http)[1].contains("author_username=me"));
        assert!(urls(&http)[2].contains("assignee_username=me"));
    }

    fn note_at(path: &str, line: u64, body: &str) -> DraftNote {
        DraftNote {
            position: Some(DiffPosition {
                path: path.into(),
                line: Some(line),
                ..Default::default()
            }),
            body: body.into(),
        }
    }

    const SIG: &str = "\n\n— LoopX review";

    #[test]
    fn draft_review_posts_one_draft_note_per_note_with_diff_refs() {
        let http = FakeHttp::new(|r: &HttpRequest| {
            Ok(if r.method == "GET" {
                json!([])
            } else {
                json!({})
            })
        });
        let h = host(http.clone(), "g/p");
        let cr = cr_from(&mr_json(3));
        let renamed = DraftNote {
            position: Some(DiffPosition {
                path: "new.rb".into(),
                old_path: Some("old.rb".into()),
                line: Some(2),
                ..Default::default()
            }),
            body: "moved".into(),
        };
        let out = h
            .draft_review(
                &cr,
                &[
                    DraftNote {
                        position: None,
                        body: "overall".into(),
                    },
                    note_at("a.rb", 4, "here"),
                    renamed,
                ],
                "",
            )
            .unwrap();
        assert_eq!(
            out,
            DraftOutcome {
                posted: 3,
                fallback: 0
            }
        );
        let calls = http.calls.lock().unwrap();
        // an empty signature never lists or deletes anything
        assert!(calls.iter().all(|c| c.method == "POST"));
        assert!(calls[0]
            .url
            .ends_with("/projects/g%2Fp/merge_requests/3/draft_notes"));
        assert_eq!(calls[0].json_body, Some(json!({"note": "overall"})));
        assert_eq!(
            calls[1].json_body,
            Some(json!({"note": "here", "position": {
            "position_type": "text", "base_sha": "b", "start_sha": "st", "head_sha": "h",
            "new_path": "a.rb", "old_path": "a.rb", "new_line": 4}}))
        );
        assert_eq!(
            calls[2].json_body.as_ref().unwrap()["position"]["old_path"],
            "old.rb"
        );
    }

    #[test]
    fn draft_review_replaces_only_its_own_signed_drafts() {
        let http = FakeHttp::new(|r: &HttpRequest| {
            Ok(if r.method == "GET" {
                json!([{"id": 11, "note": "old finding\n\n— LoopX review  "},
                       {"id": 12, "note": "my own human draft"}])
            } else {
                json!({})
            })
        });
        let h = host(http.clone(), "g/p");
        h.draft_review(
            &cr_from(&mr_json(3)),
            &[DraftNote {
                position: None,
                body: format!("new{SIG}"),
            }],
            SIG,
        )
        .unwrap();
        let calls = http.calls.lock().unwrap();
        let deletes: Vec<_> = calls
            .iter()
            .filter(|c| c.method == "DELETE")
            .map(|c| c.url.clone())
            .collect();
        assert_eq!(deletes.len(), 1);
        assert!(deletes[0].ends_with("/merge_requests/3/draft_notes/11"));
        // listed before deleting, deleted before posting
        let order: Vec<_> = calls.iter().map(|c| c.method.as_str()).collect();
        assert_eq!(order, vec!["GET", "DELETE", "POST"]);
    }

    #[test]
    fn a_rejected_line_note_falls_back_to_a_general_note() {
        let http = FakeHttp::new(|r: &HttpRequest| {
            let positioned = r
                .json_body
                .as_ref()
                .is_some_and(|b| b.get("position").is_some());
            if positioned {
                Err(crate::connectors::http::HttpError::new(
                    Some(422),
                    "line not in diff",
                    &r.url,
                ))
            } else {
                Ok(json!({}))
            }
        });
        let h = host(http.clone(), "g/p");
        let out = h
            .draft_review(&cr_from(&mr_json(3)), &[note_at("a.rb", 4, "here")], "")
            .unwrap();
        assert_eq!(
            out,
            DraftOutcome {
                posted: 1,
                fallback: 1
            }
        );
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls[1].json_body, Some(json!({"note": "`a.rb:4` here"})));
    }

    #[test]
    fn other_errors_stop_the_review() {
        let http = FakeHttp::new(|r: &HttpRequest| {
            Err(crate::connectors::http::HttpError::new(
                Some(500),
                "boom",
                &r.url,
            ))
        });
        let h = host(http, "g/p");
        assert!(h
            .draft_review(&cr_from(&mr_json(3)), &[note_at("a.rb", 4, "here")], "")
            .is_err());
    }

    #[test]
    fn a_listed_mr_without_diff_refs_is_fetched_before_positioned_notes() {
        let http = FakeHttp::new(|r: &HttpRequest| {
            Ok(if r.method == "GET" {
                mr_json(3)
            } else {
                json!({})
            })
        });
        let h = host(http.clone(), "g/p");
        let mut listed = mr_json(3);
        listed.as_object_mut().unwrap().remove("diff_refs");
        h.draft_review(&cr_from(&listed), &[note_at("a.rb", 4, "here")], "")
            .unwrap();
        let calls = http.calls.lock().unwrap();
        assert!(calls[0].url.ends_with("/projects/g%2Fp/merge_requests/3"));
        assert_eq!(
            calls[1].json_body.as_ref().unwrap()["position"]["base_sha"],
            "b"
        );
    }

    #[test]
    fn open_and_update_change_request() {
        let http = FakeHttp::ok(mr_json(8));
        let h = host(http.clone(), "g/p");
        let cr = h
            .open_change_request("feat/issue-1-x", "main", "T", "Closes #1")
            .unwrap();
        assert_eq!(cr.number, 8);
        h.update_description(8, "new").unwrap();
        let calls = http.calls.lock().unwrap();
        assert_eq!(
            calls[0].json_body,
            Some(json!({"source_branch": "feat/issue-1-x",
            "target_branch": "main", "title": "T", "description": "Closes #1",
            "remove_source_branch": true}))
        );
        assert_eq!(calls[1].method, "PUT");
        assert_eq!(calls[1].json_body, Some(json!({"description": "new"})));
    }

    #[test]
    fn pipelines_jobs_and_logs() {
        let http = FakeHttp::new(|r: &HttpRequest| {
            Ok(if r.url.contains("/jobs?") {
                json!([{"id": 5, "name": "rspec", "stage": "test", "status": "failed", "web_url": "jw"}])
            } else {
                json!([{"id": 9, "ref": "main", "sha": "s", "status": "failed", "web_url": "pw", "updated_at": "u"}])
            })
        });
        *http.text.lock().unwrap() = Ok("...boom".into());
        let h = host(http.clone(), "g/p");
        let runs = h
            .failed_pipelines("main", "2026-11-01T00:00:00Z", 20)
            .unwrap();
        assert!(urls(&http)[0]
            .contains("/pipelines?status=failed&ref=main&updated_after=2026-11-01T00%3A00%3A00Z"));
        assert_eq!(
            (runs[0].id.as_str(), runs[0].git_ref.as_str()),
            ("9", "main")
        );
        let jobs = h.failed_jobs(&runs[0]).unwrap();
        assert!(urls(&http)[1].contains("/pipelines/9/jobs?scope%5B%5D=failed"));
        assert_eq!(
            (jobs[0].name.as_str(), jobs[0].stage.as_deref()),
            ("rspec", Some("test"))
        );
        assert_eq!(h.job_log_tail(&jobs[0]).unwrap(), "...boom");
        let cr = cr_from(&mr_json(3));
        let mine = h.cr_failed_pipelines(&cr).unwrap();
        assert_eq!(mine[0].id, "9");
        assert_eq!(urls(&http).len(), 2); // head_pipeline came from cr.raw, no call
    }

    #[test]
    fn tags_and_merged_between_keep_only_mrs_up_to_the_new_tag() {
        let http = FakeHttp::new(|r: &HttpRequest| {
            Ok(if r.url.contains("/repository/tags") {
                json!([{"name": "v2", "commit": {"id": "c2", "committed_date": "2026-10-02T10:00:00+02:00"}},
                       {"name": "v1", "commit": {"id": "c1", "committed_date": "2026-10-01T00:00:00Z"}}])
            } else {
                json!([
                    {"iid": 1, "state": "merged", "merged_at": "2026-10-02T07:59:00Z"},
                    {"iid": 2, "state": "merged", "merged_at": "2026-10-02T08:01:00Z"},
                    {"iid": 3, "state": "merged", "merged_at": null},
                    // merged before v1, only commented on afterwards
                    {"iid": 4, "state": "merged", "merged_at": "2026-09-30T12:00:00Z"},
                    {"iid": 5, "state": "merged", "merged_at": "2026-10-01T00:00:00Z"}
                ])
            })
        });
        let h = host(http.clone(), "g/p");
        let tags = h.tags(2).unwrap();
        assert_eq!(
            tags[0],
            Tag {
                name: "v2".into(),
                sha: "c2".into(),
                created_at: Some("2026-10-02T10:00:00+02:00".into())
            }
        );
        let mrs = h
            .merged_between(Some(&tags[1]), &tags[0], "main", 50)
            .unwrap();
        assert!(urls(&http)[1]
            .contains("state=merged&target_branch=main&updated_after=2026-10-01T00%3A00%3A00Z"));
        // v2 is 08:00Z: iid 1 kept, iid 2 after it, iid 3 never merged,
        // iid 4 and 5 merged at or before v1 (already in its notes).
        assert_eq!(mrs.iter().map(|m| m.number).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn account_host_queries_across_projects() {
        let http = FakeHttp::ok(json!([]));
        let acct = Account::new("work", "gitlab").with_setting("url", "https://gl.example.com");
        let a = GitLabAccount::new(Box::new(GitLabConnector::new(
            acct,
            Some("t".into()),
            http.clone(),
        )));
        a.change_requests_anywhere(CrFilter::ReviewRequested, "me", None, 50)
            .unwrap();
        a.change_requests_anywhere(CrFilter::Mine, "me", Some("2026-10-01T00:00:00Z"), 50)
            .unwrap();
        a.stale_issues_anywhere("me", "2026-10-01T00:00:00Z", 50)
            .unwrap();
        let u = urls(&http);
        assert!(u[0].ends_with("/api/v4/merge_requests?reviewer_username=me&state=opened&scope=all&non_archived=true&per_page=50&page=1"));
        assert!(u[1].contains("author_username=me&state=opened&scope=all&non_archived=true&updated_before=2026-10-01T00%3A00%3A00Z&order_by=updated_at&sort=asc"));
        assert!(u[2].contains(
            "/api/v4/issues?assignee_username=me&state=opened&scope=all&updated_before="
        ));
    }

    #[test]
    fn tags_per_page_is_capped_at_100() {
        let http = FakeHttp::ok(json!([]));
        host(http.clone(), "g/p").tags(500).unwrap();
        assert!(urls(&http)[0].ends_with("/repository/tags?per_page=100"));
    }

    #[test]
    fn issue_url_resolves_a_numeric_project_id_to_its_path_once() {
        let http =
            FakeHttp::ok(json!({"path_with_namespace": "g/sub/p", "default_branch": "main"}));
        let h = host(http.clone(), "123");
        assert_eq!(h.issue_url(3), "https://gl.example.com/g/sub/p/-/issues/3");
        assert_eq!(h.issue_url(4), "https://gl.example.com/g/sub/p/-/issues/4");
        assert_eq!(
            urls(&http),
            vec!["https://gl.example.com/api/v4/projects/123"]
        );
        // a path needs no lookup; a failed lookup keeps the old form
        let http = FakeHttp::new(|r| {
            Err(crate::connectors::http::HttpError::new(
                Some(500),
                "x",
                &r.url,
            ))
        });
        let h = host(http.clone(), "77");
        assert_eq!(h.issue_url(1), "https://gl.example.com/77/-/issues/1");
        let http = FakeHttp::ok(json!({}));
        assert_eq!(
            host(http.clone(), "g/p").issue_url(1),
            "https://gl.example.com/g/p/-/issues/1"
        );
        assert!(urls(&http).is_empty());
    }
}
