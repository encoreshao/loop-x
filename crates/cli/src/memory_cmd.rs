//! `loopx memory`: durable cross-run lessons per GitLab project, stored in the
//! project's cache entry under `_memory.fix_learnings` (replaces bin/project_memory.py).

use loopx_core::gitlab_cache;
use loopx_core::pyjson;
use serde_json::{json, Value};
use std::process::ExitCode;

const MEMORY_KEY: &str = "fix_learnings";
const USAGE: &str = "Usage: loopx memory get <instance> <project_id>\n       loopx memory add <instance> <project_id> <lesson> [issue_iid] [tags_comma_separated]";

pub fn run(args: &[String]) -> ExitCode {
    match dispatch(args) {
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

fn learnings(instance: &str, project_id: &str) -> Vec<Value> {
    let project = gitlab_cache::read_json(&gitlab_cache::project_path(instance, project_id));
    project
        .pointer(&format!("/_memory/{MEMORY_KEY}"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn dispatch(args: &[String]) -> Result<String, String> {
    let Some(command) = args.first() else {
        return Err(USAGE.to_string());
    };
    match (command.as_str(), &args[1..]) {
        ("get", [instance, project_id, ..]) => Ok(pyjson::to_string_pretty(&Value::Array(
            learnings(instance, project_id),
        ))),
        ("add", [instance, project_id, lesson, rest @ ..]) => {
            add(instance, project_id, lesson, rest)?;
            Ok("OK".to_string())
        }
        ("get" | "add", _) => Err(USAGE.to_string()),
        (other, _) => Err(format!("Unknown command: {other}")),
    }
}

fn add(instance: &str, project_id: &str, lesson: &str, rest: &[String]) -> Result<(), String> {
    let non_empty = |i: usize| rest.get(i).filter(|s| !s.is_empty());
    let mut entry = json!({ "lesson": lesson });
    if let Some(iid) = non_empty(0) {
        let iid: i64 = iid
            .trim()
            .parse()
            .map_err(|_| format!("invalid issue_iid: {iid:?} (expected an integer)"))?;
        entry["issue_iid"] = json!(iid);
    }
    if let Some(tags) = non_empty(1) {
        entry["tags"] = json!(tags.split(',').collect::<Vec<_>>());
    }
    let mut all = learnings(instance, project_id);
    all.push(entry);
    gitlab_cache::annotate_project(instance, project_id, MEMORY_KEY, Value::Array(all))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_integer_issue_iid_before_writing() {
        let err = add("i", "p", "x", &["abc".to_string()]).unwrap_err();
        assert!(err.contains("invalid issue_iid"));
    }

    #[test]
    fn get_without_args_is_usage() {
        assert_eq!(dispatch(&["get".into()]).unwrap_err(), USAGE);
    }
}
