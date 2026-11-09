//! The quiet window after the loop opens a merge request.
//!
//! Once `loop/issue-<iid>` has an open MR that is less than [`QUIET_DAYS`] old,
//! the issue is already handed off: a scheduled check or a manual run must not
//! launch the agent (which would post another comment) unless someone other
//! than the loop's own account has written on the issue since. The loop posts
//! as the assignee, so "the assignee's notes" are the loop's and the user's own.
//! No model is involved.

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

pub const QUIET_DAYS: i64 = 7;

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Do nothing this run; the MR is `mr_iid`.
    Quiet { mr_iid: i64 },
    /// Run the issue as usual.
    Run,
}

fn id_of(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn parse(t: Option<&Value>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(t?.as_str()?)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// `issue` is the synced issue cache (`notes`, `_notes`, and `loop_mr` with
/// `iid` and `created_at`). Anything unclear is [`Verdict::Run`].
pub fn verdict(issue: &Value, assignee: &str, now: DateTime<Utc>) -> Verdict {
    let Some(mr) = issue.get("loop_mr").filter(|m| m.is_object()) else {
        return Verdict::Run;
    };
    let (Some(mr_iid), Some(created)) = (
        mr.get("iid").and_then(Value::as_i64),
        parse(mr.get("created_at")),
    ) else {
        return Verdict::Run;
    };
    if now - created >= Duration::days(QUIET_DAYS) {
        return Verdict::Run;
    }
    let seen = id_of(issue.pointer("/_notes/loop_last_seen_note_id"));
    let someone_wrote = issue
        .get("notes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|n| {
            n.get("system").and_then(Value::as_bool) != Some(true)
                && n.pointer("/author/username").and_then(Value::as_str) != Some(assignee)
                && id_of(n.get("id")) > seen
                && parse(n.get("created_at")).is_none_or(|t| t > created)
        });
    if someone_wrote {
        Verdict::Run
    } else {
        Verdict::Quiet { mr_iid }
    }
}

/// Fetches the issue and its open loop MR, merges them into the local cache
/// through `sync`, and applies [`verdict`]. Fails open: any error runs the
/// issue as usual.
pub fn check_with(
    client: &crate::gitlab::Client,
    project_id: &str,
    issue_iid: &str,
    assignee: &str,
    sync: &dyn Fn(&Value) -> Result<Value, String>,
) -> Verdict {
    let go = || -> Result<Verdict, String> {
        let iid: i64 = issue_iid.parse().map_err(|_| "bad iid".to_string())?;
        let mut fresh = client
            .get_issue(project_id, iid)
            .map_err(|e| e.to_string())?;
        fresh["loop_mr"] = client
            .loop_merge_request(project_id, iid)
            .map_err(|e| e.to_string())?
            .unwrap_or(Value::Null);
        let merged = sync(&fresh)?;
        Ok(verdict(&merged, assignee, Utc::now()))
    };
    go().unwrap_or(Verdict::Run)
}

/// [`check_with`] against the configured GitLab and the real issue cache.
pub fn check(
    project_id: &str,
    instance: Option<&str>,
    cache_instance: &str,
    issue_iid: &str,
    assignee: &str,
) -> Verdict {
    let config = crate::gitlab::load_config();
    let Ok(client) = crate::gitlab::Client::from_config(config.as_ref(), instance, None) else {
        return Verdict::Run;
    };
    check_with(&client, project_id, issue_iid, assignee, &|fresh| {
        crate::gitlab_cache::sync_issue(cache_instance, project_id, issue_iid, fresh)
            .map_err(|e| e.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        "2026-11-07T08:00:00Z".parse().unwrap()
    }

    fn issue(notes: Value, seen: Option<&str>) -> Value {
        json!({
            "loop_mr": {"iid": 7, "created_at": "2026-11-06T09:00:00Z"},
            "notes": notes,
            "_notes": seen.map(|s| json!({"loop_last_seen_note_id": s})).unwrap_or(json!({})),
        })
    }

    fn note(id: i64, user: &str, at: &str) -> Value {
        json!({"id": id, "author": {"username": user}, "created_at": at, "system": false})
    }

    #[test]
    fn a_fresh_mr_with_only_our_own_notes_is_quiet() {
        let i = issue(
            json!([
                note(5, "me", "2026-11-06T09:05:00Z"),
                note(6, "me", "2026-11-06T10:00:00Z")
            ]),
            None,
        );
        assert_eq!(verdict(&i, "me", now()), Verdict::Quiet { mr_iid: 7 });
    }

    #[test]
    fn someone_elses_new_comment_runs_the_issue() {
        let i = issue(json!([note(8, "alice", "2026-11-06T12:00:00Z")]), Some("6"));
        assert_eq!(verdict(&i, "me", now()), Verdict::Run);
    }

    #[test]
    fn a_comment_already_seen_or_from_before_the_mr_stays_quiet() {
        let seen = issue(json!([note(8, "alice", "2026-11-06T12:00:00Z")]), Some("8"));
        assert_eq!(verdict(&seen, "me", now()), Verdict::Quiet { mr_iid: 7 });
        let older = issue(json!([note(3, "alice", "2026-11-05T12:00:00Z")]), None);
        assert_eq!(verdict(&older, "me", now()), Verdict::Quiet { mr_iid: 7 });
    }

    #[test]
    fn system_notes_do_not_count() {
        let mut n = note(9, "gitlab-bot", "2026-11-06T12:00:00Z");
        n["system"] = json!(true);
        assert_eq!(
            verdict(&issue(json!([n]), None), "me", now()),
            Verdict::Quiet { mr_iid: 7 }
        );
    }

    #[test]
    fn after_a_week_or_without_an_mr_the_issue_runs() {
        let later: DateTime<Utc> = "2026-11-13T09:00:01Z".parse().unwrap();
        assert_eq!(verdict(&issue(json!([]), None), "me", later), Verdict::Run);
        assert_eq!(
            verdict(&json!({"notes": [], "loop_mr": null}), "me", now()),
            Verdict::Run
        );
        assert_eq!(verdict(&json!({"notes": []}), "me", now()), Verdict::Run);
    }

    #[test]
    fn check_with_asks_gitlab_and_stays_quiet_for_only_our_notes() {
        let served = crate::connectors::testutil::serve(vec![
            (
                200,
                "Content-Type: application/json\r\n",
                json!({"iid": 7}).to_string(),
            ),
            (
                200,
                "Content-Type: application/json\r\n",
                json!([note(5, "me", "2099-01-01T00:00:00Z")]).to_string(),
            ),
            (
                200,
                "Content-Type: application/json\r\n",
                json!([{"iid": 3, "web_url": "u", "created_at": Utc::now().to_rfc3339()}])
                    .to_string(),
            ),
        ]);
        let cfg = json!({"default": "g", "instances": {"g": {"url": served.url, "token": "t"}}});
        let client = crate::gitlab::Client::from_config(Some(&cfg), None, None).unwrap();
        let sync = |fresh: &Value| Ok(fresh.clone());
        assert_eq!(
            check_with(&client, "g/web", "7", "me", &sync),
            Verdict::Quiet { mr_iid: 3 }
        );
    }

    #[test]
    fn check_with_fails_open_when_gitlab_errors() {
        let cfg = json!({"default": "g", "instances": {"g": {"url": "http://127.0.0.1:1", "token": "t"}}});
        let client = crate::gitlab::Client::from_config(Some(&cfg), None, None).unwrap();
        assert_eq!(
            check_with(&client, "g/web", "7", "me", &|f| Ok(f.clone())),
            Verdict::Run
        );
    }
}
