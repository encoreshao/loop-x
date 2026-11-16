//! Records a Slack button press on an approval message.

use crate::approvals::{self, ApprovalError};
use chrono::{DateTime, Utc};
use std::path::Path;

/// What became of a button press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonOutcome {
    Recorded,
    Rejected(&'static str),
}

/// Record `user`'s press of an approval button in `dir`. `action_value` is
/// `<approval id>:approve|deny` (see `slack_bot::approval_blocks`). Only a user
/// on `allowlist` may answer, and only a still-pending approval takes the
/// answer; every other case leaves the store untouched.
pub fn handle_button(
    dir: &Path,
    allowlist: &[String],
    user: &str,
    action_value: &str,
    now: DateTime<Utc>,
) -> ButtonOutcome {
    let Some((id, decision)) = action_value.rsplit_once(':') else {
        return ButtonOutcome::Rejected("bad request");
    };
    if id.is_empty() || id.contains(':') || !matches!(decision, "approve" | "deny") {
        return ButtonOutcome::Rejected("bad request");
    }
    if user.is_empty() || !allowlist.iter().any(|u| u == user) {
        return ButtonOutcome::Rejected("not authorized");
    }
    match approvals::decide_with_notes(dir, id, decision, None, &format!("slack:{user}"), now) {
        Ok(_) => ButtonOutcome::Recorded,
        Err(ApprovalError::Unknown(_)) => ButtonOutcome::Rejected("unknown approval"),
        Err(ApprovalError::AlreadyDecided(..)) => ButtonOutcome::Rejected("already decided"),
        Err(ApprovalError::Io(_)) => ButtonOutcome::Rejected("could not record"),
        Err(_) => ButtonOutcome::Rejected("bad request"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals::NewApproval;
    use serde_json::Value;

    fn now() -> DateTime<Utc> {
        "2026-11-15T10:00:00Z".parse().unwrap()
    }

    fn pending(dir: &Path) -> String {
        let new = NewApproval {
            kind: "merge_request".into(),
            run_id: "r1".into(),
            project: "web".into(),
            issue_iid: Some(7),
            title: "Fix login".into(),
            branch: "loop/issue-7".into(),
            target_branch: "main".into(),
            summary: "Trim the email".into(),
            timeout_seconds: 3600,
            target: None,
        };
        approvals::create(dir, &new, now()).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn allow() -> Vec<String> {
        vec!["U01ABC".to_string()]
    }

    fn state(dir: &Path, id: &str) -> Value {
        approvals::get(dir, id).unwrap()
    }

    #[test]
    fn an_allowed_user_approving_records_the_decision() {
        let d = tempfile::tempdir().unwrap();
        let id = pending(d.path());
        let out = handle_button(
            d.path(),
            &allow(),
            "U01ABC",
            &format!("{id}:approve"),
            now(),
        );
        assert_eq!(out, ButtonOutcome::Recorded);
        let a = state(d.path(), &id);
        assert_eq!(a["status"], "approved");
        assert_eq!(a["decided_by"], "slack:U01ABC");
    }

    #[test]
    fn an_allowed_user_denying_records_the_denial() {
        let d = tempfile::tempdir().unwrap();
        let id = pending(d.path());
        let out = handle_button(d.path(), &allow(), "U01ABC", &format!("{id}:deny"), now());
        assert_eq!(out, ButtonOutcome::Recorded);
        assert_eq!(state(d.path(), &id)["status"], "denied");
    }

    #[test]
    fn a_user_off_the_allowlist_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let id = pending(d.path());
        for user in ["U99ZZZ", "", "u01abc"] {
            let out = handle_button(d.path(), &allow(), user, &format!("{id}:approve"), now());
            assert_eq!(out, ButtonOutcome::Rejected("not authorized"), "{user:?}");
        }
        let a = state(d.path(), &id);
        assert_eq!(a["status"], "pending");
        assert!(a["decided_by"].is_null());
    }

    #[test]
    fn an_unknown_approval_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        let out = handle_button(d.path(), &allow(), "U01ABC", "apr_nope:approve", now());
        assert_eq!(out, ButtonOutcome::Rejected("unknown approval"));
    }

    #[test]
    fn a_decided_approval_keeps_its_first_answer() {
        let d = tempfile::tempdir().unwrap();
        let id = pending(d.path());
        approvals::decide(d.path(), &id, "deny", "someone", now()).unwrap();
        let out = handle_button(
            d.path(),
            &allow(),
            "U01ABC",
            &format!("{id}:approve"),
            now(),
        );
        assert_eq!(out, ButtonOutcome::Rejected("already decided"));
        let a = state(d.path(), &id);
        assert_eq!(a["status"], "denied");
        assert_eq!(a["decided_by"], "someone");
    }

    #[test]
    fn an_expired_approval_is_not_approved() {
        let d = tempfile::tempdir().unwrap();
        let id = pending(d.path());
        let later = now() + chrono::Duration::hours(2);
        let out = handle_button(
            d.path(),
            &allow(),
            "U01ABC",
            &format!("{id}:approve"),
            later,
        );
        assert_eq!(out, ButtonOutcome::Rejected("already decided"));
        assert_eq!(state(d.path(), &id)["status"], "timeout");
    }

    #[test]
    fn a_store_that_cannot_be_written_is_its_own_rejection() {
        let ro = tempfile::tempdir().unwrap();
        let id = pending(ro.path());
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(ro.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let out = handle_button(
            ro.path(),
            &allow(),
            "U01ABC",
            &format!("{id}:approve"),
            now(),
        );
        std::fs::set_permissions(ro.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(out, ButtonOutcome::Rejected("could not record"));
    }

    #[test]
    fn malformed_values_are_rejected_without_touching_the_store() {
        let d = tempfile::tempdir().unwrap();
        let id = pending(d.path());
        for value in [
            "",
            "garbage",
            ":approve",
            &format!("{id}:"),
            &format!("{id}:request_changes"),
            &format!("{id}:approved"),
            &format!("{id}:approve:extra"),
        ] {
            let out = handle_button(d.path(), &allow(), "U01ABC", value, now());
            assert_eq!(out, ButtonOutcome::Rejected("bad request"), "{value:?}");
        }
        assert_eq!(state(d.path(), &id)["status"], "pending");
    }
}
