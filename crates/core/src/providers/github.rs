//! GitHub (github.com or Enterprise) as an issue tracker, code host and
//! account host, over the GitHub connector.

use super::*;
use crate::connectors::http::{quote_plus, urlencode};
use crate::connectors::{ApiOpts, Connector};
use serde_json::{json, Value};

/// How many tags `tags()` dates at least, to find the newest ones.
const TAG_SCAN: usize = 20;

pub struct GitHub {
    conn: Box<dyn Connector>,
    repo: String,
}

fn repo_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

fn valid_repo(repo: &str) -> bool {
    match repo.split_once('/') {
        Some((o, n)) => {
            !o.is_empty()
                && !n.is_empty()
                && o.chars().all(repo_char)
                && n.chars().all(repo_char)
                && o != ".."
                && n != ".."
        }
        None => false,
    }
}

/// `https://api.github.com` → `https://github.com`; GHE `…/api/v3` → host.
pub(crate) fn web_base(api_url: &str) -> String {
    let base = api_url.trim_end_matches('/');
    if base == "https://api.github.com" {
        return "https://github.com".into();
    }
    base.strip_suffix("/api/v3").unwrap_or(base).to_string()
}

impl GitHub {
    pub fn new(conn: Box<dyn Connector>, repo: &str) -> PResult<Self> {
        if !valid_repo(repo) {
            return Err(ProviderError::Malformed(format!(
                "{repo} is not owner/name"
            )));
        }
        Ok(Self {
            conn,
            repo: repo.to_string(),
        })
    }

    fn r(&self) -> String {
        format!("/repos/{}", self.repo)
    }

    fn get(&self, path: &str) -> PResult<Value> {
        Ok(self.conn.api("GET", path, &ApiOpts::default())?)
    }

    fn send(&self, method: &str, path: &str, body: Value) -> PResult<Value> {
        Ok(self.conn.api(method, path, &ApiOpts::json(body))?)
    }

    fn web(&self) -> String {
        web_base(self.conn.account().setting("api_url"))
    }

    /// Newest `COMMENT_CAP` entries of an ascending-only list of `count`
    /// items at `path` (no query string), oldest first.
    fn newest(&self, path: &str, count: u64) -> PResult<Vec<Value>> {
        let last = count.div_ceil(100).max(1);
        // Six pages: with a partial last page, five would hold fewer than 500.
        let first = last.saturating_sub(5).max(1);
        let mut out = Vec::new();
        for page in first..=last {
            let v = self.get(&format!("{path}?per_page=100&page={page}"))?;
            out.extend(v.as_array().cloned().unwrap_or_default());
        }
        let skip = out.len().saturating_sub(COMMENT_CAP);
        Ok(out.split_off(skip))
    }
}

impl GitHub {
    /// Submitted reviews with a summary ("Request changes: ..."), id `v<id>`.
    /// Pending reviews are drafts nobody else can see, so they are skipped.
    fn review_bodies(&self, n: u64) -> PResult<Vec<Comment>> {
        let rows = paged(
            self.conn.as_ref(),
            &format!("{}/pulls/{n}/reviews", self.r()),
            COMMENT_CAP,
            None,
        )?;
        Ok(rows
            .iter()
            .filter(|r| s(r, "state") != "PENDING" && !s(r, "body").trim().is_empty())
            .map(|r| Comment {
                id: format!("v{}", s(r, "id")),
                author: r.get("user").map(|u| s(u, "login")).unwrap_or_default(),
                body: s(r, "body"),
                created_at: s(r, "submitted_at"),
                source: CommentSource::ChangeRequest,
                position: None,
                raw: r.clone(),
            })
            .collect())
    }
}

pub(crate) fn me_of(conn: &dyn Connector) -> PResult<String> {
    let v = conn.api("GET", "/user", &ApiOpts::default())?;
    Ok(s(&v, "login"))
}

fn repo_from_url(v: &Value, key: &str) -> String {
    s(v, key)
        .split_once("/repos/")
        .map(|(_, r)| r.to_string())
        .unwrap_or_default()
}

pub(crate) fn issue_from(v: &Value) -> Issue {
    Issue {
        number: num(v, "number"),
        repo: repo_from_url(v, "repository_url"),
        title: s(v, "title"),
        body: s(v, "body"),
        state: if s(v, "state") == "open" {
            IssueState::Open
        } else {
            IssueState::Closed
        },
        labels: logins(v, "labels", "name"),
        assignees: logins(v, "assignees", "login"),
        author: v.get("user").map(|u| s(u, "login")).unwrap_or_default(),
        web_url: s(v, "html_url"),
        created_at: s(v, "created_at"),
        updated_at: s(v, "updated_at"),
        raw: v.clone(),
    }
}

fn comment_from(v: &Value, source: CommentSource, review: bool) -> Comment {
    let position = if review {
        Some(DiffPosition {
            path: s(v, "path"),
            old_path: None,
            line: v
                .get("line")
                .and_then(Value::as_u64)
                .or_else(|| v.get("original_line").and_then(Value::as_u64)),
            old_line: None,
            base_sha: None,
            start_sha: None,
            head_sha: opt_s(v, "commit_id"),
        })
    } else {
        None
    };
    Comment {
        id: if review {
            format!("r{}", s(v, "id"))
        } else {
            s(v, "id")
        },
        author: v.get("user").map(|u| s(u, "login")).unwrap_or_default(),
        body: s(v, "body"),
        created_at: s(v, "created_at"),
        source,
        position,
        raw: v.clone(),
    }
}

impl IssueTracker for GitHub {
    fn provider(&self) -> Provider {
        Provider::Github
    }
    fn me(&self) -> PResult<String> {
        me_of(self.conn.as_ref())
    }
    fn list_assigned_open(&self, user: &str, cap: usize) -> PResult<Vec<Issue>> {
        let q = urlencode(&[("state", "open".into()), ("assignee", user.into())]);
        let rows = paged(
            self.conn.as_ref(),
            &format!("{}/issues?{q}", self.r()),
            cap,
            None,
        )?;
        Ok(rows
            .iter()
            .filter(|r| r.get("pull_request").is_none())
            .map(issue_from)
            .collect())
    }
    fn issue(&self, n: u64) -> PResult<Issue> {
        let v = self.get(&format!("{}/issues/{n}", self.r()))?;
        if v.get("pull_request").is_some() {
            return Err(ProviderError::Malformed(format!("#{n} is a pull request")));
        }
        Ok(issue_from(&v))
    }
    fn comments(&self, n: u64) -> PResult<Vec<Comment>> {
        let count = num(&self.get(&format!("{}/issues/{n}", self.r()))?, "comments");
        let rows = self.newest(&format!("{}/issues/{n}/comments", self.r()), count)?;
        Ok(rows
            .iter()
            .map(|c| comment_from(c, CommentSource::Issue, false))
            .collect())
    }
    fn post_comment(&self, n: u64, body: &str) -> PResult<()> {
        self.send(
            "POST",
            &format!("{}/issues/{n}/comments", self.r()),
            json!({"body": body}),
        )?;
        Ok(())
    }
    fn add_label(&self, n: u64, label: &str) -> PResult<()> {
        self.send(
            "POST",
            &format!("{}/issues/{n}/labels", self.r()),
            json!({"labels": [label]}),
        )?;
        Ok(())
    }
    fn issue_ref(&self, n: u64) -> String {
        format!("#{n}")
    }
    fn issue_url(&self, n: u64) -> String {
        format!("{}/{}/issues/{n}", self.web(), self.repo)
    }
    fn closes_keyword(&self, n: u64) -> String {
        format!("Closes #{n}")
    }
}

pub(crate) fn pr_from(v: &Value) -> ChangeRequest {
    let head = v.get("head").cloned().unwrap_or(Value::Null);
    let base = v.get("base").cloned().unwrap_or(Value::Null);
    let merged_at = opt_s(v, "merged_at");
    let repo = base
        .get("repo")
        .map(|r| s(r, "full_name"))
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| repo_from_url(v, "repository_url"));
    ChangeRequest {
        number: num(v, "number"),
        repo,
        title: s(v, "title"),
        description: s(v, "body"),
        source_branch: s(&head, "ref"),
        target_branch: s(&base, "ref"),
        head_sha: s(&head, "sha"),
        state: if merged_at.is_some() {
            CrState::Merged
        } else if s(v, "state") == "open" {
            CrState::Open
        } else {
            CrState::Closed
        },
        draft: v.get("draft").and_then(Value::as_bool).unwrap_or(false),
        author: v.get("user").map(|u| s(u, "login")).unwrap_or_default(),
        reviewers: logins(v, "requested_reviewers", "login"),
        assignees: logins(v, "assignees", "login"),
        web_url: s(v, "html_url"),
        updated_at: s(v, "updated_at"),
        merged_at,
        raw: v.clone(),
    }
}

fn run_from(v: &Value) -> PipelineRun {
    PipelineRun {
        id: s(v, "id"),
        git_ref: s(v, "head_branch"),
        sha: s(v, "head_sha"),
        status: opt_s(v, "conclusion").unwrap_or_else(|| s(v, "status")),
        web_url: s(v, "html_url"),
        updated_at: s(v, "updated_at"),
        raw: v.clone(),
    }
}

impl GitHub {
    fn owner(&self) -> &str {
        self.repo.split('/').next().unwrap_or("")
    }
}

impl CodeHost for GitHub {
    fn provider(&self) -> Provider {
        Provider::Github
    }
    fn me(&self) -> PResult<String> {
        me_of(self.conn.as_ref())
    }
    fn repo_info(&self) -> PResult<RepoInfo> {
        let v = self.get(&self.r())?;
        Ok(RepoInfo {
            path: s(&v, "full_name"),
            default_branch: opt_s(&v, "default_branch").unwrap_or_else(|| "main".into()),
            web_url: s(&v, "html_url"),
        })
    }
    fn change_requests(
        &self,
        filter: CrFilter,
        user: &str,
        cap: usize,
    ) -> PResult<Vec<ChangeRequest>> {
        let rows = paged(
            self.conn.as_ref(),
            &format!("{}/pulls?state=open", self.r()),
            cap,
            None,
        )?;
        Ok(rows
            .iter()
            .map(pr_from)
            .filter(|c| match filter {
                CrFilter::Mine => c.author == user,
                CrFilter::ReviewRequested => c.reviewers.iter().any(|r| r == user),
                CrFilter::Assigned => c.assignees.iter().any(|a| a == user),
            })
            .collect())
    }
    fn change_request(&self, n: u64) -> PResult<ChangeRequest> {
        Ok(pr_from(&self.get(&format!("{}/pulls/{n}", self.r()))?))
    }
    fn find_by_source_branch(
        &self,
        branch: &str,
        state: Option<CrState>,
    ) -> PResult<Option<ChangeRequest>> {
        let api_state = match state {
            Some(CrState::Open) => "open",
            Some(_) => "closed",
            None => "all",
        };
        let q = urlencode(&[
            ("head", format!("{}:{branch}", self.owner())),
            ("state", api_state.into()),
        ]);
        let rows = paged(
            self.conn.as_ref(),
            &format!("{}/pulls?{q}", self.r()),
            DEFAULT_CAP,
            None,
        )?;
        Ok(rows
            .iter()
            .map(pr_from)
            .find(|c| c.source_branch == branch && state.is_none_or(|st| c.state == st)))
    }
    fn diff(&self, n: u64) -> PResult<Vec<FileDiff>> {
        let rows = paged(
            self.conn.as_ref(),
            &format!("{}/pulls/{n}/files", self.r()),
            300,
            None,
        )?;
        Ok(rows
            .iter()
            .map(|f| {
                let status = s(f, "status");
                let name = s(f, "filename");
                FileDiff {
                    old_path: opt_s(f, "previous_filename").unwrap_or_else(|| name.clone()),
                    new_path: name,
                    diff: s(f, "patch"),
                    new_file: status == "added",
                    deleted_file: status == "removed",
                    renamed_file: status == "renamed",
                }
            })
            .collect())
    }
    fn cr_comments(&self, n: u64) -> PResult<Vec<Comment>> {
        let pr = self.change_request(n)?;
        let talk = self.newest(
            &format!("{}/issues/{n}/comments", self.r()),
            num(&pr.raw, "comments"),
        )?;
        let review = self.newest(
            &format!("{}/pulls/{n}/comments", self.r()),
            num(&pr.raw, "review_comments"),
        )?;
        let mut out: Vec<Comment> = talk
            .iter()
            .map(|c| comment_from(c, CommentSource::ChangeRequest, false))
            .chain(
                review
                    .iter()
                    .map(|c| comment_from(c, CommentSource::ChangeRequest, true)),
            )
            .chain(self.review_bodies(n)?)
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        let skip = out.len().saturating_sub(COMMENT_CAP);
        Ok(out.split_off(skip))
    }
    fn draft_review(
        &self,
        cr: &ChangeRequest,
        notes: &[DraftNote],
        signature: &str,
    ) -> PResult<DraftOutcome> {
        let head_sha = if cr.head_sha.is_empty() {
            self.change_request(cr.number)?.head_sha
        } else {
            cr.head_sha.clone()
        };
        let reviews_path = format!("{}/pulls/{}/reviews", self.r(), cr.number);
        let me = me_of(self.conn.as_ref())?;
        let mine: Vec<Value> = paged(self.conn.as_ref(), &reviews_path, DEFAULT_CAP, None)?
            .into_iter()
            .filter(|r| {
                s(r, "state") == "PENDING"
                    && r.get("user").map(|u| s(u, "login")) == Some(me.clone())
            })
            .collect();
        // GitHub allows one pending review per user: one we did not write is
        // the user's own draft, never touched.
        if mine.iter().any(|r| !signed(&s(r, "body"), signature)) {
            return Err(ProviderError::PendingReviewExists);
        }
        for r in &mine {
            self.conn.api(
                "DELETE",
                &format!("{reviews_path}/{}", s(r, "id")),
                &ApiOpts::default().once(),
            )?;
        }
        let mut out = DraftOutcome::default();
        if notes.is_empty() {
            return Ok(out);
        }
        let mut body = Vec::new();
        let mut lines: Vec<(&DiffPosition, &str)> = Vec::new();
        for note in notes {
            match &note.position {
                Some(p) if p.line.is_some() => lines.push((p, &note.body)),
                Some(p) => body.push(located(p, &note.body)),
                None => body.push(note.body.clone()),
            }
        }
        let sign = |parts: &[String]| {
            let text = parts.join("\n\n");
            if signature.trim().is_empty() || signed(&text, signature) {
                text
            } else {
                format!("{text}{signature}")
            }
        };
        let comments: Vec<Value> = lines
            .iter()
            .map(|(p, b)| json!({"path": p.path, "line": p.line, "side": "RIGHT", "body": b}))
            .collect();
        let first = self.send(
            "POST",
            &reviews_path,
            json!({"commit_id": head_sha, "body": sign(&body), "comments": comments}),
        );
        match first {
            Ok(_) => {}
            // One comment GitHub cannot place fails the whole review: retry
            // once with every line comment moved into the body.
            Err(e) if !lines.is_empty() && is_rejected(&e) => {
                body.extend(lines.iter().map(|(p, b)| located(p, b)));
                self.send(
                    "POST",
                    &reviews_path,
                    json!({"commit_id": head_sha, "body": sign(&body), "comments": []}),
                )?;
                out.fallback = lines.len();
            }
            Err(e) => return Err(e),
        }
        out.posted = notes.len();
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
            &format!("{}/pulls", self.r()),
            json!({"title": title, "head": branch, "base": target, "body": body}),
        )?;
        Ok(pr_from(&v))
    }
    fn update_description(&self, n: u64, body: &str) -> PResult<()> {
        self.send(
            "PATCH",
            &format!("{}/pulls/{n}", self.r()),
            json!({"body": body}),
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
            ("branch", git_ref.into()),
            ("status", "failure".into()),
            ("created", format!(">={since}")),
        ]);
        let rows = paged(
            self.conn.as_ref(),
            &format!("{}/actions/runs?{q}", self.r()),
            cap,
            Some("workflow_runs"),
        )?;
        Ok(rows.iter().map(run_from).collect())
    }
    fn cr_failed_pipelines(&self, cr: &ChangeRequest) -> PResult<Vec<PipelineRun>> {
        let sha = if cr.head_sha.is_empty() {
            self.change_request(cr.number)?.head_sha
        } else {
            cr.head_sha.clone()
        };
        let q = urlencode(&[("head_sha", sha), ("status", "failure".into())]);
        let rows = paged(
            self.conn.as_ref(),
            &format!("{}/actions/runs?{q}", self.r()),
            20,
            Some("workflow_runs"),
        )?;
        Ok(rows.iter().map(run_from).collect())
    }
    fn failed_jobs(&self, run: &PipelineRun) -> PResult<Vec<Job>> {
        let rows = paged(
            self.conn.as_ref(),
            &format!("{}/actions/runs/{}/jobs?filter=latest", self.r(), run.id),
            DEFAULT_CAP,
            Some("jobs"),
        )?;
        Ok(rows
            .iter()
            .filter(|j| s(j, "conclusion") == "failure")
            .map(|j| Job {
                id: s(j, "id"),
                name: s(j, "name"),
                stage: None,
                status: s(j, "conclusion"),
                web_url: s(j, "html_url"),
            })
            .collect())
    }
    fn job_log_tail(&self, job: &Job) -> PResult<String> {
        Ok(self
            .conn
            .api_text(&format!("{}/actions/jobs/{}/logs", self.r(), job.id))?)
    }
    fn tags(&self, limit: usize) -> PResult<Vec<Tag>> {
        // `/tags` is not ordered by date, so scan a few more than asked for
        // and order them by their commit date.
        let scan = limit.clamp(TAG_SCAN, 100);
        let v = self.get(&format!("{}/tags?per_page={scan}", self.r()))?;
        let mut out = Vec::new();
        for t in v
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| !s(t, "name").is_empty())
        {
            let sha = t.get("commit").map(|c| s(c, "sha")).unwrap_or_default();
            let commit = self.get(&format!("{}/commits/{sha}", self.r()))?;
            let date = commit
                .get("commit")
                .and_then(|c| c.get("committer"))
                .and_then(|c| opt_s(c, "date"));
            out.push(Tag {
                name: s(t, "name"),
                sha,
                created_at: date,
            });
        }
        // RFC 3339 UTC (`Z`) strings from GitHub sort by time; undated last.
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        out.truncate(limit);
        Ok(out)
    }
    fn merged_between(
        &self,
        from: Option<&Tag>,
        to: &Tag,
        branch: &str,
        cap: usize,
    ) -> PResult<Vec<ChangeRequest>> {
        let lo = from.and_then(|f| f.created_at.clone());
        let hi = to.created_at.clone();
        let window = match (&lo, &hi) {
            (Some(l), Some(h)) => format!(" merged:{l}..{h}"),
            (None, Some(h)) => format!(" merged:<={h}"),
            (Some(l), None) => format!(" merged:>{l}"),
            (None, None) => String::new(),
        };
        let q = format!("repo:{} is:pr is:merged base:{branch}{window}", self.repo);
        // The search range is inclusive; a PR merged exactly at the previous
        // tag belongs to that release, so bounds are applied again here.
        let lower = lo.as_deref().and_then(parse_time);
        let upper = hi.as_deref().and_then(parse_time);
        Ok(search_issues(self.conn.as_ref(), &q, false, cap)?
            .iter()
            .map(pr_from_search)
            .filter(|c| match c.merged_at.as_deref().and_then(parse_time) {
                None => false,
                Some(m) => upper.is_none_or(|u| m <= u) && lower.is_none_or(|l| m > l),
            })
            .map(|mut c| {
                c.target_branch = branch.to_string();
                c
            })
            .collect())
    }
    fn cr_ref(&self, n: u64) -> String {
        format!("#{n}")
    }
    fn cr_noun(&self) -> &'static str {
        "pull request"
    }
}

pub struct GitHubAccount {
    conn: Box<dyn Connector>,
}

impl GitHubAccount {
    pub fn new(conn: Box<dyn Connector>) -> Self {
        Self { conn }
    }

    fn search(&self, query: &str, idle: bool, cap: usize) -> PResult<Vec<Value>> {
        search_issues(self.conn.as_ref(), query, idle, cap)
    }
}

/// `/search/issues` (issues and pull requests); `idle` sorts least
/// recently updated first.
fn search_issues(conn: &dyn Connector, query: &str, idle: bool, cap: usize) -> PResult<Vec<Value>> {
    let mut path = format!("/search/issues?q={}", quote_plus(query));
    if idle {
        path.push_str("&sort=updated&order=asc");
    }
    paged(conn, &path, cap, Some("items"))
}

/// `YYYY-MM-DD` of an RFC 3339 time for search qualifiers; never panics on
/// odd input (a non-ASCII byte at the cut keeps the whole string).
fn day(t: &str) -> &str {
    t.get(..10).unwrap_or(t)
}

fn parse_time(t: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(t).ok()
}

/// An issue-shaped search hit: no branches or head SHA; a pull request's
/// merge time sits under `pull_request.merged_at`.
fn pr_from_search(v: &Value) -> ChangeRequest {
    let mut cr = pr_from(v);
    cr.repo = repo_from_url(v, "repository_url");
    if let Some(m) = v.get("pull_request").and_then(|p| opt_s(p, "merged_at")) {
        cr.merged_at = Some(m);
        cr.state = CrState::Merged;
    }
    cr
}

impl AccountHost for GitHubAccount {
    fn provider(&self) -> Provider {
        Provider::Github
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
        let role = match filter {
            CrFilter::Mine => "author",
            CrFilter::ReviewRequested => "review-requested",
            CrFilter::Assigned => "assignee",
        };
        let mut q = format!("is:pr is:open archived:false {role}:{user}");
        if let Some(t) = idle_before {
            q.push_str(&format!(" updated:<{}", day(t)));
        }
        Ok(self
            .search(&q, idle_before.is_some(), cap)?
            .iter()
            .map(pr_from_search)
            .collect())
    }
    fn stale_issues_anywhere(
        &self,
        user: &str,
        idle_before: &str,
        cap: usize,
    ) -> PResult<Vec<Issue>> {
        let q = format!(
            "is:issue is:open archived:false assignee:{user} updated:<{}",
            day(idle_before)
        );
        Ok(self.search(&q, true, cap)?.iter().map(issue_from).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::github::GitHubConnector;
    use crate::connectors::http::HttpRequest;
    use crate::connectors::testutil::FakeHttp;
    use crate::connectors::Account;
    use serde_json::json;
    use std::sync::Arc;

    pub(crate) fn host(http: Arc<FakeHttp>) -> GitHub {
        let acct = Account::new("gh", "github").with_setting("api_url", "https://api.github.com");
        GitHub::new(
            Box::new(GitHubConnector::new(acct, Some("t".into()), http)),
            "o/r",
        )
        .unwrap()
    }

    pub(crate) fn urls(http: &FakeHttp) -> Vec<String> {
        http.calls
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.url.clone())
            .collect()
    }

    fn gh_issue(n: u64) -> serde_json::Value {
        json!({"number": n, "title": "T", "body": null, "state": "open",
               "labels": [{"name": "bug"}], "assignees": [{"login": "me"}],
               "user": {"login": "pm"}, "html_url": "https://github.com/o/r/issues/1",
               "repository_url": "https://api.github.com/repos/o/r",
               "created_at": "c", "updated_at": "u", "comments": 0})
    }

    #[test]
    fn repo_must_be_owner_slash_name() {
        let mk = |r: &str| {
            let acct =
                Account::new("gh", "github").with_setting("api_url", "https://api.github.com");
            GitHub::new(
                Box::new(GitHubConnector::new(acct, None, FakeHttp::ok(json!({})))),
                r,
            )
            .is_ok()
        };
        assert!(mk("o/r"));
        assert!(mk("my-org/repo.name_2"));
        for bad in ["o", "o/r/x", "../r", "o/..", "o/r?x=1", "/r", "o/", "o r/x"] {
            assert!(!mk(bad), "{bad}");
        }
    }

    #[test]
    fn assigned_issues_never_include_pull_requests() {
        let mut pr = gh_issue(2);
        pr["pull_request"] = json!({"url": "x"});
        let http = FakeHttp::ok(json!([gh_issue(1), pr]));
        let h = host(http.clone());
        let got = h.list_assigned_open("me", 100).unwrap();
        assert_eq!(got.iter().map(|i| i.number).collect::<Vec<_>>(), vec![1]);
        assert_eq!(
            urls(&http)[0],
            "https://api.github.com/repos/o/r/issues?state=open&assignee=me&per_page=100&page=1"
        );
        let i = &got[0];
        assert_eq!(
            (i.repo.as_str(), i.body.as_str(), i.author.as_str()),
            ("o/r", "", "pm")
        );
        assert_eq!(i.labels, vec!["bug"]);
        assert_eq!(i.state, IssueState::Open);
    }

    #[test]
    fn issue_rejects_a_pull_request_number() {
        let mut pr = gh_issue(2);
        pr["pull_request"] = json!({});
        assert!(matches!(
            host(FakeHttp::ok(pr)).issue(2),
            Err(ProviderError::Malformed(_))
        ));
    }

    #[test]
    fn comments_read_enough_newest_pages_for_500_ascending() {
        let http = FakeHttp::new(|r: &HttpRequest| {
            if r.url.ends_with("/issues/9") {
                return Ok(json!({"number": 9, "comments": 730}));
            }
            let page: u64 = r.url.split("page=").last().unwrap().parse().unwrap();
            let n = if page == 8 { 30 } else { 100 };
            Ok(json!((0..n)
                .map(|i| {
                    let id = (page - 1) * 100 + i + 1;
                    json!({"id": id, "user": {"login": "a"}, "body": "b", "created_at": "t"})
                })
                .collect::<Vec<_>>()))
        });
        let cs = host(http.clone()).comments(9).unwrap();
        let u = urls(&http);
        // six pages: a partial last page still leaves a full 500 newest.
        assert_eq!(u.len(), 7);
        assert!(u[1].ends_with("/issues/9/comments?per_page=100&page=3"));
        assert!(u[6].ends_with("page=8"));
        assert_eq!(cs.len(), 500);
        assert_eq!(cs.first().unwrap().id, "231");
        assert_eq!(cs.last().unwrap().id, "730");
        assert_eq!(cs[0].source, CommentSource::Issue);
    }

    #[test]
    fn writes_and_refs() {
        let http = FakeHttp::ok(json!({"login": "me"}));
        let h = host(http.clone());
        h.post_comment(4, "hi").unwrap();
        h.add_label(4, "loop:needs-human").unwrap();
        assert_eq!(IssueTracker::me(&h).unwrap(), "me");
        let calls = http.calls.lock().unwrap();
        assert!(calls[0].url.ends_with("/repos/o/r/issues/4/comments"));
        assert_eq!(calls[0].json_body, Some(json!({"body": "hi"})));
        assert!(calls[1].url.ends_with("/repos/o/r/issues/4/labels"));
        assert_eq!(
            calls[1].json_body,
            Some(json!({"labels": ["loop:needs-human"]}))
        );
        drop(calls);
        assert_eq!(h.issue_url(4), "https://github.com/o/r/issues/4");
        assert_eq!(
            (h.issue_ref(4), h.closes_keyword(4)),
            ("#4".into(), "Closes #4".into())
        );
        assert_eq!(web_base("https://ghe.corp/api/v3/"), "https://ghe.corp");
        assert_eq!(web_base("https://api.github.com"), "https://github.com");
    }
    fn pr(n: u64, author: &str) -> serde_json::Value {
        json!({"number": n, "title": "P", "body": null, "state": "open", "draft": true,
               "head": {"ref": "feat/issue-1-x", "sha": "h"},
               "base": {"ref": "main", "repo": {"full_name": "o/r"}},
               "user": {"login": author}, "requested_reviewers": [{"login": "me"}],
               "assignees": [], "html_url": "w", "updated_at": "u", "merged_at": null,
               "review_comments": 0, "comments": 0})
    }

    #[test]
    fn change_requests_filter_locally_by_role() {
        let http = FakeHttp::ok(json!([pr(1, "me"), pr(2, "other")]));
        let h = host(http.clone());
        let mine = h.change_requests(CrFilter::Mine, "me", 100).unwrap();
        assert_eq!(mine.iter().map(|c| c.number).collect::<Vec<_>>(), vec![1]);
        let review = h
            .change_requests(CrFilter::ReviewRequested, "me", 100)
            .unwrap();
        assert_eq!(review.len(), 2);
        assert!(h
            .change_requests(CrFilter::Assigned, "me", 100)
            .unwrap()
            .is_empty());
        assert!(urls(&http)[0].ends_with("/repos/o/r/pulls?state=open&per_page=100&page=1"));
        let c = &mine[0];
        assert_eq!(
            (
                c.source_branch.as_str(),
                c.target_branch.as_str(),
                c.head_sha.as_str()
            ),
            ("feat/issue-1-x", "main", "h")
        );
        assert_eq!(
            (c.repo.as_str(), c.state, c.draft),
            ("o/r", CrState::Open, true)
        );
        let mut merged = pr(3, "me");
        merged["state"] = json!("closed");
        merged["merged_at"] = json!("m");
        assert_eq!(pr_from(&merged).state, CrState::Merged);
    }

    #[test]
    fn find_by_source_branch_uses_owner_head_and_state() {
        let http = FakeHttp::ok(json!([pr(1, "me")]));
        let h = host(http.clone());
        assert_eq!(
            h.find_by_source_branch("feat/issue-1-x", Some(CrState::Open))
                .unwrap()
                .unwrap()
                .number,
            1
        );
        assert!(urls(&http)[0].contains("/repos/o/r/pulls?head=o%3Afeat%2Fissue-1-x&state=open"));
        // asked for merged; the open one does not count
        assert!(h
            .find_by_source_branch("feat/issue-1-x", Some(CrState::Merged))
            .unwrap()
            .is_none());
        assert!(urls(&http)[1].contains("state=closed"));
    }

    const SIG: &str = "\n\n— LoopX review";

    fn at(path: &str, line: Option<u64>, body: &str) -> DraftNote {
        DraftNote {
            position: Some(DiffPosition {
                path: path.into(),
                line,
                ..Default::default()
            }),
            body: body.into(),
        }
    }

    /// GET /user → me; GET reviews → `reviews`; GET the PR → `pr(5)`;
    /// POST reviews → `post(n)` for the n-th POST; DELETE → {}.
    fn review_http(
        reviews: serde_json::Value,
        post: impl Fn(usize, &HttpRequest) -> Result<serde_json::Value, crate::connectors::http::HttpError>
            + Send
            + Sync
            + 'static,
    ) -> Arc<FakeHttp> {
        let posts = std::sync::atomic::AtomicUsize::new(0);
        FakeHttp::new(move |r: &HttpRequest| {
            if r.url.ends_with("/user") {
                Ok(json!({"login": "me"}))
            } else if r.method == "GET" && r.url.contains("/reviews") {
                Ok(reviews.clone())
            } else if r.method == "GET" {
                Ok(pr(5, "x"))
            } else if r.method == "POST" {
                post(posts.fetch_add(1, std::sync::atomic::Ordering::SeqCst), r)
            } else {
                Ok(json!({}))
            }
        })
    }

    #[test]
    fn draft_review_is_one_signed_pending_review_without_event() {
        let http = review_http(
            json!([{"state": "COMMENTED", "user": {"login": "me"}}]),
            |_, _| Ok(json!({"id": 1})),
        );
        let h = host(http.clone());
        let out = h
            .draft_review(
                &pr_from(&pr(5, "x")),
                &[
                    DraftNote {
                        position: None,
                        body: "overall".into(),
                    },
                    at("a.rs", Some(3), "here"),
                    at("b.rs", None, "file-level"),
                ],
                SIG,
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
        let post = calls.iter().find(|c| c.method == "POST").unwrap();
        assert!(post.url.ends_with("/repos/o/r/pulls/5/reviews"));
        assert_eq!(
            post.json_body,
            Some(json!({
                "commit_id": "h",
                "body": format!("overall\n\n`b.rs` file-level{SIG}"),
                "comments": [{"path": "a.rs", "line": 3, "side": "RIGHT", "body": "here"}]
            }))
        );
    }

    #[test]
    fn draft_review_never_touches_a_pending_review_it_did_not_write() {
        let http = review_http(
            json!([{"id": 4, "state": "PENDING", "body": "mine, by hand", "user": {"login": "me"}}]),
            |_, _| Ok(json!({})),
        );
        let r = host(http.clone()).draft_review(
            &pr_from(&pr(5, "x")),
            &[DraftNote {
                position: None,
                body: "b".into(),
            }],
            SIG,
        );
        assert!(matches!(r, Err(ProviderError::PendingReviewExists)));
        assert!(http.calls.lock().unwrap().iter().all(|c| c.method == "GET"));
    }

    #[test]
    fn draft_review_replaces_its_own_earlier_pending_review() {
        let http = review_http(
            json!([{"id": 4, "state": "PENDING", "body": format!("old{SIG}\n"), "user": {"login": "me"}},
                   {"id": 5, "state": "PENDING", "body": format!("x{SIG}"), "user": {"login": "someone"}}]),
            |_, _| Ok(json!({})),
        );
        host(http.clone())
            .draft_review(
                &pr_from(&pr(5, "x")),
                &[DraftNote {
                    position: None,
                    body: "new".into(),
                }],
                SIG,
            )
            .unwrap();
        let calls = http.calls.lock().unwrap();
        let writes: Vec<_> = calls
            .iter()
            .filter(|c| c.method != "GET")
            .map(|c| (c.method.as_str(), c.url.clone()))
            .collect();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].0, "DELETE");
        assert!(writes[0].1.ends_with("/repos/o/r/pulls/5/reviews/4"));
        assert_eq!(writes[1].0, "POST");
    }

    #[test]
    fn a_rejected_review_moves_line_comments_into_the_body_once() {
        let http = review_http(json!([]), |n, r| {
            if n == 0 {
                Err(crate::connectors::http::HttpError::new(
                    Some(422),
                    "line not part of the diff",
                    &r.url,
                ))
            } else {
                Ok(json!({}))
            }
        });
        let out = host(http.clone())
            .draft_review(
                &pr_from(&pr(5, "x")),
                &[at("a.rs", Some(3), "here"), at("b.rs", Some(9), "there")],
                SIG,
            )
            .unwrap();
        assert_eq!(
            out,
            DraftOutcome {
                posted: 2,
                fallback: 2
            }
        );
        let calls = http.calls.lock().unwrap();
        let posts: Vec<_> = calls.iter().filter(|c| c.method == "POST").collect();
        assert_eq!(posts.len(), 2);
        assert_eq!(
            posts[1].json_body,
            Some(
                json!({"commit_id": "h", "body": format!("`a.rs:3` here\n\n`b.rs:9` there{SIG}"), "comments": []})
            )
        );
    }

    #[test]
    fn a_pr_from_search_is_fetched_for_its_head_sha() {
        let http = review_http(json!([]), |_, _| Ok(json!({})));
        let mut listed = pr_from(&pr(5, "x"));
        listed.head_sha.clear();
        host(http.clone())
            .draft_review(
                &listed,
                &[DraftNote {
                    position: None,
                    body: "b".into(),
                }],
                "",
            )
            .unwrap();
        let calls = http.calls.lock().unwrap();
        assert!(calls
            .iter()
            .any(|c| c.method == "GET" && c.url.ends_with("/repos/o/r/pulls/5")));
        let post = calls.iter().find(|c| c.method == "POST").unwrap();
        assert_eq!(post.json_body.as_ref().unwrap()["commit_id"], "h");
        // no signature: the body is the notes alone
        assert_eq!(post.json_body.as_ref().unwrap()["body"], "b");
    }

    #[test]
    fn actions_runs_jobs_and_logs() {
        let http = FakeHttp::new(|r: &HttpRequest| {
            Ok(if r.url.contains("/jobs?") {
                json!({"jobs": [{"id": 7, "name": "test", "conclusion": "failure", "html_url": "jw"},
                                {"id": 8, "name": "lint", "conclusion": "success", "html_url": "jw"}]})
            } else {
                json!({"workflow_runs": [{"id": 70, "head_branch": "main", "head_sha": "s",
                       "status": "completed", "conclusion": "failure", "html_url": "rw", "updated_at": "u"}]})
            })
        });
        *http.text.lock().unwrap() = Ok("Error: boom".into());
        let h = host(http.clone());
        let runs = h
            .failed_pipelines("main", "2026-11-01T00:00:00Z", 20)
            .unwrap();
        assert!(urls(&http)[0].contains("/repos/o/r/actions/runs?branch=main&status=failure&created=%3E%3D2026-11-01T00%3A00%3A00Z"));
        assert_eq!(
            (runs[0].id.as_str(), runs[0].status.as_str()),
            ("70", "failure")
        );
        let jobs = h.failed_jobs(&runs[0]).unwrap();
        assert_eq!(
            jobs.iter().map(|j| j.name.as_str()).collect::<Vec<_>>(),
            vec!["test"]
        );
        assert!(urls(&http)[1].contains("/actions/runs/70/jobs?filter=latest"));
        assert_eq!(h.job_log_tail(&jobs[0]).unwrap(), "Error: boom");
        h.cr_failed_pipelines(&pr_from(&pr(5, "x"))).unwrap();
        assert!(urls(&http)[2].contains("/actions/runs?head_sha=h&status=failure"));
    }

    #[test]
    fn merged_between_is_one_search_with_exact_bounds() {
        let item = |n: u64, merged: Option<&str>| {
            json!({"number": n, "state": "closed", "draft": false,
                   "repository_url": "https://api.github.com/repos/o/r",
                   "pull_request": {"merged_at": merged}, "user": {"login": "a"}})
        };
        let rows = json!({"items": [
            item(1, Some("2026-10-02T07:59:00Z")),
            item(2, Some("2026-10-02T08:01:00Z")),   // after v2
            item(3, None),                            // closed unmerged
            item(4, Some("2026-10-01T00:00:00Z")),   // exactly at v1: v1's
        ]});
        let http = FakeHttp::ok(rows);
        let h = host(http.clone());
        let v1 = Tag {
            name: "v1".into(),
            sha: "1".into(),
            created_at: Some("2026-10-01T00:00:00Z".into()),
        };
        let v2 = Tag {
            name: "v2".into(),
            sha: "2".into(),
            created_at: Some("2026-10-02T10:00:00+02:00".into()),
        };
        let prs = h.merged_between(Some(&v1), &v2, "main", 50).unwrap();
        assert_eq!(prs.iter().map(|p| p.number).collect::<Vec<_>>(), vec![1]);
        let p = &prs[0];
        assert_eq!(p.state, CrState::Merged);
        assert_eq!(p.merged_at.as_deref(), Some("2026-10-02T07:59:00Z"));
        assert_eq!((p.target_branch.as_str(), p.repo.as_str()), ("main", "o/r"));
        let u = urls(&http);
        assert_eq!(u.len(), 1);
        assert!(u[0].contains(
            "/search/issues?q=repo%3Ao%2Fr+is%3Apr+is%3Amerged+base%3Amain+merged%3A2026-10-01T00%3A00%3A00Z..2026-10-02T10%3A00%3A00%2B02%3A00"
        ), "{}", u[0]);
        // no previous tag: everything merged up to the new one
        let http = FakeHttp::ok(json!({"items": []}));
        host(http.clone())
            .merged_between(None, &v2, "main", 50)
            .unwrap();
        assert!(urls(&http)[0].contains("merged%3A%3C%3D2026-10-02T10%3A00%3A00%2B02%3A00"));
    }

    #[test]
    fn account_search_queries() {
        let http = FakeHttp::ok(json!({"items": [{"number": 4, "draft": false,
            "repository_url": "https://api.github.com/repos/o/r", "pull_request": {}, "user": {"login": "me"}}]}));
        let acct = Account::new("gh", "github").with_setting("api_url", "https://api.github.com");
        let a = GitHubAccount::new(Box::new(GitHubConnector::new(
            acct,
            Some("t".into()),
            http.clone(),
        )));
        let crs = a
            .change_requests_anywhere(CrFilter::ReviewRequested, "me", None, 50)
            .unwrap();
        assert_eq!((crs[0].number, crs[0].repo.as_str()), (4, "o/r"));
        a.change_requests_anywhere(CrFilter::Mine, "me", Some("2026-10-01T00:00:00Z"), 50)
            .unwrap();
        a.stale_issues_anywhere("me", "2026-10-01T00:00:00Z", 50)
            .unwrap();
        let u = urls(&http);
        assert!(u[0].contains("/search/issues?q=is%3Apr+is%3Aopen+archived%3Afalse+review-requested%3Ame&per_page=50&page=1"));
        assert!(u[1].contains("q=is%3Apr+is%3Aopen+archived%3Afalse+author%3Ame+updated%3A%3C2026-10-01&sort=updated&order=asc"));
        assert!(u[2].contains("q=is%3Aissue+is%3Aopen+archived%3Afalse+assignee%3Ame+updated%3A%3C2026-10-01&sort=updated&order=asc"));
    }

    #[test]
    fn tags_are_newest_first_by_commit_date_not_by_name() {
        let http = FakeHttp::new(|r: &HttpRequest| {
            let u = &r.url;
            let date = |d: &str| json!({"commit": {"committer": {"date": d}}});
            Ok(if u.contains("/tags?") {
                json!([{"name": "v1.9", "commit": {"sha": "s9"}},
                       {"name": "v1.10", "commit": {"sha": "s10"}},
                       {"name": "v1.8", "commit": {"sha": "s8"}}])
            } else if u.ends_with("/commits/s9") {
                date("2026-09-09T00:00:00Z")
            } else if u.ends_with("/commits/s10") {
                date("2026-10-10T00:00:00Z")
            } else {
                date("2026-08-08T00:00:00Z")
            })
        });
        let tags = host(http.clone()).tags(2).unwrap();
        assert_eq!(
            tags.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["v1.10", "v1.9"]
        );
        // scans more tags than asked for, so an unordered listing still
        // finds the newest; never more than one page of 100
        assert!(urls(&http)[0].ends_with("/repos/o/r/tags?per_page=20"));
        let http = FakeHttp::ok(json!([]));
        host(http.clone()).tags(500).unwrap();
        assert!(urls(&http)[0].ends_with("/tags?per_page=100"));
    }

    #[test]
    fn cr_comments_merge_conversation_diff_comments_and_review_bodies() {
        let http = FakeHttp::new(|r: &HttpRequest| {
            let u = &r.url;
            Ok(if u.ends_with("/pulls/5") {
                json!({"number": 5, "comments": 1, "review_comments": 1})
            } else if u.contains("/issues/5/comments") {
                json!([{"id": 1, "user": {"login": "a"}, "body": "talk", "created_at": "2026-10-01T01:00:00Z"}])
            } else if u.contains("/pulls/5/comments") {
                json!([{"id": 2, "user": {"login": "b"}, "body": "line", "path": "a.rs", "line": 3,
                        "created_at": "2026-10-01T03:00:00Z"}])
            } else {
                json!([
                    {"id": 7, "state": "CHANGES_REQUESTED", "body": "please rename", "user": {"login": "c"},
                     "submitted_at": "2026-10-01T02:00:00Z"},
                    {"id": 8, "state": "PENDING", "body": "draft", "user": {"login": "c"}},
                    {"id": 9, "state": "APPROVED", "body": "", "user": {"login": "c"},
                     "submitted_at": "2026-10-01T04:00:00Z"}
                ])
            })
        });
        let cs = host(http.clone()).cr_comments(5).unwrap();
        assert_eq!(
            cs.iter()
                .map(|c| (c.id.as_str(), c.body.as_str()))
                .collect::<Vec<_>>(),
            vec![("1", "talk"), ("v7", "please rename"), ("r2", "line")]
        );
        assert!(cs.iter().all(|c| c.source == CommentSource::ChangeRequest));
        assert_eq!(cs[1].created_at, "2026-10-01T02:00:00Z");
        assert!(urls(&http)
            .iter()
            .any(|u| u.contains("/pulls/5/reviews?per_page=100&page=1")));
    }

    #[test]
    fn idle_dates_never_panic_on_odd_input() {
        let http = FakeHttp::ok(json!({"items": []}));
        let acct = Account::new("gh", "github").with_setting("api_url", "https://api.github.com");
        let a = GitHubAccount::new(Box::new(GitHubConnector::new(acct, None, http.clone())));
        // a 2-byte character straddling byte 10, and a short value
        a.stale_issues_anywhere("me", "2026-10-0\u{e9}x", 5)
            .unwrap();
        a.change_requests_anywhere(CrFilter::Mine, "me", Some("2026"), 5)
            .unwrap();
        assert!(urls(&http)[1].contains("updated%3A%3C2026&"));
    }
}
