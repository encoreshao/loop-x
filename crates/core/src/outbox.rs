//! Notifications that could not go out the moment they were sent. A sender tries
//! once; a message that failed in a way that can heal (Slack down, laptop offline,
//! rate limited), or a routine one sent during quiet hours, is written to
//! `outputs/outbox/` and the scheduler's minute tick ([`Outbox::tick`]) sends it
//! later. Held quiet-hours messages to one channel go out merged when the window
//! ends. A record names its target by account id and bundle, never by URL.

use crate::file_lock::FileLock;
use crate::loop_notify::{self, SendError};
use chrono::{DateTime, Duration, Local, NaiveDateTime, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Give up retrying a message this long after it was first sent.
pub const RETRY_FOR_HOURS: i64 = 24;
/// Failed records are shown this long, then removed.
pub const KEEP_FAILED_DAYS: i64 = 7;
/// Held messages merged into one post at most.
pub const MERGE_AT_MOST: usize = 15;
/// Sends one tick may make, so a long backlog never stalls the scheduler.
pub const SENDS_PER_TICK: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    #[default]
    Routine,
    /// Someone has to act: goes out even during quiet hours.
    Urgent,
}

/// Where a message goes: a connector account, or [`loop_notify::DEFAULT_ID`] for
/// the default Slack webhook (or the webhook of `bundle`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Target {
    pub account: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
}

impl Target {
    pub fn slack(bundle: Option<&str>) -> Self {
        Self {
            account: loop_notify::DEFAULT_ID.to_string(),
            bundle: bundle.filter(|b| !b.is_empty()).map(str::to_string),
        }
    }

    pub fn connector(account: &str) -> Self {
        Self {
            account: account.to_string(),
            bundle: None,
        }
    }
}

/// One Slack-shaped rendering; a sender tries them in order until one is accepted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocks: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outgoing {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_name: Option<String>,
    pub target: Target,
    pub attempts: Vec<Attempt>,
    /// The text for connectors that show no Slack layout, and for merging.
    pub plain: String,
    #[serde(default)]
    pub urgency: Urgency,
}

impl Outgoing {
    /// A text message to the default Slack webhook (or `bundle`'s), with `blocks`
    /// tried first and the text alone after a rejection.
    pub fn slack_text(text: &str, blocks: Option<Value>, bundle: Option<&str>) -> Self {
        let mut attempts = Vec::new();
        if blocks.is_some() {
            attempts.push(Attempt {
                text: text.to_string(),
                blocks,
            });
        }
        attempts.push(Attempt {
            text: text.to_string(),
            blocks: None,
        });
        Self {
            loop_name: None,
            target: Target::slack(bundle),
            attempts,
            plain: text.to_string(),
            urgency: Urgency::Routine,
        }
    }

    /// A run report in `template`, falling back to plainer layouts on rejection.
    pub fn report(
        report: &crate::slack_report::Report,
        template: crate::slack_report::Template,
        target: Target,
    ) -> Self {
        Self {
            loop_name: None,
            target,
            attempts: crate::slack_report::attempts(report, template)
                .into_iter()
                .map(|r| Attempt {
                    text: r.text,
                    blocks: r.blocks,
                })
                .collect(),
            plain: crate::slack_report::plain_text(report),
            urgency: Urgency::Routine,
        }
    }

    pub fn urgent(self) -> Self {
        Self {
            urgency: Urgency::Urgent,
            ..self
        }
    }

    pub fn for_loop(self, name: &str) -> Self {
        Self {
            loop_name: Some(name.to_string()),
            ..self
        }
    }
}

/// Sends a message's attempts (or its plain text) to its target.
pub type Transport<'a> = &'a dyn Fn(&Target, &[Attempt], &str) -> Result<(), SendError>;

/// What became of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    Sent,
    /// Held for quiet hours.
    Held,
    /// Failed for now; the outbox retries it.
    Retrying(String),
    Failed(String),
}

impl Delivery {
    /// Sent, or certain to be tried again: nothing for the sender to report.
    pub fn handled(&self) -> bool {
        !matches!(self, Delivery::Failed(_))
    }

    /// `(id, ok, message)` for a loop's notify results.
    pub fn result(&self, id: &str) -> loop_notify::NotifyResult {
        let (ok, message) = match self {
            Delivery::Sent => (true, "sent".to_string()),
            Delivery::Held => (true, "held for quiet hours".to_string()),
            Delivery::Retrying(e) => (true, format!("will retry ({e})")),
            Delivery::Failed(e) => (false, e.clone()),
        };
        (id.to_string(), ok, message)
    }
}

/// Whether a failure can heal by itself, and a retry cannot post twice: a status
/// that means "try later", or a connection that never reached the server. A
/// timeout is not retried: the message may have arrived.
pub fn retryable(e: &SendError) -> bool {
    match e {
        SendError::Http(s) => *s == 429 || (500..600).contains(s),
        SendError::Other(class) => class == UNREACHABLE || class == "MailHTTPError",
    }
}

const UNREACHABLE: &str = "Unreachable";

/// A Slack webhook post's error (`slack::post_message`) as a [`SendError`].
pub fn webhook_error(e: &str) -> SendError {
    if let Some(code) = e
        .strip_prefix("HTTP Error ")
        .and_then(|c| c.trim().parse().ok())
    {
        return SendError::Http(code);
    }
    match e {
        "timeout" => SendError::Other("Timeout".into()),
        e if e.starts_with("connection failed") => SendError::Other(UNREACHABLE.into()),
        _ => SendError::Other(crate::connectors_config::slack_error_class(e)),
    }
}

/// Try `attempts` in order through `send`: a status answer means that layout was
/// refused, so the next one is tried; any other failure ends it.
pub fn send_attempts(
    attempts: &[Attempt],
    send: loop_notify::DefaultSender,
) -> Result<(), SendError> {
    let mut last = Err(SendError::Other("nothing to send".into()));
    for a in attempts {
        last = send(&a.text, a.blocks.as_ref());
        match &last {
            Ok(()) => return Ok(()),
            Err(SendError::Http(_)) => continue,
            Err(_) => break,
        }
    }
    last
}

/// Posts to a webhook URL (`slack::post_message`'s shape).
pub type WebhookPost<'a> = dyn Fn(&str, &str, Option<&Value>) -> Result<(), String> + 'a;

/// A transport for the default Slack webhook (or a bundle's) in `config`, posting
/// with `post`. Any other target is refused: only [`Target::slack`] is a webhook.
pub fn webhook_transport<'a>(
    config: &'a Path,
    post: &'a WebhookPost<'a>,
) -> impl Fn(&Target, &[Attempt], &str) -> Result<(), SendError> + 'a {
    move |target, attempts, _plain| {
        if target.account != loop_notify::DEFAULT_ID {
            return Err(SendError::Other("unknown connector".into()));
        }
        let url = crate::slack::load_webhook_url(config, target.bundle.as_deref())
            .map_err(|m| SendError::Other(crate::connectors_config::slack_error_class(&m)))?;
        send_attempts(attempts, &|text, blocks| {
            post(&url, text, blocks).map_err(|e| webhook_error(&e))
        })
    }
}

/// Send `msg` now through `transport`, with `outbox` (when given) taking over
/// what cannot go out now. Without one a failure is simply reported.
pub fn send(outbox: Option<&Outbox>, msg: Outgoing, transport: Transport) -> Delivery {
    match outbox {
        Some(o) => o.deliver(msg, transport),
        None => match transport(&msg.target, &msg.attempts, &msg.plain) {
            Ok(()) => Delivery::Sent,
            Err(e) => Delivery::Failed(describe(&e)),
        },
    }
}

fn describe(e: &SendError) -> String {
    match e {
        SendError::Http(s) => format!("HTTP {s}"),
        SendError::Other(c) => c.clone(),
    }
}

// ---- quiet hours -----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietHours {
    pub start: NaiveTime,
    pub end: NaiveTime,
}

impl QuietHours {
    /// `"22:00"`, `"08:00"`; `None` for a malformed time or an empty window.
    pub fn parse(start: &str, end: &str) -> Option<Self> {
        let t = |s: &str| NaiveTime::parse_from_str(s.trim(), "%H:%M").ok();
        let q = Self {
            start: t(start)?,
            end: t(end)?,
        };
        (q.start != q.end).then_some(q)
    }

    pub fn contains(&self, t: NaiveTime) -> bool {
        if self.start < self.end {
            self.start <= t && t < self.end
        } else {
            t >= self.start || t < self.end
        }
    }

    /// The first end of the window after `local`.
    pub fn next_end(&self, local: NaiveDateTime) -> NaiveDateTime {
        let today = local.date().and_time(self.end);
        if today > local {
            today
        } else {
            today + Duration::days(1)
        }
    }
}

/// `<config home>/notifications.json`.
pub fn settings_path(home: &Path) -> PathBuf {
    home.join("notifications.json")
}

/// The saved quiet hours; `None` when off, missing or unreadable.
pub fn load_quiet_hours(path: &Path) -> Option<QuietHours> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let q = v.get("quiet_hours")?;
    QuietHours::parse(q.get("start")?.as_str()?, q.get("end")?.as_str()?)
}

/// Save (or, with `None`, switch off) the quiet hours, keeping other keys.
pub fn save_quiet_hours(path: &Path, quiet: Option<QuietHours>) -> std::io::Result<()> {
    let mut v = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    match quiet {
        Some(q) => {
            v["quiet_hours"] = serde_json::json!({
                "start": q.start.format("%H:%M").to_string(),
                "end": q.end.format("%H:%M").to_string(),
            })
        }
        None => {
            v.as_object_mut().map(|m| m.remove("quiet_hours"));
        }
    }
    crate::settings_store::atomic_write_json(&v, path)
}

// ---- the outbox --------------------------------------------------------------------

/// The moment, in UTC and on the wall clock (quiet hours are wall-clock times).
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    pub utc: DateTime<Utc>,
    pub local: NaiveDateTime,
}

impl Clock {
    pub fn now() -> Self {
        let utc = Utc::now();
        Self {
            utc,
            local: utc.with_timezone(&Local).naive_local(),
        }
    }

    fn quiet(&self, q: Option<QuietHours>) -> bool {
        q.is_some_and(|q| q.contains(self.local.time()))
    }

    fn quiet_end(&self, q: QuietHours) -> DateTime<Utc> {
        self.utc + (q.next_end(self.local) - self.local)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Pending,
    Held,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub state: State,
    pub tries: u32,
    pub next_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub message: Outgoing,
}

/// After `tries` failed sends, how long until the next.
pub fn backoff(tries: u32) -> Duration {
    match tries {
        0 | 1 => Duration::minutes(1),
        2 => Duration::minutes(5),
        3 => Duration::minutes(15),
        4 => Duration::minutes(60),
        _ => Duration::hours(3),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outbox {
    pub dir: PathBuf,
    pub quiet: Option<QuietHours>,
}

/// What a tick did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tick {
    pub sent: usize,
    pub retrying: usize,
    pub failed: usize,
}

/// What is waiting, for the Settings page.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Summary {
    pub retrying: usize,
    pub held: usize,
    pub failed: Vec<Record>,
}

impl Outbox {
    /// The outbox of the checkout at `repo_root`, with the saved quiet hours.
    pub fn at(repo_root: &Path) -> Self {
        Self {
            dir: repo_root.join("outputs").join("outbox"),
            quiet: load_quiet_hours(&settings_path(&crate::config::home_dir())),
        }
    }

    pub fn real() -> Self {
        Self::at(&crate::runner_util::repo_root())
    }

    pub fn deliver(&self, msg: Outgoing, transport: Transport) -> Delivery {
        self.deliver_at(msg, transport, Clock::now())
    }

    pub fn deliver_at(&self, msg: Outgoing, transport: Transport, now: Clock) -> Delivery {
        if msg.urgency == Urgency::Routine && now.quiet(self.quiet) {
            let next_at = now.quiet_end(self.quiet.expect("quiet implies hours"));
            return match self.add(msg, State::Held, 0, next_at, None, now) {
                Ok(()) => Delivery::Held,
                Err(e) => Delivery::Failed(format!("could not hold: {e}")),
            };
        }
        match transport(&msg.target, &msg.attempts, &msg.plain) {
            Ok(()) => Delivery::Sent,
            Err(e) if retryable(&e) => {
                let why = describe(&e);
                match self.add(
                    msg,
                    State::Pending,
                    1,
                    now.utc + backoff(1),
                    Some(why.clone()),
                    now,
                ) {
                    Ok(()) => Delivery::Retrying(why),
                    Err(_) => Delivery::Failed(why),
                }
            }
            Err(e) => Delivery::Failed(describe(&e)),
        }
    }

    fn add(
        &self,
        message: Outgoing,
        state: State,
        tries: u32,
        next_at: DateTime<Utc>,
        last_error: Option<String>,
        now: Clock,
    ) -> std::io::Result<()> {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 1_000_000;
        let id = format!(
            "{}-{seq:06}-{}",
            now.utc.format("%Y%m%dT%H%M%S%.6fZ"),
            &crate::events::new_uuid_hex()[..8]
        );
        let record = Record {
            id,
            created_at: now.utc,
            state,
            tries,
            next_at,
            last_error,
            message,
        };
        let _lock = self.lock()?;
        self.write(&record)
    }

    fn lock(&self) -> std::io::Result<FileLock> {
        FileLock::exclusive(&self.dir.join(".lock"))
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn write(&self, r: &Record) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(r).map_err(std::io::Error::other)?;
        crate::settings_store::atomic_write(&self.path(&r.id), &bytes, Some(0o600))
    }

    fn remove(&self, id: &str) {
        let _ = std::fs::remove_file(self.path(id));
    }

    /// Every record, oldest first.
    pub fn records(&self) -> Vec<Record> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<Record> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| serde_json::from_slice(&std::fs::read(p).ok()?).ok())
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    pub fn summary(&self) -> Summary {
        let mut s = Summary::default();
        for r in self.records() {
            match r.state {
                State::Pending => s.retrying += 1,
                State::Held => s.held += 1,
                State::Failed => s.failed.push(r),
            }
        }
        s
    }

    /// Remove the failed records the Settings page shows.
    pub fn clear_failed(&self) {
        let Ok(_lock) = self.lock() else { return };
        for r in self.records() {
            if r.state == State::Failed {
                self.remove(&r.id);
            }
        }
    }

    pub fn tick(&self, transport: Transport) -> Tick {
        self.tick_at(transport, Clock::now())
    }

    /// Send what is due: retries one by one, and held messages, merged per
    /// target, once quiet hours are over. Routine retries that come due during
    /// quiet hours are held instead.
    pub fn tick_at(&self, transport: Transport, now: Clock) -> Tick {
        let mut tick = Tick::default();
        let Ok(_lock) = self.lock() else {
            return tick;
        };
        let quiet = now.quiet(self.quiet);
        let mut sends = 0;
        let mut held: Vec<Record> = Vec::new();
        for mut r in self.records() {
            match r.state {
                State::Failed => {
                    if now.utc - r.created_at > Duration::days(KEEP_FAILED_DAYS) {
                        self.remove(&r.id);
                    }
                }
                _ if r.next_at > now.utc => {}
                State::Held if !quiet => held.push(r),
                State::Held => {}
                State::Pending if quiet && r.message.urgency == Urgency::Routine => {
                    r.state = State::Held;
                    r.next_at = now.quiet_end(self.quiet.expect("quiet implies hours"));
                    let _ = self.write(&r);
                }
                State::Pending => {
                    if sends >= SENDS_PER_TICK {
                        continue;
                    }
                    sends += 1;
                    let m = &r.message;
                    let sent = transport(&m.target, &m.attempts, &m.plain);
                    self.settle(vec![r], sent, now, &mut tick);
                }
            }
        }
        let mut groups: Vec<(Target, Vec<Record>)> = Vec::new();
        for r in held {
            match groups.iter_mut().find(|(t, _)| *t == r.message.target) {
                Some((_, g)) => g.push(r),
                None => groups.push((r.message.target.clone(), vec![r])),
            }
        }
        for (target, records) in groups {
            for chunk in records.chunks(MERGE_AT_MOST) {
                if sends >= SENDS_PER_TICK {
                    break;
                }
                sends += 1;
                let sent = match chunk {
                    [one] => {
                        let m = &one.message;
                        transport(&m.target, &m.attempts, &m.plain)
                    }
                    many => {
                        let text = merged_text(many);
                        let attempt = Attempt {
                            text: text.clone(),
                            blocks: None,
                        };
                        transport(&target, std::slice::from_ref(&attempt), &text)
                    }
                };
                self.settle(chunk.to_vec(), sent, now, &mut tick);
            }
        }
        tick
    }

    /// Record the outcome of one send that carried `records`.
    fn settle(
        &self,
        records: Vec<Record>,
        sent: Result<(), SendError>,
        now: Clock,
        tick: &mut Tick,
    ) {
        for mut r in records {
            match &sent {
                Ok(()) => {
                    self.remove(&r.id);
                    tick.sent += 1;
                }
                Err(e) => {
                    r.tries += 1;
                    r.last_error = Some(describe(e));
                    let expired = now.utc - r.created_at >= Duration::hours(RETRY_FOR_HOURS);
                    if retryable(e) && !expired {
                        r.state = State::Pending;
                        r.next_at = now.utc + backoff(r.tries);
                        tick.retrying += 1;
                    } else {
                        r.state = State::Failed;
                        tick.failed += 1;
                    }
                    let _ = self.write(&r);
                }
            }
        }
    }
}

fn merged_text(records: &[Record]) -> String {
    let parts: Vec<&str> = records.iter().map(|r| r.message.plain.as_str()).collect();
    format!(
        "*While you were away* ({} updates)\n\n{}",
        parts.len(),
        parts.join("\n\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn at(hhmm: &str) -> Clock {
        let local =
            NaiveDateTime::parse_from_str(&format!("2026-11-27 {hhmm}"), "%Y-%m-%d %H:%M").unwrap();
        Clock {
            utc: local.and_utc(),
            local,
        }
    }

    fn later(c: Clock, minutes: i64) -> Clock {
        Clock {
            utc: c.utc + Duration::minutes(minutes),
            local: c.local + Duration::minutes(minutes),
        }
    }

    fn outbox(quiet: Option<(&str, &str)>) -> (tempfile::TempDir, Outbox) {
        let d = tempfile::tempdir().unwrap();
        let o = Outbox {
            dir: d.path().join("outbox"),
            quiet: quiet.and_then(|(s, e)| QuietHours::parse(s, e)),
        };
        (d, o)
    }

    fn msg(text: &str) -> Outgoing {
        Outgoing::slack_text(text, None, None)
    }

    /// A transport answering from a script of results, recording what it was sent.
    struct Wire {
        answers: RefCell<Vec<Result<(), SendError>>>,
        sent: RefCell<Vec<String>>,
    }

    impl Wire {
        fn new(answers: Vec<Result<(), SendError>>) -> Self {
            Self {
                answers: RefCell::new(answers),
                sent: RefCell::new(Vec::new()),
            }
        }

        fn send(&self, _t: &Target, attempts: &[Attempt], _plain: &str) -> Result<(), SendError> {
            self.sent.borrow_mut().push(attempts[0].text.clone());
            let mut a = self.answers.borrow_mut();
            if a.is_empty() {
                Ok(())
            } else {
                a.remove(0)
            }
        }
    }

    fn down() -> Result<(), SendError> {
        Err(SendError::Other(UNREACHABLE.into()))
    }

    #[test]
    fn a_message_that_goes_out_leaves_nothing_behind() {
        let (_d, o) = outbox(None);
        let wire = Wire::new(vec![]);
        let d = o.deliver_at(msg("hi"), &|t, a, p| wire.send(t, a, p), at("12:00"));
        assert_eq!(d, Delivery::Sent);
        assert!(o.records().is_empty());
    }

    #[test]
    fn an_unreachable_slack_is_retried_with_backoff_until_it_answers() {
        let (_d, o) = outbox(None);
        let wire = Wire::new(vec![down(), down(), Ok(())]);
        let send = |t: &Target, a: &[Attempt], p: &str| wire.send(t, a, p);
        let t0 = at("12:00");
        let d = o.deliver_at(msg("hi"), &send, t0);
        assert_eq!(d, Delivery::Retrying("Unreachable".into()));
        assert!(d.handled());

        assert_eq!(o.tick_at(&send, t0), Tick::default(), "not due yet");
        assert_eq!(o.tick_at(&send, later(t0, 1)).retrying, 1);
        assert_eq!(
            o.records()[0].next_at,
            later(t0, 1).utc + Duration::minutes(5)
        );
        assert_eq!(o.tick_at(&send, later(t0, 6)).sent, 1);
        assert!(o.records().is_empty());
        assert_eq!(wire.sent.borrow().len(), 3);
    }

    #[test]
    fn what_cannot_heal_or_might_have_arrived_is_not_retried() {
        let (_d, o) = outbox(None);
        for e in [
            SendError::Http(404),
            SendError::Other("Timeout".into()),
            SendError::Other("KeyError".into()),
        ] {
            let d = o.deliver_at(msg("hi"), &|_, _, _| Err(e.clone()), at("12:00"));
            assert!(matches!(d, Delivery::Failed(_)), "{e:?}");
        }
        assert!(o.records().is_empty());
        assert!(retryable(&SendError::Http(429)) && retryable(&SendError::Http(503)));
    }

    #[test]
    fn retrying_stops_after_a_day_and_the_failure_is_kept_for_a_week() {
        let (_d, o) = outbox(None);
        let t0 = at("12:00");
        o.deliver_at(msg("hi"), &|_, _, _| down(), t0);
        let day = later(t0, 60 * 24);
        assert_eq!(o.tick_at(&|_, _, _| down(), day).failed, 1);
        let s = o.summary();
        assert_eq!((s.retrying, s.failed.len()), (0, 1));
        assert_eq!(s.failed[0].last_error.as_deref(), Some("Unreachable"));

        o.tick_at(&|_, _, _| Ok(()), later(day, 60 * 24 * 8));
        assert!(o.records().is_empty());
    }

    #[test]
    fn quiet_hours_hold_routine_messages_and_merge_them_when_the_window_ends() {
        let (_d, o) = outbox(Some(("22:00", "08:00")));
        let wire = Wire::new(vec![]);
        let send = |t: &Target, a: &[Attempt], p: &str| wire.send(t, a, p);
        let night = at("23:30");
        assert_eq!(o.deliver_at(msg("first"), &send, night), Delivery::Held);
        assert_eq!(o.deliver_at(msg("second"), &send, night), Delivery::Held);
        let other = Outgoing::slack_text("elsewhere", None, Some("team"));
        assert_eq!(o.deliver_at(other, &send, night), Delivery::Held);
        assert_eq!(
            o.deliver_at(msg("needs you").urgent(), &send, night),
            Delivery::Sent
        );
        assert_eq!(*wire.sent.borrow(), ["needs you"]);

        assert_eq!(o.tick_at(&send, at("07:59")).sent, 0);
        let morning = later(at("08:00"), 24 * 60);
        assert_eq!(o.tick_at(&send, morning).sent, 3);
        let sent = wire.sent.borrow();
        assert_eq!(
            sent[1],
            "*While you were away* (2 updates)\n\nfirst\n\nsecond"
        );
        assert_eq!(sent[2], "elsewhere");
        assert!(o.records().is_empty());
    }

    #[test]
    fn a_routine_retry_that_comes_due_at_night_waits_for_the_morning() {
        let (_d, o) = outbox(Some(("22:00", "08:00")));
        let t0 = at("21:59");
        o.deliver_at(msg("hi"), &|_, _, _| down(), t0);
        let wire = Wire::new(vec![]);
        let send = |t: &Target, a: &[Attempt], p: &str| wire.send(t, a, p);
        o.tick_at(&send, later(t0, 2));
        assert!(wire.sent.borrow().is_empty());
        assert_eq!(o.records()[0].state, State::Held);
    }

    #[test]
    fn quiet_hours_parse_save_and_span_midnight() {
        let q = QuietHours::parse("22:00", "08:00").unwrap();
        let t = |s| NaiveTime::parse_from_str(s, "%H:%M").unwrap();
        assert!(q.contains(t("23:00")) && q.contains(t("07:59")) && !q.contains(t("08:00")));
        let day = QuietHours::parse("12:00", "13:00").unwrap();
        assert!(day.contains(t("12:30")) && !day.contains(t("13:30")));
        assert_eq!(QuietHours::parse("09:00", "09:00"), None);
        assert_eq!(QuietHours::parse("25:00", "09:00"), None);

        let d = tempfile::tempdir().unwrap();
        let p = settings_path(d.path());
        assert_eq!(load_quiet_hours(&p), None);
        save_quiet_hours(&p, Some(q)).unwrap();
        assert_eq!(load_quiet_hours(&p), Some(q));
        save_quiet_hours(&p, None).unwrap();
        assert_eq!(load_quiet_hours(&p), None);
    }

    #[test]
    fn a_refused_layout_falls_back_but_a_dropped_connection_does_not() {
        let tried = RefCell::new(Vec::new());
        let attempts = Outgoing::slack_text("hi", Some(serde_json::json!([1])), None).attempts;
        let r = send_attempts(&attempts, &|_, b| {
            tried.borrow_mut().push(b.is_some());
            if b.is_some() {
                Err(SendError::Http(400))
            } else {
                Ok(())
            }
        });
        assert_eq!((r, tried.take()), (Ok(()), vec![true, false]));
        let r = send_attempts(&attempts, &|_, b| {
            tried.borrow_mut().push(b.is_some());
            down()
        });
        assert_eq!((r, tried.take()), (down(), vec![true]));
    }

    #[test]
    fn webhook_errors_are_classed_without_their_words() {
        assert_eq!(webhook_error("HTTP Error 503"), SendError::Http(503));
        assert_eq!(webhook_error("timeout"), SendError::Other("Timeout".into()));
        assert_eq!(
            webhook_error("connection failed (dns)"),
            SendError::Other(UNREACHABLE.into())
        );
    }

    #[test]
    fn a_record_names_its_target_and_never_a_url() {
        let (_d, o) = outbox(None);
        let cfg = o.dir.parent().unwrap().join("slack.json");
        std::fs::write(
            &cfg,
            r#"{"webhook_url": "https://hooks.slack.com/services/T/B/SECRET"}"#,
        )
        .unwrap();
        let post = |_: &str, _: &str, _: Option<&Value>| Err("connection failed".to_string());
        let transport = webhook_transport(&cfg, &post);
        let d = o.deliver_at(msg("hi"), &transport, at("12:00"));
        assert_eq!(d, Delivery::Retrying("Unreachable".into()));
        let text = std::fs::read_to_string(o.path(&o.records()[0].id)).unwrap();
        assert!(
            !text.contains("SECRET") && !text.contains("hooks.slack"),
            "{text}"
        );
        assert!(text.contains("slack-default"), "{text}");
    }
}
