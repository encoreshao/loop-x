//! Read-only views of a project's profile and the project graph for the issue
//! agent (`loopx memory-store profile|graph`) and the dashboard chat
//! (`project_profile`). Every text returned here has passed the secret guard:
//! a profile or graph whose text looks like it holds a secret value is refused
//! rather than shown.

use crate::profile_graph::{self, Edge, EdgeKind};
use crate::profile_scan::IssueDigest;
use crate::project_profile::{self, contains_secret};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

pub const NO_PROFILE: &str = "(no profile yet)";

/// Why a read was refused. The text never contains the matched secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    Invalid(String),
    SecretRefused(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Invalid(m) => f.write_str(m),
            ReadError::SecretRefused(kind) => write!(
                f,
                "refused: the text looks like it contains a secret ({kind}); it is not shown"
            ),
        }
    }
}

fn guard(text: String) -> Result<String, ReadError> {
    match contains_secret(&text) {
        Some(kind) => Err(ReadError::SecretRefused(kind)),
        None => Ok(text),
    }
}

/// The rendered profile of `alias`, `None` when it has none.
pub fn profile_text(alias: &str, root: Option<&Path>) -> Result<Option<String>, ReadError> {
    match project_profile::load(alias, root).map_err(ReadError::Invalid)? {
        None => Ok(None),
        Some(p) => guard(project_profile::render(&p)).map(Some),
    }
}

/// The graph as JSON text. Without `alias`: every node and edge. With one: that
/// node and only the edges touching it, plus the neighbor aliases. Edges of kind
/// `shares-people` (weak, noisy) are left out unless `all`.
pub fn graph_json(
    alias: Option<&str>,
    all: bool,
    tracked: &[String],
    digests: &BTreeMap<String, IssueDigest>,
    root: Option<&Path>,
) -> Result<String, ReadError> {
    let mut profiles = Vec::new();
    for a in tracked {
        if let Some(p) = project_profile::load(a, root).map_err(ReadError::Invalid)? {
            profiles.push(p);
        }
    }
    let g = profile_graph::derive(&profiles, digests, tracked);
    let keep = |e: &&Edge| {
        (all || e.kind != EdgeKind::SharesPeople) && alias.is_none_or(|a| e.from == a || e.to == a)
    };
    let edges: Vec<Value> = g
        .edges
        .iter()
        .filter(keep)
        .map(|e| {
            json!({
                "from": e.from, "to": e.to, "kind": e.kind.as_str(),
                "evidence": e.evidence, "confidence": e.confidence,
            })
        })
        .collect();
    let value = match alias {
        None => json!({
            "nodes": g.nodes.iter().map(|n| json!({
                "alias": n.alias, "has_profile": n.freshness.is_some(), "activity": n.activity,
            })).collect::<Vec<_>>(),
            "edges": edges,
        }),
        Some(a) => {
            let mut neighbors: Vec<String> = edges
                .iter()
                .flat_map(|e| [e["from"].as_str(), e["to"].as_str()])
                .flatten()
                .filter(|n| *n != a)
                .map(String::from)
                .collect();
            neighbors.sort();
            neighbors.dedup();
            json!({"alias": a, "neighbors": neighbors, "edges": edges})
        }
    };
    guard(serde_json::to_string_pretty(&value).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_profile::{Link, Origin, Profile, Section, SectionKey, Status};

    fn section(body: &str) -> Section {
        Section {
            summary: String::new(),
            body: body.into(),
            last_verified: None,
            sources: vec![],
            origin: Origin::Agent,
        }
    }

    fn profile(alias: &str, links: Vec<Link>, secrets: &str) -> Profile {
        let mut sections = BTreeMap::new();
        sections.insert(SectionKey::Overview, section("A synthetic shop."));
        sections.insert(SectionKey::Secrets, section(secrets));
        Profile {
            alias: alias.into(),
            generated_at: None,
            head_sha: None,
            status: Status::Approved,
            links,
            sections,
        }
    }

    fn link(to: &str, kind: &str, ev: &str) -> Link {
        Link {
            to: to.into(),
            kind: kind.into(),
            evidence: ev.into(),
        }
    }

    fn names(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn digest(people: &[&str]) -> IssueDigest {
        IssueDigest {
            people: people.iter().map(|p| (p.to_string(), 3)).collect(),
            ..IssueDigest::default()
        }
    }

    #[test]
    fn absent_profile_is_none() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(profile_text("web", Some(d.path())).unwrap(), None);
    }

    #[test]
    fn profile_renders_markdown() {
        let d = tempfile::tempdir().unwrap();
        project_profile::save(&profile("web", vec![], "STRIPE_KEY"), Some(d.path())).unwrap();
        let text = profile_text("web", Some(d.path())).unwrap().unwrap();
        assert!(text.contains("## Overview") && text.contains("A synthetic shop."));
        assert!(text.contains("STRIPE_KEY"));
    }

    #[test]
    fn output_contains_no_secret_value_from_fixture() {
        let d = tempfile::tempdir().unwrap();
        // A hand-edited profile that smuggled a token in: it must never print.
        let token = format!("glpat-{}", "a1B2c3D4e5F6g7H8i9J0");
        let mut p = profile("web", vec![], "STRIPE_KEY");
        p.sections.insert(
            SectionKey::Gotchas,
            section(&format!("deploy with {token}")),
        );
        project_profile::save(&p, Some(d.path())).unwrap();
        let err = profile_text("web", Some(d.path())).unwrap_err();
        assert!(matches!(err, ReadError::SecretRefused(_)));
        assert!(!err.to_string().contains(&token), "{err}");
    }

    fn graph_fixture(d: &Path) -> (Vec<String>, BTreeMap<String, IssueDigest>) {
        let web = profile(
            "web",
            vec![link("api", "calls", "web/client.rs calls api")],
            "STRIPE_KEY",
        );
        let api = profile("api", vec![], "STRIPE_KEY");
        let docs = profile("docs", vec![], "");
        for p in [&web, &api, &docs] {
            project_profile::save(p, Some(d)).unwrap();
        }
        let digests = BTreeMap::from([
            ("web".to_string(), digest(&["ann", "bob"])),
            ("docs".to_string(), digest(&["ann", "bob"])),
        ]);
        (names(&["web", "api", "docs"]), digests)
    }

    #[test]
    fn graph_for_an_alias_lists_neighbors_without_people_edges() {
        let d = tempfile::tempdir().unwrap();
        let (tracked, digests) = graph_fixture(d.path());
        let text = graph_json(Some("web"), false, &tracked, &digests, Some(d.path())).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["alias"], "web");
        assert_eq!(v["neighbors"], json!(["api"]));
        let kinds: Vec<&str> = v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"calls"), "{kinds:?}");
        assert!(!kinds.contains(&"shares-people"), "{kinds:?}");
        let calls = v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["kind"] == "calls")
            .unwrap();
        assert!(calls["evidence"].as_str().unwrap().contains("calls api"));
        assert!(calls["confidence"].is_number());
    }

    #[test]
    fn graph_all_includes_people_edges() {
        let d = tempfile::tempdir().unwrap();
        let (tracked, digests) = graph_fixture(d.path());
        let text = graph_json(Some("web"), true, &tracked, &digests, Some(d.path())).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert!(v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "shares-people"));
        assert_eq!(v["neighbors"], json!(["api", "docs"]));
    }

    #[test]
    fn whole_graph_lists_nodes_and_edges() {
        let d = tempfile::tempdir().unwrap();
        let (tracked, digests) = graph_fixture(d.path());
        let text = graph_json(None, false, &tracked, &digests, Some(d.path())).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["nodes"].as_array().unwrap().len(), 3);
        assert!(!v["edges"].as_array().unwrap().is_empty());
    }
}
