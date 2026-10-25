//! The dashboard chat engine: the assistant's prompts and the selected AI CLI's
//! command (`claude -p` by default), the stream-json line parsers, the in-memory
//! reply jobs behind the SSE stream, and the background runner that persists a
//! reply. Mirrors the chat half of
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

const SYSTEM_PROMPT: &str = include_str!("chat_prompts/system.txt");
const TOOLS_PROMPT: &str = include_str!("chat_prompts/tools.txt");

/// Where every chat error tells the user to go.
const SWITCH_HINT: &str = "Switch CLI in Settings -> AI CLI.";
/// Cursor Agent's workspace for the chat, relative to the repo root (the
/// chat's cwd). It holds only `.cursor/cli.json`, the permission list that
/// allows the chat-tool command and denies file reads and writes.
pub const CURSOR_CHAT_WORKSPACE: &str = "outputs/chat-cursor";
/// Codex's working root for the chat, relative to the repo root: an empty
/// scratch dir, so `workspace-write` makes nothing but it and the repo's
/// `outputs/` writable (the repo root may be the loopx home itself).
pub const CODEX_CHAT_WORKSPACE: &str = "outputs/chat-codex";
/// The machine-readable streaming format of Cursor Agent and Kimi CLI.
const STREAM_JSON: &str = "stream-json";
/// Gemini CLI's list of tools that may run without a confirmation; in headless
/// mode with the default approval mode every other confirmable tool is denied.
const GEMINI_ALLOWED_TOOLS_FLAG: &str = "--allowed-tools";

/// `_AI_CLI_DISPLAY_NAMES` in `crates/server/assets/constants.json`.
pub fn chat_cli_name(cli: &str) -> &'static str {
    match cli {
        "codex" => "Codex CLI",
        "gemini" => "Gemini CLI",
        "kimi" => "Kimi CLI",
        "cursor" => "Cursor CLI",
        _ => "Claude Code",
    }
}

/// `cli` when it is one of `ai_cli_config::VALID_CLIS`, else `"claude"` (what
/// [`ChatCommand::build_command`] runs for an unknown name).
pub fn chat_cli(cli: &str) -> &'static str {
    crate::ai_cli_config::VALID_CLIS
        .iter()
        .find(|c| **c == cli)
        .copied()
        .unwrap_or("claude")
}

/// Whether a CLI's reply arrives as it is written or all at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatMode {
    Streaming,
    /// One complete reply per turn: the CLI has no stream we can parse.
    SingleReply,
}

impl ChatMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ChatMode::Streaming => "streaming",
            ChatMode::SingleReply => "single_reply",
        }
    }
}

/// Claude, Cursor and Kimi stream JSON lines; Gemini's JSON is one object and
/// `codex exec` prints its last message at the end.
pub fn chat_mode(cli: &str) -> ChatMode {
    match cli {
        "gemini" | "codex" => ChatMode::SingleReply,
        _ => ChatMode::Streaming,
    }
}

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
    /// The AI CLI that answers (`ai_cli_config::VALID_CLIS`).
    pub cli: String,
    /// Directories besides its scratch workspace that Codex's sandbox may
    /// write; [`run_chat_job`] sets it to the repo's `outputs/` (pending
    /// approvals, chat docs). Deliberately never the loopx home: the model must
    /// not be able to edit `chat_tools.json` (auto-approve), `projects.json` or
    /// `ai_cli.json`. The `loopx chat-tool` it runs inherits the sandbox, so the
    /// chat-tool actions that write the loopx home directly, without an
    /// approval (topic/loop/inbox/issue enable and disable, project save), fail
    /// under Codex; its chat warning tells the user to use Settings instead.
    pub writable_roots: Vec<String>,
}

impl ChatCommand {
    /// `LOOPX_CHAT_TOOL_COMMAND`, else `<this binary> chat-tool` when this
    /// process is `loopx`, else plain `loopx chat-tool`. The CLI is the one
    /// selected in Settings -> AI CLI.
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
        Self {
            chat_tool,
            wrapper,
            cli: chat_cli(&crate::ai_cli_config::selected_cli()).to_string(),
            writable_roots: Vec::new(),
        }
    }

    /// A Claude chat.
    pub fn new(chat_tool: &str) -> Self {
        Self {
            chat_tool: chat_tool.to_string(),
            wrapper: ["zsh", "-i", "-l", "-c"].map(String::from).to_vec(),
            cli: "claude".to_string(),
            writable_roots: Vec::new(),
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

    /// `build_chat_command` for the selected CLI. Only Claude can be held to
    /// the one chat-tool command (`--allowedTools`); the others get the
    /// strongest fence their headless mode offers and the system prompt ahead
    /// of the user's prompt, since none takes an appended system prompt.
    pub fn build_command(&self, prompt: &str, tools_description: &str) -> Vec<String> {
        if !self.is_claude() {
            let prompt = format!("{}\n\n{prompt}", self.system_prompt(tools_description));
            return self.wrap(CHAT_SUBPROCESS_TIMEOUT_SECONDS, &self.chat_args(&prompt));
        }
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
        if !self.is_claude() {
            return self.wrap(CHAT_TITLE_TIMEOUT_SECONDS, &self.title_args(&prompt));
        }
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

    fn is_claude(&self) -> bool {
        !matches!(self.cli.as_str(), "codex" | "gemini" | "kimi" | "cursor")
    }

    /// `<wrapper> 'timeout N <program> <args>'`. Cursor's program is looked up
    /// by the login shell, since it ships as `agent` or `cursor-agent`.
    fn wrap(&self, timeout: u32, args: &[String]) -> Vec<String> {
        let program = match self.cli.as_str() {
            "cursor" => {
                let lookups: Vec<String> = crate::ai_cli_config::binary_candidates("cursor")
                    .iter()
                    .map(|n| format!("command -v {n}"))
                    .collect();
                format!("\"$({} || echo agent)\"", lookups.join(" || "))
            }
            cli => shell_quote(cli),
        };
        let mut argv = self.wrapper.clone();
        argv.push(format!("timeout {timeout} {program} {}", shell_join(args)));
        argv
    }

    /// The chat turn's argv after the program, for a CLI other than Claude.
    fn chat_args(&self, prompt: &str) -> Vec<String> {
        use crate::agents::{cursor, gemini, kimi, sealed};
        let strs = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match self.cli.as_str() {
            "gemini" => {
                let allowed = format!("run_shell_command({})", self.chat_tool);
                strs(&[
                    gemini::GEMINI_OUTPUT_FORMAT_FLAG,
                    gemini::GEMINI_OUTPUT_FORMAT,
                    sealed::GEMINI_APPROVAL_MODE_FLAG,
                    sealed::GEMINI_APPROVAL_MODE_DEFAULT,
                    GEMINI_ALLOWED_TOOLS_FLAG,
                    &allowed,
                    gemini::GEMINI_PROMPT_FLAG,
                    prompt,
                ])
            }
            "kimi" => strs(&[
                sealed::KIMI_PRINT_FLAG,
                kimi::KIMI_OUTPUT_FORMAT_FLAG,
                STREAM_JSON,
                kimi::KIMI_PROMPT_FLAG,
                prompt,
            ]),
            // No `--force`: only what the workspace's permission list allows runs.
            "cursor" => strs(&[
                cursor::CURSOR_PRINT_FLAG,
                cursor::CURSOR_OUTPUT_FORMAT_FLAG,
                STREAM_JSON,
                cursor::CURSOR_WORKSPACE_FLAG,
                CURSOR_CHAT_WORKSPACE,
                prompt,
            ]),
            // The sandbox lets the model write only its scratch workspace and
            // `writable_roots` (the repo's outputs/); `--ephemeral` keeps the
            // session, and any tool output in it, off disk.
            _ => {
                let mut args = strs(&[
                    "exec",
                    "--cd",
                    CODEX_CHAT_WORKSPACE,
                    "--sandbox",
                    "workspace-write",
                    "--skip-git-repo-check",
                    "--ephemeral",
                    "-c",
                    "approval_policy=never",
                    "-c",
                    "sandbox_workspace_write.network_access=true",
                ]);
                if !self.writable_roots.is_empty() {
                    args.push("-c".into());
                    args.push(format!(
                        "sandbox_workspace_write.writable_roots={}",
                        json!(self.writable_roots)
                    ));
                }
                args.push(prompt.to_string());
                args
            }
        }
    }

    /// The title call's argv after the program: each CLI's read-only one-shot.
    fn title_args(&self, prompt: &str) -> Vec<String> {
        use crate::agents::{cursor, gemini, kimi, sealed};
        let strs = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match self.cli.as_str() {
            "gemini" => strs(&[
                gemini::GEMINI_OUTPUT_FORMAT_FLAG,
                gemini::GEMINI_OUTPUT_FORMAT,
                sealed::GEMINI_APPROVAL_MODE_FLAG,
                sealed::GEMINI_APPROVAL_MODE_DEFAULT,
                gemini::GEMINI_PROMPT_FLAG,
                prompt,
            ]),
            "kimi" => strs(&[
                sealed::KIMI_PRINT_FLAG,
                kimi::KIMI_OUTPUT_FORMAT_FLAG,
                kimi::KIMI_OUTPUT_FORMAT,
                kimi::KIMI_PROMPT_FLAG,
                prompt,
            ]),
            "cursor" => strs(&[
                cursor::CURSOR_PRINT_FLAG,
                sealed::CURSOR_MODE_FLAG,
                sealed::CURSOR_MODE_ASK,
                cursor::CURSOR_OUTPUT_FORMAT_FLAG,
                cursor::CURSOR_OUTPUT_FORMAT,
                prompt,
            ]),
            _ => strs(&[
                "exec",
                "--sandbox",
                "read-only",
                "--skip-git-repo-check",
                "--ephemeral",
                "-c",
                "approval_policy=never",
                prompt,
            ]),
        }
    }

    /// The title text in a title call's stdout (`None` when unreadable).
    fn title_text(&self, stdout: &str) -> Option<String> {
        match self.cli.as_str() {
            "gemini" => crate::agents::gemini::gemini_text(stdout),
            "cursor" => crate::agents::cursor::cursor_text(stdout),
            _ => Some(stdout.to_string()),
        }
    }
}

/// Cursor Agent's chat workspace under `repo_root` with a permission list
/// that lets the shell run only the chat-tool command (by its program name,
/// which is how Cursor matches `Shell(...)`) and denies reading or writing files.
fn prepare_cursor_workspace(repo_root: &Path, chat_tool: &str) -> std::io::Result<()> {
    let program = chat_tool.split_whitespace().next().unwrap_or("loopx");
    let mut allow = vec![format!("Shell({program})")];
    if let Some(base) = Path::new(program).file_name().map(|n| n.to_string_lossy()) {
        if base != program {
            allow.push(format!("Shell({base})"));
        }
    }
    real_workspace_dir(repo_root, CURSOR_CHAT_WORKSPACE)?;
    let dir = real_workspace_dir(repo_root, &format!("{CURSOR_CHAT_WORKSPACE}/.cursor"))?;
    let perms = json!({"permissions": {"allow": allow, "deny": ["Read(**)", "Write(**)"]}});
    // Written aside and renamed, so a concurrent turn never reads half a file.
    let tmp = dir.join(format!("cli.json.{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&perms).unwrap_or_default(),
    )?;
    std::fs::rename(&tmp, dir.join("cli.json")).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// `root/rel` as a real directory inside `root/outputs`. The model can write
/// `outputs/`, so a turn may have planted a symlink (say to the loopx home) or
/// a file at a workspace path for the next turn to use: a symlink or file there
/// is removed (never followed) and replaced by a real directory, and a path that
/// still resolves outside `outputs/` is an error.
fn real_workspace_dir(root: &Path, rel: &str) -> std::io::Result<PathBuf> {
    let outside = || {
        std::io::Error::other(format!(
            "{rel} resolves outside {}",
            root.join("outputs").display()
        ))
    };
    if !rel.starts_with("outputs/") {
        return Err(outside());
    }
    let path = root.join(rel);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            std::fs::remove_file(&path)?;
            std::fs::create_dir(&path)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(&path)?,
        Err(e) => return Err(e),
    }
    let outputs = std::fs::canonicalize(root.join("outputs"))?;
    if !std::fs::canonicalize(&path)?.starts_with(&outputs) {
        return Err(outside());
    }
    Ok(path)
}

/// The command for one turn: Codex gets the repo's `outputs/` as its only
/// writable root besides its workspace.
fn turn_command(rt: &ChatRuntime) -> ChatCommand {
    let mut command = rt.command.clone();
    if chat_cli(&command.cli) == "codex" {
        command.writable_roots = vec![rt.repo_root.join("outputs").display().to_string()];
    }
    command
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
    /// A whole assistant message (Cursor, Kimi): shown as it arrives, and the
    /// last one is the reply when no `Result` follows.
    Message(String),
    Result {
        text: String,
        is_error: bool,
    },
}

/// [`parse_chat_stream_line`] for the stream of `cli`.
///
/// Cursor Agent's `stream-json` is Claude-like: `assistant` events carry a
/// message's text blocks and a final `result` event the reply. Kimi CLI's is
/// one chat message per line (`{"role": "assistant", "content": ...}`) with no
/// final event. Both shapes come from the CLIs' docs, not a real binary.
pub fn parse_stream_line_for(cli: &str, line: &str) -> Option<StreamLine> {
    match cli {
        "cursor" => {
            let event: Value = serde_json::from_str(line.trim()).ok()?;
            match event.get("type").and_then(Value::as_str) {
                Some("assistant") => message_text(event.pointer("/message/content")?)
                    .filter(|t| !t.is_empty())
                    .map(StreamLine::Message),
                Some("result") => parse_chat_stream_line(line),
                _ => None,
            }
        }
        "kimi" => {
            let event: Value = serde_json::from_str(line.trim()).ok()?;
            if event.get("role").and_then(Value::as_str) != Some("assistant") {
                return None;
            }
            message_text(event.get("content")?)
                .filter(|t| !t.is_empty())
                .map(StreamLine::Message)
        }
        _ => parse_chat_stream_line(line),
    }
}

/// A message `content`: a string, or the joined `text` of its `text` parts.
fn message_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect(),
        ),
        _ => None,
    }
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
            out.extend(tool_start_events(id, command));
        } else if ty == "user" && kind == Some("tool_result") {
            let text = tool_result_text(block);
            let tid = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            out.extend(tool_result_events(
                tid,
                &text,
                truthy(block.get("is_error")),
            ));
        }
    }
    out
}

/// The `start` events for one shell command the assistant ran.
fn tool_start_events(id: &str, command: &str) -> Vec<Value> {
    if let Some(call) = call_re().captures(command) {
        return vec![json!({"kind": "start", "id": id, "name": &call[1],
                           "summary": tool_args_summary(&call[2])})];
    }
    command_re()
        .captures_iter(command)
        .map(|c| json!({"kind": "start", "id": id, "name": &c[1], "summary": ""}))
        .collect()
}

/// The `result` (and `pending`) events for one command's output.
fn tool_result_events(id: &str, text: &str, is_error: bool) -> Vec<Value> {
    let data = serde_json::from_str::<Value>(text).ok();
    let is_err_obj = data
        .as_ref()
        .is_some_and(|d| d.as_object().is_some_and(|o| o.contains_key("error")));
    let mut out = vec![json!({"kind": "result", "id": id, "ok": !is_error && !is_err_obj})];
    if let Some(d) = data.as_ref().filter(|d| d.is_object()) {
        if d.get("status").and_then(Value::as_str) == Some("pending_approval") {
            out.push(json!({
                "kind": "pending", "id": id,
                "pending_id": d.get("id").cloned().unwrap_or(json!("")),
                "tool": d.get("tool").cloned().unwrap_or(json!("")),
                "summary": d.get("summary").cloned().unwrap_or(json!("")),
                "args": d.get("args").cloned().unwrap_or(json!({})),
            }));
        }
    }
    out
}

/// The shell commands a Cursor or Kimi stream line starts, as `(id, command)`.
///
/// Cursor: `{"type": "tool_call", "subtype": "started", "call_id",
/// "tool_call": {"<kind>ToolCall": {"args": {"command"}}}}`. Kimi: an assistant
/// message's `tool_calls[].function.arguments`, a JSON string with `command`.
fn started_commands(cli: &str, event: &Value) -> Vec<(String, String)> {
    let text = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or("").to_string();
    match cli {
        "cursor" if event.get("type").and_then(Value::as_str) == Some("tool_call") => {
            if event.get("subtype").and_then(Value::as_str) != Some("started") {
                return Vec::new();
            }
            cursor_tool(event)
                .and_then(|t| t.pointer("/args/command"))
                .and_then(Value::as_str)
                .map(|c| vec![(text(event.get("call_id")), c.to_string())])
                .unwrap_or_default()
        }
        "kimi" if event.get("role").and_then(Value::as_str) == Some("assistant") => event
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|call| {
                let args = call.pointer("/function/arguments")?.as_str()?;
                let args: Value = serde_json::from_str(args).ok()?;
                Some((
                    text(call.get("id")),
                    args.get("command")?.as_str()?.to_string(),
                ))
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The one `<kind>ToolCall` object inside a Cursor `tool_call` event.
fn cursor_tool(event: &Value) -> Option<&Value> {
    event.get("tool_call")?.as_object()?.values().next()
}

/// [`chat_tool_events_in_stream_line`] for the stream of `cli`.
pub fn chat_tool_events_for(cli: &str, line: &str) -> Vec<Value> {
    if !matches!(cli, "cursor" | "kimi") {
        return chat_tool_events_in_stream_line(line);
    }
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let mut out: Vec<Value> = started_commands(cli, &event)
        .iter()
        .flat_map(|(id, command)| tool_start_events(id, command))
        .collect();
    if cli == "cursor"
        && event.get("type").and_then(Value::as_str) == Some("tool_call")
        && event.get("subtype").and_then(Value::as_str) == Some("completed")
    {
        let id = event.get("call_id").and_then(Value::as_str).unwrap_or("");
        let result = cursor_tool(&event).and_then(|t| t.get("result"));
        let (stdout, ok) = match result.and_then(|r| r.get("success")) {
            Some(s) => (s.get("stdout").and_then(Value::as_str).unwrap_or(""), true),
            None => ("", false),
        };
        out.extend(tool_result_events(id, stdout, !ok));
    }
    if cli == "kimi" && event.get("role").and_then(Value::as_str) == Some("tool") {
        let id = event
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let text = match event.get("content") {
            Some(c) => message_text(c).unwrap_or_default(),
            None => String::new(),
        };
        out.extend(tool_result_events(id, &text, false));
    }
    out
}

/// [`chat_tool_actions_in_stream_line`] for the stream of `cli`.
pub fn chat_tool_actions_for(cli: &str, line: &str) -> Vec<String> {
    if !matches!(cli, "cursor" | "kimi") {
        return chat_tool_actions_in_stream_line(line);
    }
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    started_commands(cli, &event)
        .iter()
        .flat_map(|(_, command)| {
            command_re()
                .captures_iter(command)
                .map(|c| c[1].to_string())
                .collect::<Vec<_>>()
        })
        .collect()
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
    record: Option<ChatJobRecord>,
}

/// How a reply job runs, for the chat page: the CLI, whether it streams, and
/// the `ai_cli_safety` warning when the CLI cannot be held to the chat tools.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatJobRecord {
    pub cli: String,
    pub chat_mode: ChatMode,
    pub warning: Option<String>,
}

impl ChatJobRecord {
    pub fn for_cli(cli: &str) -> Self {
        Self {
            cli: cli.to_string(),
            chat_mode: chat_mode(cli),
            warning: crate::ai_cli_safety::warning(cli, crate::ai_cli_safety::CallSite::Chat)
                .map(String::from),
        }
    }

    pub fn to_value(&self) -> Value {
        json!({"cli": self.cli, "chat_mode": self.chat_mode.as_str(), "warning": self.warning})
    }
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

    pub fn set_record(&self, key: &str, record: ChatJobRecord) {
        if let Some(job) = self.get(key) {
            if let Ok(mut s) = job.state.lock() {
                s.record = Some(record);
            }
        }
    }

    /// The job's [`ChatJobRecord`], once its runner has started.
    pub fn record(&self, key: &str) -> Option<ChatJobRecord> {
        self.get(key)?.state.lock().ok()?.record.clone()
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

/// `_run_chat_job`: runs the selected CLI, streams its deltas into the job (or,
/// for a [`ChatMode::SingleReply`] CLI, takes its one reply), persists a
/// successful reply, and always finishes the job exactly once. Every failure
/// becomes an error naming the CLI; nothing falls back to another CLI.
pub fn run_chat_job(rt: &ChatRuntime, reply_key: &str, prompt: &str, session_id: &str) {
    let root = &rt.repo_root;
    let turn_start = unix_now();
    let command = turn_command(rt);
    let cli = chat_cli(&command.cli);
    let name = chat_cli_name(cli);
    let record = ChatJobRecord::for_cli(cli);
    let mode = record.chat_mode;
    append_unified_log(
        root,
        "chat-assistant",
        &format!("turn started ({name})"),
        None,
    );
    if let Some(w) = &record.warning {
        append_unified_log(
            root,
            "chat-assistant",
            &format!("warning ({name})"),
            Some(w),
        );
    }
    rt.jobs.set_record(reply_key, record);
    let fail = |msg: String| {
        append_unified_log(
            root,
            "chat-assistant",
            &format!("error ({name})"),
            Some(&msg),
        );
        rt.jobs.finish(reply_key, Some(msg), None);
    };
    let workspace = match cli {
        "cursor" => prepare_cursor_workspace(root, &command.chat_tool),
        "codex" => real_workspace_dir(root, CODEX_CHAT_WORKSPACE).map(|_| ()),
        _ => Ok(()),
    };
    if let Err(e) = workspace {
        return fail(format!(
            "Could not prepare the {name} workspace: {e}. {SWITCH_HINT}"
        ));
    }
    let argv = command.build_command(prompt, &(rt.tools_description)());
    let (program, rest) = argv.split_first().expect("non-empty argv");
    let spawned = Command::new(program)
        .args(rest)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(rt.env.iter().map(|(k, v)| (k, v)))
        .env("LOOP_CHAT_SESSION", session_id)
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return fail(crate::runner_util::could_not_start(cli, program, &e)),
    };
    // Read stderr on the side (only failures show it); a process the CLI left
    // running may hold the pipe open, so the turn never waits long for it.
    let stderr = child.stderr.take();
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut e) = stderr {
            let _ = std::io::Read::read_to_string(&mut e, &mut text);
        }
        let _ = stderr_tx.send(text);
    });
    let mut finish_error: Option<String> = None;
    let mut finish_text: Option<String> = None;
    let outcome: Result<(), String> = (|| {
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut final_text: Option<String> = None;
        let mut final_is_error = false;
        let mut last_message: Option<String> = None;
        let mut unparsed = String::new();
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
            if mode == ChatMode::SingleReply {
                unparsed.push_str(&line);
                continue;
            }
            for ev in chat_tool_events_for(cli, &line) {
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
            if chat_tool_actions_for(cli, &line)
                .iter()
                .any(|a| MUTATING_ACTIONS.contains(&a.as_str()))
            {
                rt.jobs.mark_changed(reply_key);
            }
            match parse_stream_line_for(cli, &line) {
                Some(StreamLine::Delta(t)) => rt.jobs.append(reply_key, &t),
                Some(StreamLine::Message(t)) => {
                    rt.jobs.append(reply_key, &t);
                    last_message = Some(t);
                }
                Some(StreamLine::Result { text, is_error }) => {
                    final_text = Some(text);
                    final_is_error = is_error;
                }
                None if serde_json::from_str::<Value>(line.trim()).is_err() => {
                    unparsed.push_str(&line);
                }
                None => {}
            }
        }
        let code = child.wait().ok().and_then(|s| s.code());
        let stderr = stderr_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_default();
        let reply = match mode {
            ChatMode::SingleReply if code == Some(0) => single_reply(cli, &unparsed),
            ChatMode::SingleReply => Err(cli_failure(cli, code, &stderr, "")),
            ChatMode::Streaming => {
                // A message with no final event is a reply only from a clean
                // exit; after a timeout or a failure it is a partial answer.
                let text = match final_text {
                    Some(t) => Some(t),
                    None if code == Some(0) => last_message,
                    None => None,
                };
                match text.filter(|t| !t.is_empty()) {
                    Some(text) if !final_is_error => Ok(text),
                    Some(text) if cli == "claude" => Err(text),
                    Some(text) => Err(format!(
                        "{name}: {}. {SWITCH_HINT}",
                        text.trim_end().trim_end_matches('.')
                    )),
                    None => Err(cli_failure(cli, code, &stderr, &unparsed)),
                }
            }
        };
        match reply {
            Ok(text) => {
                if mode == ChatMode::SingleReply {
                    rt.jobs.append(reply_key, &text);
                }
                if cli != "claude" {
                    for ev in turn_pending_events(rt, session_id, turn_start, &tool_events) {
                        rt.jobs.event(reply_key, ev.clone());
                        tool_events.push(ev);
                    }
                }
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
            Err(e) => finish_error = Some(e),
        }
        Ok(())
    })();
    if let Err(e) = outcome {
        let _ = child.kill();
        let _ = child.wait();
        finish_error = Some(format!(
            "Error while reading {name} output: {e}. {SWITCH_HINT}"
        ));
    }
    match &finish_error {
        Some(e) => append_unified_log(root, "chat-assistant", &format!("error ({name})"), Some(e)),
        None => append_unified_log(
            root,
            "chat-assistant",
            &format!("reply ({name})"),
            finish_text.as_deref(),
        ),
    }
    rt.jobs.finish(reply_key, finish_error, finish_text);
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Tool events for the pending actions this turn stored that its stream did
/// not show: a single-reply CLI shows no tool calls at all, and a streaming
/// one's tool-call shape may be missed. Without them the reply has no Approve
/// card. Each becomes a start, an ok result and the verified pending record.
fn turn_pending_events(
    rt: &ChatRuntime,
    session_id: &str,
    turn_start: f64,
    seen: &[Value],
) -> Vec<Value> {
    let shown: Vec<&str> = seen
        .iter()
        .filter(|e| e.get("kind").and_then(Value::as_str) == Some("pending"))
        .filter_map(|e| e.get("pending_id").and_then(Value::as_str))
        .collect();
    // Whole seconds of slack: a record's `created` may be truncated.
    let since = turn_start.floor() - 1.0;
    let mut out = Vec::new();
    for pending_id in crate::chat_tools::pending::created_since(&rt.repo_root, session_id, since) {
        if shown.contains(&pending_id.as_str()) {
            continue;
        }
        let id = format!("pending-{pending_id}");
        let probe = json!({"id": id, "pending_id": pending_id});
        let Some(pending) = verified_pending_event(rt, &probe, session_id) else {
            continue;
        };
        let summary = tool_args_summary(&pending["args"].to_string());
        out.push(json!({"kind": "start", "id": id, "name": pending["tool"], "summary": summary}));
        out.push(json!({"kind": "result", "id": id, "ok": true}));
        out.push(pending);
    }
    out
}

/// The reply in a [`ChatMode::SingleReply`] CLI's stdout (it exited 0).
fn single_reply(cli: &str, stdout: &str) -> Result<String, String> {
    let reply = match cli {
        "gemini" => crate::agents::gemini::gemini_reply(stdout),
        _ => crate::agents::non_empty_reply(cli, stdout.trim().to_string()),
    };
    reply.map_err(|e| {
        format!(
            "{}: {}. {SWITCH_HINT}",
            chat_cli_name(cli),
            e.trim_end().trim_end_matches('.')
        )
    })
}

/// Why a CLI gave no reply: the exit code `timeout` or the shell uses for a
/// timeout (124) or a missing command (127), any other failing exit with the
/// end of its stderr, or output that held no reply.
fn cli_failure(cli: &str, code: Option<i32>, stderr: &str, unparsed: &str) -> String {
    use crate::runner_util::{could_not_start, tail, unreadable_output};
    let name = chat_cli_name(cli);
    match code {
        Some(124) => {
            format!("{name} timed out after {CHAT_SUBPROCESS_TIMEOUT_SECONDS} s. {SWITCH_HINT}")
        }
        Some(127) => {
            let program = crate::ai_cli_config::binary_candidates(cli)
                .first()
                .copied()
                .unwrap_or(cli);
            could_not_start(cli, program, &"command not found")
        }
        Some(0) if !unparsed.trim().is_empty() => format!(
            "{}\n{SWITCH_HINT}",
            unreadable_output(cli, tail(unparsed, 500).trim_end())
        ),
        Some(0) => format!("{name} didn't return a reply. {SWITCH_HINT}"),
        Some(n) => match tail(stderr, 500).trim() {
            "" => format!("{name} exited with code {n}. {SWITCH_HINT}"),
            err => format!("{name} exited with code {n}: {err}. {SWITCH_HINT}"),
        },
        None => format!("{name} was stopped by a signal. {SWITCH_HINT}"),
    }
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

/// `generate_chat_title`: best effort on the selected CLI; any failure keeps
/// the heuristic title and never touches the reply.
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
    let Some(text) = rt.command.title_text(&String::from_utf8_lossy(&out.stdout)) else {
        return;
    };
    let title = clean_ai_chat_title(&text);
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
        fake_cli(dir, "claude", script)
    }

    /// A fake CLI binary named `program` (and `timeout`) in a scratch bin dir.
    fn fake_cli(dir: &Path, program: &str, script: &str) -> PathBuf {
        fake_bin(dir, &[(program, script), ("timeout", "shift\nexec \"$@\"")])
    }

    fn fake_bin(dir: &Path, scripts: &[(&str, &str)]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("fakebin");
        std::fs::create_dir_all(&bin).unwrap();
        for (name, body) in scripts {
            let p = bin.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        bin
    }

    /// [`runtime`] with `cli` selected and a `PATH` of only the fake bin dir
    /// plus the system dirs, so no real AI CLI is ever found.
    fn runtime_for(dir: &Path, bin: &Path, cli: &str) -> ChatRuntime {
        let mut rt = runtime(dir, bin);
        rt.command.cli = cli.to_string();
        rt.env = vec![("PATH".into(), format!("{}:/usr/bin:/bin", bin.display()))];
        rt
    }

    fn run_turn(rt: &ChatRuntime) -> (String, Vec<StreamItem>) {
        let sid = rt.store.send_chat_message("hi", None).unwrap();
        let key = rt.jobs.create();
        run_chat_job(rt, &key, "hi", &sid);
        let mut s = rt.jobs.stream(&key).unwrap();
        (key, s.next_batch(Duration::from_millis(10)).unwrap())
    }

    fn done_error(items: &[StreamItem]) -> String {
        match items.last().unwrap() {
            StreamItem::Done {
                error: Some(e),
                final_text: None,
            } => e.clone(),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    fn unified_log(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("logs/loop-engineering.log")).unwrap()
    }

    /// The CLI's argv inside `timeout 90 ...` of the wrapped command.
    fn inner_argv(argv: &[String]) -> Vec<String> {
        let words = shlex::split(argv.last().unwrap()).unwrap();
        assert_eq!(&words[..2], ["timeout", "90"]);
        words[2..].to_vec()
    }

    #[test]
    fn chat_cli_names_follow_the_dashboard_constants() {
        assert_eq!(chat_cli_name("claude"), "Claude Code");
        assert_eq!(chat_cli_name("codex"), "Codex CLI");
        assert_eq!(chat_cli_name("gemini"), "Gemini CLI");
        assert_eq!(chat_cli_name("kimi"), "Kimi CLI");
        assert_eq!(chat_cli_name("cursor"), "Cursor CLI");
        let constants: Value =
            serde_json::from_str(include_str!("../../server/assets/constants.json")).unwrap();
        let names = constants["dashboard_server"]["_AI_CLI_DISPLAY_NAMES"]
            .as_object()
            .unwrap();
        assert!(names.contains_key("claude") && names.contains_key("codex"));
        for (cli, name) in names {
            assert_eq!(chat_cli_name(cli), name.as_str().unwrap(), "{cli}");
        }
    }

    #[test]
    fn chat_modes_stream_only_where_the_cli_streams() {
        for (cli, mode) in [
            ("claude", ChatMode::Streaming),
            ("cursor", ChatMode::Streaming),
            ("kimi", ChatMode::Streaming),
            ("gemini", ChatMode::SingleReply),
            ("codex", ChatMode::SingleReply),
        ] {
            assert_eq!(chat_mode(cli), mode, "{cli}");
        }
        assert_eq!(ChatMode::Streaming.as_str(), "streaming");
        assert_eq!(ChatMode::SingleReply.as_str(), "single_reply");
    }

    #[test]
    fn new_is_a_claude_chat() {
        assert_eq!(ChatCommand::new("loopx chat-tool").cli, "claude");
    }

    #[test]
    fn every_cli_gets_the_system_prompt_and_no_approval_bypass() {
        use crate::ai_cli_safety::{restriction, CallSite, Restriction};
        for cli in ["codex", "gemini", "kimi", "cursor"] {
            let mut c = ChatCommand::new("loopx chat-tool");
            c.cli = cli.into();
            let argv = c.build_command("it's me", "T");
            assert_eq!(&argv[..4], ["zsh", "-i", "-l", "-c"], "{cli}");
            let words = inner_argv(&argv);
            let prompt = words.last().unwrap();
            assert!(prompt.starts_with(&c.system_prompt("T")), "{cli}: {prompt}");
            assert!(prompt.ends_with("it's me"), "{cli}");
            for unsafe_flag in [
                "--yolo",
                "--force",
                "--dangerously-skip-permissions",
                "--dangerously-bypass-approvals-and-sandbox",
                "danger-full-access",
            ] {
                assert!(!words.iter().any(|w| w == unsafe_flag), "{cli}");
            }
            assert!(!words.iter().any(|w| w == "--allowedTools"), "{cli}");
            // None of them can be held to the one command the way claude is.
            assert_eq!(restriction(cli, CallSite::Chat), Restriction::Unrestricted);
        }
    }

    #[test]
    fn each_cli_arm_uses_its_strongest_headless_restriction() {
        let cmd = |cli: &str| {
            let mut c = ChatCommand::new("loopx chat-tool");
            c.cli = cli.into();
            c.writable_roots = vec!["/r/outputs".into()];
            inner_argv(&c.build_command("hi", "T"))
        };
        let g = cmd("gemini");
        assert_eq!(
            &g[..7],
            [
                "gemini",
                "--output-format",
                "json",
                "--approval-mode",
                "default",
                "--allowed-tools",
                "run_shell_command(loopx chat-tool)"
            ]
        );
        assert_eq!(g[7], "-p");
        let k = cmd("kimi");
        assert_eq!(
            &k[..5],
            ["kimi", "--print", "--output-format", "stream-json", "-p"]
        );
        let c = cmd("cursor");
        assert!(c[0].contains("command -v agent"), "{}", c[0]);
        assert_eq!(
            &c[1..6],
            [
                "--print",
                "--output-format",
                "stream-json",
                "--workspace",
                CURSOR_CHAT_WORKSPACE
            ]
        );
        let x = cmd("codex");
        assert_eq!(
            &x[..14],
            [
                "codex",
                "exec",
                "--cd",
                CODEX_CHAT_WORKSPACE,
                "--sandbox",
                "workspace-write",
                "--skip-git-repo-check",
                "--ephemeral",
                "-c",
                "approval_policy=never",
                "-c",
                "sandbox_workspace_write.network_access=true",
                "-c",
                r#"sandbox_workspace_write.writable_roots=["/r/outputs"]"#
            ]
        );
    }

    #[test]
    fn claude_arm_is_unchanged_for_an_explicit_claude() {
        let mut c = ChatCommand::new("loopx chat-tool");
        let default = c.build_command("q", "T");
        c.cli = "claude".into();
        c.writable_roots = vec!["/x".into()];
        assert_eq!(c.build_command("q", "T"), default);
        assert_eq!(
            c.build_title_command("u", "r"),
            ChatCommand::new("loopx chat-tool").build_title_command("u", "r")
        );
    }

    #[test]
    fn title_commands_are_tool_less_one_shots_for_every_cli() {
        for cli in ["codex", "gemini", "kimi", "cursor"] {
            let mut c = ChatCommand::new("loopx chat-tool");
            c.cli = cli.into();
            let argv = c.build_title_command("user asks", "assistant says");
            let words = shlex::split(argv.last().unwrap()).unwrap();
            assert_eq!(&words[..2], ["timeout", "60"], "{cli}");
            let prompt = words.last().unwrap();
            assert!(prompt.starts_with("Write a short title"), "{cli}");
            assert!(prompt.contains("User: user asks"), "{cli}");
            assert!(!words.iter().any(|w| w == "--yolo" || w == "--force"));
            assert!(!prompt.contains("chat-tool"), "{cli}");
        }
    }

    #[test]
    fn cursor_stream_lines_become_deltas_and_a_result() {
        let a = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Hel"}]},"session_id":"s"}"#;
        assert_eq!(
            parse_stream_line_for("cursor", a),
            Some(StreamLine::Message("Hel".into()))
        );
        assert_eq!(
            parse_stream_line_for(
                "cursor",
                r#"{"type":"result","subtype":"success","is_error":false,"result":"Hello"}"#
            ),
            Some(StreamLine::Result {
                text: "Hello".into(),
                is_error: false
            })
        );
        assert_eq!(
            parse_stream_line_for("cursor", r#"{"type":"system","subtype":"init"}"#),
            None
        );
        // Claude's own parser ignores a whole assistant message.
        assert_eq!(parse_stream_line_for("claude", a), None);
    }

    #[test]
    fn kimi_stream_lines_become_messages() {
        assert_eq!(
            parse_stream_line_for("kimi", r#"{"role":"assistant","content":"Hi"}"#),
            Some(StreamLine::Message("Hi".into()))
        );
        assert_eq!(
            parse_stream_line_for(
                "kimi",
                r#"{"role":"assistant","content":[{"type":"think","think":"x"},{"type":"text","text":"Yo"}]}"#
            ),
            Some(StreamLine::Message("Yo".into()))
        );
        assert_eq!(
            parse_stream_line_for("kimi", r#"{"role":"tool","content":"{}"}"#),
            None
        );
        assert_eq!(parse_stream_line_for("kimi", "plain"), None);
    }

    #[test]
    fn cursor_and_kimi_tool_calls_become_tool_events() {
        let started = json!({"type": "tool_call", "subtype": "started", "call_id": "c1",
            "tool_call": {"shellToolCall": {"args": {"command":
                "loopx chat-tool call gitlab.comment '{\"project\": \"g/p\", \"iid\": 3}'"}}}})
        .to_string();
        assert_eq!(
            chat_tool_events_for("cursor", &started),
            [json!({"kind":"start","id":"c1","name":"gitlab.comment","summary":"p#3"})]
        );
        assert_eq!(chat_tool_actions_for("cursor", &started), ["call"]);
        let done = json!({"type": "tool_call", "subtype": "completed", "call_id": "c1",
            "tool_call": {"shellToolCall": {"result": {"success": {"stdout":
                "{\"status\": \"pending_approval\", \"id\": \"p1\", \"tool\": \"gitlab.comment\"}",
                "exitCode": 0}}}}})
        .to_string();
        let ev = chat_tool_events_for("cursor", &done);
        assert_eq!(ev[0], json!({"kind":"result","id":"c1","ok":true}));
        assert_eq!(ev[1]["pending_id"], "p1");

        let call = json!({"role": "assistant", "content": "",
            "tool_calls": [{"type": "function", "id": "k1", "function": {"name": "Shell",
                "arguments": "{\"command\": \"loopx chat-tool run-now gitlab\"}"}}]})
        .to_string();
        assert_eq!(
            chat_tool_events_for("kimi", &call),
            [json!({"kind":"start","id":"k1","name":"run-now","summary":""})]
        );
        assert_eq!(chat_tool_actions_for("kimi", &call), ["run-now"]);
        let result = json!({"role": "tool", "tool_call_id": "k1",
            "content": [{"type": "text", "text": "{\"error\": \"no\"}"}]})
        .to_string();
        assert_eq!(
            chat_tool_events_for("kimi", &result),
            [json!({"kind":"result","id":"k1","ok":false})]
        );
        // Claude's events are untouched by the per-CLI entry point.
        assert!(chat_tool_events_for("claude", &call).is_empty());
    }

    #[test]
    fn a_claude_job_records_streaming_and_no_warning() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_claude(
            d.path(),
            r#"echo '{"type":"result","result":"ok","is_error":false}'"#,
        );
        let rt = runtime(d.path(), &bin);
        let (key, _) = run_turn(&rt);
        let record = rt.jobs.record(&key).unwrap();
        assert_eq!(
            record,
            ChatJobRecord {
                cli: "claude".into(),
                chat_mode: ChatMode::Streaming,
                warning: None
            }
        );
        assert_eq!(
            record.to_value(),
            json!({"cli": "claude", "chat_mode": "streaming", "warning": null})
        );
        let log = unified_log(d.path());
        assert!(log.contains("turn started (Claude Code)"), "{log}");
        assert!(log.contains("reply (Claude Code)"), "{log}");
    }

    #[test]
    fn gemini_gives_one_complete_reply_and_records_the_degraded_mode() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_cli(d.path(), "gemini", r#"echo '{"response":"hi","stats":{}}'"#);
        let rt = runtime_for(d.path(), &bin, "gemini");
        let (key, items) = run_turn(&rt);
        assert_eq!(
            items,
            vec![
                StreamItem::Chunk("hi".into()),
                StreamItem::Done {
                    error: None,
                    final_text: Some("hi".into())
                }
            ]
        );
        assert_eq!(rt.store.read_messages().last().unwrap()["text"], "hi");
        let record = rt.jobs.record(&key).unwrap();
        assert_eq!(record.chat_mode, ChatMode::SingleReply);
        assert_eq!(
            record.warning.as_deref(),
            crate::ai_cli_safety::warning("gemini", crate::ai_cli_safety::CallSite::Chat)
        );
        let log = unified_log(d.path());
        assert!(log.contains("turn started (Gemini CLI)"), "{log}");
        assert!(log.contains("warning (Gemini CLI)"), "{log}");
        assert!(log.contains("reply (Gemini CLI)"), "{log}");
        assert!(!log.contains("Claude Code"), "{log}");
    }

    #[test]
    fn codex_gives_its_stdout_as_one_reply() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_cli(d.path(), "codex", "echo 'all done'");
        let rt = runtime_for(d.path(), &bin, "codex");
        let (_, items) = run_turn(&rt);
        assert_eq!(
            items.last().unwrap(),
            &StreamItem::Done {
                error: None,
                final_text: Some("all done".into())
            }
        );
    }

    #[test]
    fn cursor_streams_deltas_then_the_final_text() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_cli(
            d.path(),
            "agent",
            r#"echo '{"type":"system","subtype":"init"}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"Hel"}]}}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"lo"}]}}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"Hello"}'"#,
        );
        let rt = runtime_for(d.path(), &bin, "cursor");
        let (key, items) = run_turn(&rt);
        assert_eq!(
            items,
            vec![
                StreamItem::Chunk("Hel".into()),
                StreamItem::Chunk("lo".into()),
                StreamItem::Done {
                    error: None,
                    final_text: Some("Hello".into())
                }
            ]
        );
        assert_eq!(rt.jobs.record(&key).unwrap().chat_mode, ChatMode::Streaming);
        let perms: Value = serde_json::from_str(
            &std::fs::read_to_string(
                d.path()
                    .join(CURSOR_CHAT_WORKSPACE)
                    .join(".cursor/cli.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(perms["permissions"]["allow"], json!(["Shell(loopx)"]));
    }

    #[test]
    fn kimi_streams_messages_and_keeps_the_last_as_the_reply() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_cli(
            d.path(),
            "kimi",
            r#"echo '{"role":"assistant","content":"Checking. ","tool_calls":[{"type":"function","id":"k1","function":{"name":"Shell","arguments":"{\"command\": \"loopx chat-tool status\"}"}}]}'
echo '{"role":"tool","tool_call_id":"k1","content":"{}"}'
echo '{"role":"assistant","content":"All green."}'"#,
        );
        let rt = runtime_for(d.path(), &bin, "kimi");
        let (_, items) = run_turn(&rt);
        assert_eq!(items[0], StreamItem::Chunk("Checking. ".into()));
        assert_eq!(items[1], StreamItem::Chunk("All green.".into()));
        assert!(items.contains(&StreamItem::Tool(
            json!({"kind":"start","id":"k1","name":"status","summary":""})
        )));
        assert_eq!(
            items.last().unwrap(),
            &StreamItem::Done {
                error: None,
                final_text: Some("All green.".into())
            }
        );
        let msgs = rt.store.read_messages();
        assert_eq!(msgs.last().unwrap()["text"], "All green.");
        assert_eq!(msgs.last().unwrap()["tools"][0]["name"], "status");
    }

    #[test]
    fn a_failing_cli_is_a_visible_error_naming_it() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_cli(d.path(), "gemini", "echo 'auth expired' >&2; exit 3");
        let rt = runtime_for(d.path(), &bin, "gemini");
        let (_, items) = run_turn(&rt);
        let e = done_error(&items);
        assert!(e.contains("Gemini CLI exited with code 3"), "{e}");
        assert!(e.contains("auth expired"), "{e}");
        assert!(e.ends_with("Switch CLI in Settings -> AI CLI."), "{e}");
        let log = unified_log(d.path());
        assert!(log.contains("error (Gemini CLI)"), "{log}");
        assert!(!log.contains("Claude Code"), "{log}");
        assert_eq!(rt.store.read_messages().len(), 1);
    }

    #[test]
    fn a_missing_cli_says_it_could_not_start_and_how_to_switch() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_bin(d.path(), &[("timeout", "shift\nexec \"$@\"")]);
        let rt = runtime_for(d.path(), &bin, "kimi");
        let (_, items) = run_turn(&rt);
        let e = done_error(&items);
        assert!(e.contains("could not start kimi (kimi CLI)"), "{e}");
        assert!(e.ends_with("Switch CLI in Settings -> AI CLI."), "{e}");
    }

    #[test]
    fn a_timed_out_cli_says_so() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_bin(
            d.path(),
            &[("cursor-agent", "exit 0"), ("timeout", "exit 124")],
        );
        let rt = runtime_for(d.path(), &bin, "cursor");
        let (_, items) = run_turn(&rt);
        let e = done_error(&items);
        assert!(e.contains("Cursor CLI timed out after 90 s"), "{e}");
        assert!(e.ends_with("Switch CLI in Settings -> AI CLI."), "{e}");
    }

    #[test]
    fn unreadable_or_error_output_is_a_visible_error() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_cli(d.path(), "gemini", "echo plain words");
        let rt = runtime_for(d.path(), &bin, "gemini");
        let e = done_error(&run_turn(&rt).1);
        assert!(e.contains("unreadable output from gemini"), "{e}");
        assert!(e.contains("plain words"), "{e}");
        assert!(e.ends_with("Switch CLI in Settings -> AI CLI."), "{e}");

        let d = tempfile::tempdir().unwrap();
        let bin = fake_cli(
            d.path(),
            "gemini",
            r#"echo '{"error":{"message":"quota exceeded"}}'"#,
        );
        let rt = runtime_for(d.path(), &bin, "gemini");
        let e = done_error(&run_turn(&rt).1);
        assert!(e.contains("Gemini CLI: quota exceeded"), "{e}");

        let d = tempfile::tempdir().unwrap();
        let bin = fake_cli(d.path(), "kimi", "echo not json");
        let rt = runtime_for(d.path(), &bin, "kimi");
        let e = done_error(&run_turn(&rt).1);
        assert!(e.contains("unreadable output from kimi"), "{e}");
    }

    #[test]
    fn codex_may_write_only_the_outputs_dir_never_the_loopx_home() {
        let d = tempfile::tempdir().unwrap();
        let mut rt = runtime(d.path(), d.path());
        rt.command.cli = "codex".into();
        rt.command.writable_roots = vec!["/somewhere/else".into()];
        let c = turn_command(&rt);
        let outputs = d.path().join("outputs").display().to_string();
        assert_eq!(c.writable_roots, [outputs.as_str()]);
        let words = inner_argv(&c.build_command("hi", "T"));
        assert!(!words.iter().any(|w| w.contains(".loopx")), "{words:?}");
        assert!(words.contains(&format!(
            "sandbox_workspace_write.writable_roots={}",
            json!([outputs])
        )));
        let i = words.iter().position(|w| w == "--cd").unwrap();
        assert_eq!(words[i + 1], CODEX_CHAT_WORKSPACE);
        // Other CLIs keep the command as configured.
        rt.command.cli = "gemini".into();
        assert_eq!(turn_command(&rt).writable_roots, ["/somewhere/else"]);
    }

    #[test]
    fn a_codex_turn_runs_in_its_scratch_workspace() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_cli(d.path(), "codex", "echo ok");
        let rt = runtime_for(d.path(), &bin, "codex");
        run_turn(&rt);
        assert!(d.path().join(CODEX_CHAT_WORKSPACE).is_dir());
    }

    #[test]
    fn an_unknown_cli_is_a_claude_chat_in_record_and_command() {
        assert_eq!(chat_cli("gpt5"), "claude");
        assert_eq!(chat_cli("kimi"), "kimi");
        let d = tempfile::tempdir().unwrap();
        let bin = fake_claude(
            d.path(),
            r#"echo '{"type":"result","result":"ok","is_error":false}'"#,
        );
        let mut rt = runtime(d.path(), &bin);
        rt.command.cli = "gpt5".into();
        let (key, items) = run_turn(&rt);
        assert_eq!(
            rt.jobs.record(&key).unwrap(),
            ChatJobRecord::for_cli("claude")
        );
        assert_eq!(
            items.last().unwrap(),
            &StreamItem::Done {
                error: None,
                final_text: Some("ok".into())
            }
        );
    }

    #[test]
    fn a_streaming_cli_that_times_out_or_fails_after_a_message_is_an_error() {
        // (timeout fake, kimi's exit, expected error)
        for (timeout, exit, want) in [
            (
                "shift\n\"$@\"\nexit 124",
                "",
                "Kimi CLI timed out after 90 s",
            ),
            (
                "shift\nexec \"$@\"",
                "exit 1",
                "Kimi CLI exited with code 1",
            ),
        ] {
            let d = tempfile::tempdir().unwrap();
            let kimi = format!("echo '{{\"role\":\"assistant\",\"content\":\"partial\"}}'\n{exit}");
            let bin = fake_bin(d.path(), &[("kimi", kimi.as_str()), ("timeout", timeout)]);
            let rt = runtime_for(d.path(), &bin, "kimi");
            let (_, items) = run_turn(&rt);
            assert_eq!(items[0], StreamItem::Chunk("partial".into()));
            let e = done_error(&items);
            assert!(e.contains(want), "{e}");
            assert_eq!(rt.store.read_messages().len(), 1, "{want}");
        }
    }

    #[test]
    fn a_cursor_error_result_names_the_cli_and_the_switch_hint() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_cli(
            d.path(),
            "agent",
            r#"echo '{"type":"result","is_error":true,"result":"auth failed"}'"#,
        );
        let rt = runtime_for(d.path(), &bin, "cursor");
        let e = done_error(&run_turn(&rt).1);
        assert_eq!(
            e,
            "Cursor CLI: auth failed. Switch CLI in Settings -> AI CLI."
        );
    }

    #[test]
    fn the_cursor_permission_file_is_replaced_whole() {
        let d = tempfile::tempdir().unwrap();
        prepare_cursor_workspace(d.path(), "/opt/bin/loopx chat-tool").unwrap();
        prepare_cursor_workspace(d.path(), "loopx chat-tool").unwrap();
        let dir = d.path().join(CURSOR_CHAT_WORKSPACE).join(".cursor");
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["cli.json"]);
        let perms: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("cli.json")).unwrap()).unwrap();
        assert_eq!(perms["permissions"]["allow"], json!(["Shell(loopx)"]));
    }

    /// A fake CLI that, like a write chat-tool call, stores a pending action
    /// for the turn's session, then replies.
    fn pending_writing_script(reply: &str) -> String {
        format!(
            r#"mkdir -p outputs/chat-pending
printf '{{"tool": "gitlab.comment", "args": {{"body": "hi"}}, "session": "%s", "summary": "g/p#1", "created": %s}}' "$LOOP_CHAT_SESSION" "$(date +%s)" > outputs/chat-pending/{NEW_ID}.json
{reply}"#
        )
    }

    const NEW_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OLD_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const OTHER_ID: &str = "cccccccccccccccccccccccccccccccc";

    fn with_real_pending_lookup(mut rt: ChatRuntime) -> ChatRuntime {
        let env = crate::chat_tools::Env::new(&rt.repo_root, rt.repo_root.join("home"));
        rt.pending_lookup = Arc::new(move |id, session| {
            crate::chat_tools::pending::lookup(&env, id, session, None).map(|p| p.to_value())
        });
        rt
    }

    fn seed_pending(root: &Path, id: &str, session: &str, created: f64) {
        let dir = root.join("outputs/chat-pending");
        std::fs::create_dir_all(&dir).unwrap();
        let rec = json!({"tool": "gitlab.comment", "args": {}, "session": session,
                         "summary": "old", "created": created});
        std::fs::write(dir.join(format!("{id}.json")), rec.to_string()).unwrap();
    }

    #[test]
    fn a_single_reply_turn_attaches_the_pending_actions_it_created() {
        let d = tempfile::tempdir().unwrap();
        let script = pending_writing_script(r#"echo '{"response":"Waiting for your approval."}'"#);
        let bin = fake_cli(d.path(), "gemini", &script);
        let rt = with_real_pending_lookup(runtime_for(d.path(), &bin, "gemini"));
        let sid = rt.store.send_chat_message("hi", None).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        seed_pending(d.path(), OLD_ID, &sid, now - 300.0);
        seed_pending(d.path(), OTHER_ID, "another-session", now);
        let key = rt.jobs.create();
        run_chat_job(&rt, &key, "hi", &sid);
        let items = rt
            .jobs
            .stream(&key)
            .unwrap()
            .next_batch(Duration::from_millis(10))
            .unwrap();
        assert!(
            items.iter().any(|i| matches!(i, StreamItem::Tool(e)
                if e["kind"] == "pending" && e["pending_id"] == NEW_ID)),
            "{items:?}"
        );
        let msgs = rt.store.read_messages();
        let tools = msgs.last().unwrap()["tools"].as_array().unwrap().clone();
        assert_eq!(tools.len(), 1, "{tools:?}");
        assert_eq!(tools[0]["name"], "gitlab.comment");
        assert_eq!(tools[0]["ok"], true);
        assert_eq!(
            tools[0]["pending"],
            json!({"pending_id": NEW_ID, "tool": "gitlab.comment", "summary": "g/p#1",
                   "args": {"body": "hi"}})
        );
    }

    #[test]
    fn a_streamed_pending_action_is_not_attached_twice() {
        let d = tempfile::tempdir().unwrap();
        let done = json!({"type": "tool_call", "subtype": "completed", "call_id": "c1",
            "tool_call": {"shellToolCall": {"result": {"success": {"stdout":
                format!("{{\"status\": \"pending_approval\", \"id\": \"{NEW_ID}\"}}")}}}}})
        .to_string();
        let started = json!({"type": "tool_call", "subtype": "started", "call_id": "c1",
            "tool_call": {"shellToolCall": {"args": {"command":
                "loopx chat-tool call gitlab.comment '{}'"}}}})
        .to_string();
        let reply = format!(
            "echo '{started}'\necho '{done}'\necho '{}'",
            r#"{"type":"result","is_error":false,"result":"Approve it."}"#
        );
        let bin = fake_cli(d.path(), "agent", &pending_writing_script(&reply));
        let rt = with_real_pending_lookup(runtime_for(d.path(), &bin, "cursor"));
        run_turn(&rt);
        let msgs = rt.store.read_messages();
        let tools = msgs.last().unwrap()["tools"].as_array().unwrap().clone();
        assert_eq!(tools.len(), 1, "{tools:?}");
        assert_eq!(tools[0]["pending"]["pending_id"], NEW_ID);
    }

    fn is_real_dir(p: &Path) -> bool {
        std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
    }

    fn empty_dir(p: &Path) -> bool {
        std::fs::read_dir(p).unwrap().next().is_none()
    }

    #[test]
    fn a_symlinked_codex_workspace_is_replaced_by_a_real_dir() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("outputs")).unwrap();
        let ws = d.path().join(CODEX_CHAT_WORKSPACE);
        std::os::unix::fs::symlink(home.path(), &ws).unwrap();
        // Codex writes into its workspace (the fake runs from the repo root).
        let bin = fake_cli(
            d.path(),
            "codex",
            &format!("echo x > {CODEX_CHAT_WORKSPACE}/written\necho done"),
        );
        let rt = runtime_for(d.path(), &bin, "codex");
        let (_, items) = run_turn(&rt);
        assert_eq!(
            items.last().unwrap(),
            &StreamItem::Done {
                error: None,
                final_text: Some("done".into())
            }
        );
        assert!(is_real_dir(&ws));
        assert!(ws.join("written").is_file());
        assert!(empty_dir(home.path()));
    }

    #[test]
    fn a_file_at_the_codex_workspace_is_replaced_by_a_real_dir() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("outputs")).unwrap();
        let ws = d.path().join(CODEX_CHAT_WORKSPACE);
        std::fs::write(&ws, "planted").unwrap();
        let bin = fake_cli(d.path(), "codex", "echo done");
        let rt = runtime_for(d.path(), &bin, "codex");
        run_turn(&rt);
        assert!(is_real_dir(&ws));
    }

    #[test]
    fn symlinked_cursor_workspace_and_cursor_dir_are_replaced() {
        for planted in [
            CURSOR_CHAT_WORKSPACE.to_string(),
            format!("{CURSOR_CHAT_WORKSPACE}/.cursor"),
        ] {
            let d = tempfile::tempdir().unwrap();
            let home = tempfile::tempdir().unwrap();
            let link = d.path().join(&planted);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(home.path(), &link).unwrap();
            prepare_cursor_workspace(d.path(), "loopx chat-tool").unwrap();
            let dir = d.path().join(CURSOR_CHAT_WORKSPACE);
            assert!(is_real_dir(&dir), "{planted}");
            assert!(is_real_dir(&dir.join(".cursor")), "{planted}");
            assert!(dir.join(".cursor/cli.json").is_file(), "{planted}");
            assert!(empty_dir(home.path()), "{planted}");
        }
    }

    #[test]
    fn a_workspace_outside_outputs_is_refused() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("outputs")).unwrap();
        assert!(real_workspace_dir(d.path(), CODEX_CHAT_WORKSPACE).is_ok());
        let e = real_workspace_dir(d.path(), "logs/elsewhere").unwrap_err();
        assert!(e.to_string().contains("outside"), "{e}");
    }

    #[test]
    fn the_codex_chat_warning_says_settings_actions_are_unavailable() {
        let rec = ChatJobRecord::for_cli("codex");
        let w = rec.warning.unwrap();
        assert!(w.contains("enabling or disabling loops"), "{w}");
        assert!(w.contains("Settings"), "{w}");
    }

    #[test]
    fn a_title_failure_leaves_the_reply_and_heuristic_title_intact() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_cli(
            d.path(),
            "gemini",
            r#"case "$*" in *"Write a short title"*) echo nope >&2; exit 1;; esac
echo '{"response":"the reply"}'"#,
        );
        let rt = runtime_for(d.path(), &bin, "gemini");
        let sid = rt.store.send_chat_message("hi there", None).unwrap();
        let before = rt.store.read_chat_sessions();
        let key = rt.jobs.create();
        run_chat_job(&rt, &key, "hi there", &sid);
        generate_chat_title(&rt, &sid, "hi there", "the reply");
        let mut s = rt.jobs.stream(&key).unwrap();
        assert_eq!(
            s.next_batch(Duration::from_millis(10)).unwrap().last(),
            Some(&StreamItem::Done {
                error: None,
                final_text: Some("the reply".into())
            })
        );
        assert_eq!(
            rt.store.read_messages().last().unwrap()["text"],
            "the reply"
        );
        let after = rt.store.read_chat_sessions();
        let title = |s: &crate::messages::ChatSessions| {
            s.sessions
                .iter()
                .find(|r| r["id"].as_str() == Some(sid.as_str()))
                .map(|r| (r["title"].clone(), r["title_source"].clone()))
        };
        assert_eq!(title(&after), title(&before));
        assert_eq!(title(&after).unwrap().1, "auto");
    }

    #[test]
    fn a_title_from_each_cli_output_shape_is_stored() {
        for (cli, program, out) in [
            ("gemini", "gemini", r#"echo '{"response":"Gemini title"}'"#),
            ("cursor", "agent", r#"echo '{"result":"Cursor title"}'"#),
            ("kimi", "kimi", "echo 'Kimi title'"),
            ("codex", "codex", "echo 'Codex title'"),
        ] {
            let d = tempfile::tempdir().unwrap();
            let bin = fake_cli(d.path(), program, out);
            let rt = runtime_for(d.path(), &bin, cli);
            let sid = rt.store.send_chat_message("hi", None).unwrap();
            generate_chat_title(&rt, &sid, "hi", "reply");
            let sessions = rt.store.read_chat_sessions();
            let rec = sessions
                .sessions
                .iter()
                .find(|r| r["id"].as_str() == Some(sid.as_str()))
                .unwrap();
            assert_eq!(rec["title_source"], "ai", "{cli}");
            assert!(
                rec["title"].as_str().unwrap().ends_with("title"),
                "{cli}: {}",
                rec["title"]
            );
        }
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
