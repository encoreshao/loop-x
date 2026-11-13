//! Proposals a person approves to change a project's memory: a `profile-section`
//! proposal ([`crate::profile_sync`]) or a `guide-page` proposal
//! ([`crate::guide_proposal`]). Every approval entry point (Needs you, the API, the
//! CLI) asks [`is_kind`] and approves through [`approve_and_apply`], so both kinds are
//! applied the moment they are approved.

use crate::{approvals, guide_proposal, profile_sync};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::Path;

/// Whether an approval's `kind` is a proposal kind.
pub fn is_kind(kind: &Value) -> bool {
    *kind == profile_sync::KIND || *kind == guide_proposal::KIND
}

/// Approve a proposal and apply it, dispatching on its kind. Returns the approval as
/// stored.
pub fn approve_and_apply(
    id: &str,
    by: &str,
    root: Option<&Path>,
    approvals_dir: &Path,
    now: DateTime<Utc>,
) -> Result<Value, String> {
    let a = approvals::get(approvals_dir, id).ok_or_else(|| format!("no approval {id}"))?;
    match a["kind"].as_str() {
        Some(profile_sync::KIND) => {
            profile_sync::approve_and_apply(id, by, root, approvals_dir, now)
        }
        Some(guide_proposal::KIND) => {
            guide_proposal::approve_and_apply(id, by, root, approvals_dir, now)
        }
        _ => Err(format!("approval {id} is not a proposal")),
    }
}

/// Why [`decide`] refused: the approval store's own error (so a caller keeps its
/// status mapping for it) or a proposal's refusal.
#[derive(Debug)]
pub enum DecideError {
    Approval(approvals::ApprovalError),
    Proposal(String),
}

impl std::fmt::Display for DecideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecideError::Approval(e) => write!(f, "{e}"),
            DecideError::Proposal(e) => f.write_str(e),
        }
    }
}

pub const NO_CHANGES_REQUESTED: &str =
    "a proposal cannot be sent back for changes; approve or dismiss it";

/// Whether deciding `kind` with `decision` approves and applies a proposal.
pub fn applies(kind: &Value, decision: &str) -> bool {
    is_kind(kind) && approvals::decision_status(decision) == Some("approved")
}

/// Record a decision on any approval: the one entry point for the Needs-you page, the
/// API and the CLI. A proposal approved by any approve verb is applied (and approved
/// only while it still applies); a proposal cannot be sent back for changes; every
/// other decision, and every decision on another kind, is [`approvals::decide_with_notes`].
pub fn decide(
    id: &str,
    decision: &str,
    notes: Option<&str>,
    by: &str,
    root: Option<&Path>,
    approvals_dir: &Path,
    now: DateTime<Utc>,
) -> Result<Value, DecideError> {
    let kind = approvals::get(approvals_dir, id).map(|a| a["kind"].clone());
    if let Some(kind) = kind.as_ref().filter(|k| is_kind(k)) {
        if applies(kind, decision) {
            return approve_and_apply(id, by, root, approvals_dir, now)
                .map_err(DecideError::Proposal);
        }
        if decision == "request-changes"
            || approvals::decision_status(decision) == Some("changes_requested")
        {
            return Err(DecideError::Proposal(NO_CHANGES_REQUESTED.into()));
        }
    }
    approvals::decide_with_notes(approvals_dir, id, decision, notes, by, now)
        .map_err(DecideError::Approval)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals::NewApproval;
    use chrono::TimeZone;
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn both_proposal_kinds_are_proposals_and_nothing_else_is() {
        assert!(is_kind(&json!("profile-section")));
        assert!(is_kind(&json!("guide-page")));
        assert!(!is_kind(&json!("fix")));
        assert!(!is_kind(&json!("merge_request")));
        assert!(!is_kind(&Value::Null));
        assert!(!is_kind(&json!({})["kind"]));
    }

    #[test]
    fn approving_an_approval_of_another_kind_is_refused_and_left_pending() {
        let apr = tempfile::tempdir().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap();
        let a = approvals::create(
            apr.path(),
            &NewApproval {
                kind: "merge_request".into(),
                project: "web".into(),
                title: "t".into(),
                summary: "s".into(),
                timeout_seconds: 3600,
                ..NewApproval::default()
            },
            now,
        )
        .unwrap();
        let id = a["id"].as_str().unwrap();
        let err = approve_and_apply(id, "me", None, apr.path(), now).unwrap_err();
        assert_eq!(err, format!("approval {id} is not a proposal"));
        assert_eq!(approvals::get(apr.path(), id).unwrap()["status"], "pending");
        assert!(approve_and_apply("nope", "me", None, apr.path(), now).is_err());
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap()
    }

    /// A pending profile proposal rewriting `alias`'s Stack section "Old." -> "New.".
    fn profile_proposal(alias: &str, memory: &Path, apr: &Path) -> String {
        use crate::project_profile::{self, Origin, Profile, Section, SectionKey, Status};
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
            alias: alias.into(),
            generated_at: None,
            head_sha: None,
            status: Status::Approved,
            links: vec![],
            sections,
        };
        project_profile::save(&p, Some(memory)).unwrap();
        approvals::create(
            apr,
            &NewApproval {
                kind: profile_sync::KIND.into(),
                project: alias.into(),
                timeout_seconds: 3600,
                target: Some(json!({"section": "stack", "alias": alias,
                    "old": "Old.", "old_summary": "s", "new": "New.", "new_summary": "n", "sources": []})),
                ..Default::default()
            },
            now(),
        )
        .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn profile_text(alias: &str, memory: &Path) -> String {
        use crate::project_profile::{self, SectionKey};
        project_profile::load(alias, Some(memory))
            .unwrap()
            .unwrap()
            .sections[&SectionKey::Stack]
            .body
            .clone()
    }

    /// A pending guide proposal rewriting `alias`'s feature-orders `what` "Old what." -> "New what.".
    fn guide_proposal(alias: &str, memory: &Path, apr: &Path) -> String {
        use crate::project_guide::{self, Kind, Page, Part};
        use crate::project_profile::{Origin, Status};
        let page = Page {
            alias: alias.into(),
            slug: "feature-orders".into(),
            kind: Kind::Feature,
            title: "Orders".into(),
            status: Status::Approved,
            generated_at: None,
            head_sha: None,
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
        guide_proposal::raise(alias, &held, apr, Some(memory), now())
            .unwrap()
            .unwrap()
    }

    fn guide_text(alias: &str, memory: &Path) -> String {
        crate::project_guide::load(alias, "feature-orders", Some(memory))
            .unwrap()
            .unwrap()
            .parts[0]
            .body
            .clone()
    }

    fn status(apr: &Path, id: &str) -> Value {
        approvals::get(apr, id).unwrap()["status"].clone()
    }

    type Make = fn(&str, &Path, &Path) -> String;
    type Text = fn(&str, &Path) -> String;
    const KINDS: [(&str, Make, Text, &str, &str); 2] = [
        (
            "dec-profile",
            profile_proposal,
            profile_text,
            "Old.",
            "New.",
        ),
        (
            "dec-guide",
            guide_proposal,
            guide_text,
            "Old what.",
            "New what.",
        ),
    ];

    #[test]
    fn every_approve_verb_applies_a_proposal_of_either_kind() {
        for (prefix, make, text, _, new) in KINDS {
            for verb in ["approve", "approved"] {
                let d = tempfile::tempdir().unwrap();
                let (memory, apr) = (d.path().join("m"), d.path().join("a"));
                let alias = format!("{prefix}-{verb}");
                let id = make(&alias, &memory, &apr);
                let a = decide(&id, verb, None, "me", Some(&memory), &apr, now()).unwrap();
                assert_eq!(a["status"], "approved", "{prefix} {verb}");
                assert_eq!(text(&alias, &memory), new, "{prefix} {verb}");
            }
        }
    }

    #[test]
    fn denying_a_proposal_records_it_without_applying() {
        for (prefix, make, text, old, _) in KINDS {
            for verb in ["deny", "denied"] {
                let d = tempfile::tempdir().unwrap();
                let (memory, apr) = (d.path().join("m"), d.path().join("a"));
                let alias = format!("{prefix}-{verb}");
                let id = make(&alias, &memory, &apr);
                let a = decide(&id, verb, None, "me", Some(&memory), &apr, now()).unwrap();
                assert_eq!(a["status"], "denied");
                assert_eq!(text(&alias, &memory), old);
            }
        }
    }

    #[test]
    fn a_proposal_cannot_be_sent_back_for_changes() {
        for (prefix, make, text, old, _) in KINDS {
            for verb in ["request_changes", "request-changes", "changes_requested"] {
                let d = tempfile::tempdir().unwrap();
                let (memory, apr) = (d.path().join("m"), d.path().join("a"));
                let alias = format!("{prefix}-{verb}");
                let id = make(&alias, &memory, &apr);
                let err = decide(&id, verb, Some("fix it"), "me", Some(&memory), &apr, now())
                    .unwrap_err();
                assert_eq!(
                    err.to_string(),
                    "a proposal cannot be sent back for changes; approve or dismiss it"
                );
                assert_eq!(status(&apr, &id), "pending");
                assert_eq!(text(&alias, &memory), old);
            }
        }
    }

    #[test]
    fn other_kinds_are_decided_exactly_as_before() {
        let apr = tempfile::tempdir().unwrap();
        let mk = || {
            approvals::create(
                apr.path(),
                &NewApproval {
                    kind: "merge_request".into(),
                    project: "web".into(),
                    title: "t".into(),
                    summary: "s".into(),
                    timeout_seconds: 3600,
                    ..NewApproval::default()
                },
                now(),
            )
            .unwrap()["id"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let id = mk();
        let a = decide(&id, "approved", None, "me", None, apr.path(), now()).unwrap();
        assert_eq!(a["status"], "approved");
        let id = mk();
        let a = decide(
            &id,
            "request_changes",
            Some("x"),
            "me",
            None,
            apr.path(),
            now(),
        )
        .unwrap();
        assert_eq!(a["status"], "changes_requested");
        let id = mk();
        assert!(matches!(
            decide(&id, "request_changes", None, "me", None, apr.path(), now()),
            Err(DecideError::Approval(
                approvals::ApprovalError::MissingNotes
            ))
        ));
        assert!(matches!(
            decide(&id, "maybe", None, "me", None, apr.path(), now()),
            Err(DecideError::Approval(
                approvals::ApprovalError::BadDecision(_)
            ))
        ));
        assert!(matches!(
            decide("apr_nope", "approve", None, "me", None, apr.path(), now()),
            Err(DecideError::Approval(approvals::ApprovalError::Unknown(_)))
        ));
        assert_eq!(status(apr.path(), &id), "pending");
    }
}
