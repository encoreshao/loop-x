//! A gated fix waiting on a person's OK before its merge request opens. The
//! run that asks for the approval saves what opening the MR needs here and
//! ends; the scheduler tick relays chat answers and, once the approval is
//! decided, queues a single-issue run that takes the record and finishes the
//! job (`issue_runner::run_one_issue`).
//!
//! One file per issue, `<dir>/<alias>-<iid>.json`. Every write and the take
//! happen under `<dir>/.lock`, so the tick never brings back a record a run
//! has already taken.

use crate::approval_relay::{Relay, SavedChannel};
use crate::approvals;
use crate::connectors::ConnectorSource;
use crate::events::{fmt_timestamp, new_uuid_hex};
use crate::file_lock::FileLock;
use crate::run_queue::{Enqueued, Reason, RunQueue};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io;
use std::path::{Path, PathBuf};

/// A decided record the queued run has not taken yet is queued again after this long.
const REQUEUE_AFTER_MINUTES: i64 = 10;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Parked {
    pub approval_id: String,
    /// The run that asked for the approval.
    pub run_id: String,
    pub alias: String,
    pub issue_iid: String,
    /// The fix handoff the agent wrote.
    pub handoff: Value,
    /// The check commands the loop's verification ran.
    pub checks: Vec<String>,
    #[serde(default)]
    pub pre_review: Option<String>,
    pub revisions_left: usize,
    #[serde(default)]
    pub setup_fixed: Option<usize>,
    pub parked_at: String,
    /// The chat apps the request went to.
    #[serde(default)]
    pub relay: Vec<SavedChannel>,
    /// When the tick last queued the run that resumes it.
    #[serde(default)]
    pub queued_at: Option<String>,
}

pub fn default_dir() -> PathBuf {
    crate::runner_util::repo_root()
        .join("outputs")
        .join("parked-approvals")
}

fn path(dir: &Path, alias: &str, iid: &str) -> PathBuf {
    let name = format!("{alias}-{iid}.json");
    // Only the final component, so an alias can never escape the store.
    let safe = Path::new(&name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dir.join(safe)
}

fn lock(dir: &Path) -> io::Result<FileLock> {
    std::fs::create_dir_all(dir)?;
    FileLock::exclusive(&dir.join(".lock"))
}

fn read(path: &Path) -> Option<Parked> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write(dir: &Path, parked: &Parked) -> io::Result<()> {
    let tmp = dir.join(format!(".{}.tmp", &new_uuid_hex()[..8]));
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(parked).map_err(io::Error::other)?,
    )?;
    std::fs::rename(&tmp, path(dir, &parked.alias, &parked.issue_iid)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

pub fn save(dir: &Path, parked: &Parked) -> io::Result<()> {
    let _lock = lock(dir)?;
    write(dir, parked)
}

pub fn load(dir: &Path, alias: &str, iid: &str) -> Option<Parked> {
    read(&path(dir, alias, iid))
}

/// Removes the record and returns it, unless another process took it first.
pub fn take(dir: &Path, alias: &str, iid: &str) -> Option<Parked> {
    let _lock = lock(dir).ok()?;
    let p = path(dir, alias, iid);
    let parked = read(&p)?;
    std::fs::remove_file(&p).ok()?;
    Some(parked)
}

pub fn all(dir: &Path) -> Vec<Parked> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Parked> = entries
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
    out.sort_by(|a, b| a.parked_at.cmp(&b.parked_at));
    out
}

/// How the approval ended (`approved`, `changes_requested`, `denied`,
/// `timeout`), or `None` while it is still pending. A vanished approval
/// counts as `denied`.
pub fn decision(approvals_dir: &Path, parked: &Parked, now: DateTime<Utc>) -> Option<String> {
    approvals::expire_overdue(approvals_dir, now);
    match approvals::get(approvals_dir, &parked.approval_id) {
        None => Some("denied".into()),
        Some(a) if a["status"] == "pending" => None,
        Some(a) => Some(a["status"].as_str().unwrap_or("denied").to_string()),
    }
}

/// Tells the chat apps how the approval ended; afterwards `parked.relay` is empty.
pub fn close_relay(
    parked: &mut Parked,
    status: &str,
    approvals_dir: &Path,
    source: &dyn ConnectorSource,
    log: &dyn Fn(&str),
) {
    if parked.relay.is_empty() {
        return;
    }
    let by = approvals::get(approvals_dir, &parked.approval_id)
        .and_then(|a| a["decided_by"].as_str().map(str::to_owned));
    Relay::restore(&parked.relay, source, log).close(status, by.as_deref(), log);
    parked.relay.clear();
}

/// The scheduler tick's share: read chat answers for the pending approvals,
/// close the chat requests of decided ones, and queue `issue-loop <alias> <iid>`
/// for each decided one. Returns what it queued, as `issue-loop <alias> #<iid>`.
pub fn tick(
    dir: &Path,
    approvals_dir: &Path,
    source: &dyn ConnectorSource,
    queue: &RunQueue,
    now: DateTime<Utc>,
    log: &dyn Fn(&str),
) -> Vec<String> {
    let Ok(_lock) = lock(dir) else {
        return Vec::new();
    };
    let mut queued = Vec::new();
    for mut parked in all(dir) {
        let before = parked.clone();
        let mut status = decision(approvals_dir, &parked, now);
        if status.is_none() && !parked.relay.is_empty() {
            let mut relay = Relay::restore(&parked.relay, source, log);
            relay.poll(approvals_dir, &parked.approval_id, log);
            parked.relay = relay.saved();
            status = decision(approvals_dir, &parked, now);
        }
        if let Some(status) = &status {
            close_relay(&mut parked, status, approvals_dir, source, log);
            let recently = parked
                .queued_at
                .as_deref()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .is_some_and(|t| {
                    now - t.with_timezone(&Utc) < Duration::minutes(REQUEUE_AFTER_MINUTES)
                });
            if !recently {
                let args = [parked.alias.clone(), parked.issue_iid.clone()];
                let stamp = crate::loop_scheduler::isoformat(
                    now.with_timezone(&chrono::Local).naive_local(),
                );
                match queue.enqueue_with_args("issue-loop", &args, Reason::Approval, &stamp) {
                    Ok(Enqueued::Queued { .. }) => {
                        parked.queued_at = Some(fmt_timestamp(now));
                        queued.push(format!("issue-loop {} #{}", parked.alias, parked.issue_iid));
                    }
                    Ok(Enqueued::Coalesced { .. }) => {}
                    Err(e) => log(&format!(
                        "could not queue the approved {} #{}: {e}",
                        parked.alias, parked.issue_iid
                    )),
                }
            }
        }
        if parked != before {
            if let Err(e) = write(dir, &parked) {
                log(&format!(
                    "could not save the parked approval of {} #{}: {e}",
                    parked.alias, parked.issue_iid
                ));
            }
        }
    }
    queued
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{Account, ChatDecision, ConnResult, Connector, TypeInfo};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<String>>>;

    /// A Telegram account whose first poll answers `answer`.
    struct Chat {
        account: Account,
        answer: Mutex<Option<ChatDecision>>,
        seen: Log,
    }

    impl Connector for Chat {
        fn info(&self) -> &'static TypeInfo {
            &crate::connectors::telegram::INFO
        }
        fn account(&self) -> &Account {
            &self.account
        }
        fn poll_approval(
            &self,
            _h: &Value,
            offset: i64,
        ) -> ConnResult<(i64, Option<ChatDecision>)> {
            self.seen.lock().unwrap().push(format!("poll {offset}"));
            Ok((offset + 1, self.answer.lock().unwrap().take()))
        }
        fn close_approval(&self, _h: &Value, outcome: &str) -> ConnResult<()> {
            self.seen.lock().unwrap().push(format!("closed: {outcome}"));
            Ok(())
        }
    }

    struct Source(Mutex<Option<ChatDecision>>, Log);

    impl ConnectorSource for Source {
        fn accounts_with_capability(&self, _c: &str) -> Vec<Account> {
            Vec::new()
        }
        fn load(&self, id: &str) -> Result<Box<dyn Connector>, crate::connectors::LoadFailure> {
            Ok(Box::new(Chat {
                account: Account::new(id, "telegram"),
                answer: Mutex::new(self.0.lock().unwrap().take()),
                seen: self.1.clone(),
            }))
        }
    }

    fn quiet() -> Source {
        Source(Mutex::new(None), Log::default())
    }

    struct Fx {
        d: tempfile::TempDir,
    }

    impl Fx {
        fn new() -> Self {
            Self {
                d: tempfile::tempdir().unwrap(),
            }
        }
        fn dir(&self) -> PathBuf {
            self.d.path().join("parked")
        }
        fn approvals(&self) -> PathBuf {
            self.d.path().join("approvals")
        }
        fn queue(&self) -> RunQueue {
            RunQueue::new(self.d.path().join("q.json"))
        }
        /// A pending approval for `web #7` and its parked record.
        fn park(&self, relay: Vec<SavedChannel>) -> Parked {
            let new = approvals::NewApproval {
                kind: "open_mr".into(),
                run_id: "r1".into(),
                project: "web".into(),
                issue_iid: Some(7),
                title: "fix: #7 x".into(),
                branch: "loop/issue-7".into(),
                target_branch: "main".into(),
                summary: "s".into(),
                timeout_seconds: 3600,
                target: None,
            };
            let a = approvals::create(&self.approvals(), &new, Utc::now()).unwrap();
            let parked = Parked {
                approval_id: a["id"].as_str().unwrap().into(),
                run_id: "r1".into(),
                alias: "web".into(),
                issue_iid: "7".into(),
                handoff: json!({"action": "fix", "branch": "loop/issue-7"}),
                checks: vec!["make test".into()],
                pre_review: None,
                revisions_left: 2,
                setup_fixed: None,
                parked_at: "2026-11-26T10:00:00Z".into(),
                relay,
                queued_at: None,
            };
            save(&self.dir(), &parked).unwrap();
            parked
        }
        fn decide(&self, parked: &Parked, decision: &str) {
            approvals::decide(
                &self.approvals(),
                &parked.approval_id,
                decision,
                "me",
                Utc::now(),
            )
            .unwrap();
        }
        fn tick(&self, source: &Source, now: DateTime<Utc>) -> Vec<String> {
            tick(
                &self.dir(),
                &self.approvals(),
                source,
                &self.queue(),
                now,
                &|_| {},
            )
        }
    }

    fn telegram() -> Vec<SavedChannel> {
        vec![SavedChannel {
            account: "phone".into(),
            handle: json!({"message_id": 3}),
            offset: 4,
        }]
    }

    #[test]
    fn a_record_round_trips_and_is_taken_once() {
        let fx = Fx::new();
        let parked = fx.park(Vec::new());
        assert_eq!(load(&fx.dir(), "web", "7"), Some(parked.clone()));
        assert_eq!(all(&fx.dir()), std::slice::from_ref(&parked));
        assert_eq!(take(&fx.dir(), "web", "7"), Some(parked));
        assert_eq!(take(&fx.dir(), "web", "7"), None);
        assert!(all(&fx.dir()).is_empty());
    }

    #[test]
    fn decision_is_none_while_pending_and_the_status_once_decided_or_expired() {
        let fx = Fx::new();
        let parked = fx.park(Vec::new());
        assert_eq!(decision(&fx.approvals(), &parked, Utc::now()), None);
        let later = Utc::now() + Duration::hours(2);
        assert_eq!(
            decision(&fx.approvals(), &parked, later).as_deref(),
            Some("timeout")
        );
        let gone = Parked {
            approval_id: "apr_missing".into(),
            ..parked
        };
        assert_eq!(
            decision(&fx.approvals(), &gone, Utc::now()).as_deref(),
            Some("denied")
        );
    }

    #[test]
    fn a_pending_approval_is_not_queued() {
        let fx = Fx::new();
        fx.park(Vec::new());
        assert!(fx.tick(&quiet(), Utc::now()).is_empty());
        assert!(fx.queue().snapshot(0).queued.is_empty());
    }

    #[test]
    fn a_decided_approval_queues_its_issue_once_until_the_requeue_window_passes() {
        let fx = Fx::new();
        let parked = fx.park(Vec::new());
        fx.decide(&parked, "approve");
        let now = Utc::now();
        assert_eq!(fx.tick(&quiet(), now), ["issue-loop web #7"]);
        let queued = fx.queue().snapshot(0).queued;
        assert_eq!(queued.len(), 1);
        assert_eq!(
            (
                queued[0].loop_name.as_str(),
                queued[0].args.clone(),
                queued[0].reason.as_str()
            ),
            (
                "issue-loop",
                vec!["web".to_string(), "7".to_string()],
                "approval"
            )
        );

        let entry = fx.queue().claim_next("t", 1).unwrap().unwrap();
        fx.queue()
            .finish(&entry.id, "done", Some(0), None, "t")
            .unwrap();
        assert!(fx.tick(&quiet(), now + Duration::minutes(1)).is_empty());
        assert_eq!(
            fx.tick(&quiet(), now + Duration::minutes(11)),
            ["issue-loop web #7"]
        );
    }

    #[test]
    fn a_chat_answer_is_recorded_its_offset_kept_and_the_request_closed() {
        let fx = Fx::new();
        fx.park(telegram());
        let seen = Log::default();
        assert!(fx
            .tick(&Source(Mutex::new(None), seen.clone()), Utc::now())
            .is_empty());
        assert_eq!(load(&fx.dir(), "web", "7").unwrap().relay[0].offset, 5);

        let allow = ChatDecision {
            decision: "approve".into(),
            notes: None,
            by: "telegram:@ana".into(),
        };
        let queued = fx.tick(&Source(Mutex::new(Some(allow)), seen.clone()), Utc::now());
        assert_eq!(queued, ["issue-loop web #7"]);
        let parked = load(&fx.dir(), "web", "7").unwrap();
        assert!(parked.relay.is_empty(), "closed once decided");
        assert_eq!(
            approvals::get(&fx.approvals(), &parked.approval_id).unwrap()["decided_by"],
            "telegram:@ana"
        );
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "poll 4",
                "poll 5",
                "closed: Approved by telegram:@ana: opening the merge request."
            ]
        );
    }

    #[test]
    fn the_tick_never_brings_back_a_record_a_run_took() {
        let fx = Fx::new();
        let parked = fx.park(Vec::new());
        fx.decide(&parked, "deny");
        take(&fx.dir(), "web", "7").unwrap();
        assert!(fx.tick(&quiet(), Utc::now()).is_empty());
        assert_eq!(load(&fx.dir(), "web", "7"), None);
    }
}
