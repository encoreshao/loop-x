//! Provider-neutral view of issue trackers and code hosts.
//!
//! Loops talk to [`IssueTracker`] (where tasks come from), [`CodeHost`]
//! (where code, change requests and CI live) and [`AccountHost`] (account-wide
//! queries across every repository). GitLab and GitHub implement all three on
//! top of their existing connectors; [`resolve`] picks the implementation from
//! the connector account's `kind`. Every normalized struct keeps the
//! provider's JSON in `raw`, so a caller can still read a provider-specific
//! field without the trait growing one method per field.

pub mod binding;
pub mod branch;
pub mod github;
pub mod gitlab;
mod resolve;

pub use resolve::{account_host, resolve, Resolved};

use crate::connectors::{ApiOpts, Connector, ConnectorError};
use serde_json::Value;

pub const DEFAULT_CAP: usize = 100;
pub const COMMENT_CAP: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Gitlab,
    Github,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Provider::Gitlab => "GitLab",
            Provider::Github => "GitHub",
        }
    }
    pub fn kind(self) -> &'static str {
        match self {
            Provider::Gitlab => "gitlab",
            Provider::Github => "github",
        }
    }

    /// Every implemented provider. A new tracker (Jira, Linear, ...) is a
    /// variant here plus its `IssueTracker` in `resolve`; the issue loop, MR
    /// Review and the dashboard pick it up from this list.
    pub const ALL: &'static [Provider] = &[Provider::Gitlab, Provider::Github];

    /// The provider of a connector account kind (`"github"`), if any.
    pub fn from_kind(kind: &str) -> Option<Provider> {
        Provider::ALL.iter().copied().find(|p| p.kind() == kind)
    }

    /// How its change requests are numbered: GitLab `!7`, GitHub `#7`.
    pub fn change_request_prefix(self) -> &'static str {
        match self {
            Provider::Gitlab => "!",
            Provider::Github => "#",
        }
    }

    /// Is a project on it set up through a connector account (`code_host`)?
    /// GitLab keeps its own instances and aliases (`~/.gitlab/config.json`).
    pub fn connected(self) -> bool {
        self != Provider::Gitlab
    }

    /// The repository's web page, from the account's API URL.
    pub fn repo_web_url(self, api_url: &str, repo: &str) -> Option<String> {
        match self {
            Provider::Github => {
                // An account saved without one uses the connector's default.
                let api = if api_url.trim().is_empty() {
                    "https://api.github.com"
                } else {
                    api_url
                };
                Some(format!("{}/{repo}", github::web_base(api)))
            }
            Provider::Gitlab => None,
        }
    }
}

#[derive(Debug)]
pub enum ProviderError {
    Connector(ConnectorError),
    /// 429, or 403 whose body mentions a rate limit.
    RateLimited,
    /// GitHub: the user already has a pending review on this pull request.
    PendingReviewExists,
    Unsupported {
        kind: String,
        role: &'static str,
    },
    UnknownAccount(String),
    Load(String),
    Malformed(String),
}

pub type PResult<T> = Result<T, ProviderError>;

impl From<ConnectorError> for ProviderError {
    fn from(e: ConnectorError) -> Self {
        if let ConnectorError::Http(h) = &e {
            let limited = h.status == Some(429)
                || (h.status == Some(403) && h.body.to_lowercase().contains("rate limit"));
            if limited {
                return ProviderError::RateLimited;
            }
        }
        ProviderError::Connector(e)
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::Connector(e) => write!(f, "{e}"),
            ProviderError::RateLimited => write!(f, "rate limited"),
            ProviderError::PendingReviewExists => {
                write!(f, "a pending review already exists on this pull request")
            }
            ProviderError::Unsupported { kind, role } => {
                write!(f, "{kind} does not provide {role}")
            }
            ProviderError::UnknownAccount(id) => write!(f, "unknown connector account {id}"),
            ProviderError::Load(class) => write!(f, "connector failed to load ({class})"),
            ProviderError::Malformed(what) => write!(f, "{what}"),
        }
    }
}

impl ProviderError {
    /// Detail-free name for logs (no URLs, tokens or response bodies).
    pub fn class_name(&self) -> String {
        match self {
            ProviderError::Connector(e) => e.class_name(),
            ProviderError::RateLimited => "RateLimited".into(),
            ProviderError::PendingReviewExists => "PendingReviewExists".into(),
            ProviderError::Unsupported { .. } => "Unsupported".into(),
            ProviderError::UnknownAccount(_) => "UnknownAccount".into(),
            ProviderError::Load(_) => "LoadFailure".into(),
            ProviderError::Malformed(_) => "Malformed".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueState {
    Open,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrState {
    Open,
    Merged,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrFilter {
    /// Authored by the user.
    Mine,
    /// The user is a requested reviewer.
    ReviewRequested,
    /// The user is an assignee.
    Assigned,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub number: u64,
    /// `group/project` or `owner/name`.
    pub repo: String,
    pub title: String,
    pub body: String,
    pub state: IssueState,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub author: String,
    pub web_url: String,
    pub created_at: String,
    pub updated_at: String,
    pub raw: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentSource {
    Issue,
    ChangeRequest,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DiffPosition {
    /// The file's path on the new side of the diff.
    pub path: String,
    /// Its path before a rename; `None` when it was not renamed.
    pub old_path: Option<String>,
    pub line: Option<u64>,
    pub old_line: Option<u64>,
    pub base_sha: Option<String>,
    pub start_sha: Option<String>,
    pub head_sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Comment {
    /// Provider id as text. GitHub review (diff) comments are prefixed `r`,
    /// GitHub review summaries `v`.
    pub id: String,
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub source: CommentSource,
    pub position: Option<DiffPosition>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangeRequest {
    pub number: u64,
    pub repo: String,
    pub title: String,
    pub description: String,
    /// Empty when the provider's listing does not carry it (GitHub search).
    pub source_branch: String,
    pub target_branch: String,
    pub head_sha: String,
    pub state: CrState,
    pub draft: bool,
    pub author: String,
    pub reviewers: Vec<String>,
    pub assignees: Vec<String>,
    pub web_url: String,
    pub updated_at: String,
    pub merged_at: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    pub old_path: String,
    pub new_path: String,
    pub diff: String,
    pub new_file: bool,
    pub deleted_file: bool,
    pub renamed_file: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PipelineRun {
    pub id: String,
    pub git_ref: String,
    pub sha: String,
    pub status: String,
    pub web_url: String,
    pub updated_at: String,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub stage: Option<String>,
    pub status: String,
    pub web_url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tag {
    pub name: String,
    pub sha: String,
    /// Commit date of the tagged commit, RFC 3339.
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepoInfo {
    pub path: String,
    pub default_branch: String,
    pub web_url: String,
}

/// What `draft_review` posted: `fallback` counts line notes the provider
/// rejected (line not in the diff, file renamed, ...) that went in as
/// general notes prefixed `path:line` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DraftOutcome {
    pub posted: usize,
    pub fallback: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DraftNote {
    pub position: Option<DiffPosition>,
    pub body: String,
}

pub trait IssueTracker: Send + Sync {
    fn provider(&self) -> Provider;
    fn me(&self) -> PResult<String>;
    fn list_assigned_open(&self, user: &str, cap: usize) -> PResult<Vec<Issue>>;
    fn issue(&self, n: u64) -> PResult<Issue>;
    /// Newest [`COMMENT_CAP`] human comments, oldest first.
    fn comments(&self, n: u64) -> PResult<Vec<Comment>>;
    fn post_comment(&self, n: u64, body: &str) -> PResult<()>;
    fn add_label(&self, n: u64, label: &str) -> PResult<()>;
    fn issue_ref(&self, n: u64) -> String;
    fn issue_url(&self, n: u64) -> String;
    fn closes_keyword(&self, n: u64) -> String;
}

pub trait CodeHost: Send + Sync {
    fn provider(&self) -> Provider;
    fn me(&self) -> PResult<String>;
    fn repo_info(&self) -> PResult<RepoInfo>;
    fn change_requests(
        &self,
        filter: CrFilter,
        user: &str,
        cap: usize,
    ) -> PResult<Vec<ChangeRequest>>;
    fn change_request(&self, n: u64) -> PResult<ChangeRequest>;
    fn find_by_source_branch(
        &self,
        branch: &str,
        state: Option<CrState>,
    ) -> PResult<Option<ChangeRequest>>;
    fn diff(&self, n: u64) -> PResult<Vec<FileDiff>>;
    /// Newest [`COMMENT_CAP`] human comments on the change request, oldest first.
    fn cr_comments(&self, n: u64) -> PResult<Vec<Comment>>;
    /// Post notes only the user sees until they publish them (GitLab draft
    /// notes; one GitHub pending review). Never approves or publishes.
    ///
    /// First removes this loop's own earlier drafts: those whose text ends
    /// with `signature` (GitLab draft notes; a GitHub pending review whose
    /// body ends with it). Nothing else is ever touched, and an empty
    /// signature removes nothing. A pending GitHub review by the user that
    /// does not carry the signature is theirs: `PendingReviewExists`.
    /// GitHub gets the signature appended to the review body; GitLab notes
    /// are posted exactly as given. A change request from a listing without
    /// diff refs / head SHA is fetched again first.
    fn draft_review(
        &self,
        cr: &ChangeRequest,
        notes: &[DraftNote],
        signature: &str,
    ) -> PResult<DraftOutcome>;
    fn open_change_request(
        &self,
        branch: &str,
        target: &str,
        title: &str,
        body: &str,
    ) -> PResult<ChangeRequest>;
    fn update_description(&self, n: u64, body: &str) -> PResult<()>;
    fn failed_pipelines(&self, git_ref: &str, since: &str, cap: usize)
        -> PResult<Vec<PipelineRun>>;
    fn cr_failed_pipelines(&self, cr: &ChangeRequest) -> PResult<Vec<PipelineRun>>;
    fn failed_jobs(&self, run: &PipelineRun) -> PResult<Vec<Job>>;
    /// The end of the job's log (failures sit at the end).
    fn job_log_tail(&self, job: &Job) -> PResult<String>;
    /// Newest first.
    fn tags(&self, limit: usize) -> PResult<Vec<Tag>>;
    fn merged_between(
        &self,
        from: Option<&Tag>,
        to: &Tag,
        branch: &str,
        cap: usize,
    ) -> PResult<Vec<ChangeRequest>>;
    fn cr_ref(&self, n: u64) -> String;
    fn cr_noun(&self) -> &'static str;
}

pub trait AccountHost: Send + Sync {
    fn provider(&self) -> Provider;
    fn me(&self) -> PResult<String>;
    /// Open change requests in any repository. `idle_before` (RFC 3339) keeps
    /// only those not updated since, oldest first.
    fn change_requests_anywhere(
        &self,
        filter: CrFilter,
        user: &str,
        idle_before: Option<&str>,
        cap: usize,
    ) -> PResult<Vec<ChangeRequest>>;
    /// Open issues assigned to `user` in any repository, not updated since
    /// `idle_before`, oldest first.
    fn stale_issues_anywhere(
        &self,
        user: &str,
        idle_before: &str,
        cap: usize,
    ) -> PResult<Vec<Issue>>;
}

// ---- shared helpers ----------------------------------------------------------

/// A string field as text: strings as-is, numbers/bools printed, else "".
pub(crate) fn s(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// `true` for a 400/422, the answer to a note on a line or file the
/// provider will not place a comment on.
pub(crate) fn is_rejected(e: &ProviderError) -> bool {
    matches!(e, ProviderError::Connector(ConnectorError::Http(h)) if matches!(h.status, Some(400) | Some(422)))
}

/// `path:line` (or `path`) prefix for a note posted without its position.
pub(crate) fn located(p: &DiffPosition, body: &str) -> String {
    match p.line {
        Some(l) => format!("`{}:{l}` {body}", p.path),
        None => format!("`{}` {body}", p.path),
    }
}

/// The text ends with the (trimmed, non-empty) signature.
pub(crate) fn signed(text: &str, signature: &str) -> bool {
    let sig = signature.trim();
    !sig.is_empty() && text.trim_end().ends_with(sig)
}

pub(crate) fn opt_s(v: &Value, key: &str) -> Option<String> {
    Some(s(v, key)).filter(|x| !x.is_empty())
}

pub(crate) fn num(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// `v[key][*][field]` as strings, skipping entries without it.
pub(crate) fn logins(v: &Value, key: &str, field: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| e.get(field).and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// GET `path` page by page (`per_page = min(cap, 100)`) until a short page or
/// `cap` items. `items_key` reads `{key: [...]}` envelopes (GitHub search,
/// Actions). `path` must not already carry `per_page`/`page`.
pub(crate) fn paged(
    conn: &dyn Connector,
    path: &str,
    cap: usize,
    items_key: Option<&str>,
) -> PResult<Vec<Value>> {
    let per_page = cap.clamp(1, 100);
    let sep = if path.contains('?') { '&' } else { '?' };
    let mut out = Vec::new();
    let mut page = 1;
    while out.len() < cap {
        let v = conn.api(
            "GET",
            &format!("{path}{sep}per_page={per_page}&page={page}"),
            &ApiOpts::default(),
        )?;
        let items = match items_key {
            Some(k) => v.get(k).cloned().unwrap_or(Value::Null),
            None => v,
        };
        let Value::Array(items) = items else {
            return Err(ProviderError::Malformed("expected a list".into()));
        };
        let short = items.len() < per_page;
        out.extend(items);
        if short {
            break;
        }
        page += 1;
    }
    out.truncate(cap);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::http::HttpError;
    use crate::connectors::testutil::FakeHttp;
    use crate::connectors::{github, Account};
    use serde_json::json;
    use std::sync::Arc;

    fn conn(http: Arc<FakeHttp>) -> Box<dyn Connector> {
        Box::new(github::GitHubConnector::new(
            Account::new("gh", "github").with_setting("api_url", "https://api.github.com"),
            Some("t".into()),
            http,
        ))
    }

    #[test]
    fn rate_limit_is_its_own_error() {
        let e: ProviderError = ConnectorError::Http(HttpError::new(
            Some(403),
            "API rate limit exceeded for user",
            "u",
        ))
        .into();
        assert!(matches!(e, ProviderError::RateLimited));
        let e: ProviderError =
            ConnectorError::Http(HttpError::new(Some(429), "slow down", "u")).into();
        assert!(matches!(e, ProviderError::RateLimited));
        let e: ProviderError =
            ConnectorError::Http(HttpError::new(Some(403), "forbidden", "u")).into();
        assert!(matches!(e, ProviderError::Connector(_)));
    }

    #[test]
    fn class_names_never_carry_detail() {
        assert_eq!(ProviderError::RateLimited.class_name(), "RateLimited");
        assert_eq!(
            ProviderError::Unsupported {
                kind: "jira".into(),
                role: "code hosting"
            }
            .class_name(),
            "Unsupported"
        );
        assert_eq!(
            ProviderError::Unsupported {
                kind: "jira".into(),
                role: "code hosting"
            }
            .to_string(),
            "jira does not provide code hosting"
        );
    }

    #[test]
    fn paged_walks_pages_until_short_page_or_cap() {
        let http = FakeHttp::new(|r| {
            let page: usize = r.url.split("page=").last().unwrap().parse().unwrap();
            let n = if page < 3 { 2 } else { 1 };
            Ok(json!((0..n)
                .map(|i| json!({"p": page, "i": i}))
                .collect::<Vec<_>>()))
        });
        let c = conn(http.clone());
        let all = paged(c.as_ref(), "/x?a=1", 10, None).unwrap();
        // per_page = min(cap, 100) = 10 → first page of 2 is "short" → stop.
        assert_eq!(all.len(), 2);
        assert!(http.calls.lock().unwrap()[0]
            .url
            .ends_with("/x?a=1&per_page=10&page=1"));

        let all = paged(c.as_ref(), "/y", 3, None).unwrap();
        assert_eq!(all.len(), 2); // per_page=3, page 1 has 2 → short → stop
        assert!(http.calls.lock().unwrap()[1]
            .url
            .ends_with("/y?per_page=3&page=1"));
    }

    #[test]
    fn paged_reads_items_key_and_respects_cap() {
        let http = FakeHttp::new(|_| Ok(json!({"items": [1, 2, 3]})));
        let c = conn(http.clone());
        let all = paged(c.as_ref(), "/search", 2, Some("items")).unwrap();
        assert_eq!(all, vec![json!(1), json!(2)]);
        assert_eq!(http.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn json_helpers_tolerate_missing_and_numbers() {
        let v = json!({"a": "x", "n": 7, "z": null, "u": [{"login": "p"}, {"login": "q"}, {}]});
        assert_eq!(s(&v, "a"), "x");
        assert_eq!(s(&v, "n"), "7");
        assert_eq!(s(&v, "z"), "");
        assert_eq!(s(&v, "missing"), "");
        assert_eq!(opt_s(&v, "z"), None);
        assert_eq!(num(&v, "n"), 7);
        assert_eq!(num(&v, "a"), 0);
        assert_eq!(logins(&v, "u", "login"), vec!["p", "q"]);
    }

    #[test]
    fn providers_are_found_by_kind_and_number_change_requests_their_way() {
        for p in Provider::ALL {
            assert_eq!(Provider::from_kind(p.kind()), Some(*p));
        }
        assert_eq!(Provider::from_kind("jira"), None);
        assert_eq!(Provider::Gitlab.change_request_prefix(), "!");
        assert_eq!(Provider::Github.change_request_prefix(), "#");
        // GitLab has its own config path; every other provider is connected.
        assert!(!Provider::Gitlab.connected());
        assert!(Provider::Github.connected());
        assert_eq!(
            Provider::Github.repo_web_url("https://api.github.com", "o/r"),
            Some("https://github.com/o/r".into())
        );
        assert_eq!(
            Provider::Github.repo_web_url("https://ghe.example/api/v3/", "o/r"),
            Some("https://ghe.example/o/r".into())
        );
        assert_eq!(
            Provider::Github.repo_web_url("", "o/r"),
            Some("https://github.com/o/r".into())
        );
    }
}
