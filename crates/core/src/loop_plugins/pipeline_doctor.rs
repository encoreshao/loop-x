//! Pipeline Doctor loop on LoopKit (mirrors `bin/loop_plugins/pipeline_doctor.py`):
//! finds failed CI pipelines (default branch of tracked projects, plus the
//! user's own open MRs), has the model diagnose each from the failed jobs' log
//! tails, and reports the diagnosis through the loop's notifier. Notify only:
//! it never writes to GitLab. Failures whose fingerprint keeps recurring
//! within 7 days are flagged.
#![allow(
    clippy::type_complexity,
    clippy::field_reassign_with_default,
    clippy::let_unit_value
)]

use crate::config::{default_config_path, Config, ConfigError};
use crate::connectors::http::quote;
use crate::connectors::{ApiOpts, Connector, ConnectorSource, LoadFailure};
use crate::loopkit::{
    chat_link, chat_text, sha1_hex, LoopContext, LoopPlugin, Outcome, PluginError, Status, WorkItem,
};
use crate::loops_config::{py_str, py_truthy};
use crate::pyjson;
use crate::seen_store::{parse_iso, py_isoformat};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const TRACE_TAIL_LINES: usize = 300;
pub const MAX_JOBS: usize = 3;
pub const MAX_TRACE_CHARS: usize = 20_000;
pub const RECURRING_WINDOW_DAYS: i64 = 7;
pub const RECURRING_THRESHOLD: usize = 3;
pub const LOOKBACK_HOURS: i64 = 26;
pub const MAX_MR_DETAIL_FETCHES: usize = 20;
pub const CATEGORIES: [&str; 8] = [
    "flaky",
    "infra",
    "test_failure",
    "lint",
    "build",
    "dependency",
    "config",
    "unknown",
];

pub type ProjectsFn = Arc<dyn Fn() -> Result<Vec<Value>, PluginError> + Send + Sync>;

fn load_class(f: &LoadFailure) -> String {
    match f {
        LoadFailure::Unknown => "KeyError".into(),
        LoadFailure::Config(_) => "ConnectorConfigError".into(),
        LoadFailure::Other(class) => class.clone(),
    }
}

// ---- trace text --------------------------------------------------------------------

/// Remove `ESC [ params intermediates final` escape sequences.
fn strip_ansi(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < c.len() {
        if c[i] == '\x1b' && c.get(i + 1) == Some(&'[') {
            let mut j = i + 2;
            while j < c.len() && ('0'..='?').contains(&c[j]) {
                j += 1;
            }
            while j < c.len() && (' '..='/').contains(&c[j]) {
                j += 1;
            }
            if j < c.len() && ('@'..='~').contains(&c[j]) {
                i = j + 1;
                continue;
            }
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

/// Remove GitLab `section_start:<digits>:name` / `section_end:...` markers
/// (through the end of the line's non-newline text and one optional `\r`).
fn strip_sections(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < c.len() {
        if c[i] == 's' {
            let rest: String = c[i..c.len().min(i + 14)].iter().collect();
            let prefix = ["section_start:", "section_end:"]
                .into_iter()
                .find(|p| rest.starts_with(p));
            if let Some(p) = prefix {
                let mut j = i + p.len();
                let digits_start = j;
                while j < c.len() && c[j].is_ascii_digit() {
                    j += 1;
                }
                if j > digits_start && c.get(j) == Some(&':') {
                    j += 1;
                    while j < c.len() && c[j] != '\r' && c[j] != '\n' {
                        j += 1;
                    }
                    if c.get(j) == Some(&'\r') {
                        j += 1;
                    }
                    i = j;
                    continue;
                }
            }
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

/// Last `n` lines of a CI log with ANSI escapes and GitLab section markers removed.
pub fn trace_tail(text: &str, n: usize) -> String {
    let cleaned = strip_ansi(&strip_sections(text));
    let cleaned = strip_ansi(&cleaned.replace("\r\n", "\n").replace('\r', "\n"));
    let mut lines: Vec<&str> = cleaned.split('\n').collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if n == 0 {
        return String::new();
    }
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Lowercase, collapse whitespace and replace digit/hex runs with `#`, so line
/// numbers, shas and job indexes do not split one recurring failure.
fn normalise(text: &str) -> String {
    let collapsed = text
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = String::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.chars().any(|c| c.is_ascii_digit()) {
            out.push('#');
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in collapsed.chars() {
        if c.is_ascii_digit() || ('a'..='f').contains(&c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

pub fn fingerprint(job_name: &str, category: &str, culprit: &str) -> String {
    let raw = [job_name, category, culprit]
        .iter()
        .map(|p| normalise(p))
        .collect::<Vec<_>>()
        .join("\n");
    sha1_hex(raw.as_bytes())[..12].to_string()
}

// ---- fingerprint store ---------------------------------------------------------------

pub type NowFn = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// `{fingerprint: [iso timestamps]}` kept for 7 days, written atomically.
pub struct FingerprintStore {
    path: PathBuf,
    now_fn: Option<NowFn>,
}

impl FingerprintStore {
    pub fn new(path: &Path, now_fn: Option<NowFn>) -> Self {
        Self {
            path: path.to_path_buf(),
            now_fn,
        }
    }

    fn now(&self) -> DateTime<Utc> {
        self.now_fn.as_ref().map_or_else(Utc::now, |f| f())
    }

    fn load(&self) -> Map<String, Value> {
        read_object(&self.path)
    }

    fn recorded_path(&self) -> PathBuf {
        self.path.with_file_name("recorded_pipelines.json")
    }

    fn write(path: &Path, data: &Map<String, Value>) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tmp = path.with_file_name(format!("{name}.tmp{}", std::process::id()));
        std::fs::write(&tmp, pyjson::to_string_pretty(&Value::Object(data.clone())))?;
        std::fs::rename(&tmp, path)
    }

    /// Pipelines already counted within the window (iso strings compared as
    /// text, like Python).
    fn recorded(&self, now: DateTime<Utc>) -> Map<String, Value> {
        let cutoff = py_isoformat(now - Duration::days(RECURRING_WINDOW_DAYS));
        read_object(&self.recorded_path())
            .into_iter()
            .filter(|(_, v)| v.as_str().is_some_and(|s| s >= cutoff.as_str()))
            .collect()
    }

    /// Count this occurrence; with `pipeline_key`, each pipeline counts once (a
    /// `--force` re-diagnosis only reads the current count).
    pub fn record(&self, fp: &str, pipeline_key: Option<&str>) -> std::io::Result<bool> {
        let now = self.now();
        let mut recorded = if pipeline_key.is_some() {
            self.recorded(now)
        } else {
            Map::new()
        };
        if pipeline_key.is_some_and(|k| recorded.contains_key(k)) {
            let count = self
                .load()
                .get(fp)
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            return Ok(count >= RECURRING_THRESHOLD);
        }
        let cutoff = now - Duration::days(RECURRING_WINDOW_DAYS);
        let mut fresh = Map::new();
        for (key, stamps) in self.load() {
            let kept: Vec<Value> = stamps
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str().and_then(parse_iso))
                .filter(|when| *when >= cutoff)
                .map(|when| Value::String(py_isoformat(when)))
                .collect();
            if !kept.is_empty() {
                fresh.insert(key, Value::Array(kept));
            }
        }
        let entry = fresh.entry(fp.to_string()).or_insert_with(|| json!([]));
        if let Value::Array(a) = entry {
            a.push(Value::String(py_isoformat(now)));
        }
        let count = fresh[fp].as_array().map_or(0, Vec::len);
        Self::write(&self.path, &fresh)?;
        if let Some(key) = pipeline_key {
            recorded.insert(key.to_string(), Value::String(py_isoformat(now)));
            Self::write(&self.recorded_path(), &recorded)?;
        }
        Ok(count >= RECURRING_THRESHOLD)
    }
}

fn read_object(path: &Path) -> Map<String, Value> {
    match std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

// ---- projects -------------------------------------------------------------------------

/// Tracked projects from `projects.json` as `{alias, gitlab_instance,
/// project_id (id or path), default_branch (or null)}`.
pub fn default_projects_from(path: &Path) -> Result<Vec<Value>, PluginError> {
    let config = Config::load(path).map_err(|e| match e {
        ConfigError::Missing(_) => PluginError::new("FileNotFoundError", e.to_string()),
        ConfigError::Parse(..) => PluginError::new("JSONDecodeError", e.to_string()),
        _ => PluginError::new("OSError", e.to_string()),
    })?;
    let global = config
        .top_level("gitlab_instance")
        .cloned()
        .unwrap_or(Value::Null);
    let mut rows = Vec::new();
    let empty = Map::new();
    let projects = config
        .top_level("projects")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    for (alias, project) in projects {
        let instance = project
            .get("instance")
            .filter(|v| py_truthy(v))
            .cloned()
            .unwrap_or_else(|| global.clone());
        rows.push(json!({
            "alias": alias,
            "gitlab_instance": instance,
            "project_id": project.get("project_id").cloned().unwrap_or(Value::Null),
            "default_branch": project.get("default_branch").cloned().unwrap_or(Value::Null),
        }));
    }
    Ok(rows)
}

pub fn default_projects() -> Result<Vec<Value>, PluginError> {
    default_projects_from(&default_config_path())
}

fn is_digits(v: &Value) -> bool {
    let s = py_str(v);
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

fn key_err(key: &str) -> PluginError {
    PluginError::new("KeyError", key)
}

// ---- the plugin ------------------------------------------------------------------------

pub struct PipelineDoctor {
    connectors: Arc<dyn ConnectorSource>,
    projects_fn: Option<ProjectsFn>,
    state_dir: Option<PathBuf>,
}

impl PipelineDoctor {
    pub fn new(connectors: Arc<dyn ConnectorSource>) -> Self {
        Self {
            connectors,
            projects_fn: None,
            state_dir: None,
        }
    }

    /// Override where tracked projects come from (`projects_fn`).
    pub fn with_projects(mut self, f: ProjectsFn) -> Self {
        self.projects_fn = Some(f);
        self
    }

    /// Override the fingerprint directory (`state_dir`).
    pub fn with_state_dir(mut self, dir: &Path) -> Self {
        self.state_dir = Some(dir.to_path_buf());
        self
    }

    fn project_ref(
        &self,
        conn: &dyn Connector,
        project: &Value,
    ) -> Result<(Value, String), PluginError> {
        let mut pid = project
            .get("project_id")
            .cloned()
            .ok_or_else(|| key_err("project_id"))?;
        let branch = project
            .get("default_branch")
            .filter(|v| py_truthy(v))
            .map(py_str);
        if !is_digits(&pid) || branch.is_none() {
            let info = conn.api(
                "GET",
                &format!("/projects/{}", quote(&py_str(&pid))),
                &ApiOpts::default(),
            )?;
            if !is_digits(&pid) {
                pid = info.get("id").cloned().ok_or_else(|| key_err("id"))?;
            }
            let branch = branch
                .or_else(|| {
                    info.get("default_branch")
                        .filter(|v| py_truthy(v))
                        .map(py_str)
                })
                .unwrap_or_else(|| "main".into());
            return Ok((pid, branch));
        }
        Ok((pid, branch.unwrap_or_default()))
    }

    fn my_mr_pipelines(
        &self,
        conn: &dyn Connector,
        names: &HashMap<String, String>,
    ) -> Result<Vec<(Value, String, Value)>, PluginError> {
        let me = conn.api("GET", "/user", &ApiOpts::default())?;
        let username = me.get("username").ok_or_else(|| key_err("username"))?;
        let rows = conn.api(
            "GET",
            &format!(
                "/merge_requests?author_username={}&state=opened&scope=all&per_page=50",
                quote(&py_str(username))
            ),
            &ApiOpts::default(),
        )?;
        let mut out = Vec::new();
        let mut fallbacks = 0;
        for mr in rows.as_array().into_iter().flatten() {
            let Some(obj) = mr.as_object() else { continue };
            let pipe = if obj.contains_key("head_pipeline") {
                obj["head_pipeline"].clone()
            } else if fallbacks < MAX_MR_DETAIL_FETCHES {
                // The list endpoint does not always include head_pipeline.
                fallbacks += 1;
                let (Some(pid), Some(iid)) = (obj.get("project_id"), obj.get("iid")) else {
                    continue;
                };
                let path = format!("/projects/{}/merge_requests/{}", py_str(pid), py_str(iid));
                match conn.api("GET", &path, &ApiOpts::default()) {
                    Ok(detail) => detail.get("head_pipeline").cloned().unwrap_or(Value::Null),
                    Err(_) => continue,
                }
            } else {
                continue;
            };
            if pipe.is_object() && pipe.get("status").and_then(Value::as_str) == Some("failed") {
                let pid = obj.get("project_id").ok_or_else(|| key_err("project_id"))?;
                let label = names
                    .get(&py_str(pid))
                    .filter(|l| !l.is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("project {}", py_str(pid)));
                out.push((pid.clone(), label, pipe));
            }
        }
        Ok(out)
    }

    fn item(
        &self,
        conn: &dyn Connector,
        key: &str,
        aid: &str,
        pid: &Value,
        label: &str,
        pipe: &Value,
    ) -> Result<WorkItem, PluginError> {
        let pipe_id = pipe.get("id").ok_or_else(|| key_err("id"))?;
        let pid_s = py_str(pid);
        let jobs = conn.api(
            "GET",
            &format!(
                "/projects/{pid_s}/pipelines/{}/jobs?scope[]=failed",
                py_str(pipe_id)
            ),
            &ApiOpts::default(),
        )?;
        let mut out = Vec::new();
        for job in jobs.as_array().into_iter().flatten().take(MAX_JOBS) {
            let trace = job
                .get("id")
                .and_then(|id| {
                    conn.api_text(&format!("/projects/{pid_s}/jobs/{}/trace", py_str(id)))
                        .ok()
                })
                .map(|t| {
                    let tail = trace_tail(&t, TRACE_TAIL_LINES);
                    let n = tail.chars().count();
                    tail.chars()
                        .skip(n.saturating_sub(MAX_TRACE_CHARS))
                        .collect::<String>()
                })
                .unwrap_or_default();
            let field = |k: &str| {
                job.get(k)
                    .filter(|v| py_truthy(v))
                    .map(py_str)
                    .unwrap_or_default()
            };
            out.push(json!({"name": field("name"), "stage": field("stage"),
                            "web_url": field("web_url"), "trace_tail": trace}));
        }
        let url = pipe
            .get("web_url")
            .filter(|v| py_truthy(v))
            .map(py_str)
            .unwrap_or_default();
        Ok(WorkItem::new(
            key,
            format!("{label} #{}", py_str(pipe_id)),
            json!({"account": aid, "project": label, "project_id": pid, "pipeline_id": pipe_id,
                   "web_url": url, "jobs": out}),
        )
        .with_url(url))
    }

    fn store(&self, ctx: &LoopContext) -> FingerprintStore {
        let base = self.state_dir.clone().unwrap_or_else(|| {
            ctx.repo_root
                .join("outputs")
                .join("loops")
                .join(self.loop_name())
        });
        let now = ctx.now;
        FingerprintStore::new(&base.join("fingerprints.json"), Some(Arc::new(move || now)))
    }
}

impl LoopPlugin for PipelineDoctor {
    fn loop_name(&self) -> &str {
        "pipeline-doctor-loop"
    }
    fn definition_dir(&self) -> &str {
        "pipeline-doctor"
    }
    fn max_items_per_run(&self) -> usize {
        10
    }
    fn output_keys(&self) -> &[&'static str] {
        &[
            "category",
            "culprit",
            "explanation",
            "suggested_fix",
            "confidence",
        ]
    }

    fn discover(&self, ctx: &LoopContext) -> Result<Vec<WorkItem>, PluginError> {
        let projects = match &self.projects_fn {
            Some(f) => f(),
            None => default_projects(),
        };
        let projects = match projects {
            Ok(p) => p,
            Err(e) => {
                ctx.log(&format!(
                    "pipeline-doctor: project config failed: {}",
                    e.class
                ));
                return Ok(Vec::new());
            }
        };
        let accounts: Vec<_> = self
            .connectors
            .accounts_with_capability("pipelines")
            .into_iter()
            .filter(|a| a.kind == "gitlab")
            .collect();
        let since = (ctx.now - Duration::hours(LOOKBACK_HOURS))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let mut items = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for account in accounts {
            let aid = account.id;
            let mine: Vec<&Value> = projects
                .iter()
                .filter(|p| p.get("gitlab_instance").and_then(Value::as_str) == Some(aid.as_str()))
                .collect();
            if mine.is_empty() {
                continue;
            }
            let conn = match self.connectors.load(&aid) {
                Ok(c) => c,
                Err(f) => {
                    ctx.log(&format!(
                        "pipeline-doctor: account {aid} failed: {}",
                        load_class(&f)
                    ));
                    continue;
                }
            };
            let conn = conn.as_ref();
            let mut names: HashMap<String, String> = HashMap::new();
            let mut found: Vec<(Value, String, Value)> = Vec::new();
            for project in mine {
                let alias_of = || project.get("alias").map(py_str);
                let result: Result<(), PluginError> = (|| {
                    let (pid, branch) = self.project_ref(conn, project)?;
                    let alias = project.get("alias").ok_or_else(|| key_err("alias"))?;
                    names.insert(py_str(&pid), py_str(alias));
                    let rows = conn.api(
                        "GET",
                        &format!(
                            "/projects/{}/pipelines?status=failed&ref={}&updated_after={since}&per_page=20",
                            py_str(&pid),
                            quote(&branch)
                        ),
                        &ApiOpts::default(),
                    )?;
                    for r in rows
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|r| r.is_object())
                    {
                        found.push((pid.clone(), py_str(alias), r.clone()));
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    ctx.log(&format!(
                        "pipeline-doctor: project {} failed: {}",
                        alias_of().unwrap_or_else(|| "None".into()),
                        e.class
                    ));
                }
            }
            match self.my_mr_pipelines(conn, &names) {
                Ok(more) => found.extend(more),
                Err(e) => ctx.log(&format!(
                    "pipeline-doctor: account {aid} MRs failed: {}",
                    e.class
                )),
            }
            for (pid, label, pipe) in found {
                let id = pipe.get("id").cloned().unwrap_or(Value::Null);
                let key = format!("pipe:{aid}:{}#{}", py_str(&pid), py_str(&id));
                if seen.contains(&key) || id.is_null() {
                    continue;
                }
                seen.insert(key.clone());
                match self.item(conn, &key, &aid, &pid, &label, &pipe) {
                    Ok(item) => items.push(item),
                    Err(e) => ctx.log(&format!(
                        "pipeline-doctor: pipeline #{} failed to load: {}",
                        py_str(&id),
                        e.class
                    )),
                }
            }
        }
        Ok(items)
    }

    fn after_item(
        &self,
        item: &WorkItem,
        answer: &Value,
        ctx: &LoopContext,
    ) -> Result<Outcome, PluginError> {
        let payload = &item.payload;
        let category = answer
            .get("category")
            .and_then(Value::as_str)
            .filter(|c| CATEGORIES.contains(c))
            .unwrap_or("unknown");
        let culprit: String = answer
            .get("culprit")
            .and_then(Value::as_str)
            .map(|c| c.trim().chars().take(200).collect())
            .unwrap_or_default();
        let text = |key: &str, max: usize| -> String {
            answer
                .get(key)
                .filter(|v| py_truthy(v))
                .map(py_str)
                .unwrap_or_default()
                .trim()
                .chars()
                .take(max)
                .collect()
        };
        let explanation = text("explanation", 1000);
        let fix = text("suggested_fix", 200);
        let confidence = match answer.get("confidence") {
            Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
            Some(Value::Bool(b)) => f64::from(u8::from(*b)),
            Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(0.0),
            _ => 0.0,
        };
        let confidence = if confidence.is_nan() {
            0.0
        } else {
            confidence.clamp(0.0, 1.0)
        };
        let first_job = payload
            .get("jobs")
            .and_then(Value::as_array)
            .and_then(|j| j.first())
            .and_then(|j| j.get("name"))
            .map(py_str)
            .unwrap_or_default();
        let recurring = self
            .store(ctx)
            .record(
                &fingerprint(&first_job, category, &culprit),
                Some(&item.key),
            )
            .map_err(|e| PluginError::new("OSError", e.to_string()))?;
        let get = |k: &str| payload.get(k).map_or("None".to_string(), py_str);
        let summary = format!(
            "[{category}] {} #{}: {culprit}",
            get("project"),
            get("pipeline_id")
        );
        let url = if item.url.is_empty() {
            payload.get("web_url").map(py_str).unwrap_or_default()
        } else {
            item.url.clone()
        };
        Ok(Outcome::new(&item.key, Status::Done, summary)
            .with_url(url)
            .with_data(json!({
            "category": category, "culprit": culprit, "explanation": explanation,
            "suggested_fix": fix, "confidence": confidence, "recurring": recurring})))
    }

    fn digest(&self, outcomes: &[Outcome], _ctx: &LoopContext) -> Option<String> {
        let done: Vec<&Outcome> = outcomes
            .iter()
            .filter(|o| o.status == Status::Done)
            .collect();
        if done.is_empty() {
            return None;
        }
        let is_recurring = |o: &Outcome| o.data.get("recurring").is_some_and(py_truthy);
        let category = |o: &Outcome| {
            o.data
                .get("category")
                .filter(|v| py_truthy(v))
                .map_or("unknown".to_string(), py_str)
        };
        let recurring: Vec<&Outcome> = done.iter().copied().filter(|o| is_recurring(o)).collect();
        let mut lines = vec![format!(
            "Pipeline Doctor: {} failed pipelines diagnosed ({} recurring).",
            done.len(),
            recurring.len()
        )];
        let line = |o: &Outcome, mark: &str| {
            let text = format!("{mark}{}", chat_link(&o.summary, &o.url));
            let fix = chat_text(
                &o.data
                    .get("suggested_fix")
                    .filter(|v| py_truthy(v))
                    .map(py_str)
                    .unwrap_or_default(),
                200,
            );
            if fix.is_empty() {
                text
            } else {
                format!("{text} - fix: {fix}")
            }
        };
        if !recurring.is_empty() {
            lines.push("Recurring:".into());
            lines.extend(
                recurring
                    .iter()
                    .map(|o| format!("- {}", line(o, "\u{1F501} "))),
            );
        }
        let rest: Vec<&Outcome> = done.iter().copied().filter(|o| !is_recurring(o)).collect();
        let cats: BTreeSet<String> = rest.iter().map(|o| category(o)).collect();
        for cat in cats {
            lines.push(format!("{}:", chat_text(&cat, 300)));
            lines.extend(
                rest.iter()
                    .filter(|o| category(o) == cat)
                    .map(|o| format!("- {}", line(o, ""))),
            );
        }
        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{gitlab, Account, ConnResult, ConnectorError, HttpError, TypeInfo};
    use crate::loop_definition::LoopDefinition;
    use crate::loopkit::LogFn;
    use chrono::TimeZone;
    use std::sync::Mutex;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 6, 9, 0, 0).unwrap()
    }

    fn repo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn ctx_at(root: &Path) -> (LoopContext, Arc<Mutex<Vec<String>>>) {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let sink = logs.clone();
        let log: LogFn = Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_string()));
        let d = LoopDefinition::from_yaml(&repo().join("loops/pipeline-doctor/loop.yaml")).unwrap();
        (
            LoopContext {
                loop_name: "pipeline-doctor-loop".into(),
                run_id: "r".into(),
                now: now(),
                definition: Arc::new(d),
                settings: json!({}),
                history_dir: PathBuf::new(),
                log,
                repo_root: root.to_path_buf(),
                force: false,
                call_budget_usd: None,
                model: None,
                state_dir: None,
            },
            logs,
        )
    }

    fn ctx() -> (LoopContext, Arc<Mutex<Vec<String>>>) {
        ctx_at(Path::new("/nonexistent"))
    }

    // ---- fake GitLab ----

    type Handler = Box<dyn Fn(&str) -> Option<ConnResult<Value>> + Send + Sync>;

    struct G {
        account: Account,
        over: Handler,
        trace: Box<dyn Fn(&str) -> ConnResult<String> + Send + Sync>,
        calls: Mutex<Vec<(String, String)>>,
        texts: Mutex<Vec<String>>,
    }

    impl G {
        fn new() -> Self {
            Self {
                account: Account::new("work", "gitlab"),
                over: Box::new(|_| None),
                trace: Box::new(|_| Ok("boom\n".into())),
                calls: Mutex::new(vec![]),
                texts: Mutex::new(vec![]),
            }
        }
        fn over(
            mut self,
            f: impl Fn(&str) -> Option<ConnResult<Value>> + Send + Sync + 'static,
        ) -> Self {
            self.over = Box::new(f);
            self
        }
        fn paths(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|c| c.1.clone())
                .collect()
        }
    }

    struct Conn(Arc<G>);

    impl Connector for Conn {
        fn info(&self) -> &'static TypeInfo {
            &gitlab::INFO
        }
        fn account(&self) -> &Account {
            &self.0.account
        }
        fn api(&self, method: &str, path: &str, _o: &ApiOpts) -> ConnResult<Value> {
            self.0
                .calls
                .lock()
                .unwrap()
                .push((method.into(), path.into()));
            if let Some(r) = (self.0.over)(path) {
                return r;
            }
            if path == "/user" {
                return Ok(json!({"id": 1, "username": "me"}));
            }
            if path.contains("/pipelines?status=failed") {
                return Ok(json!([{"id": 100, "web_url": "https://gl/p/100"}]));
            }
            if path.starts_with("/merge_requests?") {
                return Ok(json!([
                    {"iid": 7, "project_id": 9, "head_pipeline": {"id": 200, "status": "failed", "web_url": "https://gl/p/200"}},
                    {"iid": 8, "project_id": 9, "head_pipeline": {"id": 100, "status": "failed", "web_url": "https://gl/p/100"}},
                    {"iid": 9, "project_id": 9, "head_pipeline": {"id": 300, "status": "success"}}]));
            }
            if path.contains("/jobs?scope") {
                return Ok(
                    json!([{"id": 5, "name": "rspec", "stage": "test", "web_url": "https://gl/j/5"}]),
                );
            }
            panic!("unexpected {path}")
        }
        fn api_text(&self, path: &str) -> ConnResult<String> {
            self.0.texts.lock().unwrap().push(path.to_string());
            (self.0.trace)(path)
        }
    }

    struct Source(Vec<(String, Arc<G>)>);

    impl ConnectorSource for Source {
        fn accounts_with_capability(&self, _c: &str) -> Vec<Account> {
            self.0
                .iter()
                .map(|(id, _)| Account::new(id, "gitlab"))
                .collect()
        }
        fn load(&self, id: &str) -> Result<Box<dyn Connector>, LoadFailure> {
            self.0
                .iter()
                .find(|(k, _)| k == id)
                .map(|(_, g)| Box::new(Conn(g.clone())) as Box<dyn Connector>)
                .ok_or(LoadFailure::Unknown)
        }
    }

    fn web() -> Value {
        json!({"alias": "web", "gitlab_instance": "work", "project_id": 9, "default_branch": "main"})
    }

    fn doctor(g: &Arc<G>, projects: Vec<Value>) -> PipelineDoctor {
        PipelineDoctor::new(Arc::new(Source(vec![("work".into(), g.clone())])))
            .with_projects(Arc::new(move || Ok(projects.clone())))
    }

    fn bare() -> PipelineDoctor {
        PipelineDoctor::new(Arc::new(Source(vec![])))
    }

    // ---- pure helpers ----

    #[test]
    fn trace_tail_only() {
        let text = (0..1000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = trace_tail(&text, 300);
        assert_eq!(tail.lines().next().unwrap(), "line 700");
        assert_eq!(tail.lines().count(), 300);
        assert_eq!(trace_tail("a\nb", 0), "");
    }

    #[test]
    fn trace_tail_strips_ansi_and_sections() {
        let raw = "\x1b[31mERROR\x1b[0m boom\nsection_start:1700000000:step_script\r\x1b[0Kok\n";
        assert_eq!(trace_tail(raw, TRACE_TAIL_LINES), "ERROR boom\nok");
        assert_eq!(trace_tail("section_end:12:x\r\nnext", 10), "\nnext");
    }

    #[test]
    fn recurring_after_three_in_window() {
        let d = tempfile::tempdir().unwrap();
        let store = FingerprintStore::new(&d.path().join("fp.json"), Some(Arc::new(now)));
        for _ in 0..2 {
            assert!(!store.record("abc", None).unwrap());
        }
        assert!(store.record("abc", None).unwrap());
    }

    #[test]
    fn old_occurrences_fall_out_of_window() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("fp.json");
        let old = py_isoformat(now() - Duration::days(8));
        std::fs::write(&path, json!({"abc": [old, old, old]}).to_string()).unwrap();
        let store = FingerprintStore::new(&path, Some(Arc::new(now)));
        assert!(!store.record("abc", None).unwrap());
        let data: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(data["abc"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn corrupt_store_starts_empty() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("fp.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(!FingerprintStore::new(&path, Some(Arc::new(now)))
            .record("x", None)
            .unwrap());
    }

    #[test]
    fn fingerprint_is_stable_12_hex() {
        let fp = fingerprint("rspec", "flaky", "spec/x_spec.rb");
        assert_eq!(fp, fingerprint("rspec", "flaky", "spec/x_spec.rb"));
        assert_eq!(fp.len(), 12);
        assert_ne!(fp, fingerprint("lint", "flaky", "spec/x_spec.rb"));
        assert_ne!(fp, fingerprint("rspec", "lint", "spec/x_spec.rb"));
    }

    #[test]
    fn fingerprint_normalises_case_space_and_digits() {
        let a = fingerprint(
            "rspec 3/10",
            "flaky",
            "Timeout  in  spec/x_spec.rb:42 (abc123def)",
        );
        let b = fingerprint(
            "RSPEC 7/10",
            "flaky",
            "timeout in spec/x_spec.rb:99 (0badf00d1)",
        );
        assert_eq!(a, b);
        assert_eq!(normalise("Foo  Bar a1 deadbeef"), "foo bar # deadbeef");
    }

    #[test]
    fn recorded_non_dict_json_treated_as_empty() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("recorded_pipelines.json"), "[1, 2]").unwrap();
        let store = FingerprintStore::new(&d.path().join("fp.json"), Some(Arc::new(now)));
        assert!(!store.record("abc", Some("pipe:1")).unwrap());
    }

    // ---- discovery ----

    #[test]
    fn discover_failed_default_branch_and_my_mr_pipelines() {
        let g = Arc::new(G::new());
        let (c, _) = ctx();
        let mut keys: Vec<_> = doctor(&g, vec![web()])
            .discover(&c)
            .unwrap()
            .into_iter()
            .map(|i| i.key)
            .collect();
        keys.sort();
        assert_eq!(keys, ["pipe:work:9#100", "pipe:work:9#200"]);
    }

    #[test]
    fn discover_only_issues_gets_and_builds_payload() {
        let g = Arc::new(G::new());
        let (c, _) = ctx();
        let items = doctor(&g, vec![web()]).discover(&c).unwrap();
        assert!(g.calls.lock().unwrap().iter().all(|(m, _)| m == "GET"));
        assert!(g.paths().iter().any(|p| p.contains("ref=main")
            && p.contains("updated_after=2026-10-05T07:00:00Z")
            && p.contains("per_page=20")));
        let item = items.iter().find(|i| i.key == "pipe:work:9#100").unwrap();
        assert_eq!(item.payload["project"], "web");
        assert_eq!(item.payload["pipeline_id"], 100);
        assert_eq!(
            item.payload["jobs"],
            json!([{"name": "rspec", "stage": "test", "web_url": "https://gl/j/5", "trace_tail": "boom"}])
        );
        assert_eq!(item.url, "https://gl/p/100");
        assert!(g
            .texts
            .lock()
            .unwrap()
            .contains(&"/projects/9/jobs/5/trace".to_string()));
    }

    #[test]
    fn discover_resolves_path_to_id_and_default_branch() {
        let g = Arc::new(G::new().over(|p| {
            (p == "/projects/loop%2Fweb").then(|| Ok(json!({"id": 9, "default_branch": "trunk"})))
        }));
        let (c, _) = ctx();
        let items = doctor(
            &g,
            vec![json!({"alias": "web", "gitlab_instance": "work", "project_id": "loop/web"})],
        )
        .discover(&c)
        .unwrap();
        assert!(g
            .paths()
            .iter()
            .any(|p| p.contains("/projects/9/pipelines?status=failed&ref=trunk")));
        let mut keys: Vec<_> = items.iter().map(|i| i.key.clone()).collect();
        keys.sort();
        assert_eq!(keys, ["pipe:work:9#100", "pipe:work:9#200"]);
    }

    #[test]
    fn jobs_capped_and_trace_capped_and_failures_isolated() {
        let mut g = G::new().over(|p| {
            p.contains("/jobs?scope").then(|| {
                Ok(Value::Array(
                    (0..10).map(|i| json!({"id": i, "name": format!("j{i}"), "stage": "t", "web_url": ""})).collect(),
                ))
            })
        });
        g.trace = Box::new(|p| {
            if p.ends_with("/jobs/1/trace") {
                Err(ConnectorError::Http(HttpError::new(Some(500), "", "")))
            } else {
                Ok("x".repeat(100_000))
            }
        });
        let g = Arc::new(g);
        let (c, _) = ctx();
        let item = doctor(&g, vec![web()]).discover(&c).unwrap().remove(0);
        let jobs = item.payload["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), MAX_JOBS);
        assert_eq!(jobs[1]["trace_tail"], "");
        assert!(jobs[0]["trace_tail"].as_str().unwrap().chars().count() <= MAX_TRACE_CHARS);
    }

    #[test]
    fn account_failure_logs_class_name_only_and_others_continue() {
        let bad = Arc::new(G::new().over(|_| {
            Some(Err(ConnectorError::Http(HttpError::new(
                Some(500),
                "tok-secret",
                "https://gl/secret",
            ))))
        }));
        let ok = Arc::new(G::new());
        let projects = vec![
            json!({"alias": "a", "gitlab_instance": "bad", "project_id": 1, "default_branch": "main"}),
            web(),
        ];
        let p = PipelineDoctor::new(Arc::new(Source(vec![
            ("bad".into(), bad),
            ("work".into(), ok),
        ])))
        .with_projects(Arc::new(move || Ok(projects.clone())));
        let (c, logs) = ctx();
        assert_eq!(p.discover(&c).unwrap().len(), 2);
        let logs = logs.lock().unwrap();
        assert!(!logs.is_empty() && logs.iter().all(|m| !m.contains("secret")));
        assert!(logs.iter().any(|m| m.contains("MailHTTPError")));
    }

    #[test]
    fn projects_for_other_instances_skipped() {
        let g = Arc::new(G::new());
        let (c, _) = ctx();
        let items = doctor(
            &g,
            vec![json!({"alias": "x", "gitlab_instance": "other", "project_id": 9, "default_branch": "main"})],
        )
        .discover(&c)
        .unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn project_config_failure_is_logged_and_yields_nothing() {
        let g = Arc::new(G::new());
        let p = PipelineDoctor::new(Arc::new(Source(vec![("work".into(), g)])))
            .with_projects(Arc::new(|| Err(PluginError::new("FileNotFoundError", "x"))));
        let (c, logs) = ctx();
        assert!(p.discover(&c).unwrap().is_empty());
        assert!(logs.lock().unwrap()[0].contains("FileNotFoundError"));
    }

    #[test]
    fn default_projects_fn_maps_config() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("projects.json");
        std::fs::write(
            &path,
            json!({"gitlab_instance": "loop", "projects": {
                "web": {"project_id": "loop/web", "default_branch": "main"},
                "h": {"project_id": "o/h", "instance": "other"}}})
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            default_projects_from(&path).unwrap(),
            vec![
                json!({"alias": "web", "gitlab_instance": "loop", "project_id": "loop/web", "default_branch": "main"}),
                json!({"alias": "h", "gitlab_instance": "other", "project_id": "o/h", "default_branch": null}),
            ]
        );
        assert_eq!(
            default_projects_from(&d.path().join("none.json"))
                .unwrap_err()
                .class,
            "FileNotFoundError"
        );
    }

    #[test]
    fn mr_without_head_pipeline_in_list_falls_back_to_detail_gets_only() {
        let g = Arc::new(G::new().over(|p| {
            if p.starts_with("/merge_requests?") {
                return Some(Ok(json!([{"iid": 7, "project_id": 9}])));
            }
            (p == "/projects/9/merge_requests/7").then(|| {
                Ok(json!({"head_pipeline": {"id": 777, "status": "failed", "web_url": "https://gl/p/777"}}))
            })
        }));
        let (c, _) = ctx();
        let keys: Vec<_> = doctor(&g, vec![web()])
            .discover(&c)
            .unwrap()
            .into_iter()
            .map(|i| i.key)
            .collect();
        assert!(keys.contains(&"pipe:work:9#777".to_string()));
        assert!(g.calls.lock().unwrap().iter().all(|(m, _)| m == "GET"));
    }

    #[test]
    fn mr_detail_fallback_bounded_to_20() {
        let details = Arc::new(Mutex::new(0usize));
        let counter = details.clone();
        let g = Arc::new(G::new().over(move |p| {
            if p.starts_with("/merge_requests?") {
                return Some(Ok(Value::Array(
                    (0..30)
                        .map(|i| json!({"iid": i, "project_id": 9}))
                        .collect(),
                )));
            }
            if p.contains("/merge_requests/") {
                *counter.lock().unwrap() += 1;
                return Some(Ok(json!({"head_pipeline": null})));
            }
            None
        }));
        let (c, _) = ctx();
        doctor(&g, vec![web()]).discover(&c).unwrap();
        assert_eq!(*details.lock().unwrap(), 20);
    }

    // ---- after_item / digest ----

    fn item(pid: i64) -> WorkItem {
        WorkItem::new(
            format!("pipe:work:9#{pid}"),
            format!("web #{pid}"),
            json!({"account": "work", "project": "web", "project_id": 9, "pipeline_id": pid,
                   "jobs": [{"name": "rspec", "stage": "test", "web_url": "u", "trace_tail": "x"}]}),
        )
        .with_url(format!("https://gl/p/{pid}"))
    }

    fn flaky() -> Value {
        json!({"category": "flaky", "culprit": "spec/x_spec.rb", "explanation": "Timeout\nmore",
               "suggested_fix": "retry", "confidence": 0.8})
    }

    #[test]
    fn after_item_records_fingerprint_and_flags_recurring() {
        let d = tempfile::tempdir().unwrap();
        let (c, _) = ctx_at(d.path());
        let p = bare().with_state_dir(&d.path().join("state"));
        let outs: Vec<_> = (0..3)
            .map(|i| p.after_item(&item(100 + i), &flaky(), &c).unwrap())
            .collect();
        let rec: Vec<_> = outs.iter().map(|o| o.data["recurring"].clone()).collect();
        assert_eq!(rec, [json!(false), json!(false), json!(true)]);
        assert_eq!(outs[0].status, Status::Done);
        assert_eq!(outs[0].summary, "[flaky] web #100: spec/x_spec.rb");
        assert_eq!(outs[0].url, "https://gl/p/100");
        assert_eq!(outs[0].data["category"], "flaky");
        assert!(d.path().join("state/fingerprints.json").exists());
    }

    #[test]
    fn recurrence_ignores_explanation_wording() {
        let d = tempfile::tempdir().unwrap();
        let (c, _) = ctx_at(d.path());
        let p = bare().with_state_dir(&d.path().join("state"));
        let rec: Vec<_> = (0..3)
            .map(|i| {
                let mut a = flaky();
                a["explanation"] = json!(format!("Different words {i}"));
                p.after_item(&item(100 + i), &a, &c).unwrap().data["recurring"].clone()
            })
            .collect();
        assert_eq!(rec, [json!(false), json!(false), json!(true)]);
    }

    #[test]
    fn after_item_default_store_under_outputs_loops() {
        let d = tempfile::tempdir().unwrap();
        let (c, _) = ctx_at(d.path());
        bare()
            .after_item(
                &item(100),
                &json!({"category": "lint", "culprit": "", "explanation": "e", "suggested_fix": "f", "confidence": 1}),
                &c,
            )
            .unwrap();
        assert!(d
            .path()
            .join("outputs/loops/pipeline-doctor-loop/fingerprints.json")
            .exists());
    }

    #[test]
    fn after_item_normalises_bad_answer() {
        let d = tempfile::tempdir().unwrap();
        let (c, _) = ctx_at(d.path());
        let out = bare()
            .with_state_dir(d.path())
            .after_item(
                &item(100),
                &json!({"category": "wizardry", "culprit": 5, "explanation": "e".repeat(5000),
                        "suggested_fix": "f".repeat(900), "confidence": 7}),
                &c,
            )
            .unwrap();
        assert_eq!(out.data["category"], "unknown");
        assert_eq!(out.data["confidence"], 1.0);
        assert_eq!(out.data["culprit"], "");
        assert!(out.data["suggested_fix"].as_str().unwrap().len() <= 200);
        assert!(out.data["explanation"].as_str().unwrap().len() <= 1000);
    }

    #[test]
    fn force_rediagnosis_does_not_rerecord_fingerprint() {
        let d = tempfile::tempdir().unwrap();
        let (c, _) = ctx_at(d.path());
        let p = bare().with_state_dir(&d.path().join("s"));
        let a = json!({"category": "flaky", "culprit": "x", "explanation": "Timeout", "suggested_fix": "f", "confidence": 1});
        for _ in 0..5 {
            assert_eq!(
                p.after_item(&item(100), &a, &c).unwrap().data["recurring"],
                false
            );
        }
        let fps: Value = serde_json::from_str(
            &std::fs::read_to_string(d.path().join("s/fingerprints.json")).unwrap(),
        )
        .unwrap();
        let lens: Vec<_> = fps
            .as_object()
            .unwrap()
            .values()
            .map(|v| v.as_array().unwrap().len())
            .collect();
        assert_eq!(lens, [1]);
    }

    #[test]
    fn digest_puts_recurring_first() {
        let outs = [
            Outcome::new("a", Status::Done, "[lint] web #1: rubocop")
                .with_url("https://gl/p/1")
                .with_data(json!({"category": "lint", "recurring": false, "suggested_fix": "run rubocop -a"})),
            Outcome::new("b", Status::Done, "[flaky] web #2: spec/x_spec.rb")
                .with_url("https://gl/p/2")
                .with_data(json!({"category": "flaky", "recurring": true, "suggested_fix": "quarantine"})),
        ];
        let (c, _) = ctx();
        let text = bare().digest(&outs, &c).unwrap();
        assert!(text.find("https://gl/p/2").unwrap() < text.find("https://gl/p/1").unwrap());
        assert!(text
            .split("https://gl/p/2")
            .next()
            .unwrap()
            .contains('\u{1F501}'));
        assert!(text.starts_with("Pipeline Doctor: 2 failed pipelines diagnosed (1 recurring)."));
        assert!(
            text.contains("lint:\n- [lint] web #1: rubocop (https://gl/p/1) - fix: run rubocop -a")
        );
    }

    #[test]
    fn digest_chat_safe_capped_and_empty_none() {
        let o = Outcome::new("a", Status::Done, "[lint] web #1: <!channel>")
            .with_url("https://gl/p/1")
            .with_data(json!({"category": "lint", "recurring": false,
                              "suggested_fix": format!("<@U1> {}", "z".repeat(500))}));
        let (c, _) = ctx();
        let text = bare().digest(&[o], &c).unwrap();
        assert!(!text.contains('<') && !text.contains('>') && text.len() < 600);
        assert!(bare().digest(&[], &c).is_none());
    }

    #[test]
    fn definition_files_and_template() {
        let d = LoopDefinition::from_yaml(&repo().join("loops/pipeline-doctor/loop.yaml")).unwrap();
        assert_eq!(d.name, "pipeline-doctor-loop");
        let prompt =
            std::fs::read_to_string(repo().join("loops/pipeline-doctor/prompt.md")).unwrap();
        assert_eq!(prompt.matches("{{item_json}}").count(), 1);
        assert!(prompt.contains("untrusted"));
        let loops: Value = serde_json::from_str(
            &std::fs::read_to_string(repo().join("config/loops.json.template")).unwrap(),
        )
        .unwrap();
        let e = loops
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["name"] == "pipeline-doctor-loop")
            .unwrap();
        assert_eq!(e["requires"], json!(["pipelines"]));
        assert_eq!(e["routes_notifications"], true);
        assert_eq!(
            e["schedule"],
            json!({"frequency": "hourly", "interval_hours": 1})
        );
        assert_eq!(e["timeout_seconds"], 1800);
        assert_eq!(e["enabled"], false);
        assert_eq!(e["entry_point"], "bin.loop_plugins.pipeline_doctor");
    }
}
