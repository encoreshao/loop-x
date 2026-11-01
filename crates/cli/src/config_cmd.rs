use loopx_core::command_hints::command_hints;
use loopx_core::config::{changes_code, default_config_path, use_worktree, Config, ConfigError};
use loopx_core::pyjson;
use serde_json::{json, Value};
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "Usage: loopx config <aliases|local-paths|worktree-root|assignee|instance>\n       loopx config project <alias>\n       loopx config command-hints <alias>";

pub fn run(args: &[String]) -> ExitCode {
    let Some(command) = args.first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(1);
    };
    match dispatch(command, &args[1..]) {
        Ok(out) => {
            println!("{out}");
            ExitCode::SUCCESS
        }
        Err(Failure::Usage(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
        Err(Failure::Config(e)) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

enum Failure {
    Usage(String),
    Config(ConfigError),
}

impl From<ConfigError> for Failure {
    fn from(e: ConfigError) -> Self {
        Failure::Config(e)
    }
}

fn dispatch(command: &str, rest: &[String]) -> Result<String, Failure> {
    dispatch_with(&default_config_path(), command, rest)
}

/// A project as the agent sees it: the stored commands under `hints` (they
/// are starting points, not orders), and `use_worktree` and `code_changes`
/// (false for an issues-only project, which has no checkout) always present.
fn agent_view(mut project: Value, connectors: &loopx_core::connectors_config::Paths) -> Value {
    let mut hints = serde_json::Map::new();
    if let Some(obj) = project.as_object_mut() {
        for kind in ["install", "lint", "test"] {
            let cmd = obj
                .shift_remove(&format!("{kind}_cmd"))
                .unwrap_or(json!(""));
            hints.insert(kind.into(), cmd);
        }
    }
    // A connector-bound project (GitHub, ...): its account and repository are
    // the `<instance> <project_id>` the cache and memory helpers key on.
    if loopx_core::hosted::is_hosted(&project) {
        project["instance"] = json!(loopx_core::hosted::data_instance(&project, connectors));
        project["project_id"] = project["code_host"]["repo"].clone();
    }
    project["use_worktree"] = json!(use_worktree(&project));
    project["code_changes"] = json!(changes_code(&project));
    project["hints"] = Value::Object(hints);
    project
}

fn dispatch_with(path: &Path, command: &str, rest: &[String]) -> Result<String, Failure> {
    let load = || Config::load(path);
    let alias = |usage: &str| {
        rest.first()
            .cloned()
            .ok_or_else(|| Failure::Usage(format!("Usage: loopx config {usage} <alias>")))
    };
    Ok(match command {
        "aliases" => load()?.aliases()?.join("\n"),
        "local-paths" => load()?.local_paths()?.join("\n"),
        "worktree-root" => load()?.worktree_root()?,
        "assignee" => load()?.assignee_username()?,
        "instance" => load()?.instance()?,
        "project" => {
            let alias = alias("project")?;
            // connectors.json sits beside projects.json.
            let connectors = loopx_core::connectors_config::Paths {
                config_path: path.parent().map(|d| d.join("connectors.json")),
                ..Default::default()
            };
            pyjson::to_string_pretty(&agent_view(load()?.project(&alias)?, &connectors))
        }
        "command-hints" => {
            let alias = alias("command-hints")?;
            let config = load()?;
            config.project(&alias)?;
            pyjson::to_string_pretty(&command_hints(&alias, &config, None))
        }
        other => return Err(Failure::Usage(format!("Unknown command: {other}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_output_moves_commands_under_hints() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("projects.json");
        std::fs::write(
            &p,
            json!({"gitlab_instance": "g", "projects": {"web": {
                "local_path": "/w", "target_branch": "main",
                "install_cmd": "bundle install", "lint_cmd": "", "test_cmd": "bundle exec rspec"
            }}})
            .to_string(),
        )
        .unwrap();
        let Ok(out) = dispatch_with(&p, "project", &["web".to_string()]) else {
            panic!("dispatch failed");
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["hints"],
            json!({"install": "bundle install", "lint": "", "test": "bundle exec rspec"})
        );
        assert_eq!(v["use_worktree"], true);
        assert!(v.get("test_cmd").is_none());
        assert_eq!(v["target_branch"], "main");
        assert_eq!(v["code_changes"], true);
    }

    #[test]
    fn a_code_host_project_reads_its_account_and_repo_as_instance_and_project_id() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("projects.json");
        std::fs::write(
            &p,
            json!({"gitlab_instance": "g", "projects": {"hub": {
                "local_path": "/h", "code_host": {"account": "gh-work", "repo": "o/r"}
            }}})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            d.path().join("connectors.json"),
            r#"[{"id": "gh-work", "type": "github", "settings": {}}]"#,
        )
        .unwrap();
        let Ok(out) = dispatch_with(&p, "project", &["hub".to_string()]) else {
            panic!("dispatch failed");
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        // The cache and memory helpers key on these two: `github/<account>`
        // keeps the account's data in its own folder (data/github/gh-work).
        assert_eq!(v["instance"], "github/gh-work");
        assert_eq!(v["project_id"], "o/r");
    }

    #[test]
    fn project_without_a_checkout_is_issues_only() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("projects.json");
        std::fs::write(
            &p,
            json!({"gitlab_instance": "g", "projects": {"q": {"project_id": "g/q"}}}).to_string(),
        )
        .unwrap();
        let Ok(out) = dispatch_with(&p, "project", &["q".to_string()]) else {
            panic!("dispatch failed");
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["code_changes"], false);
        assert_eq!(v["local_path"], Value::Null);
        assert_eq!(v["target_branch"], Value::Null);
        assert!(matches!(dispatch_with(&p, "local-paths", &[]), Ok(s) if s.is_empty()));
    }
}
