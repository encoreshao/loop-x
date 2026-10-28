//! `loopx topic`: topic registry and dedup state.

use chrono::Utc;
use loopx_core::{pyjson, runner_util, topic_report, topic_seen, topics};
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

pub fn report(args: &[String]) -> ExitCode {
    let path = std::env::var_os(topic_report::PATH_ENV).map(std::path::PathBuf::from);
    finish(run_report(args, path, &runner_util::repo_root()))
}

const REPORT_USAGE: &str = "Usage: loopx topic report <topic_name> '{\"summary\": \"...\", \"items\": [{\"title\": \"...\", \"description\": \"...\", \"url\": \"https://...\"}]}'";

/// Saves to the runner-given path, else the topic's default one under `root`.
fn run_report(
    args: &[String],
    path: Option<std::path::PathBuf>,
    root: &std::path::Path,
) -> Result<String, String> {
    let [name, json] = args else {
        return Err(REPORT_USAGE.into());
    };
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        return Err(format!("Not a topic name: {name}"));
    }
    let path = path.unwrap_or_else(|| topic_report::default_path(root, name));
    let briefing = topic_report::save(&path, json)?;
    Ok(format!(
        "Saved the {name} briefing ({} item{}); the loop posts it to Slack when this topic finishes.",
        briefing.items.len(),
        if briefing.items.len() == 1 { "" } else { "s" }
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_saves_where_the_runner_says_and_rejects_bad_input() {
        let d = tempfile::tempdir().unwrap();
        let given = d.path().join("given.json");
        let args = |n: &str, j: &str| vec![n.to_string(), j.to_string()];
        let out = run_report(
            &args("ai", r#"{"summary": "s", "items": []}"#),
            Some(given.clone()),
            d.path(),
        )
        .unwrap();
        assert!(out.starts_with("Saved the ai briefing (0 items)"), "{out}");
        assert!(given.exists());
        run_report(&args("ai", r#"{"summary": "s"}"#), None, d.path()).unwrap();
        assert!(topic_report::default_path(d.path(), "ai").exists());
        assert!(run_report(&args("../x", "{}"), None, d.path()).is_err());
        assert!(run_report(&args("ai", "nope"), None, d.path()).is_err());
        assert!(run_report(&["ai".into()], None, d.path())
            .unwrap_err()
            .starts_with("Usage"));
    }
}
