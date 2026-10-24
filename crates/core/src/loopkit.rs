//! LoopKit: a small plugin runner for "discover items -> ask the model about
//! each -> act on the answer" loops (mirrors `bin/loopkit.py`). A plugin
//! supplies `discover` and `after_item`; LoopKit supplies the run lock,
//! de-dup (seen store), the `LoopRuntime` iteration with an output-contract
//! verifier (so a malformed answer is retried with feedback), result /
//! history / last-run files and chat notifications. Paths resolve at call
//! time under `repo_root` so tests can point everything at a temp dir.

use crate::agents::sealed::{self, SealedHooks};
use crate::connectors::Field;
use crate::loop_definition::LoopDefinition;
use crate::loop_notify::NotifyResult;
use crate::loop_result::{IterationResult, VerificationResult};
use crate::loop_runtime::{AgentFailure, AgentSuccess, LoopContext as RuntimeContext, LoopRuntime};
use crate::loop_serialize::write_result;
use crate::loop_state::LoopState;
use crate::loop_verifiers::{build_verifiers, Verifier};
use crate::seen_store::{py_isoformat, SeenStore};
use crate::{content_archive, cost, loops_config, pyjson, runner_util};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const LAST_RUN_OUTCOME_CAP: usize = 50;
const CRASH_SUMMARY_CAP: usize = 300;

// ---- plugin-facing types ------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct WorkItem {
    pub key: String,
    pub title: String,
    pub url: String,
    pub payload: Value,
}

impl WorkItem {
    pub fn new(key: impl Into<String>, title: impl Into<String>, payload: Value) -> Self {
        Self {
            key: key.into(),
            title: title.into(),
            url: String::new(),
            payload,
        }
    }
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Done,
    Skipped,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Done => "done",
            Status::Skipped => "skipped",
            Status::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub item_key: String,
    pub status: Status,
    pub summary: String,
    pub url: String,
    /// A JSON object (empty when the item produced no content).
    pub data: Value,
}

impl Outcome {
    pub fn new(item_key: impl Into<String>, status: Status, summary: impl Into<String>) -> Self {
        Self {
            item_key: item_key.into(),
            status,
            summary: summary.into(),
            url: String::new(),
            data: Value::Object(Map::new()),
        }
    }
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = data;
        self
    }
}

/// A failure inside a plugin: a Python-style class name (the only part that is
/// safe to log, since the message can carry URLs or tokens) plus a message.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginError {
    pub class: String,
    pub message: String,
}

impl PluginError {
    pub fn new(class: &str, message: impl Into<String>) -> Self {
        Self {
            class: class.to_string(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            f.write_str(&self.class)
        } else {
            write!(f, "{}: {}", self.class, self.message)
        }
    }
}

impl std::error::Error for PluginError {}

impl From<crate::connectors::ConnectorError> for PluginError {
    fn from(e: crate::connectors::ConnectorError) -> Self {
        PluginError::new(&e.class_name(), e.to_string())
    }
}

/// A successful model call.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelAnswer {
    pub text: String,
    pub cost_usd: Option<f64>,
}

/// A failed model call; it may still have spent money (`cost_usd`).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelError {
    pub class: String,
    pub message: String,
    pub cost_usd: Option<f64>,
}

impl ModelError {
    pub fn new(class: &str, message: impl Into<String>) -> Self {
        Self {
            class: class.into(),
            message: message.into(),
            cost_usd: None,
        }
    }
}

/// `(prompt, timeout_seconds, max_budget_usd)` -> the model's answer.
pub type ModelFn =
    Arc<dyn Fn(&str, f64, Option<f64>) -> Result<ModelAnswer, ModelError> + Send + Sync>;
pub type Notifier = Arc<dyn Fn(&str, &str) -> Vec<NotifyResult> + Send + Sync>;
pub type LoopLookupFn = Arc<dyn Fn(&str) -> Result<Value, String> + Send + Sync>;
pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

/// What a plugin sees for one run (`LoopContext`). Cheap to clone.
#[derive(Clone)]
pub struct LoopContext {
    pub loop_name: String,
    pub run_id: String,
    pub now: DateTime<Utc>,
    pub definition: Arc<LoopDefinition>,
    /// The loop's `settings` object from the registry.
    pub settings: Value,
    pub history_dir: PathBuf,
    pub log: LogFn,
    pub repo_root: PathBuf,
    pub force: bool,
    /// What is left of this item's `max_cost_usd` for the next model call (set
    /// per call by LoopKit); `None` means the definition's full `max_cost_usd`.
    pub call_budget_usd: Option<f64>,
    /// Override for the model call (tests); `None` = the sealed Claude call.
    pub model: Option<ModelFn>,
    /// Override for the seen-store root (tests); `None` = `<root>/outputs/loops`.
    pub state_dir: Option<PathBuf>,
}

impl LoopContext {
    pub fn log(&self, message: &str) {
        (self.log)(message)
    }

    /// `ctx.settings.get(key)` as text (`""` when absent or not a string).
    pub fn setting_str(&self, key: &str) -> String {
        self.settings
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }
}

pub trait LoopPlugin: Send + Sync {
    fn loop_name(&self) -> &str;
    fn definition_dir(&self) -> &str;
    fn max_items_per_run(&self) -> usize {
        20
    }
    fn output_keys(&self) -> &[&'static str] {
        &[]
    }
    /// The settings form the dashboard renders for this loop.
    fn settings_fields(&self) -> &'static [Field] {
        &[]
    }

    fn discover(&self, ctx: &LoopContext) -> Result<Vec<WorkItem>, PluginError>;

    /// Default: `loops/<definition_dir>/prompt.md` with `{{item_json}}` and
    /// `{{settings_json}}` substituted.
    fn build_prompt(&self, item: &WorkItem, ctx: &LoopContext) -> Result<String, PluginError> {
        default_build_prompt(self.definition_dir(), item, ctx)
    }

    /// Default: the sealed (tool-less) Claude call.
    fn call_model(&self, prompt: &str, ctx: &LoopContext) -> Result<ModelAnswer, ModelError> {
        default_call_model(prompt, ctx)
    }

    /// `answer` is the parsed JSON object (or the raw text as a JSON string
    /// when the plugin declares no `output_keys`).
    fn after_item(
        &self,
        item: &WorkItem,
        answer: &Value,
        ctx: &LoopContext,
    ) -> Result<Outcome, PluginError>;

    fn digest(&self, _outcomes: &[Outcome], _ctx: &LoopContext) -> Option<String> {
        None
    }
}

pub fn default_build_prompt(
    definition_dir: &str,
    item: &WorkItem,
    ctx: &LoopContext,
) -> Result<String, PluginError> {
    let path = ctx
        .repo_root
        .join("loops")
        .join(definition_dir)
        .join("prompt.md");
    let template = std::fs::read_to_string(&path)
        .map_err(|e| PluginError::new("FileNotFoundError", format!("{}: {e}", path.display())))?;
    let item_json = pyjson::to_string_pretty(&json!({
        "key": item.key, "title": item.title, "url": item.url, "payload": item.payload}));
    Ok(template.replace("{{item_json}}", &item_json).replace(
        "{{settings_json}}",
        &pyjson::to_string_pretty(&ctx.settings),
    ))
}

pub fn default_call_model(prompt: &str, ctx: &LoopContext) -> Result<ModelAnswer, ModelError> {
    let timeout = ctx.definition.stop_conditions.max_runtime_minutes * 60.0;
    let cap = ctx
        .call_budget_usd
        .or(Some(ctx.definition.stop_conditions.max_cost_usd));
    if let Some(model) = &ctx.model {
        return model(prompt, timeout, cap);
    }
    let log = ctx.log.clone();
    let mut sink = move |m: &str| log(m);
    sealed::sealed_call(prompt, timeout, &mut sink, cap, &SealedHooks::default())
        .map(|r| ModelAnswer {
            text: r.text,
            cost_usd: r.cost_usd,
        })
        .map_err(|e| ModelError::new("SealedCallFailed", e.0))
}

// ---- text helpers ---------------------------------------------------------------

/// Make untrusted text safe to embed in any chat message: no control
/// characters, collapsed whitespace, and `<`/`>` swapped for look-alikes so no
/// Slack control sequence (`<!channel>`, `<@U123>`, `<url|label>`) can form.
pub fn chat_text(value: &str, max_len: usize) -> String {
    let no_ctrl: String = value
        .chars()
        .map(|c| {
            if (c as u32) < 0x20 || c as u32 == 0x7f {
                ' '
            } else {
                c
            }
        })
        .collect();
    let text = no_ctrl
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('<', "\u{2039}")
        .replace('>', "\u{203a}");
    if text.chars().count() > max_len {
        let cut: String = text.chars().take(max_len.saturating_sub(1)).collect();
        format!("{}\u{2026}", cut.trim_end())
    } else {
        text
    }
}

/// The URL if it is a plain http(s) URL with nothing that could break out of a
/// chat link, else `""`.
pub fn chat_url(url: &str) -> String {
    if (url.starts_with("http://") || url.starts_with("https://"))
        && !url
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | '|' | '"'))
    {
        url.to_string()
    } else {
        String::new()
    }
}

/// Plain-text `label (url)`: renders acceptably in every chat service.
pub fn chat_link(text: &str, url: &str) -> String {
    let safe = chat_url(url);
    if safe.is_empty() {
        chat_text(text, 300)
    } else {
        format!("{} ({safe})", chat_text(text, 300))
    }
}

/// Slack mrkdwn `<url|label>` for a plain http(s) URL, label sanitised; just
/// the label when the URL isn't safe.
pub fn slack_link(url: &str, text: &str) -> String {
    let safe = chat_url(url);
    let label = chat_text(&text.replace('|', " "), 300);
    if !safe.is_empty() && !label.is_empty() {
        format!("<{safe}|{label}>")
    } else if !label.is_empty() {
        label
    } else {
        safe
    }
}

/// Parse the model's answer. With `required_keys`: a JSON object (optionally
/// wrapped in a ```json fence) containing every key, else `Err`. Without: any
/// non-empty text, returned as a JSON string.
pub fn parse_answer(text: Option<&str>, required_keys: &[&str]) -> Result<Value, String> {
    let stripped = text.unwrap_or("").trim();
    if required_keys.is_empty() {
        if stripped.is_empty() {
            return Err("empty answer".into());
        }
        return Ok(Value::String(stripped.to_string()));
    }
    let mut body = stripped;
    if body.len() >= 6 && body.starts_with("```") && body.ends_with("```") {
        let inner = &body[3..body.len() - 3];
        body = inner.strip_prefix("json").unwrap_or(inner).trim();
    }
    let data: Value =
        serde_json::from_str(body).map_err(|e| format!("answer is not valid JSON: {e}"))?;
    let Some(obj) = data.as_object() else {
        return Err("answer must be a JSON object".into());
    };
    let missing: Vec<&str> = required_keys
        .iter()
        .copied()
        .filter(|k| !obj.contains_key(*k))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "answer is missing required keys: {}",
            missing.join(", ")
        ));
    }
    Ok(data)
}

// ---- output-contract verifier -------------------------------------------------------

#[derive(Default)]
struct Holder {
    text: Option<String>,
    error: Option<String>,
}

struct OutputContractVerifier {
    holder: Arc<Mutex<Holder>>,
    required_keys: Vec<String>,
}

impl Verifier for OutputContractVerifier {
    fn verify(&self, _context: &RuntimeContext<'_>) -> VerificationResult {
        let keys: Vec<&str> = self.required_keys.iter().map(String::as_str).collect();
        let (error, text) = {
            let h = self.holder.lock().unwrap_or_else(|e| e.into_inner());
            (h.error.clone(), h.text.clone())
        };
        let error = error.or_else(|| parse_answer(text.as_deref(), &keys).err());
        let passed = error.is_none();
        VerificationResult {
            name: "output_contract".into(),
            passed,
            exit_code: Some(if passed { 0 } else { 1 }),
            duration_ms: 0,
            output: error.unwrap_or_default(),
            evidence: json!({"required_keys": self.required_keys}),
        }
    }
}

// ---- run lock -----------------------------------------------------------------------

/// An exclusive, non-blocking `flock` on a lock file. The kernel drops it when
/// the holder exits, however it exits, so a killed run can never leave it stuck.
pub struct RunLock {
    file: std::fs::File,
}

impl RunLock {
    /// `Ok(None)` when another run holds the lock.
    pub fn acquire(path: &Path) -> std::io::Result<Option<RunLock>> {
        use std::os::unix::io::AsRawFd;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        // SAFETY: flock on a valid descriptor we own.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return if err.kind() == std::io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(err)
            };
        }
        Ok(Some(RunLock { file }))
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: unlocking the descriptor we locked.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

static TERMINATED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigterm(_sig: libc::c_int) {
    TERMINATED.store(true, Ordering::SeqCst);
}

/// `run-loop-now`'s timeout stops a run with SIGTERM; raising a flag (checked
/// between items) lets cleanup happen: the seen store and reports are written.
pub fn install_sigterm_flag() {
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, on_sigterm as *const () as libc::sighandler_t);
    }
}

pub fn terminated() -> bool {
    TERMINATED.load(Ordering::SeqCst)
}

// ---- helpers ----------------------------------------------------------------------

fn crash_summary(err: &PluginError) -> String {
    chat_text(
        &format!("{}: {}", err.class, err.message),
        CRASH_SUMMARY_CAP,
    )
}

/// `sha1(data)` as lowercase hex (the slug's disambiguating suffix).
pub fn sha1_hex(data: &[u8]) -> String {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6u32),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

/// A filesystem-safe, unique name for an item key.
pub fn slug(key: &str) -> String {
    let mut base = String::new();
    let mut in_run = false;
    for c in key.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            base.push(c);
            in_run = false;
        } else if !in_run {
            base.push('-');
            in_run = true;
        }
    }
    let base: String = base.trim_matches('-').chars().take(40).collect();
    let base = if base.is_empty() {
        "item".to_string()
    } else {
        base
    };
    format!("{base}-{}", &sha1_hex(key.as_bytes())[..8])
}

fn make_logger(repo_root: &Path, loop_name: &str) -> LogFn {
    let path = repo_root.join("logs").join("loop-engineering.log");
    let loop_name = loop_name.to_string();
    Arc::new(move |message: &str| {
        let line = format!("{} {loop_name}: {message}\n", py_isoformat(Utc::now()));
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    })
}

// ---- per-item run -----------------------------------------------------------------

struct ItemEnv<'a> {
    plugin: &'a Arc<dyn LoopPlugin>,
    ctx: &'a LoopContext,
    run_id: &'a str,
    events_dir: Option<&'a Path>,
    results_dir: &'a Path,
    repo_root: &'a Path,
}

fn run_item(env: &ItemEnv, item: &WorkItem) -> Outcome {
    let (plugin, ctx) = (env.plugin, env.ctx);
    let holder = Arc::new(Mutex::new(Holder::default()));
    let max_cost = ctx.definition.stop_conditions.max_cost_usd;
    let spent = Arc::new(Mutex::new(0.0f64));
    let keys: Vec<&'static str> = plugin.output_keys().to_vec();

    let agent_fn = {
        let (plugin, ctx, item) = (plugin.clone(), ctx.clone(), item.clone());
        let (holder, spent) = (holder.clone(), spent.clone());
        move |context: &RuntimeContext<'_>| -> Result<AgentSuccess, AgentFailure> {
            let mut prompt = plugin.build_prompt(&item, &ctx).map_err(|e| AgentFailure {
                message: e.to_string(),
                cost_usd: None,
            })?;
            let mut h = holder.lock().unwrap_or_else(|e| e.into_inner());
            if let (Some(previous), Some(_)) = (context.previous, h.text.as_ref()) {
                if let Some(failed) = previous.verification_results.iter().find(|r| !r.passed) {
                    if failed.name == "output_contract" {
                        prompt.push_str(&format!(
                            "\n\nYour previous answer violated the output contract: {}. Answer again with valid JSON only.",
                            failed.output
                        ));
                    } else {
                        prompt.push_str(&format!(
                            "\n\nYour previous attempt failed verification: {}. Fix it and answer again.",
                            failed.output
                        ));
                    }
                }
            }
            h.error = None;
            h.text = None;
            drop(h);
            // Each attempt may spend only what is left of the item's budget.
            let spent_now = *spent.lock().unwrap_or_else(|e| e.into_inner());
            let cap = cost::remaining_budget(Some(max_cost), Some(spent_now), 0.05);
            let mut call_ctx = ctx.clone();
            call_ctx.call_budget_usd = cap;
            let charge = |cost: Option<f64>| {
                // An unknown cost is booked at the cap it ran under (never $0).
                *spent.lock().unwrap_or_else(|e| e.into_inner()) +=
                    cost.unwrap_or(cap.unwrap_or(0.0));
            };
            match plugin.call_model(&prompt, &call_ctx) {
                Ok(res) => {
                    holder.lock().unwrap_or_else(|e| e.into_inner()).text = Some(res.text);
                    charge(res.cost_usd);
                    Ok(AgentSuccess {
                        cost_usd: res.cost_usd,
                    })
                }
                Err(e) => {
                    // A crashed model call is a verification failure, so
                    // LoopRuntime retries it.
                    holder.lock().unwrap_or_else(|e| e.into_inner()).error =
                        Some(format!("model call failed: {}: {}", e.class, e.message));
                    charge(e.cost_usd);
                    Ok(AgentSuccess {
                        cost_usd: e.cost_usd,
                    })
                }
            }
        }
    };

    let fail = |class: &str, msg: String| {
        Outcome::new(
            &item.key,
            Status::Failed,
            crash_summary(&PluginError::new(class, msg)),
        )
        .with_url(item.url.clone())
    };

    let mut verifiers: Vec<Box<dyn Verifier>> = vec![Box::new(OutputContractVerifier {
        holder: holder.clone(),
        required_keys: keys.iter().map(|k| k.to_string()).collect(),
    })];
    match build_verifiers(
        &ctx.definition.verifiers,
        Some(env.repo_root),
        None,
        "observe",
    ) {
        Ok(v) => verifiers.extend(v),
        Err(e) => return fail("ValueError", e.0),
    }
    let mut runtime = LoopRuntime::new(Box::new(agent_fn), verifiers);
    if let Some(dir) = env.events_dir {
        runtime = runtime.with_events_dir(dir.to_path_buf());
    }
    let run_id = format!("{}_{}", env.run_id, slug(&item.key));
    let result = match runtime.start(&ctx.definition, &run_id, Some(plugin.loop_name())) {
        Ok(r) => r,
        Err(e) => return fail("RuntimeError", e.to_string()),
    };
    if let Err(e) = write_result(&result, Some(env.results_dir), env.events_dir, None) {
        return fail("OSError", e.to_string());
    }
    if result.final_state == LoopState::Completed {
        let text = holder
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .text
            .clone();
        let outcome = parse_answer(text.as_deref(), &keys)
            .map_err(|m| PluginError::new("ValueError", m))
            .and_then(|answer| plugin.after_item(item, &answer, ctx));
        return match outcome {
            Ok(mut o) => {
                if o.url.is_empty() {
                    o.url = item.url.clone();
                }
                o
            }
            Err(e) => Outcome::new(&item.key, Status::Failed, crash_summary(&e))
                .with_url(item.url.clone()),
        };
    }
    let error = holder
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .error
        .clone();
    let cause = error
        .or_else(|| first_failed_output(&result.iterations))
        .unwrap_or_default();
    let mut summary = format!(
        "loop ended {}: {}",
        result.final_state.as_str(),
        result.stop_reason
    );
    if !cause.is_empty() {
        summary.push_str(&format!(
            " ({})",
            cause.chars().take(200).collect::<String>()
        ));
    }
    Outcome::new(&item.key, Status::Failed, summary).with_url(item.url.clone())
}

/// The output of the first failing verification, newest iteration first.
fn first_failed_output(iterations: &[IterationResult]) -> Option<String> {
    iterations
        .iter()
        .rev()
        .flat_map(|it| it.verification_results.iter())
        .find(|r| !r.passed)
        .map(|r| r.output.clone())
}

// ---- reports -----------------------------------------------------------------------

/// A human name for an outcome (a plugin sets `data.label`, e.g. a meeting
/// title); the raw item key (or `default`) when it has none.
fn outcome_label(outcome: &Outcome, default: Option<&str>) -> String {
    if let Some(label) = outcome.data.get("label").and_then(Value::as_str) {
        if !label.trim().is_empty() {
            return chat_text(label, 120);
        }
    }
    default.map_or_else(|| outcome.item_key.clone(), str::to_string)
}

fn write_reports(
    plugin: &dyn LoopPlugin,
    run_id: &str,
    now: DateTime<Utc>,
    outcomes: &[Outcome],
    history_dir: &Path,
    last_run_path: &Path,
) -> std::io::Result<Map<String, Value>> {
    let count = |s: Status| outcomes.iter().filter(|o| o.status == s).count();
    let mut counts = Map::new();
    for s in [Status::Done, Status::Skipped, Status::Failed] {
        counts.insert(s.as_str().to_string(), json!(count(s)));
    }
    if !outcomes.is_empty() {
        std::fs::create_dir_all(history_dir)?;
        let mut lines = vec![
            format!("# {} run {run_id}", plugin.loop_name()),
            String::new(),
        ];
        for o in outcomes {
            let url = if o.url.is_empty() {
                String::new()
            } else {
                format!(" ({})", o.url)
            };
            lines.push(format!(
                "- [{}] {}: {}{url}",
                o.status.as_str(),
                outcome_label(o, None),
                o.summary
            ));
        }
        std::fs::write(
            history_dir.join(format!("{}.md", now.format("%Y-%m-%d_%H%M%S"))),
            lines.join("\n") + "\n",
        )?;
    }
    if let Some(dir) = last_run_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listed: Vec<Value> = outcomes
        .iter()
        .take(LAST_RUN_OUTCOME_CAP)
        .map(|o| {
            json!({"item_key": o.item_key, "label": outcome_label(o, Some("")), "status": o.status.as_str(),
                   "summary": o.summary, "url": o.url})
        })
        .collect();
    let data = json!({
        "run_id": run_id,
        "finished_at": py_isoformat(Utc::now()),
        "counts": counts,
        "outcomes": listed,
    });
    std::fs::write(last_run_path, pyjson::to_string_pretty(&data) + "\n")?;
    archive_content(
        run_id,
        now,
        outcomes,
        &last_run_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("content.json"),
    );
    Ok(counts)
}

/// Keep what each finished item produced (its `Outcome.data`: briefs,
/// highlights, ...) so the dashboard's Content tab can show it later.
/// Best-effort: observability never fails a run.
fn archive_content(run_id: &str, now: DateTime<Utc>, outcomes: &[Outcome], path: &Path) {
    let records: Vec<Value> = outcomes
        .iter()
        .filter(|o| o.status == Status::Done && o.data.as_object().is_some_and(|m| !m.is_empty()))
        .map(|o| {
            json!({"run_id": run_id, "saved_at": py_isoformat(now), "item_key": o.item_key,
                   "label": outcome_label(o, Some("")), "summary": o.summary, "url": o.url, "data": o.data})
        })
        .collect();
    let _ = content_archive::append(path, &records, content_archive::DEFAULT_CAP);
}

/// Add notification status to last-run.json: connector ids and ok flags only,
/// never the result messages.
fn record_notified(last_run_path: &Path, results: &[NotifyResult]) {
    let Ok(text) = std::fs::read_to_string(last_run_path) else {
        return;
    };
    let Ok(Value::Object(mut data)) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    data.insert("notified".into(), json!(results.iter().any(|r| r.1)));
    data.insert(
        "notify_targets".into(),
        Value::Array(
            results
                .iter()
                .map(|(id, ok, _)| json!({"id": id, "ok": ok}))
                .collect(),
        ),
    );
    let _ = std::fs::write(
        last_run_path,
        pyjson::to_string_pretty(&Value::Object(data)) + "\n",
    );
}

// ---- the run -----------------------------------------------------------------------

#[derive(Default, Clone)]
pub struct RunOptions {
    pub repo_root: Option<PathBuf>,
    pub now: Option<DateTime<Utc>>,
    pub results_dir: Option<PathBuf>,
    pub events_dir: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub history_dir: Option<PathBuf>,
    pub lock_path: Option<PathBuf>,
    pub loop_lookup: Option<LoopLookupFn>,
    pub notifier: Option<Notifier>,
    pub force: bool,
    pub model: Option<ModelFn>,
}

/// What `run_plugin` hands back besides the outcomes (`report` in Python).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RunReport {
    /// A digest was produced and every notification target failed.
    pub notify_failed: bool,
}

pub fn default_loop_lookup() -> LoopLookupFn {
    Arc::new(|name: &str| {
        loops_config::get_loop(name, &loops_config::default_config_path(), None)
            .map_err(|e| e.to_string())
    })
}

pub fn run_plugin(
    plugin: Arc<dyn LoopPlugin>,
    run_id: &str,
    opts: RunOptions,
) -> Result<(Vec<Outcome>, RunReport), PluginError> {
    let repo_root = opts
        .repo_root
        .clone()
        .unwrap_or_else(runner_util::repo_root);
    let now = opts.now.unwrap_or_else(Utc::now);
    let loop_name = plugin.loop_name().to_string();
    let loop_dir = repo_root.join("outputs").join("loops").join(&loop_name);
    let lock_path = opts
        .lock_path
        .clone()
        .unwrap_or_else(|| loop_dir.join("run.lock"));
    let results_dir = opts
        .results_dir
        .clone()
        .unwrap_or_else(|| repo_root.join("outputs").join("loop-runs"));
    let history_dir = opts
        .history_dir
        .clone()
        .unwrap_or_else(|| loop_dir.join("history"));
    let notifier: Notifier = opts.notifier.clone().unwrap_or_else(|| {
        Arc::new(|loop_name: &str, text: &str| {
            crate::connectors_config::notify_loop(loop_name, text, None)
        })
    });
    let lookup = opts.loop_lookup.clone().unwrap_or_else(default_loop_lookup);
    let log = make_logger(&repo_root, &loop_name);
    let io_err = |e: std::io::Error| PluginError::new("OSError", e.to_string());

    let Some(_lock) = RunLock::acquire(&lock_path).map_err(io_err)? else {
        log("another run holds the lock; skipping");
        return Ok((Vec::new(), RunReport::default()));
    };
    let def_path = repo_root
        .join("loops")
        .join(plugin.definition_dir())
        .join("loop.yaml");
    let definition = LoopDefinition::from_yaml(&def_path)
        .map_err(|e| PluginError::new("ValueError", e.to_string()))?;
    let settings = lookup(&loop_name)
        .map_err(|e| PluginError::new("KeyError", e))?
        .get("settings")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let ctx = LoopContext {
        loop_name: loop_name.clone(),
        run_id: run_id.to_string(),
        now,
        definition: Arc::new(definition),
        settings,
        history_dir: history_dir.clone(),
        log: log.clone(),
        repo_root: repo_root.clone(),
        force: opts.force,
        call_budget_usd: None,
        model: opts.model.clone(),
        state_dir: opts.state_dir.clone(),
    };
    let items = match plugin.discover(&ctx) {
        Ok(items) => items,
        Err(e) => {
            notifier(
                &loop_name,
                &format!(
                    "{loop_name} FAILED during discovery: {}",
                    chat_text(&e.class, 300)
                ),
            );
            log(&format!("discovery failed: {}", e.class));
            return Err(e);
        }
    };
    let mut seen = SeenStore::open(
        &loop_name,
        opts.state_dir.as_deref(),
        Some(Box::new(move || now)),
    )
    .map_err(|m| PluginError::new("ValueError", m))?;
    let mut items: Vec<WorkItem> = if opts.force {
        items
    } else {
        items.into_iter().filter(|i| !seen.has(&i.key)).collect()
    };
    items.truncate(plugin.max_items_per_run());
    log(&format!("processing {} item(s)", items.len()));

    let send = |text: &str| -> Vec<NotifyResult> {
        let results = notifier(&loop_name, text);
        for (cid, ok, _) in &results {
            log(&format!(
                "notify {}: {}",
                chat_text(cid, 60),
                if *ok { "ok" } else { "FAILED" }
            ));
        }
        results
    };

    let env = ItemEnv {
        plugin: &plugin,
        ctx: &ctx,
        run_id,
        events_dir: opts.events_dir.as_deref(),
        results_dir: &results_dir,
        repo_root: &repo_root,
    };
    let mut outcomes: Vec<Outcome> = Vec::new();
    let mut interrupted = false;
    for item in &items {
        if terminated() {
            interrupted = true;
            break;
        }
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| run_item(&env, item)))
            .unwrap_or_else(|_| {
                Outcome::new(&item.key, Status::Failed, "panic: item crashed")
                    .with_url(item.url.clone())
            });
        if matches!(outcome.status, Status::Done | Status::Skipped) {
            seen.add(&item.key);
            // Persist now: a later crash/SIGTERM must not re-run acted-on items.
            seen.save().map_err(io_err)?;
        }
        log(&format!(
            "item {}: {}",
            slug(&item.key),
            outcome.status.as_str()
        ));
        outcomes.push(outcome);
    }
    let last_run = loop_dir.join("last-run.json");
    if interrupted {
        let _ = seen.save();
        if write_reports(
            plugin.as_ref(),
            run_id,
            now,
            &outcomes,
            &history_dir,
            &last_run,
        )
        .is_err()
        {
            log("could not write reports while failing: OSError");
        }
        return Err(PluginError::new("SystemExit", "143"));
    }
    seen.save().map_err(io_err)?;
    let counts = write_reports(
        plugin.as_ref(),
        run_id,
        now,
        &outcomes,
        &history_dir,
        &last_run,
    )
    .map_err(io_err)?;
    let text = plugin.digest(&outcomes, &ctx).filter(|t| !t.is_empty());
    let mut sends: Vec<NotifyResult> = Vec::new();
    let mut digest_sends: Vec<NotifyResult> = Vec::new();
    if let Some(text) = &text {
        digest_sends = send(text);
        sends = digest_sends.clone();
    }
    let failed = counts.get("failed").and_then(Value::as_u64).unwrap_or(0);
    if failed > 0 {
        sends.extend(send(&format!(
            "{loop_name}: {failed} of {} item(s) failed",
            outcomes.len()
        )));
    }
    if !sends.is_empty() {
        record_notified(&last_run, &sends);
    }
    let report = RunReport {
        notify_failed: text.is_some() && !digest_sends.iter().any(|r| r.1),
    };
    Ok((outcomes, report))
}

/// `loopkit.main`: `<run_id> [--force]`; returns the process exit code.
pub fn main(plugin: Arc<dyn LoopPlugin>, argv: &[String], opts: RunOptions) -> i32 {
    let force = argv.iter().any(|a| a == "--force");
    let args: Vec<&String> = argv.iter().filter(|a| *a != "--force").collect();
    if args.len() != 1 {
        eprintln!("Usage: {} <run_id> [--force]", plugin.loop_name());
        return 2;
    }
    install_sigterm_flag();
    let opts = RunOptions { force, ..opts };
    match run_plugin(plugin, args[0], opts) {
        Ok((outcomes, report)) => {
            if report.notify_failed
                || (!outcomes.is_empty() && outcomes.iter().all(|o| o.status == Status::Failed))
            {
                1
            } else {
                0
            }
        }
        Err(e) if e.class == "SystemExit" => 143,
        Err(e) => {
            eprintln!("loopx: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::collections::VecDeque;

    const LOOP_YAML: &str = "name: demo-loop
version: 1
trigger: {type: schedule, schedule: \"0 9 * * *\"}
goal: {type: demo}
agent: {provider: claude, model: default}
actions: [read_issue]
stop_conditions: {max_iterations: 2, max_runtime_minutes: 5, max_cost_usd: 1, no_progress_iterations: 2}
retry: {enabled: true, max_attempts: 2}
";

    type After = Box<dyn Fn(&WorkItem, &Value) -> Result<Outcome, PluginError> + Send + Sync>;

    struct Demo {
        items: Vec<WorkItem>,
        answers: Mutex<VecDeque<Result<String, String>>>,
        costs: Mutex<VecDeque<Option<f64>>>,
        prompts: Mutex<Vec<String>>,
        caps: Mutex<Vec<Option<f64>>>,
        after: Option<After>,
        max: usize,
        discover_error: bool,
    }

    impl Demo {
        fn new(items: Vec<WorkItem>, answers: &[&str]) -> Self {
            Self {
                items,
                answers: Mutex::new(answers.iter().map(|a| Ok(a.to_string())).collect()),
                costs: Mutex::new(VecDeque::new()),
                prompts: Mutex::new(Vec::new()),
                caps: Mutex::new(Vec::new()),
                after: None,
                max: 20,
                discover_error: false,
            }
        }
        fn with_answers(mut self, answers: Vec<Result<String, String>>) -> Self {
            self.answers = Mutex::new(answers.into());
            self
        }
        fn with_costs(self, costs: &[Option<f64>]) -> Self {
            *self.costs.lock().unwrap() = costs.iter().copied().collect();
            self
        }
        fn prompts(&self) -> Vec<String> {
            self.prompts.lock().unwrap().clone()
        }
    }

    impl LoopPlugin for Demo {
        fn loop_name(&self) -> &str {
            "demo-loop"
        }
        fn definition_dir(&self) -> &str {
            "demo"
        }
        fn max_items_per_run(&self) -> usize {
            self.max
        }
        fn output_keys(&self) -> &[&'static str] {
            &["verdict"]
        }
        fn discover(&self, _c: &LoopContext) -> Result<Vec<WorkItem>, PluginError> {
            if self.discover_error {
                return Err(PluginError::new("RuntimeError", "api down"));
            }
            Ok(self.items.clone())
        }
        fn build_prompt(&self, item: &WorkItem, _c: &LoopContext) -> Result<String, PluginError> {
            Ok(format!("judge {}", item.key))
        }
        fn call_model(&self, prompt: &str, ctx: &LoopContext) -> Result<ModelAnswer, ModelError> {
            self.prompts.lock().unwrap().push(prompt.to_string());
            self.caps.lock().unwrap().push(ctx.call_budget_usd);
            let a = self
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted answer");
            let cost = self.costs.lock().unwrap().pop_front().unwrap_or(Some(0.01));
            match a {
                Ok(text) => Ok(ModelAnswer {
                    text,
                    cost_usd: cost,
                }),
                Err(m) => Err(ModelError::new("RuntimeError", m)),
            }
        }
        fn after_item(
            &self,
            item: &WorkItem,
            answer: &Value,
            _c: &LoopContext,
        ) -> Result<Outcome, PluginError> {
            match &self.after {
                Some(f) => f(item, answer),
                None => Ok(Outcome::new(
                    &item.key,
                    Status::Done,
                    answer["verdict"].as_str().unwrap_or(""),
                )),
            }
        }
        fn digest(&self, outcomes: &[Outcome], _c: &LoopContext) -> Option<String> {
            Some(format!("{} done", outcomes.len()))
        }
    }

    struct World {
        dir: tempfile::TempDir,
        sent: Arc<Mutex<Vec<String>>>,
        opts: RunOptions,
    }

    fn world() -> World {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir_all(p.join("loops").join("demo")).unwrap();
        std::fs::write(p.join("loops").join("demo").join("loop.yaml"), LOOP_YAML).unwrap();
        let sent: Arc<Mutex<Vec<String>>> = Arc::default();
        let s = sent.clone();
        let opts = RunOptions {
            repo_root: Some(p.to_path_buf()),
            results_dir: Some(p.join("results")),
            events_dir: Some(p.join("events")),
            state_dir: Some(p.join("state")),
            history_dir: Some(p.join("history")),
            loop_lookup: Some(Arc::new(|n: &str| {
                Ok(json!({"name": n, "settings": {"k": "v"}}))
            })),
            notifier: Some(Arc::new(move |_l: &str, t: &str| {
                s.lock().unwrap().push(t.to_string());
                vec![("slack-default".to_string(), true, "sent".to_string())]
            })),
            now: Some(Utc.with_ymd_and_hms(2026, 10, 6, 0, 0, 0).unwrap()),
            ..RunOptions::default()
        };
        World { dir, sent, opts }
    }

    fn item(k: &str) -> WorkItem {
        WorkItem::new(k, "", json!({}))
    }

    fn run(w: &World, p: &Arc<Demo>, id: &str) -> Vec<Outcome> {
        run_plugin(p.clone(), id, w.opts.clone()).unwrap().0
    }

    fn statuses(o: &[Outcome]) -> Vec<&'static str> {
        o.iter().map(|o| o.status.as_str()).collect()
    }

    fn last_run(w: &World) -> Value {
        let p = w.dir.path().join("outputs/loops/demo-loop/last-run.json");
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    const OK: &str = r#"{"verdict": "ok"}"#;

    #[test]
    fn happy_path_processes_items_and_digests() {
        let w = world();
        let p = Arc::new(Demo::new(vec![item("a"), item("b")], &[OK, OK]));
        assert_eq!(statuses(&run(&w, &p, "run_1")), ["done", "done"]);
        assert_eq!(*w.sent.lock().unwrap(), ["2 done"]);
    }

    #[test]
    fn seen_items_are_skipped_next_run() {
        let w = world();
        run(&w, &Arc::new(Demo::new(vec![item("a")], &[OK])), "run_1");
        let p = Arc::new(Demo::new(vec![item("a")], &[]));
        assert!(run(&w, &p, "run_2").is_empty() && p.prompts().is_empty());
        let mut forced = w.opts.clone();
        forced.force = true;
        let p = Arc::new(Demo::new(vec![item("a")], &[OK]));
        assert_eq!(run_plugin(p, "run_3", forced).unwrap().0.len(), 1);
    }

    #[test]
    fn contract_violation_retries_with_feedback() {
        let w = world();
        let p = Arc::new(Demo::new(vec![item("a")], &["not json", OK]));
        assert_eq!(statuses(&run(&w, &p, "run_1")), ["done"]);
        assert!(p.prompts()[1].contains("violated the output contract"));
    }

    #[test]
    fn failed_item_is_not_seen_and_the_run_continues() {
        let w = world();
        let p = Arc::new(
            Demo::new(vec![item("a"), item("b")], &[]).with_answers(vec![
                Err("boom".into()),
                Err("boom".into()),
                Ok(OK.into()),
            ]),
        );
        assert_eq!(statuses(&run(&w, &p, "run_1")), ["failed", "done"]);
        assert!(w
            .sent
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.to_lowercase().contains("failed")));
        let p2 = Arc::new(Demo::new(vec![item("a")], &[OK]));
        assert_eq!(statuses(&run(&w, &p2, "run_2")), ["done"]);
    }

    #[test]
    fn model_crash_summary_has_the_real_cause() {
        let w = world();
        let p = Arc::new(
            Demo::new(vec![item("a")], &[])
                .with_answers(vec![Err("boom".into()), Err("boom".into())]),
        );
        let out = run(&w, &p, "run_1");
        assert!(out[0].summary.contains("boom"), "{}", out[0].summary);
    }

    #[test]
    fn after_item_error_becomes_a_failed_outcome_with_capped_summary() {
        let w = world();
        let mut d = Demo::new(vec![item("a")], &[OK]);
        d.after = Some(Box::new(|_, _| {
            Err(PluginError::new("ValueError", "post failed"))
        }));
        let out = run(&w, &Arc::new(d), "run_1");
        assert!(out[0].status == Status::Failed && out[0].summary.contains("post failed"));
        let w = world();
        let mut d = Demo::new(vec![item("a")], &[OK]);
        d.after = Some(Box::new(|_, _| {
            Err(PluginError::new("RuntimeError", "x".repeat(5000)))
        }));
        let out = run(&w, &Arc::new(d), "run_1");
        assert!(
            out[0].summary.starts_with("RuntimeError: ") && out[0].summary.chars().count() <= 300
        );
    }

    #[test]
    fn discovery_failure_notifies_the_class_only_and_errors() {
        let w = world();
        let mut d = Demo::new(vec![], &[]);
        d.discover_error = true;
        assert!(run_plugin(Arc::new(d), "run_1", w.opts.clone()).is_err());
        let sent = w.sent.lock().unwrap();
        assert!(sent[0].contains("RuntimeError") && !sent[0].contains("api down"));
    }

    #[test]
    fn a_second_run_skips_while_the_lock_is_held() {
        let w = world();
        let lock = w.dir.path().join("lock");
        let held = RunLock::acquire(&lock).unwrap().expect("first lock");
        let mut opts = w.opts.clone();
        opts.lock_path = Some(lock.clone());
        let p = Arc::new(Demo::new(vec![item("a")], &[OK]));
        assert!(
            run_plugin(p.clone(), "run_1", opts.clone())
                .unwrap()
                .0
                .is_empty()
                && p.prompts().is_empty()
        );
        drop(held);
        assert_eq!(run_plugin(p, "run_1", opts).unwrap().0.len(), 1);
    }

    #[test]
    fn writes_history_and_last_run() {
        let w = world();
        let it = item("a").with_url("https://x/a");
        run(&w, &Arc::new(Demo::new(vec![it], &[OK])), "run_1");
        assert_eq!(
            last_run(&w)["counts"],
            json!({"done": 1, "skipped": 0, "failed": 0})
        );
        let hist = std::fs::read_dir(w.dir.path().join("history"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(std::fs::read_to_string(&hist)
            .unwrap()
            .contains("https://x/a"));
        assert_eq!(
            hist.file_name().unwrap().to_str().unwrap(),
            "2026-10-06_000000.md"
        );
    }

    #[test]
    fn outcome_label_names_the_item_in_reports() {
        let w = world();
        let mut d = Demo::new(vec![item("cal:1")], &[OK]);
        d.after = Some(Box::new(|i, _| {
            Ok(Outcome::new(&i.key, Status::Done, "s").with_data(json!({"label": "Team sync"})))
        }));
        run(&w, &Arc::new(d), "run_1");
        assert_eq!(last_run(&w)["outcomes"][0]["label"], "Team sync");
        let hist = std::fs::read_dir(w.dir.path().join("history"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(std::fs::read_to_string(hist).unwrap().contains("Team sync"));
    }

    #[test]
    fn max_items_per_run_caps_the_batch() {
        let w = world();
        let mut d = Demo::new((0..5).map(|i| item(&i.to_string())).collect(), &[OK; 5]);
        d.max = 2;
        assert_eq!(run(&w, &Arc::new(d), "run_1").len(), 2);
    }

    #[test]
    fn a_crash_in_item_two_keeps_item_one_seen_and_continues() {
        let w = world();
        // A file where item b's result directory must go makes its write fail.
        std::fs::create_dir_all(w.dir.path().join("results")).unwrap();
        std::fs::write(
            w.dir
                .path()
                .join("results")
                .join(format!("run_1_{}", slug("b"))),
            "x",
        )
        .unwrap();
        let p = Arc::new(Demo::new(vec![item("a"), item("b"), item("c")], &[OK; 3]));
        let out = run(&w, &p, "run_1");
        assert_eq!(statuses(&out), ["done", "failed", "done"]);
        assert!(out[1].summary.contains("OSError"));
        let p2 = Arc::new(Demo::new(vec![item("a"), item("b"), item("c")], &[OK]));
        let keys: Vec<_> = run(&w, &p2, "run_2")
            .into_iter()
            .map(|o| o.item_key)
            .collect();
        assert_eq!(keys, ["b"]);
    }

    #[test]
    fn definition_verifier_failure_gets_generic_feedback() {
        let w = world();
        let y = LOOP_YAML.replace(
            "retry:",
            "verifiers:\n  - {name: nope, type: command, command: \"false\"}\nretry:",
        );
        std::fs::write(w.dir.path().join("loops/demo/loop.yaml"), y).unwrap();
        let p = Arc::new(Demo::new(vec![item("a")], &[OK, OK]));
        run(&w, &p, "run_1");
        let prompts = p.prompts();
        assert!(
            prompts[1].contains("failed verification") && !prompts[1].contains("output contract")
        );
    }

    #[test]
    fn notify_results_are_recorded_status_only() {
        let mut w = world();
        w.opts.notifier = Some(Arc::new(|_, _| {
            vec![
                ("slack-default".into(), true, "sent".into()),
                ("mail".into(), false, "secret-url".into()),
            ]
        }));
        run(&w, &Arc::new(Demo::new(vec![item("a")], &[OK])), "run_1");
        let raw =
            std::fs::read_to_string(w.dir.path().join("outputs/loops/demo-loop/last-run.json"))
                .unwrap();
        let last: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(last["notified"], true);
        assert_eq!(
            last["notify_targets"],
            json!([{"id": "slack-default", "ok": true}, {"id": "mail", "ok": false}])
        );
        assert!(!raw.contains("secret-url"));
        let log = std::fs::read_to_string(w.dir.path().join("logs/loop-engineering.log")).unwrap();
        assert!(log.contains("mail") && !log.contains("secret-url"));
    }

    #[test]
    fn all_digest_targets_failing_sets_notify_failed_and_the_exit_code() {
        let mut w = world();
        w.opts.notifier = Some(Arc::new(|_, _| {
            vec![
                ("slack-default".into(), false, "HTTP 500".into()),
                ("mail".into(), false, "Boom".into()),
            ]
        }));
        let (out, report) = run_plugin(
            Arc::new(Demo::new(vec![item("a")], &[OK])),
            "run_1",
            w.opts.clone(),
        )
        .unwrap();
        assert!(out[0].status == Status::Done && report.notify_failed);
        assert_eq!(last_run(&w)["notified"], false);
        // items stay seen
        assert!(run_plugin(
            Arc::new(Demo::new(vec![item("a")], &[])),
            "run_2",
            w.opts.clone()
        )
        .unwrap()
        .0
        .is_empty());
        let code = main(
            Arc::new(Demo::new(vec![item("z")], &[OK])),
            &["run_3".to_string()],
            w.opts.clone(),
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn partial_notify_failure_keeps_exit_zero() {
        let mut w = world();
        w.opts.notifier = Some(Arc::new(|_, _| {
            vec![
                ("a".into(), true, "sent".into()),
                ("b".into(), false, "x".into()),
            ]
        }));
        let (_, report) = run_plugin(
            Arc::new(Demo::new(vec![item("a")], &[OK])),
            "run_1",
            w.opts.clone(),
        )
        .unwrap();
        assert!(!report.notify_failed);
        assert_eq!(
            main(Arc::new(Demo::new(vec![], &[])), &[], w.opts.clone()),
            2
        );
    }

    #[test]
    fn no_history_file_without_outcomes_but_last_run_is_written() {
        let w = world();
        run(&w, &Arc::new(Demo::new(vec![], &[])), "run_1");
        assert!(!w.dir.path().join("history").exists());
        assert!(w
            .dir
            .path()
            .join("outputs/loops/demo-loop/last-run.json")
            .exists());
    }

    #[test]
    fn loop_result_goes_to_the_callers_events_dir() {
        let w = world();
        run(&w, &Arc::new(Demo::new(vec![item("a")], &[OK])), "run_1");
        let mut n = 0;
        for f in std::fs::read_dir(w.dir.path().join("events")).unwrap() {
            n += std::fs::read_to_string(f.unwrap().path())
                .unwrap()
                .matches("\"loop.result\"")
                .count();
        }
        assert_eq!(n, 1);
    }

    #[test]
    fn a_retry_gets_only_the_remaining_budget() {
        let w = world();
        let p = Arc::new(
            Demo::new(vec![item("a")], &["not json", OK]).with_costs(&[Some(0.3), Some(0.1)]),
        );
        assert_eq!(statuses(&run(&w, &p, "run_1")), ["done"]);
        let caps = p.caps.lock().unwrap().clone();
        assert_eq!(caps[0], Some(1.0));
        assert!((caps[1].unwrap() - 0.7).abs() < 1e-9);
    }

    #[test]
    fn an_unknown_cost_is_booked_at_the_cap() {
        let w = world();
        let p =
            Arc::new(Demo::new(vec![item("a")], &["not json", OK]).with_costs(&[None, Some(0.01)]));
        run(&w, &p, "run_1");
        assert_eq!(*p.caps.lock().unwrap(), [Some(1.0), Some(0.05)]);
    }

    #[test]
    fn each_item_starts_with_the_full_budget() {
        let w = world();
        let p = Arc::new(
            Demo::new(vec![item("a"), item("b")], &[OK, OK]).with_costs(&[Some(0.9), Some(0.9)]),
        );
        run(&w, &p, "run_1");
        assert_eq!(*p.caps.lock().unwrap(), [Some(1.0), Some(1.0)]);
    }

    #[test]
    fn done_outcome_data_is_archived_and_skipped_is_not() {
        let w = world();
        let mut d = Demo::new(vec![item("a"), item("b")], &[OK, OK]);
        d.after = Some(Box::new(|i, _| {
            Ok(if i.key == "a" {
                Outcome::new("a", Status::Done, "s")
                    .with_url("https://x/1")
                    .with_data(json!({"label": "a", "brief": {"k": 1}}))
            } else {
                Outcome::new("b", Status::Skipped, "nope").with_data(json!({"x": 1}))
            })
        }));
        run(&w, &Arc::new(d), "run_1");
        let recs: Value = serde_json::from_str(
            &std::fs::read_to_string(w.dir.path().join("outputs/loops/demo-loop/content.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(recs.as_array().unwrap().len(), 1);
        assert_eq!(
            (recs[0]["item_key"].as_str(), recs[0]["run_id"].as_str()),
            (Some("a"), Some("run_1"))
        );
        assert_eq!(recs[0]["data"]["brief"], json!({"k": 1}));
        assert_eq!(recs[0]["url"], "https://x/1");
        assert!(recs[0]["saved_at"]
            .as_str()
            .unwrap()
            .starts_with("2026-10-06"));
    }

    #[test]
    fn outcomes_without_data_are_not_archived() {
        let w = world();
        run(&w, &Arc::new(Demo::new(vec![item("a")], &[OK])), "run_1");
        assert!(!w
            .dir
            .path()
            .join("outputs/loops/demo-loop/content.json")
            .exists());
    }

    #[test]
    fn default_call_model_passes_the_call_budget() {
        let w = world();
        let seen: Arc<Mutex<Vec<Option<f64>>>> = Arc::default();
        let s = seen.clone();
        let def = LoopDefinition::from_yaml(&w.dir.path().join("loops/demo/loop.yaml")).unwrap();
        let ctx = LoopContext {
            loop_name: "demo-loop".into(),
            run_id: "r".into(),
            now: Utc::now(),
            definition: Arc::new(def),
            settings: json!({}),
            history_dir: w.dir.path().join("h"),
            log: Arc::new(|_| {}),
            repo_root: w.dir.path().to_path_buf(),
            force: false,
            call_budget_usd: Some(0.4),
            model: Some(Arc::new(move |_p, _t, cap| {
                s.lock().unwrap().push(cap);
                Ok(ModelAnswer {
                    text: "x".into(),
                    cost_usd: Some(0.0),
                })
            })),
            state_dir: None,
        };
        default_call_model("p", &ctx).unwrap();
        let mut full = ctx.clone();
        full.call_budget_usd = None;
        default_call_model("p", &full).unwrap();
        assert_eq!(*seen.lock().unwrap(), [Some(0.4), Some(1.0)]);
    }

    #[test]
    fn default_prompt_substitutes_item_and_settings() {
        let w = world();
        std::fs::write(
            w.dir.path().join("loops/demo/prompt.md"),
            "ITEM {{item_json}} SET {{settings_json}}",
        )
        .unwrap();
        let def = LoopDefinition::from_yaml(&w.dir.path().join("loops/demo/loop.yaml")).unwrap();
        let ctx = LoopContext {
            loop_name: "l".into(),
            run_id: "r".into(),
            now: Utc::now(),
            definition: Arc::new(def),
            settings: json!({"k": "v"}),
            history_dir: w.dir.path().join("h"),
            log: Arc::new(|_| {}),
            repo_root: w.dir.path().to_path_buf(),
            force: false,
            call_budget_usd: None,
            model: None,
            state_dir: None,
        };
        let it = WorkItem::new("a", "T", json!({"p": 1})).with_url("https://x");
        let out = default_build_prompt("demo", &it, &ctx).unwrap();
        assert!(out.starts_with("ITEM {\n  \"key\": \"a\",\n  \"title\": \"T\",\n  \"url\": \"https://x\",\n  \"payload\": {\n    \"p\": 1\n  }\n}"));
        assert!(out.ends_with("SET {\n  \"k\": \"v\"\n}"));
        assert!(default_build_prompt("missing", &it, &ctx).is_err());
    }

    #[test]
    fn parse_answer_strips_fences_and_accepts_free_text() {
        assert_eq!(
            parse_answer(Some("```json\n{\"verdict\": 1}\n```"), &["verdict"]).unwrap(),
            json!({"verdict": 1})
        );
        assert_eq!(
            parse_answer(Some("```\n{\"verdict\": 1}\n```"), &["verdict"]).unwrap(),
            json!({"verdict": 1})
        );
        assert_eq!(parse_answer(Some(" hello "), &[]).unwrap(), json!("hello"));
        assert_eq!(parse_answer(Some("  "), &[]).unwrap_err(), "empty answer");
        assert_eq!(
            parse_answer(None, &["a"]).unwrap_err().split(':').next(),
            Some("answer is not valid JSON")
        );
        assert_eq!(
            parse_answer(Some("[1]"), &["a"]).unwrap_err(),
            "answer must be a JSON object"
        );
        assert_eq!(
            parse_answer(Some("{\"a\": 1}"), &["a", "b", "c"]).unwrap_err(),
            "answer is missing required keys: b, c"
        );
    }

    #[test]
    fn chat_text_neutralises_slack_control_sequences() {
        for evil in ["x|y> <!channel>", "<https://evil|click>", "<@U123> hi"] {
            let out = chat_text(evil, 300);
            assert!(!out.contains('<') && !out.contains('>'), "{out}");
        }
        assert_eq!(chat_text("a\x00b\n\n  c\t", 300), "a b c");
        assert_eq!(
            chat_text(&"x".repeat(400), 10),
            format!("{}\u{2026}", "x".repeat(9))
        );
    }

    #[test]
    fn chat_url_rejects_unsafe_urls() {
        assert_eq!(chat_url("https://gl/x?a=1"), "https://gl/x?a=1");
        for bad in [
            "javascript:alert(1)",
            "https://a b",
            "https://a|b",
            "https://a>b",
            "https://a<b",
            "https://a\"b",
            "",
        ] {
            assert_eq!(chat_url(bad), "", "{bad}");
        }
    }

    #[test]
    fn chat_and_slack_links() {
        assert_eq!(chat_link("Review", "https://gl/1"), "Review (https://gl/1)");
        assert_eq!(
            chat_link("<!channel>", "https://evil|x"),
            "\u{2039}!channel\u{203a}"
        );
        assert_eq!(
            slack_link("https://x.test/a?b=1&c=2", "Fix <!channel> | now"),
            "<https://x.test/a?b=1&c=2|Fix \u{2039}!channel\u{203a} now>"
        );
        assert_eq!(slack_link("javascript:alert(1)", "Fix"), "Fix");
        assert_eq!(slack_link("https://x.test/a>b", "Fix"), "Fix");
    }

    #[test]
    fn slug_is_safe_and_stable() {
        assert!(slug("a/b c").starts_with("a-b-c-"));
        assert_eq!(slug("a/b c"), slug("a/b c"));
        assert_ne!(slug("a/b"), slug("a-b"));
        assert!(slug("///").starts_with("item-"));
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(slug("x").len(), "x-".len() + 8);
    }
}
