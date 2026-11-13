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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals::NewApproval;
    use chrono::TimeZone;
    use serde_json::json;

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
}
