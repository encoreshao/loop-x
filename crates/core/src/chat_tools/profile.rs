//! `project_profile.read`: the saved profile of one tracked project (overview,
//! structure, stack, integrations, secret NAMES, conventions, gotchas). A pure
//! read of `<home_dir>/memory/<alias>/PROFILE.md`, so it needs no approval card.
//! The profile is written from repo content, so it is returned as untrusted
//! text, and a profile whose text looks like it holds a secret is refused.

use super::{
    connector_of, get, req_str, untrusted, untrusted_value, Arg, Env, Handler, Mode, Tool,
    ToolError, ToolResult,
};
use crate::connectors::http::quote;
use crate::connectors::{Account, ApiOpts, Connector};
use crate::profile_read::{self, ReadError};
use crate::profile_wiki::{self, WikiRef};
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

static WIKI_ARGS: [Arg; 2] = [Arg::req("url"), Arg::opt("alias")];

/// The GitLab account whose host is the wiki link's host; with a link that is not
/// a wiki link, the first account (the handler then refuses the link).
fn select_wiki(_e: &Env, accounts: &[Account], args: &Map<String, Value>) -> Option<String> {
    let url = args.get("url").and_then(Value::as_str).unwrap_or("");
    let Ok(r) = profile_wiki::parse_wiki_url(url) else {
        return accounts.first().map(|a| a.id.clone());
    };
    accounts
        .iter()
        .find(|a| super::gitlab::host(a).eq_ignore_ascii_case(&r.host))
        .map(|a| a.id.clone())
}

/// The tracked project a wiki link belongs to: the named alias, or the one whose
/// `project_id` is the link's project path. Returns `(alias, project_id)`.
fn project_of(env: &Env, r: &WikiRef, named: Option<&str>) -> Result<(String, String), ToolError> {
    let cfg = crate::config::Config::load(&env.home_dir.join("projects.json"))
        .map_err(|_| ToolError::input("no projects are configured"))?;
    let aliases = cfg.aliases().unwrap_or_default();
    let id_of = |alias: &str| {
        cfg.project(alias)
            .ok()
            .and_then(|p| {
                p.get("project_id")
                    .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
            })
            .unwrap_or_default()
    };
    let alias = match named.filter(|a| !a.is_empty()) {
        Some(a) if aliases.iter().any(|x| x == a) => a.to_string(),
        Some(_) => return Err(ToolError::input("unknown project alias")),
        None => aliases
            .iter()
            .find(|a| id_of(a).eq_ignore_ascii_case(&r.project_path))
            .cloned()
            .ok_or_else(|| {
                ToolError::input("no tracked project matches that wiki; pass its alias")
            })?,
    };
    let project_id = id_of(&alias);
    Ok((alias, project_id))
}

fn add_wiki(env: &Env, c: Option<&dyn Connector>, args: &Map<String, Value>) -> ToolResult {
    let c = connector_of(c)?;
    let url = req_str(args, "url")?;
    let r = profile_wiki::parse_wiki_url(&url).map_err(ToolError::input)?;
    let named = args.get("alias").and_then(Value::as_str);
    let (alias, project_id) = project_of(env, &r, named)?;
    let fetch = |w: &WikiRef| -> Result<(String, String), String> {
        let page = c
            .api(
                "GET",
                &format!(
                    "/projects/{}/wikis/{}",
                    quote(&w.project_path),
                    quote(&w.slug)
                ),
                &ApiOpts::default(),
            )
            .map_err(|e| crate::profile_sync::safe_failure_line(&e.to_string()))?;
        let text = |k: &str| get(&page, k).as_str().unwrap_or("").to_string();
        let title = text("title");
        Ok((
            if title.is_empty() {
                w.slug.clone()
            } else {
                title
            },
            text("content"),
        ))
    };
    let root = env.home_dir.join("memory");
    let added = profile_wiki::add_page(
        &alias,
        &url,
        &project_id,
        Some(&root),
        chrono::Utc::now(),
        &fetch,
    )
    .map_err(ToolError::input)?;
    if added.changed {
        crate::profile_sync::queue_wiki_refresh(&alias, Some(&root));
    }
    Ok(json!({
        "saved": true,
        "alias": alias,
        "title": untrusted_value(&Value::String(added.title)),
        "changed": added.changed,
        "note": "Proposals for the affected profile sections appear on the Memory page for the user to approve.",
    }))
}

pub(crate) fn tools() -> Vec<Tool> {
    let handler: Handler = read;
    vec![
        Tool {
            name: "project_profile.read",
            description: "Read the saved profile of a tracked project by its alias: overview, structure, stack, integrations, secret names (never values), conventions and gotchas. The result's profile is null when the project has none yet",
            args: &ARGS,
            mode: Mode::Read,
            connector_type: None,
            handler,
            select: None,
        },
        Tool {
            name: "project_profile.add_wiki",
            description: "Save one GitLab wiki page (pass its link) into a tracked project's Memory so the profile can be refreshed from it; the project is found from the link, or pass its alias. The page text is read as data only (needs the user's approval)",
            args: &WIKI_ARGS,
            mode: Mode::Write,
            connector_type: Some("gitlab"),
            handler: add_wiki,
            select: Some(select_wiki),
        },
    ]
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

    mod wiki {
        use super::super::super::testutil::*;
        use super::super::super::*;
        use serde_json::json;
        use std::collections::HashMap;
        use std::sync::Arc;

        fn accounts() -> Vec<Account> {
            vec![account("gl-main", "gitlab").with_setting("url", "https://gitlab.example.com")]
        }

        fn fake(page: Value) -> Fake {
            let mut f = Fake::new(account("x", "gitlab"));
            let routes: HashMap<String, Value> =
                HashMap::from([("/projects/g%2Fp/wikis/Runbook".to_string(), page)]);
            f.api = Some(Arc::new(move |_, path, _| {
                routes
                    .get(path)
                    .cloned()
                    .ok_or_else(|| panic!("no route {path}"))
            }));
            f
        }

        fn projects(home: &std::path::Path) {
            std::fs::create_dir_all(home).unwrap();
            std::fs::write(
                home.join("projects.json"),
                json!({"gitlab_instance": "main", "projects": {"shop": {"project_id": "g/p"}}})
                    .to_string(),
            )
            .unwrap();
        }

        const LINK: &str = "https://gitlab.example.com/g/p/-/wikis/Runbook";

        #[test]
        fn add_wiki_is_a_write_tool_so_run_only_proposes_it() {
            let f = fake(json!({"title": "Runbook", "content": "x"}));
            let d = tempfile::tempdir().unwrap();
            projects(&d.path().join("home"));
            let (e, _) = env_with(d.path(), accounts(), &f);
            let r = run(
                &e,
                "project_profile.add_wiki",
                &json!({"url": LINK}).to_string(),
                "s1",
            );
            assert_eq!(tool_mode("project_profile.add_wiki"), Some(Mode::Write));
            assert_eq!(r["status"], "pending_approval", "{r}");
            assert!(f.log().is_empty());
            assert!(!d.path().join("home/memory").exists());
        }

        #[test]
        fn an_approved_add_saves_the_page_for_the_project_found_from_the_link() {
            let f = fake(json!({"title": "Runbook", "content": "Restart with make up."}));
            let d = tempfile::tempdir().unwrap();
            projects(&d.path().join("home"));
            let (e, _) = env_with(d.path(), accounts(), &f);
            let r = execute_tool(&e, "project_profile.add_wiki", &json!({"url": LINK}));
            assert_eq!(r["result"]["saved"], true, "{r}");
            assert_eq!(r["result"]["alias"], "shop");
            let pages = crate::profile_wiki::list("shop", Some(&d.path().join("home/memory")));
            assert_eq!(pages.len(), 1);
            assert_eq!(pages[0].content, "Restart with make up.");
        }

        #[test]
        fn a_link_for_no_tracked_project_or_a_secret_page_saves_nothing() {
            let d = tempfile::tempdir().unwrap();
            projects(&d.path().join("home"));
            let (e, _) = env_with(
                d.path(),
                accounts(),
                &fake(json!({"title": "R", "content": "x"})),
            );
            let other = execute_tool(
                &e,
                "project_profile.add_wiki",
                &json!({"url": "https://gitlab.example.com/other/proj/-/wikis/Runbook"}),
            );
            assert!(
                other["error"]
                    .as_str()
                    .unwrap()
                    .contains("no tracked project"),
                "{other}"
            );
            let bad = execute_tool(
                &e,
                "project_profile.add_wiki",
                &json!({"url": "https://gitlab.example.com/g/p"}),
            );
            assert!(bad["error"].is_string());
            let token = ["glpat-", "abcDEF123456ghiJKL78"].concat();
            let (e, _) = env_with(
                d.path(),
                accounts(),
                &fake(json!({"title": "R", "content": format!("use {token}")})),
            );
            let secret = execute_tool(&e, "project_profile.add_wiki", &json!({"url": LINK}));
            assert!(
                secret["error"].as_str().unwrap().contains("secret"),
                "{secret}"
            );
            assert!(!secret.to_string().contains("glpat"));
            assert!(
                crate::profile_wiki::list("shop", Some(&d.path().join("home/memory"))).is_empty()
            );
        }

        #[test]
        fn a_wiki_link_on_an_unconfigured_host_is_rejected_before_any_pending_action() {
            let d = tempfile::tempdir().unwrap();
            projects(&d.path().join("home"));
            let (e, loaded) = env_with(d.path(), accounts(), &fake(json!({})));
            let r = run(
                &e,
                "project_profile.add_wiki",
                &json!({"url": "https://nowhere.example/g/p/-/wikis/Runbook"}).to_string(),
                "s1",
            );
            assert!(
                r["error"].is_string() && loaded.lock().unwrap().is_empty(),
                "{r}"
            );
        }
    }
}
