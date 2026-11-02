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
use crate::mr_threads::{IssueThreads, ThreadState, ThreadStatus, LOOP_SIGNATURE};
use crate::runner_util::tail;
use crate::{cost, events, prompt};
use serde_json::{json, Value};
use std::cell::Cell;
use std::path::{Path, PathBuf};

/// How much of the branch diff the validator sees.
const DIFF_MAX_CHARS: usize = 20_000;
/// How much of the checks' output the validator (or a retry) sees.
const CHECKS_TAIL_CHARS: usize = 4_000;

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

/// The issue's git worktree and branch.
pub trait Workspace {
    /// Fetches the remote `loop/issue-<iid>` and says whether it has commits
    /// the local branch lacks (someone else pushed). Recreates a missing
    /// local branch from the remote one.
    fn diverged(&self) -> Result<bool, String>;
    /// Creates or reuses the issue's worktree; returns its path.
    fn ensure(&self) -> Result<PathBuf, String>;
    fn head(&self, worktree: &Path) -> Result<String, String>;
    /// `git diff <base> HEAD`, bounded.
    fn diff(&self, worktree: &Path, base: &str) -> Result<String, String>;
    /// Moves the branch back to `base`, keeping the discarded commits under
    /// `refs/loopx/mr-feedback/<keep_as>`.
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
    };
    match round.run() {
        Ok(outcome) => outcome,
        Err(e) => {
            round.log(&format!("skipped: {e}"));
            RoundOutcome::Skipped
        }
    }
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
        // Marked working before any agent runs, with `last_note_id` left as is,
        // so a crash leaves the thread for one more try.
        let attempts = match &prev {
            Some(p) if p.status == ThreadStatus::Working => p.attempts.saturating_add(1),
            _ => 1,
        };
        let t = state
            .threads
            .entry(id.to_string())
            .or_insert_with(|| fresh(id));
        t.status = ThreadStatus::Working;
        t.attempts = attempts;
        t.updated_at = now();
        self.save(state);

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
            Decision::Execute(plan) if plan.reply.trim().is_empty() => self.needs_you(
                state,
                id,
                Some(&plan),
                "the plan has no reply to post",
                latest,
            ),
            Decision::Execute(plan) => match plan.action {
                PlanAction::ReplyOnly => self.reply_only(state, mr, a, &plan, latest),
                PlanAction::ChangeCode => self.change_code(state, mr, a, &plan, latest),
                PlanAction::Wait => {
                    self.needs_you(state, id, Some(&plan), "unexpected wait plan", latest)
                }
            },
        }
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
    fn validate(&self, a: &Actionable, plan: &Plan, change: Option<(&str, &str)>) -> Verdict {
        let nonce = events::new_uuid_hex();
        let mut input = mr_plan::build_plan_input_with_nonce(a, None, None, &nonce);
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
    ) -> ThreadEnd {
        let id = a.discussion_id.as_str();
        match self.validate(a, plan, None) {
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
        let body = format!("{}\n\n{LOOP_SIGNATURE}", plan.reply.trim());
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
    ) -> String {
        let nonce = events::new_uuid_hex();
        let mut input = mr_plan::build_plan_input_with_nonce(a, None, None, &nonce);
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
        self.rollback(worktree, base, id);
        self.needs_you(state, id, Some(plan), reason, latest)
    }

    fn rollback(&self, worktree: &Path, base: &str, id: &str) {
        let keep_as: String = id.chars().filter(char::is_ascii_alphanumeric).collect();
        if let Err(e) = self.deps.workspace.rollback(worktree, base, &keep_as) {
            self.log(&format!("rolling back {} failed: {e}", worktree.display()));
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
        if !crate::config::use_worktree(&self.deps.project) || local_path.is_empty() {
            return self.needs_you(
                state,
                id,
                Some(plan),
                "the project has no worktree for this issue (use_worktree is off), so the loop does not change its code for review feedback",
                latest,
            );
        }
        match ws.diverged() {
            Ok(false) => {}
            Ok(true) => {
                return self.needs_you(
                    state,
                    id,
                    Some(plan),
                    &format!("someone else pushed to {branch}; the loop does not build on commits it has not seen"),
                    latest,
                )
            }
            Err(e) => {
                return self.needs_you(
                    state,
                    id,
                    Some(plan),
                    &format!("could not compare {branch} with its remote: {e}"),
                    latest,
                )
            }
        }
        let worktree = match ws.ensure() {
            Ok(w) => w,
            Err(e) => {
                return self.needs_you(
                    state,
                    id,
                    Some(plan),
                    &format!("could not prepare the worktree: {e}"),
                    latest,
                )
            }
        };
        let base = match ws.head(&worktree) {
            Ok(h) => h,
            Err(e) => {
                return self.needs_you(
                    state,
                    id,
                    Some(plan),
                    &format!("could not read the worktree's HEAD: {e}"),
                    latest,
                )
            }
        };
        let handoff = handoff_path(
            &self.ctx.repo_root,
            self.run_id,
            self.alias,
            self.issue_iid,
            id,
        );
        let attempts = self.definition.stop_conditions.max_iterations.max(1);
        let mut feedback: Option<String> = None;
        let mut approved = false;
        let abandon = |state: &mut IssueThreads, reason: &str| {
            self.abandon(state, &worktree, &base, id, plan, reason, latest)
        };
        for _ in 0..attempts {
            let _ = std::fs::remove_file(&handoff);
            if let Some(dir) = handoff.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let prompt = self.execute_prompt(a, plan, &worktree, &handoff, feedback.as_deref());
            if let Err(e) = self.call(Phase::Execute, &prompt) {
                return abandon(state, &format!("the executor failed: {e}"));
            }
            if let Err(reason) = read_execute_handoff(&handoff) {
                return abandon(state, &reason);
            }
            match ws.head(&worktree) {
                Ok(h) if h != base => {}
                Ok(_) => return abandon(state, "the executor committed nothing"),
                Err(e) => return abandon(state, &format!("could not read HEAD: {e}")),
            }
            let checks = (self.deps.verify)(&handoff);
            let checks_out = checks_text(&checks);
            if !checks.passed {
                feedback = Some(format!("The project's checks failed:\n{checks_out}"));
                continue;
            }
            let diff = match ws.diff(&worktree, &base) {
                Ok(d) => d,
                Err(e) => return abandon(state, &format!("could not read the diff: {e}")),
            };
            match self.validate(a, plan, Some((&diff, &checks_out))) {
                Verdict::Approve => {
                    approved = true;
                    break;
                }
                Verdict::Reject(r) => feedback = Some(format!("The validator rejected it: {r}")),
                Verdict::Invalid(e) => {
                    return abandon(state, &format!("the validator's output was unusable: {e}"))
                }
            }
        }
        if !approved {
            return abandon(
                state,
                &format!(
                    "the change was not accepted after {attempts} attempt(s): {}",
                    feedback.as_deref().unwrap_or("")
                ),
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
            json!({"discussion_id": id, "branch": branch, "files": plan.files}),
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

impl Workspace for GitWorkspace {
    fn diverged(&self) -> Result<bool, String> {
        let branch = crate::worktree::branch_name(&self.issue_iid);
        let remote = format!("refs/remotes/origin/{branch}");
        let local = format!("refs/heads/{branch}");
        git_ok(
            &self.local_path,
            &["fetch", "origin", &format!("+refs/heads/{branch}:{remote}")],
        )?;
        if git(
            &self.local_path,
            &["show-ref", "--verify", "--quiet", &local],
        )?
        .0 != Some(0)
        {
            // The branch was swept with its worktree: rebuild it from the MR's.
            git_ok(&self.local_path, &["branch", &branch, &remote])?;
            return Ok(false);
        }
        match git(
            &self.local_path,
            &["merge-base", "--is-ancestor", &remote, &local],
        )? {
            (Some(0), _) => Ok(false),
            (Some(1), _) => Ok(true),
            (_, out) => Err(format!("git merge-base: {}", out.trim())),
        }
    }

    fn ensure(&self) -> Result<PathBuf, String> {
        crate::worktree::new_worktree(
            &self.local_path,
            &self.target_branch,
            &self.issue_iid,
            &self.worktree_root,
        )
        .map(PathBuf::from)
        .map_err(|f| f.message)
    }

    fn head(&self, worktree: &Path) -> Result<String, String> {
        git_ok(&worktree.display().to_string(), &["rev-parse", "HEAD"])
    }

    fn diff(&self, worktree: &Path, base: &str) -> Result<String, String> {
        let diff = git_ok(&worktree.display().to_string(), &["diff", base, "HEAD"])?;
        if diff.chars().count() <= DIFF_MAX_CHARS {
            return Ok(diff);
        }
        let cut: String = diff.chars().take(DIFF_MAX_CHARS).collect();
        Ok(format!(
            "{cut}\n[diff truncated at {DIFF_MAX_CHARS} characters]"
        ))
    }

    fn rollback(&self, worktree: &Path, base: &str, keep_as: &str) -> Result<(), String> {
        let dir = worktree.display().to_string();
        let head = self.head(worktree)?;
        if head != base {
            git_ok(
                &dir,
                &[
                    "update-ref",
                    &format!("refs/loopx/mr-feedback/{keep_as}"),
                    &head,
                ],
            )?;
        }
        git_ok(&dir, &["reset", "--hard", base]).map(|_| ())
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
        diverged: bool,
        head: Shared<String>,
    }

    impl Workspace for FakeWorkspace {
        fn diverged(&self) -> Result<bool, String> {
            Ok(self.diverged)
        }
        fn ensure(&self) -> Result<PathBuf, String> {
            self.log.borrow_mut().ensured += 1;
            Ok(PathBuf::from("/wt/repo-issue-7"))
        }
        fn head(&self, _: &Path) -> Result<String, String> {
            Ok(self.head.borrow().clone())
        }
        fn diff(&self, _: &Path, base: &str) -> Result<String, String> {
            Ok(format!("diff {base}..HEAD\n-old\n+new"))
        }
        fn rollback(&self, _: &Path, base: &str, keep_as: &str) -> Result<(), String> {
            self.log
                .borrow_mut()
                .rollbacks
                .push((base.into(), keep_as.into()));
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
        diverged: bool,
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
                diverged: false,
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
            let commits = Cell::new(0);
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
                let text = match call.phase {
                    Phase::Plan => pop(&plans),
                    Phase::Validate => pop(&verdicts),
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
                    diverged: self.diverged,
                    head: self.head.clone(),
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
        h.diverged = true;
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
    fn code_change_on_a_project_without_worktree_goes_to_needs_you() {
        let mut h = H::new();
        h.use_worktree = false;
        h.plans.borrow_mut().push_back(plan("change_code", 92));
        let out = h.run();
        assert!(matches!(out, RoundOutcome::Processed { needs_you: 1, .. }));
        assert_eq!(h.log.borrow().ensured, 0);
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
        commit(&repo, "b.txt", "loop fix");
        sh(&repo, &["push", "-q", "origin", "loop/issue-7"]);
        sh(&repo, &["checkout", "-q", "main"]);
        let ws = GitWorkspace {
            local_path: repo.display().to_string(),
            target_branch: "main".into(),
            issue_iid: "7".into(),
            worktree_root: p.join("wt").display().to_string(),
        };
        assert_eq!(ws.diverged(), Ok(false));
        let wt = ws.ensure().unwrap();
        assert!(
            wt.join("b.txt").exists(),
            "the worktree is on the MR branch"
        );
        let base = ws.head(&wt).unwrap();
        let fix = commit(&wt, "c.txt", "review fix");
        assert!(ws.diff(&wt, &base).unwrap().contains("+review fix"));
        // A rejected change leaves the branch but is kept under a ref.
        ws.rollback(&wt, &base, "d1").unwrap();
        assert_eq!(ws.head(&wt).unwrap(), base);
        assert_eq!(sh(&repo, &["rev-parse", "refs/loopx/mr-feedback/d1"]), fix);
        assert_eq!(ws.diverged(), Ok(false));

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
        assert_eq!(ws.diverged(), Ok(true));

        // A checkout that lost the local branch gets it back from the MR's.
        sh(p, &["clone", "-q", &origin.display().to_string(), "fresh"]);
        let fresh = GitWorkspace {
            local_path: p.join("fresh").display().to_string(),
            ..ws
        };
        assert_eq!(fresh.diverged(), Ok(false));
        assert_eq!(
            sh(&p.join("fresh"), &["rev-parse", "loop/issue-7"]),
            sh(&p.join("other"), &["rev-parse", "HEAD"])
        );
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
