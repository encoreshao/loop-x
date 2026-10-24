//! `loopx events` (replaces bin/events.py) and `loopx ledger` (the
//! `loop_cli.py ledger backfill` subcommand).

use loopx_core::events::{self, EventFields};
use loopx_core::ledger;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Python's `_parse_flag`: the argument after the first occurrence of `name`.
fn parse_flag<'a>(argv: &'a [String], name: &str) -> Option<&'a str> {
    let idx = argv.iter().position(|a| a == name)?;
    argv.get(idx + 1).map(String::as_str)
}

fn dir_flag(argv: &[String]) -> Option<PathBuf> {
    parse_flag(argv, "--events-dir")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

pub fn run(args: &[String]) -> ExitCode {
    let Some(command) = args.first() else {
        eprintln!("Usage: loopx events emit --type T --run-id R [...] | loopx events list (--date D | --run-id R) [...]");
        return ExitCode::from(1);
    };
    let rest = &args[1..];
    match command.as_str() {
        "emit" => ExitCode::from(cmd_emit(rest)),
        "list" => ExitCode::from(cmd_list(rest)),
        other => {
            eprintln!("Usage: unknown command '{other}' (expected emit|list)");
            ExitCode::from(1)
        }
    }
}

fn cmd_emit(argv: &[String]) -> u8 {
    let event_type = parse_flag(argv, "--type").filter(|s| !s.is_empty());
    let run_id = parse_flag(argv, "--run-id").filter(|s| !s.is_empty());
    let Some(event_type) = event_type else {
        eprintln!("emit: --type is required");
        return 1;
    };
    let Some(run_id) = run_id else {
        eprintln!("emit: --run-id is required");
        return 1;
    };
    let mut data = None;
    if let Some(raw) = parse_flag(argv, "--data") {
        match serde_json::from_str::<Value>(raw) {
            Ok(Value::Null) => {}
            Ok(v) => data = Some(v),
            Err(e) => {
                eprintln!("emit: --data is not valid JSON: {e}");
                return 1;
            }
        }
    }
    let issue_iid = match parse_flag(argv, "--issue-iid") {
        None => None,
        Some(raw) => match raw.trim().parse::<i64>() {
            Ok(n) => Some(n),
            Err(_) => {
                eprintln!("emit: --issue-iid is not an integer: {raw:?}");
                return 1;
            }
        },
    };
    let events_dir = dir_flag(argv);
    let fields = EventFields {
        issue_run_id: parse_flag(argv, "--issue-run-id").map(str::to_string),
        project: parse_flag(argv, "--project").map(str::to_string),
        issue_iid,
        data,
    };
    match events::emit(event_type, run_id, fields, events_dir.as_deref()) {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("emit: {e}");
            1
        }
    }
}

fn cmd_list(argv: &[String]) -> u8 {
    let date = parse_flag(argv, "--date").filter(|s| !s.is_empty());
    let run_id = parse_flag(argv, "--run-id").filter(|s| !s.is_empty());
    if date.is_none() && run_id.is_none() {
        eprintln!("list: either --date or --run-id is required");
        return 1;
    }
    let dir = dir_flag(argv);
    for line in events::list_lines(date, run_id, dir.as_deref()) {
        println!("{line}");
    }
    0
}

/// `loopx ledger backfill [--results-dir DIR] [--events-dir DIR]`.
pub fn ledger(args: &[String]) -> ExitCode {
    if args.first().map(String::as_str) != Some("backfill") {
        eprintln!("Usage: loopx ledger backfill [--results-dir DIR] [--events-dir DIR]");
        return ExitCode::from(2);
    }
    let rest = &args[1..];
    let results = parse_flag(rest, "--results-dir").map(PathBuf::from);
    let events = parse_flag(rest, "--events-dir").map(PathBuf::from);
    match ledger::backfill_from_results(
        results.as_deref().map(Path::new),
        events.as_deref().map(Path::new),
    ) {
        Ok(n) => {
            println!("Backfilled {n} run(s) into the ledger");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ledger: backfill failed: {e}");
            ExitCode::from(1)
        }
    }
}
