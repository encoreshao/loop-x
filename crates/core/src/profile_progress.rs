//! Live progress of project profile jobs (a build, a section refresh, the weekly
//! sync), written the same way a GitLab agent's hooks report: one activity record
//! per running job under `outputs/agent-activity/` (read by the Agents page and
//! pill, the Terminal page and the Mac app's snapshot) and one line per step in
//! `outputs/agent-trace.jsonl` (the Terminal page and `loopx watch`).
//!
//! A record and a trace line carry only step codes, counts, the project alias,
//! the job's kind and section slug, and a failure reason from a fixed list, never
//! repo text, agent output or an error message.
//!
//! The issue runner's stall watch only ever watches the activity file of the call
//! it started, so these records are never stopped by it.

use crate::agent_activity;
use crate::project_profile::{contains_secret, SectionKey};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The `event` of every profile record and trace line.
pub const EVENT: &str = "Profile";
/// The `tool` of every profile record and trace line.
pub const TOOL: &str = "profile";
/// Steps of a whole-profile build: scan, issues, ai, check, save.
pub const BUILD_STEPS: usize = 5;
/// Steps of a one-section refresh: scan, ai, check, proposal.
pub const REFRESH_STEPS: usize = 4;

/// What a job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Build,
    Refresh(SectionKey),
    /// The weekly sync (or "Sync now"): one record for the whole pass.
    Sync,
}

impl JobKind {
    pub fn code(self) -> &'static str {
        match self {
            JobKind::Build => "build",
            JobKind::Refresh(_) => "refresh",
            JobKind::Sync => "sync",
        }
    }

    fn steps(self) -> usize {
        match self {
            JobKind::Build => BUILD_STEPS,
            JobKind::Refresh(_) => REFRESH_STEPS,
            JobKind::Sync => 0,
        }
    }
}

/// One step of a job, as a fixed code (what the pages translate) and plain English.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Start,
    Scan,
    Issues,
    Ai,
    Check,
    Save,
    Proposal,
    /// A sync working on one project (the record names the project's alias).
    Project,
}

impl Step {
    const ALL: [Step; 8] = [
        Step::Start,
        Step::Scan,
        Step::Issues,
        Step::Ai,
        Step::Check,
        Step::Save,
        Step::Proposal,
        Step::Project,
    ];

    pub fn code(self) -> &'static str {
        match self {
            Step::Start => "start",
            Step::Scan => "scan",
            Step::Issues => "issues",
            Step::Ai => "ai",
            Step::Check => "check",
            Step::Save => "save",
            Step::Proposal => "proposal",
            Step::Project => "project",
        }
    }

    /// The English name; the dashboard translates it from the code.
    pub fn name(self) -> &'static str {
        match self {
            Step::Start => "Starting",
            Step::Scan => "Scanning repo",
            Step::Issues => "Reading issues",
            Step::Ai => "Asking the AI",
            Step::Check => "Checking the draft",
            Step::Save => "Saving",
            Step::Proposal => "Raising the proposal",
            Step::Project => "Syncing a project",
        }
    }

    pub fn from_code(code: &str) -> Option<Step> {
        Step::ALL.into_iter().find(|s| s.code() == code)
    }
}

/// What a job reports as it goes. [`ProfileJob`] writes the records; tests record calls.
pub trait Progress {
    /// Step `n` of `total` started.
    fn step(&self, n: usize, total: usize, step: Step);
    /// The job finished well.
    fn done(&self);
    /// The job failed with `err` (only a content-free reason is kept).
    fn fail(&self, err: &str);
}

/// Every reason a failure line can give: nothing else is ever written.
pub const FAIL_REASONS: [&str; 11] = [
    crate::profile_sync::DETAILS_HIDDEN,
    "already running",
    "checkout missing",
    "secret-shaped text found",
    "the reply did not parse",
    "no profile yet",
    "internal error",
    "the AI call failed",
    "the draft was rejected",
    "could not save",
    "error",
];

/// The content-free reason for a failure at `step`: a fixed phrase, never the
/// error's own text. A message that quotes anything secret-shaped is "details hidden".
pub fn failure_reason(step: Option<Step>, err: &str) -> &'static str {
    if contains_secret(err).is_some() {
        return crate::profile_sync::DETAILS_HIDDEN;
    }
    let e = err.to_lowercase();
    if e.contains("already in progress") {
        "already running"
    } else if e.contains("checkout") && (e.contains("missing") || e.contains("not found")) {
        "checkout missing"
    } else if e.contains("looks like a secret") {
        "secret-shaped text found"
    } else if e.contains("did not parse") || e.contains("reply had no") {
        "the reply did not parse"
    } else if e.contains("no profile yet") {
        "no profile yet"
    } else if e.contains("internal error") {
        "internal error"
    } else {
        match step {
            Some(Step::Ai) => "the AI call failed",
            Some(Step::Check) => "the draft was rejected",
            Some(Step::Save | Step::Proposal) => "could not save",
            _ => "error",
        }
    }
}

fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A job's activity file beside the shared `base` (`outputs/agent-activity.json`):
/// `agent-activity/profile.<alias>.json`, or `agent-activity/profile-sync.json`.
/// Issue files are `<alias>-<iid>.json` with a numeric iid, so the two never meet.
pub fn job_file(base: &Path, kind: JobKind, alias: &str) -> PathBuf {
    let dir = base.with_file_name("agent-activity");
    match kind {
        JobKind::Sync => dir.join("profile-sync.json"),
        _ => dir.join(format!("profile.{}.json", safe_name(alias))),
    }
}

/// Whether an activity record (or trace line) belongs to a profile job.
pub fn is_profile_record(v: &Value) -> bool {
    v.get("event").and_then(Value::as_str) == Some(EVENT)
        && v.get("tool").and_then(Value::as_str) == Some(TOOL)
}

/// The plain label a job's record carries; the pages build their own translated one.
pub fn label(kind: JobKind, alias: &str) -> String {
    match kind {
        JobKind::Sync => "Profile sync".to_string(),
        _ => format!("Profile \u{b7} {alias}"),
    }
}

struct State {
    step: usize,
    steps: usize,
    code: Step,
    /// The alias the job works on now (a sync moves from project to project).
    alias: String,
    settled: bool,
}

/// One running profile job's live record. Dropping it removes the record, also on
/// an early return or a panic; a job dropped before [`Progress::done`] or
/// [`Progress::fail`] leaves a "stopped before finishing" line in the trace.
pub struct ProfileJob {
    file: PathBuf,
    kind: JobKind,
    label: String,
    started_at: String,
    state: Mutex<State>,
}

impl ProfileJob {
    /// Start the job's record under `base` (the shared `outputs/agent-activity.json`
    /// the dashboard reads). For a sync, `alias` is empty until [`ProfileJob::project`].
    /// Never fails: a record that cannot be written only means the pages do not see it.
    pub fn start(base: &Path, kind: JobKind, alias: &str) -> ProfileJob {
        let file = job_file(base, kind, alias);
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let job = ProfileJob {
            file,
            kind,
            label: label(kind, alias),
            started_at: agent_activity::now_iso(),
            state: Mutex::new(State {
                step: 0,
                steps: kind.steps(),
                code: Step::Start,
                alias: alias.to_string(),
                settled: false,
            }),
        };
        job.write("start", None);
        job
    }

    /// The record's path (for tests and the Memory page).
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// A sync moved on to project `i` of `n`, `alias`.
    pub fn project(&self, i: usize, n: usize, alias: &str) {
        {
            let mut st = self.lock();
            st.step = i;
            st.steps = n;
            st.code = Step::Project;
            st.alias = alias.to_string();
        }
        self.write("step", None);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record(&self, st: &State) -> Value {
        let detail = match st.code {
            Step::Project => st.alias.clone(),
            s => s.name().to_string(),
        };
        let section = match self.kind {
            JobKind::Refresh(k) => json!(k.slug()),
            _ => Value::Null,
        };
        json!({
            "at": agent_activity::now_iso(),
            "started_at": self.started_at,
            "label": self.label,
            "event": EVENT,
            "tool": TOOL,
            "detail": detail,
            "kind": self.kind.code(),
            "section": section,
            "alias": st.alias,
            "step": st.step,
            "steps": st.steps,
            "step_code": st.code.code(),
        })
    }

    /// Append a trace line for `phase`; the live file is rewritten unless the job
    /// is over (then only `Drop` touches it, to remove it).
    fn write(&self, phase: &str, reason: Option<&str>) {
        let st = self.lock();
        let rec = self.record(&st);
        let mut line = rec.clone();
        line["phase"] = json!(phase);
        if let Some(r) = reason {
            line["reason"] = json!(r);
        }
        let _ = agent_activity::append_trace(&self.file, &line);
        if !st.settled {
            let _ =
                crate::settings_store::atomic_write(&self.file, rec.to_string().as_bytes(), None);
        }
    }
}

impl Progress for ProfileJob {
    fn step(&self, n: usize, total: usize, step: Step) {
        {
            let mut st = self.lock();
            if st.settled {
                return;
            }
            st.step = n;
            st.steps = total;
            st.code = step;
        }
        self.write("step", None);
    }

    fn done(&self) {
        {
            let mut st = self.lock();
            if st.settled {
                return;
            }
            st.settled = true;
        }
        self.write("done", None);
    }

    fn fail(&self, err: &str) {
        let reason = {
            let mut st = self.lock();
            if st.settled {
                return;
            }
            st.settled = true;
            failure_reason(Some(st.code), err)
        };
        self.write("failed", Some(reason));
    }
}

impl Drop for ProfileJob {
    fn drop(&mut self) {
        let unsettled = {
            let mut st = self.lock();
            !std::mem::replace(&mut st.settled, true)
        };
        if unsettled {
            self.write("interrupted", None);
        }
        agent_activity::finish(&self.file);
    }
}

/// `progress.step(..)` when there is one.
pub fn report(progress: Option<&dyn Progress>, n: usize, total: usize, step: Step) {
    if let Some(p) = progress {
        p.step(n, total, step);
    }
}

/// Settle `progress` with `res`: done on `Ok`, a failure line on `Err`.
pub fn settle<T>(progress: Option<&dyn Progress>, res: &Result<T, String>) {
    if let Some(p) = progress {
        match res {
            Ok(_) => p.done(),
            Err(e) => p.fail(e),
        }
    }
}

/// A running job as the pages read it back from its record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobView {
    pub kind: String,
    /// The section slug of a refresh.
    pub section: Option<String>,
    /// The project the job works on now (empty for a sync that has not started one).
    pub alias: String,
    pub step: usize,
    pub steps: usize,
    pub step_code: String,
    /// When the job started (RFC 3339), for "running for".
    pub started_at: String,
    /// When the record last changed (RFC 3339).
    pub at: String,
}

impl JobView {
    /// A profile record with sane fields, else `None` (any other or a malformed record).
    pub fn from_record(v: &Value) -> Option<JobView> {
        if !is_profile_record(v) {
            return None;
        }
        let s = |k: &str| v.get(k).and_then(Value::as_str);
        let n = |k: &str| {
            v.get(k)
                .and_then(Value::as_u64)
                .and_then(|x| usize::try_from(x).ok())
        };
        let kind = s("kind").filter(|k| matches!(*k, "build" | "refresh" | "sync"))?;
        let step_code = s("step_code").and_then(Step::from_code)?;
        let section = s("section")
            .and_then(SectionKey::from_slug)
            .map(|k| k.slug().to_string());
        Some(JobView {
            kind: kind.to_string(),
            section,
            alias: s("alias").unwrap_or("").to_string(),
            step: n("step")?,
            steps: n("steps")?,
            step_code: step_code.code().to_string(),
            started_at: s("started_at").unwrap_or("").to_string(),
            at: s("at").unwrap_or("").to_string(),
        })
    }

    pub fn is_sync(&self) -> bool {
        self.kind == "sync"
    }

    pub fn step_kind(&self) -> Step {
        Step::from_code(&self.step_code).unwrap_or(Step::Start)
    }

    /// The plain English label (the record's label is not trusted for display).
    pub fn label(&self) -> String {
        if self.is_sync() {
            "Profile sync".to_string()
        } else {
            format!("Profile \u{b7} {}", self.alias)
        }
    }

    /// What it is doing now, in English: "Asking the AI · step 3 of 5", or for a
    /// sync "web · project 2 of 3".
    pub fn step_text(&self) -> String {
        let step = self.step_kind();
        if step == Step::Start || self.steps == 0 {
            return Step::Start.name().to_string();
        }
        if self.is_sync() {
            format!(
                "{} \u{b7} project {} of {}",
                self.alias, self.step, self.steps
            )
        } else {
            format!(
                "{} \u{b7} step {} of {}",
                step.name(),
                self.step,
                self.steps
            )
        }
    }

    /// The key the snapshot lists the job under: `profile:<alias>` or `profile-sync`.
    pub fn snapshot_key(&self) -> String {
        if self.is_sync() {
            "profile-sync".to_string()
        } else {
            format!("profile:{}", self.alias)
        }
    }

    /// The running job as a loop status for the snapshot (the Mac island): the
    /// fields older apps read, plus `kind`, `section`, `alias`, `step`, `steps`,
    /// `step_code` and `step_name` for the ones that know them.
    pub fn island_status(&self) -> Value {
        let step_name = if self.is_sync() && self.step_kind() == Step::Project {
            self.alias.clone()
        } else {
            self.step_kind().name().to_string()
        };
        json!({
            "state": "running",
            "updated_at": self.at,
            "started_at": self.started_at,
            "current_issue": self.label(),
            "current_step": self.step_text(),
            "kind": self.kind,
            "section": self.section,
            "alias": self.alias,
            "step": self.step,
            "steps": self.steps,
            "step_code": self.step_code,
            "step_name": step_name,
        })
    }
}

/// Every running profile job under `base`, newest first.
pub fn running(base: &Path) -> Vec<JobView> {
    agent_activity::read_all(base)
        .iter()
        .filter_map(JobView::from_record)
        .collect()
}

/// The running job for `alias` (not the sync's overall record), if any.
pub fn running_for(base: &Path, alias: &str) -> Option<JobView> {
    running(base)
        .into_iter()
        .find(|j| !j.is_sync() && j.alias == alias)
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::cell::RefCell;

    /// A recording fake for the functions that take `Option<&dyn Progress>`.
    #[derive(Default)]
    pub(crate) struct Recorder(pub RefCell<Vec<String>>);

    impl Progress for Recorder {
        fn step(&self, n: usize, total: usize, step: Step) {
            self.0
                .borrow_mut()
                .push(format!("{n}/{total} {}", step.code()));
        }
        fn done(&self) {
            self.0.borrow_mut().push("done".into());
        }
        fn fail(&self, err: &str) {
            self.0
                .borrow_mut()
                .push(format!("fail {}", failure_reason(None, err)));
        }
    }
}

#[cfg(test)]
pub(crate) use testing::Recorder;

#[cfg(test)]
mod tests {
    use super::*;

    fn base(d: &Path) -> PathBuf {
        d.join("outputs/agent-activity.json")
    }

    fn trace(d: &Path) -> Vec<Value> {
        std::fs::read_to_string(d.join("outputs/agent-trace.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn a_job_writes_a_record_with_its_label_kind_step_and_alias() {
        let d = tempfile::tempdir().unwrap();
        let job = ProfileJob::start(&base(d.path()), JobKind::Build, "web");
        let v = agent_activity::read(job.file()).unwrap();
        assert_eq!(v["label"], "Profile \u{b7} web");
        assert_eq!(v["event"], EVENT);
        assert_eq!(v["tool"], TOOL);
        assert_eq!(v["kind"], "build");
        assert_eq!(v["alias"], "web");
        assert_eq!(
            (v["step"].as_u64(), v["steps"].as_u64()),
            (Some(0), Some(5))
        );
        assert_eq!(v["step_code"], "start");
        assert_eq!(
            job.file(),
            d.path().join("outputs/agent-activity/profile.web.json")
        );
        let refresh =
            ProfileJob::start(&base(d.path()), JobKind::Refresh(SectionKey::Stack), "api");
        let v = agent_activity::read(refresh.file()).unwrap();
        assert_eq!(
            (v["kind"].as_str(), v["section"].as_str()),
            (Some("refresh"), Some("stack"))
        );
        assert_eq!(v["steps"], 4);
    }

    #[test]
    fn each_step_overwrites_the_record() {
        let d = tempfile::tempdir().unwrap();
        let job = ProfileJob::start(&base(d.path()), JobKind::Build, "web");
        job.step(1, 5, Step::Scan);
        job.step(3, 5, Step::Ai);
        let v = agent_activity::read(job.file()).unwrap();
        assert_eq!(v["step"], 3);
        assert_eq!(v["steps"], 5);
        assert_eq!(v["step_code"], "ai");
        assert_eq!(v["detail"], "Asking the AI");
        assert_eq!(agent_activity::read_all(&base(d.path())).len(), 1);
    }

    #[test]
    fn dropping_the_job_removes_the_record_also_after_a_panic() {
        let d = tempfile::tempdir().unwrap();
        let b = base(d.path());
        let file = {
            let job = ProfileJob::start(&b, JobKind::Build, "web");
            job.step(1, 5, Step::Scan);
            job.done();
            job.file().to_path_buf()
        };
        assert!(!file.exists());
        let r = std::panic::catch_unwind(|| {
            let job = ProfileJob::start(&b, JobKind::Build, "api");
            job.step(2, 5, Step::Issues);
            panic!("boom");
        });
        assert!(r.is_err());
        assert!(running(&b).is_empty(), "{:?}", running(&b));
        let last = trace(d.path()).pop().unwrap();
        assert_eq!(last["phase"], "interrupted");
        assert_eq!(last["alias"], "api");
    }

    #[test]
    fn every_step_and_the_end_are_appended_to_the_trace() {
        let d = tempfile::tempdir().unwrap();
        {
            let job = ProfileJob::start(&base(d.path()), JobKind::Build, "web");
            job.step(1, 5, Step::Scan);
            job.step(2, 5, Step::Issues);
            job.done();
        }
        let phases: Vec<String> = trace(d.path())
            .iter()
            .map(|l| {
                format!(
                    "{}:{}",
                    l["phase"].as_str().unwrap(),
                    l["step_code"].as_str().unwrap()
                )
            })
            .collect();
        assert_eq!(
            phases,
            ["start:start", "step:scan", "step:issues", "done:issues"]
        );
        assert!(trace(d.path())
            .iter()
            .all(|l| l["alias"] == "web" && l["label"] == "Profile \u{b7} web"));
    }

    #[test]
    fn a_failure_line_gives_a_fixed_reason_never_the_error_text() {
        let d = tempfile::tempdir().unwrap();
        {
            let job = ProfileJob::start(&base(d.path()), JobKind::Build, "web");
            job.step(3, 5, Step::Ai);
            job.fail("claude said: the README of /Users/me/secret-project is wrong");
            job.done();
        }
        let lines = trace(d.path());
        let failed: Vec<&Value> = lines.iter().filter(|l| l["phase"] == "failed").collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0]["reason"], "the AI call failed");
        assert!(lines.iter().all(|l| l["phase"] != "done"), "settled once");
        let text = std::fs::read_to_string(d.path().join("outputs/agent-trace.jsonl")).unwrap();
        assert!(
            !text.contains("README") && !text.contains("/Users/me"),
            "{text}"
        );
    }

    #[test]
    fn failure_reasons_are_content_free_and_secret_free() {
        let token = "glpat-abcdefghij0123456789";
        assert_eq!(
            failure_reason(Some(Step::Ai), &format!("bad token {token}")),
            "details hidden"
        );
        assert_eq!(
            failure_reason(None, "a profile run for web is already in progress"),
            "already running"
        );
        assert_eq!(
            failure_reason(None, "the checkout for web is missing (/x); clone it first"),
            "checkout missing"
        );
        assert_eq!(
            failure_reason(
                Some(Step::Check),
                "the agent's profile did not parse: line 3"
            ),
            "the reply did not parse"
        );
        assert_eq!(
            failure_reason(
                Some(Step::Check),
                "refusing to send the prompt: it contains what looks like a secret (x)"
            ),
            "secret-shaped text found"
        );
        assert_eq!(
            failure_reason(Some(Step::Save), "disk full"),
            "could not save"
        );
        for (step, err) in [(None, "anything"), (Some(Step::Ai), "timeout")] {
            assert!(FAIL_REASONS.contains(&failure_reason(step, err)));
        }
    }

    #[test]
    fn a_sync_record_counts_projects_and_names_the_current_one() {
        let d = tempfile::tempdir().unwrap();
        let job = ProfileJob::start(&base(d.path()), JobKind::Sync, "");
        job.project(2, 3, "api");
        let v = agent_activity::read(job.file()).unwrap();
        assert_eq!(v["label"], "Profile sync");
        assert_eq!(
            (v["step"].as_u64(), v["steps"].as_u64()),
            (Some(2), Some(3))
        );
        assert_eq!(
            (v["detail"].as_str(), v["alias"].as_str()),
            (Some("api"), Some("api"))
        );
        let views = running(&base(d.path()));
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].step_text(), "api \u{b7} project 2 of 3");
        assert_eq!(views[0].snapshot_key(), "profile-sync");
        drop(job);
        assert!(running(&base(d.path())).is_empty());
    }

    #[test]
    fn job_files_never_collide_with_issue_task_files() {
        let b = Path::new("/o/agent-activity.json");
        let issue = agent_activity::for_task(b, "profile", "7");
        assert_ne!(job_file(b, JobKind::Build, "7"), issue);
        assert_eq!(
            job_file(b, JobKind::Build, "../x"),
            Path::new("/o/agent-activity/profile..._x.json")
        );
        assert_eq!(
            job_file(b, JobKind::Sync, "web"),
            Path::new("/o/agent-activity/profile-sync.json")
        );
    }

    #[test]
    fn the_view_reads_only_well_formed_profile_records() {
        let d = tempfile::tempdir().unwrap();
        let b = base(d.path());
        let job = ProfileJob::start(&b, JobKind::Build, "web");
        job.step(3, 5, Step::Ai);
        agent_activity::start(&agent_activity::for_task(&b, "web", "7"), "web #7", "t").unwrap();
        let bad = b.with_file_name("agent-activity").join("profile.bad.json");
        std::fs::write(&bad, r#"{"event":"Profile","tool":"profile","kind":"rm -rf","step":1,"steps":5,"step_code":"ai"}"#).unwrap();
        let views = running(&b);
        assert_eq!(views.len(), 1, "{views:?}");
        let v = &views[0];
        assert_eq!(v.label(), "Profile \u{b7} web");
        assert_eq!(v.step_text(), "Asking the AI \u{b7} step 3 of 5");
        let island = v.island_status();
        assert_eq!(island["state"], "running");
        assert_eq!(island["step"], 3);
        assert_eq!(island["steps"], 5);
        assert_eq!(island["step_name"], "Asking the AI");
        assert_eq!(island["alias"], "web");
        assert_eq!(island["kind"], "build");
        assert_eq!(running_for(&b, "web").map(|j| j.step), Some(3));
        assert!(running_for(&b, "api").is_none());
    }

    #[test]
    fn nothing_here_resolves_a_default_home_or_outputs_path() {
        // Every path comes from the caller (the dashboard's own root, a test's temp dir).
        let src = include_str!("profile_progress.rs");
        for banned in [
            concat!("home", "_dir("),
            concat!("default_", "events_dir"),
            concat!("status_path_", "for_loop("),
            concat!("agent_activity::", "path("),
            concat!("env::", "var"),
        ] {
            assert!(!src.contains(banned), "{banned}");
        }
    }

    #[test]
    fn report_and_settle_pass_through_to_the_progress() {
        let r = Recorder::default();
        report(Some(&r), 1, 5, Step::Scan);
        report(None, 2, 5, Step::Issues);
        settle::<()>(Some(&r), &Err("internal error".into()));
        settle(Some(&r), &Ok(()));
        assert_eq!(*r.0.borrow(), ["1/5 scan", "fail internal error", "done"]);
    }
}
