//! RSS Watch loop on LoopKit (mirrors `bin/loop_plugins/rss_watch.py`):
//! collects unseen entries from each RSS/Atom connector account's feeds, has
//! the model rank them against the user's interests (`settings.interests`)
//! and sends a short digest through the loop's notifier. Feed content is
//! untrusted: entries are trimmed before the prompt, the model runs sealed and
//! every highlight link must be one that was offered.

use crate::connectors::{ConnectorSource, Field, LoadFailure};
use crate::loopkit::{
    chat_text, chat_url, LoopContext, LoopPlugin, Outcome, PluginError, Status, WorkItem,
};
use crate::loops_config::{py_str, py_truthy};
use crate::seen_store::SeenStore;
use chrono::{DateTime, Local, NaiveDateTime, Utc};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub const MAX_ENTRIES: usize = 40;
pub const MAX_HIGHLIGHTS: usize = 10;
pub const MAX_TITLE: usize = 200;
pub const MAX_SUMMARY: usize = 500;
pub const MAX_ACTION: usize = 200;
pub const MAX_LINKS: usize = 4;
pub const NUMBER_EMOJI: [&str; 10] = [
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "keycap_ten",
];
pub const SEPARATOR: &str = "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}";
const SEEN_NAME: &str = "rss-watch-loop-entries";

pub const SETTINGS_FIELDS: [Field; 1] = [Field::new("interests", "Interests")
    .kind("textarea")
    .optional()
    .help("Comma-separated topics you care about")
    .placeholder("Rails security, Postgres performance, AI coding agents")];

fn load_class(f: &LoadFailure) -> String {
    match f {
        LoadFailure::Unknown => "KeyError".into(),
        LoadFailure::Config(_) => "ConnectorConfigError".into(),
        LoadFailure::Other(class) => class.clone(),
    }
}

/// `min(5, max(1, int(value)))`, 1 when `value` has no integer reading.
fn score(value: Option<&Value>) -> i64 {
    let n = match value {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        Some(Value::Bool(b)) => Some(i64::from(*b)),
        _ => None,
    };
    n.map_or(1, |n| n.clamp(1, 5))
}

/// `row.get(key, default)` as text (`str(None)` for an explicit null).
fn get_str(row: &Value, key: &str, default: &str) -> String {
    row.get(key).map_or_else(|| default.to_string(), py_str)
}

/// `h.get(key) or fallback`.
fn or_else_str(row: &Value, key: &str, fallback: &str) -> String {
    match row.get(key) {
        Some(v) if py_truthy(v) => py_str(v),
        _ => fallback.to_string(),
    }
}

/// `re.sub(r"<[^>]*>", " ", text)`.
fn strip_tags(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        match rest[start..].find('>') {
            Some(end) => {
                out.push_str(&rest[..start]);
                out.push(' ');
                rest = &rest[start + end + 1..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// Python `strftime("%Z")` of the local zone at `dt` (an abbreviation like
/// `CST`/`UTC`, not an offset).
fn local_zone_name(dt: DateTime<Utc>) -> String {
    let t = dt.timestamp() as libc::time_t;
    // SAFETY: localtime_r fills the zeroed `tm` we own; tm_zone is a static C string.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() || tm.tm_zone.is_null() {
            return String::new();
        }
        std::ffi::CStr::from_ptr(tm.tm_zone)
            .to_string_lossy()
            .into_owned()
    }
}

/// `HH:MM` (local time) of a feed's `published` string, or `""`.
fn clock(published: &str) -> String {
    let text = published.replace("GMT", "+0000");
    let local = |dt: DateTime<Utc>| dt.with_timezone(&Local).format("%H:%M").to_string();
    // "%a, %d %b %Y %H:%M:%S %z" (the weekday is not validated, like strptime).
    if let Some((_, rest)) = text.split_once(',') {
        let rest = rest.trim_start();
        if let Ok(dt) = DateTime::parse_from_str(rest, "%d %b %Y %H:%M:%S %z") {
            return local(dt.with_timezone(&Utc));
        }
    }
    // "%Y-%m-%dT%H:%M:%S%z" (%z also accepts a trailing Z).
    let iso = match text.strip_suffix('Z') {
        Some(head) => format!("{head}+0000"),
        None => text.clone(),
    };
    if let Ok(dt) = DateTime::parse_from_str(&iso, "%Y-%m-%dT%H:%M:%S%z") {
        return local(dt.with_timezone(&Utc));
    }
    // "%Y-%m-%dT%H:%M:%SZ": naive, taken as already local.
    if let Some(head) = text.strip_suffix('Z') {
        if let Ok(n) = NaiveDateTime::parse_from_str(head, "%Y-%m-%dT%H:%M:%S") {
            return n.format("%H:%M").to_string();
        }
    }
    String::new()
}

pub struct RssWatch {
    connectors: Arc<dyn ConnectorSource>,
    seen: Mutex<Option<SeenStore>>,
}

impl RssWatch {
    pub fn new(connectors: Arc<dyn ConnectorSource>) -> Self {
        Self {
            connectors,
            seen: Mutex::new(None),
        }
    }

    /// Inject the de-dup store (`seen`); otherwise opened lazily under the
    /// run's state root (default `<root>/outputs/loops`).
    pub fn with_seen(self, seen: SeenStore) -> Self {
        *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(seen);
        self
    }

    fn with_store<R>(
        &self,
        ctx: &LoopContext,
        f: impl FnOnce(&mut SeenStore) -> R,
    ) -> Result<R, PluginError> {
        let mut guard = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            let store = SeenStore::open(SEEN_NAME, ctx.state_dir.as_deref(), None)
                .map_err(|m| PluginError::new("ValueError", m))?;
            *guard = Some(store);
        }
        Ok(f(guard.as_mut().expect("store opened above")))
    }
}

impl LoopPlugin for RssWatch {
    fn loop_name(&self) -> &str {
        "rss-watch-loop"
    }
    fn definition_dir(&self) -> &str {
        "rss-watch"
    }
    fn output_keys(&self) -> &[&'static str] {
        &["highlights"]
    }
    fn settings_fields(&self) -> &'static [Field] {
        &SETTINGS_FIELDS
    }

    fn discover(&self, ctx: &LoopContext) -> Result<Vec<WorkItem>, PluginError> {
        let interests: String = ctx
            .settings
            .get("interests")
            .filter(|v| py_truthy(v))
            .map(py_str)
            .unwrap_or_default()
            .chars()
            .take(1000)
            .collect();
        let mut items = Vec::new();
        let mut refreshed = false;
        for account in self.connectors.accounts_with_capability("feed") {
            let aid = account.id;
            let conn = match self.connectors.load(&aid) {
                Ok(c) => c,
                Err(f) => {
                    ctx.log(&format!(
                        "rss-watch: account {aid} failed: {}",
                        load_class(&f)
                    ));
                    continue;
                }
            };
            let urls: Vec<String> = conn
                .account()
                .setting("feeds")
                .split(['\n', '\r'])
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .collect();
            let mut entries: Vec<Value> = Vec::new();
            let mut links: HashSet<String> = HashSet::new();
            for url in urls {
                let rows = match conn.entries(&url) {
                    Ok(r) => r,
                    Err(e) => {
                        // One feed failing never stops the rest.
                        ctx.log(&format!(
                            "rss-watch: a feed of {aid} failed: {}",
                            e.class_name()
                        ));
                        continue;
                    }
                };
                for row in rows {
                    let link = if row.is_object() {
                        chat_url(row.get("link").and_then(Value::as_str).unwrap_or(""))
                    } else {
                        String::new()
                    };
                    if link.is_empty() || links.contains(&link) {
                        continue;
                    }
                    let is_seen = self.with_store(ctx, |s| {
                        let has = s.has(&link);
                        if has {
                            // Still in the feed: refresh its timestamp so a persistent
                            // entry never ages out of the seen window and gets re-sent.
                            s.add(&link);
                        }
                        has
                    })?;
                    links.insert(link.clone());
                    if is_seen {
                        refreshed = true;
                        continue;
                    }
                    entries.push(json!({
                        "title": chat_text(&get_str(&row, "title", ""), MAX_TITLE),
                        "link": link,
                        "published": chat_text(&get_str(&row, "published", ""), 40),
                        "summary": chat_text(&strip_tags(&get_str(&row, "summary", "")), MAX_SUMMARY),
                    }));
                }
            }
            if !entries.is_empty() {
                entries.truncate(MAX_ENTRIES);
                // The local calendar day: `ctx.now.astimezone().date()`.
                let day = ctx
                    .now
                    .with_timezone(&Local)
                    .date_naive()
                    .format("%Y-%m-%d");
                items.push(WorkItem::new(
                    format!("rss:{aid}:{day}"),
                    aid,
                    json!({"interests": interests, "entries": entries}),
                ));
            }
        }
        if refreshed {
            self.with_store(ctx, |s| s.save())?
                .map_err(|e| PluginError::new("OSError", e.to_string()))?;
        }
        Ok(items)
    }

    fn after_item(
        &self,
        item: &WorkItem,
        answer: &Value,
        ctx: &LoopContext,
    ) -> Result<Outcome, PluginError> {
        let offered_list = item.payload["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let offered: HashMap<String, &Value> = offered_list
            .iter()
            .filter_map(|e| e["link"].as_str().map(|l| (l.to_string(), e)))
            .collect();
        let mut highlights: Vec<Value> = Vec::new();
        let mut used: HashSet<String> = HashSet::new();
        for h in answer
            .get("highlights")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(link) = h
                .get("link")
                .and_then(Value::as_str)
                .filter(|l| offered.contains_key(*l) && !used.contains(*l))
            else {
                continue;
            };
            used.insert(link.to_string());
            let mut extra: Vec<String> = Vec::new();
            for other in h
                .get("links")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(o) = other.as_str() {
                    if offered.contains_key(o) && o != link && !extra.iter().any(|e| e == o) {
                        extra.push(o.to_string());
                    }
                }
            }
            let mut all_links = vec![json!(link)];
            all_links.extend(extra.iter().take(MAX_LINKS - 1).map(|l| json!(l)));
            let source = or_else_str(h, "source", item.title.as_str());
            let summary = match h.get("summary").filter(|v| py_truthy(v)) {
                Some(v) => py_str(v),
                None => get_str(h, "why", ""),
            };
            highlights.push(json!({
                "title": chat_text(&get_str(h, "title", ""), MAX_TITLE),
                "link": link,
                "links": all_links,
                "source": chat_text(&source, 80),
                "published": offered[link].get("published").cloned().unwrap_or(json!("")),
                "summary": chat_text(&summary, MAX_SUMMARY),
                "do_next": chat_text(&get_str(h, "do_next", ""), MAX_ACTION),
                "kill_if": chat_text(&get_str(h, "kill_if", ""), MAX_ACTION),
                "score": score(h.get("score")),
            }));
        }
        highlights.sort_by_key(|h| -h["score"].as_i64().unwrap_or(1));
        highlights.truncate(MAX_HIGHLIGHTS);
        self.with_store(ctx, |s| {
            for link in offered.keys() {
                s.add(link);
            }
            s.save()
        })?
        .map_err(|e| PluginError::new("OSError", e.to_string()))?;
        Ok(Outcome::new(
            &item.key,
            Status::Done,
            format!("{} highlights", highlights.len()),
        )
        .with_data(json!({"account": item.title, "highlights": highlights})))
    }

    fn digest(&self, outcomes: &[Outcome], ctx: &LoopContext) -> Option<String> {
        let mut highlights: Vec<&Value> = outcomes
            .iter()
            .filter(|o| o.status == Status::Done)
            .flat_map(|o| {
                o.data
                    .get("highlights")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .collect();
        highlights.sort_by_key(|h| -h["score"].as_i64().unwrap_or(1));
        highlights.truncate(MAX_HIGHLIGHTS);
        if highlights.is_empty() {
            return None;
        }
        let now = ctx.now.with_timezone(&Local);
        let stamp = format!(
            "{} {}",
            now.format("%Y-%m-%d %H:%M"),
            local_zone_name(ctx.now)
        );
        let mut out = vec![format!("RSS Watch Brief \u{b7} {stamp}")];
        for (i, h) in highlights.iter().enumerate() {
            let text = |k: &str| h[k].as_str().unwrap_or("").to_string();
            out.extend([
                String::new(),
                SEPARATOR.to_string(),
                format!(":{}: {}", NUMBER_EMOJI[i], text("title")),
            ]);
            let meta = [clock(&text("published")), text("source")]
                .into_iter()
                .filter(|x| !x.is_empty())
                .collect::<Vec<_>>()
                .join(" \u{b7} ");
            if !meta.is_empty() {
                out.push(meta);
            }
            if !text("summary").is_empty() {
                out.extend([String::new(), text("summary")]);
            }
            if !text("do_next").is_empty() || !text("kill_if").is_empty() {
                out.push(String::new());
            }
            if !text("do_next").is_empty() {
                out.push(format!("Do next: {}", text("do_next")));
            }
            if !text("kill_if").is_empty() {
                out.push(format!("Kill if: {}", text("kill_if")));
            }
            for link in h["links"].as_array().into_iter().flatten() {
                let safe = chat_url(link.as_str().unwrap_or(""));
                if !safe.is_empty() {
                    out.push(format!("\u{2022} {safe}"));
                }
            }
        }
        out.extend([
            String::new(),
            SEPARATOR.to_string(),
            format!("Run timestamp {stamp}"),
        ]);
        Some(out.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{rss, Account, ConnResult, Connector, ConnectorError, TypeInfo};
    use crate::loop_definition::LoopDefinition;
    use crate::loopkit::{LogFn, LoopPlugin};
    use chrono::{Duration, TimeZone};
    use std::path::{Path, PathBuf};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 6, 8, 0, 0).unwrap()
    }

    fn local_day(dt: DateTime<Utc>) -> String {
        dt.with_timezone(&Local).format("%Y-%m-%d").to_string()
    }

    type EntriesFn = Arc<dyn Fn() -> Vec<Value> + Send + Sync>;

    struct Feed {
        account: Account,
        entries: EntriesFn,
    }

    impl Connector for Feed {
        fn info(&self) -> &'static TypeInfo {
            &rss::INFO
        }
        fn account(&self) -> &Account {
            &self.account
        }
        fn entries(&self, _url: &str) -> ConnResult<Vec<Value>> {
            Ok((self.entries)())
        }
    }

    struct Source {
        entries: EntriesFn,
        fail: bool,
    }

    impl ConnectorSource for Source {
        fn accounts_with_capability(&self, _c: &str) -> Vec<Account> {
            vec![Account::new("news", "rss")]
        }
        fn load(&self, _id: &str) -> Result<Box<dyn Connector>, LoadFailure> {
            if self.fail {
                return Err(LoadFailure::Other("RuntimeError".into()));
            }
            Ok(Box::new(Feed {
                account: Account::new("news", "rss").with_setting("feeds", "https://a/feed"),
                entries: self.entries.clone(),
            }))
        }
    }

    fn numbered(n: usize) -> EntriesFn {
        Arc::new(move || {
            (0..n)
                .map(|i| {
                    json!({"title": format!("t{i}"), "link": format!("https://a/{i}"),
                                "id": format!("https://a/{i}"), "published": ""})
                })
                .collect()
        })
    }

    fn store(dir: &Path, at: DateTime<Utc>) -> SeenStore {
        SeenStore::open(SEEN_NAME, Some(dir), Some(Box::new(move || at))).unwrap()
    }

    fn plugin(dir: &Path, n: usize) -> RssWatch {
        RssWatch::new(Arc::new(Source {
            entries: numbered(n),
            fail: false,
        }))
        .with_seen(store(dir, now()))
    }

    fn ctx_at(at: DateTime<Utc>) -> (LoopContext, Arc<Mutex<Vec<String>>>) {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let sink = logs.clone();
        let log: LogFn = Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_string()));
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let d = LoopDefinition::from_yaml(&repo.join("loops/rss-watch/loop.yaml")).unwrap();
        (
            LoopContext {
                loop_name: "rss-watch-loop".into(),
                run_id: "r".into(),
                now: at,
                definition: Arc::new(d),
                settings: json!({"interests": "rails"}),
                history_dir: PathBuf::new(),
                log,
                repo_root: repo,
                force: false,
                call_budget_usd: None,
                model: None,
                state_dir: None,
            },
            logs,
        )
    }

    fn ctx() -> LoopContext {
        ctx_at(now()).0
    }

    #[test]
    fn entry_cap_40() {
        let d = tempfile::tempdir().unwrap();
        let items = plugin(d.path(), 100).discover(&ctx()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].payload["entries"].as_array().unwrap().len(), 40);
        assert_eq!(items[0].key, format!("rss:news:{}", local_day(now())));
        assert_eq!(items[0].payload["interests"], "rails");
    }

    #[test]
    fn entries_marked_seen_after_item() {
        let d = tempfile::tempdir().unwrap();
        let p = plugin(d.path(), 3);
        let item = p.discover(&ctx()).unwrap().remove(0);
        p.after_item(
            &item,
            &json!({"highlights": [{"title": "t0", "link": "https://a/0", "why": "w", "score": 5}]}),
            &ctx(),
        )
        .unwrap();
        // a fresh plugin reading the same state sees all 3 offered entries as seen
        assert!(plugin(d.path(), 3).discover(&ctx()).unwrap().is_empty());
    }

    #[test]
    fn no_item_when_nothing_new() {
        let d = tempfile::tempdir().unwrap();
        assert!(plugin(d.path(), 0).discover(&ctx()).unwrap().is_empty());
    }

    #[test]
    fn unknown_links_dropped() {
        let d = tempfile::tempdir().unwrap();
        let p = plugin(d.path(), 3);
        let item = p.discover(&ctx()).unwrap().remove(0);
        let out = p
            .after_item(
                &item,
                &json!({"highlights": [
                    {"title": "x", "link": "https://evil/1", "why": "w", "score": 5},
                    {"title": "t1", "link": "https://a/1", "why": "w", "score": 3}]}),
                &ctx(),
            )
            .unwrap();
        let links: Vec<_> = out.data["highlights"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["link"].clone())
            .collect();
        assert_eq!(links, [json!("https://a/1")]);
    }

    #[test]
    fn untrusted_entries_trimmed() {
        let d = tempfile::tempdir().unwrap();
        let entries: EntriesFn = Arc::new(|| {
            vec![
                json!({"title": "x".repeat(500), "link": "https://a/ok", "id": "1", "published": ""}),
                json!({"title": "bad", "link": "javascript:alert(1)", "id": "2", "published": ""}),
            ]
        });
        let p = RssWatch::new(Arc::new(Source {
            entries,
            fail: false,
        }))
        .with_seen(store(d.path(), now()));
        let items = p.discover(&ctx()).unwrap();
        let es = items[0].payload["entries"].as_array().unwrap();
        assert_eq!(es.len(), 1);
        assert!(es[0]["title"].as_str().unwrap().chars().count() <= 200);
    }

    #[test]
    fn html_is_stripped_from_summaries() {
        let d = tempfile::tempdir().unwrap();
        let entries: EntriesFn = Arc::new(|| {
            vec![
                json!({"title": "t", "link": "https://a/1", "summary": "<p>Hi <b>there</b></p> a < b"}),
            ]
        });
        let p = RssWatch::new(Arc::new(Source {
            entries,
            fail: false,
        }))
        .with_seen(store(d.path(), now()));
        let items = p.discover(&ctx()).unwrap();
        assert_eq!(
            items[0].payload["entries"][0]["summary"],
            "Hi there a \u{2039} b"
        );
    }

    #[test]
    fn highlights_capped_sorted_and_digest() {
        let d = tempfile::tempdir().unwrap();
        let p = plugin(d.path(), 15);
        let item = p.discover(&ctx()).unwrap().remove(0);
        let hl: Vec<Value> = (0..15)
            .map(|i| {
                json!({"title": format!("t{i}"), "link": format!("https://a/{i}"),
                            "why": "w".repeat(300), "score": 1 + i % 5})
            })
            .collect();
        let out = p
            .after_item(&item, &json!({"highlights": hl}), &ctx())
            .unwrap();
        let got = out.data["highlights"].as_array().unwrap();
        assert_eq!(got.len(), 10);
        assert!(got[0]["summary"].as_str().unwrap().chars().count() <= 500);
        let scores: Vec<i64> = got.iter().map(|h| h["score"].as_i64().unwrap()).collect();
        let mut sorted = scores.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(scores, sorted);
        let text = p.digest(&[out], &ctx()).unwrap();
        assert!(text.contains("news") || text.contains("https://a/"));
    }

    #[test]
    fn digest_numbered_brief_layout() {
        let d = tempfile::tempdir().unwrap();
        let p = plugin(d.path(), 3);
        let item = p.discover(&ctx()).unwrap().remove(0);
        let hl = json!([
            {"title": "Voxtral 3", "link": "https://a/0", "links": ["https://a/1", "https://evil/x"],
             "source": "Mistral AI", "summary": "Voice stack.", "do_next": "Pilot it.",
             "kill_if": "No edge.", "score": 5},
            {"title": "Second", "link": "https://a/2", "summary": "S2.", "score": 3}]);
        let out = p
            .after_item(&item, &json!({"highlights": hl}), &ctx())
            .unwrap();
        let text = p.digest(&[out], &ctx()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with(&format!("RSS Watch Brief \u{b7} {}", local_day(now()))));
        for want in [
            ":one: Voxtral 3",
            ":two: Second",
            "Do next: Pilot it.",
            "Kill if: No edge.",
            "\u{2022} https://a/0",
            "\u{2022} https://a/1",
        ] {
            assert!(lines.contains(&want), "{want}");
        }
        assert!(!lines.iter().any(|l| l.contains("evil")));
        assert!(lines
            .last()
            .unwrap()
            .starts_with(&format!("Run timestamp {}", local_day(now()))));
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], SEPARATOR);
        assert!(lines.contains(&"Mistral AI"));
    }

    #[test]
    fn settings_fields_declared() {
        let f = SETTINGS_FIELDS[0];
        assert_eq!(f.key, "interests");
        assert!(f.kind == "textarea" && !f.required);
        let d = tempfile::tempdir().unwrap();
        assert_eq!(plugin(d.path(), 1).settings_fields().len(), 1);
    }

    #[test]
    fn persistent_feed_entries_never_expire_from_seen() {
        let d = tempfile::tempdir().unwrap();
        let mk = |n: usize, at: DateTime<Utc>| {
            RssWatch::new(Arc::new(Source {
                entries: Arc::new(move || {
                    (0..n)
                        .map(|i| json!({"title": format!("t{i}"), "link": format!("https://a/{i}"), "published": ""}))
                        .collect()
                }),
                fail: false,
            }))
            .with_seen(store(d.path(), at))
        };
        let p = mk(3, now());
        let item = p.discover(&ctx()).unwrap().remove(0);
        p.after_item(&item, &json!({"highlights": []}), &ctx())
            .unwrap();
        // 31 days later entries 0-2 are still in the feed, plus a new one
        let later = now() + Duration::days(31);
        let p = mk(4, later);
        let items = p.discover(&ctx_at(later).0).unwrap();
        let links: Vec<_> = items[0].payload["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["link"].clone())
            .collect();
        assert_eq!(links, [json!("https://a/3")]);
        p.after_item(&items[0], &json!({"highlights": []}), &ctx_at(later).0)
            .unwrap();
        let next = later + Duration::days(1);
        assert!(mk(4, next).discover(&ctx_at(next).0).unwrap().is_empty());
    }

    #[test]
    fn failing_account_and_feed_are_logged_by_class_name() {
        let d = tempfile::tempdir().unwrap();
        let (c, logs) = ctx_at(now());
        let p = RssWatch::new(Arc::new(Source {
            entries: numbered(1),
            fail: true,
        }))
        .with_seen(store(d.path(), now()));
        assert!(p.discover(&c).unwrap().is_empty());
        assert!(logs.lock().unwrap()[0].contains("RuntimeError"));

        struct Bad(Account);
        impl Connector for Bad {
            fn info(&self) -> &'static TypeInfo {
                &rss::INFO
            }
            fn account(&self) -> &Account {
                &self.0
            }
            fn entries(&self, _u: &str) -> ConnResult<Vec<Value>> {
                Err(ConnectorError::msg("boom https://secret"))
            }
        }
        struct BadSource;
        impl ConnectorSource for BadSource {
            fn accounts_with_capability(&self, _c: &str) -> Vec<Account> {
                vec![Account::new("news", "rss")]
            }
            fn load(&self, _i: &str) -> Result<Box<dyn Connector>, LoadFailure> {
                Ok(Box::new(Bad(
                    Account::new("news", "rss").with_setting("feeds", "https://a/f\n\nhttps://b/f")
                )))
            }
        }
        let p = RssWatch::new(Arc::new(BadSource)).with_seen(store(d.path(), now()));
        let (c, logs) = ctx_at(now());
        assert!(p.discover(&c).unwrap().is_empty());
        let logs = logs.lock().unwrap();
        assert_eq!(logs.len(), 2);
        assert!(logs
            .iter()
            .all(|m| m.contains("ConnectorError") && !m.contains("secret")));
    }

    #[test]
    fn helper_semantics() {
        assert_eq!(score(Some(&json!(9))), 5);
        assert_eq!(score(Some(&json!(0))), 1);
        assert_eq!(score(Some(&json!("4"))), 4);
        assert_eq!(score(Some(&json!("4.5"))), 1);
        assert_eq!(score(Some(&json!(3.9))), 3);
        assert_eq!(score(None), 1);
        assert_eq!(strip_tags("a<b>c</b>d <e"), "a c d <e");
        // the weekday is not validated, %z takes +0000 / Z, other zones fail
        assert!(!clock("Tue, 06 Oct 2026 10:00:00 GMT").is_empty());
        assert!(!clock("2026-10-06T10:00:00Z").is_empty());
        assert!(!clock("2026-10-06T10:00:00+02:00").is_empty());
        assert_eq!(clock("yesterday"), "");
        assert_eq!(clock(""), "");
    }
}
