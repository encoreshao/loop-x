//! `project_profile.read`: the saved profile of one tracked project (overview,
//! structure, stack, integrations, secret NAMES, conventions, gotchas). A pure
//! read of `<home_dir>/memory/<alias>/PROFILE.md`, so it needs no approval card.
//! The profile is written from repo content, so it is returned as untrusted
//! text, and a profile whose text looks like it holds a secret is refused.

use super::{req_str, untrusted, Arg, Env, Handler, Mode, Tool, ToolError, ToolResult};
use crate::connectors::Connector;
use crate::profile_read::{self, ReadError};
use serde_json::{json, Map, Value};

static ARGS: [Arg; 1] = [Arg::req("alias")];

fn read(env: &Env, _c: Option<&dyn Connector>, args: &Map<String, Value>) -> ToolResult {
    let alias = req_str(args, "alias")?;
    let root = env.home_dir.join("memory");
    match profile_read::profile_text(&alias, Some(&root)) {
        Ok(None) => Ok(json!({"profile": null})),
        Ok(Some(text)) => Ok(json!({"profile": untrusted(&text)})),
        Err(e @ (ReadError::Invalid(_) | ReadError::SecretRefused(_))) => {
            Err(ToolError::Input(e.to_string()))
        }
    }
}

pub(crate) fn tools() -> Vec<Tool> {
    let handler: Handler = read;
    vec![Tool {
        name: "project_profile.read",
        description: "Read the saved profile of a tracked project by its alias: overview, structure, stack, integrations, secret names (never values), conventions and gotchas. The result's profile is null when the project has none yet",
        args: &ARGS,
        mode: Mode::Read,
        connector_type: None,
        handler,
        select: None,
    }]
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::project_profile::{self, Origin, Profile, Section, SectionKey, Status};
    use std::collections::BTreeMap;

    fn env(tmp: &std::path::Path) -> Env {
        Env::new(tmp.join("repo"), tmp.join("home"))
    }

    fn save(e: &Env, alias: &str, gotchas: &str) {
        let section = |body: &str| Section {
            summary: String::new(),
            body: body.into(),
            last_verified: None,
            sources: vec![],
            origin: Origin::Agent,
        };
        let mut sections = BTreeMap::new();
        sections.insert(SectionKey::Overview, section("A synthetic shop."));
        sections.insert(SectionKey::Gotchas, section(gotchas));
        let p = Profile {
            alias: alias.into(),
            generated_at: None,
            head_sha: None,
            status: Status::Approved,
            links: vec![],
            sections,
        };
        project_profile::save(&p, Some(&e.home_dir.join("memory"))).unwrap();
    }

    #[test]
    fn chat_tool_profile_is_read_only() {
        assert_eq!(tool_mode("project_profile.read"), Some(Mode::Read));
        assert!(!write_tool_names().contains(&"project_profile.read".to_string()));
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        save(&e, "web", "Run migrations first.");
        let before = std::fs::read(e.home_dir.join("memory/web/PROFILE.md")).unwrap();
        // Even with a chat session, a read tool returns a result, never an approval card.
        let r = run(&e, "project_profile.read", r#"{"alias": "web"}"#, "s1");
        assert!(r.get("status").is_none(), "{r}");
        let text = r["result"]["profile"].as_str().unwrap();
        assert!(text.starts_with("<untrusted_content>"));
        assert!(text.contains("A synthetic shop."));
        assert_eq!(
            std::fs::read(e.home_dir.join("memory/web/PROFILE.md")).unwrap(),
            before
        );
        assert!(!e.repo_root.join("outputs/chat-pending").exists());
    }

    #[test]
    fn missing_profile_answers_null() {
        let d = tempfile::tempdir().unwrap();
        let r = run(
            &env(d.path()),
            "project_profile.read",
            r#"{"alias": "web"}"#,
            "",
        );
        assert_eq!(r["result"], json!({"profile": null}));
    }

    #[test]
    fn a_profile_holding_a_secret_is_refused_without_echoing_it() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let token = format!("glpat-{}", "a1B2c3D4e5F6g7H8i9J0");
        save(&e, "web", &format!("use {token}"));
        let r = run(&e, "project_profile.read", r#"{"alias": "web"}"#, "");
        assert!(r["error"].as_str().unwrap().contains("refused"), "{r}");
        assert!(!r.to_string().contains(&token), "{r}");
    }

    #[test]
    fn a_path_like_alias_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        let r = run(
            &env(d.path()),
            "project_profile.read",
            r#"{"alias": "../x"}"#,
            "",
        );
        assert!(r.get("error").is_some(), "{r}");
    }

    #[test]
    fn a_malformed_profile_errors_without_quoting_the_line() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let token = ["glpat-", "abcDEF123456ghiJKL78"].concat();
        let path = e.home_dir.join("memory/web/PROFILE.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("---\nalias: web\nstatus: {token}\n---\n")).unwrap();
        let r = run(&e, "project_profile.read", r#"{"alias": "web"}"#, "");
        assert!(r.get("error").is_some(), "{r}");
        assert!(
            !r.to_string().contains("glpat") && !r.to_string().contains("abcDEF"),
            "{r}"
        );
    }
}
