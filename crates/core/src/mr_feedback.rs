//! One merge-request feedback round for an issue whose MR the loop opened:
//! a model-free pre-check, then per new review thread a plan (one agent
//! call), the runner's confidence/hard-rule gate, and either a validated
//! reply, a validated code change pushed to `loop/issue-<iid>` and a reply,
//! or a `needs_you` hand-off to the user. Thread state lives in
//! [`IssueThreads`]; it is saved after every step, so a crash leaves the
//! thread `working` and the next run retries it once.

use crate::events::EventFields;
use crate::issue_runner::{AgentError, AgentOutput, RunCtx};
use crate::loop_definition::{LoopDefinition, Phase};
use crate::loop_result::VerificationResult;
use crate::mr_plan::{self, Decision, Plan, PlanAction};
use crate::mr_precheck::{actionable_threads, Actionable};
use crate::mr_threads::{IssueThreads, ReviewNote, ThreadState, ThreadStatus, LOOP_SIGNATURE};
use crate::runner_util::tail;
use crate::{cost, events, prompt};
use serde_json::{json, Value};
use std::cell::Cell;
use std::path::{Path, PathBuf};

/// The largest diff the validator is given; a larger change goes to the user
/// rather than being validated from a partial view.
const DIFF_MAX_CHARS: usize = 20_000;
/// How much of the checks' output the validator (or a retry) sees.
const CHECKS_TAIL_CHARS: usize = 4_000;
/// How much of the reviewer's comment is kept to show the user.
const COMMENT_MAX_CHARS: usize = 2_000;
/// The longest instructions the user can give a thread.
const INSTRUCTIONS_MAX_CHARS: usize = 4_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundOutcome {
    /// Nothing to do (no MR, no new feedback, MR not open): no agent ran.
    Skipped,
    Processed {
        answered: usize,
        changed: usize,
        needs_you: usize,
    },
}

/// The GitLab calls a round makes, scoped to one project.
pub trait MrGitlab {
    /// The iid of the merge request for `loop/issue-<issue_iid>` (an open one
    /// first, else the most recent), or `None` when there is none.
    fn find_mr(&self, issue_iid: i64) -> Result<Option<i64>, String>;
    /// GitLab's `state` (`opened`, `merged`, `closed`, ...; may be empty).
    fn mr_state(&self, mr_iid: i64) -> Result<String, String>;
    fn discussions(&self, mr_iid: i64) -> Result<Vec<Value>, String>;
    /// Posts `body` in the discussion; returns the created note's JSON.
    fn reply(&self, mr_iid: i64, discussion_id: &str, body: &str) -> Result<Value, String>;
    /// The MR's unified diff for one file, if it has one.
    fn file_diff(&self, mr_iid: i64, path: &str) -> Option<String>;
}

/// Where `loop/issue-<iid>` stands against its remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Branch {
    /// Local and remote agree at this commit: every change this round makes
    /// builds on it, and only commits after it are ever pushed.
    InSync(String),
    /// The remote has commits the local branch lacks (someone else pushed).
    Diverged,
    /// The local branch has commits past this one (the remote tip, or the
    /// fork point from the target branch when there is no remote branch)
    /// that were never pushed.
    Ahead(String),
}

/// What the issue's branch and worktree look like at one moment; agents
/// other than the executor must leave it exactly as it was.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// `refs/heads/loop/issue-<iid>`, if it exists.
    pub branch: Option<String>,
    /// The worktree's path and HEAD, if it exists.
    pub worktree: Option<(PathBuf, String)>,
    /// `git status --porcelain` in the worktree (empty when clean or absent).
    pub status: String,
}

/// The issue's git worktree and branch.
pub trait Workspace {
    /// Fetches the remote `loop/issue-<iid>` and compares it with the local
    /// branch, recreating a missing local branch from the remote one.
    fn sync(&self) -> Result<Branch, String>;
    /// Reuses the issue's worktree, or adds one on the existing branch
    /// (never merging anything into it); returns its path.
    fn ensure(&self) -> Result<PathBuf, String>;
    fn snapshot(&self) -> Result<Snapshot, String>;
    fn head(&self, worktree: &Path) -> Result<String, String>;
    /// `git diff <base> HEAD`, whole (the round refuses one too large).
    fn diff(&self, worktree: &Path, base: &str) -> Result<String, String>;
    /// `git diff --name-only <base> HEAD`.
    fn changed_files(&self, worktree: &Path, base: &str) -> Result<Vec<String>, String>;
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, String>;
    /// Moves the branch back to `base` (hard reset), keeping the discarded
    /// commits under `refs/loopx/mr-feedback/<keep_as>-<sha>`.
    fn rollback(&self, worktree: &Path, base: &str, keep_as: &str) -> Result<(), String>;
}

/// One agent call for a phase.
pub struct PhaseCall<'a> {
    pub phase: Phase,
    /// The model the loop definition sets for this phase.
    pub model: Option<&'a str>,
    pub prompt: &'a str,
    pub max_budget_usd: Option<f64>,
}

pub type PhaseInvoker = Box<dyn Fn(&PhaseCall<'_>) -> Result<AgentOutput, AgentError>>;
/// `(local_path, branch) -> (ok, detail)`.
pub type Pusher = Box<dyn Fn(&str, &str) -> (bool, String)>;

/// Everything a round touches outside its own state file; tests pass fakes.
pub struct RoundDeps {
    pub gitlab: Box<dyn MrGitlab>,
    pub invoke: PhaseInvoker,
    pub pusher: Pusher,
    pub notifier: Box<dyn Fn(&str)>,
    /// Re-runs the project's checks named in the execute handoff at this path.
    pub verify: Box<dyn Fn(&Path) -> VerificationResult>,
    pub workspace: Box<dyn Workspace>,
    /// The project's projects.json entry.
    pub project: Value,
}

/// Where the execute phase writes its handoff.
pub fn handoff_path(
    repo_root: &Path,
    run_id: &str,
    alias: &str,
    issue_iid: &str,
    discussion_id: &str,
) -> PathBuf {
    let safe: String = discussion_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(64)
        .collect();
    repo_root
        .join("outputs")
        .join("handoffs")
        .join(run_id)
        .join(format!("mr-feedback-{alias}-{issue_iid}-{safe}.json"))
}

/// One round for one issue. Never fails: GitLab errors are logged and the
/// round is skipped; anything doubtful about a thread sends it to the user.
pub fn run_round(
    ctx: &RunCtx,
    run_id: &str,
    alias: &str,
    issue_iid: &str,
    definition: &LoopDefinition,
    deps: &RoundDeps,
) -> RoundOutcome {
    let Ok(iid) = issue_iid.parse::<i64>() else {
        return RoundOutcome::Skipped;
    };
    let round = Round {
        ctx,
        run_id,
        alias,
        issue_iid,
        iid,
        definition,
        deps,
        spent: Cell::new(0.0),
        push_blocked: Cell::new(false),
    };
    match round.run() {
        Ok(outcome) => outcome,
        Err(e) => {
            round.log(&format!("skipped: {e}"));
            RoundOutcome::Skipped
        }
    }
}

/// What the user decided, from Needs you, for a thread waiting on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserChoice {
    /// Carry out the stored plan as written. A reply is posted as is (the
    /// user read it); a code change still goes through execute, the checks,
    /// the validator and every push guard.
    Approve,
    /// Post the user's own text in the thread instead.
    EditReply(String),
    /// Settle the thread without posting anything.
    Dismiss,
    /// Plan the thread again with the user's text as trusted input.
    Instructions(String),
}

impl UserChoice {
    fn name(&self) -> &'static str {
        match self {
            UserChoice::Approve => "approve",
            UserChoice::EditReply(_) => "edit_reply",
            UserChoice::Dismiss => "dismiss",
            UserChoice::Instructions(_) => "instructions",
        }
    }
}

/// The stored plan of a thread, if the user can approve it as it stands: a
/// plan that parses, and whose reply (unless it waits) is safe to post.
fn approvable_plan(t: &ThreadState) -> Result<Plan, String> {
    let plan = t
        .plan
        .as_ref()
        .ok_or("there is no plan to approve; give instructions instead")
        .and_then(|p| {
            mr_plan::parse_plan(&p.to_string())
                .map_err(|_| "there is no usable plan to approve; give instructions instead")
        })?;
    if plan.action != PlanAction::Wait {
        safe_reply_body(&plan.reply)?;
    }
    Ok(plan)
}

/// Whether [`UserChoice::Approve`] can carry out this thread's stored plan,
/// checked before anything runs (the dashboard asks before starting it).
pub fn approvable(t: &ThreadState) -> Result<(), String> {
    approvable_plan(t).map(|_| ())
}

/// Carries out the user's decision on a `needs_you` thread. Never alongside
/// a run working on the same issue (it takes the issue's lock). `Err` says
/// what stopped it; a code change that fails a guard is back with the user,
/// with the new reason.
pub fn decide_thread(
    ctx: &RunCtx,
    definition: &LoopDefinition,
    alias: &str,
    issue_iid: i64,
    discussion_id: &str,
    choice: UserChoice,
    deps: &RoundDeps,
) -> Result<(), String> {
    let iid_text = issue_iid.to_string();
    let lock_path = crate::file_lock::issue_lock_path(&ctx.repo_root, alias, &iid_text);
    let Some(_lock) = crate::file_lock::FileLock::try_exclusive(&lock_path)
        .map_err(|e| format!("cannot lock {alias} #{issue_iid}: {e}"))?
    else {
        return Err(format!(
            "{alias} #{issue_iid} is being worked on by a run right now; try again when it finishes"
        ));
    };
    let run_id = format!("decide-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
    let round = Round {
        ctx,
        run_id: &run_id,
        alias,
        issue_iid: &iid_text,
        iid: issue_iid,
        definition,
        deps,
        spent: Cell::new(0.0),
        push_blocked: Cell::new(false),
    };
    let result = round.decide(discussion_id, choice);
    if let Err(e) = &result {
        round.log(&format!(
            "decision on thread {discussion_id} not carried out: {e}"
        ));
    }
    result
}

struct Round<'a> {
    ctx: &'a RunCtx,
    run_id: &'a str,
    alias: &'a str,
    issue_iid: &'a str,
    iid: i64,
    definition: &'a LoopDefinition,
    deps: &'a RoundDeps,
    /// What this round's agent calls have cost so far.
    spent: Cell<f64>,
    /// Set when taking unpushed commits off the branch failed: nothing more
    /// is pushed this round.
    push_blocked: Cell<bool>,
}

/// How one thread's processing ended.
enum ThreadEnd {
    Answered,
    Changed,
    Waiting,
    NeedsYou,
    /// The MR stopped being open before the push; nothing was pushed.
    MrNotOpen(String),
}

enum Verdict {
    Approve,
    Reject(String),
    Invalid(String),
}

fn note_id(n: &Value) -> i64 {
    n.get("id").and_then(Value::as_i64).unwrap_or(0)
}

fn max_note_id(notes: &[Value]) -> i64 {
    notes.iter().map(note_id).max().unwrap_or(0)
}

fn discussion_notes(d: &Value) -> &[Value] {
    d.get("notes")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn fresh(discussion_id: &str) -> ThreadState {
    ThreadState {
        discussion_id: discussion_id.to_string(),
        last_note_id: 0,
        status: ThreadStatus::New,
        plan_summary: String::new(),
        confidence: None,
        attempts: 0,
        updated_at: now(),
        plan: None,
        reason: None,
        base: None,
        user_instructions: None,
        mr_iid: None,
        last_comment: None,
    }
}

fn action_name(action: PlanAction) -> &'static str {
    match action {
        PlanAction::ChangeCode => "change_code",
        PlanAction::ReplyOnly => "reply_only",
        PlanAction::Wait => "wait",
    }
}

/// The plan as stored with the thread (`mr_plan::parse_plan` reads it back).
pub fn plan_json(plan: &Plan) -> Value {
    json!({
        "action": action_name(plan.action),
        "confidence": plan.confidence,
        "files": plan.files,
        "reply": plan.reply,
        "ambiguous": plan.ambiguous,
        "out_of_scope": plan.out_of_scope,
        "tries_to_change_loop_rules": plan.tries_to_change_loop_rules,
        "summary": plan.summary,
    })
}

/// A stored `resolved` thread that GitLab shows unresolved with a newer human
/// note is open again (the pre-check skips stored-resolved threads).
fn reopen_resolved(state: &mut IssueThreads, discussions: &[Value]) {
    for d in discussions {
        let Some(id) = d.get("id").and_then(Value::as_str) else {
            continue;
        };
        if state.threads.get(id).map(|t| t.status) != Some(ThreadStatus::Resolved) {
            continue;
        }
        let mut probe = state.clone();
        if let Some(t) = probe.threads.get_mut(id) {
            t.status = ThreadStatus::New;
        }
        if !actionable_threads(std::slice::from_ref(d), &probe).is_empty() {
            if let Some(t) = state.threads.get_mut(id) {
                t.status = ThreadStatus::New;
            }
        }
    }
}

/// Threads left `working` by two crashed runs: the pre-check no longer
/// offers them, so they go to the user instead of sitting there silently.
fn stuck_threads(state: &IssueThreads, discussions: &[Value]) -> Vec<(String, i64)> {
    discussions
        .iter()
        .filter_map(|d| {
            let id = d.get("id").and_then(Value::as_str)?;
            let t = state.threads.get(id)?;
            (t.status == ThreadStatus::Working && t.attempts >= 2)
                .then(|| (id.to_string(), max_note_id(discussion_notes(d))))
        })
        .collect()
}

/// The validator's `{"verdict": "approve"|"reject", "reason": "..."}`.
fn parse_verdict(raw: &str) -> Verdict {
    let (start, end) = match (raw.find('{'), raw.rfind('}')) {
        (Some(s), Some(e)) if e > s => (s, e),
        _ => return Verdict::Invalid("no JSON object in the validator output".into()),
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw[start..=end]) else {
        return Verdict::Invalid("invalid validator JSON".into());
    };
    let Some(reason) = v.get("reason").and_then(Value::as_str) else {
        return Verdict::Invalid("`reason` is missing or not a string".into());
    };
    match v.get("verdict").and_then(Value::as_str) {
        Some("approve") => Verdict::Approve,
        Some("reject") => Verdict::Reject(reason.to_string()),
        other => Verdict::Invalid(format!("unknown verdict {other:?}")),
    }
}

/// `Ok` for a `done` execute handoff; otherwise why the thread stops.
fn read_execute_handoff(path: &Path) -> Result<(), String> {
    let data: Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .ok_or("the executor wrote no valid handoff")?;
    match data.get("status").and_then(Value::as_str) {
        Some("plan_mismatch") => Err(format!(
            "the plan did not fit the code: {}",
            data.get("reason").and_then(Value::as_str).unwrap_or("")
        )),
        Some("done")
            if data.get("action").and_then(Value::as_str) == Some("fix")
                && data.get("verification").is_some_and(Value::is_object) =>
        {
            Ok(())
        }
        _ => Err("the executor's handoff is malformed".into()),
    }
}

/// The checks' summary line per command plus each command's output tail.
fn checks_text(result: &VerificationResult) -> String {
    let mut text = result.output.clone();
    for c in result
        .evidence
        .get("commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let field = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("");
        text.push_str(&format!("\n\n$ {}\n{}", field("command"), field("output")));
    }
    tail(&text, CHECKS_TAIL_CHARS)
}

/// The hunk of a file's unified diff that holds new-file line `line`, else
/// the whole diff.
fn hunk_around(diff: &str, line: Option<i64>) -> String {
    let Some(line) = line else {
        return diff.to_string();
    };
    let mut hunks: Vec<(i64, i64, Vec<&str>)> = Vec::new();
    for l in diff.lines() {
        if let Some(rest) = l.strip_prefix("@@ ") {
            let new = rest
                .split_whitespace()
                .find_map(|p| p.strip_prefix('+'))
                .unwrap_or("0");
            let (start, len) = new.split_once(',').unwrap_or((new, "1"));
            hunks.push((
                start.parse().unwrap_or(0),
                len.parse().unwrap_or(1),
                vec![l],
            ));
        } else if let Some(h) = hunks.last_mut() {
            h.2.push(l);
        }
    }
    hunks
        .into_iter()
        .find(|(start, len, _)| (*start..start + len.max(&1)).contains(&line))
        .map(|(_, _, lines)| lines.join("\n") + "\n")
        .unwrap_or_else(|| diff.to_string())
}

/// The note the loop posts: the reply plus its signature. GitLab runs a line
/// starting with `/` as a quick action (`/merge`, `/approve`, `/close`, ...)
/// with the poster's token, the user's own here, so such a reply is refused.
pub fn safe_reply_body(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("the reply is empty".into());
    }
    let quick_action = text.lines().any(|l| {
        l.trim_start()
            .strip_prefix('/')
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_alphabetic()))
    });
    if quick_action {
        return Err("reply contains a GitLab quick action".into());
    }
    Ok(format!("{text}\n\n{LOOP_SIGNATURE}"))
}

/// A ref-safe, never-empty, time-stamped name for a thread's discarded work.
fn keep_ref_name(discussion_id: &str) -> String {
    let mut name: String = discussion_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(64)
        .collect();
    if name.is_empty() {
        // FNV-1a of the raw id.
        let mut h: u64 = 0xcbf29ce484222325;
        for b in discussion_id.bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
        }
        name = format!("d{h:016x}");
    }
    format!("{name}-{}", chrono::Utc::now().format("%Y%m%d%H%M%S"))
}

/// `git status --porcelain` lines for tracked files (untracked `??` output,
/// such as files a test run leaves behind, is allowed).
fn tracked_changes(status: &str) -> Vec<&str> {
    status
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("??"))
        .collect()
}

fn normalize_path(p: &str) -> String {
    let p = p.trim();
    p.strip_prefix("./").unwrap_or(p).to_string()
}

/// The hard rules applied to what the executor actually changed.
fn files_rule(changed: &[String], planned: &[String]) -> Option<String> {
    let planned: std::collections::BTreeSet<String> =
        planned.iter().map(|p| normalize_path(p)).collect();
    let changed: Vec<String> = changed.iter().map(|p| normalize_path(p)).collect();
    if changed.len() > mr_plan::MAX_FILES {
        return Some(format!(
            "the change touches {} files (limit {}): {}",
            changed.len(),
            mr_plan::MAX_FILES,
            changed.join(", ")
        ));
    }
    let outside: Vec<&str> = changed
        .iter()
        .filter(|f| !planned.contains(*f))
        .map(String::as_str)
        .collect();
    (!outside.is_empty()).then(|| {
        format!(
            "the change touches files outside the plan: {}",
            outside.join(", ")
        )
    })
}

/// The user's trusted instructions for a thread, if they gave any.
fn user_instructions(state: &IssueThreads, id: &str) -> Option<String> {
    state
        .threads
        .get(id)
        .and_then(|t| t.user_instructions.clone())
}

fn is_loop_note(n: &Value, state: &IssueThreads) -> bool {
    !n.get("system").and_then(Value::as_bool).unwrap_or(false)
        && (state.ledger.contains(&note_id(n))
            || n.get("body")
                .and_then(Value::as_str)
                .is_some_and(|b| b.contains(LOOP_SIGNATURE)))
}

impl Round<'_> {
    fn log(&self, text: &str) {
        self.ctx.log(&format!(
            "mr feedback {} #{}: {text}",
            self.alias, self.issue_iid
        ));
    }

    fn emit(&self, event_type: &str, data: Value) {
        self.ctx.emit(
            event_type,
            Some(self.run_id),
            EventFields {
                issue_run_id: Some(format!("{}_{}_{}", self.run_id, self.alias, self.issue_iid)),
                project: Some(self.alias.to_string()),
                issue_iid: Some(self.iid),
                data: Some(data),
            },
        );
    }

    fn save(&self, state: &IssueThreads) {
        if let Err(e) = state.save(self.alias, self.iid, self.ctx.memory_root.as_deref()) {
            self.log(&format!("saving thread state failed: {e}"));
        }
    }

    fn run(&self) -> Result<RoundOutcome, String> {
        // An in-place checkout is the user's working copy: the round never
        // touches it, so the whole round is skipped for such a project.
        let local_path = self
            .deps
            .project
            .get("local_path")
            .and_then(Value::as_str)
            .unwrap_or("");
        let config = crate::issue_runner::project_config_path(self.ctx);
        if !crate::config::use_worktree(&self.deps.project)
            || !crate::worktree::uses_worktree_for(local_path, &config)
        {
            self.log("skipped: the project works in its checkout (use_worktree is off), so review feedback on its merge request is left to you");
            return Ok(RoundOutcome::Skipped);
        }
        let gitlab = &*self.deps.gitlab;
        let Some(mr) = gitlab.find_mr(self.iid)? else {
            return Ok(RoundOutcome::Skipped);
        };
        let discussions = gitlab.discussions(mr)?;
        let mut state = IssueThreads::load(self.alias, self.iid, self.ctx.memory_root.as_deref());
        reopen_resolved(&mut state, &discussions);
        let actionable = actionable_threads(&discussions, &state);
        let stuck = stuck_threads(&state, &discussions);
        if actionable.is_empty() && stuck.is_empty() {
            return Ok(RoundOutcome::Skipped);
        }
        self.undo_unvalidated(&state);
        let mr_state = gitlab.mr_state(mr)?;
        match mr_state.as_str() {
            "opened" => {}
            "merged" | "closed" => {
                self.resolve_all(&mut state, &discussions, &mr_state);
                return Ok(RoundOutcome::Skipped);
            }
            other => {
                self.log(&format!(
                    "merge request !{mr} is in state {other:?}; doing nothing this run"
                ));
                return Ok(RoundOutcome::Skipped);
            }
        }
        let (mut answered, mut changed, mut needs_you) = (0, 0, 0);
        for (id, latest) in &stuck {
            self.needs_you(
                &mut state,
                id,
                None,
                "the loop stopped twice while handling this thread",
                *latest,
            );
            needs_you += 1;
        }
        for a in &actionable {
            match self.thread(&mut state, mr, a) {
                ThreadEnd::Answered => answered += 1,
                ThreadEnd::Changed => changed += 1,
                ThreadEnd::NeedsYou => needs_you += 1,
                ThreadEnd::Waiting => {}
                ThreadEnd::MrNotOpen(s) => {
                    if s == "merged" || s == "closed" {
                        self.resolve_all(&mut state, &discussions, &s);
                    }
                    break;
                }
            }
        }
        Ok(RoundOutcome::Processed {
            answered,
            changed,
            needs_you,
        })
    }

    /// A run that stopped between the executor's commit and the push leaves
    /// unvalidated commits on the branch, past the `base` the thread recorded.
    /// They are taken off (kept under a ref) before anything else looks at
    /// the branch, so neither this round nor the issue pass can push them.
    fn undo_unvalidated(&self, state: &IssueThreads) {
        let ws = &*self.deps.workspace;
        for (id, t) in &state.threads {
            let Some(base) = t
                .base
                .as_deref()
                .filter(|_| t.status == ThreadStatus::Working)
            else {
                continue;
            };
            if !matches!(ws.sync(), Ok(Branch::Ahead(tip)) if tip == base) {
                continue;
            }
            let worktree = match ws.ensure() {
                Ok(w) => w,
                Err(e) => {
                    self.push_blocked.set(true);
                    self.log(&format!(
                        "thread {id}: unvalidated commits on the branch, and its worktree is unusable ({e}); no pushes this round"
                    ));
                    continue;
                }
            };
            self.log(&format!(
                "thread {id}: taking unvalidated commits from a stopped run off the branch"
            ));
            self.rollback(&worktree, base, id);
        }
    }

    fn decide(&self, id: &str, choice: UserChoice) -> Result<(), String> {
        let mut state = IssueThreads::load(self.alias, self.iid, self.ctx.memory_root.as_deref());
        let Some(t) = state.threads.get(id).cloned() else {
            return Err(format!("there is no review thread {id} for this issue"));
        };
        if t.status != ThreadStatus::NeedsYou {
            return Err(format!(
                "the thread is {}, not waiting on you",
                serde_json::to_value(t.status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
            ));
        }
        let name = choice.name();
        match choice {
            UserChoice::Dismiss => {
                // Everything in the thread so far counts as seen; only a newer
                // human note brings it back. GitLab is asked on a best-effort
                // basis: the decision stands without it.
                let latest = self
                    .open_thread(id)
                    .map(|(_, d)| max_note_id(discussion_notes(&d)))
                    .unwrap_or(0);
                self.settle(&mut state, id, ThreadStatus::Decided, latest);
            }
            UserChoice::Instructions(text) => {
                let text = text.trim();
                if text.is_empty() {
                    return Err("write what the loop should do with the thread".into());
                }
                let entry = state.threads.get_mut(id).expect("checked above");
                entry.status = ThreadStatus::New;
                entry.attempts = 0;
                entry.user_instructions = Some(text.chars().take(INSTRUCTIONS_MAX_CHARS).collect());
                entry.updated_at = now();
                self.save(&state);
            }
            UserChoice::EditReply(text) => {
                let body = safe_reply_body(&text)?;
                let (mr, d) = self.open_mr_thread(&mut state, id)?;
                let note = self.deps.gitlab.reply(mr, id, &body)?;
                let posted = note.get("id").and_then(Value::as_i64);
                if let Some(n) = posted {
                    state.ledger.insert(n);
                }
                let latest = max_note_id(discussion_notes(&d)).max(posted.unwrap_or(0));
                self.settle(&mut state, id, ThreadStatus::Answered, latest);
                self.emit(
                    "mr_feedback.replied",
                    json!({"discussion_id": id, "note_id": posted, "action": "user_reply"}),
                );
            }
            UserChoice::Approve => self.approve(&mut state, id, &t)?,
        }
        self.log(&format!("thread {id}: the user chose {name}"));
        self.emit(
            "mr_feedback.decided",
            json!({"discussion_id": id, "choice": name}),
        );
        Ok(())
    }

    /// The merge request and the discussion `id` on it, as GitLab has them.
    fn open_thread(&self, id: &str) -> Result<(i64, Value), String> {
        let mr = self
            .deps
            .gitlab
            .find_mr(self.iid)?
            .ok_or("the issue has no merge request")?;
        let d = self
            .deps
            .gitlab
            .discussions(mr)?
            .into_iter()
            .find(|d| d.get("id").and_then(Value::as_str) == Some(id))
            .ok_or("the discussion is no longer on the merge request")?;
        Ok((mr, d))
    }

    /// [`Self::open_thread`] on a merge request that is still open; a merged or
    /// closed one resolves the thread instead.
    fn open_mr_thread(&self, state: &mut IssueThreads, id: &str) -> Result<(i64, Value), String> {
        let (mr, d) = self.open_thread(id)?;
        match self.deps.gitlab.mr_state(mr)?.as_str() {
            "opened" => Ok((mr, d)),
            s @ ("merged" | "closed") => {
                self.resolve_all(state, std::slice::from_ref(&d), s);
                Err(format!("the merge request is {s}; nothing was done"))
            }
            other => Err(format!(
                "the merge request is in state {other:?}; nothing was done"
            )),
        }
    }

    /// A decided thread: `status`, every note up to `latest` seen, no reason.
    fn settle(&self, state: &mut IssueThreads, id: &str, status: ThreadStatus, latest: i64) {
        self.finish(state, id, status, None, None, latest);
    }

    /// Executes the stored plan the user approved, through the same guards
    /// as an automatic one.
    fn approve(&self, state: &mut IssueThreads, id: &str, t: &ThreadState) -> Result<(), String> {
        let plan = approvable_plan(t)?;
        let (mr, d) = self.open_mr_thread(state, id)?;
        let notes: Vec<Value> = discussion_notes(&d)
            .iter()
            .filter(|n| !n.get("system").and_then(Value::as_bool).unwrap_or(false))
            .cloned()
            .collect();
        if notes
            .iter()
            .any(|n| crate::mr_precheck::is_human(n, state) && note_id(n) > t.last_note_id)
        {
            return Err(
                "someone replied in the thread after this plan was made; the loop plans it again on its next run"
                    .into(),
            );
        }
        let a = Actionable {
            discussion_id: id.to_string(),
            position: notes.first().and_then(|n| n.get("position")).cloned(),
            notes,
        };
        let latest = max_note_id(&a.notes);
        let end = match plan.action {
            PlanAction::Wait => {
                self.finish(
                    state,
                    id,
                    ThreadStatus::WaitingOnReviewer,
                    Some(&plan),
                    None,
                    latest,
                );
                ThreadEnd::Waiting
            }
            PlanAction::ReplyOnly => self.post(state, mr, id, &plan, latest),
            PlanAction::ChangeCode => {
                // Its notes are already seen, so a crash would leave it working
                // with nothing to retry it: marked as on its last attempt, the
                // next round hands it straight back to the user (after taking
                // any unvalidated commit off the branch).
                self.start_working(state, mr, &a, 2);
                match self.clean_snapshot(state, id, Some(&plan), latest) {
                    Ok(_) => self.change_code(state, mr, &a, &plan, latest),
                    Err(end) => end,
                }
            }
        };
        match end {
            ThreadEnd::Answered | ThreadEnd::Changed | ThreadEnd::Waiting => Ok(()),
            ThreadEnd::NeedsYou => Err(state
                .threads
                .get(id)
                .and_then(|t| t.reason.clone())
                .unwrap_or_else(|| "the plan was not carried out".into())),
            ThreadEnd::MrNotOpen(s) => {
                if s == "merged" || s == "closed" {
                    self.resolve_all(state, std::slice::from_ref(&d), &s);
                }
                Err(format!("the merge request is {s}; nothing was pushed"))
            }
        }
    }

    /// A merged or closed MR: every thread is done, nothing runs.
    fn resolve_all(&self, state: &mut IssueThreads, discussions: &[Value], mr_state: &str) {
        for d in discussions {
            let Some(id) = d.get("id").and_then(Value::as_str) else {
                continue;
            };
            let t = state
                .threads
                .entry(id.to_string())
                .or_insert_with(|| fresh(id));
            t.status = ThreadStatus::Resolved;
            t.last_note_id = t.last_note_id.max(max_note_id(discussion_notes(d)));
            t.updated_at = now();
        }
        self.save(state);
        self.log(&format!("merge request is {mr_state}; threads resolved"));
    }

    /// Settles a thread: status, plan, and every note up to `latest` seen.
    fn finish(
        &self,
        state: &mut IssueThreads,
        id: &str,
        status: ThreadStatus,
        plan: Option<&Plan>,
        reason: Option<&str>,
        latest: i64,
    ) {
        let t = state
            .threads
            .entry(id.to_string())
            .or_insert_with(|| fresh(id));
        t.status = status;
        if let Some(p) = plan {
            t.plan_summary = p.summary.clone();
            t.confidence = Some(p.confidence);
            t.plan = Some(plan_json(p));
        } else if let Some(r) = reason {
            t.plan_summary = r.to_string();
        }
        t.reason = reason.map(str::to_owned);
        t.base = None;
        // The user's instructions are for the plan they triggered: used once.
        t.user_instructions = None;
        t.last_note_id = t.last_note_id.max(latest);
        t.updated_at = now();
        self.save(state);
    }

    fn needs_you(
        &self,
        state: &mut IssueThreads,
        id: &str,
        plan: Option<&Plan>,
        reason: &str,
        latest: i64,
    ) -> ThreadEnd {
        let reason: String = reason.chars().take(500).collect();
        self.finish(
            state,
            id,
            ThreadStatus::NeedsYou,
            plan,
            Some(&reason),
            latest,
        );
        self.log(&format!("thread {id} needs you: {reason}"));
        self.emit(
            "mr_feedback.needs_you",
            json!({
                "discussion_id": id,
                "reason": reason,
                "confidence": plan.map(|p| p.confidence),
                "action": plan.map(|p| action_name(p.action)),
            }),
        );
        let link = crate::issue_runner::slack_issue_link(self.alias, self.issue_iid, None);
        (self.deps.notifier)(&format!(
            "{link}: a review thread on the merge request needs you ({reason}). Nothing was posted; decide in LoopX under Needs you."
        ));
        ThreadEnd::NeedsYou
    }

    /// One agent call, within what is left of the round's budget.
    fn call(&self, phase: Phase, prompt: &str) -> Result<String, String> {
        let max = self.definition.stop_conditions.max_cost_usd;
        if self.spent.get() >= max {
            return Err(format!("this round's budget of ${max:.2} is spent"));
        }
        let call = PhaseCall {
            phase,
            model: self.definition.agent.model_for(phase),
            prompt,
            max_budget_usd: cost::remaining_budget(Some(max), Some(self.spent.get()), 0.05),
        };
        match (self.deps.invoke)(&call) {
            Ok(out) => {
                self.spent
                    .set(self.spent.get() + out.cost_usd.unwrap_or(0.0));
                Ok(out.text)
            }
            Err(e) => {
                self.spent.set(self.spent.get() + e.cost_usd.unwrap_or(0.0));
                Err(e.message)
            }
        }
    }

    fn thread(&self, state: &mut IssueThreads, mr: i64, a: &Actionable) -> ThreadEnd {
        let id = a.discussion_id.as_str();
        let prev = state.threads.get(id).cloned();
        let latest = max_note_id(&a.notes);
        // A retry after a crash: the reply or the push may already be out.
        if let Some(p) = prev.as_ref().filter(|p| p.status == ThreadStatus::Working) {
            if let Some(end) = self.recover(state, mr, a, p, latest) {
                return end;
            }
        }
        let attempts = match &prev {
            Some(p) if p.status == ThreadStatus::Working => p.attempts.saturating_add(1),
            _ => 1,
        };
        self.start_working(state, mr, a, attempts);
        // The planner and validator can edit files (there is no read-only
        // allowlist), so the branch and worktree are pinned around them.
        let before = match self.clean_snapshot(state, id, None, latest) {
            Ok(s) => s,
            Err(end) => return end,
        };
        let decision = self.plan(mr, a, prev.as_ref());
        let (label, plan) = match &decision {
            Decision::Execute(p) => ("execute", Some(p)),
            Decision::Wait(p) => ("wait", Some(p)),
            Decision::NeedsYou { plan, .. } => ("needs_you", plan.as_ref()),
        };
        self.emit(
            "mr_feedback.planned",
            json!({
                "discussion_id": id,
                "decision": label,
                "action": plan.map(|p| action_name(p.action)),
                "confidence": plan.map(|p| p.confidence),
            }),
        );
        if let Some(end) = self.check_untouched(state, id, &before, plan, latest) {
            return end;
        }
        match decision {
            Decision::NeedsYou { plan, reason } => {
                self.needs_you(state, id, plan.as_ref(), &reason, latest)
            }
            Decision::Wait(plan) => {
                self.finish(
                    state,
                    id,
                    ThreadStatus::WaitingOnReviewer,
                    Some(&plan),
                    None,
                    latest,
                );
                ThreadEnd::Waiting
            }
            Decision::Execute(plan) => {
                if let Err(reason) = safe_reply_body(&plan.reply) {
                    return self.needs_you(state, id, Some(&plan), &reason, latest);
                }
                match plan.action {
                    PlanAction::ReplyOnly => self.reply_only(state, mr, a, &plan, latest, &before),
                    PlanAction::ChangeCode => self.change_code(state, mr, a, &plan, latest),
                    PlanAction::Wait => {
                        self.needs_you(state, id, Some(&plan), "unexpected wait plan", latest)
                    }
                }
            }
        }
    }

    /// Marks the thread `working` before any agent runs, with `last_note_id`
    /// left as is so a crash leaves it for one more try, and keeps what the
    /// user needs to see should it end up with them.
    fn start_working(&self, state: &mut IssueThreads, mr: i64, a: &Actionable, attempts: u8) {
        let comment = a
            .notes
            .iter()
            .filter(|n| crate::mr_precheck::is_human(n, state))
            .max_by_key(|n| note_id(n))
            .map(|n| ReviewNote {
                note_id: note_id(n),
                author: n
                    .pointer("/author/username")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                body: n
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .chars()
                    .take(COMMENT_MAX_CHARS)
                    .collect(),
            });
        let id = a.discussion_id.as_str();
        let t = state
            .threads
            .entry(id.to_string())
            .or_insert_with(|| fresh(id));
        t.status = ThreadStatus::Working;
        t.attempts = attempts;
        t.mr_iid = Some(mr);
        if comment.is_some() {
            t.last_comment = comment;
        }
        t.updated_at = now();
        self.save(state);
    }

    /// The branch and worktree as they are, which must hold no uncommitted
    /// tracked change; otherwise the thread goes to the user.
    fn clean_snapshot(
        &self,
        state: &mut IssueThreads,
        id: &str,
        plan: Option<&Plan>,
        latest: i64,
    ) -> Result<Snapshot, ThreadEnd> {
        match self.deps.workspace.snapshot() {
            Ok(s) if tracked_changes(&s.status).is_empty() => Ok(s),
            Ok(_) => Err(self.needs_you(
                state,
                id,
                plan,
                "the issue's worktree has uncommitted changes",
                latest,
            )),
            Err(e) => Err(self.needs_you(
                state,
                id,
                plan,
                &format!("could not read the issue's worktree: {e}"),
                latest,
            )),
        }
    }

    /// A `working` thread whose last run stopped after GitLab already got
    /// the reply, or after the change was pushed: finish it without planning
    /// (or posting) again.
    fn recover(
        &self,
        state: &mut IssueThreads,
        mr: i64,
        a: &Actionable,
        prev: &ThreadState,
        latest: i64,
    ) -> Option<ThreadEnd> {
        let id = a.discussion_id.as_str();
        let last_human = a
            .notes
            .iter()
            .filter(|n| crate::mr_precheck::is_human(n, state))
            .map(note_id)
            .max()
            .unwrap_or(0);
        let ours: Vec<i64> = a
            .notes
            .iter()
            .filter(|n| is_loop_note(n, state))
            .map(note_id)
            .collect();
        if ours.iter().any(|n| *n > last_human) {
            state.ledger.extend(ours);
            self.finish(state, id, ThreadStatus::Answered, None, None, latest);
            self.log(&format!(
                "thread {id} was answered before the last run stopped"
            ));
            return Some(ThreadEnd::Answered);
        }
        let base = prev.base.as_deref()?;
        let plan = mr_plan::parse_plan(&prev.plan.as_ref()?.to_string()).ok()?;
        if plan.action != PlanAction::ChangeCode {
            return None;
        }
        let ws = &*self.deps.workspace;
        match ws.sync() {
            Ok(Branch::InSync(tip)) if tip != base && ws.is_ancestor(base, &tip) == Ok(true) => {
                self.log(&format!(
                    "thread {id}: its change was pushed before the last run stopped; replying"
                ));
                Some(match self.post(state, mr, id, &plan, latest) {
                    ThreadEnd::Answered => ThreadEnd::Changed,
                    other => other,
                })
            }
            _ => None,
        }
    }

    /// `None` when the branch and worktree still match `expected`; otherwise
    /// puts the worktree back and sends the thread to the user.
    fn check_untouched(
        &self,
        state: &mut IssueThreads,
        id: &str,
        expected: &Snapshot,
        plan: Option<&Plan>,
        latest: i64,
    ) -> Option<ThreadEnd> {
        let now = self.deps.workspace.snapshot();
        if now.as_ref() == Ok(expected) {
            return None;
        }
        let mut reason = "an agent changed the worktree outside the execute phase".to_string();
        if let Some((worktree, head)) = &expected.worktree {
            if let Some(e) = self.rollback(worktree, head, id) {
                reason.push_str(&format!(
                    "; rolling the branch back failed ({e}), so nothing more is pushed this round"
                ));
            }
        }
        Some(self.needs_you(state, id, plan, &reason, latest))
    }

    fn plan(&self, mr: i64, a: &Actionable, prev: Option<&ThreadState>) -> Decision {
        let hunk = a.position.as_ref().and_then(|p| {
            let path = p.get("new_path").and_then(Value::as_str)?;
            let diff = self.deps.gitlab.file_diff(mr, path)?;
            Some(hunk_around(
                &diff,
                p.get("new_line").and_then(Value::as_i64),
            ))
        });
        let input = mr_plan::build_plan_input(a, prev, hunk.as_deref());
        let prompt = format!(
            "{}\n\n## The thread\n\n{input}",
            prompt::mr_feedback(&self.ctx.repo_root, Phase::Plan)
        );
        match self.call(Phase::Plan, &prompt) {
            Ok(text) => mr_plan::decide(mr_plan::parse_plan(&text)),
            Err(e) => Decision::NeedsYou {
                plan: None,
                reason: format!("the planner failed: {e}"),
            },
        }
    }

    /// The validator's verdict on the plan's reply and, for a code change,
    /// the diff and the checks' output. Everything it reads is fenced.
    fn validate(
        &self,
        a: &Actionable,
        plan: &Plan,
        change: Option<(&str, &str)>,
        instructions: Option<&str>,
    ) -> Verdict {
        let nonce = events::new_uuid_hex();
        let mut input = mr_plan::build_plan_input_with_nonce(a, None, None, &nonce);
        if let Some(text) = instructions {
            mr_plan::trusted_instructions(&mut input, &nonce, text);
        }
        let plan_text = serde_json::to_string_pretty(&plan_json(plan)).unwrap_or_default();
        mr_plan::fenced(&mut input, "PLAN", &nonce, &plan_text);
        mr_plan::fenced(&mut input, "REPLY DRAFT", &nonce, &plan.reply);
        if let Some((diff, checks)) = change {
            mr_plan::fenced(&mut input, "DIFF", &nonce, diff);
            mr_plan::fenced(&mut input, "CHECKS OUTPUT", &nonce, checks);
        }
        let prompt = format!(
            "{}\n\n## The thread and the response to check\n\n{input}",
            prompt::mr_feedback(&self.ctx.repo_root, Phase::Validate)
        );
        match self.call(Phase::Validate, &prompt) {
            Ok(text) => parse_verdict(&text),
            Err(e) => Verdict::Invalid(format!("the validator failed: {e}")),
        }
    }

    fn reply_only(
        &self,
        state: &mut IssueThreads,
        mr: i64,
        a: &Actionable,
        plan: &Plan,
        latest: i64,
        before: &Snapshot,
    ) -> ThreadEnd {
        let id = a.discussion_id.as_str();
        let verdict = self.validate(a, plan, None, user_instructions(state, id).as_deref());
        if let Some(end) = self.check_untouched(state, id, before, Some(plan), latest) {
            return end;
        }
        match verdict {
            Verdict::Approve => self.post(state, mr, id, plan, latest),
            Verdict::Reject(r) => self.needs_you(
                state,
                id,
                Some(plan),
                &format!("the validator rejected the reply: {r}"),
                latest,
            ),
            Verdict::Invalid(e) => self.needs_you(
                state,
                id,
                Some(plan),
                &format!("the validator's output was unusable: {e}"),
                latest,
            ),
        }
    }

    /// Posts the plan's reply, signed, in the reviewer's thread and records
    /// the note in the ledger so no later round answers the loop itself.
    fn post(
        &self,
        state: &mut IssueThreads,
        mr: i64,
        id: &str,
        plan: &Plan,
        latest: i64,
    ) -> ThreadEnd {
        let body = match safe_reply_body(&plan.reply) {
            Ok(b) => b,
            Err(reason) => return self.needs_you(state, id, Some(plan), &reason, latest),
        };
        match self.deps.gitlab.reply(mr, id, &body) {
            Ok(note) => {
                let posted = note.get("id").and_then(Value::as_i64);
                if let Some(n) = posted {
                    state.ledger.insert(n);
                }
                self.finish(
                    state,
                    id,
                    ThreadStatus::Answered,
                    Some(plan),
                    None,
                    latest.max(posted.unwrap_or(0)),
                );
                self.emit(
                    "mr_feedback.replied",
                    json!({"discussion_id": id, "note_id": posted, "action": action_name(plan.action)}),
                );
                ThreadEnd::Answered
            }
            Err(e) => self.needs_you(
                state,
                id,
                Some(plan),
                &format!("posting the reply failed: {e}"),
                latest,
            ),
        }
    }

    fn execute_prompt(
        &self,
        a: &Actionable,
        plan: &Plan,
        worktree: &Path,
        handoff: &Path,
        feedback: Option<&str>,
        instructions: Option<&str>,
    ) -> String {
        let nonce = events::new_uuid_hex();
        let mut input = mr_plan::build_plan_input_with_nonce(a, None, None, &nonce);
        if let Some(text) = instructions {
            mr_plan::trusted_instructions(&mut input, &nonce, text);
        }
        let plan_text = serde_json::to_string_pretty(&plan_json(plan)).unwrap_or_default();
        mr_plan::fenced(&mut input, "PLAN", &nonce, &plan_text);
        if let Some(f) = feedback {
            input.push_str(
                "\nYour previous attempt was not accepted; the reason follows. Fix that within the plan, commit, and write a new handoff.\n",
            );
            mr_plan::fenced(&mut input, "PREVIOUS ATTEMPT", &nonce, f);
        }
        format!(
            "{}\n\n## This run\n\nWorktree: {}\nBranch: loop/issue-{}\nHandoff path: {}\n\n## The thread and the approved plan\n\n{input}",
            prompt::mr_feedback(&self.ctx.repo_root, Phase::Execute),
            worktree.display(),
            self.issue_iid,
            handoff.display(),
        )
    }

    /// Sends the thread to the user after a code change that will not be
    /// pushed, taking its commits off the branch first so a later push (this
    /// run's issue pass included) cannot carry them.
    #[allow(clippy::too_many_arguments)]
    fn abandon(
        &self,
        state: &mut IssueThreads,
        worktree: &Path,
        base: &str,
        id: &str,
        plan: &Plan,
        reason: &str,
        latest: i64,
    ) -> ThreadEnd {
        let mut reason = reason.to_string();
        if let Some(e) = self.rollback(worktree, base, id) {
            reason.push_str(&format!(
                "; rolling the branch back failed ({e}), so nothing more is pushed this round"
            ));
        }
        self.needs_you(state, id, Some(plan), &reason, latest)
    }

    /// Resets the branch to `base`, keeping what it drops under a ref. On
    /// failure no later push happens this round; returns the error.
    fn rollback(&self, worktree: &Path, base: &str, id: &str) -> Option<String> {
        match self
            .deps
            .workspace
            .rollback(worktree, base, &keep_ref_name(id))
        {
            Ok(()) => None,
            Err(e) => {
                self.push_blocked.set(true);
                self.log(&format!(
                    "rolling {} back to {base} failed: {e}; no more pushes this round",
                    worktree.display()
                ));
                Some(e)
            }
        }
    }

    fn change_code(
        &self,
        state: &mut IssueThreads,
        mr: i64,
        a: &Actionable,
        plan: &Plan,
        latest: i64,
    ) -> ThreadEnd {
        let id = a.discussion_id.as_str();
        let ws = &*self.deps.workspace;
        let branch = crate::worktree::branch_name(self.issue_iid);
        let local_path = self
            .deps
            .project
            .get("local_path")
            .and_then(Value::as_str)
            .unwrap_or("");
        let refuse = |state: &mut IssueThreads, reason: &str| {
            self.needs_you(state, id, Some(plan), reason, latest)
        };
        if local_path.is_empty() {
            return refuse(state, "the project has no local checkout");
        }
        // Every change builds on the remote branch tip, so what is pushed is
        // exactly the commits validated below.
        let base = match ws.sync() {
            Ok(Branch::InSync(b)) => b,
            Ok(Branch::Diverged) => {
                return refuse(
                    state,
                    &format!("someone else pushed to {branch}; the loop does not build on commits it has not seen"),
                )
            }
            Ok(Branch::Ahead(_)) => {
                return refuse(
                    state,
                    &format!("{branch} has unpushed commits; the loop only pushes commits it validated in this round"),
                )
            }
            Err(e) => {
                return refuse(
                    state,
                    &format!("could not compare {branch} with its remote: {e}"),
                )
            }
        };
        if self.push_blocked.get() {
            return refuse(
                state,
                "an earlier rollback in this round failed, so nothing more is changed or pushed",
            );
        }
        let worktree = match ws.ensure() {
            Ok(w) => w,
            Err(e) => return refuse(state, &format!("could not prepare the worktree: {e}")),
        };
        match ws.head(&worktree) {
            Ok(h) if h == base => {}
            Ok(h) => {
                return refuse(
                    state,
                    &format!("the worktree is at {h}, not at {branch}'s tip {base}"),
                )
            }
            Err(e) => return refuse(state, &format!("could not read the worktree's HEAD: {e}")),
        }
        // Recorded so a retry after a crash can tell the change was pushed.
        if let Some(t) = state.threads.get_mut(id) {
            t.base = Some(base.clone());
            t.plan = Some(plan_json(plan));
        }
        self.save(state);
        let handoff = handoff_path(
            &self.ctx.repo_root,
            self.run_id,
            self.alias,
            self.issue_iid,
            id,
        );
        let attempts = self.definition.stop_conditions.max_iterations.max(1);
        let instructions = user_instructions(state, id);
        let mut feedback: Option<String> = None;
        let mut approved: Option<String> = None;
        let abandon = |state: &mut IssueThreads, reason: &str| {
            self.abandon(state, &worktree, &base, id, plan, reason, latest)
        };
        for _ in 0..attempts {
            let _ = std::fs::remove_file(&handoff);
            if let Some(dir) = handoff.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let prompt = self.execute_prompt(
                a,
                plan,
                &worktree,
                &handoff,
                feedback.as_deref(),
                instructions.as_deref(),
            );
            if let Err(e) = self.call(Phase::Execute, &prompt) {
                return abandon(state, &format!("the executor failed: {e}"));
            }
            if let Err(reason) = read_execute_handoff(&handoff) {
                return abandon(state, &reason);
            }
            let head = match ws.head(&worktree) {
                Ok(h) if h != base => h,
                Ok(_) => return abandon(state, "the executor committed nothing"),
                Err(e) => return abandon(state, &format!("could not read HEAD: {e}")),
            };
            // Only commits are validated and pushed, so the checks must run on
            // exactly them: no tracked edits may be left uncommitted.
            match ws.snapshot() {
                Ok(snap) if tracked_changes(&snap.status).is_empty() => {}
                Ok(snap) => {
                    return abandon(
                        state,
                        &format!(
                            "the executor left uncommitted changes: {}",
                            tracked_changes(&snap.status).join(", ")
                        ),
                    )
                }
                Err(e) => return abandon(state, &format!("could not read the worktree: {e}")),
            }
            // The hard rules hold for what changed, not only for the plan.
            match ws.changed_files(&worktree, &base) {
                Ok(files) => {
                    if let Some(reason) = files_rule(&files, &plan.files) {
                        return abandon(state, &reason);
                    }
                }
                Err(e) => return abandon(state, &format!("could not list changed files: {e}")),
            }
            let checks = (self.deps.verify)(&handoff);
            let checks_out = checks_text(&checks);
            if !checks.passed {
                feedback = Some(format!("The project's checks failed:\n{checks_out}"));
                continue;
            }
            let diff = match ws.diff(&worktree, &base) {
                Ok(d) if d.chars().count() > DIFF_MAX_CHARS => {
                    return abandon(state, "the diff is too large to validate")
                }
                Ok(d) => d,
                Err(e) => return abandon(state, &format!("could not read the diff: {e}")),
            };
            let pinned = match ws.snapshot() {
                Ok(s) if tracked_changes(&s.status).is_empty() => s,
                Ok(s) => {
                    return abandon(
                        state,
                        &format!(
                            "the project's checks changed tracked files: {}",
                            tracked_changes(&s.status).join(", ")
                        ),
                    )
                }
                Err(e) => return abandon(state, &format!("could not read the worktree: {e}")),
            };
            let verdict =
                self.validate(a, plan, Some((&diff, &checks_out)), instructions.as_deref());
            if ws.snapshot().as_ref() != Ok(&pinned) {
                return abandon(
                    state,
                    "an agent changed the worktree outside the execute phase",
                );
            }
            match verdict {
                Verdict::Approve => {
                    approved = Some(head);
                    break;
                }
                Verdict::Reject(r) => feedback = Some(format!("The validator rejected it: {r}")),
                Verdict::Invalid(e) => {
                    return abandon(state, &format!("the validator's output was unusable: {e}"))
                }
            }
        }
        let Some(approved) = approved else {
            return abandon(
                state,
                &format!(
                    "the change was not accepted after {attempts} attempt(s): {}",
                    feedback.as_deref().unwrap_or("")
                ),
            );
        };
        if self.push_blocked.get() {
            return abandon(
                state,
                "an earlier rollback in this round failed, so nothing more is pushed",
            );
        }
        // Never push to a merge request that is no longer open.
        match self.deps.gitlab.mr_state(mr) {
            Ok(s) if s == "opened" => {}
            Ok(s) => {
                self.rollback(&worktree, &base, id);
                return ThreadEnd::MrNotOpen(s);
            }
            Err(e) => {
                return abandon(
                    state,
                    &format!("could not confirm the merge request is still open: {e}"),
                )
            }
        }
        // Push only the commit the validator approved.
        match ws.head(&worktree) {
            Ok(h) if h == approved => {}
            _ => return abandon(state, "the branch moved after validation"),
        }
        if let Err(e) = crate::mr::check_branch(&branch) {
            return abandon(state, &e);
        }
        let (ok, detail) = (self.deps.pusher)(local_path, &branch);
        if !ok {
            return abandon(
                state,
                &format!("pushing {branch} failed: {}", tail(&detail, 500)),
            );
        }
        self.emit(
            "mr_feedback.executed",
            json!({"discussion_id": id, "branch": branch, "head": approved, "files": plan.files}),
        );
        match self.post(state, mr, id, plan, latest) {
            ThreadEnd::Answered => ThreadEnd::Changed,
            other => other,
        }
    }
}

// ---- the real seams ---------------------------------------------------------

const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// `(exit code, stdout or stdout+stderr)` of `git -C dir args`.
fn git(dir: &str, args: &[&str]) -> Result<(Option<i32>, String), String> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    match crate::proc::run(&mut cmd, None, Some(GIT_TIMEOUT)) {
        Ok(o) if o.timed_out => Err(format!("git {} timed out", args.join(" "))),
        Ok(o) if o.success() => Ok((o.code, o.stdout)),
        Ok(o) => Ok((o.code, format!("{}{}", o.stdout, o.stderr))),
        Err(e) => Err(format!("git {}: {e}", args.join(" "))),
    }
}

fn git_ok(dir: &str, args: &[&str]) -> Result<String, String> {
    match git(dir, args)? {
        (Some(0), out) => Ok(out.trim().to_string()),
        (_, out) => Err(format!("git {}: {}", args.join(" "), out.trim())),
    }
}

/// The issue's real worktree under `worktree_root`, on `loop/issue-<iid>`.
pub struct GitWorkspace {
    pub local_path: String,
    pub target_branch: String,
    pub issue_iid: String,
    pub worktree_root: String,
}

impl GitWorkspace {
    fn branch(&self) -> String {
        crate::worktree::branch_name(&self.issue_iid)
    }

    fn worktree(&self) -> PathBuf {
        PathBuf::from(crate::worktree::worktree_path(
            &self.local_path,
            &self.issue_iid,
            &self.worktree_root,
        ))
    }

    /// The commit `rev` names in the checkout, if it exists.
    fn rev(&self, rev: &str) -> Result<Option<String>, String> {
        match git(&self.local_path, &["rev-parse", "--verify", "--quiet", rev])? {
            (Some(0), out) => Ok(Some(out.trim().to_string())),
            (Some(1), _) => Ok(None),
            (_, out) => Err(format!("git rev-parse {rev}: {}", out.trim())),
        }
    }
}

impl Workspace for GitWorkspace {
    fn sync(&self) -> Result<Branch, String> {
        // Parallel issues of one checkout fetch into the same repository.
        let _lock = crate::worktree::checkout_lock(&self.local_path).map_err(|f| f.message)?;
        let branch = self.branch();
        let remote_ref = format!("refs/remotes/origin/{branch}");
        let local_ref = format!("refs/heads/{branch}");
        let remote = match git(
            &self.local_path,
            &["ls-remote", "--exit-code", "--heads", "origin", &local_ref],
        )? {
            (Some(0), _) => {
                git_ok(
                    &self.local_path,
                    &["fetch", "origin", &format!("+{local_ref}:{remote_ref}")],
                )?;
                self.rev(&remote_ref)?
            }
            (Some(2), _) => None,
            (_, out) => return Err(format!("git ls-remote: {}", out.trim())),
        };
        match (self.rev(&local_ref)?, remote) {
            (None, Some(r)) => {
                // The branch was swept with its worktree: rebuild it from the MR's.
                git_ok(&self.local_path, &["branch", &branch, &r])?;
                Ok(Branch::InSync(r))
            }
            (None, None) => Err(format!("{branch} exists neither locally nor on the remote")),
            (Some(l), Some(r)) if l == r => Ok(Branch::InSync(r)),
            (Some(l), Some(r)) => Ok(if self.is_ancestor(&r, &l)? {
                Branch::Ahead(r)
            } else {
                Branch::Diverged
            }),
            (Some(l), None) => {
                let target = format!("refs/remotes/origin/{}", self.target_branch);
                let base = git_ok(&self.local_path, &["merge-base", &target, &l])?;
                Ok(if base == l {
                    Branch::InSync(base)
                } else {
                    Branch::Ahead(base)
                })
            }
        }
    }

    fn ensure(&self) -> Result<PathBuf, String> {
        let wt =
            crate::worktree::reuse_worktree(&self.local_path, &self.issue_iid, &self.worktree_root)
                .map(PathBuf::from)
                .map_err(|f| f.message)?;
        let (on, branch) = (
            git_ok(
                &wt.display().to_string(),
                &["symbolic-ref", "--short", "HEAD"],
            )?,
            self.branch(),
        );
        if on != branch {
            return Err(format!("{} is on {on}, not {branch}", wt.display()));
        }
        Ok(wt)
    }

    fn snapshot(&self) -> Result<Snapshot, String> {
        let branch = self.rev(&format!("refs/heads/{}", self.branch()))?;
        let wt = self.worktree();
        if !wt.is_dir() {
            return Ok(Snapshot {
                branch,
                ..Snapshot::default()
            });
        }
        let dir = wt.display().to_string();
        let head = git_ok(&dir, &["rev-parse", "HEAD"])?;
        let status = git_ok(&dir, &["status", "--porcelain"])?;
        Ok(Snapshot {
            branch,
            worktree: Some((wt, head)),
            status,
        })
    }

    fn head(&self, worktree: &Path) -> Result<String, String> {
        git_ok(&worktree.display().to_string(), &["rev-parse", "HEAD"])
    }

    fn diff(&self, worktree: &Path, base: &str) -> Result<String, String> {
        git_ok(&worktree.display().to_string(), &["diff", base, "HEAD"])
    }

    fn changed_files(&self, worktree: &Path, base: &str) -> Result<Vec<String>, String> {
        let out = git_ok(
            &worktree.display().to_string(),
            // A rename would list only the new path, hiding its source.
            &["diff", "--name-only", "--no-renames", base, "HEAD"],
        )?;
        Ok(out
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect())
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, String> {
        match git(
            &self.local_path,
            &["merge-base", "--is-ancestor", ancestor, descendant],
        )? {
            (Some(0), _) => Ok(true),
            (Some(1), _) => Ok(false),
            (_, out) => Err(format!("git merge-base: {}", out.trim())),
        }
    }

    fn rollback(&self, worktree: &Path, base: &str, keep_as: &str) -> Result<(), String> {
        let dir = worktree.display().to_string();
        let mut errors = Vec::new();
        match self.head(worktree) {
            Ok(head) if head != base => {
                let short: String = head.chars().take(12).collect();
                let keep = format!("refs/loopx/mr-feedback/{keep_as}-{short}");
                if let Err(e) = git_ok(&dir, &["update-ref", &keep, &head]) {
                    errors.push(e);
                }
            }
            Ok(_) => {}
            Err(e) => errors.push(e),
        }
        // The reset runs even when keeping the ref failed: unvalidated
        // commits must not stay on the branch (they remain in the reflog).
        if let Err(e) = git_ok(&dir, &["reset", "--hard", base]) {
            errors.push(e);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

/// The project's GitLab, with the user's own token from the GitLab config.
pub struct RealMrGitlab {
    pub project_id: String,
    pub instance: Option<String>,
}

impl RealMrGitlab {
    fn client(&self) -> Result<crate::gitlab::Client, String> {
        let config = crate::gitlab::load_config();
        crate::gitlab::Client::from_config(config.as_ref(), self.instance.as_deref(), None)
            .map_err(|e| e.to_string())
    }
}

impl MrGitlab for RealMrGitlab {
    fn find_mr(&self, issue_iid: i64) -> Result<Option<i64>, String> {
        let branch = crate::worktree::branch_name(&issue_iid.to_string());
        let mrs = self
            .client()?
            .get_all(
                &format!(
                    "projects/{}/merge_requests",
                    crate::gitlab::quote(&self.project_id)
                ),
                &[("source_branch", &branch)],
            )
            .map_err(|e| e.to_string())?;
        let open = mrs
            .iter()
            .find(|m| m.get("state").and_then(Value::as_str) == Some("opened"));
        Ok(open
            .or(mrs.first())
            .and_then(|m| m.get("iid"))
            .and_then(Value::as_i64))
    }

    fn mr_state(&self, mr_iid: i64) -> Result<String, String> {
        self.client()?
            .mr_state(&self.project_id, mr_iid)
            .map_err(|e| e.to_string())
    }

    fn discussions(&self, mr_iid: i64) -> Result<Vec<Value>, String> {
        self.client()?
            .list_mr_discussions(&self.project_id, mr_iid)
            .map_err(|e| e.to_string())
    }

    fn reply(&self, mr_iid: i64, discussion_id: &str, body: &str) -> Result<Value, String> {
        self.client()?
            .reply_to_mr_discussion(&self.project_id, mr_iid, discussion_id, body)
            .map_err(|e| e.to_string())
    }

    fn file_diff(&self, mr_iid: i64, path: &str) -> Option<String> {
        let changes = self
            .client()
            .ok()?
            .get_json(&format!(
                "projects/{}/merge_requests/{mr_iid}/changes",
                crate::gitlab::quote(&self.project_id)
            ))
            .ok()?;
        changes
            .get("changes")?
            .as_array()?
            .iter()
            .find(|c| c.get("new_path").and_then(Value::as_str) == Some(path))
            .and_then(|c| c.get("diff"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }
}

impl RoundDeps {
    /// The real GitLab, agent CLI, git, push and Slack for `alias`. `None`
    /// for a project the loop never opens a GitLab merge request for (an
    /// issues-only or `code_host` project).
    pub fn real(
        ctx: &RunCtx,
        alias: &str,
        issue_iid: &str,
        definition: &LoopDefinition,
    ) -> Result<Option<RoundDeps>, String> {
        let config = crate::config::Config::load(&crate::issue_runner::project_config_path(ctx))
            .map_err(|e| e.to_string())?;
        let project = config.project(alias).map_err(|e| e.to_string())?;
        if crate::hosted::is_hosted(&project) || !crate::config::changes_code(&project) {
            return Ok(None);
        }
        let text = |k: &str| {
            project
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let project_id = text("project_id");
        if project_id.is_empty() {
            return Err(format!("project {alias} has no project_id"));
        }
        let worktree_root = config.worktree_root().map_err(|e| e.to_string())?;
        let (bundle, _) = crate::issue_runner::project_slack_target(alias, &project, None);

        let invoke: PhaseInvoker = {
            let (ctx, definition) = (ctx.clone(), definition.clone());
            let (alias, iid) = (alias.to_string(), issue_iid.to_string());
            Box::new(move |call: &PhaseCall<'_>| {
                // Every phase runs with the gate allowlist: no push, no MR.
                crate::issue_runner::invoke_phase_with(
                    &ctx,
                    call.model,
                    &definition,
                    call.prompt,
                    (&alias, &iid),
                    true,
                    call.max_budget_usd,
                )
            })
        };
        let verify = {
            let (alias, iid) = (alias.to_string(), issue_iid.to_string());
            let (config_path, definition) = (ctx.config_path.clone(), definition.clone());
            let timeout = definition.stop_conditions.max_runtime_minutes * 60.0;
            Box::new(move |handoff: &Path| {
                use crate::loop_verifiers::{ProjectCommandsVerifier, Verifier};
                ProjectCommandsVerifier::new("project_commands", &alias, &iid, timeout)
                    .with_handoff_path(Some(handoff.to_path_buf()))
                    .with_config_path(config_path.clone())
                    .verify(&crate::loop_runtime::LoopContext {
                        iteration: 1,
                        definition: &definition,
                        previous: None,
                    })
            })
        };
        Ok(Some(RoundDeps {
            gitlab: Box::new(RealMrGitlab {
                project_id,
                instance: project
                    .get("instance")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }),
            invoke,
            pusher: Box::new(crate::issue_runner::real_pusher),
            notifier: Box::new(move |m: &str| {
                crate::runner_util::notify_slack_best_effort(m, None, bundle.as_deref());
            }),
            verify,
            workspace: Box::new(GitWorkspace {
                local_path: text("local_path"),
                target_branch: text("target_branch"),
                issue_iid: issue_iid.to_string(),
                worktree_root,
            }),
            project,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mr_threads::{IssueThreads, ThreadState, ThreadStatus, LOOP_SIGNATURE};
    use serde_json::json;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    #[derive(Default)]
    struct Log {
        calls: Vec<(Phase, Option<String>)>,
        prompts: Vec<String>,
        replies: Vec<(i64, String, String)>,
        pushes: Vec<(String, String)>,
        order: Vec<String>,
        notified: Vec<String>,
        ensured: usize,
        rollbacks: Vec<(String, String)>,
    }

    type Shared<T> = Rc<RefCell<T>>;

    struct FakeGitlab {
        log: Shared<Log>,
        mr: Option<i64>,
        state: Shared<String>,
        discussions: Shared<Vec<Value>>,
        next_id: Cell<i64>,
    }

    impl MrGitlab for FakeGitlab {
        fn find_mr(&self, issue_iid: i64) -> Result<Option<i64>, String> {
            assert_eq!(issue_iid, 7);
            Ok(self.mr)
        }
        fn mr_state(&self, _: i64) -> Result<String, String> {
            Ok(self.state.borrow().clone())
        }
        fn discussions(&self, _: i64) -> Result<Vec<Value>, String> {
            Ok(self.discussions.borrow().clone())
        }
        fn reply(&self, mr: i64, discussion: &str, body: &str) -> Result<Value, String> {
            let id = self.next_id.get();
            self.next_id.set(id + 1);
            let mut log = self.log.borrow_mut();
            log.replies.push((mr, discussion.into(), body.into()));
            log.order.push("reply".into());
            // GitLab now shows the loop's note in the thread.
            for d in self.discussions.borrow_mut().iter_mut() {
                if d["id"] == discussion {
                    d["notes"]
                        .as_array_mut()
                        .unwrap()
                        .push(note(id, "loop answer without signature"));
                }
            }
            Ok(json!({"id": id, "body": body}))
        }
        fn file_diff(&self, _: i64, _: &str) -> Option<String> {
            None
        }
    }

    struct FakeWorkspace {
        log: Shared<Log>,
        /// Sync results to give first, before `branch`.
        syncs: Shared<VecDeque<Branch>>,
        branch: Option<Branch>,
        head: Shared<String>,
        dirty: Shared<String>,
        changed: Option<Vec<String>>,
        rollback_fails: bool,
        big_diff: bool,
    }

    impl Workspace for FakeWorkspace {
        fn sync(&self) -> Result<Branch, String> {
            if let Some(b) = self.syncs.borrow_mut().pop_front() {
                return Ok(b);
            }
            Ok(self
                .branch
                .clone()
                .unwrap_or_else(|| Branch::InSync(self.head.borrow().clone())))
        }
        fn ensure(&self) -> Result<PathBuf, String> {
            self.log.borrow_mut().ensured += 1;
            Ok(PathBuf::from("/wt/repo-issue-7"))
        }
        fn snapshot(&self) -> Result<Snapshot, String> {
            let head = self.head.borrow().clone();
            Ok(Snapshot {
                branch: Some(head.clone()),
                worktree: Some((PathBuf::from("/wt/repo-issue-7"), head)),
                status: self.dirty.borrow().clone(),
            })
        }
        fn head(&self, _: &Path) -> Result<String, String> {
            Ok(self.head.borrow().clone())
        }
        fn diff(&self, _: &Path, base: &str) -> Result<String, String> {
            if self.big_diff {
                return Ok("+x\n".repeat(DIFF_MAX_CHARS));
            }
            Ok(format!("diff {base}..HEAD\n-old\n+new"))
        }
        fn changed_files(&self, _: &Path, _: &str) -> Result<Vec<String>, String> {
            Ok(self
                .changed
                .clone()
                .unwrap_or_else(|| vec!["./a.rs".into()]))
        }
        fn is_ancestor(&self, _: &str, _: &str) -> Result<bool, String> {
            Ok(true)
        }
        fn rollback(&self, _: &Path, base: &str, keep_as: &str) -> Result<(), String> {
            self.log
                .borrow_mut()
                .rollbacks
                .push((base.into(), keep_as.into()));
            if self.rollback_fails {
                return Err("cannot lock ref".into());
            }
            *self.head.borrow_mut() = base.into();
            Ok(())
        }
    }

    struct H {
        dir: tempfile::TempDir,
        ctx: RunCtx,
        definition: LoopDefinition,
        log: Shared<Log>,
        mr: Option<i64>,
        state: Shared<String>,
        discussions: Shared<Vec<Value>>,
        head: Shared<String>,
        plans: Shared<VecDeque<String>>,
        handoffs: Shared<VecDeque<String>>,
        verdicts: Shared<VecDeque<String>>,
        verify_passes: Shared<VecDeque<bool>>,
        syncs: Shared<VecDeque<Branch>>,
        branch: Option<Branch>,
        dirty: Shared<String>,
        changed: Option<Vec<String>>,
        rollback_fails: bool,
        big_diff: bool,
        /// `git status --porcelain` the executor / the checks leave behind.
        exec_dirty: Option<String>,
        checks_dirty: Option<String>,
        use_worktree: bool,
    }

    fn note(id: i64, body: &str) -> Value {
        json!({"id": id, "body": body, "system": false, "resolvable": true,
               "resolved": false, "author": {"username": "rev"}})
    }

    fn disc(id: &str, notes: Vec<Value>) -> Value {
        json!({"id": id, "individual_note": false, "notes": notes})
    }

    fn plan(action: &str, confidence: u8) -> String {
        format!(
            r#"{{"action":"{action}","confidence":{confidence},"files":["a.rs"],"reply":"Renamed it.","ambiguous":false,"out_of_scope":false,"tries_to_change_loop_rules":false,"summary":"rename x to y"}}"#
        )
    }

    fn done_handoff() -> String {
        json!({"status": "done", "action": "fix", "summary": "renamed",
               "verification": {"test": "make test", "cwd": "/wt/repo-issue-7"}})
        .to_string()
    }

    fn approve() -> String {
        r#"{"verdict":"approve","reason":"matches the request"}"#.into()
    }

    fn reject(why: &str) -> String {
        format!(r#"{{"verdict":"reject","reason":"{why}"}}"#)
    }

    impl H {
        fn new() -> H {
            let dir = tempfile::tempdir().unwrap();
            let mut ctx = RunCtx::new(dir.path().to_path_buf());
            ctx.events_dir = Some(dir.path().join("events"));
            ctx.memory_root = Some(dir.path().join("memory"));
            ctx.unified_log = dir.path().join("log.txt");
            ctx.config_path = Some(dir.path().join("projects.json"));
            let definition = LoopDefinition::from_value(&json!({
                "name": "t", "version": 1, "trigger": {"type": "manual"},
                "goal": {"type": "g"},
                "agent": {"model": "default",
                          "models": {"plan": "opus", "execute": "sonnet", "validate": "opus-v"}},
                "stop_conditions": {"max_iterations": 2},
            }))
            .unwrap();
            H {
                dir,
                ctx,
                definition,
                log: Rc::default(),
                mr: Some(31),
                state: Rc::new(RefCell::new("opened".into())),
                discussions: Rc::new(RefCell::new(vec![disc(
                    "d1",
                    vec![note(1, "please rename x")],
                )])),
                head: Rc::new(RefCell::new("base".into())),
                plans: Rc::default(),
                handoffs: Rc::default(),
                verdicts: Rc::default(),
                verify_passes: Rc::default(),
                syncs: Rc::default(),
                branch: None,
                dirty: Rc::default(),
                changed: None,
                rollback_fails: false,
                big_diff: false,
                exec_dirty: None,
                checks_dirty: None,
                use_worktree: true,
            }
        }

        fn deps(&self) -> RoundDeps {
            let (log, head) = (self.log.clone(), self.head.clone());
            let (plans, handoffs, verdicts) = (
                self.plans.clone(),
                self.handoffs.clone(),
                self.verdicts.clone(),
            );
            let projects = json!({"web": {"project_id": "g/p", "local_path": "/repo",
                                          "target_branch": "main", "use_worktree": self.use_worktree}});
            std::fs::write(
                self.ctx.config_path.as_ref().unwrap(),
                json!({"worktree_root": "/wt", "projects": projects}).to_string(),
            )
            .unwrap();
            let commits = Cell::new(0);
            let (dirty, exec_dirty) = (self.dirty.clone(), self.exec_dirty.clone());
            let invoke: PhaseInvoker = Box::new(move |call: &PhaseCall<'_>| {
                {
                    let mut l = log.borrow_mut();
                    l.calls.push((call.phase, call.model.map(str::to_owned)));
                    l.prompts.push(call.prompt.to_string());
                    l.order.push(format!("{:?}", call.phase));
                }
                let pop = |q: &Shared<VecDeque<String>>| {
                    q.borrow_mut()
                        .pop_front()
                        .unwrap_or_else(|| panic!("unexpected {:?} call", call.phase))
                };
                // "COMMIT <text>": the agent commits in the worktree, then answers.
                let agent_commit = |text: String| match text.strip_prefix("COMMIT ") {
                    Some(rest) => {
                        *head.borrow_mut() = "agent-commit".into();
                        rest.to_string()
                    }
                    None => text,
                };
                let text = match call.phase {
                    Phase::Plan => agent_commit(pop(&plans)),
                    Phase::Validate => agent_commit(pop(&verdicts)),
                    Phase::Execute => {
                        let handoff = pop(&handoffs);
                        let path = call
                            .prompt
                            .lines()
                            .find_map(|l| l.strip_prefix("Handoff path: "))
                            .expect("the execute prompt names the handoff path");
                        std::fs::create_dir_all(Path::new(path).parent().unwrap()).unwrap();
                        std::fs::write(path, &handoff).unwrap();
                        if handoff.contains("\"done\"") {
                            commits.set(commits.get() + 1);
                            *head.borrow_mut() = format!("c{}", commits.get());
                        }
                        if let Some(d) = &exec_dirty {
                            *dirty.borrow_mut() = d.clone();
                        }
                        "finished".to_string()
                    }
                };
                if text == "PANIC" {
                    panic!("simulated crash");
                }
                Ok(AgentOutput {
                    cost_usd: Some(0.01),
                    text,
                    ..AgentOutput::default()
                })
            });
            let push_log = self.log.clone();
            let notify_log = self.log.clone();
            let passes = self.verify_passes.clone();
            let (dirty, checks_dirty) = (self.dirty.clone(), self.checks_dirty.clone());
            RoundDeps {
                gitlab: Box::new(FakeGitlab {
                    log: self.log.clone(),
                    mr: self.mr,
                    state: self.state.clone(),
                    discussions: self.discussions.clone(),
                    next_id: Cell::new(100),
                }),
                invoke,
                pusher: Box::new(move |local, branch| {
                    let mut l = push_log.borrow_mut();
                    l.pushes.push((local.into(), branch.into()));
                    l.order.push("push".into());
                    (true, "pushed".into())
                }),
                notifier: Box::new(move |m| notify_log.borrow_mut().notified.push(m.into())),
                verify: Box::new(move |handoff| {
                    assert!(handoff.exists());
                    let passed = passes.borrow_mut().pop_front().unwrap_or(true);
                    if let Some(d) = &checks_dirty {
                        *dirty.borrow_mut() = d.clone();
                    }
                    VerificationResult {
                        name: "project_commands".into(),
                        passed,
                        exit_code: Some(if passed { 0 } else { 1 }),
                        duration_ms: 1,
                        output: format!(
                            "$ make test: {}",
                            if passed { "passed" } else { "failed" }
                        ),
                        evidence: json!({"commands": [{"command": "make test", "passed": passed,
                                         "output": if passed { "ok" } else { "1 failure: test_rename" }}]}),
                    }
                }),
                workspace: Box::new(FakeWorkspace {
                    log: self.log.clone(),
                    syncs: self.syncs.clone(),
                    branch: self.branch.clone(),
                    head: self.head.clone(),
                    dirty: self.dirty.clone(),
                    changed: self.changed.clone(),
                    rollback_fails: self.rollback_fails,
                    big_diff: self.big_diff,
                }),
                project: json!({"project_id": "g/p", "local_path": "/repo", "instance": "g",
                                "target_branch": "main", "use_worktree": self.use_worktree}),
            }
        }

        fn run(&self) -> RoundOutcome {
            run_round(&self.ctx, "r1", "web", "7", &self.definition, &self.deps())
        }

        fn threads(&self) -> IssueThreads {
            IssueThreads::load("web", 7, self.ctx.memory_root.as_deref())
        }

        fn thread(&self) -> ThreadState {
            self.threads().threads["d1"].clone()
        }

        fn save(&self, t: &IssueThreads) {
            t.save("web", 7, self.ctx.memory_root.as_deref()).unwrap();
        }

        fn events(&self, kind: &str) -> Vec<Value> {
            let Ok(dir) = std::fs::read_dir(self.dir.path().join("events")) else {
                return Vec::new();
            };
            dir.flatten()
                .flat_map(|e| crate::events::read_events_file(&e.path()))
                .filter(|e| e["event_type"] == kind)
                .collect()
        }
    }

    fn stored(status: ThreadStatus, last: i64, attempts: u8) -> IssueThreads {
        let mut t = IssueThreads::default();
        t.threads.insert(
            "d1".into(),
            ThreadState {
                discussion_id: "d1".into(),
                last_note_id: last,
                status,
                plan_summary: String::new(),
                confidence: None,
                attempts,
                updated_at: String::new(),
                plan: None,
                reason: None,
                base: None,
                user_instructions: None,
                mr_iid: None,
                last_comment: None,
            },
        );
        t
    }

    #[test]
    fn quiet_mr_launches_no_agent() {
        // Nothing new in the thread: no agent, no worktree.
        let h = H::new();
        h.save(&stored(ThreadStatus::Answered, 1, 1));
        assert_eq!(h.run(), RoundOutcome::Skipped);
        // No merge request for the branch at all.
        let mut h = H::new();
        h.mr = None;
        assert_eq!(h.run(), RoundOutcome::Skipped);
        // Only the loop's own (ledgered) note.
        let h = H::new();
        let mut t = IssueThreads::default();
        t.ledger.insert(1);
        h.save(&t);
        assert_eq!(h.run(), RoundOutcome::Skipped);
        let log = h.log.borrow();
        assert!(log.calls.is_empty() && log.ensured == 0 && log.replies.is_empty());
    }

    #[test]
    fn high_confidence_reply_only_posts_signed_reply_in_thread_and_records_ledger() {
        let h = H::new();
        h.plans.borrow_mut().push_back(plan("reply_only", 95));
        h.verdicts.borrow_mut().push_back(approve());
        assert_eq!(
            h.run(),
            RoundOutcome::Processed {
                answered: 1,
                changed: 0,
                needs_you: 0
            }
        );
        {
            let log = h.log.borrow();
            assert_eq!(log.replies.len(), 1);
            let (mr, discussion, body) = &log.replies[0];
            assert_eq!((*mr, discussion.as_str()), (31, "d1"));
            assert!(body.starts_with("Renamed it."), "{body}");
            assert!(body.trim_end().ends_with(LOOP_SIGNATURE), "{body}");
            assert!(log.pushes.is_empty() && log.ensured == 0);
            // The validator saw the draft, fenced.
            let validate = &log.prompts[1];
            assert!(validate.contains("Renamed it.") && validate.contains("UNTRUSTED"));
        }
        let threads = h.threads();
        assert!(threads.ledger.contains(&100));
        let t = &threads.threads["d1"];
        assert_eq!(t.status, ThreadStatus::Answered);
        assert_eq!(t.last_note_id, 100);
        assert_eq!(t.confidence, Some(95));
        assert_eq!(h.events("mr_feedback.replied").len(), 1);
        assert_eq!(h.events("mr_feedback.planned").len(), 1);
        // The next run sees its own note and does nothing.
        assert_eq!(h.run(), RoundOutcome::Skipped);
        assert_eq!(h.log.borrow().calls.len(), 2);
    }

    #[test]
    fn low_confidence_posts_nothing_and_sets_needs_you() {
        let h = H::new();
        h.plans.borrow_mut().push_back(plan("change_code", 60));
        assert_eq!(
            h.run(),
            RoundOutcome::Processed {
                answered: 0,
                changed: 0,
                needs_you: 1
            }
        );
        let log = h.log.borrow();
        assert_eq!(log.calls.len(), 1, "only the planner ran");
        assert!(log.replies.is_empty() && log.pushes.is_empty() && log.ensured == 0);
        assert_eq!(log.notified.len(), 1);
        assert!(log.notified[0].contains("web #7"), "{}", log.notified[0]);
        let t = h.thread();
        assert_eq!(t.status, ThreadStatus::NeedsYou);
        assert_eq!(t.confidence, Some(60));
        assert_eq!(t.plan_summary, "rename x to y");
        assert_eq!(t.last_note_id, 1);
        assert_eq!(t.plan.as_ref().unwrap()["action"], "change_code");
        let ev = h.events("mr_feedback.needs_you");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0]["data"]["discussion_id"], "d1");
        assert!(ev[0]["data"]["reason"]
            .as_str()
            .unwrap()
            .contains("confidence"));
    }

    #[test]
    fn invalid_plan_goes_to_needs_you() {
        let h = H::new();
        h.plans
            .borrow_mut()
            .push_back("I think we should rename".into());
        let out = h.run();
        assert!(matches!(out, RoundOutcome::Processed { needs_you: 1, .. }));
        assert_eq!(h.thread().status, ThreadStatus::NeedsYou);
        assert!(h.log.borrow().replies.is_empty());
    }

    #[test]
    fn code_change_commits_pushes_then_replies() {
        let h = H::new();
        h.plans.borrow_mut().push_back(plan("change_code", 92));
        h.handoffs.borrow_mut().push_back(done_handoff());
        h.verdicts.borrow_mut().push_back(approve());
        assert_eq!(
            h.run(),
            RoundOutcome::Processed {
                answered: 0,
                changed: 1,
                needs_you: 0
            }
        );
        let log = h.log.borrow();
        assert_eq!(log.pushes, vec![("/repo".into(), "loop/issue-7".into())]);
        assert_eq!(log.order, ["Plan", "Execute", "Validate", "push", "reply"]);
        assert_eq!(log.ensured, 1);
        assert!(log.rollbacks.is_empty());
        // The executor got the plan and the worktree; the validator the diff
        // and the checks' output.
        assert!(log.prompts[1].contains("/wt/repo-issue-7"));
        assert!(log.prompts[1].contains("rename x to y"));
        assert!(log.prompts[2].contains("diff base..HEAD"));
        assert!(log.prompts[2].contains("make test"));
        let t = h.thread();
        assert_eq!(t.status, ThreadStatus::Answered);
        assert!(h.threads().ledger.contains(&100));
        assert_eq!(h.events("mr_feedback.executed").len(), 1);
    }

    #[test]
    fn validation_rejection_retries_then_needs_you() {
        let h = H::new();
        h.plans.borrow_mut().push_back(plan("change_code", 92));
        h.handoffs
            .borrow_mut()
            .extend([done_handoff(), done_handoff()]);
        h.verdicts
            .borrow_mut()
            .extend([reject("also rename the test"), reject("still wrong")]);
        let out = h.run();
        assert!(matches!(
            out,
            RoundOutcome::Processed {
                needs_you: 1,
                changed: 0,
                ..
            }
        ));
        let log = h.log.borrow();
        let phases: Vec<Phase> = log.calls.iter().map(|c| c.0).collect();
        assert_eq!(
            phases,
            [
                Phase::Plan,
                Phase::Execute,
                Phase::Validate,
                Phase::Execute,
                Phase::Validate
            ]
        );
        // The second attempt is told why the first was rejected.
        assert!(log.prompts[3].contains("also rename the test"));
        assert!(log.pushes.is_empty() && log.replies.is_empty());
        // The rejected commits leave the branch, so nothing pushes them later.
        assert_eq!(log.rollbacks.len(), 1);
        assert_eq!(log.rollbacks[0].0, "base");
        let t = h.thread();
        assert_eq!(t.status, ThreadStatus::NeedsYou);
        assert!(h.events("mr_feedback.needs_you")[0]["data"]["reason"]
            .as_str()
            .unwrap()
            .contains("still wrong"));
    }

    #[test]
    fn failing_checks_count_as_a_rejection_without_a_validator_call() {
        let h = H::new();
        h.plans.borrow_mut().push_back(plan("change_code", 92));
        h.handoffs
            .borrow_mut()
            .extend([done_handoff(), done_handoff()]);
        h.verify_passes.borrow_mut().push_back(false);
        h.verdicts.borrow_mut().push_back(approve());
        let out = h.run();
        assert!(matches!(out, RoundOutcome::Processed { changed: 1, .. }));
        let log = h.log.borrow();
        let phases: Vec<Phase> = log.calls.iter().map(|c| c.0).collect();
        assert_eq!(
            phases,
            [Phase::Plan, Phase::Execute, Phase::Execute, Phase::Validate]
        );
        assert!(log.prompts[2].contains("1 failure: test_rename"));
    }

    #[test]
    fn plan_mismatch_goes_to_needs_you_without_push() {
        let h = H::new();
        h.plans.borrow_mut().push_back(plan("change_code", 92));
        h.handoffs
            .borrow_mut()
            .push_back(r#"{"status":"plan_mismatch","reason":"x is not defined in a.rs"}"#.into());
        let out = h.run();
        assert!(matches!(out, RoundOutcome::Processed { needs_you: 1, .. }));
        let log = h.log.borrow();
        assert!(log.pushes.is_empty() && log.replies.is_empty());
        assert_eq!(log.calls.len(), 2, "no validation of a mismatch");
        assert!(h.events("mr_feedback.needs_you")[0]["data"]["reason"]
            .as_str()
            .unwrap()
            .contains("x is not defined"));
    }

    #[test]
    fn invalid_validator_output_goes_to_needs_you() {
        let h = H::new();
        h.plans.borrow_mut().push_back(plan("reply_only", 95));
        h.verdicts.borrow_mut().push_back("looks fine to me".into());
        let out = h.run();
        assert!(matches!(out, RoundOutcome::Processed { needs_you: 1, .. }));
        assert!(h.log.borrow().replies.is_empty());
    }

    #[test]
    fn merged_mr_resolves_threads_without_push() {
        let h = H::new();
        *h.state.borrow_mut() = "merged".into();
        assert_eq!(h.run(), RoundOutcome::Skipped);
        let log = h.log.borrow();
        assert!(log.calls.is_empty() && log.pushes.is_empty() && log.replies.is_empty());
        let t = h.thread();
        assert_eq!(t.status, ThreadStatus::Resolved);
        assert_eq!(t.last_note_id, 1);
        drop(log);
        // Stays resolved: the next run does not reopen it.
        assert_eq!(h.run(), RoundOutcome::Skipped);
        assert_eq!(h.thread().status, ThreadStatus::Resolved);
    }

    #[test]
    fn unknown_mr_state_does_nothing() {
        for state in ["", "locked"] {
            let h = H::new();
            *h.state.borrow_mut() = state.into();
            assert_eq!(h.run(), RoundOutcome::Skipped);
            let log = h.log.borrow();
            assert!(log.calls.is_empty() && log.pushes.is_empty() && log.replies.is_empty());
            assert!(h.threads().threads.is_empty(), "{state:?} keeps state");
        }
    }

    #[test]
    fn mr_closed_before_push_pushes_nothing() {
        let h = H::new();
        h.plans.borrow_mut().push_back(plan("change_code", 92));
        h.handoffs.borrow_mut().push_back(done_handoff());
        let state = h.state.clone();
        // Validation approves, but the MR got closed in the meantime.
        h.verdicts.borrow_mut().push_back(approve());
        let deps = h.deps();
        let closing = RoundDeps {
            invoke: Box::new(move |call: &PhaseCall<'_>| {
                if call.phase == Phase::Validate {
                    *state.borrow_mut() = "closed".into();
                }
                (deps.invoke)(call)
            }),
            ..h.deps()
        };
        run_round(&h.ctx, "r1", "web", "7", &h.definition, &closing);
        let log = h.log.borrow();
        assert!(log.pushes.is_empty() && log.replies.is_empty());
        assert_eq!(h.thread().status, ThreadStatus::Resolved);
    }

    #[test]
    fn diverged_branch_goes_to_needs_you() {
        let mut h = H::new();
        h.branch = Some(Branch::Diverged);
        h.plans.borrow_mut().push_back(plan("change_code", 92));
        let out = h.run();
        assert!(matches!(out, RoundOutcome::Processed { needs_you: 1, .. }));
        let log = h.log.borrow();
        assert_eq!(log.calls.len(), 1, "no executor on a diverged branch");
        assert_eq!(log.ensured, 0);
        assert!(log.pushes.is_empty() && log.replies.is_empty());
        assert_eq!(h.thread().status, ThreadStatus::NeedsYou);
    }

    #[test]
    fn a_project_working_in_its_checkout_skips_the_round() {
        {
            let mut h = H::new();
            h.use_worktree = false;
            h.plans.borrow_mut().push_back(plan("reply_only", 95));
            assert_eq!(h.run(), RoundOutcome::Skipped);
            let log = h.log.borrow();
            assert!(log.calls.is_empty() && log.ensured == 0 && log.replies.is_empty());
            let text = std::fs::read_to_string(&h.ctx.unified_log).unwrap();
            assert!(text.contains("use_worktree is off"), "{text}");
        }
    }

    #[test]
    fn a_reply_with_a_quick_action_never_reaches_gitlab() {
        for reply in ["/merge", "Done.\n  /approve", "/assign_reviewer @me"] {
            let h = H::new();
            let p = plan("reply_only", 95).replace("Renamed it.", &reply.replace('\n', "\\n"));
            h.plans.borrow_mut().push_back(p);
            let out = h.run();
            assert!(
                matches!(out, RoundOutcome::Processed { needs_you: 1, .. }),
                "{reply}"
            );
            let log = h.log.borrow();
            assert!(log.replies.is_empty(), "{reply}");
            assert_eq!(log.calls.len(), 1, "no validation of {reply:?}");
            assert_eq!(
                h.thread().reason.as_deref(),
                Some("reply contains a GitLab quick action")
            );
        }
        assert!(safe_reply_body("Use /usr/bin/env or a/b.").is_ok());
        assert!(safe_reply_body("/ 2 is fine").is_ok());
        assert!(safe_reply_body("ok\n/close").is_err());
        assert!(safe_reply_body("  ").is_err());
    }

    #[test]
    fn local_branch_ahead_of_the_remote_goes_to_needs_you_without_push() {
        let mut h = H::new();
        h.branch = Some(Branch::Ahead("base".into()));
        h.plans.borrow_mut().push_back(plan("change_code", 92));
        let out = h.run();
        assert!(matches!(out, RoundOutcome::Processed { needs_you: 1, .. }));
        let log = h.log.borrow();
        assert_eq!(log.calls.len(), 1);
        assert!(log.pushes.is_empty() && log.ensured == 0);
        assert!(h.thread().reason.unwrap().contains("unpushed commits"));
    }

    #[test]
    fn a_planner_commit_goes_to_needs_you_and_is_rolled_back() {
        let h = H::new();
        h.plans
            .borrow_mut()
            .push_back(format!("COMMIT {}", plan("change_code", 92)));
        let out = h.run();
        assert!(matches!(out, RoundOutcome::Processed { needs_you: 1, .. }));
        let log = h.log.borrow();
        assert_eq!(log.calls.len(), 1, "no executor after a planner commit");
        assert!(log.pushes.is_empty() && log.replies.is_empty());
        assert_eq!(log.rollbacks.len(), 1);
        assert_eq!(*h.head.borrow(), "base");
        assert!(h
            .thread()
            .reason
            .unwrap()
            .contains("outside the execute phase"));
    }

    #[test]
    fn a_dirty_worktree_before_planning_goes_to_needs_you() {
        let h = H::new();
        *h.dirty.borrow_mut() = " M a.rs".into();
        let out = h.run();
        assert!(matches!(out, RoundOutcome::Processed { needs_you: 1, .. }));
        assert!(h.log.borrow().calls.is_empty());
    }

    #[test]
    fn a_validator_commit_is_never_pushed() {
        for action in ["change_code", "reply_only"] {
            let h = H::new();
            h.plans.borrow_mut().push_back(plan(action, 92));
            if action == "change_code" {
                h.handoffs.borrow_mut().push_back(done_handoff());
            }
            h.verdicts
                .borrow_mut()
                .push_back(format!("COMMIT {}", approve()));
            let out = h.run();
            assert!(
                matches!(out, RoundOutcome::Processed { needs_you: 1, .. }),
                "{action}"
            );
            let log = h.log.borrow();
            assert!(log.pushes.is_empty() && log.replies.is_empty(), "{action}");
            assert_eq!(*h.head.borrow(), "base", "{action}");
        }
    }

    #[test]
    fn changed_files_outside_the_plan_or_over_the_limit_go_to_needs_you() {
        let cases = [
            (vec!["a.rs", "b.rs"], "outside the plan: b.rs"),
            (
                vec!["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs"],
                "touches 6 files",
            ),
        ];
        for (files, why) in cases {
            let mut h = H::new();
            h.changed = Some(files.iter().map(|f| f.to_string()).collect());
            h.plans.borrow_mut().push_back(plan("change_code", 92));
            h.handoffs.borrow_mut().push_back(done_handoff());
            let out = h.run();
            assert!(matches!(out, RoundOutcome::Processed { needs_you: 1, .. }));
            let log = h.log.borrow();
            assert!(log.pushes.is_empty() && log.replies.is_empty());
            assert_eq!(log.calls.len(), 2, "no checks or validation");
            assert_eq!(log.rollbacks.len(), 1);
            let reason = h.thread().reason.unwrap();
            assert!(reason.contains(why), "{reason}");
        }
    }

    #[test]
    fn a_failed_rollback_blocks_every_later_push_in_the_round() {
        let mut h = H::new();
        h.rollback_fails = true;
        h.discussions
            .borrow_mut()
            .push(disc("d2", vec![note(2, "also rename z")]));
        // d1 is rejected twice and cannot be rolled back; d2 is approved.
        h.plans.borrow_mut().push_back(plan("change_code", 92));
        h.handoffs
            .borrow_mut()
            .extend([done_handoff(), done_handoff()]);
        h.verdicts
            .borrow_mut()
            .extend([reject("no"), reject("still no")]);
        h.plans.borrow_mut().push_back(plan("change_code", 92));
        let out = h.run();
        assert_eq!(
            out,
            RoundOutcome::Processed {
                answered: 0,
                changed: 0,
                needs_you: 2
            }
        );
        let log = h.log.borrow();
        assert!(log.pushes.is_empty() && log.replies.is_empty());
        let threads = h.threads();
        let first = threads.threads["d1"].reason.clone().unwrap();
        assert!(first.contains("rolling the branch back failed"), "{first}");
        let second = threads.threads["d2"].reason.clone().unwrap();
        assert!(second.contains("earlier rollback"), "{second}");
        assert_eq!(h.events("mr_feedback.needs_you").len(), 2);
    }

    fn change_round(h: &H) -> RoundOutcome {
        h.plans.borrow_mut().push_back(plan("change_code", 92));
        h.handoffs.borrow_mut().push_back(done_handoff());
        h.verdicts.borrow_mut().push_back(approve());
        h.run()
    }

    #[test]
    fn executor_leaving_tracked_edits_uncommitted_goes_to_needs_you() {
        let mut h = H::new();
        h.exec_dirty = Some(" M a.rs".into());
        let out = change_round(&h);
        assert!(matches!(out, RoundOutcome::Processed { needs_you: 1, .. }));
        {
            let log = h.log.borrow();
            assert_eq!(log.calls.len(), 2, "no checks or validation");
            assert!(log.pushes.is_empty() && log.replies.is_empty());
            assert_eq!(log.rollbacks.len(), 1);
        }
        let reason = h.thread().reason.unwrap();
        assert!(
            reason.contains("left uncommitted changes:  M a.rs"),
            "{reason}"
        );
        // Untracked leftovers are fine.
        let mut h = H::new();
        h.exec_dirty = Some("?? tmp/out.txt".into());
        assert!(matches!(
            change_round(&h),
            RoundOutcome::Processed { changed: 1, .. }
        ));
    }

    #[test]
    fn checks_that_change_tracked_files_go_to_needs_you() {
        let mut h = H::new();
        h.checks_dirty = Some(" M db/schema.rb".into());
        let out = change_round(&h);
        assert!(matches!(out, RoundOutcome::Processed { needs_you: 1, .. }));
        {
            let log = h.log.borrow();
            assert!(log.pushes.is_empty() && log.replies.is_empty());
            assert_eq!(log.calls.len(), 2, "no validation");
        }
        assert!(h
            .thread()
            .reason
            .unwrap()
            .contains("checks changed tracked files"));
        // Untracked test output is allowed and pushes as usual.
        let mut h = H::new();
        h.checks_dirty = Some("?? coverage/".into());
        assert!(matches!(
            change_round(&h),
            RoundOutcome::Processed { changed: 1, .. }
        ));
        assert_eq!(h.log.borrow().pushes.len(), 1);
    }

    #[test]
    fn a_diff_too_large_to_validate_goes_to_needs_you() {
        let mut h = H::new();
        h.big_diff = true;
        h.plans.borrow_mut().push_back(plan("change_code", 92));
        h.handoffs.borrow_mut().push_back(done_handoff());
        let out = h.run();
        assert!(matches!(out, RoundOutcome::Processed { needs_you: 1, .. }));
        {
            let log = h.log.borrow();
            assert_eq!(log.calls.len(), 2, "no validation of a partial view");
            assert!(log.pushes.is_empty());
        }
        assert!(h.thread().reason.unwrap().contains("too large to validate"));
    }

    #[test]
    fn unvalidated_commits_from_a_stopped_run_are_taken_off_the_branch() {
        let mut h = H::new();
        let mut t = stored(ThreadStatus::Working, 0, 1);
        let entry = t.threads.get_mut("d1").unwrap();
        entry.base = Some("base".into());
        entry.plan = Some(serde_json::from_str(&plan("change_code", 92)).unwrap());
        h.save(&t);
        // The executor committed, then the run stopped before validation.
        *h.head.borrow_mut() = "c1".into();
        h.branch = Some(Branch::Ahead("base".into()));
        h.plans.borrow_mut().push_back(plan("wait", 90));
        h.run();
        {
            let log = h.log.borrow();
            assert_eq!(log.rollbacks.len(), 1);
            assert_eq!(log.rollbacks[0].0, "base");
            assert!(log.rollbacks[0].1.starts_with("d1-"), "{:?}", log.rollbacks);
            assert!(log.pushes.is_empty() && log.replies.is_empty());
            // Not mistaken for a pushed change: the thread was planned again.
            assert_eq!(log.calls.len(), 1);
        }
        assert_eq!(*h.head.borrow(), "base");
        assert_eq!(h.thread().status, ThreadStatus::WaitingOnReviewer);
        // Ahead of something other than the recorded base: left alone.
        let mut h = H::new();
        let mut t = stored(ThreadStatus::Working, 0, 1);
        t.threads.get_mut("d1").unwrap().base = Some("base".into());
        h.save(&t);
        h.branch = Some(Branch::Ahead("other".into()));
        h.plans.borrow_mut().push_back(plan("wait", 90));
        h.run();
        assert!(h.log.borrow().rollbacks.is_empty());
    }

    #[test]
    fn a_failed_undo_of_unvalidated_commits_blocks_pushes() {
        let mut h = H::new();
        h.rollback_fails = true;
        let mut t = stored(ThreadStatus::Working, 0, 1);
        t.threads.get_mut("d1").unwrap().base = Some("base".into());
        h.save(&t);
        *h.head.borrow_mut() = "c1".into();
        // Only the round-start check sees the stopped run's commits; d2's
        // change is then built, validated and approved...
        h.syncs.borrow_mut().push_back(Branch::Ahead("base".into()));
        h.discussions
            .borrow_mut()
            .push(disc("d2", vec![note(2, "rename z")]));
        h.plans.borrow_mut().push_back(plan("wait", 90));
        h.plans.borrow_mut().push_back(plan("change_code", 92));
        h.handoffs.borrow_mut().push_back(done_handoff());
        h.verdicts.borrow_mut().push_back(approve());
        h.run();
        // ...but never pushed after the failed undo.
        assert!(h.log.borrow().pushes.is_empty());
        let reason = h.threads().threads["d2"].reason.clone().unwrap();
        assert!(reason.contains("earlier rollback"), "{reason}");
    }

    #[test]
    fn kept_refs_are_never_empty_and_never_collide_by_name() {
        let a = keep_ref_name("d1");
        assert!(a.starts_with("d1-"), "{a}");
        let b = keep_ref_name("!!!");
        assert!(b.starts_with('d') && b.len() > 18, "{b}");
        assert_ne!(keep_ref_name("!!!"), keep_ref_name("???"));
    }

    #[test]
    fn a_retry_after_the_reply_went_out_does_not_reply_again() {
        let h = H::new();
        h.save(&stored(ThreadStatus::Working, 0, 1));
        let body = format!("Renamed it.\n\n{LOOP_SIGNATURE}");
        h.discussions.borrow_mut()[0]["notes"]
            .as_array_mut()
            .unwrap()
            .push(note(5, &body));
        assert_eq!(
            h.run(),
            RoundOutcome::Processed {
                answered: 1,
                changed: 0,
                needs_you: 0
            }
        );
        let log = h.log.borrow();
        assert!(log.calls.is_empty() && log.replies.is_empty());
        let t = h.threads();
        assert!(t.ledger.contains(&5));
        assert_eq!(t.threads["d1"].status, ThreadStatus::Answered);
        assert_eq!(t.threads["d1"].last_note_id, 5);
    }

    #[test]
    fn a_retry_after_the_push_only_replies() {
        let h = H::new();
        let mut t = stored(ThreadStatus::Working, 0, 1);
        let entry = t.threads.get_mut("d1").unwrap();
        entry.base = Some("base".into());
        entry.plan = Some(serde_json::from_str(&plan("change_code", 92)).unwrap());
        h.save(&t);
        // The remote and local branch already carry the pushed change.
        *h.head.borrow_mut() = "c1".into();
        assert_eq!(
            h.run(),
            RoundOutcome::Processed {
                answered: 0,
                changed: 1,
                needs_you: 0
            }
        );
        let log = h.log.borrow();
        assert!(log.calls.is_empty() && log.pushes.is_empty());
        assert_eq!(log.replies.len(), 1);
        assert_eq!(h.thread().status, ThreadStatus::Answered);
        assert_eq!(h.thread().base, None);
    }

    #[test]
    fn each_phase_uses_its_configured_model() {
        let h = H::new();
        h.plans.borrow_mut().push_back(plan("change_code", 92));
        h.handoffs.borrow_mut().push_back(done_handoff());
        h.verdicts.borrow_mut().push_back(approve());
        h.run();
        let calls = h.log.borrow().calls.clone();
        assert_eq!(
            calls,
            vec![
                (Phase::Plan, Some("opus".to_string())),
                (Phase::Execute, Some("sonnet".to_string())),
                (Phase::Validate, Some("opus-v".to_string())),
            ]
        );
    }

    #[test]
    fn wait_plan_posts_nothing_and_waits_on_reviewer() {
        let h = H::new();
        h.plans.borrow_mut().push_back(plan("wait", 90));
        let out = h.run();
        assert_eq!(
            out,
            RoundOutcome::Processed {
                answered: 0,
                changed: 0,
                needs_you: 0
            }
        );
        let t = h.thread();
        assert_eq!(t.status, ThreadStatus::WaitingOnReviewer);
        assert_eq!(t.last_note_id, 1);
        assert!(h.log.borrow().replies.is_empty());
    }

    #[test]
    fn thread_left_working_by_a_crash_is_retried_exactly_once_more() {
        let h = H::new();
        let crash = |h: &H| {
            h.plans.borrow_mut().push_back("PANIC".into());
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h.run()));
            assert!(r.is_err());
        };
        crash(&h);
        let t = h.thread();
        assert_eq!(
            (t.status, t.attempts, t.last_note_id),
            (ThreadStatus::Working, 1, 0)
        );
        // Retried once more: the planner runs again.
        crash(&h);
        assert_eq!(h.log.borrow().calls.len(), 2);
        let t = h.thread();
        assert_eq!((t.status, t.attempts), (ThreadStatus::Working, 2));
        // Not a third time: the thread goes to the user instead.
        let out = h.run();
        assert_eq!(h.log.borrow().calls.len(), 2);
        assert!(matches!(out, RoundOutcome::Processed { needs_you: 1, .. }));
        assert_eq!(h.thread().status, ThreadStatus::NeedsYou);
        assert_eq!(h.events("mr_feedback.needs_you").len(), 1);
    }

    #[test]
    fn a_finished_thread_starts_its_attempts_afresh() {
        let h = H::new();
        h.save(&stored(ThreadStatus::Answered, 1, 2));
        h.discussions.borrow_mut()[0]["notes"]
            .as_array_mut()
            .unwrap()
            .push(note(2, "not convinced"));
        h.plans.borrow_mut().push_back("PANIC".into());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h.run()));
        let t = h.thread();
        assert_eq!(
            (t.status, t.attempts, t.last_note_id),
            (ThreadStatus::Working, 1, 1)
        );
    }

    #[test]
    fn resolved_thread_reopened_on_gitlab_with_a_newer_note_is_processed() {
        let h = H::new();
        h.save(&stored(ThreadStatus::Resolved, 1, 1));
        h.discussions.borrow_mut()[0]["notes"]
            .as_array_mut()
            .unwrap()
            .push(note(2, "this is still broken"));
        h.plans.borrow_mut().push_back(plan("wait", 90));
        h.run();
        assert_eq!(h.log.borrow().calls.len(), 1);
        assert_eq!(h.thread().status, ThreadStatus::WaitingOnReviewer);
        // A resolved thread with nothing newer stays resolved.
        let h = H::new();
        h.save(&stored(ThreadStatus::Resolved, 1, 1));
        assert_eq!(h.run(), RoundOutcome::Skipped);
        assert_eq!(h.thread().status, ThreadStatus::Resolved);
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@e",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(dir: &Path, file: &str, text: &str) -> String {
        std::fs::write(dir.join(file), text).unwrap();
        sh(dir, &["add", file]);
        sh(dir, &["commit", "-qm", file]);
        sh(dir, &["rev-parse", "HEAD"])
    }

    #[test]
    fn git_workspace_tracks_the_branch_rolls_back_and_spots_a_foreign_push() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        let origin = p.join("origin.git");
        sh(
            p,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                &origin.display().to_string(),
            ],
        );
        sh(p, &["clone", "-q", &origin.display().to_string(), "repo"]);
        let repo = p.join("repo");
        sh(&repo, &["checkout", "-q", "-b", "main"]);
        commit(&repo, "a.txt", "1");
        sh(&repo, &["push", "-q", "origin", "main"]);
        sh(&repo, &["checkout", "-q", "-b", "loop/issue-7"]);
        let pushed = commit(&repo, "b.txt", "loop fix");
        sh(&repo, &["push", "-q", "origin", "loop/issue-7"]);
        sh(&repo, &["checkout", "-q", "main"]);
        // main moves on: the worktree must NOT merge it into the MR branch.
        commit(&repo, "m.txt", "later on main");
        sh(&repo, &["push", "-q", "origin", "main"]);
        let ws = GitWorkspace {
            local_path: repo.display().to_string(),
            target_branch: "main".into(),
            issue_iid: "7".into(),
            worktree_root: p.join("wt").display().to_string(),
        };
        assert_eq!(ws.sync(), Ok(Branch::InSync(pushed.clone())));
        let before = ws.snapshot().unwrap();
        assert_eq!(before.worktree, None);
        let wt = ws.ensure().unwrap();
        assert!(wt.join("b.txt").exists() && !wt.join("m.txt").exists());
        assert_eq!(ws.head(&wt).unwrap(), pushed);
        let snap = ws.snapshot().unwrap();
        assert_eq!(snap.status, "");
        assert_eq!(snap.worktree.as_ref().unwrap().1, pushed);
        std::fs::write(wt.join("scratch.txt"), "x").unwrap();
        assert_ne!(ws.snapshot().unwrap(), snap, "an untracked file shows");
        std::fs::remove_file(wt.join("scratch.txt")).unwrap();

        let fix = commit(&wt, "c.txt", "review fix");
        assert!(ws.diff(&wt, &pushed).unwrap().contains("+review fix"));
        assert_eq!(ws.changed_files(&wt, &pushed).unwrap(), ["c.txt"]);
        assert_eq!(
            ws.sync(),
            Ok(Branch::Ahead(pushed.clone())),
            "unpushed commits"
        );
        assert_eq!(ws.is_ancestor(&pushed, &fix), Ok(true));
        assert_eq!(ws.is_ancestor(&fix, &pushed), Ok(false));
        // A rejected change leaves the branch but is kept under a ref.
        ws.rollback(&wt, &pushed, "d1-x").unwrap();
        assert_eq!(ws.head(&wt).unwrap(), pushed);
        let kept = sh(
            &repo,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/loopx/mr-feedback/d1-x-*",
            ],
        );
        assert!(
            kept.starts_with("refs/loopx/mr-feedback/d1-x-") && kept.ends_with(&fix),
            "{kept}"
        );
        assert_eq!(ws.sync(), Ok(Branch::InSync(pushed.clone())));
        // A rename lists both paths, so an unplanned file cannot hide behind one.
        sh(&wt, &["mv", "b.txt", "planned.txt"]);
        sh(&wt, &["commit", "-qm", "rename"]);
        let files = ws.changed_files(&wt, &pushed).unwrap();
        assert!(files.contains(&"b.txt".to_string()), "{files:?}");
        assert!(files.contains(&"planned.txt".to_string()), "{files:?}");
        assert_eq!(
            files_rule(&files, &["planned.txt".into()]).unwrap(),
            "the change touches files outside the plan: b.txt"
        );
        ws.rollback(&wt, &pushed, "d1-r").unwrap();
        // A rollback that cannot keep the ref still resets, and says so.
        commit(&wt, "e.txt", "again");
        assert!(ws.rollback(&wt, &pushed, "bad..name").is_err());
        assert_eq!(ws.head(&wt).unwrap(), pushed);

        // A human pushes to the MR branch from elsewhere.
        sh(
            p,
            &[
                "clone",
                "-q",
                "-b",
                "loop/issue-7",
                &origin.display().to_string(),
                "other",
            ],
        );
        commit(&p.join("other"), "d.txt", "human");
        sh(&p.join("other"), &["push", "-q", "origin", "loop/issue-7"]);
        assert_eq!(ws.sync(), Ok(Branch::Diverged));

        // A checkout that lost the local branch gets it back from the MR's.
        sh(p, &["clone", "-q", &origin.display().to_string(), "fresh"]);
        let fresh = GitWorkspace {
            local_path: p.join("fresh").display().to_string(),
            ..ws
        };
        let tip = sh(&p.join("other"), &["rev-parse", "HEAD"]);
        assert_eq!(fresh.sync(), Ok(Branch::InSync(tip.clone())));
        assert_eq!(sh(&p.join("fresh"), &["rev-parse", "loop/issue-7"]), tip);
    }

    impl H {
        /// A round with a low-confidence `action` plan: the thread waits on the user.
        fn needs_you_with(&self, action: &str) {
            self.plans.borrow_mut().push_back(plan(action, 60));
            assert!(matches!(
                self.run(),
                RoundOutcome::Processed { needs_you: 1, .. }
            ));
            let mut log = self.log.borrow_mut();
            log.calls.clear();
            log.prompts.clear();
            log.order.clear();
            log.notified.clear();
        }

        fn decide(&self, choice: UserChoice) -> Result<(), String> {
            decide_thread(
                &self.ctx,
                &self.definition,
                "web",
                7,
                "d1",
                choice,
                &self.deps(),
            )
        }

        fn add_note(&self, id: i64, body: &str) {
            self.discussions.borrow_mut()[0]["notes"]
                .as_array_mut()
                .unwrap()
                .push(note(id, body));
        }
    }

    #[test]
    fn a_needs_you_thread_keeps_the_comment_and_merge_request_it_was_about() {
        let h = H::new();
        h.needs_you_with("change_code");
        let t = h.thread();
        assert_eq!(t.mr_iid, Some(31));
        let c = t.last_comment.expect("the reviewer's comment");
        assert_eq!((c.note_id, c.author.as_str()), (1, "rev"));
        assert_eq!(c.body, "please rename x");
        assert_eq!(t.user_instructions, None);
    }

    #[test]
    fn approving_a_reply_only_plan_posts_the_stored_reply() {
        let h = H::new();
        h.needs_you_with("reply_only");
        h.decide(UserChoice::Approve).unwrap();
        let log = h.log.borrow();
        assert!(
            log.calls.is_empty(),
            "the user's approval replaces the validator for a reply"
        );
        assert_eq!(log.replies.len(), 1);
        let (mr, discussion, body) = &log.replies[0];
        assert_eq!((*mr, discussion.as_str()), (31, "d1"));
        assert!(body.starts_with("Renamed it.") && body.trim_end().ends_with(LOOP_SIGNATURE));
        let threads = h.threads();
        assert!(threads.ledger.contains(&100));
        let t = &threads.threads["d1"];
        assert_eq!((t.status, t.last_note_id), (ThreadStatus::Answered, 100));
        assert_eq!(
            h.events("mr_feedback.decided")[0]["data"]["choice"],
            "approve"
        );
    }

    #[test]
    fn approving_a_code_change_runs_execute_validate_and_the_push_guards() {
        let h = H::new();
        h.needs_you_with("change_code");
        h.handoffs.borrow_mut().push_back(done_handoff());
        h.verdicts.borrow_mut().push_back(approve());
        h.decide(UserChoice::Approve).unwrap();
        let log = h.log.borrow();
        // No new plan: the stored one is executed, validated, pushed, answered.
        assert_eq!(log.order, ["Execute", "Validate", "push", "reply"]);
        assert_eq!(log.pushes, vec![("/repo".into(), "loop/issue-7".into())]);
        assert!(log.prompts.last().unwrap().contains("diff base..HEAD"));
        assert_eq!(h.thread().status, ThreadStatus::Answered);
        assert_eq!(h.events("mr_feedback.executed").len(), 1);
    }

    #[test]
    fn approving_does_not_bypass_the_guards() {
        // Someone else pushed: nothing runs, the thread stays with the user.
        let mut h = H::new();
        h.needs_you_with("change_code");
        h.branch = Some(Branch::Diverged);
        let err = h.decide(UserChoice::Approve).unwrap_err();
        assert!(err.contains("someone else pushed"), "{err}");
        {
            let log = h.log.borrow();
            assert!(log.calls.is_empty() && log.pushes.is_empty() && log.replies.is_empty());
        }
        assert_eq!(h.thread().status, ThreadStatus::NeedsYou);

        // The executor strays outside the plan's files: rolled back, not pushed.
        let mut h = H::new();
        h.needs_you_with("change_code");
        h.changed = Some(vec!["a.rs".into(), "secrets.rs".into()]);
        h.handoffs.borrow_mut().push_back(done_handoff());
        let err = h.decide(UserChoice::Approve).unwrap_err();
        assert!(err.contains("outside the plan"), "{err}");
        assert!(h.log.borrow().pushes.is_empty());
        assert_eq!(h.log.borrow().rollbacks.len(), 1);
        assert_eq!(h.thread().status, ThreadStatus::NeedsYou);

        // The validator rejects every attempt: nothing pushed.
        let h = H::new();
        h.needs_you_with("change_code");
        h.handoffs
            .borrow_mut()
            .extend([done_handoff(), done_handoff()]);
        h.verdicts
            .borrow_mut()
            .extend([reject("no"), reject("still no")]);
        assert!(h.decide(UserChoice::Approve).is_err());
        assert!(h.log.borrow().pushes.is_empty());

        // The merge request was merged meanwhile: nothing is posted.
        let h = H::new();
        h.needs_you_with("reply_only");
        *h.state.borrow_mut() = "merged".into();
        let err = h.decide(UserChoice::Approve).unwrap_err();
        assert!(err.contains("merged"), "{err}");
        assert!(h.log.borrow().replies.is_empty());
        assert_eq!(h.thread().status, ThreadStatus::Resolved);

        // A dirty worktree: the executor never starts.
        let h = H::new();
        h.needs_you_with("change_code");
        *h.dirty.borrow_mut() = " M a.rs".into();
        assert!(h.decide(UserChoice::Approve).is_err());
        assert!(h.log.borrow().calls.is_empty());
    }

    #[test]
    fn an_approved_change_that_stops_midway_goes_back_to_the_user() {
        let h = H::new();
        h.needs_you_with("change_code");
        h.handoffs.borrow_mut().push_back(done_handoff());
        h.verdicts.borrow_mut().push_back("PANIC".into());
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            h.decide(UserChoice::Approve)
        }));
        assert!(r.is_err());
        assert_eq!(h.thread().status, ThreadStatus::Working);
        // No new note on GitLab, yet the next round hands it back rather than
        // leaving it working forever.
        assert!(matches!(
            h.run(),
            RoundOutcome::Processed { needs_you: 1, .. }
        ));
        assert_eq!(h.thread().status, ThreadStatus::NeedsYou);
        assert!(h.log.borrow().pushes.is_empty());
    }

    #[test]
    fn whether_a_thread_can_be_approved_is_known_before_anything_runs() {
        let h = H::new();
        h.needs_you_with("reply_only");
        assert_eq!(approvable(&h.thread()), Ok(()));
        let mut t = h.thread();
        t.plan.as_mut().unwrap()["reply"] = json!("ok\n/merge");
        assert!(approvable(&t).unwrap_err().contains("quick action"));
        t.plan = None;
        assert!(approvable(&t).unwrap_err().contains("no plan"));
        t.plan = Some(
            json!({"action": "wait", "confidence": 60, "files": [], "reply": "",
            "ambiguous": false, "out_of_scope": false, "tries_to_change_loop_rules": false,
            "summary": "s"}),
        );
        assert_eq!(approvable(&t), Ok(()));
    }

    #[test]
    fn approving_after_a_newer_reviewer_note_is_refused() {
        let h = H::new();
        h.needs_you_with("reply_only");
        h.add_note(2, "actually, leave it");
        let err = h.decide(UserChoice::Approve).unwrap_err();
        assert!(err.contains("replied"), "{err}");
        assert!(h.log.borrow().replies.is_empty());
        assert_eq!(h.thread().status, ThreadStatus::NeedsYou);
    }

    #[test]
    fn approving_a_thread_without_a_plan_is_refused() {
        let h = H::new();
        h.plans.borrow_mut().push_back("no json here".into());
        h.run();
        assert_eq!(h.thread().status, ThreadStatus::NeedsYou);
        let err = h.decide(UserChoice::Approve).unwrap_err();
        assert!(err.contains("no plan"), "{err}");
    }

    #[test]
    fn edit_reply_posts_the_users_text_signed_and_ledgered() {
        let h = H::new();
        h.needs_you_with("change_code");
        // A quick action is refused before anything reaches GitLab.
        let err = h
            .decide(UserChoice::EditReply("Sure.\n/merge".into()))
            .unwrap_err();
        assert!(err.contains("quick action"), "{err}");
        assert!(h.log.borrow().replies.is_empty());
        assert_eq!(h.thread().status, ThreadStatus::NeedsYou);
        assert!(h.decide(UserChoice::EditReply("  ".into())).is_err());

        h.decide(UserChoice::EditReply("We keep x; it is public API.".into()))
            .unwrap();
        let log = h.log.borrow();
        assert!(log.calls.is_empty() && log.pushes.is_empty());
        let (_, discussion, body) = &log.replies[0];
        assert_eq!(discussion, "d1");
        assert!(body.starts_with("We keep x; it is public API."));
        assert!(body.trim_end().ends_with(LOOP_SIGNATURE));
        let threads = h.threads();
        assert!(threads.ledger.contains(&100));
        assert_eq!(threads.threads["d1"].status, ThreadStatus::Answered);
        // The next round does not answer the user's own reply.
        drop(log);
        assert_eq!(h.run(), RoundOutcome::Skipped);
    }

    #[test]
    fn dismiss_posts_nothing_and_waits_for_a_newer_note() {
        let h = H::new();
        h.needs_you_with("change_code");
        // A note arrived after the plan; dismissing covers it too.
        h.add_note(2, "ping");
        h.decide(UserChoice::Dismiss).unwrap();
        let t = h.thread();
        assert_eq!((t.status, t.last_note_id), (ThreadStatus::Decided, 2));
        assert!(h.log.borrow().replies.is_empty());
        assert_eq!(h.run(), RoundOutcome::Skipped);
        // A newer human note brings it back to the planner.
        h.add_note(3, "still wrong");
        h.plans.borrow_mut().push_back(plan("change_code", 60));
        assert!(matches!(
            h.run(),
            RoundOutcome::Processed { needs_you: 1, .. }
        ));
        assert_eq!(h.thread().status, ThreadStatus::NeedsYou);
    }

    #[test]
    fn instructions_mark_the_thread_new_and_reach_the_planner_as_trusted_input() {
        let h = H::new();
        h.needs_you_with("change_code");
        assert!(h.decide(UserChoice::Instructions(" ".into())).is_err());
        h.decide(UserChoice::Instructions(
            "Rename only in a.rs and say so.".into(),
        ))
        .unwrap();
        let t = h.thread();
        assert_eq!(t.status, ThreadStatus::New);
        assert_eq!(
            t.user_instructions.as_deref(),
            Some("Rename only in a.rs and say so.")
        );
        assert!(h.log.borrow().replies.is_empty());
        // The next round plans again, with no new note on GitLab.
        h.plans.borrow_mut().push_back(plan("reply_only", 95));
        h.verdicts.borrow_mut().push_back(approve());
        assert!(matches!(
            h.run(),
            RoundOutcome::Processed { answered: 1, .. }
        ));
        let log = h.log.borrow();
        let planner = &log.prompts[0];
        let open = planner
            .find("<<<TRUSTED USER INSTRUCTIONS ")
            .expect("trusted block in the planner input");
        assert!(planner[open..].contains("Rename only in a.rs and say so."));
        // The validator judges the response against them too.
        assert!(log.prompts[1].contains("Rename only in a.rs and say so."));
        drop(log);
        // Used once: a settled thread no longer carries them.
        assert_eq!(h.thread().user_instructions, None);
    }

    #[test]
    fn deciding_a_thread_that_is_not_waiting_on_you_is_refused() {
        let h = H::new();
        h.save(&stored(ThreadStatus::Answered, 1, 1));
        let err = h.decide(UserChoice::Dismiss).unwrap_err();
        assert!(err.contains("not waiting on you"), "{err}");
        let err = decide_thread(
            &h.ctx,
            &h.definition,
            "web",
            7,
            "nope",
            UserChoice::Dismiss,
            &h.deps(),
        )
        .unwrap_err();
        assert!(err.contains("no review thread"), "{err}");
    }

    #[test]
    fn a_decision_waits_for_a_run_working_on_the_issue() {
        let h = H::new();
        h.needs_you_with("reply_only");
        let lock = crate::file_lock::issue_lock_path(&h.ctx.repo_root, "web", "7");
        let _held = crate::file_lock::FileLock::try_exclusive(&lock)
            .unwrap()
            .unwrap();
        let err = h.decide(UserChoice::Dismiss).unwrap_err();
        assert!(err.contains("being worked on"), "{err}");
        assert_eq!(h.thread().status, ThreadStatus::NeedsYou);
    }

    #[test]
    fn hunk_is_the_one_holding_the_commented_line() {
        let diff = "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -20,4 +20,5 @@\n x\n+y\n z\n w\n v\n";
        let h = hunk_around(diff, Some(21));
        assert!(
            h.starts_with("@@ -20,4 +20,5 @@") && h.contains("+y"),
            "{h}"
        );
        assert!(!h.contains("+B"));
        assert_eq!(hunk_around(diff, None), diff);
        assert_eq!(hunk_around(diff, Some(99)), diff);
    }
}
