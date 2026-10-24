//! `loopx loops` (replaces `bin/loops_config.py`'s CLI, used by run-loop-now.sh)
//! and `loopx ai-cli` (replaces `bin/ai_cli_config.py`'s CLI, used by the
//! run-loop shell scripts).

use loopx_core::ai_cli_config;
use loopx_core::loops_config::{self, py_str, shell_quote, LoopsError};
use serde_json::Value;
use std::process::ExitCode;

const LOOPS_USAGE: &str =
    "Usage: loopx loops <names|entry-point|timeout-seconds|log-suffix|emit-run-events|bash-env> [name]";
const AI_CLI_USAGE: &str = "Usage: loopx ai-cli get\n       loopx ai-cli set <claude|codex>";

fn fail(msg: &str) -> ExitCode {
    eprintln!("{msg}");
    ExitCode::from(1)
}

pub fn loops(args: &[String]) -> ExitCode {
    let Some(command) = args.first() else {
        return fail(LOOPS_USAGE);
    };
    let config = loops_config::default_config_path();
    if command == "names" {
        return match loops_config::list_loops(&config, None) {
            Ok(all) => {
                let names: Vec<&str> = all
                    .iter()
                    .filter_map(|l| l.get("name").and_then(Value::as_str))
                    .collect();
                println!("{}", names.join("\n"));
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        };
    }
    let Some(name) = args.get(1) else {
        return fail(&format!("Usage: loopx loops {command} <name>"));
    };
    let entry = match loops_config::get_loop(name, &config, None) {
        Ok(l) => l,
        Err(
            e @ (LoopsError::UnknownLoop(_)
            | LoopsError::Invalid(_)
            | LoopsError::NotFound(_)
            | LoopsError::Json(_)),
        ) => return fail(&e.to_string()),
        Err(e) => return fail(&e.to_string()),
    };
    let field = |key: &str| entry.get(key);
    let emit = || {
        if field("emit_run_events").is_some_and(loops_config::py_truthy) {
            "true"
        } else {
            "false"
        }
    };
    match command.as_str() {
        "entry-point" | "timeout-seconds" => {
            let key = if command == "entry-point" {
                "entry_point"
            } else {
                "timeout_seconds"
            };
            match field(key) {
                Some(v) => println!("{}", py_str(v)),
                None => {
                    return fail(&format!(
                        "Loop '{name}' is missing required registry field '{key}'"
                    ))
                }
            }
        }
        "log-suffix" => println!("{}", field("log_suffix").map_or(String::new(), py_str)),
        "emit-run-events" => println!("{}", emit()),
        "bash-env" => {
            let (Some(entry_point), Some(timeout)) =
                (field("entry_point"), field("timeout_seconds"))
            else {
                let missing = if field("entry_point").is_none() {
                    "entry_point"
                } else {
                    "timeout_seconds"
                };
                return fail(&format!(
                    "Loop '{name}' is missing required registry field '{missing}'"
                ));
            };
            println!("ENTRY_POINT={}", shell_quote(&py_str(entry_point)));
            println!("TIMEOUT_SECONDS={}", shell_quote(&py_str(timeout)));
            println!(
                "LOG_SUFFIX={}",
                shell_quote(&field("log_suffix").map_or(String::new(), py_str))
            );
            println!("EMIT_RUN_EVENTS={}", shell_quote(emit()));
        }
        other => return fail(&format!("Unknown command: {other}")),
    }
    ExitCode::SUCCESS
}

pub fn ai_cli(args: &[String]) -> ExitCode {
    let path = ai_cli_config::default_config_path();
    match args.first().map(String::as_str) {
        None => fail(AI_CLI_USAGE),
        Some("get") => {
            println!("{}", ai_cli_config::get_selected_cli(&path));
            ExitCode::SUCCESS
        }
        Some("set") => {
            let Some(cli) = args.get(1) else {
                return fail("Usage: loopx ai-cli set <claude|codex>");
            };
            match ai_cli_config::set_selected_cli(cli, &path) {
                Ok(msg) => {
                    println!("{msg}");
                    ExitCode::SUCCESS
                }
                Err(msg) => fail(&msg),
            }
        }
        Some(other) => fail(&format!("Unknown command: {other}")),
    }
}
