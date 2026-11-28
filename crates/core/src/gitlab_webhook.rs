//! GitLab webhooks (`POST /api/inbox/gitlab`), so a comment, an assignment or a
//! failed pipeline starts work within a minute instead of at the next poll. The
//! dashboard listens on 127.0.0.1 only, so GitLab reaches it through a tunnel
//! you run. Every request must carry the secret token (`X-Gitlab-Token`) set up
//! with `loopx webhook setup`; a delivery GitLab retries (same
//! `X-Gitlab-Event-UUID`) is acted on once. A comment on, or assignment of, an
//! issue of a configured project queues that issue's run; a failed pipeline
//! queues the Pipeline Doctor. Nothing from the payload's text is stored.

use crate::keychain::SecretStore;
use crate::run_queue::{Reason, RunQueue};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

pub const TOKEN_SERVICE: &str = "loop-engineering.gitlab-webhook";
pub const TOKEN_ACCOUNT: &str = "token";
pub const PATH: &str = "/api/inbox/gitlab";
/// Delivery ids remembered for de-duplication.
const SEEN_CAP: usize = 1000;
pub const PIPELINE_LOOP: &str = "pipeline-doctor-loop";
pub const MR_REVIEW_LOOP: &str = "mr-review-loop";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Unauthorized,
    Duplicate,
    /// Accepted; the runs it queued (possibly none).
    Accepted(Vec<String>),
}

/// Where the handler reads and writes.
pub struct Env<'a> {
    pub tokens: &'a dyn SecretStore,
    pub seen_path: PathBuf,
    /// `projects.json`.
    pub projects: PathBuf,
    pub queue: &'a RunQueue,
    pub events_dir: Option<PathBuf>,
    /// Where `loopx <verb>` merge request commands are queued for the MR Review loop.
    pub commands_dir: PathBuf,
}

impl Env<'_> {
    pub fn seen_path_in(home: &Path) -> PathBuf {
        home.join("gitlab_webhook_seen.json")
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether `uuid` was handled before; records it when not.
fn first_delivery(path: &Path, uuid: &str) -> bool {
    if uuid.is_empty() {
        return true;
    }
    let Ok(_lock) = crate::file_lock::FileLock::exclusive(&path.with_extension("lock")) else {
        return true;
    };
    let mut seen: VecDeque<String> = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if seen.iter().any(|s| s == uuid) {
        return false;
    }
    seen.push_back(uuid.to_string());
    while seen.len() > SEEN_CAP {
        seen.pop_front();
    }
    if let Ok(bytes) = serde_json::to_vec(&seen) {
        let _ = crate::settings_store::atomic_write(path, &bytes, Some(0o600));
    }
    true
}

/// The configured alias of the payload's project, matched by path or numeric id.
fn alias_of(projects: &Path, project: &Value) -> Option<String> {
    let config = crate::config::Config::load(projects).ok()?;
    let path = project["path_with_namespace"].as_str().unwrap_or("");
    let id = project["id"]
        .as_i64()
        .map(|i| i.to_string())
        .unwrap_or_default();
    config.aliases().ok()?.into_iter().find(|alias| {
        config
            .project(alias)
            .ok()
            .and_then(|p| p["project_id"].as_str().map(str::to_string))
            .is_some_and(|pid| (!path.is_empty() && pid == path) || (!id.is_empty() && pid == id))
    })
}

fn me(projects: &Path) -> Option<String> {
    crate::config::Config::load(projects)
        .ok()?
        .assignee_username()
        .ok()
}

/// A `loopx <verb>` command on a merge request, to queue for the MR Review loop.
fn mr_command(payload: &Value, now: &str) -> Option<crate::review::commands::Queued> {
    let attrs = &payload["object_attributes"];
    let verb = crate::review::commands::parse(attrs["note"].as_str()?)?;
    let pid = &payload["project"]["id"];
    Some(crate::review::commands::Queued {
        verb,
        project_id: pid
            .as_i64()
            .map(|i| i.to_string())
            .or_else(|| pid.as_str().map(str::to_string))?,
        iid: payload["merge_request"]["iid"].as_i64()?.to_string(),
        author: payload["user"]["username"].as_str()?.to_string(),
        author_id: payload["user"]["id"].as_i64().unwrap_or(0),
        received_at: now.to_string(),
    })
}

/// Is `me` an assignee or a reviewer of the merge request in this payload?
fn mine(payload: &Value, me: Option<&str>) -> bool {
    let Some(me) = me else { return false };
    ["assignees", "reviewers"].iter().any(|k| {
        payload[*k]
            .as_array()
            .is_some_and(|a| a.iter().any(|u| u["username"].as_str() == Some(me)))
    })
}

/// What a delivery asks for: `(loop, args)` to queue, and a summary for the event log.
fn wanted(payload: &Value, projects: &Path) -> (Option<(String, Vec<String>)>, Value) {
    let kind = payload["object_kind"].as_str().unwrap_or("");
    let alias = alias_of(projects, &payload["project"]);
    let me = me(projects);
    let mut summary = json!({"kind": kind, "project": alias});
    let issue_run = |iid: Option<i64>| -> Option<(String, Vec<String>)> {
        Some((
            crate::loops_config::ISSUE_LOOP.to_string(),
            vec![alias.clone()?, iid?.to_string()],
        ))
    };
    let queued = match kind {
        "note" if payload["object_attributes"]["noteable_type"] == "Issue" => {
            let author = payload["user"]["username"].as_str();
            let iid = payload["issue"]["iid"].as_i64();
            summary["issue_iid"] = json!(iid);
            if author.is_some() && author == me.as_deref() {
                summary["skipped"] = json!("own comment");
                None
            } else {
                issue_run(iid)
            }
        }
        "issue" => {
            let attrs = &payload["object_attributes"];
            let iid = attrs["iid"].as_i64();
            summary["issue_iid"] = json!(iid);
            summary["action"] = attrs["action"].clone();
            let assigned = payload["assignees"]
                .as_array()
                .is_some_and(|a| a.iter().any(|u| u["username"].as_str() == me.as_deref()));
            let open = matches!(attrs["action"].as_str(), Some("open" | "reopen" | "update"));
            if assigned && open && attrs["state"] != "closed" {
                issue_run(iid)
            } else {
                None
            }
        }
        "note" if payload["object_attributes"]["noteable_type"] == "MergeRequest" => {
            summary["mr_iid"] = payload["merge_request"]["iid"].clone();
            crate::review::commands::parse(
                payload["object_attributes"]["note"].as_str().unwrap_or(""),
            )
            .map(|v| {
                summary["command"] = json!(v.name());
                (MR_REVIEW_LOOP.to_string(), Vec::new())
            })
        }
        "merge_request" => {
            let attrs = &payload["object_attributes"];
            let action = attrs["action"].as_str().unwrap_or("");
            summary["mr_iid"] = attrs["iid"].clone();
            summary["action"] = json!(action);
            let pushed =
                action == "update" && attrs["oldrev"].as_str().is_some_and(|r| !r.is_empty());
            let opened = matches!(action, "open" | "reopen");
            ((pushed || opened) && mine(payload, me.as_deref()))
                .then(|| (MR_REVIEW_LOOP.to_string(), Vec::new()))
        }
        "pipeline" => {
            let status = payload["object_attributes"]["status"]
                .as_str()
                .unwrap_or("");
            summary["status"] = json!(status);
            (status == "failed" && alias.is_some()).then(|| (PIPELINE_LOOP.to_string(), Vec::new()))
        }
        _ => None,
    };
    (queued, summary)
}

/// Handle one delivery.
pub fn handle(env: &Env<'_>, token: &str, uuid: &str, body: &[u8], now: &str) -> Outcome {
    let expected = env
        .tokens
        .get(TOKEN_ACCOUNT)
        .ok()
        .flatten()
        .unwrap_or_default();
    if expected.is_empty() || !constant_time_eq(token.as_bytes(), expected.as_bytes()) {
        return Outcome::Unauthorized;
    }
    if !first_delivery(&env.seen_path, uuid) {
        return Outcome::Duplicate;
    }
    let Ok(payload) = serde_json::from_slice::<Value>(body) else {
        return Outcome::Accepted(Vec::new());
    };
    let (queued, mut summary) = wanted(&payload, &env.projects);
    if queued.is_some() && summary.get("command").is_some() {
        let saved = mr_command(&payload, now).map(|q| q.save(&env.commands_dir));
        if !matches!(saved, Some(Ok(_))) {
            summary["skipped"] = json!("command not queued");
            let _ = crate::events::emit(
                "gitlab.webhook",
                "webhook",
                crate::events::EventFields {
                    data: Some(json!({"summary": summary, "queued": []})),
                    ..Default::default()
                },
                env.events_dir.as_deref(),
            );
            return Outcome::Accepted(Vec::new());
        }
    }
    let mut done = Vec::new();
    if let Some((loop_name, args)) = queued {
        if env
            .queue
            .enqueue_with_args(&loop_name, &args, Reason::Event, now)
            .is_ok()
        {
            done.push(format!("{loop_name} {}", args.join(" ")).trim().to_string());
        }
    }
    let _ = crate::events::emit(
        "gitlab.webhook",
        "webhook",
        crate::events::EventFields {
            data: Some(json!({"summary": summary, "queued": done})),
            ..Default::default()
        },
        env.events_dir.as_deref(),
    );
    Outcome::Accepted(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MemoryStore;

    struct Fx {
        d: tempfile::TempDir,
        tokens: MemoryStore,
        queue: RunQueue,
    }

    impl Fx {
        fn new() -> Self {
            let d = tempfile::tempdir().unwrap();
            std::fs::write(
                d.path().join("projects.json"),
                r#"{"gitlab_instance": "main", "assignee_username": "me",
                    "projects": {"web": {"project_id": "group/web"}, "api": {"project_id": "42"}}}"#,
            )
            .unwrap();
            let tokens = MemoryStore::default();
            tokens.set(TOKEN_ACCOUNT, "s3cret").unwrap();
            let queue = RunQueue::new(d.path().join("queue.json"));
            Self { d, tokens, queue }
        }

        fn send(&self, token: &str, uuid: &str, body: Value) -> Outcome {
            let env = Env {
                tokens: &self.tokens,
                seen_path: self.d.path().join("seen.json"),
                projects: self.d.path().join("projects.json"),
                queue: &self.queue,
                events_dir: Some(self.d.path().join("events")),
                commands_dir: self.d.path().join("commands"),
            };
            handle(
                &env,
                token,
                uuid,
                body.to_string().as_bytes(),
                "2026-11-27T10:00:00",
            )
        }
    }

    #[test]
    fn mr_comment_commands_are_queued_for_the_review_loop_and_other_comments_ignored() {
        let fx = Fx::new();
        let note = |text: &str| {
            json!({"object_kind": "note", "user": {"username": "ana", "id": 42},
                   "project": {"id": 9, "path_with_namespace": "group/other"},
                   "object_attributes": {"noteable_type": "MergeRequest", "note": text},
                   "merge_request": {"iid": 7}})
        };
        assert_eq!(
            fx.send("s3cret", "a", note("loopx fix\nthanks")),
            Outcome::Accepted(vec!["mr-review-loop".into()])
        );
        let queued = crate::review::commands::pending(&fx.d.path().join("commands"));
        assert_eq!(queued.len(), 1);
        let q = &queued[0].1;
        assert_eq!(
            (
                q.verb,
                q.project_id.as_str(),
                q.iid.as_str(),
                q.author.as_str(),
                q.author_id
            ),
            (crate::review::commands::Verb::Fix, "9", "7", "ana", 42)
        );
        assert!(
            !std::fs::read_to_string(&queued[0].0)
                .unwrap()
                .contains("thanks"),
            "no comment text kept"
        );
        assert_eq!(
            fx.send("s3cret", "b", note("looks good, loopx review later")),
            Outcome::Accepted(vec![])
        );
        assert_eq!(
            crate::review::commands::pending(&fx.d.path().join("commands")).len(),
            1
        );
    }

    #[test]
    fn a_push_to_my_merge_request_queues_a_review() {
        let fx = Fx::new();
        let mr = |action: &str, oldrev: Option<&str>, reviewer: &str| {
            json!({"object_kind": "merge_request", "project": {"id": 9},
                   "object_attributes": {"iid": 7, "action": action, "oldrev": oldrev},
                   "assignees": [{"username": "bob"}], "reviewers": [{"username": reviewer}]})
        };
        assert_eq!(
            fx.send("s3cret", "1", mr("update", Some("abc"), "me")),
            Outcome::Accepted(vec!["mr-review-loop".into()])
        );
        assert_eq!(
            fx.send("s3cret", "2", mr("update", None, "me")),
            Outcome::Accepted(vec![]),
            "label change, no push"
        );
        assert_eq!(
            fx.send("s3cret", "3", mr("update", Some("abc"), "zed")),
            Outcome::Accepted(vec![]),
            "not mine"
        );
        assert_eq!(
            fx.send("s3cret", "4", mr("open", None, "me")),
            Outcome::Accepted(vec!["mr-review-loop".into()])
        );
    }

    fn comment(author: &str, project: Value) -> Value {
        json!({"object_kind": "note", "user": {"username": author}, "project": project,
               "object_attributes": {"noteable_type": "Issue", "note": "please also fix X"},
               "issue": {"iid": 7}})
    }

    #[test]
    fn a_comment_on_an_issue_queues_that_issue_once() {
        let fx = Fx::new();
        let body = comment(
            "alice",
            json!({"path_with_namespace": "group/web", "id": 9}),
        );
        assert_eq!(
            fx.send("s3cret", "u1", body.clone()),
            Outcome::Accepted(vec!["issue-loop web 7".into()])
        );
        assert_eq!(fx.send("s3cret", "u1", body), Outcome::Duplicate);
        let events = std::fs::read_dir(fx.d.path().join("events"))
            .unwrap()
            .count();
        assert_eq!(events, 1);
        let log: String = std::fs::read_dir(fx.d.path().join("events"))
            .unwrap()
            .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
            .collect();
        assert!(!log.contains("please also fix X"), "{log}");
    }

    #[test]
    fn a_wrong_or_missing_token_is_refused() {
        let fx = Fx::new();
        let body = comment("alice", json!({"path_with_namespace": "group/web"}));
        assert_eq!(fx.send("nope", "u1", body.clone()), Outcome::Unauthorized);
        assert_eq!(fx.send("", "u2", body), Outcome::Unauthorized);
        let empty = Fx::new();
        empty.tokens.delete(TOKEN_ACCOUNT).unwrap();
        assert_eq!(empty.send("", "u3", json!({})), Outcome::Unauthorized);
    }

    #[test]
    fn the_loops_own_comments_and_unknown_projects_queue_nothing() {
        let fx = Fx::new();
        assert_eq!(
            fx.send(
                "s3cret",
                "a",
                comment("me", json!({"path_with_namespace": "group/web"}))
            ),
            Outcome::Accepted(vec![])
        );
        assert_eq!(
            fx.send(
                "s3cret",
                "b",
                comment("alice", json!({"path_with_namespace": "other/repo"}))
            ),
            Outcome::Accepted(vec![])
        );
        assert_eq!(
            fx.send("s3cret", "c", json!({"object_kind": "push"})),
            Outcome::Accepted(vec![])
        );
    }

    #[test]
    fn an_assignment_and_a_failed_pipeline_start_work() {
        let fx = Fx::new();
        let assigned = json!({"object_kind": "issue", "project": {"id": 42},
            "object_attributes": {"iid": 3, "action": "update", "state": "opened"},
            "assignees": [{"username": "me"}]});
        assert_eq!(
            fx.send("s3cret", "x", assigned),
            Outcome::Accepted(vec!["issue-loop api 3".into()])
        );
        let failed = json!({"object_kind": "pipeline", "project": {"path_with_namespace": "group/web"},
            "object_attributes": {"status": "failed"}});
        assert_eq!(
            fx.send("s3cret", "y", failed),
            Outcome::Accepted(vec![PIPELINE_LOOP.into()])
        );
        let passed = json!({"object_kind": "pipeline", "project": {"path_with_namespace": "group/web"},
            "object_attributes": {"status": "success"}});
        assert_eq!(fx.send("s3cret", "z", passed), Outcome::Accepted(vec![]));
    }
}
