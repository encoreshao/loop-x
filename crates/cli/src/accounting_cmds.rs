//! CLIs for the accounting/policy/memory slice: `risk` (bin/risk.py), `cost`
//! (bin/cost.py), `learning` (bin/learning.py), `memory-store`
//! (bin/memory_store.py) and `notify-loop` (bin/notify.py).

use chrono::{Duration, Utc};
use loopx_core::pydumps::{dumps, py_repr_str};
use loopx_core::{cost, learning, memory_store, risk};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn fail(msg: &str) -> ExitCode {
    eprintln!("{msg}");
    ExitCode::from(1)
}

/// Python `_parse_flag`: the value after `name`, `None` if absent or last.
fn parse_flag<'a>(argv: &'a [String], name: &str) -> Option<&'a str> {
    let idx = argv.iter().position(|a| a == name)?;
    argv.get(idx + 1).map(String::as_str)
}

/// `--days N` -> (since, until) UTC dates, like Python.
fn days_window(argv: &[String], who: &str) -> Result<(Option<String>, Option<String>), String> {
    let Some(raw) = parse_flag(argv, "--days") else {
        return Ok((None, None));
    };
    let days: i64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("{who}: --days must be an integer, got {}", py_repr_str(raw)))?;
    let until = Utc::now().date_naive();
    let since = until - Duration::days(days - 1);
    Ok((Some(since.to_string()), Some(until.to_string())))
}

fn events_dir_flag(argv: &[String], who: &str) -> Result<Option<PathBuf>, String> {
    match parse_flag(argv, "--events-dir").filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(raw) if !Path::new(raw).exists() => {
            Err(format!("{who}: --events-dir {raw} does not exist"))
        }
        Some(raw) => Ok(Some(PathBuf::from(raw))),
    }
}

pub fn risk(args: &[String]) -> ExitCode {
    if args.first().map(String::as_str) != Some("score") {
        return fail("Usage: risk.py score --title T --description D");
    }
    let Some(title) = parse_flag(args, "--title") else {
        return fail("risk.py score: --title is required");
    };
    let Some(description) = parse_flag(args, "--description") else {
        return fail("risk.py score: --description is required");
    };
    println!(
        "{}",
        dumps(&risk::score(&format!("{title} {description}")).to_value())
    );
    ExitCode::SUCCESS
}

pub fn cost(args: &[String]) -> ExitCode {
    let Some((command, argv)) = args.split_first() else {
        return fail("Usage: cost.py extract-result-text --cli-output-file F | cost.py usage-json --cli-output-file F | cost.py report [--days N] [--events-dir PATH]");
    };
    match command.as_str() {
        "extract-result-text" => {
            let Some(path) = parse_flag(argv, "--cli-output-file").filter(|s| !s.is_empty()) else {
                return fail("extract-result-text: --cli-output-file is required");
            };
            match cost::read_cli_output_file(Path::new(path)) {
                (None, raw) => println!("{raw}"),
                (Some(parsed), _) => println!("{}", cost::extract_result_text(&parsed)),
            }
            ExitCode::SUCCESS
        }
        "usage-json" => {
            let Some(path) = parse_flag(argv, "--cli-output-file").filter(|s| !s.is_empty()) else {
                return fail("usage-json: --cli-output-file is required");
            };
            let Some(parsed) = cost::read_cli_output_file(Path::new(path)).0 else {
                eprintln!(
                    "usage-json: CLI output is not valid Claude JSON, skipping usage extraction"
                );
                return ExitCode::SUCCESS;
            };
            match cost::extract_claude_usage(&parsed) {
                None => eprintln!(
                    "usage-json: CLI output is missing total_cost_usd/usage, skipping usage extraction"
                ),
                Some(usage) => println!("{}", dumps(&usage)),
            }
            ExitCode::SUCCESS
        }
        "report" => {
            let (since, until) = match days_window(argv, "report") {
                Ok(w) => w,
                Err(m) => return fail(&m),
            };
            let dir = match events_dir_flag(argv, "report") {
                Ok(d) => d,
                Err(m) => return fail(&m),
            };
            let report =
                cost::build_cost_report(dir.as_deref(), since.as_deref(), until.as_deref(), None);
            println!("{}", cost::format_cost_report(&report));
            ExitCode::SUCCESS
        }
        other => fail(&format!(
            "Usage: unknown command '{other}' (expected extract-result-text|usage-json|report)"
        )),
    }
}

pub fn learning(args: &[String]) -> ExitCode {
    let (since, until) = match days_window(args, "learning") {
        Ok(w) => w,
        Err(m) => return fail(&m),
    };
    let dir = match events_dir_flag(args, "learning") {
        Ok(d) => d,
        Err(m) => return fail(&m),
    };
    let report = learning::build_learning_report(
        dir.as_deref(),
        since.as_deref(),
        until.as_deref(),
        parse_flag(args, "--project"),
        None,
    );
    println!("{}", learning::format_learning_report(&report));
    ExitCode::SUCCESS
}

const MEMORY_USAGE: &str = "Usage: memory_store.py add <alias> <issue_iid> <lesson> [tags_comma_separated] [category]\n       memory_store.py get <alias> <issue_iid>\n       memory_store.py list <alias>";

pub fn memory_store(args: &[String]) -> ExitCode {
    let Some(command) = args.first() else {
        return fail(MEMORY_USAGE);
    };
    let iid = |s: &String| s.trim().parse::<i64>();
    match (command.as_str(), &args[1..]) {
        ("add", [alias, issue_iid, lesson, rest @ ..]) => {
            let Ok(issue_iid) = iid(issue_iid) else {
                return fail(&format!(
                    "invalid literal for int() with base 10: {}",
                    py_repr_str(&args[2])
                ));
            };
            let tags: Vec<String> = rest
                .first()
                .filter(|t| !t.is_empty())
                .map(|t| t.split(',').map(str::to_string).collect())
                .unwrap_or_default();
            let category = rest.get(1).filter(|c| !c.is_empty()).map(String::as_str);
            match memory_store::add_task_memory(alias, issue_iid, lesson, &tags, category, None) {
                Ok(r) => {
                    println!(
                        "{}",
                        dumps(&serde_json::json!({
                            "action": if r.created { "created" } else { "updated" },
                            "lesson_id": r.lesson_id,
                            "path": r.path.to_string_lossy(),
                        }))
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        ("get", [alias, issue_iid, ..]) => match iid(issue_iid) {
            Ok(n) => {
                let entry = memory_store::get_task_memory(alias, n, None);
                println!("{}", memory_store::task_memory_json(entry.as_ref()));
                ExitCode::SUCCESS
            }
            Err(_) => fail(&format!(
                "invalid literal for int() with base 10: {}",
                py_repr_str(issue_iid)
            )),
        },
        ("list", [alias, ..]) => {
            let all: Vec<Value> = memory_store::list_task_memories(alias, None)
                .iter()
                .map(|m| m.to_value())
                .collect();
            println!(
                "{}",
                loopx_core::pyjson::to_string_pretty(&Value::Array(all))
            );
            ExitCode::SUCCESS
        }
        ("add" | "get" | "list", _) => fail(MEMORY_USAGE),
        (other, _) => fail(&format!("Unknown command: {other}")),
    }
}

pub fn notify_loop(args: &[String]) -> ExitCode {
    let [loop_name, text] = args else {
        return fail("Usage: notify.py <loop_name> <text>");
    };
    let results = loopx_core::connectors_config::notify_loop(loop_name, text, None);
    for (id, ok, message) in &results {
        println!("{id}: {}: {message}", if *ok { "ok" } else { "FAILED" });
    }
    if results.iter().any(|r| r.1) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
