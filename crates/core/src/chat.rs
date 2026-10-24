//! The dashboard chat engine: the assistant's prompts and `claude -p` command,
//! the stream-json line parsers, the in-memory reply jobs behind the SSE stream,
//! and the background runner that persists a reply. Mirrors the chat half of
//! `bin/web/dashboard_server.py` (`build_chat_command`, `_run_chat_job`,
//! `_chat_job_*`, `parse_chat_stream_line`, `chat_tool_events_in_stream_line`).
//!
//! The one deliberate difference from Python is the command the assistant is
//! allowed to run: Python's was `python3 <repo>/bin/web/dashboard_server.py
//! chat-tool`, here it is [`ChatCommand::chat_tool`] (default `loopx chat-tool`).
//! With `LOOPX_CHAT_TOOL_COMMAND` set to Python's prefix the generated prompt and
//! argv are byte-identical to Python's.

use crate::messages::{MessageStore, LEGACY_CHAT_SESSION_ID};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

pub const CHAT_MESSAGE_HISTORY_LIMIT: usize = 10;
pub const CHAT_SUBPROCESS_TIMEOUT_SECONDS: u32 = 90;
pub const CHAT_TITLE_TIMEOUT_SECONDS: u32 = 60;
pub const STREAM_IDLE_TIMEOUT_SECONDS: u64 = 15;
/// `_AI_CLI_DISPLAY_NAMES["claude"]`.
pub const AI_CLI_NAME: &str = "Claude Code";

const SYSTEM_PROMPT: &str = include_str!("chat_prompts/system.txt");
const TOOLS_PROMPT: &str = include_str!("chat_prompts/tools.txt");

/// `shlex.quote`.
pub fn shell_quote(s: &str) -> String {
    crate::loops_config::shell_quote(s)
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// How the assistant reaches the chat tools, and how its shell is wrapped.
#[derive(Debug, Clone)]
pub struct ChatCommand {
    /// The command prefix the assistant may run, e.g. `loopx chat-tool`.
    pub chat_tool: String,
    /// The shell wrapper around `timeout N claude ...` (default `zsh -i -l -c`).
    pub wrapper: Vec<String>,
}

impl ChatCommand {
    /// `LOOPX_CHAT_TOOL_COMMAND`, else `<this binary> chat-tool` when this
    /// process is `loopx`, else plain `loopx chat-tool`.
    pub fn from_env() -> Self {
        let chat_tool = std::env::var("LOOPX_CHAT_TOOL_COMMAND")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| {
                let exe = std::env::current_exe().ok().filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("loopx"))
                        && !p.to_string_lossy().contains(' ')
                });
                match exe {
                    Some(p) => format!("{} chat-tool", p.display()),
                    None => "loopx chat-tool".to_string(),
                }
            });
        let wrapper = std::env::var("LOOPX_CHAT_SHELL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .and_then(|v| shlex::split(&v))
            .unwrap_or_else(|| ["zsh", "-i", "-l", "-c"].map(String::from).to_vec());
        Self { chat_tool, wrapper }
    }

    pub fn new(chat_tool: &str) -> Self {
        Self {
            chat_tool: chat_tool.to_string(),
            wrapper: ["zsh", "-i", "-l", "-c"].map(String::from).to_vec(),
        }
    }

    /// `_CHAT_ASSISTANT_SYSTEM_PROMPT`.
    pub fn assistant_system_prompt(&self) -> String {
        SYSTEM_PROMPT.replace("@CMD@", &self.chat_tool)
    }

    /// `build_chat_system_prompt`: the base prompt plus the tools section.
    pub fn system_prompt(&self, tools_description: &str) -> String {
        let tools = TOOLS_PROMPT
            .replace("@CMD@", &self.chat_tool)
            .replace("@TOOLS@", tools_description);
        self.assistant_system_prompt() + &tools
    }

    /// `build_chat_command`.
    pub fn build_command(&self, prompt: &str, tools_description: &str) -> Vec<String> {
        let allowed = format!("Bash({} *)", self.chat_tool);
        let disallowed = "Read Write Edit Grep Glob \
            Bash(git*) Bash(rm*) Bash(curl*) Bash(sh*) Bash(bash*) Bash(zsh*) \
            Bash(python3 -c*) Bash(nc*) Bash(osascript*) Bash(launchctl*) \
            WebFetch WebSearch";
        let claude: Vec<String> = [
            "claude",
            "-p",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--verbose",
            "--safe-mode",
            "--allowedTools",
            &allowed,
            "--disallowedTools",
            disallowed,
            "--append-system-prompt",
            &self.system_prompt(tools_description),
            prompt,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let command = format!(
            "timeout {CHAT_SUBPROCESS_TIMEOUT_SECONDS} {}",
            shell_join(&claude)
        );
        let mut argv = self.wrapper.clone();
        argv.push(command);
        argv
    }

    /// `build_chat_title_command`.
    pub fn build_title_command(&self, user_text: &str, reply_text: &str) -> Vec<String> {
        let clip = |s: &str| s.chars().take(1500).collect::<String>();
        let prompt = format!(
            "Write a short title (3 to 6 words) for a chat that starts with the exchange below. \
             Reply with the title only - no quotes, no trailing punctuation.\n\n\
             User: {}\n\nAssistant: {}",
            clip(user_text),
            clip(reply_text)
        );
        let claude: Vec<String> = [
            "claude",
            "-p",
            "--safe-mode",
            "--disallowedTools",
            "Bash Read Write Edit Grep Glob WebFetch WebSearch",
            &prompt,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut argv = self.wrapper.clone();
        argv.push(format!(
            "timeout {CHAT_TITLE_TIMEOUT_SECONDS} {}",
            shell_join(&claude)
        ));
        argv
    }
}

/// `build_chat_prompt`. `page_context` is `(label, path)` of the nav item the AI
/// side panel was opened on (already resolved from a known key by the caller).
pub fn build_chat_prompt(
    user_text: &str,
    recent: &[Value],
    page_context: Option<(&str, &str)>,
) -> String {
    let context = page_context.map_or_else(String::new, |(label, path)| {
        format!("(The user is viewing the dashboard's {label} page, {path}.)\n\n")
    });
    if recent.is_empty() {
        return context + user_text;
    }
    let mut lines = vec![if context.is_empty() {
        "Recent conversation:".to_string()
    } else {
        format!("{context}Recent conversation:")
    }];
    for m in recent {
        let speaker = if m.get("from").and_then(Value::as_str) == Some("user") {
            "User"
        } else {
            "Assistant"
        };
        let text = match m.get("text") {
            Some(Value::String(s)) => s.clone(),
            Some(v) => crate::pydumps::py_str(v),
            None => String::new(),
        };
        lines.push(format!("{speaker}: {text}"));
    }
    lines.push(String::new());
    lines.push(format!("New message: {user_text}"));
    lines.join("\n")
}

/// `_clean_ai_chat_title`.
pub fn clean_ai_chat_title(raw: &str) -> String {
    let line = raw
        .split(['\n', '\r'])
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)^\s*title\s*:\s*").expect("regex"));
    let line = re.replace(line, "");
    let line = line
        .trim()
        .trim_matches(['"', '\'', '*', '`'])
        .trim()
        .trim_end_matches(['.', '!']);
    if line.is_empty() {
        String::new()
    } else {
        crate::messages::heuristic_chat_title(line)
    }
}

// ----- stream-json parsing -----

/// `parse_chat_stream_line`.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamLine {
    Delta(String),
    Result { text: String, is_error: bool },
}

pub fn parse_chat_stream_line(line: &str) -> Option<StreamLine> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let event: Value = serde_json::from_str(line).ok()?;
    match event.get("type").and_then(Value::as_str) {
        Some("stream_event") => {
            let inner = event.get("event").filter(|v| v.is_object())?;
            if inner.get("type").and_then(Value::as_str) == Some("content_block_delta") {
                let delta = inner.get("delta").filter(|v| v.is_object())?;
                if delta.get("type").and_then(Value::as_str) == Some("text_delta") {
                    return Some(StreamLine::Delta(
                        delta
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ));
                }
            }
            None
        }
        Some("result") => Some(StreamLine::Result {
            text: event
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            is_error: truthy(event.get("is_error")),
        }),
        _ => None,
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn command_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(?:dashboard_server\.py|loopx)\s+chat-tool\s+([a-z-]+)").expect("regex")
    })
}

fn call_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?s)\b(?:dashboard_server\.py|loopx)\s+chat-tool\s+call\s+([a-z0-9_.-]+)\s*(.*)$",
        )
        .expect("regex")
    })
}

fn assistant_blocks<'a>(event: &'a Value, kinds: &[&str]) -> Option<(&'a str, &'a Vec<Value>)> {
    let ty = event.get("type").and_then(Value::as_str)?;
    if !kinds.contains(&ty) {
        return None;
    }
    let content = event.get("message")?.get("content")?.as_array()?;
    Some((ty, content))
}

/// `chat_tool_actions_in_stream_line`.
pub fn chat_tool_actions_in_stream_line(line: &str) -> Vec<String> {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let Some((_, content)) = assistant_blocks(&event, &["assistant"]) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let command = block
            .pointer("/input/command")
            .and_then(Value::as_str)
            .unwrap_or("");
        for c in command_re().captures_iter(command) {
            out.push(c[1].to_string());
        }
    }
    out
}

/// `short_tool_ref`: an issue/MR/PR URL as `project#N`.
pub fn short_tool_ref(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^https?://[^/\s]+/(.+?)/(?:-/)?(?:issues|merge_requests|pull)/(\d+)")
            .expect("regex")
    });
    match re.captures(text) {
        Some(c) => format!("{}#{}", c[1].rsplit('/').next().unwrap_or(""), &c[2]),
        None => text.to_string(),
    }
}

fn clip_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `_tool_args_summary`.
pub fn tool_args_summary(raw: &str) -> String {
    let mut raw = raw.trim();
    let b = raw.as_bytes();
    if b.len() >= 2 && b[0] == b[b.len() - 1] && (b[0] == b'\'' || b[0] == b'"') {
        raw = &raw[1..raw.len() - 1];
    }
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(raw) else {
        return String::new();
    };
    for (repo_key, number_key) in [("project", "iid"), ("repo", "number")] {
        if let (Some(Value::String(repo)), Some(Value::Number(n))) =
            (map.get(repo_key), map.get(number_key))
        {
            if let Some(n) = n.as_i64() {
                let base = repo.rsplit('/').next().unwrap_or("");
                return clip_chars(&format!("{base}#{n}"), 80);
            }
        }
    }
    for item in map.values() {
        match item {
            Value::String(s) => return clip_chars(&short_tool_ref(s), 80),
            Value::Number(n) if n.is_i64() || n.is_u64() => {
                return clip_chars(&short_tool_ref(&n.to_string()), 80)
            }
            _ => {}
        }
    }
    String::new()
}

fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.is_object())
            .map(|p| p.get("text").and_then(Value::as_str).unwrap_or(""))
            .collect(),
        _ => String::new(),
    }
}

/// `chat_tool_events_in_stream_line`: start/result/pending events.
pub fn chat_tool_events_in_stream_line(line: &str) -> Vec<Value> {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let Some((ty, content)) = assistant_blocks(&event, &["assistant", "user"]) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for block in content {
        if !block.is_object() {
            continue;
        }
        let kind = block.get("type").and_then(Value::as_str);
        if ty == "assistant" && kind == Some("tool_use") {
            let command = block
                .pointer("/input/command")
                .and_then(Value::as_str)
                .unwrap_or("");
            let id = block.get("id").and_then(Value::as_str).unwrap_or("");
            if let Some(call) = call_re().captures(command) {
                out.push(json!({"kind": "start", "id": id, "name": &call[1],
                                "summary": tool_args_summary(&call[2])}));
            } else {
                for c in command_re().captures_iter(command) {
                    out.push(json!({"kind": "start", "id": id, "name": &c[1], "summary": ""}));
                }
            }
        } else if ty == "user" && kind == Some("tool_result") {
            let text = tool_result_text(block);
            let data = serde_json::from_str::<Value>(&text).ok();
            let is_err_obj = data
                .as_ref()
                .is_some_and(|d| d.as_object().is_some_and(|o| o.contains_key("error")));
            let ok = !truthy(block.get("is_error")) && !is_err_obj;
            let tid = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            out.push(json!({"kind": "result", "id": tid, "ok": ok}));
            if let Some(d) = data.as_ref().filter(|d| d.is_object()) {
                if d.get("status").and_then(Value::as_str) == Some("pending_approval") {
                    out.push(json!({
                        "kind": "pending", "id": tid,
                        "pending_id": d.get("id").cloned().unwrap_or(json!("")),
                        "tool": d.get("tool").cloned().unwrap_or(json!("")),
                        "summary": d.get("summary").cloned().unwrap_or(json!("")),
                        "args": d.get("args").cloned().unwrap_or(json!({})),
                    }));
                }
            }
        }
    }
    out
}

/// `merge_tool_events`: `[{"name","summary","ok"[,"pending"]}]` in call order.
pub fn merge_tool_events(events: &[Value]) -> Vec<Value> {
    let mut order: Vec<Map<String, Value>> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for e in events {
        let id = e
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match e.get("kind").and_then(Value::as_str) {
            Some("start") => {
                let mut m = Map::new();
                m.insert("name".into(), e["name"].clone());
                m.insert("summary".into(), e["summary"].clone());
                m.insert("ok".into(), Value::Null);
                by_id.insert(id, order.len());
                order.push(m);
            }
            Some("result") => {
                if let Some(&i) = by_id.get(&id) {
                    order[i].insert("ok".into(), e["ok"].clone());
                }
            }
            Some("pending") => {
                if let Some(&i) = by_id.get(&id) {
                    let pending: Map<String, Value> = e
                        .as_object()
                        .into_iter()
                        .flatten()
                        .filter(|(k, _)| k.as_str() != "kind" && k.as_str() != "id")
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    order[i].insert("pending".into(), Value::Object(pending));
                }
            }
            _ => {}
        }
    }
    order.into_iter().map(Value::Object).collect()
}

/// `_CHAT_MUTATING_ACTIONS`.
pub const MUTATING_ACTIONS: [&str; 15] = [
    "daemon-enable",
    "daemon-disable",
    "run-now",
    "run-issue",
    "topic-save",
    "topic-enable",
    "topic-disable",
    "project-save",
    "loop-enable",
    "loop-disable",
    "issue-enable",
    "issue-disable",
    "inbox-enable",
    "inbox-disable",
    "history-delete",
];

/// `_sse_frame`.
pub fn sse_frame(event: &str, data: &Value) -> Vec<u8> {
    format!("event: {event}\ndata: {}\n\n", crate::pydumps::dumps(data)).into_bytes()
}

// ----- reply jobs -----

#[derive(Default)]
struct JobState {
    chunks: Vec<String>,
    events: Vec<Value>,
    done: bool,
    error: Option<String>,
    final_text: Option<String>,
    changed: bool,
}

struct Job {
    state: Mutex<JobState>,
    cond: Condvar,
}

/// What a stream consumer sees, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamItem {
    Chunk(String),
    Tool(Value),
    Changed,
    Done {
        error: Option<String>,
        final_text: Option<String>,
    },
    Idle,
}

/// The in-memory registry of live chat replies (`_CHAT_JOBS`).
#[derive(Clone, Default)]
pub struct ChatJobs {
    jobs: Arc<Mutex<HashMap<String, Arc<Job>>>>,
}

impl ChatJobs {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&self, key: &str) -> Option<Arc<Job>> {
        self.jobs.lock().ok()?.get(key).cloned()
    }

    pub fn create(&self) -> String {
        let key = uuid::Uuid::new_v4().to_string();
        let job = Arc::new(Job {
            state: Mutex::new(JobState::default()),
            cond: Condvar::new(),
        });
        if let Ok(mut m) = self.jobs.lock() {
            m.insert(key.clone(), job);
        }
        key
    }

    pub fn append(&self, key: &str, text: &str) {
        if let Some(job) = self.get(key) {
            if let Ok(mut s) = job.state.lock() {
                s.chunks.push(text.to_string());
            }
            job.cond.notify_all();
        }
    }

    pub fn event(&self, key: &str, event: Value) {
        if let Some(job) = self.get(key) {
            if let Ok(mut s) = job.state.lock() {
                s.events.push(event);
            }
            job.cond.notify_all();
        }
    }

    pub fn mark_changed(&self, key: &str) {
        if let Some(job) = self.get(key) {
            if let Ok(mut s) = job.state.lock() {
                s.changed = true;
            }
        }
    }

    /// `_chat_job_finish`; the job is dropped from the registry 60 s later.
    pub fn finish(&self, key: &str, error: Option<String>, final_text: Option<String>) {
        let Some(job) = self.get(key) else { return };
        if let Ok(mut s) = job.state.lock() {
            s.done = true;
            s.error = error;
            s.final_text = final_text;
        }
        job.cond.notify_all();
        let jobs = self.jobs.clone();
        let key = key.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(60));
            if let Ok(mut m) = jobs.lock() {
                m.remove(&key);
            }
        });
    }

    /// `_iter_chat_job_chunks`: `None` for an unknown key.
    pub fn stream(&self, key: &str) -> Option<JobStream> {
        let job = self.get(key)?;
        Some(JobStream {
            job,
            sent: 0,
            sent_events: 0,
            finished: false,
        })
    }
}

pub struct JobStream {
    job: Arc<Job>,
    sent: usize,
    sent_events: usize,
    finished: bool,
}

impl JobStream {
    /// The next batch of items (blocking up to `idle`); `None` once `Done` was
    /// delivered.
    pub fn next_batch(&mut self, idle: Duration) -> Option<Vec<StreamItem>> {
        if self.finished {
            return None;
        }
        let mut guard = self.job.state.lock().ok()?;
        let mut waited = false;
        if self.sent >= guard.chunks.len() && self.sent_events >= guard.events.len() && !guard.done
        {
            guard = self.job.cond.wait_timeout(guard, idle).ok()?.0;
            waited = true;
        }
        let mut out: Vec<StreamItem> = guard.chunks[self.sent..]
            .iter()
            .cloned()
            .map(StreamItem::Chunk)
            .collect();
        let no_chunks = out.is_empty();
        self.sent = guard.chunks.len();
        let new_events: Vec<Value> = guard.events[self.sent_events..].to_vec();
        self.sent_events = guard.events.len();
        let no_events = new_events.is_empty();
        out.extend(new_events.into_iter().map(StreamItem::Tool));
        if guard.done {
            if guard.changed {
                out.push(StreamItem::Changed);
            }
            out.push(StreamItem::Done {
                error: guard.error.clone(),
                final_text: guard.final_text.clone(),
            });
            self.finished = true;
        } else if waited && no_chunks && no_events {
            out.push(StreamItem::Idle);
        }
        Some(out)
    }
}

/// An item as the SSE frame the browser receives (`None` for `Idle`, which is
/// the `: keepalive` comment).
pub fn stream_item_frame(item: &StreamItem) -> Vec<u8> {
    match item {
        StreamItem::Chunk(t) => sse_frame("chunk", &json!(t)),
        StreamItem::Tool(e) => sse_frame("tool", e),
        StreamItem::Changed => sse_frame("changed", &json!(true)),
        StreamItem::Done { error: Some(e), .. } => sse_frame("error", &json!(e)),
        StreamItem::Done {
            error: None,
            final_text,
        } => sse_frame("done", &json!(final_text.clone().unwrap_or_default())),
        StreamItem::Idle => b": keepalive\n\n".to_vec(),
    }
}

// ----- the background runner -----

/// Everything a reply thread needs; paths are injected so tests use tempdirs.
#[derive(Clone)]
pub struct ChatRuntime {
    pub repo_root: PathBuf,
    pub store: MessageStore,
    pub command: ChatCommand,
    pub jobs: ChatJobs,
    /// `registry.describe_all()` for the system prompt.
    pub tools_description: Arc<dyn Fn() -> String + Send + Sync>,
    /// `pending.lookup(id, session)`.
    #[allow(clippy::type_complexity)]
    pub pending_lookup: Arc<dyn Fn(&str, &str) -> Option<Value> + Send + Sync>,
    /// Extra environment for the assistant process (tests put a fake `claude` on PATH).
    pub env: Vec<(String, String)>,
}

/// `append_unified_log` for `logs/loop-engineering.log` (local time).
pub fn append_unified_log(repo_root: &Path, source: &str, detail: &str, body: Option<&str>) {
    use std::io::Write;
    let path = crate::runner_util::unified_log_path(repo_root);
    let header = format!(
        "[{}] ---- {source} ---- {detail} ----\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(header.as_bytes());
        if let Some(b) = body.filter(|b| !b.is_empty()) {
            let _ = f.write_all(b.as_bytes());
            if !b.ends_with('\n') {
                let _ = f.write_all(b"\n");
            }
        }
    }
}

/// `_verified_pending_event`.
fn verified_pending_event(rt: &ChatRuntime, event: &Value, session_id: &str) -> Option<Value> {
    let pending_id = event.get("pending_id")?.as_str()?;
    let record = (rt.pending_lookup)(pending_id, session_id)?;
    Some(json!({
        "kind": "pending",
        "id": event.get("id").cloned().unwrap_or(json!("")),
        "pending_id": pending_id,
        "tool": record.get("tool").cloned().unwrap_or(json!("")),
        "summary": record.get("summary").cloned().unwrap_or(json!("")),
        "args": record.get("args").cloned().unwrap_or(json!({})),
    }))
}

/// `_run_chat_job`: runs `claude -p`, streams deltas into the job, persists a
/// successful reply, and always finishes the job exactly once.
pub fn run_chat_job(rt: &ChatRuntime, reply_key: &str, prompt: &str, session_id: &str) {
    let root = &rt.repo_root;
    append_unified_log(
        root,
        "chat-assistant",
        &format!("turn started ({AI_CLI_NAME})"),
        None,
    );
    let argv = rt.command.build_command(prompt, &(rt.tools_description)());
    let (program, rest) = argv.split_first().expect("non-empty argv");
    let spawned = Command::new(program)
        .args(rest)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .envs(rt.env.iter().map(|(k, v)| (k, v)))
        .env("LOOP_CHAT_SESSION", session_id)
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("Could not start assistant: {e}");
            append_unified_log(
                root,
                "chat-assistant",
                &format!("error ({AI_CLI_NAME})"),
                Some(&msg),
            );
            rt.jobs.finish(reply_key, Some(msg), None);
            return;
        }
    };
    let mut finish_error: Option<String> = None;
    let mut finish_text: Option<String> = None;
    let outcome: Result<(), String> = (|| {
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut final_text: Option<String> = None;
        let mut final_is_error = false;
        let mut tool_events: Vec<Value> = Vec::new();
        let mut reader = BufReader::new(stdout);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            let n = reader
                .read_until(b'\n', &mut raw)
                .map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            let line = String::from_utf8_lossy(&raw).into_owned();
            for ev in chat_tool_events_in_stream_line(&line) {
                let ev = if ev.get("kind").and_then(Value::as_str) == Some("pending") {
                    match verified_pending_event(rt, &ev, session_id) {
                        Some(v) => v,
                        None => continue,
                    }
                } else {
                    ev
                };
                rt.jobs.event(reply_key, ev.clone());
                tool_events.push(ev);
            }
            if chat_tool_actions_in_stream_line(&line)
                .iter()
                .any(|a| MUTATING_ACTIONS.contains(&a.as_str()))
            {
                rt.jobs.mark_changed(reply_key);
            }
            match parse_chat_stream_line(&line) {
                Some(StreamLine::Delta(t)) => rt.jobs.append(reply_key, &t),
                Some(StreamLine::Result { text, is_error }) => {
                    final_text = Some(text);
                    final_is_error = is_error;
                }
                None => {}
            }
        }
        let _ = child.wait();
        match final_text.filter(|t| !t.is_empty()) {
            Some(text) if !final_is_error => {
                let tools = merge_tool_events(&tool_events);
                rt.store.append_message(
                    "loop",
                    &text,
                    Some(session_id),
                    Some(&Value::Array(tools)),
                );
                finish_text = Some(text.clone());
                maybe_title_chat_session(rt, session_id, &text);
            }
            Some(text) => finish_error = Some(text),
            None => finish_error = Some("The assistant didn't return a reply.".to_string()),
        }
        Ok(())
    })();
    if let Err(e) = outcome {
        let _ = child.kill();
        let _ = child.wait();
        finish_error = Some(format!("Error while reading assistant output: {e}"));
    }
    match &finish_error {
        Some(e) => append_unified_log(
            root,
            "chat-assistant",
            &format!("error ({AI_CLI_NAME})"),
            Some(e),
        ),
        None => append_unified_log(
            root,
            "chat-assistant",
            &format!("reply ({AI_CLI_NAME})"),
            finish_text.as_deref(),
        ),
    }
    rt.jobs.finish(reply_key, finish_error, finish_text);
}

/// `_maybe_title_chat_session`: after a session's first reply, ask for an AI
/// title in the background.
fn maybe_title_chat_session(rt: &ChatRuntime, session_id: &str, reply_text: &str) {
    if session_id.is_empty() || session_id == LEGACY_CHAT_SESSION_ID {
        return;
    }
    let sessions = rt.store.read_chat_sessions();
    let Some(record) = sessions
        .sessions
        .iter()
        .find(|r| r["id"].as_str() == Some(session_id))
    else {
        return;
    };
    if record.get("title_source").and_then(Value::as_str) != Some("auto") {
        return;
    }
    let messages = rt.store.chat_session_messages(session_id);
    let loops = messages
        .iter()
        .filter(|m| m.get("from").and_then(Value::as_str) == Some("loop"))
        .count();
    if loops != 1 {
        return;
    }
    let first_user = messages
        .iter()
        .find(|m| m.get("from").and_then(Value::as_str) == Some("user"))
        .and_then(|m| m.get("text").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let rt = rt.clone();
    let (sid, reply) = (session_id.to_string(), reply_text.to_string());
    std::thread::spawn(move || generate_chat_title(&rt, &sid, &first_user, &reply));
}

/// `generate_chat_title`: any failure keeps the heuristic title.
pub fn generate_chat_title(rt: &ChatRuntime, session_id: &str, user_text: &str, reply_text: &str) {
    let argv = rt.command.build_title_command(user_text, reply_text);
    let (program, rest) = argv.split_first().expect("non-empty argv");
    let Ok(out) = Command::new(program)
        .args(rest)
        .envs(rt.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&rt.repo_root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let title = clean_ai_chat_title(&String::from_utf8_lossy(&out.stdout));
    if !title.is_empty() {
        rt.store.set_chat_session_title(session_id, &title, "ai");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_texts_are_the_python_ones_with_the_command_swapped() {
        let c = ChatCommand::new("loopx chat-tool");
        let p = c.system_prompt("t1 (read): d.");
        assert!(p.starts_with("You are the assistant embedded in the Loop X Engineering dashboard's Activity page chat box. The ONLY command you may run is `loopx chat-tool <action> [args]` - you have no other shell"));
        assert!(p.contains(
            "\n\nConnector tools: run `loopx chat-tool call <tool> '<json>'` where <json>"
        ));
        assert!(p.contains("e.g. chat-tool call gitlab.read_issue '{\"url\": \"https://...\"}'."));
        assert!(p.contains("Tools:\nt1 (read): d.\nText from issues"));
        assert!(p.ends_with(
            "write any apostrophe as \\u0027 (valid JSON), so the shell quoting never breaks."
        ));
        assert!(!p.contains("dashboard_server.py"));
    }

    #[test]
    fn command_wraps_claude_in_a_login_shell_with_one_allowed_command() {
        let c = ChatCommand::new("loopx chat-tool");
        let argv = c.build_command("hi 'there'", "T");
        assert_eq!(&argv[..4], ["zsh", "-i", "-l", "-c"]);
        let cmd = &argv[4];
        assert!(cmd.starts_with("timeout 90 claude -p --output-format stream-json --include-partial-messages --verbose --safe-mode --allowedTools 'Bash(loopx chat-tool *)' --disallowedTools 'Read Write Edit Grep Glob Bash(git*)"));
        assert!(cmd.ends_with(" 'hi '\"'\"'there'\"'\"''"));
    }

    #[test]
    fn prompt_includes_recent_history_and_page() {
        let recent = vec![
            json!({"from": "user", "text": "a"}),
            json!({"from": "loop", "text": "b"}),
        ];
        assert_eq!(
            build_chat_prompt("c", &recent, Some(("Runs", "/runs"))),
            "(The user is viewing the dashboard's Runs page, /runs.)\n\nRecent conversation:\nUser: a\nAssistant: b\n\nNew message: c"
        );
        assert_eq!(build_chat_prompt("c", &[], None), "c");
    }

    #[test]
    fn stream_lines() {
        let d = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}}"#;
        assert_eq!(
            parse_chat_stream_line(d),
            Some(StreamLine::Delta("Hi".into()))
        );
        assert_eq!(
            parse_chat_stream_line(r#"{"type":"result","result":"ok","is_error":false}"#),
            Some(StreamLine::Result {
                text: "ok".into(),
                is_error: false
            })
        );
        assert_eq!(
            parse_chat_stream_line(r#"{"type":"stream_event","event":null}"#),
            None
        );
        assert_eq!(parse_chat_stream_line("garbage"), None);
    }

    #[test]
    fn tool_events_for_both_command_flavours() {
        let line = json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": "t1", "input": {"command":
              "loopx chat-tool call gitlab.read_issue '{\"url\": \"https://gl.example.com/g/proj/-/issues/7\"}'"}},
            {"type": "tool_use", "id": "t2", "input": {"command":
              "python3 /x/bin/web/dashboard_server.py chat-tool run-now gitlab"}}
        ]}})
        .to_string();
        let ev = chat_tool_events_in_stream_line(&line);
        assert_eq!(
            ev[0],
            json!({"kind":"start","id":"t1","name":"gitlab.read_issue","summary":"proj#7"})
        );
        assert_eq!(
            ev[1],
            json!({"kind":"start","id":"t2","name":"run-now","summary":""})
        );
        assert_eq!(chat_tool_actions_in_stream_line(&line), ["call", "run-now"]);
        let res = json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1",
            "content": [{"text": "{\"status\": \"pending_approval\", \"id\": \"abc\", \"tool\": \"gitlab.comment\", \"summary\": \"s\", \"args\": {\"a\": 1}}"}]}]}})
        .to_string();
        let ev2 = chat_tool_events_in_stream_line(&res);
        assert_eq!(ev2[0], json!({"kind":"result","id":"t1","ok":true}));
        assert_eq!(ev2[1]["pending_id"], "abc");
        let mut all = ev;
        all.extend(ev2);
        let merged = merge_tool_events(&all);
        assert_eq!(merged[0]["ok"], true);
        assert_eq!(merged[0]["pending"]["tool"], "gitlab.comment");
        assert!(merged[1]["ok"].is_null());
    }

    #[test]
    fn args_summary_variants() {
        assert_eq!(
            tool_args_summary(r#"'{"project": "g/p", "iid": 4}'"#),
            "p#4"
        );
        assert_eq!(tool_args_summary(r#"{"q": "invoices"}"#), "invoices");
        assert_eq!(tool_args_summary("nope"), "");
    }

    #[test]
    fn job_stream_replays_then_finishes() {
        let jobs = ChatJobs::new();
        let k = jobs.create();
        jobs.append(&k, "he");
        jobs.append(&k, "llo");
        jobs.mark_changed(&k);
        jobs.finish(&k, None, Some("hello".into()));
        let mut s = jobs.stream(&k).unwrap();
        let items = s.next_batch(Duration::from_millis(10)).unwrap();
        assert_eq!(
            items,
            vec![
                StreamItem::Chunk("he".into()),
                StreamItem::Chunk("llo".into()),
                StreamItem::Changed,
                StreamItem::Done {
                    error: None,
                    final_text: Some("hello".into())
                },
            ]
        );
        assert!(s.next_batch(Duration::from_millis(10)).is_none());
        assert!(jobs.stream("unknown").is_none());
    }

    #[test]
    fn idle_wakeups_become_keepalives() {
        let jobs = ChatJobs::new();
        let k = jobs.create();
        let mut s = jobs.stream(&k).unwrap();
        assert_eq!(
            s.next_batch(Duration::from_millis(10)).unwrap(),
            vec![StreamItem::Idle]
        );
        assert_eq!(stream_item_frame(&StreamItem::Idle), b": keepalive\n\n");
        assert_eq!(
            stream_item_frame(&StreamItem::Chunk("a\nb".into())),
            b"event: chunk\ndata: \"a\\nb\"\n\n"
        );
    }

    #[test]
    fn ai_titles_are_cleaned() {
        assert_eq!(
            clean_ai_chat_title("Title: \"Weekly digest setup.\"\nextra"),
            "Weekly digest setup"
        );
        assert_eq!(clean_ai_chat_title("  \n"), "");
    }

    fn runtime(dir: &Path, bin: &Path) -> ChatRuntime {
        let mut command = ChatCommand::new("loopx chat-tool");
        command.wrapper = vec!["sh".into(), "-c".into()];
        let old = std::env::var("PATH").unwrap_or_default();
        ChatRuntime {
            repo_root: dir.to_path_buf(),
            store: MessageStore::for_root(dir),
            command,
            jobs: ChatJobs::new(),
            tools_description: Arc::new(|| "tools".to_string()),
            pending_lookup: Arc::new(|_, _| None),
            env: vec![("PATH".into(), format!("{}:{old}", bin.display()))],
        }
    }

    /// A fake `claude` (and `timeout`, which macOS lacks) in a scratch bin dir.
    fn fake_claude(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("fakebin");
        std::fs::create_dir_all(&bin).unwrap();
        for (name, body) in [("claude", script), ("timeout", "shift\nexec \"$@\"")] {
            let p = bin.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        bin
    }

    #[test]
    fn a_successful_reply_is_streamed_and_persisted() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_claude(
            d.path(),
            r#"echo '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hel"}}}'
echo '{"type":"result","result":"Hello there","is_error":false}'"#,
        );
        let rt = runtime(d.path(), &bin);
        let sid = rt.store.send_chat_message("hi", None).unwrap();
        let key = rt.jobs.create();
        run_chat_job(&rt, &key, "hi", &sid);
        let mut s = rt.jobs.stream(&key).unwrap();
        let items = s.next_batch(Duration::from_millis(10)).unwrap();
        assert_eq!(items[0], StreamItem::Chunk("Hel".into()));
        assert_eq!(
            items.last().unwrap(),
            &StreamItem::Done {
                error: None,
                final_text: Some("Hello there".into())
            }
        );
        let msgs = rt.store.read_messages();
        assert_eq!(msgs.last().unwrap()["text"], "Hello there");
        assert!(d.path().join("logs/loop-engineering.log").is_file());
    }

    #[test]
    fn a_failed_reply_persists_nothing() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_claude(
            d.path(),
            r#"echo '{"type":"result","result":"boom","is_error":true}'"#,
        );
        let rt = runtime(d.path(), &bin);
        let sid = rt.store.send_chat_message("hi", None).unwrap();
        let key = rt.jobs.create();
        run_chat_job(&rt, &key, "hi", &sid);
        let mut s = rt.jobs.stream(&key).unwrap();
        let items = s.next_batch(Duration::from_millis(10)).unwrap();
        assert_eq!(
            items.last().unwrap(),
            &StreamItem::Done {
                error: Some("boom".into()),
                final_text: None
            }
        );
        assert_eq!(rt.store.read_messages().len(), 1);
    }
}
