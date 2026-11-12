//! "Needs you": everything that is waiting on a person, in one list. An
//! approval request, an issue the GitLab loop escalated, a sign-in that has
//! died, a loop whose last run failed. The dashboard's topbar bell and the Mac
//! app's menu bar count them; each one already alerted once in Slack, so this
//! is where it stays visible instead of being alerted again.
//!
//! A merge-request review thread the MR-feedback round handed to the user
//! (`<memory root>/<alias>/mr-threads/<iid>.json`, status `needs_you`) waits
//! here too, with the plan, the reviewer's comment and why it stopped; the
//! user decides it (`mr_feedback::decide_thread`).
//!
//! A project whose last weekly profile sync failed (`<home>/profile_sync.json`,
//! next to the memory root) waits here too, with the guarded failure line only.
//!
//! Escalations, failing loops, dead sign-ins, review threads and profile sync
//! failures can be dismissed
//! (`needs_you_dismissed.json`: `{<item id>: <UTC ISO>}`); a newer escalation
//! of the same issue, a newer failure, or a sign-in that recovers and breaks
//! again brings them back. Reconnecting clears a sign-in on its own
//! (`auth_health::mark_reconnected`). Approvals can't be dismissed: deciding
//! is what clears them.

use crate::config::home_dir;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How far back an escalation still counts as waiting.
pub const ESCALATION_DAYS: i64 = 7;

/// Events that mean the loop picked an escalated issue up again.
const MOVED_ON: [&str; 4] = [
    "issue.escalated",
    "issue.completed",
    "issue.queued",
    "approval.requested",
];

pub fn default_dismissed_path() -> PathBuf {
    home_dir().join("needs_you_dismissed.json")
}

/// Where the sources live.
pub struct Sources<'a> {
    pub approvals_dir: &'a Path,
    pub events_dir: &'a Path,
    /// `auth_health.json`.
    pub auth_state: &'a Path,
    /// Each loop's status document (with its effective `state`).
    pub loops: &'a Map<String, Value>,
    pub dismissed: &'a Path,
    /// Task memory, where each issue's review-thread state lives.
    pub memory_root: &'a Path,
}

/// An RFC 3339 time, or a naive one in local time (`auth_health`, run status).
pub fn parse_time(t: &str) -> Option<DateTime<Utc>> {
    parse(t)
}

fn parse(t: &str) -> Option<DateTime<Utc>> {
    if let Ok(t) = DateTime::parse_from_rfc3339(t) {
        return Some(t.with_timezone(&Utc));
    }
    chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()?
        .and_local_timezone(chrono::Local)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
}

fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

/// Whether `id` was dismissed at or after `since`.
fn dismissed_since(dismissed: &Value, id: &str, since: Option<&str>) -> bool {
    let at = dismissed.get(id).and_then(Value::as_str).and_then(parse);
    match (at, since.and_then(parse)) {
        (Some(at), Some(since)) => at >= since,
        (Some(_), None) => true,
        _ => false,
    }
}

fn item(id: String, kind: &str, title: String, since: Option<&str>, fields: Value) -> Value {
    let mut v = json!({
        "id": id,
        "kind": kind,
        "title": title,
        "since": since,
        "dismissable": matches!(
            kind,
            "escalated" | "loop_failed" | "sign_in" | "mr_thread" | "profile_sync_failed"
        ),
    });
    if let (Some(v), Value::Object(f)) = (v.as_object_mut(), fields) {
        v.extend(f);
    }
    v
}

fn approvals(src: &Sources, now: DateTime<Utc>) -> Vec<Value> {
    crate::approvals::list(src.approvals_dir, Some("pending"), now)
        .into_iter()
        .map(|a| {
            let project = crate::pydumps::py_str(&a["project"]);
            let iid = crate::pydumps::py_str(&a["issue_iid"]);
            let title = a["title"].as_str().unwrap_or("");
            if a["kind"] == crate::profile_sync::KIND {
                // A profile section proposal has no issue, branch or merge request.
                let section = a["target"]["section"].as_str().unwrap_or("");
                return item(
                    format!("approval:{}", a["id"].as_str().unwrap_or("")),
                    "approval",
                    format!("Review the proposed {section} change to the {project} profile"),
                    a["created_at"].as_str(),
                    json!({"approval_id": a["id"], "project": project, "approval_kind": a["kind"], "section": section}),
                );
            }
            item(
                format!("approval:{}", a["id"].as_str().unwrap_or("")),
                "approval",
                format!("Approve the fix for {project} #{iid}: {title}"),
                a["created_at"].as_str(),
                json!({"approval_id": a["id"], "project": project, "issue_iid": iid, "issue_title": title}),
            )
        })
        .collect()
}

fn escalations(src: &Sources, dismissed: &Value, now: DateTime<Utc>) -> Vec<Value> {
    let since = (now - Duration::days(ESCALATION_DAYS))
        .format("%Y-%m-%d")
        .to_string();
    let cutoff = now - Duration::days(ESCALATION_DAYS);
    // The newest event per issue that says where it stands.
    let mut latest: BTreeMap<(String, String), Value> = BTreeMap::new();
    for e in crate::events::iter_events(Some(src.events_dir), Some(&since), None) {
        if !e["event_type"]
            .as_str()
            .is_some_and(|t| MOVED_ON.contains(&t))
        {
            continue;
        }
        let (Some(project), false) = (e["project"].as_str(), e["issue_iid"].is_null()) else {
            continue;
        };
        let key = (project.to_string(), crate::pydumps::py_str(&e["issue_iid"]));
        let newer = latest.get(&key).is_none_or(|old| {
            e["timestamp"].as_str().and_then(parse) >= old["timestamp"].as_str().and_then(parse)
        });
        if newer {
            latest.insert(key, e);
        }
    }
    latest
        .into_iter()
        .filter(|(_, e)| e["event_type"] == "issue.escalated")
        .filter(|(_, e)| {
            e["timestamp"]
                .as_str()
                .and_then(parse)
                .is_some_and(|t| t >= cutoff)
        })
        .filter_map(|((project, iid), e)| {
            let id = format!("escalated:{project}#{iid}");
            let at = e["timestamp"].as_str();
            if dismissed_since(dismissed, &id, at) {
                return None;
            }
            let reason = e["data"]["reason"].as_str().unwrap_or("");
            Some(item(
                id,
                "escalated",
                format!("{project} #{iid} was escalated to you ({reason})"),
                at,
                json!({"project": project, "issue_iid": iid, "reason": reason}),
            ))
        })
        .collect()
}

fn sign_ins(src: &Sources, dismissed: &Value) -> Vec<Value> {
    let state = read_json(src.auth_state);
    let Some(checks) = state.get("checks").and_then(Value::as_object) else {
        return Vec::new();
    };
    checks
        .iter()
        .filter(|(_, c)| c["ok"] == false)
        .filter(|(_, c)| {
            // Older state files have no `confirmed`: two failures in a row is.
            c.get("confirmed")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| {
                    c["fail_count"].as_i64().unwrap_or(0)
                        >= crate::auth_health::SOFT_FAILURES_BEFORE_ALERT
                })
        })
        .filter_map(|(probe, c)| {
            let id = format!("sign_in:{probe}");
            let since = c["failing_since"].as_str();
            if dismissed_since(dismissed, &id, since) {
                return None;
            }
            let label = c["label"].as_str().unwrap_or(probe);
            let message = c["message"].as_str().unwrap_or("");
            Some(item(
                id,
                "sign_in",
                format!("Sign-in needs attention: {label} - {message}"),
                since,
                json!({"probe": probe, "label": label, "message": message}),
            ))
        })
        .collect()
}

fn failed_loops(src: &Sources, dismissed: &Value) -> Vec<Value> {
    src.loops
        .iter()
        .filter(|(_, s)| matches!(s["state"].as_str(), Some("failed" | "stale")))
        .filter_map(|(name, s)| {
            let id = format!("loop_failed:{name}");
            let at = s["updated_at"].as_str();
            if dismissed_since(dismissed, &id, at) {
                return None;
            }
            let exit = s.get("last_exit_code").cloned().unwrap_or(Value::Null);
            let failures = s["consecutive_failures"].as_i64().unwrap_or(1);
            Some(item(
                id,
                "loop_failed",
                format!("{name} failed its last run"),
                at,
                json!({"loop": name, "exit_code": exit, "consecutive_failures": failures}),
            ))
        })
        .collect()
}

/// Projects whose last profile sync failed, from the sync store beside the
/// memory root (never the process home). Only the stored, guarded failure line
/// is shown, and a secret-shaped one is replaced.
fn profile_sync_failures(src: &Sources, dismissed: &Value) -> Vec<Value> {
    use crate::profile_sync::{self, Outcome, DETAILS_HIDDEN};
    let Some(home) = src.memory_root.parent() else {
        return Vec::new();
    };
    let Ok(store) = profile_sync::load(&home.join("profile_sync.json")) else {
        return Vec::new();
    };
    store
        .status
        .per_project
        .iter()
        .filter_map(|(alias, s)| {
            let Outcome::Failed(msg) = &s.outcome else {
                return None;
            };
            let id = format!("profile_sync:{alias}");
            let at = s.at.to_rfc3339();
            if dismissed_since(dismissed, &id, Some(&at)) {
                return None;
            }
            let message = if crate::project_profile::contains_secret(msg).is_some() {
                DETAILS_HIDDEN.to_string()
            } else {
                profile_sync::safe_failure_line(msg)
            };
            Some(item(
                id,
                "profile_sync_failed",
                format!("Profile sync failed for {alias}: {message}"),
                Some(&at),
                json!({"project": alias, "message": message}),
            ))
        })
        .collect()
}

/// Review threads in `needs_you`, from every issue's thread state.
fn mr_threads(src: &Sources, dismissed: &Value) -> Vec<Value> {
    use crate::mr_threads::{IssueThreads, ThreadStatus};
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(src.memory_root) else {
        return out;
    };
    let mut files: Vec<(String, i64)> = projects
        .flatten()
        .filter_map(|p| {
            let alias = p.file_name().to_str()?.to_string();
            let dir = std::fs::read_dir(p.path().join("mr-threads")).ok()?;
            Some(
                dir.flatten()
                    .filter_map(|f| {
                        let name = f.file_name();
                        let iid = name.to_str()?.strip_suffix(".json")?.parse().ok()?;
                        Some((alias.clone(), iid))
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect();
    files.sort();
    for (alias, iid) in files {
        let state = IssueThreads::load(&alias, iid, Some(src.memory_root));
        for (discussion, t) in &state.threads {
            if t.status != ThreadStatus::NeedsYou {
                continue;
            }
            let id = format!("mr_thread:{alias}:{iid}:{discussion}");
            if dismissed_since(dismissed, &id, Some(&t.updated_at)) {
                continue;
            }
            let reason = t.reason.clone().unwrap_or_default();
            let plan = t.plan.as_ref();
            let plan_text = |k: &str| plan.and_then(|p| p.get(k)).and_then(Value::as_str);
            let comment = t.last_comment.as_ref();
            out.push(item(
                id,
                "mr_thread",
                format!(
                    "{alias} #{iid}: a review thread on the merge request needs you ({reason})"
                ),
                Some(&t.updated_at),
                json!({
                    "project": alias,
                    "issue_iid": iid,
                    "discussion_id": discussion,
                    "mr_iid": t.mr_iid,
                    "reason": reason,
                    "plan_summary": t.plan_summary,
                    "confidence": t.confidence,
                    "action": plan_text("action"),
                    "reply": plan_text("reply"),
                    "comment_author": comment.map(|c| c.author.as_str()),
                    "comment_body": comment.map(|c| c.body.as_str()),
                    "last_note_id": t.last_note_id,
                }),
            ));
        }
    }
    out
}

/// Everything waiting on a person, oldest first.
pub fn collect(src: &Sources, now: DateTime<Utc>) -> Vec<Value> {
    let dismissed = read_json(src.dismissed);
    let mut items = approvals(src, now);
    items.extend(escalations(src, &dismissed, now));
    items.extend(sign_ins(src, &dismissed));
    items.extend(failed_loops(src, &dismissed));
    items.extend(mr_threads(src, &dismissed));
    items.extend(profile_sync_failures(src, &dismissed));
    // Oldest first; an item with no time goes last.
    items.sort_by_key(|i| {
        let since = i["since"].as_str().and_then(parse);
        (since.is_none(), since)
    });
    items
}

/// Hide a dismissable item until it happens again.
pub fn dismiss(path: &Path, id: &str, now: DateTime<Utc>) -> Result<(), String> {
    if ![
        "escalated:",
        "loop_failed:",
        "sign_in:",
        "mr_thread:",
        "profile_sync:",
    ]
    .iter()
    .any(|p| id.starts_with(p))
    {
        return Err(format!("{id} can't be dismissed"));
    }
    let mut map = match read_json(path) {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    map.insert(id.to_string(), json!(now.to_rfc3339()));
    crate::settings_store::atomic_write(path, Value::Object(map).to_string().as_bytes(), None)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap()
    }

    struct Fx {
        dir: tempfile::TempDir,
        loops: Map<String, Value>,
    }

    impl Fx {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
                loops: Map::new(),
            }
        }
        fn p(&self, rel: &str) -> PathBuf {
            self.dir.path().join(rel)
        }
        fn event(&self, kind: &str, at: &str, iid: i64, data: Value) {
            let dir = self.p("events");
            std::fs::create_dir_all(&dir).unwrap();
            let line = json!({"timestamp": at, "event_type": kind, "project": "web",
                              "issue_iid": iid, "data": data});
            let file = dir.join(format!("{}.jsonl", &at[..10]));
            let mut text = std::fs::read_to_string(&file).unwrap_or_default();
            text.push_str(&format!("{line}\n"));
            std::fs::write(file, text).unwrap();
        }
        fn collect(&self) -> Vec<Value> {
            let (approvals, events, auth, dismissed, memory) = (
                self.p("approvals"),
                self.p("events"),
                self.p("auth_health.json"),
                self.p("dismissed.json"),
                self.p("memory"),
            );
            collect(
                &Sources {
                    approvals_dir: &approvals,
                    events_dir: &events,
                    auth_state: &auth,
                    loops: &self.loops,
                    dismissed: &dismissed,
                    memory_root: &memory,
                },
                now(),
            )
        }
        fn ids(&self) -> Vec<String> {
            self.collect()
                .iter()
                .map(|i| i["id"].as_str().unwrap().to_string())
                .collect()
        }
    }

    fn thread(
        status: crate::mr_threads::ThreadStatus,
        updated_at: &str,
    ) -> crate::mr_threads::ThreadState {
        crate::mr_threads::ThreadState {
            discussion_id: "d1".into(),
            last_note_id: 4,
            status,
            plan_summary: "rename x to y".into(),
            confidence: Some(60),
            attempts: 1,
            updated_at: updated_at.into(),
            plan: Some(
                json!({"action": "change_code", "confidence": 60, "files": ["a.rs"],
                              "reply": "Renamed <it>.", "summary": "rename x to y"}),
            ),
            reason: Some("confidence 60 is at or below 80".into()),
            base: None,
            user_instructions: None,
            mr_iid: Some(31),
            last_comment: Some(crate::mr_threads::ReviewNote {
                note_id: 4,
                author: "rev".into(),
                body: "please <b>rename</b> x".into(),
            }),
        }
    }

    fn save_threads(f: &Fx, threads: &[(&str, crate::mr_threads::ThreadState)]) {
        let mut t = crate::mr_threads::IssueThreads::default();
        for (id, s) in threads {
            let mut s = s.clone();
            s.discussion_id = (*id).into();
            t.threads.insert((*id).into(), s);
        }
        t.save("web", 7, Some(&f.p("memory"))).unwrap();
    }

    #[test]
    fn needs_you_lists_mr_threads_in_needs_you_state() {
        use crate::mr_threads::ThreadStatus;
        let f = Fx::new();
        save_threads(
            &f,
            &[
                ("d1", thread(ThreadStatus::NeedsYou, "2026-10-31T09:00:00Z")),
                ("d2", thread(ThreadStatus::Answered, "2026-10-31T09:00:00Z")),
                ("d3", thread(ThreadStatus::Decided, "2026-10-31T09:00:00Z")),
            ],
        );
        // A stray file under memory/ is not a thread state.
        std::fs::write(f.p("memory/web/notes.md"), "x").unwrap();
        let items = f.collect();
        assert_eq!(items.len(), 1, "{items:?}");
        let i = &items[0];
        assert_eq!(i["id"], "mr_thread:web:7:d1");
        assert_eq!(i["kind"], "mr_thread");
        assert_eq!(i["since"], "2026-10-31T09:00:00Z");
        assert_eq!(i["dismissable"], true);
        assert_eq!(
            i["title"],
            "web #7: a review thread on the merge request needs you (confidence 60 is at or below 80)"
        );
        assert_eq!(
            (
                i["project"].as_str(),
                i["issue_iid"].as_i64(),
                i["discussion_id"].as_str()
            ),
            (Some("web"), Some(7), Some("d1"))
        );
        assert_eq!(i["mr_iid"], 31);
        assert_eq!(i["plan_summary"], "rename x to y");
        assert_eq!(i["confidence"], 60);
        assert_eq!(i["action"], "change_code");
        assert_eq!(i["reply"], "Renamed <it>.");
        assert_eq!(i["comment_author"], "rev");
        assert_eq!(i["comment_body"], "please <b>rename</b> x");
        assert_eq!(i["last_note_id"], 4);
    }

    #[test]
    fn dismissed_thread_leaves_the_list_until_newer_note() {
        use crate::mr_threads::ThreadStatus;
        let f = Fx::new();
        save_threads(
            &f,
            &[("d1", thread(ThreadStatus::NeedsYou, "2026-10-31T09:00:00Z"))],
        );
        dismiss(&f.p("dismissed.json"), "mr_thread:web:7:d1", now()).unwrap();
        assert!(f.collect().is_empty());
        // A newer note made the loop send the thread to the user again.
        save_threads(
            &f,
            &[("d1", thread(ThreadStatus::NeedsYou, "2026-10-31T13:00:00Z"))],
        );
        assert_eq!(f.ids(), ["mr_thread:web:7:d1"]);
    }

    #[test]
    fn naive_local_times_are_read_as_local() {
        let local = chrono::Local
            .with_ymd_and_hms(2026, 10, 31, 9, 30, 0)
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(parse("2026-10-31T09:30:00"), Some(local));
        assert_eq!(
            parse("2026-10-31T09:30:00.123456").map(|t| t.timestamp()),
            Some(local.timestamp())
        );
    }

    #[test]
    fn nothing_waiting_is_an_empty_list() {
        assert!(Fx::new().collect().is_empty());
    }

    #[test]
    fn a_pending_approval_waits_until_decided() {
        let f = Fx::new();
        let new = crate::approvals::NewApproval {
            kind: "merge_request".into(),
            run_id: "r".into(),
            project: "web".into(),
            issue_iid: Some(7),
            title: "Fix login".into(),
            branch: "loop/issue-7".into(),
            target_branch: "main".into(),
            summary: "s".into(),
            timeout_seconds: 1800,
            target: None,
        };
        let a = crate::approvals::create(&f.p("approvals"), &new, now()).unwrap();
        let items = f.collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["kind"], "approval");
        assert_eq!(items[0]["title"], "Approve the fix for web #7: Fix login");
        assert_eq!(items[0]["dismissable"], false);
        crate::approvals::decide(
            &f.p("approvals"),
            a["id"].as_str().unwrap(),
            "approve",
            "me",
            now(),
        )
        .unwrap();
        assert!(f.collect().is_empty());
    }

    #[test]
    fn a_profile_proposal_is_not_worded_as_a_fix_for_an_issue() {
        let f = Fx::new();
        let new = crate::approvals::NewApproval {
            kind: "profile-section".into(),
            project: "web".into(),
            title: "Update the stack section of the web profile".into(),
            summary: "s".into(),
            timeout_seconds: 1800,
            target: Some(json!({"section": "stack", "alias": "web"})),
            ..Default::default()
        };
        crate::approvals::create(&f.p("approvals"), &new, now()).unwrap();
        let items = f.collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["kind"], "approval");
        assert_eq!(items[0]["approval_kind"], "profile-section");
        assert_eq!(items[0]["section"], "stack");
        assert_eq!(
            items[0]["title"],
            "Review the proposed stack change to the web profile"
        );
    }

    #[test]
    fn an_escalation_waits_until_the_loop_picks_the_issue_up_again() {
        let f = Fx::new();
        f.event(
            "issue.escalated",
            "2026-10-30T09:00:00+00:00",
            7,
            json!({"reason": "ambiguous"}),
        );
        f.event(
            "issue.escalated",
            "2026-10-30T09:00:00+00:00",
            8,
            json!({"reason": "setup_blocked"}),
        );
        f.event("issue.queued", "2026-10-31T08:00:00+00:00", 8, json!({}));
        let items = f.collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], "escalated:web#7");
        assert_eq!(items[0]["title"], "web #7 was escalated to you (ambiguous)");
        assert_eq!(items[0]["dismissable"], true);
    }

    #[test]
    fn old_escalations_drop_off() {
        let f = Fx::new();
        f.event("issue.escalated", "2026-10-20T09:00:00+00:00", 7, json!({}));
        assert!(f.collect().is_empty());
    }

    #[test]
    fn a_dismissed_escalation_comes_back_when_it_escalates_again() {
        let f = Fx::new();
        f.event("issue.escalated", "2026-10-30T09:00:00+00:00", 7, json!({}));
        dismiss(&f.p("dismissed.json"), "escalated:web#7", now()).unwrap();
        assert!(f.collect().is_empty());
        f.event("issue.escalated", "2026-10-31T13:00:00+00:00", 7, json!({}));
        assert_eq!(f.ids(), ["escalated:web#7"]);
    }

    #[test]
    fn only_confirmed_dead_sign_ins_count() {
        let f = Fx::new();
        std::fs::write(
            f.p("auth_health.json"),
            json!({"checks": {
                "connector:gl": {"ok": false, "message": "401", "label": "Main GitLab", "confirmed": true,
                                 "fail_count": 1, "failing_since": "2026-10-31T06:00:00+00:00"},
                "connector:cal": {"ok": false, "message": "timeout", "confirmed": false, "fail_count": 1},
                "mailbox:old": {"ok": false, "message": "expired", "fail_count": 2},
                "ai_cli": {"ok": true, "message": "fine"}
            }})
            .to_string(),
        )
        .unwrap();
        let items = f.collect();
        let ids: Vec<&str> = items.iter().map(|i| i["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["sign_in:connector:gl", "sign_in:mailbox:old"]);
        assert_eq!(
            items[0]["title"],
            "Sign-in needs attention: Main GitLab - 401"
        );
        assert_eq!(items[0]["dismissable"], true);
    }

    #[test]
    fn a_failed_loop_waits_until_dismissed_or_it_fails_again() {
        let mut f = Fx::new();
        f.loops.insert(
            "rss-loop".into(),
            json!({"state": "failed", "updated_at": "2026-10-31T10:00:00+00:00",
                   "last_exit_code": 1, "consecutive_failures": 3}),
        );
        f.loops
            .insert("topic-loop".into(), json!({"state": "idle"}));
        let items = f.collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "rss-loop failed its last run");
        assert_eq!(items[0]["consecutive_failures"], 3);
        dismiss(&f.p("dismissed.json"), "loop_failed:rss-loop", now()).unwrap();
        assert!(f.collect().is_empty());
        f.loops["rss-loop"]["updated_at"] = json!("2026-10-31T13:00:00+00:00");
        assert_eq!(f.ids(), ["loop_failed:rss-loop"]);
    }

    #[test]
    fn a_failed_profile_sync_waits_until_dismissed_or_it_fails_again() {
        use crate::profile_sync::{with_store, Outcome, ProjectSync};
        let f = Fx::new();
        let store = f.p("profile_sync.json");
        let token = ["glpat-", "abcDEF123456ghiJKL78"].concat();
        let at = |h: u32| Utc.with_ymd_and_hms(2026, 10, 31, h, 0, 0).unwrap();
        with_store(&store, |s| {
            s.status.per_project.insert(
                "web".into(),
                ProjectSync {
                    at: at(9),
                    outcome: Outcome::Failed("checkout not found".into()),
                },
            );
            s.status.per_project.insert(
                "api".into(),
                ProjectSync {
                    at: at(9),
                    outcome: Outcome::Failed(format!("bad {token}")),
                },
            );
            s.status.per_project.insert(
                "docs".into(),
                ProjectSync {
                    at: at(9),
                    outcome: Outcome::Skipped,
                },
            );
        })
        .unwrap();
        let items = f.collect();
        let ids: Vec<&str> = items.iter().map(|i| i["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["profile_sync:api", "profile_sync:web"]);
        assert_eq!(
            items[1]["title"],
            "Profile sync failed for web: checkout not found"
        );
        assert_eq!(items[1]["kind"], "profile_sync_failed");
        assert_eq!(items[1]["project"], "web");
        assert_eq!(items[1]["dismissable"], true);
        assert!(!serde_json::to_string(&items).unwrap().contains("glpat"));
        assert_eq!(items[0]["message"], "details hidden");
        dismiss(&f.p("dismissed.json"), "profile_sync:web", now()).unwrap();
        assert_eq!(f.ids(), ["profile_sync:api"]);
        with_store(&store, |s| {
            s.status.per_project.insert(
                "web".into(),
                ProjectSync {
                    at: at(13),
                    outcome: Outcome::Failed("again".into()),
                },
            );
        })
        .unwrap();
        assert_eq!(f.ids(), ["profile_sync:api", "profile_sync:web"]);
    }

    #[test]
    fn approvals_cannot_be_dismissed() {
        let f = Fx::new();
        assert!(dismiss(&f.p("d.json"), "approval:apr_1", now()).is_err());
    }

    #[test]
    fn a_dismissed_sign_in_stays_hidden_until_it_breaks_again() {
        let f = Fx::new();
        let write = |since: &str| {
            std::fs::write(
                f.p("auth_health.json"),
                json!({"checks": {"mailbox:work": {"ok": false, "message": "invalid_grant",
                    "label": "work (gmail)", "confirmed": true, "failing_since": since}}})
                .to_string(),
            )
            .unwrap();
        };
        write("2026-10-31T06:00:00+00:00");
        assert_eq!(f.collect()[0]["dismissable"], true);
        dismiss(&f.p("dismissed.json"), "sign_in:mailbox:work", now()).unwrap();
        assert!(f.collect().is_empty());
        // Still the same failure on the next probe: still hidden.
        write("2026-10-31T06:00:00+00:00");
        assert!(f.collect().is_empty());
        // It recovered and then broke again later: back in the list.
        write("2026-10-31T13:00:00+00:00");
        assert_eq!(f.ids(), ["sign_in:mailbox:work"]);
    }
}
