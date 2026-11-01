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
                // Only GitLab projects need it (checked below).
                cfg.assignee_username().unwrap_or_default(),
                cfg.aliases().map_err(|e| e.to_string())?,
            )
        }
        n if n >= 2 => (args[0].clone(), args[1..].to_vec()),
        _ => return Err(USAGE.to_string()),
    };
    let config = gitlab::load_config();
    let mut result = Map::new();
    for alias in aliases {
        // A `code_host` project (GitHub, ...): its own assignee, else the
        // token's account; see `loopx_core::hosted::assignee`.
        if let Some(project) = loopx_core::hosted::hosted_project(&alias) {
            let src = loopx_core::connectors_config::ConfigConnectors::real();
            let r = loopx_core::hosted::resolve_project(&src, &project)
                .map_err(|e| format!("{alias}: {e}"))?;
            let issues = loopx_core::hosted::helper(&r, &project, "list-issues", &[])
                .map_err(|e| format!("{alias}: {e}"))?;
            result.insert(alias, issues);
            continue;
        }
        if username.is_empty() {
            return Err(format!(
                "{alias}: no GitLab username - set assignee_username in projects.json"
            ));
        }
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
/// The same watermark for the notes on the loop's own merge request.
const LAST_SEEN_MR_KEY: &str = "loop_last_seen_mr_note_id";

const COMMENTS_USAGE: &str = "Usage: loopx issues comments <instance> <project_id> <issue_iid> [exclude_author]\n       loopx issues comments mark-seen <instance> <project_id> <issue_iid>";

fn note_id(note: &Value) -> Result<i64, String> {
    match note.get("id") {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
    .ok_or_else(|| format!("note without a numeric id: {note}"))
}

fn watermark(cache: &Value, key: &str) -> Result<i64, String> {
    match cache.get("_notes").and_then(|n| n.get(key)) {
        None | Some(Value::Null) => Ok(0),
        Some(v) => note_id(&serde_json::json!({ "id": v })),
    }
}

/// `notes` newer than the `key` watermark, optionally minus one author and
/// minus GitLab's own system notes ("added 1 commit").
fn notes_after(
    cache: &Value,
    notes: Option<&Value>,
    key: &str,
    exclude_author: Option<&str>,
    skip_system: bool,
) -> Result<Vec<Value>, String> {
    let last_seen = watermark(cache, key)?;
    let mut fresh = Vec::new();
    for note in notes.and_then(Value::as_array).into_iter().flatten() {
        if note_id(note)? <= last_seen {
            continue;
        }
        if skip_system && note.get("system").and_then(Value::as_bool) == Some(true) {
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

/// The issue's unseen notes, then the unseen review notes on the loop's own
/// merge request (`loop_mr`), each tagged `"source": "merge_request"` with the
/// MR's URL; diff notes keep GitLab's `position` (file and line).
fn unseen_notes(cache: &Value, exclude_author: Option<&str>) -> Result<Vec<Value>, String> {
    let mut notes = notes_after(
        cache,
        cache.get("notes"),
        LAST_SEEN_KEY,
        exclude_author,
        false,
    )?;
    let mr = cache.get("loop_mr");
    let url = mr
        .and_then(|m| m.get("web_url"))
        .cloned()
        .unwrap_or(Value::Null);
    for mut note in notes_after(
        cache,
        mr.and_then(|m| m.get("notes")),
        LAST_SEEN_MR_KEY,
        exclude_author,
        true,
    )? {
        if let Some(obj) = note.as_object_mut() {
            obj.insert("source".into(), Value::String("merge_request".into()));
            obj.insert("merge_request_url".into(), url.clone());
        }
        notes.push(note);
    }
    Ok(notes)
}

/// The watermarks `mark-seen` moves: past every unseen note, the loop's own
/// and system ones included, so they never stall it.
fn seen_watermarks(cache: &Value) -> Result<Vec<(&'static str, String)>, String> {
    let mut marks = Vec::new();
    let mr_notes = cache.get("loop_mr").and_then(|m| m.get("notes"));
    for (key, notes) in [
        (LAST_SEEN_KEY, cache.get("notes")),
        (LAST_SEEN_MR_KEY, mr_notes),
    ] {
        let max = notes_after(cache, notes, key, None, false)?
            .iter()
            .map(note_id)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max();
        if let Some(max) = max {
            marks.push((key, max.to_string()));
        }
    }
    Ok(marks)
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
        let Some(cache) = gitlab_cache::get_issue_cache(instance, project_id, iid) else {
            return Ok("OK".to_string());
        };
        for (key, max_id) in seen_watermarks(&cache)? {
            gitlab_cache::annotate_issue(instance, project_id, iid, key, Value::String(max_id))
                .map_err(|e| e.to_string())?;
        }
        return Ok("OK".to_string());
    }
    let notes = match gitlab_cache::get_issue_cache(&args[0], &args[1], &args[2]) {
        Some(cache) => unseen_notes(&cache, args.get(3).map(String::as_str))?,
        None => Vec::new(),
    };
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

    fn cached_issue() -> Value {
        json!({
            "notes": [
                {"id": 10, "body": "seen", "author": {"username": "pm"}},
                {"id": 11, "body": "new on the issue", "author": {"username": "pm"}},
                {"id": 12, "body": "the loop's own", "author": {"username": "bot"}}
            ],
            "loop_mr": {
                "iid": 5,
                "web_url": "https://gitlab.test/g/web/-/merge_requests/5",
                "notes": [
                    {"id": 200, "body": "old review", "author": {"username": "rev"}},
                    {"id": 201, "body": "Rename this helper.", "author": {"username": "rev"},
                     "position": {"new_path": "lib/page.py", "new_line": 3}},
                    {"id": 202, "body": "added 1 commit", "system": true, "author": {"username": "bot"}},
                    {"id": 203, "body": "Done.", "author": {"username": "bot"}}
                ]
            },
            "_notes": {"loop_last_seen_note_id": "10", "loop_last_seen_mr_note_id": "200"}
        })
    }

    #[test]
    fn new_merge_request_review_notes_come_after_the_issue_notes_tagged() {
        let notes = unseen_notes(&cached_issue(), Some("bot")).unwrap();
        let bodies: Vec<&str> = notes.iter().map(|n| n["body"].as_str().unwrap()).collect();
        assert_eq!(bodies, ["new on the issue", "Rename this helper."]);
        assert!(notes[0].get("source").is_none());
        assert_eq!(notes[1]["source"], "merge_request");
        assert_eq!(
            notes[1]["merge_request_url"],
            "https://gitlab.test/g/web/-/merge_requests/5"
        );
        assert_eq!(notes[1]["position"]["new_path"], "lib/page.py");
    }

    #[test]
    fn mark_seen_moves_both_watermarks_past_everything_unseen() {
        let marks = seen_watermarks(&cached_issue()).unwrap();
        assert_eq!(
            marks,
            vec![
                (LAST_SEEN_KEY, "12".to_string()),
                (LAST_SEEN_MR_KEY, "203".to_string())
            ]
        );
        let nothing_new = json!({"notes": [{"id": 1}], "_notes": {"loop_last_seen_note_id": "1"}});
        assert!(seen_watermarks(&nothing_new).unwrap().is_empty());
    }

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
