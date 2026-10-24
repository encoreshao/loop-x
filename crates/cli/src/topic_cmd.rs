//! `loopx topic`: topic registry and dedup state (replaces bin/topic_config.py
//! and bin/topic_seen.py).

use chrono::Utc;
use loopx_core::{pyjson, topic_seen, topics};
use serde_json::Value;
use std::process::ExitCode;

fn finish(result: Result<String, String>) -> ExitCode {
    match result {
        Ok(out) => {
            println!("{out}");
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
    }
}

pub fn config(args: &[String]) -> ExitCode {
    finish(run_config(args))
}

pub fn seen(args: &[String]) -> ExitCode {
    finish(run_seen(args))
}

fn run_config(args: &[String]) -> Result<String, String> {
    let path = topics::default_config_path();
    let e = |e: topics::TopicError| e.to_string();
    match args.first().map(String::as_str) {
        None => {
            Err("Usage: loopx topic config names\n       loopx topic config topic <name>".into())
        }
        Some("names") => Ok(topics::names(&path).map_err(e)?.join("\n")),
        Some("topic") => {
            let name = args
                .get(1)
                .ok_or("Usage: loopx topic config topic <name>")?;
            Ok(pyjson::to_string_pretty(
                &topics::get(&path, name).map_err(e)?,
            ))
        }
        Some(other) => Err(format!("Unknown command: {other}")),
    }
}

fn run_seen(args: &[String]) -> Result<String, String> {
    if args.len() < 2 {
        return Err("Usage: loopx topic seen get <topic_name>\n       loopx topic seen add <topic_name> <url> <title>".into());
    }
    let (command, topic) = (&args[0], &args[1]);
    let dir = topic_seen::default_state_dir();
    match command.as_str() {
        "get" => {
            let items = topic_seen::get_seen(&dir, topic, Utc::now())?;
            Ok(pyjson::to_string_pretty(&Value::Array(items)))
        }
        "add" => {
            let [_, _, url, title, ..] = args else {
                return Err("Usage: loopx topic seen add <topic_name> <url> <title>".into());
            };
            topic_seen::add_seen(&dir, topic, url, title, Utc::now())?;
            Ok("OK".into())
        }
        other => Err(format!("Unknown command: {other}")),
    }
}
