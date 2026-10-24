//! Daily Digest loop on LoopKit (mirrors `bin/loop_plugins/daily_digest.py`):
//! gathers todos / assigned issues / MRs / today's meetings / yesterday's
//! Loop X outcomes into one payload, asks the model for a brief and sends it
//! through the loop's notifier.
#![allow(
    clippy::type_complexity,
    clippy::field_reassign_with_default,
    clippy::let_unit_value
)]

use crate::connectors::{Account, ApiOpts, Connector, ConnectorSource, LoadFailure};
use crate::loopkit::{self, LoopContext, LoopPlugin, Outcome, PluginError, Status, WorkItem};
use crate::loops_config::{py_str, py_truthy};
use crate::{events, pydumps};
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveTime, SecondsFormat, TimeZone, Utc};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const MAX_PAYLOAD_BYTES: usize = 60_000;
const SECTIONS: [(&str, &str); 5] = [
    ("needs_you", "Needs you"),
    ("waiting_on_others", "Waiting on others"),
    ("meetings", "Today's meetings"),
    ("loop_x_did", "What Loop X did"),
    ("fyi", "FYI"),
];
const SHORT_LABELS: [(&str, &str); 5] = [
    ("needs_you", "need you"),
    ("waiting_on_others", "waiting"),
    ("meetings", "meetings"),
    ("loop_x_did", "Loop X"),
    ("fyi", "FYI"),
];
const PER_PAGE: &str = "per_page=50";
const OUTPUT_KEYS: [&str; 5] = [
    "needs_you",
    "waiting_on_others",
    "loop_x_did",
    "fyi",
    "meetings",
];

/// A data source that may fail; `Err` carries only the exception class name.
pub type SourceFn = Arc<dyn Fn(&LoopContext) -> Result<Value, String> + Send + Sync>;
/// `calendar_accounts_fn`: the calendar accounts to read (`Err` = none).
pub type CalendarAccountsFn = Arc<dyn Fn() -> Result<Vec<Account>, String> + Send + Sync>;

pub struct DailyDigest {
    connectors: Arc<dyn ConnectorSource>,
    loop_x_fn: Option<SourceFn>,
    inbox_fn: Option<SourceFn>,
    topics_fn: Option<SourceFn>,
    calendar_accounts_fn: Option<CalendarAccountsFn>,
    events_dir: Option<PathBuf>,
}

impl DailyDigest {
    pub fn new(connectors: Arc<dyn ConnectorSource>) -> Self {
        Self {
            connectors,
            loop_x_fn: None,
            inbox_fn: None,
            topics_fn: None,
            calendar_accounts_fn: None,
            events_dir: None,
        }
    }
    pub fn with_loop_x_fn(mut self, f: SourceFn) -> Self {
        self.loop_x_fn = Some(f);
        self
    }
    pub fn with_inbox_fn(mut self, f: SourceFn) -> Self {
        self.inbox_fn = Some(f);
        self
    }
    pub fn with_topics_fn(mut self, f: SourceFn) -> Self {
        self.topics_fn = Some(f);
        self
    }
    pub fn with_calendar_accounts_fn(mut self, f: CalendarAccountsFn) -> Self {
        self.calendar_accounts_fn = Some(f);
        self
    }
    /// Event-ledger directory for the default Loop X source (tests).
    pub fn with_events_dir(mut self, dir: PathBuf) -> Self {
        self.events_dir = Some(dir);
        self
    }

    /// `collect(ctx, ...)`: the payload handed to the model.
    pub fn collect(&self, ctx: &LoopContext) -> Value {
        let yesterday = (local_now(ctx).date_naive() - Duration::days(1)).to_string();
        let mut accounts = Vec::new();
        for account in self.connectors.accounts_with_capability("issues") {
            let mut entry = Map::new();
            entry.insert("account".into(), account.id.clone().into());
            entry.insert("type".into(), account.kind.clone().into());
            let gathered = self
                .connectors
                .load(&account.id)
                .map_err(|f| load_class(&f))
                .and_then(|conn| {
                    if account.kind == "github" {
                        collect_github(conn.as_ref(), &yesterday)
                    } else {
                        collect_gitlab(conn.as_ref())
                    }
                });
            match gathered {
                Ok(extra) => entry.extend(extra),
                Err(class) => {
                    entry = Map::new();
                    entry.insert("account".into(), account.id.clone().into());
                    entry.insert("error".into(), class.into());
                }
            }
            accounts.push(Value::Object(entry));
        }
        let cal_accounts = match &self.calendar_accounts_fn {
            Some(f) => f().unwrap_or_default(),
            None => self.connectors.accounts_with_capability("calendar"),
        };
        let events_dir = self.events_dir.clone();
        let mut payload = Map::new();
        payload.insert(
            "date".into(),
            local_now(ctx).date_naive().to_string().into(),
        );
        payload.insert("accounts".into(), Value::Array(accounts));
        payload.insert(
            "meetings".into(),
            meetings(ctx, &cal_accounts, self.connectors.as_ref()),
        );
        payload.insert(
            "loop_x".into(),
            safe(self.loop_x_fn.clone(), ctx, move |c| {
                default_loop_x(c, events_dir.as_deref())
            }),
        );
        payload.insert(
            "inbox".into(),
            safe(self.inbox_fn.clone(), ctx, default_inbox),
        );
        payload.insert(
            "topics".into(),
            safe(self.topics_fn.clone(), ctx, default_topics_fn),
        );
        cap(Value::Object(payload))
    }
}

fn load_class(f: &LoadFailure) -> String {
    match f {
        LoadFailure::Unknown => "KeyError".into(),
        LoadFailure::Config(_) => "ConnectorConfigError".into(),
        LoadFailure::Other(c) => c.clone(),
    }
}

fn safe(
    custom: Option<SourceFn>,
    ctx: &LoopContext,
    default: impl Fn(&LoopContext) -> Result<Value, String>,
) -> Value {
    let result = match custom {
        Some(f) => f(ctx),
        None => default(ctx),
    };
    result.unwrap_or_else(|class| json!({"error": class}))
}

// ---- local time ----------------------------------------------------------------

fn local_now(ctx: &LoopContext) -> DateTime<Local> {
    ctx.now.with_timezone(&Local)
}

/// Local midnight `days` after today's, with that day's own UTC offset. Built
/// from the calendar date (a naive local time resolved by the system's zone
/// rules), never by adding 24 hours, so a DST change day keeps its real length.
fn local_midnight(ctx: &LoopContext, days: i64) -> DateTime<Local> {
    let day: NaiveDate = local_now(ctx).date_naive() + Duration::days(days);
    let naive = day.and_time(NaiveTime::MIN);
    Local
        .from_local_datetime(&naive)
        .earliest()
        .or_else(|| {
            Local
                .from_local_datetime(&(naive + Duration::hours(1)))
                .earliest()
        })
        .unwrap_or_else(|| Utc.from_utc_datetime(&naive).with_timezone(&Local))
}

fn iso<Tz: TimeZone>(dt: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    dt.to_rfc3339_opts(SecondsFormat::Secs, false)
}

// ---- gitlab / github -----------------------------------------------------------

fn truthy(v: Option<&Value>) -> bool {
    v.is_some_and(py_truthy)
}

fn or_str(candidates: &[Option<&Value>]) -> String {
    candidates
        .iter()
        .find(|c| truthy(**c))
        .map(|v| py_str(v.expect("truthy")))
        .unwrap_or_default()
}

fn gitlab_item(raw: &Map<String, Value>) -> Value {
    let empty = Map::new();
    let obj = |k: &str| raw.get(k).and_then(Value::as_object).unwrap_or(&empty);
    let (target, project, refs) = (obj("target"), obj("project"), obj("references"));
    let full = refs
        .get("full")
        .filter(|v| py_truthy(v))
        .map(py_str)
        .unwrap_or_default();
    let before_marker = full.split(['#', '!']).next().unwrap_or("").to_string();
    let project_name = {
        let p = project.get("path_with_namespace");
        if truthy(p) {
            py_str(p.expect("truthy"))
        } else {
            before_marker
        }
    };
    let labels = match raw.get("labels") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    let mut item = Map::new();
    item.insert(
        "title".into(),
        or_str(&[raw.get("title"), target.get("title")]).into(),
    );
    item.insert(
        "url".into(),
        or_str(&[
            raw.get("web_url"),
            target.get("web_url"),
            raw.get("target_url"),
        ])
        .into(),
    );
    item.insert("project".into(), project_name.into());
    item.insert("updated_at".into(), or_str(&[raw.get("updated_at")]).into());
    item.insert("labels".into(), Value::Array(labels));
    if truthy(raw.get("action_name")) {
        item.insert("action".into(), py_str(&raw["action_name"]).into());
    }
    Value::Object(item)
}

fn item_list(rows: &Value) -> Value {
    match rows {
        Value::Array(a) => Value::Array(
            a.iter()
                .filter_map(Value::as_object)
                .map(gitlab_item)
                .collect(),
        ),
        _ => Value::Array(Vec::new()),
    }
}

fn err_class(e: crate::connectors::ConnectorError) -> String {
    e.class_name()
}

/// `conn.api("GET", path)` returning a JSON object's field, like
/// `conn.api(...).get(key, "")`; a non-object answer is an `AttributeError`.
fn api_field(conn: &dyn Connector, path: &str, key: &str) -> Result<String, String> {
    let v = conn
        .api("GET", path, &ApiOpts::default())
        .map_err(err_class)?;
    match v.as_object() {
        Some(o) => Ok(o.get(key).map(py_str).unwrap_or_default()),
        None => Err("AttributeError".into()),
    }
}

fn collect_gitlab(conn: &dyn Connector) -> Result<Map<String, Value>, String> {
    let user = api_field(conn, "/user", "username")?;
    let get = |path: String| -> Result<Value, String> {
        conn.api("GET", &path, &ApiOpts::default())
            .map(|v| item_list(&v))
            .map_err(err_class)
    };
    let mut out = Map::new();
    out.insert(
        "todos".into(),
        get(format!("/todos?state=pending&{PER_PAGE}"))?,
    );
    out.insert(
        "assigned".into(),
        get(format!(
            "/issues?scope=assigned_to_me&state=opened&{PER_PAGE}"
        ))?,
    );
    out.insert(
        "review_requests".into(),
        get(format!(
            "/merge_requests?reviewer_username={user}&state=opened&scope=all&{PER_PAGE}"
        ))?,
    );
    out.insert(
        "my_merge_requests".into(),
        get(format!(
            "/merge_requests?author_username={user}&state=opened&scope=all&{PER_PAGE}"
        ))?,
    );
    Ok(out)
}

fn collect_github(conn: &dyn Connector, yesterday: &str) -> Result<Map<String, Value>, String> {
    let user = api_field(conn, "/user", "login")?;
    let found = conn
        .api(
            "GET",
            &format!(
                "/search/issues?q=involves:{user}+state:open+updated:>={yesterday}&{PER_PAGE}"
            ),
            &ApiOpts::default(),
        )
        .map_err(err_class)?;
    let items = found
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut rows = Vec::new();
    for r in &items {
        let Some(r) = r.as_object() else {
            return Err("AttributeError".into());
        };
        let s = |k: &str| {
            r.get(k)
                .filter(|v| py_truthy(v))
                .map(py_str)
                .unwrap_or_default()
        };
        let repo = s("repository_url");
        let project = repo.rsplit("/repos/").next().unwrap_or("").to_string();
        let labels: Vec<Value> = r
            .get("labels")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_object)
                    .map(|l| Value::String(l.get("name").map(py_str).unwrap_or("None".into())))
                    .collect()
            })
            .unwrap_or_default();
        rows.push(
            json!({"title": s("title"), "url": s("html_url"), "project": project,
                         "updated_at": s("updated_at"), "labels": labels}),
        );
    }
    let mut out = Map::new();
    out.insert("involved".into(), Value::Array(rows));
    Ok(out)
}

// ---- default sources -----------------------------------------------------------

/// Yesterday (local time) in the event ledger: `issue.completed` /
/// `issue.escalated` counts. Event files and timestamps are UTC.
pub fn default_loop_x(ctx: &LoopContext, events_dir: Option<&Path>) -> Result<Value, String> {
    let end = local_midnight(ctx, 0);
    let start = local_midnight(ctx, -1);
    let since = start.with_timezone(&Utc).date_naive().to_string();
    let until = (end - Duration::seconds(1))
        .with_timezone(&Utc)
        .date_naive()
        .to_string();
    let mut completed = Vec::new();
    let mut escalated = Vec::new();
    for ev in events::iter_events(events_dir, Some(&since), Some(&until)) {
        let ts = ev.get("timestamp").map(py_str).unwrap_or_default();
        if let Some(when) = crate::seen_store::parse_iso(&ts) {
            if !(start.with_timezone(&Utc) <= when && when < end.with_timezone(&Utc)) {
                continue;
            }
        }
        let row = json!({"project": ev.get("project").cloned().unwrap_or(Value::Null),
                         "issue_iid": ev.get("issue_iid").cloned().unwrap_or(Value::Null)});
        match ev.get("event_type").and_then(Value::as_str) {
            Some("issue.completed") => completed.push(row),
            Some("issue.escalated") => escalated.push(row),
            _ => {}
        }
    }
    Ok(json!({"completed": completed, "escalated": escalated}))
}

/// Urgent counts from `outputs/inbox-triage/status.json`.
pub fn default_inbox(ctx: &LoopContext) -> Result<Value, String> {
    let path = ctx.repo_root.join("outputs/inbox-triage/status.json");
    let data: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({"inboxes": {}}));
    let mut rows = Map::new();
    let mut total = 0usize;
    if let Some(inboxes) = data.get("inboxes").and_then(Value::as_object) {
        for (name, e) in inboxes {
            if let Some(e) = e.as_object() {
                let n = e
                    .get("urgent")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                total += n;
                rows.insert(name.clone(), json!(n));
            }
        }
    }
    Ok(json!({"urgent_by_inbox": rows, "urgent_total": total}))
}

fn default_topics_fn(ctx: &LoopContext) -> Result<Value, String> {
    let dir = ctx.repo_root.join("outputs/topic-monitor/history");
    Ok(Value::Object(default_topics(&dir)))
}

/// Latest briefing headline per topic (files are `<date>-<topic>.md`).
pub fn default_topics(history_dir: &Path) -> Map<String, Value> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(history_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "md"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let mut latest: Vec<(String, PathBuf)> = Vec::new();
    for path in names {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(topic) = topic_of(&name) else {
            continue;
        };
        match latest.iter_mut().find(|(t, _)| *t == topic) {
            Some(slot) => slot.1 = path,
            None => latest.push((topic, path)),
        }
    }
    let mut out = Map::new();
    for (topic, path) in latest {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            if !line.trim().is_empty() && !line.trim_start().starts_with('#') {
                out.insert(
                    topic.clone(),
                    line.trim().chars().take(240).collect::<String>().into(),
                );
                break;
            }
        }
    }
    out
}

/// `^(\d{4}-\d{2}-\d{2})-(.+)\.md$` group 2.
fn topic_of(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".md")?;
    let (date, topic) = (stem.get(..10)?, stem.get(11..)?);
    let b = date.as_bytes();
    let shape = b.iter().enumerate().all(|(i, c)| match i {
        4 | 7 => *c == b'-',
        _ => c.is_ascii_digit(),
    });
    (shape && stem.as_bytes()[10] == b'-' && !topic.is_empty()).then(|| topic.to_string())
}

fn meetings(ctx: &LoopContext, accounts: &[Account], source: &dyn ConnectorSource) -> Value {
    let start = iso(&local_midnight(ctx, 0));
    let end = iso(&local_midnight(ctx, 1));
    let mut out = Vec::new();
    for account in accounts {
        let result = source
            .load(&account.id)
            .map_err(|f| load_class(&f))
            .and_then(|c| c.list_events(&start, &end, 50).map_err(err_class));
        out.push(match result {
            Ok(events) => {
                let g = |e: &Value, k: &str, d: Value| e.get(k).cloned().unwrap_or(d);
                let rows: Vec<Value> = events
                    .iter()
                    .map(|e| {
                        json!({"title": g(e, "summary", json!("")), "start": g(e, "start", json!("")),
                               "end": g(e, "end", json!("")), "url": g(e, "html_link", json!("")),
                               "attendees": g(e, "attendees_count", json!(0))})
                    })
                    .collect();
                json!({"account": account.id, "events": rows})
            }
            Err(class) => json!({"account": account.id, "error": class}),
        });
    }
    Value::Array(out)
}

// ---- payload cap -------------------------------------------------------------------

#[derive(Clone)]
enum Seg {
    Key(String),
    Idx(usize),
}

fn list_paths(payload: &Value) -> Vec<Vec<Seg>> {
    let mut paths = Vec::new();
    if let Some(accs) = payload.get("accounts").and_then(Value::as_array) {
        for (i, acc) in accs.iter().enumerate() {
            if let Some(o) = acc.as_object() {
                for (k, v) in o {
                    if v.is_array() {
                        paths.push(vec![
                            Seg::Key("accounts".into()),
                            Seg::Idx(i),
                            Seg::Key(k.clone()),
                        ]);
                    }
                }
            }
        }
    }
    if let Some(ms) = payload.get("meetings").and_then(Value::as_array) {
        for (i, m) in ms.iter().enumerate() {
            if m.get("events").is_some_and(Value::is_array) {
                paths.push(vec![
                    Seg::Key("meetings".into()),
                    Seg::Idx(i),
                    Seg::Key("events".into()),
                ]);
            }
        }
    }
    if let Some(o) = payload.get("loop_x").and_then(Value::as_object) {
        for (k, v) in o {
            if v.is_array() {
                paths.push(vec![Seg::Key("loop_x".into()), Seg::Key(k.clone())]);
            }
        }
    }
    paths
}

fn at_mut<'a>(mut v: &'a mut Value, path: &[Seg]) -> &'a mut Value {
    for seg in path {
        v = match seg {
            Seg::Key(k) => &mut v[k.as_str()],
            Seg::Idx(i) => &mut v[*i],
        };
    }
    v
}

fn dumps_len(v: &Value) -> usize {
    pydumps::dumps(v).len()
}

/// Shrink the biggest lists (halving) until the payload fits.
fn cap(mut payload: Value) -> Value {
    payload["truncated"] = Value::Bool(false);
    while dumps_len(&payload) > MAX_PAYLOAD_BYTES {
        let mut biggest: Option<(usize, Vec<Seg>)> = None;
        for path in list_paths(&payload) {
            let size = dumps_len(at_mut(&mut payload, &path));
            if biggest.as_ref().is_none_or(|(s, _)| size > *s) {
                biggest = Some((size, path));
            }
        }
        let list = biggest.map(|(_, p)| p).filter(|p| {
            at_mut(&mut payload, p)
                .as_array()
                .is_some_and(|a| !a.is_empty())
        });
        let Some(path) = list else {
            payload["topics"] = json!({});
            break;
        };
        if let Value::Array(a) = at_mut(&mut payload, &path) {
            let keep = a.len() / 2;
            a.truncate(keep);
        }
        payload["truncated"] = Value::Bool(true);
    }
    payload
}

// ---- answer handling -----------------------------------------------------------------

/// Every non-empty string under a `url` key, anywhere in `payload`.
pub fn known_urls(payload: &Value) -> std::collections::HashSet<String> {
    fn walk(node: &Value, urls: &mut std::collections::HashSet<String>) {
        match node {
            Value::Object(m) => {
                for (k, v) in m {
                    match v {
                        Value::String(s) if k == "url" && !s.is_empty() => {
                            urls.insert(s.clone());
                        }
                        _ => walk(v, urls),
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|v| walk(v, urls)),
            _ => {}
        }
    }
    let mut urls = std::collections::HashSet::new();
    walk(payload, &mut urls);
    urls
}

fn text_of(v: Option<&Value>) -> String {
    v.filter(|v| py_truthy(v)).map(py_str).unwrap_or_default()
}

fn list_of<'a>(answer: &'a Value, key: &str) -> &'a [Value] {
    match answer.get(key) {
        Some(Value::Array(a)) => a,
        _ => &[],
    }
}

/// One high-level line for the run: the model's own `summary` when it gave
/// one, else per-section counts ("3 need you, 2 meetings").
pub fn headline(answer: &Value) -> String {
    let own = loopkit::chat_text(&text_of(answer.get("summary")), 200);
    if !own.is_empty() {
        return own;
    }
    let parts: Vec<String> = SHORT_LABELS
        .iter()
        .filter(|(k, _)| answer.get(*k).is_some_and(py_truthy))
        .map(|(k, label)| format!("{} {label}", list_of(answer, k).len()))
        .collect();
    if parts.is_empty() {
        "Nothing needs your attention".into()
    } else {
        parts.join(", ")
    }
}

/// Slack mrkdwn: bold title, an italic one-line summary, then a bold header
/// and bullets (`<url|text>` links) per section.
pub fn format_digest(answer: &Value, date: &str) -> String {
    let mut lines = vec![format!(
        "*Daily digest* \u{b7} {}",
        loopkit::chat_text(date, 20)
    )];
    let mut any = false;
    let summary = loopkit::chat_text(&text_of(answer.get("summary")), 200);
    if !summary.is_empty() {
        lines.push(format!("_{summary}_"));
    }
    for (key, title) in SECTIONS {
        let bullets: Vec<&Value> = list_of(answer, key)
            .iter()
            .filter(|b| b.is_object())
            .collect();
        if bullets.is_empty() {
            continue;
        }
        any = true;
        lines.push(String::new());
        lines.push(format!(
            "*{}* ({})",
            loopkit::chat_text(title, 300),
            bullets.len()
        ));
        for b in bullets {
            let url = b.get("url").and_then(Value::as_str).unwrap_or("");
            let text = b.get("text").map(py_str).unwrap_or_default();
            lines.push(format!("\u{2022} {}", loopkit::slack_link(url, &text)));
        }
    }
    if !any {
        lines.push(String::new());
        lines.push("Nothing needs your attention today.".into());
    }
    lines.join("\n")
}

impl LoopPlugin for DailyDigest {
    fn loop_name(&self) -> &str {
        "daily-digest-loop"
    }
    fn definition_dir(&self) -> &str {
        "daily-digest"
    }
    fn output_keys(&self) -> &[&'static str] {
        &OUTPUT_KEYS
    }

    fn discover(&self, ctx: &LoopContext) -> Result<Vec<WorkItem>, PluginError> {
        Ok(vec![WorkItem::new(
            format!("digest:{}", local_now(ctx).date_naive()),
            "Daily digest",
            self.collect(ctx),
        )])
    }

    fn after_item(
        &self,
        item: &WorkItem,
        answer: &Value,
        _ctx: &LoopContext,
    ) -> Result<Outcome, PluginError> {
        let known = known_urls(&item.payload);
        let mut cleaned = Map::new();
        for key in OUTPUT_KEYS {
            let bullets: Vec<Value> = list_of(answer, key)
                .iter()
                .filter_map(Value::as_object)
                .map(|b| {
                    let url = match b.get("url") {
                        Some(Value::String(u)) if known.contains(u) => u.clone(),
                        _ => String::new(),
                    };
                    json!({"text": b.get("text").map_or("".to_string(), py_str), "url": url})
                })
                .collect();
            cleaned.insert(key.into(), Value::Array(bullets));
        }
        cleaned.insert(
            "summary".into(),
            loopkit::chat_text(&text_of(answer.get("summary")), 200).into(),
        );
        let cleaned = Value::Object(cleaned);
        Ok(Outcome::new(&item.key, Status::Done, headline(&cleaned)).with_data(cleaned))
    }

    fn digest(&self, outcomes: &[Outcome], ctx: &LoopContext) -> Option<String> {
        outcomes
            .iter()
            .find(|o| o.status == Status::Done)
            .map(|o| format_digest(&o.data, &local_now(ctx).date_naive().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{
        github, gitlab, google_calendar, ConnResult, ConnectorError, TypeInfo,
    };
    use crate::loop_definition::LoopDefinition;
    use std::sync::Mutex;

    static TZ_LOCK: Mutex<()> = Mutex::new(());

    /// Run `f` on a fresh thread (fresh chrono tz cache) with `TZ` set.
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

    type Handler = Box<dyn Fn(&str) -> ConnResult<Value> + Send + Sync>;

    struct FakeApi {
        info: &'static TypeInfo,
        account: Account,
        handler: Handler,
        paths: Arc<Mutex<Vec<String>>>,
    }

    impl Connector for FakeApi {
        fn info(&self) -> &'static TypeInfo {
            self.info
        }
        fn account(&self) -> &Account {
            &self.account
        }
        fn api(&self, _m: &str, path: &str, _o: &ApiOpts) -> ConnResult<Value> {
            self.paths.lock().unwrap().push(path.to_string());
            (self.handler)(path)
        }
    }

    type Events = Arc<Mutex<Vec<(String, String)>>>;
    struct FakeCal {
        account: Account,
        fail: bool,
        ranges: Events,
    }

    impl Connector for FakeCal {
        fn info(&self) -> &'static TypeInfo {
            &google_calendar::INFO
        }
        fn account(&self) -> &Account {
            &self.account
        }
        fn list_events(&self, a: &str, b: &str, _n: u32) -> ConnResult<Vec<Value>> {
            self.ranges.lock().unwrap().push((a.into(), b.into()));
            if self.fail {
                return Err(ConnectorError::Other {
                    class: "ValueError".into(),
                    detail: "token abc".into(),
                });
            }
            Ok(vec![json!({"summary": "Standup", "start": "s", "end": "e",
                           "html_link": "https://cal/1", "attendees_count": 3})])
        }
    }

    #[derive(Default)]
    struct Source {
        accounts: Vec<Account>,
        conns: Mutex<
            std::collections::HashMap<String, Box<dyn Fn() -> Box<dyn Connector> + Send + Sync>>,
        >,
    }

    impl Source {
        fn add(
            &self,
            account: Account,
            make: impl Fn() -> Box<dyn Connector> + Send + Sync + 'static,
        ) {
            self.conns
                .lock()
                .unwrap()
                .insert(account.id.clone(), Box::new(make));
        }
    }

    impl ConnectorSource for Source {
        fn accounts_with_capability(&self, cap: &str) -> Vec<Account> {
            self.accounts
                .iter()
                .filter(|a| {
                    crate::connectors::get_type(&a.kind).is_some_and(|t| t.has_capability(cap))
                })
                .cloned()
                .collect()
        }
        fn load(&self, id: &str) -> Result<Box<dyn Connector>, LoadFailure> {
            self.conns
                .lock()
                .unwrap()
                .get(id)
                .map(|f| f())
                .ok_or(LoadFailure::Unknown)
        }
    }

    fn gl_account(id: &str) -> Account {
        Account::new(id, "gitlab")
    }

    fn gitlab_conn(id: &str, fail: bool, paths: Arc<Mutex<Vec<String>>>) -> Box<dyn Connector> {
        Box::new(FakeApi {
            info: &gitlab::INFO,
            account: gl_account(id),
            paths,
            handler: Box::new(move |p| {
                if fail {
                    return Err(ConnectorError::Other {
                        class: "RuntimeError".into(),
                        detail: "401 https://secret/token".into(),
                    });
                }
                if p == "/user" {
                    return Ok(json!({"username": "enc"}));
                }
                if p.starts_with("/todos") {
                    return Ok(json!([{"action_name": "mentioned",
                        "target": {"title": "T", "web_url": "https://gl/t/1"},
                        "project": {"path_with_namespace": "g/p"}}]));
                }
                Ok(json!([]))
            }),
        })
    }

    const YAML: &str = include_str!("../../../../loops/daily-digest/loop.yaml");

    fn ctx_at(now: DateTime<Utc>) -> LoopContext {
        LoopContext {
            loop_name: "daily-digest-loop".into(),
            run_id: "r".into(),
            now,
            definition: Arc::new(LoopDefinition::from_yaml_str(YAML, "test").unwrap()),
            settings: json!({}),
            history_dir: PathBuf::from("/nonexistent"),
            log: Arc::new(|_| {}),
            repo_root: PathBuf::from("/nonexistent"),
            force: false,
            call_budget_usd: None,
            model: None,
            state_dir: None,
        }
    }

    fn ctx() -> LoopContext {
        ctx_at(Utc.with_ymd_and_hms(2026, 10, 6, 9, 30, 0).unwrap())
    }

    fn plugin(source: Source) -> DailyDigest {
        DailyDigest::new(Arc::new(source))
            .with_loop_x_fn(Arc::new(|_| Ok(json!({}))))
            .with_inbox_fn(Arc::new(|_| Ok(json!({}))))
            .with_topics_fn(Arc::new(|_| Ok(json!({}))))
            .with_calendar_accounts_fn(Arc::new(|| Ok(vec![])))
    }

    #[test]
    fn collect_queries_gitlab_endpoints() {
        with_tz("UTC", || {
            let paths = Arc::new(Mutex::new(Vec::new()));
            let mut s = Source::default();
            s.accounts.push(gl_account("work"));
            let p2 = paths.clone();
            s.add(gl_account("work"), move || {
                gitlab_conn("work", false, p2.clone())
            });
            let data = plugin(s).collect(&ctx());
            let paths = paths.lock().unwrap();
            assert!(
                paths.contains(&"/user".to_string())
                    && paths.iter().any(|p| p.starts_with("/todos"))
            );
            assert_eq!(data["accounts"][0]["todos"][0]["url"], "https://gl/t/1");
            assert!(paths
                .iter()
                .any(|p| p
                    == "/merge_requests?reviewer_username=enc&state=opened&scope=all&per_page=50"));
        });
    }

    #[test]
    fn one_failing_account_does_not_fail_the_digest() {
        with_tz("UTC", || {
            let mut s = Source::default();
            s.accounts.extend([gl_account("a"), gl_account("b")]);
            for id in ["a", "b"] {
                s.add(gl_account(id), move || {
                    gitlab_conn(id, id == "a", Default::default())
                });
            }
            let data = plugin(s).collect(&ctx());
            assert_eq!(
                data["accounts"][0],
                json!({"account": "a", "error": "RuntimeError"})
            );
            assert!(data["accounts"][1].get("todos").is_some());
            assert!(!data.to_string().contains("secret"));
        });
    }

    #[test]
    fn payload_is_capped() {
        with_tz("UTC", || {
            let mut s = Source::default();
            s.accounts.push(gl_account("a"));
            s.add(gl_account("a"), || {
                Box::new(FakeApi {
                    info: &gitlab::INFO,
                    account: gl_account("a"),
                    paths: Default::default(),
                    handler: Box::new(|p| {
                        if p == "/user" {
                            return Ok(json!({"username": "u"}));
                        }
                        Ok(Value::Array(
                            (0..500).map(|i| json!({"title": "x".repeat(500), "web_url": format!("https://gl/{i}")})).collect(),
                        ))
                    }),
                })
            });
            let data = plugin(s).collect(&ctx());
            assert!(pydumps::dumps(&data).len() <= 60_000);
            assert_eq!(data["truncated"], true);
        });
    }

    #[test]
    fn github_search() {
        with_tz("UTC", || {
            let paths = Arc::new(Mutex::new(Vec::new()));
            let mut s = Source::default();
            let acct = Account::new("g", "github");
            s.accounts.push(acct.clone());
            let p2 = paths.clone();
            s.add(acct.clone(), move || {
                Box::new(FakeApi {
                    info: &github::INFO,
                    account: acct.clone(),
                    paths: p2.clone(),
                    handler: Box::new(|p| {
                        if p == "/user" {
                            return Ok(json!({"login": "ghu"}));
                        }
                        Ok(json!({"items": [{"title": "PR", "html_url": "https://gh/1",
                                  "repository_url": "https://api.github.com/repos/o/r", "updated_at": "z"}]}))
                    }),
                })
            });
            let data = plugin(s).collect(&ctx());
            assert!(paths
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.contains("involves:ghu") && p.contains("2026-10-05")));
            let row = &data["accounts"][0]["involved"][0];
            assert_eq!(row["url"], "https://gh/1");
            assert_eq!(row["project"], "o/r");
        });
    }

    #[test]
    fn meetings_and_failing_calendar() {
        with_tz("UTC", || {
            let ranges: Events = Default::default();
            let s = Source::default();
            let (c1, c2) = (
                Account::new("c1", "google_calendar"),
                Account::new("c2", "google_calendar"),
            );
            for (acct, fail) in [(c1.clone(), false), (c2.clone(), true)] {
                let r = ranges.clone();
                s.add(acct.clone(), move || {
                    Box::new(FakeCal {
                        account: acct.clone(),
                        fail,
                        ranges: r.clone(),
                    })
                });
            }
            let p = plugin(s)
                .with_calendar_accounts_fn(Arc::new(move || Ok(vec![c1.clone(), c2.clone()])));
            let data = p.collect(&ctx());
            assert_eq!(data["meetings"][0]["events"][0]["url"], "https://cal/1");
            assert_eq!(
                data["meetings"][1],
                json!({"account": "c2", "error": "ValueError"})
            );
            let r = ranges.lock().unwrap();
            assert!(r[0].0 < r[0].1);
            assert_eq!(r[0].0, "2026-10-06T00:00:00+00:00");
        });
    }

    #[test]
    fn known_urls_collects_from_every_list() {
        let urls = known_urls(&json!({"accounts": [{"todos": [{"url": "https://a"}]}],
            "meetings": [{"events": [{"url": "https://m"}]}],
            "loop_x": {"completed": [{"url": "https://x"}]}}));
        for u in ["https://a", "https://m", "https://x"] {
            assert!(urls.contains(u));
        }
    }

    fn empty_answer() -> Value {
        json!({"needs_you": [], "waiting_on_others": [], "loop_x_did": [], "fyi": [], "meetings": []})
    }

    #[test]
    fn after_item_drops_unknown_urls() {
        let p = plugin(Source::default());
        let item = WorkItem::new(
            "digest:2026-10-06",
            "d",
            json!({"accounts": [{"todos": [{"url": "https://gl/t/1"}]}]}),
        );
        let mut answer = empty_answer();
        answer["needs_you"] =
            json!([{"text": "a", "url": "https://gl/t/1"}, {"text": "b", "url": "https://evil/x"}]);
        let out = p.after_item(&item, &answer, &ctx()).unwrap();
        let urls: Vec<_> = out.data["needs_you"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["url"].as_str().unwrap())
            .collect();
        assert_eq!(urls, ["https://gl/t/1", ""]);
        assert_eq!(
            (out.status, out.summary.as_str()),
            (Status::Done, "2 need you")
        );
    }

    #[test]
    fn format_digest_sections_and_empty_message() {
        let mut a = empty_answer();
        a["needs_you"] = json!([{"text": "Review !7", "url": "https://gl/m/7"}]);
        a["meetings"] = json!([{"text": "Standup 10:00", "url": ""}]);
        let text = format_digest(&a, "2026-10-06");
        assert!(
            text.contains("*Needs you* (1)")
                && text.contains("\u{2022} <https://gl/m/7|Review !7>")
        );
        assert!(text.contains("Today's meetings") && text.contains("Standup 10:00"));
        assert!(format_digest(&empty_answer(), "2026-10-06").contains("Nothing"));
    }

    #[test]
    fn output_keys_include_meetings() {
        let p = plugin(Source::default());
        let mut keys = p.output_keys().to_vec();
        keys.sort();
        assert_eq!(
            keys,
            [
                "fyi",
                "loop_x_did",
                "meetings",
                "needs_you",
                "waiting_on_others"
            ]
        );
    }

    #[test]
    fn discover_single_item_keyed_by_date() {
        with_tz("UTC", || {
            let items = plugin(Source::default()).discover(&ctx()).unwrap();
            assert_eq!(
                items.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
                ["digest:2026-10-06"]
            );
        });
    }

    fn write_events(dir: &Path, file: &str, rows: &[Value]) {
        let text: Vec<String> = rows.iter().map(|r| r.to_string()).collect();
        std::fs::write(dir.join(file), text.join("\n") + "\n").unwrap();
    }

    #[test]
    fn loop_x_yesterday_counts() {
        with_tz("UTC", || {
            let d = tempfile::tempdir().unwrap();
            write_events(
                d.path(),
                "2026-10-05.jsonl",
                &[
                    json!({"event_type": "issue.completed", "project": "p", "issue_iid": 3, "data": {}}),
                    json!({"event_type": "issue.escalated", "project": "p", "issue_iid": 4, "data": {}}),
                    json!({"event_type": "issue.started", "project": "p", "issue_iid": 5, "data": {}}),
                ],
            );
            let out = default_loop_x(&ctx(), Some(d.path())).unwrap();
            assert_eq!(out["completed"], json!([{"project": "p", "issue_iid": 3}]));
            assert_eq!(out["escalated"], json!([{"project": "p", "issue_iid": 4}]));
        });
    }

    #[test]
    fn topic_headlines() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("2026-10-04-news.md"), "# T\n\nold headline\n").unwrap();
        std::fs::write(
            d.path().join("2026-10-05-news.md"),
            "# T\n\nNew headline here\n\n## More\n",
        )
        .unwrap();
        std::fs::write(d.path().join("2026-10-05-ai-news.md"), "Other\n").unwrap();
        std::fs::write(d.path().join("notes.md"), "ignored\n").unwrap();
        assert_eq!(
            Value::Object(default_topics(d.path())),
            json!({"news": "New headline here", "ai-news": "Other"})
        );
    }

    #[test]
    fn definition_and_prompt() {
        let d = LoopDefinition::from_yaml_str(YAML, "t").unwrap();
        assert_eq!(
            d.actions,
            ["fetch_connector_data", "summarize", "send_notification"]
        );
        let prompt = include_str!("../../../../loops/daily-digest/prompt.md");
        assert!(prompt.contains("{{item_json}}"));
    }

    #[test]
    fn format_digest_neutralises_injected_text() {
        let mut a = empty_answer();
        a["needs_you"] =
            json!([{"text": "x|y> <!channel> <https://evil|click>", "url": "https://gl/a|b>"}]);
        let text = format_digest(&a, "2026-10-06");
        assert!(!text.contains('<') && !text.contains('>') && !text.contains("https://gl/a"));
    }

    #[test]
    fn after_item_ignores_non_string_url() {
        let p = plugin(Source::default());
        let item = WorkItem::new("k", "d", json!({}));
        let mut a = empty_answer();
        a["needs_you"] = json!([{"text": "a", "url": ["x"]}, {"text": "b", "url": {"a": 1}}]);
        let out = p.after_item(&item, &a, &ctx()).unwrap();
        assert_eq!(out.data["needs_you"][0]["url"], "");
        assert_eq!(out.data["needs_you"][1]["url"], "");
    }

    #[test]
    fn local_dates_near_midnight() {
        let (out, key) = with_tz("Australia/Brisbane", || {
            let d = tempfile::tempdir().unwrap();
            let c = ctx_at(Utc.with_ymd_and_hms(2026, 10, 5, 20, 0, 0).unwrap());
            // local 2026-10-06 06:00; local yesterday = 10-04 14:00Z .. 10-05 14:00Z
            write_events(
                d.path(),
                "2026-10-04.jsonl",
                &[
                    json!({"event_type": "issue.completed", "project": "in", "issue_iid": 1, "timestamp": "2026-10-04T15:00:00.000Z"}),
                    json!({"event_type": "issue.completed", "project": "before", "issue_iid": 2, "timestamp": "2026-10-04T13:00:00.000Z"}),
                ],
            );
            write_events(
                d.path(),
                "2026-10-05.jsonl",
                &[
                    json!({"event_type": "issue.escalated", "project": "in2", "issue_iid": 3, "timestamp": "2026-10-05T13:00:00.000Z"}),
                    json!({"event_type": "issue.completed", "project": "after", "issue_iid": 4, "timestamp": "2026-10-05T15:00:00.000Z"}),
                ],
            );
            let out = default_loop_x(&c, Some(d.path())).unwrap();
            let key = plugin(Source::default()).discover(&c).unwrap()[0]
                .key
                .clone();
            (out, key)
        });
        assert_eq!(out["completed"], json!([{"project": "in", "issue_iid": 1}]));
        assert_eq!(
            out["escalated"],
            json!([{"project": "in2", "issue_iid": 3}])
        );
        assert_eq!(key, "digest:2026-10-06");
    }

    #[test]
    fn yesterday_window_spans_a_dst_change() {
        // New York leaves DST on 2026-11-01: that local day is 25 hours long.
        let out = with_tz("America/New_York", || {
            let d = tempfile::tempdir().unwrap();
            let c = ctx_at(Utc.with_ymd_and_hms(2026, 11, 2, 14, 30, 0).unwrap());
            write_events(
                d.path(),
                "2026-11-01.jsonl",
                &[
                    json!({"event_type": "issue.completed", "project": "first-hour", "issue_iid": 1, "timestamp": "2026-11-01T04:30:00.000Z"}),
                    json!({"event_type": "issue.completed", "project": "before", "issue_iid": 2, "timestamp": "2026-11-01T03:30:00.000Z"}),
                ],
            );
            write_events(
                d.path(),
                "2026-11-02.jsonl",
                &[
                    json!({"event_type": "issue.completed", "project": "last-hour", "issue_iid": 3, "timestamp": "2026-11-02T04:30:00.000Z"}),
                ],
            );
            default_loop_x(&c, Some(d.path())).unwrap()
        });
        let names: Vec<_> = out["completed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["project"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["first-hour", "last-hour"]);
    }

    #[test]
    fn todays_meetings_window_spans_a_dst_change() {
        let (a, b) = with_tz("America/New_York", || {
            let c = ctx_at(Utc.with_ymd_and_hms(2026, 11, 1, 14, 30, 0).unwrap());
            let ranges: Events = Default::default();
            let acct = Account::new("cal", "google_calendar");
            let s = Source::default();
            let r = ranges.clone();
            let a2 = acct.clone();
            s.add(acct.clone(), move || {
                Box::new(FakeCal {
                    account: a2.clone(),
                    fail: false,
                    ranges: r.clone(),
                })
            });
            meetings(&c, &[acct], &s);
            let r = ranges.lock().unwrap()[0].clone();
            (r.0, r.1)
        });
        let parse = |t: &str| DateTime::parse_from_rfc3339(t).unwrap().with_timezone(&Utc);
        assert_eq!(
            parse(&a),
            Utc.with_ymd_and_hms(2026, 11, 1, 4, 0, 0).unwrap()
        );
        assert_eq!(
            parse(&b),
            Utc.with_ymd_and_hms(2026, 11, 2, 5, 0, 0).unwrap()
        );
    }

    #[test]
    fn summary_prefers_the_model_headline_and_is_stored() {
        let p = plugin(Source::default());
        let item = WorkItem::new("digest:2026-10-06", "d", json!({}));
        let mut a = empty_answer();
        a["summary"] = json!("Light day; review !7 first.");
        a["needs_you"] = json!([{"text": "a", "url": ""}]);
        let out = p.after_item(&item, &a, &ctx()).unwrap();
        assert_eq!(out.summary, "Light day; review !7 first.");
        assert_eq!(out.data["summary"], out.summary.as_str());
        assert!(format_digest(&out.data, "2026-10-06").contains("_Light day; review !7 first._"));
        assert_eq!(headline(&empty_answer()), "Nothing needs your attention");
    }

    #[test]
    fn default_inbox_counts_urgent_per_inbox() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("outputs/inbox-triage")).unwrap();
        std::fs::write(
            d.path().join("outputs/inbox-triage/status.json"),
            json!({"inboxes": {"a": {"urgent": [1, 2]}, "b": {}, "c": 5}}).to_string(),
        )
        .unwrap();
        let mut c = ctx();
        c.repo_root = d.path().to_path_buf();
        assert_eq!(
            default_inbox(&c).unwrap(),
            json!({"urgent_by_inbox": {"a": 2, "b": 0}, "urgent_total": 2})
        );
    }

    #[test]
    fn digest_uses_the_first_done_outcome() {
        with_tz("UTC", || {
            let p = plugin(Source::default());
            let failed = Outcome::new("k", Status::Failed, "x");
            let done = Outcome::new("k2", Status::Done, "ok").with_data(empty_answer());
            assert!(p.digest(std::slice::from_ref(&failed), &ctx()).is_none());
            assert!(p
                .digest(&[failed, done], &ctx())
                .unwrap()
                .starts_with("*Daily digest* \u{b7} 2026-10-06"));
        });
    }
}
