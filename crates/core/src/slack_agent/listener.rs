//! The Socket Mode listener behind `loopx slack listen`: one per state dir
//! (a lock file), a status file, acks before any work, the intake gates, a
//! thread per request, approval buttons, and reconnects with backoff.
//!
//! Slack text is untrusted. Nothing here logs or stores a token or a message
//! body; the status file and the log carry Slack ids and short reasons only.

use super::approvals_bridge::{self, ButtonOutcome};
use super::catchup::{self, History};
use super::executor::{Executor, Poster};
use super::intake::{Intake, Request, Verdict};
use super::limits::RateLimit;
use super::seen::SeenEvents;
use super::socket::{self, Frame, SocketClient, SocketError};
use super::status::{self, State, Status};
use super::threads::ThreadMap;
use super::web::{self, AuthInfo, SlackWeb, WebError};
use crate::chat::ChatRuntime;
use crate::file_lock::FileLock;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The one reply a user off the allowlist gets (at most once per
/// [`UNAUTHORIZED_WINDOW`]); it says nothing about the machine or the setup.
pub const NOT_AUTHORIZED: &str = "You're not authorized to use LoopX";
/// The reply to a request over the per-user hourly limit.
pub const TOO_FAST: &str = "You're going a bit fast; try again in a bit.";
/// How long a user off the allowlist waits before a second [`NOT_AUTHORIZED`].
pub const UNAUTHORIZED_WINDOW: Duration = Duration::from_secs(600);
/// Requests per user per hour when the connector setting is missing or invalid.
pub const DEFAULT_RATE_PER_HOUR: u32 = 10;
/// `action_id` of the Allow / Deny buttons (`slack_bot::approval_blocks`).
pub const APPROVAL_ACTION: &str = "loopx_approval";

/// The Slack calls the listener makes on top of [`Poster`]. Implemented by
/// [`SlackWeb`]; every call is a single attempt.
pub trait SlackApi: Poster + History + 'static {
    fn open_socket_url(&self) -> Result<String, WebError>;
    fn auth_test(&self) -> Result<AuthInfo, WebError>;
}

impl SlackApi for SlackWeb {
    fn open_socket_url(&self) -> Result<String, WebError> {
        SlackWeb::open_socket_url(self)
    }
    fn auth_test(&self) -> Result<AuthInfo, WebError> {
        SlackWeb::auth_test(self)
    }
}

pub struct Config {
    /// Slack user ids (`slack_bot::parse_allowlist` output). Empty refuses to start.
    pub allowlist: Vec<String>,
    pub rate_per_hour: u32,
    pub approvals_dir: PathBuf,
    /// Holds `seen.json`, `threads.json`, `status.json` and `listener.lock`.
    pub state_dir: PathBuf,
}

impl Config {
    pub fn status_path(&self) -> PathBuf {
        status_path(&self.state_dir)
    }
}

/// Where the listener keeps its state for a LoopX root.
pub fn default_state_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("outputs").join("slack")
}

pub fn status_path(state_dir: &Path) -> PathBuf {
    state_dir.join("status.json")
}

/// The connector's `rate_per_hour` setting: a positive whole number, else
/// [`DEFAULT_RATE_PER_HOUR`].
pub fn rate_from_setting(raw: &str) -> u32 {
    match raw.trim().parse::<u32>() {
        Ok(n) if n > 0 => n,
        _ => DEFAULT_RATE_PER_HOUR,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// Slack rejected a token: status says so and the listener stops.
    TokenRejected,
    /// Another listener holds this state dir.
    LockHeld,
    /// The stop flag was set.
    Shutdown,
    /// Refused before any network call (say an empty allowlist).
    Misconfigured(String),
}

/// A [`Poster`] that remembers the first failure other than a rate limit, so
/// the status file can show it (a missing scope above all), and flags a
/// rejected (revoked, invalid, inactive) bot token so the listener stops.
/// A one-off failure (a network error, a Slack API error code) is forgotten
/// at the next successful call; a missing scope stays until a restart.
pub struct Observed<W> {
    inner: W,
    first_error: Arc<Mutex<Option<String>>>,
    /// The remembered failure is a one-off one (guarded by `first_error`'s lock).
    transient: AtomicBool,
    token_rejected: AtomicBool,
}

impl<W> Observed<W> {
    pub fn new(inner: W, first_error: Arc<Mutex<Option<String>>>) -> Self {
        Self {
            inner,
            first_error,
            transient: AtomicBool::new(false),
            token_rejected: AtomicBool::new(false),
        }
    }

    pub fn inner(&self) -> &W {
        &self.inner
    }

    /// Notes a rejected bot token without recording an error (catch-up reads
    /// do this: their other failures are handled where they happen).
    fn note_token_only<T>(&self, r: Result<T, WebError>) -> Result<T, WebError> {
        if matches!(r, Err(WebError::Auth)) {
            self.token_rejected.store(true, Ordering::SeqCst);
        }
        r
    }

    /// True once any call through this poster got [`WebError::Auth`]. Every
    /// [`Poster`] call uses the bot token, so the bot token is the one rejected.
    pub fn token_rejected(&self) -> bool {
        self.token_rejected.load(Ordering::SeqCst)
    }

    fn note<T>(&self, r: Result<T, WebError>) -> Result<T, WebError> {
        match &r {
            Err(e) => {
                if matches!(e, WebError::Auth) {
                    self.token_rejected.store(true, Ordering::SeqCst);
                }
                if !matches!(e, WebError::RateLimited { .. }) {
                    let mut slot = self.first_error.lock().unwrap_or_else(|p| p.into_inner());
                    if slot.is_none() {
                        *slot = Some(e.to_string());
                        let one_off = matches!(e, WebError::Net | WebError::Api(_));
                        self.transient.store(one_off, Ordering::SeqCst);
                    }
                }
            }
            Ok(_) if self.transient.load(Ordering::SeqCst) => {
                let mut slot = self.first_error.lock().unwrap_or_else(|p| p.into_inner());
                if self.transient.swap(false, Ordering::SeqCst) {
                    *slot = None;
                }
            }
            Ok(_) => {}
        }
        r
    }
}

impl<W: Poster> Poster for Observed<W> {
    fn post(&self, channel: &str, thread_ts: Option<&str>, text: &str) -> Result<String, WebError> {
        self.note(self.inner.post(channel, thread_ts, text))
    }
    fn post_blocks(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
        blocks: &Value,
    ) -> Result<String, WebError> {
        self.note(self.inner.post_blocks(channel, thread_ts, text, blocks))
    }
    fn update(&self, channel: &str, ts: &str, text: &str) -> Result<(), WebError> {
        self.note(self.inner.update(channel, ts, text))
    }
    fn update_blocks(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
        blocks: &Value,
    ) -> Result<(), WebError> {
        self.note(self.inner.update_blocks(channel, ts, text, blocks))
    }
    fn react(&self, channel: &str, ts: &str, emoji: &str, on: bool) -> Result<(), WebError> {
        self.note(self.inner.react(channel, ts, emoji, on))
    }
    fn open_dm(&self, user: &str) -> Result<String, WebError> {
        self.note(self.inner.open_dm(user))
    }
}

impl<W: History> History for Observed<W> {
    fn history(&self, channel: &str, oldest: &str) -> Result<Vec<Value>, WebError> {
        self.note_token_only(self.inner.history(channel, oldest))
    }
    fn member_channels(&self) -> Result<Vec<String>, WebError> {
        self.note_token_only(self.inner.member_channels())
    }
    fn message(&self, channel: &str, ts: &str) -> Result<Option<Value>, WebError> {
        self.note_token_only(self.inner.message(channel, ts))
    }
    fn replies(
        &self,
        channel: &str,
        thread_ts: &str,
        oldest: &str,
    ) -> Result<Vec<Value>, WebError> {
        self.note_token_only(self.inner.replies(channel, thread_ts, oldest))
    }
}

/// What the reconnect hook (offline catch-up) is given.
pub struct CatchUp<'a, W> {
    pub web: &'a W,
    pub allowlist: &'a [String],
    pub bot_user_id: &'a str,
    /// When the dropped connection was noticed.
    pub offline_since: DateTime<Utc>,
}

type CatchUpHook<W> = Box<dyn FnMut(&CatchUp<'_, W>) + Send>;
type BackoffFn = Box<dyn Fn(u32) -> Duration + Send>;

pub struct Listener<W: SlackApi> {
    cfg: Config,
    executor: Executor<Observed<W>>,
    first_error: Arc<Mutex<Option<String>>>,
    /// Longest wait on the socket before the stop flag is checked again.
    pub tick: Duration,
    /// A socket silent for longer than this is treated as dead.
    pub idle_limit: Duration,
    /// See [`UNAUTHORIZED_WINDOW`].
    pub unauthorized_window: Duration,
    backoff: BackoffFn,
    catch_up: Option<CatchUpHook<W>>,
    /// How many times to try the listener lock before [`Exit::LockHeld`], and
    /// the wait between tries: the dashboard's liveness probe holds it for an
    /// instant, which must not make a starting listener give up (default
    /// 20 x 100 ms, about 2 s).
    pub lock_tries: u32,
    pub lock_retry_every: Duration,
}

/// Runs a listener until `stop` is set or it cannot go on.
/// How long a Socket Mode connection may stay completely silent (pings
/// included) before the listener reconnects.
pub const IDLE_LIMIT: Duration = Duration::from_secs(120);

pub fn run_listener<W: SlackApi>(
    cfg: Config,
    web: W,
    runtime: ChatRuntime,
    stop: Arc<AtomicBool>,
) -> Exit {
    Listener::new(cfg, web, runtime).run(stop)
}

impl<W: SlackApi> Listener<W> {
    pub fn new(cfg: Config, web: W, runtime: ChatRuntime) -> Self {
        let first_error = Arc::new(Mutex::new(None));
        let threads = ThreadMap::new(cfg.state_dir.join("threads.json"));
        let executor = Executor::new(Observed::new(web, first_error.clone()), runtime, threads);
        Self {
            cfg,
            executor,
            first_error,
            tick: Duration::from_secs(1),
            idle_limit: IDLE_LIMIT,
            unauthorized_window: UNAUTHORIZED_WINDOW,
            backoff: Box::new(|attempt| socket::backoff(attempt) + jitter()),
            catch_up: None,
            lock_tries: 20,
            lock_retry_every: Duration::from_millis(100),
        }
    }

    /// Replaces the chat job runner (tests use runners that never spawn a CLI).
    pub fn with_runner(
        mut self,
        runner: impl Fn(&ChatRuntime, &str, &str, &str) + Send + Sync + 'static,
    ) -> Self {
        self.executor = self.executor.with_runner(runner);
        self
    }

    /// Replaces the wait before reconnect attempt `n` (default: doubling from
    /// 1 s, capped at 60 s, plus up to 1 s of jitter).
    pub fn with_backoff(mut self, f: impl Fn(u32) -> Duration + Send + 'static) -> Self {
        self.backoff = Box::new(f);
        self
    }

    /// Called, before the built-in catch-up, after each `hello` that follows
    /// an offline period: a dropped live connection, or a relaunch that found
    /// an earlier status file (the first `hello` of this process).
    pub fn on_reconnect(mut self, hook: impl FnMut(&CatchUp<'_, W>) + Send + 'static) -> Self {
        self.catch_up = Some(Box::new(hook));
        self
    }

    /// Takes the lock, then connects, handles frames and reconnects until
    /// `stop` is set or a token is rejected.
    pub fn run(self, stop: Arc<AtomicBool>) -> Exit {
        if self.cfg.allowlist.is_empty() {
            return Exit::Misconfigured("No allowed Slack users are set".into());
        }
        if let Err(e) = std::fs::create_dir_all(&self.cfg.state_dir) {
            return Exit::Misconfigured(format!("Cannot create the Slack state folder: {e}"));
        }
        let lock_path = self.cfg.state_dir.join("listener.lock");
        let mut tries = 0;
        let _lock = loop {
            match FileLock::try_exclusive(&lock_path) {
                Ok(Some(lock)) => break lock,
                Ok(None) => {
                    tries += 1;
                    if tries >= self.lock_tries.max(1) {
                        return Exit::LockHeld;
                    }
                    std::thread::sleep(self.lock_retry_every);
                }
                Err(e) => {
                    return Exit::Misconfigured(format!("Cannot open the listener lock: {e}"))
                }
            }
        };
        let Listener {
            cfg,
            executor,
            first_error,
            tick,
            idle_limit,
            unauthorized_window,
            backoff,
            catch_up,
            ..
        } = self;
        // Read BEFORE `run.write()` replaces it: a status file from an earlier
        // process (any state: a crash leaves "connected") means this start
        // follows an offline period.
        let previous = status::read_status(&cfg.status_path()).map(|s| {
            DateTime::parse_from_rfc3339(&s.since)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now())
        });
        let mut run = Run {
            seen: SeenEvents::new(cfg.state_dir.join("seen.json")),
            seen_path: cfg.state_dir.join("seen.json"),
            rate: Arc::new(Mutex::new(RateLimit::new(cfg.rate_per_hour))),
            status_path: cfg.status_path(),
            cfg,
            ex: Arc::new(executor),
            first_error,
            intake: None,
            unauthorized: HashMap::new(),
            unauthorized_window,
            status: Status {
                state: State::Offline,
                since: now(),
                last_event: None,
                last_error: None,
            },
            conn_error: None,
            sticky_error: None,
            catch_up_note: Arc::new(Mutex::new(None)),
            tick,
            idle_limit,
            backoff,
            catch_up,
            catch_up_noted: Arc::new(AtomicBool::new(false)),
            relaunch_outage: previous,
            stop,
        };
        run.write();
        run.main()
    }
}

/// The running listener's state.
struct Run<W: SlackApi> {
    cfg: Config,
    ex: Arc<Executor<Observed<W>>>,
    first_error: Arc<Mutex<Option<String>>>,
    intake: Option<Intake>,
    seen: SeenEvents,
    seen_path: PathBuf,
    rate: Arc<Mutex<RateLimit>>,
    /// When each user off the allowlist was last answered.
    unauthorized: HashMap<String, Instant>,
    unauthorized_window: Duration,
    status: Status,
    status_path: PathBuf,
    /// Why the connection is down; cleared on `hello`.
    conn_error: Option<String>,
    /// The seen store cannot be written; shown while connected.
    sticky_error: Option<String>,
    /// "catch-up unavailable (missing scope)" until a later pass succeeds.
    catch_up_note: Arc<Mutex<Option<String>>>,
    tick: Duration,
    idle_limit: Duration,
    backoff: BackoffFn,
    catch_up: Option<CatchUpHook<W>>,
    /// Set once the missing-scope catch-up note was logged.
    catch_up_noted: Arc<AtomicBool>,
    /// The offline period before this process started, if any; consumed by
    /// the first `hello`.
    relaunch_outage: Option<DateTime<Utc>>,
    stop: Arc<AtomicBool>,
}

/// How a connection ended.
enum Ended {
    Stop,
    /// A Slack call (on any thread) got the bot token rejected.
    BotTokenRejected,
    /// `live`: the connection had said `hello`.
    Dropped {
        why: String,
        live: bool,
    },
}

impl<W: SlackApi> Run<W> {
    fn main(&mut self) -> Exit {
        let mut attempt: u32 = 0;
        // Set at a relaunch over an earlier status file and when a live
        // connection drops; the next `hello` runs catch-up.
        let mut outage: Option<DateTime<Utc>> = self.relaunch_outage.take();
        loop {
            if self.stopped() {
                return self.shutdown();
            }
            if self.ex.poster().token_rejected() {
                return self.rejected("Slack rejected the bot token");
            }
            match self.connect() {
                Ok(client) => match self.serve(client, &mut attempt, &mut outage) {
                    Ended::Stop => return self.shutdown(),
                    Ended::BotTokenRejected => {
                        return self.rejected("Slack rejected the bot token")
                    }
                    Ended::Dropped { why, live } => {
                        let _ = self.offline(why);
                        if live && outage.is_none() {
                            outage = Some(self.since());
                        }
                    }
                },
                Err(Some(exit)) => return exit,
                Err(None) => {}
            }
            if self.wait((self.backoff)(attempt)) {
                return self.shutdown();
            }
            attempt = attempt.saturating_add(1);
        }
    }

    /// The bot's identity (once), a socket URL and the websocket.
    /// `Err(Some(exit))` ends the listener; `Err(None)` retries after a backoff.
    fn connect(&mut self) -> Result<SocketClient, Option<Exit>> {
        if self.intake.is_none() {
            match self.ex.poster().inner().auth_test() {
                Ok(info) => {
                    self.intake = Some(Intake {
                        allowlist: self.cfg.allowlist.clone(),
                        bot_user_id: info.bot_user_id,
                    })
                }
                Err(WebError::Auth) => {
                    return Err(Some(self.rejected("Slack rejected the bot token")))
                }
                // Retrying cannot fix the wrong kind of token: stop like a
                // rejected one instead of reconnecting every minute.
                Err(e) if web::is_token_setup_error(&e) => {
                    return Err(Some(self.rejected(
                        "Slack refused the bot token: it is not a bot token (xoxb-); are the two tokens swapped?",
                    )))
                }
                Err(e) => return Err(self.offline(e.to_string())),
            }
        }
        let url = match self.ex.poster().inner().open_socket_url() {
            Ok(url) => url,
            Err(WebError::Auth) => {
                return Err(Some(self.rejected("Slack rejected the app-level token")))
            }
            Err(WebError::MissingScope(_)) => {
                return Err(Some(self.rejected(
                    "Slack refused the app-level token: it lacks the connections:write scope",
                )))
            }
            Err(e) if web::is_token_setup_error(&e) => {
                return Err(Some(self.rejected(
                    "Slack refused the app-level token: it is not an app-level token (xapp-); are the two tokens swapped?",
                )))
            }
            Err(e) => return Err(self.offline(e.to_string())),
        };
        // Socket errors never carry the URL (it holds a ticket).
        SocketClient::connect(&url).map_err(|e| self.offline(format!("Slack connection: {e}")))
    }

    /// Handles frames until the connection drops or `stop` is set.
    fn serve(
        &mut self,
        mut client: SocketClient,
        attempt: &mut u32,
        outage: &mut Option<DateTime<Utc>>,
    ) -> Ended {
        let mut live = false;
        let dropped = |e: SocketError, live: bool| Ended::Dropped {
            why: format!("Slack connection: {e}"),
            live,
        };
        loop {
            if self.stopped() {
                return Ended::Stop;
            }
            if self.ex.poster().token_rejected() {
                return Ended::BotTokenRejected;
            }
            self.surface_errors();
            // Slack keeps a live socket busy with pings; one that has gone quiet
            // (the Mac slept, the network changed) is dead, so reconnect and
            // let catch-up offer what was missed.
            if client.idle_for() > self.idle_limit {
                return dropped(SocketError::Closed, live);
            }
            let frame = match client.next(self.tick) {
                // Checked again before any work: no envelope is handled (and
                // no run started) once the bot token has been rejected.
                Ok(Some(_)) if self.ex.poster().token_rejected() => return Ended::BotTokenRejected,
                Ok(Some(f)) => f,
                Ok(None) => continue,
                Err(e) => return dropped(e, live),
            };
            match frame {
                Frame::Hello => {
                    live = true;
                    *attempt = 0;
                    self.conn_error = None;
                    self.set_state(State::Connected);
                    if let Some(since) = outage.take() {
                        self.run_catch_up(since);
                    }
                }
                Frame::Disconnect { .. } => {
                    return Ended::Dropped {
                        why: "Slack asked to reconnect".into(),
                        live,
                    }
                }
                Frame::Event {
                    envelope_id,
                    payload,
                } => {
                    // Ack first: Slack retries anything not acked within 3 s.
                    if let Err(e) = client.ack(&envelope_id) {
                        return dropped(e, live);
                    }
                    self.on_event(&payload);
                }
                Frame::Interactive {
                    envelope_id,
                    payload,
                } => {
                    if let Err(e) = client.ack(&envelope_id) {
                        return dropped(e, live);
                    }
                    self.on_interactive(&payload);
                }
                Frame::Other => {}
            }
        }
    }

    fn on_event(&mut self, payload: &Value) {
        let verdict = match &self.intake {
            Some(intake) => intake.judge(payload),
            None => return,
        };
        self.status.last_event = Some(now());
        match verdict {
            Verdict::Ignore => {}
            Verdict::Unauthorized(req) => {
                if self.first_time(&req.event_id) {
                    self.on_unauthorized(&req);
                }
            }
            Verdict::Run(req) => {
                if self.first_time(&req.event_id) {
                    // One key per message, shared with pressed catch-up
                    // offers: a redelivery or an offer for a message that
                    // already ran (either way) does not run it again.
                    if self.first_time(&catchup::message_key(&req.channel, &req.ts)) {
                        dispatch(&self.ex, &self.rate, req);
                    } else {
                        self.log(&format!(
                            "message in {} already handled; not run again",
                            req.channel
                        ));
                    }
                }
            }
        }
        self.write();
    }

    /// One fixed reply per user per window; every attempt is logged.
    fn on_unauthorized(&mut self, req: &Request) {
        let answered = self.answer_unauthorized(&req.user);
        self.log(&format!(
            "unauthorized Slack user {} in {} ({})",
            req.user,
            req.channel,
            if answered {
                "answered"
            } else {
                "not answered again"
            }
        ));
        if answered {
            self.post_async(&req.channel, Some(&req.thread_ts), NOT_AUTHORIZED);
        }
    }

    /// True when `user` may get the not-authorized reply now (and records it).
    fn answer_unauthorized(&mut self, user: &str) -> bool {
        let now = Instant::now();
        let window = self.unauthorized_window;
        self.unauthorized
            .retain(|_, at| now.saturating_duration_since(*at) < window);
        if self.unauthorized.contains_key(user) {
            return false;
        }
        self.unauthorized.insert(user.to_string(), now);
        true
    }

    /// Approval and catch-up buttons; any other button is acked and left alone.
    fn on_interactive(&mut self, payload: &Value) {
        if payload.get("type").and_then(Value::as_str) != Some("block_actions") {
            return;
        }
        let text = |p: &str| payload.pointer(p).and_then(Value::as_str).unwrap_or("");
        let user = text("/user/id").to_string();
        let channel = text("/channel/id").to_string();
        let message_ts = match text("/container/message_ts") {
            "" => text("/message/ts"),
            ts => ts,
        }
        .to_string();
        let thread = match text("/message/thread_ts") {
            "" => message_ts.clone(),
            ts => ts.to_string(),
        };
        let actions = payload
            .get("actions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for action in actions {
            let value = action.get("value").and_then(Value::as_str).unwrap_or("");
            match action.get("action_id").and_then(Value::as_str) {
                Some(APPROVAL_ACTION) => {}
                Some(catchup::CATCHUP_ACTION) => {
                    self.on_catchup(&user, &channel, &message_ts, value);
                    continue;
                }
                _ => continue,
            }
            let outcome = approvals_bridge::handle_button(
                &self.cfg.approvals_dir,
                &self.cfg.allowlist,
                &user,
                value,
                Utc::now(),
            );
            self.log(&format!("approval button from {user}: {outcome:?}"));
            let reply = match outcome {
                ButtonOutcome::Recorded if value.ends_with(":deny") => Some("Denied.".to_string()),
                ButtonOutcome::Recorded => Some("Approved.".to_string()),
                ButtonOutcome::Rejected("not authorized") => self
                    .answer_unauthorized(&user)
                    .then(|| NOT_AUTHORIZED.to_string()),
                ButtonOutcome::Rejected(why) => Some(format!("Not recorded: {why}.")),
            };
            if let Some(reply) = reply.filter(|_| !channel.is_empty()) {
                let thread = (!thread.is_empty()).then_some(thread.as_str());
                self.post_async(&channel, thread, &reply);
            }
        }
    }

    /// A Run / Ignore press on a catch-up offer (`offer_ts` is the offer
    /// message). Only allowlisted users; anyone else is ignored (no reply, no
    /// state change). `Run` fetches the original message again and sends it
    /// down the live path: [`SeenEvents`] on the synthetic id (a double press
    /// runs once), the rate limit, the executor's thread guard.
    fn on_catchup(&mut self, user: &str, channel: &str, offer_ts: &str, value: &str) {
        if !self.cfg.allowlist.iter().any(|u| u == user) {
            self.log(&format!(
                "catch-up button from {user} ignored: not allowlisted"
            ));
            return;
        }
        let Some((orig_channel, ts, run)) = catchup::parse_catchup_value(value) else {
            self.log("catch-up button ignored: malformed value");
            return;
        };
        // Slack sends the press from the offer's own channel.
        if orig_channel != channel {
            self.log("catch-up button ignored: channel mismatch");
            return;
        }
        if !run {
            self.log(&format!("catch-up ignored by {user} in {channel}"));
            self.finish_offer(channel, offer_ts, "Ignored.");
            return;
        }
        let Some(bot) = self.intake.as_ref().map(|i| i.bot_user_id.clone()) else {
            return;
        };
        // Off the loop thread: the socket keeps being acked and answered, and
        // `stop` seen, while Slack is asked for the message.
        let press = Press {
            ex: self.ex.clone(),
            rate: self.rate.clone(),
            seen_path: self.seen_path.clone(),
            allowlist: self.cfg.allowlist.clone(),
            bot,
            user: user.to_string(),
            channel: channel.to_string(),
            ts,
            offer_ts: offer_ts.to_string(),
        };
        let _ = std::thread::Builder::new()
            .name("slack-catchup-press".into())
            .spawn(move || press.run());
    }

    /// Replaces an offer with `text` and no buttons (from a short-lived thread).
    fn finish_offer(&self, channel: &str, offer_ts: &str, text: &str) {
        let (ex, channel, ts, text) = (
            self.ex.clone(),
            channel.to_string(),
            offer_ts.to_string(),
            text.to_string(),
        );
        let _ = std::thread::Builder::new()
            .name("slack-reply".into())
            .spawn(move || replace_offer(&ex, &channel, &ts, &text));
    }

    /// [`SeenEvents::first_time`], noting in status when the store could not
    /// be written (it then fails open).
    fn first_time(&mut self, event_id: &str) -> bool {
        let first = self.seen.first_time(event_id);
        if first {
            let stored = std::fs::read_to_string(&self.seen_path)
                .is_ok_and(|t| t.lines().any(|l| l == event_id));
            if !stored && self.sticky_error.is_none() {
                self.sticky_error = Some("The Slack event de-dupe store cannot be written".into());
                self.log("the seen-events store cannot be written; duplicates may run");
            }
        }
        first
    }

    /// The test hook, then the built-in pass on its own thread (it makes
    /// many Slack calls; the socket must keep being read and acked).
    fn run_catch_up(&mut self, since: DateTime<Utc>) {
        let Some(bot) = self.intake.as_ref().map(|i| i.bot_user_id.clone()) else {
            return;
        };
        if let Some(hook) = self.catch_up.as_mut() {
            hook(&CatchUp {
                web: self.ex.poster().inner(),
                allowlist: &self.cfg.allowlist,
                bot_user_id: &bot,
                offline_since: since,
            });
        }
        let ex = self.ex.clone();
        let allowlist = self.cfg.allowlist.clone();
        let seen = SeenEvents::new(self.seen_path.clone());
        let note = self.catch_up_note.clone();
        let noted = self.catch_up_noted.clone();
        let _ = std::thread::Builder::new()
            .name("slack-catchup".into())
            .spawn(move || {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                let root = &ex.runtime().repo_root;
                let out = catchup::offer_missed(ex.poster(), &allowlist, &bot, &seen, now);
                log_line(root, &format!("catch-up: {} offer(s) posted", out.offers));
                let set_note = |v: Option<&str>| {
                    *note.lock().unwrap_or_else(|p| p.into_inner()) = v.map(str::to_string);
                };
                match out.error {
                    Some(WebError::MissingScope(_)) => {
                        if !noted.swap(true, Ordering::SeqCst) {
                            log_line(root, catchup::MISSING_SCOPE_NOTE);
                        }
                        set_note(Some(catchup::MISSING_SCOPE_NOTE));
                    }
                    // A full pass: the scopes are there now.
                    None => set_note(None),
                    // A rejected token was flagged by `Observed`; the listener stops.
                    Some(WebError::Auth) => {}
                    Some(e) => log_line(root, &format!("catch-up stopped early: {e}")),
                }
            });
    }

    /// Posts from a short-lived thread, so a slow Slack never delays acks.
    fn post_async(&self, channel: &str, thread: Option<&str>, text: &str) {
        let ex = self.ex.clone();
        let (channel, thread, text) = (
            channel.to_string(),
            thread.map(str::to_string),
            text.to_string(),
        );
        let _ = std::thread::Builder::new()
            .name("slack-reply".into())
            .spawn(move || {
                let _ = ex.poster().post(&channel, thread.as_deref(), &text);
            });
    }

    /// Rewrites the status file when the error it should show changed (a Web
    /// API failure seen or forgotten, the catch-up note set or cleared).
    fn surface_errors(&mut self) {
        if self.status.last_error != self.last_error() {
            self.write();
        }
    }

    /// What the status shows: why the connection is down, else a lasting
    /// problem (the seen store, a Web API failure, the catch-up note).
    fn last_error(&self) -> Option<String> {
        let slot = |m: &Mutex<Option<String>>| m.lock().unwrap_or_else(|p| p.into_inner()).clone();
        self.conn_error
            .clone()
            .or_else(|| self.sticky_error.clone())
            .or_else(|| slot(&self.first_error))
            .or_else(|| slot(&self.catch_up_note))
    }

    /// When the current state began (the status file's `since`).
    fn since(&self) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&self.status.since)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now())
    }

    /// Records a transient failure; the caller retries after a backoff.
    fn offline(&mut self, why: String) -> Option<Exit> {
        self.conn_error = Some(why);
        self.set_state(State::Offline);
        None
    }

    fn rejected(&mut self, why: &str) -> Exit {
        self.conn_error = Some(why.to_string());
        self.log(why);
        self.set_state(State::NeedsAttention);
        Exit::TokenRejected
    }

    fn shutdown(&mut self) -> Exit {
        self.conn_error = None;
        self.set_state(State::Offline);
        Exit::Shutdown
    }

    fn set_state(&mut self, state: State) {
        if self.status.state != state {
            self.status.state = state;
            self.status.since = now();
        }
        self.write();
    }

    fn write(&mut self) {
        self.status.last_error = self.last_error();
        status::write_status(&self.status_path, &self.status);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Sleeps `d` in short slices; true when `stop` was set meanwhile.
    fn wait(&self, d: Duration) -> bool {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if self.stopped() {
                return true;
            }
            let left = end.saturating_duration_since(Instant::now());
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
        self.stopped()
    }

    /// Ids and short reasons only: never a token or a message body.
    fn log(&self, detail: &str) {
        log_line(&self.ex.runtime().repo_root, detail);
    }
}

/// The rate limit, then a thread running the request (the executor's thread
/// guard applies there). True when a run was started. Live messages and
/// pressed catch-up offers both come through here.
fn dispatch<W: SlackApi>(
    ex: &Arc<Executor<Observed<W>>>,
    rate: &Mutex<RateLimit>,
    req: Request,
) -> bool {
    let allowed = rate
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .allow(&req.user, Instant::now());
    if allowed {
        let ex = ex.clone();
        let _ = std::thread::Builder::new()
            .name("slack-request".into())
            .spawn(move || ex.run(&req));
        return true;
    }
    log_line(
        &ex.runtime().repo_root,
        &format!(
            "rate limit: request from {} in {} not run",
            req.user, req.channel
        ),
    );
    let ex = ex.clone();
    let _ = std::thread::Builder::new()
        .name("slack-reply".into())
        .spawn(move || {
            let _ = ex
                .poster()
                .post(&req.channel, Some(&req.thread_ts), TOO_FAST);
        });
    false
}

/// Replaces an offer message with `text` and no buttons.
fn replace_offer<W: SlackApi>(
    ex: &Executor<Observed<W>>,
    channel: &str,
    offer_ts: &str,
    text: &str,
) {
    if channel.is_empty() || offer_ts.is_empty() {
        return;
    }
    let blocks = catchup::result_blocks(text);
    let _ = ex.poster().update_blocks(channel, offer_ts, text, &blocks);
}

/// A pressed `Run`, handled on its own thread.
struct Press<W: SlackApi> {
    ex: Arc<Executor<Observed<W>>>,
    rate: Arc<Mutex<RateLimit>>,
    seen_path: PathBuf,
    allowlist: Vec<String>,
    bot: String,
    user: String,
    channel: String,
    /// The original message.
    ts: String,
    offer_ts: String,
}

impl<W: SlackApi> Press<W> {
    /// Fetches exactly the original message, re-judges it, checks the bot
    /// has not answered it, then records the per-message key and sends it
    /// down the live path (rate limit, executor thread guard).
    fn run(self) {
        let root = self.ex.runtime().repo_root.clone();
        let log = |d: &str| log_line(&root, d);
        let finish = |text: &str| replace_offer(&self.ex, &self.channel, &self.offer_ts, text);
        let req = match catchup::original_request(
            self.ex.poster(),
            &self.channel,
            &self.ts,
            &self.bot,
            &self.allowlist,
        ) {
            Ok(Some(req)) => req,
            Ok(None) => return finish("That message is no longer available."),
            Err(e) => {
                // Nothing consumed: the user can press again.
                return log(&format!("catch-up: could not read the message again ({e})"));
            }
        };
        match catchup::already_answered(self.ex.poster(), &req, &self.bot, &self.offer_ts) {
            Ok(false) => {}
            Ok(true) => return finish(catchup::ALREADY_HANDLED),
            Err(e) => return log(&format!("catch-up: could not read the thread ({e})")),
        }
        let seen = SeenEvents::new(self.seen_path.clone());
        // A double press: the first one is already on it.
        if !seen.first_time(&req.event_id) {
            return;
        }
        if !seen.first_time(&catchup::message_key(&req.channel, &req.ts)) {
            return finish(catchup::ALREADY_HANDLED);
        }
        log(&format!(
            "catch-up run pressed by {} in {}",
            self.user, self.channel
        ));
        if dispatch(&self.ex, &self.rate, req) {
            finish("Running it now.");
        }
    }
}

fn log_line(repo_root: &Path, detail: &str) {
    eprintln!("loopx slack: {detail}");
    crate::chat::append_unified_log(repo_root, "slack", detail, None);
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

static SIGNALLED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    SIGNALLED.store(true, Ordering::SeqCst);
}

/// A stop flag for [`run_listener`] that SIGTERM (launchd's unload) or SIGINT
/// sets. For the `loopx slack listen` process only: it replaces the
/// process's handlers for both signals.
pub fn stop_on_signals() -> Arc<AtomicBool> {
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    for sig in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler only stores to an atomic.
        unsafe { libc::signal(sig, handler) };
    }
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::spawn(move || loop {
        if SIGNALLED.load(Ordering::SeqCst) {
            flag.store(true, Ordering::SeqCst);
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    });
    stop
}

/// Up to a second, from the clock (no `rand` dependency).
fn jitter() -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    Duration::from_millis(u64::from(nanos) % 1000)
}

#[cfg(test)]
#[path = "listener_tests.rs"]
mod tests;
