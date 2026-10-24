//! `loopx status`: write run status (replaces `dashboard_server.py write-status`
//! and `write-topic-status`).

use loopx_core::{loop_scheduler, status};
use serde_json::{json, Value};
use std::process::ExitCode;

const WRITE_USAGE: &str = "Usage: loopx status write <state> [--loop NAME] [--exit-code N] [--current-issue TEXT] [--current-step TEXT] [--pid N]";
const TOPIC_USAGE: &str = "Usage: loopx status topic <topic_name> <state> [--current-step TEXT]";

pub fn run(args: &[String]) -> ExitCode {
    let result = match args.first().map(String::as_str) {
        Some("write") => write(&args[1..]),
        Some("topic") => topic(&args[1..]),
        _ => Err(format!("{WRITE_USAGE}\n       {TOPIC_USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
    }
}

/// Value following `--flag` anywhere in `args`.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Result<Option<&'a str>, String> {
    match args.iter().position(|a| a == flag) {
        None => Ok(None),
        Some(i) => args
            .get(i + 1)
            .map(|v| Some(v.as_str()))
            .ok_or_else(|| format!("{flag} needs a value")),
    }
}

fn int_flag(args: &[String], flag: &str) -> Result<Option<Value>, String> {
    flag_value(args, flag)?
        .map(|v| {
            v.parse::<i64>()
                .map(|n| json!(n))
                .map_err(|_| format!("{flag} needs an integer, got {v:?}"))
        })
        .transpose()
}

fn write(args: &[String]) -> Result<(), String> {
    let state = args.first().ok_or(WRITE_USAGE)?;
    let loop_name = flag_value(args, "--loop")?.unwrap_or("gitlab-loop");
    let mut extra: Vec<(&str, Value)> = Vec::new();
    if let Some(pid) = int_flag(args, "--pid")? {
        extra.push(("pid", pid));
    }
    if let Some(code) = int_flag(args, "--exit-code")? {
        extra.push(("last_exit_code", code));
    }
    if let Some(issue) = flag_value(args, "--current-issue")? {
        extra.push(("current_issue", json!(issue)));
    }
    if let Some(step) = flag_value(args, "--current-step")? {
        extra.push(("current_step", json!(step)));
    }
    let path = loop_scheduler::status_path_for_loop(loop_name, None);
    status::write_status(&path, state, &extra)
        .map(|_| ())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn topic(args: &[String]) -> Result<(), String> {
    let [name, state, ..] = args else {
        return Err(TOPIC_USAGE.to_string());
    };
    let mut extra: Vec<(&str, Value)> = Vec::new();
    if let Some(step) = flag_value(args, "--current-step")? {
        extra.push(("current_step", json!(step)));
    }
    let path = status::topic_status_path();
    status::write_topic_status(&path, name, state, &extra)
        .map(|_| ())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flag_value_finds_anywhere() {
        let args = s(&["idle", "--loop", "x", "--pid", "9"]);
        assert_eq!(flag_value(&args, "--loop").unwrap(), Some("x"));
        assert_eq!(int_flag(&args, "--pid").unwrap(), Some(json!(9)));
        assert_eq!(flag_value(&args, "--nope").unwrap(), None);
    }

    #[test]
    fn dangling_flag_is_an_error() {
        assert!(flag_value(&s(&["idle", "--loop"]), "--loop").is_err());
        assert!(int_flag(&s(&["idle", "--pid", "abc"]), "--pid").is_err());
    }
}
