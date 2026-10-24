//! `loopx loop <subcommand>`: the loop-definition CLI (replaces
//! `bin/loop_cli.py`): `init`, `validate`, `audit`, `run`, `status`,
//! `inspect`, `replay`, `cost`, `doctor`, `eval` (scripted, `--golden`,
//! `--chat`) and `ledger backfill` (also available as `loopx ledger`).
//!
//! Manual argv handling, like the Python: flags are looked up anywhere in the
//! argument list and the first non-flag shape of each subcommand is kept.

use chrono::Local;
use loopx_core::agents::{self, AgentStatus, RunOptions};
use loopx_core::loop_audit::{self, format_report, CheckStatus};
use loopx_core::loop_definition::LoopDefinition;
use loopx_core::loop_result::{IterationResult, LoopResult};
use loopx_core::loop_runtime::{AgentFailure, AgentFn, AgentSuccess, LoopContext, LoopRuntime};
use loopx_core::loop_serialize::{self, effective_passed, truthy};
use loopx_core::loop_state::LoopState;
use loopx_core::loop_templates;
use loopx_core::loop_verifiers::build_verifiers;
use loopx_core::pydumps::{py_repr_str, py_str};
use loopx_core::{chat_eval, golden_eval, ledger, loop_eval, pyjson};
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

/// Floor guardrail for the CLI's own run/replay invocations: the same deny
/// list as the GitLab runner's. Defense in depth: even if a loop definition
/// ever supplied a looser allow list, these can never run.
const DEFAULT_DISALLOWED_TOOLS: &str = "Bash(git merge*) Bash(git push --force*) Bash(git push -f*) Bash(git checkout*) Bash(git reset*) Bash(git clean*) Read(**/.env*) Read(**/*.key) Read(**/id_rsa*)";

const COMMANDS: [&str; 11] = [
    "init", "validate", "audit", "run", "status", "inspect", "cost", "doctor", "eval", "replay",
    "ledger",
];

fn code(c: u8) -> ExitCode {
    ExitCode::from(c)
}

fn err(msg: &str, c: u8) -> ExitCode {
    eprintln!("{msg}");
    code(c)
}

/// Python `_parse_flag`: the value after `name`, `default` if absent or last.
fn parse_flag<'a>(argv: &'a [String], name: &str) -> Option<&'a str> {
    let idx = argv.iter().position(|a| a == name)?;
    argv.get(idx + 1).map(String::as_str)
}

/// A path printed the way Python's `Path` would: no `.` segments, no
/// trailing slash, `.` for an empty path.
fn norm(path: &str) -> PathBuf {
    let p: PathBuf = Path::new(path)
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    if p.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        p
    }
}

pub fn run(args: &[String]) -> ExitCode {
    let Some((cmd, rest)) = args.split_first() else {
        return err(
            &format!("Usage: loopx loop <{}> ...", COMMANDS.join("|")),
            2,
        );
    };
    match cmd.as_str() {
        "init" => cmd_init(rest),
        "validate" => cmd_validate(rest),
        "audit" => cmd_audit(rest),
        "run" => cmd_run(rest),
        "status" => cmd_status(rest),
        "inspect" => cmd_inspect(rest),
        "replay" => cmd_replay(rest),
        "cost" => cmd_cost(rest),
        "doctor" => cmd_doctor(rest),
        "eval" => cmd_eval(rest),
        "ledger" => cmd_ledger(rest),
        // The hidden child of the golden suite.
        "eval-agent" => ExitCode::from(golden_eval::agent_child_main(rest).clamp(0, 255) as u8),
        _ => err(
            &format!("Usage: loopx loop <{}> ...", COMMANDS.join("|")),
            2,
        ),
    }
}

// ---- init / validate / audit / doctor -------------------------------------------

fn cmd_init(argv: &[String]) -> ExitCode {
    let template = parse_flag(argv, "--template").unwrap_or("generic");
    let target = norm(parse_flag(argv, "--dir").unwrap_or("."));
    let force = argv.iter().any(|a| a == "--force");
    let templates = loop_templates::available_templates();
    let Some((_, text)) = templates.iter().find(|(n, _)| n == template) else {
        let names: Vec<&str> = templates.iter().map(|(n, _)| n.as_str()).collect();
        return err(
            &format!(
                "init: unknown template {} (choices: {})",
                py_repr_str(template),
                names.join(", ")
            ),
            1,
        );
    };
    let loop_dir = target.join(".loop");
    let loop_yaml = loop_dir.join("loop.yaml");
    if loop_yaml.exists() && !force {
        return err(
            &format!(
                "init: {} already exists - pass --force to overwrite",
                loop_yaml.display()
            ),
            1,
        );
    }
    if let Err(e) =
        std::fs::create_dir_all(&loop_dir).and_then(|()| std::fs::write(&loop_yaml, text))
    {
        return err(&format!("init: {e}"), 1);
    }
    println!("Created {}", loop_yaml.display());
    ExitCode::SUCCESS
}

fn cmd_validate(argv: &[String]) -> ExitCode {
    let Some(path) = argv.first() else {
        return err("Usage: loopx loop validate <path/to/loop.yaml>", 2);
    };
    let (definition, nulls) = match loop_audit::load(Path::new(path)) {
        Ok(v) => v,
        Err(e) => return err(&format!("Invalid loop configuration: {e}"), 1),
    };
    let line = |ok: bool, good: &str, bad: &str| println!("{}", if ok { good } else { bad });
    line(
        !definition.trigger.kind.is_empty(),
        "\u{2713} trigger configured",
        "\u{2717} trigger missing",
    );
    line(
        !definition.goal.kind.is_empty(),
        "\u{2713} goal configured",
        "\u{2717} goal missing",
    );
    line(
        !definition.verifiers.is_empty(),
        "\u{2713} verifier(s) configured",
        "\u{2717} no verifiers configured",
    );
    println!("\u{2713} stop conditions configured");
    line(
        !nulls.max_cost_usd,
        "\u{2713} budget configured",
        "\u{2717} budget unbounded",
    );
    line(
        !definition.human_gates.is_empty(),
        "\u{2713} human gates configured",
        "\u{2717} no human gates configured",
    );
    println!();
    println!("Loop configuration valid.");
    ExitCode::SUCCESS
}

fn cmd_audit(argv: &[String]) -> ExitCode {
    let raw = parse_flag(argv, "--min-score").unwrap_or("0");
    let Ok(min_score) = raw.trim().parse::<f64>() else {
        return err(
            &format!("could not convert string to float: {}", py_repr_str(raw)),
            1,
        );
    };
    let mut paths: Vec<&String> = Vec::new();
    let mut skip = false;
    for arg in argv {
        if skip {
            skip = false;
        } else if arg == "--min-score" {
            skip = true;
        } else {
            paths.push(arg);
        }
    }
    if paths.is_empty() {
        return err(
            "Usage: loopx loop audit <path/to/loop.yaml>... [--min-score N]",
            2,
        );
    }
    let mut failed = false;
    for path in &paths {
        let report = match loop_audit::audit_path(Path::new(path)) {
            Ok(r) => r,
            Err(e) => return err(&format!("audit: {e}"), 1),
        };
        if paths.len() > 1 {
            println!("== {path}");
        }
        print!("{}", format_report(&report));
        if paths.len() > 1 {
            println!();
        }
        if report.has_fail() || report.score.unwrap_or(0.0) < min_score {
            failed = true;
        }
    }
    code(u8::from(failed))
}

fn cmd_doctor(argv: &[String]) -> ExitCode {
    let Some(path) = argv.first() else {
        return err("Usage: loopx loop doctor <path/to/loop.yaml>", 2);
    };
    let report = match loop_audit::audit_path(Path::new(path)) {
        Ok(r) => r,
        Err(e) => return err(&format!("doctor: {e}"), 1),
    };
    println!("Loop Health: {}/100", report.score_text());
    println!();
    // FAILs first (stable), then WARNs, top three.
    let mut issues: Vec<&loop_audit::AuditCheck> = report
        .checks
        .iter()
        .filter(|c| c.status != CheckStatus::Pass)
        .collect();
    issues.sort_by_key(|c| c.status != CheckStatus::Fail);
    if issues.is_empty() {
        println!("No issues found.");
    } else {
        for (i, c) in issues.iter().take(3).enumerate() {
            println!(
                "{}. [{}] {}: {}",
                i + 1,
                c.status.as_str(),
                c.name,
                c.detail
            );
        }
    }
    ExitCode::SUCCESS
}

// ---- run / replay ---------------------------------------------------------------

fn new_run_id() -> String {
    format!(
        "run_{}_{}",
        Local::now().format("%Y%m%d_%H%M%S"),
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
}

/// The in-progress `result.json` the dashboard polls: the final result's
/// shape with `status`/`final_state`/`stop_reason` set to `running`.
fn write_running_snapshot(
    results_dir: Option<&Path>,
    run_id: &str,
    loop_id: &str,
    definition_name: &str,
    iterations: &[IterationResult],
) {
    let partial = LoopResult::new(
        loop_id,
        run_id,
        definition_name,
        LoopState::Pending,
        iterations.to_vec(),
        "running",
    );
    let mut data = loop_serialize::to_json_dict(&partial);
    data["final_state"] = json!("running");
    data["status"] = json!("running");
    let dir = results_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(loop_serialize::default_results_dir)
        .join(run_id);
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join("result.json.tmp");
        std::fs::write(&tmp, pyjson::to_string_pretty(&data))?;
        std::fs::rename(tmp, dir.join("result.json"))
    };
    let _ = write();
}

/// The last `n` characters.
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

fn agent_fn_for(
    definition: &LoopDefinition,
    prompt: Option<String>,
    cwd: PathBuf,
) -> Result<AgentFn, String> {
    let Some(prompt) = prompt else {
        // L0/observe: nothing changes, no cost.
        return Ok(Box::new(|_ctx: &LoopContext<'_>| {
            Ok(AgentSuccess::default())
        }));
    };
    let agent = agents::get_agent(definition.agent.provider.as_deref(), None)?;
    let timeout = definition.stop_conditions.max_runtime_minutes * 60.0;
    Ok(Box::new(move |_ctx: &LoopContext<'_>| {
        let mut opts = RunOptions::new(&cwd, timeout);
        opts.disallowed_tools = Some(DEFAULT_DISALLOWED_TOOLS.to_string());
        let result = agent.run(&prompt, &opts);
        if result.status != AgentStatus::Success {
            let message = tail_chars(&result.output, 800);
            eprintln!("agent invocation {}: {message}", result.status.as_str());
            return Err(AgentFailure {
                message,
                cost_usd: None,
            });
        }
        Ok(AgentSuccess {
            cost_usd: result.estimated_cost_usd,
        })
    }))
}

struct Executed {
    result: LoopResult,
    path: PathBuf,
}

/// One run of `definition` to a terminal state, persisted to `results_dir`.
/// `Err` carries the message and exit code to end with.
fn execute(
    verb: &str,
    definition: &LoopDefinition,
    cwd: &Path,
    prompt: Option<String>,
    results_dir: Option<&Path>,
    definition_path: &str,
    new_run: &str,
) -> Result<Executed, (String, u8)> {
    let verifiers = build_verifiers(&definition.verifiers, Some(cwd), None, "observe")
        .map_err(|e| (e.to_string(), 1))?;
    let agent_fn =
        agent_fn_for(definition, prompt.clone(), cwd.to_path_buf()).map_err(|e| (e, 1))?;
    let loop_id = format!("loop_{}", uuid::Uuid::new_v4().simple());
    let dir_for_progress = results_dir.map(Path::to_path_buf);
    write_running_snapshot(results_dir, new_run, &loop_id, &definition.name, &[]);
    let mut runtime = LoopRuntime::new(agent_fn, verifiers).with_on_iteration(Box::new(
        move |run_id, loop_id, name, iterations| {
            write_running_snapshot(
                dir_for_progress.as_deref(),
                run_id,
                loop_id,
                name,
                iterations,
            );
        },
    ));
    let mut result = runtime
        .start(definition, new_run, Some(&loop_id))
        .map_err(|e| {
            (
                format!("{verb}: policy violation, refusing to start: {e}"),
                1,
            )
        })?;
    result.prompt = prompt;
    result.definition_path = Some(definition_path.to_string());
    result.status = "finished".into();
    let path = loop_serialize::write_result(&result, results_dir, None, None)
        .map_err(|e| (format!("{verb}: cannot write the result: {e}"), 1))?;
    Ok(Executed { result, path })
}

fn cmd_run(argv: &[String]) -> ExitCode {
    let Some(def_arg) = argv.first() else {
        return err(
            "Usage: loopx loop run <path/to/loop.yaml> [--cwd PATH] [--results-dir PATH] [--prompt TEXT | --prompt-file PATH]",
            2,
        );
    };
    let definition_path = Path::new(def_arg);
    let resolved = std::fs::canonicalize(definition_path).unwrap_or_else(|_| {
        std::env::current_dir()
            .unwrap_or_default()
            .join(definition_path)
    });
    let cwd = parse_flag(argv, "--cwd")
        .map(PathBuf::from)
        .unwrap_or_else(|| resolved.parent().map(Path::to_path_buf).unwrap_or_default());
    let results_dir = parse_flag(argv, "--results-dir").map(PathBuf::from);
    let prompt = match parse_flag(argv, "--prompt-file") {
        Some(file) => match std::fs::read_to_string(file) {
            Ok(t) => Some(t),
            Err(e) => return err(&format!("run: could not read --prompt-file: {e}"), 1),
        },
        None => parse_flag(argv, "--prompt").map(str::to_string),
    };
    let definition = match LoopDefinition::from_yaml(definition_path) {
        Ok(d) => d,
        Err(e) => return err(&format!("run: {e}"), 1),
    };
    let run_id = new_run_id();
    let done = match execute(
        "run",
        &definition,
        &cwd,
        prompt,
        results_dir.as_deref(),
        &resolved.display().to_string(),
        &run_id,
    ) {
        Ok(d) => d,
        Err((m, c)) => return err(&m, c),
    };
    println!("run_id: {}", done.result.run_id);
    println!("final_state: {}", done.result.final_state.as_str());
    println!("stop_reason: {}", done.result.stop_reason);
    println!("Result written to {}", done.path.display());
    code(u8::from(done.result.final_state != LoopState::Completed))
}

fn find_stored(run_id: &str, results_dir: Option<&Path>) -> Option<Value> {
    loop_serialize::list_results(results_dir)
        .iter()
        .filter_map(|p| loop_serialize::read_result(p).ok())
        .find(|d| d.get("run_id").and_then(Value::as_str) == Some(run_id))
}

fn cmd_replay(argv: &[String]) -> ExitCode {
    let Some(run_id) = argv.first() else {
        return err("Usage: loopx loop replay <run_id> [--results-dir PATH]", 2);
    };
    let results_dir = parse_flag(argv, "--results-dir").map(PathBuf::from);
    let Some(stored) = find_stored(run_id, results_dir.as_deref()) else {
        return err(
            &format!("replay: no result found for run_id {}", py_repr_str(run_id)),
            1,
        );
    };
    let prompt = stored.get("prompt").and_then(Value::as_str);
    let definition_path = stored.get("definition_path").and_then(Value::as_str);
    let (Some(prompt), Some(definition_path)) = (prompt, definition_path) else {
        println!("replay: no prompt recorded for this run \u{2014} showing inspect output instead");
        print_run_detail(&stored);
        return ExitCode::SUCCESS;
    };
    let definition = match LoopDefinition::from_yaml(Path::new(definition_path)) {
        Ok(d) => d,
        Err(e) => return err(&format!("replay: {e}"), 1),
    };
    let resolved =
        std::fs::canonicalize(definition_path).unwrap_or_else(|_| PathBuf::from(definition_path));
    let cwd = resolved.parent().map(Path::to_path_buf).unwrap_or_default();
    let new_run = new_run_id();
    let done = match execute(
        "replay",
        &definition,
        &cwd,
        Some(prompt.to_string()),
        results_dir.as_deref(),
        definition_path,
        &new_run,
    ) {
        Ok(d) => d,
        Err((m, c)) => return err(&m, c),
    };
    println!("replayed run_id: {run_id}");
    println!("new run_id: {}", done.result.run_id);
    println!("final_state: {}", done.result.final_state.as_str());
    println!("stop_reason: {}", done.result.stop_reason);
    println!("Result written to {}", done.path.display());
    code(u8::from(done.result.final_state != LoopState::Completed))
}

// ---- status / inspect / cost ----------------------------------------------------

fn text(v: &Value, key: &str) -> String {
    v.get(key).map_or("None".to_string(), py_str)
}

fn cmd_status(argv: &[String]) -> ExitCode {
    let results_dir = parse_flag(argv, "--results-dir").map(PathBuf::from);
    let Some(latest) = loop_serialize::find_latest_result(results_dir.as_deref()) else {
        println!("No runs yet.");
        return ExitCode::SUCCESS;
    };
    let data = match loop_serialize::read_result(&latest) {
        Ok(d) => d,
        Err(e) => return err(&format!("status: cannot read {}: {e}", latest.display()), 1),
    };
    println!("{}", text(&data, "definition_name"));
    println!();
    if data.get("status").and_then(Value::as_str) == Some("running") {
        let last = data
            .get("iterations")
            .and_then(Value::as_array)
            .and_then(|a| a.last())
            .cloned()
            .unwrap_or(json!({}));
        let budget = last.get("budget").cloned().unwrap_or(json!({}));
        let iterations = budget.get("iterations").cloned().unwrap_or(json!({}));
        let cost = budget.get("cost").cloned().unwrap_or(json!({}));
        println!("Status: RUNNING");
        println!("Run: {}", text(&data, "run_id"));
        let used = iterations.get("used").filter(|v| !v.is_null());
        let limit = iterations.get("limit");
        if let Some(used) = used {
            if limit.is_some_and(truthy) {
                let limit = limit.expect("checked above");
                println!("Iteration: {}/{}", py_str(used), py_str(limit));
                let pct = (used.as_f64().unwrap_or(0.0) / limit.as_f64().unwrap_or(1.0)).min(1.0);
                let filled = (pct * 10.0) as usize;
                println!(
                    "Budget: {}{} {:.0}%",
                    "\u{2588}".repeat(filled),
                    "\u{2591}".repeat(10 - filled),
                    pct * 100.0
                );
            } else {
                println!("Iteration: {}", py_str(used));
            }
        }
        if let Some(cost_used) = cost.get("used_usd").and_then(Value::as_f64) {
            println!("Cost: ${cost_used:.2}");
        }
        return ExitCode::SUCCESS;
    }
    println!(
        "Status: {}",
        data.get("final_state")
            .and_then(Value::as_str)
            .unwrap_or("None")
            .to_uppercase()
    );
    println!("Run: {}", text(&data, "run_id"));
    println!(
        "Iterations: {}",
        data.get("iterations")
            .and_then(Value::as_array)
            .map_or(0, Vec::len)
    );
    ExitCode::SUCCESS
}

fn print_run_detail(data: &Value) {
    println!(
        "Run {} ({})",
        text(data, "run_id"),
        text(data, "definition_name")
    );
    println!(
        "Final state: {}  Stop reason: {}",
        text(data, "final_state"),
        text(data, "stop_reason")
    );
    for iteration in data
        .get("iterations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        println!(
            "  Iteration {}: {}",
            text(iteration, "iteration"),
            text(iteration, "state")
        );
        for v in iteration
            .get("verification_results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let mark = if truthy(&effective_passed(v)) {
                "PASS"
            } else {
                "FAIL"
            };
            println!("    [{mark}] {}", text(v, "name"));
        }
    }
}

fn cmd_inspect(argv: &[String]) -> ExitCode {
    let Some(run_id) = argv.first() else {
        return err("Usage: loopx loop inspect <run_id> [--results-dir PATH]", 2);
    };
    let results_dir = parse_flag(argv, "--results-dir").map(PathBuf::from);
    match find_stored(run_id, results_dir.as_deref()) {
        Some(data) => {
            print_run_detail(&data);
            ExitCode::SUCCESS
        }
        None => err(
            &format!(
                "inspect: no result found for run_id {}",
                py_repr_str(run_id)
            ),
            1,
        ),
    }
}

fn cmd_cost(argv: &[String]) -> ExitCode {
    if argv.iter().any(|a| a == "--results-dir") {
        return err(
            "cost: --results-dir is no longer supported; costs are read from the event ledger. Run `loop ledger backfill [--results-dir DIR]` then `loop cost [--events-dir DIR]`.",
            2,
        );
    }
    let events_dir = parse_flag(argv, "--events-dir").map(PathBuf::from);
    let runs = ledger::iter_runs(&ledger::RunQuery {
        events_dir: events_dir.as_deref(),
        ..Default::default()
    });
    let summary = loop_serialize::summarize_run_costs(Some(&runs));
    println!("Loop Cost Report");
    println!();
    println!("Runs                  {}", summary.total_runs);
    println!("Estimated Cost       ${:.2}", summary.total_cost_usd);
    if let Some(per_run) = summary.cost_per_run_usd {
        println!("Cost / Run           ${per_run:.2}");
    }
    ExitCode::SUCCESS
}

fn cmd_ledger(argv: &[String]) -> ExitCode {
    crate::events_cmd::ledger(argv)
}

// ---- eval -----------------------------------------------------------------------

/// `<LOOP_EVALS_DIR | repo outputs/evals>/last.json`.
fn write_eval_last(outcomes: &[loop_eval::EvalOutcome]) -> std::io::Result<PathBuf> {
    let path = golden_eval::evals_dir().join("last.json");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let results: Vec<Value> = outcomes
        .iter()
        .map(|o| {
            json!({
                "name": o.case_name,
                "passed": o.passed,
                "reasons": if o.detail.is_empty() { json!([]) } else { json!([o.detail]) },
            })
        })
        .collect();
    let data = json!({"finished_at": golden_eval::finished_at_now(), "results": results});
    std::fs::write(&path, format!("{}\n", pyjson::to_string_pretty(&data)))?;
    Ok(path)
}

fn cmd_eval(argv: &[String]) -> ExitCode {
    if argv.iter().any(|a| a == "--golden") {
        let rest: Vec<String> = argv.iter().filter(|a| *a != "--golden").cloned().collect();
        return run_paid_suite(&rest, "golden", "10");
    }
    if argv.iter().any(|a| a == "--chat") {
        let rest: Vec<String> = argv.iter().filter(|a| *a != "--chat").cloned().collect();
        return run_paid_suite(&rest, "chat", "3");
    }
    let (cases, shown_dir) = match argv.first() {
        Some(dir) => {
            let dir = Path::new(dir);
            if !dir.is_dir() {
                return err(&format!("eval: no such directory {}", dir.display()), 2);
            }
            (loop_eval::load_cases(dir), dir.display().to_string())
        }
        None => match std::env::var_os("LOOPX_ROOT")
            .map(|r| PathBuf::from(r).join("evals").join("cases"))
            .filter(|d| d.is_dir())
        {
            Some(dir) => (loop_eval::load_cases(&dir), dir.display().to_string()),
            None => (loop_eval::builtin_cases(), "evals/cases".to_string()),
        },
    };
    let cases = match cases {
        Ok(c) => c,
        Err(e) => return err(&format!("eval: {e}"), 1),
    };
    if cases.is_empty() {
        return err(&format!("eval: no case files found in {shown_dir}"), 2);
    }
    let outcomes = match loop_eval::run_cases(&cases) {
        Ok(o) => o,
        Err(e) => return err(&format!("eval: {e}"), 1),
    };
    for o in &outcomes {
        println!(
            "{}  {}",
            if o.passed { "PASS" } else { "FAIL" },
            o.case_name
        );
        if !o.passed {
            println!("      {}", o.detail);
        }
    }
    if argv.is_empty() {
        // Only a default-cases run is the canonical record.
        let _ = write_eval_last(&outcomes);
    }
    let passed = outcomes.iter().filter(|o| o.passed).count();
    println!();
    println!("{passed}/{} cases passed", outcomes.len());
    code(u8::from(passed != outcomes.len()))
}

/// `eval --golden|--chat [--budget-usd N] [--case NAME]...`: the paid suites.
fn run_paid_suite(argv: &[String], label: &str, default_budget: &str) -> ExitCode {
    let raw = parse_flag(argv, "--budget-usd").unwrap_or(default_budget);
    let budget = raw.trim().parse::<f64>().unwrap_or(0.0);
    if budget <= 0.0 {
        return err(
            &format!(
                "eval --{label}: --budget-usd must be a positive number, got {}",
                py_repr_str(raw)
            ),
            2,
        );
    }
    let names: Vec<String> = argv
        .iter()
        .take(argv.len().saturating_sub(1))
        .enumerate()
        .filter(|(_, a)| *a == "--case")
        .map(|(i, _)| argv[i + 1].clone())
        .collect();
    let filter = (!names.is_empty()).then_some(names.as_slice());
    let no_cases = || err(&format!("eval --{label}: no {label} cases found"), 2);
    let (total, summary) = if label == "golden" {
        match golden_eval::default_cases(filter) {
            Err(e) => return err(&format!("eval --{label}: {e}"), 2),
            Ok(c) if c.is_empty() => return no_cases(),
            Ok(c) => (c.len(), golden_eval::run_suite_real(&c, budget)),
        }
    } else {
        match chat_eval::default_cases(filter) {
            Err(e) => return err(&format!("eval --{label}: {e}"), 2),
            Ok(c) if c.is_empty() => return no_cases(),
            Ok(c) => (c.len(), chat_eval::run_suite_real(&c, budget)),
        }
    };
    // A --case subset is not the canonical record; don't overwrite the full run.
    let path = if names.is_empty() {
        let written = if label == "golden" {
            golden_eval::write_last_run(&summary, budget, None)
        } else {
            chat_eval::write_last_run(&summary, budget, None)
        };
        written.ok()
    } else {
        None
    };
    for r in &summary.results {
        println!(
            "{}  {}  (${:.2})",
            if r.passed { "PASS" } else { "FAIL" },
            r.name,
            r.cost_usd
        );
        for reason in &r.reasons {
            println!("      {reason}");
        }
    }
    for name in &summary.not_run {
        println!("NOT RUN  {name}  (budget spent)");
    }
    let passed = summary.results.iter().filter(|r| r.passed).count();
    println!();
    println!(
        "{passed}/{total} {label} cases passed, ${:.2} of ${budget:.2} spent",
        summary.spent_usd
    );
    match path {
        Some(p) => println!("Summary written to {}", p.display()),
        None => println!("Summary not recorded (--case subset)"),
    }
    code(golden_eval::exit_code(&summary) as u8)
}
