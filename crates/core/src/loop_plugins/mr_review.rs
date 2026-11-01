//! MR Review loop on LoopKit: pre-reviews open, non-draft GitLab merge requests
//! assigned to the user (as assignee or reviewer, never their own) and leaves
//! the findings as *draft* review notes only (mirrors
//! `bin/loop_plugins/mr_review.py`). The user publishes them from the MR
//! (Review -> Submit). This plugin never calls bulk_publish, approve, or the
//! published `/notes` endpoint.
//!
//! The dashboard's Merge requests tab lists the user's MRs ([`list_my_mrs`])
//! and keeps per-MR choices in [`MrPrefs`]: MRs to skip, and MRs to review on
//! the next run even when already reviewed.
#![allow(
    clippy::type_complexity,
    clippy::field_reassign_with_default,
    clippy::let_unit_value
)]

use crate::connectors::{ApiOpts, Connector, ConnectorError, ConnectorSource};
use crate::loopkit::{self, LoopContext, LoopPlugin, Outcome, PluginError, Status, WorkItem};
use crate::loops_config::{py_str, py_truthy};
use crate::providers::binding::{Binding, Endpoint};
use crate::providers::{ChangeRequest, CrFilter, DiffPosition, DraftNote, ProviderError};
use crate::seen_store::SeenStore;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const DIFF_CAP_BYTES: usize = 150_000;
pub const MAX_FINDINGS: usize = 15;
pub const MAX_DESCRIPTION: usize = 4096;
pub const MAX_BODY: usize = 2000;
pub const MAX_SUMMARY: usize = 600;
const MAX_DIFF_PAGES: u32 = 20;
const MAX_DRAFT_PAGES: u32 = 20;
pub const MAX_TITLE: usize = 120;
pub const MAX_OVERVIEW: usize = 4;
pub const MAX_RISKS: usize = 8;
pub const MAX_NOTE: usize = 300;
const SEVERITIES: [&str; 4] = ["nit", "minor", "major", "blocker"];
const VERDICTS: [&str; 3] = ["approve", "comment", "request_changes"];
const LEVELS: [&str; 3] = ["low", "medium", "high"];
/// `(key, label)`; the model answers with the key.
const AREAS: [(&str, &str); 8] = [
    ("correctness", "Correctness"),
    ("security", "Security"),
    ("performance", "Performance"),
    ("data", "Data & migrations"),
    ("reliability", "Reliability"),
    ("compatibility", "Compatibility"),
    ("tests", "Tests"),
    ("maintainability", "Maintainability"),
];
const SIGNATURE: &str = "\n\n_\u{2014} LoopX pre-review_";
const PREFS_FILE: &str = "mr-prefs.json";
pub const DENIED_MESSAGE: &str = "token needs the api scope to write draft notes";

/// What a poster can fail with.
#[derive(Debug, Clone, PartialEq)]
pub enum PostError {
    /// GitLab refused a draft-note write with 401/403 (read-only token).
    Denied(String),
    Other(ConnectorError),
}

impl PostError {
    fn into_plugin(self) -> PluginError {
        match self {
            PostError::Denied(m) => PluginError::new("DraftWriteDenied", m),
            PostError::Other(e) => e.into(),
        }
    }
}

/// `(connector, payload, cleaned answer)` -> `{drafts, fallback_general[, partial]}`.
pub type Poster =
    Box<dyn Fn(&dyn Connector, &Value, &Value) -> Result<Value, PostError> + Send + Sync>;
/// Has this item key been reviewed already? (read-only)
pub type SeenFn = Box<dyn Fn(&str) -> bool + Send + Sync>;

pub fn item_key(account_id: &str, project_id: &str, iid: &str, head_sha: &str) -> String {
    format!("mr:{account_id}:{project_id}!{iid}@{head_sha}")
}

/// `(text, truncated)`; `cap` counts characters.
pub fn build_diff_text(changes: &[Value], cap: usize) -> (String, bool) {
    let mut parts = String::new();
    let (mut size, mut truncated) = (0usize, false);
    for ch in changes {
        let path = ["new_path", "old_path"]
            .iter()
            .filter_map(|k| ch.get(*k))
            .find(|v| py_truthy(v))
            .map(py_str)
            .unwrap_or_default();
        let diff = ch
            .get("diff")
            .filter(|v| py_truthy(v))
            .map(py_str)
            .unwrap_or_default();
        let chunk = format!("--- a/{path}\n+++ b/{path}\n{diff}\n");
        let len = chunk.chars().count();
        if size + len > cap {
            parts.extend(chunk.chars().take(cap.saturating_sub(size)));
            truncated = true;
            break;
        }
        parts.push_str(&chunk);
        size += len;
    }
    if truncated {
        parts.push_str(&format!("\n[diff truncated at {cap} characters]"));
    }
    (parts, truncated)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Draft notes are Markdown on GitLab: break word-start @mentions so a
/// model-echoed @all / @group cannot notify anyone.
fn defuse(text: &str) -> String {
    let mut out = String::new();
    let mut prev: Option<char> = None;
    for c in text.chars() {
        out.push(c);
        if c == '@' && !prev.is_some_and(|p| is_word(p) || p == '@') {
            out.push('\u{200b}');
        }
        prev = Some(c);
    }
    out.replace("![", "!\u{200b}[")
}

fn drafts_path(mr: &Value) -> String {
    format!(
        "/projects/{}/merge_requests/{}/draft_notes",
        py_str(&mr["project_id"]),
        py_str(&mr["iid"])
    )
}

fn post_draft(
    conn: &dyn Connector,
    mr: &Value,
    note: &str,
    position: Option<&Value>,
) -> Result<Value, ConnectorError> {
    let mut body = Map::new();
    body.insert("note".into(), json!(note));
    if let Some(p) = position {
        body.insert("position".into(), p.clone());
    }
    // Non-idempotent: never auto-retry (a timeout after creation would duplicate it).
    conn.api(
        "POST",
        &drafts_path(mr),
        &ApiOpts::json(Value::Object(body)).once(),
    )
}

/// Delete this loop's own earlier drafts (partial runs, older SHAs), across
/// every page. Drafts without the LoopX signature are never touched. All pages
/// are listed before deleting, so a deletion never shifts a later page.
fn clear_old_drafts(conn: &dyn Connector, mr: &Value) -> Result<(), ConnectorError> {
    let mut ours: Vec<i64> = Vec::new();
    let sig = SIGNATURE.trim();
    for page in 1..=MAX_DRAFT_PAGES {
        let rows = conn.api(
            "GET",
            &format!("{}?per_page=100&page={page}", drafts_path(mr)),
            &ApiOpts::default(),
        )?;
        let rows = rows.as_array().cloned().unwrap_or_default();
        for row in &rows {
            let Some(obj) = row.as_object() else { continue };
            let Some(id) = obj.get("id").filter(|v| !v.is_null()) else {
                continue;
            };
            let note = obj
                .get("note")
                .filter(|v| py_truthy(v))
                .map(py_str)
                .unwrap_or_default();
            if note.trim_end().ends_with(sig) {
                if let Some(n) = id
                    .as_i64()
                    .or_else(|| id.as_str().and_then(|s| s.parse().ok()))
                {
                    ours.push(n);
                }
            }
        }
        if rows.len() < 100 {
            break;
        }
    }
    for id in ours {
        conn.api(
            "DELETE",
            &format!("{}/{id}", drafts_path(mr)),
            &ApiOpts::default().once(),
        )?;
    }
    Ok(())
}

/// Python `str.splitlines()` line boundaries.
fn splitlines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let brk = matches!(
            c,
            '\n' | '\r'
                | '\u{b}'
                | '\u{c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if brk {
            out.push(&text[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' {
                if let Some(&(_, '\n')) = chars.peek() {
                    chars.next();
                    end += 1;
                }
            }
            start = end;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// `@@ -a[,n] +b[,m] @@` -> `(a, b)`.
fn parse_hunk(line: &str) -> Option<(i64, i64)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = take_num(rest)?;
    let rest = skip_count(rest);
    let rest = rest.strip_prefix(" +")?;
    let (new, rest) = take_num(rest)?;
    let rest = skip_count(rest);
    rest.starts_with(" @@").then_some((old, new))
}

fn take_num(s: &str) -> Option<(i64, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    Some((s[..end].parse().ok()?, &s[end..]))
}

fn skip_count(s: &str) -> &str {
    match s.strip_prefix(',') {
        Some(r) => {
            let end = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
            if end == 0 {
                s
            } else {
                &r[end..]
            }
        }
        None => s,
    }
}

/// `{path: {new_line: old_line}}` for the unchanged context lines of each file
/// in [`build_diff_text`]'s output. Added lines are absent (they have no old
/// line); GitLab needs `old_line` as well as `new_line` to anchor a note on a
/// context line.
pub fn diff_line_map(
    diff_text: &str,
) -> std::collections::BTreeMap<String, std::collections::BTreeMap<i64, i64>> {
    let mut out: std::collections::BTreeMap<String, std::collections::BTreeMap<i64, i64>> =
        Default::default();
    let (mut path, mut old, mut new): (Option<String>, i64, i64) = (None, 0, 0);
    for line in splitlines(diff_text) {
        if let Some(p) = line.strip_prefix("+++ b/") {
            out.entry(p.to_string()).or_default();
            path = Some(p.to_string());
            continue;
        }
        if line.starts_with("--- a/") {
            continue;
        }
        if let Some((o, n)) = parse_hunk(line) {
            old = o;
            new = n;
            continue;
        }
        let Some(p) = &path else { continue };
        if old == 0 && new == 0 {
            continue;
        }
        if line.starts_with('+') {
            new += 1;
        } else if line.starts_with('-') {
            old += 1;
        } else if line.starts_with(' ') {
            out.entry(p.clone()).or_default().insert(new, old);
            old += 1;
            new += 1;
        }
    }
    out
}

fn post_all(
    conn: &dyn Connector,
    mr: &Value,
    answer: &Value,
    drafts: &mut u64,
    fallback: &mut u64,
) -> Result<(), ConnectorError> {
    let refs = &mr["diff_refs"];
    let context_lines = diff_line_map(mr.get("diff").and_then(Value::as_str).unwrap_or(""));
    let old_paths = mr.get("old_paths").cloned().unwrap_or(Value::Null);
    clear_old_drafts(conn, mr)?;
    for f in answer
        .get("findings")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let path = py_str(&f["path"]);
        let note = format!("{}{SIGNATURE}", finding_note(&f));
        let mut position = Map::new();
        position.insert("position_type".into(), json!("text"));
        for k in ["base_sha", "start_sha", "head_sha"] {
            position.insert(k.into(), refs[k].clone());
        }
        let old_path = old_paths.get(&path).cloned().unwrap_or_else(|| json!(path));
        position.insert("old_path".into(), old_path);
        position.insert("new_path".into(), json!(path));
        position.insert("new_line".into(), f["line"].clone());
        if let Some(old_line) = f["line"]
            .as_i64()
            .and_then(|l| context_lines.get(&path)?.get(&l))
        {
            position.insert("old_line".into(), json!(old_line));
        }
        match post_draft(conn, mr, &note, Some(&Value::Object(position))) {
            Ok(_) => {}
            Err(ConnectorError::Http(e)) if matches!(e.status, Some(400) | Some(422)) => {
                post_draft(
                    conn,
                    mr,
                    &format!("`{path}:{}` {note}", py_str(&f["line"])),
                    None,
                )?;
                *fallback += 1;
            }
            Err(e) => return Err(e),
        }
        *drafts += 1;
    }
    if let Some(summary) = summary_note(answer) {
        post_draft(conn, mr, &format!("{summary}{SIGNATURE}"), None)?;
        *drafts += 1;
    }
    Ok(())
}

fn text_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn severity_badge(sev: &str) -> &'static str {
    match sev {
        "blocker" => "\u{1f534} **Blocker**",
        "major" => "\u{1f7e0} **Major**",
        "minor" => "\u{1f7e1} **Minor**",
        _ => "\u{26aa} **Nit**",
    }
}

fn area_label(key: &str) -> &'static str {
    AREAS
        .iter()
        .find(|(k, _)| *k == key)
        .map_or("Correctness", |(_, l)| l)
}

/// One line for a Markdown table cell.
fn cell(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// An inline draft note: severity and area up front, then the problem and
/// (when given) the fix, so it reads at a glance in the MR.
pub fn finding_note(f: &Value) -> String {
    let sev = text_of(f, "severity");
    let title = text_of(f, "title");
    let mut head = format!(
        "{} \u{b7} {}",
        severity_badge(&sev),
        area_label(&text_of(f, "category"))
    );
    if !title.is_empty() {
        head.push_str(&format!(" \u{2014} {}", defuse(&title)));
    }
    let mut note = format!("{head}\n\n{}", defuse(&text_of(f, "body")));
    let fix = text_of(f, "suggestion");
    if !fix.is_empty() {
        note.push_str(&format!("\n\n**Suggested fix:** {}", defuse(&fix)));
    }
    note
}

/// The overall review note: verdict, what the MR does, a risk table by area
/// and the findings index. `None` when the answer has nothing to say.
pub fn summary_note(answer: &Value) -> Option<String> {
    let summary = text_of(answer, "summary");
    let overview: Vec<String> = answer
        .get("overview")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(defuse)
        .collect();
    let risks: Vec<&Value> = answer
        .get("risks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let findings: Vec<&Value> = answer
        .get("findings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    if summary.is_empty() && overview.is_empty() && risks.is_empty() && findings.is_empty() {
        return None;
    }
    let verdict = match answer.get("verdict").and_then(Value::as_str) {
        Some("request_changes") => "\u{26d4} Changes requested",
        Some("approve") => "\u{2705} Looks good to merge",
        _ => "\u{1f4ac} Mergeable after comments",
    };
    let mut out = vec![format!("### LoopX pre-review \u{2014} {verdict}")];
    if !summary.is_empty() {
        out.push(defuse(&summary));
    }
    if !overview.is_empty() {
        let bullets: Vec<String> = overview.iter().map(|b| format!("- {b}")).collect();
        out.push(format!("**What this MR does**\n{}", bullets.join("\n")));
    }
    if risks.is_empty() {
        out.push("**Risk assessment:** no notable risks found.".to_string());
    } else {
        let mut table = vec![
            "**Risk assessment**".to_string(),
            String::new(),
            "| Area | Level | Notes |".to_string(),
            "| --- | --- | --- |".to_string(),
        ];
        for r in risks {
            let level = match text_of(r, "level").as_str() {
                "high" => "\u{1f534} High",
                "medium" => "\u{1f7e0} Medium",
                _ => "\u{1f7e2} Low",
            };
            table.push(format!(
                "| {} | {level} | {} |",
                area_label(&text_of(r, "area")),
                cell(&defuse(&text_of(r, "note")))
            ));
        }
        out.push(table.join("\n"));
    }
    if findings.is_empty() {
        out.push("**Findings:** none worth raising.".to_string());
    } else {
        let mut lines = vec!["**Findings** (posted as inline notes)".to_string()];
        for f in findings {
            let title = text_of(f, "title");
            let title = if title.is_empty() {
                text_of(f, "body").chars().take(MAX_TITLE).collect()
            } else {
                title
            };
            lines.push(format!(
                "- {} \u{b7} {} \u{2014} `{}:{}` {}",
                severity_badge(&text_of(f, "severity")),
                area_label(&text_of(f, "category")),
                py_str(&f["path"]).replace('`', "'"),
                py_str(&f["line"]),
                cell(&defuse(&title))
            ));
        }
        out.push(lines.join("\n"));
    }
    Some(out.join("\n\n"))
}

/// Returns `{"drafts", "fallback_general"}` (+ `"partial": ClassName` when a
/// failure hit after at least one draft was posted). `Denied` on 401/403, and
/// the original error when nothing was posted.
pub fn post_review(conn: &dyn Connector, mr: &Value, answer: &Value) -> Result<Value, PostError> {
    let (mut drafts, mut fallback) = (0u64, 0u64);
    match post_all(conn, mr, answer, &mut drafts, &mut fallback) {
        Ok(()) => Ok(json!({"drafts": drafts, "fallback_general": fallback})),
        Err(e) => {
            if drafts > 0 {
                return Ok(
                    json!({"drafts": drafts, "fallback_general": fallback, "partial": e.class_name()}),
                );
            }
            if matches!(e.status(), Some(401) | Some(403)) {
                return Err(PostError::Denied(DENIED_MESSAGE.into()));
            }
            Err(PostError::Other(e))
        }
    }
}

fn verdict_label(verdict: &str) -> &'static str {
    match verdict {
        "approve" => "Looks good",
        "request_changes" => "Changes requested",
        _ => "Comments",
    }
}

/// Is this MR a draft? GitLab's `draft` flag (`work_in_progress` before 13.x),
/// or a draft title prefix.
pub fn is_draft(mr: &Value) -> bool {
    let flag = |k: &str| mr.get(k).and_then(Value::as_bool) == Some(true);
    let title = mr
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_start()
        .to_lowercase();
    flag("draft")
        || flag("work_in_progress")
        || ["draft:", "[draft]", "(draft)", "draft -", "wip:", "[wip]"]
            .iter()
            .any(|p| title.starts_with(p))
}

/// One MR regardless of its head SHA: `account:project_id!iid`.
pub fn mr_ref(account_id: &str, project_id: &str, iid: &str) -> String {
    format!("{account_id}:{project_id}!{iid}")
}

fn row_ref(account_id: &str, row: &Value) -> String {
    mr_ref(
        account_id,
        &row.get("project_id").map_or("None".into(), py_str),
        &row.get("iid").map_or("None".into(), py_str),
    )
}

/// The user's per-MR choices, kept beside the loop's `seen.json`
/// (`<state root>/mr-review-loop/mr-prefs.json`): MRs never to review, MRs
/// to review on the next run even when already reviewed, and MRs whose review
/// failed (with why), left alone on every commit until "Review now".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MrPrefs {
    pub skip: BTreeSet<String>,
    pub requested: BTreeSet<String>,
    pub failed: BTreeMap<String, String>,
}

impl MrPrefs {
    pub fn path(state_root: &Path) -> PathBuf {
        state_root.join("mr-review-loop").join(PREFS_FILE)
    }

    /// Missing or malformed reads as empty.
    pub fn load(state_root: &Path) -> Self {
        let data: Value = std::fs::read_to_string(Self::path(state_root))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        let set = |k: &str| {
            data.get(k)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        };
        let failed = data
            .get("failed")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
            .collect();
        Self {
            skip: set("skip"),
            requested: set("requested"),
            failed,
        }
    }

    /// Load, change, and write back atomically.
    pub fn update(state_root: &Path, change: impl FnOnce(&mut Self)) -> std::io::Result<Self> {
        let mut prefs = Self::load(state_root);
        change(&mut prefs);
        let path = Self::path(state_root);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let body =
            json!({"skip": prefs.skip, "requested": prefs.requested, "failed": prefs.failed});
        std::fs::write(&tmp, serde_json::to_string_pretty(&body)? + "\n")?;
        std::fs::rename(&tmp, &path)?;
        Ok(prefs)
    }
}

/// One open MR the user authored or is assigned to (as assignee or reviewer).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MyMr {
    pub account: String,
    pub project_id: String,
    pub iid: String,
    pub title: String,
    pub web_url: String,
    /// `group/project!iid`.
    pub reference: String,
    pub author: String,
    pub author_username: String,
    pub created_at: String,
    pub sha: String,
    pub draft: bool,
    pub authored: bool,
    pub assignee: bool,
    pub reviewer: bool,
}

impl MyMr {
    pub fn mr_ref(&self) -> String {
        mr_ref(&self.account, &self.project_id, &self.iid)
    }

    /// Is it assigned to the user (and not their own)? Only these are reviewed.
    pub fn reviewable(&self) -> bool {
        (self.assignee || self.reviewer) && !self.authored
    }
}

/// Where an MR stands with the review loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewState {
    /// The user's own MR: never reviewed.
    Own,
    Draft,
    /// The user chose to skip it.
    Skipped,
    /// Its review failed; left alone until "Review now".
    Failed,
    /// "Review now" was pressed; the next run reviews it.
    Requested,
    /// Its current head SHA has been reviewed.
    Reviewed,
    /// New, or new commits since the last review: the next run reviews it.
    Pending,
}

pub fn review_state(mr: &MyMr, prefs: &MrPrefs, seen: impl Fn(&str) -> bool) -> ReviewState {
    let r = mr.mr_ref();
    if !mr.reviewable() {
        ReviewState::Own
    } else if mr.draft {
        ReviewState::Draft
    } else if prefs.requested.contains(&r) {
        ReviewState::Requested
    } else if prefs.skip.contains(&r) {
        ReviewState::Skipped
    } else if prefs.failed.contains_key(&r) {
        ReviewState::Failed
    } else if !mr.sha.is_empty() && seen(&item_key(&mr.account, &mr.project_id, &mr.iid, &mr.sha)) {
        ReviewState::Reviewed
    } else {
        ReviewState::Pending
    }
}

/// The MRs of one GitLab account; `error` is a safe class name only.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AccountMrs {
    pub account_id: String,
    pub account_label: String,
    pub mrs: Vec<MyMr>,
    pub error: Option<String>,
}

const MY_MR_ROLES: [&str; 3] = ["author_id", "assignee_id", "reviewer_id"];

/// Is the MR's project still active, i.e. neither archived nor scheduled for
/// deletion? `non_archived=true` already filters the lists on GitLab versions
/// that honour it; this catches the rest. Looked up once per project; a failed
/// lookup counts as active, so an MR is never hidden by a flaky request.
fn project_active(conn: &dyn Connector, cache: &mut BTreeMap<String, bool>, row: &Value) -> bool {
    let pid = row.get("project_id").map_or("None".into(), py_str);
    *cache.entry(pid).or_insert_with_key(|pid| {
        conn.api("GET", &format!("/projects/{pid}"), &ApiOpts::default())
            .map_or(true, |p| {
                let set = |k: &str| p.get(k).is_some_and(py_truthy);
                !set("archived") && !set("marked_for_deletion_at") && !set("marked_for_deletion_on")
            })
    })
}

fn my_mrs(conn: &dyn Connector, account_id: &str) -> Result<Vec<MyMr>, ConnectorError> {
    let me = conn.api("GET", "/user", &ApiOpts::default())?;
    let id = me
        .get("id")
        .map(py_str)
        .ok_or_else(|| ConnectorError::Other {
            class: "KeyError".into(),
            detail: "id".into(),
        })?;
    let mut out: Vec<MyMr> = Vec::new();
    let mut active = BTreeMap::new();
    for role in MY_MR_ROLES {
        let rows = conn.api(
            "GET",
            &format!(
                "/merge_requests?{role}={id}&state=opened&scope=all&non_archived=true&per_page=100"
            ),
            &ApiOpts::default(),
        )?;
        for row in rows.as_array().into_iter().flatten() {
            if !project_active(conn, &mut active, row) {
                continue;
            }
            let s = |k: &str| {
                row.get(k)
                    .filter(|v| py_truthy(v))
                    .map_or(String::new(), py_str)
            };
            let r = row_ref(account_id, row);
            let i = match out.iter().position(|m| m.mr_ref() == r) {
                Some(i) => i,
                None => {
                    let author = row.get("author").cloned().unwrap_or(Value::Null);
                    let iid = s("iid");
                    out.push(MyMr {
                        account: account_id.to_string(),
                        project_id: s("project_id"),
                        reference: row
                            .pointer("/references/full")
                            .filter(|v| py_truthy(v))
                            .map_or_else(|| format!("!{iid}"), py_str),
                        iid,
                        title: s("title"),
                        web_url: s("web_url"),
                        author: author.get("name").map_or(String::new(), py_str),
                        author_username: author.get("username").map_or(String::new(), py_str),
                        created_at: s("created_at"),
                        sha: s("sha"),
                        draft: is_draft(row),
                        ..MyMr::default()
                    });
                    out.len() - 1
                }
            };
            let mr = &mut out[i];
            match role {
                "author_id" => mr.authored = true,
                "assignee_id" => mr.assignee = true,
                _ => mr.reviewer = true,
            }
        }
    }
    // Newest first (ISO 8601 timestamps sort as text).
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

// ---- GitHub (through the provider layer) --------------------------------------

/// Is this account's kind reviewed through `crate::providers` rather than
/// GitLab's own API (draft notes)? Every connected provider is.
fn via_provider(kind: &str) -> bool {
    crate::providers::Provider::from_kind(kind).is_some_and(|p| p.connected())
}

fn provider_err(e: ProviderError) -> ConnectorError {
    match e {
        ProviderError::Connector(c) => c,
        other => ConnectorError::Other {
            class: other.class_name(),
            detail: String::new(),
        },
    }
}

fn repo_binding(account: &str, repo: &str) -> Binding {
    let ep = Endpoint {
        account: account.to_string(),
        path: repo.to_string(),
    };
    Binding {
        code: ep.clone(),
        tracker: ep,
    }
}

/// `(changed new paths, {new_path: old_path} of renamed files)`.
fn changed_paths(changes: &[Value]) -> (BTreeSet<String>, Map<String, Value>) {
    let changed: BTreeSet<String> = changes
        .iter()
        .filter_map(|c| c.get("new_path").filter(|v| py_truthy(v)).map(py_str))
        .collect();
    let mut old_paths = Map::new();
    for c in changes {
        let (old, new) = (c.get("old_path"), c.get("new_path"));
        if let (Some(o), Some(n)) = (old.filter(|v| py_truthy(v)), new.filter(|v| py_truthy(v))) {
            if o != n {
                old_paths.insert(py_str(n), json!(py_str(o)));
            }
        }
    }
    (changed, old_paths)
}

/// The user's open pull requests on a GitHub account, in any repository.
fn hosted_my_mrs(
    source: &dyn ConnectorSource,
    account_id: &str,
) -> Result<Vec<MyMr>, ConnectorError> {
    let host = crate::providers::account_host(source, account_id).map_err(provider_err)?;
    let me = host.me().map_err(provider_err)?;
    let mut out: Vec<MyMr> = Vec::new();
    for filter in [
        CrFilter::Mine,
        CrFilter::Assigned,
        CrFilter::ReviewRequested,
    ] {
        for cr in host
            .change_requests_anywhere(filter, &me, None, 100)
            .map_err(provider_err)?
        {
            let iid = cr.number.to_string();
            let r = mr_ref(account_id, &cr.repo, &iid);
            let i = match out.iter().position(|m| m.mr_ref() == r) {
                Some(i) => i,
                None => {
                    out.push(MyMr {
                        account: account_id.to_string(),
                        project_id: cr.repo.clone(),
                        reference: format!("{}#{}", cr.repo, cr.number),
                        iid,
                        title: cr.title.clone(),
                        web_url: cr.web_url.clone(),
                        author: cr.author.clone(),
                        author_username: cr.author.clone(),
                        created_at: cr.raw.get("created_at").map_or(String::new(), py_str),
                        sha: cr.head_sha.clone(),
                        draft: cr.draft || is_draft(&json!({"title": cr.title})),
                        ..MyMr::default()
                    });
                    out.len() - 1
                }
            };
            let mr = &mut out[i];
            match filter {
                CrFilter::Mine => mr.authored = true,
                CrFilter::Assigned => mr.assignee = true,
                CrFilter::ReviewRequested => mr.reviewer = true,
            }
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

/// The pending review for a provider-hosted pull request: the summary and
/// every finding the provider can place as a line comment, in one review
/// the user submits themselves. Never approves or submits.
pub fn post_review_hosted(
    src: &dyn ConnectorSource,
    mr: &Value,
    answer: &Value,
) -> Result<Value, PostError> {
    let other = |e: ProviderError| PostError::Other(provider_err(e));
    let n = mr["iid"]
        .as_u64()
        .or_else(|| py_str(&mr["iid"]).parse().ok())
        .ok_or_else(|| PostError::Other(ConnectorError::msg("no pull request number")))?;
    let r = crate::providers::resolve(
        src,
        &repo_binding(&py_str(&mr["account"]), &py_str(&mr["project_id"])),
    )
    .map_err(other)?;
    let cr = r.code.change_request(n).map_err(other)?;
    let old_paths = mr.get("old_paths").cloned().unwrap_or(Value::Null);
    let mut notes: Vec<DraftNote> = Vec::new();
    if let Some(summary) = summary_note(answer) {
        notes.push(DraftNote {
            position: None,
            body: summary,
        });
    }
    for f in answer
        .get("findings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let path = py_str(&f["path"]);
        notes.push(DraftNote {
            position: Some(DiffPosition {
                old_path: old_paths.get(&path).map(py_str),
                line: f["line"].as_u64(),
                head_sha: Some(cr.head_sha.clone()),
                path,
                ..DiffPosition::default()
            }),
            body: finding_note(f),
        });
    }
    match r.code.draft_review(&cr, &notes, SIGNATURE) {
        Ok(o) => Ok(json!({"drafts": o.posted, "fallback_general": o.fallback})),
        Err(ProviderError::PendingReviewExists) => Err(PostError::Denied(
            "you already have a pending review on this pull request".into(),
        )),
        Err(e) => {
            let e = provider_err(e);
            if matches!(e.status(), Some(401) | Some(403)) {
                return Err(PostError::Denied(
                    "token needs write access to pull requests to leave a pending review".into(),
                ));
            }
            Err(PostError::Other(e))
        }
    }
}

/// Every GitLab and GitHub account's open MRs that the user authored or is
/// assigned to. An account that fails reports its error and does not stop
/// the others.
pub fn list_my_mrs(source: &dyn ConnectorSource) -> Vec<AccountMrs> {
    source
        .accounts_with_capability(crate::connectors::MERGE_REQUESTS)
        .into_iter()
        .filter(|a| a.kind == "gitlab" || via_provider(&a.kind))
        .map(|account| {
            let listed = if via_provider(&account.kind) {
                hosted_my_mrs(source, &account.id).map_err(|e| e.class_name())
            } else {
                source
                    .load(&account.id)
                    .map_err(|_| "KeyError".to_string())
                    .and_then(|c| my_mrs(c.as_ref(), &account.id).map_err(|e| e.class_name()))
            };
            let (mrs, error) = match listed {
                Ok(m) => (m, None),
                Err(e) => (Vec::new(), Some(e)),
            };
            AccountMrs {
                account_label: if account.label.is_empty() {
                    account.id.clone()
                } else {
                    account.label.clone()
                },
                account_id: account.id,
                mrs,
                error,
            }
        })
        .collect()
}

/// `account:project_id!iid` of an item key `mr:<that>@<sha>`.
fn ref_of_key(key: &str) -> Option<&str> {
    key.strip_prefix("mr:")?.rsplit_once('@').map(|(r, _)| r)
}

/// A failed review is left alone on later runs until "Review now"; one that
/// went through again is forgotten.
fn remember_failures(outcomes: &[Outcome], ctx: &LoopContext) {
    if outcomes.is_empty() {
        return;
    }
    let state_root = ctx
        .state_dir
        .clone()
        .unwrap_or_else(crate::seen_store::default_state_root);
    if let Err(e) = MrPrefs::update(&state_root, |p| {
        for o in outcomes {
            let Some(r) = ref_of_key(&o.item_key) else {
                continue;
            };
            if o.status == Status::Failed {
                p.failed.insert(r.to_string(), o.summary.clone());
            } else {
                p.failed.remove(r);
            }
        }
    }) {
        ctx.log(&format!("mr-review: could not save failed reviews: {e}"));
    }
}

fn sev_rank(name: &str) -> usize {
    SEVERITIES.iter().position(|s| *s == name).unwrap_or(1)
}

pub struct MrReview {
    connectors: Arc<dyn ConnectorSource>,
    poster: Option<Poster>,
    seen_fn: Option<SeenFn>,
    seen: Mutex<Option<SeenStore>>,
}

impl MrReview {
    pub fn new(connectors: Arc<dyn ConnectorSource>) -> Self {
        Self {
            connectors,
            poster: None,
            seen_fn: None,
            seen: Mutex::new(None),
        }
    }

    pub fn with_poster(mut self, poster: Poster) -> Self {
        self.poster = Some(poster);
        self
    }

    pub fn with_seen(mut self, seen: SeenFn) -> Self {
        self.seen_fn = Some(seen);
        self
    }

    /// Read-only here: LoopKit itself marks items seen after acting.
    fn already_seen(&self, key: &str, ctx: &LoopContext) -> bool {
        if let Some(f) = &self.seen_fn {
            return f(key);
        }
        let mut guard = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            let now = ctx.now;
            *guard = SeenStore::open(
                "mr-review-loop",
                ctx.state_dir.as_deref(),
                Some(Box::new(move || now)),
            )
            .ok();
        }
        guard.as_ref().is_some_and(|s| s.has(key))
    }

    fn item(
        &self,
        conn: &dyn Connector,
        account_id: &str,
        me: &Value,
        row: &Value,
    ) -> Result<Option<WorkItem>, ConnectorError> {
        let author_id = row
            .get("author")
            .filter(|v| py_truthy(v))
            .and_then(|a| a.get("id"));
        if author_id == me.get("id") {
            return Ok(None);
        }
        let key_err = || ConnectorError::Other {
            class: "KeyError".into(),
            detail: "project_id/iid".into(),
        };
        let pid = row.get("project_id").ok_or_else(key_err)?;
        let iid = row.get("iid").ok_or_else(key_err)?;
        let (pid_s, iid_s) = (py_str(pid), py_str(iid));
        let detail = conn.api(
            "GET",
            &format!("/projects/{pid_s}/merge_requests/{iid_s}"),
            &ApiOpts::default(),
        )?;
        if is_draft(&detail) {
            return Ok(None);
        }
        let empty = json!({});
        let refs = detail
            .get("diff_refs")
            .filter(|v| py_truthy(v))
            .unwrap_or(&empty);
        let head = detail
            .get("sha")
            .filter(|v| py_truthy(v))
            .or_else(|| refs.get("head_sha"))
            .filter(|v| py_truthy(v));
        let Some(head) = head else { return Ok(None) };
        if !["base_sha", "start_sha", "head_sha"]
            .iter()
            .all(|k| refs.get(*k).is_some_and(py_truthy))
        {
            return Ok(None);
        }
        let mut changes: Vec<Value> = Vec::new();
        let mut size = 0usize;
        for page in 1..=MAX_DIFF_PAGES {
            let batch = conn.api(
                "GET",
                &format!("/projects/{pid_s}/merge_requests/{iid_s}/diffs?per_page=100&page={page}"),
                &ApiOpts::default(),
            )?;
            if !py_truthy(&batch) {
                break;
            }
            let rows = batch.as_array().cloned().unwrap_or_default();
            size += rows
                .iter()
                .map(|c| {
                    c.get("diff")
                        .filter(|v| py_truthy(v))
                        .map_or(0, |d| py_str(d).chars().count())
                })
                .sum::<usize>();
            changes.extend(rows);
            if size > DIFF_CAP_BYTES {
                break;
            }
        }
        let (diff, truncated) = build_diff_text(&changes, DIFF_CAP_BYTES);
        let pick = |k: &str| {
            detail
                .get(k)
                .filter(|v| py_truthy(v))
                .or_else(|| row.get(k).filter(|v| py_truthy(v)))
                .map(py_str)
                .unwrap_or_default()
        };
        let (title, url) = (pick("title"), pick("web_url"));
        let description: String = detail
            .get("description")
            .filter(|v| py_truthy(v))
            .map(py_str)
            .unwrap_or_default()
            .chars()
            .take(MAX_DESCRIPTION)
            .collect();
        let changed: BTreeSet<String> = changes
            .iter()
            .filter_map(|c| c.get("new_path").filter(|v| py_truthy(v)).map(py_str))
            .collect();
        let mut old_paths = Map::new();
        for c in &changes {
            let (old, new) = (c.get("old_path"), c.get("new_path"));
            if let (Some(o), Some(n)) = (old.filter(|v| py_truthy(v)), new.filter(|v| py_truthy(v)))
            {
                if o != n {
                    old_paths.insert(py_str(n), json!(py_str(o)));
                }
            }
        }
        let head_s = py_str(head);
        let reference = detail
            .pointer("/references/full")
            .or_else(|| row.pointer("/references/full"))
            .filter(|v| py_truthy(v))
            .map_or_else(|| format!("!{iid_s}"), py_str);
        let author = detail
            .get("author")
            .filter(|v| py_truthy(v))
            .or_else(|| row.get("author"))
            .cloned()
            .unwrap_or(Value::Null);
        let meta = json!({
            "label": format!("{title} ({reference})"),
            "title": title, "ref": reference,
            "author": author.get("name").map_or(String::new(), py_str),
            "author_username": author.get("username").map_or(String::new(), py_str),
            "created_at": pick("created_at"),
        });
        Ok(Some(
            WorkItem::new(
                item_key(account_id, &pid_s, &iid_s, &head_s),
                title.clone(),
                json!({
                    "account": account_id, "project_id": pid, "iid": iid, "title": title,
                    "description": description, "web_url": url,
                    "diff_refs": {"base_sha": refs["base_sha"], "start_sha": refs["start_sha"], "head_sha": refs["head_sha"]},
                    "diff": diff, "truncated": truncated,
                    "changed_paths": changed.into_iter().collect::<Vec<_>>(),
                    "old_paths": old_paths,
                }),
            )
            .with_url(url)
            .with_meta(meta),
        ))
    }
}

impl MrReview {
    /// The pull requests a provider-hosted (GitHub) account asks the user to
    /// review, as an assignee or a requested reviewer; never their own or a
    /// draft. Same choices (skip, failed, "Review now") as GitLab's.
    fn discover_hosted(
        &self,
        ctx: &LoopContext,
        account_id: &str,
        prefs: &MrPrefs,
        requested: &mut Vec<WorkItem>,
        items: &mut Vec<WorkItem>,
        load_failures: &mut Vec<(String, String)>,
    ) {
        let listed = (|| -> Result<(String, Vec<ChangeRequest>), ProviderError> {
            let host = crate::providers::account_host(self.connectors.as_ref(), account_id)?;
            let me = host.me()?;
            let mut rows: Vec<ChangeRequest> = Vec::new();
            for filter in [CrFilter::ReviewRequested, CrFilter::Assigned] {
                for cr in host.change_requests_anywhere(filter, &me, None, 50)? {
                    if !rows
                        .iter()
                        .any(|r| r.repo == cr.repo && r.number == cr.number)
                    {
                        rows.push(cr);
                    }
                }
            }
            Ok((me, rows))
        })();
        let (me, rows) = match listed {
            Ok(v) => v,
            Err(e) => {
                ctx.log(&format!(
                    "mr-review: account {account_id} failed: {}",
                    e.class_name()
                ));
                return;
            }
        };
        for cr in rows {
            if cr.author == me || cr.draft || is_draft(&json!({"title": cr.title})) {
                continue;
            }
            let r = mr_ref(account_id, &cr.repo, &cr.number.to_string());
            let wanted = prefs.requested.contains(&r);
            if !wanted && (prefs.skip.contains(&r) || prefs.failed.contains_key(&r)) {
                continue;
            }
            match self.hosted_item(ctx, account_id, &cr, wanted) {
                Ok(Some(item)) if wanted => requested.push(item.with_force()),
                Ok(Some(item)) => items.push(item),
                Ok(None) => {}
                Err(e) => {
                    ctx.log(&format!(
                        "mr-review: {} failed to load: {}",
                        r,
                        e.class_name()
                    ));
                    load_failures.push((r, format!("could not load: {}", e.class_name())));
                }
            }
        }
    }

    fn hosted_item(
        &self,
        ctx: &LoopContext,
        account_id: &str,
        listed: &ChangeRequest,
        wanted: bool,
    ) -> Result<Option<WorkItem>, ProviderError> {
        let res = crate::providers::resolve(
            self.connectors.as_ref(),
            &repo_binding(account_id, &listed.repo),
        )?;
        // A search hit carries no head SHA: the detail does.
        let cr = res.code.change_request(listed.number)?;
        if cr.draft || cr.head_sha.is_empty() {
            return Ok(None);
        }
        let iid = cr.number.to_string();
        let key = item_key(account_id, &listed.repo, &iid, &cr.head_sha);
        if !ctx.force && !wanted && self.already_seen(&key, ctx) {
            return Ok(None);
        }
        let changes: Vec<Value> = res
            .code
            .diff(cr.number)?
            .iter()
            .map(|f| {
                json!({"old_path": f.old_path, "new_path": f.new_path, "diff": f.diff,
                       "new_file": f.new_file, "deleted_file": f.deleted_file,
                       "renamed_file": f.renamed_file})
            })
            .collect();
        let (diff, truncated) = build_diff_text(&changes, DIFF_CAP_BYTES);
        let (changed, old_paths) = changed_paths(&changes);
        let description: String = cr.description.chars().take(MAX_DESCRIPTION).collect();
        let reference = format!("{}#{}", listed.repo, cr.number);
        let meta = json!({
            "label": format!("{} ({reference})", cr.title),
            "title": cr.title, "ref": reference,
            "author": cr.author, "author_username": cr.author,
            "created_at": listed.raw.get("created_at").map_or(String::new(), py_str),
        });
        Ok(Some(
            WorkItem::new(
                key,
                cr.title.clone(),
                json!({
                    "provider": res.code.provider().kind(),
                    "account": account_id, "project_id": listed.repo, "iid": cr.number,
                    "title": cr.title, "description": description, "web_url": cr.web_url,
                    "diff_refs": {"head_sha": cr.head_sha},
                    "diff": diff, "truncated": truncated,
                    "changed_paths": changed.into_iter().collect::<Vec<_>>(),
                    "old_paths": old_paths,
                }),
            )
            .with_url(cr.web_url.clone())
            .with_meta(meta),
        ))
    }
}

impl LoopPlugin for MrReview {
    fn loop_name(&self) -> &str {
        "mr-review-loop"
    }
    fn definition_dir(&self) -> &str {
        "mr-review"
    }
    fn max_items_per_run(&self) -> usize {
        8
    }
    fn output_keys(&self) -> &[&'static str] {
        &["summary", "verdict", "overview", "risks", "findings"]
    }

    fn discover(&self, ctx: &LoopContext) -> Result<Vec<WorkItem>, PluginError> {
        let state_root = ctx
            .state_dir
            .clone()
            .unwrap_or_else(crate::seen_store::default_state_root);
        let prefs = MrPrefs::load(&state_root);
        let (mut requested, mut items) = (Vec::new(), Vec::new());
        let mut load_failures: Vec<(String, String)> = Vec::new();
        for account in self
            .connectors
            .accounts_with_capability(crate::connectors::MERGE_REQUESTS)
        {
            if via_provider(&account.kind) {
                self.discover_hosted(
                    ctx,
                    &account.id,
                    &prefs,
                    &mut requested,
                    &mut items,
                    &mut load_failures,
                );
                continue;
            }
            if account.kind != "gitlab" {
                continue;
            }
            let loaded = (|| -> Result<(Box<dyn Connector>, Value, Vec<Value>), ConnectorError> {
                let conn =
                    self.connectors
                        .load(&account.id)
                        .map_err(|_| ConnectorError::Other {
                            class: "KeyError".into(),
                            detail: String::new(),
                        })?;
                let me = conn.api("GET", "/user", &ApiOpts::default())?;
                let id = me.get("id").ok_or_else(|| ConnectorError::Other {
                    class: "KeyError".into(),
                    detail: "id".into(),
                })?;
                // Assigned to me: as a reviewer or as an assignee.
                let mut rows: Vec<Value> = Vec::new();
                let mut listed: BTreeSet<String> = BTreeSet::new();
                for role in ["reviewer_id", "assignee_id"] {
                    let batch = conn.api(
                        "GET",
                        &format!(
                            "/merge_requests?{role}={}&state=opened&scope=all&draft=no&non_archived=true&per_page=50",
                            py_str(id)
                        ),
                        &ApiOpts::default(),
                    )?;
                    for row in batch.as_array().into_iter().flatten() {
                        if listed.insert(row_ref(&account.id, row)) {
                            rows.push(row.clone());
                        }
                    }
                }
                Ok((conn, me, rows))
            })();
            let (conn, me, rows) = match loaded {
                Ok(v) => v,
                Err(e) => {
                    ctx.log(&format!(
                        "mr-review: account {} failed: {}",
                        account.id,
                        e.class_name()
                    ));
                    continue;
                }
            };
            let mut active = BTreeMap::new();
            for row in &rows {
                // Older GitLab ignores `draft=no`; never review a draft.
                if is_draft(row) {
                    continue;
                }
                let r = row_ref(&account.id, row);
                let wanted = prefs.requested.contains(&r);
                // Skipped by the user, or failed before: only "Review now" brings it back.
                if !wanted && (prefs.skip.contains(&r) || prefs.failed.contains_key(&r)) {
                    continue;
                }
                // The list already carries the head SHA: skip a reviewed one
                // before paying for its detail and diff requests.
                if let Some(sha) = row.get("sha").filter(|v| py_truthy(v)) {
                    let key = item_key(
                        &account.id,
                        &row.get("project_id").map_or("None".into(), py_str),
                        &row.get("iid").map_or("None".into(), py_str),
                        &py_str(sha),
                    );
                    if !ctx.force && !wanted && self.already_seen(&key, ctx) {
                        continue;
                    }
                }
                if !project_active(conn.as_ref(), &mut active, row) {
                    continue;
                }
                match self.item(conn.as_ref(), &account.id, &me, row) {
                    Ok(Some(item)) if wanted => requested.push(item.with_force()),
                    Ok(Some(item)) => items.push(item),
                    Ok(None) => {}
                    Err(e) => {
                        ctx.log(&format!(
                            "mr-review: MR !{} failed to load: {}",
                            row.get("iid").map_or("None".into(), py_str),
                            e.class_name()
                        ));
                        load_failures.push((r, format!("could not load: {}", e.class_name())));
                    }
                }
            }
        }
        // A request is answered by this run (or the MR is gone): drop it.
        if !prefs.requested.is_empty() || !load_failures.is_empty() {
            let taken = prefs.requested.clone();
            if let Err(e) = MrPrefs::update(&state_root, |p| {
                p.requested.retain(|r| !taken.contains(r));
                p.failed.extend(load_failures);
            }) {
                ctx.log(&format!("mr-review: could not save requests: {e}"));
            }
        }
        // Requested MRs first, so the per-run cap never drops them.
        requested.extend(items);
        Ok(requested)
    }

    fn after_item(
        &self,
        item: &WorkItem,
        answer: &Value,
        ctx: &LoopContext,
    ) -> Result<Outcome, PluginError> {
        let payload = &item.payload;
        let allowed: BTreeSet<&str> = payload
            .get("changed_paths")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let min = ctx
            .settings
            .get("min_severity")
            .filter(|v| py_truthy(v))
            .map_or("minor".to_string(), py_str);
        let floor = sev_rank(&min);
        let mut kept: Vec<Value> = Vec::new();
        for f in answer
            .get("findings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(obj) = f.as_object() else { continue };
            let line = obj
                .get("line")
                .and_then(|v| if v.is_boolean() { None } else { v.as_i64() });
            let sev = obj.get("severity").and_then(Value::as_str);
            let body = obj.get("body").and_then(Value::as_str);
            let path = obj.get("path").and_then(Value::as_str);
            let (Some(path), Some(line), Some(sev), Some(body)) = (path, line, sev, body) else {
                continue;
            };
            if !allowed.contains(path)
                || line < 1
                || !SEVERITIES.contains(&sev)
                || sev_rank(sev) < floor
                || body.trim().is_empty()
            {
                continue;
            }
            let short = |k: &str, cap: usize| {
                obj.get(k)
                    .and_then(Value::as_str)
                    .map(|t| t.trim().chars().take(cap).collect::<String>())
                    .unwrap_or_default()
            };
            let category = obj
                .get("category")
                .and_then(Value::as_str)
                .filter(|c| AREAS.iter().any(|(k, _)| k == c))
                .unwrap_or("correctness");
            kept.push(json!({
                "path": path, "line": line, "severity": sev, "category": category,
                "title": short("title", MAX_TITLE),
                "body": body.trim().chars().take(MAX_BODY).collect::<String>(),
                "suggestion": short("suggestion", MAX_BODY),
            }));
        }
        // Most severe first, so the cap keeps what matters.
        kept.sort_by_key(|f| std::cmp::Reverse(sev_rank(f["severity"].as_str().unwrap_or(""))));
        kept.truncate(MAX_FINDINGS);
        let summary = answer
            .get("summary")
            .and_then(Value::as_str)
            .map(|s| s.trim().chars().take(MAX_SUMMARY).collect::<String>())
            .unwrap_or_default();
        let verdict = answer
            .get("verdict")
            .and_then(Value::as_str)
            .filter(|v| VERDICTS.contains(v))
            .unwrap_or("comment");
        let overview: Vec<String> = answer
            .get("overview")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|b| b.trim().chars().take(MAX_NOTE).collect::<String>())
            .filter(|b| !b.is_empty())
            .take(MAX_OVERVIEW)
            .collect();
        let mut risks: Vec<Value> = answer
            .get("risks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| {
                let area = r.get("area")?.as_str()?;
                let level = r.get("level")?.as_str()?;
                let note = r.get("note")?.as_str()?.trim();
                (AREAS.iter().any(|(k, _)| *k == area) && LEVELS.contains(&level) && !note.is_empty())
                    .then(|| json!({"area": area, "level": level, "note": note.chars().take(MAX_NOTE).collect::<String>()}))
            })
            .collect();
        risks.sort_by_key(|r| {
            std::cmp::Reverse(LEVELS.iter().position(|l| r["level"] == *l).unwrap_or(0))
        });
        risks.truncate(MAX_RISKS);
        let cleaned = json!({"summary": summary, "verdict": verdict, "overview": overview,
                             "risks": risks, "findings": kept});
        let account = py_str(&payload["account"]);
        let conn = self
            .connectors
            .load(&account)
            .map_err(|_| PluginError::new("KeyError", account.clone()))?;
        let url = payload.get("web_url").map_or(String::new(), py_str);
        let posted = match &self.poster {
            Some(p) => p(conn.as_ref(), payload, &cleaned),
            None if payload
                .get("provider")
                .and_then(Value::as_str)
                .is_some_and(via_provider) =>
            {
                post_review_hosted(self.connectors.as_ref(), payload, &cleaned)
            }
            None => post_review(conn.as_ref(), payload, &cleaned),
        };
        let result = match posted {
            Ok(r) => r,
            // "skipped" is marked seen for this MR SHA, so a read-only token
            // does not pay for the same model call on every run.
            Err(PostError::Denied(m)) => {
                return Ok(Outcome::new(&item.key, Status::Skipped, m).with_url(url))
            }
            Err(e) => return Err(e.into_plugin()),
        };
        let blockers = kept.iter().filter(|f| f["severity"] == "blocker").count();
        let drafts = result.get("drafts").and_then(Value::as_u64).unwrap_or(0);
        let summary_text = match result.get("partial").filter(|v| py_truthy(v)) {
            Some(p) => format!("{drafts} draft notes (partial: {})", py_str(p)),
            None => format!(
                "{}: {} findings ({blockers} blocker), {} risks",
                verdict_label(verdict),
                kept.len(),
                risks.len()
            ),
        };
        Ok(Outcome::new(&item.key, Status::Done, summary_text)
            .with_url(url)
            .with_data(
                json!({"title": item.title, "drafts": drafts, "blockers": blockers,
                              "verdict": verdict}),
            ))
    }

    fn digest(&self, outcomes: &[Outcome], ctx: &LoopContext) -> Option<String> {
        remember_failures(outcomes, ctx);
        let done: Vec<&Outcome> = outcomes
            .iter()
            .filter(|o| o.status == Status::Done)
            .collect();
        if done.is_empty() {
            return None;
        }
        let sum = |k: &str| {
            done.iter()
                .map(|o| o.data.get(k).and_then(Value::as_u64).unwrap_or(0))
                .sum::<u64>()
        };
        let mut lines = vec![format!(
            "MR pre-review: {} MRs, {} draft notes ({} blockers). Open each MR \u{2192} Review \u{2192} Submit to publish.",
            done.len(),
            sum("drafts"),
            sum("blockers")
        )];
        for o in done {
            let title = o
                .data
                .get("title")
                .filter(|v| py_truthy(v))
                .map_or(o.item_key.clone(), py_str);
            lines.push(format!("- {}", loopkit::chat_link(&title, &o.url)));
        }
        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::http::HttpError;
    use crate::connectors::{gitlab, Account, LoadFailure, TypeInfo};
    use crate::loop_definition::LoopDefinition;
    use crate::loopkit::LoopContext;
    use chrono::{TimeZone, Utc};
    use std::path::{Path, PathBuf};

    type Handler =
        Box<dyn Fn(&str, &str, Option<&Value>) -> Result<Value, ConnectorError> + Send + Sync>;

    struct Conn {
        account: Account,
        /// `(method, path, json_body, max_attempts)`
        calls: Mutex<Vec<(String, String, Option<Value>, u32)>>,
        handler: Handler,
    }

    impl Conn {
        fn new(
            h: impl Fn(&str, &str, Option<&Value>) -> Result<Value, ConnectorError>
                + Send
                + Sync
                + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                account: Account::new("work", "gitlab"),
                calls: Mutex::new(vec![]),
                handler: Box::new(h),
            })
        }
        fn calls(&self) -> Vec<(String, String, Option<Value>, u32)> {
            self.calls.lock().unwrap().clone()
        }
        fn posts(&self) -> Vec<Value> {
            self.calls()
                .into_iter()
                .filter(|c| c.0 == "POST")
                .filter_map(|c| c.2)
                .collect()
        }
    }

    struct Shared(Arc<Conn>);

    impl Connector for Shared {
        fn info(&self) -> &'static TypeInfo {
            &gitlab::INFO
        }
        fn account(&self) -> &Account {
            &self.0.account
        }
        fn api(&self, method: &str, path: &str, opts: &ApiOpts) -> Result<Value, ConnectorError> {
            self.0.calls.lock().unwrap().push((
                method.into(),
                path.into(),
                opts.json_body.clone(),
                opts.max_attempts,
            ));
            (self.0.handler)(method, path, opts.json_body.as_ref())
        }
    }

    fn http_err(status: Option<u16>) -> ConnectorError {
        ConnectorError::Http(HttpError::new(status, "x", "u"))
    }

    fn ok_conn() -> Arc<Conn> {
        Conn::new(|_, _, _| Ok(json!({})))
    }

    struct Source {
        accounts: Vec<Account>,
        conn: Arc<Conn>,
    }

    impl ConnectorSource for Source {
        fn accounts_with_capability(&self, _c: &str) -> Vec<Account> {
            self.accounts.clone()
        }
        fn load(&self, id: &str) -> Result<Box<dyn Connector>, LoadFailure> {
            if id == "ghost" {
                return Err(LoadFailure::Unknown);
            }
            Ok(Box::new(Shared(self.conn.clone())))
        }
    }

    fn plugin_with(conn: Arc<Conn>) -> MrReview {
        MrReview::new(Arc::new(Source {
            accounts: vec![Account::new("work", "gitlab")],
            conn,
        }))
    }

    fn repo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn ctx(settings: Value, force: bool, logs: Arc<Mutex<Vec<String>>>) -> LoopContext {
        let def = LoopDefinition::from_yaml(&repo().join("loops/mr-review/loop.yaml")).unwrap();
        LoopContext {
            loop_name: "mr-review-loop".into(),
            run_id: "r".into(),
            now: Utc.with_ymd_and_hms(2026, 10, 6, 10, 0, 0).unwrap(),
            definition: Arc::new(def),
            settings,
            history_dir: PathBuf::from("/nonexistent"),
            log: Arc::new(move |m| logs.lock().unwrap().push(m.to_string())),
            repo_root: repo(),
            force,
            call_budget_usd: None,
            model: None,
            state_dir: Some(PathBuf::from("/nonexistent")),
        }
    }

    fn c() -> LoopContext {
        ctx(json!({"min_severity": "minor"}), false, Default::default())
    }

    fn diff() -> Vec<Value> {
        vec![
            json!({"new_path": "app/a.rb", "diff": "@@ -1,2 +1,3 @@\n a\n+b = eval(params[:x])\n c\n"}),
        ]
    }

    fn mr() -> Value {
        json!({"project_id": 9, "iid": 7, "web_url": "https://gl/g/p/-/merge_requests/7",
               "diff_refs": {"base_sha": "b", "start_sha": "s", "head_sha": "h"}})
    }

    fn finding(path: &str, line: i64, sev: &str, body: &str) -> Value {
        json!({"path": path, "line": line, "severity": sev, "body": body})
    }

    fn f3() -> Vec<Value> {
        (0..3)
            .map(|i| finding("app/a.rb", 2, "major", &i.to_string()))
            .collect()
    }

    #[test]
    fn new_head_sha_is_new_item() {
        assert_ne!(
            item_key("work", "9", "7", "aaa"),
            item_key("work", "9", "7", "bbb")
        );
        assert_eq!(item_key("work", "9", "7", "h"), "mr:work:9!7@h");
    }

    #[test]
    fn diff_over_cap_is_truncated_with_marker() {
        let big = vec![json!({"new_path": "x", "diff": format!("+{}", "y".repeat(200_000))})];
        let (text, truncated) = build_diff_text(&big, 1000);
        assert!(truncated && text.chars().count() <= 1200);
        assert!(text.contains("[diff truncated") && text.contains("characters"));
    }

    #[test]
    fn small_diff_not_truncated() {
        let (text, truncated) = build_diff_text(&diff(), DIFF_CAP_BYTES);
        assert!(!truncated && text.contains("eval(params"));
    }

    #[test]
    fn findings_only_posted_as_draft_notes() {
        let conn = ok_conn();
        let answer = json!({"summary": "s", "findings": [finding("app/a.rb", 2, "blocker", "eval on params")]});
        post_review(&Shared(conn.clone()), &mr(), &answer).unwrap();
        let calls = conn.calls();
        for (m, p, _, _) in &calls {
            assert!(
                p.split('?').next().unwrap().ends_with("/draft_notes"),
                "{p}"
            );
            assert!(!p.contains("bulk_publish") && !p.contains("approve"));
            assert!(m == "GET" || m == "POST");
        }
        let first = &conn.posts()[0];
        assert_eq!(
            first["position"],
            json!({"position_type": "text", "base_sha": "b", "start_sha": "s", "head_sha": "h",
                   "old_path": "app/a.rb", "new_path": "app/a.rb", "new_line": 2})
        );
        assert!(first["note"]
            .as_str()
            .unwrap()
            .starts_with("\u{1f534} **Blocker** \u{b7} Correctness\n\neval on params"));
    }

    #[test]
    fn position_rejected_falls_back_to_general_draft() {
        let conn = Conn::new(|m, _, body| {
            if m == "POST" && body.is_some_and(|b| b.get("position").is_some()) {
                return Err(http_err(Some(400)));
            }
            Ok(json!({}))
        });
        let answer = json!({"summary": "s", "findings": [finding("app/a.rb", 2, "major", "x")]});
        let out = post_review(&Shared(conn.clone()), &mr(), &answer).unwrap();
        assert_eq!(out["fallback_general"], 1);
        let posts = conn.posts();
        assert!(posts[1]["note"]
            .as_str()
            .unwrap()
            .starts_with("`app/a.rb:2`"));
        assert!(posts[1].get("position").is_none());
    }

    #[test]
    fn other_http_errors_propagate() {
        let conn = Conn::new(|m, _, _| {
            if m == "POST" {
                Err(http_err(Some(500)))
            } else {
                Ok(json!([]))
            }
        });
        let answer = json!({"summary": "s", "findings": [finding("a", 1, "major", "x")]});
        let err = post_review(&Shared(conn), &mr(), &answer).unwrap_err();
        assert!(matches!(err, PostError::Other(ConnectorError::Http(_))));
    }

    #[test]
    fn mentions_defused_in_posted_text() {
        let conn = ok_conn();
        let answer = json!({"summary": "cc @all",
            "findings": [finding("app/a.rb", 2, "major", "ping @group/team and a@b.com")]});
        post_review(&Shared(conn.clone()), &mr(), &answer).unwrap();
        let notes: Vec<String> = conn
            .posts()
            .iter()
            .map(|b| b["note"].as_str().unwrap().to_string())
            .collect();
        assert!(notes[1].contains("@\u{200b}all") && notes[0].contains("@\u{200b}group"));
        assert!(notes[0].contains("a@b.com"));
        assert!(!notes
            .iter()
            .any(|n| n.contains(" @all") || n.contains(" @group")));
    }

    fn fake2(existing: Value, fail_on: Option<u32>, status: u16) -> Arc<Conn> {
        let posts = Mutex::new(0u32);
        Conn::new(move |m, _, _| {
            if m == "GET" {
                return Ok(existing.clone());
            }
            if m == "POST" {
                let mut p = posts.lock().unwrap();
                *p += 1;
                if fail_on.is_some_and(|n| *p >= n) {
                    return Err(http_err(Some(status)));
                }
            }
            Ok(json!({}))
        })
    }

    const SIG_NOTE: &str = "x\n\n_\u{2014} LoopX pre-review_";

    #[test]
    fn old_loopx_drafts_deleted_foreign_untouched() {
        let conn = fake2(
            json!([{"id": 11, "note": SIG_NOTE}, {"id": 12, "note": "my own draft"}]),
            None,
            502,
        );
        post_review(
            &Shared(conn.clone()),
            &mr(),
            &json!({"summary": "s", "findings": [f3()[0].clone()]}),
        )
        .unwrap();
        let calls = conn.calls();
        let deletes: Vec<_> = calls
            .iter()
            .filter(|c| c.0 == "DELETE")
            .map(|c| c.1.clone())
            .collect();
        assert_eq!(deletes, ["/projects/9/merge_requests/7/draft_notes/11"]);
        assert_eq!(calls[0].0, "GET");
        let del = calls.iter().position(|c| c.0 == "DELETE").unwrap();
        assert!(del < calls.iter().position(|c| c.0 == "POST").unwrap());
    }

    #[test]
    fn draft_writes_never_retry() {
        let conn = fake2(
            json!([{"id": 11, "note": "_\u{2014} LoopX pre-review_"}]),
            None,
            502,
        );
        post_review(
            &Shared(conn.clone()),
            &mr(),
            &json!({"summary": "s", "findings": [f3()[0].clone()]}),
        )
        .unwrap();
        let writes: Vec<_> = conn
            .calls()
            .into_iter()
            .filter(|c| c.0 == "POST" || c.0 == "DELETE")
            .collect();
        assert!(!writes.is_empty() && writes.iter().all(|c| c.3 == 1));
    }

    fn item() -> WorkItem {
        let mut payload = mr();
        payload["account"] = json!("work");
        payload["diff"] = json!(build_diff_text(&diff(), DIFF_CAP_BYTES).0);
        payload["changed_paths"] = json!(["app/a.rb"]);
        WorkItem::new("k", "t", payload)
    }

    #[test]
    fn partial_failure_is_done_and_seen() {
        let conn = fake2(json!([]), Some(2), 502);
        let out = plugin_with(conn)
            .after_item(&item(), &json!({"summary": "s", "findings": f3()}), &c())
            .unwrap();
        assert_eq!(out.status, Status::Done);
        assert_eq!(out.summary, "1 draft notes (partial: MailHTTPError)");
    }

    #[test]
    fn failure_with_nothing_posted_raises() {
        let err = post_review(
            &Shared(fake2(json!([]), Some(1), 502)),
            &mr(),
            &json!({"summary": "s", "findings": f3()}),
        );
        assert!(matches!(
            err,
            Err(PostError::Other(ConnectorError::Http(_)))
        ));
    }

    #[test]
    fn read_only_token_skips_item_so_it_is_seen() {
        let conn = fake2(json!([]), Some(1), 403);
        let out = plugin_with(conn)
            .after_item(&item(), &json!({"summary": "s", "findings": f3()}), &c())
            .unwrap();
        assert_eq!(out.status, Status::Skipped);
        assert_eq!(
            out.summary,
            "token needs the api scope to write draft notes"
        );
    }

    #[test]
    fn images_neutralised_in_body() {
        let conn = ok_conn();
        let answer =
            json!({"summary": "", "findings": [finding("a", 1, "major", "![x](http://t/p.png)")]});
        post_review(&Shared(conn.clone()), &mr(), &answer).unwrap();
        assert!(!conn.posts()[0]["note"].as_str().unwrap().contains("!["));
    }

    fn with_poster(posted: Arc<Mutex<Vec<Value>>>) -> MrReview {
        plugin_with(ok_conn()).with_poster(Box::new(move |_c, _m, a| {
            posted.lock().unwrap().push(a.clone());
            Ok(json!({"drafts": a["findings"].as_array().unwrap().len(), "fallback_general": 0}))
        }))
    }

    #[test]
    fn after_item_filters_severity_unknown_paths_and_caps() {
        let mut findings = vec![
            finding("app/a.rb", 2, "nit", "n"),
            finding("ghost.rb", 1, "blocker", "g"),
        ];
        findings.extend((0..30).map(|i| finding("app/a.rb", 2, "major", &i.to_string())));
        let posted = Arc::new(Mutex::new(vec![]));
        with_poster(posted.clone())
            .after_item(
                &item(),
                &json!({"summary": "s", "findings": findings}),
                &c(),
            )
            .unwrap();
        let p = posted.lock().unwrap();
        let kept = p[0]["findings"].as_array().unwrap();
        assert_eq!(kept.len(), MAX_FINDINGS);
        assert!(kept
            .iter()
            .all(|f| f["path"] == "app/a.rb" && f["severity"] != "nit"));
    }

    #[test]
    fn after_item_drops_malformed_and_trims() {
        let findings = json!([
            {"path": "app/a.rb", "line": "2", "severity": "major", "body": "str line"},
            {"path": "app/a.rb", "line": true, "severity": "major", "body": "bool line"},
            {"path": "app/a.rb", "line": 2, "severity": "catastrophic", "body": "bad sev"},
            {"path": "app/a.rb", "line": 2, "severity": "major", "body": 5},
            "junk",
            {"path": "app/a.rb", "line": 2, "severity": "major", "body": "z".repeat(5000)}
        ]);
        let posted = Arc::new(Mutex::new(vec![]));
        with_poster(posted.clone())
            .after_item(
                &item(),
                &json!({"summary": "s".repeat(900), "findings": findings}),
                &c(),
            )
            .unwrap();
        let p = posted.lock().unwrap();
        let kept = p[0]["findings"].as_array().unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0]["body"].as_str().unwrap().len(), 2000);
        assert!(p[0]["summary"].as_str().unwrap().len() <= 600);
    }

    #[test]
    fn after_item_outcome_counts_blockers() {
        let posted = Arc::new(Mutex::new(vec![]));
        let out = with_poster(posted)
            .after_item(
                &item(),
                &json!({"summary": "s", "findings": [finding("app/a.rb", 2, "blocker", "b")]}),
                &c(),
            )
            .unwrap();
        assert_eq!(out.status, Status::Done);
        assert_eq!(out.summary, "Comments: 1 findings (1 blocker), 0 risks");
        assert_eq!(out.data["verdict"], "comment");
        assert_eq!(out.url, "https://gl/g/p/-/merge_requests/7");
    }

    #[test]
    fn digest_text_is_chat_safe() {
        let o = Outcome::new("k", Status::Done, "x")
            .with_url("https://gl/mr/1")
            .with_data(json!({"title": "T <!channel>", "drafts": 2, "blockers": 1}));
        let p = plugin_with(ok_conn());
        let text = p.digest(&[o], &c()).unwrap();
        assert!(text.starts_with("MR pre-review: 1 MRs, 2 draft notes (1 blockers). Open each MR"));
        assert!(!text.contains('<') && text.contains("(https://gl/mr/1)"));
        assert!(p.digest(&[], &c()).is_none());
    }

    fn d_handler(m: &str, path: &str, extra: Option<Value>) -> Result<Value, ConnectorError> {
        let _ = m;
        if path == "/user" {
            return Ok(json!({"id": 1, "username": "me"}));
        }
        if path.starts_with("/merge_requests?") {
            return Ok(extra.unwrap_or_else(|| {
                json!([
                    {"project_id": 9, "iid": 7, "author": {"id": 2}, "title": "T", "web_url": "u"},
                    {"project_id": 9, "iid": 8, "author": {"id": 1}, "title": "mine", "web_url": "u"}
                ])
            }));
        }
        if path == "/projects/9" {
            return Ok(json!({"id": 9, "archived": false}));
        }
        if path.ends_with("/merge_requests/7") {
            return Ok(
                json!({"sha": "h", "diff_refs": mr()["diff_refs"], "description": "d".repeat(9000)}),
            );
        }
        if path.contains("/diffs") {
            return Ok(if path.contains("page=1") {
                json!(diff())
            } else {
                json!([])
            });
        }
        panic!("unexpected {path}")
    }

    fn d_conn() -> Arc<Conn> {
        Conn::new(|m, p, _| d_handler(m, p, None))
    }

    #[test]
    fn discover_skips_own_mrs_and_builds_keys() {
        let items = plugin_with(d_conn()).discover(&c()).unwrap();
        assert_eq!(
            items.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
            ["mr:work:9!7@h"]
        );
        let p = &items[0].payload;
        assert_eq!(p["changed_paths"], json!(["app/a.rb"]));
        assert!(p["description"].as_str().unwrap().chars().count() <= 4096);
        assert_eq!(p["truncated"], false);
    }

    #[test]
    fn discover_isolates_accounts_and_mrs() {
        struct Multi;
        impl ConnectorSource for Multi {
            fn accounts_with_capability(&self, _c: &str) -> Vec<Account> {
                vec![
                    Account::new("bad", "gitlab"),
                    Account::new("gh", "github"),
                    Account::new("work", "gitlab"),
                ]
            }
            fn load(&self, id: &str) -> Result<Box<dyn Connector>, LoadFailure> {
                let conn = match id {
                    "bad" => Conn::new(|_, _, _| {
                        Err(ConnectorError::Other {
                            class: "RuntimeError".into(),
                            detail: "secret https://x/token".into(),
                        })
                    }),
                    "gh" => gh_conn(),
                    _ => Conn::new(|m, p, _| d_handler(m, p, None)),
                };
                Ok(Box::new(Shared(conn)))
            }
        }
        let logs: Arc<Mutex<Vec<String>>> = Default::default();
        let ctx = ctx(json!({}), false, logs.clone());
        let items = MrReview::new(Arc::new(Multi)).discover(&ctx).unwrap();
        assert_eq!(
            items.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
            ["mr:gh:o/r!1@h1", "mr:work:9!7@h"]
        );
        let logs = logs.lock().unwrap();
        assert!(
            !logs.is_empty()
                && logs
                    .iter()
                    .all(|l| !l.contains("secret") && !l.contains("token"))
        );
    }

    // ---- GitHub accounts ------------------------------------------------------

    fn gh_hit(n: u64, author: &str, draft: bool) -> Value {
        json!({"number": n, "title": format!("PR {n}"), "user": {"login": author},
               "html_url": format!("https://github.com/o/r/pull/{n}"), "draft": draft,
               "repository_url": "https://api.github.com/repos/o/r", "state": "open",
               "pull_request": {}, "created_at": format!("2026-10-0{n}T00:00:00Z")})
    }

    fn gh_pr(n: u64, author: &str) -> Value {
        json!({"number": n, "title": format!("PR {n}"), "body": "why", "user": {"login": author},
               "html_url": format!("https://github.com/o/r/pull/{n}"), "draft": false, "state": "open",
               "head": {"ref": "feat", "sha": format!("h{n}")},
               "base": {"ref": "main", "repo": {"full_name": "o/r"}}})
    }

    /// A fake GitHub API: #1 by `x` assigned to and review-requested from
    /// `me`, #2 by `me`, #3 a draft by `x`.
    fn gh_handler(m: &str, p: &str, _b: Option<&Value>) -> Result<Value, ConnectorError> {
        let search = p.starts_with("/search/issues");
        match (m, p) {
            ("GET", "/user") => Ok(json!({"login": "me"})),
            ("GET", p) if search && p.contains("author%3Ame") => {
                Ok(json!({"items": [gh_hit(2, "me", false)]}))
            }
            ("GET", p) if search && p.contains("assignee%3Ame") => {
                Ok(json!({"items": [gh_hit(1, "x", false), gh_hit(3, "x", true)]}))
            }
            ("GET", _) if search => Ok(json!({"items": [gh_hit(1, "x", false)]})),
            ("GET", "/repos/o/r/pulls/1") => Ok(gh_pr(1, "x")),
            ("GET", p) if p.starts_with("/repos/o/r/pulls/1/files") => Ok(json!([
                {"filename": "app/a.rb", "status": "modified",
                 "patch": "@@ -1,2 +1,3 @@\n a\n+b = eval(params[:x])\n c\n"},
                {"filename": "app/new.rb", "previous_filename": "app/old.rb", "status": "renamed", "patch": ""}
            ])),
            ("GET", p) if p.starts_with("/repos/o/r/pulls/1/reviews") => Ok(json!([])),
            ("POST", "/repos/o/r/pulls/1/reviews") => Ok(json!({"id": 5})),
            _ => Err(ConnectorError::Other {
                class: "NotFound".into(),
                detail: p.into(),
            }),
        }
    }

    fn gh_conn() -> Arc<Conn> {
        Arc::new(Conn {
            account: Account::new("gh", "github").with_setting("api_url", "https://api.github.com"),
            calls: Mutex::new(vec![]),
            handler: Box::new(gh_handler),
        })
    }

    fn gh_plugin(conn: Arc<Conn>) -> MrReview {
        MrReview::new(Arc::new(Source {
            accounts: vec![Account::new("gh", "github")],
            conn,
        }))
    }

    #[test]
    fn github_pull_requests_are_listed_with_their_roles() {
        let accounts = list_my_mrs(&Source {
            accounts: vec![Account::new("gh", "github")],
            conn: gh_conn(),
        });
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].error, None);
        let mrs = &accounts[0].mrs;
        let by_iid = |n: &str| mrs.iter().find(|m| m.iid == n).unwrap();
        let one = by_iid("1");
        assert_eq!(
            (one.assignee, one.reviewer, one.authored),
            (true, true, false)
        );
        assert_eq!(one.reference, "o/r#1");
        assert_eq!(one.project_id, "o/r");
        assert_eq!(one.web_url, "https://github.com/o/r/pull/1");
        assert!(one.reviewable());
        assert!(by_iid("2").authored);
        assert!(by_iid("3").draft);
    }

    #[test]
    fn discover_reviews_assigned_github_pull_requests_but_not_own_or_drafts() {
        let items = gh_plugin(gh_conn()).discover(&c()).unwrap();
        assert_eq!(
            items.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
            ["mr:gh:o/r!1@h1"]
        );
        let p = &items[0].payload;
        assert_eq!(p["provider"], "github");
        assert_eq!(p["diff_refs"]["head_sha"], "h1");
        assert_eq!(p["changed_paths"], json!(["app/a.rb", "app/new.rb"]));
        assert_eq!(p["old_paths"], json!({"app/new.rb": "app/old.rb"}));
        assert!(p["diff"].as_str().unwrap().contains("eval(params[:x])"));
        assert_eq!(p["web_url"], "https://github.com/o/r/pull/1");
    }

    #[test]
    fn a_github_review_is_one_pending_review_with_line_comments() {
        let conn = gh_conn();
        let plugin = gh_plugin(conn.clone());
        let item = plugin.discover(&c()).unwrap().remove(0);
        let answer = json!({"summary": "Risky.", "verdict": "request_changes",
                            "findings": [finding("app/a.rb", 2, "major", "eval of user input")]});
        let out = plugin.after_item(&item, &answer, &c()).unwrap();
        assert_eq!(out.status, Status::Done);
        let posts = conn.posts();
        assert_eq!(posts.len(), 1, "{posts:?}");
        let review = &posts[0];
        assert_eq!(review["commit_id"], "h1");
        assert!(review.get("event").is_none(), "never submitted: {review}");
        assert_eq!(review["comments"][0]["path"], "app/a.rb");
        assert_eq!(review["comments"][0]["line"], 2);
        assert!(review["comments"][0]["body"]
            .as_str()
            .unwrap()
            .contains("eval of user input"));
        let body = review["body"].as_str().unwrap();
        assert!(body.contains("Risky."), "{body}");
        assert!(body.trim_end().ends_with(SIGNATURE.trim()), "{body}");
    }

    #[test]
    fn definition_and_template() {
        let d = LoopDefinition::from_yaml(&repo().join("loops/mr-review/loop.yaml")).unwrap();
        assert!(d.actions.iter().any(|a| a == "create_draft_review_note"));
        assert!(crate::policy::PolicyEngine::new()
            .validate_definition(&d.actions, &d.human_gates)
            .is_empty());
        let tpl: Value = serde_json::from_str(
            &std::fs::read_to_string(repo().join("config/loops.json.template")).unwrap(),
        )
        .unwrap();
        let e = tpl
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == "mr-review-loop")
            .unwrap();
        assert_eq!(e["requires"], json!(["merge_requests"]));
        assert_eq!(e["routes_notifications"], true);
        assert_eq!(
            e["schedule"],
            json!({"frequency": "hourly", "interval_hours": 2})
        );
        assert_eq!(e["entry_point"], "bin.loop_plugins.mr_review");
        assert_eq!(e["enabled"], false);
        let prompt = std::fs::read_to_string(repo().join("loops/mr-review/prompt.md")).unwrap();
        assert!(prompt.contains("untrusted") && prompt.contains("{{item_json}}"));
        for key in MrReview::new(Arc::new(Source {
            accounts: vec![],
            conn: ok_conn(),
        }))
        .output_keys()
        {
            assert!(prompt.contains(&format!("\"{key}\"")), "{key}");
        }
        for (area, _) in AREAS {
            assert!(prompt.contains(area), "{area}");
        }
    }

    #[test]
    fn foreign_draft_quoting_signature_midtext_not_deleted() {
        let quoted = "I disagree with \u{201c}_\u{2014} LoopX pre-review_\u{201d} being added here, thoughts?";
        let conn = fake2(
            json!([{"id": 11, "note": "x\n\n_\u{2014} LoopX pre-review_\n"}, {"id": 12, "note": quoted}]),
            None,
            502,
        );
        post_review(
            &Shared(conn.clone()),
            &mr(),
            &json!({"summary": "s", "findings": [f3()[0].clone()]}),
        )
        .unwrap();
        let deletes: Vec<_> = conn
            .calls()
            .into_iter()
            .filter(|c| c.0 == "DELETE")
            .map(|c| c.1)
            .collect();
        assert_eq!(deletes, ["/projects/9/merge_requests/7/draft_notes/11"]);
    }

    fn seen_d() -> Arc<Conn> {
        Conn::new(|m, p, _| {
            if p.starts_with("/merge_requests?") {
                return d_handler(
                    m,
                    p,
                    Some(
                        json!([{"project_id": 9, "iid": 7, "sha": "h", "author": {"id": 2}, "title": "T", "web_url": "u"}]),
                    ),
                );
            }
            d_handler(m, p, None)
        })
    }

    #[test]
    fn discover_skips_an_already_reviewed_sha_before_fetching_anything() {
        let conn = seen_d();
        let plugin = plugin_with(conn.clone()).with_seen(Box::new(|k| k == "mr:work:9!7@h"));
        assert!(plugin.discover(&c()).unwrap().is_empty());
        assert!(!conn
            .calls()
            .iter()
            .any(|c| c.1.contains("/diffs") || c.1.ends_with("/merge_requests/7")));
    }

    #[test]
    fn discover_force_reviews_a_seen_sha_again() {
        let plugin = plugin_with(seen_d()).with_seen(Box::new(|k| k == "mr:work:9!7@h"));
        let ctx = ctx(json!({}), true, Default::default());
        let keys: Vec<_> = plugin
            .discover(&ctx)
            .unwrap()
            .into_iter()
            .map(|i| i.key)
            .collect();
        assert_eq!(keys, ["mr:work:9!7@h"]);
    }

    #[test]
    fn old_drafts_are_found_beyond_the_first_page() {
        let conn = Conn::new(|m, p, _| {
            if m == "GET" {
                let page: u32 = p.split("&page=").nth(1).unwrap().parse().unwrap();
                return Ok(match page {
                    1 => json!((0..100)
                        .map(|n| json!({"id": n, "note": "foreign"}))
                        .collect::<Vec<_>>()),
                    2 => json!([{"id": 500, "note": SIG_NOTE}]),
                    _ => json!([]),
                });
            }
            Ok(json!({}))
        });
        post_review(
            &Shared(conn.clone()),
            &mr(),
            &json!({"summary": "", "findings": []}),
        )
        .unwrap();
        let deletes: Vec<_> = conn
            .calls()
            .into_iter()
            .filter(|c| c.0 == "DELETE")
            .map(|c| c.1)
            .collect();
        assert_eq!(deletes, ["/projects/9/merge_requests/7/draft_notes/500"]);
    }

    #[test]
    fn positions_carry_old_line_for_context_lines_and_old_path_for_renames() {
        let mut payload = mr();
        payload["diff"] = json!("--- a/new.rb\n+++ b/new.rb\n@@ -1,2 +1,3 @@\n a\n+b\n c\n");
        payload["old_paths"] = json!({"new.rb": "old.rb"});
        let conn = ok_conn();
        let findings = json!([
            finding("new.rb", 3, "major", "context"),
            finding("new.rb", 2, "major", "added")
        ]);
        post_review(
            &Shared(conn.clone()),
            &payload,
            &json!({"summary": "", "findings": findings}),
        )
        .unwrap();
        let posts = conn.posts();
        let (context, added) = (&posts[0]["position"], &posts[1]["position"]);
        assert_eq!(
            (&context["old_path"], &context["new_path"]),
            (&json!("old.rb"), &json!("new.rb"))
        );
        assert_eq!(
            (&context["old_line"], &context["new_line"]),
            (&json!(2), &json!(3))
        );
        assert!(added.get("old_line").is_none() && added["new_line"] == 2);
    }

    #[test]
    fn discover_records_old_paths_of_renamed_files() {
        let conn = Conn::new(|m, p, _| {
            if p.contains("/diffs") {
                return Ok(if p.contains("page=1") {
                    json!([{"old_path": "old.rb", "new_path": "new.rb", "diff": "@@ -1 +1 @@\n-a\n+b\n"}])
                } else {
                    json!([])
                });
            }
            d_handler(m, p, None)
        });
        let items = plugin_with(conn).discover(&c()).unwrap();
        assert_eq!(items[0].payload["old_paths"], json!({"new.rb": "old.rb"}));
    }

    #[test]
    fn diff_line_map_and_splitlines_helpers() {
        assert_eq!(splitlines("a\r\nb\rc\n"), ["a", "b", "c"]);
        assert_eq!(parse_hunk("@@ -3,4 +5 @@ foo"), Some((3, 5)));
        assert_eq!(parse_hunk("@@ nope"), None);
        let map = diff_line_map("--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-a\n+b\n c\n");
        assert_eq!(map["x"].get(&2), Some(&2));
        assert!(!map["x"].contains_key(&1));
    }

    #[test]
    fn finding_note_reads_severity_area_title_body_and_fix() {
        let f = json!({"path": "db/m.rb", "line": 3, "severity": "major", "category": "data",
                       "title": "No lock_timeout", "body": "ATTACH can stall traffic.",
                       "suggestion": "Set `lock_timeout` first."});
        assert_eq!(
            finding_note(&f),
            "\u{1f7e0} **Major** \u{b7} Data & migrations \u{2014} No lock_timeout\n\n\
ATTACH can stall traffic.\n\n**Suggested fix:** Set `lock_timeout` first."
        );
    }

    #[test]
    fn summary_note_has_verdict_overview_risk_table_and_findings_index() {
        let answer = json!({
            "summary": "Sound, but ATTACH needs a lock timeout.",
            "verdict": "request_changes",
            "overview": ["Adds a partial index on ml_ratings", "Updates structure.sql"],
            "risks": [{"area": "data", "level": "high", "note": "ATTACH | no timeout\nstalls"}],
            "findings": [{"path": "db/m.rb", "line": 3, "severity": "blocker", "category": "data",
                          "title": "No lock_timeout", "body": "b"}]
        });
        let note = summary_note(&answer).unwrap();
        assert!(
            note.starts_with("### LoopX pre-review \u{2014} \u{26d4} Changes requested\n\n"),
            "{note}"
        );
        assert!(note.contains(
            "**What this MR does**\n- Adds a partial index on ml_ratings\n- Updates structure.sql"
        ));
        assert!(note.contains("| Area | Level | Notes |\n| --- | --- | --- |\n| Data & migrations | \u{1f534} High | ATTACH \\| no timeout stalls |"), "{note}");
        assert!(note.contains(
            "- \u{1f534} **Blocker** \u{b7} Data & migrations \u{2014} `db/m.rb:3` No lock_timeout"
        ));
        assert!(summary_note(&json!({"summary": "", "findings": []})).is_none());
        let quiet = summary_note(
            &json!({"summary": "Fine.", "verdict": "approve", "risks": [], "findings": []}),
        )
        .unwrap();
        assert!(
            quiet.contains("\u{2705} Looks good to merge")
                && quiet.contains("no notable risks found")
        );
    }

    #[test]
    fn after_item_cleans_verdict_overview_risks_and_sorts_findings() {
        let posted = Arc::new(Mutex::new(vec![]));
        let answer = json!({
            "summary": "s", "verdict": "ship it!!",
            "overview": ["a", "", 5, "b", "c", "d", "e"],
            "risks": [
                {"area": "performance", "level": "low", "note": "n1"},
                {"area": "weather", "level": "high", "note": "bad area"},
                {"area": "security", "level": "high", "note": "n2"},
                {"area": "tests", "level": "medium", "note": "  "}
            ],
            "findings": [
                finding("app/a.rb", 2, "minor", "m"),
                {"path": "app/a.rb", "line": 2, "severity": "blocker", "category": "nonsense", "title": "t", "body": "b"}
            ]
        });
        with_poster(posted.clone())
            .after_item(&item(), &answer, &c())
            .unwrap();
        let p = &posted.lock().unwrap()[0];
        assert_eq!(p["verdict"], "comment");
        assert_eq!(p["overview"], json!(["a", "b", "c", "d"]));
        assert_eq!(
            p["risks"],
            json!([{"area": "security", "level": "high", "note": "n2"},
                   {"area": "performance", "level": "low", "note": "n1"}])
        );
        assert_eq!(p["findings"][0]["severity"], "blocker");
        assert_eq!(p["findings"][0]["category"], "correctness");
        assert_eq!(p["findings"][1]["suggestion"], "");
    }

    #[test]
    fn drafts_are_recognised_by_flag_or_title() {
        for mr in [
            json!({"draft": true, "title": "x"}),
            json!({"work_in_progress": true, "title": "x"}),
            json!({"title": "Draft: speed up"}),
            json!({"title": "  [Draft] speed up"}),
            json!({"title": "WIP: speed up"}),
        ] {
            assert!(is_draft(&mr), "{mr}");
        }
        assert!(!is_draft(
            &json!({"draft": false, "title": "Drafting rules for exports"})
        ));
    }

    fn rows_handler(rows: Value) -> Arc<Conn> {
        Conn::new(move |m, p, _| {
            if p.starts_with("/merge_requests?") {
                return Ok(rows.clone());
            }
            d_handler(m, p, None)
        })
    }

    #[test]
    fn discover_asks_for_reviewer_and_assignee_mrs_once_each_and_skips_drafts() {
        let conn = rows_handler(json!([
            {"project_id": 9, "iid": 7, "author": {"id": 2}, "title": "T"},
            {"project_id": 9, "iid": 8, "author": {"id": 2}, "title": "Draft: later", "draft": true}
        ]));
        let items = plugin_with(conn.clone()).discover(&c()).unwrap();
        assert_eq!(
            items.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
            ["mr:work:9!7@h"]
        );
        let lists: Vec<String> = conn
            .calls()
            .into_iter()
            .map(|c| c.1)
            .filter(|p| p.starts_with("/merge_requests?"))
            .collect();
        assert!(
            lists[0].contains("reviewer_id=1") && lists[1].contains("assignee_id=1"),
            "{lists:?}"
        );
        assert!(!conn
            .calls()
            .iter()
            .any(|c| c.1.contains("/merge_requests/8")));
    }

    #[test]
    fn discover_drops_a_draft_seen_only_in_the_detail() {
        let conn = Conn::new(|m, p, _| {
            if p.ends_with("/merge_requests/7") {
                return Ok(json!({"sha": "h", "diff_refs": mr()["diff_refs"], "draft": true}));
            }
            d_handler(m, p, None)
        });
        assert!(plugin_with(conn).discover(&c()).unwrap().is_empty());
    }

    #[test]
    fn discover_items_carry_meta_for_the_dashboard() {
        let conn = rows_handler(json!([{
            "project_id": 9, "iid": 7, "title": "Fix login",
            "author": {"id": 2, "name": "Ana Lee", "username": "ana"},
            "created_at": "2026-10-01T09:00:00Z", "references": {"full": "web/app!7"}
        }]));
        let items = plugin_with(conn).discover(&c()).unwrap();
        assert_eq!(
            items[0].meta,
            json!({"label": "Fix login (web/app!7)", "title": "Fix login", "ref": "web/app!7", "author": "Ana Lee",
                   "author_username": "ana", "created_at": "2026-10-01T09:00:00Z"})
        );
        assert!(items[0].payload.get("meta").is_none());
    }

    fn ctx_in(dir: &Path) -> LoopContext {
        let mut ctx = c();
        ctx.state_dir = Some(dir.to_path_buf());
        ctx
    }

    #[test]
    fn skipped_mrs_are_left_alone_and_requested_ones_jump_the_queue_once() {
        let d = tempfile::tempdir().unwrap();
        let rows = json!([
            {"project_id": 9, "iid": 6, "sha": "h", "author": {"id": 2}, "title": "A"},
            {"project_id": 9, "iid": 7, "sha": "h", "author": {"id": 2}, "title": "B"}
        ]);
        let conn = Conn::new(move |m, p, _| {
            if p.starts_with("/merge_requests?") {
                return Ok(rows.clone());
            }
            if p.ends_with("/merge_requests/6") {
                return Ok(json!({"sha": "h", "diff_refs": mr()["diff_refs"]}));
            }
            d_handler(m, p, None)
        });
        MrPrefs::update(d.path(), |p| {
            p.skip.insert("work:9!6".into());
        })
        .unwrap();
        // Both already reviewed: nothing to do.
        let seen = || -> SeenFn { Box::new(|_k| true) };
        let plugin = plugin_with(conn.clone()).with_seen(seen());
        assert!(plugin.discover(&ctx_in(d.path())).unwrap().is_empty());
        // "Review now" on both: the skipped one too, forced, and only once.
        MrPrefs::update(d.path(), |p| {
            p.requested.insert("work:9!7".into());
            p.requested.insert("work:9!6".into());
        })
        .unwrap();
        let items = plugin.discover(&ctx_in(d.path())).unwrap();
        assert_eq!(
            items
                .iter()
                .map(|i| (i.key.as_str(), i.force))
                .collect::<Vec<_>>(),
            [("mr:work:9!6@h", true), ("mr:work:9!7@h", true)]
        );
        let prefs = MrPrefs::load(d.path());
        assert!(prefs.requested.is_empty() && prefs.skip.contains("work:9!6"));
        assert!(plugin.discover(&ctx_in(d.path())).unwrap().is_empty());
    }

    fn archive_handler(rows: Value) -> Arc<Conn> {
        Conn::new(move |m, p, _| match p {
            "/projects/3" => Ok(json!({"id": 3, "archived": true})),
            "/projects/4" => {
                Ok(json!({"id": 4, "archived": false, "marked_for_deletion_at": "2026-10-01"}))
            }
            "/projects/5" => Err(http_err(Some(500))),
            _ if p.starts_with("/merge_requests?") => Ok(rows.clone()),
            _ => d_handler(m, p, None),
        })
    }

    #[test]
    fn discover_skips_mrs_in_archived_or_deleted_projects() {
        let conn = archive_handler(json!([
            {"project_id": 3, "iid": 1, "author": {"id": 2}, "title": "archived"},
            {"project_id": 4, "iid": 2, "author": {"id": 2}, "title": "being deleted"},
            {"project_id": 9, "iid": 7, "author": {"id": 2}, "title": "T"}
        ]));
        let items = plugin_with(conn.clone()).discover(&c()).unwrap();
        assert_eq!(
            items.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
            ["mr:work:9!7@h"]
        );
        let calls: Vec<String> = conn.calls().into_iter().map(|c| c.1).collect();
        assert!(!calls
            .iter()
            .any(|p| p.contains("/projects/3/") || p.contains("/projects/4/")));
        // One lookup per project, however many roles listed its MRs.
        assert_eq!(calls.iter().filter(|p| *p == "/projects/9").count(), 1);
        assert!(calls
            .iter()
            .filter(|p| p.starts_with("/merge_requests?"))
            .all(|p| p.contains("non_archived=true")));
    }

    #[test]
    fn list_my_mrs_hides_archived_projects_and_keeps_unknown_ones() {
        let conn = archive_handler(json!([
            {"project_id": 3, "iid": 1, "title": "archived", "author": {"id": 2}},
            {"project_id": 5, "iid": 2, "title": "lookup failed", "author": {"id": 2}},
            {"project_id": 9, "iid": 3, "title": "active", "author": {"id": 2}}
        ]));
        let out = list_my_mrs(&Source {
            accounts: vec![Account::new("work", "gitlab")],
            conn,
        });
        assert!(out[0].error.is_none());
        let mut iids: Vec<&str> = out[0].mrs.iter().map(|m| m.iid.as_str()).collect();
        iids.sort();
        assert_eq!(iids, ["2", "3"]);
    }

    #[test]
    fn failed_mrs_round_trip_with_their_reason() {
        let d = tempfile::tempdir().unwrap();
        MrPrefs::update(d.path(), |p| {
            p.failed.insert("a:1!2".into(), "HTTPError".into());
        })
        .unwrap();
        assert_eq!(
            MrPrefs::load(d.path())
                .failed
                .get("a:1!2")
                .map(String::as_str),
            Some("HTTPError")
        );
    }

    #[test]
    fn a_failed_mr_is_skipped_on_every_commit_until_review_now() {
        let d = tempfile::tempdir().unwrap();
        MrPrefs::update(d.path(), |p| {
            p.failed.insert("work:9!7".into(), "boom".into());
        })
        .unwrap();
        // A new head SHA does not bring it back.
        let conn = rows_handler(json!([
            {"project_id": 9, "iid": 7, "sha": "new", "author": {"id": 2}, "title": "T"}
        ]));
        let plugin = plugin_with(conn.clone());
        assert!(plugin.discover(&ctx_in(d.path())).unwrap().is_empty());
        assert!(!conn
            .calls()
            .iter()
            .any(|c| c.1.contains("/merge_requests/7")));
        // "Review now" is the way to continue: it is tried once more.
        MrPrefs::update(d.path(), |p| {
            p.requested.insert("work:9!7".into());
        })
        .unwrap();
        let items = plugin.discover(&ctx_in(d.path())).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].force);
    }

    #[test]
    fn an_mr_that_fails_to_load_is_remembered_as_failed() {
        let d = tempfile::tempdir().unwrap();
        let conn = Conn::new(|m, p, _| {
            if p.ends_with("/merge_requests/7") {
                return Err(http_err(Some(500)));
            }
            d_handler(m, p, None)
        });
        assert!(plugin_with(conn)
            .discover(&ctx_in(d.path()))
            .unwrap()
            .is_empty());
        let failed = MrPrefs::load(d.path()).failed;
        assert!(failed.contains_key("work:9!7"), "{failed:?}");
    }

    #[test]
    fn digest_remembers_failed_reviews_and_forgets_ones_that_now_work() {
        let d = tempfile::tempdir().unwrap();
        MrPrefs::update(d.path(), |p| {
            p.failed.insert("work:9!6".into(), "old".into());
        })
        .unwrap();
        let outcomes = [
            Outcome::new("mr:work:9!7@h", Status::Failed, "model timed out"),
            Outcome::new("mr:work:9!6@h2", Status::Done, "ok"),
        ];
        plugin_with(ok_conn()).digest(&outcomes, &ctx_in(d.path()));
        let failed = MrPrefs::load(d.path()).failed;
        assert_eq!(
            failed.get("work:9!7").map(String::as_str),
            Some("model timed out")
        );
        assert!(!failed.contains_key("work:9!6"));
    }

    #[test]
    fn prefs_round_trip_and_tolerate_a_broken_file() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(MrPrefs::load(d.path()), MrPrefs::default());
        MrPrefs::update(d.path(), |p| {
            p.skip.insert("a:1!2".into());
        })
        .unwrap();
        assert!(MrPrefs::load(d.path()).skip.contains("a:1!2"));
        std::fs::write(MrPrefs::path(d.path()), "{nope").unwrap();
        assert_eq!(MrPrefs::load(d.path()), MrPrefs::default());
    }

    #[test]
    fn list_my_mrs_merges_roles_and_sorts_newest_first() {
        let conn = Conn::new(|_, p, _| {
            if p == "/user" {
                return Ok(json!({"id": 1}));
            }
            let mine = json!({"project_id": 9, "iid": 1, "title": "Mine", "author": {"id": 1, "name": "Me"},
                              "created_at": "2026-10-03T00:00:00Z", "references": {"full": "g/p!1"}});
            let theirs = json!({"project_id": 9, "iid": 2, "title": "Draft: theirs", "sha": "s2",
                                "author": {"id": 2, "name": "Ana", "username": "ana"},
                                "created_at": "2026-10-05T00:00:00Z", "web_url": "https://gl/2"});
            Ok(if p.contains("author_id=1") {
                json!([mine])
            } else if p.contains("assignee_id=1") {
                json!([theirs, mine])
            } else {
                json!([theirs])
            })
        });
        let out = list_my_mrs(&Source {
            accounts: vec![Account::new("work", "gitlab")],
            conn,
        });
        assert_eq!(out.len(), 1);
        assert!(out[0].error.is_none());
        let mrs = &out[0].mrs;
        assert_eq!(
            mrs.iter().map(|m| m.iid.as_str()).collect::<Vec<_>>(),
            ["2", "1"]
        );
        let (theirs, mine) = (&mrs[0], &mrs[1]);
        assert!(theirs.assignee && theirs.reviewer && !theirs.authored && theirs.draft);
        assert_eq!(
            (theirs.reference.as_str(), theirs.author.as_str()),
            ("!2", "Ana")
        );
        assert!(mine.authored && mine.assignee && !mine.reviewable());
        assert_eq!(mine.reference, "g/p!1");
    }

    #[test]
    fn list_my_mrs_reports_a_failing_account_without_secrets() {
        let conn = Conn::new(|_, _, _| {
            Err(ConnectorError::Other {
                class: "RuntimeError".into(),
                detail: "https://x/token".into(),
            })
        });
        let out = list_my_mrs(&Source {
            accounts: vec![Account::new("work", "gitlab")],
            conn,
        });
        assert_eq!(out[0].error.as_deref(), Some("RuntimeError"));
    }

    #[test]
    fn review_state_covers_each_case() {
        let mr = MyMr {
            account: "w".into(),
            project_id: "9".into(),
            iid: "7".into(),
            sha: "h".into(),
            reviewer: true,
            ..MyMr::default()
        };
        let none = MrPrefs::default();
        let never = |_: &str| false;
        assert_eq!(review_state(&mr, &none, never), ReviewState::Pending);
        assert_eq!(
            review_state(&mr, &none, |k| k == "mr:w:9!7@h"),
            ReviewState::Reviewed
        );
        let mut prefs = MrPrefs::default();
        prefs.skip.insert("w:9!7".into());
        assert_eq!(review_state(&mr, &prefs, never), ReviewState::Skipped);
        prefs.requested.insert("w:9!7".into());
        assert_eq!(review_state(&mr, &prefs, never), ReviewState::Requested);
        let mut failed = MrPrefs::default();
        failed.failed.insert("w:9!7".into(), "x".into());
        assert_eq!(review_state(&mr, &failed, never), ReviewState::Failed);
        failed.requested.insert("w:9!7".into());
        assert_eq!(review_state(&mr, &failed, never), ReviewState::Requested);
        let draft = MyMr {
            draft: true,
            ..mr.clone()
        };
        assert_eq!(review_state(&draft, &prefs, never), ReviewState::Draft);
        let own = MyMr {
            authored: true,
            ..mr.clone()
        };
        assert_eq!(review_state(&own, &none, never), ReviewState::Own);
    }
}
