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
//!   code - [`alert_on_incomplete_results`] announces it in Slack instead.

use crate::loop_definition::LoopDefinition;
use crate::loop_result::{AgentCost, LoopResult};
use crate::loop_runtime::{AgentFailure, AgentFn, AgentSuccess, LoopRuntime};
use crate::loop_serialize::write_result;
use crate::loop_state::LoopState;
use crate::loop_verifiers::build_verifiers;
use crate::runner_util::{
    append_unified_log, notify_slack_best_effort, parse_envelope, unified_log_path,
};
use crate::{ai_cli_config, cost, proc, prompt, status, topics};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DEFINITION_RELATIVE_PATH: &str = "loops/topic-monitor/loop.yaml";

/// `loopx` subcommands the topic agent may run (its only helpers).
const AGENT_BASH: [&str; 4] = ["topic config", "topic seen", "notify", "status"];

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
/// result text and `total_cost_usd`; anything else is returned as-is with an
/// unknown cost.
pub fn parse_cli_output(stdout: &str) -> (String, Option<f64>) {
    match parse_envelope(stdout) {
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

#[derive(Debug, Clone)]
pub struct TopicCtx {
    pub repo_root: PathBuf,
    pub events_dir: Option<PathBuf>,
    pub results_dir: Option<PathBuf>,
    pub status_path: PathBuf,
    pub unified_log: PathBuf,
    pub exe: String,
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
    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]).current_dir(&ctx.repo_root);
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
            let detail = format!("could not start {}: {e}", argv[0]);
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
        return Err(TopicError {
            message: reason,
            cost_usd: None,
        });
    }
    let (result_text, cost_usd) = parse_cli_output(&out.stdout);
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

/// Write this topic's terminal `failed` state ourselves: the agent normally does
/// it, but a timeout-killed session never gets there. Best-effort.
pub fn mark_topic_failed(name: &str, status_path: &Path) -> bool {
    match status::write_topic_status(status_path, name, "failed", &[]) {
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
    let agent_fn: AgentFn = {
        let (name, costs) = (name.to_string(), costs.clone());
        Box::new(
            move |_context| match invoker(&name, timeout_seconds, Some(max_budget)) {
                Ok(out) => {
                    costs.lock().unwrap().push(out.cost_usd);
                    Ok(AgentSuccess {
                        cost_usd: out.cost_usd,
                    })
                }
                Err(e) => Err(AgentFailure {
                    message: e.message,
                    cost_usd: e.cost_usd,
                }),
            },
        )
    };
    let verifiers =
        build_verifiers(&definition.verifiers, None, None, "observe").map_err(|e| e.to_string())?;
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
    if result.final_state != LoopState::Completed {
        mark_topic_failed(name, &ctx.status_path);
    }
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
    names
        .iter()
        .map(|name| run_one_topic(ctx, run_id, name, &definition, invoker.clone()))
        .collect()
}

/// A per-topic failure never reaches the exit code, so ping Slack here.
pub fn alert_on_incomplete_results(results: &[LoopResult]) -> usize {
    let incomplete: Vec<&LoopResult> = results
        .iter()
        .filter(|r| r.final_state != LoopState::Completed)
        .collect();
    if incomplete.is_empty() {
        return 0;
    }
    let detail = incomplete
        .iter()
        .map(|r| {
            format!(
                "{} ({}: {})",
                r.run_id,
                r.final_state.as_str(),
                r.stop_reason
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    notify_slack_best_effort(
        &format!(
            "*Topic monitor loop:* {} of {} topics did not complete \u{2014} {detail} \u{2014} see outputs/loop-runs/ and logs/loop-engineering.log",
            incomplete.len(),
            results.len()
        ),
        Some("topic_monitor_incomplete"),
        None,
    );
    incomplete.len()
}

/// `<run_id>` only. Still 0 when topics failed: each failure is contained,
/// recorded in its own result, its status written back as `failed`, and
/// announced in Slack.
pub fn main(ctx: &TopicCtx, argv: &[String], invoker: TopicInvoker) -> i32 {
    if argv.len() != 1 {
        eprintln!("Usage: loopx run topic-loop: <run_id>");
        return 2;
    }
    match run_all_topics(ctx, &argv[0], invoker, None) {
        Ok(results) => {
            alert_on_incomplete_results(&results);
            0
        }
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
        assert!(a.contains("Bash(/bin/loopx notify*)"));
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
    fn failed_marker_updates_only_that_topic() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("status.json");
        status::write_topic_status(&p, "a", "running", &[]).unwrap();
        status::write_topic_status(&p, "b", "running", &[]).unwrap();
        assert!(mark_topic_failed("a", &p));
        let doc = status::read_topic_status(&p);
        assert_eq!(doc["topics"]["a"]["state"], "failed");
        assert_eq!(doc["topics"]["b"]["state"], "running");
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
