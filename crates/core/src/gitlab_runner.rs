//! The real GitLab issue loop wired to [`LoopRuntime`]. Ports
//! `bin/gitlab_loop_runner.py`: build each issue's prompt, run the agent CLI
//! under the runtime's budget/verification, and (in gate mode) open the merge
//! request only after the loop's own checks pass, otherwise escalate.
//!
//! Three agent-invocation shapes share one subprocess boundary
//! ([`CliInvoker`]): the dashboard's on-demand single issue, one issue inside the
//! scheduled batch, and the batch's single end-of-run wrap-up.

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
    append_unified_log, emit_best_effort, failed_call_cost, head, notify_slack_best_effort,
    parse_envelope, tail, unified_log_path, STDERR_EXCERPT_CHARS,
};
use crate::{
    ai_cli_config, approvals, cost, events, issue_tracking_config, learning, mr, proc, prompt,
};
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const DEFINITION_RELATIVE_PATH: &str = "loops/gitlab-issue/loop.yaml";
pub const NEEDS_HUMAN_LABEL: &str = "loop:needs-human";
const HANDOFF_ACTIONS: [&str; 4] = ["fix", "answer", "escalate", "wait_for_review"];
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
const AGENT_SUBCOMMANDS: [&str; 11] = [
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
];
/// The helpers an offline (golden eval) run keeps.
const OFFLINE_SUBCOMMANDS: [&str; 5] = ["config", "events", "risk", "memory-store", "memory get"];
/// What an offline run must never reach: GitLab, Slack, the live dashboard.
const OFFLINE_DENIED_SUBCOMMANDS: [&str; 5] = ["gitlab", "issues", "notify", "status", "messages"];

const DISALLOWED_TOOLS: &str = "Bash(git merge*) Bash(git push --force*) Bash(git push -f*) Bash(git checkout*) Bash(git reset*) Bash(git clean*) Read(**/.env*) Read(**/*.key) Read(**/id_rsa*)";

const GATE_OVERRIDE: &str = "## Harness gate is ON for this run \u{2014} this overrides steps 9 and 10\n\nDo NOT run `loopx mr open` and do NOT push: skip step 9. The loop re-runs the project's checks itself and opens the merge request only if they pass.\n\nFor a fix, once it is committed on loop/issue-<iid> and your own checks pass, do step 10 with these changes: annotate loop_last_action as \"fix_handed_off\" (not \"mr_opened: ...\"), still run `loopx issues comments mark-seen`, still record a learning if there is one (`loopx memory-store add` and its memory.created emit), and still cd back to the loop directory. Do NOT send the \"Finished\" Slack message and do NOT emit issue.completed \u{2014} the loop does both after it opens the merge request. Then write this JSON to <handoff_path> (also in $LOOP_HANDOFF_PATH) and stop:\n\n{\"action\": \"fix\", \"branch\": \"loop/issue-<iid>\", \"target_branch\": \"<target>\", \"title\": \"Fix #<iid>: <short title>\", \"summary\": \"<2-4 sentences on what you changed and why; the loop posts it on the issue next to the merge request link>\"}\n\nEvery issue in a gated run must end with a handoff file at that path, written after finishing the matching path as usual:\n- answered directly: {\"action\": \"answer\"}\n- escalated (needs clarification, or your own verification failed): {\"action\": \"escalate\"}\n- Wait for reviewer, or nothing to act on this run: {\"action\": \"wait_for_review\"}\nA missing or malformed handoff is escalated to a human.";

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
        t.extend(
            [
                "Bash(RAILS_ENV=test bundle exec rspec*)",
                "Bash(bundle exec rspec*)",
                "Bash(bundle exec rubocop*)",
                "Bash(bundle check*)",
                "Bash(bundle install*)",
                "Bash(RAILS_ENV=test bundle exec rake db:test:prepare*)",
                "Bash(npm run test*)",
                "Bash(npm run lint*)",
                "Bash(npm ci*)",
                "Bash(yarn install*)",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
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
) -> Vec<String> {
    let s = |v: &str| v.to_string();
    if ai_cli == "codex" {
        // `codex exec` has no --add-dir/--ask-for-approval; the two roots become
        // a writable_roots override, and network access is needed for git push
        // and GitLab calls.
        let roots = serde_json::to_string(&[repo_root, worktree_root]).expect("serialisable");
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
    let mut cmd = vec![
        s("claude"),
        s("-p"),
        s("--add-dir"),
        s(repo_root),
        s("--add-dir"),
        s(worktree_root),
        s("--permission-mode"),
        s("acceptEdits"),
        s("--allowedTools"),
        allowed_tools(repo_root, exe, gate, offline),
        s("--disallowedTools"),
        disallowed_tools(exe, gate, offline),
    ];
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
}

impl RunCtx {
    pub fn new(repo_root: PathBuf) -> Self {
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "loopx".to_string());
        let unified_log = unified_log_path(&repo_root);
        Self {
            repo_root,
            events_dir: None,
            results_dir: None,
            unified_log,
            exe,
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
    let base = match &req.mode {
        InvokeMode::Single => prompt::single_issue(&ctx.repo_root, &req.alias, &req.issue_iid),
        InvokeMode::Batch => prompt::batch_issue(&ctx.repo_root, &req.alias, &req.issue_iid),
        InvokeMode::IssueFile(file) => {
            prompt::issue_file(&ctx.repo_root, &req.alias, &req.issue_iid, file).map_err(fail)?
        }
    };
    let mut text = with_feedback(base, req.feedback.as_deref());
    let offline = matches!(req.mode, InvokeMode::IssueFile(_));
    // The golden eval always runs in gate mode.
    let gate = req.gate || offline;
    let mut env = Vec::new();
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
        env = e;
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

/// The wrap-up call for a scheduled batch ("End of run" only).
pub fn invoke_batch_end_of_run(
    ctx: &RunCtx,
    timeout_seconds: f64,
) -> Result<AgentOutput, AgentError> {
    let text = prompt::batch_end_of_run(&ctx.repo_root);
    invoke_cli_with_prompt(ctx, &text, timeout_seconds, None, &[], false, None, false)
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
    let argv = cli_command(
        &ai_cli,
        prompt_text,
        &repo_root,
        &worktree_root,
        &ctx.exe,
        gate,
        max_budget_usd,
        offline,
    );

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
    command.args(&argv[1..]);
    for (k, v) in env {
        command.env(k, v);
    }
    // Make `loopx` resolvable for the agent whatever PATH the scheduler gave us.
    if let Some(dir) = Path::new(&ctx.exe)
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
    {
        let path = std::env::var("PATH").unwrap_or_default();
        command.env("PATH", format!("{}:{path}", dir.display()));
    }
    let timeout = std::time::Duration::from_secs_f64(timeout_seconds.max(0.0));
    let out = match proc::run(&mut command, None, Some(timeout)) {
        Ok(out) => out,
        Err(e) => {
            let detail = format!("could not start {}: {e}", argv[0]);
            report_failure("failed to start", &detail, None);
            return Err(AgentError {
                message: detail,
                cost_usd: None,
            });
        }
    };

    if out.timed_out || !out.success() {
        let reason = if out.timed_out {
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
/// `target_branch` and `title`.
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
        "approval_denied" | "approval_timeout" | "approval_unavailable" => (
            "I fixed this and the loop's own checks passed, but opening the merge request needs a human's approval first.".to_string(),
            match reason {
                "approval_denied" => "The approval was denied, so no merge request was opened.".to_string(),
                "approval_timeout" => "Nobody approved it before the approval request expired, so no merge request was opened.".to_string(),
                _ => format!(
                    "The approval request could not be created:{}",
                    if fenced.is_empty() { "\n(No output.)" } else { &fenced }
                ),
            },
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
        "**What I tried**\n{tried}\n\n**What failed**\n{failed}\n\n**Where the work is**\n- Branch `loop/issue-{issue_iid}` in this issue's local worktree (not pushed).\n\n**What I need from you**\n- Look at the failure above and the unpushed branch, then fix it or tell me how to proceed."
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
    let mut issue_label = format!("#{issue_iid} ({alias})");
    let notify = |bundle: &Option<String>, message: String| match deps.notifier {
        Some(n) => n(&message),
        None => {
            notify_slack_best_effort(&message, None, bundle.as_deref());
        }
    };

    let project = match deps.project.clone() {
        Some(p) => Ok(p),
        None => Config::load(&default_config_path())
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
                    format!("Loop escalated {issue_label} (project_config_error); see the issue for details."),
                );
                return "escalated:project_config_error".to_string();
            }
        };
    bundle = b;
    if let Some(url) = url {
        issue_label = format!("<{url}/-/issues/{issue_iid}|{issue_label}>");
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
        notify(
            &bundle,
            format!("Loop escalated {issue_label} ({reason}); see the issue for details."),
        );
        format!("escalated:{reason}")
    };

    let data = read_handoff(&handoff, issue_iid);
    if let Some(outcome) = data
        .as_ref()
        .and_then(|d| d.get("action"))
        .and_then(Value::as_str)
        .and_then(agent_outcome)
    {
        return outcome.to_string();
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
            .unwrap_or_else(approvals::default_dir);
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
        notify(
            &bundle,
            format!(
                "*Approval needed* {issue_label}: open MR `{branch}` \u{2192} `{target}`? Approve it in LoopX (expires {}).",
                approval["expires_at"].as_str().unwrap_or("?")
            ),
        );
        let poll = deps
            .approval_poll
            .unwrap_or(std::time::Duration::from_secs(1));
        let status = approvals::wait_for_decision(&dir, &approval_id, poll);
        emit(
            "approval.decided",
            json!({"approval_id": approval_id, "status": status}),
        );
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

    let checks: Vec<String> = ["test_cmd", "lint_cmd"]
        .iter()
        .filter_map(|k| {
            project
                .get(*k)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
        })
        .map(|c| format!("- `{c}` \u{2705}"))
        .collect();
    let summary = data.get("summary").and_then(Value::as_str).unwrap_or("");
    let body = format!(
        "Opened merge request for this fix: {branch} \u{2192} {target}. Verified by the loop with:\n{}\n\n{summary}",
        checks.join("\n")
    )
    .trim_end()
    .to_string();
    api(
        "POST",
        &format!("{notes_path}/notes"),
        json!({"body": body}),
    );
    // Same shape the batch wrap-up reads from an agent-emitted fix event.
    let mr_url = find_mr_url(&detail);
    emit(
        "issue.completed",
        json!({"outcome": "mr_opened", "gated": true, "action": "fix", "mr_url": mr_url}),
    );
    // Step 10's "Finished" message, which the gate override tells the agent not to send.
    let link = match &mr_url {
        Some(u) => format!("<{u}|view MR>"),
        None => branch.clone(),
    };
    notify(
        &bundle,
        format!("*Finished* {issue_label}: MR opened \u{2192} {link}"),
    );
    "mr_opened".to_string()
}

/// First `https?://<non-space>/merge_requests/<digits>` in `text`.
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
    let timeout_seconds = definition.stop_conditions.max_runtime_minutes * 60.0;
    let verification_mode = definition.verification.mode.clone();
    let gate = verification_mode == "gate";
    let raw_costs: Arc<Mutex<Vec<Option<f64>>>> = Arc::default();
    let raw_usages: Arc<Mutex<Vec<Option<Value>>>> = Arc::default();
    let handoff = gate.then(|| handoff_path(run_id, alias, issue_iid, &ctx.repo_root));

    let mut issue = IssueContext::new(alias, issue_iid, timeout_seconds);
    issue.handoff_path = handoff.clone();

    let max_cost = definition.stop_conditions.max_cost_usd;
    let agent_fn: AgentFn = {
        let (raw_costs, raw_usages) = (raw_costs.clone(), raw_usages.clone());
        let (alias, issue_iid, run_id) =
            (alias.to_string(), issue_iid.to_string(), run_id.to_string());
        let handoff = handoff.clone();
        Box::new(move |context| {
            if let Some(h) = &handoff {
                // Each attempt starts with no handoff, so finalize can never
                // read an earlier attempt's.
                let _ = std::fs::remove_file(h);
            }
            let feedback = context
                .previous
                .filter(|p| iteration_failed_verification(p))
                .map(format_feedback);
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
                emit_verification_events(
                    &ctx,
                    &run_id,
                    &issue_run_id,
                    &alias,
                    &issue_iid,
                    &mode,
                    latest,
                );
            },
        )
    };

    let mut runtime = LoopRuntime::new(agent_fn, verifiers).with_on_iteration(on_iteration);
    if let Some(dir) = &ctx.events_dir {
        runtime = runtime.with_events_dir(dir.clone());
    }
    let mut result = runtime
        .start(definition, &issue_run_id, None)
        .map_err(|e| e.to_string())?;

    // LoopRuntime counts a None cost as 0 in the budget, so record the raw,
    // un-coerced figure to keep "cost $0" apart from "never got a cost".
    result.agent_cost_usd = match sum_or_none(&raw_costs.lock().unwrap()) {
        Some(v) => AgentCost::Usd(v),
        None => AgentCost::Unknown,
    };
    let usage = sum_usages(&raw_usages.lock().unwrap());

    let mut gate_outcome = None;
    if gate {
        let outcome = finalize_gated_issue(
            &result,
            alias,
            issue_iid,
            run_id,
            ctx,
            FinalizeDeps::default(),
        );
        if is_loop_escalation(Some(&outcome)) && result.final_state == LoopState::Completed {
            // Visible on Loop Runs as an escalation, not "completed".
            result.final_state = LoopState::Escalated;
            result.stop_reason = format!("gate:{}", outcome.split_once(':').map_or("", |x| x.1));
        }
        gate_outcome = Some(outcome);
    }

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
    })
}

// ---- whole runs -------------------------------------------------------------

pub fn run_all_issues(
    ctx: &RunCtx,
    run_id: &str,
    invoker: Invoker,
    aliases: Option<Vec<String>>,
    username: Option<String>,
) -> Result<Vec<IssueRun>, String> {
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
    // One title message so the per-issue Starting/Finished lines that follow
    // read as a single run in Slack.
    let count = todo.len();
    notify_slack_best_effort(
        &format!(
            "*GitLab loop started* \u{2014} {count} issue{} to check",
            if count != 1 { "s" } else { "" }
        ),
        None,
        None,
    );

    let mut results = Vec::new();
    for (alias, issue) in todo {
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
        results.push(run_one_issue(
            ctx,
            run_id,
            &alias,
            &iid_text,
            &definition,
            invoker.clone(),
            InvokeMode::Batch,
        )?);
    }

    // UNCONDITIONAL, and outside LoopRuntime: "End of run" is housekeeping, not
    // a verifiable task. It must run even with zero results: a morning with no
    // assigned issues is exactly when nothing else would report anything.
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
        notify_slack_best_effort(
            &format!("*Daily GitLab loop:* the end-of-run wrap-up FAILED, so today's digest/daily-review was NOT produced \u{2014} {detail}"),
            Some("gitlab_wrapup_failed"),
            None,
        );
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
    Ok(results)
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

/// A per-issue failure is caught by `LoopRuntime` and never reaches the exit
/// code, so announce it in Slack. A gated issue the loop already escalated
/// notified from `finalize_gated_issue`, so it is skipped to alert exactly once.
pub fn alert_on_incomplete_results(runs: &[IssueRun]) -> usize {
    let incomplete: Vec<&IssueRun> = runs
        .iter()
        .filter(|r| {
            r.result.final_state != LoopState::Completed
                && !is_loop_escalation(r.gate_outcome.as_deref())
        })
        .collect();
    if incomplete.is_empty() {
        return 0;
    }
    let detail = incomplete
        .iter()
        .map(|r| {
            format!(
                "{} ({}: {})",
                r.result.run_id,
                r.result.final_state.as_str(),
                r.result.stop_reason
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    notify_slack_best_effort(
        &format!(
            "*Daily GitLab loop:* {} of {} issues did not complete \u{2014} {detail} \u{2014} see outputs/loop-runs/ and logs/loop-engineering.log",
            incomplete.len(),
            runs.len()
        ),
        Some("gitlab_issues_incomplete"),
        None,
    );
    incomplete.len()
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
    let runs = if argv.len() == 3 {
        run_single_issue(ctx, run_id, &argv[1], &argv[2], invoker).map(|r| vec![r])
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
    alert_on_incomplete_results(&runs);
    // Still 0 when issues failed: each failure is contained, recorded in its
    // own LoopResult, and announced in Slack. A non-zero exit would report the
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
        );
        assert_eq!(&c[..2], ["claude", "-p"]);
        let i = c.iter().position(|a| a == "--max-budget-usd").unwrap();
        assert_eq!(c[i + 1], "1.50");
        assert_eq!(c.last().unwrap(), "PROMPT");
        assert_eq!(c[c.len() - 3], "--output-format");
        assert!(c.contains(&"--add-dir".to_string()));
        let nocap = cli_command("claude", "P", "/r", "/wt", "x", false, None, false);
        assert!(!nocap.contains(&"--max-budget-usd".to_string()));
    }

    #[test]
    fn codex_command_uses_compact_writable_roots() {
        let c = cli_command("codex", "P", "/r", "/wt", "x", false, None, false);
        assert_eq!(&c[..4], ["codex", "exec", "--sandbox", "workspace-write"]);
        assert!(c.contains(&r#"sandbox_workspace_write.writable_roots=["/r","/wt"]"#.to_string()));
        assert_eq!(c.last().unwrap(), "P");
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
