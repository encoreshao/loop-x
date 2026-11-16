//! Executor and replier: runs one Slack request through the dashboard chat
//! engine ([`chat::run_chat_job`]) on the thread's chat session, and reports
//! back in the thread: an `eyes` reaction and an "on it" message, throttled
//! progress edits of that message, then the final reply (split when long)
//! and a check or an X.
//!
//! Slack text is untrusted user content. Only the stripped request text is
//! stored and sent as the user's turn; Slack ids never reach the prompt.

use super::intake::Request;
use super::threads::ThreadMap;
use super::web::{split_message, SlackWeb, WebError};
use crate::chat::{self, ChatRuntime, StreamItem};
use crate::chat_tools::private_ledger;
use crate::connectors::slack_bot::escape_mrkdwn;
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(test)]
pub(crate) use super::web::SLACK_TEXT_LIMIT;

pub const ON_IT: &str = "On it…";
pub const STILL_WORKING: &str = "Still working on your last message in this thread.";
pub const DM_NOTICE: &str = "I sent the answer to you in a DM because it includes private data.";
const DM_FAILED: &str =
    "The answer includes private data, and I couldn't open a DM to send it to you.";
const STOPPED: &str = "The run was stopped.";
/// The only failure text a channel sees when the turn or thread is private.
pub const GENERIC_ERROR: &str = "Sorry, that didn't work.";
/// The chat job's worker ended without finishing the job.
pub const UNEXPECTED_STOP: &str = "The assistant stopped unexpectedly";
/// The turn ran past [`Executor::turn_deadline`] and was cancelled.
pub const TIMED_OUT: &str = "The assistant took too long and was stopped";

/// Tool-name prefixes whose results are private to the user. The real tools
/// today are `mail.search`, `mail.read`, `mail.send`, `calendar.list_events`,
/// `calendar.free_busy` and `calendar.create_event`; `gmail.` and `inbox.`
/// cover the names a future mail or inbox tool would take.
pub const PRIVATE_TOOL_PREFIXES: &[&str] = &["mail.", "gmail.", "calendar.", "inbox."];

/// Retries of a rate-limited final reply call, after the first attempt.
const FINAL_RETRIES: u32 = 3;
/// How long one wait on the chat job's stream may block.
const POLL: Duration = Duration::from_millis(100);
/// Tool lines kept in the progress message.
const PROGRESS_TOOLS: usize = 8;

/// The Slack calls the executor (and the approval bridge) make. Implemented by
/// [`SlackWeb`] and by test recorders. Every call is a single attempt; the
/// caller owns retry policy.
pub trait Poster: Send + Sync {
    fn post(&self, channel: &str, thread_ts: Option<&str>, text: &str) -> Result<String, WebError>;
    fn post_blocks(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
        blocks: &Value,
    ) -> Result<String, WebError>;
    fn update(&self, channel: &str, ts: &str, text: &str) -> Result<(), WebError>;
    /// Replaces a message's text and blocks (a pressed offer loses its
    /// buttons). Defaults to a text-only [`Poster::update`] for posters that
    /// carry no blocks.
    fn update_blocks(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
        _blocks: &Value,
    ) -> Result<(), WebError> {
        self.update(channel, ts, text)
    }
    fn react(&self, channel: &str, ts: &str, emoji: &str, on: bool) -> Result<(), WebError>;
    fn open_dm(&self, user: &str) -> Result<String, WebError>;
}

impl Poster for SlackWeb {
    fn post(&self, channel: &str, thread_ts: Option<&str>, text: &str) -> Result<String, WebError> {
        SlackWeb::post(self, channel, thread_ts, text, None)
    }
    fn post_blocks(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
        blocks: &Value,
    ) -> Result<String, WebError> {
        SlackWeb::post(self, channel, thread_ts, text, Some(blocks))
    }
    fn update(&self, channel: &str, ts: &str, text: &str) -> Result<(), WebError> {
        SlackWeb::update(self, channel, ts, text, None)
    }
    fn update_blocks(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
        blocks: &Value,
    ) -> Result<(), WebError> {
        SlackWeb::update(self, channel, ts, text, Some(blocks))
    }
    fn react(&self, channel: &str, ts: &str, emoji: &str, on: bool) -> Result<(), WebError> {
        if on {
            SlackWeb::react(self, channel, ts, emoji)
        } else {
            SlackWeb::unreact(self, channel, ts, emoji)
        }
    }
    fn open_dm(&self, user: &str) -> Result<String, WebError> {
        SlackWeb::open_dm(self, user)
    }
}

/// True when any of the tools a turn used (`tool_names`) reads or writes the
/// user's mail, calendar or inbox: a known prefix, or a registered tool the
/// one shared predicate ([`private_ledger::is_private`]) marks private.
pub fn reply_in_dm(tool_names: &[String]) -> bool {
    tool_names.iter().any(|n| is_private_tool(n))
}

fn is_private_tool(name: &str) -> bool {
    has_private_prefix(name) || private_ledger::is_private_name(name)
}

fn has_private_prefix(name: &str) -> bool {
    PRIVATE_TOOL_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// A one-line failure for the thread. Only recognized engine messages are
/// kept (cut before their detail, path-like words dropped); anything else,
/// such as model text from an `is_error` result, becomes [`GENERIC_ERROR`].
pub fn short_error(error: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"^(?:(?:Claude Code|Codex CLI|Gemini CLI|Kimi CLI|Cursor CLI|Grok CLI) (?:exited with code -?\d+|timed out after \d+ s|was stopped by a signal|didn't return a reply)|(?i:could not) (?:start|prepare)\b[^:(]*)",
        )
        .expect("regex")
    });
    let first = error
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let own = [STOPPED, UNEXPECTED_STOP, TIMED_OUT, chat::STOPPED_MESSAGE];
    let head = if own.contains(&first) {
        first
    } else {
        match re.find(first) {
            Some(m) => m.as_str(),
            None => return GENERIC_ERROR.to_string(),
        }
    };
    let words: Vec<&str> = head
        .split_whitespace()
        .filter(|w| !w.contains('/') && !w.contains('\\') && !w.starts_with('~'))
        .collect();
    let head: String = words.join(" ").chars().take(200).collect();
    let head = head.trim_end_matches('.');
    if head.is_empty() {
        GENERIC_ERROR.to_string()
    } else {
        format!("Sorry, that didn't work: {head}.")
    }
}

/// Removes its thread from the active set when the run ends, however it ends.
struct ActiveGuard<'a> {
    set: &'a Mutex<HashSet<String>>,
    key: String,
}

impl<'a> ActiveGuard<'a> {
    fn claim(set: &'a Mutex<HashSet<String>>, key: String) -> Option<Self> {
        // Never build the guard on refusal: dropping it would remove the
        // running request's key (and re-lock the held mutex).
        let claimed = set
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone());
        claimed.then(|| Self { set, key })
    }
}

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        let mut s = self.set.lock().unwrap_or_else(|e| e.into_inner());
        s.remove(&self.key);
    }
}

/// A finished turn: the reply or the engine's error, and on both arms the
/// tools the stream showed.
struct Turn {
    result: Result<String, String>,
    tools: Vec<String>,
}

type SleepFn = Arc<dyn Fn(Duration) + Send + Sync>;
/// Runs one chat job: `(runtime, reply_key, prompt, session_id)`.
type RunnerFn = Arc<dyn Fn(&ChatRuntime, &str, &str, &str) + Send + Sync>;

pub struct Executor<P: Poster> {
    poster: P,
    rt: ChatRuntime,
    threads: ThreadMap,
    active: Mutex<HashSet<String>>,
    /// The shortest gap between two progress edits of one message.
    pub min_edit_interval: Duration,
    /// How long one turn may run before it is cancelled.
    pub turn_deadline: Duration,
    /// How long a cancelled turn gets to wind down before the run gives up.
    pub cancel_grace: Duration,
    sleep: SleepFn,
    runner: RunnerFn,
}

impl<P: Poster> Executor<P> {
    pub fn new(poster: P, rt: ChatRuntime, threads: ThreadMap) -> Self {
        Self {
            poster,
            rt,
            threads,
            active: Mutex::new(HashSet::new()),
            min_edit_interval: Duration::from_secs(2),
            turn_deadline: Duration::from_secs(
                u64::from(chat::CHAT_SUBPROCESS_TIMEOUT_SECONDS) + 60,
            ),
            cancel_grace: Duration::from_secs(5),
            sleep: Arc::new(std::thread::sleep),
            runner: Arc::new(chat::run_chat_job),
        }
    }

    /// Replaces the wait before a rate-limited retry (tests record it).
    pub fn with_sleep(mut self, sleep: impl Fn(Duration) + Send + Sync + 'static) -> Self {
        self.sleep = Arc::new(sleep);
        self
    }

    /// Replaces [`chat::run_chat_job`] (tests use runners that hang or never finish).
    pub fn with_runner(
        mut self,
        runner: impl Fn(&ChatRuntime, &str, &str, &str) + Send + Sync + 'static,
    ) -> Self {
        self.runner = Arc::new(runner);
        self
    }

    pub fn poster(&self) -> &P {
        &self.poster
    }

    pub fn runtime(&self) -> &ChatRuntime {
        &self.rt
    }

    pub fn threads(&self) -> &ThreadMap {
        &self.threads
    }

    /// Runs one request to completion (blocking; the caller spawns a thread
    /// per request). One run per thread at a time: a request for a busy thread
    /// gets one short reply and no run.
    pub fn run(&self, req: &Request) {
        let thread_key = format!("{}:{}", req.channel, req.thread_ts);
        let Some(_guard) = ActiveGuard::claim(&self.active, thread_key) else {
            let _ = self
                .poster
                .post(&req.channel, Some(&req.thread_ts), STILL_WORKING);
            return;
        };
        let _ = self.poster.react(&req.channel, &req.ts, "eyes", true);
        let progress = self
            .poster
            .post(&req.channel, Some(&req.thread_ts), ON_IT)
            .ok();
        let session = self.session_for(req);
        let turn = self.converse(req, &session, progress.as_deref());
        let ok = self.deliver(req, &session, progress.as_deref(), turn);
        let _ = self.poster.react(&req.channel, &req.ts, "eyes", false);
        let done = if ok { "white_check_mark" } else { "x" };
        let _ = self.poster.react(&req.channel, &req.ts, done, true);
    }

    /// The thread's bound session, or a new one bound to it.
    fn session_for(&self, req: &Request) -> String {
        if let Some(sid) = self
            .threads
            .session_for(&req.channel, &req.thread_ts)
            .filter(|s| self.rt.store.chat_session_exists(s))
        {
            return sid;
        }
        let sid = self
            .rt
            .store
            .create_chat_session(&req.text, chrono::Utc::now());
        self.threads.bind(&req.channel, &req.thread_ts, &sid);
        sid
    }

    /// Stores the user's turn, runs the chat job and follows its stream,
    /// editing the progress message as tools start. Ends on the job's `Done`,
    /// when its worker exits without one, or at [`Self::turn_deadline`]
    /// (the job is then cancelled like the dashboard's Stop).
    fn converse(&self, req: &Request, session: &str, progress: Option<&str>) -> Turn {
        let store = &self.rt.store;
        // Seen: the Slack text belongs to this chat session, never to a loop.
        store.append_message_seen("user", &req.text, Some(session));
        let all = store.chat_session_messages(session);
        let upto = all.len().saturating_sub(1);
        let start = upto.saturating_sub(chat::CHAT_MESSAGE_HISTORY_LIMIT);
        let prompt = chat::build_chat_prompt(&req.text, &all[start..upto], None);

        let mut tools: Vec<String> = Vec::new();
        let failed = |e: &str, tools: Vec<String>| Turn {
            result: Err(e.to_string()),
            tools,
        };
        let key = self.rt.jobs.create();
        let Some(mut stream) = self.rt.jobs.stream(&key) else {
            return failed("Could not start the assistant", tools);
        };
        let worker = {
            let (rt, key, sid) = (self.rt.clone(), key.clone(), session.to_string());
            let runner = self.runner.clone();
            std::thread::Builder::new()
                .name("slack-chat-reply".into())
                .spawn(move || runner(&rt, &key, &prompt, &sid))
        };
        let Ok(worker) = worker else {
            return failed("Could not start the assistant", tools);
        };

        let deadline = Instant::now() + self.turn_deadline;
        let mut shown = 0;
        let mut last_edit = Instant::now();
        let mut done: Option<(Option<String>, Option<String>)> = None;
        loop {
            let Some(batch) = stream.next_batch(POLL) else {
                break;
            };
            absorb(batch, &mut tools, &mut done);
            if done.is_some() {
                break;
            }
            if worker.is_finished() {
                // It may have finished the job just after that batch.
                if let Some(batch) = stream.next_batch(Duration::ZERO) {
                    absorb(batch, &mut tools, &mut done);
                }
                done.get_or_insert((Some(UNEXPECTED_STOP.into()), None));
                break;
            }
            if Instant::now() >= deadline {
                self.rt.jobs.cancel(&key);
                let grace = Instant::now() + self.cancel_grace;
                while !worker.is_finished() && Instant::now() < grace {
                    if let Some(batch) = stream.next_batch(POLL) {
                        absorb(batch, &mut tools, &mut done);
                    }
                }
                done = Some((Some(TIMED_OUT.into()), None));
                break;
            }
            if tools.len() == shown {
                continue;
            }
            let Some(ts) = progress else { continue };
            if last_edit.elapsed() < self.min_edit_interval {
                continue;
            }
            // A failed progress edit (a 429 above all) is simply dropped.
            if self
                .poster
                .update(&req.channel, ts, &progress_text(&tools))
                .is_ok()
            {
                shown = tools.len();
            }
            last_edit = Instant::now();
        }
        // A worker still running past the grace period is left to wind down.
        if worker.is_finished() {
            let _ = worker.join();
        }
        let result = match done {
            Some((None, Some(text))) => Ok(text),
            Some((Some(e), _)) => Err(e),
            _ => Err(STOPPED.into()),
        };
        Turn { result, tools }
    }

    /// Whether the reply may hold private data: this turn's streamed tools, the
    /// session's private-call ledger (any entry, so a thread that ever touched
    /// mail or calendar stays private; an unreadable ledger counts as private),
    /// or private tools on an earlier reply in the session.
    fn is_private(&self, session: &str, tools: &[String]) -> bool {
        if reply_in_dm(tools) {
            return true;
        }
        match private_ledger::entries(&self.rt.repo_root, session) {
            Ok(entries) if entries.is_empty() => {}
            _ => return true,
        }
        self.rt
            .store
            .chat_session_messages(session)
            .iter()
            .filter_map(|m| m.get("tools").and_then(Value::as_array))
            .flatten()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .any(is_private_tool)
    }

    /// Posts the outcome; `true` when the reply reached the user.
    fn deliver(&self, req: &Request, session: &str, progress: Option<&str>, turn: Turn) -> bool {
        let private = !req.is_dm && self.is_private(session, &turn.tools);
        let text = match turn.result {
            Ok(text) => text,
            Err(e) => {
                let shown = if private {
                    GENERIC_ERROR.to_string()
                } else {
                    short_error(&e)
                };
                let _ = self.replace(req, progress, &shown);
                return false;
            }
        };
        if !private {
            return self.post_parts(&req.channel, Some(&req.thread_ts), progress, &text);
        }
        // Keep why the DM failed: opening and posting fail for different
        // reasons (a missing `im:write` scope, a closed account, a bad user id).
        let failure = match self.final_call(|| self.poster.open_dm(&req.user)) {
            Ok(dm) => {
                if self.post_parts(&dm, None, None, &text) {
                    None
                } else {
                    Some("posting to the DM failed".to_string())
                }
            }
            Err(e) => Some(format!("conversations.open failed: {e}")),
        };
        let sent = failure.is_none();
        let notice = match &failure {
            None => DM_NOTICE.to_string(),
            Some(why) => {
                eprintln!("loopx slack: DM delivery failed: {why}");
                format!("{DM_FAILED} ({why})")
            }
        };
        let _ = self.replace(req, progress, &notice);
        sent
    }

    /// Posts `text` escaped and split for Slack: the first part replaces
    /// `replace_ts` when given, the rest follow as new messages. Escaping comes
    /// first because Slack's length limit counts the escaped text;
    /// [`split_message`] never cuts an entity.
    fn post_parts(
        &self,
        channel: &str,
        thread: Option<&str>,
        replace_ts: Option<&str>,
        text: &str,
    ) -> bool {
        let mut ok = true;
        for (i, part) in split_message(&to_mrkdwn(text)).iter().enumerate() {
            let sent = match replace_ts.filter(|_| i == 0) {
                Some(ts) => self.update_or_post(channel, thread, ts, part),
                None => self
                    .final_call(|| self.poster.post(channel, thread, part))
                    .is_ok(),
            };
            ok &= sent;
        }
        ok
    }

    /// Replaces the progress message (or posts when there is none) with
    /// `text`, escaped (an error line can carry engine or model text).
    fn replace(&self, req: &Request, progress: Option<&str>, text: &str) -> bool {
        let thread = Some(req.thread_ts.as_str());
        let text = &to_mrkdwn(text);
        match progress {
            Some(ts) => self.update_or_post(&req.channel, thread, ts, text),
            None => self
                .final_call(|| self.poster.post(&req.channel, thread, text))
                .is_ok(),
        }
    }

    /// Edits `ts`; when the edit fails for a reason other than the rate limit
    /// (say the message was deleted), posts the text as a new message.
    fn update_or_post(&self, channel: &str, thread: Option<&str>, ts: &str, text: &str) -> bool {
        match self.final_call(|| self.poster.update(channel, ts, text)) {
            Ok(()) => true,
            Err(WebError::RateLimited { .. }) => false,
            Err(_) => self
                .final_call(|| self.poster.post(channel, thread, text))
                .is_ok(),
        }
    }

    /// A final-result call: a 429 waits `retry_after` (1 to 60 s) and retries,
    /// at most [`FINAL_RETRIES`] times.
    fn final_call<T>(&self, f: impl Fn() -> Result<T, WebError>) -> Result<T, WebError> {
        let mut retries = 0;
        loop {
            match f() {
                Err(WebError::RateLimited { retry_after }) if retries < FINAL_RETRIES => {
                    (self.sleep)(Duration::from_secs(retry_after.clamp(1, 60)));
                    retries += 1;
                }
                r => return r,
            }
        }
    }
}

/// Takes a stream batch's tool names and its `Done`.
fn absorb(
    batch: Vec<StreamItem>,
    tools: &mut Vec<String>,
    done: &mut Option<(Option<String>, Option<String>)>,
) {
    for item in batch {
        match item {
            StreamItem::Tool(ev) if ev["kind"] == "start" => {
                if let Some(name) = ev["name"].as_str().filter(|n| !n.is_empty()) {
                    tools.push(name.to_string());
                }
            }
            StreamItem::Done { error, final_text } => *done = Some((error, final_text)),
            _ => {}
        }
    }
}

/// The progress message: the tools started so far, private ones unnamed.
fn progress_text(tools: &[String]) -> String {
    let start = tools.len().saturating_sub(PROGRESS_TOOLS);
    let mut lines = vec![ON_IT.to_string()];
    lines.extend(tools[start..].iter().map(|t| {
        if is_private_tool(t) {
            "• Reading a private source".to_string()
        } else {
            format!("• `{}`", escape_mrkdwn(t))
        }
    }));
    lines.join("\n")
}

/// Turns the agent's Markdown into Slack mrkdwn, escaped: `**bold**` and
/// `__bold__` become `*bold*`, `# Heading` lines become bold lines and
/// `[label](url)` becomes `<url|label>`. Code spans and fenced blocks pass
/// through untouched (apart from escaping).
pub(crate) fn to_mrkdwn(text: &str) -> String {
    static BOLD: OnceLock<Regex> = OnceLock::new();
    static LINK: OnceLock<Regex> = OnceLock::new();
    static HEAD: OnceLock<Regex> = OnceLock::new();
    let bold = BOLD.get_or_init(|| Regex::new(r"\*\*([^*\n]+?)\*\*|__([^_\n]+?)__").unwrap());
    let link = LINK.get_or_init(|| Regex::new(r"\[([^\]\n]+)\]\((https?://[^\s)]+)\)").unwrap());
    let head = HEAD.get_or_init(|| Regex::new(r"^#{1,6}\s+(.+?)\s*#*$").unwrap());
    let mut in_fence = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            lines.push(escape_mrkdwn(line));
            continue;
        }
        if in_fence {
            lines.push(escape_mrkdwn(line));
            continue;
        }
        let line = match head.captures(line) {
            Some(c) => format!("**{}**", &c[1]),
            None => line.to_string(),
        };
        let converted: Vec<String> = line
            .split('`')
            .enumerate()
            .map(|(i, seg)| {
                let seg = escape_mrkdwn(seg);
                if i % 2 == 1 {
                    return seg;
                }
                let seg = link.replace_all(&seg, "<$2|$1>");
                bold.replace_all(&seg, |c: &regex::Captures| {
                    format!("*{}*", c.get(1).or(c.get(2)).map_or("", |m| m.as_str()))
                })
                .into_owned()
            })
            .collect();
        lines.push(converted.join("`"));
    }
    lines.join("\n")
}

#[cfg(test)]
#[path = "executor_tests.rs"]
mod tests;
