//! Meeting Prep loop on LoopKit (mirrors `bin/loop_plugins/calendar_prep.py`):
//! shortly before each meeting, gathers the invite (agenda, attendees, join
//! link), the GitLab issues/MRs it links to, recent mail with the attendees
//! and what the last brief for the same series said, has the model write a
//! short prep brief and sends it through the loop's notifier. Read-only
//! everywhere. Invite text, GitLab titles and mail snippets are untrusted:
//! trimmed before the prompt, the model runs sealed, every output string is
//! sanitised and a brief may only link to the GitLab URLs it was offered.
#![allow(
    clippy::type_complexity,
    clippy::field_reassign_with_default,
    clippy::let_unit_value
)]

use crate::connectors::http::quote;
use crate::connectors::{ApiOpts, Connector, ConnectorSource, Field};
use crate::loopkit::{self, LoopContext, LoopPlugin, Outcome, PluginError, Status, WorkItem};
use crate::loops_config::{py_str, py_truthy};
use crate::seen_store::{parse_iso, py_isoformat};
use crate::{ai_cli_config, config, memory_store, pyjson};
use chrono::{DateTime, Duration, FixedOffset, Local, NaiveTime, SecondsFormat, TimeZone, Utc};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const DEFAULT_LEAD_MINUTES: i64 = 45;
pub const LEAD_RANGE: (i64, i64) = (15, 120);
pub const DEFAULT_MAIL_DAYS: i64 = 30;
pub const MAX_MAIL_DAYS: i64 = 365;
pub const MAIL_LIMIT: usize = 10;
pub const MAX_GITLAB_LINKS: usize = 5;
pub const MAX_DESCRIPTION: usize = 4000;
pub const MAX_SNIPPET: usize = 300;
pub const MAX_SUMMARY: usize = 400;
pub const MAX_ENTRY: usize = 200;
pub const MAX_ENTRIES: usize = 5;
/// Names shown in the Slack brief before "+N more".
pub const MAX_ATTENDEES: usize = 8;
/// Briefs kept for the Live page, newest first.
pub const MAX_BRIEFS: usize = 100;
pub const NO_CLAUDE: &str = "mail skipped: reading mail requires the Claude CLI";
const MAX_MEMORY: usize = 400;

pub const SETTINGS_FIELDS: [Field; 4] = [
    Field::new("calendar_account", "Calendar account")
        .optional()
        .help("Connector id of the calendar to watch; empty watches every calendar")
        .placeholder("google-calendar"),
    Field::new("lead_minutes", "Minutes ahead")
        .optional()
        .default("45")
        .help("Brief meetings starting within this many minutes (15-120)")
        .placeholder("45"),
    Field::new("mail_days", "Mail lookback (days)")
        .optional()
        .default("30")
        .help("Search recent mail with the attendees; 0 turns mail off")
        .placeholder("30"),
    Field::new("include_solo", "Include solo events")
        .optional()
        .default("no")
        .help("yes also briefs events with no other attendees")
        .placeholder("no"),
];

const OUTPUT_KEYS: [&str; 5] = [
    "summary",
    "agenda",
    "open_items",
    "talking_points",
    "follow_ups",
];

/// `mail_fn(addresses, days, limit)`: recent mail rows (`subject`, `from`,
/// `date`, `snippet`); `Err` carries only the exception class name.
pub type MailFn = Arc<dyn Fn(&[String], i64, usize) -> Result<Vec<Value>, String> + Send + Sync>;
/// `memory_fn(project path, issue iid)`: the recorded task memory, `""` if none.
pub type MemoryFn = Arc<dyn Fn(&str, &str) -> Result<String, String> + Send + Sync>;
/// `cli_fn()`: the selected AI CLI (`claude` / `codex`).
pub type CliFn = Arc<dyn Fn() -> String + Send + Sync>;

pub struct CalendarPrep {
    connectors: Arc<dyn ConnectorSource>,
    mail_fn: Option<MailFn>,
    cli_fn: Option<CliFn>,
    memory_fn: Option<MemoryFn>,
    series_path: Option<PathBuf>,
    today_path: Option<PathBuf>,
    briefs_path: Option<PathBuf>,
}

impl CalendarPrep {
    pub fn new(connectors: Arc<dyn ConnectorSource>) -> Self {
        Self {
            connectors,
            mail_fn: None,
            cli_fn: None,
            memory_fn: None,
            series_path: None,
            today_path: None,
            briefs_path: None,
        }
    }
    pub fn with_mail_fn(mut self, f: MailFn) -> Self {
        self.mail_fn = Some(f);
        self
    }
    pub fn with_cli_fn(mut self, f: CliFn) -> Self {
        self.cli_fn = Some(f);
        self
    }
    pub fn with_memory_fn(mut self, f: MemoryFn) -> Self {
        self.memory_fn = Some(f);
        self
    }
    pub fn with_series_path(mut self, p: PathBuf) -> Self {
        self.series_path = Some(p);
        self
    }
    pub fn with_today_path(mut self, p: PathBuf) -> Self {
        self.today_path = Some(p);
        self
    }
    pub fn with_briefs_path(mut self, p: PathBuf) -> Self {
        self.briefs_path = Some(p);
        self
    }

    fn state_file(&self, custom: &Option<PathBuf>, ctx: &LoopContext, name: &str) -> PathBuf {
        custom.clone().unwrap_or_else(|| {
            ctx.repo_root
                .join("outputs")
                .join("loops")
                .join(self.loop_name())
                .join(name)
        })
    }

    fn series_file(&self, ctx: &LoopContext) -> PathBuf {
        self.state_file(&self.series_path, ctx, "series.json")
    }
    fn today_file(&self, ctx: &LoopContext) -> PathBuf {
        self.state_file(&self.today_path, ctx, "today.json")
    }
    fn briefs_file(&self, ctx: &LoopContext) -> PathBuf {
        self.state_file(&self.briefs_path, ctx, "briefs.json")
    }

    fn cli(&self) -> String {
        match &self.cli_fn {
            Some(f) => f(),
            None => ai_cli_config::selected_cli(),
        }
    }

    /// Keep the brief by meeting key so the Live page can show each meeting's
    /// summary; only the newest `MAX_BRIEFS` are kept.
    fn record_brief(&self, ctx: &LoopContext, key: &str, brief: &Value) {
        let path = self.briefs_file(ctx);
        let mut briefs = read_json(&path);
        briefs.insert(
            key.to_string(),
            json!({"summary": brief["summary"], "agenda": brief["agenda"],
                   "prepared_at": py_isoformat(ctx.now)}),
        );
        let prepared = |k: &String| -> String {
            briefs
                .get(k)
                .and_then(|v| v.get("prepared_at"))
                .filter(|v| py_truthy(v))
                .map(py_str)
                .unwrap_or_default()
        };
        let mut newest: Vec<String> = briefs.keys().cloned().collect();
        newest.sort_by_key(|k| std::cmp::Reverse(prepared(k)));
        let kept: Map<String, Value> = newest
            .into_iter()
            .take(MAX_BRIEFS)
            .map(|k| (k.clone(), briefs[&k].clone()))
            .collect();
        if write_json_atomic(&path, &Value::Object(kept)).is_err() {
            ctx.log("calendar-prep: could not record the brief: OSError");
        }
    }

    // --- discovery -----------------------------------------------------------

    fn gitlab_connectors(&self, ctx: &LoopContext) -> Vec<Box<dyn Connector>> {
        let mut found = Vec::new();
        for account in self.connectors.accounts_with_capability("issues") {
            if account.kind != "gitlab" {
                continue;
            }
            match self.connectors.load(&account.id) {
                Ok(c) => found.push(c),
                Err(f) => ctx.log(&format!(
                    "calendar-prep: GitLab account {} failed: {}",
                    account.id,
                    load_class(&f)
                )),
            }
        }
        found
    }

    fn item(
        &self,
        aid: &str,
        ev: &Value,
        gitlabs: &[Box<dyn Connector>],
        mail_days: i64,
        series: &Map<String, Value>,
        ctx: &LoopContext,
    ) -> WorkItem {
        let attendees: Vec<&Value> = ev
            .get("attendees")
            .and_then(Value::as_array)
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        let is_self = |a: &Value| a.get("self").is_some_and(py_truthy);
        let me: Vec<String> = attendees
            .iter()
            .filter(|a| is_self(a))
            .map(|a| field_text(a, "email").to_lowercase())
            .collect();
        let others: Vec<&Value> = attendees.iter().copied().filter(|a| !is_self(a)).collect();
        let title = loopkit::chat_text(&field_text(ev, "summary"), MAX_ENTRY);
        let join_url = loopkit::chat_url(&field_text(ev, "join_url"));
        let description: String = field_text(ev, "description")
            .chars()
            .take(MAX_DESCRIPTION)
            .collect();
        let event = json!({
            "title": title,
            "start": raw(ev, "start"),
            "end": raw(ev, "end"),
            "organizer": loopkit::chat_text(&field_text(ev, "organizer"), 120),
            "attendees": others.iter().map(|a| {
                let who = if truthy(a.get("name")) { field_text(a, "name") } else { field_text(a, "email") };
                loopkit::chat_text(&who, 80)
            }).collect::<Vec<_>>(),
            "location": loopkit::chat_text(&field_text(ev, "location"), MAX_ENTRY),
            "join_url": join_url,
            "description": description,
        });
        let mut payload = Map::new();
        payload.insert("event".into(), event);
        let (rows, gitlab_error) = self.linked_gitlab(&field_text(ev, "description"), gitlabs, ctx);
        payload.insert("gitlab".into(), Value::Array(rows));
        if let Some(e) = gitlab_error {
            payload.insert("gitlab_error".into(), e.into());
        }
        let mut candidates: Vec<String> = Vec::new();
        if truthy(ev.get("organizer")) {
            candidates.push(py_str(&ev["organizer"]).to_lowercase());
        }
        for a in &others {
            if truthy(a.get("email")) {
                candidates.push(py_str(&a["email"]).to_lowercase());
            }
        }
        let mut addresses: Vec<String> = Vec::new();
        for a in candidates {
            if !me.contains(&a) && !addresses.contains(&a) {
                addresses.push(a);
            }
        }
        let (mail, mail_error) = self.mail(&addresses, mail_days, ctx);
        payload.insert("mail".into(), Value::Array(mail));
        if let Some(e) = mail_error {
            payload.insert("mail_error".into(), e.into());
        }
        let series_id = if truthy(ev.get("recurring_event_id")) {
            field_text(ev, "recurring_event_id")
        } else {
            field_text(ev, "id")
        };
        payload.insert("series_id".into(), series_id.clone().into());
        if let Some(last) = series.get(&series_id).filter(|v| v.is_object()) {
            payload.insert("last_time".into(), last.clone());
        }
        let key = format!("cal:{aid}:{}:{}", raw_str(ev, "id"), raw_str(ev, "start"));
        let url = loopkit::chat_url(&field_text(ev, "html_link"));
        WorkItem::new(key, title, Value::Object(payload)).with_url(url)
    }

    /// `(rows, error)` for GitLab issue/MR URLs in the invite that belong to a
    /// configured GitLab account; at most `MAX_GITLAB_LINKS` fetches.
    fn linked_gitlab(
        &self,
        description: &str,
        gitlabs: &[Box<dyn Connector>],
        ctx: &LoopContext,
    ) -> (Vec<Value>, Option<String>) {
        struct Ref<'a> {
            conn: &'a dyn Connector,
            base: String,
            path: String,
            kind: String,
            iid: String,
        }
        let mut refs: Vec<Ref> = Vec::new();
        for url in find_urls(description) {
            for conn in gitlabs {
                let Ok(base) = conn.base_url() else { continue };
                let base = base.trim_end_matches('/').to_string();
                let Some(rest) = url.strip_prefix(&format!("{base}/")) else {
                    continue;
                };
                if let Some((path, kind, iid)) = parse_gitlab_path(rest) {
                    let dup = refs.iter().any(|r| {
                        r.base == base && r.path == path && r.kind == kind && r.iid == iid
                    });
                    if !dup {
                        refs.push(Ref {
                            conn: conn.as_ref(),
                            base,
                            path,
                            kind,
                            iid,
                        });
                    }
                }
                break;
            }
        }
        let mut rows = Vec::new();
        let mut failed = false;
        for r in refs.iter().take(MAX_GITLAB_LINKS) {
            let api = format!("/projects/{}/{}/{}", quote(&r.path), r.kind, r.iid);
            let data = match r.conn.api("GET", &api, &ApiOpts::default()) {
                Ok(d) => d,
                Err(e) => {
                    ctx.log(&format!(
                        "calendar-prep: a GitLab link failed: {}",
                        e.class_name()
                    ));
                    failed = true;
                    continue;
                }
            };
            let mut row = Map::new();
            row.insert(
                "kind".into(),
                if r.kind == "issues" {
                    "issue"
                } else {
                    "merge_request"
                }
                .into(),
            );
            row.insert(
                "url".into(),
                format!("{}/{}/-/{}/{}", r.base, r.path, r.kind, r.iid).into(),
            );
            row.insert(
                "title".into(),
                loopkit::chat_text(&field_text(&data, "title"), MAX_ENTRY).into(),
            );
            row.insert(
                "state".into(),
                loopkit::chat_text(&field_text(&data, "state"), 40).into(),
            );
            row.insert(
                "updated_at".into(),
                loopkit::chat_text(&field_text(&data, "updated_at"), 40).into(),
            );
            row.insert("assignees".into(), usernames(data.get("assignees")));
            if r.kind == "merge_requests" {
                row.insert("reviewers".into(), usernames(data.get("reviewers")));
                row.insert(
                    "merge_status".into(),
                    loopkit::chat_text(&field_text(&data, "detailed_merge_status"), 40).into(),
                );
            }
            let memory = if r.kind == "issues" {
                let looked_up = match &self.memory_fn {
                    Some(f) => f(&r.path, &r.iid),
                    None => default_memory(&r.path, &r.iid),
                };
                match looked_up {
                    Ok(m) => m,
                    Err(class) => {
                        ctx.log(&format!("calendar-prep: memory lookup failed: {class}"));
                        String::new()
                    }
                }
            } else {
                String::new()
            };
            if !memory.is_empty() {
                row.insert(
                    "memory".into(),
                    loopkit::chat_text(&memory, MAX_MEMORY).into(),
                );
            }
            rows.push(Value::Object(row));
        }
        (rows, failed.then(|| "GitLab links unavailable".to_string()))
    }

    fn mail(
        &self,
        addresses: &[String],
        days: i64,
        ctx: &LoopContext,
    ) -> (Vec<Value>, Option<String>) {
        if days <= 0 || addresses.is_empty() {
            return (Vec::new(), None);
        }
        if self.cli() != "claude" {
            return (Vec::new(), Some(NO_CLAUDE.into()));
        }
        let result = match &self.mail_fn {
            Some(f) => f(addresses, days, MAIL_LIMIT),
            None => self.default_mail(addresses, days, MAIL_LIMIT),
        };
        let raw_rows = match result {
            Ok(r) => r,
            Err(class) => {
                ctx.log(&format!("calendar-prep: mail search failed: {class}"));
                return (Vec::new(), Some("mail unavailable".into()));
            }
        };
        let rows = raw_rows
            .iter()
            .take(MAIL_LIMIT)
            .filter(|r| r.is_object() && issue_related(r))
            .map(|r| {
                json!({"subject": loopkit::chat_text(&field_text(r, "subject"), MAX_ENTRY),
                       "from": loopkit::chat_text(&field_text(r, "from"), 120),
                       "date": loopkit::chat_text(&field_text(r, "date"), 40),
                       "snippet": loopkit::chat_text(&field_text(r, "snippet"), MAX_SNIPPET)})
            })
            .collect();
        (rows, None)
    }

    /// Recent mail with any of `addresses` across every connected mailbox,
    /// newest first. Fails only when every mailbox failed.
    fn default_mail(
        &self,
        addresses: &[String],
        days: i64,
        limit: usize,
    ) -> Result<Vec<Value>, String> {
        let mut rows: Vec<Value> = Vec::new();
        let mut failure: Option<String> = None;
        for account in self.connectors.accounts_with_capability("mail") {
            let own = account.setting("account").trim().to_lowercase();
            let wanted: Vec<&String> = addresses.iter().filter(|a| **a != own).collect();
            if wanted.is_empty() {
                continue;
            }
            let query = if account.setting("provider") == "outlook" {
                wanted
                    .iter()
                    .map(|a| format!("participants:{a}"))
                    .collect::<Vec<_>>()
                    .join(" OR ")
            } else {
                let terms = wanted
                    .iter()
                    .map(|a| format!("from:{a} to:{a}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("{{{terms}}} newer_than:{days}d")
            };
            let found = self
                .connectors
                .load(&account.id)
                .map_err(|f| load_class(&f))
                .and_then(|c| {
                    c.search_messages(&query, limit as u32)
                        .map_err(|e| e.class_name())
                });
            match found {
                Ok(r) => rows.extend(r),
                Err(class) => failure = Some(class),
            }
        }
        if let (Some(class), true) = (failure, rows.is_empty()) {
            return Err(class);
        }
        rows.sort_by_key(|r| std::cmp::Reverse(field_text(r, "date")));
        rows.truncate(limit);
        Ok(rows)
    }

    fn write_today(
        &self,
        ctx: &LoopContext,
        day_start: DateTime<FixedOffset>,
        rows: &[Option<Value>],
    ) {
        let mut meetings: Vec<&Value> = rows.iter().flatten().collect();
        meetings.sort_by_key(|r| field_text(r, "start"));
        let data = json!({"date": day_start.date_naive().to_string(),
                          "generated_at": py_isoformat(ctx.now), "meetings": meetings});
        if write_json_atomic(&self.today_file(ctx), &data).is_err() {
            ctx.log("calendar-prep: could not write today's meetings: OSError");
        }
    }
}

// ---- helpers --------------------------------------------------------------------

fn load_class(f: &crate::connectors::LoadFailure) -> String {
    use crate::connectors::LoadFailure::*;
    match f {
        Unknown => "KeyError".into(),
        Config(_) => "ConnectorConfigError".into(),
        Other(c) => c.clone(),
    }
}

fn truthy(v: Option<&Value>) -> bool {
    v.is_some_and(py_truthy)
}

/// `str(obj.get(key) or "")`.
fn field_text(obj: &Value, key: &str) -> String {
    obj.get(key)
        .filter(|v| py_truthy(v))
        .map(py_str)
        .unwrap_or_default()
}

/// `obj.get(key, "")` as a JSON value.
fn raw(obj: &Value, key: &str) -> Value {
    obj.get(key)
        .cloned()
        .unwrap_or_else(|| Value::String(String::new()))
}

/// `str(obj.get(key, ""))`.
fn raw_str(obj: &Value, key: &str) -> String {
    obj.get(key).map(py_str).unwrap_or_default()
}

fn usernames(v: Option<&Value>) -> Value {
    Value::Array(
        v.and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|u| u.is_object() && truthy(u.get("username")))
                    .map(|u| Value::String(py_str(&u["username"])))
                    .collect()
            })
            .unwrap_or_default(),
    )
}

fn int_setting(settings: &Value, key: &str, default: i64, lo: i64, hi: i64) -> i64 {
    let text = settings
        .get(key)
        .map_or_else(|| default.to_string(), py_str);
    match text.trim().parse::<i64>() {
        Ok(v) => v.clamp(lo, hi),
        Err(_) => default,
    }
}

pub fn lead_minutes(settings: &Value) -> i64 {
    int_setting(
        settings,
        "lead_minutes",
        DEFAULT_LEAD_MINUTES,
        LEAD_RANGE.0,
        LEAD_RANGE.1,
    )
}

pub fn mail_days(settings: &Value) -> i64 {
    int_setting(settings, "mail_days", DEFAULT_MAIL_DAYS, 0, MAX_MAIL_DAYS)
}

/// A timed event's start/end as an instant; `None` for all-day
/// (`YYYY-MM-DD`) or unparsable values.
fn parse_time(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let s = value?.as_str()?;
    if !s.contains('T') {
        return None;
    }
    parse_iso(s)
}

fn read_json(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

fn write_json_atomic(path: &Path, data: &Value) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(
        ".{name}.{}.tmp",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    let result = std::fs::write(&tmp, pyjson::to_string_pretty(data) + "\n")
        .and_then(|_| std::fs::rename(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    result
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `https?://[^\s<>"'()\[\]]+` matches, left to right.
fn find_urls(text: &str) -> Vec<String> {
    let bad =
        |c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '(' | ')' | '[' | ']');
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let scheme = if rest.starts_with("https://") {
            8
        } else if rest.starts_with("http://") {
            7
        } else {
            i += rest.chars().next().map_or(1, char::len_utf8);
            continue;
        };
        let body: String = rest[scheme..].chars().take_while(|c| !bad(*c)).collect();
        if body.is_empty() {
            i += scheme;
            continue;
        }
        out.push(format!("{}{body}", &rest[..scheme]));
        i += scheme + body.len();
    }
    out
}

/// `^(.+?)/-/(issues|merge_requests)/(\d+)(?:[/?#].*)?$` -> (path, kind, iid).
fn parse_gitlab_path(rest: &str) -> Option<(String, String, String)> {
    let mut from = 1;
    while let Some(pos) = rest.get(from..).and_then(|s| s.find("/-/")) {
        let at = from + pos;
        let tail = &rest[at + 3..];
        for kind in ["issues", "merge_requests"] {
            if let Some(after) = tail.strip_prefix(kind).and_then(|t| t.strip_prefix('/')) {
                let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
                let next = after[digits.len()..].chars().next();
                if !digits.is_empty() && next.is_none_or(|c| matches!(c, '/' | '?' | '#')) {
                    return Some((rest[..at].to_string(), kind.to_string(), digits));
                }
            }
        }
        from = at + 1;
    }
    None
}

/// True when a mail row's subject or snippet mentions an issue/MR (`#12`,
/// `!7`, "issue", "merge request", a GitLab link).
fn issue_related(row: &Value) -> bool {
    let text = format!(
        "{} {}",
        field_text(row, "subject"),
        field_text(row, "snippet")
    )
    .to_ascii_lowercase();
    if text.contains("/-/issues/") || text.contains("/-/merge_requests/") {
        return true;
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    // [#!]\d+\b right after the start, whitespace, "(" or "[".
    for (n, (i, c)) in chars.iter().enumerate() {
        if !matches!(c, '#' | '!') {
            continue;
        }
        let prev_ok =
            n == 0 || chars[n - 1].1.is_whitespace() || matches!(chars[n - 1].1, '(' | '[');
        let digits: String = text[i + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        let after = text[i + 1 + digits.len()..].chars().next();
        if prev_ok && !digits.is_empty() && !after.is_some_and(is_word) {
            return true;
        }
    }
    for word in [
        "issues",
        "issue",
        "merge requests",
        "merge request",
        "mrs",
        "mr",
    ] {
        let mut from = 0;
        while let Some(pos) = text[from..].find(word) {
            let start = from + pos;
            let end = start + word.len();
            let before_ok = !text[..start].chars().next_back().is_some_and(is_word);
            let after_ok = !text[end..].chars().next().is_some_and(is_word);
            if before_ok && after_ok {
                return true;
            }
            from = start + 1;
        }
    }
    false
}

/// The recorded task memory for a linked issue as one short string, or `""`
/// when the project is not configured here or nothing is recorded.
fn default_memory(path: &str, iid: &str) -> Result<String, String> {
    let Ok(cfg) = config::Config::load(&config::default_config_path()) else {
        return Ok(String::new());
    };
    let projects = cfg
        .top_level("projects")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let Ok(iid) = iid.parse::<i64>() else {
        return Ok(String::new());
    };
    for (alias, project) in &projects {
        if project.get("project_id").and_then(Value::as_str) == Some(path) {
            if let Some(entry) = memory_store::get_task_memory(alias, iid, None) {
                let desc = entry.to_value()["description"].clone();
                let desc = if py_truthy(&desc) {
                    py_str(&desc)
                } else {
                    String::new()
                };
                return Ok(format!("{desc} {}", entry.body).trim().to_string());
            }
        }
    }
    Ok(String::new())
}

// ---- cleaning model output ----------------------------------------------------------

/// `^\s*(?:[-*•‣]+|\d{1,2}[.)])\s+` stripped from the start.
fn strip_lead_mark(text: &str) -> &str {
    let t = text.trim_start();
    let after_mark = {
        let run = t
            .chars()
            .take_while(|c| matches!(c, '-' | '*' | '\u{2022}' | '\u{2023}'))
            .count();
        if run > 0 {
            Some(t.char_indices().nth(run).map_or(t.len(), |(i, _)| i))
        } else {
            let d = t.chars().take_while(char::is_ascii_digit).count();
            let punct = t[d..].chars().next();
            ((1..=2).contains(&d) && matches!(punct, Some('.') | Some(')'))).then_some(d + 1)
        }
    };
    let Some(at) = after_mark else { return text };
    let rest = &t[at..];
    let trimmed = rest.trim_start();
    if trimmed.len() < rest.len() {
        trimmed
    } else {
        text
    }
}

fn is_placeholder(text: &str) -> bool {
    fn core(s: &str) -> bool {
        const WORDS: [&str; 11] = [
            "none",
            "na",
            "n/a",
            "null",
            "nil",
            "unknown",
            "tbd",
            "tba",
            "undefined",
            "nothing",
            "empty",
        ];
        if WORDS.contains(&s) {
            return true;
        }
        if !s.is_empty() && (s.chars().all(|c| c == '-') || s.chars().all(|c| c == '.')) {
            return true;
        }
        if let Some(rest) = s.strip_prefix("no ") {
            for w in [
                "provided",
                "available",
                "found",
                "identified",
                "specified",
                "mentioned",
                "listed",
            ] {
                if let Some(mid) = rest.strip_suffix(w) {
                    if mid.chars().count() <= 40
                        && mid
                            .chars()
                            .all(|c| is_word(c) || matches!(c, ' ' | ',' | '/' | '-'))
                    {
                        return true;
                    }
                }
            }
        }
        if let Some(w) = s.strip_prefix("not ") {
            return [
                "provided",
                "available",
                "specified",
                "applicable",
                "mentioned",
            ]
            .contains(&w);
        }
        false
    }
    let lower = text.to_lowercase();
    core(&lower) || lower.strip_suffix('.').is_some_and(core)
}

/// A model-written string with list markers stripped, or `""` when it is
/// empty, a placeholder or too short to mean anything.
fn clean_entry(value: &str, max_len: usize) -> String {
    let text = loopkit::chat_text(strip_lead_mark(value), max_len);
    if text.chars().count() < 3 || is_placeholder(&text) {
        String::new()
    } else {
        text
    }
}

fn clean_entries(values: Option<&Value>, max_len: usize, limit: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for v in values.and_then(Value::as_array).into_iter().flatten() {
        let text = v
            .as_str()
            .map(|s| clean_entry(s, max_len))
            .unwrap_or_default();
        if !text.is_empty() && seen.insert(text.to_lowercase()) {
            out.push(text);
        }
    }
    out.truncate(limit);
    out
}

/// Python's round-half-to-even for the "in N min" figure.
fn round_even(x: f64) -> i64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 && (r as i64) % 2 != 0 {
        r as i64 - x.signum() as i64
    } else {
        r as i64
    }
}

extern "C" {
    fn tzset();
}

/// The local zone abbreviation at `ts` (`strftime("%Z")`), or `""`.
fn zone_abbrev(ts: i64) -> String {
    // SAFETY: localtime_r fills a zeroed tm; tm_zone points into libc's tz data.
    unsafe {
        tzset();
        let t: libc::time_t = ts as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() || tm.tm_zone.is_null() {
            return String::new();
        }
        std::ffi::CStr::from_ptr(tm.tm_zone)
            .to_string_lossy()
            .into_owned()
    }
}

fn render(data: &Value, ctx: &LoopContext) -> String {
    let (ev, brief) = (&data["event"], &data["brief"]);
    let start = parse_time(ev.get("start"));
    let end = parse_time(ev.get("end"));
    let title = loopkit::chat_text(&field_text(ev, "title"), MAX_ENTRY).replace('*', "");
    let mut lines = vec![format!("*\u{1F4C5} Meeting Prep: {title}*"), String::new()];
    if let Some(start) = start {
        let local = start.with_timezone(&Local);
        let mut when = format!(
            "{} \u{b7} {}",
            local.format("%a, %b %-d"),
            local.format("%H:%M")
        );
        if let Some(end) = end {
            when.push('\u{2013}');
            when.push_str(&end.with_timezone(&Local).format("%H:%M").to_string());
        }
        let zone = zone_abbrev(start.timestamp());
        if !zone.is_empty() {
            when.push_str(&format!(" ({zone})"));
        }
        let minutes =
            round_even((start - ctx.now).num_milliseconds() as f64 / 1000.0 / 60.0).max(0);
        lines.push(format!("*Date & Time:* {when} \u{b7} in {minutes} min"));
    }
    let names: Vec<String> = ev
        .get("attendees")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|x| loopkit::chat_text(&py_str(x), 80).replace('*', ""))
        .filter(|n| !n.is_empty())
        .collect();
    if !names.is_empty() {
        let mut shown = names
            .iter()
            .take(MAX_ATTENDEES)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if names.len() > MAX_ATTENDEES {
            shown.push_str(&format!(" +{} more", names.len() - MAX_ATTENDEES));
        }
        lines.push(format!("*Attendees:* {shown}"));
    }
    let link = field_text(ev, "join_url");
    lines.push(format!(
        "*Meeting Link:* {}",
        if link.is_empty() {
            "_none in the invite_".to_string()
        } else {
            loopkit::slack_link(&link, "Join meeting")
        }
    ));
    let summary = field_text(brief, "summary");
    if !summary.is_empty() {
        lines.push(String::new());
        lines.push(format!("> {summary}"));
    }
    let mut section = |heading: &str, entries: Vec<String>, numbered: bool| {
        if entries.is_empty() {
            return;
        }
        lines.push(String::new());
        lines.push(format!("*{heading}*"));
        for (n, e) in entries.iter().enumerate() {
            lines.push(if numbered {
                format!("{}. {e}", n + 1)
            } else {
                format!("\u{2022} {e}")
            });
        }
    };
    let strings = |key: &str| -> Vec<String> {
        brief
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(py_str)
            .collect()
    };
    section("\u{1F4CB} Agenda", strings("agenda"), true);
    let open: Vec<String> = brief
        .get("open_items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|o| {
            let (link, text) = (field_text(o, "link"), field_text(o, "text"));
            if link.is_empty() {
                text
            } else {
                loopkit::slack_link(&link, &text)
            }
        })
        .collect();
    section("\u{1F513} Open items", open, false);
    section("\u{1F4AC} Raise", strings("talking_points"), false);
    section(
        "\u{21A9}\u{FE0F} From last time",
        strings("follow_ups"),
        false,
    );
    lines.join("\n")
}

// ---- the plugin ---------------------------------------------------------------------

fn today_row(aid: &str, ev: &Value) -> Option<Value> {
    if parse_time(ev.get("start")).is_none()
        || ev.get("self_response").and_then(Value::as_str) == Some("declined")
    {
        return None;
    }
    Some(json!({
        "key": format!("cal:{aid}:{}:{}", raw_str(ev, "id"), raw_str(ev, "start")),
        "title": loopkit::chat_text(&field_text(ev, "summary"), MAX_ENTRY),
        "start": raw(ev, "start"), "end": raw(ev, "end"),
        "join_url": loopkit::chat_url(&field_text(ev, "join_url")),
        "html_link": loopkit::chat_url(&field_text(ev, "html_link")),
    }))
}

fn wanted(ev: &Value, now: DateTime<Utc>, window_end: DateTime<Utc>, include_solo: bool) -> bool {
    let Some(start) = parse_time(ev.get("start")) else {
        return false;
    };
    if start <= now || start > window_end {
        return false;
    }
    if ev.get("self_response").and_then(Value::as_str) == Some("declined") {
        return false;
    }
    let others = ev
        .get("attendees")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|a| !truthy(a.get("self")));
    others || include_solo
}

impl LoopPlugin for CalendarPrep {
    fn loop_name(&self) -> &str {
        "meeting-prep-loop"
    }
    fn definition_dir(&self) -> &str {
        "calendar-prep"
    }
    fn output_keys(&self) -> &[&'static str] {
        &OUTPUT_KEYS
    }
    fn settings_fields(&self) -> &'static [Field] {
        &SETTINGS_FIELDS
    }

    fn discover(&self, ctx: &LoopContext) -> Result<Vec<WorkItem>, PluginError> {
        let settings = if ctx.settings.is_object() {
            ctx.settings.clone()
        } else {
            json!({})
        };
        let lead = lead_minutes(&settings);
        let mail_days = mail_days(&settings);
        let include_solo = field_text(&settings, "include_solo").trim().to_lowercase() == "yes";
        let only = field_text(&settings, "calendar_account").trim().to_string();
        let window_end = ctx.now + Duration::minutes(lead);
        // One call covers the whole local day: it feeds both the briefs (only
        // meetings inside the lead window) and the Live page's agenda. Like
        // Python's `.astimezone().replace(hour=0, ...)` the current offset is kept.
        let local = ctx.now.with_timezone(&Local);
        let offset = *local.offset();
        let tz = FixedOffset::east_opt(offset.local_minus_utc()).expect("valid offset");
        let day_start = tz
            .from_local_datetime(&local.date_naive().and_time(NaiveTime::MIN))
            .single()
            .expect("fixed offsets are unambiguous");
        let day_end = day_start + Duration::days(1);
        let fetch_end = day_end.with_timezone(&Utc).max(window_end);
        let fetch_end_iso = if fetch_end == day_end.with_timezone(&Utc) {
            day_end.to_rfc3339_opts(SecondsFormat::Secs, false)
        } else {
            window_end.to_rfc3339_opts(SecondsFormat::Secs, false)
        };
        let day_start_iso = day_start.to_rfc3339_opts(SecondsFormat::Secs, false);
        let mut today: Vec<Option<Value>> = Vec::new();
        let mut fetched = false;
        let series = read_json(&self.series_file(ctx));
        let mut gitlabs: Option<Vec<Box<dyn Connector>>> = None;
        let mut items = Vec::new();
        for account in self.connectors.accounts_with_capability("calendar") {
            let aid = account.id.clone();
            if !only.is_empty() && aid != only {
                continue;
            }
            let events = self
                .connectors
                .load(&aid)
                .map_err(|f| load_class(&f))
                .and_then(|c| {
                    c.list_events(&day_start_iso, &fetch_end_iso, 50)
                        .map_err(|e| e.class_name())
                });
            let events = match events {
                Ok(e) => e,
                Err(class) => {
                    ctx.log(&format!("calendar-prep: calendar {aid} failed: {class}"));
                    continue;
                }
            };
            fetched = true;
            for ev in &events {
                today.push(today_row(&aid, ev));
                if !wanted(ev, ctx.now, window_end, include_solo) {
                    continue;
                }
                let g = gitlabs.get_or_insert_with(|| self.gitlab_connectors(ctx));
                items.push(self.item(&aid, ev, g, mail_days, &series, ctx));
            }
        }
        if fetched {
            self.write_today(ctx, day_start, &today);
        }
        Ok(items)
    }

    fn after_item(
        &self,
        item: &WorkItem,
        answer: &Value,
        ctx: &LoopContext,
    ) -> Result<Outcome, PluginError> {
        let offered: Vec<&str> = item
            .payload
            .get("gitlab")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| r.get("url").and_then(Value::as_str))
            .collect();
        let mut open_items: Vec<Value> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for o in answer
            .get("open_items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let text = if o.is_object() {
                clean_entry(&field_text(o, "text"), MAX_ENTRY)
            } else {
                String::new()
            };
            if !text.is_empty() && seen.insert(text.to_lowercase()) {
                let link = match o.get("link") {
                    Some(Value::String(l)) if offered.contains(&l.as_str()) => l.clone(),
                    _ => String::new(),
                };
                open_items.push(json!({"text": text, "link": link}));
            }
        }
        open_items.truncate(MAX_ENTRIES);
        let brief = json!({
            "summary": clean_entry(&field_text(answer, "summary"), MAX_SUMMARY),
            "agenda": clean_entries(answer.get("agenda"), MAX_ENTRY, MAX_ENTRIES),
            "open_items": open_items,
            "talking_points": clean_entries(answer.get("talking_points"), MAX_ENTRY, MAX_ENTRIES),
            "follow_ups": clean_entries(answer.get("follow_ups"), MAX_ENTRY, MAX_ENTRIES),
        });
        let series_id = field_text(&item.payload, "series_id");
        if !series_id.is_empty() {
            let path = self.series_file(ctx);
            let mut series = read_json(&path);
            series.insert(
                series_id,
                json!({"date": ctx.now.date_naive().to_string(), "summary": brief["summary"],
                       "follow_ups": brief["follow_ups"]}),
            );
            write_json_atomic(&path, &Value::Object(series))
                .map_err(|e| PluginError::new("OSError", e.to_string()))?;
        }
        self.record_brief(ctx, &item.key, &brief);
        let ev = &item.payload["event"];
        let summary: String = brief["summary"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(120)
            .collect();
        let join = field_text(ev, "join_url");
        let mut event = Map::new();
        for k in ["title", "start", "end", "join_url", "attendees"] {
            event.insert(k.into(), ev[k].clone());
        }
        Ok(Outcome::new(
            &item.key,
            Status::Done,
            if summary.is_empty() {
                "brief written".to_string()
            } else {
                summary
            },
        )
        .with_url(if join.is_empty() {
            item.url.clone()
        } else {
            join
        })
        .with_data(json!({"label": ev["title"], "event": event, "brief": brief})))
    }

    fn digest(&self, outcomes: &[Outcome], ctx: &LoopContext) -> Option<String> {
        let blocks: Vec<String> = outcomes
            .iter()
            .filter(|o| o.status == Status::Done && o.data.get("brief").is_some_and(py_truthy))
            .map(|o| render(&o.data, ctx))
            .collect();
        (!blocks.is_empty()).then(|| blocks.join(&format!("\n\n{}\n\n", "\u{2500}".repeat(10))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{
        gitlab, google_calendar, Account, ConnResult, ConnectorError, LoadFailure, TypeInfo,
    };
    use crate::loop_definition::LoopDefinition;
    use std::sync::Mutex;

    static TZ_LOCK: Mutex<()> = Mutex::new(());

    fn with_tz<R: Send + 'static>(tz: &str, f: impl FnOnce() -> R + Send + 'static) -> R {
        let _g = TZ_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var("TZ").ok();
        std::env::set_var("TZ", tz);
        let r = std::thread::spawn(f).join().unwrap();
        match old {
            Some(v) => std::env::set_var("TZ", v),
            None => std::env::remove_var("TZ"),
        }
        r
    }

    const GL: &str = "https://gitlab.example.com";
    const YAML: &str = include_str!("../../../../loops/calendar-prep/loop.yaml");

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 7, 9, 0, 0).unwrap()
    }

    fn iso(minutes: i64) -> String {
        (now() + Duration::minutes(minutes))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    fn event(eid: &str) -> Value {
        event_with(
            eid,
            30,
            "accepted",
            "",
            "",
            "https://meet.google.com/abc",
            None,
        )
    }

    fn event_with(
        eid: &str,
        start: i64,
        self_response: &str,
        description: &str,
        recurring: &str,
        join_url: &str,
        attendees: Option<Value>,
    ) -> Value {
        let attendees = attendees.unwrap_or_else(|| {
            json!([{"name": "", "email": "me@example.com", "response": self_response, "self": true},
                   {"name": "Ann", "email": "ann@example.com", "response": "accepted", "self": false}])
        });
        json!({"id": eid, "summary": "Sync", "start": iso(start), "end": iso(start + 30),
               "html_link": format!("https://calendar.google.com/{eid}"),
               "attendees_count": attendees.as_array().unwrap().len(),
               "recurring_event_id": recurring, "description": description, "location": "",
               "join_url": join_url, "organizer": "boss@example.com", "attendees": attendees,
               "self_response": self_response})
    }

    #[derive(Default)]
    struct Shared {
        calls: Mutex<Vec<(String, String)>>,
        api_calls: Mutex<Vec<String>>,
    }

    struct Fake {
        info: &'static TypeInfo,
        account: Account,
        events: Vec<Value>,
        fail: bool,
        shared: Arc<Shared>,
    }

    impl Connector for Fake {
        fn info(&self) -> &'static TypeInfo {
            self.info
        }
        fn account(&self) -> &Account {
            &self.account
        }
        fn base_url(&self) -> ConnResult<String> {
            Ok(GL.into())
        }
        fn list_events(&self, a: &str, b: &str, _n: u32) -> ConnResult<Vec<Value>> {
            self.shared.calls.lock().unwrap().push((a.into(), b.into()));
            if self.fail {
                return Err(ConnectorError::Other {
                    class: "RuntimeError".into(),
                    detail: "boom https://secret".into(),
                });
            }
            Ok(self.events.clone())
        }
        fn api(&self, _m: &str, path: &str, _o: &ApiOpts) -> ConnResult<Value> {
            self.shared.api_calls.lock().unwrap().push(path.to_string());
            if self.fail {
                return Err(ConnectorError::Other {
                    class: "RuntimeError".into(),
                    detail: "nope".into(),
                });
            }
            if path.contains("/merge_requests/") {
                return Ok(
                    json!({"title": "Fix login", "state": "opened", "updated_at": "2026-10-06T10:00:00Z",
                    "reviewers": [{"username": "me"}], "assignees": [], "detailed_merge_status": "mergeable",
                    "web_url": format!("{GL}/g/p/-/merge_requests/7")}),
                );
            }
            Ok(
                json!({"title": "Login broken", "state": "opened", "updated_at": "2026-10-05T10:00:00Z",
                      "assignees": [{"username": "ann"}], "web_url": format!("{GL}/g/p/-/issues/3")}),
            )
        }
    }

    struct CalSpec {
        id: String,
        events: Vec<Value>,
        fail: bool,
        shared: Arc<Shared>,
    }

    struct Source {
        calendars: Vec<CalSpec>,
        gitlab_fail: bool,
        gitlab: Arc<Shared>,
    }

    impl ConnectorSource for Source {
        fn accounts_with_capability(&self, cap: &str) -> Vec<Account> {
            match cap {
                "calendar" => self
                    .calendars
                    .iter()
                    .map(|c| Account::new(&c.id, "google_calendar"))
                    .collect(),
                "issues" => vec![Account::new("gl", "gitlab")],
                _ => vec![],
            }
        }
        fn load(&self, id: &str) -> Result<Box<dyn Connector>, LoadFailure> {
            if let Some(c) = self.calendars.iter().find(|c| c.id == id) {
                return Ok(Box::new(Fake {
                    info: &google_calendar::INFO,
                    account: Account::new(id, "google_calendar"),
                    events: c.events.clone(),
                    fail: c.fail,
                    shared: c.shared.clone(),
                }));
            }
            Ok(Box::new(Fake {
                info: &gitlab::INFO,
                account: Account::new(id, "gitlab"),
                events: vec![],
                fail: self.gitlab_fail,
                shared: self.gitlab.clone(),
            }))
        }
    }

    struct Harness {
        plugin: CalendarPrep,
        mail_calls: Arc<Mutex<Vec<(Vec<String>, i64, usize)>>>,
        gitlab: Arc<Shared>,
        cals: Vec<Arc<Shared>>,
        dir: tempfile::TempDir,
    }

    struct Opts {
        calendars: Vec<(&'static str, Vec<Value>, bool)>,
        gitlab_fail: bool,
        mail_rows: Vec<Value>,
        mail_error: Option<&'static str>,
        cli: &'static str,
    }

    impl Default for Opts {
        fn default() -> Self {
            Self {
                calendars: vec![("cal", vec![event("ev1")], false)],
                gitlab_fail: false,
                mail_rows: vec![],
                mail_error: None,
                cli: "claude",
            }
        }
    }

    fn harness(o: Opts) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let gitlab = Arc::new(Shared::default());
        let cals: Vec<Arc<Shared>> = o
            .calendars
            .iter()
            .map(|_| Arc::new(Shared::default()))
            .collect();
        let source = Source {
            calendars: o
                .calendars
                .iter()
                .zip(&cals)
                .map(|((id, events, fail), s)| CalSpec {
                    id: id.to_string(),
                    events: events.clone(),
                    fail: *fail,
                    shared: s.clone(),
                })
                .collect(),
            gitlab_fail: o.gitlab_fail,
            gitlab: gitlab.clone(),
        };
        let mail_calls: Arc<Mutex<Vec<(Vec<String>, i64, usize)>>> = Default::default();
        let (mc, rows, err) = (mail_calls.clone(), o.mail_rows.clone(), o.mail_error);
        let cli = o.cli.to_string();
        let plugin = CalendarPrep::new(Arc::new(source))
            .with_mail_fn(Arc::new(move |a, d, l| {
                mc.lock().unwrap().push((a.to_vec(), d, l));
                match err {
                    Some(_) => Err("RuntimeError".to_string()),
                    None => Ok(rows.clone()),
                }
            }))
            .with_cli_fn(Arc::new(move || cli.clone()))
            .with_series_path(dir.path().join("series.json"))
            .with_today_path(dir.path().join("today.json"))
            .with_briefs_path(dir.path().join("briefs.json"));
        Harness {
            plugin,
            mail_calls,
            gitlab,
            cals,
            dir,
        }
    }

    fn ctx_with(settings: Value) -> (LoopContext, Arc<Mutex<Vec<String>>>) {
        let logs: Arc<Mutex<Vec<String>>> = Default::default();
        let l = logs.clone();
        let ctx = LoopContext {
            loop_name: "meeting-prep-loop".into(),
            run_id: "r".into(),
            now: now(),
            definition: Arc::new(LoopDefinition::from_yaml_str(YAML, "t").unwrap()),
            settings,
            history_dir: PathBuf::from("/nonexistent"),
            log: Arc::new(move |m| l.lock().unwrap().push(m.to_string())),
            repo_root: PathBuf::from("/nonexistent"),
            force: false,
            call_budget_usd: None,
            model: None,
            state_dir: None,
        };
        (ctx, logs)
    }

    fn ctx() -> LoopContext {
        ctx_with(json!({})).0
    }

    fn event_id(key: &str) -> String {
        key.split(':').nth(2).unwrap().to_string()
    }

    fn keys(items: &[WorkItem]) -> Vec<String> {
        items.iter().map(|i| event_id(&i.key)).collect()
    }

    fn discover(h: &Harness, settings: Value) -> Vec<WorkItem> {
        h.plugin.discover(&ctx_with(settings).0).unwrap()
    }

    fn one_cal(events: Vec<Value>) -> Opts {
        Opts {
            calendars: vec![("cal", events, false)],
            ..Opts::default()
        }
    }

    #[test]
    fn one_item_per_upcoming_meeting() {
        let items = discover(&harness(Opts::default()), json!({}));
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.key, format!("cal:cal:ev1:{}", iso(30)));
        assert_eq!(
            (item.title.as_str(), item.url.as_str()),
            ("Sync", "https://calendar.google.com/ev1")
        );
        let ev = &item.payload["event"];
        assert_eq!(ev["title"], "Sync");
        assert_eq!(ev["join_url"], "https://meet.google.com/abc");
        assert_eq!(ev["attendees"], json!(["Ann"]));
    }

    #[test]
    fn fetches_the_whole_local_day_in_one_call() {
        let (calls, midnight) = with_tz("America/New_York", || {
            let h = harness(Opts::default());
            discover(&h, json!({"lead_minutes": "60"}));
            let c = h.cals[0].calls.lock().unwrap().clone();
            (c, ())
        });
        let _ = midnight;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "2026-10-07T00:00:00-04:00");
        assert_eq!(calls[0].1, "2026-10-08T00:00:00-04:00");
    }

    #[test]
    fn only_briefs_the_lead_window() {
        let events = vec![
            event_with("soon", 50, "accepted", "", "", "x", None),
            event_with("later", 61, "accepted", "", "", "x", None),
            event_with("earlier", -120, "accepted", "", "", "x", None),
        ];
        let h = harness(one_cal(events));
        assert_eq!(keys(&discover(&h, json!({"lead_minutes": "60"}))), ["soon"]);
    }

    fn today(h: &Harness) -> Value {
        serde_json::from_str(&std::fs::read_to_string(h.dir.path().join("today.json")).unwrap())
            .unwrap()
    }

    #[test]
    fn records_todays_meetings_for_the_live_page() {
        let mut allday = event("allday");
        allday["start"] = json!("2026-10-07");
        allday["end"] = json!("2026-10-08");
        let events = vec![
            event_with(
                "past",
                -120,
                "accepted",
                "",
                "",
                "https://meet.google.com/abc",
                None,
            ),
            event_with(
                "now",
                -10,
                "accepted",
                "",
                "",
                "https://meet.google.com/abc",
                None,
            ),
            event("soon"),
            allday,
            event_with("declined", 30, "declined", "", "", "x", None),
        ];
        let h = harness(one_cal(events));
        discover(&h, json!({}));
        let data = today(&h);
        assert_eq!(data["generated_at"], "2026-10-07T09:00:00+00:00");
        let ids: Vec<_> = data["meetings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| event_id(m["key"].as_str().unwrap()))
            .collect();
        assert_eq!(ids, ["past", "now", "soon"]);
        let soon = &data["meetings"][2];
        assert_eq!(soon["key"], format!("cal:cal:soon:{}", iso(30)));
        assert_eq!(soon["title"], "Sync");
        assert_eq!(
            (
                soon["start"].as_str().unwrap(),
                soon["end"].as_str().unwrap()
            ),
            (iso(30).as_str(), iso(60).as_str())
        );
        assert_eq!(soon["join_url"], "https://meet.google.com/abc");
        assert_eq!(soon["html_link"], "https://calendar.google.com/soon");
    }

    #[test]
    fn a_failing_calendar_keeps_the_previous_snapshot() {
        let h = harness(one_cal(vec![event("keep")]));
        discover(&h, json!({}));
        let failing = Opts {
            calendars: vec![("cal", vec![], true)],
            ..Opts::default()
        };
        let h2 = harness(failing);
        // same directory: point the second plugin at the first's files
        let p2 = CalendarPrep::new(Arc::new(Source {
            calendars: vec![CalSpec {
                id: "cal".into(),
                events: vec![],
                fail: true,
                shared: Default::default(),
            }],
            gitlab_fail: false,
            gitlab: Default::default(),
        }))
        .with_today_path(h.dir.path().join("today.json"))
        .with_series_path(h2.dir.path().join("series.json"))
        .with_briefs_path(h2.dir.path().join("briefs.json"))
        .with_cli_fn(Arc::new(|| "claude".into()));
        p2.discover(&ctx()).unwrap();
        let ids: Vec<_> = today(&h)["meetings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| event_id(m["key"].as_str().unwrap()))
            .collect();
        assert_eq!(ids, ["keep"]);
    }

    #[test]
    fn skips_all_day_started_declined_and_solo_events() {
        let solo =
            json!([{"name": "", "email": "me@example.com", "response": "accepted", "self": true}]);
        let mut allday = event("allday");
        allday["start"] = json!("2026-10-07");
        allday["end"] = json!("2026-10-08");
        let events = vec![
            allday,
            event_with("started", -5, "accepted", "", "", "x", None),
            event_with("declined", 30, "declined", "", "", "x", None),
            event_with("solo", 30, "accepted", "", "", "x", Some(solo)),
            event_with("far", 90, "accepted", "", "", "x", None),
            event("ok"),
        ];
        let h = harness(one_cal(events.clone()));
        assert_eq!(keys(&discover(&h, json!({}))), ["ok"]);
        let h = harness(one_cal(events));
        assert_eq!(
            keys(&discover(&h, json!({"include_solo": "yes"}))),
            ["solo", "ok"]
        );
    }

    #[test]
    fn invalid_lead_minutes_falls_back_and_is_clamped() {
        assert_eq!(lead_minutes(&json!({"lead_minutes": "soon"})), 45);
        assert_eq!(lead_minutes(&json!({"lead_minutes": "5"})), 15);
        assert_eq!(lead_minutes(&json!({"lead_minutes": "500"})), 120);
        assert_eq!(mail_days(&json!({"mail_days": "x"})), 30);
        assert_eq!(mail_days(&json!({"mail_days": "0"})), 0);
    }

    #[test]
    fn calendar_account_setting_limits_the_calendars() {
        let h = harness(Opts {
            calendars: vec![
                ("a", vec![event("a")], false),
                ("b", vec![event("b")], false),
            ],
            ..Opts::default()
        });
        let items = discover(&h, json!({"calendar_account": "b"}));
        assert_eq!(
            items
                .iter()
                .map(|i| i.key.split(':').nth(1).unwrap().to_string())
                .collect::<Vec<_>>(),
            ["b"]
        );
        assert!(h.cals[0].calls.lock().unwrap().is_empty());
    }

    #[test]
    fn one_failing_calendar_does_not_stop_the_others() {
        let h = harness(Opts {
            calendars: vec![("a", vec![], true), ("b", vec![event("ev1")], false)],
            ..Opts::default()
        });
        let (c, logs) = ctx_with(json!({}));
        let items = h.plugin.discover(&c).unwrap();
        assert_eq!(items.len(), 1);
        let logs = logs.lock().unwrap();
        assert!(
            logs.iter().any(|m| m.contains("RuntimeError"))
                && !logs.iter().any(|m| m.contains("secret"))
        );
    }

    #[test]
    fn linked_gitlab_work_is_fetched_for_matching_urls_only() {
        let desc = format!("Agenda: {GL}/g/p/-/issues/3 and {GL}/g/p/-/merge_requests/7 and https://other.example.com/x/-/issues/1");
        let h = harness(one_cal(vec![event_with(
            "ev1", 30, "accepted", &desc, "", "x", None,
        )]));
        let item = discover(&h, json!({})).remove(0);
        assert_eq!(
            *h.gitlab.api_calls.lock().unwrap(),
            [
                "/projects/g%2Fp/issues/3",
                "/projects/g%2Fp/merge_requests/7"
            ]
        );
        let rows = item.payload["gitlab"].as_array().unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r["kind"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["issue", "merge_request"]
        );
        assert_eq!(rows[0]["url"], format!("{GL}/g/p/-/issues/3"));
        assert_eq!(rows[0]["assignees"], json!(["ann"]));
        assert_eq!(rows[1]["merge_status"], "mergeable");
        assert_eq!(rows[1]["reviewers"], json!(["me"]));
    }

    #[test]
    fn gitlab_links_are_capped_and_failures_noted() {
        let desc = (1..=8)
            .map(|n| format!("{GL}/g/p/-/issues/{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut o = one_cal(vec![event_with(
            "ev1", 30, "accepted", &desc, "", "x", None,
        )]);
        o.gitlab_fail = true;
        let h = harness(o);
        let item = discover(&h, json!({})).remove(0);
        assert_eq!(h.gitlab.api_calls.lock().unwrap().len(), 5);
        assert_eq!(item.payload["gitlab"], json!([]));
        assert_eq!(item.payload["gitlab_error"], "GitLab links unavailable");
    }

    #[test]
    fn mail_with_attendees_excludes_me() {
        let mut o = Opts::default();
        o.mail_rows = vec![
            json!({"subject": "Plan for issue #3", "from": "Ann <ann@example.com>",
                                  "date": "2026-10-06T07:00:00+00:00", "snippet": "x".repeat(900)}),
        ];
        let h = harness(o);
        let item = discover(&h, json!({})).remove(0);
        assert_eq!(
            *h.mail_calls.lock().unwrap(),
            [(
                vec![
                    "boss@example.com".to_string(),
                    "ann@example.com".to_string()
                ],
                30,
                10
            )]
        );
        let m = &item.payload["mail"][0];
        assert_eq!(m["subject"], "Plan for issue #3");
        assert!(m["snippet"].as_str().unwrap().chars().count() <= 300);
    }

    #[test]
    fn mail_is_skipped_without_claude_or_when_turned_off() {
        let h = harness(Opts {
            cli: "codex",
            ..Opts::default()
        });
        let item = discover(&h, json!({})).remove(0);
        assert!(h.mail_calls.lock().unwrap().is_empty());
        assert_eq!(item.payload["mail"], json!([]));
        assert_eq!(item.payload["mail_error"], NO_CLAUDE);
        let h = harness(Opts::default());
        let item = discover(&h, json!({"mail_days": "0"})).remove(0);
        assert!(h.mail_calls.lock().unwrap().is_empty());
        assert!(item.payload.get("mail_error").is_none());
    }

    #[test]
    fn mail_failure_is_noted_not_fatal() {
        let h = harness(Opts {
            mail_error: Some("token https://x"),
            ..Opts::default()
        });
        let (c, logs) = ctx_with(json!({}));
        let item = h.plugin.discover(&c).unwrap().remove(0);
        assert_eq!(item.payload["mail"], json!([]));
        assert_eq!(item.payload["mail_error"], "mail unavailable");
        assert!(!logs.lock().unwrap().iter().any(|m| m.contains("token")));
    }

    #[test]
    fn description_is_trimmed() {
        let h = harness(one_cal(vec![event_with(
            "ev1",
            30,
            "accepted",
            &"d".repeat(9000),
            "",
            "x",
            None,
        )]));
        let item = discover(&h, json!({})).remove(0);
        assert!(
            item.payload["event"]["description"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= 4000
        );
    }

    fn answer() -> Value {
        let mut points = vec![json!("<!channel> ship it")];
        points.extend((0..9).map(|n| json!(format!("point {n}"))));
        json!({"summary": "Decide the login fix.", "agenda": ["Review MR"],
               "open_items": [{"text": "MR 7 waits on you", "link": format!("{GL}/g/p/-/merge_requests/7")},
                              {"text": "evil", "link": "https://evil.example.com"}],
               "talking_points": points, "follow_ups": ["Ann owes the test plan"]})
    }

    fn with_links(recurring: &str) -> (Harness, WorkItem) {
        let desc = format!("{GL}/g/p/-/merge_requests/7");
        let h = harness(one_cal(vec![event_with(
            "ev1",
            30,
            "accepted",
            &desc,
            recurring,
            "https://meet.google.com/abc",
            None,
        )]));
        let item = discover(&h, json!({})).remove(0);
        (h, item)
    }

    #[test]
    fn after_item_sanitizes_and_keeps_only_offered_links() {
        let (h, item) = with_links("series1");
        let out = h.plugin.after_item(&item, &answer(), &ctx()).unwrap();
        assert_eq!(out.status, Status::Done);
        let brief = &out.data["brief"];
        let links: Vec<_> = brief["open_items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["link"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            links,
            [format!("{GL}/g/p/-/merge_requests/7"), String::new()]
        );
        assert_eq!(brief["talking_points"].as_array().unwrap().len(), 5);
        assert!(!brief["talking_points"][0].as_str().unwrap().contains('<'));
    }

    #[test]
    fn past_brief_is_offered_next_time() {
        let (h, item) = with_links("series1");
        h.plugin.after_item(&item, &answer(), &ctx()).unwrap();
        let saved: Value = serde_json::from_str(
            &std::fs::read_to_string(h.dir.path().join("series.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["series1"]["summary"], "Decide the login fix.");
        assert_eq!(saved["series1"]["date"], "2026-10-07");
        let h2 = harness(one_cal(vec![event_with(
            "ev2", 30, "accepted", "", "series1", "x", None,
        )]));
        let p2 = CalendarPrep::new(h2.plugin.connectors.clone())
            .with_series_path(h.dir.path().join("series.json"))
            .with_today_path(h2.dir.path().join("today.json"))
            .with_cli_fn(Arc::new(|| "codex".into()));
        let nxt = p2.discover(&ctx()).unwrap().remove(0);
        assert_eq!(
            nxt.payload["last_time"]["follow_ups"],
            json!(["Ann owes the test plan"])
        );
    }

    #[test]
    fn digest_renders_the_brief() {
        with_tz("UTC", || {
            let (h, item) = with_links("series1");
            let out = h.plugin.after_item(&item, &answer(), &ctx()).unwrap();
            let text = h.plugin.digest(&[out], &ctx()).unwrap();
            let lines: Vec<&str> = text.split('\n').collect();
            assert_eq!(lines[0], "*\u{1F4C5} Meeting Prep: Sync*");
            assert_eq!(lines[1], "");
            assert!(
                lines[2].starts_with("*Date & Time:* ")
                    && !lines[2].contains("2026")
                    && lines[2].contains("in 30 min")
            );
            assert_eq!(
                lines[2],
                "*Date & Time:* Wed, Oct 7 \u{b7} 09:30\u{2013}10:00 (UTC) \u{b7} in 30 min"
            );
            assert_eq!(lines[3], "*Attendees:* Ann");
            assert_eq!(
                lines[4],
                "*Meeting Link:* <https://meet.google.com/abc|Join meeting>"
            );
            assert!(text.contains("> Decide the login fix."));
            assert!(text.contains("*\u{1F4CB} Agenda*\n1. Review MR"));
            assert!(text.contains(&format!(
                "*\u{1F513} Open items*\n\u{2022} <{GL}/g/p/-/merge_requests/7|MR 7 waits on you>"
            )));
            assert!(text.contains("\u{2022} evil"));
            assert!(text.contains("*\u{1F4AC} Raise*\n\u{2022} "));
            assert!(
                text.contains("*\u{21A9}\u{FE0F} From last time*\n\u{2022} Ann owes the test plan")
            );
            assert!(!text.contains("; ") && !text.contains("<!channel>") && !text.contains("**"));
            assert!(h
                .plugin
                .digest(&[Outcome::new("k", Status::Failed, "x")], &ctx())
                .is_none());
        });
    }

    #[test]
    fn digest_caps_attendees_and_marks_missing_link() {
        let names: Vec<Value> = (0..10)
            .map(
                |n| json!({"name": format!("P{n}"), "email": format!("p{n}@x.com"), "self": false}),
            )
            .collect();
        let h = harness(one_cal(vec![event_with(
            "ev1",
            30,
            "accepted",
            "",
            "",
            "",
            Some(Value::Array(names)),
        )]));
        let item = discover(&h, json!({})).remove(0);
        let out = h.plugin.after_item(&item, &answer(), &ctx()).unwrap();
        let text = h.plugin.digest(&[out], &ctx()).unwrap();
        assert!(text.contains("*Attendees:* P0, P1, P2, P3, P4, P5, P6, P7 +2 more"));
        assert!(text.contains("*Meeting Link:* _none in the invite_"));
    }

    #[test]
    fn invalid_entries_are_removed() {
        let (h, item) = with_links("series1");
        let answer = json!({"summary": "N/A", "agenda": ["1. Review MR", "- review mr", "None", "tbd", "ok"],
            "open_items": [{"text": "null", "link": ""}, {"text": "Real item", "link": ""}],
            "talking_points": ["No agenda provided", "Ask about the rollout date"],
            "follow_ups": ["", "unknown"]});
        let out = h.plugin.after_item(&item, &answer, &ctx()).unwrap();
        let brief = &out.data["brief"];
        assert_eq!(brief["summary"], "");
        assert_eq!(brief["agenda"], json!(["Review MR"]));
        assert_eq!(
            brief["open_items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o["text"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["Real item"]
        );
        assert_eq!(
            brief["talking_points"],
            json!(["Ask about the rollout date"])
        );
        assert_eq!(brief["follow_ups"], json!([]));
        let text = h.plugin.digest(&[out], &ctx()).unwrap();
        assert!(!text.contains("From last time") && !text.contains("N/A"));
    }

    #[test]
    fn records_the_brief_for_the_live_page() {
        let (h, item) = with_links("series1");
        h.plugin.after_item(&item, &answer(), &ctx()).unwrap();
        let saved: Value = serde_json::from_str(
            &std::fs::read_to_string(h.dir.path().join("briefs.json")).unwrap(),
        )
        .unwrap();
        let s = &saved[&item.key];
        assert_eq!(s["summary"], "Decide the login fix.");
        assert_eq!(s["prepared_at"], "2026-10-07T09:00:00+00:00");
        assert_eq!(s["agenda"], json!(["Review MR"]));
    }

    #[test]
    fn recorded_briefs_are_capped_to_the_newest() {
        let (h, item) = with_links("series1");
        let old: Map<String, Value> = (0..150)
            .map(|n| (format!("k{n}"), json!({"summary": "s", "prepared_at": format!("2026-01-{:02}T00:00:00+00:00", n % 28 + 1)})))
            .collect();
        std::fs::write(
            h.dir.path().join("briefs.json"),
            Value::Object(old).to_string(),
        )
        .unwrap();
        h.plugin.after_item(&item, &answer(), &ctx()).unwrap();
        let saved: Value = serde_json::from_str(
            &std::fs::read_to_string(h.dir.path().join("briefs.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.as_object().unwrap().len(), MAX_BRIEFS);
        assert!(saved.get(&item.key).is_some());
    }

    #[test]
    fn outcome_links_to_the_meeting_link_else_the_calendar_event() {
        let (h, item) = with_links("series1");
        assert_eq!(
            h.plugin.after_item(&item, &answer(), &ctx()).unwrap().url,
            "https://meet.google.com/abc"
        );
        let h = harness(one_cal(vec![event_with(
            "ev1", 30, "accepted", "", "", "", None,
        )]));
        let item = discover(&h, json!({})).remove(0);
        assert_eq!(
            h.plugin.after_item(&item, &answer(), &ctx()).unwrap().url,
            "https://calendar.google.com/ev1"
        );
    }

    #[test]
    fn settings_fields_declared() {
        let keys: Vec<_> = SETTINGS_FIELDS.iter().map(|f| f.key).collect();
        assert_eq!(
            keys,
            [
                "calendar_account",
                "lead_minutes",
                "mail_days",
                "include_solo"
            ]
        );
        assert!(SETTINGS_FIELDS.iter().all(|f| !f.required));
        let h = harness(Opts::default());
        assert_eq!(h.plugin.settings_fields().len(), 4);
    }

    #[test]
    fn definition_and_prompt_load() {
        let d = LoopDefinition::from_yaml_str(YAML, "t").unwrap();
        assert_eq!(d.name, "meeting-prep-loop");
        let prompt = include_str!("../../../../loops/calendar-prep/prompt.md");
        assert!(prompt.contains("{{item_json}}") && prompt.contains("untrusted"));
    }

    #[test]
    fn template_registers_the_loop_disabled_every_15_minutes() {
        let text = include_str!("../../../../config/loops.json.template");
        let v: Value = serde_json::from_str(text).unwrap();
        let loops = v.get("loops").unwrap_or(&v).as_array().unwrap();
        let entry = loops
            .iter()
            .find(|l| l["name"] == "meeting-prep-loop")
            .unwrap();
        assert_eq!(entry["entry_point"], "bin.loop_plugins.calendar_prep");
        assert_eq!(entry["enabled"], false);
        assert_eq!(
            entry["schedule"],
            json!({"frequency": "hourly", "interval_minutes": 15})
        );
        assert_eq!(entry["requires"], json!(["calendar"]));
        assert_eq!(entry["routes_notifications"], true);
    }

    #[test]
    fn mail_unrelated_to_issues_is_dropped() {
        let mut o = Opts::default();
        o.mail_rows = vec![
            json!({"subject": "Lunch?", "from": "Ann", "date": "d", "snippet": "pizza"}),
            json!({"subject": "Re: MR !7 review", "from": "Ann", "date": "d", "snippet": ""}),
        ];
        let h = harness(o);
        let item = discover(&h, json!({})).remove(0);
        let subjects: Vec<_> = item.payload["mail"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["subject"].as_str().unwrap())
            .collect();
        assert_eq!(subjects, ["Re: MR !7 review"]);
    }

    #[test]
    fn linked_issue_carries_its_recorded_memory() {
        let desc = format!("{GL}/g/p/-/issues/3 {GL}/g/p/-/merge_requests/7");
        let h = harness(one_cal(vec![event_with(
            "ev1", 30, "accepted", &desc, "", "x", None,
        )]));
        let plugin = CalendarPrep::new(h.plugin.connectors.clone())
            .with_cli_fn(Arc::new(|| "codex".into()))
            .with_today_path(h.dir.path().join("t.json"))
            .with_series_path(h.dir.path().join("s.json"))
            .with_memory_fn(Arc::new(|path, iid| {
                Ok(if (path, iid) == ("g/p", "3") {
                    "Fix needs a migration"
                } else {
                    "x"
                }
                .to_string())
            }));
        let rows = plugin.discover(&ctx()).unwrap().remove(0).payload["gitlab"].clone();
        assert_eq!(rows[0]["memory"], "Fix needs a migration");
        assert!(rows[1].get("memory").is_none());
    }

    #[test]
    fn helper_parsers_match_the_python_regexes() {
        assert_eq!(
            find_urls("see https://a.io/x?y=1, (https://b.io/z) and http://c.io"),
            ["https://a.io/x?y=1,", "https://b.io/z", "http://c.io"]
        );
        assert_eq!(
            parse_gitlab_path("g/p/-/issues/3"),
            Some(("g/p".into(), "issues".into(), "3".into()))
        );
        assert_eq!(
            parse_gitlab_path("g/p/-/merge_requests/12/diffs?x=1"),
            Some(("g/p".into(), "merge_requests".into(), "12".into()))
        );
        assert_eq!(parse_gitlab_path("g/p/-/issues/3a"), None);
        assert_eq!(parse_gitlab_path("g/p/-/pipelines/3"), None);
        for yes in [
            "issue #3",
            "see (#12)",
            "!7 review",
            "Merge Requests",
            "MRs pending",
            "a/-/issues/9",
            "x\n[!5]",
        ] {
            assert!(issue_related(&json!({"subject": yes})), "{yes}");
        }
        for no in ["Lunch?", "tissue paper", "C#3", "#12a", "submrx"] {
            assert!(!issue_related(&json!({"subject": no})), "{no}");
        }
        assert_eq!(strip_lead_mark("  \u{2022}  hi"), "hi");
        assert_eq!(strip_lead_mark("12) item"), "item");
        assert_eq!(strip_lead_mark("123. item"), "123. item");
        assert_eq!(strip_lead_mark("-x"), "-x");
        for p in [
            "N/A",
            "none.",
            "...",
            "---",
            "No agenda provided",
            "not applicable",
            "TBD",
        ] {
            assert!(is_placeholder(p), "{p}");
        }
        assert!(
            !is_placeholder("Ask about the rollout date") && !is_placeholder("nothing special")
        );
        assert_eq!(round_even(2.5), 2);
        assert_eq!(round_even(3.5), 4);
        assert_eq!(round_even(29.6), 30);
    }

    #[test]
    fn unsupported_load_failure_is_logged_by_class() {
        struct Broken;
        impl ConnectorSource for Broken {
            fn accounts_with_capability(&self, cap: &str) -> Vec<Account> {
                match cap {
                    "calendar" => vec![Account::new("cal", "google_calendar")],
                    _ => vec![],
                }
            }
            fn load(&self, _id: &str) -> Result<Box<dyn Connector>, LoadFailure> {
                Err(LoadFailure::Config("secret detail".into()))
            }
        }
        let (c, logs) = ctx_with(json!({}));
        let items = CalendarPrep::new(Arc::new(Broken)).discover(&c).unwrap();
        assert!(items.is_empty());
        let logs = logs.lock().unwrap();
        assert!(
            logs.iter().any(|m| m.contains("ConnectorConfigError"))
                && !logs.iter().any(|m| m.contains("secret"))
        );
    }
}
