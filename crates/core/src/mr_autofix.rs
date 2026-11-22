//! Pipeline Auto-fix (`mr-autofix-loop`): an event-triggered loop that gives a
//! red pipeline on a merge request the issue loop opened exactly one automatic
//! Fix now attempt, then escalates to Needs you and Slack.
//!
//! The event log is untrusted (agents write it too), so every event is
//! re-verified against GitLab before anything is spent, and the attempt ledger
//! (`<home>/mr-autofix/attempts.json`) is keyed by GitLab's own ids for the
//! merge request, never by the event's spelling. An attempt is recorded before
//! its job is spawned, so a crash can never give a merge request a second one.
//! Each run handles one event; the rest wait in the inbox for the next run.

use crate::event_triggers::{self, RETRY_AFTER};
use crate::file_lock;
use crate::mr_fix::jobs;
use crate::mr_fix::run::CiFailure;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const LOOP_NAME: &str = "mr-autofix-loop";
pub const MAX_ATTEMPTS_PER_MR: usize = 1;
pub const DEFAULT_MAX_PER_DAY: usize = 3;
pub const DEFAULT_MIN_CONFIDENCE: f64 = 0.7;
/// Under the registry's 45-minute run timeout, so the run can still escalate.
const FIX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(40 * 60);
/// A `started` attempt older than this outlived any run that could finish it.
const STALE_AFTER_MINUTES: i64 = 50;
/// A GitLab check that keeps failing gives up after this many runs or this age.
const MAX_VERIFY_TRIES: u64 = 5;
const VERIFY_GIVE_UP_HOURS: i64 = 24;
const VERIFY_FAILURES: &str = "_verify_failures";
const ESCALATED_KEY: &str = "escalated";

#[derive(Debug, Clone, PartialEq)]
pub struct Verified {
    /// `account:<target project id>!<iid>`, as Fix now jobs are keyed.
    pub mr_ref: String,
    /// `<gitlab host>/<target project id>!<iid>`: the ledger's key for this merge request.
    pub key: String,
    pub title: String,
    pub web_url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VerifyError {
    /// GitLab's answer rules the event out; it is dropped.
    Refused(String),
    /// GitLab could not be asked (network, token); the event is retried.
    Transient(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum FixOutcome {
    Done(String),
    Nothing(String),
    Failed(String),
    TimedOut,
}

pub trait AutofixApi {
    fn verify(&self, ev: &Value) -> Result<Verified, VerifyError>;
    /// A Fix now job is already running on the merge request.
    fn job_running(&self, mr_ref: &str) -> bool;
    /// Starts a CI-mode Fix now job; returns its id.
    fn start_fix(&self, mr_ref: &str, title: &str, ci: &CiFailure) -> Result<String, String>;
    fn wait(&self, job_id: &str, timeout: std::time::Duration) -> FixOutcome;
}

#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    pub max_per_day: usize,
    pub min_confidence: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_per_day: DEFAULT_MAX_PER_DAY,
            min_confidence: DEFAULT_MIN_CONFIDENCE,
        }
    }
}

impl Limits {
    /// From the registry entry's `settings`; bad or missing values keep the defaults.
    pub fn from_settings(settings: &Value) -> Self {
        let d = Self::default();
        let num = |k: &str| {
            settings.get(k).and_then(|v| {
                v.as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            })
        };
        Self {
            max_per_day: num("max_attempts_per_day")
                .filter(|n| *n >= 0.0)
                .map_or(d.max_per_day, |n| n as usize),
            min_confidence: num("min_confidence")
                .filter(|n| (0.0..=1.0).contains(n))
                .unwrap_or(d.min_confidence),
        }
    }
}

pub fn ledger_path(loopx_home: &Path) -> PathBuf {
    loopx_home.join("mr-autofix").join("attempts.json")
}

/// A missing ledger is empty; one that exists but cannot be read is an error,
/// so it is never mistaken for "no attempts yet" and overwritten.
fn load_ledger(path: &Path) -> Result<Map<String, Value>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err(format!("{} is not a JSON object", path.display())),
    }
}

/// Read-modify-write of the ledger under its lock; an unreadable ledger is left as it is.
fn update_ledger<T>(
    path: &Path,
    f: impl FnOnce(&mut Map<String, Value>) -> T,
) -> Result<T, String> {
    file_lock::with_lock(path, || {
        let mut ledger = load_ledger(path)?;
        let out = f(&mut ledger);
        let text = crate::pyjson::to_string_pretty(&Value::Object(ledger)) + "\n";
        file_lock::write_atomic(path, text.as_bytes())
            .map(|()| out)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    })
    .map_err(|e| format!("cannot lock {}: {e}", path.display()))?
}

fn attempts<'a>(ledger: &'a Map<String, Value>, key: &str) -> &'a [Value] {
    ledger
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn attempts_since(ledger: &Map<String, Value>, since: DateTime<Utc>) -> usize {
    ledger
        .iter()
        .filter(|(k, _)| k.as_str() != ESCALATED_KEY)
        .filter_map(|(_, v)| v.as_array())
        .flatten()
        .filter_map(|a| a.get("at").and_then(Value::as_str))
        .filter_map(crate::seen_store::parse_iso)
        .filter(|at| *at > since)
        .count()
}

fn set_result(path: &Path, key: &str, event_id: &str, job_id: Option<&str>, result: &str) {
    let updated = update_ledger(path, |ledger| {
        let Some(list) = ledger.get_mut(key).and_then(Value::as_array_mut) else {
            return;
        };
        if let Some(a) = list.iter_mut().rev().find(|a| a["event_id"] == event_id) {
            if let Some(id) = job_id {
                a["job_id"] = json!(id);
            }
            a["result"] = json!(result);
        }
    });
    if let Err(e) = updated {
        eprintln!("mr-autofix: {e}");
    }
}

pub fn ci_failure_of(ev: &Value) -> CiFailure {
    let d = &ev["data"];
    let s = |k: &str| {
        d.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    CiFailure {
        pipeline_id: d.get("pipeline_id").and_then(Value::as_i64).unwrap_or(0),
        sha: s("sha"),
        category: s("category"),
        culprit: s("culprit"),
        explanation: s("explanation"),
        suggested_fix: s("suggested_fix"),
        failed_jobs: d
            .get("failed_jobs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        web_url: s("web_url"),
    }
}

/// What one run works with.
pub struct Ctx<'a> {
    pub api: &'a dyn AutofixApi,
    pub ledger: &'a Path,
    /// `<home>/event_triggers`, where the loop's inbox lives.
    pub inbox_dir: &'a Path,
    pub events_dir: Option<&'a Path>,
    pub notify: &'a dyn Fn(&str),
    pub limits: &'a Limits,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Default)]
pub struct RunReport {
    pub lines: Vec<String>,
    /// An event was taken from the inbox (handled, dropped or put back).
    pub took_event: bool,
}

impl Ctx<'_> {
    fn emit(&self, event_type: &str, data: Value) {
        let run_id = std::env::var("LOOP_RUN_ID")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| LOOP_NAME.to_string());
        let fields = crate::events::EventFields {
            data: Some(data),
            ..Default::default()
        };
        if let Err(e) = crate::events::emit(event_type, &run_id, fields, self.events_dir) {
            eprintln!("mr-autofix: emitting {event_type} failed: {e}");
        }
    }

    fn announce(&self, mr_ref: &str, web_url: &str, sha: &str, pipeline_id: Value, reason: &str) {
        self.emit(
            "mr_autofix.escalated",
            json!({"mr_ref": mr_ref, "mr_web_url": web_url, "sha": sha,
                   "pipeline_id": pipeline_id, "reason": reason}),
        );
        let mut line = format!(
            "Pipeline Auto-fix stopped on {mr_ref} ({})",
            reason_words(reason)
        );
        if !web_url.is_empty() {
            line.push_str(&format!(": {web_url}"));
        }
        (self.notify)(&line);
    }

    /// One escalation per `(merge request, sha)`: the event, a Slack line, and
    /// (through the event) a Needs you item.
    fn escalate(&self, v: &Verified, sha: &str, pipeline_id: i64, reason: &str) -> String {
        let key = format!("{}@{sha}", v.key);
        let fresh = update_ledger(self.ledger, |ledger| {
            let list = ledger.entry(ESCALATED_KEY).or_insert_with(|| json!([]));
            if !list.is_array() {
                *list = json!([]);
            }
            let list = list.as_array_mut().expect("just made an array");
            if list.iter().any(|k| k == key.as_str()) {
                return false;
            }
            list.push(json!(key));
            true
        });
        match fresh {
            Ok(true) => {}
            Ok(false) => return format!("{}: {reason}, already escalated", v.mr_ref),
            Err(e) => return format!("{}: {reason}, not escalated: {e}", v.mr_ref),
        }
        self.announce(&v.mr_ref, &v.web_url, sha, json!(pipeline_id), reason);
        format!("{}: escalated ({reason})", v.mr_ref)
    }

    /// An unreadable ledger stops every run; it is reported once per state of the file.
    fn ledger_unreadable(&self, why: &str) -> String {
        let meta = std::fs::metadata(self.ledger).ok();
        let signature = format!(
            "{}-{}",
            meta.as_ref().map_or(0, |m| m.len()),
            meta.and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs())
        );
        let marker = self.ledger.with_file_name("attempts.json.reported");
        if std::fs::read_to_string(&marker).ok().as_deref() == Some(signature.as_str()) {
            return format!("ledger unreadable ({why}); already reported");
        }
        self.announce(
            "attempts.json",
            "",
            &signature,
            Value::Null,
            "ledger_corrupt",
        );
        if let Err(e) = std::fs::write(&marker, &signature) {
            eprintln!("mr-autofix: cannot record the ledger report: {e}");
        }
        format!("ledger unreadable ({why}); nothing started, reported")
    }

    /// Attempts still `started` long after any run could have finished them: the
    /// run that made them was stopped or killed. Each is escalated once.
    fn sweep_stale(&self, ledger: &Map<String, Value>) -> Vec<String> {
        let cutoff = self.now - Duration::minutes(STALE_AFTER_MINUTES);
        let mut out = Vec::new();
        for (key, list) in ledger.iter().filter(|(k, _)| k.as_str() != ESCALATED_KEY) {
            for a in list.as_array().into_iter().flatten() {
                let at = a
                    .get("at")
                    .and_then(Value::as_str)
                    .and_then(crate::seen_store::parse_iso);
                if a["result"] != "started" || at.is_none_or(|t| t > cutoff) {
                    continue;
                }
                let text = |k: &str| {
                    a.get(k)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                };
                let mr_ref = text("mr_ref");
                if !mr_ref.is_empty() && self.api.job_running(&mr_ref) {
                    continue;
                }
                set_result(self.ledger, key, &text("event_id"), None, "timed_out");
                let v = Verified {
                    mr_ref: if mr_ref.is_empty() {
                        key.clone()
                    } else {
                        mr_ref
                    },
                    key: key.clone(),
                    title: String::new(),
                    web_url: text("web_url"),
                };
                let pipeline = a.get("pipeline_id").and_then(Value::as_i64).unwrap_or(0);
                out.push(self.escalate(&v, &text("sha"), pipeline, "fix_failed"));
            }
        }
        out
    }

    /// GitLab could not be asked: the event goes back with a growing delay, and
    /// after too many tries or too long it is escalated once instead.
    fn retry_or_give_up(&self, mut ev: Value, why: &str) -> String {
        let event_id = ev["event_id"].as_str().unwrap_or("?").to_string();
        let tries = ev.get(VERIFY_FAILURES).and_then(Value::as_u64).unwrap_or(0) + 1;
        let born = ev["timestamp"]
            .as_str()
            .and_then(crate::seen_store::parse_iso);
        let too_old = born.is_none_or(|t| self.now - t > Duration::hours(VERIFY_GIVE_UP_HOURS));
        if tries < MAX_VERIFY_TRIES && !too_old {
            ev[VERIFY_FAILURES] = json!(tries);
            let delay = Duration::minutes(5 * (1 << (tries - 1)));
            ev[RETRY_AFTER] = json!(crate::events::fmt_timestamp(self.now + delay));
            event_triggers::put_back(self.inbox_dir, LOOP_NAME, ev);
            return format!("{event_id}: GitLab check failed ({why}); retrying later");
        }
        let mr_ref = mr_ref_of(&ev).map_or_else(|_| event_id.clone(), |(r, _, _)| r);
        let ci = ci_failure_of(&ev);
        let v = Verified {
            key: format!("unverified:{mr_ref}"),
            mr_ref,
            title: String::new(),
            web_url: String::new(),
        };
        self.escalate(&v, &ci.sha, ci.pipeline_id, "verify_failed")
    }

    fn handle(&self, ev: Value) -> String {
        let event_id = ev["event_id"].as_str().unwrap_or("?").to_string();
        let v = match self.api.verify(&ev) {
            Ok(v) => v,
            Err(VerifyError::Refused(e)) => return format!("{event_id}: dropped: {e}"),
            Err(VerifyError::Transient(e)) => return self.retry_or_give_up(ev, &e),
        };
        let ci = ci_failure_of(&ev);
        let escalate = |reason: &str| self.escalate(&v, &ci.sha, ci.pipeline_id, reason);
        let confidence = ev["data"]["confidence"].as_f64().unwrap_or(0.0);
        if confidence < self.limits.min_confidence {
            return escalate("low_confidence");
        }
        if self.api.job_running(&v.mr_ref) {
            return escalate("job_busy");
        }
        let (now, limits) = (self.now, self.limits);
        let attempt = json!({"sha": ci.sha, "pipeline_id": ci.pipeline_id, "event_id": event_id,
            "job_id": null, "at": now.to_rfc3339_opts(SecondsFormat::Secs, true), "result": "started",
            "mr_ref": v.mr_ref, "web_url": v.web_url});
        // Checked and recorded under one lock, so two runs never both record an attempt.
        let recorded = update_ledger(self.ledger, |ledger| {
            if attempts(ledger, &v.key).len() >= MAX_ATTEMPTS_PER_MR {
                return Some("already_attempted");
            }
            if attempts_since(ledger, now - Duration::hours(24)) >= limits.max_per_day {
                return Some("daily_cap");
            }
            let list = ledger.entry(v.key.clone()).or_insert_with(|| json!([]));
            match list.as_array_mut() {
                Some(a) => a.push(attempt),
                None => *list = json!([attempt]),
            }
            None
        });
        match recorded {
            Ok(None) => {}
            Ok(Some(reason)) => return escalate(reason),
            Err(e) => return format!("{}: not started: {e}", v.mr_ref),
        }
        let job_id = match self.api.start_fix(&v.mr_ref, &v.title, &ci) {
            Ok(id) => id,
            Err(e) => {
                set_result(self.ledger, &v.key, &event_id, None, "failed");
                eprintln!("mr-autofix: {}: could not start Fix now: {e}", v.mr_ref);
                return escalate("fix_failed");
            }
        };
        set_result(self.ledger, &v.key, &event_id, Some(&job_id), "started");
        let outcome = self.api.wait(&job_id, FIX_TIMEOUT);
        let (result, reason) = match &outcome {
            FixOutcome::Done(_) => ("done", None),
            FixOutcome::Nothing(_) => ("nothing", Some("fix_nothing")),
            FixOutcome::Failed(_) => ("failed", Some("fix_failed")),
            FixOutcome::TimedOut => ("timed_out", Some("fix_failed")),
        };
        set_result(self.ledger, &v.key, &event_id, Some(&job_id), result);
        match (outcome, reason) {
            (FixOutcome::Done(summary), _) => {
                self.emit(
                    "mr_autofix.completed",
                    json!({"mr_ref": v.mr_ref, "sha": ci.sha, "job_id": job_id, "summary": summary}),
                );
                format!("{}: fixed ({summary})", v.mr_ref)
            }
            (_, Some(reason)) => escalate(reason),
            (_, None) => unreachable!("every non-done outcome has a reason"),
        }
    }
}

/// One run: stop on an unreadable ledger, escalate attempts left `started` by a
/// run that was stopped, then handle at most one due event from the inbox.
pub fn run_once(ctx: &Ctx<'_>) -> RunReport {
    let mut report = RunReport::default();
    let ledger = match load_ledger(ctx.ledger) {
        Ok(l) => l,
        Err(e) => {
            report.lines.push(ctx.ledger_unreadable(&e));
            return report;
        }
    };
    report.lines.extend(ctx.sweep_stale(&ledger));
    if let Some(ev) = event_triggers::take_ready_one(ctx.inbox_dir, LOOP_NAME, ctx.now) {
        report.took_event = true;
        report.lines.push(ctx.handle(ev));
    }
    report
}

/// Plain words for an escalation reason (Slack and logs).
pub fn reason_words(reason: &str) -> &'static str {
    match reason {
        "already_attempted" => "it already had its one automatic attempt",
        "fix_failed" => "the automatic fix failed",
        "fix_nothing" => "the automatic fix changed nothing",
        "daily_cap" => "the daily limit of automatic fixes was reached",
        "low_confidence" => "the diagnosis was not confident enough",
        "job_busy" => "a Fix now run was already in progress",
        "verify_failed" => "GitLab could not be checked",
        "ledger_corrupt" => "its attempt ledger is unreadable",
        _ => "it needs a human",
    }
}

/// The registry's kill switch: a switched-off loop starts nothing and leaves its inbox alone.
pub fn is_enabled(entry: &Value) -> bool {
    entry.get("enabled").and_then(Value::as_bool) == Some(true)
}

/// Why GitLab's view of the merge request rules out an automatic push, if it does.
pub fn check_mr(ev: &Value, mr: &Value, me: &str) -> Result<(), String> {
    let d = &ev["data"];
    let s = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    if s(mr, "state") != "opened" {
        return Err(format!("the merge request is {}", s(mr, "state")));
    }
    if ["draft", "work_in_progress"]
        .iter()
        .any(|k| mr.get(*k).and_then(Value::as_bool) == Some(true))
    {
        return Err("the merge request is a draft".into());
    }
    let src = mr.get("source_project_id").and_then(Value::as_i64);
    let dst = mr.get("target_project_id").and_then(Value::as_i64);
    if src.is_none() || src != dst {
        return Err("the merge request comes from a fork".into());
    }
    let branch = s(mr, "source_branch");
    if !crate::providers::branch::is_any_loop_branch(&branch) {
        return Err(format!("{branch} is not a loop branch"));
    }
    let author = mr.pointer("/author/username").and_then(Value::as_str);
    if me.is_empty() || author != Some(me) {
        return Err("the merge request is not the token user's own".into());
    }
    let sha = s(d, "sha");
    if sha.is_empty() || s(mr, "sha") != sha {
        return Err("the branch moved since the diagnosis".into());
    }
    let pipe = mr.get("head_pipeline").unwrap_or(&Value::Null);
    let pid = d.get("pipeline_id").and_then(Value::as_i64);
    if pid.is_none() || pipe.get("id").and_then(Value::as_i64) != pid {
        return Err("the diagnosed pipeline is not the head pipeline".into());
    }
    if pipe.get("status").and_then(Value::as_str) != Some("failed") {
        return Err("the head pipeline is not failed".into());
    }
    Ok(())
}

/// The GitLab host of a merge request page (`https://host[:port]/...`), lower-cased.
fn instance_of(web_url: &str) -> Option<String> {
    let rest = web_url
        .strip_prefix("https://")
        .or_else(|| web_url.strip_prefix("http://"))?;
    let host = rest.split('/').next().filter(|h| !h.is_empty())?;
    Some(host.to_ascii_lowercase())
}

/// The checked merge request, named by GitLab's own ids (target project id and
/// iid, plus the instance), however the event spelled the project.
pub fn verified_from(ev: &Value, mr: &Value, me: &str, account: &str) -> Result<Verified, String> {
    check_mr(ev, mr, me)?;
    let project = mr
        .get("target_project_id")
        .and_then(Value::as_i64)
        .ok_or("GitLab gave no target project id")?;
    let iid = mr
        .get("iid")
        .and_then(Value::as_i64)
        .ok_or("GitLab gave no merge request iid")?;
    let web_url = mr
        .get("web_url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let host = instance_of(web_url).ok_or("GitLab gave no merge request URL")?;
    let mr_ref = format!("{account}:{project}!{iid}");
    if !crate::mr_fix::valid_mr_ref(&mr_ref) {
        return Err(format!("bad merge request reference {mr_ref:?}"));
    }
    Ok(Verified {
        mr_ref,
        key: format!("{host}/{project}!{iid}"),
        title: mr
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        web_url: web_url.to_string(),
    })
}

/// `account:project!iid` as the event spells it, when it names an MR pipeline.
/// Only for reaching GitLab and for display; never a ledger key.
pub fn mr_ref_of(ev: &Value) -> Result<(String, String, i64), String> {
    let d = &ev["data"];
    let account = d.get("account").and_then(Value::as_str).unwrap_or_default();
    let project = match d.get("project_id") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let iid = d
        .get("mr_iid")
        .and_then(Value::as_i64)
        .ok_or("not a merge request pipeline")?;
    let mr_ref = format!("{account}:{project}!{iid}");
    if !crate::mr_fix::valid_mr_ref(&mr_ref) {
        return Err(format!("bad merge request reference {mr_ref:?}"));
    }
    Ok((mr_ref, project, iid))
}

/// GitLab through the user's connectors, and Fix now jobs under the loopx home.
pub struct RealApi {
    pub connectors: crate::connectors_config::ConfigConnectors,
    pub jobs_dir: PathBuf,
    pub exe: String,
}

impl AutofixApi for RealApi {
    fn verify(&self, ev: &Value) -> Result<Verified, VerifyError> {
        let (_, project, iid) = mr_ref_of(ev).map_err(VerifyError::Refused)?;
        let account = ev["data"]["account"].as_str().unwrap_or_default();
        let client = self
            .connectors
            .gitlab_client(account)
            .map_err(VerifyError::Transient)?;
        let me = client
            .current_user()
            .map_err(|e| VerifyError::Transient(e.to_string()))?
            .get("username")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mr = client
            .get_merge_request(&project, iid)
            .map_err(|e| VerifyError::Transient(e.to_string()))?;
        verified_from(ev, &mr, &me, account).map_err(VerifyError::Refused)
    }

    fn job_running(&self, mr_ref: &str) -> bool {
        jobs::running(&jobs::list(&self.jobs_dir), mr_ref).is_some()
    }

    fn start_fix(&self, mr_ref: &str, title: &str, ci: &CiFailure) -> Result<String, String> {
        let spawn = |log: &Path| {
            let file = log.with_extension("ci.json");
            let text = serde_json::to_string_pretty(ci).map_err(|e| e.to_string())?;
            write_private(&file, &text)?;
            let extra = ["--ci-failure".to_string(), file.display().to_string()];
            jobs::spawn_runner(log, &self.exe, mr_ref, &extra)
        };
        jobs::start(&self.jobs_dir, mr_ref, title, &spawn).map(|j| j.id)
    }

    /// Polls the job; on timeout, or when this run is being stopped (SIGTERM),
    /// stops the job first so nothing is pushed after the run gave up on it.
    fn wait(&self, job_id: &str, timeout: std::time::Duration) -> FixOutcome {
        let deadline = std::time::Instant::now() + timeout;
        let stop = || {
            if let Err(e) = jobs::stop(&self.jobs_dir, job_id) {
                eprintln!("mr-autofix: could not stop job {job_id}: {e}");
            }
        };
        loop {
            match jobs::get(&self.jobs_dir, job_id) {
                None => return FixOutcome::Failed("the job record vanished".into()),
                Some(j) if j.state != "running" => return outcome_of(&j.state, &j.summary),
                Some(_) if crate::loopkit::terminated() => {
                    stop();
                    return FixOutcome::Failed("the Pipeline Auto-fix run was stopped".into());
                }
                Some(_) if std::time::Instant::now() >= deadline => {
                    stop();
                    return FixOutcome::TimedOut;
                }
                Some(_) => std::thread::sleep(std::time::Duration::from_secs(1)),
            }
        }
    }
}

fn write_private(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    f.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

/// A settled Fix now job as an outcome; "Fixed 0 of N" pushed nothing.
pub fn outcome_of(state: &str, summary: &str) -> FixOutcome {
    match state {
        "done" if summary.starts_with("Fixed 0 ") => FixOutcome::Nothing(summary.to_string()),
        "done" => FixOutcome::Done(summary.to_string()),
        "nothing" => FixOutcome::Nothing(summary.to_string()),
        _ => FixOutcome::Failed(summary.to_string()),
    }
}

/// `loopx _entry bin.mr_autofix <run_id>`.
pub fn main(argv: &[String]) -> i32 {
    if argv.is_empty() {
        eprintln!("Usage: {LOOP_NAME} <run_id>");
        return 2;
    }
    crate::loopkit::install_sigterm_flag();
    let home = crate::config::home_dir();
    let entry = match crate::loops_config::get_loop(
        LOOP_NAME,
        &crate::loops_config::default_config_path(),
        None,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("mr-autofix: {e}");
            return 1;
        }
    };
    if !is_enabled(&entry) {
        println!("mr-autofix: the loop is switched off; nothing done");
        return crate::run_loop::PRECHECK_SKIPPED_EXIT;
    }
    let limits = Limits::from_settings(entry.get("settings").unwrap_or(&Value::Null));
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "loopx".into());
    let api = RealApi {
        connectors: crate::connectors_config::ConfigConnectors::real(),
        jobs_dir: jobs::jobs_dir(&home),
        exe,
    };
    let notify = |line: &str| {
        crate::runner_util::notify_slack_best_effort(line, None, None);
    };
    let (ledger, inbox_dir) = (ledger_path(&home), home.join("event_triggers"));
    let report = run_once(&Ctx {
        api: &api,
        ledger: &ledger,
        inbox_dir: &inbox_dir,
        events_dir: None,
        notify: &notify,
        limits: &limits,
        now: Utc::now(),
    });
    for line in &report.lines {
        println!("mr-autofix: {line}");
    }
    if report.lines.is_empty() {
        println!("mr-autofix: no pending events");
        return crate::run_loop::PRECHECK_SKIPPED_EXIT;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    type VerifyFn = Box<dyn Fn(&Value) -> Result<Verified, VerifyError>>;

    /// GitLab's answer for the merge request an event names: always project 9.
    fn gitlab_mr(ev: &Value) -> Value {
        let d = &ev["data"];
        let iid = d["mr_iid"].as_i64().unwrap_or(0);
        json!({"iid": iid, "state": "opened", "draft": false, "source_project_id": 9, "target_project_id": 9,
            "source_branch": format!("feature#{iid}-fix"), "author": {"username": "me"}, "sha": d["sha"],
            "head_pipeline": {"id": d["pipeline_id"], "status": "failed"}, "title": format!("Fix #{iid}"),
            "web_url": format!("https://GitLab.example.com/acme/web/-/merge_requests/{iid}")})
    }

    struct Fake {
        verify: VerifyFn,
        busy: Cell<bool>,
        outcome: RefCell<FixOutcome>,
        started: RefCell<Vec<(String, CiFailure)>>,
        ledger: PathBuf,
        seen_in_ledger: RefCell<Vec<bool>>,
    }

    impl Fake {
        fn new(ledger: &Path) -> Self {
            Fake {
                verify: Box::new(|ev| {
                    verified_from(ev, &gitlab_mr(ev), "me", "work").map_err(VerifyError::Refused)
                }),
                busy: Cell::new(false),
                outcome: RefCell::new(FixOutcome::Done("Fixed 1 of 1".into())),
                started: RefCell::new(Vec::new()),
                ledger: ledger.to_path_buf(),
                seen_in_ledger: RefCell::new(Vec::new()),
            }
        }
    }

    impl AutofixApi for Fake {
        fn verify(&self, ev: &Value) -> Result<Verified, VerifyError> {
            (self.verify)(ev)
        }
        fn job_running(&self, _mr_ref: &str) -> bool {
            self.busy.get()
        }
        fn start_fix(&self, mr_ref: &str, _title: &str, ci: &CiFailure) -> Result<String, String> {
            let ledger = load_ledger(&self.ledger).unwrap();
            let recorded = ledger
                .values()
                .filter_map(Value::as_array)
                .flatten()
                .any(|a| a["pipeline_id"] == ci.pipeline_id && a["result"] == "started");
            self.seen_in_ledger.borrow_mut().push(recorded);
            self.started
                .borrow_mut()
                .push((mr_ref.to_string(), ci.clone()));
            Ok(format!("job{}", self.started.borrow().len()))
        }
        fn wait(&self, _job_id: &str, _t: std::time::Duration) -> FixOutcome {
            self.outcome.borrow().clone()
        }
    }

    fn ev(id: &str, sha: &str, pipeline: i64, confidence: f64, iid: i64) -> Value {
        json!({"event_id": id, "event_type": "pipeline_doctor.diagnosed", "timestamp": "2026-11-22T10:00:00.000Z",
               "data": {"account": "work", "project_id": 9, "mr_iid": iid, "pipeline_id": pipeline, "sha": sha,
                        "category": "test_failure", "confidence": confidence, "culprit": "spec/x_spec.rb",
                        "explanation": "e", "suggested_fix": "f", "failed_jobs": ["rspec"],
                        "web_url": format!("https://gl/p/{pipeline}"), "mr_owned_by_loop": true}})
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-22T12:00:00Z")
            .unwrap()
            .to_utc()
    }

    const KEY7: &str = "gitlab.example.com/9!7";

    struct Fx {
        _tmp: tempfile::TempDir,
        ledger: PathBuf,
        events: PathBuf,
        inbox: PathBuf,
        slack: RefCell<Vec<String>>,
    }

    impl Fx {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            Fx {
                ledger: tmp.path().join("mr-autofix/attempts.json"),
                events: tmp.path().join("events"),
                inbox: tmp.path().join("event_triggers"),
                _tmp: tmp,
                slack: RefCell::new(Vec::new()),
            }
        }
        fn push(&self, events: Vec<Value>) {
            for e in events.into_iter().rev() {
                event_triggers::put_back(&self.inbox, LOOP_NAME, e);
            }
        }
        fn pending(&self) -> usize {
            event_triggers::pending_count(&self.inbox, LOOP_NAME)
        }
        fn once_at(&self, api: &Fake, limits: &Limits, at: DateTime<Utc>) -> RunReport {
            let notify = |l: &str| self.slack.borrow_mut().push(l.to_string());
            run_once(&Ctx {
                api,
                ledger: &self.ledger,
                inbox_dir: &self.inbox,
                events_dir: Some(&self.events),
                notify: &notify,
                limits,
                now: at,
            })
        }
        /// Queues `events` and runs until the inbox has nothing due; every line.
        fn go(&self, api: &Fake, events: Vec<Value>, limits: &Limits) -> Vec<String> {
            self.push(events);
            let mut lines = Vec::new();
            for _ in 0..20 {
                let r = self.once_at(api, limits, now());
                lines.extend(r.lines);
                if !r.took_event {
                    break;
                }
            }
            lines
        }
        fn emitted(&self, kind: &str) -> Vec<Value> {
            crate::events::iter_events(Some(&self.events), None, None)
                .into_iter()
                .filter(|e| e["event_type"] == kind)
                .collect()
        }
        fn ledger(&self) -> Map<String, Value> {
            load_ledger(&self.ledger).unwrap()
        }
    }

    #[test]
    fn forged_or_non_loop_event_never_starts_a_fix() {
        let fx = Fx::new();
        let mut api = Fake::new(&fx.ledger);
        api.verify = Box::new(|_| {
            Err(VerifyError::Refused(
                "feature#12-x is not the token user's own".into(),
            ))
        });
        let lines = fx.go(&api, vec![ev("e1", "abc", 100, 0.9, 7)], &Limits::default());
        assert!(lines[0].contains("dropped"), "{lines:?}");
        assert!(api.started.borrow().is_empty());
        assert!(fx.emitted("mr_autofix.escalated").is_empty());
        assert!(fx.slack.borrow().is_empty());
        assert!(!fx.ledger.exists());
        assert_eq!(fx.pending(), 0, "a refused event is dropped");
    }

    fn mr(over: Value) -> Value {
        let mut m = json!({"iid": 7, "state": "opened", "draft": false, "source_project_id": 9, "target_project_id": 9,
            "source_branch": "feature#7-fix-login", "author": {"username": "me"}, "sha": "abc",
            "head_pipeline": {"id": 100, "status": "failed"},
            "web_url": "https://gitlab.example.com/acme/web/-/merge_requests/7"});
        for (k, v) in over.as_object().unwrap() {
            m[k] = v.clone();
        }
        m
    }

    #[test]
    fn check_mr_refuses_non_loop_forked_draft_moved_or_foreign_mrs() {
        let e = ev("e1", "abc", 100, 0.9, 7);
        assert_eq!(check_mr(&e, &mr(json!({})), "me"), Ok(()));
        for (over, why) in [
            (json!({"state": "merged"}), "merged"),
            (json!({"draft": true}), "draft"),
            (json!({"work_in_progress": true}), "draft"),
            (json!({"source_project_id": 10}), "fork"),
            (
                json!({"source_branch": "feature-12-x"}),
                "not a loop branch",
            ),
            (json!({"source_branch": "hotfix/x"}), "not a loop branch"),
            (json!({"author": {"username": "someone"}}), "own"),
            (json!({"sha": "pushed-since"}), "moved"),
            (
                json!({"head_pipeline": {"id": 101, "status": "failed"}}),
                "head pipeline",
            ),
            (
                json!({"head_pipeline": {"id": 100, "status": "running"}}),
                "not failed",
            ),
            (json!({"head_pipeline": null}), "head pipeline"),
        ] {
            let err = check_mr(&e, &mr(over.clone()), "me").unwrap_err();
            assert!(err.contains(why), "{over}: {err}");
        }
        assert!(check_mr(&e, &mr(json!({})), "").is_err());
        let mut no_sha = e.clone();
        no_sha["data"]["sha"] = json!("");
        assert!(check_mr(&no_sha, &mr(json!({"sha": ""})), "me").is_err());
        let mut default_branch = e.clone();
        default_branch["data"]["mr_iid"] = Value::Null;
        assert!(mr_ref_of(&default_branch).is_err());
    }

    #[test]
    fn the_ledger_key_comes_from_gitlab_not_the_event() {
        let mut spelled = ev("e1", "abc", 100, 0.9, 7);
        spelled["data"]["project_id"] = json!("acme/web");
        let v = verified_from(&spelled, &mr(json!({})), "me", "work").unwrap();
        assert_eq!(v.mr_ref, "work:9!7");
        assert_eq!(v.key, KEY7);
        let other_host =
            mr(json!({"web_url": "https://other.example.com/acme/web/-/merge_requests/7"}));
        assert_ne!(
            verified_from(&spelled, &other_host, "me", "work")
                .unwrap()
                .key,
            KEY7
        );
        assert!(verified_from(
            &spelled,
            &mr(json!({"web_url": "javascript:x"})),
            "me",
            "work"
        )
        .is_err());
        assert!(verified_from(
            &spelled,
            &mr(json!({"target_project_id": null})),
            "me",
            "work"
        )
        .is_err());
    }

    #[test]
    fn the_same_mr_spelled_two_ways_gets_one_attempt() {
        let fx = Fx::new();
        let api = Fake::new(&fx.ledger);
        let numeric = ev("e1", "abc", 100, 0.9, 7);
        let mut by_path = ev("e2", "def", 101, 0.9, 7);
        by_path["data"]["project_id"] = json!("acme/web");
        let lines = fx.go(&api, vec![numeric, by_path], &Limits::default());
        assert_eq!(api.started.borrow().len(), 1, "{lines:?}");
        assert!(lines[1].contains("already_attempted"), "{lines:?}");
        assert_eq!(
            fx.ledger().keys().filter(|k| *k != ESCALATED_KEY).count(),
            1
        );
    }

    #[test]
    fn first_red_pipeline_gets_one_fix_and_records_it_before_spawning() {
        let fx = Fx::new();
        let api = Fake::new(&fx.ledger);
        let lines = fx.go(&api, vec![ev("e1", "abc", 100, 0.9, 7)], &Limits::default());
        assert!(lines[0].contains("fixed"), "{lines:?}");
        assert_eq!(*api.seen_in_ledger.borrow(), [true]);
        let (mr_ref, ci) = api.started.borrow()[0].clone();
        assert_eq!(mr_ref, "work:9!7");
        assert_eq!(
            (ci.pipeline_id, ci.sha.as_str(), ci.culprit.as_str()),
            (100, "abc", "spec/x_spec.rb")
        );
        let ledger = fx.ledger();
        let a = &attempts(&ledger, KEY7)[0];
        assert_eq!(
            (a["job_id"].as_str(), a["result"].as_str()),
            (Some("job1"), Some("done"))
        );
        assert_eq!(
            (a["event_id"].as_str(), a["mr_ref"].as_str()),
            (Some("e1"), Some("work:9!7"))
        );
        let done = fx.emitted("mr_autofix.completed");
        assert_eq!(done.len(), 1);
        assert_eq!(done[0]["data"]["job_id"], "job1");
        assert!(fx.slack.borrow().is_empty());
    }

    #[test]
    fn second_red_pipeline_escalates_instead_of_fixing() {
        let fx = Fx::new();
        let api = Fake::new(&fx.ledger);
        fx.go(&api, vec![ev("e1", "abc", 100, 0.9, 7)], &Limits::default());
        let second = ev("e2", "def", 101, 0.9, 7);
        let lines = fx.go(&api, vec![second.clone()], &Limits::default());
        assert!(lines[0].contains("already_attempted"), "{lines:?}");
        assert_eq!(api.started.borrow().len(), 1, "no second job");
        let esc = fx.emitted("mr_autofix.escalated");
        assert_eq!(esc.len(), 1);
        assert_eq!(esc[0]["data"]["reason"], "already_attempted");
        assert_eq!(esc[0]["data"]["sha"], "def");
        assert_eq!(esc[0]["data"]["mr_ref"], "work:9!7");
        assert_eq!(
            esc[0]["data"]["mr_web_url"],
            "https://GitLab.example.com/acme/web/-/merge_requests/7"
        );
        fx.go(&api, vec![second], &Limits::default());
        assert_eq!(
            fx.emitted("mr_autofix.escalated").len(),
            1,
            "replay escalates nothing more"
        );
        assert_eq!(fx.slack.borrow().len(), 1);
    }

    #[test]
    fn failed_fix_escalates_once_with_slack() {
        for (outcome, reason) in [
            (FixOutcome::Failed("checks failed".into()), "fix_failed"),
            (FixOutcome::TimedOut, "fix_failed"),
            (FixOutcome::Nothing("Fixed 0 of 1".into()), "fix_nothing"),
        ] {
            let fx = Fx::new();
            let api = Fake::new(&fx.ledger);
            *api.outcome.borrow_mut() = outcome;
            fx.go(&api, vec![ev("e1", "abc", 100, 0.9, 7)], &Limits::default());
            let esc = fx.emitted("mr_autofix.escalated");
            assert_eq!(esc.len(), 1, "{reason}");
            assert_eq!(esc[0]["data"]["reason"], reason);
            assert_eq!(fx.slack.borrow().len(), 1);
            assert!(fx.slack.borrow()[0].contains("work:9!7"));
            assert!(fx.emitted("mr_autofix.completed").is_empty());
            fx.go(&api, vec![ev("e1", "abc", 100, 0.9, 7)], &Limits::default());
            assert_eq!(fx.slack.borrow().len(), 1, "{reason}: once");
            assert_eq!(api.started.borrow().len(), 1);
        }
    }

    #[test]
    fn daily_cap_and_low_confidence_escalate_without_spending() {
        let fx = Fx::new();
        let api = Fake::new(&fx.ledger);
        let limits = Limits {
            max_per_day: 2,
            min_confidence: 0.7,
        };
        let lines = fx.go(
            &api,
            vec![
                ev("e1", "a1", 100, 0.9, 1),
                ev("e2", "a2", 101, 0.9, 2),
                ev("e3", "a3", 102, 0.9, 3),
                ev("e4", "a4", 103, 0.5, 4),
            ],
            &limits,
        );
        assert_eq!(api.started.borrow().len(), 2);
        assert!(lines[2].contains("daily_cap"), "{lines:?}");
        assert!(lines[3].contains("low_confidence"), "{lines:?}");
        let reasons: Vec<_> = fx
            .emitted("mr_autofix.escalated")
            .iter()
            .map(|e| e["data"]["reason"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(reasons, ["daily_cap", "low_confidence"]);
        assert!(attempts(&fx.ledger(), "gitlab.example.com/9!3").is_empty());
    }

    #[test]
    fn a_running_fix_job_escalates_job_busy() {
        let fx = Fx::new();
        let api = Fake::new(&fx.ledger);
        api.busy.set(true);
        let lines = fx.go(&api, vec![ev("e1", "abc", 100, 0.9, 7)], &Limits::default());
        assert!(lines[0].contains("job_busy"), "{lines:?}");
        assert!(api.started.borrow().is_empty());
        assert!(attempts(&fx.ledger(), KEY7).is_empty());
    }

    #[test]
    fn each_run_handles_one_event_and_leaves_the_rest_for_the_next() {
        let fx = Fx::new();
        let api = Fake::new(&fx.ledger);
        fx.push(vec![
            ev("e1", "a1", 100, 0.9, 1),
            ev("e2", "a2", 101, 0.9, 2),
        ]);
        let first = fx.once_at(&api, &Limits::default(), now());
        assert!(first.took_event);
        assert_eq!(api.started.borrow().len(), 1);
        assert_eq!(fx.pending(), 1, "the second event waits for its own run");
        let second = fx.once_at(&api, &Limits::default(), now());
        assert!(second.took_event);
        assert_eq!(api.started.borrow().len(), 2);
        assert_eq!(fx.pending(), 0);
        assert!(!fx.once_at(&api, &Limits::default(), now()).took_event);
    }

    fn write_ledger(fx: &Fx, v: Value) {
        std::fs::create_dir_all(fx.ledger.parent().unwrap()).unwrap();
        std::fs::write(&fx.ledger, v.to_string()).unwrap();
    }

    #[test]
    fn an_attempt_left_started_by_a_stopped_run_is_escalated_once() {
        let fx = Fx::new();
        let api = Fake::new(&fx.ledger);
        let at = (now() - Duration::hours(2)).to_rfc3339_opts(SecondsFormat::Secs, true);
        write_ledger(
            &fx,
            json!({KEY7: [{"sha": "abc", "pipeline_id": 100, "event_id": "e1", "job_id": "j1",
                "at": at, "result": "started", "mr_ref": "work:9!7",
                "web_url": "https://gitlab.example.com/acme/web/-/merge_requests/7"}]}),
        );

        api.busy.set(true);
        fx.once_at(&api, &Limits::default(), now());
        assert!(
            fx.emitted("mr_autofix.escalated").is_empty(),
            "its job still runs"
        );

        api.busy.set(false);
        let r = fx.once_at(&api, &Limits::default(), now());
        assert!(r.lines[0].contains("fix_failed"), "{:?}", r.lines);
        let esc = fx.emitted("mr_autofix.escalated");
        assert_eq!(esc.len(), 1);
        assert_eq!(esc[0]["data"]["mr_ref"], "work:9!7");
        assert_eq!(esc[0]["data"]["reason"], "fix_failed");
        assert_eq!(attempts(&fx.ledger(), KEY7)[0]["result"], "timed_out");
        fx.once_at(&api, &Limits::default(), now());
        assert_eq!(fx.emitted("mr_autofix.escalated").len(), 1, "once");
        assert_eq!(fx.slack.borrow().len(), 1);

        let fresh = Fx::new();
        let recent = (now() - Duration::minutes(10)).to_rfc3339_opts(SecondsFormat::Secs, true);
        write_ledger(
            &fresh,
            json!({KEY7: [{"sha": "abc", "pipeline_id": 100, "event_id": "e1", "at": recent,
                "result": "started", "mr_ref": "work:9!7"}]}),
        );
        fresh.once_at(&Fake::new(&fresh.ledger), &Limits::default(), now());
        assert!(
            fresh.emitted("mr_autofix.escalated").is_empty(),
            "a recent attempt may still be running"
        );
    }

    fn transient(fx: &Fx) -> Fake {
        let mut api = Fake::new(&fx.ledger);
        api.verify = Box::new(|_| Err(VerifyError::Transient("connection refused".into())));
        api
    }

    fn fresh_event() -> Value {
        let mut e = ev("e1", "abc", 100, 0.9, 7);
        e["timestamp"] = json!(crate::events::fmt_timestamp(now()));
        e
    }

    #[test]
    fn a_gitlab_error_puts_the_event_back_and_gives_up_after_a_day() {
        let fx = Fx::new();
        let api = transient(&fx);
        fx.push(vec![fresh_event()]);
        let r = fx.once_at(&api, &Limits::default(), now());
        assert!(r.lines[0].contains("retrying"), "{:?}", r.lines);
        assert_eq!(fx.pending(), 1, "back in the inbox");
        assert!(
            !fx.once_at(&api, &Limits::default(), now()).took_event,
            "not due yet"
        );
        assert!(fx.emitted("mr_autofix.escalated").is_empty());

        let r = fx.once_at(&api, &Limits::default(), now() + Duration::hours(25));
        assert!(r.lines[0].contains("verify_failed"), "{:?}", r.lines);
        assert_eq!(fx.pending(), 0);
        let esc = fx.emitted("mr_autofix.escalated");
        assert_eq!(esc.len(), 1);
        assert_eq!(esc[0]["data"]["reason"], "verify_failed");
        assert!(api.started.borrow().is_empty());
        assert_eq!(fx.slack.borrow().len(), 1);
    }

    #[test]
    fn a_gitlab_error_gives_up_after_five_tries() {
        let fx = Fx::new();
        let api = transient(&fx);
        fx.push(vec![fresh_event()]);
        let mut t = now();
        for _ in 0..4 {
            let r = fx.once_at(&api, &Limits::default(), t);
            assert!(r.lines[0].contains("retrying"), "{:?}", r.lines);
            t += Duration::hours(2);
        }
        let r = fx.once_at(&api, &Limits::default(), t);
        assert!(r.lines[0].contains("verify_failed"), "{:?}", r.lines);
        assert_eq!(fx.pending(), 0);
        assert_eq!(fx.emitted("mr_autofix.escalated").len(), 1);
    }

    #[test]
    fn a_corrupt_ledger_starts_nothing_and_is_never_overwritten() {
        let fx = Fx::new();
        std::fs::create_dir_all(fx.ledger.parent().unwrap()).unwrap();
        std::fs::write(&fx.ledger, "{not json").unwrap();
        let api = Fake::new(&fx.ledger);
        fx.go(&api, vec![ev("e1", "abc", 100, 0.9, 7)], &Limits::default());
        assert!(api.started.borrow().is_empty(), "nothing started");
        assert_eq!(std::fs::read_to_string(&fx.ledger).unwrap(), "{not json");
        assert_eq!(fx.pending(), 1, "the event waits until the ledger is fixed");
        let esc = fx.emitted("mr_autofix.escalated");
        assert_eq!(esc.len(), 1);
        assert_eq!(esc[0]["data"]["reason"], "ledger_corrupt");
        fx.go(&api, vec![], &Limits::default());
        assert_eq!(fx.emitted("mr_autofix.escalated").len(), 1, "reported once");
        assert_eq!(fx.slack.borrow().len(), 1);
        assert!(load_ledger(&fx.ledger).is_err());
        std::fs::write(&fx.ledger, "[1]").unwrap();
        assert!(load_ledger(&fx.ledger).is_err(), "not an object");
    }

    #[test]
    fn a_switched_off_loop_is_not_enabled() {
        assert!(!is_enabled(&json!({"name": LOOP_NAME, "enabled": false})));
        assert!(!is_enabled(&json!({"name": LOOP_NAME})));
        assert!(is_enabled(&json!({"name": LOOP_NAME, "enabled": true})));
    }

    #[test]
    fn limits_come_from_settings_with_defaults() {
        assert_eq!(Limits::from_settings(&Value::Null), Limits::default());
        let l = Limits::from_settings(&json!({"max_attempts_per_day": 5, "min_confidence": "0.9"}));
        assert_eq!((l.max_per_day, l.min_confidence), (5, 0.9));
        let bad = Limits::from_settings(&json!({"max_attempts_per_day": -1, "min_confidence": 4}));
        assert_eq!(bad, Limits::default());
    }

    #[test]
    fn job_states_map_to_outcomes() {
        assert_eq!(
            outcome_of("done", "Fixed 1 of 1"),
            FixOutcome::Done("Fixed 1 of 1".into())
        );
        assert_eq!(
            outcome_of("done", "Fixed 0 of 1"),
            FixOutcome::Nothing("Fixed 0 of 1".into())
        );
        assert_eq!(
            outcome_of("nothing", "moved"),
            FixOutcome::Nothing("moved".into())
        );
        assert_eq!(outcome_of("stopped", ""), FixOutcome::Failed(String::new()));
        assert_eq!(outcome_of("failed", "x"), FixOutcome::Failed("x".into()));
    }
}
