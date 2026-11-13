//! `loopx approvals`: see and answer the loop's pending approvals (the same store
//! the dashboard server and the Mac app use).

use chrono::Utc;
use loopx_core::{approvals, proposals, pyjson};
use serde_json::Value;
use std::process::ExitCode;

const USAGE: &str = "Usage: loopx approvals list [--all]\n       loopx approvals show <id>\n       loopx approvals approve <id> [--by NAME]\n       loopx approvals deny <id> [--by NAME]\n       loopx approvals request-changes <id> --notes TEXT [--by NAME]";

pub fn run(args: &[String]) -> ExitCode {
    match dispatch(args) {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn by(args: &[String]) -> String {
    flag(args, "--by").unwrap_or_else(|| "cli".to_string())
}

fn dispatch(args: &[String]) -> Result<String, String> {
    dispatch_in(args, &approvals::default_dir(), None)
}

/// [`dispatch`] against explicit stores (`memory_root` `None`: the default one).
fn dispatch_in(
    args: &[String],
    dir: &std::path::Path,
    memory_root: Option<&std::path::Path>,
) -> Result<String, String> {
    let now = Utc::now();
    match args.first().map(String::as_str) {
        Some("list") => {
            let status = if args.iter().any(|a| a == "--all") {
                None
            } else {
                Some("pending")
            };
            Ok(pyjson::to_string_pretty(&Value::Array(approvals::list(
                dir, status, now,
            ))))
        }
        Some("show") => {
            let id = args.get(1).ok_or(USAGE)?;
            approvals::expire_overdue(dir, now);
            approvals::get(dir, id)
                .map(|a| pyjson::to_string_pretty(&a))
                .ok_or_else(|| format!("no approval {id}"))
        }
        Some(cmd @ ("approve" | "deny")) => {
            let id = args.get(1).ok_or(USAGE)?;
            // A profile or guide page proposal is applied when approved, and
            // approved only while it still applies.
            proposals::decide(id, cmd, None, &by(args), memory_root, dir, now)
                .map(|a| pyjson::to_string_pretty(&a))
                .map_err(|e| e.to_string())
        }
        Some("request-changes") => {
            let id = args.get(1).ok_or(USAGE)?;
            let notes = flag(args, "--notes");
            proposals::decide(
                id,
                "request_changes",
                notes.as_deref(),
                &by(args),
                memory_root,
                dir,
                now,
            )
            .map(|a| pyjson::to_string_pretty(&a))
            .map_err(|e| e.to_string())
        }
        _ => Err(USAGE.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loopx_core::profile_sync;

    #[test]
    fn by_defaults_to_cli() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(by(&a(&["approve", "x"])), "cli");
        assert_eq!(by(&a(&["approve", "x", "--by", "mac-app"])), "mac-app");
    }

    #[test]
    fn unknown_subcommand_is_usage() {
        assert!(dispatch(&[]).unwrap_err().starts_with("Usage:"));
        assert!(dispatch(&["bogus".into()])
            .unwrap_err()
            .starts_with("Usage:"));
        assert!(dispatch(&["approve".into()])
            .unwrap_err()
            .starts_with("Usage:"));
        assert!(dispatch(&["request-changes".into()])
            .unwrap_err()
            .starts_with("Usage:"));
    }

    #[test]
    fn approving_a_profile_proposal_applies_it() {
        use loopx_core::project_profile::{self, Origin, Profile, Section, SectionKey, Status};
        let d = tempfile::tempdir().unwrap();
        let (apr, memory) = (d.path().join("approvals"), d.path().join("memory"));
        let mut sections = std::collections::BTreeMap::new();
        sections.insert(
            SectionKey::Stack,
            Section {
                summary: "s".into(),
                body: "Old.".into(),
                last_verified: None,
                sources: vec![],
                origin: Origin::Agent,
            },
        );
        let p = Profile {
            alias: "web".into(),
            generated_at: None,
            head_sha: None,
            status: Status::Approved,
            links: vec![],
            sections,
        };
        project_profile::save(&p, Some(&memory)).unwrap();
        let id = approvals::create(
            &apr,
            &approvals::NewApproval {
                kind: profile_sync::KIND.into(),
                project: "web".into(),
                timeout_seconds: 3600,
                target: Some(serde_json::json!({"section": "stack", "alias": "web",
                    "old": "Old.", "old_summary": "s", "new": "New.", "new_summary": "n", "sources": []})),
                ..Default::default()
            },
            Utc::now(),
        )
        .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let args: Vec<String> = ["approve", id.as_str()]
            .iter()
            .map(|s| s.to_string())
            .collect();
        dispatch_in(&args, &apr, Some(&memory)).unwrap();
        let back = project_profile::load("web", Some(&memory))
            .unwrap()
            .unwrap();
        assert_eq!(back.sections[&SectionKey::Stack].body, "New.");
    }

    /// An approved `feature-orders` page of `alias` under `memory` and a pending
    /// `guide-page` proposal rewriting its `what` part; returns the approval id.
    fn pending_guide_proposal(
        alias: &str,
        memory: &std::path::Path,
        apr: &std::path::Path,
    ) -> String {
        use loopx_core::project_guide::{self, Kind, Page, Part};
        use loopx_core::project_profile::{Origin, Status};
        let page = Page {
            alias: alias.into(),
            slug: "feature-orders".into(),
            kind: Kind::Feature,
            title: "Orders".into(),
            status: Status::Approved,
            generated_at: None,
            head_sha: Some("abc123".into()),
            summary: "Taking orders.".into(),
            related: vec![],
            parts: vec![Part {
                key: "what".into(),
                body: "Old what.".into(),
                origin: Origin::Agent,
            }],
            sources: vec![],
        };
        project_guide::save(&page, Some(memory)).unwrap();
        let mut held = page;
        held.status = Status::Draft;
        held.parts[0].body = "New what.".into();
        loopx_core::guide_proposal::raise(alias, &held, apr, Some(memory), Utc::now())
            .unwrap()
            .expect("a proposal")
    }

    fn guide_what(alias: &str, memory: &std::path::Path) -> String {
        loopx_core::project_guide::load(alias, "feature-orders", Some(memory))
            .unwrap()
            .unwrap()
            .parts[0]
            .body
            .clone()
    }

    #[test]
    fn approving_a_guide_page_proposal_applies_it_and_a_stale_one_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let (apr, memory) = (d.path().join("approvals"), d.path().join("memory"));
        let approve = |id: &str| {
            let args: Vec<String> = ["approve", id].iter().map(|s| s.to_string()).collect();
            dispatch_in(&args, &apr, Some(&memory))
        };
        let id = pending_guide_proposal("cli-guide-web", &memory, &apr);
        approve(&id).unwrap();
        assert_eq!(guide_what("cli-guide-web", &memory), "New what.");
        assert_eq!(approvals::get(&apr, &id).unwrap()["status"], "approved");

        let stale = pending_guide_proposal("cli-guide-stale", &memory, &apr);
        loopx_core::project_guide::update_locked(
            "cli-guide-stale",
            "feature-orders",
            Some(&memory),
            |p| {
                let mut p = p.unwrap();
                p.parts[0].body = "Edited by hand.".into();
                Ok(p)
            },
        )
        .unwrap();
        let err = approve(&stale).unwrap_err();
        assert!(err.contains("changed since"), "{err}");
        assert_eq!(approvals::get(&apr, &stale).unwrap()["status"], "pending");
        assert_eq!(guide_what("cli-guide-stale", &memory), "Edited by hand.");
    }

    #[test]
    fn request_changes_on_a_guide_page_proposal_is_refused_and_it_stays_pending() {
        let d = tempfile::tempdir().unwrap();
        let (apr, memory) = (d.path().join("approvals"), d.path().join("memory"));
        let id = pending_guide_proposal("cli-guide-changes", &memory, &apr);
        let args: Vec<String> = ["request-changes", id.as_str(), "--notes", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let err = dispatch_in(&args, &apr, Some(&memory)).unwrap_err();
        assert_eq!(err, proposals::NO_CHANGES_REQUESTED);
        assert_eq!(approvals::get(&apr, &id).unwrap()["status"], "pending");
        assert_eq!(guide_what("cli-guide-changes", &memory), "Old what.");
    }
}
