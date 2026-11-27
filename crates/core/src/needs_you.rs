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
//! A spec the issue loop posted for approval (`spec_gate`) waits here too,
//! until it is approved or changes are requested.
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
    /// The Slack listener's `status.json`, only while its launchd agent is
    /// installed (`None` otherwise, so a stale file after uninstall is ignored).
    pub slack_status: Option<&'a Path>,
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
            "escalated"
                | "loop_failed"
                | "sign_in"
                | "mr_thread"
                | "profile_sync_failed"
                | "slack_listener"
                | "mr_autofix"
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
            if a["kind"] == crate::user_memory::KIND {
                let text = a["target"]["text"].as_str().unwrap_or("");
                return item(
                    format!("approval:{}", a["id"].as_str().unwrap_or("")),
                    "approval",
                    format!("Remember this preference: {text}"),
                    a["created_at"].as_str(),
                    json!({"approval_id": a["id"], "approval_kind": a["kind"], "preference": text, "source": a["target"]["source"]}),
                );
            }
            if a["kind"] == crate::guide_proposal::KIND {
                // A guide page proposal has no issue, branch or merge request either.
                let slug = a["target"]["slug"].as_str().unwrap_or("");
                return item(
                    format!("approval:{}", a["id"].as_str().unwrap_or("")),
                    "approval",
                    format!("Review the proposed change to the {slug} guide page of {project}"),
                    a["created_at"].as_str(),
                    json!({"approval_id": a["id"], "project": project, "approval_kind": a["kind"], "slug": slug}),
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

/// Pipeline Auto-fix escalations: the newest `mr_autofix.escalated` per
/// `(mr_ref, sha)` in the window, until a newer `mr_autofix.completed` for the MR.
fn autofix_escalations(src: &Sources, dismissed: &Value, now: DateTime<Utc>) -> Vec<Value> {
    let cutoff = now - Duration::days(ESCALATION_DAYS);
    let since = cutoff.format("%Y-%m-%d").to_string();
    let at = |e: &Value| e["timestamp"].as_str().and_then(parse);
    let mut escalated: BTreeMap<(String, String), Value> = BTreeMap::new();
    let mut completed: BTreeMap<String, DateTime<Utc>> = BTreeMap::new();
    for e in crate::events::iter_events(Some(src.events_dir), Some(&since), None) {
        let Some(mr_ref) = e["data"]["mr_ref"].as_str().map(str::to_string) else {
            continue;
        };
        let Some(t) = at(&e) else { continue };
        match e["event_type"].as_str() {
            Some("mr_autofix.completed") => {
                let newest = completed.entry(mr_ref).or_insert(t);
                *newest = (*newest).max(t);
            }
            Some("mr_autofix.escalated") if t >= cutoff => {
                let key = (mr_ref, e["data"]["sha"].as_str().unwrap_or("").to_string());
                if escalated.get(&key).is_none_or(|old| at(old) <= Some(t)) {
                    escalated.insert(key, e);
                }
            }
            _ => {}
        }
    }
    escalated
        .into_iter()
        .filter(|((mr_ref, _), e)| completed.get(mr_ref).is_none_or(|done| at(e) > Some(*done)))
        .filter_map(|((mr_ref, sha), e)| {
            let id = format!("mr_autofix:{mr_ref}:{sha}");
            let since = e["timestamp"].as_str();
            if dismissed_since(dismissed, &id, since) {
                return None;
            }
            let reason = e["data"]["reason"].as_str().unwrap_or("");
            Some(item(
                id,
                "mr_autofix",
                format!("Automatic fix stopped on {mr_ref}: {reason}"),
                since,
                json!({"mr_ref": mr_ref, "mr_web_url": e["data"]["mr_web_url"], "reason": reason, "sha": sha}),
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

/// Specs the issue loop posted that wait on an approval
/// (`<memory root>/<alias>/spec/<iid>.json`, status `awaiting`). Deciding is
/// what clears them, so they can't be dismissed.
fn specs(src: &Sources) -> Vec<Value> {
    crate::spec_gate::awaiting(src.memory_root)
        .into_iter()
        .map(|(alias, iid, state)| {
            item(
                format!("spec:{alias}:{iid}"),
                "spec",
                format!("Approve the spec for {alias} #{iid}"),
                Some(&state.posted_at),
                json!({
                    "project": alias,
                    "issue_iid": iid,
                    "body": state.spec.chars().take(400).collect::<String>(),
                    "body_truncated": state.spec.chars().count() > 400,
                }),
            )
        })
        .collect()
}

/// The installed Slack listener stopped on a problem only the user can fix
/// (usually a rejected token). Its `last_error` is token-free by construction.
/// Fixing it (the listener writes a new state) or dismissing it clears it; a
/// newer failure brings it back.
fn slack_listener(src: &Sources, dismissed: &Value) -> Vec<Value> {
    use crate::slack_agent::status::{read_status, State};
    let Some(status) = src.slack_status.and_then(read_status) else {
        return Vec::new();
    };
    if status.state != State::NeedsAttention {
        return Vec::new();
    }
    let id = "slack_listener".to_string();
    if dismissed_since(dismissed, &id, Some(&status.since)) {
        return Vec::new();
    }
    let error = status.last_error.unwrap_or_default();
    vec![item(
        id,
        "slack_listener",
        format!("The Slack listener needs attention: {error}"),
        Some(&status.since),
        json!({"error": error}),
    )]
}

/// Everything waiting on a person, oldest first.
pub fn collect(src: &Sources, now: DateTime<Utc>) -> Vec<Value> {
    let dismissed = read_json(src.dismissed);
    let mut items = approvals(src, now);
    items.extend(escalations(src, &dismissed, now));
    items.extend(autofix_escalations(src, &dismissed, now));
    items.extend(sign_ins(src, &dismissed));
    items.extend(failed_loops(src, &dismissed));
    items.extend(mr_threads(src, &dismissed));
    items.extend(specs(src));
    items.extend(profile_sync_failures(src, &dismissed));
    items.extend(slack_listener(src, &dismissed));
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
        "slack_listener",
        "mr_autofix:",
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
        slack_status: Option<PathBuf>,
    }

    impl Fx {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
                loops: Map::new(),
                slack_status: None,
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
                    slack_status: self.slack_status.as_deref(),
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
    fn a_guide_page_proposal_names_its_page_and_project() {
        use crate::project_guide::{self, Kind, Page, Part};
        use crate::project_profile::{Origin, Status};
        let f = Fx::new();
        let mem = tempfile::tempdir().unwrap();
        let page = Page {
            alias: "web".into(),
            slug: "feature-orders".into(),
            kind: Kind::Feature,
            title: "Orders".into(),
            status: Status::Approved,
            generated_at: Some(now()),
            head_sha: Some("abc123".into()),
            summary: "Taking orders.".into(),
            related: vec![],
            parts: vec![Part {
                key: "what".into(),
                body: "Lets a customer place an order.".into(),
                origin: Origin::Agent,
            }],
            sources: vec![],
        };
        project_guide::save(&page, Some(mem.path())).unwrap();
        let mut held = page.clone();
        held.status = Status::Draft;
        held.parts[0].body = "Places and cancels orders.".into();
        crate::guide_proposal::raise("web", &held, &f.p("approvals"), Some(mem.path()), now())
            .unwrap()
            .expect("a proposal");
        let items = f.collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["kind"], "approval");
        assert_eq!(items[0]["approval_kind"], "guide-page");
        assert_eq!(items[0]["slug"], "feature-orders");
        assert_eq!(items[0]["project"], "web");
        assert!(items[0].get("issue_iid").is_none(), "{}", items[0]);
        assert!(items[0].get("section").is_none(), "{}", items[0]);
        assert_eq!(
            items[0]["title"],
            "Review the proposed change to the feature-orders guide page of web"
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

    fn autofix(f: &Fx, kind: &str, at: &str, sha: &str, reason: &str) {
        f.event(
            kind,
            at,
            0,
            json!({"mr_ref": "work:9!7", "mr_web_url": "https://gl/g/p/-/merge_requests/7",
                   "sha": sha, "pipeline_id": 100, "reason": reason}),
        );
    }

    #[test]
    fn autofix_escalation_is_listed_until_a_fix_completes() {
        let f = Fx::new();
        autofix(
            &f,
            "mr_autofix.escalated",
            "2026-10-30T09:00:00+00:00",
            "abc",
            "fix_failed",
        );
        autofix(
            &f,
            "mr_autofix.escalated",
            "2026-10-30T09:00:00+00:00",
            "abc",
            "fix_failed",
        );
        let items = f.collect();
        assert_eq!(items.len(), 1, "{items:?}");
        let it = &items[0];
        assert_eq!(it["id"], "mr_autofix:work:9!7:abc");
        assert_eq!(it["kind"], "mr_autofix");
        assert_eq!(it["dismissable"], true);
        assert_eq!(it["mr_ref"], "work:9!7");
        assert_eq!(it["mr_web_url"], "https://gl/g/p/-/merge_requests/7");
        assert_eq!(it["reason"], "fix_failed");
        assert_eq!(it["since"], "2026-10-30T09:00:00+00:00");

        autofix(
            &f,
            "mr_autofix.escalated",
            "2026-10-30T10:00:00+00:00",
            "def",
            "already_attempted",
        );
        assert_eq!(
            f.ids(),
            ["mr_autofix:work:9!7:abc", "mr_autofix:work:9!7:def"]
        );
        autofix(
            &f,
            "mr_autofix.completed",
            "2026-10-30T11:00:00+00:00",
            "ghi",
            "",
        );
        assert!(
            f.collect().is_empty(),
            "a newer completed fix clears the MR"
        );
        autofix(
            &f,
            "mr_autofix.escalated",
            "2026-10-30T12:00:00+00:00",
            "jkl",
            "already_attempted",
        );
        assert_eq!(f.ids(), ["mr_autofix:work:9!7:jkl"]);

        dismiss(&f.p("dismissed.json"), "mr_autofix:work:9!7:jkl", now()).unwrap();
        assert!(f.collect().is_empty());
        autofix(
            &f,
            "mr_autofix.escalated",
            "2026-10-20T12:00:00+00:00",
            "old",
            "daily_cap",
        );
        assert!(f.collect().is_empty(), "older than 7 days");
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
    fn spec_items_listed_while_awaiting() {
        use crate::spec_gate;
        let f = Fx::new();
        let memory = f.p("memory");
        let long = format!("**Goal:** paging\n{}", "x".repeat(600));
        spec_gate::record_posted(&memory, "web", "7", &long, now()).unwrap();
        spec_gate::record_posted(&memory, "api", "3", "**Goal:** y", now()).unwrap();
        spec_gate::decide(&memory, "api", "3", true, "", "dashboard", now()).unwrap();
        // A stray file in a spec folder is not a spec state.
        std::fs::write(memory.join("web/spec/notes.txt"), "x").unwrap();
        let items = f.collect();
        assert_eq!(items.len(), 1, "{items:?}");
        let i = &items[0];
        assert_eq!(i["id"], "spec:web:7");
        assert_eq!(i["kind"], "spec");
        assert_eq!(i["dismissable"], false);
        assert_eq!(i["project"], "web");
        assert_eq!(i["issue_iid"], "7");
        assert_eq!(i["title"], "Approve the spec for web #7");
        assert_eq!(i["since"], now().to_rfc3339());
        let body = i["body"].as_str().unwrap();
        assert!(body.starts_with("**Goal:** paging"), "{body}");
        assert_eq!(body.chars().count(), 400);
        assert!(dismiss(&f.p("d.json"), "spec:web:7", now()).is_err());

        spec_gate::decide(&memory, "web", "7", false, "More.", "dashboard", now()).unwrap();
        assert!(f.collect().is_empty());
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

    #[test]
    fn a_slack_listener_that_needs_attention_waits_until_fixed_or_dismissed() {
        use crate::slack_agent::status::{write_status, State, Status};
        let mut f = Fx::new();
        let path = f.p("slack/status.json");
        let status = |state, since: &str| Status {
            state,
            since: since.into(),
            last_event: None,
            last_error: Some("Slack rejected the token (invalid_auth)".into()),
        };
        write_status(
            &path,
            &status(State::NeedsAttention, "2026-10-31T09:00:00Z"),
        );
        // Not installed (the caller passes no status): nothing, whatever the file says.
        assert!(f.ids().is_empty());
        f.slack_status = Some(path.clone());
        let items = f.collect();
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item["kind"], "slack_listener");
        assert_eq!(item["id"], "slack_listener");
        assert_eq!(item["error"], "Slack rejected the token (invalid_auth)");
        assert_eq!(item["since"], "2026-10-31T09:00:00Z");
        assert_eq!(item["dismissable"], true);
        for ok in [State::Connected, State::Offline] {
            write_status(&path, &status(ok, "2026-10-31T10:00:00Z"));
            assert!(f.ids().is_empty(), "{ok:?}");
        }
        write_status(
            &path,
            &status(State::NeedsAttention, "2026-10-31T09:00:00Z"),
        );
        dismiss(&f.p("dismissed.json"), "slack_listener", now()).unwrap();
        assert!(f.ids().is_empty());
        // A newer rejection comes back.
        write_status(
            &path,
            &status(State::NeedsAttention, "2026-10-31T13:00:00Z"),
        );
        assert_eq!(f.ids(), ["slack_listener"]);
    }
}
