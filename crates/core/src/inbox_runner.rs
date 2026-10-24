//! The Inbox Triage loop (`inbox-triage-loop`). Ports
//! `bin/inbox_triage_runner.py`: Rust owns every mail call; the AI gets one
//! tool-less, MCP-less call per inbox ([`crate::agents::sealed`]) and returns
//! JSON decisions. One [`LoopRuntime`] iteration per inbox, failures contained per
//! inbox, exit 0 even when an inbox fails.
//!
//! Claude-only: `claude -p --tools ""` gives the model no tools at all, but
//! `codex exec` always gives it a shell and records the prompt (the email
//! bodies) under `~/.codex/sessions/`. So when the selected AI CLI is codex,
//! every inbox fails up front with [`CODEX_REFUSAL`], before any mail is fetched,
//! labelled or recorded, and no AI is invoked.

use crate::agents::sealed::{self, SealedHooks};
use crate::inbox_config::{self, categories_for, string_list};
use crate::inbox_seen;
use crate::inbox_status;
use crate::inbox_triage::{self, Decision, TriageResponseError};
use crate::loop_definition::LoopDefinition;
use crate::loop_runtime::{AgentFailure, AgentFn, AgentSuccess, LoopRuntime};
use crate::loop_serialize::write_result;
use crate::loop_verifiers::build_verifiers;
use crate::loopkit::{self, RunLock};
use crate::mail_auth;
use crate::mail_http::MailError;
use crate::mail_provider::{self, MailMessage, MailProvider, RefreshFn};
use crate::mail_time::isoformat_utc;
use crate::runner_util::{append_unified_log, notify_slack_with, repo_root, unified_log_path};
use crate::{ai_cli_config, config, content_archive, cost, slack};
use chrono::{DateTime, Local, Utc};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const MESSAGE_CAP: usize = 50;
pub const DEFINITION_RELATIVE_PATH: &str = "loops/inbox-triage/loop.yaml";
/// `inbox-triage` instructions: the loopx-flavoured copy, else the original.
const INSTRUCTION_FILES: [&str; 2] = [
    "instructions/loopx/inbox-triage.md",
    "instructions/inbox-triage.md",
];
pub const CODEX_REFUSAL: &str =
    "Inbox Triage requires the Claude CLI (codex gives the model a shell) - \
switch AI CLI to Claude in Settings";
const REAUTH_FALLBACK: &str = "Sign-in expired - reconnect this inbox";
const DRAFT_CAP: usize = 300;

// ------------------------------------------------------------------ context

/// Every path a run touches; `new` is the production layout.
#[derive(Debug, Clone)]
pub struct InboxCtx {
    pub repo_root: PathBuf,
    /// The loop-engineering home (custom instructions live here).
    pub home: PathBuf,
    pub events_dir: Option<PathBuf>,
    pub results_dir: Option<PathBuf>,
    pub status_path: PathBuf,
    pub history_dir: PathBuf,
    pub drafts_path: PathBuf,
    pub lock_path: PathBuf,
    pub state_dir: PathBuf,
    pub config_path: PathBuf,
    pub unified_log: PathBuf,
    pub slack_config: PathBuf,
}

impl InboxCtx {
    pub fn new(repo_root: PathBuf) -> Self {
        let out = inbox_seen::output_dir_for(&repo_root);
        Self {
            home: config::home_dir(),
            events_dir: None,
            results_dir: None,
            status_path: out.join("status.json"),
            history_dir: out.join("history"),
            drafts_path: out.join("drafts.json"),
            lock_path: out.join("run.lock"),
            state_dir: out.join("state"),
            config_path: inbox_config::default_config_path(),
            unified_log: unified_log_path(&repo_root),
            slack_config: slack::default_config_path(),
            repo_root,
        }
    }

    pub fn definition_path(&self) -> PathBuf {
        self.repo_root.join(DEFINITION_RELATIVE_PATH)
    }
}

// --------------------------------------------------------------------- deps

/// What the AI call returned.
#[derive(Debug, Clone, PartialEq)]
pub struct AiOut {
    pub text: String,
    pub cost_usd: Option<f64>,
}

/// Why an AI call failed; the Python exception class names are what the
/// status/Slack text shows.
#[derive(Debug, Clone, PartialEq)]
pub enum AiFail {
    /// `subprocess.TimeoutExpired`
    Timeout,
    /// `subprocess.CalledProcessError`
    Exited,
    /// `TriageFailed`
    Triage(String),
}

impl AiFail {
    pub fn class_name(&self) -> &'static str {
        match self {
            AiFail::Timeout => "TimeoutExpired",
            AiFail::Exited => "CalledProcessError",
            AiFail::Triage(_) => "TriageFailed",
        }
    }
}

pub type TokenFn = Arc<dyn Fn(&Value) -> Result<String, MailError> + Send + Sync>;
pub type ProviderFactory =
    Arc<dyn Fn(&Value, &str, RefreshFn) -> Result<Box<dyn MailProvider>, String> + Send + Sync>;
pub type Invoker = Arc<dyn Fn(&str, Option<f64>) -> Result<AiOut, AiFail> + Send + Sync>;
/// Replaces [`triage_inbox`] wholesale (tests).
pub type TriageFn = Arc<dyn Fn(&Value, &Value, DateTime<Utc>) -> Outcome + Send + Sync>;
pub type SlackPost = Arc<dyn Fn(&str, &str, Option<&Value>) -> Result<(), String> + Send + Sync>;

/// The injection points of a run (tests substitute their own).
#[derive(Clone)]
pub struct Deps {
    pub token_fn: TokenFn,
    pub provider_factory: ProviderFactory,
    pub invoke: Invoker,
    /// The selected AI CLI (`claude` / `codex`).
    pub cli_fn: Arc<dyn Fn() -> String + Send + Sync>,
    pub slack_post: SlackPost,
    /// Composed instructions; `None` reads them from disk per inbox.
    pub instructions: Option<String>,
    /// Per-inbox spend cap (`stop_conditions.max_cost_usd`).
    pub max_cost_usd: Option<f64>,
    /// Replaces the whole per-inbox triage (tests); `None` is the real thing.
    pub triage: Option<TriageFn>,
}

/// `$LOOPX_GMAIL_BASE_URL` / `$LOOPX_GRAPH_BASE_URL` point the providers at a
/// fake server (tests and sandboxes); unset means the real API.
fn provider_base_url(inbox: &Value) -> Option<String> {
    let var = match inbox.get("provider").and_then(Value::as_str) {
        Some("gmail") => "LOOPX_GMAIL_BASE_URL",
        Some("outlook") => "LOOPX_GRAPH_BASE_URL",
        _ => return None,
    };
    std::env::var(var).ok().filter(|v| !v.is_empty())
}

impl Deps {
    /// The production wiring for `ctx`; `timeout_seconds` bounds each AI call.
    pub fn real(ctx: &InboxCtx, timeout_seconds: f64, max_cost_usd: Option<f64>) -> Self {
        let log = ctx.unified_log.clone();
        Self {
            token_fn: Arc::new(mail_auth::get_access_token_default),
            provider_factory: Arc::new(|inbox, token, refresh| {
                mail_provider::get_provider(
                    inbox,
                    token,
                    Some(refresh),
                    provider_base_url(inbox).as_deref(),
                )
            }),
            invoke: Arc::new(move |prompt, cap| {
                invoke_triage_agent(prompt, timeout_seconds, cap, &log)
            }),
            cli_fn: Arc::new(ai_cli_config::selected_cli),
            slack_post: Arc::new(|url, text, blocks| {
                slack::post_message(url, text, blocks).map(|_| ())
            }),
            instructions: None,
            max_cost_usd,
            triage: None,
        }
    }
}

/// The one AI subprocess boundary. Only exit status, timing and error class are
/// logged: stdout/stderr can echo message content, which must never persist. The
/// call runs in a disposable scratch directory ([`sealed::sealed_call`]).
pub fn invoke_triage_agent(
    prompt: &str,
    timeout_seconds: f64,
    max_budget_usd: Option<f64>,
    unified_log: &Path,
) -> Result<AiOut, AiFail> {
    let mut log = |msg: &str| {
        let stamp = Local::now().format("%Y-%m-%d %H:%M:%S");
        append_unified_log(
            &format!("[{stamp}] inbox-triage: claude triage call {msg}"),
            unified_log,
        );
    };
    match sealed::sealed_call(
        prompt,
        timeout_seconds,
        &mut log,
        max_budget_usd,
        &SealedHooks::default(),
    ) {
        Ok(out) => Ok(AiOut {
            text: out.text,
            cost_usd: out.cost_usd,
        }),
        Err(e) if e.0 == "claude call timed out" => Err(AiFail::Timeout),
        Err(e) if e.0.starts_with("claude exited") => Err(AiFail::Exited),
        Err(e) => {
            if ai_cli_config::selected_cli() != "claude" {
                Err(AiFail::Triage(CODEX_REFUSAL.into()))
            } else {
                Err(AiFail::Triage(e.0))
            }
        }
    }
}

// ----------------------------------------------------------------- outcomes

#[derive(Debug, Clone, PartialEq)]
pub struct UrgentItem {
    pub from: String,
    pub subject: String,
    pub draft_link: Option<String>,
    pub needs_manual_reply: bool,
    pub draft_failed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub date: String,
    pub from: String,
    pub subject: String,
    pub category: String,
    pub reason: String,
    pub draft_link: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DraftRecord {
    pub from: String,
    pub subject: String,
    pub date: String,
    pub category: String,
    pub body: String,
    pub draft_link: String,
}

/// One inbox's result: `status` is `ok`, `quiet`, `failed` or `needs_reauth`.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub name: String,
    pub label: String,
    pub status: String,
    pub counts: Map<String, Value>,
    pub urgent: Vec<UrgentItem>,
    pub rows: Vec<Row>,
    pub drafts: Vec<DraftRecord>,
    pub overflow: bool,
    pub error: Option<String>,
    pub cost_usd: Option<f64>,
    pub slack_bundle: Option<String>,
}

impl Outcome {
    pub fn new(inbox: &Value, status: &str, error: Option<&str>) -> Self {
        let name = inbox
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let label = inbox
            .get("label")
            .and_then(Value::as_str)
            .map_or_else(|| name.clone(), str::to_owned);
        Self {
            name,
            label,
            status: status.into(),
            counts: Map::new(),
            urgent: vec![],
            rows: vec![],
            drafts: vec![],
            overflow: false,
            error: error.map(str::to_owned),
            cost_usd: None,
            slack_bundle: inbox
                .get("slack_bundle")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }
    }

    fn failed(inbox: &Value, error: impl AsRef<str>) -> Self {
        Self::new(inbox, "failed", Some(error.as_ref()))
    }

    pub fn urgent_value(&self) -> Value {
        Value::Array(
            self.urgent
                .iter()
                .map(|u| {
                    json!({"from": u.from, "subject": u.subject, "draft_link": u.draft_link,
                           "needs_manual_reply": u.needs_manual_reply, "draft_failed": u.draft_failed})
                })
                .collect(),
        )
    }
}

fn status_state(status: &str) -> &'static str {
    match status {
        "ok" | "quiet" => "idle",
        "needs_reauth" => "needs_reauth",
        _ => "failed",
    }
}

// ----------------------------------------------------------------- classify

/// Up to two AI attempts; each is capped at what is left of the inbox's
/// `max_cost_usd` (an attempt with unknown cost is booked at its cap).
pub fn classify(
    prompt: &str,
    messages: &[MailMessage],
    categories: &[Value],
    invoke: &Invoker,
    max_cost_usd: Option<f64>,
) -> Result<(Vec<Decision>, Option<f64>), String> {
    let (mut total_cost, mut last_error, mut spent): (Option<f64>, String, f64) =
        (None, String::new(), 0.0);
    for _ in 0..2 {
        let cap = cost::remaining_budget(max_cost_usd, Some(spent), 0.05);
        let out = match invoke(prompt, cap) {
            Ok(o) => o,
            Err(f) => {
                last_error = format!("AI call failed: {}", f.class_name());
                spent += cap.unwrap_or(0.0);
                continue;
            }
        };
        spent += out.cost_usd.unwrap_or(cap.unwrap_or(0.0));
        if let Some(c) = out.cost_usd {
            total_cost = Some(total_cost.unwrap_or(0.0) + c);
        }
        match inbox_triage::parse_response(&out.text, messages, categories) {
            Ok(d) => return Ok((d, total_cost)),
            Err(TriageResponseError(m)) => last_error = m,
        }
    }
    Err(last_error)
}

// ------------------------------------------------------------- instructions

/// The built-in instructions plus the user's own, on top of (never in place of)
/// them (`bin/instructions.py`'s `compose`).
pub fn compose_instructions(repo_root: &Path, home: &Path) -> Result<String, String> {
    let path = INSTRUCTION_FILES
        .iter()
        .map(|f| repo_root.join(f))
        .find(|p| p.exists())
        .ok_or_else(|| {
            format!(
                "instructions/inbox-triage.md not found under {}",
                repo_root.display()
            )
        })?;
    let mut text =
        std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let custom: Vec<String> = [
        home.join("instructions.md"),
        home.join("instructions").join("inbox-triage.md"),
    ]
    .iter()
    .filter_map(|p| std::fs::read_to_string(p).ok())
    .map(|t| t.trim().to_string())
    .filter(|t| !t.is_empty())
    .collect();
    if !custom.is_empty() {
        text = format!(
            "{}\n\n## Custom instructions\n\nThe user's own instructions, on top of (never in place of) everything above:\n\n{}\n",
            text.trim_end(),
            custom.join("\n\n")
        );
    }
    Ok(text)
}

// ------------------------------------------------------------------- triage

type Fetched = Result<(Box<dyn MailProvider>, Vec<MailMessage>), Outcome>;

/// What `triage_inbox` needs beyond the injected [`Deps`].
pub struct TriageEnv<'a> {
    pub deps: &'a Deps,
    pub state_dir: &'a Path,
    pub repo_root: &'a Path,
    pub home: &'a Path,
}

fn reauth_text(e: &MailError) -> String {
    let m = e.to_string();
    if m.is_empty() {
        REAUTH_FALLBACK.into()
    } else {
        m
    }
}

fn is_reauth(e: &MailError) -> bool {
    e.is_auth_expired() || matches!(e, MailError::Reauth(_))
}

/// Triage one inbox end to end; never panics, never returns an error: every
/// failure is an [`Outcome`] with `status` `failed` / `needs_reauth`.
pub fn triage_inbox(
    inbox: &Value,
    config: &Value,
    now: DateTime<Utc>,
    env: &TriageEnv<'_>,
) -> Outcome {
    let deps = env.deps;
    // Before any token/provider call, so under codex no mail is ever read.
    if (deps.cli_fn)() != "claude" {
        return Outcome::failed(inbox, CODEX_REFUSAL);
    }
    let instructions = match &deps.instructions {
        Some(i) => i.clone(),
        None => match compose_instructions(env.repo_root, env.home) {
            Ok(i) => i,
            Err(e) => return Outcome::failed(inbox, e),
        },
    };
    let categories = categories_for(inbox, config);
    let labels: HashMap<String, String> = categories
        .iter()
        .filter_map(|c| {
            Some((
                c.get("key")?.as_str()?.to_string(),
                c.get("label")?.as_str()?.to_string(),
            ))
        })
        .collect();
    let name = inbox
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let account = inbox
        .get("account")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let exclude = string_list(inbox, "exclude_senders");

    let fetched = (|| -> Result<Fetched, MailError> {
        let token = (deps.token_fn)(inbox)?;
        let (token_fn, inbox_for_refresh) = (deps.token_fn.clone(), inbox.clone());
        let refresh: RefreshFn = Box::new(move || token_fn(&inbox_for_refresh));
        let mut provider = match (deps.provider_factory)(inbox, &token, refresh) {
            Ok(p) => p,
            Err(e) => return Ok(Err(Outcome::failed(inbox, e))),
        };
        let address = provider.profile_address()?;
        if address != account.trim().to_lowercase() {
            return Ok(Err(Outcome::failed(
                inbox,
                format!("Signed in as {address}, expected {account}"),
            )));
        }
        let state = inbox_seen::load(name, env.state_dir);
        let seen: HashSet<String> = state.seen.keys().cloned().collect();
        let fetched = provider.fetch_new(
            inbox_seen::since(&state, now),
            &seen,
            &exclude,
            MESSAGE_CAP + 1,
        )?;
        Ok(Ok((provider, fetched)))
    })();
    let (mut provider, fetched) = match fetched {
        Ok(Ok(pair)) => pair,
        Ok(Err(outcome)) => return outcome,
        Err(e) if is_reauth(&e) => {
            return Outcome::new(inbox, "needs_reauth", Some(&reauth_text(&e)))
        }
        Err(e) => return Outcome::failed(inbox, e.to_string()),
    };

    let overflow = fetched.len() > MESSAGE_CAP;
    let messages =
        inbox_triage::filter_excluded(fetched.into_iter().take(MESSAGE_CAP).collect(), &exclude);
    if messages.is_empty() {
        let mut o = Outcome::new(inbox, "quiet", None);
        o.overflow = overflow;
        return o;
    }

    let prompt = inbox_triage::build_prompt(&instructions, inbox, &categories, &messages);
    let (decisions, cost) = match classify(
        &prompt,
        &messages,
        &categories,
        &deps.invoke,
        deps.max_cost_usd,
    ) {
        Ok(r) => r,
        Err(e) => return Outcome::failed(inbox, format!("AI triage failed: {e}")),
    };
    let decisions = inbox_triage::apply_rules(decisions, &messages, inbox);
    let by_id: HashMap<&str, &MailMessage> = messages.iter().map(|m| (m.id.as_str(), m)).collect();
    let label_of = |d: &Decision| labels.get(&d.category).cloned().unwrap_or_default();

    let record = |labelled: &[&MailMessage]| {
        let pairs: Vec<(String, String)> = labelled
            .iter()
            .map(|m| (m.id.clone(), m.date.clone()))
            .collect();
        if let Err(e) = inbox_seen::record(name, &pairs, now, env.state_dir) {
            eprintln!("inbox_triage_runner: recording seen ids for {name} failed: {e}");
        }
    };

    let mut labelled: Vec<&MailMessage> = Vec::new();
    let labelling = (|| -> Result<(), MailError> {
        let mut wanted: Vec<String> = decisions.iter().map(label_of).collect();
        wanted.sort();
        wanted.dedup();
        let label_ids = provider.ensure_labels(&wanted)?;
        for d in &decisions {
            let id = label_ids.get(&label_of(d)).cloned().unwrap_or_default();
            provider.apply_label(&d.id, &id)?;
            labelled.push(by_id[d.id.as_str()]);
        }
        Ok(())
    })();
    if let Err(e) = labelling {
        record(&labelled);
        let mut o = if is_reauth(&e) {
            Outcome::new(inbox, "needs_reauth", Some(&reauth_text(&e)))
        } else {
            Outcome::failed(
                inbox,
                format!(
                    "Labelling stopped after {} of {}: {e}",
                    labelled.len(),
                    decisions.len()
                ),
            )
        };
        o.cost_usd = cost;
        return o;
    }
    record(&labelled);

    let (mut rows, mut urgent, mut drafts) = (Vec::new(), Vec::new(), Vec::new());
    for d in &decisions {
        let message = by_id[d.id.as_str()];
        let (mut link, mut draft_failed) = (None, false);
        if let Some(body) = &d.draft_body {
            // Any failure, not just the HTTP/auth ones: labels and seen ids are
            // already recorded, so an escape here would throw away the urgent
            // list and never retry the remaining drafts. Only the class name is
            // logged - the message text can quote mail content.
            match provider.create_reply_draft(message, body) {
                Ok(l) => link = Some(l).filter(|l| !l.is_empty()),
                Err(e) => {
                    draft_failed = true;
                    eprintln!(
                        "inbox_triage_runner: draft for one message in {name} failed: {}",
                        e.class_name()
                    );
                }
            }
        }
        if let (Some(l), Some(body)) = (&link, &d.draft_body) {
            drafts.push(DraftRecord {
                from: message.from.clone(),
                subject: message.subject.clone(),
                date: message.date.clone(),
                category: d.category.clone(),
                body: body.clone(),
                draft_link: l.clone(),
            });
        }
        rows.push(Row {
            date: message.date.clone(),
            from: message.from.clone(),
            subject: message.subject.clone(),
            category: d.category.clone(),
            reason: d.reason.clone(),
            draft_link: link.clone(),
        });
        if d.category == "urgent" {
            urgent.push(UrgentItem {
                from: message.from.clone(),
                subject: message.subject.clone(),
                draft_link: link,
                needs_manual_reply: d.needs_manual_reply,
                draft_failed,
            });
        }
    }
    let mut o = Outcome::new(inbox, "ok", None);
    o.counts = inbox_triage::count_by_category(&decisions);
    (o.urgent, o.rows, o.drafts, o.overflow, o.cost_usd) = (urgent, rows, drafts, overflow, cost);
    o
}

// ------------------------------------------------- history, digest, drafts

/// Untrusted text (sender, subject, AI reason, error) -> markdown the dashboard
/// shows literally: every metacharacter it honours is backslash-escaped, `://`
/// is broken up so a bare URL isn't auto-linked, a leading list marker can't
/// start a list, and newlines/control characters become spaces. Links the loop
/// writes itself (draft links) are built outside this.
pub fn md(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if c.is_control() && (c as u32) < 0x20 || c == '\u{7f}' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let mut escaped = String::with_capacity(cleaned.len());
    for c in cleaned.chars() {
        if "\\`*_[]!|#<>".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    let escaped = escaped.replace("://", "\\://");
    // "- x" / "+ x" -> backslash before the marker.
    let lead = escaped.len() - escaped.trim_start().len();
    let (indent, rest) = escaped.split_at(lead);
    let mut chars = rest.chars();
    let bullet =
        matches!(chars.next(), Some('-' | '+')) && chars.next().is_some_and(char::is_whitespace);
    let escaped = if bullet {
        format!("{indent}\\{rest}")
    } else {
        escaped
    };
    // "12. x" -> backslash before the dot.
    let lead = escaped.len() - escaped.trim_start().len();
    let digits = escaped[lead..]
        .chars()
        .take_while(char::is_ascii_digit)
        .count();
    if digits > 0 && escaped[lead + digits..].starts_with('.') {
        let at = lead + digits;
        return format!("{}\\{}", &escaped[..at], &escaped[at..]);
    }
    escaped
}

/// Slack's own required escaping of `& < >`, so untrusted text can't ping
/// (`<!channel>`) or spoof a link (`<https://evil|Open draft>`).
pub fn slack_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn local_hhmm(now: DateTime<Utc>) -> String {
    now.with_timezone(&Local).format("%H:%M").to_string()
}

pub fn write_history(
    outcome: &Outcome,
    now: DateTime<Utc>,
    history_dir: &Path,
) -> std::io::Result<PathBuf> {
    let date = now.format("%Y-%m-%d").to_string();
    let path = history_dir.join(format!("{date}-{}.md", outcome.name));
    std::fs::create_dir_all(history_dir)?;
    let mut text = String::new();
    if !path.exists() {
        text.push_str(&format!("# {} - {date}\n", outcome.label));
    }
    text.push_str(&format!(
        "\n## Run {} - {}\n",
        local_hhmm(now),
        outcome.status
    ));
    if let Some(e) = outcome.error.as_deref().filter(|e| !e.is_empty()) {
        text.push_str(&format!("\n{}\n", md(e)));
    }
    if !outcome.rows.is_empty() {
        text.push_str("\n| Received | From | Subject | Category | Reason | Draft |\n|---|---|---|---|---|---|\n");
        for row in &outcome.rows {
            let draft = row
                .draft_link
                .as_ref()
                .map(|l| format!("[draft]({l})"))
                .unwrap_or_default();
            let hhmm: String = row.date.chars().skip(11).take(5).collect();
            text.push_str(&format!(
                "| {hhmm} | {} | {} | {} | {} | {draft} |\n",
                md(&row.from),
                md(&row.subject),
                row.category,
                md(&row.reason)
            ));
        }
    } else if outcome.status == "quiet" {
        text.push_str("\nNo new mail.\n");
    }
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?
        .write_all(text.as_bytes())?;
    Ok(path)
}

fn summary_line(o: &Outcome) -> String {
    let label = slack_escape(&o.label);
    match o.status.as_str() {
        "quiet" => format!("*{label}*: no new mail"),
        "needs_reauth" => {
            format!(
                "*{label}*: needs re-auth - reconnect it from the dashboard's Inbox Triage page"
            )
        }
        "failed" => format!(
            "*{label}*: failed - {}",
            slack_escape(o.error.as_deref().unwrap_or("None"))
        ),
        _ => {
            let n = |k: &str| o.counts.get(k).and_then(Value::as_i64).unwrap_or(0);
            let (urgent, action) = (n("urgent"), n("action"));
            let other: i64 = o
                .counts
                .iter()
                .filter(|(k, _)| *k != "urgent" && *k != "action")
                .filter_map(|(_, v)| v.as_i64())
                .sum();
            let ready = if o.urgent.iter().any(|u| u.draft_link.is_some()) {
                " (drafts ready)"
            } else {
                ""
            };
            format!("*{label}*: {urgent} urgent{ready}, {action} action, {other} other")
        }
    }
}

pub fn format_digest(outcomes: &[&Outcome], now: DateTime<Utc>) -> String {
    let mut lines = vec![format!(
        "*Inbox Triage* - {}",
        now.with_timezone(&Local).format("%Y-%m-%d %H:%M")
    )];
    for o in outcomes {
        lines.push(summary_line(o));
        for item in &o.urgent {
            let note = match (&item.draft_link, item.draft_failed) {
                (Some(l), _) => format!("<{l}|draft>"),
                (None, true) => "draft failed - reply manually".to_string(),
                (None, false) => "reply manually".to_string(),
            };
            lines.push(format!(
                "    \u{2022} {} - \"{}\" ({note})",
                slack_escape(&item.from),
                slack_escape(&item.subject)
            ));
        }
        if o.overflow {
            lines.push(format!(
                "    more waiting - over {MESSAGE_CAP} new messages, the rest are picked up next run"
            ));
        }
    }
    lines.join("\n")
}

/// One digest per Slack bundle; best-effort (a rejected Block Kit template is
/// retried once as plain text), never fails the run.
pub fn send_digests(
    outcomes: &[Outcome],
    now: DateTime<Utc>,
    slack_config: &Path,
    post: &SlackPost,
) {
    let mut groups: Vec<(Option<String>, Vec<&Outcome>)> = Vec::new();
    for o in outcomes {
        match groups.iter_mut().find(|(b, _)| *b == o.slack_bundle) {
            Some((_, g)) => g.push(o),
            None => groups.push((o.slack_bundle.clone(), vec![o])),
        }
    }
    for (bundle, group) in groups {
        let text = format_digest(&group, now);
        notify_slack_with(
            &text,
            Some("inbox_triage_digest"),
            bundle.as_deref(),
            slack_config,
            &|u, t, b| post(u, t, b),
        );
    }
}

/// Keep the reply drafts this run wrote (text included) so the dashboard's
/// Drafts tab can show them any time. Best-effort, like [`write_history`].
pub fn save_drafts(outcome: &Outcome, inbox: &Value, now: DateTime<Utc>, drafts_path: &Path) {
    let account = inbox
        .get("account")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let records: Vec<Value> = outcome
        .drafts
        .iter()
        .map(|d| {
            json!({"from": d.from, "subject": d.subject, "date": d.date, "category": d.category,
                   "body": d.body, "draft_link": d.draft_link, "inbox": outcome.name,
                   "inbox_label": outcome.label, "account": account, "saved_at": isoformat_utc(now)})
        })
        .collect();
    if let Err(e) = content_archive::append(drafts_path, &records, DRAFT_CAP) {
        eprintln!(
            "inbox_triage_runner: saving drafts for {} failed: {}",
            outcome.name,
            e.kind()
        );
    }
}

/// Write this inbox's terminal `failed` state ourselves when the normal
/// end-of-inbox write never happened. `trigger_inbox_triage_run` refuses a new
/// run while an inbox reads "running", so an inbox left there would latch.
/// Best-effort: failing to record a failure must not take down the rest of the run.
pub fn mark_inbox_failed(name: &str, reason: &str, status_path: &Path) -> bool {
    match inbox_status::write(name, "failed", status_path, &[("error", json!(reason))]) {
        Ok(_) => true,
        Err(e) => {
            eprintln!(
                "inbox_triage_runner: writing failed status for {name} failed: {}",
                e.kind()
            );
            false
        }
    }
}

// --------------------------------------------------------------------- run

fn run_one_inbox(
    ctx: &InboxCtx,
    inbox: &Value,
    config: &Value,
    now: DateTime<Utc>,
    run_id: &str,
    definition: &LoopDefinition,
    deps: &Deps,
) -> Outcome {
    let name = inbox
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let captured: Arc<Mutex<Option<Outcome>>> = Arc::default();
    let agent_fn: AgentFn = {
        let (captured, inbox, config, deps) = (
            captured.clone(),
            inbox.clone(),
            config.clone(),
            deps.clone(),
        );
        let (state_dir, repo_root, home) = (
            ctx.state_dir.clone(),
            ctx.repo_root.clone(),
            ctx.home.clone(),
        );
        Box::new(move |_context| {
            let env = TriageEnv {
                deps: &deps,
                state_dir: &state_dir,
                repo_root: &repo_root,
                home: &home,
            };
            // A panic is contained per inbox, like Python's `except Exception`.
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &deps.triage {
                    Some(t) => t(&inbox, &config, now),
                    None => triage_inbox(&inbox, &config, now, &env),
                }))
                .unwrap_or_else(|p| {
                    let what = p
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_default();
                    Outcome::failed(&inbox, format!("panic: {what}"))
                });
            let result = if matches!(outcome.status.as_str(), "failed" | "needs_reauth") {
                Err(AgentFailure {
                    message: outcome.error.clone().unwrap_or_default(),
                    cost_usd: None,
                })
            } else {
                Ok(AgentSuccess {
                    cost_usd: outcome.cost_usd,
                })
            };
            *captured.lock().unwrap() = Some(outcome);
            result
        })
    };
    let runtime_result = (|| -> Result<(), String> {
        let verifiers = build_verifiers(&definition.verifiers, None, None, "observe")
            .map_err(|e| e.to_string())?;
        let mut runtime = LoopRuntime::new(agent_fn, verifiers);
        if let Some(dir) = &ctx.events_dir {
            runtime = runtime.with_events_dir(dir.clone());
        }
        let result = runtime
            .start(definition, &format!("{run_id}_{name}"), None)
            .map_err(|e| e.to_string())?;
        write_result(
            &result,
            ctx.results_dir.as_deref(),
            ctx.events_dir.as_deref(),
            None,
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })();
    if let Err(e) = runtime_result {
        // A triage result, if any, is kept.
        eprintln!("inbox_triage_runner: loop runtime for {name} failed: {e}");
        let mut slot = captured.lock().unwrap();
        if slot.is_none() {
            *slot = Some(Outcome::failed(inbox, format!("Loop runtime error: {e}")));
        }
    }
    let outcome = captured
        .lock()
        .unwrap()
        .take()
        .unwrap_or_else(|| Outcome::failed(inbox, "runtime stopped before triage"));
    if let Err(e) = write_history(&outcome, now, &ctx.history_dir) {
        // History is observability; the status write still has to happen.
        eprintln!(
            "inbox_triage_runner: writing history for {name} failed: {}",
            e.kind()
        );
    }
    save_drafts(&outcome, inbox, now, &ctx.drafts_path);
    outcome
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("{0}")]
    Config(String),
    /// SIGTERM arrived (a `timeout` stop): the in-flight inbox was marked failed.
    #[error("interrupted")]
    Interrupted,
}

/// One lock for the whole run: the scheduler and the dashboard's run-now can both
/// launch a run, and two overlapping runs would each draft replies to the same
/// not-yet-recorded messages. A second run exits at once (`Ok` with no outcomes).
pub fn run_all_inboxes(
    ctx: &InboxCtx,
    run_id: &str,
    now: DateTime<Utc>,
    deps: &Deps,
) -> Result<Vec<Outcome>, RunError> {
    let _lock = match RunLock::acquire(&ctx.lock_path) {
        Ok(Some(l)) => l,
        Ok(None) => {
            eprintln!("inbox_triage_runner: another Inbox Triage run is already running - exiting");
            return Ok(vec![]);
        }
        Err(e) => {
            return Err(RunError::Config(format!(
                "cannot take {}: {e}",
                ctx.lock_path.display()
            )))
        }
    };
    let config =
        inbox_config::load_config(&ctx.config_path).map_err(|e| RunError::Config(e.to_string()))?;
    let definition = LoopDefinition::from_yaml(&ctx.definition_path())
        .map_err(|e| RunError::Config(e.to_string()))?;
    let mut outcomes = Vec::new();
    let enabled: Vec<Value> = config["inboxes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i.get("enabled").is_none_or(crate::loops_config::py_truthy))
        .cloned()
        .collect();
    for inbox in &enabled {
        let name = inbox["name"].as_str().unwrap_or_default().to_string();
        let _ = inbox_status::write(&name, "running", &ctx.status_path, &[]);
        let outcome = run_one_inbox(ctx, inbox, &config, now, run_id, &definition, deps);
        if loopkit::terminated() {
            mark_inbox_failed(
                &name,
                "Run was interrupted before this inbox finished",
                &ctx.status_path,
            );
            return Err(RunError::Interrupted);
        }
        let extra = [
            ("last_run_at", json!(isoformat_utc(now))),
            ("counts", Value::Object(outcome.counts.clone())),
            ("urgent", outcome.urgent_value()),
            ("error", json!(outcome.error)),
            ("overflow", json!(outcome.overflow)),
        ];
        if let Err(e) = inbox_status::write(
            &name,
            status_state(&outcome.status),
            &ctx.status_path,
            &extra,
        ) {
            // Contained per inbox, like every other failure here.
            eprintln!(
                "inbox_triage_runner: {name}: Run stopped unexpectedly ({})",
                e.kind()
            );
            mark_inbox_failed(
                &name,
                "Run was interrupted before this inbox finished",
                &ctx.status_path,
            );
        }
        outcomes.push(outcome);
    }
    send_digests(&outcomes, now, &ctx.slack_config, &deps.slack_post);
    Ok(outcomes)
}

/// `<run_id>`. Exit 0 even when inboxes failed: each failure is contained,
/// recorded in status/history, and announced in the digest - same reasoning as
/// the topic runner.
pub fn main(ctx: &InboxCtx, argv: &[String], deps: Option<Deps>) -> i32 {
    if argv.len() != 1 {
        eprintln!("Usage: loopx run inbox-triage-loop: <run_id>");
        return 2;
    }
    loopkit::install_sigterm_flag();
    let deps = match deps {
        Some(d) => d,
        None => {
            let (timeout, max_cost) = match LoopDefinition::from_yaml(&ctx.definition_path()) {
                Ok(d) => (
                    d.stop_conditions.max_runtime_minutes * 60.0,
                    Some(d.stop_conditions.max_cost_usd),
                ),
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            Deps::real(ctx, timeout, max_cost)
        }
    };
    match run_all_inboxes(ctx, &argv[0], Utc::now(), &deps) {
        Ok(_) => 0,
        Err(RunError::Interrupted) => 128 + libc::SIGTERM,
        Err(RunError::Config(e)) => {
            eprintln!("{e}");
            1
        }
    }
}

/// The checkout / install root a run uses.
pub fn default_ctx() -> InboxCtx {
    InboxCtx::new(repo_root())
}

#[cfg(test)]
#[path = "inbox_runner_tests.rs"]
mod tests;
