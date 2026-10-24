//! LinkedIn Autopilot loop on LoopKit (stage 1, read-only; mirrors
//! `bin/loop_plugins/linkedin_autopilot.py`): browses the user's own signed-in
//! LinkedIn session at a human pace, collects the feed, notifications and
//! messages not seen before, has the sealed model summarize them and pick posts
//! and people worth attention, and notifies a short overview. Everything from
//! LinkedIn is untrusted: it is sanitized before the prompt, the model runs
//! sealed and every returned id must be one that was offered. Account problems
//! (expired session, security check, no browser) become failed outcomes and
//! never call the model.
//!
//! The browser automation is NOT in this file: it is owned by another stream
//! and plugs in through [`LinkedInSession`] / [`SessionFactory`] (see
//! [`register_session_factory`]) and the pacing hook [`Human`].
#![allow(
    clippy::type_complexity,
    clippy::field_reassign_with_default,
    clippy::let_unit_value
)]

use crate::connectors::{ConnectorSource, Field};
use crate::loopkit::{
    self, default_build_prompt, LoopContext, LoopPlugin, ModelAnswer, ModelError, Outcome,
    PluginError, Status, WorkItem,
};
use crate::loops_config::{py_str, py_truthy};
use crate::seen_store::{py_isoformat, SeenStore};
use chrono::{DateTime, Datelike, Local, NaiveDateTime, Timelike, Utc};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const MAX_NOTIFICATIONS: usize = 30;
pub const MAX_FEED: usize = 30;
pub const MAX_MESSAGES: usize = 20;
pub const MAX_TEXT: usize = 300;
pub const MAX_WHY: usize = 140;
pub const MAX_TOPICS: usize = 10;
pub const DEFAULT_PEOPLE_QUERY: &str = "AI builder";
pub const PROBLEM_PROMPT: &str = "__linkedin_problem__";
const LOOP_NAME: &str = "linkedin-autopilot-loop";

/// `(problem key, message)`.
pub const PROBLEMS: [(&str, &str); 8] = [
    ("session_expired", "LinkedIn session expired - reconnect it on the Connectors page"),
    ("challenge", "LinkedIn showed a security check - open LinkedIn in a normal browser, then reconnect"),
    ("browser_unavailable", "Playwright or Chrome is not available - see the Connectors page"),
    ("not_connected", "LinkedIn is not connected - press Connect LinkedIn on the Connectors page"),
    ("page_timeout", "LinkedIn did not respond or render within 10 minutes - the run was stopped, try again later"),
    ("page_changed", "LinkedIn's page layout was not recognised - no feed posts found, the loop may need an update"),
    ("schedule_outside_hours", "The loop's schedule hour is outside its active hours - change one of them on the loop's page"),
    ("error", "LinkedIn could not be read - check the loop log"),
];

fn problem_text(key: &str) -> &'static str {
    let find = |k: &str| PROBLEMS.iter().find(|(p, _)| *p == k).map(|(_, m)| *m);
    find(key).or_else(|| find("error")).unwrap_or("")
}

pub static SETTINGS_FIELDS: [Field; 5] = [
    Field::new("headless", "Run Chrome headless")
        .kind("select")
        .optional()
        .default("no")
        .options(&["no", "yes"])
        .help("Off opens a visible Chrome window while the loop runs (looks most like a real user). On is invisible but easier for LinkedIn to detect."),
    Field::new("active_hours", "Active hours")
        .optional()
        .default("9-21")
        .placeholder("9-21")
        .help("Hours (24h clock) in which the loop may browse, e.g. 9-21"),
    Field::new("people_query", "People search")
        .optional()
        .default(DEFAULT_PEOPLE_QUERY)
        .placeholder(DEFAULT_PEOPLE_QUERY)
        .help("Searched in LinkedIn's header search bar each run to collect people to review on the People tab. Leave empty to turn it off."),
    Field::new("people_target", "New people per run")
        .optional()
        .default("30")
        .placeholder("30")
        .help("How many people not collected before to look for each run (20-50)"),
    Field::new("topics", "Topics you care about")
        .kind("textarea")
        .optional()
        .help("Comma-separated topics the loop looks for")
        .placeholder("Rails, AI coding agents, hiring"),
];

// ---- the browser boundary (implemented by the browser stream) -----------------------

/// What can go wrong driving the browser (`linkedin_browser` exceptions plus
/// `PageTimeout` from `linkedin_humanize`).
#[derive(Debug, Clone, PartialEq)]
pub enum BrowserError {
    /// Playwright or Chrome missing (`BrowserUnavailable`).
    Unavailable(String),
    /// The saved session no longer works (`SessionExpired`).
    SessionExpired,
    /// LinkedIn showed a security check (`ChallengeError`).
    Challenge,
    /// A page did not respond or render within 10 minutes (`PageTimeout`).
    PageTimeout,
    /// Anything else; only the class name is ever logged.
    Other { class: String, detail: String },
}

impl BrowserError {
    pub fn class_name(&self) -> String {
        match self {
            BrowserError::Unavailable(_) => "BrowserUnavailable".into(),
            BrowserError::SessionExpired => "SessionExpired".into(),
            BrowserError::Challenge => "ChallengeError".into(),
            BrowserError::PageTimeout => "PageTimeout".into(),
            BrowserError::Other { class, .. } => class.clone(),
        }
    }
}

/// Human-like pacing (`linkedin_humanize.Humanizer`); injectable so tests are
/// deterministic. The browser stream's real humanizer implements this too.
pub trait Human: Send + Sync {
    fn sleep(&self, seconds: f64);
    /// Random delay (0..1800 s) before a scheduled run starts browsing.
    fn start_delay_seconds(&self) -> f64;
    /// Occasionally (10%) skip a whole day.
    fn skip_today(&self) -> bool;
    /// Shuffle the order in which the secondary pages are visited.
    fn shuffle(&self, items: &mut Vec<&'static str>);
}

/// One signed-in browser session (`linkedin_browser.Session` as a context
/// manager: dropping it closes the browser). Rows are JSON objects:
/// feed/notifications/messages rows carry `id`, `text`, `url` (+ `profile` for
/// feed posts); people rows carry `name`, `url`, `avatar`, `company`, `position`.
pub trait LinkedInSession {
    fn feed(&mut self) -> Result<Vec<Value>, BrowserError>;
    fn notifications(&mut self) -> Result<Vec<Value>, BrowserError>;
    fn messages(&mut self) -> Result<Vec<Value>, BrowserError>;
    /// Browse a few extra pages, like a person would.
    fn wander(&mut self) -> Result<(), BrowserError>;
    /// Search the header bar for `query`, returning up to `want` people whose
    /// profile URL is not in `known`.
    fn search_people(
        &mut self,
        query: &str,
        want: usize,
        known: &BTreeSet<String>,
    ) -> Result<Vec<Value>, BrowserError>;
}

/// `(account_id, headless, human)` -> an open session (`Err` when the browser
/// cannot be started, e.g. [`BrowserError::Unavailable`]).
pub type SessionFactory = Arc<
    dyn Fn(&str, bool, Arc<dyn Human>) -> Result<Box<dyn LinkedInSession>, BrowserError>
        + Send
        + Sync,
>;

static GLOBAL_FACTORY: Mutex<Option<SessionFactory>> = Mutex::new(None);

/// Plug in the real browser implementation (called once at startup by the
/// browser stream). Until then [`build`] returns `None`.
pub fn register_session_factory(factory: SessionFactory) {
    *GLOBAL_FACTORY.lock().unwrap_or_else(|e| e.into_inner()) = Some(factory);
}

fn global_factory() -> Option<SessionFactory> {
    GLOBAL_FACTORY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// The plugin when a browser backend is registered, else `None` (the caller
/// then falls back to the legacy `python3` entry).
pub fn build(connectors: Arc<dyn ConnectorSource>) -> Option<Arc<dyn LoopPlugin>> {
    let factory = global_factory()?;
    Some(Arc::new(
        LinkedInAutopilot::new(connectors).with_session_factory(factory),
    ))
}

// ---- default pacing -----------------------------------------------------------------

/// A small default [`Human`] (xorshift RNG seeded from the clock).
pub struct SystemHuman {
    state: Mutex<u64>,
}

impl Default for SystemHuman {
    fn default() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64);
        Self::seeded(nanos ^ (u64::from(std::process::id()) << 32))
    }
}

impl SystemHuman {
    pub fn seeded(seed: u64) -> Self {
        Self {
            state: Mutex::new(seed | 1),
        }
    }
    fn next_f64(&self) -> f64 {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        (*s >> 11) as f64 / (1u64 << 53) as f64
    }
}

impl Human for SystemHuman {
    fn sleep(&self, seconds: f64) {
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds.max(0.0)));
    }
    fn start_delay_seconds(&self) -> f64 {
        self.next_f64() * 1800.0
    }
    fn skip_today(&self) -> bool {
        self.next_f64() < 0.1
    }
    fn shuffle(&self, items: &mut Vec<&'static str>) {
        for i in (1..items.len()).rev() {
            let j = (self.next_f64() * (i + 1) as f64) as usize;
            items.swap(i, j.min(i));
        }
    }
}

/// `'9-21'` -> `(9, 21)`; anything malformed or out of range -> `(9, 21)`.
pub fn parse_active_hours(value: Option<&Value>) -> (i64, i64) {
    let default = (9, 21);
    let Some(text) = value.and_then(Value::as_str) else {
        return default;
    };
    let Some((a, b)) = text.split_once('-') else {
        return default;
    };
    let digits = |s: &str| {
        let t = s.trim();
        (!t.is_empty() && t.len() <= 2 && t.chars().all(|c| c.is_ascii_digit()))
            .then(|| t.parse::<i64>().ok())?
    };
    match (digits(a), digits(b)) {
        (Some(s), Some(e)) if 0 <= s && s < e && e <= 24 => (s, e),
        _ => default,
    }
}

pub fn within_hours(hour: i64, window: (i64, i64)) -> bool {
    window.0 <= hour && hour < window.1
}

// ---- the people store (`bin/linkedin_people.py`) ---------------------------------------

pub mod people {
    use super::*;

    pub const MIN_TARGET: i64 = 20;
    pub const MAX_TARGET: i64 = 50;
    pub const DEFAULT_TARGET: i64 = 30;
    pub const MAX_STORED: usize = 1000;

    pub fn default_path(state_dir: Option<&Path>) -> PathBuf {
        let root = state_dir.map_or_else(crate::seen_store::default_state_root, Path::to_path_buf);
        root.join(LOOP_NAME).join("people.json")
    }

    /// How many NEW people one run should collect: 20-50, default 30.
    pub fn target(value: Option<&Value>) -> usize {
        let parsed = value.map(py_str).and_then(|s| s.trim().parse::<i64>().ok());
        parsed.map_or(DEFAULT_TARGET, |n| n.clamp(MIN_TARGET, MAX_TARGET)) as usize
    }

    pub fn load(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| match v {
                Value::Array(a) => Some(
                    a.into_iter()
                        .filter(|r| r.is_object() && r.get("url").is_some_and(py_truthy))
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub fn known_urls(path: &Path) -> BTreeSet<String> {
        load(path)
            .iter()
            .filter_map(|r| r.get("url").map(py_str))
            .collect()
    }

    /// `json.dumps(value, indent=1)`: the 2-space pretty form with each
    /// indentation level halved (string contents never hold raw newlines).
    fn dumps_indent1(value: &Value) -> String {
        crate::pyjson::to_string_pretty(value)
            .lines()
            .map(|l| {
                let lead = l.len() - l.trim_start_matches(' ').len();
                format!("{}{}", " ".repeat(lead / 2), &l[lead..])
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Add rows (newest first), refresh ones already known; returns how many
    /// were new.
    pub fn merge(
        rows: &[Value],
        query: &str,
        now: DateTime<Utc>,
        path: &Path,
    ) -> std::io::Result<usize> {
        let stamp = py_isoformat(now);
        let mut existing = load(path);
        let mut fresh: Vec<Value> = Vec::new();
        for row in rows {
            let Some(url) = row.get("url").filter(|v| py_truthy(v)).map(py_str) else {
                continue;
            };
            let mut keep = Map::new();
            for k in ["name", "url", "avatar", "company", "position"] {
                keep.insert(k.into(), row.get(k).cloned().unwrap_or_else(|| json!("")));
            }
            let known = fresh
                .iter_mut()
                .chain(existing.iter_mut())
                .find(|r| r.get("url").map(py_str).as_deref() == Some(url.as_str()));
            if let Some(Value::Object(cur)) = known {
                for (k, v) in &keep {
                    if py_truthy(v) {
                        cur.insert(k.clone(), v.clone());
                    }
                }
                cur.insert("last_seen".into(), json!(stamp));
            } else {
                keep.insert("query".into(), json!(query));
                keep.insert("first_seen".into(), json!(stamp));
                keep.insert("last_seen".into(), json!(stamp));
                fresh.push(Value::Object(keep));
            }
        }
        let added = fresh.len();
        let mut merged: Vec<Value> = fresh.into_iter().rev().collect();
        merged.extend(existing);
        merged.truncate(MAX_STORED);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, dumps_indent1(&Value::Array(merged)))?;
        std::fs::rename(&tmp, path)?;
        Ok(added)
    }
}

// ---- the plugin ---------------------------------------------------------------------

type ConnectedFn = Box<dyn Fn(&str) -> bool + Send + Sync>;
type DisconnectFn = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;
type ScheduleHourFn = Box<dyn Fn() -> Option<i64> + Send + Sync>;
type LocalFn = Arc<dyn Fn(DateTime<Utc>) -> NaiveDateTime + Send + Sync>;

pub struct LinkedInAutopilot {
    connectors: Arc<dyn ConnectorSource>,
    session_factory: Option<SessionFactory>,
    human: Option<Arc<dyn Human>>,
    seen: Mutex<Option<SeenStore>>,
    loop_seen: Mutex<Option<SeenStore>>,
    connected_fn: Option<ConnectedFn>,
    disconnect_fn: Option<DisconnectFn>,
    schedule_hour_fn: Option<ScheduleHourFn>,
    people_path: Option<PathBuf>,
    local: LocalFn,
}

fn default_connected(account_id: &str) -> bool {
    valid_account_id(account_id) && marker_path(account_id).is_file()
}

fn default_disconnect(account_id: &str) -> Result<(), String> {
    if !valid_account_id(account_id) {
        return Err("invalid LinkedIn account id".into());
    }
    match std::fs::remove_file(marker_path(account_id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

fn marker_path(account_id: &str) -> PathBuf {
    crate::config::home_dir()
        .join("linkedin")
        .join(account_id)
        .join("connected")
}

fn valid_account_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.len() <= 48
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn default_schedule_hour() -> Option<i64> {
    let entry =
        crate::loops_config::get_loop(LOOP_NAME, &crate::loops_config::default_config_path(), None)
            .ok()?;
    entry.get("schedule")?.get("hour")?.as_i64()
}

fn as_list(value: Option<&Value>) -> Vec<Value> {
    value.and_then(Value::as_array).cloned().unwrap_or_default()
}

fn clean(rows: &[Value], cap: usize) -> Vec<Value> {
    let text = |r: &Value, k: &str| r.get(k).map_or(String::new(), py_str);
    rows.iter()
        .take(cap)
        .filter(|r| r.is_object() && r.get("id").is_some_and(py_truthy))
        .map(|r| {
            json!({
                "id": py_str(&r["id"]),
                "text": loopkit::chat_text(&text(r, "text"), MAX_TEXT),
                "url": loopkit::chat_url(&text(r, "url")),
                "profile": loopkit::chat_url(&text(r, "profile")),
            })
        })
        .collect()
}

impl LinkedInAutopilot {
    pub fn new(connectors: Arc<dyn ConnectorSource>) -> Self {
        Self {
            connectors,
            session_factory: None,
            human: None,
            seen: Mutex::new(None),
            loop_seen: Mutex::new(None),
            connected_fn: None,
            disconnect_fn: None,
            schedule_hour_fn: None,
            people_path: None,
            local: Arc::new(|d| d.with_timezone(&Local).naive_local()),
        }
    }

    pub fn with_session_factory(mut self, f: SessionFactory) -> Self {
        self.session_factory = Some(f);
        self
    }
    pub fn with_human(mut self, h: Arc<dyn Human>) -> Self {
        self.human = Some(h);
        self
    }
    /// The per-row seen store (default: `linkedin-autopilot-loop-items`).
    pub fn with_seen(mut self, s: SeenStore) -> Self {
        self.seen = Mutex::new(Some(s));
        self
    }
    /// The once-a-day store (default: `linkedin-autopilot-loop`).
    pub fn with_loop_seen(mut self, s: SeenStore) -> Self {
        self.loop_seen = Mutex::new(Some(s));
        self
    }
    pub fn with_connected_fn(mut self, f: ConnectedFn) -> Self {
        self.connected_fn = Some(f);
        self
    }
    pub fn with_disconnect_fn(mut self, f: DisconnectFn) -> Self {
        self.disconnect_fn = Some(f);
        self
    }
    pub fn with_schedule_hour_fn(mut self, f: ScheduleHourFn) -> Self {
        self.schedule_hour_fn = Some(f);
        self
    }
    pub fn with_people_path(mut self, p: PathBuf) -> Self {
        self.people_path = Some(p);
        self
    }
    /// How a UTC instant becomes the local wall clock (default: system zone).
    pub fn with_local_clock(mut self, f: LocalFn) -> Self {
        self.local = f;
        self
    }

    fn with_store<R>(
        slot: &Mutex<Option<SeenStore>>,
        name: &str,
        ctx: &LoopContext,
        f: impl FnOnce(&mut SeenStore) -> R,
    ) -> Result<R, PluginError> {
        let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            let now = ctx.now;
            *guard = Some(
                SeenStore::open(name, ctx.state_dir.as_deref(), Some(Box::new(move || now)))
                    .map_err(|m| PluginError::new("ValueError", m))?,
            );
        }
        Ok(f(guard.as_mut().expect("just set")))
    }

    fn is_connected(&self, aid: &str) -> bool {
        match &self.connected_fn {
            Some(f) => f(aid),
            None => default_connected(aid),
        }
    }

    fn disconnect(&self, aid: &str, ctx: &LoopContext) {
        let result = match &self.disconnect_fn {
            Some(f) => f(aid),
            None => default_disconnect(aid),
        };
        if result.is_err() {
            ctx.log(&format!(
                "linkedin-autopilot: could not mark {aid} disconnected: Exception"
            ));
        }
    }

    fn schedule_hour(&self) -> Option<i64> {
        match &self.schedule_hour_fn {
            Some(f) => f(),
            None => default_schedule_hour(),
        }
    }

    fn people_file(&self, ctx: &LoopContext) -> PathBuf {
        self.people_path
            .clone()
            .unwrap_or_else(|| people::default_path(ctx.state_dir.as_deref()))
    }

    /// Search for new people; a failure here never costs the run its feed.
    fn collect_people(
        &self,
        session: &mut dyn LinkedInSession,
        settings: &Value,
        ctx: &LoopContext,
    ) -> Result<(), BrowserError> {
        let query: String = match settings.get("people_query") {
            None => DEFAULT_PEOPLE_QUERY.to_string(),
            Some(v) if py_truthy(v) => py_str(v).trim().chars().take(100).collect(),
            Some(_) => String::new(),
        };
        if query.is_empty() {
            return Ok(());
        }
        let want = people::target(settings.get("people_target"));
        let path = self.people_file(ctx);
        let outcome = session
            .search_people(&query, want, &people::known_urls(&path))
            .and_then(|rows| {
                people::merge(&rows, &query, ctx.now, &path).map_err(|e| BrowserError::Other {
                    class: "OSError".into(),
                    detail: e.to_string(),
                })
            });
        match outcome {
            Ok(added) => {
                ctx.log(&format!(
                    "linkedin-autopilot: {added} new people for {}",
                    crate::loops_config::py_repr_str(&query)
                ));
                Ok(())
            }
            Err(e @ (BrowserError::SessionExpired | BrowserError::Challenge)) => Err(e),
            Err(e) => {
                ctx.log(&format!(
                    "linkedin-autopilot: people search failed: {}",
                    e.class_name()
                ));
                Ok(())
            }
        }
    }

    fn collect(
        &self,
        aid: &str,
        settings: &Value,
        human: &Arc<dyn Human>,
        ctx: &LoopContext,
    ) -> Result<Vec<(&'static str, Vec<Value>)>, BrowserError> {
        let headless = settings
            .get("headless")
            .filter(|v| py_truthy(v))
            .map_or("no".to_string(), py_str)
            == "yes";
        let factory = self
            .session_factory
            .clone()
            .or_else(global_factory)
            .ok_or_else(|| {
                BrowserError::Unavailable("no browser session factory registered".into())
            })?;
        let mut session = factory(aid, headless, human.clone())?;
        let mut out: Vec<(&'static str, Vec<Value>)> = vec![("feed", session.feed()?)];
        let mut order = vec!["notifications", "messages", "wander", "people"];
        human.shuffle(&mut order);
        for name in order {
            match name {
                "notifications" => out.push(("notifications", session.notifications()?)),
                "messages" => out.push(("messages", session.messages()?)),
                "wander" => session.wander()?,
                _ => self.collect_people(session.as_mut(), settings, ctx)?,
            }
        }
        Ok(out)
    }

    fn problem(key: &str, aid: &str, problem: &str) -> WorkItem {
        WorkItem::new(key, aid, json!({"problem": problem}))
    }

    fn accounts(&self) -> Vec<crate::connectors::Account> {
        self.connectors
            .accounts_with_capability(crate::connectors::SOCIAL)
            .into_iter()
            .filter(|a| a.kind == "linkedin")
            .collect()
    }
}

impl LoopPlugin for LinkedInAutopilot {
    fn loop_name(&self) -> &str {
        LOOP_NAME
    }
    fn definition_dir(&self) -> &str {
        "linkedin-autopilot"
    }
    fn output_keys(&self) -> &[&'static str] {
        &["summary", "topics", "worth_reading", "people"]
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
        let human: Arc<dyn Human> = self
            .human
            .clone()
            .unwrap_or_else(|| Arc::new(SystemHuman::default()));
        let local = (self.local)(ctx.now);
        let date = format!(
            "{:04}-{:02}-{:02}",
            local.year(),
            local.month(),
            local.day()
        );
        if !ctx.force {
            let window = parse_active_hours(settings.get("active_hours"));
            if let Some(hour) = self.schedule_hour() {
                if !within_hours(hour, window) {
                    return Ok(vec![Self::problem(
                        &format!("linkedin:schedule:{date}"),
                        "schedule",
                        "schedule_outside_hours",
                    )]);
                }
            }
            if !within_hours(i64::from(local.hour()), window) {
                ctx.log("linkedin-autopilot: outside active hours");
                return Ok(vec![]);
            }
            if human.skip_today() {
                ctx.log("linkedin-autopilot: skipping today");
                return Ok(vec![]);
            }
        }
        let topics: String = settings
            .get("topics")
            .filter(|v| py_truthy(v))
            .map(py_str)
            .unwrap_or_default()
            .chars()
            .take(1000)
            .collect();
        let mut items = Vec::new();
        for account in self.accounts() {
            let aid = account.id.clone();
            let key = format!("linkedin:{aid}:{date}");
            if !self.is_connected(&aid) {
                items.push(Self::problem(&key, &aid, "not_connected"));
                continue;
            }
            if !ctx.force {
                let ran = Self::with_store(&self.loop_seen, LOOP_NAME, ctx, |s| s.has(&key))?;
                if ran {
                    ctx.log(&format!("linkedin-autopilot: {aid} already ran today"));
                    continue;
                }
                human.sleep(human.start_delay_seconds());
            }
            let data = match self.collect(&aid, &settings, &human, ctx) {
                Ok(d) => d,
                Err(BrowserError::SessionExpired) => {
                    self.disconnect(&aid, ctx);
                    items.push(Self::problem(&key, &aid, "session_expired"));
                    continue;
                }
                Err(BrowserError::Challenge) => {
                    self.disconnect(&aid, ctx);
                    items.push(Self::problem(&key, &aid, "challenge"));
                    continue;
                }
                Err(BrowserError::PageTimeout) => {
                    items.push(Self::problem(&key, &aid, "page_timeout"));
                    continue;
                }
                Err(BrowserError::Unavailable(_)) => {
                    items.push(Self::problem(&key, &aid, "browser_unavailable"));
                    continue;
                }
                Err(e) => {
                    ctx.log(&format!(
                        "linkedin-autopilot: account {aid} failed: {}",
                        e.class_name()
                    ));
                    items.push(Self::problem(&key, &aid, "error"));
                    continue;
                }
            };
            if !data
                .iter()
                .any(|(n, rows)| *n == "feed" && !rows.is_empty())
            {
                items.push(Self::problem(&key, &aid, "page_changed"));
                continue;
            }
            let fresh =
                Self::with_store(&self.seen, "linkedin-autopilot-loop-items", ctx, |seen| {
                    data.iter()
                        .map(|(name, rows)| {
                            let kept: Vec<Value> = rows
                                .iter()
                                .filter(|r| {
                                    r.is_object()
                                        && r.get("id").is_some_and(py_truthy)
                                        && !seen.has(&py_str(&r["id"]))
                                })
                                .cloned()
                                .collect();
                            (*name, kept)
                        })
                        .collect::<Vec<_>>()
                })?;
            if fresh.iter().all(|(_, rows)| rows.is_empty()) {
                continue;
            }
            let rows_of = |n: &str| {
                fresh
                    .iter()
                    .find(|(k, _)| *k == n)
                    .map(|(_, r)| r.clone())
                    .unwrap_or_default()
            };
            items.push(WorkItem::new(
                key,
                aid,
                json!({
                    "topics": topics,
                    "notifications": clean(&rows_of("notifications"), MAX_NOTIFICATIONS),
                    "feed": clean(&rows_of("feed"), MAX_FEED),
                    "messages": clean(&rows_of("messages"), MAX_MESSAGES),
                }),
            ));
        }
        Ok(items)
    }

    fn build_prompt(&self, item: &WorkItem, ctx: &LoopContext) -> Result<String, PluginError> {
        if item.payload.get("problem").is_some_and(py_truthy) {
            return Ok(PROBLEM_PROMPT.to_string());
        }
        default_build_prompt(self.definition_dir(), item, ctx)
    }

    fn call_model(&self, prompt: &str, ctx: &LoopContext) -> Result<ModelAnswer, ModelError> {
        if prompt == PROBLEM_PROMPT {
            let empty = json!({"summary": "", "topics": [], "worth_reading": [], "people": []});
            return Ok(ModelAnswer {
                text: crate::pydumps::dumps(&empty),
                cost_usd: Some(0.0),
            });
        }
        loopkit::default_call_model(prompt, ctx)
    }

    fn after_item(
        &self,
        item: &WorkItem,
        answer: &Value,
        ctx: &LoopContext,
    ) -> Result<Outcome, PluginError> {
        let payload = &item.payload;
        if let Some(problem) = payload.get("problem").filter(|v| py_truthy(v)).map(py_str) {
            return Ok(
                Outcome::new(&item.key, Status::Failed, problem_text(&problem))
                    .with_data(json!({"problem": problem})),
            );
        }
        // Mark the offered rows seen first: a malformed answer must never get
        // the same rows re-offered (and re-paid for) on every run.
        let (notifications, feed_rows, messages) = (
            as_list(payload.get("notifications")),
            as_list(payload.get("feed")),
            as_list(payload.get("messages")),
        );
        Self::with_store(&self.seen, "linkedin-autopilot-loop-items", ctx, |seen| {
            for rows in [&notifications, &feed_rows, &messages] {
                for row in rows {
                    seen.add(&py_str(&row["id"]));
                }
            }
            seen.save()
        })?
        .map_err(|e| PluginError::new("OSError", e.to_string()))?;
        let feed: std::collections::BTreeMap<String, &Value> =
            feed_rows.iter().map(|r| (py_str(&r["id"]), r)).collect();
        let why =
            |v: &Value| loopkit::chat_text(&v.get("why").map_or(String::new(), py_str), MAX_WHY);
        let (mut worth, mut people_out) = (Vec::new(), Vec::new());
        let (mut used_worth, mut used_people) = (BTreeSet::new(), BTreeSet::new());
        for w in as_list(answer.get("worth_reading")) {
            let Some(row) = w
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| feed.get(id))
            else {
                continue;
            };
            let id = py_str(&row["id"]);
            if !used_worth.insert(id.clone()) {
                continue;
            }
            worth.push(json!({
                "id": id, "title": loopkit::chat_text(&py_str(&row["text"]), 100),
                "url": row["url"], "why": why(&w)}));
        }
        for p in as_list(answer.get("people")) {
            let Some(row) = p
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| feed.get(id))
            else {
                continue;
            };
            if !py_truthy(&row["profile"]) {
                continue;
            }
            let id = py_str(&row["id"]);
            if !used_people.insert(id.clone()) {
                continue;
            }
            people_out.push(json!({
                "id": id, "title": loopkit::chat_text(&py_str(&row["text"]), 60),
                "profile": row["profile"], "url": row["url"], "why": why(&p)}));
        }
        let topics: Vec<String> = as_list(answer.get("topics"))
            .iter()
            .filter_map(Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .map(|t| loopkit::chat_text(t, 40))
            .take(MAX_TOPICS)
            .collect();
        let counts = json!({"notifications": notifications.len(), "messages": messages.len(), "feed": feed_rows.len()});
        Ok(Outcome::new(
            &item.key,
            Status::Done,
            format!(
                "{} notifications, {} messages, {} posts",
                notifications.len(),
                messages.len(),
                feed_rows.len()
            ),
        )
        .with_data(json!({
            "account": item.title, "label": "LinkedIn",
            "summary": loopkit::chat_text(&answer.get("summary").map_or(String::new(), py_str), 600),
            "topics": topics, "worth_reading": worth, "people": people_out, "counts": counts})))
    }

    fn digest(&self, outcomes: &[Outcome], ctx: &LoopContext) -> Option<String> {
        let mut lines: Vec<String> = Vec::new();
        for o in outcomes {
            match o.status {
                Status::Failed => {
                    let problem = o.data.get("problem").is_some_and(py_truthy);
                    let text = if problem {
                        o.summary.as_str()
                    } else {
                        "the run failed - check the loop log"
                    };
                    lines.push(String::new());
                    lines.push(format!("LinkedIn needs attention: {text}"));
                }
                Status::Done => {
                    let d = &o.data;
                    let c = &d["counts"];
                    lines.push(String::new());
                    lines.push(loopkit::chat_text(
                        &d.get("account").map_or(String::new(), py_str),
                        60,
                    ));
                    if let Some(s) = d.get("summary").filter(|v| py_truthy(v)) {
                        lines.push(py_str(s));
                    }
                    lines.push(format!(
                        "{} notifications, {} messages, {} feed posts",
                        py_str(&c["notifications"]),
                        py_str(&c["messages"]),
                        py_str(&c["feed"])
                    ));
                    let worth = as_list(d.get("worth_reading"));
                    if !worth.is_empty() {
                        lines.push("Worth reading:".into());
                        for w in &worth {
                            lines.push(format!(
                                "- {} - {}",
                                loopkit::chat_link(&py_str(&w["title"]), &py_str(&w["url"])),
                                py_str(&w["why"])
                            ));
                        }
                    }
                    let ppl = as_list(d.get("people"));
                    if !ppl.is_empty() {
                        lines.push("People to know:".into());
                        for p in &ppl {
                            lines.push(format!(
                                "- {} - {}",
                                loopkit::chat_link(&py_str(&p["title"]), &py_str(&p["profile"])),
                                py_str(&p["why"])
                            ));
                        }
                    }
                }
                Status::Skipped => {}
            }
        }
        if lines.is_empty() {
            return None;
        }
        let local = (self.local)(ctx.now);
        let mut out = vec![format!(
            "LinkedIn Autopilot - {:04}-{:02}-{:02}",
            local.year(),
            local.month(),
            local.day()
        )];
        out.extend(lines);
        Some(out.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{Account, Connector, LoadFailure};
    use crate::loop_definition::LoopDefinition;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 6, 10, 0, 0).unwrap()
    }

    fn repo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn ctx_at(at: DateTime<Utc>, settings: Value, force: bool) -> LoopContext {
        let def =
            LoopDefinition::from_yaml(&repo().join("loops/linkedin-autopilot/loop.yaml")).unwrap();
        LoopContext {
            loop_name: LOOP_NAME.into(),
            run_id: "r".into(),
            now: at,
            definition: Arc::new(def),
            settings,
            history_dir: PathBuf::from("/nonexistent"),
            log: Arc::new(|_| {}),
            repo_root: repo(),
            force,
            call_budget_usd: None,
            model: None,
            state_dir: None,
        }
    }

    fn ctx() -> LoopContext {
        ctx_at(now(), json!({"topics": "rails, ai agents"}), true)
    }

    fn ctx_with(settings: Value, force: bool) -> LoopContext {
        ctx_at(now(), settings, force)
    }

    fn feed() -> Vec<Value> {
        (0..40)
            .map(|i| {
                json!({"id": format!("urn:li:activity:{i}"), "text": format!("post {i}"),
                 "url": format!("https://www.linkedin.com/feed/update/urn:li:activity:{i}/"),
                 "profile": if i % 2 == 0 { format!("https://www.linkedin.com/in/p{i}/") } else { String::new() }})
            })
            .collect()
    }
    fn notes() -> Vec<Value> {
        (0..35).map(|i| json!({"id": format!("notification:{i}"), "text": format!("note {i}"), "url": ""})).collect()
    }
    fn msgs() -> Vec<Value> {
        (0..25)
            .map(|i| json!({"id": format!("message:{i}"), "text": format!("dm {i}"), "url": ""}))
            .collect()
    }

    #[derive(Default)]
    struct World {
        feed_rows: Vec<Value>,
        note_rows: Vec<Value>,
        msg_rows: Vec<Value>,
        error: Option<BrowserError>,
        people_rows: Vec<Value>,
        people_error: Option<BrowserError>,
        /// `(account, headless, calls, people_args)`
        sessions: Vec<(
            String,
            bool,
            Vec<String>,
            Option<(String, usize, BTreeSet<String>)>,
        )>,
    }

    type W = Arc<Mutex<World>>;

    fn world() -> W {
        Arc::new(Mutex::new(World {
            feed_rows: feed(),
            note_rows: notes(),
            msg_rows: msgs(),
            ..Default::default()
        }))
    }

    struct FakeSession {
        w: W,
        idx: usize,
    }

    impl FakeSession {
        fn go(
            &self,
            name: &str,
            rows: impl FnOnce(&World) -> Vec<Value>,
        ) -> Result<Vec<Value>, BrowserError> {
            let mut w = self.w.lock().unwrap();
            w.sessions[self.idx].2.push(name.into());
            if let Some(e) = &w.error {
                return Err(e.clone());
            }
            Ok(rows(&w))
        }
    }

    impl LinkedInSession for FakeSession {
        fn feed(&mut self) -> Result<Vec<Value>, BrowserError> {
            self.go("feed", |w| w.feed_rows.clone())
        }
        fn notifications(&mut self) -> Result<Vec<Value>, BrowserError> {
            self.go("notifications", |w| w.note_rows.clone())
        }
        fn messages(&mut self) -> Result<Vec<Value>, BrowserError> {
            self.go("messages", |w| w.msg_rows.clone())
        }
        fn wander(&mut self) -> Result<(), BrowserError> {
            self.w.lock().unwrap().sessions[self.idx]
                .2
                .push("wander".into());
            Ok(())
        }
        fn search_people(
            &mut self,
            q: &str,
            want: usize,
            known: &BTreeSet<String>,
        ) -> Result<Vec<Value>, BrowserError> {
            let mut w = self.w.lock().unwrap();
            w.sessions[self.idx].2.push("people".into());
            w.sessions[self.idx].3 = Some((q.into(), want, known.clone()));
            match &w.people_error {
                Some(e) => Err(e.clone()),
                None => Ok(w.people_rows.clone()),
            }
        }
    }

    fn factory(w: &W) -> SessionFactory {
        let w = w.clone();
        Arc::new(move |account, headless, _human| {
            let mut g = w.lock().unwrap();
            g.sessions.push((account.into(), headless, vec![], None));
            let idx = g.sessions.len() - 1;
            Ok(Box::new(FakeSession { w: w.clone(), idx }) as Box<dyn LinkedInSession>)
        })
    }

    struct FakeHuman {
        sleeps: Mutex<Vec<f64>>,
        skip: bool,
        delay: f64,
        seed: u64,
    }

    impl FakeHuman {
        fn new(seed: u64) -> Arc<Self> {
            Arc::new(Self {
                sleeps: Mutex::new(vec![]),
                skip: false,
                delay: 100.0,
                seed,
            })
        }
    }

    impl Human for FakeHuman {
        fn sleep(&self, s: f64) {
            self.sleeps.lock().unwrap().push(s);
        }
        fn start_delay_seconds(&self) -> f64 {
            self.delay
        }
        fn skip_today(&self) -> bool {
            self.skip
        }
        fn shuffle(&self, items: &mut Vec<&'static str>) {
            SystemHuman::seeded(self.seed.wrapping_mul(0x9E3779B97F4A7C15)).shuffle(items);
        }
    }

    struct Source;
    impl ConnectorSource for Source {
        fn accounts_with_capability(&self, _c: &str) -> Vec<Account> {
            vec![Account::new("li", "linkedin"), Account::new("other", "rss")]
        }
        fn load(&self, _id: &str) -> Result<Box<dyn Connector>, LoadFailure> {
            Err(LoadFailure::Unknown)
        }
    }

    fn store(dir: &Path, name: &str) -> SeenStore {
        let at = now();
        SeenStore::open(name, Some(dir), Some(Box::new(move || at))).unwrap()
    }

    #[allow(dead_code)]
    struct Fx {
        w: W,
        disconnects: Arc<Mutex<Vec<String>>>,
    }

    fn plugin_h(
        dir: &Path,
        w: &W,
        human: Arc<dyn Human>,
    ) -> (LinkedInAutopilot, Arc<Mutex<Vec<String>>>) {
        let disconnects: Arc<Mutex<Vec<String>>> = Default::default();
        let d = disconnects.clone();
        let p = LinkedInAutopilot::new(Arc::new(Source))
            .with_session_factory(factory(w))
            .with_human(human)
            .with_seen(store(dir, "linkedin-autopilot-loop-items"))
            .with_loop_seen(store(&dir.join("loopseen"), LOOP_NAME))
            .with_connected_fn(Box::new(|_| true))
            .with_disconnect_fn(Box::new(move |a| {
                d.lock().unwrap().push(a.into());
                Ok(())
            }))
            .with_schedule_hour_fn(Box::new(|| None))
            .with_people_path(dir.join("people.json"))
            .with_local_clock(Arc::new(|d| d.naive_utc()));
        (p, disconnects)
    }

    fn plugin(dir: &Path, w: &W) -> (LinkedInAutopilot, Fx) {
        let (p, disconnects) = plugin_h(dir, w, FakeHuman::new(0));
        (
            p,
            Fx {
                w: w.clone(),
                disconnects,
            },
        )
    }

    fn empty_answer() -> Value {
        json!({"summary": "", "topics": [], "worth_reading": [], "people": []})
    }

    #[test]
    fn discover_caps_and_sanitizes() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        let mut rows = feed();
        rows[0] = json!({"id": "urn:li:activity:1", "text": format!("<!channel> hi\n<@U1> {}", "x".repeat(900)),
            "url": "https://www.linkedin.com/feed/update/urn:li:activity:1/", "profile": "javascript:alert(1)"});
        w.lock().unwrap().feed_rows = rows;
        let (p, _fx) = plugin(d.path(), &w);
        let items = p.discover(&ctx()).unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(
            (item.key.as_str(), item.title.as_str()),
            ("linkedin:li:2026-10-06", "li")
        );
        let pl = &item.payload;
        assert_eq!(pl["feed"].as_array().unwrap().len(), 30);
        assert_eq!(pl["notifications"].as_array().unwrap().len(), 30);
        assert_eq!(pl["messages"].as_array().unwrap().len(), 20);
        let first = &pl["feed"][0];
        let t = first["text"].as_str().unwrap();
        assert!(
            !t.contains('<') && !t.contains('>') && !t.contains('\n') && t.chars().count() <= 300
        );
        assert_eq!(first["profile"], "");
        assert_eq!(pl["topics"], "rails, ai agents");
        let accounts: Vec<_> = w
            .lock()
            .unwrap()
            .sessions
            .iter()
            .map(|s| s.0.clone())
            .collect();
        assert_eq!(accounts, ["li"]);
    }

    #[test]
    fn headless_setting_reaches_the_session() {
        let w = world();
        for (i, settings) in [json!({"headless": "yes"}), json!({})]
            .into_iter()
            .enumerate()
        {
            let d = tempfile::tempdir().unwrap();
            let (p, _) = plugin(d.path(), &w);
            p.discover(&ctx_with(settings, true)).unwrap();
            assert_eq!(w.lock().unwrap().sessions[i].1, i == 0);
        }
    }

    #[test]
    fn reads_feed_first_then_the_rest_in_varying_order() {
        let mut orders = BTreeSet::new();
        for seed in 0..12u64 {
            let d = tempfile::tempdir().unwrap();
            let w = world();
            let (p, _) = plugin_h(d.path(), &w, FakeHuman::new(seed));
            p.discover(&ctx()).unwrap();
            let calls = w.lock().unwrap().sessions[0].2.clone();
            assert_eq!(calls[0], "feed");
            let mut rest = calls[1..].to_vec();
            rest.sort();
            assert_eq!(rest, ["messages", "notifications", "people", "wander"]);
            orders.insert(calls);
        }
        assert!(orders.len() > 1);
    }

    fn answer_full() -> Value {
        json!({"summary": "A busy day <@U1>", "topics": ["rails", 5, "x".repeat(99)],
            "worth_reading": [{"id": "urn:li:activity:2", "why": "w".repeat(400)},
                {"id": "urn:li:activity:2", "why": "dup"},
                {"id": "urn:li:activity:99", "why": "never offered"}, "junk", {"id": 3}],
            "people": [{"id": "urn:li:activity:2", "why": "ships Rails"},
                {"id": "urn:li:activity:1", "why": "no profile url offered"},
                {"id": "evil", "why": "x"}]})
    }

    #[test]
    fn after_item_validates_ids_and_marks_seen() {
        let d = tempfile::tempdir().unwrap();
        let (p, _fx) = plugin(d.path(), &world());
        let item = p.discover(&ctx()).unwrap().remove(0);
        let out = p.after_item(&item, &answer_full(), &ctx()).unwrap();
        let data = &out.data;
        assert_eq!(out.status, Status::Done);
        assert_eq!(
            data["counts"],
            json!({"notifications": 30, "messages": 20, "feed": 30})
        );
        assert_eq!(data["worth_reading"].as_array().unwrap().len(), 1);
        assert_eq!(data["worth_reading"][0]["id"], "urn:li:activity:2");
        assert!(
            data["worth_reading"][0]["why"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= 140
        );
        assert_eq!(data["people"].as_array().unwrap().len(), 1);
        assert_eq!(data["people"][0]["id"], "urn:li:activity:2");
        assert_eq!(
            data["people"][0]["profile"],
            "https://www.linkedin.com/in/p2/"
        );
        assert!(!data["summary"].as_str().unwrap().contains('<'));
        assert_eq!(
            data["topics"],
            json!(["rails", format!("{}\u{2026}", "x".repeat(39))])
        );
    }

    #[test]
    fn offered_rows_are_not_offered_again() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        let (p, _) = plugin(d.path(), &w);
        let item = p.discover(&ctx()).unwrap().remove(0);
        p.after_item(&item, &empty_answer(), &ctx()).unwrap();
        let (p2, _) = plugin(d.path(), &w);
        let again = p2.discover(&ctx()).unwrap().remove(0);
        let ids = |v: &Value| -> BTreeSet<String> {
            v["feed"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_string())
                .collect()
        };
        assert!(ids(&again.payload).is_disjoint(&ids(&item.payload)));
    }

    #[test]
    fn nothing_new_means_no_item() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        let (p, _) = plugin(d.path(), &w);
        let item = p.discover(&ctx()).unwrap().remove(0);
        p.after_item(&item, &empty_answer(), &ctx()).unwrap();
        {
            let mut g = w.lock().unwrap();
            g.feed_rows = feed()[..30].to_vec();
            g.note_rows = notes()[..30].to_vec();
            g.msg_rows = msgs()[..20].to_vec();
        }
        let (p2, _) = plugin(d.path(), &w);
        assert!(p2.discover(&ctx()).unwrap().is_empty());
    }

    fn model_round_trip(p: &LinkedInAutopilot, item: &WorkItem) -> Outcome {
        let c = ctx();
        let prompt = p.build_prompt(item, &c).unwrap();
        let reply = p.call_model(&prompt, &c).unwrap();
        assert_eq!(reply.cost_usd, Some(0.0));
        let answer = loopkit::parse_answer(Some(&reply.text), p.output_keys()).unwrap();
        assert_eq!(answer["summary"], "");
        p.after_item(item, &answer, &c).unwrap()
    }

    #[test]
    fn empty_feed_means_page_changed() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        w.lock().unwrap().feed_rows = vec![];
        let (p, _) = plugin(d.path(), &w);
        let item = p.discover(&ctx()).unwrap().remove(0);
        assert_eq!(item.payload, json!({"problem": "page_changed"}));
        let out = model_round_trip(&p, &item);
        assert_eq!(out.status, Status::Failed);
        assert!(out.summary.contains("layout was not recognised"));
    }

    #[test]
    fn not_connected_is_a_problem_and_never_browses() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        let (p, _) = plugin(d.path(), &w);
        let p = p.with_connected_fn(Box::new(|_| false));
        let item = p.discover(&ctx()).unwrap().remove(0);
        assert_eq!(item.payload, json!({"problem": "not_connected"}));
        assert!(w.lock().unwrap().sessions.is_empty());
        assert!(p
            .after_item(&item, &json!({}), &ctx())
            .unwrap()
            .summary
            .contains("not connected"));
    }

    #[test]
    fn expired_or_challenged_session_is_disconnected() {
        for error in [BrowserError::SessionExpired, BrowserError::Challenge] {
            let d = tempfile::tempdir().unwrap();
            let w = world();
            w.lock().unwrap().error = Some(error);
            let (p, fx) = plugin(d.path(), &w);
            p.discover(&ctx()).unwrap();
            assert_eq!(*fx.disconnects.lock().unwrap(), ["li"]);
        }
    }

    #[test]
    fn disconnect_failure_never_raises() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        w.lock().unwrap().error = Some(BrowserError::Challenge);
        let (p, _) = plugin(d.path(), &w);
        let p = p.with_disconnect_fn(Box::new(|_| Err("x".into())));
        let items = p.discover(&ctx()).unwrap();
        assert_eq!(items[0].payload, json!({"problem": "challenge"}));
    }

    #[test]
    fn next_run_after_disconnect_does_not_browse() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        w.lock().unwrap().error = Some(BrowserError::Challenge);
        let on = Arc::new(Mutex::new(true));
        let (on1, on2) = (on.clone(), on.clone());
        let (p, _) = plugin(d.path(), &w);
        let p = p
            .with_connected_fn(Box::new(move |_| *on1.lock().unwrap()))
            .with_disconnect_fn(Box::new(move |_| {
                *on2.lock().unwrap() = false;
                Ok(())
            }));
        p.discover(&ctx()).unwrap();
        w.lock().unwrap().sessions.clear();
        let items = p.discover(&ctx()).unwrap();
        assert_eq!(items[0].payload, json!({"problem": "not_connected"}));
        assert!(w.lock().unwrap().sessions.is_empty());
    }

    #[test]
    fn repeat_same_day_run_is_skipped_unless_forced() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        let mut loop_seen = store(&d.path().join("ls"), LOOP_NAME);
        loop_seen.add("linkedin:li:2026-10-06");
        let (p, _) = plugin(d.path(), &w);
        let p = p.with_loop_seen(loop_seen);
        let c = ctx_with(json!({"active_hours": "9-21"}), false);
        assert!(p.discover(&c).unwrap().is_empty());
        assert!(w.lock().unwrap().sessions.is_empty());
        assert_eq!(p.discover(&ctx()).unwrap().len(), 1);
    }

    #[test]
    fn schedule_hour_outside_active_hours_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        let (p, _) = plugin(d.path(), &w);
        let p = p.with_schedule_hour_fn(Box::new(|| Some(7)));
        let c = ctx_with(json!({"active_hours": "9-21"}), false);
        let item = p.discover(&c).unwrap().remove(0);
        assert_eq!(
            (item.key.as_str(), item.title.as_str()),
            ("linkedin:schedule:2026-10-06", "schedule")
        );
        assert_eq!(item.payload, json!({"problem": "schedule_outside_hours"}));
        assert!(w.lock().unwrap().sessions.is_empty());
        let out = p.after_item(&item, &json!({}), &ctx()).unwrap();
        assert_eq!(out.status, Status::Failed);
        assert!(out
            .summary
            .contains("schedule hour is outside its active hours"));
        let d2 = tempfile::tempdir().unwrap();
        let (p2, _) = plugin(d2.path(), &world());
        let p2 = p2.with_schedule_hour_fn(Box::new(|| Some(7)));
        let forced = p2
            .discover(&ctx_with(json!({"active_hours": "9-21"}), true))
            .unwrap();
        assert!(forced[0].payload.get("problem").is_none());
    }

    #[test]
    fn manual_run_at_night_with_schedule_inside_window_is_silent() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        let (p, _) = plugin(d.path(), &w);
        let p = p.with_schedule_hour_fn(Box::new(|| Some(10)));
        let night = Utc.with_ymd_and_hms(2026, 10, 6, 3, 0, 0).unwrap();
        let c = ctx_at(night, json!({"active_hours": "9-21"}), false);
        assert!(p.discover(&c).unwrap().is_empty());
        assert!(w.lock().unwrap().sessions.is_empty());
    }

    #[test]
    fn problems_become_failed_outcomes_without_calling_the_model() {
        let cases = [
            (BrowserError::SessionExpired, "session_expired"),
            (BrowserError::Challenge, "challenge"),
            (BrowserError::Unavailable("x".into()), "browser_unavailable"),
            (
                BrowserError::Other {
                    class: "RuntimeError".into(),
                    detail: "secret token".into(),
                },
                "error",
            ),
        ];
        for (error, problem) in cases {
            let d = tempfile::tempdir().unwrap();
            let w = world();
            w.lock().unwrap().error = Some(error);
            let (p, _) = plugin(d.path(), &w);
            let item = p.discover(&ctx()).unwrap().remove(0);
            assert_eq!(item.payload, json!({"problem": problem}));
            let out = model_round_trip(&p, &item);
            assert_eq!(out.status, Status::Failed);
            assert_eq!(out.data["problem"], problem);
            assert!(!out.summary.contains("secret token"));
        }
    }

    #[test]
    fn challenge_stops_before_reading_other_pages() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        w.lock().unwrap().error = Some(BrowserError::Challenge);
        plugin(d.path(), &w).0.discover(&ctx()).unwrap();
        assert_eq!(w.lock().unwrap().sessions[0].2, ["feed"]);
    }

    #[test]
    fn outside_active_hours_or_skip_day_discovers_nothing_unless_forced() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        let (p, _) = plugin(d.path(), &w);
        assert!(p
            .discover(&ctx_with(json!({"active_hours": "13-18"}), false))
            .unwrap()
            .is_empty());
        assert!(w.lock().unwrap().sessions.is_empty());
        let skipper = Arc::new(FakeHuman {
            sleeps: Mutex::new(vec![]),
            skip: true,
            delay: 1.0,
            seed: 0,
        });
        let d2 = tempfile::tempdir().unwrap();
        let (p2, _) = plugin_h(d2.path(), &w, skipper.clone());
        assert!(p2
            .discover(&ctx_with(json!({"active_hours": "9-21"}), false))
            .unwrap()
            .is_empty());
        let d3 = tempfile::tempdir().unwrap();
        let (p3, _) = plugin_h(d3.path(), &w, skipper);
        assert_eq!(p3.discover(&ctx()).unwrap().len(), 1);
    }

    #[test]
    fn random_start_delay_is_slept_unless_forced() {
        let human = FakeHuman::new(0);
        let d = tempfile::tempdir().unwrap();
        let (p, _) = plugin_h(d.path(), &world(), human.clone());
        p.discover(&ctx_with(json!({"active_hours": "9-21"}), false))
            .unwrap();
        assert!(human.sleeps.lock().unwrap().iter().any(|s| *s > 0.0));
        human.sleeps.lock().unwrap().clear();
        let d2 = tempfile::tempdir().unwrap();
        let (p2, _) = plugin_h(d2.path(), &world(), human.clone());
        p2.discover(&ctx()).unwrap();
        assert!(!human.sleeps.lock().unwrap().iter().any(|s| *s > 60.0));
    }

    #[test]
    fn digest_lists_summary_links_and_problems() {
        let d = tempfile::tempdir().unwrap();
        let (p, _) = plugin(d.path(), &world());
        let item = p.discover(&ctx()).unwrap().remove(0);
        let answer = json!({"summary": "Quiet day", "topics": ["rails"],
            "worth_reading": [{"id": "urn:li:activity:2", "why": "useful"}],
            "people": [{"id": "urn:li:activity:2", "why": "ships Rails"}]});
        let out = p.after_item(&item, &answer, &ctx()).unwrap();
        let text = p.digest(&[out], &ctx()).unwrap();
        assert!(text.contains("LinkedIn Autopilot - 2026-10-06") && text.contains("Quiet day"));
        assert!(text.contains("30 notifications, 20 messages, 30 feed posts"));
        assert!(
            text.contains("https://www.linkedin.com/feed/update/urn:li:activity:2/")
                && text.contains("useful")
        );
        let failed = Outcome::new("k", Status::Failed, PROBLEMS[0].1)
            .with_data(json!({"problem": "session_expired"}));
        assert!(p
            .digest(&[failed], &ctx())
            .unwrap()
            .to_lowercase()
            .contains("needs attention"));
        assert!(p.digest(&[], &ctx()).is_none());
    }

    #[test]
    fn definition_and_prompt_load() {
        let d =
            LoopDefinition::from_yaml(&repo().join("loops/linkedin-autopilot/loop.yaml")).unwrap();
        assert_eq!(d.name, "linkedin-autopilot-loop");
        let prompt =
            std::fs::read_to_string(repo().join("loops/linkedin-autopilot/prompt.md")).unwrap();
        assert!(
            prompt.contains("{{item_json}}")
                && prompt.contains("{{settings_json}}")
                && prompt.contains("untrusted")
        );
        assert!(prompt.contains("worth_reading") && prompt.contains("people"));
    }

    #[test]
    fn settings_fields_are_declared() {
        let keys: Vec<_> = SETTINGS_FIELDS.iter().map(|f| f.key).collect();
        assert_eq!(
            keys,
            [
                "headless",
                "active_hours",
                "people_query",
                "people_target",
                "topics"
            ]
        );
        assert_eq!(SETTINGS_FIELDS[0].default, "no");
        assert_eq!(SETTINGS_FIELDS[0].options, &["no", "yes"]);
    }

    #[test]
    fn wrong_typed_ids_are_ignored() {
        let d = tempfile::tempdir().unwrap();
        let (p, _) = plugin(d.path(), &world());
        let item = p.discover(&ctx()).unwrap().remove(0);
        let bad = [
            json!(["x"]),
            json!({"a": 1}),
            json!(3),
            Value::Null,
            json!(true),
        ];
        let list: Vec<Value> = bad.iter().map(|i| json!({"id": i, "why": "w"})).collect();
        let answer = json!({"summary": "s", "topics": [], "worth_reading": list, "people": list});
        let out = p.after_item(&item, &answer, &ctx()).unwrap();
        assert_eq!(out.status, Status::Done);
        assert_eq!(out.data["worth_reading"], json!([]));
        assert_eq!(out.data["people"], json!([]));
    }

    #[test]
    fn wrong_typed_lists_become_empty() {
        for bad in [json!(5), json!(true), json!("rails"), json!({"a": 1})] {
            let d = tempfile::tempdir().unwrap();
            let (p, _) = plugin(d.path(), &world());
            let item = p.discover(&ctx()).unwrap().remove(0);
            let answer =
                json!({"summary": "s", "topics": bad, "worth_reading": bad, "people": bad});
            let out = p.after_item(&item, &answer, &ctx()).unwrap();
            assert_eq!(out.status, Status::Done);
            assert_eq!(out.data["topics"], json!([]));
            assert_eq!(out.data["worth_reading"], json!([]));
            assert_eq!(out.data["people"], json!([]));
        }
    }

    #[test]
    fn rows_are_marked_seen_before_the_answer_is_validated() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        let (p, _) = plugin(d.path(), &w);
        let item = p.discover(&ctx()).unwrap().remove(0);
        // A garbage answer still marks every offered row seen.
        p.after_item(
            &item,
            &json!({"summary": 5, "topics": 1, "worth_reading": "x", "people": null}),
            &ctx(),
        )
        .unwrap();
        let again = plugin(d.path(), &w).0.discover(&ctx()).unwrap().remove(0);
        let ids = |v: &Value| -> BTreeSet<String> {
            v["feed"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_string())
                .collect()
        };
        assert!(ids(&again.payload).is_disjoint(&ids(&item.payload)));
    }

    #[test]
    fn digest_reports_a_crashed_run_without_problem() {
        let d = tempfile::tempdir().unwrap();
        let (p, _) = plugin(d.path(), &world());
        let crashed = Outcome::new("k", Status::Failed, "TypeError: secret");
        let text = p.digest(&[crashed], &ctx()).unwrap();
        assert!(text.contains("LinkedIn needs attention: the run failed - check the loop log"));
        assert!(!text.contains("secret"));
    }

    fn person(i: usize) -> Value {
        json!({"name": format!("P{i}"), "url": format!("https://www.linkedin.com/in/p{i}"), "avatar": "",
               "company": "Acme", "position": "Eng"})
    }

    #[test]
    fn people_are_searched_for_the_configured_query_and_saved() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        w.lock().unwrap().people_rows = (0..25).map(person).collect();
        let (p, _) = plugin(d.path(), &w);
        p.discover(&ctx_with(
            json!({"people_query": "AI builder", "people_target": "25"}),
            true,
        ))
        .unwrap();
        assert_eq!(
            w.lock().unwrap().sessions[0].3,
            Some(("AI builder".to_string(), 25, BTreeSet::new()))
        );
        assert_eq!(people::load(&d.path().join("people.json")).len(), 25);
    }

    #[test]
    fn people_search_defaults_to_ai_builder_and_30_and_skips_known_people() {
        let d = tempfile::tempdir().unwrap();
        people::merge(
            &[person(1)],
            "AI builder",
            now(),
            &d.path().join("people.json"),
        )
        .unwrap();
        let w = world();
        plugin(d.path(), &w).0.discover(&ctx()).unwrap();
        let known: BTreeSet<String> = [person(1)["url"].as_str().unwrap().to_string()].into();
        assert_eq!(
            w.lock().unwrap().sessions[0].3,
            Some(("AI builder".to_string(), 30, known))
        );
    }

    #[test]
    fn people_search_can_be_turned_off_with_an_empty_query() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        plugin(d.path(), &w)
            .0
            .discover(&ctx_with(json!({"people_query": ""}), true))
            .unwrap();
        assert!(!w.lock().unwrap().sessions[0]
            .2
            .contains(&"people".to_string()));
    }

    #[test]
    fn a_people_search_timeout_does_not_lose_the_feed_run() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        w.lock().unwrap().people_error = Some(BrowserError::PageTimeout);
        let items = plugin(d.path(), &w).0.discover(&ctx()).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].payload.get("problem").is_none());
    }

    #[test]
    fn a_feed_that_never_renders_is_a_page_timeout_problem() {
        let d = tempfile::tempdir().unwrap();
        let w = world();
        w.lock().unwrap().error = Some(BrowserError::PageTimeout);
        let (p, _) = plugin(d.path(), &w);
        let item = p.discover(&ctx()).unwrap().remove(0);
        let out = p.after_item(&item, &json!({}), &ctx()).unwrap();
        assert_eq!(out.status, Status::Failed);
        assert_eq!(out.data["problem"], "page_timeout");
        assert!(out.summary.contains("10 minutes"));
    }

    #[test]
    fn people_store_helpers() {
        assert_eq!(people::target(Some(&json!("25"))), 25);
        assert_eq!(people::target(Some(&json!(5))), 20);
        assert_eq!(people::target(Some(&json!("99"))), 50);
        assert_eq!(people::target(Some(&json!("x"))), 30);
        assert_eq!(people::target(None), 30);
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("p.json");
        assert_eq!(
            people::merge(
                &[person(1), person(2), json!({"name": "no url"})],
                "q",
                now(),
                &path
            )
            .unwrap(),
            2
        );
        let stored = people::load(&path);
        assert_eq!(stored[0]["name"], "P2");
        assert_eq!(stored[1]["first_seen"], "2026-10-06T10:00:00+00:00");
        // a known person is refreshed, not duplicated
        let mut again = person(1);
        again["company"] = json!("NewCo");
        assert_eq!(people::merge(&[again], "q2", now(), &path).unwrap(), 0);
        let stored = people::load(&path);
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[1]["company"], "NewCo");
        assert_eq!(stored[1]["query"], "q");
        assert!(std::fs::read_to_string(&path).unwrap().contains("\n {"));
    }

    #[test]
    fn active_hours_parsing() {
        assert_eq!(parse_active_hours(Some(&json!("9-21"))), (9, 21));
        assert_eq!(parse_active_hours(Some(&json!(" 8 - 18 "))), (8, 18));
        assert_eq!(parse_active_hours(Some(&json!("21-9"))), (9, 21));
        assert_eq!(parse_active_hours(Some(&json!("0-25"))), (9, 21));
        assert_eq!(parse_active_hours(Some(&json!("junk"))), (9, 21));
        assert_eq!(parse_active_hours(None), (9, 21));
        assert!(within_hours(9, (9, 21)) && !within_hours(21, (9, 21)));
    }

    /// One test owns the process-global factory (tests run in parallel).
    #[test]
    fn build_needs_a_registered_factory_and_no_factory_is_browser_unavailable() {
        *GLOBAL_FACTORY.lock().unwrap() = None;
        assert!(build(Arc::new(Source)).is_none());
        let d = tempfile::tempdir().unwrap();
        let (p, _) = plugin_h(d.path(), &world(), FakeHuman::new(0));
        let p = LinkedInAutopilot {
            session_factory: None,
            ..p
        };
        let items = p.discover(&ctx()).unwrap();
        assert_eq!(items[0].payload, json!({"problem": "browser_unavailable"}));
        let w = world();
        register_session_factory(factory(&w));
        assert!(build(Arc::new(Source)).is_some());
        *GLOBAL_FACTORY.lock().unwrap() = None;
    }
}
