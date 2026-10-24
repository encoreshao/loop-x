//! `loopx gitlab`: GitLab REST helpers for the agent and the loop (replaces the
//! gitlab-config skill's `gitlab_api.py`, plus `loopx gitlab cache ...` for its
//! `gitlab_cache.py`).

use loopx_core::gitlab::{self, Client};
use loopx_core::gitlab_cache;
use loopx_core::pyjson;
use serde_json::{json, Value};
use std::process::ExitCode;

const USAGE: &str = "Usage: loopx gitlab [--instance=<name>] <command> [args...]

Commands:
  list-instances                                   - List configured GitLab instances
  list-projects                                    - List configured project aliases
  project-info <alias>                             - Print {project_id, instance, bundle} for one alias as JSON
  whoami                                           - Show the authenticated GitLab user
  get-issue <project> <issue_iid>                  - Fetch issue with comments
  list-issues <project> [state] [labels...] [--assignee=<user>] [--author=<user>]
                                                   - List issues with filters (paginates through all results)
  get-mr <project> <mr_iid>                        - Fetch merge request with changes
  list-mrs <project> [state] [--assignee=<user>] [--author=<user>]
                                                   - List merge requests (paginates through all results)
  post-issue-comment <project> <issue_iid> <comment>
  post-mr-comment <project> <mr_iid> <comment>
  get-diff <project> <mr_iid>                      - Get unified diff for MR
  aggregate-issues <project> [days]                - Get issue statistics
  sync-issue <project> <issue_iid>                 - Fetch issue+notes and merge into the local cache
  sync-project <project>                           - Fetch project+members (and its parent group) into the cache
  sync-group <group_path>                          - Fetch group+members into the cache
  cached-issue <project> <issue_iid>               - Read the cached issue (no network call)
  cache <get-issue|get-users|get-group|get-project|annotate-issue|annotate-project> ...

Options:
  --instance=<name>  Specify which GitLab instance to use (from config file)

Note: <project> can be a project alias (from config) or full project ID";

type Out = Result<Option<String>, String>;

pub fn run(args: &[String]) -> ExitCode {
    match dispatch(args) {
        Ok(Some(text)) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
    }
}

fn pretty(v: &Value) -> Out {
    Ok(Some(pyjson::to_string_pretty(v)))
}

/// Split `--key=value` flags out of `args`, returning (positional, flags).
fn pop_kv_flags(args: &[String], keys: &[&str]) -> (Vec<String>, Vec<(String, String)>) {
    let mut positional = Vec::new();
    let mut found = Vec::new();
    for arg in args {
        let hit = keys.iter().find_map(|k| {
            arg.strip_prefix(&format!("--{k}="))
                .map(|v| (k.to_string(), v.to_string()))
        });
        match hit {
            Some(kv) => found.push(kv),
            None => positional.push(arg.clone()),
        }
    }
    (positional, found)
}

fn flag<'a>(flags: &'a [(String, String)], key: &str) -> Option<&'a str> {
    flags
        .iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn int(arg: &str) -> Result<i64, String> {
    arg.trim()
        .parse()
        .map_err(|_| format!("invalid integer: {arg:?}"))
}

fn api_err(e: gitlab::GitlabError) -> String {
    match e {
        gitlab::GitlabError::Http { status, url, body } => {
            format!("HTTP Error: {status} for url: {url}\nResponse: {body}")
        }
        other => other.to_string(),
    }
}

fn dispatch(argv: &[String]) -> Out {
    if argv.is_empty() {
        return Err(USAGE.to_string());
    }
    let mut args: Vec<String> = argv.to_vec();
    let mut instance_name: Option<String> = None;
    if let Some(name) = args[0].strip_prefix("--instance=") {
        instance_name = Some(name.to_string());
        args.remove(0);
        if args.is_empty() {
            return Err("Error: No command specified".into());
        }
    }
    let command = args[0].clone();
    let config = gitlab::load_config();

    match command.as_str() {
        "list-instances" => return list_instances(config.as_ref()),
        "list-projects" => return list_projects(config.as_ref()),
        "project-info" => {
            let alias = args
                .get(1)
                .ok_or("Usage: loopx gitlab project-info <alias>")?;
            let project = config
                .as_ref()
                .and_then(|c| c.get("projects"))
                .and_then(|p| p.get(alias.as_str()))
                .ok_or_else(|| format!("Error: Unknown project alias '{alias}'"))?;
            return pretty(&json!({
                "project_id": project.get("project_id").cloned().unwrap_or(json!(alias)),
                "instance": project.get("instance").cloned().unwrap_or(Value::Null),
                "bundle": project.get("bundle").cloned().unwrap_or(Value::Null),
            }));
        }
        "cache" => return cache_cmd(&args[1..]),
        _ => {}
    }

    let mut bundle_name = None;
    if args.len() > 1 {
        let explicit_instance = instance_name.is_some();
        let (project_id, resolved_instance, resolved_bundle) =
            gitlab::resolve_project_alias(config.as_ref(), &args[1], instance_name.as_deref());
        args[1] = project_id;
        if instance_name.is_none() {
            instance_name = resolved_instance;
        }
        // An explicit --instance= means "ignore this alias's instance-specific
        // settings, including its bundle".
        bundle_name = if explicit_instance {
            None
        } else {
            resolved_bundle
        };
    }

    let client = Client::from_config(
        config.as_ref(),
        instance_name.as_deref(),
        bundle_name.as_deref(),
    )
    .map_err(api_err)?;
    let invalid = || format!("Invalid arguments for command: {command}");
    let arg = |i: usize| args.get(i).map(String::as_str).ok_or_else(invalid);
    // The cache key: the concrete instance name, never empty.
    let instance_key = || -> String {
        instance_name
            .clone()
            .filter(|n| !n.is_empty())
            .or_else(|| {
                config
                    .as_ref()
                    .and_then(|c| c.get("default"))
                    .and_then(Value::as_str)
                    .filter(|n| !n.is_empty())
                    .map(str::to_owned)
            })
            .or_else(|| std::env::var("GITLAB_INSTANCE").ok())
            .unwrap_or_else(|| "default".to_string())
    };

    match command.as_str() {
        "whoami" => pretty(&client.current_user().map_err(api_err)?),
        "get-issue" => pretty(&client.get_issue(arg(1)?, int(arg(2)?)?).map_err(api_err)?),
        "list-issues" => {
            let (positional, flags) = pop_kv_flags(&args[1..], &["assignee", "author"]);
            let project = positional.first().ok_or_else(invalid)?;
            let state = positional.get(1).map(String::as_str).unwrap_or("opened");
            let labels: Vec<String> = positional.iter().skip(2).cloned().collect();
            let issues = client
                .list_issues_filtered(
                    project,
                    state,
                    &labels,
                    flag(&flags, "assignee"),
                    flag(&flags, "author"),
                )
                .map_err(api_err)?;
            pretty(&Value::Array(issues))
        }
        "get-mr" => pretty(
            &client
                .get_merge_request(arg(1)?, int(arg(2)?)?)
                .map_err(api_err)?,
        ),
        "list-mrs" => {
            let (positional, flags) = pop_kv_flags(&args[1..], &["assignee", "author"]);
            let project = positional.first().ok_or_else(invalid)?;
            let state = positional.get(1).map(String::as_str).unwrap_or("opened");
            let mrs = client
                .list_merge_requests(
                    project,
                    state,
                    flag(&flags, "assignee"),
                    flag(&flags, "author"),
                )
                .map_err(api_err)?;
            pretty(&Value::Array(mrs))
        }
        "post-issue-comment" => pretty(
            &client
                .post_issue_comment(arg(1)?, int(arg(2)?)?, arg(3)?)
                .map_err(api_err)?,
        ),
        "post-mr-comment" => pretty(
            &client
                .post_mr_comment(arg(1)?, int(arg(2)?)?, arg(3)?)
                .map_err(api_err)?,
        ),
        "get-diff" => Ok(Some(
            client
                .get_diff_content(arg(1)?, int(arg(2)?)?)
                .map_err(api_err)?,
        )),
        "aggregate-issues" => {
            let project = arg(1)?;
            if let Some(days) = args.get(2) {
                int(days)?;
            }
            pretty(&client.aggregate_issues(project).map_err(api_err)?)
        }
        "sync-issue" => {
            let (project, iid) = (arg(1)?, int(arg(2)?)?);
            let fresh = client.get_issue(project, iid).map_err(api_err)?;
            let merged =
                gitlab_cache::sync_issue(&instance_key(), project, &iid.to_string(), &fresh)
                    .map_err(|e| e.to_string())?;
            pretty(&merged)
        }
        "sync-project" => {
            let project = arg(1)?;
            let key = instance_key();
            let mut fresh = client.get_project(project).map_err(api_err)?;
            let members = client.get_project_members(project).map_err(api_err)?;
            fresh["members"] = members.clone();
            let merged =
                gitlab_cache::upsert_project(&key, project, &fresh).map_err(|e| e.to_string())?;
            gitlab_cache::upsert_users(&key, members.as_array().map_or(&[][..], Vec::as_slice))
                .map_err(|e| e.to_string())?;
            let namespace = fresh.get("namespace").cloned().unwrap_or(Value::Null);
            if namespace.get("kind").and_then(Value::as_str) == Some("group") {
                if let Some(path) = namespace.get("full_path").and_then(Value::as_str) {
                    sync_group(&client, &key, path)?;
                }
            }
            pretty(&merged)
        }
        "sync-group" => {
            let path = arg(1)?;
            let merged = sync_group(&client, &instance_key(), path)?;
            pretty(&merged)
        }
        "cached-issue" => {
            let (project, iid) = (arg(1)?, int(arg(2)?)?);
            match gitlab_cache::get_issue_cache(&instance_key(), project, &iid.to_string()) {
                Some(v) => pretty(&v),
                None => Err(format!(
                    "No cache for {project}#{iid} \u{2014} run sync-issue first"
                )),
            }
        }
        other => Err(format!("Unknown command: {other}")),
    }
}

fn sync_group(client: &Client, instance: &str, path: &str) -> Result<Value, String> {
    let mut fresh = client.get_group(path).map_err(api_err)?;
    let members = client.get_group_members(path).map_err(api_err)?;
    fresh["members"] = members.clone();
    let merged = gitlab_cache::upsert_group(instance, path, &fresh).map_err(|e| e.to_string())?;
    gitlab_cache::upsert_users(instance, members.as_array().map_or(&[][..], Vec::as_slice))
        .map_err(|e| e.to_string())?;
    Ok(merged)
}

fn list_instances(config: Option<&Value>) -> Out {
    let Some(config) = config else {
        return Ok(Some(
            "No GitLab configuration file found.\n\nSearched locations:\n  - ./gitlab_config.json\n  - ~/.gitlab/config.json\n\nCreate a gitlab_config.json file (see gitlab_config.json.template)"
                .to_string(),
        ));
    };
    let default = config.get("default").and_then(Value::as_str).unwrap_or("");
    let mut out = format!("Default instance: {default}\n\nAvailable instances:");
    for (name, inst) in config
        .get("instances")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let text = |k: &str, d: &str| inst.get(k).and_then(Value::as_str).unwrap_or(d).to_string();
        let marker = if name == default { " (default)" } else { "" };
        out.push_str(&format!(
            "\n  - {name}{marker}\n    URL: {}\n    Description: {}",
            text("url", "No URL"),
            text("description", "No description")
        ));
    }
    Ok(Some(out))
}

fn list_projects(config: Option<&Value>) -> Out {
    let projects = config
        .and_then(|c| c.get("projects"))
        .and_then(Value::as_object)
        .filter(|p| !p.is_empty());
    let Some(projects) = projects else {
        return Ok(Some(
            "No project aliases configured.\n\nAdd a 'projects' section to your gitlab_config.json"
                .to_string(),
        ));
    };
    let mut out = String::from("Available project aliases:");
    for (alias, project) in projects {
        let text = |k: &str, d: &str| {
            project
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or(d)
                .to_string()
        };
        out.push_str(&format!(
            "\n  - {alias}\n    Project ID: {}\n    Instance: {}\n    Description: {}\n",
            text("project_id", "No ID"),
            text("instance", "default"),
            text("description", "No description")
        ));
    }
    Ok(Some(out))
}

/// `loopx gitlab cache ...` (the gitlab_cache.py CLI).
fn cache_cmd(args: &[String]) -> Out {
    let Some(command) = args.first() else {
        return Err("Usage: loopx gitlab cache <get-issue|get-users|get-group|get-project|annotate-issue|annotate-project> [args...]".into());
    };
    let bad = || format!("Invalid arguments for command: {command}");
    let arg = |i: usize| args.get(i).map(String::as_str).ok_or_else(bad);
    match command.as_str() {
        "get-issue" => match gitlab_cache::get_issue_cache(arg(1)?, arg(2)?, arg(3)?) {
            Some(v) => pretty(&v),
            None => Ok(Some("null".into())),
        },
        "get-users" => pretty(&gitlab_cache::get_users(arg(1)?)),
        "get-group" => pretty(&gitlab_cache::get_group(arg(1)?, arg(2)?)),
        "get-project" => pretty(&gitlab_cache::get_project(arg(1)?, arg(2)?)),
        "annotate-issue" => {
            let (i, p, iid, k, v) = (arg(1)?, arg(2)?, arg(3)?, arg(4)?, arg(5)?);
            gitlab_cache::annotate_issue(i, p, iid, k, json!(v)).map_err(|e| e.to_string())?;
            pretty(&gitlab_cache::read_json(&gitlab_cache::issue_path(
                i, p, iid,
            )))
        }
        "annotate-project" => {
            let (i, p, k, v) = (arg(1)?, arg(2)?, arg(3)?, arg(4)?);
            gitlab_cache::annotate_project(i, p, k, json!(v)).map_err(|e| e.to_string())?;
            pretty(&gitlab_cache::get_project(i, p))
        }
        other => Err(format!("Unknown command: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn kv_flags_can_appear_anywhere() {
        let (pos, flags) = pop_kv_flags(
            &s(&["p", "--assignee=me", "opened", "--author=you"]),
            &["assignee", "author"],
        );
        assert_eq!(pos, vec!["p", "opened"]);
        assert_eq!(flag(&flags, "assignee"), Some("me"));
        assert_eq!(flag(&flags, "author"), Some("you"));
    }

    #[test]
    fn no_args_is_usage() {
        assert!(dispatch(&[])
            .unwrap_err()
            .starts_with("Usage: loopx gitlab"));
    }

    #[test]
    fn instance_flag_without_command_errors() {
        assert_eq!(
            dispatch(&s(&["--instance=x"])).unwrap_err(),
            "Error: No command specified"
        );
    }
}
