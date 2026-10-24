use loopx_core::config::{default_config_path, Config};
use loopx_core::gitlab::{self, Client};
use loopx_core::gitlab_cache;
use loopx_core::pyjson;
use serde_json::{Map, Value};
use std::process::ExitCode;

const USAGE: &str = "Usage: loopx issues list [username] [project_alias...] (with no arguments, both default from ~/.loopx/projects.json)";

pub fn list(args: &[String]) -> ExitCode {
    match run_list(args) {
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

fn run_list(args: &[String]) -> Result<String, String> {
    let (username, aliases) = match args.len() {
        0 => {
            let cfg = Config::load(&default_config_path()).map_err(|e| e.to_string())?;
            (
                cfg.assignee_username().map_err(|e| e.to_string())?,
                cfg.aliases().map_err(|e| e.to_string())?,
            )
        }
        n if n >= 2 => (args[0].clone(), args[1..].to_vec()),
        _ => return Err(USAGE.to_string()),
    };
    let config = gitlab::load_config();
    let mut result = Map::new();
    for alias in aliases {
        let (project_id, instance, bundle) =
            gitlab::resolve_project_alias(config.as_ref(), &alias, None);
        let client = Client::from_config(config.as_ref(), instance.as_deref(), bundle.as_deref())
            .map_err(|e| e.to_string())?;
        let issues = client
            .list_issues(&project_id, "opened")
            .map_err(|e| e.to_string())?;
        result.insert(alias, Value::Array(filter_assigned_to(issues, &username)));
    }
    Ok(pyjson::to_string_pretty(&Value::Object(result)))
}

const LAST_SEEN_KEY: &str = "loop_last_seen_note_id";

const COMMENTS_USAGE: &str = "Usage: loopx issues comments <instance> <project_id> <issue_iid> [exclude_author]\n       loopx issues comments mark-seen <instance> <project_id> <issue_iid>";

fn note_id(note: &Value) -> Result<i64, String> {
    match note.get("id") {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
    .ok_or_else(|| format!("note without a numeric id: {note}"))
}

/// Notes newer than the issue's last-seen watermark, optionally minus one author.
fn new_notes(
    instance: &str,
    project_id: &str,
    iid: &str,
    exclude_author: Option<&str>,
) -> Result<Vec<Value>, String> {
    let Some(cache) = gitlab_cache::get_issue_cache(instance, project_id, iid) else {
        return Ok(Vec::new());
    };
    let last_seen = match cache.get("_notes").and_then(|n| n.get(LAST_SEEN_KEY)) {
        None | Some(Value::Null) => 0,
        Some(v) => note_id(&serde_json::json!({ "id": v }))?,
    };
    let mut fresh = Vec::new();
    for note in cache
        .get("notes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if note_id(note)? <= last_seen {
            continue;
        }
        let author = note.pointer("/author/username").and_then(Value::as_str);
        if exclude_author.is_some() && author == exclude_author {
            continue;
        }
        fresh.push(note.clone());
    }
    Ok(fresh)
}

pub fn comments(args: &[String]) -> ExitCode {
    match run_comments(args) {
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

fn run_comments(args: &[String]) -> Result<String, String> {
    if args.len() < 3 {
        return Err(COMMENTS_USAGE.to_string());
    }
    // `mark-seen` consumes every unseen note (no author filter), so the loop's
    // own comments still advance the watermark instead of stalling it.
    if args[0] == "mark-seen" {
        let [_, instance, project_id, iid, ..] = args else {
            return Err(
                "Usage: loopx issues comments mark-seen <instance> <project_id> <issue_iid>".into(),
            );
        };
        let notes = new_notes(instance, project_id, iid, None)?;
        if let Some(max_id) = notes
            .iter()
            .map(note_id)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
        {
            gitlab_cache::annotate_issue(
                instance,
                project_id,
                iid,
                LAST_SEEN_KEY,
                Value::String(max_id.to_string()),
            )
            .map_err(|e| e.to_string())?;
        }
        return Ok("OK".to_string());
    }
    let notes = new_notes(
        &args[0],
        &args[1],
        &args[2],
        args.get(3).map(String::as_str),
    )?;
    Ok(pyjson::to_string_pretty(&Value::Array(notes)))
}

/// Keep only issues where `username` appears in the `assignees` list.
fn filter_assigned_to(issues: Vec<Value>, username: &str) -> Vec<Value> {
    issues
        .into_iter()
        .filter(|issue| {
            issue
                .get("assignees")
                .and_then(Value::as_array)
                .is_some_and(|a| {
                    a.iter()
                        .any(|u| u.get("username").and_then(Value::as_str) == Some(username))
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keeps_only_issues_with_the_assignee() {
        let issues = vec![
            json!({"iid": 1, "assignees": [{"username": "dev"}]}),
            json!({"iid": 2, "assignees": [{"username": "x"}]}),
            json!({"iid": 3}),
        ];
        let kept = filter_assigned_to(issues, "dev");
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0]["iid"], 1);
    }
}
