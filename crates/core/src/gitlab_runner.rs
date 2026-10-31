//! The real GitLab issue loop wired to [`LoopRuntime`]. Ports
//! `bin/gitlab_loop_runner.py`: build each issue's prompt, run the agent CLI
//! under the runtime's budget/verification, and (in gate mode) open the merge
//! request only after the loop's own checks pass, otherwise escalate.
//!
//! Three agent-invocation shapes share one subprocess boundary
//! ([`CliInvoker`]): the dashboard's on-demand single issue, one issue inside the
//! scheduled batch, and the batch's single end-of-run wrap-up.

use crate::agents::{extract_new_cli_reply, new_cli_args, RunOptions};
use crate::config::{default_config_path, Config};
use crate::events::EventFields;
use crate::gitlab::{self, quote, Client};
use crate::loop_definition::LoopDefinition;
use crate::loop_result::{AgentCost, IterationResult, LoopResult};
use crate::loop_runtime::{AgentFailure, AgentFn, AgentSuccess, LoopRuntime};
use crate::loop_serialize::write_result;
use crate::loop_state::LoopState;
use crate::loop_verifiers::{build_verifiers, IssueContext};
use crate::runner_util::{
    append_unified_log, could_not_start, emit_best_effort, failed_call_cost, head,
    notify_slack_best_effort, parse_envelope, resolved_program, tail, unified_log_path,
    STDERR_EXCERPT_CHARS,
};
use crate::{
    ai_cli_config, approvals, cost, events, gitlab_report, issue_tracking_config, learning,
    memory_store, mr, mr_screenshots, proc, prompt, setup_doctor, slack_report, worktree,
};
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use setup_doctor::{DoctorResult, DoctorStatus};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const DEFINITION_RELATIVE_PATH: &str = "loops/gitlab-issue/loop.yaml";
pub const NEEDS_HUMAN_LABEL: &str = "loop:needs-human";
const HANDOFF_ACTIONS: [&str; 5] = [
    "fix",
    "answer",
    "escalate",
    "wait_for_review",
    "setup_blocked",
];
const FAILURE_TAIL_CHARS: usize = 1500;
const OPENER_TIMEOUT_SECONDS: u64 = 300;
const FEEDBACK_MAX_CHARS: usize = 6000;
const FEEDBACK_PER_RESULT_CHARS: usize = 2500;
const FEEDBACK_HEADER: &str = "## Previous attempt failed external verification\n\nThe loop re-ran this project's own checks in your worktree after your last attempt. Fix the cause, re-run the same commands yourself, and only then finish.\n\n";
const USAGE_TOKEN_FIELDS: [&str; 4] = [
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
];

/// `loopx` subcommands the agent may run. `mr` (opening the MR) is withheld in
/// gate mode, where the loop itself opens it after verification.
const AGENT_SUBCOMMANDS: [&str; 12] = [
    "config",
    "events",
    "notify",
    "gitlab",
    "issues",
    "memory",
    "memory-store",
    "risk",
    "status",
    "messages",
    "worktree",
    "screenshot",
];
/// The helpers an offline (golden eval) run keeps.
const OFFLINE_SUBCOMMANDS: [&str; 5] = ["config", "events", "risk", "memory-store", "memory get"];
/// What an offline run must never reach: GitLab, Slack, the live dashboard.
const OFFLINE_DENIED_SUBCOMMANDS: [&str; 5] = ["gitlab", "issues", "notify", "status", "messages"];

pub(crate) const DISALLOWED_TOOLS: &str = "Bash(git merge*) Bash(git push --force*) Bash(git push -f*) Bash(git checkout*) Bash(git reset*) Bash(git clean*) Read(**/.env*) Read(**/*.key) Read(**/id_rsa*)";

const GATE_OVERRIDE: &str = "## Harness gate is ON for this run \u{2014} this overrides steps 9 and 10\n\nDo NOT run `loopx mr open` and do NOT push: skip step 9. The loop re-runs the project's checks itself and opens the merge request only if they pass.\n\nFor a fix, once it is committed on loop/issue-<iid> and your own checks pass, do step 10 with these changes: annotate loop_last_action as \"fix_handed_off\" (not \"mr_opened: ...\"), still run `loopx issues comments mark-seen`, still record a learning if there is one (`loopx memory-store add` and its memory.created emit), and still cd back to the loop directory. Do NOT send the \"Finished\" Slack message and do NOT emit issue.completed \u{2014} the loop does both after it opens the merge request. Then write this JSON to <handoff_path> (also in $LOOP_HANDOFF_PATH) and stop:\n\n{\"action\": \"fix\", \"branch\": \"loop/issue-<iid>\", \"target_branch\": \"<target>\", \"title\": \"<fix|feat|refactor|test|chore|docs>: #<iid> <what was fixed or added, from the user's point of view, not the implementation's>\", \"summary\": \"<the comment the loop posts on the issue under the merge request link, as markdown, written for the person who filed it: first the line '**Goal:** <the outcome the issue asked for, now delivered, in one sentence from the user\u{2019}s view>', then 2-4 '- ' bullets of key results and metrics (what a user can now do, edge cases now handled, numbers that matter such as records affected or tests added). Outcomes only: no commands, file, class or method names, branches or commits, and no description of the steps you took \u{2014} all of that goes in the description; the loop appends the checks it ran as the last bullet>\", \"description\": \"<the merge request description, as markdown, following the create-mr template: '## What and why' starting with the line 'Closes #<iid>', then bullets '- Problem: <what was broken or missing, from the user's view>', '- Fix: <what changed to solve it>', '- Result: <how you confirmed it is actually fixed>'; '## How' with one bullet per non-obvious decision (skip what the diff makes obvious); '## Verified' with checked boxes for the original problem confirmed fixed, the tests and lint you ran (by command), and anything you checked by hand; and '## Related' only if the issue thread mentions another MR, a commit or a prior attempt, one line each. Bullets, not prose, readable in 30 seconds; never list every file you touched>\", \"verification\": {\"install\": \"<install command you used, or empty>\", \"lint\": \"<lint command, or empty if the repo has no linter>\", \"test\": \"<test command - required>\", \"cwd\": \"<absolute directory you ran them in: the worktree, or the checkout when worktree is off>\", \"stack\": \"<e.g. rails, node, python>\", \"summary\": \"<1-3 sentences: what you detected, which commands you chose and why, any setup you fixed>\"}, \"screenshots\": [{\"label\": \"<what this pair shows, e.g. the status select on the order form>\", \"before\": \"1-before.png\", \"after\": \"1-after.png\"}], \"screenshots_note\": \"<optional: one line on why a shot is missing>\"}\n\n`screenshots` and `screenshots_note` are only for a visible UI change (see \"UI changes: before/after screenshots\"); omit both otherwise. Take the \"after\" shots last, right before writing this file. List at most 6 pairs of PNG files of up to 5 MB each, named by file name inside outputs/screens/<alias>-<iid>/ in the loop directory; leave out `before` when there is none. The loop puts them in the merge request description and then deletes every image.\n\nThe loop re-runs exactly that lint and test command in that directory before it opens the merge request; a command outside the allowed stack commands, or a missing test command, fails verification.\n\nEvery issue in a gated run must end with a handoff file at that path, written after finishing the matching path as usual:\n- answered directly: {\"action\": \"answer\"}\n- escalated (needs clarification, or your own verification failed): {\"action\": \"escalate\"}\n- Wait for reviewer, or nothing to act on this run: {\"action\": \"wait_for_review\"}\n- blocked by setup, not by your change (step 5's `loopx worktree new` failed for anything but a merge conflict, a step 7 check failed on a missing toolchain, dependency, submodule, built asset, database or credential, or step 7.1 found SETUP.md's Worktree prep steps you may not run and that are not done in this worktree): first commit your work in progress with only the files you changed (`git commit -m \"WIP #<iid>: <title>\"`, never `git add -A`/`.`) so `git status` is clean, then {\"action\": \"setup_blocked\", \"command\": \"<the exact command that failed>\", \"cwd\": \"<absolute directory you ran it in>\", \"error\": \"<the relevant error output, trimmed>\", \"diagnosis\": \"<one or two sentences: why this is setup, not your change>\"}; `command` and `error` are both required and non-empty (when no check ran, `command` is the check you would have run and `error` describes the missing prep) \u{2014} no GitLab comment, no Slack message; the loop runs a setup doctor and re-runs you if it fixes setup\nA missing or malformed handoff is escalated to a human.";

/// Asset-build commands the agent may run before taking a screenshot.
pub(crate) const ASSET_BUILD_PATTERNS: [&str; 4] = [
    "Bash(yarn build*)",
    "Bash(npm run build*)",
    "Bash(pnpm build*)",
    "Bash(RAILS_ENV=test bin/rails assets:precompile*)",
];

fn tool_patterns(exe: &str, subs: &[&str]) -> Vec<String> {
    subs.iter()
        .flat_map(|s| [format!("Bash(loopx {s}*)"), format!("Bash({exe} {s}*)")])
        .collect()
}

/// The enforced Bash/file permission list for the agent. `gate` withholds
/// opening the MR and pushing (the loop does both after verification);
/// `offline` swaps GitLab/Slack/dashboard helpers for the offline helper set.
pub fn allowed_tools(repo_root: &str, exe: &str, gate: bool, offline: bool) -> String {
    let mut t: Vec<String> = ["Read", "Edit", "Write"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    t.extend(
        [
            "Bash(git status*)",
            "Bash(git diff*)",
            "Bash(git add*)",
            "Bash(git commit*)",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    if !gate {
        t.push("Bash(git push origin loop/issue-*)".into());
    }
    t.push("Bash(cd *)".into());
    if offline {
        // The golden fixtures are Python repos: their test_cmd must be runnable.
        t.push("Bash(python3 -m pytest*)".into());
        t.extend(tool_patterns(exe, &OFFLINE_SUBCOMMANDS));
    } else {
        t.extend(crate::verify_commands::claude_patterns());
        t.push("Bash(RAILS_ENV=test bundle exec rake db:test:prepare*)".into());
        // Asset builds a screenshot may need; the verify allowlist stays closed to them.
        t.extend(ASSET_BUILD_PATTERNS.iter().map(|s| s.to_string()));
        t.extend(tool_patterns(exe, &AGENT_SUBCOMMANDS));
        if !gate {
            t.extend(tool_patterns(exe, &["mr"]));
        }
    }
    if gate {
        t.push(format!("Write({repo_root}/outputs/handoffs/**)"));
    }
    let _ = repo_root;
    t.join(" ")
}

pub fn disallowed_tools(exe: &str, gate: bool, offline: bool) -> String {
    let mut parts = vec![DISALLOWED_TOOLS.to_string()];
    if gate {
        parts.push(format!(
            "Bash(loopx mr open*) Bash({exe} mr open*) Bash(git push origin loop/issue-*)"
        ));
    }
    if offline {
        parts.extend(tool_patterns(exe, &OFFLINE_DENIED_SUBCOMMANDS));
    }
    parts.join(" ")
}

/// The full agent CLI argv.
#[allow(clippy::too_many_arguments)]
pub fn cli_command(
    ai_cli: &str,
    prompt: &str,
    repo_root: &str,
    worktree_root: &str,
    exe: &str,
    gate: bool,
    max_budget_usd: Option<f64>,
    offline: bool,
    extra_dirs: &[String],
) -> Vec<String> {
    let s = |v: &str| v.to_string();
    let mut roots = vec![s(repo_root), s(worktree_root)];
    roots.extend(extra_dirs.iter().cloned());
    if matches!(ai_cli, "gemini" | "kimi" | "cursor") {
        // No tool allowlist exists for these; cursor's workspace is the repo root.
        let mut opts = RunOptions::new(Path::new(repo_root), 0.0);
        opts.add_dirs = roots.iter().map(PathBuf::from).collect();
        let args = new_cli_args(ai_cli, prompt, &opts).expect("a new CLI");
        return std::iter::once(resolved_program(ai_cli))
            .chain(args)
            .collect();
    }
    if ai_cli == "codex" {
        // `codex exec` has no --add-dir/--ask-for-approval; the two roots become
        // a writable_roots override, and network access is needed for git push
        // and GitLab calls.
        let roots = serde_json::to_string(&roots).expect("serialisable");
        return vec![
            s("codex"),
            s("exec"),
            s("--sandbox"),
            s("workspace-write"),
            s("-c"),
            s("approval_policy=never"),
            s("-c"),
            format!("sandbox_workspace_write.writable_roots={roots}"),
            s("-c"),
            s("sandbox_workspace_write.network_access=true"),
            s(prompt),
        ];
    }
    let mut cmd = vec![s("claude"), s("-p")];
    for root in &roots {
        cmd.extend([s("--add-dir"), root.clone()]);
    }
    cmd.extend([
        s("--permission-mode"),
        s("acceptEdits"),
        s("--allowedTools"),
        allowed_tools(repo_root, exe, gate, offline),
        s("--disallowedTools"),
        disallowed_tools(exe, gate, offline),
    ]);
    cmd.extend(cost::budget_args(max_budget_usd));
    cmd.extend([s("--output-format"), s("json"), s(prompt)]);
    cmd
}

// ---- run context ------------------------------------------------------------

/// Where a run reads and writes; every path is injectable for tests.
#[derive(Debug, Clone)]
pub struct RunCtx {
    pub repo_root: PathBuf,
    pub events_dir: Option<PathBuf>,
    pub results_dir: Option<PathBuf>,
    pub unified_log: PathBuf,
    /// The `loopx` binary the agent is told it may run.
    pub exe: String,
    /// projects.json to read (default: the per-machine one).
    pub config_path: Option<PathBuf>,
    /// Tests: GitLab calls and Slack messages made by the gated finalize go
    /// here instead of to the real services.
    pub fakes: Option<Arc<RunFakes>>,
    /// Where task memory and `SETUP.md` live (default: the per-machine one).
    pub memory_root: Option<PathBuf>,
    /// What a running claude call last did (see `agent_activity`).
    pub activity_file: PathBuf,
    /// A claude call with no hook activity for this long is stopped.
    pub stall_after: std::time::Duration,
}

pub struct RunFakes {
    pub gitlab: Box<dyn GitlabCalls + Send + Sync>,
    pub notifier: Box<dyn Fn(&str) + Send + Sync>,
    pub gitlab_config_path: PathBuf,
}

impl std::fmt::Debug for RunFakes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RunFakes")
    }
}

impl RunCtx {
    pub fn new(repo_root: PathBuf) -> Self {
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "loopx".to_string());
        let unified_log = unified_log_path(&repo_root);
        let activity_file = crate::agent_activity::path(Some(&repo_root));
        Self {
            repo_root,
            events_dir: None,
            results_dir: None,
            unified_log,
            exe,
            config_path: None,
            fakes: None,
            memory_root: None,
            activity_file,
            stall_after: crate::agent_activity::STALL_AFTER,
        }
    }

    fn log(&self, text: &str) {
        append_unified_log(text, &self.unified_log);
    }

    fn emit(&self, event_type: &str, run_id: Option<&str>, fields: EventFields) {
        emit_best_effort(event_type, run_id, fields, self.events_dir.as_deref());
    }

    pub fn definition_path(&self) -> PathBuf {
        self.repo_root.join(DEFINITION_RELATIVE_PATH)
    }
}

pub fn handoff_path(run_id: &str, alias: &str, issue_iid: &str, repo_root: &Path) -> PathBuf {
    repo_root
        .join("outputs")
        .join("handoffs")
        .join(run_id)
        .join(format!("{alias}-{issue_iid}.json"))
}

fn gate_prompt_and_env(
    prompt: &str,
    alias: &str,
    issue_iid: &str,
    repo_root: &Path,
    run_id: Option<&str>,
) -> Result<(String, Vec<(String, String)>), String> {
    let run_id = run_id
        .map(str::to_owned)
        .or_else(|| std::env::var("LOOP_RUN_ID").ok().filter(|v| !v.is_empty()))
        .ok_or("gate mode needs a run_id (argument or LOOP_RUN_ID) to locate the handoff file")?;
    let path = handoff_path(&run_id, alias, issue_iid, repo_root);
    // The path is stated literally: the agent's Write tool needs one.
    let override_text = GATE_OVERRIDE.replace("<handoff_path>", &path.display().to_string());
    Ok((
        format!("{prompt}\n\n{override_text}"),
        vec![("LOOP_HANDOFF_PATH".into(), path.display().to_string())],
    ))
}

// ---- invoking the agent -----------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum InvokeMode {
    /// The dashboard's on-demand run: does its own full "End of run".
    Single,
    /// One issue inside the scheduled batch: no "End of run".
    Batch,
    /// The golden eval's offline mode: issue text from a file.
    IssueFile(PathBuf),
}

#[derive(Debug, Clone)]
pub struct InvokeRequest {
    pub alias: String,
    pub issue_iid: String,
    pub mode: InvokeMode,
    pub feedback: Option<String>,
    pub gate: bool,
    pub run_id: Option<String>,
    pub max_budget_usd: Option<f64>,
    pub timeout_seconds: f64,
    /// `Some` runs the setup doctor for this issue instead of the main agent.
    pub doctor: Option<DoctorRequest>,
}

/// What the setup doctor session is told about the blocked issue.
#[derive(Debug, Clone)]
pub struct DoctorRequest {
    /// The issue's worktree, the one place it may edit files.
    pub work_dir: String,
    /// The main agent's `setup_blocked` handoff.
    pub blocked: Value,
    pub max_attempts: i64,
    /// `SETUP.md` sources that changed since it was written.
    pub stale: Vec<String>,
    /// Whether the project already has a `SETUP.md`.
    pub has_knowledge: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentOutput {
    pub cost_usd: Option<f64>,
    pub usage: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentError {
    pub message: String,
    pub cost_usd: Option<f64>,
}

/// The one agent boundary. Tests substitute their own.
pub type Invoker = Arc<dyn Fn(&InvokeRequest) -> Result<AgentOutput, AgentError> + Send + Sync>;

fn with_feedback(prompt: String, feedback: Option<&str>) -> String {
    match feedback.filter(|f| !f.is_empty()) {
        Some(f) => format!("{prompt}\n\n{f}"),
        None => prompt,
    }
}

/// The real invoker: builds the prompt and runs `claude -p` / `codex exec`.
pub fn cli_invoker(ctx: RunCtx) -> Invoker {
    Arc::new(move |req| invoke_request(&ctx, req))
}

fn invoke_request(ctx: &RunCtx, req: &InvokeRequest) -> Result<AgentOutput, AgentError> {
    let fail = |m: String| AgentError {
        message: m,
        cost_usd: None,
    };
    if let Some(doctor) = &req.doctor {
        return invoke_doctor(ctx, req, doctor);
    }
    let base = match &req.mode {
        InvokeMode::Single => prompt::single_issue(&ctx.repo_root, &req.alias, &req.issue_iid),
        InvokeMode::Batch => prompt::batch_issue(&ctx.repo_root, &req.alias, &req.issue_iid),
        InvokeMode::IssueFile(file) => {
            let worktree = offline_worktree(&req.alias, &req.issue_iid)?;
            prompt::issue_file(&ctx.repo_root, &req.alias, &req.issue_iid, file, &worktree)
                .map_err(fail)?
        }
    };
    let mut text = with_feedback(base, req.feedback.as_deref());
    let offline = matches!(req.mode, InvokeMode::IssueFile(_));
    // The golden eval always runs in gate mode.
    let gate = req.gate || offline;
    let mut env = Vec::new();
    if matches!(req.mode, InvokeMode::Batch | InvokeMode::Single) {
        env.push(hold_slack());
    }
    if gate {
        let (t, e) = gate_prompt_and_env(
            &text,
            &req.alias,
            &req.issue_iid,
            &ctx.repo_root,
            req.run_id.as_deref(),
        )
        .map_err(fail)?;
        text = t;
        env.extend(e);
    }
    invoke_cli_with_prompt(
        ctx,
        &text,
        req.timeout_seconds,
        Some((&req.alias, &req.issue_iid)),
        &env,
        gate,
        req.max_budget_usd,
        offline,
    )
}

/// The setup doctor session for a `setup_blocked` issue: its own prompt and
/// its own, narrower-scoped permissions.
fn invoke_doctor(
    ctx: &RunCtx,
    req: &InvokeRequest,
    doctor: &DoctorRequest,
) -> Result<AgentOutput, AgentError> {
    let run_id = req
        .run_id
        .clone()
        .or_else(|| std::env::var("LOOP_RUN_ID").ok().filter(|v| !v.is_empty()))
        .ok_or_else(|| AgentError {
            message: "the setup doctor needs a run_id to locate its result file".into(),
            cost_usd: None,
        })?;
    let handoff = handoff_path(&run_id, &req.alias, &req.issue_iid, &ctx.repo_root);
    let text = prompt::setup_doctor(
        &ctx.repo_root,
        &req.alias,
        &req.issue_iid,
        &doctor.work_dir,
        &doctor.blocked,
        &setup_doctor::doctor_result_path(&handoff),
        &setup_doctor::setup_draft_path(&handoff),
        doctor.max_attempts,
        &doctor.stale,
        doctor.has_knowledge,
    );
    let text = prompt::with_run_id(text, Some(&run_id));
    let ai_cli = ai_cli_config::selected_cli();
    let argv = setup_doctor::doctor_cli_command(
        &ai_cli,
        &text,
        &ctx.repo_root.display().to_string(),
        &doctor.work_dir,
        &handoff,
        &ctx.exe,
        req.max_budget_usd,
    );
    run_cli(
        ctx,
        &ai_cli,
        &argv,
        req.timeout_seconds,
        Some((&req.alias, &req.issue_iid)),
        &[hold_slack()],
        req.max_budget_usd,
    )
}

/// The wrap-up call for a scheduled batch ("End of run" only).
pub fn invoke_batch_end_of_run(
    ctx: &RunCtx,
    timeout_seconds: f64,
) -> Result<AgentOutput, AgentError> {
    let text = prompt::batch_end_of_run(&ctx.repo_root);
    invoke_cli_with_prompt(
        ctx,
        &text,
        timeout_seconds,
        None,
        &[hold_slack()],
        false,
        None,
        false,
    )
}

/// The agent's own `loopx notify` calls in a batch are held, not posted: the
/// run sends one report at the end instead (`loopx notify` reads this).
fn hold_slack() -> (String, String) {
    (crate::slack_report::HOLD_ENV.to_string(), "1".to_string())
}

/// The checkout of `alias` when it works in place (`use_worktree: false`):
/// the agent edits it directly, so it must be a writable dir for that issue.
fn in_place_dirs(config_path: &Path, alias: &str) -> Vec<String> {
    Config::load(config_path)
        .and_then(|c| c.project(alias))
        .ok()
        .filter(|p| !crate::config::use_worktree(p))
        .and_then(|p| {
            p.get("local_path")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .into_iter()
        .collect()
}

/// The issue worktree the golden fixture already created, from the sandbox
/// config (offline runs may not call `loopx worktree new`).
fn offline_worktree(alias: &str, issue_iid: &str) -> Result<String, AgentError> {
    let fail = |message: String| AgentError {
        message,
        cost_usd: None,
    };
    let config = Config::load(&default_config_path()).map_err(|e| fail(e.to_string()))?;
    let project = config.project(alias).map_err(|e| fail(e.to_string()))?;
    let local_path = project
        .get("local_path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let root = config.worktree_root().map_err(|e| fail(e.to_string()))?;
    Ok(crate::worktree::worktree_path(local_path, issue_iid, &root))
}

fn worktree_root() -> Result<String, AgentError> {
    Config::load(&default_config_path())
        .and_then(|c| c.worktree_root())
        .map_err(|e| AgentError {
            message: e.to_string(),
            cost_usd: None,
        })
}

#[allow(clippy::too_many_arguments)]
fn invoke_cli_with_prompt(
    ctx: &RunCtx,
    prompt_text: &str,
    timeout_seconds: f64,
    issue: Option<(&str, &str)>,
    env: &[(String, String)],
    gate: bool,
    max_budget_usd: Option<f64>,
    offline: bool,
) -> Result<AgentOutput, AgentError> {
    let worktree_root = worktree_root()?;
    let ai_cli = ai_cli_config::selected_cli();
    let repo_root = ctx.repo_root.display().to_string();
    let run_id = std::env::var("LOOP_RUN_ID").ok();
    let prompt_text = &prompt::with_run_id(prompt_text.to_string(), run_id.as_deref());
    let extra_dirs = issue
        .map(|(alias, _)| in_place_dirs(&project_config_path(ctx), alias))
        .unwrap_or_default();
    let argv = cli_command(
        &ai_cli,
        prompt_text,
        &repo_root,
        &worktree_root,
        &ctx.exe,
        gate,
        max_budget_usd,
        offline,
        &extra_dirs,
    );
    run_cli(
        ctx,
        &ai_cli,
        &argv,
        timeout_seconds,
        issue,
        env,
        max_budget_usd,
    )
}

/// Spawn `argv` (the selected CLI's command) and turn its outcome into the
/// agent's result.
fn run_cli(
    ctx: &RunCtx,
    ai_cli: &str,
    argv: &[String],
    timeout_seconds: f64,
    issue: Option<(&str, &str)>,
    env: &[(String, String)],
    max_budget_usd: Option<f64>,
) -> Result<AgentOutput, AgentError> {
    let report_failure = |reason: &str, detail: &str, event_reason: Option<&str>| {
        let label = issue
            .map(|(a, i)| format!(" for {a} #{i}"))
            .unwrap_or_default();
        ctx.log(&format!(
            "{ai_cli} invocation{label} FAILED ({reason}):\n{detail}"
        ));
        let run_id = std::env::var("LOOP_RUN_ID").ok().filter(|v| !v.is_empty());
        let issue_run_id = match (&run_id, issue) {
            (Some(r), Some((a, i))) => Some(format!("{r}_{a}_{i}")),
            _ => None,
        };
        ctx.emit(
            "issue.agent_failed",
            run_id.as_deref(),
            EventFields {
                issue_run_id,
                project: issue.map(|(a, _)| a.to_string()),
                issue_iid: issue.and_then(|(_, i)| i.parse().ok()),
                data: Some(json!({
                    "reason": event_reason.unwrap_or(reason),
                    "cli": ai_cli,
                    "stderr_excerpt": tail(detail, STDERR_EXCERPT_CHARS),
                })),
            },
        );
    };

    let mut command = std::process::Command::new(&argv[0]);
    // Claude reports each tool call through hooks, so the dashboard can say what
    // it is doing and a wedged call is stopped long before its timeout.
    // Issues run in parallel: each one's hooks write its own file, so agents
    // never overwrite each other's record or mask each other's stall.
    let activity_file = match issue {
        Some((a, i)) => crate::agent_activity::for_task(&ctx.activity_file, a, i),
        None => ctx.activity_file.clone(),
    };
    let stall = (ai_cli == "claude").then(|| {
        let label = issue.map_or_else(|| "end of run".to_string(), |(a, i)| format!("{a} #{i}"));
        let settings = crate::agent_activity::hook_settings(&ctx.exe, &activity_file, &label);
        if let Err(e) =
            crate::agent_activity::start(&activity_file, &label, &crate::agent_activity::now_iso())
        {
            ctx.log(&format!("cannot write {}: {e}", activity_file.display()));
        }
        command
            .args(&argv[1..2])
            .args(["--settings", &settings])
            .args(&argv[2..]);
        proc::Stall {
            file: activity_file.clone(),
            after: ctx.stall_after,
        }
    });
    if stall.is_none() {
        command.args(&argv[1..]);
    }
    for (k, v) in env {
        command.env(k, v);
    }
    let mut path = std::env::var("PATH").unwrap_or_default();
    // `node`/`npm`/... resolve to the Node each project needs, not whichever
    // is first on PATH (see `node_env`).
    let shims = ctx.repo_root.join("outputs/node-shims");
    match crate::node_env::write_shims(&shims, Path::new(&ctx.exe)) {
        Ok(()) => path = format!("{}:{path}", shims.display()),
        Err(e) => ctx.log(&format!(
            "cannot write Node shims to {}: {e}",
            shims.display()
        )),
    }
    // Make `loopx` resolvable for the agent whatever PATH the scheduler gave us.
    if let Some(dir) = Path::new(&ctx.exe)
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
    {
        path = format!("{}:{path}", dir.display());
    }
    command.env("PATH", path);
    let timeout = std::time::Duration::from_secs_f64(timeout_seconds.max(0.0));
    let watched = proc::run_watched(&mut command, None, Some(timeout), stall.as_ref());
    if stall.is_some() && issue.is_some() {
        crate::agent_activity::finish(&activity_file);
    }
    let (out, stalled) = match watched {
        Ok(out) => out,
        Err(e) => {
            let detail = could_not_start(ai_cli, &argv[0], &e);
            report_failure("failed to start", &detail, None);
            return Err(AgentError {
                message: detail,
                cost_usd: None,
            });
        }
    };

    if out.timed_out || !out.success() {
        let reason = if stalled {
            format!(
                "stalled: no agent activity for {}s",
                ctx.stall_after.as_secs_f64().round() as i64
            )
        } else if out.timed_out {
            format!("timed out after {}s", timeout_seconds as i64)
        } else {
            format!("exited {}", out.code.unwrap_or(-1))
        };
        let detail = if out.stderr.is_empty() {
            out.stdout.clone()
        } else {
            out.stderr.clone()
        };
        let envelope = if ai_cli == "claude" {
            parse_envelope(&out.stdout)
        } else {
            None
        };
        // What the failed call spent (the CLI's JSON error envelope still
        // carries total_cost_usd), so the run's remaining budget stays honest.
        let budget = envelope
            .as_ref()
            .and_then(|e| e.get("subtype"))
            .and_then(Value::as_str)
            == Some("error_max_budget_usd");
        let spent = failed_call_cost(envelope.as_ref(), budget, max_budget_usd);
        report_failure(&reason, &detail, budget.then_some("budget_exceeded"));
        return Err(AgentError {
            message: reason,
            cost_usd: spent,
        });
    }

    let (result_text, cost_usd, usage) = if ai_cli == "claude" {
        let parsed = parse_envelope(&out.stdout);
        if let Some(p) = parsed
            .as_ref()
            .filter(|p| p.get("is_error").is_some_and(crate::loop_serialize::truthy))
        {
            let budget = p.get("subtype").and_then(Value::as_str) == Some("error_max_budget_usd");
            report_failure(
                "error envelope on exit 0",
                &out.stdout,
                budget.then_some("budget_exceeded"),
            );
            return Err(AgentError {
                message: "claude reported an error".into(),
                cost_usd: failed_call_cost(Some(p), budget, max_budget_usd),
            });
        }
        let text = match &parsed {
            Some(p) => cost::extract_result_text(p),
            None => "(no result text in CLI output)".to_string(),
        };
        let usage = parsed.as_ref().and_then(cost::extract_claude_usage);
        let cost_usd = usage
            .as_ref()
            .and_then(|u| u.get("cost_usd"))
            .and_then(Value::as_f64);
        (text, cost_usd, usage)
    } else if let Some(reply) = extract_new_cli_reply(ai_cli, &out.stdout) {
        match reply {
            Ok(text) => (text, None, None),
            Err(message) => {
                report_failure("unusable output on exit 0", &message, None);
                return Err(AgentError {
                    message,
                    cost_usd: None,
                });
            }
        }
    } else {
        (out.stdout.clone(), None, None)
    };

    println!("{result_text}");
    ctx.log(&result_text);
    // The CLI's stderr (warnings, partial errors on an otherwise-zero exit)
    // would otherwise vanish, since the run captures it.
    if !out.stderr.is_empty() {
        ctx.log(&format!("{ai_cli} stderr:\n{}", out.stderr));
    }
    Ok(AgentOutput { cost_usd, usage })
}

// ---- verification feedback --------------------------------------------------

fn failed_commands(evidence: &Value) -> Option<Vec<(String, String, bool)>> {
    let commands = evidence.get("commands")?.as_array()?;
    if commands.is_empty() {
        return None;
    }
    Some(
        commands
            .iter()
            .map(|c| {
                (
                    c.get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    c.get("output")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    c.get("passed").is_some_and(crate::loop_serialize::truthy),
                )
            })
            .collect(),
    )
}

/// Failure feedback for the next attempt: each failed verifier's output tail
/// (its output already carries `$ <command>` headers), bounded.
pub fn format_feedback(previous: &IterationResult) -> String {
    let mut parts: Vec<String> = Vec::new();
    for r in previous.verification_results.iter().filter(|r| !r.passed) {
        match failed_commands(&r.evidence) {
            Some(cmds) => parts.extend(
                cmds.into_iter()
                    .filter(|(_, _, passed)| !passed)
                    .map(|(c, out, _)| format!("$ {c}\n{}", tail(&out, FEEDBACK_PER_RESULT_CHARS))),
            ),
            None => parts.push(tail(&r.output, FEEDBACK_PER_RESULT_CHARS)),
        }
    }
    head(
        &format!("{FEEDBACK_HEADER}{}", parts.join("\n\n")),
        FEEDBACK_MAX_CHARS,
    )
}

pub fn iteration_failed_verification(iteration: &IterationResult) -> bool {
    iteration.verification_results.iter().any(|r| !r.passed)
}

/// `verification.external_skipped` for a vacuous pass (no worktree / no
/// commands, or an iteration that never reached verification) and
/// `external_completed` otherwise.
fn emit_verification_events(
    ctx: &RunCtx,
    run_id: &str,
    issue_run_id: &str,
    alias: &str,
    issue_iid: &str,
    mode: &str,
    iteration: &IterationResult,
) {
    let fields = |data: Option<Value>| EventFields {
        issue_run_id: Some(issue_run_id.to_string()),
        project: Some(alias.to_string()),
        issue_iid: issue_iid.parse().ok(),
        data,
    };
    if iteration.verification_results.is_empty() {
        ctx.emit("verification.external_skipped", Some(run_id), fields(None));
    }
    for result in &iteration.verification_results {
        let ev = &result.evidence;
        let vacuous = ev.get("vacuous").is_some_and(crate::loop_serialize::truthy)
            || ev
                .get("commands")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty);
        if vacuous {
            ctx.emit("verification.external_skipped", Some(run_id), fields(None));
            continue;
        }
        let mut data = json!({
            "verifier": result.name,
            "passed": result.passed,
            "observed_passed": ev.get("observed_passed").cloned().unwrap_or(json!(result.passed)),
            "mode": mode,
            "iteration": iteration.iteration,
        });
        if ev.get("error").is_some_and(crate::loop_serialize::truthy) {
            // A verifier config error says nothing about the agent's fix.
            data["error"] = json!(true);
        }
        ctx.emit(
            "verification.external_completed",
            Some(run_id),
            fields(Some(data)),
        );
    }
}

// ---- gate finalize ----------------------------------------------------------

/// The agent's handoff JSON, or `None` when missing/malformed. A `fix` must
/// name exactly this issue's `loop/issue-<iid>` branch and carry a non-empty
/// `target_branch` and `title`; a `setup_blocked` a non-empty `command` and `error`.
pub fn read_handoff(path: &Path, issue_iid: &str) -> Option<Value> {
    let data: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let action = data.get("action")?.as_str()?;
    if !HANDOFF_ACTIONS.contains(&action) {
        return None;
    }
    if action == "fix" {
        let branch = data.get("branch")?.as_str()?;
        let digits = branch.strip_prefix("loop/issue-")?;
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        if branch != format!("loop/issue-{issue_iid}") {
            return None;
        }
        for key in ["target_branch", "title"] {
            if data.get(key)?.as_str()?.trim().is_empty() {
                return None;
            }
        }
    }
    if action == "setup_blocked" {
        for key in ["command", "error"] {
            if data.get(key)?.as_str()?.trim().is_empty() {
                return None;
            }
        }
    }
    Some(data)
}

fn agent_outcome(action: &str) -> Option<&'static str> {
    match action {
        "answer" => Some("answered"),
        "wait_for_review" => Some("waiting_for_review"),
        "escalate" => Some("escalated:agent"),
        _ => None,
    }
}

fn last_failed_iteration(result: &LoopResult) -> Option<&IterationResult> {
    // A retry that crashed or hit the budget has no verification results of its
    // own, so the failure worth reporting is the attempt before it.
    for it in result.iterations.iter().rev() {
        if !it.verification_results.is_empty() {
            return iteration_failed_verification(it).then_some(it);
        }
    }
    None
}

fn failure_text(result: &LoopResult) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(it) = last_failed_iteration(result) {
        for r in it.verification_results.iter().filter(|r| !r.passed) {
            match failed_commands(&r.evidence) {
                Some(cmds) => parts.extend(
                    cmds.into_iter()
                        .filter(|(_, _, passed)| !passed)
                        .map(|(c, out, _)| format!("$ {c}\n{out}")),
                ),
                None => parts.push(r.output.clone()),
            }
        }
    }
    tail(&parts.join("\n\n"), FAILURE_TAIL_CHARS)
}

/// The issue as Slack mrkdwn: `<url|alias #iid>`, or bold text with no URL.
pub fn slack_issue_link(alias: &str, issue_iid: &str, project_url: Option<&str>) -> String {
    match project_url {
        Some(url) => format!("<{url}/-/issues/{issue_iid}|{alias} #{issue_iid}>"),
        None => format!("*{alias} #{issue_iid}*"),
    }
}

/// One plain sentence on why the loop (or the agent) escalated.
pub(crate) fn escalation_reason_text(reason: &str, stop_reason: Option<&str>) -> String {
    match reason {
        "run_incomplete" => stop_reason
            .and_then(stop_reason_text)
            .unwrap_or("The run stopped before the agent finished.")
            .to_string(),
        "verification_failed" => {
            "The loop's own checks of the fix failed, so no merge request was opened.".into()
        }
        "handoff_invalid" => {
            "The agent left no valid handoff, so the loop could not tell what to do next.".into()
        }
        "mr_open_failed" => {
            "The fix passed the loop's checks, but the merge request could not be opened.".into()
        }
        "checkout_problem" => {
            "The project's own checkout (worktree off) is in a state the loop will not touch."
                .into()
        }
        "project_config_error" => "The project's config could not be read.".into(),
        "needs_clarification" => {
            "The issue needs clarification - the agent asked on the issue.".into()
        }
        "worktree_creation_failed" => "A worktree for the fix could not be created.".into(),
        "setup_blocked" => {
            "Setup problems blocked the project's checks, so no merge request was opened - the details are on the issue.".into()
        }
        "approval_denied" => "Opening the merge request was not approved.".into(),
        "approval_changes_requested" => {
            "The reviewer still asked for changes after the loop's revisions, so no merge request was opened.".into()
        }
        "approval_timeout" => {
            "Nobody approved opening the merge request before the request expired.".into()
        }
        "approval_unavailable" => {
            "The approval request for the merge request could not be created.".into()
        }
        other => format!("The loop stopped this issue ({}).", other.replace('_', " ")),
    }
}

/// The one Slack message for an issue the loop escalated.
pub fn escalation_slack(issue_link: &str, reason: &str, stop_reason: Option<&str>) -> String {
    format!(
        "*Escalated* {issue_link}\n{}\nNeeds a human - the details are on the issue.",
        escalation_reason_text(reason, stop_reason)
    )
}

fn stop_reason_text(stop_reason: &str) -> Option<&'static str> {
    match stop_reason {
        "agent_failed" => {
            Some("The agent session failed or timed out (details in the loop's log).")
        }
        "budget_exceeded" => Some("The run hit its iteration, time or cost budget."),
        _ => None,
    }
}

pub fn escalation_comment(
    issue_iid: &str,
    reason: &str,
    failure_text: &str,
    stop_reason: Option<&str>,
) -> String {
    let footer = format!(
        "**Where the work is**\n- Branch `loop/issue-{issue_iid}` in this issue's local worktree (not pushed)."
    );
    if reason == "setup_blocked" {
        // The setup report is already the whole comment body.
        return format!("{}\n\n{footer}", failure_text.trim_end());
    }
    let fenced = if failure_text.is_empty() {
        String::new()
    } else {
        format!("\n\n```\n{failure_text}\n```")
    };
    let (tried, failed) = match reason {
        "handoff_invalid" => (
            "I worked on this issue but did not leave a valid handoff, so the loop could not tell what to do next.".to_string(),
            "The handoff file was missing or malformed; no merge request was opened.".to_string(),
        ),
        "run_incomplete" => (
            "I started on this issue, but the run stopped before I finished, so no merge request was opened.".to_string(),
            match stop_reason {
                Some(s) => stop_reason_text(s)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("The run stopped early ({s}).")),
                None => "The run stopped early (None).".to_string(),
            },
        ),
        "approval_denied" | "approval_timeout" | "approval_unavailable"
        | "approval_changes_requested" => (
            "I fixed this and the loop's own checks passed, but opening the merge request needs a human's approval first.".to_string(),
            match reason {
                "approval_denied" => "The approval was denied, so no merge request was opened.".to_string(),
                "approval_changes_requested" => format!(
                    "The reviewer asked for changes again after {MAX_REVISIONS} revisions, so no merge request was opened. Their last notes:{}",
                    if fenced.is_empty() { "\n(No notes.)" } else { &fenced }
                ),
                "approval_timeout" => "Nobody approved it before the approval request expired, so no merge request was opened.".to_string(),
                _ => format!(
                    "The approval request could not be created:{}",
                    if fenced.is_empty() { "\n(No output.)" } else { &fenced }
                ),
            },
        ),
        "checkout_problem" => (
            "This project is worked on in its own checkout (worktree off), and the loop found that checkout in a state it will not touch, so no merge request was opened.".to_string(),
            format!("What the loop found:{fenced}"),
        ),
        "mr_open_failed" => (
            "I fixed this and the loop's own checks passed, but the merge request could not be opened.".to_string(),
            format!(
                "Opening the merge request failed:{}",
                if fenced.is_empty() { "\n(No output.)" } else { &fenced }
            ),
        ),
        _ => {
            let how = if stop_reason == Some("no_progress") {
                "failed the same way on consecutive attempts"
            } else {
                "failed"
            };
            let mut failed = format!(
                "The project's checks, re-run by the loop in my worktree, {how}:{}",
                if fenced.is_empty() {
                    "\n(The checks produced no output.)"
                } else {
                    &fenced
                }
            );
            if let Some(text) = stop_reason.and_then(stop_reason_text) {
                failed.push_str(&format!("\n\nThe retry did not finish: {text}"));
            }
            (
                "I implemented a fix, but the loop's own verification of it failed, so no merge request was opened.".to_string(),
                failed,
            )
        }
    };
    format!(
        "**What I tried**\n{tried}\n\n**What failed**\n{failed}\n\n{footer}\n\n**What I need from you**\n- Look at the failure above and the unpushed branch, then fix it or tell me how to proceed."
    )
}

/// `(bundle, issue_web_url)` for this alias from `~/.gitlab/config.json`: the
/// same `bundle` the agent's own `loopx notify` calls use. Never fails.
pub fn project_slack_target(
    alias: &str,
    project: &Value,
    gitlab_config_path: Option<&Path>,
) -> (Option<String>, Option<String>) {
    let config: Value = match gitlab_config_path {
        Some(p) => std::fs::read_to_string(p)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok()),
        None => gitlab::load_config(),
    }
    .filter(Value::is_object)
    .unwrap_or_else(|| json!({}));
    let bundle = config
        .pointer(&format!("/projects/{}/bundle", escape_pointer(alias)))
        .and_then(Value::as_str)
        .filter(|b| !b.is_empty())
        .map(str::to_owned);
    let base = project
        .get("instance")
        .and_then(Value::as_str)
        .and_then(|i| config.pointer(&format!("/instances/{}/url", escape_pointer(i))))
        .and_then(Value::as_str);
    let project_id = project.get("project_id").and_then(Value::as_str);
    let url = match (base, project_id) {
        (Some(b), Some(id))
            if (b.starts_with("http://") || b.starts_with("https://")) && id.contains('/') =>
        {
            Some(format!("{}/{id}", b.trim_end_matches('/')))
        }
        _ => None,
    };
    (bundle, url)
}

fn escape_pointer(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// GitLab write calls made while finalizing (injectable for tests).
pub trait GitlabCalls {
    fn call(&self, method: &str, endpoint: &str, body: &Value) -> Result<(), String>;

    /// Upload an image to the project; returns its project-relative URL.
    fn upload(&self, _project_id: &str, _path: &Path) -> Result<String, String> {
        Err("upload unsupported".into())
    }
}

struct RealGitlab {
    instance: String,
}

impl GitlabCalls for RealGitlab {
    fn call(&self, method: &str, endpoint: &str, body: &Value) -> Result<(), String> {
        let config = gitlab::load_config();
        let client = Client::from_config(config.as_ref(), Some(&self.instance), None)
            .map_err(|e| e.to_string())?;
        match method {
            "POST" => client.post_json(endpoint, body),
            "PUT" => client.put_json(endpoint, body),
            other => return Err(format!("unsupported method {other}")),
        }
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    fn upload(&self, project_id: &str, path: &Path) -> Result<String, String> {
        let config = gitlab::load_config();
        let client = Client::from_config(config.as_ref(), Some(&self.instance), None)
            .map_err(|e| e.to_string())?;
        let resp = client
            .upload_file(project_id, path)
            .map_err(|e| e.to_string())?;
        resp.get("url")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "upload response has no url".to_string())
    }
}

/// Deletes a run's screenshot artefacts when finalizing ends, on any path.
struct ScreenshotCleanup {
    data: Option<Value>,
    handoff_dir: PathBuf,
    screens_dir: PathBuf,
}

impl Drop for ScreenshotCleanup {
    fn drop(&mut self) {
        mr_screenshots::remove_screenshots(
            self.data.as_ref(),
            &self.handoff_dir,
            &self.screens_dir,
        );
    }
}

/// `(ok, detail)` of opening the merge request for `branch`.
pub type Opener<'a> = dyn Fn(&str, &str, &str, &str) -> (bool, String) + 'a;

fn real_opener(local_path: &str, branch: &str, target: &str, title: &str) -> (bool, String) {
    if let Err(msg) = mr::check_branch(branch) {
        return (false, msg);
    }
    let argv = mr::push_command(local_path, branch, target, title);
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    match proc::run(
        &mut cmd,
        None,
        Some(std::time::Duration::from_secs(OPENER_TIMEOUT_SECONDS)),
    ) {
        Ok(out) if out.timed_out => (false, "TimeoutExpired: push timed out".into()),
        Ok(out) => (
            out.success(),
            format!("{}{}", out.stdout, out.stderr).trim().to_string(),
        ),
        Err(e) => (false, format!("OSError: {e}")),
    }
}

/// Dependencies of [`finalize_gated_issue`]; `None` uses the real thing.
#[derive(Default)]
pub struct FinalizeDeps<'a> {
    pub opener: Option<&'a Opener<'a>>,
    pub gitlab: Option<&'a dyn GitlabCalls>,
    pub notifier: Option<&'a dyn Fn(&str)>,
    pub handoff: Option<PathBuf>,
    pub project: Option<Value>,
    pub gitlab_config_path: Option<PathBuf>,
    /// Overrides `require_approval` from projects.json.
    pub require_approval: Option<bool>,
    /// Where approvals live (default `outputs/approvals`).
    pub approval_dir: Option<PathBuf>,
    /// How often to look for a decision (default 1s).
    pub approval_poll: Option<std::time::Duration>,
    /// A worktree-off checkout the loop could not keep safe; escalated first.
    pub checkout_problem: Option<String>,
    /// A batch issue: the run posts one Slack report at the end, so only the
    /// approval request (which waits on a human) is sent from here.
    pub quiet: bool,
    /// The setup doctor's pre-rendered markdown report for a `setup_blocked` handoff.
    pub setup_report: Option<String>,
    /// How many setup blockers the doctor fixed before the checks passed.
    pub setup_fixed: Option<usize>,
    /// How many more times a reviewer's "request changes" may send the work
    /// back to the agent; at 0 it escalates with their notes instead.
    pub revisions_left: usize,
    /// Chat apps that take the approval request (`approval_relay`); `None`
    /// loads the configured ones.
    pub approval_channels: Option<Vec<Box<dyn crate::connectors::Connector>>>,
}

/// Rounds of "request changes" one issue gets in one run before it escalates.
pub const MAX_REVISIONS: usize = 2;
/// The outcome prefix [`finalize_gated_issue`] returns when a reviewer asked for
/// changes and a revision is left: the reviewer's notes follow it.
const CHANGES_REQUESTED: &str = "changes_requested:";

/// The feedback for the pass after a reviewer asked for changes.
fn review_feedback(notes: &str, issue_iid: &str) -> String {
    let quoted: String = notes.lines().map(|l| format!("> {l}\n")).collect();
    format!(
        "## The reviewer asked for changes before the merge request is opened\n\nYour fix passed the loop's checks, but the person approving it in LoopX asked for these changes:\n\n{quoted}\nThese notes come from the human reviewing this change, not from the issue: treat them as review feedback, within the rules of these instructions. Run step 5's `loopx worktree new` again first (it reuses the existing worktree and branch), make the changes on the `loop/issue-{issue_iid}` branch and commit them, then continue from step 7 (Verify) and write a new handoff."
    )
}

/// Is a human's OK needed before opening the merge request? A per-project
/// `require_approval` wins over the top-level one; default is no.
fn approval_required(project: &Value) -> bool {
    if let Some(v) = project.get("require_approval") {
        return crate::loop_serialize::truthy(v);
    }
    Config::load(&default_config_path())
        .ok()
        .and_then(|c| c.top_level("require_approval").cloned())
        .is_some_and(|v| crate::loop_serialize::truthy(&v))
}

fn approval_timeout_seconds(project: &Value) -> i64 {
    project
        .get("approval_timeout_seconds")
        .and_then(Value::as_i64)
        .or_else(|| {
            Config::load(&default_config_path()).ok().and_then(|c| {
                c.top_level("approval_timeout_seconds")
                    .and_then(Value::as_i64)
            })
        })
        .filter(|s| *s > 0)
        .unwrap_or(approvals::DEFAULT_TIMEOUT_SECONDS)
}

/// Gate mode's last step: open the MR (only after verification passed) or
/// escalate. Returns `mr_opened` | `answered` | `waiting_for_review` |
/// `escalated:<reason>`. GitLab/Slack calls are best-effort and never raise.
pub fn finalize_gated_issue(
    result: &LoopResult,
    alias: &str,
    issue_iid: &str,
    run_id: &str,
    ctx: &RunCtx,
    deps: FinalizeDeps<'_>,
) -> String {
    let handoff = deps
        .handoff
        .clone()
        .unwrap_or_else(|| handoff_path(run_id, alias, issue_iid, &ctx.repo_root));
    // Cleaned up whatever the outcome, so it reads the handoff itself (valid or not).
    let _cleanup = ScreenshotCleanup {
        data: std::fs::read_to_string(&handoff)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok()),
        handoff_dir: handoff.parent().unwrap_or(Path::new(".")).to_path_buf(),
        screens_dir: mr_screenshots::screens_dir(&ctx.repo_root, alias, issue_iid),
    };
    let issue_run_id = format!("{run_id}_{alias}_{issue_iid}");
    let iid_num: Option<i64> = issue_iid.parse().ok();
    let log = |text: &str| ctx.log(&format!("gate finalize {alias} #{issue_iid}: {text}"));
    let emit = |event_type: &str, data: Value| {
        ctx.emit(
            event_type,
            Some(run_id),
            EventFields {
                issue_run_id: Some(issue_run_id.clone()),
                project: Some(alias.to_string()),
                issue_iid: iid_num,
                data: Some(data),
            },
        )
    };

    let mut bundle: Option<String> = None;
    let mut issue_label = slack_issue_link(alias, issue_iid, None);
    let send = |bundle: &Option<String>, message: String| match deps.notifier {
        Some(n) => n(&message),
        None => {
            notify_slack_best_effort(&message, None, bundle.as_deref());
        }
    };
    let notify = |bundle: &Option<String>, message: String| {
        if !deps.quiet {
            send(bundle, message);
        }
    };

    let project = match deps.project.clone() {
        Some(p) => Ok(p),
        None => Config::load(&project_config_path(ctx))
            .and_then(|c| c.project(alias))
            .map_err(|e| e.to_string()),
    };
    let setup = project.and_then(|project| {
        let (b, url) = project_slack_target(alias, &project, deps.gitlab_config_path.as_deref());
        let id = project
            .get("project_id")
            .and_then(Value::as_str)
            .ok_or("KeyError: 'project_id'")?
            .to_string();
        let local_path = project
            .get("local_path")
            .and_then(Value::as_str)
            .ok_or("KeyError: 'local_path'")?
            .to_string();
        let instance = project
            .get("instance")
            .and_then(Value::as_str)
            .ok_or("KeyError: 'instance'")?
            .to_string();
        Ok((project, b, url, id, local_path, instance))
    });
    let (project, b, url, project_id, local_path, instance): (Value, _, _, String, String, String) =
        match setup {
            Ok(t) => t,
            Err(e) => {
                log(&format!("project config error: {e}"));
                emit(
                    "issue.escalated",
                    json!({"reason": "project_config_error", "gated": true}),
                );
                notify(
                    &bundle,
                    escalation_slack(&issue_label, "project_config_error", None),
                );
                return "escalated:project_config_error".to_string();
            }
        };
    bundle = b;
    if let Some(url) = url {
        issue_label = slack_issue_link(alias, issue_iid, Some(&url));
    }
    let notes_path = format!("projects/{}/issues/{issue_iid}", quote(&project_id));
    let real_gitlab = RealGitlab { instance };
    let gitlab_calls: &dyn GitlabCalls = deps.gitlab.unwrap_or(&real_gitlab);
    let api = |method: &str, endpoint: &str, body: Value| {
        if let Err(e) = gitlab_calls.call(method, endpoint, &body) {
            log(&format!("GitLab {method} {endpoint} failed: {e}"));
        }
    };

    let escalate = |reason: &str, failure_text: &str, stop_reason: Option<&str>| -> String {
        let comment = escalation_comment(issue_iid, reason, failure_text, stop_reason);
        api(
            "POST",
            &format!("{notes_path}/notes"),
            json!({"body": comment}),
        );
        api("PUT", &notes_path, json!({"add_labels": NEEDS_HUMAN_LABEL}));
        let mut data = json!({"reason": reason, "gated": true});
        if let Some(s) = stop_reason.filter(|s| !s.is_empty()) {
            data["stop_reason"] = json!(s);
        }
        emit("issue.escalated", data);
        notify(&bundle, escalation_slack(&issue_label, reason, stop_reason));
        format!("escalated:{reason}")
    };

    if let Some(problem) = &deps.checkout_problem {
        return escalate("checkout_problem", problem, None);
    }
    let data = read_handoff(&handoff, issue_iid);
    let screens = mr_screenshots::screens_dir(&ctx.repo_root, alias, issue_iid);
    let handoff_dir = handoff.parent().unwrap_or(Path::new(".")).to_path_buf();
    if let Some(outcome) = data
        .as_ref()
        .and_then(|d| d.get("action"))
        .and_then(Value::as_str)
        .and_then(agent_outcome)
    {
        return outcome.to_string();
    }
    if data
        .as_ref()
        .and_then(|d| d.get("action"))
        .and_then(Value::as_str)
        == Some("setup_blocked")
    {
        let report = deps.setup_report.clone().unwrap_or_else(|| {
            crate::setup_doctor::doctor_report_markdown(data.as_ref().unwrap_or(&json!({})), None)
        });
        return escalate("setup_blocked", &report, None);
    }
    let stop_reason = Some(result.stop_reason.as_str());
    // The handoff is cleared before every attempt, so after a crash or a
    // budget stop a missing handoff means "never got that far", not a bad one.
    if matches!(result.final_state, LoopState::Failed | LoopState::Stopped) {
        if last_failed_iteration(result).is_some() {
            return escalate("verification_failed", &failure_text(result), stop_reason);
        }
        return escalate("run_incomplete", "", stop_reason);
    }
    let Some(data) = data else {
        return escalate("handoff_invalid", "", None);
    };
    if result.final_state != LoopState::Completed {
        return escalate("verification_failed", &failure_text(result), stop_reason);
    }

    let branch = data["branch"].as_str().unwrap_or_default().to_string();
    let target = data["target_branch"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let title = data["title"].as_str().unwrap_or_default().to_string();

    // A human's OK before the merge request, when this project asks for one.
    // The checks have already passed; this only decides whether to push.
    if deps
        .require_approval
        .unwrap_or_else(|| approval_required(&project))
    {
        let dir = deps
            .approval_dir
            .clone()
            .unwrap_or_else(|| ctx.repo_root.join("outputs").join("approvals"));
        let request = approvals::NewApproval {
            kind: "open_mr".into(),
            run_id: run_id.to_string(),
            project: alias.to_string(),
            issue_iid: iid_num,
            title: title.clone(),
            branch: branch.clone(),
            target_branch: target.clone(),
            summary: data
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            timeout_seconds: approval_timeout_seconds(&project),
        };
        let approval = match approvals::create(&dir, &request, Utc::now()) {
            Ok(a) => a,
            Err(e) => {
                log(&format!("could not create an approval request: {e}"));
                return escalate("approval_unavailable", &e.to_string(), None);
            }
        };
        let approval_id = approval["id"].as_str().unwrap_or_default().to_string();
        emit(
            "approval.requested",
            json!({"approval_id": approval_id, "kind": "open_mr", "branch": branch, "expires_at": approval["expires_at"]}),
        );
        send(
            &bundle,
            format!(
                "*Approval needed* {issue_label}: open MR `{branch}` \u{2192} `{target}`? Approve it in LoopX (expires {}).",
                approval["expires_at"].as_str().unwrap_or("?")
            ),
        );
        let poll = deps
            .approval_poll
            .unwrap_or(std::time::Duration::from_secs(1));
        let channels = deps.approval_channels.unwrap_or_else(|| {
            crate::approval_relay::channels(&crate::connectors_config::ConfigConnectors::real())
        });
        let mut relay = crate::approval_relay::Relay::start(channels, &approval, &log);
        let status = approvals::wait_for_decision_with(&dir, &approval_id, poll, &mut || {
            relay.poll(&dir, &approval_id, &log)
        });
        let decided_by = approvals::get(&dir, &approval_id)
            .and_then(|a| a["decided_by"].as_str().map(str::to_owned));
        relay.close(&status, decided_by.as_deref(), &log);
        emit(
            "approval.decided",
            json!({"approval_id": approval_id, "status": status}),
        );
        if status == "changes_requested" {
            let notes = approvals::get(&dir, &approval_id)
                .and_then(|a| a["notes"].as_str().map(str::to_owned))
                .unwrap_or_default();
            if deps.revisions_left > 0 {
                log("the reviewer asked for changes; sending them back to the agent");
                return format!("{CHANGES_REQUESTED}{notes}");
            }
            return escalate("approval_changes_requested", &notes, None);
        }
        if status != "approved" {
            let reason = if status == "timeout" {
                "approval_timeout"
            } else {
                "approval_denied"
            };
            return escalate(reason, "", None);
        }
    }
    let (ok, detail) = match deps.opener {
        Some(open) => open(&local_path, &branch, &target, &title),
        None => real_opener(&local_path, &branch, &target, &title),
    };
    if !ok {
        return escalate("mr_open_failed", &tail(&detail, FAILURE_TAIL_CHARS), None);
    }

    // The commands the verifier actually ran (the agent's logged ones),
    // else the stored ones for a result without that evidence.
    let ran: Vec<String> = result
        .iterations
        .last()
        .into_iter()
        .flat_map(|i| &i.verification_results)
        .filter_map(|r| r.evidence.get("commands").and_then(Value::as_array))
        .flatten()
        .filter_map(|c| c.get("command").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let ran = if ran.is_empty() {
        ["test_cmd", "lint_cmd"]
            .iter()
            .filter_map(|k| {
                project
                    .get(*k)
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
            })
            .map(str::to_owned)
            .collect()
    } else {
        ran
    };
    let checks: Vec<String> = ran.iter().map(|c| format!("- `{c}` \u{2705}")).collect();
    let summary = data.get("summary").and_then(Value::as_str).unwrap_or("");
    // Same shape the batch wrap-up reads from an agent-emitted fix event.
    let mr_url = find_mr_url(&detail);
    api(
        "POST",
        &format!("{notes_path}/notes"),
        json!({"body": issue_comment(mr_url.as_deref(), &branch, &target, summary, checks.len())}),
    );
    // A push option can't carry a multi-line description, so set it once the MR exists.
    match mr_url.as_deref().and_then(mr_iid) {
        Some(mr) => {
            let mut description = mr_description(&data, issue_iid, summary, &checks);
            let section = screenshots_description(
                &data,
                &handoff_dir,
                &screens,
                &project_id,
                gitlab_calls,
                &log,
            );
            if let Some(section) = section {
                description = format!("{description}\n\n{section}");
            }
            api(
                "PUT",
                &format!("projects/{}/merge_requests/{mr}", quote(&project_id)),
                json!({"description": description}),
            )
        }
        None => log("no merge request URL in the push output; its description was not set"),
    }
    emit(
        "issue.completed",
        json!({"outcome": "mr_opened", "gated": true, "action": "fix", "mr_url": mr_url}),
    );
    // Step 10's "Finished" message, which the gate override tells the agent not to send.
    let link = match &mr_url {
        Some(u) => format!("<{u}|view MR>"),
        None => branch.clone(),
    };
    let setup_note = match deps.setup_fixed {
        Some(n) if n > 0 => setup_fixed_note(n),
        _ => String::new(),
    };
    notify(
        &bundle,
        format!("*Finished* {issue_label}: MR opened \u{2192} {link}{setup_note}"),
    );
    "mr_opened".to_string()
}

/// The "## Screenshots" section for the handoff's screenshot pairs: uploads
/// each image, drops a pair whose `after` fails, and folds every problem and
/// the agent's `screenshots_note` into one note line.
fn screenshots_description(
    data: &Value,
    handoff_dir: &Path,
    screens: &Path,
    project_id: &str,
    gitlab_calls: &dyn GitlabCalls,
    log: &dyn Fn(&str),
) -> Option<String> {
    let (pairs, mut problems) = mr_screenshots::screenshot_pairs(data, handoff_dir, screens);
    let mut uploaded = Vec::new();
    for pair in pairs {
        let after = match gitlab_calls.upload(project_id, &pair.after) {
            Ok(url) => url,
            Err(e) => {
                log(&format!("screenshot upload failed: {e}"));
                problems.push(format!("\"{}\" dropped: upload failed", pair.label));
                continue;
            }
        };
        let before = pair.before.as_ref().and_then(|p| {
            gitlab_calls
                .upload(project_id, p)
                .map_err(|e| {
                    log(&format!("screenshot upload failed: {e}"));
                    problems.push(format!("\"{}\" before image not uploaded", pair.label));
                })
                .ok()
        });
        uploaded.push(mr_screenshots::UploadedPair {
            label: pair.label,
            before_failed: pair.before.is_some() && before.is_none(),
            before,
            after,
        });
    }
    for p in &problems {
        log(&format!("screenshots: {p}"));
    }
    // Validation drops and upload failures alike, then the agent's own note
    // as a sentence of its own.
    let mut sentences = Vec::new();
    if !problems.is_empty() {
        sentences.push(format!(
            "Some screenshots were left out: {}.",
            problems.join("; ")
        ));
    }
    if let Some(extra) = data
        .get("screenshots_note")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let end = if extra.ends_with(['.', '!', '?']) {
            ""
        } else {
            "."
        };
        sentences.push(format!("{extra}{end}"));
    }
    let note = (!sentences.is_empty()).then(|| sentences.join(" "));
    mr_screenshots::screenshots_section(&uploaded, note.as_deref())
}

/// The comment posted on the issue: the merge request link, then the agent's goal and
/// key results, then how many checks the loop ran. The commands themselves, like
/// everything else about how it was done, are in the merge request description.
fn issue_comment(
    mr_url: Option<&str>,
    branch: &str,
    target: &str,
    summary: &str,
    checks: usize,
) -> String {
    let mr = match mr_url.and_then(mr_iid) {
        Some(iid) => format!("!{iid}"),
        None => format!("`{branch}`"),
    };
    let summary = summary.trim();
    // Continue the agent's bullet list when it ends in one, else start a new one.
    let sep = match summary.lines().last() {
        None => "",
        Some(l) if l.trim_start().starts_with("- ") => "\n",
        Some(_) => "\n\n",
    };
    format!(
        "**Merge request ready for review:** {mr} \u{2192} `{target}`\n\n{summary}{sep}- Checks: {checks}/{checks} passed \u{2705} (commands in the MR)"
    )
}

/// First `https?://<non-space>/merge_requests/<digits>` in `text`.
/// The merge request's number from its web URL.
pub(crate) fn mr_iid(url: &str) -> Option<&str> {
    url.rsplit_once("/merge_requests/")
        .map(|(_, iid)| iid)
        .filter(|iid| !iid.is_empty() && iid.bytes().all(|b| b.is_ascii_digit()))
}

/// The merge request description: the agent's (written to the create-mr skill's
/// template), led by `Closes #<iid>` so GitLab links and closes the issue; else one
/// built from the handoff summary and the checks the loop ran.
fn mr_description(data: &Value, issue_iid: &str, summary: &str, checks: &[String]) -> String {
    let closes = format!("Closes #{issue_iid}");
    match data
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
    {
        Some(d) if !d.is_empty() && d.contains(&closes) => d.to_string(),
        Some(d) if !d.is_empty() => format!("{closes}\n\n{d}"),
        _ => format!(
            "## What and why\n\n{closes}\n\n{summary}\n\n## Verified\n\n{}",
            checks.join("\n")
        )
        .trim_end()
        .to_string(),
    }
}

fn find_mr_url(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut start = 0;
    while let Some(rel) = text[start..].find("http") {
        let at = start + rel;
        start = at + 4;
        let rest = &text[at..];
        if !(rest.starts_with("http://") || rest.starts_with("https://")) {
            continue;
        }
        let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        let token = &rest[..end];
        if let Some(pos) = token.rfind("/merge_requests/") {
            let digits: String = token[pos + "/merge_requests/".len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if !digits.is_empty() {
                let _ = bytes;
                return Some(format!("{}/merge_requests/{digits}", &token[..pos]));
            }
        }
    }
    None
}

pub fn is_loop_escalation(outcome: Option<&str>) -> bool {
    outcome.is_some_and(|o| o.starts_with("escalated:") && o != "escalated:agent")
}

// ---- one issue --------------------------------------------------------------

/// The outcome of one issue's run, with the runner-side figures that don't
/// live in `result.json`.
pub struct IssueRun {
    pub result: LoopResult,
    pub usage: Option<Value>,
    pub gate_outcome: Option<String>,
    /// How many setup blockers the setup doctor fixed for this issue.
    pub setup_fixed: Option<usize>,
}

fn sum_or_none(values: &[Option<f64>]) -> Option<f64> {
    let priced: Vec<f64> = values.iter().flatten().copied().collect();
    (!priced.is_empty()).then(|| priced.iter().sum())
}

fn sum_usages(usages: &[Option<Value>]) -> Option<Value> {
    let real: Vec<&Value> = usages
        .iter()
        .flatten()
        .filter(|u| crate::loop_serialize::truthy(u))
        .collect();
    if real.is_empty() {
        return None;
    }
    let mut out = serde_json::Map::new();
    for field in USAGE_TOKEN_FIELDS {
        let total: i64 = real
            .iter()
            .map(|u| u.get(field).and_then(Value::as_i64).unwrap_or(0))
            .sum();
        out.insert(field.to_string(), json!(total));
    }
    Some(Value::Object(out))
}

/// What one [`LoopRuntime`] pass over an issue returns: its result and the
/// raw (un-coerced) cost and usage of each agent call it made.
type PassOutput = (LoopResult, Vec<Option<f64>>, Vec<Option<Value>>);

/// One runtime pass over an issue: the agent under the loop's budget and
/// verification. `initial_feedback` goes to the first attempt;
/// later attempts get the failed verification as feedback. `dirty_before`
/// refuses every attempt (a worktree-off checkout holding the user's work).
#[allow(clippy::too_many_arguments)]
fn run_pass(
    ctx: &RunCtx,
    run_id: &str,
    alias: &str,
    issue_iid: &str,
    definition: &LoopDefinition,
    invoker: Invoker,
    mode: InvokeMode,
    initial_feedback: Option<String>,
    dirty_before: bool,
    iteration_offset: i64,
) -> Result<PassOutput, String> {
    let issue_run_id = format!("{run_id}_{alias}_{issue_iid}");
    let timeout_seconds = definition.stop_conditions.max_runtime_minutes * 60.0;
    let verification_mode = definition.verification.mode.clone();
    let gate = verification_mode == "gate";
    let raw_costs: Arc<Mutex<Vec<Option<f64>>>> = Arc::default();
    let raw_usages: Arc<Mutex<Vec<Option<Value>>>> = Arc::default();
    let handoff = gate.then(|| handoff_path(run_id, alias, issue_iid, &ctx.repo_root));

    let mut issue = IssueContext::new(alias, issue_iid, timeout_seconds);
    issue.handoff_path = handoff.clone();
    issue.config_path = ctx.config_path.clone();

    let max_cost = definition.stop_conditions.max_cost_usd;
    let agent_fn: AgentFn = {
        let (raw_costs, raw_usages) = (raw_costs.clone(), raw_usages.clone());
        let (alias, issue_iid, run_id) =
            (alias.to_string(), issue_iid.to_string(), run_id.to_string());
        let handoff = handoff.clone();
        Box::new(move |context| {
            if dirty_before {
                return Err(AgentFailure {
                    message: "checkout_dirty".into(),
                    cost_usd: None,
                });
            }
            if let Some(h) = &handoff {
                // Each attempt starts with no handoff, so finalize can never
                // read an earlier attempt's.
                let _ = std::fs::remove_file(h);
            }
            let feedback = match context.previous {
                None => initial_feedback.clone(),
                Some(p) => Some(p)
                    .filter(|p| iteration_failed_verification(p))
                    .map(format_feedback),
            };
            let spent: f64 = raw_costs.lock().unwrap().iter().flatten().sum();
            let cap = cost::remaining_budget(Some(max_cost), Some(spent), 0.05);
            let req = InvokeRequest {
                alias: alias.clone(),
                issue_iid: issue_iid.clone(),
                mode: mode.clone(),
                feedback,
                gate,
                run_id: Some(run_id.clone()),
                max_budget_usd: cap,
                timeout_seconds,
                doctor: None,
            };
            match invoker(&req) {
                Ok(out) => {
                    raw_costs.lock().unwrap().push(out.cost_usd);
                    raw_usages.lock().unwrap().push(out.usage);
                    Ok(AgentSuccess {
                        cost_usd: out.cost_usd,
                    })
                }
                Err(e) => {
                    // A failed call may still have spent money; None = unknown.
                    raw_costs.lock().unwrap().push(e.cost_usd);
                    Err(AgentFailure {
                        message: e.message,
                        cost_usd: e.cost_usd,
                    })
                }
            }
        })
    };

    let verifiers = build_verifiers(
        &definition.verifiers,
        None,
        Some(&issue),
        &verification_mode,
    )
    .map_err(|e| e.to_string())?;
    let emitted: Arc<Mutex<Vec<i64>>> = Arc::default();
    let on_iteration = {
        let ctx = ctx.clone();
        let (run_id, issue_run_id, alias, issue_iid, mode) = (
            run_id.to_string(),
            issue_run_id.clone(),
            alias.to_string(),
            issue_iid.to_string(),
            verification_mode.clone(),
        );
        Box::new(
            move |_run: &str, _loop: &str, _def: &str, iterations: &[IterationResult]| {
                // Called with the cumulative list after every iteration; emit
                // only the newest one's verification events.
                let Some(latest) = iterations.last() else {
                    return;
                };
                let mut done = emitted.lock().unwrap();
                if done.contains(&latest.iteration) {
                    return;
                }
                done.push(latest.iteration);
                // Numbered after any earlier pass's iterations of this issue.
                let mut latest = latest.clone();
                latest.iteration += iteration_offset;
                emit_verification_events(
                    &ctx,
                    &run_id,
                    &issue_run_id,
                    &alias,
                    &issue_iid,
                    &mode,
                    &latest,
                );
            },
        )
    };

    let mut runtime = LoopRuntime::new(agent_fn, verifiers).with_on_iteration(on_iteration);
    if let Some(dir) = &ctx.events_dir {
        runtime = runtime.with_events_dir(dir.clone());
    }
    let result = runtime
        .start(definition, &issue_run_id, None)
        .map_err(|e| e.to_string())?;
    let costs = raw_costs.lock().unwrap().clone();
    let usages = raw_usages.lock().unwrap().clone();
    Ok((result, costs, usages))
}

pub fn run_one_issue(
    ctx: &RunCtx,
    run_id: &str,
    alias: &str,
    issue_iid: &str,
    definition: &LoopDefinition,
    invoker: Invoker,
    mode: InvokeMode,
) -> Result<IssueRun, String> {
    let issue_run_id = format!("{run_id}_{alias}_{issue_iid}");
    let gate = definition.verification.mode == "gate";
    // The scheduled batch and the dashboard's single-issue run each end with
    // one Slack report; only the golden eval's offline mode skips it.
    let quiet = matches!(mode, InvokeMode::Batch | InvokeMode::Single);

    // Worktree off: never start the agent on a checkout holding the user's
    // uncommitted work (a leftover restore record is healed by `worktree new`).
    let guard = checkout_guard(ctx, alias);
    let dirty_before = guard.as_ref().is_some_and(|g| !g.clean && !g.marked);

    let started = run_pass(
        ctx,
        run_id,
        alias,
        issue_iid,
        definition,
        invoker.clone(),
        mode.clone(),
        None,
        dirty_before,
        0,
    );
    // Whatever happened, a checkout branched in place goes back to the
    // user's branch (the push below needs only the branch ref).
    let checkout_problem = if dirty_before {
        guard.as_ref().map(|g| {
            format!(
                "The checkout {} has uncommitted or untracked changes, so the agent did not start. Commit or stash them and the next run picks this issue up again.",
                g.local_path
            )
        })
    } else {
        restore_in_place_checkout(ctx, alias, guard.as_ref())
    };
    if let Some(problem) = checkout_problem.as_deref().filter(|_| !gate) {
        ctx.log(&format!("{alias} #{issue_iid} checkout: {problem}"));
        ctx.emit(
            "issue.escalated",
            Some(run_id),
            EventFields {
                issue_run_id: Some(issue_run_id.clone()),
                project: Some(alias.to_string()),
                issue_iid: issue_iid.parse().ok(),
                data: Some(json!({"reason": "checkout_problem", "detail": problem})),
            },
        );
    }
    // finalize never runs on an error, so the screenshots go here.
    let failed = |e: String| {
        remove_issue_screenshots(ctx, run_id, alias, issue_iid);
        Err(e)
    };
    let pass = match started {
        Ok(p) => p,
        Err(e) => return failed(e),
    };
    let (pass, setup) = if gate && checkout_problem.is_none() {
        let issue = IssueArgs {
            ctx,
            run_id,
            alias,
            issue_iid,
            definition,
        };
        match after_setup_blocked(&issue, &invoker, &mode, pass) {
            Ok(p) => p,
            Err(e) => return failed(e),
        }
    } else {
        (pass, SetupOutcome::default())
    };
    let (mut result, mut raw_costs, mut raw_usages) = pass;
    let setup_fixed_for_report = setup.fixed;
    let mut setup_report = setup.report;
    let mut checkout_problem = checkout_problem;
    let mut revisions = 0;

    let mut gate_outcome = None;
    // A reviewer's "request changes" sends the work back for another pass and
    // a fresh approval, up to MAX_REVISIONS times.
    while gate && gate_outcome.is_none() {
        let fakes = ctx.fakes.as_deref();
        let notifier = fakes.map(|f| &*f.notifier as &dyn Fn(&str));
        let outcome = finalize_gated_issue(
            &result,
            alias,
            issue_iid,
            run_id,
            ctx,
            FinalizeDeps {
                gitlab: fakes.map(|f| &*f.gitlab as &dyn GitlabCalls),
                notifier,
                gitlab_config_path: fakes.map(|f| f.gitlab_config_path.clone()),
                checkout_problem: checkout_problem.take(),
                quiet,
                setup_report: setup_report.take(),
                setup_fixed: setup_fixed_for_report,
                revisions_left: MAX_REVISIONS - revisions,
                // Tests never reach the configured chat apps.
                approval_channels: fakes.map(|_| Vec::new()),
                ..FinalizeDeps::default()
            },
        );
        if let Some(notes) = outcome.strip_prefix(CHANGES_REQUESTED) {
            revisions += 1;
            let issue = IssueArgs {
                ctx,
                run_id,
                alias,
                issue_iid,
                definition,
            };
            issue.emit("approval.revision_started", json!({"round": revisions}));
            let (next, more_costs, more_usages) = run_pass(
                ctx,
                run_id,
                alias,
                issue_iid,
                definition,
                invoker.clone(),
                mode.clone(),
                Some(review_feedback(notes, issue_iid)),
                false,
                result.iterations.len() as i64,
            )?;
            raw_costs.extend(more_costs);
            raw_usages.extend(more_usages);
            result = merge_passes(result, next);
            continue;
        }
        if is_loop_escalation(Some(&outcome)) && result.final_state == LoopState::Completed {
            // Visible on Loop Runs as an escalation, not "completed".
            result.final_state = LoopState::Escalated;
            result.stop_reason = format!("gate:{}", outcome.split_once(':').map_or("", |x| x.1));
        }
        gate_outcome = Some(outcome);
    }

    // LoopRuntime counts a None cost as 0 in the budget, so record the raw,
    // un-coerced figure to keep "cost $0" apart from "never got a cost".
    result.agent_cost_usd = match sum_or_none(&raw_costs) {
        Some(v) => AgentCost::Usd(v),
        None => AgentCost::Unknown,
    };
    let usage = sum_usages(&raw_usages);

    if let Err(e) = write_result(
        &result,
        ctx.results_dir.as_deref(),
        ctx.events_dir.as_deref(),
        None,
    ) {
        ctx.log(&format!(
            "writing result.json for {issue_run_id} failed: {e}"
        ));
    }
    Ok(IssueRun {
        result,
        usage,
        gate_outcome,
        setup_fixed: setup_fixed_for_report,
    })
}

fn remove_issue_screenshots(ctx: &RunCtx, run_id: &str, alias: &str, issue_iid: &str) {
    let handoff = handoff_path(run_id, alias, issue_iid, &ctx.repo_root);
    let data = std::fs::read_to_string(&handoff)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());
    mr_screenshots::remove_screenshots(
        data.as_ref(),
        handoff.parent().unwrap_or(Path::new(".")),
        &mr_screenshots::screens_dir(&ctx.repo_root, alias, issue_iid),
    );
}

// ---- the setup doctor -------------------------------------------------------

/// The issue a pass and the setup doctor work on.
struct IssueArgs<'a> {
    ctx: &'a RunCtx,
    run_id: &'a str,
    alias: &'a str,
    issue_iid: &'a str,
    definition: &'a LoopDefinition,
}

impl IssueArgs<'_> {
    fn emit(&self, event_type: &str, data: Value) {
        self.ctx.emit(
            event_type,
            Some(self.run_id),
            EventFields {
                issue_run_id: Some(format!("{}_{}_{}", self.run_id, self.alias, self.issue_iid)),
                project: Some(self.alias.to_string()),
                issue_iid: self.issue_iid.parse().ok(),
                data: Some(data),
            },
        );
    }

    fn handoff(&self) -> PathBuf {
        handoff_path(self.run_id, self.alias, self.issue_iid, &self.ctx.repo_root)
    }

    /// The handoff, when it is a valid `setup_blocked` one (announced as
    /// `setup.blocked`).
    fn setup_blocked(&self) -> Option<Value> {
        let blocked = read_handoff(&self.handoff(), self.issue_iid)
            .filter(|h| h.get("action").and_then(Value::as_str) == Some("setup_blocked"))?;
        self.emit(
            "setup.blocked",
            json!({"command": blocked["command"], "error": blocked["error"]}),
        );
        Some(blocked)
    }
}

/// What finalize needs to know about the setup doctor.
#[derive(Default)]
struct SetupOutcome {
    report: Option<String>,
    fixed: Option<usize>,
}

/// After the first pass: a `setup_blocked` handoff gets one setup doctor
/// session and, when it fixes the setup, a second pass with a fresh issue
/// budget. Otherwise (or when the doctor cannot run) the report for the
/// escalation. The pass returned is the last one, with every earlier
/// iteration, cost and usage folded in.
fn after_setup_blocked(
    issue: &IssueArgs<'_>,
    invoker: &Invoker,
    mode: &InvokeMode,
    pass: PassOutput,
) -> Result<(PassOutput, SetupOutcome), String> {
    let Some(blocked) = issue.setup_blocked() else {
        return Ok((pass, SetupOutcome::default()));
    };
    let work_dir = match doctor_work_dir(issue, mode) {
        Ok(dir) => dir,
        Err(why) => {
            let report = setup_doctor::report_without_doctor(&blocked, why);
            return Ok((
                pass,
                SetupOutcome {
                    report: Some(report),
                    fixed: None,
                },
            ));
        }
    };
    // The doctor's own dirty-tree check must only ever see what it did. A
    // missing worktree (`loopx worktree new` failed) has nothing to lose.
    let uncommitted = worktree_changes(&work_dir);
    if !uncommitted.is_empty() {
        let report = format!(
            "The agent handed off setup_blocked without committing its work first, so the setup doctor did not run. Uncommitted in the worktree `{}`: {}.\n\n{}",
            work_dir.display(),
            backticked(&uncommitted),
            setup_doctor::report_without_doctor(&blocked, "the worktree had uncommitted changes")
        );
        return Ok((
            pass,
            SetupOutcome {
                report: Some(report),
                fixed: None,
            },
        ));
    }
    let (first, mut costs, mut usages) = pass;
    let doctor = run_doctor(issue, invoker, mode, &blocked, &work_dir);
    costs.push(doctor.cost_usd);
    usages.push(doctor.usage);
    let fixed = match doctor.result {
        Some(r) if r.status == DoctorStatus::Fixed => r,
        result => {
            let mut report = setup_doctor::doctor_report_markdown(&blocked, result.as_ref());
            if !doctor.changes.is_empty() {
                report = format!(
                    "After the setup doctor ran, the worktree `{}` has uncommitted changes, so the loop stopped instead of continuing on it: {}. The doctor may only write gitignored files; review these and remove them before re-running the issue.\n\n{report}",
                    work_dir.display(),
                    backticked(&doctor.changes)
                );
            }
            let outcome = SetupOutcome {
                report: Some(report),
                fixed: None,
            };
            return Ok(((first, costs, usages), outcome));
        }
    };
    let (second, more_costs, more_usages) = run_pass(
        issue.ctx,
        issue.run_id,
        issue.alias,
        issue.issue_iid,
        issue.definition,
        invoker.clone(),
        mode.clone(),
        Some(setup_doctor::doctor_feedback(&fixed, issue.issue_iid)),
        false,
        first.iterations.len() as i64,
    )
    .map_err(|e| second_pass_failed(issue, &costs, e))?;
    costs.extend(more_costs);
    usages.extend(more_usages);
    let outcome = SetupOutcome {
        // Blocked again: the doctor runs once per issue per run.
        report: issue
            .setup_blocked()
            .map(|again| setup_doctor::repeat_block_report(&fixed, &again)),
        fixed: Some(fixed.blockers.iter().filter(|b| b.fixed).count()),
    };
    Ok(((merge_passes(first, second), costs, usages), outcome))
}

/// The issue's worktree when the doctor may run there, else why it may not.
fn doctor_work_dir(issue: &IssueArgs<'_>, mode: &InvokeMode) -> Result<PathBuf, &'static str> {
    if !issue.definition.setup_doctor.enabled {
        return Err("it is switched off for this loop");
    }
    if matches!(mode, InvokeMode::IssueFile(_)) {
        return Err("it never runs in an offline eval");
    }
    let config = Config::load(&project_config_path(issue.ctx));
    let (project, root) = config
        .and_then(|c| Ok((c.project(issue.alias)?, c.worktree_root()?)))
        .map_err(|_| "the project's configuration could not be read")?;
    if !crate::config::use_worktree(&project) {
        return Err("it never touches a project's own checkout (`use_worktree: false`)");
    }
    Ok(crate::loop_verifiers::expected_work_dir(
        &project,
        Path::new(&root),
        issue.issue_iid,
    ))
}

/// One setup doctor session and what it left behind.
struct DoctorRun {
    /// `None` when it crashed, timed out or wrote no valid result.
    result: Option<DoctorResult>,
    /// What `git status --porcelain` lists in the worktree afterwards.
    changes: Vec<String>,
    cost_usd: Option<f64>,
    usage: Option<Value>,
}

/// Runs the doctor on its own clock and budget, then records what it learnt
/// (its `SETUP.md` draft and its one-line learning). A worktree it left
/// dirty turns the result into `needs_human`.
fn run_doctor(
    issue: &IssueArgs<'_>,
    invoker: &Invoker,
    mode: &InvokeMode,
    blocked: &Value,
    work_dir: &Path,
) -> DoctorRun {
    let ctx = issue.ctx;
    let config = &issue.definition.setup_doctor;
    let handoff = issue.handoff();
    let result_path = setup_doctor::doctor_result_path(&handoff);
    let draft_path = setup_doctor::setup_draft_path(&handoff);
    // Only this session's files count.
    let _ = std::fs::remove_file(&result_path);
    let _ = std::fs::remove_file(&draft_path);
    let memory = ctx.memory_root.as_deref();
    let knowledge = memory_store::read_setup_knowledge(issue.alias, memory);
    let stale = knowledge
        .as_deref()
        .map(|k| memory_store::stale_setup_sources(k, work_dir))
        .unwrap_or_default();
    issue.emit(
        "setup.doctor_started",
        json!({"work_dir": work_dir.display().to_string(), "stale": stale}),
    );
    let req = InvokeRequest {
        alias: issue.alias.to_string(),
        issue_iid: issue.issue_iid.to_string(),
        mode: mode.clone(),
        feedback: None,
        gate: true,
        run_id: Some(issue.run_id.to_string()),
        max_budget_usd: Some(config.max_cost_usd),
        timeout_seconds: config.max_runtime_minutes * 60.0,
        doctor: Some(DoctorRequest {
            work_dir: work_dir.display().to_string(),
            blocked: blocked.clone(),
            max_attempts: config.max_attempts,
            stale,
            has_knowledge: knowledge.is_some(),
        }),
    };
    let log = |text: &str| {
        ctx.log(&format!(
            "setup doctor {} #{}: {text}",
            issue.alias, issue.issue_iid
        ))
    };
    let (cost_usd, usage) = match invoker(&req) {
        Ok(out) => (out.cost_usd, out.usage),
        Err(e) => {
            log(&format!("the session failed: {}", e.message));
            (e.cost_usd, None)
        }
    };
    let mut result = setup_doctor::read_doctor_result(&result_path);
    let changes = worktree_changes(work_dir);
    if let Some(r) = result.as_mut().filter(|_| !changes.is_empty()) {
        r.status = DoctorStatus::NeedsHuman;
        r.blockers.push(setup_doctor::Blocker {
            error: format!(
                "uncommitted changes in the worktree: {}",
                changes.join(", ")
            ),
            cause: "the setup doctor left git-visible changes in the worktree".into(),
            tried: Vec::new(),
            fixed: false,
        });
    }
    if let Ok(draft) = std::fs::read_to_string(&draft_path) {
        match memory_store::save_setup_knowledge(issue.alias, &draft, memory) {
            Ok(true) => {}
            Ok(false) => log("its SETUP.md draft has no `# ` heading; kept the old one"),
            Err(e) => log(&format!("saving SETUP.md failed: {e}")),
        }
    }
    let learning = result.as_ref().and_then(|r| r.learning.as_deref());
    if let (Some(lesson), Ok(iid)) = (learning, issue.issue_iid.parse::<i64>()) {
        let tags = ["setup".to_string()];
        if let Err(e) =
            memory_store::add_task_memory(issue.alias, iid, lesson, &tags, Some("setup"), memory)
        {
            log(&format!("recording its learning failed: {e}"));
        }
    }
    let status = match result.as_ref().map(|r| r.status) {
        Some(DoctorStatus::Fixed) => "fixed",
        Some(DoctorStatus::GaveUp) => "gave_up",
        Some(DoctorStatus::NeedsHuman) => "needs_human",
        None => "did_not_finish",
    };
    let blockers = result.as_ref().map_or(&[][..], |r| &r.blockers[..]);
    issue.emit(
        "setup.doctor_finished",
        json!({"status": status, "blockers": blockers.len(),
               "fixed": blockers.iter().filter(|b| b.fixed).count(), "cost_usd": cost_usd}),
    );
    DoctorRun {
        result,
        changes,
        cost_usd,
        usage,
    }
}

/// The paths `git status --porcelain` lists in `work_dir` (none when the
/// worktree does not exist).
fn worktree_changes(work_dir: &Path) -> Vec<String> {
    if !work_dir.is_dir() {
        return Vec::new();
    }
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(work_dir)
        .args(["status", "--porcelain"])
        .output();
    match out {
        // Not trimmed: each line's two status columns carry meaning.
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.get(3..).unwrap_or(l).to_string())
            .collect(),
        _ => vec![format!("(git status failed in {})", work_dir.display())],
    }
}

/// `a`, `b`: file names as inline code, comma-separated.
fn backticked(files: &[String]) -> String {
    files
        .iter()
        .map(|f| format!("`{f}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The note on an issue's Slack line when the setup doctor fixed its setup.
fn setup_fixed_note(n: usize) -> String {
    let noun = if n == 1 { "blocker" } else { "blockers" };
    format!(" (setup fixed automatically: {n} {noun})")
}

/// A second pass that errored returns no result, so what pass 1 and the
/// doctor spent is logged and emitted here before the error goes up.
fn second_pass_failed(issue: &IssueArgs<'_>, costs: &[Option<f64>], error: String) -> String {
    let spent = sum_or_none(costs);
    let shown = spent.map_or("an unknown amount".to_string(), |c| format!("${c:.2}"));
    issue.ctx.log(&format!(
        "{} #{}: the pass after the setup doctor failed ({error}); {shown} was already spent on this issue",
        issue.alias, issue.issue_iid
    ));
    issue.emit(
        "setup.pass2_failed",
        json!({"error": error, "cost_usd": spent}),
    );
    error
}

/// The second pass's result, with the first pass's iterations before its own.
fn merge_passes(first: LoopResult, mut second: LoopResult) -> LoopResult {
    let offset = first.iterations.len() as i64;
    for it in &mut second.iterations {
        it.iteration += offset;
    }
    let mut iterations = first.iterations;
    iterations.append(&mut second.iterations);
    second.iterations = iterations;
    second.cost_reported = match (first.cost_reported, second.cost_reported) {
        (Some(a), Some(b)) => Some(a || b),
        (a, b) => a.or(b),
    };
    second
}

fn project_config_path(ctx: &RunCtx) -> PathBuf {
    ctx.config_path.clone().unwrap_or_else(default_config_path)
}

/// A worktree-off project's checkout, as the runner sees it around a run.
struct CheckoutGuard {
    local_path: String,
    head: String,
    clean: bool,
    marked: bool,
}

fn git_out(dir: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn restore_marked(local_path: &str) -> bool {
    git_out(local_path, &["rev-parse", "--absolute-git-dir"])
        .is_some_and(|d| Path::new(&d).join("loopx-restore-branch").exists())
}

/// The checkout of `alias` when it is worked on in place, with its state now.
fn checkout_guard(ctx: &RunCtx, alias: &str) -> Option<CheckoutGuard> {
    let project = Config::load(&project_config_path(ctx))
        .and_then(|c| c.project(alias))
        .ok()?;
    if crate::config::use_worktree(&project) {
        return None;
    }
    let local_path = project.get("local_path")?.as_str()?.to_string();
    Some(CheckoutGuard {
        head: git_out(&local_path, &["rev-parse", "HEAD"]).unwrap_or_default(),
        clean: git_out(&local_path, &["status", "--porcelain"]).is_some_and(|s| s.is_empty()),
        marked: restore_marked(&local_path),
        local_path,
    })
}

/// Put an in-place checkout back on the user's branch whenever a restore
/// record exists (whatever `use_worktree` says now), and check the agent did
/// not change a checkout it never branched. `Some(problem)` to escalate.
fn restore_in_place_checkout(
    ctx: &RunCtx,
    alias: &str,
    before: Option<&CheckoutGuard>,
) -> Option<String> {
    let local_path = match before {
        Some(g) => g.local_path.clone(),
        None => Config::load(&project_config_path(ctx))
            .and_then(|c| c.project(alias))
            .ok()?
            .get("local_path")?
            .as_str()?
            .to_string(),
    };
    match crate::worktree::restore_in_place(&local_path) {
        Err(e) => Some(e.message),
        Ok(Some(_)) => None,
        Ok(None) => {
            let g = before.filter(|g| !g.marked)?;
            let head = git_out(&local_path, &["rev-parse", "HEAD"]).unwrap_or_default();
            let clean =
                git_out(&local_path, &["status", "--porcelain"]).is_some_and(|s| s.is_empty());
            (head != g.head || clean != g.clean).then(|| {
                format!(
                    "The checkout {local_path} changed during the run without `loopx worktree new` branching it first, so the loop left it exactly as it is for you to review."
                )
            })
        }
    }
}

// ---- whole runs -------------------------------------------------------------

/// Deletes each tracked checkout's `loop/issue-<iid>` branch (and worktree) once
/// GitLab says the issue is done. Best-effort: anything it cannot check or
/// remove is logged and kept for the next run.
fn sweep_finished_branches(ctx: &RunCtx, cfg: &Config, aliases: &[String]) {
    let gitlab_config = gitlab::load_config();
    for alias in aliases {
        let Some(local_path) = cfg.project(alias).ok().and_then(|p| {
            p.get("local_path")
                .and_then(Value::as_str)
                .map(str::to_owned)
        }) else {
            continue;
        };
        let (project_id, instance, bundle) =
            gitlab::resolve_project_alias(gitlab_config.as_ref(), alias, None);
        let client = match Client::from_config(
            gitlab_config.as_ref(),
            instance.as_deref(),
            bundle.as_deref(),
        ) {
            Ok(c) => c,
            Err(e) => {
                ctx.log(&format!("branch cleanup skipped for {alias}: {e}"));
                continue;
            }
        };
        let mut is_finished =
            |iid: &str| match iid.parse().map(|n| client.issue_finished(&project_id, n)) {
                Ok(Ok(done)) => done,
                Ok(Err(e)) => {
                    ctx.log(&format!(
                        "branch cleanup: could not check {alias} #{iid}: {e}"
                    ));
                    false
                }
                Err(_) => false,
            };
        match worktree::sweep_finished_issue_branches(&local_path, &mut is_finished) {
            Ok(sweep) => {
                for iid in &sweep.removed {
                    ctx.log(&format!(
                        "branch cleanup: deleted {} ({alias} #{iid} is done)",
                        worktree::branch_name(iid)
                    ));
                }
                for (iid, why) in &sweep.skipped {
                    ctx.log(&format!(
                        "branch cleanup: kept {} ({alias} #{iid}): {why}",
                        worktree::branch_name(iid)
                    ));
                }
            }
            Err(e) => ctx.log(&format!(
                "branch cleanup skipped for {alias}: {}",
                e.message
            )),
        }
    }
}

pub fn run_all_issues(
    ctx: &RunCtx,
    run_id: &str,
    invoker: Invoker,
    aliases: Option<Vec<String>>,
    username: Option<String>,
) -> Result<Vec<IssueRun>, String> {
    let started = std::time::Instant::now();
    let cfg = Config::load(&default_config_path()).map_err(|e| e.to_string())?;
    let aliases = match aliases {
        Some(a) => a,
        None => cfg.aliases().map_err(|e| e.to_string())?,
    };
    let username = match username {
        Some(u) => u,
        None => cfg.assignee_username().map_err(|e| e.to_string())?,
    };
    let definition =
        LoopDefinition::from_yaml(&ctx.definition_path()).map_err(|e| e.to_string())?;
    let assigned =
        gitlab::list_assigned_issues(gitlab::load_config().as_ref(), &aliases, &username)
            .map_err(|e| e.to_string())?;
    let timeout_seconds = definition.stop_conditions.max_runtime_minutes * 60.0;
    sweep_finished_branches(ctx, &cfg, &aliases);

    let tracking = issue_tracking_config::default_tracking_path();
    let mut todo: Vec<(String, Value)> = Vec::new();
    for (alias, issues) in assigned {
        for issue in issues {
            let iid = issue.get("iid").cloned().unwrap_or(Value::Null);
            let iid_text = value_text(&iid);
            let enabled = issue_tracking_config::is_issue_enabled(&alias, &iid_text, &tracking)
                .unwrap_or_else(|e| {
                    ctx.log(&format!("issue tracking config unreadable ({e}); treating {alias} #{iid_text} as enabled"));
                    true
                });
            if enabled {
                todo.push((alias.clone(), issue));
            }
        }
    }
    let results = run_issue_batch(ctx, run_id, &cfg, todo, &definition, invoker, 1)?;

    // UNCONDITIONAL, and outside LoopRuntime: "End of run" is housekeeping, not
    // a verifiable task. It must run even with zero results: a morning with no
    // assigned issues is exactly when nothing else would report anything.
    let mut wrapup_error = None;
    if let Err(e) = invoke_batch_end_of_run(ctx, timeout_seconds) {
        let detail = format!("AgentError: {}", e.message);
        ctx.log(&format!(
            "end-of-run wrap-up FAILED - no daily digest was sent: {detail}"
        ));
        ctx.emit(
            "run.wrapup_failed",
            Some(run_id),
            EventFields {
                data: Some(json!({"error": tail(&detail, STDERR_EXCERPT_CHARS)})),
                ..Default::default()
            },
        );
        wrapup_error = Some(tail(&detail, STDERR_EXCERPT_CHARS));
    }

    // Down-weight lessons whose reuse preceded a failure. Best-effort and
    // idempotent (applied.json), so a two-day event window is safe.
    let since = (Utc::now() - Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let recent = events::iter_events(ctx.events_dir.as_deref(), Some(&since), None);
    let applied = ctx
        .repo_root
        .join("outputs")
        .join("learning")
        .join("applied.json");
    if let Err(e) = learning::apply_outcomes(&recent, None, Some(&applied)) {
        ctx.log(&format!("memory down-weighting FAILED: {e}"));
    }
    send_run_report(
        ctx,
        run_id,
        "Daily GitLab loop",
        &results,
        started.elapsed().as_secs_f64(),
        wrapup_error,
    );
    Ok(results)
}

/// The batch's issues, at most `max_parallel` at once (see [`crate::parallel`]).
/// Each issue holds its own lock while it runs, so an issue another run is
/// already working on is skipped rather than worked twice; issues of a project
/// worked on in its own checkout (worktree off) never overlap.
pub fn run_issue_batch(
    ctx: &RunCtx,
    run_id: &str,
    cfg: &Config,
    todo: Vec<(String, Value)>,
    definition: &LoopDefinition,
    invoker: Invoker,
    max_parallel: usize,
) -> Result<Vec<IssueRun>, String> {
    let tasks: Vec<crate::parallel::Task<(String, Value)>> = todo
        .into_iter()
        .map(|(alias, issue)| {
            let iid = value_text(&issue["iid"]);
            crate::parallel::Task {
                lock_path: crate::file_lock::issue_lock_path(&ctx.repo_root, &alias, &iid),
                group: in_place_group(cfg, &alias),
                item: (alias, issue),
            }
        })
        .collect();
    let outcomes = crate::parallel::run_tasks(&tasks, max_parallel, |(alias, issue)| {
        let iid_text = value_text(&issue["iid"]);
        // Title and link go in the ledger so the dashboard's Live page can show
        // them without calling GitLab when it renders.
        ctx.emit(
            "issue.queued",
            Some(run_id),
            EventFields {
                issue_run_id: Some(format!("{run_id}_{alias}_{iid_text}")),
                project: Some(alias.clone()),
                issue_iid: iid_text.parse().ok(),
                data: Some(json!({
                    "title": value_text_or_empty(&issue["title"]),
                    "web_url": value_text_or_empty(&issue["web_url"]),
                })),
            },
        );
        run_one_issue(
            ctx,
            run_id,
            alias,
            &iid_text,
            definition,
            invoker.clone(),
            InvokeMode::Batch,
        )
    });
    let mut results = Vec::new();
    let mut first_error = None;
    for (task, outcome) in tasks.iter().zip(outcomes) {
        let (alias, issue) = &task.item;
        let label = format!("{alias} #{}", value_text(&issue["iid"]));
        match outcome {
            crate::parallel::Outcome::Ran(Ok(run)) => results.push(run),
            crate::parallel::Outcome::Ran(Err(e)) => {
                first_error.get_or_insert(e);
            }
            crate::parallel::Outcome::AlreadyRunning => ctx.log(&format!(
                "{label} skipped: another run is already working on it"
            )),
            crate::parallel::Outcome::LockFailed(e) => {
                ctx.log(&format!("{label} skipped: cannot take its lock: {e}"))
            }
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(results),
    }
}

/// Issues of a worktree-off project share its checkout, so they share a group
/// and never run at once; worktree issues each get their own directory.
fn in_place_group(cfg: &Config, alias: &str) -> Option<String> {
    let project = cfg.project(alias).ok()?;
    if crate::config::use_worktree(&project) {
        return None;
    }
    let path = project
        .get("local_path")
        .and_then(Value::as_str)
        .unwrap_or(alias);
    Some(format!("checkout:{path}"))
}

/// The batch's one Slack report per bundle, built from this run's events.
/// Replaces the per-issue Starting/Finished/Escalated lines, the agent's
/// digest and the "issues incomplete" alert, all held during the batch.
pub fn send_run_report(
    ctx: &RunCtx,
    run_id: &str,
    title: &str,
    runs: &[IssueRun],
    elapsed_seconds: f64,
    wrapup_error: Option<String>,
) {
    let since = (Utc::now() - Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let events = events::iter_events(ctx.events_dir.as_deref(), Some(&since), None);
    let unfinished: Vec<(String, String)> = runs
        .iter()
        .filter(|r| r.result.final_state != LoopState::Completed)
        .map(|r| {
            (
                r.result.run_id.clone(),
                format!(
                    "{}: {}",
                    r.result.final_state.as_str(),
                    r.result.stop_reason
                ),
            )
        })
        .collect();
    let mut lines = gitlab_report::issue_lines(run_id, &events, &unfinished);
    for line in lines.iter_mut() {
        let issue_run_id = format!("{run_id}_{}_{}", line.alias, line.iid);
        if let Some(n) = runs
            .iter()
            .find(|r| r.result.run_id == issue_run_id)
            .and_then(|r| r.setup_fixed)
            .filter(|&n| n > 0)
        {
            if matches!(line.outcome, gitlab_report::Outcome::Escalated(_)) {
                continue;
            }
            line.title.push_str(&setup_fixed_note(n));
        }
    }
    let fakes = ctx.fakes.as_deref();
    let gitlab_config = fakes.map(|f| f.gitlab_config_path.clone());
    // Only a batch records each issue's link (`issue.queued`); a single-issue
    // run's line gets one built from its project, so Slack can still link it.
    let config = Config::load(&project_config_path(ctx)).ok();
    for line in lines.iter_mut().filter(|l| l.web_url.is_none()) {
        let project = config.as_ref().and_then(|c| c.project(&line.alias).ok());
        if let (Some(project), true) = (project, !line.iid.is_empty()) {
            if let (_, Some(url)) =
                project_slack_target(&line.alias, &project, gitlab_config.as_deref())
            {
                line.web_url = Some(format!("{url}/-/issues/{}", line.iid));
            }
        }
    }
    let facts = gitlab_report::RunFacts {
        title: title.to_string(),
        run_id: run_id.to_string(),
        elapsed_seconds,
        cost_usd: aggregate_cost_usd(runs),
        wrapup_error,
    };
    let bundle_of =
        |alias: &str| project_slack_target(alias, &json!({}), gitlab_config.as_deref()).0;
    for (bundle, lines) in gitlab_report::by_bundle(lines, &bundle_of) {
        let report = gitlab_report::build(&lines, &facts);
        match fakes {
            Some(f) => (f.notifier)(&slack_report::plain_text(&report)),
            None => {
                if !slack_report::send(&report, bundle.as_deref()) {
                    ctx.log("the run's Slack report was not sent (see the loop's stderr)");
                }
            }
        }
    }
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn value_text_or_empty(v: &Value) -> String {
    value_text(v)
}

pub fn run_single_issue(
    ctx: &RunCtx,
    run_id: &str,
    alias: &str,
    issue_iid: &str,
    invoker: Invoker,
) -> Result<IssueRun, String> {
    let definition =
        LoopDefinition::from_yaml(&ctx.definition_path()).map_err(|e| e.to_string())?;
    // Never alongside another run working on the same issue.
    let lock_path = crate::file_lock::issue_lock_path(&ctx.repo_root, alias, issue_iid);
    let Some(_lock) = crate::file_lock::FileLock::try_exclusive(&lock_path)
        .map_err(|e| format!("cannot lock {alias} #{issue_iid}: {e}"))?
    else {
        return Err(format!(
            "{alias} #{issue_iid} is already being worked on by another run"
        ));
    };
    // The dashboard's on-demand path keeps the 2-arg prompt with its own full
    // "End of run".
    run_one_issue(
        ctx,
        run_id,
        alias,
        issue_iid,
        &definition,
        invoker,
        InvokeMode::Single,
    )
}

/// Total real dollars across `runs`, or `None` when not a single run
/// contributed an actual figure. `None`, not 0.0: cost metrics read a non-null
/// `cost_usd` on `run.completed` as "this run was priced".
pub fn aggregate_cost_usd(runs: &[IssueRun]) -> Option<f64> {
    let contributions: Vec<Option<f64>> = runs
        .iter()
        .filter_map(|r| match r.result.agent_cost_usd {
            AgentCost::Usd(v) => Some(Some(v)),
            AgentCost::Unknown => Some(None),
            AgentCost::Unset => r
                .result
                .iterations
                .last()
                .map(|it| it.budget.pointer("/cost/used_usd").and_then(Value::as_f64)),
        })
        .collect();
    sum_or_none(&contributions)
}

pub fn aggregate_usage_tokens(runs: &[IssueRun]) -> Option<Value> {
    let per: Vec<Option<Value>> = runs.iter().map(|r| r.usage.clone()).collect();
    sum_usages(&per)
}

fn emit_run_completed(ctx: &RunCtx, run_id: &str, runs: &[IssueRun]) {
    let mut data = json!({"issues": runs.len()});
    if let Some(cost) = aggregate_cost_usd(runs) {
        data["cost_usd"] = json!(cost);
    }
    if let Some(Value::Object(tokens)) = aggregate_usage_tokens(runs) {
        for (k, v) in tokens {
            data[k] = v;
        }
    }
    ctx.emit(
        "run.completed",
        Some(run_id),
        EventFields {
            data: Some(data),
            ..Default::default()
        },
    );
}

/// `loopx` entry for the GitLab loop: `<run_id>` (the batch) or
/// `<run_id> <alias> <issue_iid>` (a scoped single issue). Anything else is a
/// mistake, not a batch run: a typo like `harbor` alone must not fall through
/// and process every assigned issue across every project.
pub fn main(ctx: &RunCtx, argv: &[String], invoker: Invoker) -> i32 {
    if argv.len() != 1 && argv.len() != 3 {
        eprintln!("Usage: loopx run gitlab-loop: <run_id> [alias issue_iid]");
        return 2;
    }
    let run_id = &argv[0];
    let started = std::time::Instant::now();
    let runs = if argv.len() == 3 {
        run_single_issue(ctx, run_id, &argv[1], &argv[2], invoker).map(|r| {
            let runs = vec![r];
            let title = format!("GitLab issue run: {} #{}", argv[1], argv[2]);
            send_run_report(
                ctx,
                run_id,
                &title,
                &runs,
                started.elapsed().as_secs_f64(),
                None,
            );
            runs
        })
    } else {
        run_all_issues(ctx, run_id, invoker, None, None)
    };
    let runs = match runs {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    emit_run_completed(ctx, run_id, &runs);
    // Still 0 when issues failed: each failure is contained, recorded in its
    // own LoopResult, and said in the run's Slack report. A non-zero exit would report the
    // whole run as failed, which it wasn't.
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loop_result::VerificationResult;

    fn v(name: &str, passed: bool, output: &str, evidence: Value) -> VerificationResult {
        VerificationResult {
            name: name.into(),
            passed,
            exit_code: Some(if passed { 0 } else { 1 }),
            duration_ms: 1,
            output: output.into(),
            evidence,
        }
    }

    fn iteration(results: Vec<VerificationResult>) -> IterationResult {
        IterationResult {
            iteration: 1,
            state: LoopState::Evaluating,
            verification_results: results,
            budget: json!({}),
            progressed: true,
        }
    }

    struct Notes(std::sync::Mutex<Vec<(String, String, Value)>>);
    impl GitlabCalls for Notes {
        fn call(&self, method: &str, endpoint: &str, body: &Value) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push((method.into(), endpoint.into(), body.clone()));
            Ok(())
        }
    }

    #[test]
    fn mr_description_lists_commands_actually_run() {
        let d = tempfile::tempdir().unwrap();
        let handoff = d.path().join("h.json");
        std::fs::write(
            &handoff,
            json!({"action": "fix", "branch": "loop/issue-7", "target_branch": "main", "title": "Fix #7"}).to_string(),
        )
        .unwrap();
        let evidence = json!({"commands": [{"kind": "test", "command": "pnpm test", "passed": true, "source": "handoff"}]});
        let result = LoopResult::new(
            "l",
            "r",
            "gitlab-issue",
            LoopState::Completed,
            vec![iteration(vec![v("project_commands", true, "", evidence)])],
            "",
        );
        let project = json!({"project_id": "g/p", "local_path": "/x", "instance": "g", "test_cmd": "bundle exec rspec"});
        let notes = Notes(Default::default());
        let opener = |_: &str, _: &str, _: &str, _: &str| {
            (
                true,
                "remote:   https://gl.example/g/p/-/merge_requests/1489\n".to_string(),
            )
        };
        let ctx = RunCtx {
            repo_root: d.path().into(),
            events_dir: Some(d.path().join("events")),
            results_dir: None,
            unified_log: d.path().join("log"),
            exe: "loopx".into(),
            config_path: None,
            fakes: None,
            memory_root: None,
            activity_file: d.path().join("agent-activity.json"),
            stall_after: crate::agent_activity::STALL_AFTER,
        };
        let notifier = |_: &str| {};
        let outcome = finalize_gated_issue(
            &result,
            "web",
            "7",
            "r",
            &ctx,
            FinalizeDeps {
                opener: Some(&opener),
                gitlab: Some(&notes),
                notifier: Some(&notifier),
                handoff: Some(handoff),
                project: Some(project),
                gitlab_config_path: Some(d.path().join("none.json")),
                require_approval: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(outcome, "mr_opened");
        let calls = notes.0.into_inner().unwrap();
        let body = mr_description(&calls);
        assert!(body.contains("`pnpm test`"), "{body}");
        assert!(!body.contains("rspec"), "{body}");
    }

    /// A chat app (Telegram) where someone presses Allow on the first poll.
    struct AllowingChat(crate::connectors::Account, Arc<Mutex<Vec<String>>>);

    impl crate::connectors::Connector for AllowingChat {
        fn info(&self) -> &'static crate::connectors::TypeInfo {
            &crate::connectors::telegram::INFO
        }
        fn account(&self) -> &crate::connectors::Account {
            &self.0
        }
        fn request_approval(&self, a: &Value) -> crate::connectors::ConnResult<Value> {
            self.1
                .lock()
                .unwrap()
                .push(format!("asked {}", a["project"]));
            Ok(json!({"message_id": 1}))
        }
        fn poll_approval(
            &self,
            _h: &Value,
            offset: i64,
        ) -> crate::connectors::ConnResult<(i64, Option<crate::connectors::ChatDecision>)> {
            Ok((
                offset + 1,
                Some(crate::connectors::ChatDecision {
                    decision: "approve".into(),
                    notes: None,
                    by: "telegram:@ana".into(),
                }),
            ))
        }
        fn close_approval(&self, _h: &Value, outcome: &str) -> crate::connectors::ConnResult<()> {
            self.1.lock().unwrap().push(outcome.to_string());
            Ok(())
        }
    }

    #[test]
    fn an_approval_can_be_answered_from_a_chat_app() {
        let d = tempfile::tempdir().unwrap();
        let handoff = d.path().join("h.json");
        std::fs::write(
            &handoff,
            json!({"action": "fix", "branch": "loop/issue-7", "target_branch": "main", "title": "Fix #7"}).to_string(),
        )
        .unwrap();
        let result = LoopResult::new("l", "r", "gitlab-issue", LoopState::Completed, vec![], "");
        let project = json!({"project_id": "g/p", "local_path": "/x", "instance": "g"});
        let notes = Notes(Default::default());
        let opener = |_: &str, _: &str, _: &str, _: &str| {
            (
                true,
                "remote:   https://gl.example/g/p/-/merge_requests/1489\n".to_string(),
            )
        };
        let ctx = RunCtx {
            repo_root: d.path().into(),
            events_dir: Some(d.path().join("events")),
            results_dir: None,
            unified_log: d.path().join("log"),
            exe: "loopx".into(),
            config_path: None,
            fakes: None,
            memory_root: None,
            activity_file: d.path().join("agent-activity.json"),
            stall_after: crate::agent_activity::STALL_AFTER,
        };
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let chat: Box<dyn crate::connectors::Connector> = Box::new(AllowingChat(
            crate::connectors::Account::new("phone", "telegram"),
            seen.clone(),
        ));
        let approvals_dir = d.path().join("approvals");
        let outcome = finalize_gated_issue(
            &result,
            "web",
            "7",
            "r",
            &ctx,
            FinalizeDeps {
                opener: Some(&opener),
                gitlab: Some(&notes),
                notifier: Some(&|_: &str| {}),
                handoff: Some(handoff),
                project: Some(project),
                gitlab_config_path: Some(d.path().join("none.json")),
                require_approval: Some(true),
                approval_dir: Some(approvals_dir.clone()),
                approval_poll: Some(std::time::Duration::from_millis(5)),
                approval_channels: Some(vec![chat]),
                ..Default::default()
            },
        );
        assert_eq!(outcome, "mr_opened");
        let decided = approvals::list(&approvals_dir, None, Utc::now());
        assert_eq!(decided[0]["decided_by"], "telegram:@ana");
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "asked \"web\"",
                "Approved by telegram:@ana: opening the merge request."
            ]
        );
    }

    /// Finalize a passed gated fix for `web #7` from `handoff`, with a push whose
    /// output names MR !1489; the GitLab calls it made.
    fn finalize_fix(handoff: Value) -> Vec<(String, String, Value)> {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("h.json");
        std::fs::write(&path, handoff.to_string()).unwrap();
        let evidence = json!({"commands": [{"kind": "test", "command": "pnpm test", "passed": true, "source": "handoff"}]});
        let result = LoopResult::new(
            "l",
            "r",
            "gitlab-issue",
            LoopState::Completed,
            vec![iteration(vec![v("project_commands", true, "", evidence)])],
            "",
        );
        let notes = Notes(Default::default());
        let opener = |_: &str, _: &str, _: &str, _: &str| {
            (
                true,
                "remote: View merge request for loop/issue-7:\nremote:   https://gl.example/g/p/-/merge_requests/1489\n".to_string(),
            )
        };
        let ctx = RunCtx {
            repo_root: d.path().into(),
            events_dir: Some(d.path().join("events")),
            results_dir: None,
            unified_log: d.path().join("log"),
            exe: "loopx".into(),
            config_path: None,
            fakes: None,
            memory_root: None,
            activity_file: d.path().join("agent-activity.json"),
            stall_after: crate::agent_activity::STALL_AFTER,
        };
        let notifier = |_: &str| {};
        let outcome = finalize_gated_issue(
            &result,
            "web",
            "7",
            "r",
            &ctx,
            FinalizeDeps {
                opener: Some(&opener),
                gitlab: Some(&notes),
                notifier: Some(&notifier),
                handoff: Some(path),
                project: Some(json!({"project_id": "g/p", "local_path": "/x", "instance": "g"})),
                gitlab_config_path: Some(d.path().join("none.json")),
                require_approval: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(outcome, "mr_opened");
        notes.0.into_inner().unwrap()
    }

    fn mr_description(calls: &[(String, String, Value)]) -> String {
        let put = calls
            .iter()
            .find(|(m, e, _)| m == "PUT" && e == "projects/g%2Fp/merge_requests/1489")
            .unwrap_or_else(|| panic!("no description update in {calls:?}"));
        put.2["description"].as_str().unwrap().to_string()
    }

    #[test]
    fn the_opened_mr_gets_the_agents_description_closing_the_issue() {
        let description = "## What and why\n\n- Problem: p\n- Fix: f\n- Result: r\n\n## How\n\n- h";
        let calls = finalize_fix(
            json!({"action": "fix", "branch": "loop/issue-7", "target_branch": "main",
            "title": "fix: #7 x", "summary": "S.", "description": description}),
        );
        let body = mr_description(&calls);
        assert!(body.starts_with("Closes #7\n\n## What and why"), "{body}");
        assert!(body.ends_with("- h"), "{body}");
    }

    /// What a finalize with screenshots produced.
    struct Shots {
        dir: tempfile::TempDir,
        outcome: String,
        calls: Vec<(String, String, Value)>,
        uploads: Vec<String>,
    }

    struct Uploads {
        ok: bool,
        seen: std::sync::Mutex<Vec<String>>,
        notes: Notes,
    }

    impl GitlabCalls for Uploads {
        fn call(&self, method: &str, endpoint: &str, body: &Value) -> Result<(), String> {
            self.notes.call(method, endpoint, body)
        }
        fn upload(&self, _project_id: &str, path: &Path) -> Result<String, String> {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            self.seen.lock().unwrap().push(name.clone());
            // A file named "unuploadable" fails even when the rest go through.
            if self.ok && !name.contains("unuploadable") {
                Ok(format!("/uploads/h/{name}"))
            } else {
                Err("boom".into())
            }
        }
    }

    fn write_png(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&[1, 2, 3]);
        std::fs::write(path, bytes).unwrap();
    }

    /// Finalize `web #7` with `handoff` (written to `<dir>/handoff/h.json`),
    /// after `setup(handoff_dir, screens_dir)` created the artefact files.
    fn finalize_shots(
        handoff: Value,
        uploads_ok: bool,
        final_state: LoopState,
        setup: impl Fn(&Path, &Path),
    ) -> Shots {
        finalize_shots_with(handoff, uploads_ok, final_state, setup, None, None)
    }

    fn finalize_shots_with(
        handoff: Value,
        uploads_ok: bool,
        final_state: LoopState,
        setup: impl Fn(&Path, &Path),
        checkout_problem: Option<String>,
        project: Option<Value>,
    ) -> Shots {
        let d = tempfile::tempdir().unwrap();
        let handoff_dir = d.path().join("handoff");
        let screens = crate::mr_screenshots::screens_dir(d.path(), "web", "7");
        std::fs::create_dir_all(&handoff_dir).unwrap();
        setup(&handoff_dir, &screens);
        let path = handoff_dir.join("h.json");
        std::fs::write(&path, handoff.to_string()).unwrap();
        let result = LoopResult::new(
            "l",
            "r",
            "gitlab-issue",
            final_state,
            vec![iteration(vec![v("project_commands", true, "", json!({}))])],
            "",
        );
        let gl = Uploads {
            ok: uploads_ok,
            seen: Default::default(),
            notes: Notes(Default::default()),
        };
        let opener = |_: &str, _: &str, _: &str, _: &str| {
            (
                true,
                "remote:   https://gl.example/g/p/-/merge_requests/1489\n".to_string(),
            )
        };
        let ctx = RunCtx {
            repo_root: d.path().into(),
            events_dir: Some(d.path().join("events")),
            results_dir: None,
            unified_log: d.path().join("log"),
            exe: "loopx".into(),
            config_path: None,
            fakes: None,
            memory_root: None,
            activity_file: d.path().join("agent-activity.json"),
            stall_after: crate::agent_activity::STALL_AFTER,
        };
        let notifier = |_: &str| {};
        let outcome = finalize_gated_issue(
            &result,
            "web",
            "7",
            "r",
            &ctx,
            FinalizeDeps {
                opener: Some(&opener),
                gitlab: Some(&gl),
                notifier: Some(&notifier),
                handoff: Some(path),
                project: Some(project.unwrap_or_else(
                    || json!({"project_id": "g/p", "local_path": "/x", "instance": "g"}),
                )),
                checkout_problem,
                gitlab_config_path: Some(d.path().join("none.json")),
                require_approval: Some(false),
                ..Default::default()
            },
        );
        Shots {
            outcome,
            calls: gl.notes.0.into_inner().unwrap(),
            uploads: gl.seen.into_inner().unwrap(),
            dir: d,
        }
    }

    fn fix_with_shots(extra: Value) -> Value {
        let mut h = json!({"action": "fix", "branch": "loop/issue-7", "target_branch": "main",
            "title": "fix: #7 x", "summary": "S."});
        for (k, v) in extra.as_object().unwrap() {
            h[k] = v.clone();
        }
        h
    }

    fn make_pair_files(handoff: &Path, screens: &Path) {
        write_png(&handoff.join("1-before.png"));
        write_png(&screens.join("1-after.png"));
        std::fs::write(screens.join("mocks.json"), "{}").unwrap();
        std::fs::write(handoff.join("session.json"), "{}").unwrap();
    }

    fn one_pair() -> Value {
        json!({"screenshots": [{"label": "Home", "before": "1-before.png", "after": "1-after.png"}]})
    }

    #[test]
    fn gated_fix_uploads_screenshots_into_the_description() {
        let s = finalize_shots(
            fix_with_shots(one_pair()),
            true,
            LoopState::Completed,
            make_pair_files,
        );
        assert_eq!(s.outcome, "mr_opened");
        assert_eq!(s.uploads.len(), 2, "{:?}", s.uploads);
        let body = mr_description(&s.calls);
        assert!(body.contains("## Screenshots"), "{body}");
        assert!(body.contains("/uploads/h/1-before.png"), "{body}");
        assert!(body.contains("/uploads/h/1-after.png"), "{body}");
    }

    #[test]
    fn failed_uploads_leave_a_note_and_still_open() {
        let s = finalize_shots(
            fix_with_shots(one_pair()),
            false,
            LoopState::Completed,
            make_pair_files,
        );
        assert_eq!(s.outcome, "mr_opened");
        let body = mr_description(&s.calls);
        assert!(body.contains("## Screenshots"), "{body}");
        assert!(
            body.ends_with("Some screenshots were left out: \"Home\" dropped: upload failed."),
            "{body}"
        );
    }

    #[test]
    fn left_out_shots_and_the_agents_note_are_separate_sentences() {
        let handoff = fix_with_shots(json!({
            "screenshots": [
                {"label": "Home", "before": "1-before.png", "after": "1-after.png"},
                {"label": "Gone", "after": "missing.png"}
            ],
            "screenshots_note": "The settings page needs a real account"
        }));
        let s = finalize_shots(handoff, true, LoopState::Completed, make_pair_files);
        let body = mr_description(&s.calls);
        assert!(
            body.ends_with(
                "Some screenshots were left out: \"Gone\" dropped: missing.png not found. \
                 The settings page needs a real account."
            ),
            "{body}"
        );
        assert!(!body.contains("could not be uploaded"), "{body}");
    }

    #[test]
    fn a_before_that_fails_to_upload_is_not_shown_as_a_new_page() {
        let handoff = fix_with_shots(json!({"screenshots": [
            {"label": "Home", "before": "1-unuploadable.png", "after": "1-after.png"}
        ]}));
        let s = finalize_shots(handoff, true, LoopState::Completed, |h, sc| {
            write_png(&h.join("1-unuploadable.png"));
            write_png(&sc.join("1-after.png"));
        });
        assert_eq!(s.outcome, "mr_opened");
        let body = mr_description(&s.calls);
        assert!(
            body.contains(
                "| Before image could not be uploaded | ![after](/uploads/h/1-after.png) |"
            ),
            "{body}"
        );
        assert!(!body.contains("New page"), "{body}");
    }

    #[test]
    fn screenshots_note_without_pairs_is_shown() {
        let s = finalize_shots(
            fix_with_shots(json!({"screenshots_note": "No browser-test setup"})),
            true,
            LoopState::Completed,
            |_, _| {},
        );
        assert_eq!(s.outcome, "mr_opened");
        let body = mr_description(&s.calls);
        assert!(body.contains("## Screenshots"), "{body}");
        assert!(body.contains("No browser-test setup"), "{body}");
    }

    #[test]
    fn no_screenshot_fields_leaves_the_description_unchanged() {
        let s = finalize_shots(
            fix_with_shots(json!({})),
            true,
            LoopState::Completed,
            |_, _| {},
        );
        let body = mr_description(&s.calls);
        assert!(!body.contains("Screenshots"), "{body}");
        assert!(body.ends_with("## Verified"), "{body:?}");
    }

    #[test]
    fn screenshot_files_are_deleted_after_finalize_whatever_the_outcome() {
        let cases = [
            (fix_with_shots(one_pair()), true),
            (fix_with_shots(one_pair()), false),
            (
                json!({"action": "escalate", "reason": "stuck", "screenshots":
                    [{"label": "Home", "before": "1-before.png", "after": "1-after.png"}]}),
                true,
            ),
        ];
        for (handoff, ok) in cases {
            let s = finalize_shots(handoff, ok, LoopState::Completed, make_pair_files);
            let handoff_dir = s.dir.path().join("handoff");
            let screens = crate::mr_screenshots::screens_dir(s.dir.path(), "web", "7");
            assert!(!screens.exists(), "{}", s.outcome);
            assert!(!handoff_dir.join("1-before.png").exists(), "{}", s.outcome);
            assert!(handoff_dir.join("h.json").exists(), "{}", s.outcome);
        }
    }

    #[test]
    fn screenshot_files_are_deleted_on_early_escalations_too() {
        let shots = json!({"screenshots":
            [{"label": "Home", "before": "1-before.png", "after": "1-after.png"}]});
        // A handoff read_handoff rejects (no title/branch) still names images.
        let mut invalid =
            json!({"action": "fix", "branch": "wrong", "screenshots": shots["screenshots"]});
        invalid["target_branch"] = json!("main");
        let cases = [
            (invalid, None, None, "escalated:handoff_invalid"),
            (
                fix_with_shots(shots.clone()),
                Some("dirty".to_string()),
                None,
                "escalated:checkout_problem",
            ),
            (
                fix_with_shots(shots.clone()),
                None,
                Some(json!({"local_path": "/x"})),
                "escalated:project_config_error",
            ),
        ];
        for (handoff, problem, project, want) in cases {
            let s = finalize_shots_with(
                handoff,
                true,
                LoopState::Completed,
                make_pair_files,
                problem,
                project,
            );
            assert_eq!(s.outcome, want);
            let handoff_dir = s.dir.path().join("handoff");
            let screens = crate::mr_screenshots::screens_dir(s.dir.path(), "web", "7");
            assert!(!screens.exists(), "{want}");
            assert!(!handoff_dir.join("1-before.png").exists(), "{want}");
        }
    }

    #[test]
    fn the_issue_comment_is_goal_based_and_leaves_commands_to_the_mr() {
        let summary = "**Goal:** users can add a missing company by its domain.\n- Duplicates return the existing company";
        let calls = finalize_fix(
            json!({"action": "fix", "branch": "loop/issue-7", "target_branch": "main",
            "title": "fix: #7 x", "summary": summary}),
        );
        let note = calls
            .iter()
            .find(|(m, e, _)| m == "POST" && e.ends_with("/notes"))
            .unwrap_or_else(|| panic!("no issue comment in {calls:?}"));
        let body = note.2["body"].as_str().unwrap();
        assert_eq!(
            body,
            "**Merge request ready for review:** !1489 \u{2192} `main`\n\n\
             **Goal:** users can add a missing company by its domain.\n\
             - Duplicates return the existing company\n\
             - Checks: 1/1 passed \u{2705} (commands in the MR)"
        );
    }

    #[test]
    fn an_mr_without_an_agent_description_still_gets_one_from_the_summary_and_checks() {
        let calls = finalize_fix(
            json!({"action": "fix", "branch": "loop/issue-7", "target_branch": "main",
            "title": "fix: #7 x", "summary": "Fixed the thing."}),
        );
        let body = mr_description(&calls);
        assert!(
            body.starts_with("## What and why\n\nCloses #7\n\nFixed the thing."),
            "{body}"
        );
        assert!(
            body.contains("## Verified") && body.contains("- `pnpm test` \u{2705}"),
            "{body}"
        );
    }

    /// A worktree-off project `web` whose checkout (a clone of a bare origin
    /// with one commit on `main`) is on `feature-x`, and a run context whose
    /// GitLab calls and Slack messages are recorded, never sent.
    type Log = Arc<Mutex<Vec<String>>>;

    struct InPlace {
        d: tempfile::TempDir,
        local: PathBuf,
        ctx: RunCtx,
        notes: Log,
        slack: Log,
    }

    struct RecordingGitlab(Log);
    impl GitlabCalls for RecordingGitlab {
        fn call(&self, _method: &str, _endpoint: &str, body: &Value) -> Result<(), String> {
            if let Some(b) = body.get("body").and_then(Value::as_str) {
                self.0.lock().unwrap().push(b.to_string());
            }
            Ok(())
        }
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
            ])
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    impl InPlace {
        fn new() -> Self {
            let d = tempfile::tempdir().unwrap();
            let origin = d.path().join("origin.git");
            std::fs::create_dir(&origin).unwrap();
            sh(&origin, &["init", "-q", "--bare", "-b", "main"]);
            sh(
                d.path(),
                &["clone", "-q", &origin.display().to_string(), "local"],
            );
            let local = d.path().join("local");
            sh(&local, &["checkout", "-q", "-b", "main"]);
            std::fs::write(local.join("a.txt"), "1").unwrap();
            sh(&local, &["add", "."]);
            sh(&local, &["commit", "-qm", "init"]);
            sh(&local, &["push", "-q", "origin", "main"]);
            sh(&local, &["checkout", "-q", "-b", "feature-x"]);
            let config = d.path().join("projects.json");
            let (notes, slack): (Log, Log) = Default::default();
            let slack_sink = slack.clone();
            let ctx = RunCtx {
                repo_root: d.path().into(),
                events_dir: Some(d.path().join("events")),
                results_dir: Some(d.path().join("results")),
                unified_log: d.path().join("log"),
                exe: "loopx".into(),
                config_path: Some(config),
                fakes: Some(Arc::new(RunFakes {
                    gitlab: Box::new(RecordingGitlab(notes.clone())),
                    notifier: Box::new(move |m: &str| {
                        slack_sink.lock().unwrap().push(m.to_string())
                    }),
                    gitlab_config_path: d.path().join("gitlab.json"),
                })),
                memory_root: Some(d.path().join("memory")),
                activity_file: d.path().join("agent-activity.json"),
                stall_after: crate::agent_activity::STALL_AFTER,
            };
            let me = Self {
                d,
                local,
                ctx,
                notes,
                slack,
            };
            me.set_use_worktree(false);
            me
        }
        fn set_use_worktree(&self, on: bool) {
            std::fs::write(
                self.ctx.config_path.as_ref().unwrap(),
                json!({"gitlab_instance": "g", "worktree_root": self.d.path().join("wt").display().to_string(),
                       "projects": {"web": {"project_id": "g/web", "local_path": self.local.display().to_string(),
                                            "target_branch": "main", "use_worktree": on}}})
                .to_string(),
            )
            .unwrap();
        }
        fn run(&self, invoker: Invoker) {
            let definition = LoopDefinition::from_yaml(
                &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../loops/gitlab-issue/loop.yaml"),
            )
            .unwrap();
            let _ = run_one_issue(
                &self.ctx,
                "r1",
                "web",
                "7",
                &definition,
                invoker,
                InvokeMode::Batch,
            );
        }
        fn head(&self) -> String {
            sh(&self.local, &["rev-parse", "--abbrev-ref", "HEAD"])
        }
        fn path(&self) -> String {
            self.local.display().to_string()
        }
    }

    fn failing() -> Result<AgentOutput, AgentError> {
        Err(AgentError {
            message: "boom".into(),
            cost_usd: None,
        })
    }

    #[test]
    fn screenshots_are_deleted_when_the_runtime_fails_before_finalize() {
        let t = InPlace::new();
        let root = t.d.path();
        let screens = crate::mr_screenshots::screens_dir(root, "web", "7");
        let handoff = handoff_path("r1", "web", "7", root);
        write_png(&screens.join("1-before.png"));
        write_png(&handoff.with_file_name("1-after.png"));
        std::fs::write(
            &handoff,
            json!({"action": "fix", "screenshots": [{"label": "x", "after": "1-after.png"}]})
                .to_string(),
        )
        .unwrap();
        // An L3 action outside human_gates makes LoopRuntime::start refuse.
        let mut definition = LoopDefinition::from_yaml(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../loops/gitlab-issue/loop.yaml"),
        )
        .unwrap();
        definition.actions.push("drop_everything".into());
        let r = run_one_issue(
            &t.ctx,
            "r1",
            "web",
            "7",
            &definition,
            Arc::new(|_req| failing()),
            InvokeMode::Batch,
        );
        assert!(r.is_err());
        assert!(!screens.exists());
        assert!(!handoff.with_file_name("1-after.png").exists());
    }

    /// Runs `iids` of project "web" through the batch, `max` at once; returns
    /// the peak number of agents that ran at once and which issues the agent
    /// was started for.
    fn batch_peak(t: &InPlace, iids: &[i64], max: usize) -> (usize, Vec<String>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let definition = LoopDefinition::from_yaml(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../loops/gitlab-issue/loop.yaml"),
        )
        .unwrap();
        let cfg = Config::load(t.ctx.config_path.as_ref().unwrap()).unwrap();
        let (now, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let started: Log = Default::default();
        let (n, p, st) = (now.clone(), peak.clone(), started.clone());
        let invoker: Invoker = Arc::new(move |req| {
            st.lock().unwrap().push(req.issue_iid.clone());
            p.fetch_max(n.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(150));
            n.fetch_sub(1, Ordering::SeqCst);
            failing()
        });
        let todo = iids
            .iter()
            .map(|i| ("web".to_string(), json!({"iid": i, "title": "t"})))
            .collect();
        let _ = run_issue_batch(&t.ctx, "r1", &cfg, todo, &definition, invoker, max);
        let mut started = started.lock().unwrap().clone();
        started.sort();
        started.dedup();
        (peak.load(Ordering::SeqCst), started)
    }

    #[test]
    fn a_batch_works_on_several_worktree_issues_at_once() {
        let t = InPlace::new();
        t.set_use_worktree(true);
        let (peak, started) = batch_peak(&t, &[1, 2, 3, 4], 3);
        assert_eq!(peak, 3);
        assert_eq!(started, ["1", "2", "3", "4"]);
    }

    #[test]
    fn issues_sharing_one_in_place_checkout_never_run_at_once() {
        let t = InPlace::new();
        let (peak, started) = batch_peak(&t, &[1, 2, 3], 3);
        assert_eq!(peak, 1);
        assert_eq!(started, ["1", "2", "3"]);
    }

    #[test]
    fn an_issue_another_run_is_working_on_is_skipped() {
        let t = InPlace::new();
        t.set_use_worktree(true);
        let lock = crate::file_lock::issue_lock_path(&t.ctx.repo_root, "web", "2");
        let _other = crate::file_lock::FileLock::try_exclusive(&lock)
            .unwrap()
            .unwrap();
        let (_, started) = batch_peak(&t, &[1, 2, 3], 3);
        assert_eq!(started, ["1", "3"]);
        let log = std::fs::read_to_string(&t.ctx.unified_log).unwrap();
        assert!(
            log.contains("web #2 skipped: another run is already working on it"),
            "{log}"
        );
    }

    #[test]
    fn a_single_issue_run_refuses_an_issue_already_being_worked_on() {
        let t = InPlace::new();
        let lock = crate::file_lock::issue_lock_path(&t.ctx.repo_root, "web", "7");
        let _other = crate::file_lock::FileLock::try_exclusive(&lock)
            .unwrap()
            .unwrap();
        let mut ctx = RunCtx::new(t.ctx.repo_root.clone());
        ctx.unified_log = t.ctx.unified_log.clone();
        std::fs::create_dir_all(ctx.definition_path().parent().unwrap()).unwrap();
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../loops/gitlab-issue/loop.yaml"),
            ctx.definition_path(),
        )
        .unwrap();
        let called = Arc::new(Mutex::new(false));
        let c = called.clone();
        let e = run_single_issue(
            &ctx,
            "r1",
            "web",
            "7",
            Arc::new(move |_| {
                *c.lock().unwrap() = true;
                failing()
            }),
        )
        .err()
        .expect("refused");
        assert!(e.contains("already being worked on"), "{e}");
        assert!(!*called.lock().unwrap());
    }

    #[test]
    fn restores_checkout_after_agent_error() {
        let t = InPlace::new();
        let local = t.path();
        t.run(Arc::new(move |_req| {
            let _ = crate::worktree::prepare_in_place(&local, "main", "7");
            failing()
        }));
        assert_eq!(t.head(), "feature-x");
        assert!(
            t.slack.lock().unwrap().is_empty(),
            "a batch issue's escalation waits for the run's report"
        );
    }

    #[test]
    fn restores_checkout_even_if_worktree_is_switched_on_mid_run() {
        let t = InPlace::new();
        let local = t.path();
        let config = t.ctx.config_path.clone().unwrap();
        t.run(Arc::new(move |_req| {
            let _ = crate::worktree::prepare_in_place(&local, "main", "7");
            let text = std::fs::read_to_string(&config)
                .unwrap()
                .replace("\"use_worktree\":false", "\"use_worktree\":true");
            std::fs::write(&config, text).unwrap();
            failing()
        }));
        assert_eq!(t.head(), "feature-x");
    }

    #[test]
    fn dirty_checkout_is_refused_before_the_agent_runs() {
        let t = InPlace::new();
        std::fs::write(t.local.join("mine.txt"), "user work").unwrap();
        let calls = Arc::new(Mutex::new(0));
        let counter = calls.clone();
        t.run(Arc::new(move |_req| {
            *counter.lock().unwrap() += 1;
            failing()
        }));
        assert_eq!(*calls.lock().unwrap(), 0, "agent never started");
        assert_eq!(t.head(), "feature-x");
        assert_eq!(
            std::fs::read_to_string(t.local.join("mine.txt")).unwrap(),
            "user work"
        );
        let notes = t.notes.lock().unwrap();
        assert!(notes.iter().any(|n| n.contains("uncommitted")), "{notes:?}");
    }

    #[test]
    fn agent_changing_the_checkout_without_branching_escalates() {
        let t = InPlace::new();
        let local = t.path();
        t.run(Arc::new(move |_req| {
            std::fs::write(
                Path::new(&local).join("a.txt"),
                "edited on the user's branch",
            )
            .unwrap();
            failing()
        }));
        let notes = t.notes.lock().unwrap();
        assert!(
            notes.iter().any(|n| n.contains("loopx worktree new")),
            "{notes:?}"
        );
    }

    /// A worktree-on `web` (an `InPlace` checkout plus its issue worktree on
    /// `loop/issue-7`, with a passing `make test`), whose fake invoker plays
    /// the main agent and the setup doctor from scripts and records each call.
    struct Doctored {
        t: InPlace,
        wt: PathBuf,
        calls: Arc<Mutex<Vec<InvokeRequest>>>,
    }

    /// One scripted agent call: what it writes, and the cost it reports.
    #[derive(Clone)]
    enum Step {
        /// The main agent writes this handoff.
        Main(Value),
        /// The main agent writes this handoff and leaves the listed
        /// untracked files in the worktree.
        MainLeaving(Value, Vec<&'static str>),
        /// The doctor writes this result (none when `None`), a SETUP.md
        /// draft, and leaves the listed untracked files in the worktree.
        Doctor(Option<Value>, Vec<&'static str>),
        /// The doctor crashes before writing anything.
        DoctorCrash,
    }

    const DRAFT: &str = "# web setup\n\n## Worktree prep\n\n- make generate\n";

    fn blocked(error: &str) -> Value {
        json!({"action": "setup_blocked", "command": "make test", "cwd": "", "error": error,
               "diagnosis": "the generated module is missing"})
    }

    impl Doctored {
        fn new() -> Self {
            let t = InPlace::new();
            // The bare origin accepts the MR push options the opener sends.
            sh(
                &t.d.path().join("origin.git"),
                &["config", "receive.advertisePushOptions", "true"],
            );
            let wt = t.d.path().join("wt").join("local-issue-7");
            sh(
                &t.local,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "loop/issue-7",
                    &wt.display().to_string(),
                    "main",
                ],
            );
            std::fs::write(wt.join("Makefile"), "test:\n\t@true\n").unwrap();
            sh(&wt, &["add", "Makefile"]);
            sh(&wt, &["commit", "-qm", "WIP #7: x"]);
            let me = Self {
                t,
                wt,
                calls: Arc::default(),
            };
            me.configure(true);
            me
        }

        fn configure(&self, use_worktree: bool) {
            self.configure_with(use_worktree, false);
        }

        fn configure_with(&self, use_worktree: bool, require_approval: bool) {
            std::fs::write(
                self.t.ctx.config_path.as_ref().unwrap(),
                json!({"gitlab_instance": "g", "worktree_root": self.t.d.path().join("wt").display().to_string(),
                       "projects": {"web": {"project_id": "g/web", "local_path": self.t.path(),
                                            "target_branch": "main", "use_worktree": use_worktree,
                                            "require_approval": require_approval}}})
                .to_string(),
            )
            .unwrap();
        }

        /// A reviewer in another thread answering each approval request in turn.
        fn reviewer(
            &self,
            answers: Vec<(&'static str, Option<&'static str>)>,
        ) -> std::thread::JoinHandle<()> {
            let dir = self.t.d.path().join("outputs").join("approvals");
            std::thread::spawn(move || {
                for (decision, notes) in answers {
                    loop {
                        let pending = approvals::list(&dir, Some("pending"), Utc::now());
                        if let Some(a) = pending.first() {
                            let id = a["id"].as_str().unwrap();
                            approvals::decide_with_notes(
                                &dir,
                                id,
                                decision,
                                notes,
                                "test",
                                Utc::now(),
                            )
                            .unwrap();
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
            })
        }

        fn fix(&self) -> Value {
            json!({"action": "fix", "branch": "loop/issue-7", "target_branch": "main",
                   "title": "fix: #7 x", "summary": "**Goal:** x",
                   "verification": {"test": "make test", "cwd": self.wt.display().to_string()}})
        }

        fn run_with(&self, steps: Vec<Step>, definition: LoopDefinition) -> IssueRun {
            let root = self.t.d.path().to_path_buf();
            let handoff = handoff_path("r1", "web", "7", &root);
            let (wt, calls) = (self.wt.clone(), self.calls.clone());
            let steps = Arc::new(Mutex::new(std::collections::VecDeque::from(steps)));
            let invoker: Invoker = Arc::new(move |req| {
                calls.lock().unwrap().push(req.clone());
                let step = steps
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("an unscripted call");
                assert_eq!(
                    req.doctor.is_some(),
                    !matches!(step, Step::Main(_) | Step::MainLeaving(..)),
                    "call kind"
                );
                std::fs::create_dir_all(handoff.parent().unwrap()).unwrap();
                let cost = match step {
                    Step::Main(h) => {
                        std::fs::write(&handoff, h.to_string()).unwrap();
                        0.5
                    }
                    Step::MainLeaving(h, junk) => {
                        std::fs::write(&handoff, h.to_string()).unwrap();
                        for f in junk {
                            std::fs::write(wt.join(f), "uncommitted").unwrap();
                        }
                        0.5
                    }
                    Step::Doctor(result, junk) => {
                        if let Some(r) = result {
                            std::fs::write(
                                crate::setup_doctor::doctor_result_path(&handoff),
                                r.to_string(),
                            )
                            .unwrap();
                        }
                        std::fs::write(crate::setup_doctor::setup_draft_path(&handoff), DRAFT)
                            .unwrap();
                        for f in junk {
                            std::fs::write(wt.join(f), "left behind").unwrap();
                        }
                        1.0
                    }
                    Step::DoctorCrash => {
                        return Err(AgentError {
                            message: "timed out after 1500s".into(),
                            cost_usd: Some(1.0),
                        })
                    }
                };
                Ok(AgentOutput {
                    cost_usd: Some(cost),
                    usage: None,
                })
            });
            run_one_issue(
                &self.t.ctx,
                "r1",
                "web",
                "7",
                &definition,
                invoker,
                InvokeMode::Batch,
            )
            .unwrap()
        }

        fn run(&self, steps: Vec<Step>) -> IssueRun {
            self.run_with(steps, gitlab_definition())
        }

        /// `m` for a main-agent call, `d` for a doctor call, in order.
        fn kinds(&self) -> String {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|r| if r.doctor.is_some() { 'd' } else { 'm' })
                .collect()
        }

        fn notes(&self) -> String {
            self.t.notes.lock().unwrap().join("\n---\n")
        }

        fn event_types(&self) -> Vec<String> {
            events::iter_events(self.t.ctx.events_dir.as_deref(), None, None)
                .iter()
                .filter_map(|e| e["event_type"].as_str().map(str::to_owned))
                .collect()
        }

        fn memory_root(&self) -> PathBuf {
            self.t.ctx.memory_root.clone().unwrap()
        }
    }

    fn gitlab_definition() -> LoopDefinition {
        LoopDefinition::from_yaml(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../loops/gitlab-issue/loop.yaml"),
        )
        .unwrap()
    }

    fn doctor_result(status: &str, blockers: Value) -> Option<Value> {
        Some(json!({"status": status, "blockers": blockers,
                    "learning": "run make generate before make test"}))
    }

    #[test]
    fn requested_changes_go_back_to_the_agent_and_the_mr_opens_once_approved() {
        let h = Doctored::new();
        h.configure_with(true, true);
        let reviewer = h.reviewer(vec![
            ("request_changes", Some("Rename the helper to page_rows.")),
            ("approve", None),
        ]);
        let run = h.run(vec![Step::Main(h.fix()), Step::Main(h.fix())]);
        reviewer.join().unwrap();
        assert_eq!(h.kinds(), "mm");
        let calls = h.calls.lock().unwrap();
        let feedback = calls[1].feedback.as_deref().unwrap_or("");
        assert!(feedback.contains("asked for changes"), "{feedback}");
        assert!(
            feedback.contains("> Rename the helper to page_rows."),
            "{feedback}"
        );
        assert!(feedback.contains("loop/issue-7"), "{feedback}");
        drop(calls);
        assert_eq!(
            run.gate_outcome.as_deref(),
            Some("mr_opened"),
            "{}",
            h.notes()
        );
        assert_eq!(run.result.iterations.len(), 2, "both passes' iterations");
        assert_eq!(run.result.agent_cost_usd, AgentCost::Usd(1.0));
        let decided = h
            .event_types()
            .iter()
            .filter(|t| *t == "approval.decided")
            .count();
        assert_eq!(decided, 2);
    }

    #[test]
    fn changes_requested_again_after_two_revisions_escalate_with_the_notes() {
        let h = Doctored::new();
        h.configure_with(true, true);
        let reviewer = h.reviewer(vec![
            ("request_changes", Some("First round.")),
            ("request_changes", Some("Second round.")),
            (
                "request_changes",
                Some("Still not right: keep the old name."),
            ),
        ]);
        let run = h.run(vec![
            Step::Main(h.fix()),
            Step::Main(h.fix()),
            Step::Main(h.fix()),
        ]);
        reviewer.join().unwrap();
        assert_eq!(h.kinds(), "mmm");
        assert_eq!(
            run.gate_outcome.as_deref(),
            Some("escalated:approval_changes_requested")
        );
        let notes = h.notes();
        assert!(
            notes.contains("Still not right: keep the old name."),
            "{notes}"
        );
    }

    #[test]
    fn setup_blocked_runs_the_doctor_then_a_second_pass() {
        let h = Doctored::new();
        let fixed = json!([{"error": "No module named generated", "cause": "generated module missing",
                            "tried": ["make generate"], "result": "fixed"}]);
        let run = h.run(vec![
            Step::Main(blocked("No module named generated")),
            Step::Doctor(doctor_result("fixed", fixed), vec![]),
            Step::Main(h.fix()),
        ]);
        assert_eq!(h.kinds(), "mdm");
        let calls = h.calls.lock().unwrap();
        let doctor = calls[1].doctor.as_ref().unwrap();
        assert_eq!(doctor.work_dir, h.wt.display().to_string());
        assert_eq!(doctor.blocked["error"], "No module named generated");
        assert_eq!(doctor.max_attempts, 3);
        assert!(!doctor.has_knowledge);
        assert_eq!(calls[1].max_budget_usd, Some(2.0));
        assert_eq!(calls[1].timeout_seconds, 1500.0);
        let feedback = calls[2].feedback.as_deref().unwrap_or("");
        assert!(
            feedback.contains("Setup was fixed by the setup doctor"),
            "{feedback}"
        );
        drop(calls);
        assert_eq!(
            crate::memory_store::read_setup_knowledge("web", Some(&h.memory_root())).as_deref(),
            Some(DRAFT)
        );
        let lesson = crate::memory_store::get_task_memory("web", 7, Some(&h.memory_root()))
            .expect("the doctor's learning is recorded");
        assert!(lesson.body.contains("make generate"), "{}", lesson.body);
        assert_eq!(
            run.gate_outcome.as_deref(),
            Some("mr_opened"),
            "{}",
            h.notes()
        );
        assert_eq!(run.result.final_state, LoopState::Completed);
        assert_eq!(run.result.iterations.len(), 2, "both passes' iterations");
        assert_eq!(run.result.agent_cost_usd, AgentCost::Usd(2.0));
        let types = h.event_types();
        for t in [
            "setup.blocked",
            "setup.doctor_started",
            "setup.doctor_finished",
        ] {
            assert!(types.iter().any(|e| e == t), "{t} missing from {types:?}");
        }
        let finished = events::iter_events(h.t.ctx.events_dir.as_deref(), None, None)
            .into_iter()
            .find(|e| e["event_type"] == "setup.doctor_finished")
            .unwrap();
        assert_eq!(finished["data"]["status"], "fixed");
        assert_eq!(finished["data"]["blockers"], 1);
        assert_eq!(finished["data"]["fixed"], 1);
        assert_eq!(finished["data"]["cost_usd"], 1.0);
        assert_eq!(run.setup_fixed, Some(1));
        // Pass 2's verification events follow pass 1's iterations.
        let iterations: Vec<i64> = events::iter_events(h.t.ctx.events_dir.as_deref(), None, None)
            .iter()
            .filter(|e| e["event_type"] == "verification.external_completed")
            .filter_map(|e| e["data"]["iteration"].as_i64())
            .collect();
        assert_eq!(iterations, [2]);
    }

    #[test]
    fn a_failed_second_pass_still_reports_what_was_spent() {
        let h = Doctored::new();
        let issue = IssueArgs {
            ctx: &h.t.ctx,
            run_id: "r1",
            alias: "web",
            issue_iid: "7",
            definition: &gitlab_definition(),
        };
        let e = second_pass_failed(&issue, &[Some(0.5), Some(1.0)], "policy denied".into());
        assert_eq!(e, "policy denied");
        let log = std::fs::read_to_string(&h.t.ctx.unified_log).unwrap();
        assert!(log.contains("$1.50"), "{log}");
        let ev = events::iter_events(h.t.ctx.events_dir.as_deref(), None, None)
            .into_iter()
            .find(|e| e["event_type"] == "setup.pass2_failed")
            .expect("a setup.pass2_failed event");
        assert_eq!(ev["data"]["error"], "policy denied");
        assert_eq!(ev["data"]["cost_usd"], 1.5);
    }

    #[test]
    fn the_run_report_notes_auto_fixed_setup() {
        let d = tempfile::tempdir().unwrap();
        let sent: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = sent.clone();
        let mut ctx = RunCtx::new(d.path().into());
        ctx.events_dir = Some(d.path().join("events"));
        ctx.config_path = Some(d.path().join("projects.json"));
        ctx.fakes = Some(Arc::new(RunFakes {
            gitlab: Box::new(Notes(Default::default())),
            notifier: Box::new(move |m: &str| sink.lock().unwrap().push(m.to_string())),
            gitlab_config_path: d.path().join("gitlab.json"),
        }));
        let emit = |kind: &str, data: Value| {
            ctx.emit(
                kind,
                Some("R"),
                EventFields {
                    issue_run_id: Some("R_web_7".into()),
                    project: Some("web".into()),
                    issue_iid: Some(7),
                    data: Some(data),
                },
            )
        };
        emit("issue.queued", json!({"title": "Fix login", "web_url": ""}));
        emit(
            "issue.completed",
            json!({"action": "fix", "mr_url": "https://g/web/-/merge_requests/3"}),
        );
        let run = IssueRun {
            result: LoopResult::new(
                "l",
                "R_web_7",
                "gitlab-issue",
                LoopState::Completed,
                vec![],
                "",
            ),
            usage: None,
            gate_outcome: Some("mr_opened".into()),
            setup_fixed: Some(1),
        };
        send_run_report(&ctx, "R", "Daily GitLab loop", &[run], 5.0, None);
        let sent = sent.lock().unwrap();
        assert!(
            sent[0].contains("Fix login (setup fixed automatically: 1 blocker)"),
            "{}",
            sent[0]
        );
    }

    #[test]
    fn the_run_report_skips_the_setup_note_on_escalations_and_zero() {
        for (kind, data, outcome, fixed) in [
            (
                "issue.escalated",
                json!({"reason": "setup_blocked"}),
                "escalated:setup_blocked",
                Some(2),
            ),
            (
                "issue.completed",
                json!({"action": "fix", "mr_url": "https://g/web/-/merge_requests/3"}),
                "mr_opened",
                Some(0),
            ),
        ] {
            let d = tempfile::tempdir().unwrap();
            let sent: Arc<Mutex<Vec<String>>> = Arc::default();
            let sink = sent.clone();
            let mut ctx = RunCtx::new(d.path().into());
            ctx.events_dir = Some(d.path().join("events"));
            ctx.config_path = Some(d.path().join("projects.json"));
            ctx.fakes = Some(Arc::new(RunFakes {
                gitlab: Box::new(Notes(Default::default())),
                notifier: Box::new(move |m: &str| sink.lock().unwrap().push(m.to_string())),
                gitlab_config_path: d.path().join("gitlab.json"),
            }));
            let emit = |kind: &str, data: Value| {
                ctx.emit(
                    kind,
                    Some("R"),
                    EventFields {
                        issue_run_id: Some("R_web_7".into()),
                        project: Some("web".into()),
                        issue_iid: Some(7),
                        data: Some(data),
                    },
                )
            };
            emit("issue.queued", json!({"title": "Fix login", "web_url": ""}));
            emit(kind, data);
            let run = IssueRun {
                result: LoopResult::new(
                    "l",
                    "R_web_7",
                    "gitlab-issue",
                    LoopState::Completed,
                    vec![],
                    "",
                ),
                usage: None,
                gate_outcome: Some(outcome.to_string()),
                setup_fixed: fixed,
            };
            send_run_report(&ctx, "R", "Daily GitLab loop", &[run], 5.0, None);
            let sent = sent.lock().unwrap();
            assert!(sent[0].contains("Fix login"), "{}", sent[0]);
            assert!(
                !sent[0].contains("setup fixed automatically"),
                "{}",
                sent[0]
            );
        }
    }

    #[test]
    fn doctor_without_result_escalates_setup_blocked() {
        for doctor in [Step::DoctorCrash, Step::Doctor(None, vec![])] {
            let h = Doctored::new();
            let run = h.run(vec![
                Step::Main(blocked("No module named generated")),
                doctor,
            ]);
            assert_eq!(h.kinds(), "md", "no second pass");
            assert_eq!(run.gate_outcome.as_deref(), Some("escalated:setup_blocked"));
            let notes = h.notes();
            assert!(notes.contains("did not finish"), "{notes}");
            assert!(notes.contains("No module named generated"), "{notes}");
            assert_eq!(run.result.agent_cost_usd, AgentCost::Usd(1.5));
        }
    }

    #[test]
    fn dirty_worktree_after_doctor_escalates() {
        let h = Doctored::new();
        let fixed =
            json!([{"error": "e", "cause": "c", "tried": ["make generate"], "result": "fixed"}]);
        let run = h.run(vec![
            Step::Main(blocked("No module named generated")),
            Step::Doctor(doctor_result("fixed", fixed), vec!["junk.txt"]),
        ]);
        assert_eq!(h.kinds(), "md", "no second pass on a dirty tree");
        assert_eq!(run.gate_outcome.as_deref(), Some("escalated:setup_blocked"));
        let notes = h.notes();
        assert!(notes.contains("junk.txt"), "{notes}");
        let finished = events::iter_events(h.t.ctx.events_dir.as_deref(), None, None)
            .into_iter()
            .find(|e| e["event_type"] == "setup.doctor_finished")
            .unwrap();
        assert_eq!(finished["data"]["status"], "needs_human");
    }

    #[test]
    fn uncommitted_work_before_the_doctor_escalates_without_it() {
        let h = Doctored::new();
        let run = h.run(vec![Step::MainLeaving(
            blocked("No module named generated"),
            vec!["half_done.rb"],
        )]);
        assert_eq!(h.kinds(), "m", "the doctor never sees a dirty worktree");
        assert_eq!(run.gate_outcome.as_deref(), Some("escalated:setup_blocked"));
        let notes = h.notes();
        assert!(
            notes.contains("The agent handed off setup_blocked without committing its work first, so the setup doctor did not run"),
            "{notes}"
        );
        assert!(notes.contains("half_done.rb"), "{notes}");
        assert!(notes.contains("No module named generated"), "{notes}");
        assert!(!h.event_types().iter().any(|e| e == "setup.doctor_started"));
    }

    #[test]
    fn a_missing_worktree_still_gets_the_doctor() {
        // `loopx worktree new` itself failed: there is nothing to be dirty.
        let h = Doctored::new();
        std::fs::remove_dir_all(&h.wt).unwrap();
        let blockers = json!([{"error": "submodules not initialised: sub", "cause": "no access",
                               "tried": ["git submodule update --init"], "result": "failed"}]);
        let mut wt_failed = blocked("submodules not initialised: sub");
        wt_failed["command"] = json!("loopx worktree new web 7");
        let run = h.run(vec![
            Step::Main(wt_failed),
            Step::Doctor(doctor_result("needs_human", blockers), vec![]),
        ]);
        assert_eq!(h.kinds(), "md");
        assert_eq!(run.gate_outcome.as_deref(), Some("escalated:setup_blocked"));
        assert!(!h.notes().contains("without committing"), "{}", h.notes());
    }

    #[test]
    fn second_setup_blocked_escalates_without_a_second_doctor() {
        let h = Doctored::new();
        let fixed = json!([{"error": "No module named generated", "cause": "generated module missing",
                            "tried": ["make generate"], "result": "fixed"}]);
        let run = h.run(vec![
            Step::Main(blocked("No module named generated")),
            Step::Doctor(doctor_result("fixed", fixed), vec![]),
            Step::Main(blocked("database does not exist")),
        ]);
        assert_eq!(h.kinds(), "mdm", "one doctor session per issue per run");
        assert_eq!(run.gate_outcome.as_deref(), Some("escalated:setup_blocked"));
        let notes = h.notes();
        assert!(notes.contains("generated module missing"), "{notes}");
        assert!(notes.contains("database does not exist"), "{notes}");
    }

    #[test]
    fn setup_blocked_without_doctor_escalates() {
        // Worktree off: the user's own checkout is never handed to the doctor.
        let h = Doctored::new();
        h.configure(false);
        let run = h.run(vec![Step::Main(blocked("No module named generated"))]);
        assert_eq!(h.kinds(), "m");
        assert_eq!(run.gate_outcome.as_deref(), Some("escalated:setup_blocked"));
        assert!(h.notes().contains("No module named generated"));
        assert_eq!(h.t.head(), "feature-x");

        // The doctor switched off.
        let h = Doctored::new();
        let mut definition = gitlab_definition();
        definition.setup_doctor.enabled = false;
        let run = h.run_with(
            vec![Step::Main(blocked("No module named generated"))],
            definition,
        );
        assert_eq!(h.kinds(), "m");
        assert_eq!(run.gate_outcome.as_deref(), Some("escalated:setup_blocked"));
        let notes = h.notes();
        assert!(notes.contains("No module named generated"), "{notes}");
        assert!(!notes.contains("did not finish"), "{notes}");
        assert!(h.event_types().iter().any(|e| e == "setup.blocked"));
        assert!(!h.event_types().iter().any(|e| e == "setup.doctor_started"));
    }

    #[test]
    fn doctor_gave_up_escalates_with_its_blockers() {
        let h = Doctored::new();
        let blockers = json!([
            {"error": "ruby 3.3.1 not installed", "cause": "missing Ruby", "tried": ["rbenv install 3.3.1"], "result": "fixed"},
            {"error": "FATAL: role web does not exist", "cause": "no test database role", "tried": ["createuser web", "createdb web_test"], "result": "failed"}
        ]);
        let run = h.run(vec![
            Step::Main(blocked("No module named generated")),
            Step::Doctor(doctor_result("gave_up", blockers), vec![]),
        ]);
        assert_eq!(h.kinds(), "md");
        assert_eq!(run.gate_outcome.as_deref(), Some("escalated:setup_blocked"));
        let notes = h.notes();
        for part in [
            "missing Ruby",
            "rbenv install 3.3.1",
            "no test database role",
            "createdb web_test",
        ] {
            assert!(notes.contains(part), "{part} missing from {notes}");
        }
    }

    #[test]
    fn gate_prompt_describes_screenshot_handoff_fields() {
        for part in [
            "\"screenshots\": [",
            "\"before\"",
            "\"after\"",
            "\"screenshots_note\"",
            "outputs/screens/",
        ] {
            assert!(GATE_OVERRIDE.contains(part), "{part}: {GATE_OVERRIDE}");
        }
    }

    #[test]
    fn gate_override_documents_setup_blocked() {
        for part in [
            "\"setup_blocked\"",
            "\"command\"",
            "\"diagnosis\"",
            "Worktree prep",
            "both required",
        ] {
            assert!(GATE_OVERRIDE.contains(part), "{part}: {GATE_OVERRIDE}");
        }
    }

    #[test]
    fn instructions_mention_setup_knowledge() {
        let md = include_str!("../../../instructions/loopx/gitlab-issue.md");
        for part in [
            "loopx memory-store setup <alias>",
            "timeout: 600000",
            "\"action\": \"setup_blocked\"",
            "WIP #<issue_iid>",
            "`command` and `error` are both required",
            "the WIP commit stays",
        ] {
            assert!(md.contains(part), "{part} missing from gitlab-issue.md");
        }
    }

    #[test]
    fn gate_prompt_asks_for_verification_block() {
        let d = tempfile::tempdir().unwrap();
        let (prompt, _) = gate_prompt_and_env("p", "web", "7", d.path(), Some("r")).unwrap();
        assert!(prompt.contains("\"verification\""), "{prompt}");
        assert!(prompt.contains("\"cwd\""), "{prompt}");
    }

    #[test]
    fn gate_prompt_asks_for_an_mr_description_in_the_create_mr_shape() {
        let d = tempfile::tempdir().unwrap();
        let (prompt, _) = gate_prompt_and_env("p", "web", "7", d.path(), Some("r")).unwrap();
        for part in [
            "\"description\"",
            "Closes #<iid>",
            "## What and why",
            "## How",
            "## Verified",
        ] {
            assert!(prompt.contains(part), "{part}: {prompt}");
        }
        assert!(prompt.contains("\"title\": \"<fix|feat"), "{prompt}");
    }

    #[test]
    fn shipped_gitlab_loop_is_gated() {
        let definition = LoopDefinition::from_yaml(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../loops/gitlab-issue/loop.yaml"),
        )
        .unwrap();
        assert_eq!(definition.verification.mode, "gate");
    }

    #[test]
    fn allowed_tools_in_gate_mode_withhold_push_and_mr_open() {
        let t = allowed_tools("/r", "/bin/loopx", true, false);
        assert!(t.contains("Write(/r/outputs/handoffs/**)"));
        assert!(!t.contains("git push origin"));
        assert!(!t.contains("Bash(loopx mr*)"));
        assert!(t.contains("Bash(loopx worktree*)"));
        assert!(t.contains("Bash(/bin/loopx events*)"));
        let normal = allowed_tools("/r", "/bin/loopx", false, false);
        assert!(normal.contains("Bash(git push origin loop/issue-*)"));
        assert!(normal.contains("Bash(loopx mr*)"));
        assert!(!normal.contains("handoffs"));
    }

    #[test]
    fn allowed_tools_include_stack_patterns() {
        let t = allowed_tools("/r", "loopx", true, false);
        assert!(t.contains("Bash(pnpm test*)"));
        assert!(t.contains("Bash(RAILS_ENV=test bin/rails test*)"));
        assert!(!t.contains("Bash(bundle exec *)"));
        let offline = allowed_tools("/r", "loopx", true, true);
        assert!(!offline.contains("Bash(pnpm test*)"));
    }

    #[test]
    fn allowed_tools_include_screenshot_and_asset_builds() {
        let want = [
            "Bash(loopx screenshot*)",
            "Bash(yarn build*)",
            "Bash(npm run build*)",
            "Bash(pnpm build*)",
            "Bash(RAILS_ENV=test bin/rails assets:precompile*)",
        ];
        for gate in [true, false] {
            let t = allowed_tools("/r", "loopx", gate, false);
            for w in want {
                assert!(t.contains(w), "{w}");
            }
            let off = allowed_tools("/r", "loopx", gate, true);
            for w in want {
                assert!(!off.contains(w), "offline {w}");
            }
        }
        assert!(crate::verify_commands::check_verify_command("yarn build").is_err());
    }

    #[test]
    fn offline_tools_swap_in_the_helper_allowlist() {
        let t = allowed_tools("/r", "/bin/loopx", true, true);
        assert!(t.contains("Bash(python3 -m pytest*)"));
        assert!(t.contains("Bash(loopx memory get*)"));
        assert!(!t.contains("Bash(loopx gitlab*)"));
        assert!(!t.contains("bundle exec"));
        let d = disallowed_tools("/bin/loopx", true, true);
        for sub in ["gitlab", "issues", "notify", "status", "messages"] {
            assert!(d.contains(&format!("Bash(loopx {sub}*)")), "{sub}");
        }
        assert!(d.contains("Bash(loopx mr open*)"));
        assert!(d.contains("Bash(git merge*)"));
    }

    #[test]
    fn claude_command_shape() {
        let c = cli_command(
            "claude",
            "PROMPT",
            "/r",
            "/wt",
            "/bin/loopx",
            false,
            Some(1.5),
            false,
            &[],
        );
        assert_eq!(&c[..2], ["claude", "-p"]);
        let i = c.iter().position(|a| a == "--max-budget-usd").unwrap();
        assert_eq!(c[i + 1], "1.50");
        assert_eq!(c.last().unwrap(), "PROMPT");
        assert_eq!(c[c.len() - 3], "--output-format");
        assert!(c.contains(&"--add-dir".to_string()));
        let nocap = cli_command("claude", "P", "/r", "/wt", "x", false, None, false, &[]);
        assert!(!nocap.contains(&"--max-budget-usd".to_string()));
    }

    #[test]
    fn in_place_checkout_is_added_as_a_writable_dir() {
        let extra = ["/co".to_string()];
        let c = cli_command("claude", "P", "/r", "/wt", "x", true, None, false, &extra);
        let i = c.iter().rposition(|a| a == "/co").unwrap();
        assert_eq!(c[i - 1], "--add-dir");
        let x = cli_command("codex", "P", "/r", "/wt", "x", true, None, false, &extra);
        assert!(
            x.contains(&r#"sandbox_workspace_write.writable_roots=["/r","/wt","/co"]"#.to_string())
        );
    }

    #[test]
    fn only_a_worktree_off_project_exposes_its_checkout() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("projects.json");
        std::fs::write(
            &p,
            r#"{"gitlab_instance": "g", "projects": {"a": {"local_path": "/a", "use_worktree": false}, "b": {"local_path": "/b"}}}"#,
        )
        .unwrap();
        assert_eq!(in_place_dirs(&p, "a"), ["/a"]);
        assert!(in_place_dirs(&p, "b").is_empty());
        assert!(in_place_dirs(&p, "zzz").is_empty());
    }

    #[test]
    fn codex_command_uses_compact_writable_roots() {
        let c = cli_command("codex", "P", "/r", "/wt", "x", false, None, false, &[]);
        assert_eq!(&c[..4], ["codex", "exec", "--sandbox", "workspace-write"]);
        assert!(c.contains(&r#"sandbox_workspace_write.writable_roots=["/r","/wt"]"#.to_string()));
        assert_eq!(c.last().unwrap(), "P");
    }

    #[test]
    fn new_clis_get_their_binary_prompt_and_write_flag() {
        for (cli, flag) in [
            ("gemini", "--yolo"),
            ("kimi", "--yolo"),
            ("cursor", "--force"),
        ] {
            let c = cli_command(cli, "P", "/r", "/wt", "x", false, None, false, &[]);
            assert!(
                ai_cli_config::binary_candidates(cli).contains(&c[0].as_str()),
                "{cli}: {}",
                c[0]
            );
            assert!(c.contains(&"P".to_string()), "{cli}");
            assert!(c.contains(&flag.to_string()), "{cli}");
        }
        let g = cli_command("gemini", "P", "/r", "/wt", "x", false, None, false, &[]);
        assert!(g
            .windows(2)
            .any(|w| w[0] == "--include-directories" && w[1] == "/r,/wt"));
        let c = cli_command("cursor", "P", "/r", "/wt", "x", false, None, false, &[]);
        assert!(c.windows(2).any(|w| w[0] == "--workspace" && w[1] == "/r"));
    }

    fn script_cli(dir: &Path, body: &str) -> String {
        crate::agents::testutil::script(dir, "fake-cli", body)
            .display()
            .to_string()
    }

    #[test]
    fn missing_binary_names_the_cli_and_the_settings_hint() {
        let d = tempfile::tempdir().unwrap();
        let ctx = RunCtx::new(d.path().to_path_buf());
        let argv = vec!["/nonexistent/gemini-bin".to_string(), "P".to_string()];
        let e = run_cli(&ctx, "gemini", &argv, 5.0, None, &[], None).unwrap_err();
        assert!(
            e.message
                .contains("could not start /nonexistent/gemini-bin"),
            "{}",
            e.message
        );
        assert!(e.message.contains("gemini"), "{}", e.message);
        assert!(
            e.message.ends_with("Switch CLI in Settings -> AI CLI."),
            "{}",
            e.message
        );
        let log = std::fs::read_to_string(&ctx.unified_log).unwrap();
        assert!(log.contains("could not start"), "{log}");
    }

    #[test]
    fn new_cli_output_is_extracted_or_fails_visibly() {
        let d = tempfile::tempdir().unwrap();
        let ctx = RunCtx::new(d.path().to_path_buf());
        let ok = vec![script_cli(d.path(), r#"echo '{"response":"fixed it"}'"#)];
        let out = run_cli(&ctx, "gemini", &ok, 30.0, None, &[], None).unwrap();
        assert_eq!((out.cost_usd, out.usage), (None, None));
        let log = std::fs::read_to_string(&ctx.unified_log).unwrap();
        assert!(log.contains("fixed it"), "{log}");
        for (cli, body, want) in [
            ("gemini", "echo plain", "unreadable output from gemini"),
            (
                "gemini",
                r#"echo '{"error":{"message":"quota exceeded"}}'"#,
                "quota exceeded",
            ),
            (
                "cursor",
                r#"echo '{"is_error":true,"result":"auth failed"}'"#,
                "auth failed",
            ),
            ("kimi", "printf '  '", "empty output from kimi"),
        ] {
            let argv = vec![script_cli(d.path(), body)];
            let e = run_cli(&ctx, cli, &argv, 30.0, None, &[], None).unwrap_err();
            assert!(e.message.contains(want), "{cli}: {}", e.message);
        }
    }

    #[test]
    fn the_agent_finds_node_through_the_per_project_shims() {
        let d = tempfile::tempdir().unwrap();
        let mut ctx = RunCtx::new(d.path().to_path_buf());
        ctx.exe = "/opt/loopx/bin/loopx".into();
        let argv = vec![script_cli(
            d.path(),
            r#"echo "{\"response\":\"PATH=$PATH\"}""#,
        )];
        run_cli(&ctx, "gemini", &argv, 30.0, None, &[], None).unwrap();
        let shims = d.path().join("outputs/node-shims");
        let log = std::fs::read_to_string(&ctx.unified_log).unwrap();
        assert!(
            log.contains(&format!("PATH=/opt/loopx/bin:{}:", shims.display())),
            "{log}"
        );
        let npm = std::fs::read_to_string(shims.join("npm")).unwrap();
        assert!(
            npm.contains("exec '/opt/loopx/bin/loopx' _node-shim"),
            "{npm}"
        );
    }

    #[test]
    fn the_activity_file_sits_beside_the_runs_own_status() {
        let ctx = RunCtx::new(PathBuf::from("/r/root"));
        assert_eq!(
            ctx.activity_file,
            PathBuf::from("/r/root/outputs/agent-activity.json")
        );
    }

    fn claude_ctx(dir: &Path) -> RunCtx {
        let mut ctx = RunCtx::new(dir.to_path_buf());
        ctx.exe = "/opt/loopx/bin/loopx".into();
        ctx.activity_file = dir.join("outputs/agent-activity.json");
        ctx
    }

    #[test]
    fn a_claude_call_reports_its_activity_through_hooks() {
        let d = tempfile::tempdir().unwrap();
        let ctx = claude_ctx(d.path());
        let argv_file = d.path().join("argv.txt");
        let argv = vec![
            script_cli(
                d.path(),
                &format!(
                    "printf '%s\\n' \"$@\" > '{}'\necho '{{\"result\":\"ok\"}}'",
                    argv_file.display()
                ),
            ),
            "-p".to_string(),
            "P".to_string(),
        ];
        run_cli(&ctx, "claude", &argv, 30.0, Some(("web", "3")), &[], None).unwrap();
        let args: Vec<String> = std::fs::read_to_string(&argv_file)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(args[..2], ["-p", "--settings"]);
        assert!(
            args[2].contains("'/opt/loopx/bin/loopx' _agent-hook") && args[2].contains("'web #3'"),
            "{}",
            args[2]
        );
        assert_eq!(args.last().unwrap(), "P");
        // Each issue reports to its own file, so parallel agents never
        // overwrite each other's record or mask each other's stall; it is
        // dropped once the call is over.
        let own = crate::agent_activity::for_task(&ctx.activity_file, "web", "3");
        assert!(args[2].contains(&own.display().to_string()), "{}", args[2]);
        assert!(!own.exists());
        assert!(!ctx.activity_file.exists());
    }

    #[test]
    fn parallel_claude_calls_each_report_to_their_own_activity_file() {
        let d = tempfile::tempdir().unwrap();
        let ctx = claude_ctx(d.path());
        // Each fake agent copies its activity record before exiting
        // (`$3` is the --settings JSON naming the hook's file, `$4` the prompt).
        let cli = script_cli(
            d.path(),
            &format!(
                "f=$(printf '%s' \"$3\" | sed -E \"s/.*_agent-hook '([^']+)'.*/\\1/\")\ncp \"$f\" '{}'/seen-$4.json\necho '{{\"result\":\"ok\"}}'",
                d.path().display()
            ),
        );
        std::thread::scope(|s| {
            for iid in ["1", "2", "3"] {
                let (ctx, cli) = (&ctx, &cli);
                s.spawn(move || {
                    let argv = vec![cli.clone(), "-p".into(), iid.to_string()];
                    run_cli(ctx, "claude", &argv, 30.0, Some(("web", iid)), &[], None).unwrap();
                });
            }
        });
        for iid in ["1", "2", "3"] {
            let seen = crate::agent_activity::read(&d.path().join(format!("seen-{iid}.json")))
                .unwrap_or_else(|| panic!("no activity record for #{iid}"));
            assert_eq!(seen["label"], format!("web #{iid}"));
        }
    }

    #[test]
    fn a_claude_call_that_goes_quiet_is_stopped_early() {
        let d = tempfile::tempdir().unwrap();
        let mut ctx = claude_ctx(d.path());
        ctx.stall_after = std::time::Duration::from_millis(400);
        let argv = vec![
            script_cli(d.path(), "exec sleep 30"),
            "-p".into(),
            "P".into(),
        ];
        let start = std::time::Instant::now();
        let e = run_cli(&ctx, "claude", &argv, 60.0, Some(("web", "3")), &[], None).unwrap_err();
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        assert!(
            e.message.starts_with("stalled: no agent activity for"),
            "{}",
            e.message
        );
    }

    #[test]
    fn other_clis_get_no_hooks() {
        let d = tempfile::tempdir().unwrap();
        let ctx = claude_ctx(d.path());
        let argv = vec![
            script_cli(d.path(), r#"echo "{\"response\":\"$*\"}""#),
            "P".into(),
        ];
        run_cli(&ctx, "gemini", &argv, 30.0, Some(("web", "3")), &[], None).unwrap();
        let log = std::fs::read_to_string(&ctx.unified_log).unwrap();
        assert!(!log.contains("--settings"), "{log}");
        assert!(!ctx.activity_file.exists());
    }

    #[test]
    fn unreadable_output_message_names_the_cli_and_keeps_stdout() {
        let m = crate::runner_util::unreadable_output("gemini", "garbage");
        assert!(m.starts_with("unreadable output from gemini"), "{m}");
        assert!(m.contains("garbage"));
    }

    #[test]
    fn gate_prompt_states_the_handoff_path() {
        let (p, env) =
            gate_prompt_and_env("BASE", "web", "7", Path::new("/r"), Some("run_1")).unwrap();
        assert!(p.starts_with("BASE\n\n## Harness gate is ON"));
        assert!(p.contains("/r/outputs/handoffs/run_1/web-7.json"));
        assert_eq!(env[0].0, "LOOP_HANDOFF_PATH");
        assert!(
            gate_prompt_and_env("B", "a", "1", Path::new("/r"), None).is_err()
                || std::env::var("LOOP_RUN_ID").is_ok()
        );
    }

    #[test]
    fn feedback_lists_failed_commands_and_is_bounded() {
        let ev = json!({"commands": [
            {"command": "rspec", "passed": false, "output": "x".repeat(5000)},
            {"command": "rubocop", "passed": true, "output": "fine"},
        ]});
        let it = iteration(vec![v("project", false, "", ev)]);
        let fb = format_feedback(&it);
        assert!(fb.starts_with("## Previous attempt failed external verification"));
        assert!(fb.contains("$ rspec"));
        assert!(!fb.contains("rubocop"));
        assert!(fb.chars().count() <= FEEDBACK_MAX_CHARS);
        let plain = iteration(vec![v("x", false, "plain output", json!({}))]);
        assert!(format_feedback(&plain).ends_with("plain output"));
    }

    #[test]
    fn handoff_validation() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("h.json");
        let w = |s: &str| std::fs::write(&p, s).unwrap();
        w(r#"{"action":"fix","branch":"loop/issue-7","target_branch":"main","title":"Fix #7"}"#);
        assert!(read_handoff(&p, "7").is_some());
        assert!(
            read_handoff(&p, "8").is_none(),
            "branch must match the issue"
        );
        w(r#"{"action":"fix","branch":"loop/issue-7","target_branch":" ","title":"t"}"#);
        assert!(read_handoff(&p, "7").is_none());
        w(r#"{"action":"fix","branch":"staging","target_branch":"main","title":"t"}"#);
        assert!(read_handoff(&p, "7").is_none());
        w(r#"{"action":"fix","branch":"loop/issue-","target_branch":"main","title":"t"}"#);
        assert!(read_handoff(&p, "7").is_none());
        w(r#"{"action":"answer"}"#);
        assert!(read_handoff(&p, "7").is_some());
        w(r#"{"action":"explode"}"#);
        assert!(read_handoff(&p, "7").is_none());
        w("not json");
        assert!(read_handoff(&p, "7").is_none());
        assert!(read_handoff(&d.path().join("missing.json"), "7").is_none());
    }

    #[test]
    fn setup_blocked_handoff_needs_command_and_error() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("h.json");
        let w = |s: &str| std::fs::write(&p, s).unwrap();
        w(
            r#"{"action":"setup_blocked","command":"bundle exec rspec","error":"Could not load database configuration","diagnosis":"no database.yml"}"#,
        );
        assert!(read_handoff(&p, "7").is_some());
        w(r#"{"action":"setup_blocked","command":"bundle exec rspec"}"#);
        assert!(read_handoff(&p, "7").is_none());
        w(r#"{"action":"setup_blocked","error":"boom"}"#);
        assert!(read_handoff(&p, "7").is_none());
        w(r#"{"action":"setup_blocked","command":" ","error":"boom"}"#);
        assert!(read_handoff(&p, "7").is_none());
        w(r#"{"action":"setup_blocked","command":"x","error":""}"#);
        assert!(read_handoff(&p, "7").is_none());
        w(r#"{"action":"setup_blocked","command":1,"error":"boom"}"#);
        assert!(read_handoff(&p, "7").is_none());
    }

    #[test]
    fn finalize_escalates_setup_blocked_with_the_report() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("h.json");
        std::fs::write(
            &path,
            json!({"action": "setup_blocked", "command": "bundle exec rspec",
                "error": "Could not load database configuration"})
            .to_string(),
        )
        .unwrap();
        let result = LoopResult::new("l", "r", "gitlab-issue", LoopState::Completed, vec![], "");
        let posted: Log = Default::default();
        let gitlab = RecordingGitlab(posted.clone());
        let sent = Mutex::new(Vec::<String>::new());
        let notifier = |m: &str| sent.lock().unwrap().push(m.to_string());
        let mut ctx = RunCtx::new(d.path().into());
        ctx.events_dir = Some(d.path().join("events"));
        let report = "**Problem**\n\nBLOCKER-TEXT: no database.yml";
        let outcome = finalize_gated_issue(
            &result,
            "web",
            "7",
            "r",
            &ctx,
            FinalizeDeps {
                gitlab: Some(&gitlab),
                notifier: Some(&notifier),
                handoff: Some(path.clone()),
                project: Some(json!({"project_id": "g/p", "local_path": "/x", "instance": "g"})),
                gitlab_config_path: Some(d.path().join("none.json")),
                setup_report: Some(report.to_string()),
                ..Default::default()
            },
        );
        assert_eq!(outcome, "escalated:setup_blocked");
        let notes = posted.lock().unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].starts_with(report), "{}", notes[0]);
        assert!(!notes[0].contains("What I tried"), "{}", notes[0]);
        assert!(notes[0].contains("**Where the work is**"), "{}", notes[0]);
        drop(notes);
        let events = events::iter_events(ctx.events_dir.as_deref(), None, None);
        let ev = events
            .iter()
            .find(|e| e["event_type"] == "issue.escalated")
            .unwrap_or_else(|| panic!("no escalation event in {events:?}"));
        assert_eq!(ev["data"]["reason"], "setup_blocked");
        assert!(
            sent.lock().unwrap()[0]
                .contains("Setup problems blocked the project's checks, so no merge request was opened - the details are on the issue."),
            "{sent:?}"
        );

        // Without a pre-rendered report, the handoff alone still yields one.
        posted.lock().unwrap().clear();
        let outcome = finalize_gated_issue(
            &result,
            "web",
            "7",
            "r",
            &ctx,
            FinalizeDeps {
                gitlab: Some(&gitlab),
                notifier: Some(&notifier),
                handoff: Some(path),
                project: Some(json!({"project_id": "g/p", "local_path": "/x", "instance": "g"})),
                gitlab_config_path: Some(d.path().join("none.json")),
                ..Default::default()
            },
        );
        assert_eq!(outcome, "escalated:setup_blocked");
        let notes = posted.lock().unwrap();
        assert!(
            notes[0].contains("Could not load database configuration"),
            "{}",
            notes[0]
        );
    }

    #[test]
    fn opened_mr_slack_mentions_auto_fixed_setup() {
        assert_eq!(
            setup_fixed_note(1),
            " (setup fixed automatically: 1 blocker)"
        );
        assert_eq!(
            setup_fixed_note(2),
            " (setup fixed automatically: 2 blockers)"
        );
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("h.json");
        std::fs::write(
            &path,
            json!({"action": "fix", "branch": "loop/issue-7", "target_branch": "main",
                "title": "fix: #7 x"})
            .to_string(),
        )
        .unwrap();
        let evidence = json!({"commands": [{"kind": "test", "command": "pnpm test", "passed": true, "source": "handoff"}]});
        let result = LoopResult::new(
            "l",
            "r",
            "gitlab-issue",
            LoopState::Completed,
            vec![iteration(vec![v("project_commands", true, "", evidence)])],
            "",
        );
        let opener = |_: &str, _: &str, _: &str, _: &str| {
            (
                true,
                "remote:   https://gl.example/g/p/-/merge_requests/1489\n".to_string(),
            )
        };
        let mut ctx = RunCtx::new(d.path().into());
        ctx.events_dir = Some(d.path().join("events"));
        let gitlab = Notes(Default::default());
        let sent = Mutex::new(Vec::<String>::new());
        let notifier = |m: &str| sent.lock().unwrap().push(m.to_string());
        for fixed in [Some(2), None] {
            sent.lock().unwrap().clear();
            let outcome = finalize_gated_issue(
                &result,
                "web",
                "7",
                "r",
                &ctx,
                FinalizeDeps {
                    opener: Some(&opener),
                    gitlab: Some(&gitlab),
                    notifier: Some(&notifier),
                    handoff: Some(path.clone()),
                    project: Some(
                        json!({"project_id": "g/p", "local_path": "/x", "instance": "g"}),
                    ),
                    gitlab_config_path: Some(d.path().join("none.json")),
                    require_approval: Some(false),
                    setup_fixed: fixed,
                    ..Default::default()
                },
            );
            assert_eq!(outcome, "mr_opened");
            let sent = sent.lock().unwrap();
            let finished = sent.iter().find(|m| m.contains("MR opened")).unwrap();
            assert_eq!(
                finished.contains(" (setup fixed automatically: 2 blockers)"),
                fixed.is_some(),
                "{finished}"
            );
        }
    }

    #[test]
    fn slack_escalation_links_the_issue_and_says_why() {
        let link = slack_issue_link("web", "7", Some("https://g.example/g/web"));
        assert_eq!(link, "<https://g.example/g/web/-/issues/7|web #7>");
        assert_eq!(slack_issue_link("web", "7", None), "*web #7*");
        let m = escalation_slack(&link, "run_incomplete", Some("agent_failed"));
        assert_eq!(
            m,
            "*Escalated* <https://g.example/g/web/-/issues/7|web #7>\nThe agent session failed or timed out (details in the loop's log).\nNeeds a human - the details are on the issue."
        );
        let m = escalation_slack(&link, "verification_failed", None);
        assert!(
            m.contains(
                "\nThe loop's own checks of the fix failed, so no merge request was opened.\n"
            ),
            "{m}"
        );
        assert!(!escalation_slack(&link, "some_new_reason", None).contains('_'));
    }

    #[test]
    fn finalize_slack_messages_link_the_issue() {
        let d = tempfile::tempdir().unwrap();
        let gitlab = d.path().join("gitlab.json");
        std::fs::write(
            &gitlab,
            r#"{"instances": {"g": {"url": "https://g.example"}}}"#,
        )
        .unwrap();
        let result = LoopResult::new(
            "l",
            "r",
            "gitlab-issue",
            LoopState::Failed,
            vec![],
            "agent_failed",
        );
        let project = json!({"project_id": "g/p", "local_path": "/x", "instance": "g"});
        let sent = Mutex::new(Vec::<String>::new());
        let notifier = |m: &str| sent.lock().unwrap().push(m.to_string());
        let notes = Notes(Default::default());
        let mut ctx = RunCtx::new(d.path().into());
        ctx.events_dir = Some(d.path().join("events"));
        let outcome = finalize_gated_issue(
            &result,
            "web",
            "7",
            "r",
            &ctx,
            FinalizeDeps {
                gitlab: Some(&notes),
                notifier: Some(&notifier),
                handoff: Some(d.path().join("none.json")),
                project: Some(project),
                gitlab_config_path: Some(gitlab),
                ..Default::default()
            },
        );
        assert_eq!(outcome, "escalated:run_incomplete");
        assert_eq!(sent.lock().unwrap().len(), 1);
        let quiet = finalize_gated_issue(
            &result,
            "web",
            "7",
            "r",
            &ctx,
            FinalizeDeps {
                gitlab: Some(&notes),
                notifier: Some(&notifier),
                handoff: Some(d.path().join("none.json")),
                project: Some(json!({"project_id": "g/p", "local_path": "/x", "instance": "g"})),
                gitlab_config_path: Some(d.path().join("gitlab.json")),
                quiet: true,
                ..Default::default()
            },
        );
        assert_eq!(quiet, "escalated:run_incomplete");
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "quiet: no second message {sent:?}");
        assert!(
            sent[0].starts_with("*Escalated* <https://g.example/g/p/-/issues/7|web #7>\n"),
            "{}",
            sent[0]
        );
    }

    #[test]
    fn a_report_links_an_issue_whose_run_recorded_no_link() {
        // A single-issue run (Run now) has no `issue.queued` event, so the
        // report builds the issue's link from the project config.
        let d = tempfile::tempdir().unwrap();
        let gitlab = d.path().join("gitlab.json");
        std::fs::write(
            &gitlab,
            r#"{"instances": {"g": {"url": "https://g.example/"}}}"#,
        )
        .unwrap();
        let config = d.path().join("projects.json");
        std::fs::write(
            &config,
            r#"{"gitlab_instance": "g", "projects": {"web": {"project_id": "grp/web", "local_path": "/w"}}}"#,
        )
        .unwrap();
        let sent: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = sent.clone();
        let mut ctx = RunCtx::new(d.path().into());
        ctx.events_dir = Some(d.path().join("events"));
        ctx.config_path = Some(config);
        ctx.fakes = Some(Arc::new(RunFakes {
            gitlab: Box::new(Notes(Default::default())),
            notifier: Box::new(move |m: &str| sink.lock().unwrap().push(m.to_string())),
            gitlab_config_path: gitlab,
        }));
        for (kind, data) in [
            ("issue.started", json!({})),
            (
                "issue.completed",
                json!({"action": "fix", "mr_url": "https://g.example/grp/web/-/merge_requests/9"}),
            ),
        ] {
            ctx.emit(
                kind,
                Some("R"),
                EventFields {
                    issue_run_id: Some("R_web_16".into()),
                    project: Some("web".into()),
                    issue_iid: Some(16),
                    data: Some(data),
                },
            );
        }
        send_run_report(&ctx, "R", "GitLab issue run: web #16", &[], 5.0, None);
        let sent = sent.lock().unwrap();
        assert!(
            sent[0].contains("<https://g.example/grp/web/-/issues/16|#16 (web)>"),
            "{}",
            sent[0]
        );
        assert!(
            sent[0].contains("<https://g.example/grp/web/-/merge_requests/9|!9>"),
            "{}",
            sent[0]
        );
    }

    #[test]
    fn a_batch_ends_with_one_report_per_slack_bundle_from_its_events() {
        let d = tempfile::tempdir().unwrap();
        let gitlab = d.path().join("gitlab.json");
        std::fs::write(&gitlab, r#"{"projects": {"api": {"bundle": "team-api"}}}"#).unwrap();
        let sent: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = sent.clone();
        let mut ctx = RunCtx::new(d.path().into());
        ctx.events_dir = Some(d.path().join("events"));
        ctx.fakes = Some(Arc::new(RunFakes {
            gitlab: Box::new(Notes(Default::default())),
            notifier: Box::new(move |m: &str| sink.lock().unwrap().push(m.to_string())),
            gitlab_config_path: gitlab,
        }));
        let emit = |kind: &str, alias: &str, data: Value| {
            ctx.emit(
                kind,
                Some("R"),
                EventFields {
                    issue_run_id: Some(format!("R_{alias}_7")),
                    project: Some(alias.into()),
                    issue_iid: Some(7),
                    data: Some(data),
                },
            )
        };
        emit(
            "issue.queued",
            "web",
            json!({"title": "Fix login", "web_url": ""}),
        );
        emit(
            "issue.completed",
            "web",
            json!({"action": "fix", "mr_url": "https://g/web/-/merge_requests/3"}),
        );
        emit(
            "issue.queued",
            "api",
            json!({"title": "Crashes", "web_url": ""}),
        );
        let failed = IssueRun {
            result: LoopResult::new(
                "l",
                "R_api_7",
                "gitlab-issue",
                LoopState::Failed,
                vec![],
                "agent_failed",
            ),
            usage: None,
            gate_outcome: None,
            setup_fixed: None,
        };
        send_run_report(&ctx, "R", "Daily GitLab loop", &[failed], 61.0, None);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert!(
            sent[0].contains("Completed (61.0s, 1/1 issues)"),
            "{}",
            sent[0]
        );
        assert!(
            sent[0].contains("#7 (web)* \u{2014} Fix login <https://g/web/-/merge_requests/3|!3>"),
            "{}",
            sent[0]
        );
        assert!(sent[1].contains("Finished with problems"), "{}", sent[1]);
        assert!(
            sent[1].contains("Crashes - the run did not finish (failed: agent_failed)"),
            "{}",
            sent[1]
        );
    }

    #[test]
    fn escalation_comment_per_reason() {
        let c = escalation_comment("7", "verification_failed", "boom", Some("no_progress"));
        assert!(c.contains("failed the same way on consecutive attempts:\n\n```\nboom\n```"));
        assert!(c.contains("Branch `loop/issue-7`"));
        let c = escalation_comment("7", "verification_failed", "", Some("budget_exceeded"));
        assert!(c.contains("(The checks produced no output.)"));
        assert!(
            c.contains("The retry did not finish: The run hit its iteration, time or cost budget.")
        );
        let c = escalation_comment("7", "mr_open_failed", "push rejected", None);
        assert!(c.contains("the merge request could not be opened"));
        assert!(c.contains("```\npush rejected\n```"));
        let c = escalation_comment("7", "handoff_invalid", "", None);
        assert!(c.contains("did not leave a valid handoff"));
        let c = escalation_comment("7", "run_incomplete", "", Some("agent_failed"));
        assert!(c.contains("The agent session failed or timed out"));
        let c = escalation_comment("7", "run_incomplete", "", Some("weird"));
        assert!(c.contains("The run stopped early (weird)."));
    }

    #[test]
    fn slack_target_comes_from_the_gitlab_config() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("config.json");
        std::fs::write(
            &cfg,
            json!({
                "projects": {"web": {"bundle": "vip"}},
                "instances": {"work": {"url": "https://gl.example.com/"}}
            })
            .to_string(),
        )
        .unwrap();
        let project = json!({"instance": "work", "project_id": "grp/web"});
        let (bundle, url) = project_slack_target("web", &project, Some(&cfg));
        assert_eq!(bundle.as_deref(), Some("vip"));
        assert_eq!(url.as_deref(), Some("https://gl.example.com/grp/web"));
        let numeric = json!({"instance": "work", "project_id": "123"});
        assert_eq!(project_slack_target("web", &numeric, Some(&cfg)).1, None);
        let (b, u) = project_slack_target("web", &project, Some(&d.path().join("none.json")));
        assert_eq!((b, u), (None, None));
    }

    #[test]
    fn mr_url_is_found_in_push_output() {
        let out = "remote: View merge request for loop/issue-7:\nremote:   https://gl.example.com/grp/web/-/merge_requests/42\nremote:";
        assert_eq!(
            find_mr_url(out).as_deref(),
            Some("https://gl.example.com/grp/web/-/merge_requests/42")
        );
        assert_eq!(find_mr_url("nothing here"), None);
        assert_eq!(find_mr_url("see https://example.com/page"), None);
    }

    #[test]
    fn cost_and_usage_aggregation() {
        assert_eq!(sum_or_none(&[]), None);
        assert_eq!(sum_or_none(&[None, None]), None);
        assert_eq!(sum_or_none(&[Some(1.0), None, Some(0.5)]), Some(1.5));
        let u = sum_usages(&[
            Some(json!({"input_tokens": 1, "output_tokens": 2})),
            None,
            Some(json!({"input_tokens": 3, "cache_read_tokens": 4})),
        ])
        .unwrap();
        assert_eq!(u["input_tokens"], 4);
        assert_eq!(u["output_tokens"], 2);
        assert_eq!(u["cache_read_tokens"], 4);
        assert_eq!(u["cache_write_tokens"], 0);
        assert_eq!(sum_usages(&[None]), None);
    }

    #[test]
    fn loop_escalation_excludes_the_agents_own() {
        assert!(is_loop_escalation(Some("escalated:verification_failed")));
        assert!(!is_loop_escalation(Some("escalated:agent")));
        assert!(!is_loop_escalation(Some("mr_opened")));
        assert!(!is_loop_escalation(None));
    }

    #[test]
    fn main_rejects_wrong_arity() {
        let ctx = RunCtx::new(PathBuf::from("/nonexistent"));
        let inv: Invoker = Arc::new(|_| Ok(AgentOutput::default()));
        assert_eq!(main(&ctx, &[], inv.clone()), 2);
        assert_eq!(main(&ctx, &["r".into(), "harbor".into()], inv), 2);
    }
}
