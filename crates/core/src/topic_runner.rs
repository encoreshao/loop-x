//! The real Topic Monitor loop wired to [`LoopRuntime`]. Ports
//! `bin/topic_monitor_runner.py`: one agent session per configured topic, each
//! with its own runtime budget. Two consequences of one-session-per-topic are
//! handled here because neither the agent nor a wrapper script can see them:
//!
//! - a topic whose session is killed by the per-topic timeout never reaches its
//!   own "write status idle/failed" step, and the shared
//!   `outputs/topic-monitor/status.json` blocks every future run while any
//!   entry reads "running" - so [`mark_topic_failed`] is that reaper;
//! - `LoopRuntime` contains a per-topic failure, so it never reaches the exit
//!   code - the topic's Slack report (posted here, never by the agent) says so.

use crate::agents::{extract_new_cli_reply, new_cli_args, RunOptions};
use crate::ai_cli_safety::{self, CallSite};
use crate::loop_definition::LoopDefinition;
use crate::loop_result::{AgentCost, LoopResult};
use crate::loop_runtime::{AgentFailure, AgentFn, AgentSuccess, LoopRuntime};
use crate::loop_serialize::write_result;
use crate::loop_state::LoopState;
use crate::loop_verifiers::build_verifiers;
use crate::runner_util::{
    append_unified_log, could_not_start, head, parse_envelope, resolved_program, tail,
    unified_log_path,
};
use crate::topic_report::{self, Ending};
use crate::{ai_cli_config, cost, proc, prompt, slack, slack_report, status, topics};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DEFINITION_RELATIVE_PATH: &str = "loops/topic-monitor/loop.yaml";

/// How much of a failed CLI's output a topic's error keeps.
const CLI_MESSAGE_CHARS: usize = 300;

/// `loopx` subcommands the topic agent may run (its only helpers).
const AGENT_BASH: [&str; 4] = ["topic config", "topic seen", "topic report", "status"];

/// The allow list. The real write boundary is the deny list below: an allow rule
/// grants, it never revokes, and `--add-dir` only adds directories.
pub fn allowed_tools(exe: &str) -> String {
    let mut t = vec![
        "Read(**/outputs/topic-monitor/**)".to_string(),
        "Edit(**/outputs/topic-monitor/**)".into(),
        "Read(**/instructions/topic-monitor.md)".into(),
        "Read(**/instructions/loopx/topic-monitor.md)".into(),
        "Read(**/docs/tasks/topic-monitor-loop.md)".into(),
        "Read(**/.loop-engineering/instructions.md)".into(),
        "Read(**/.loop-engineering/instructions/topic-monitor.md)".into(),
        "Read(**/.loopx/instructions.md)".into(),
        "Read(**/.loopx/instructions/topic-monitor.md)".into(),
        "WebSearch".into(),
        "WebFetch".into(),
        "Bash(cd *)".into(),
    ];
    for sub in AGENT_BASH {
        t.push(format!("Bash(loopx {sub}*)"));
        t.push(format!("Bash({exe} {sub}*)"));
    }
    t.join(" ")
}

/// Deny beats every allow, local or global, so this is the only rule kind that
/// can actually stop a write. Every `**/` rule is anchored to the run's cwd; the
/// installed LaunchAgents directory is outside it, so it gets an absolute rule
/// built from the real (expanded) home directory - a literal `$HOME` would
/// match nothing, since no shell expands it.
pub fn disallowed_tools(home: &str) -> String {
    format!(
        "Bash(git*) \
Read(**/.env*) Read(**/*.key) Read(**/id_rsa*) Read(**/.ssh/**) \
Edit(**/*.sh) Edit(**/*.py) Edit(**/*.plist) Edit(**/*.json) Edit(**/*.yml) Edit(**/*.yaml) Edit(**/*.toml) Edit(**/*.rs) \
Edit(**/bin/**) Edit(**/crates/**) Edit(**/launchd/**) Edit(**/docs/**) Edit(**/config/**) Edit(**/tests/**) Edit(**/assets/**) \
Edit(**/.claude/**) Edit(**/.git/**) Edit(**/.ssh/**) \
Edit(**/instructions/gitlab-issue.md) Edit(**/instructions/topic-monitor.md) Edit(**/instructions/loopx/**) Edit(**/CLAUDE.md) \
Edit(**/README.md) Edit(**/TASK.md) Edit(**/PROGRESS.md) \
Edit(**/outputs/history/**) Edit(**/outputs/daily-review.md) \
Edit(/{home}/Library/LaunchAgents/**)"
    )
}

pub fn cli_command(
    ai_cli: &str,
    prompt: &str,
    exe: &str,
    home: &str,
    max_budget_usd: Option<f64>,
) -> Vec<String> {
    let s = |v: &str| v.to_string();
    if crate::agents::is_new_cli(ai_cli) {
        // No tool allowlist exists for these. The run's cwd is the repo root, so
        // cursor's workspace is ".".
        let opts = RunOptions::new(Path::new("."), 0.0);
        let args = new_cli_args(ai_cli, prompt, &opts).expect("a new CLI");
        return std::iter::once(resolved_program(ai_cli))
            .chain(args)
            .collect();
    }
    if ai_cli == "codex" {
        return vec![
            s("codex"),
            s("exec"),
            s("--sandbox"),
            s("workspace-write"),
            s("-c"),
            s("approval_policy=never"),
            s("-c"),
            s("sandbox_workspace_write.network_access=true"),
            s("-c"),
            s("tools.web_search=true"),
            s(prompt),
        ];
    }
    let mut cmd = vec![
        s("claude"),
        s("-p"),
        s("--permission-mode"),
        s("acceptEdits"),
        s("--allowedTools"),
        allowed_tools(exe),
        s("--disallowedTools"),
        disallowed_tools(home),
    ];
    cmd.extend(cost::budget_args(max_budget_usd));
    cmd.extend([s("--output-format"), s("json"), s(prompt)]);
    cmd
}

/// `(text, cost_usd)` from the CLI's stdout: Claude's JSON envelope gives the
/// result text and `total_cost_usd`; Gemini's JSON gives its `response`; anything
/// else is returned as-is with an unknown cost.
pub fn parse_cli_output(stdout: &str) -> (String, Option<f64>) {
    match parse_envelope(stdout) {
        Some(env) if env.get("result").is_none() && env.get("response").is_some() => {
            let text = match env.get("response") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            (text, None)
        }
        Some(env) if env.get("result").is_some() => {
            let text = match env.get("result") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) | None => String::new(),
                Some(other) => other.to_string(),
            };
            let cost = env.get("total_cost_usd").and_then(Value::as_f64);
            (text, cost)
        }
        _ => (stdout.to_string(), None),
    }
}

/// The reply and cost from the selected CLI's stdout. Gemini, Kimi and Cursor
/// output with no reply (or an error on exit 0) is an `Err` message; Claude's
/// and Codex's output goes through [`parse_cli_output`] untouched.
pub fn read_reply(ai_cli: &str, stdout: &str) -> Result<(String, Option<f64>), String> {
    match extract_new_cli_reply(ai_cli, stdout) {
        Some(reply) => reply.map(|text| (text, None)),
        None => Ok(parse_cli_output(stdout)),
    }
}

#[derive(Debug, Clone)]
pub struct TopicCtx {
    pub repo_root: PathBuf,
    pub events_dir: Option<PathBuf>,
    pub results_dir: Option<PathBuf>,
    pub status_path: PathBuf,
    pub unified_log: PathBuf,
    pub exe: String,
    /// Where each topic's Slack report goes; injectable so tests never post.
    pub slack_config: PathBuf,
    pub topics_config: PathBuf,
    pub slack_post: fn(&str, &str, Option<&Value>) -> Result<(), String>,
}

fn post_to_slack(url: &str, text: &str, blocks: Option<&Value>) -> Result<(), String> {
    slack::post_message(url, text, blocks).map(|_| ())
}

impl TopicCtx {
    pub fn new(repo_root: PathBuf) -> Self {
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "loopx".to_string());
        Self {
            unified_log: unified_log_path(&repo_root),
            status_path: status::topic_status_path(),
            repo_root,
            events_dir: None,
            results_dir: None,
            exe,
            slack_config: slack::default_config_path(),
            topics_config: topics::default_config_path(),
            slack_post: post_to_slack,
        }
    }

    pub fn definition_path(&self) -> PathBuf {
        self.repo_root.join(DEFINITION_RELATIVE_PATH)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TopicOutput {
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TopicError {
    pub message: String,
    pub cost_usd: Option<f64>,
}

/// The one subprocess boundary; tests substitute their own.
pub type TopicInvoker =
    Arc<dyn Fn(&str, f64, Option<f64>) -> Result<TopicOutput, TopicError> + Send + Sync>;

pub fn cli_invoker(ctx: TopicCtx) -> TopicInvoker {
    Arc::new(move |name, timeout_seconds, max_budget| {
        invoke_topic_agent(&ctx, name, timeout_seconds, max_budget)
    })
}

fn home_string() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// Run the agent for one topic. Failures are logged here first, since
/// `LoopRuntime` swallows the message.
pub fn invoke_topic_agent(
    ctx: &TopicCtx,
    name: &str,
    timeout_seconds: f64,
    max_budget_usd: Option<f64>,
) -> Result<TopicOutput, TopicError> {
    let text = prompt::topic(&ctx.repo_root, Some(name));
    let ai_cli = ai_cli_config::selected_cli();
    let argv = cli_command(&ai_cli, &text, &ctx.exe, &home_string(), max_budget_usd);
    run_topic_cli(ctx, &ai_cli, name, &argv, timeout_seconds)
}

/// Spawn `argv` (the selected CLI's command) for one topic and turn its outcome
/// into the topic's result.
fn run_topic_cli(
    ctx: &TopicCtx,
    ai_cli: &str,
    name: &str,
    argv: &[String],
    timeout_seconds: f64,
) -> Result<TopicOutput, TopicError> {
    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]).current_dir(&ctx.repo_root).env(
        topic_report::PATH_ENV,
        topic_report::default_path(&ctx.repo_root, name),
    );
    if let Some(dir) = Path::new(&ctx.exe)
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
    {
        let path = std::env::var("PATH").unwrap_or_default();
        command.env("PATH", format!("{}:{path}", dir.display()));
    }
    let out = proc::run(
        &mut command,
        None,
        Some(Duration::from_secs_f64(timeout_seconds.max(0.0))),
    );
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            let detail = could_not_start(ai_cli, &argv[0], &e);
            append_unified_log(
                &format!(
                    "{ai_cli} invocation for topic '{name}' FAILED (failed to start):\n{detail}"
                ),
                &ctx.unified_log,
            );
            return Err(TopicError {
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
            &out.stdout
        } else {
            &out.stderr
        };
        append_unified_log(
            &format!("{ai_cli} invocation for topic '{name}' FAILED ({reason}):\n{detail}"),
            &ctx.unified_log,
        );
        // The CLI's own words, for the dashboard; a topic prompt holds no
        // private text, so stdout is fine when stderr is empty.
        let excerpt = tail(detail.trim(), CLI_MESSAGE_CHARS);
        return Err(TopicError {
            message: if excerpt.is_empty() {
                reason
            } else {
                format!("{reason}: {excerpt}")
            },
            cost_usd: None,
        });
    }
    let (result_text, cost_usd) = match read_reply(ai_cli, &out.stdout) {
        Ok(reply) => reply,
        Err(message) => {
            append_unified_log(
                &format!(
                    "{ai_cli} invocation for topic '{name}' FAILED (unusable output on exit 0):\n{message}"
                ),
                &ctx.unified_log,
            );
            return Err(TopicError {
                message,
                cost_usd: None,
            });
        }
    };
    println!("{result_text}");
    append_unified_log(&result_text, &ctx.unified_log);
    // The CLI's stderr would otherwise vanish, since the run captures it.
    if !out.stderr.is_empty() {
        append_unified_log(
            &format!("{ai_cli} stderr:\n{}", out.stderr),
            &ctx.unified_log,
        );
    }
    Ok(TopicOutput { cost_usd })
}

/// Note on the topic status document whether this run's CLI runs without a
/// tool allowlist (the dashboard shows it); a restricted CLI clears it.
/// Best-effort.
pub fn record_run_warning(status_path: &Path, cli: &str) {
    let warning = ai_cli_safety::warning(cli, CallSite::TopicMonitor);
    if let Err(e) = status::write_topic_run_warning(status_path, warning) {
        eprintln!("loopx: recording the AI CLI warning failed: {e}");
    }
}

/// Write this topic's terminal `failed` state ourselves: the agent normally does
/// it, but a timeout-killed session never gets there. Best-effort.
/// `error` is the agent call's failure, when that is why the topic failed.
pub fn mark_topic_failed(name: &str, status_path: &Path, error: Option<&str>) -> bool {
    let extra: Vec<(&str, Value)> = error
        .map(|e| ("error", Value::from(e)))
        .into_iter()
        .collect();
    match status::write_topic_status(status_path, name, "failed", &extra) {
        Ok(_) => true,
        Err(e) => {
            eprintln!("loopx: writing failed status for topic '{name}' failed: {e}");
            false
        }
    }
}

pub fn run_one_topic(
    ctx: &TopicCtx,
    run_id: &str,
    name: &str,
    definition: &LoopDefinition,
    invoker: TopicInvoker,
) -> Result<LoopResult, String> {
    let topic_run_id = format!("{run_id}_{name}");
    let timeout_seconds = definition.stop_conditions.max_runtime_minutes * 60.0;
    let max_budget = definition.stop_conditions.max_cost_usd;
    let costs: Arc<Mutex<Vec<Option<f64>>>> = Arc::default();
    // `LoopRuntime` drops the failure message, so keep the last one here.
    let last_error: Arc<Mutex<Option<String>>> = Arc::default();
    let agent_fn: AgentFn = {
        let (name, costs, last_error) = (name.to_string(), costs.clone(), last_error.clone());
        Box::new(
            move |_context| match invoker(&name, timeout_seconds, Some(max_budget)) {
                Ok(out) => {
                    costs.lock().unwrap().push(out.cost_usd);
                    Ok(AgentSuccess {
                        cost_usd: out.cost_usd,
                    })
                }
                Err(e) => {
                    // Bounded: an unreadable reply carries the CLI's whole stdout.
                    *last_error.lock().unwrap() = Some(head(&e.message, CLI_MESSAGE_CHARS));
                    Err(AgentFailure {
                        message: e.message,
                        cost_usd: e.cost_usd,
                    })
                }
            },
        )
    };
    let verifiers =
        build_verifiers(&definition.verifiers, None, None, "observe").map_err(|e| e.to_string())?;
    // Only this session's own briefing may be posted.
    let briefing_path = topic_report::default_path(&ctx.repo_root, name);
    let _ = std::fs::remove_file(&briefing_path);
    let started = std::time::Instant::now();
    let mut runtime = LoopRuntime::new(agent_fn, verifiers);
    if let Some(dir) = &ctx.events_dir {
        runtime = runtime.with_events_dir(dir.clone());
    }
    let mut result = runtime
        .start(definition, &topic_run_id, None)
        .map_err(|e| e.to_string())?;
    let priced: Vec<f64> = costs.lock().unwrap().iter().flatten().copied().collect();
    result.agent_cost_usd = if priced.is_empty() {
        AgentCost::Unknown
    } else {
        AgentCost::Usd(priced.iter().sum())
    };
    // Only on a non-COMPLETED result: a topic that finished normally wrote its
    // own `idle` (or its own `failed`) from inside its session, which is the
    // more accurate self-report.
    let error = last_error.lock().unwrap().clone();
    if result.final_state != LoopState::Completed {
        mark_topic_failed(name, &ctx.status_path, error.as_deref());
    }
    let failure = (result.final_state != LoopState::Completed).then(|| {
        error.unwrap_or_else(|| format!("{}: {}", result.final_state.as_str(), result.stop_reason))
    });
    send_topic_report(
        ctx,
        name,
        topic_report::load(&briefing_path).as_ref(),
        failure.as_deref(),
        started.elapsed().as_secs_f64(),
    );
    if let Err(e) = write_result(
        &result,
        ctx.results_dir.as_deref(),
        ctx.events_dir.as_deref(),
        None,
    ) {
        append_unified_log(
            &format!("writing result.json for {topic_run_id} failed: {e}"),
            &ctx.unified_log,
        );
    }
    Ok(result)
}

/// Run every enabled topic (or `names`) one after another.
pub fn run_all_topics(
    ctx: &TopicCtx,
    run_id: &str,
    invoker: TopicInvoker,
    names: Option<Vec<String>>,
) -> Result<Vec<LoopResult>, String> {
    let names = match names {
        Some(n) => n,
        None => {
            // Skip disabled topics on a real scheduled run: `names()` alone
            // returns every configured topic, enabled or not.
            let path = topics::default_config_path();
            topics::load(&path)
                .map_err(|e| e.to_string())?
                .iter()
                .filter(|t| t.get("enabled").is_none_or(crate::loop_serialize::truthy))
                .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_owned))
                .collect()
        }
    };
    let definition =
        LoopDefinition::from_yaml(&ctx.definition_path()).map_err(|e| e.to_string())?;
    let max_parallel = crate::concurrency_config::current().topics;
    run_topic_batch(ctx, run_id, names, &definition, invoker, max_parallel)
}

/// The topics, at most `max_parallel` at once (see [`crate::parallel`]); a
/// topic another run is already working on is skipped, never run twice.
pub fn run_topic_batch(
    ctx: &TopicCtx,
    run_id: &str,
    names: Vec<String>,
    definition: &LoopDefinition,
    invoker: TopicInvoker,
    max_parallel: usize,
) -> Result<Vec<LoopResult>, String> {
    let tasks: Vec<crate::parallel::Task<String>> = names
        .into_iter()
        .map(|name| crate::parallel::Task {
            lock_path: crate::file_lock::topic_lock_path(&ctx.repo_root, &name),
            group: None,
            item: name,
        })
        .collect();
    let outcomes = crate::parallel::run_tasks(&tasks, max_parallel, |name| {
        run_one_topic(ctx, run_id, name, definition, invoker.clone())
    });
    let mut results = Vec::new();
    let mut first_error = None;
    for (task, outcome) in tasks.iter().zip(outcomes) {
        match outcome {
            crate::parallel::Outcome::Ran(Ok(r)) => results.push(r),
            crate::parallel::Outcome::Ran(Err(e)) => {
                first_error.get_or_insert(e);
            }
            crate::parallel::Outcome::AlreadyRunning => append_unified_log(
                &format!(
                    "topic {} skipped: another run is already working on it",
                    task.item
                ),
                &ctx.unified_log,
            ),
            crate::parallel::Outcome::LockFailed(e) => append_unified_log(
                &format!("topic {} skipped: cannot take its lock: {e}", task.item),
                &ctx.unified_log,
            ),
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(results),
    }
}

/// The topic's one Slack message, to its own bundle. Best-effort.
fn send_topic_report(
    ctx: &TopicCtx,
    name: &str,
    briefing: Option<&topic_report::Briefing>,
    failure: Option<&str>,
    elapsed_seconds: f64,
) {
    let topic = topics::get(&ctx.topics_config, name).unwrap_or(Value::Null);
    let label = topic["label"]
        .as_str()
        .filter(|l| !l.is_empty())
        .unwrap_or(name);
    let bundle = topic["slack_bundle"].as_str().filter(|b| !b.is_empty());
    let ending = match failure {
        Some(why) => Ending::Failed(why),
        None => Ending::Completed,
    };
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let report = topic_report::build(label, &date, briefing, ending, elapsed_seconds);
    let post = ctx.slack_post;
    if !slack_report::send_with(&report, bundle, &ctx.slack_config, &post) {
        append_unified_log(
            &format!("topic '{name}': its Slack report was not sent"),
            &ctx.unified_log,
        );
    }
}

/// `<run_id>` only. Still 0 when topics failed: each failure is contained,
/// recorded in its own result, its status written back as `failed`, and
/// said in that topic's Slack report.
pub fn main(ctx: &TopicCtx, argv: &[String], invoker: TopicInvoker) -> i32 {
    if argv.len() != 1 {
        eprintln!("Usage: loopx run topic-loop: <run_id>");
        return 2;
    }
    record_run_warning(&ctx.status_path, &ai_cli_config::selected_cli());
    match run_all_topics(ctx, &argv[0], invoker, None) {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn disallowed_tools_use_the_real_home_for_launchagents() {
        let d = disallowed_tools("/Users/me");
        assert!(d.contains("Edit(//Users/me/Library/LaunchAgents/**)"));
        assert!(d.starts_with("Bash(git*) "));
        for deny in [
            "Edit(**/*.py)",
            "Edit(**/bin/**)",
            "Edit(**/crates/**)",
            "Edit(**/outputs/history/**)",
        ] {
            assert!(d.contains(deny), "{deny}");
        }
        assert!(!d.contains("$HOME"));
    }

    #[test]
    fn allowed_tools_only_grant_the_topic_helpers() {
        let a = allowed_tools("/bin/loopx");
        assert!(a.contains("WebSearch WebFetch"));
        assert!(a.contains("Bash(loopx topic seen*)"));
        assert!(a.contains("Bash(/bin/loopx topic report*)"));
        assert!(
            !a.contains("notify"),
            "the runner posts the briefing, not the agent"
        );
        assert!(!a.contains("git"));
        assert!(!a.contains("Bash(loopx gitlab"));
    }

    #[test]
    fn claude_command_has_no_add_dir_and_codex_gets_web_search() {
        let c = cli_command("claude", "P", "x", "/h", Some(2.0));
        assert!(!c.contains(&"--add-dir".to_string()));
        assert!(c
            .windows(2)
            .any(|w| w[0] == "--max-budget-usd" && w[1] == "2.00"));
        assert_eq!(c.last().unwrap(), "P");
        let x = cli_command("codex", "P", "x", "/h", None);
        assert!(x.contains(&"tools.web_search=true".to_string()));
    }

    #[test]
    fn cli_output_parsing() {
        let (t, c) = parse_cli_output(r#"{"result": "briefing", "total_cost_usd": 0.12}"#);
        assert_eq!((t.as_str(), c), ("briefing", Some(0.12)));
        let (t, c) = parse_cli_output("plain text");
        assert_eq!((t.as_str(), c), ("plain text", None));
        let (t, c) = parse_cli_output(r#"{"other": 1}"#);
        assert_eq!((t.as_str(), c), (r#"{"other": 1}"#, None));
        let (_, c) = parse_cli_output(r#"{"result": "x", "total_cost_usd": "n/a"}"#);
        assert_eq!(c, None);
    }

    #[test]
    fn new_clis_get_their_binary_prompt_and_write_flag() {
        for (cli, flag) in [
            ("gemini", "--yolo"),
            ("kimi", "--yolo"),
            ("cursor", "--force"),
            ("grok", "--yolo"),
        ] {
            let c = cli_command(cli, "P", "x", "/h", None);
            assert!(
                ai_cli_config::binary_candidates(cli).contains(&c[0].as_str()),
                "{cli}: {}",
                c[0]
            );
            assert!(c.contains(&"P".to_string()), "{cli}");
            assert!(c.contains(&flag.to_string()), "{cli}");
        }
    }

    #[test]
    fn gemini_and_cursor_json_yield_their_text_with_no_cost() {
        let (t, c) = parse_cli_output(r#"{"response": "from gemini", "stats": {}}"#);
        assert_eq!((t.as_str(), c), ("from gemini", None));
        let (t, c) = parse_cli_output(r#"{"type": "result", "result": "from cursor"}"#);
        assert_eq!((t.as_str(), c), ("from cursor", None));
    }

    #[test]
    fn new_cli_garbage_or_error_output_is_a_failure() {
        let e = read_reply("gemini", "garbage").unwrap_err();
        assert!(e.contains("unreadable output from gemini"), "{e}");
        assert_eq!(
            read_reply("gemini", r#"{"response":"ok"}"#).unwrap().0,
            "ok"
        );
        let e = read_reply("cursor", r#"{"is_error":true,"result":"auth failed"}"#).unwrap_err();
        assert!(e.contains("auth failed"), "{e}");
        assert!(read_reply("kimi", "  \n")
            .unwrap_err()
            .contains("empty output from kimi"));
        // Claude and Codex keep taking whatever stdout is.
        assert_eq!(read_reply("codex", "plain").unwrap().0, "plain");
    }

    #[test]
    fn missing_binary_is_a_visible_start_failure() {
        let d = tempfile::tempdir().unwrap();
        let ctx = TopicCtx::new(d.path().to_path_buf());
        let argv = vec!["/nonexistent/kimi-bin".to_string(), "P".to_string()];
        let e = run_topic_cli(&ctx, "kimi", "t", &argv, 5.0).unwrap_err();
        assert!(
            e.message.contains("could not start /nonexistent/kimi-bin"),
            "{}",
            e.message
        );
        assert!(e.message.contains("kimi"), "{}", e.message);
        assert!(
            e.message.ends_with("Switch CLI in Settings -> AI CLI."),
            "{}",
            e.message
        );
    }

    #[test]
    fn failed_marker_updates_only_that_topic() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("status.json");
        status::write_topic_status(&p, "a", "running", &[]).unwrap();
        status::write_topic_status(&p, "b", "running", &[]).unwrap();
        assert!(mark_topic_failed("a", &p, None));
        let doc = status::read_topic_status(&p);
        assert_eq!(doc["topics"]["a"]["state"], "failed");
        assert_eq!(doc["topics"]["b"]["state"], "running");
    }

    #[test]
    fn a_failed_cli_exit_carries_the_cli_message() {
        let d = tempfile::tempdir().unwrap();
        let ctx = TopicCtx::new(d.path().to_path_buf());
        let fake = crate::agents::testutil::script(
            d.path(),
            "fake-gemini",
            "echo 'quota exceeded' >&2; exit 3",
        );
        let argv = vec![fake.display().to_string()];
        let e = run_topic_cli(&ctx, "gemini", "t", &argv, 30.0).unwrap_err();
        assert!(e.message.starts_with("exited 3"), "{}", e.message);
        assert!(e.message.contains("quota exceeded"), "{}", e.message);
    }

    thread_local! {
        static POSTED: std::cell::RefCell<Vec<(String, String)>> = Default::default();
    }

    fn record_post(url: &str, text: &str, _blocks: Option<&Value>) -> Result<(), String> {
        POSTED.with(|p| p.borrow_mut().push((url.to_string(), text.to_string())));
        Ok(())
    }

    fn posted() -> Vec<(String, String)> {
        POSTED.with(|p| p.borrow_mut().drain(..).collect())
    }

    fn topic_ctx(d: &Path) -> TopicCtx {
        let mut ctx = TopicCtx::new(d.to_path_buf());
        ctx.status_path = d.join("status.json");
        ctx.events_dir = Some(d.join("events"));
        ctx.results_dir = Some(d.join("results"));
        ctx.slack_config = d.join("slack.json");
        std::fs::write(
            &ctx.slack_config,
            r#"{"webhook_url": "https://hooks/D", "bundle_webhooks": {"news-room": "https://hooks/N"}, "message_template": "classic"}"#,
        )
        .unwrap();
        ctx.topics_config = d.join("topics.json");
        std::fs::write(
            &ctx.topics_config,
            r#"[{"name": "news", "label": "AI news", "brief": "b", "slack_bundle": "news-room", "enabled": true}]"#,
        )
        .unwrap();
        ctx.slack_post = record_post;
        ctx
    }

    #[test]
    fn a_completed_topic_posts_its_saved_briefing_to_its_bundle() {
        let d = tempfile::tempdir().unwrap();
        let ctx = topic_ctx(d.path());
        let stale = topic_report::default_path(d.path(), "news");
        topic_report::save(&stale, r#"{"summary": "stale"}"#).unwrap();
        let root = d.path().to_path_buf();
        let ok: TopicInvoker = Arc::new(move |name, _, _| {
            let path = topic_report::default_path(&root, name);
            assert!(!path.exists(), "an earlier run's briefing is cleared first");
            topic_report::save(
                &path,
                r#"{"summary": "One launch.", "items": [{"title": "Acme ships X", "description": "New.", "url": "https://www.example.com/x"}]}"#,
            )
            .unwrap();
            Ok(TopicOutput::default())
        });
        posted();
        run_one_topic(&ctx, "run_1", "news", &definition(), ok).unwrap();
        let sent = posted();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].0, "https://hooks/N");
        let text = &sent[0].1;
        assert!(text.starts_with("*AI news briefing ("), "{text}");
        assert!(text.contains(":white_check_mark: Completed ("), "{text}");
        assert!(text.contains("1 new item)"), "{text}");
        assert!(
            text.contains(
                "\u{2022} *Acme ships X* \u{2014} New. <https://www.example.com/x|example.com>"
            ),
            "{text}"
        );
    }

    #[test]
    fn topics_run_side_by_side_and_one_already_running_is_skipped() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let d = tempfile::tempdir().unwrap();
        let ctx = topic_ctx(d.path());
        std::fs::write(
            &ctx.topics_config,
            r#"[{"name": "a", "label": "A", "brief": "b", "enabled": true},
                {"name": "b", "label": "B", "brief": "b", "enabled": true},
                {"name": "c", "label": "C", "brief": "b", "enabled": true},
                {"name": "d", "label": "D", "brief": "b", "enabled": true}]"#,
        )
        .unwrap();
        let _other = crate::file_lock::FileLock::try_exclusive(&crate::file_lock::topic_lock_path(
            &ctx.repo_root,
            "d",
        ))
        .unwrap()
        .unwrap();
        let (now, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let started = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (n, p, st) = (now.clone(), peak.clone(), started.clone());
        let invoker: TopicInvoker = Arc::new(move |name, _, _| {
            st.lock().unwrap().push(name.to_string());
            p.fetch_max(n.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(150));
            n.fetch_sub(1, Ordering::SeqCst);
            Ok(TopicOutput::default())
        });
        let names = ["a", "b", "c", "d"].map(String::from).to_vec();
        let results = run_topic_batch(&ctx, "run_1", names, &definition(), invoker, 2).unwrap();
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        let mut started = started.lock().unwrap().clone();
        started.sort();
        assert_eq!(started, ["a", "b", "c"]);
        assert_eq!(results.len(), 3);
        let log = std::fs::read_to_string(&ctx.unified_log).unwrap();
        assert!(
            log.contains("topic d skipped: another run is already working on it"),
            "{log}"
        );
    }

    fn definition() -> LoopDefinition {
        LoopDefinition::from_yaml(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(DEFINITION_RELATIVE_PATH),
        )
        .unwrap()
    }

    #[test]
    fn a_failed_topic_records_the_cli_message_in_its_status() {
        let d = tempfile::tempdir().unwrap();
        let ctx = topic_ctx(d.path());
        let failing: TopicInvoker = Arc::new(|_, _, _| {
            Err(TopicError {
                message: "exited 3: quota exceeded".into(),
                cost_usd: None,
            })
        });
        posted();
        run_one_topic(&ctx, "run_1", "news", &definition(), failing).unwrap();
        let doc = status::read_topic_status(&ctx.status_path);
        assert_eq!(doc["topics"]["news"]["state"], "failed");
        assert_eq!(doc["topics"]["news"]["error"], "exited 3: quota exceeded");
        let sent = posted();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].1.contains(":x: Did not finish"), "{}", sent[0].1);
        assert!(
            sent[0]
                .1
                .contains("did not finish: exited 3: quota exceeded"),
            "{}",
            sent[0].1
        );
    }

    #[test]
    fn a_huge_failure_message_is_stored_bounded() {
        let d = tempfile::tempdir().unwrap();
        let ctx = topic_ctx(d.path());
        let failing: TopicInvoker = Arc::new(|_, _, _| {
            Err(TopicError {
                message: format!("unreadable output from gemini\n{}", "x".repeat(5000)),
                cost_usd: None,
            })
        });
        run_one_topic(&ctx, "run_1", "news", &definition(), failing).unwrap();
        let doc = status::read_topic_status(&ctx.status_path);
        let error = doc["topics"]["news"]["error"].as_str().unwrap();
        assert!(
            error.starts_with("unreadable output from gemini"),
            "{error}"
        );
        assert!(
            error.chars().count() <= CLI_MESSAGE_CHARS + 1,
            "{}",
            error.len()
        );
    }

    #[test]
    fn a_completed_topic_records_no_error() {
        let d = tempfile::tempdir().unwrap();
        let ctx = topic_ctx(d.path());
        let ok: TopicInvoker = Arc::new(|_, _, _| Ok(TopicOutput::default()));
        run_one_topic(&ctx, "run_1", "news", &definition(), ok).unwrap();
        let doc = status::read_topic_status(&ctx.status_path);
        assert!(doc["topics"].get("news").is_none(), "{doc}");
    }

    #[test]
    fn the_run_warning_follows_the_selected_cli_and_survives_topic_updates() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("status.json");
        record_run_warning(&p, "gemini");
        status::write_topic_status(&p, "news", "idle", &[]).unwrap();
        let doc = status::read_topic_status(&p);
        assert_eq!(
            doc["ai_cli_warning"],
            crate::ai_cli_safety::warning("gemini", crate::ai_cli_safety::CallSite::TopicMonitor)
                .unwrap()
        );
        assert_eq!(doc["topics"]["news"]["state"], "idle");
        record_run_warning(&p, "claude");
        let doc = status::read_topic_status(&p);
        assert!(doc.get("ai_cli_warning").is_none(), "{doc}");
        assert_eq!(doc["topics"]["news"]["state"], "idle");
    }

    #[test]
    fn main_requires_exactly_a_run_id() {
        let ctx = TopicCtx::new(PathBuf::from("/nonexistent"));
        let inv: TopicInvoker = Arc::new(|_, _, _| Ok(TopicOutput::default()));
        assert_eq!(main(&ctx, &[], inv.clone()), 2);
        assert_eq!(main(&ctx, &["a".into(), "b".into()], inv), 2);
        let _ = json!({});
    }
}
