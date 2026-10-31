//! Human approvals for actions the loop must not take on its own (opening a
//! merge request). One JSON file per approval under `outputs/approvals/`, so the
//! runner, the CLI, the dashboard server and the Mac app all share one store
//! with no daemon in between.
//!
//! A request starts `pending`, and ends exactly once as `approved`, `denied`,
//! `changes_requested` (with the reviewer's `notes`) or `timeout` (when
//! `expires_at` passes undecided). The runner blocks in [`wait_for_decision`]:
//! only `approved` proceeds, and `changes_requested` sends the notes back to the
//! agent for another pass.

use crate::events::{fmt_timestamp, new_uuid_hex};
use crate::pyjson;
use crate::runner_util::repo_root;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const DEFAULT_TIMEOUT_SECONDS: i64 = 1800;
/// The longest review notes a "request changes" decision may carry.
pub const MAX_NOTES_CHARS: usize = 4000;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ApprovalError {
    #[error("no approval {0}")]
    Unknown(String),
    #[error("approval {0} is already {1}")]
    AlreadyDecided(String, String),
    #[error("decision must be 'approve', 'deny' or 'request_changes', got {0:?}")]
    BadDecision(String),
    #[error("requesting changes needs notes saying what to change")]
    MissingNotes,
    #[error("the notes are longer than 4000 characters")]
    NotesTooLong,
    #[error("{0}")]
    Io(String),
}

pub fn default_dir() -> PathBuf {
    repo_root().join("outputs").join("approvals")
}

/// What the loop asks a human to approve.
#[derive(Debug, Clone, Default)]
pub struct NewApproval {
    pub kind: String,
    pub run_id: String,
    pub project: String,
    pub issue_iid: Option<i64>,
    pub title: String,
    pub branch: String,
    pub target_branch: String,
    pub summary: String,
    pub timeout_seconds: i64,
}

fn path_of(dir: &Path, id: &str) -> PathBuf {
    // Only the final component, so an id can never escape the store.
    let safe = Path::new(id)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dir.join(format!("{safe}.json"))
}

fn read(path: &Path) -> Option<Value> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v.is_object().then_some(v)
}

fn write(dir: &Path, approval: &Value) -> Result<(), ApprovalError> {
    let io = |e: std::io::Error| ApprovalError::Io(e.to_string());
    std::fs::create_dir_all(dir).map_err(io)?;
    let id = approval["id"].as_str().unwrap_or_default();
    let path = path_of(dir, id);
    let tmp = dir.join(format!(".{id}.tmp"));
    std::fs::write(&tmp, pyjson::to_string_pretty(approval)).map_err(io)?;
    std::fs::rename(&tmp, &path).map_err(io)
}

/// Create a `pending` approval and return it.
pub fn create(dir: &Path, new: &NewApproval, now: DateTime<Utc>) -> Result<Value, ApprovalError> {
    let timeout = if new.timeout_seconds > 0 {
        new.timeout_seconds
    } else {
        DEFAULT_TIMEOUT_SECONDS
    };
    let approval = json!({
        "id": format!("apr_{}", &new_uuid_hex()[..12]),
        "kind": new.kind,
        "run_id": new.run_id,
        "project": new.project,
        "issue_iid": new.issue_iid,
        "title": new.title,
        "branch": new.branch,
        "target_branch": new.target_branch,
        "summary": new.summary,
        "status": "pending",
        "created_at": fmt_timestamp(now),
        "expires_at": fmt_timestamp(now + Duration::seconds(timeout)),
        "decided_at": null,
        "decided_by": null,
    });
    write(dir, &approval)?;
    Ok(approval)
}

pub fn get(dir: &Path, id: &str) -> Option<Value> {
    read(&path_of(dir, id))
}

fn is_overdue(approval: &Value, now: DateTime<Utc>) -> bool {
    approval["status"] == "pending"
        && approval["expires_at"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|t| t <= now)
}

/// Mark every undecided approval past its `expires_at` as `timeout`. Returns the
/// ids that were just expired.
pub fn expire_overdue(dir: &Path, now: DateTime<Utc>) -> Vec<String> {
    let mut expired = Vec::new();
    for mut approval in all(dir) {
        if is_overdue(&approval, now) {
            approval["status"] = json!("timeout");
            approval["decided_at"] = json!(fmt_timestamp(now));
            approval["decided_by"] = json!("timeout");
            if write(dir, &approval).is_ok() {
                expired.push(approval["id"].as_str().unwrap_or_default().to_string());
            }
        }
    }
    expired
}

/// Every approval, newest first.
pub fn all(dir: &Path) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Value> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "json")
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .filter_map(|p| read(&p))
        .collect();
    out.sort_by(|a, b| {
        b["created_at"]
            .as_str()
            .unwrap_or("")
            .cmp(a["created_at"].as_str().unwrap_or(""))
    });
    out
}

/// Approvals with the given `status` (after expiring overdue ones).
pub fn list(dir: &Path, status: Option<&str>, now: DateTime<Utc>) -> Vec<Value> {
    expire_overdue(dir, now);
    all(dir)
        .into_iter()
        .filter(|a| status.is_none_or(|s| a["status"] == s))
        .collect()
}

/// Record a human's decision (`approve` / `deny`) on a pending approval.
pub fn decide(
    dir: &Path,
    id: &str,
    decision: &str,
    decided_by: &str,
    now: DateTime<Utc>,
) -> Result<Value, ApprovalError> {
    decide_with_notes(dir, id, decision, None, decided_by, now)
}

/// [`decide`], plus `request_changes`, which needs `notes` saying what to change.
pub fn decide_with_notes(
    dir: &Path,
    id: &str,
    decision: &str,
    notes: Option<&str>,
    decided_by: &str,
    now: DateTime<Utc>,
) -> Result<Value, ApprovalError> {
    let status = match decision {
        "approve" | "approved" => "approved",
        "deny" | "denied" => "denied",
        "request_changes" | "changes_requested" => "changes_requested",
        other => return Err(ApprovalError::BadDecision(other.to_string())),
    };
    let notes = notes.map(str::trim).filter(|n| !n.is_empty());
    if status == "changes_requested" {
        match notes {
            None => return Err(ApprovalError::MissingNotes),
            Some(n) if n.chars().count() > MAX_NOTES_CHARS => {
                return Err(ApprovalError::NotesTooLong)
            }
            Some(_) => {}
        }
    }
    expire_overdue(dir, now);
    let mut approval = get(dir, id).ok_or_else(|| ApprovalError::Unknown(id.to_string()))?;
    if approval["status"] != "pending" {
        return Err(ApprovalError::AlreadyDecided(
            id.to_string(),
            approval["status"].as_str().unwrap_or("?").to_string(),
        ));
    }
    let obj: &mut Map<String, Value> = approval.as_object_mut().expect("object");
    obj.insert("status".into(), json!(status));
    obj.insert("decided_at".into(), json!(fmt_timestamp(now)));
    obj.insert("decided_by".into(), json!(decided_by));
    if status == "changes_requested" {
        obj.insert("notes".into(), json!(notes));
    }
    write(dir, &approval)?;
    Ok(approval)
}

/// Block until `id` is decided or expires; returns the final status
/// (`approved` | `denied` | `timeout`). A vanished approval counts as `denied`.
pub fn wait_for_decision(dir: &Path, id: &str, poll: std::time::Duration) -> String {
    wait_for_decision_with(dir, id, poll, &mut || {})
}

/// [`wait_for_decision`] that calls `tick` before each look, e.g. to read
/// answers from a chat app (`approval_relay`).
pub fn wait_for_decision_with(
    dir: &Path,
    id: &str,
    poll: std::time::Duration,
    tick: &mut dyn FnMut(),
) -> String {
    loop {
        tick();
        expire_overdue(dir, Utc::now());
        match get(dir, id) {
            None => return "denied".to_string(),
            Some(a) if a["status"] != "pending" => {
                return a["status"].as_str().unwrap_or("denied").to_string()
            }
            Some(_) => std::thread::sleep(poll),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(sec: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 22, 10, 0, 0).unwrap() + Duration::seconds(sec)
    }

    #[test]
    fn the_wait_lets_a_tick_decide() {
        let d = tempfile::tempdir().unwrap();
        let id = create(d.path(), &req(), Utc::now()).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let mut ticks = 0;
        let status = wait_for_decision_with(
            d.path(),
            &id,
            std::time::Duration::from_millis(1),
            &mut || {
                ticks += 1;
                if ticks == 3 {
                    decide(d.path(), &id, "approve", "telegram:@ana", Utc::now()).unwrap();
                }
            },
        );
        assert_eq!((status.as_str(), ticks), ("approved", 3));
    }

    fn req() -> NewApproval {
        NewApproval {
            kind: "open_mr".into(),
            run_id: "run_1".into(),
            project: "web".into(),
            issue_iid: Some(7),
            title: "Fix #7: x".into(),
            branch: "loop/issue-7".into(),
            target_branch: "main".into(),
            summary: "s".into(),
            timeout_seconds: 60,
        }
    }

    #[test]
    fn create_writes_a_pending_approval_with_an_expiry() {
        let d = tempfile::tempdir().unwrap();
        let a = create(d.path(), &req(), t(0)).unwrap();
        assert!(a["id"].as_str().unwrap().starts_with("apr_"));
        assert_eq!(a["status"], "pending");
        assert_eq!(a["created_at"], "2026-10-22T10:00:00.000Z");
        assert_eq!(a["expires_at"], "2026-10-22T10:01:00.000Z");
        assert_eq!(get(d.path(), a["id"].as_str().unwrap()).unwrap(), a);
    }

    #[test]
    fn zero_timeout_falls_back_to_the_default() {
        let d = tempfile::tempdir().unwrap();
        let mut r = req();
        r.timeout_seconds = 0;
        let a = create(d.path(), &r, t(0)).unwrap();
        assert_eq!(a["expires_at"], "2026-10-22T10:30:00.000Z");
    }

    #[test]
    fn decide_approves_once() {
        let d = tempfile::tempdir().unwrap();
        let a = create(d.path(), &req(), t(0)).unwrap();
        let id = a["id"].as_str().unwrap();
        let done = decide(d.path(), id, "approve", "mac-app", t(5)).unwrap();
        assert_eq!(done["status"], "approved");
        assert_eq!(done["decided_by"], "mac-app");
        assert_eq!(
            decide(d.path(), id, "deny", "x", t(6)).unwrap_err(),
            ApprovalError::AlreadyDecided(id.into(), "approved".into())
        );
    }

    #[test]
    fn a_reviewer_can_request_changes_with_notes() {
        let d = tempfile::tempdir().unwrap();
        let a = create(d.path(), &req(), t(0)).unwrap();
        let id = a["id"].as_str().unwrap();
        assert_eq!(
            decide_with_notes(d.path(), id, "request_changes", Some("  "), "mac-app", t(1))
                .unwrap_err(),
            ApprovalError::MissingNotes
        );
        let done = decide_with_notes(
            d.path(),
            id,
            "request_changes",
            Some(" Rename the helper to page_rows. "),
            "mac-app",
            t(2),
        )
        .unwrap();
        assert_eq!(done["status"], "changes_requested");
        assert_eq!(done["notes"], "Rename the helper to page_rows.");
        assert_eq!(get(d.path(), id).unwrap(), done);
        let long = "x".repeat(MAX_NOTES_CHARS + 1);
        let b = create(d.path(), &req(), t(3)).unwrap();
        assert_eq!(
            decide_with_notes(
                d.path(),
                b["id"].as_str().unwrap(),
                "request_changes",
                Some(&long),
                "x",
                t(4)
            )
            .unwrap_err(),
            ApprovalError::NotesTooLong
        );
    }

    #[test]
    fn wait_returns_changes_requested() {
        let d = tempfile::tempdir().unwrap();
        let a = create(d.path(), &req(), Utc::now()).unwrap();
        let id = a["id"].as_str().unwrap();
        decide_with_notes(d.path(), id, "request_changes", Some("n"), "x", Utc::now()).unwrap();
        assert_eq!(
            wait_for_decision(d.path(), id, std::time::Duration::from_millis(10)),
            "changes_requested"
        );
    }

    #[test]
    fn deny_and_bad_inputs() {
        let d = tempfile::tempdir().unwrap();
        let a = create(d.path(), &req(), t(0)).unwrap();
        let id = a["id"].as_str().unwrap();
        assert_eq!(
            decide(d.path(), id, "maybe", "x", t(1)).unwrap_err(),
            ApprovalError::BadDecision("maybe".into())
        );
        assert_eq!(
            decide(d.path(), "apr_nope", "deny", "x", t(1)).unwrap_err(),
            ApprovalError::Unknown("apr_nope".into())
        );
        assert_eq!(
            decide(d.path(), id, "deny", "x", t(1)).unwrap()["status"],
            "denied"
        );
    }

    #[test]
    fn undecided_approvals_time_out_and_can_no_longer_be_approved() {
        let d = tempfile::tempdir().unwrap();
        let a = create(d.path(), &req(), t(0)).unwrap();
        let id = a["id"].as_str().unwrap().to_string();
        assert!(expire_overdue(d.path(), t(59)).is_empty());
        assert_eq!(expire_overdue(d.path(), t(60)), vec![id.clone()]);
        assert_eq!(get(d.path(), &id).unwrap()["status"], "timeout");
        assert!(matches!(
            decide(d.path(), &id, "approve", "x", t(61)),
            Err(ApprovalError::AlreadyDecided(_, _))
        ));
    }

    #[test]
    fn decide_after_expiry_is_refused_even_without_a_sweep() {
        let d = tempfile::tempdir().unwrap();
        let a = create(d.path(), &req(), t(0)).unwrap();
        let id = a["id"].as_str().unwrap();
        assert!(decide(d.path(), id, "approve", "x", t(120)).is_err());
    }

    #[test]
    fn list_filters_by_status_newest_first() {
        let d = tempfile::tempdir().unwrap();
        let a = create(d.path(), &req(), t(0)).unwrap();
        let b = create(d.path(), &req(), t(10)).unwrap();
        decide(d.path(), a["id"].as_str().unwrap(), "approve", "x", t(11)).unwrap();
        let pending = list(d.path(), Some("pending"), t(12));
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["id"], b["id"]);
        let every = list(d.path(), None, t(12));
        assert_eq!(every.len(), 2);
        assert_eq!(every[0]["id"], b["id"]);
    }

    #[test]
    fn ids_cannot_escape_the_store() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("secret.json"),
            r#"{"id":"secret","status":"pending"}"#,
        )
        .unwrap();
        let inner = d.path().join("store");
        std::fs::create_dir_all(&inner).unwrap();
        assert!(get(&inner, "../secret").is_none());
    }

    #[test]
    fn wait_returns_the_decision_made_by_another_thread() {
        let d = tempfile::tempdir().unwrap();
        let a = create(d.path(), &req(), Utc::now()).unwrap();
        let id = a["id"].as_str().unwrap().to_string();
        let dir = d.path().to_path_buf();
        let id2 = id.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            decide(&dir, &id2, "approve", "t", Utc::now()).unwrap();
        });
        let got = wait_for_decision(d.path(), &id, std::time::Duration::from_millis(20));
        h.join().unwrap();
        assert_eq!(got, "approved");
    }

    #[test]
    fn wait_times_out_when_nobody_answers() {
        let d = tempfile::tempdir().unwrap();
        let mut r = req();
        r.timeout_seconds = 1;
        let a = create(d.path(), &r, Utc::now()).unwrap();
        let got = wait_for_decision(
            d.path(),
            a["id"].as_str().unwrap(),
            std::time::Duration::from_millis(50),
        );
        assert_eq!(got, "timeout");
    }

    #[test]
    fn a_vanished_approval_counts_as_denied() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            wait_for_decision(d.path(), "apr_gone", std::time::Duration::from_millis(5)),
            "denied"
        );
    }
}
