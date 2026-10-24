//! MR Review loop on LoopKit: pre-reviews GitLab merge requests where the user
//! is a reviewer and leaves the findings as *draft* review notes only (mirrors
//! `bin/loop_plugins/mr_review.py`). The user publishes them from the MR
//! (Review -> Submit). This plugin never calls bulk_publish, approve, or the
//! published `/notes` endpoint.
#![allow(
    clippy::type_complexity,
    clippy::field_reassign_with_default,
    clippy::let_unit_value
)]

use crate::connectors::{ApiOpts, Connector, ConnectorError, ConnectorSource};
use crate::loopkit::{self, LoopContext, LoopPlugin, Outcome, PluginError, Status, WorkItem};
use crate::loops_config::{py_str, py_truthy};
use crate::seen_store::SeenStore;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

pub const DIFF_CAP_BYTES: usize = 150_000;
pub const MAX_FINDINGS: usize = 15;
pub const MAX_DESCRIPTION: usize = 4096;
pub const MAX_BODY: usize = 2000;
pub const MAX_SUMMARY: usize = 600;
const MAX_DIFF_PAGES: u32 = 20;
const MAX_DRAFT_PAGES: u32 = 20;
const SEVERITIES: [&str; 4] = ["nit", "minor", "major", "blocker"];
const SIGNATURE: &str = "\n\n_\u{2014} Loop X pre-review_";
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
/// every page. Drafts without the Loop X signature are never touched. All pages
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
        let note = format!(
            "**[{}]** {}{SIGNATURE}",
            py_str(&f["severity"]),
            defuse(&py_str(&f["body"]))
        );
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
    let summary = defuse(
        py_str(
            answer
                .get("summary")
                .filter(|v| py_truthy(v))
                .unwrap_or(&json!("")),
        )
        .trim(),
    );
    if !summary.is_empty() {
        post_draft(
            conn,
            mr,
            &format!("**Summary:** {summary}{SIGNATURE}"),
            None,
        )?;
        *drafts += 1;
    }
    Ok(())
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
            .with_url(url),
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
        &["summary", "findings"]
    }

    fn discover(&self, ctx: &LoopContext) -> Result<Vec<WorkItem>, PluginError> {
        let mut items = Vec::new();
        for account in self
            .connectors
            .accounts_with_capability(crate::connectors::MERGE_REQUESTS)
        {
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
                let rows = conn.api(
                    "GET",
                    &format!("/merge_requests?reviewer_id={}&state=opened&scope=all&draft=no&per_page=50", py_str(id)),
                    &ApiOpts::default(),
                )?;
                Ok((conn, me, rows.as_array().cloned().unwrap_or_default()))
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
            for row in &rows {
                // The list already carries the head SHA: skip a reviewed one
                // before paying for its detail and diff requests.
                if let Some(sha) = row.get("sha").filter(|v| py_truthy(v)) {
                    let key = item_key(
                        &account.id,
                        &row.get("project_id").map_or("None".into(), py_str),
                        &row.get("iid").map_or("None".into(), py_str),
                        &py_str(sha),
                    );
                    if !ctx.force && self.already_seen(&key, ctx) {
                        continue;
                    }
                }
                match self.item(conn.as_ref(), &account.id, &me, row) {
                    Ok(Some(item)) => items.push(item),
                    Ok(None) => {}
                    Err(e) => ctx.log(&format!(
                        "mr-review: MR !{} failed to load: {}",
                        row.get("iid").map_or("None".into(), py_str),
                        e.class_name()
                    )),
                }
            }
        }
        Ok(items)
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
            kept.push(json!({
                "path": path, "line": line, "severity": sev,
                "body": body.trim().chars().take(MAX_BODY).collect::<String>(),
            }));
        }
        kept.truncate(MAX_FINDINGS);
        let summary = answer
            .get("summary")
            .and_then(Value::as_str)
            .map(|s| s.trim().chars().take(MAX_SUMMARY).collect::<String>())
            .unwrap_or_default();
        let cleaned = json!({"summary": summary, "findings": kept});
        let account = py_str(&payload["account"]);
        let conn = self
            .connectors
            .load(&account)
            .map_err(|_| PluginError::new("KeyError", account.clone()))?;
        let url = payload.get("web_url").map_or(String::new(), py_str);
        let posted = match &self.poster {
            Some(p) => p(conn.as_ref(), payload, &cleaned),
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
            None => format!("{} findings + summary ({blockers} blocker)", kept.len()),
        };
        Ok(Outcome::new(&item.key, Status::Done, summary_text)
            .with_url(url)
            .with_data(json!({"title": item.title, "drafts": drafts, "blockers": blockers})))
    }

    fn digest(&self, outcomes: &[Outcome], _ctx: &LoopContext) -> Option<String> {
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
            state_dir: None,
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
            .starts_with("**[blocker]** eval on params"));
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

    const SIG_NOTE: &str = "x\n\n_\u{2014} Loop X pre-review_";

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
            json!([{"id": 11, "note": "_\u{2014} Loop X pre-review_"}]),
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
        assert_eq!(out.summary, "1 findings + summary (1 blocker)");
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
                let conn = if id == "bad" {
                    Conn::new(|_, _, _| {
                        Err(ConnectorError::Other {
                            class: "RuntimeError".into(),
                            detail: "secret https://x/token".into(),
                        })
                    })
                } else {
                    Conn::new(|m, p, _| d_handler(m, p, None))
                };
                Ok(Box::new(Shared(conn)))
            }
        }
        let logs: Arc<Mutex<Vec<String>>> = Default::default();
        let ctx = ctx(json!({}), false, logs.clone());
        let items = MrReview::new(Arc::new(Multi)).discover(&ctx).unwrap();
        assert_eq!(
            items.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
            ["mr:work:9!7@h"]
        );
        let logs = logs.lock().unwrap();
        assert!(
            !logs.is_empty()
                && logs
                    .iter()
                    .all(|l| !l.contains("secret") && !l.contains("token"))
        );
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
    }

    #[test]
    fn foreign_draft_quoting_signature_midtext_not_deleted() {
        let quoted = "I disagree with \u{201c}_\u{2014} Loop X pre-review_\u{201d} being added here, thoughts?";
        let conn = fake2(
            json!([{"id": 11, "note": "x\n\n_\u{2014} Loop X pre-review_\n"}, {"id": 12, "note": quoted}]),
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
}
