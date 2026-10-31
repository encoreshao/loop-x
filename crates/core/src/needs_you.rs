//! "Needs you": everything that is waiting on a person, in one list. An
//! approval request, an issue the GitLab loop escalated, a sign-in that has
//! died, a loop whose last run failed. The dashboard's topbar bell and the Mac
//! app's menu bar count them; each one already alerted once in Slack, so this
//! is where it stays visible instead of being alerted again.
//!
//! Escalations, failing loops and dead sign-ins can be dismissed
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
}

/// An RFC 3339 time, or a naive one in local time (`auth_health`, run status).
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
        "dismissable": matches!(kind, "escalated" | "loop_failed" | "sign_in"),
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

/// Everything waiting on a person, oldest first.
pub fn collect(src: &Sources, now: DateTime<Utc>) -> Vec<Value> {
    let dismissed = read_json(src.dismissed);
    let mut items = approvals(src, now);
    items.extend(escalations(src, &dismissed, now));
    items.extend(sign_ins(src, &dismissed));
    items.extend(failed_loops(src, &dismissed));
    // Oldest first; an item with no time goes last.
    items.sort_by_key(|i| {
        let since = i["since"].as_str().and_then(parse);
        (since.is_none(), since)
    });
    items
}

/// Hide a dismissable item until it happens again.
pub fn dismiss(path: &Path, id: &str, now: DateTime<Utc>) -> Result<(), String> {
    if !["escalated:", "loop_failed:", "sign_in:"]
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
            let (approvals, events, auth, dismissed) = (
                self.p("approvals"),
                self.p("events"),
                self.p("auth_health.json"),
                self.p("dismissed.json"),
            );
            collect(
                &Sources {
                    approvals_dir: &approvals,
                    events_dir: &events,
                    auth_state: &auth,
                    loops: &self.loops,
                    dismissed: &dismissed,
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
