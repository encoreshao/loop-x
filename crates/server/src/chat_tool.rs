//! `loopx chat-tool <action> [args]`: the one command the dashboard chat's
//! `claude -p` subprocess may run (see `loopx_core::chat`). The action list
//! here IS the capability surface granted to the assistant, so every branch is
//! a thin, safe wrapper around an existing internal function - never a new
//! capability invented for chat. Prints the result as pretty JSON.

use crate::ui::state::UiState;
use loopx_core::chat_tools::{self, Env as ToolsEnv};
use loopx_core::connectors_config::{self, Paths};
use loopx_core::run_control::{self, Launcher, TriggerOutcome};
use loopx_core::{
    chat, inbox_config, inbox_seen, inbox_status, issue_tracking_config, launchd, loops_config,
    memory_store, pyjson, run_history, status, topics,
};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Allowed `key=value` fields. Deliberately excludes install/lint/test
/// commands: the loop runs those as shell commands and this assistant's
/// context can carry third-party GitLab text, so a prompt-injected reply must
/// never set them.
const TOPIC_FIELDS: [&str; 4] = ["name", "label", "brief", "slack_bundle"];
const PROJECT_FIELDS: [&str; 5] = [
    "alias",
    "project_id",
    "local_path",
    "target_branch",
    "instance",
];
const CONNECTOR_SETTINGS_KEYS: [&str; 4] = ["url", "api_url", "site_url", "format"];
/// The daemons chat must never disable: doing so would kill the process
/// serving the reply with no way to re-enable it from the dead dashboard.
const PROTECTED_DAEMONS: [&str; 2] = [
    "com.hermes.loopx.backend.plist",
    "com.hermes.loop-engineering-dashboard.plist",
];

/// One CLI call: `Ok(result)` is printed as JSON, `Err(usage)` goes to stderr
/// with exit code 1.
pub fn run(state: &UiState, action: &str, args: &[String], session: &str) -> Result<Value, String> {
    let usage = |text: &str| Err(format!("Usage: chat-tool {text}"));
    match action {
        "status" => Ok(status_snapshot(state)),
        "history-list" => Ok(json!(run_history::list_run_history(&history_dir(state)))),
        "history-read" => match args.first() {
            None => usage("history-read <name>"),
            Some(name) => Ok(
                match run_history::read_history_file(name, &history_dir(state)) {
                    Some(content) => json!({ "content": content }),
                    None => json!({ "error": format!("No history entry named {}", py_repr(name)) }),
                },
            ),
        },
        "history-delete" => match args.first() {
            None => usage("history-delete <name>"),
            Some(name) => Ok(history_delete(&history_dir(state), name)),
        },
        "memory" => Ok(project_memory(state)),
        "progress" => Ok(
            match std::fs::read_to_string(state.repo_root.join("PROGRESS.md")) {
                Ok(content) => json!({ "content": content }),
                Err(e) => json!({ "error": e.to_string() }),
            },
        ),
        "daemon-list" => Ok(daemon_list(state)),
        "daemon-enable" | "daemon-disable" => match args.first() {
            None => usage(&format!("{action} <filename>")),
            Some(file) => Ok(daemon_toggle(state, file, action == "daemon-enable")),
        },
        "inbox-status" => Ok(inbox_status_view(state)),
        "run-now" => match args.first() {
            None => usage("run-now <gitlab|topic-monitor|inbox-triage>"),
            Some(kind) => Ok(run_now(state, kind)),
        },
        "run-issue" => match args.first() {
            None => usage("run-issue <gitlab issue url>"),
            Some(url) => Ok(run_issue(state, url)),
        },
        "topic-list" => {
            Ok(json!({ "topics": topics::load(&topics_path(state)).unwrap_or_default() }))
        }
        "topic-save" | "project-save" => match parse_fields(args) {
            Err(e) => Err(format!("Usage: chat-tool {action} key=value ... ({e})")),
            Ok(fields) if action == "topic-save" => Ok(topic_save(state, &fields)),
            Ok(fields) => Ok(project_save(state, &fields)),
        },
        "project-list" => Ok(project_list(state)),
        "connector-list" => Ok(connector_list(state)),
        "loop-list" => Ok(
            match loops_config::list_loops(
                &state.loops_config_path(),
                Some(&state.loops_template_path()),
            ) {
                Ok(loops) => json!({ "loops": loops }),
                Err(e) => json!({ "error": e.to_string() }),
            },
        ),
        "topic-enable" | "topic-disable" | "loop-enable" | "loop-disable" | "inbox-enable"
        | "inbox-disable" => match args.first() {
            None => usage(&format!("{action} <name>")),
            Some(name) => {
                let (kind, verb) = action.split_once('-').unwrap_or((action, ""));
                Ok(set_enabled(state, kind, name, verb == "enable"))
            }
        },
        "issue-enable" | "issue-disable" => match (args.first(), args.get(1)) {
            (Some(alias), Some(iid))
                if !iid.is_empty() && iid.chars().all(|c| c.is_ascii_digit()) =>
            {
                let iid: i64 = iid.parse().map_err(|_| "invalid issue iid".to_string())?;
                Ok(outcome(
                    issue_tracking_config::set_issue_enabled(
                        alias,
                        iid,
                        action == "issue-enable",
                        &issue_tracking_path(state),
                    )
                    .map(|m| (true, m))
                    .unwrap_or_else(|m| (false, m)),
                ))
            }
            _ => usage(&format!("{action} <project alias> <issue iid>")),
        },
        "call" => match args.first() {
            None => usage("call <tool> <json>"),
            Some(tool) => {
                let env = tools_env(state);
                let args_json = args.get(1).map_or("{}", String::as_str);
                let result = chat_tools::run(&env, tool, args_json, session);
                // Tool name and outcome only: the args may hold issue or email text.
                let body = match result.get("error") {
                    Some(e) => format!("error: {}", loopx_core::pydumps::py_str(e)),
                    None => "ok".to_string(),
                };
                chat::append_unified_log(
                    &state.repo_root,
                    "chat-tool",
                    &format!("call {tool}"),
                    Some(&body),
                );
                Ok(result)
            }
        },
        other => Err(format!("Unknown chat-tool action: {}", py_repr(other))),
    }
}

pub fn render(result: &Value) -> String {
    pyjson::to_string_pretty(result)
}

/// The tools registry's environment (also what the chat page approves with).
pub fn tools_env(state: &UiState) -> ToolsEnv {
    ToolsEnv::new(&state.repo_root, &state.home_dir)
}

fn py_repr(s: &str) -> String {
    loopx_core::loops_config::py_repr_str(s)
}

fn outcome((ok, message): (bool, String)) -> Value {
    json!({ "ok": ok, "message": message })
}

fn history_dir(state: &UiState) -> PathBuf {
    state.repo_root.join("outputs").join("history")
}

fn topics_path(state: &UiState) -> PathBuf {
    state.home_dir.join("topics.json")
}

fn projects_path(state: &UiState) -> PathBuf {
    state.home_dir.join("projects.json")
}

fn issue_tracking_path(state: &UiState) -> PathBuf {
    state.home_dir.join("issue_tracking.json")
}

fn connector_paths(state: &UiState) -> Paths {
    Paths {
        config_path: Some(state.home_dir.join("connectors.json")),
        gitlab_config_path: Some(state.user_home.join(".gitlab").join("config.json")),
        slack_config_path: Some(state.user_home.join(".slack").join("config.json")),
        inbox_config_path: Some(state.home_dir.join("inboxes.json")),
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn read_object(path: &Path) -> Value {
    read_json(path)
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn write_json_atomic(value: &Value, path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, pyjson::to_string_pretty(value)).map_err(|e| e.to_string())?;
    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn topic_status_path(state: &UiState) -> PathBuf {
    state
        .repo_root
        .join("outputs")
        .join("topic-monitor")
        .join("status.json")
}

fn status_snapshot(state: &UiState) -> Value {
    let projects = read_object(&projects_path(state));
    let tracked: Vec<String> = projects
        .get("projects")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    json!({
        "gitlab_loop": status::read_status(&state.status_path()),
        "topic_monitor": status::read_topic_status(&topic_status_path(state)),
        "configured_topics": topics::load(&topics_path(state)).unwrap_or_default(),
        "tracked_projects": tracked,
    })
}

/// Soft delete: the entry moves into `<history_dir>/.trash/`. Chat's prompt can
/// carry third-party text, so a prompt-injected delete must stay recoverable.
fn history_delete(dir: &Path, name: &str) -> Value {
    let safe = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !safe.ends_with(".md") {
        return json!({ "ok": false, "message": format!("Invalid history filename: {}", py_repr(name)) });
    }
    let path = dir.join(&safe);
    if !path.is_file() {
        return json!({ "ok": false, "message": format!("{safe} not found") });
    }
    let trash = dir.join(".trash");
    if let Err(e) = std::fs::create_dir_all(&trash) {
        return json!({ "ok": false, "message": e.to_string() });
    }
    let mut target = trash.join(&safe);
    if target.exists() {
        let stem = safe.trim_end_matches(".md");
        let stamp = chrono::Local::now().format("%Y%m%d%H%M%S%6f");
        target = trash.join(format!("{stem}.{stamp}.md"));
    }
    if let Err(e) = std::fs::rename(&path, &target) {
        return json!({ "ok": false, "message": e.to_string() });
    }
    let shown = target
        .strip_prefix(dir)
        .unwrap_or(&target)
        .display()
        .to_string();
    json!({ "ok": true, "message": format!("Moved {safe} to {shown} (recoverable)") })
}

fn project_memory(state: &UiState) -> Value {
    let projects = read_object(&projects_path(state));
    let default_instance = projects
        .get("gitlab_instance")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let root = state.home_dir.join("memory");
    let mut out = Map::new();
    if let Some(map) = projects.get("projects").and_then(Value::as_object) {
        for (alias, project) in map {
            let instance = project
                .get("instance")
                .and_then(Value::as_str)
                .unwrap_or(&default_instance);
            let project_id = loopx_core::pydumps::py_str(&project["project_id"]);
            let legacy = read_json(
                &state
                    .user_home
                    .join(".gitlab")
                    .join("cache")
                    .join(instance)
                    .join("projects")
                    .join(project_id.replace('/', "__"))
                    .join("project.json"),
            )
            .and_then(|v| v.get("_memory")?.get("fix_learnings")?.as_array().cloned())
            .unwrap_or_default();
            let tasks: Vec<Value> = memory_store::list_task_memories(alias, Some(&root))
                .iter()
                .map(memory_store::TaskMemory::to_value)
                .collect();
            out.insert(alias.clone(), json!({ "legacy": legacy, "tasks": tasks }));
        }
    }
    Value::Object(out)
}

fn launchctl(state: &UiState) -> launchd::Launchctl {
    launchd::Launchctl {
        command: state.launchctl.clone(),
        list_file: Some(state.repo_root.join("outputs").join("launchctl_list.txt")),
    }
}

fn daemon_list(state: &UiState) -> Value {
    let ctl = launchctl(state);
    let daemons = launchd::get_daemons_status(&state.repo_root.join("launchd"), &ctl.list_output());
    Value::Array(
        daemons
            .into_iter()
            .map(|d| {
                let mut e = Map::new();
                e.insert("file".into(), json!(d.file));
                if let Some(err) = d.error {
                    e.insert("error".into(), json!(err));
                    return Value::Object(e);
                }
                e.insert("label".into(), json!(d.label));
                e.insert("loaded".into(), json!(d.loaded));
                e.insert("pid".into(), json!(d.pid));
                e.insert("program_arguments".into(), json!(d.program_arguments));
                e.insert("run_at_load".into(), json!(d.run_at_load));
                e.insert("keep_alive".into(), json!(d.keep_alive));
                e.insert("schedule".into(), json!(d.schedule));
                e.insert("start_interval".into(), json!(d.start_interval));
                Value::Object(e)
            })
            .collect(),
    )
}

fn daemon_toggle(state: &UiState, file: &str, enable: bool) -> Value {
    let name = Path::new(file)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if !enable && PROTECTED_DAEMONS.contains(&name.as_str()) {
        return json!({
            "ok": false,
            "message": format!(
                "Refusing to disable {file} from chat - that's the dashboard's own daemon, and \
                 disabling it would kill the process serving this reply with no way to re-enable \
                 it from here. Disable it manually (launchctl unload -w) if you really want to."
            ),
        });
    }
    let dir = state.repo_root.join("launchd");
    let agents = launchd::launch_agents_dir(&state.user_home);
    let ctl = launchctl(state);
    let (ok, msg) = if enable {
        launchd::enable_daemon(file, &dir, &agents, &ctl)
    } else {
        launchd::disable_daemon(file, &dir, &agents, &ctl)
    };
    json!({ "ok": ok, "message": msg.english() })
}

fn inbox_status_view(state: &UiState) -> Value {
    let config = match inbox_config::load_config_or_empty(&state.home_dir.join("inboxes.json")) {
        Ok(c) => c,
        Err(e) => return json!({ "error": format!("Could not read inbox config: {e}") }),
    };
    let entries =
        inbox_status::read(&inbox_seen::output_dir_for(&state.repo_root).join("status.json"));
    let entries = entries.get("inboxes").cloned().unwrap_or_else(|| json!({}));
    let list: Vec<Value> = config["inboxes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|inbox| {
            let entry = entries
                .get(inbox["name"].as_str().unwrap_or(""))
                .cloned()
                .unwrap_or(json!({}));
            let urgent: Vec<Value> = entry["urgent"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|item| {
                    json!({
                        "from": item.get("from").cloned().unwrap_or(json!("")),
                        "subject": item.get("subject").cloned().unwrap_or(json!("")),
                        "has_draft": loopx_core::loops_config::py_truthy(&item["draft_link"]),
                        "draft_failed": loopx_core::loops_config::py_truthy(&item["draft_failed"]),
                    })
                })
                .collect();
            json!({
                "name": inbox["name"],
                "label": inbox["label"],
                "provider": inbox["provider"],
                "account": inbox["account"],
                "enabled": inbox.get("enabled").cloned().unwrap_or(json!(true)),
                "state": entry.get("state").cloned().unwrap_or(Value::Null),
                "last_run_at": entry.get("last_run_at").cloned().unwrap_or(Value::Null),
                "counts": if loopx_core::loops_config::py_truthy(&entry["counts"]) { entry["counts"].clone() } else { json!({}) },
                "error": entry.get("error").cloned().unwrap_or(Value::Null),
                "urgent": urgent,
            })
        })
        .collect();
    json!({ "inboxes": list })
}

fn trigger_message(outcome: TriggerOutcome, started: &str) -> Value {
    match outcome {
        TriggerOutcome::AlreadyRunning => {
            json!({ "ok": false, "message": "A run is already in progress" })
        }
        TriggerOutcome::LauncherMissing(path) => {
            json!({ "ok": false, "message": format!("run-loop-now.sh not found at {path}") })
        }
        TriggerOutcome::Started => json!({ "ok": true, "message": started }),
    }
}

const STARTED: &str = "Run started - check back here for progress";

fn run_now(state: &UiState, kind: &str) -> Value {
    let fallback = state.repo_root.join("run-loop-now.sh");
    let launcher = Launcher {
        cmd: &state.run_cmd,
        cwd: &state.repo_root,
        fallback_path: &fallback,
    };
    match kind {
        "gitlab" => trigger_message(
            run_control::trigger_manual_run(&state.status_path(), "gitlab-loop", &launcher),
            STARTED,
        ),
        "topic-monitor" => trigger_message(
            run_control::trigger_topic_monitor_run(
                &topic_status_path(state),
                "topic-loop",
                &launcher,
            ),
            STARTED,
        ),
        "inbox-triage" => trigger_message(
            run_control::trigger_inbox_triage_run(
                &state.status_path_for_loop("inbox-triage-loop"),
                "inbox-triage-loop",
                &launcher,
            ),
            "Inbox triage started - check back here for results",
        ),
        other => json!({
            "error": format!(
                "Unknown run-now kind {} - expected 'gitlab', 'topic-monitor' or 'inbox-triage'",
                py_repr(other)
            )
        }),
    }
}

/// Matches a pasted GitLab issue URL against the tracked projects' issue URL
/// prefixes (`/-/issues/N` and the newer `/-/work_items/N`).
fn resolve_issue_url(
    url: &str,
    prefixes: &crate::ui::markdown::GitlabPrefixes,
) -> Option<(String, i64)> {
    let url = url.trim();
    for (alias, base) in prefixes.iter() {
        let pattern = format!(
            r"^{}/-/(?:issues|work_items)/(\d+)/?$",
            regex::escape(base.trim_end_matches('/'))
        );
        if let Some(caps) = Regex::new(&pattern).ok()?.captures(url) {
            return Some((alias.clone(), caps[1].parse().ok()?));
        }
    }
    None
}

fn run_issue(state: &UiState, url: &str) -> Value {
    let prefixes = crate::ui::gitlab::issue_url_prefixes(state);
    let Some((alias, iid)) = resolve_issue_url(url, &prefixes) else {
        return json!({ "ok": false, "message": format!("Could not match {} to a tracked project", py_repr(url)) });
    };
    if status::read_status(&state.status_path())
        .get("state")
        .and_then(Value::as_str)
        == Some("running")
    {
        return json!({ "ok": false, "message": "A run is already in progress" });
    }
    let fallback = state.repo_root.join("run-loop-now.sh");
    let Some((program, rest)) = state.run_cmd.split_first() else {
        return json!({ "ok": false, "message": format!("run-loop-now.sh not found at {}", fallback.display()) });
    };
    let mut command = std::process::Command::new(program);
    command
        .args(rest)
        .args(["gitlab-loop", &alias, &iid.to_string()])
        .current_dir(&state.repo_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    match command.spawn() {
        Ok(child) => {
            std::mem::forget(child);
            json!({ "ok": true, "message": format!("Started work on {alias} #{iid}") })
        }
        Err(_) => {
            json!({ "ok": false, "message": format!("run-loop-now.sh not found at {program}") })
        }
    }
}

/// `key=value` words (one per shell word) into pairs; a word with no `=` is an
/// error, so a positional value can never land in the wrong field.
fn parse_fields(args: &[String]) -> Result<Vec<(String, String)>, String> {
    args.iter()
        .map(|a| match a.split_once('=') {
            Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
            _ => Err(format!("Expected key=value, got {}", py_repr(a))),
        })
        .collect()
}

fn unknown_fields(fields: &[(String, String)], allowed: &[&str]) -> Option<Value> {
    let mut unknown: Vec<&str> = fields
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| !allowed.contains(k))
        .collect();
    unknown.sort_unstable();
    unknown.dedup();
    if unknown.is_empty() {
        return None;
    }
    Some(json!({
        "ok": false,
        "message": format!("Unknown field(s): {} - allowed: {}", unknown.join(", "), allowed.join(", ")),
    }))
}

fn field<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn topic_save(state: &UiState, fields: &[(String, String)]) -> Value {
    if let Some(refused) = unknown_fields(fields, &TOPIC_FIELDS) {
        return refused;
    }
    let get = |k: &str| field(fields, k).unwrap_or("");
    outcome(topics::upsert_topic(
        get("name"),
        get("label"),
        get("brief"),
        get("slack_bundle"),
        &topics_path(state),
    ))
}

/// Adds a tracked GitLab project or updates one by alias, keeping any field not
/// given (and always the command fields) from the existing entry.
fn project_save(state: &UiState, fields: &[(String, String)]) -> Value {
    if let Some(refused) = unknown_fields(fields, &PROJECT_FIELDS) {
        return refused;
    }
    let path = projects_path(state);
    let alias = field(fields, "alias").unwrap_or("").trim().to_string();
    let mut config = read_object(&path);
    let existing = config["projects"][&alias].clone();
    let pick = |key: &str| -> String {
        match field(fields, key) {
            Some(v) => v.trim().to_string(),
            None => existing
                .get(key)
                .map(loopx_core::pydumps::py_str)
                .unwrap_or_default(),
        }
    };
    let project_id = pick("project_id");
    if alias.is_empty() {
        return json!({ "ok": false, "message": "Project alias is required" });
    }
    if project_id.is_empty() {
        return json!({ "ok": false, "message": "Project ID is required" });
    }
    let instance = pick("instance");
    let mut entry = json!({
        "project_id": project_id,
        "local_path": pick("local_path"),
        "target_branch": pick("target_branch"),
        "install_cmd": pick("install_cmd"),
        "lint_cmd": pick("lint_cmd"),
        "test_cmd": pick("test_cmd"),
    });
    if !instance.is_empty() {
        entry["instance"] = json!(instance);
    }
    if !config["projects"].is_object() {
        config["projects"] = json!({});
    }
    let is_new = !config["projects"]
        .as_object()
        .is_some_and(|m| m.contains_key(&alias));
    config["projects"][&alias] = entry;
    if let Err(e) = write_json_atomic(&config, &path) {
        return json!({ "ok": false, "message": format!("Could not save: {e}") });
    }
    json!({ "ok": true, "message": format!("{} project {alias}", if is_new { "Added" } else { "Updated" }) })
}

fn project_list(state: &UiState) -> Value {
    let config = read_object(&projects_path(state));
    let gitlab = read_object(&state.user_home.join(".gitlab").join("config.json"));
    let instances: Map<String, Value> = gitlab
        .get("instances")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(name, entry)| (name.clone(), entry.get("url").cloned().unwrap_or(json!(""))))
                .collect()
        })
        .unwrap_or_default();
    let default_instance = match config.get("gitlab_instance") {
        Some(v) if loopx_core::loops_config::py_truthy(v) => v.clone(),
        _ => gitlab.get("default").cloned().unwrap_or(Value::Null),
    };
    json!({
        "projects": config.get("projects").cloned().unwrap_or(json!({})),
        "default_instance": default_instance,
        "assignee_username": config.get("assignee_username").cloned().unwrap_or(Value::Null),
        "worktree_root": config.get("worktree_root").cloned().unwrap_or(Value::Null),
        "gitlab_instances": instances,
    })
}

/// Configured connector accounts. Read-only, and `settings` is cut down to a
/// fixed allowlist of non-secret keys so no credential-adjacent value can reach
/// the model.
fn connector_list(state: &UiState) -> Value {
    let accounts = match connectors_config::list_accounts(&connector_paths(state)) {
        Ok(a) => a,
        Err(e) => return json!({ "error": format!("Could not read connectors: {}", e.0) }),
    };
    Value::Array(
        accounts
            .iter()
            .map(|a| {
                let mut caps: Vec<&str> = loopx_core::connectors::get_type(&a.kind)
                    .map(|t| t.capabilities.to_vec())
                    .unwrap_or_default();
                caps.sort_unstable();
                let settings: Map<String, Value> = CONNECTOR_SETTINGS_KEYS
                    .iter()
                    .filter_map(|k| Some((k.to_string(), a.settings.get(*k)?.clone())))
                    .collect();
                json!({
                    "id": a.id, "type": a.kind, "label": a.label, "capabilities": caps,
                    "managed_by": a.managed_by, "settings": settings,
                })
            })
            .collect(),
    )
}

fn set_enabled(state: &UiState, kind: &str, name: &str, enabled: bool) -> Value {
    match kind {
        "topic" => outcome(topics::set_enabled(name, enabled, &topics_path(state))),
        "inbox" => outcome(inbox_config::set_enabled(
            name,
            enabled,
            &state.home_dir.join("inboxes.json"),
        )),
        _ => loop_set_enabled(state, name, enabled),
    }
}

/// Enabling a registered loop is gated on its `requires` capabilities having an
/// enabled connector account, like the Daemons page.
fn loop_set_enabled(state: &UiState, name: &str, enabled: bool) -> Value {
    let (cfg, tpl) = (state.loops_config_path(), state.loops_template_path());
    if enabled {
        let missing = loop_requirements_missing(state, name);
        if !missing.is_empty() {
            return json!({
                "ok": false,
                "message": format!(
                    "Cannot enable {name}: missing connector for {}",
                    missing.join(", ")
                ),
            });
        }
    }
    match loops_config::set_enabled(name, enabled, &cfg, Some(&tpl)) {
        Ok(message) => json!({ "ok": true, "message": message }),
        Err(message) => json!({ "ok": false, "message": message }),
    }
}

fn loop_requirements_missing(state: &UiState, name: &str) -> Vec<String> {
    let Ok(loop_) = loops_config::get_loop(
        name,
        &state.loops_config_path(),
        Some(&state.loops_template_path()),
    ) else {
        return Vec::new();
    };
    let Some(Value::Array(requires)) = loop_.get("requires") else {
        return Vec::new();
    };
    // A malformed connectors.json counts as "no accounts".
    let accounts = connectors_config::list_accounts(&connector_paths(state)).unwrap_or_default();
    requires
        .iter()
        .map(loops_config::py_str)
        .filter(|cap| {
            !accounts.iter().any(|a| {
                a.enabled
                    && loopx_core::connectors::get_type(&a.kind)
                        .is_some_and(|t| t.has_capability(cap))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(tmp: &Path) -> UiState {
        UiState::explicit(
            &tmp.join("repo"),
            &tmp.join("home"),
            &tmp.join("user"),
            &tmp.join("update"),
            "tok",
            None,
        )
    }

    fn call(state: &UiState, action: &str, args: &[&str]) -> Result<Value, String> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        run(state, action, &args, "")
    }

    #[test]
    fn unknown_action_and_missing_args_are_usage_errors() {
        let d = tempfile::tempdir().unwrap();
        let s = state(d.path());
        assert_eq!(
            call(&s, "nope", &[]).unwrap_err(),
            "Unknown chat-tool action: 'nope'"
        );
        assert!(call(&s, "history-read", &[])
            .unwrap_err()
            .starts_with("Usage: chat-tool history-read"));
        assert!(call(&s, "issue-enable", &["web", "x"])
            .unwrap_err()
            .contains("<issue iid>"));
        assert!(call(&s, "topic-save", &["bare"])
            .unwrap_err()
            .contains("Expected key=value"));
    }

    #[test]
    fn project_save_adds_updates_and_never_takes_command_fields() {
        let d = tempfile::tempdir().unwrap();
        let s = state(d.path());
        let added = call(&s, "project-save", &["alias=web", "project_id=g/web"]).unwrap();
        assert_eq!(added["message"], "Added project web");
        let refused = call(&s, "project-save", &["alias=web", "lint_cmd=rm -rf /"]).unwrap();
        assert_eq!(refused["ok"], false);
        let cfg = read_object(&projects_path(&s));
        assert_eq!(cfg["projects"]["web"]["lint_cmd"], "");
        let updated = call(&s, "project-save", &["alias=web", "target_branch=main"]).unwrap();
        assert_eq!(updated["message"], "Updated project web");
        let cfg = read_object(&projects_path(&s));
        assert_eq!(cfg["projects"]["web"]["project_id"], "g/web");
        assert_eq!(cfg["projects"]["web"]["target_branch"], "main");
    }

    #[test]
    fn topic_save_then_list_and_disable() {
        let d = tempfile::tempdir().unwrap();
        let s = state(d.path());
        assert_eq!(
            call(&s, "topic-save", &["name=ai", "label=AI", "brief=news"]).unwrap()["ok"],
            true
        );
        assert_eq!(
            call(&s, "topic-list", &[]).unwrap()["topics"][0]["name"],
            "ai"
        );
        assert_eq!(call(&s, "topic-disable", &["ai"]).unwrap()["ok"], true);
        assert_eq!(
            call(&s, "topic-list", &[]).unwrap()["topics"][0]["enabled"],
            false
        );
    }

    #[test]
    fn history_delete_is_recoverable_and_confined_to_md_names() {
        let d = tempfile::tempdir().unwrap();
        let s = state(d.path());
        let dir = history_dir(&s);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-01-01.md"), "x").unwrap();
        assert_eq!(
            call(&s, "history-list", &[]).unwrap(),
            json!(["2026-01-01.md"])
        );
        assert_eq!(
            call(&s, "history-delete", &["../../etc/passwd"]).unwrap()["ok"],
            false
        );
        let done = call(&s, "history-delete", &["2026-01-01.md"]).unwrap();
        assert_eq!(
            done["message"],
            "Moved 2026-01-01.md to .trash/2026-01-01.md (recoverable)"
        );
        assert!(dir.join(".trash").join("2026-01-01.md").is_file());
        assert_eq!(call(&s, "history-list", &[]).unwrap(), json!([]));
    }

    #[test]
    fn chat_cannot_disable_the_backend_daemon() {
        let d = tempfile::tempdir().unwrap();
        let s = state(d.path());
        let r = call(&s, "daemon-disable", &["COM.HERMES.LOOPX.BACKEND.PLIST"]).unwrap();
        assert_eq!(r["ok"], false);
        assert!(r["message"]
            .as_str()
            .unwrap()
            .starts_with("Refusing to disable"));
    }

    #[test]
    fn issue_urls_resolve_to_tracked_aliases() {
        let mut prefixes = crate::ui::markdown::GitlabPrefixes::new();
        prefixes.insert("web".into(), "https://gl.example.com/g/web".into());
        assert_eq!(
            resolve_issue_url(" https://gl.example.com/g/web/-/issues/7 ", &prefixes),
            Some(("web".into(), 7))
        );
        assert_eq!(
            resolve_issue_url("https://gl.example.com/g/web/-/work_items/9/", &prefixes),
            Some(("web".into(), 9))
        );
        assert_eq!(
            resolve_issue_url("https://gl.example.com/g/web/-/merge_requests/9", &prefixes),
            None
        );
        assert_eq!(
            resolve_issue_url("https://other.example.com/g/web/-/issues/7", &prefixes),
            None
        );
    }

    #[test]
    fn run_now_rejects_unknown_kinds() {
        let d = tempfile::tempdir().unwrap();
        let s = state(d.path());
        assert!(call(&s, "run-now", &["bogus"]).unwrap()["error"]
            .as_str()
            .unwrap()
            .starts_with("Unknown run-now kind 'bogus'"));
    }
}
