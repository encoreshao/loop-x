//! Approval requests answered from a chat app. While the GitLab loop waits on
//! an approval, each connector account with "Approvals here" switched on
//! (Telegram today) gets the request with Allow / Deny buttons; a press, or a
//! reply asking for changes, is recorded with `approvals::decide_with_notes`
//! like a decision made in the Mac app or the dashboard. Whichever answers
//! first wins; the others are told how it ended.
//!
//! A chat channel that fails is logged once and dropped: the approval can
//! still be decided everywhere else, so a broken bot never blocks the loop.

use crate::approvals;
use crate::connectors::{Connector, ConnectorSource, NOTIFY};
use chrono::Utc;
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};

/// How often the chat apps are asked for answers.
pub const POLL_EVERY: Duration = Duration::from_secs(2);

/// The account setting that turns approvals on for a chat connector.
pub const SETTING: &str = "approvals";

struct Channel {
    conn: Box<dyn Connector>,
    handle: Value,
    offset: i64,
}

pub struct Relay {
    channels: Vec<Channel>,
    last_poll: Option<Instant>,
    every: Duration,
}

/// The enabled chat accounts that take approvals, loaded from `source`.
pub fn channels(source: &dyn ConnectorSource) -> Vec<Box<dyn Connector>> {
    source
        .accounts_with_capability(NOTIFY)
        .into_iter()
        .filter(|a| a.enabled && a.kind == "telegram" && a.setting(SETTING) == "on")
        .filter_map(|a| source.load(&a.id).ok())
        .collect()
}

fn label(conn: &dyn Connector) -> String {
    format!("{} ({})", conn.account().label, conn.account().kind)
}

impl Relay {
    /// Post `approval` to every channel; one that can't be reached is logged and skipped.
    pub fn start(conns: Vec<Box<dyn Connector>>, approval: &Value, log: &dyn Fn(&str)) -> Self {
        let channels = conns
            .into_iter()
            .filter_map(|conn| match conn.request_approval(approval) {
                Ok(handle) => Some(Channel {
                    conn,
                    handle,
                    offset: 0,
                }),
                Err(e) => {
                    log(&format!(
                        "could not send the approval request to {}: {e}",
                        label(conn.as_ref())
                    ));
                    None
                }
            })
            .collect();
        Self {
            channels,
            last_poll: None,
            every: POLL_EVERY,
        }
    }

    #[cfg(test)]
    fn polling_every(mut self, every: Duration) -> Self {
        self.every = every;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    /// Record the first chat answer to approval `id` in `dir`; at most once per [`POLL_EVERY`].
    pub fn poll(&mut self, dir: &Path, id: &str, log: &dyn Fn(&str)) {
        if self.last_poll.is_some_and(|t| t.elapsed() < self.every) {
            return;
        }
        self.last_poll = Some(Instant::now());
        let mut broken = Vec::new();
        for (i, ch) in self.channels.iter_mut().enumerate() {
            match ch.conn.poll_approval(&ch.handle, ch.offset) {
                Ok((offset, decision)) => {
                    ch.offset = offset;
                    if let Some(d) = decision {
                        let recorded = approvals::decide_with_notes(
                            dir,
                            id,
                            &d.decision,
                            d.notes.as_deref(),
                            &d.by,
                            Utc::now(),
                        );
                        if let Err(e) = recorded {
                            log(&format!("a chat answer to {id} was not recorded: {e}"));
                        }
                        return;
                    }
                }
                Err(e) => {
                    log(&format!(
                        "stopped reading approval answers from {}: {e}",
                        label(ch.conn.as_ref())
                    ));
                    broken.push(i);
                }
            }
        }
        for i in broken.into_iter().rev() {
            self.channels.remove(i);
        }
    }

    /// Tell every channel how the approval ended (`approved`, `denied`,
    /// `changes_requested` or `timeout`).
    pub fn close(&self, status: &str, by: Option<&str>, log: &dyn Fn(&str)) {
        let by = by.map(|b| format!(" by {b}")).unwrap_or_default();
        let outcome = match status {
            "approved" => format!("Approved{by}: opening the merge request."),
            "changes_requested" => format!("Changes requested{by}: sent back to the agent."),
            "timeout" => "Expired with no decision; the issue was escalated.".to_string(),
            _ => format!("Denied{by}; the issue was escalated."),
        };
        for ch in &self.channels {
            if let Err(e) = ch.conn.close_approval(&ch.handle, &outcome) {
                log(&format!(
                    "could not update the approval message in {}: {e}",
                    label(ch.conn.as_ref())
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{Account, ChatDecision, ConnResult, ConnectorError, TypeInfo};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<String>>>;
    type Reply = ConnResult<(i64, Option<ChatDecision>)>;

    /// A chat app that answers each poll with the next scripted reply.
    struct FakeChat {
        account: Account,
        replies: Mutex<Vec<Reply>>,
        fail_send: bool,
        seen: Log,
    }

    impl FakeChat {
        fn boxed(replies: Vec<Reply>, seen: &Log) -> Box<dyn Connector> {
            Box::new(Self {
                account: Account::new("tg", "telegram"),
                replies: Mutex::new(replies),
                fail_send: false,
                seen: seen.clone(),
            })
        }
    }

    impl Connector for FakeChat {
        fn info(&self) -> &'static TypeInfo {
            &crate::connectors::telegram::INFO
        }
        fn account(&self) -> &Account {
            &self.account
        }
        fn request_approval(&self, approval: &Value) -> ConnResult<Value> {
            if self.fail_send {
                return Err(ConnectorError::Unsupported("down".into()));
            }
            self.seen
                .lock()
                .unwrap()
                .push(format!("sent {}", approval["id"]));
            Ok(json!({"message_id": 1}))
        }
        fn poll_approval(
            &self,
            _h: &Value,
            offset: i64,
        ) -> ConnResult<(i64, Option<ChatDecision>)> {
            self.seen.lock().unwrap().push(format!("poll {offset}"));
            let mut r = self.replies.lock().unwrap();
            if r.is_empty() {
                Ok((offset, None))
            } else {
                r.remove(0)
            }
        }
        fn close_approval(&self, _h: &Value, outcome: &str) -> ConnResult<()> {
            self.seen.lock().unwrap().push(format!("closed: {outcome}"));
            Ok(())
        }
    }

    fn pending(dir: &Path) -> String {
        let new = approvals::NewApproval {
            kind: "open_mr".into(),
            run_id: "r".into(),
            project: "web".into(),
            issue_iid: Some(7),
            title: "Fix".into(),
            branch: "loop/issue-7".into(),
            target_branch: "main".into(),
            summary: "s".into(),
            timeout_seconds: 600,
        };
        approvals::create(dir, &new, Utc::now()).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn deny() -> ChatDecision {
        ChatDecision {
            decision: "deny".into(),
            notes: None,
            by: "telegram:@ana".into(),
        }
    }

    #[test]
    fn a_chat_answer_decides_the_approval_and_everyone_is_told() {
        let d = tempfile::tempdir().unwrap();
        let id = pending(d.path());
        let seen: Log = Log::default();
        let conns = vec![FakeChat::boxed(
            vec![Ok((5, None)), Ok((9, Some(deny())))],
            &seen,
        )];
        let mut relay =
            Relay::start(conns, &json!({"id": id}), &|_| {}).polling_every(Duration::ZERO);
        relay.poll(d.path(), &id, &|_| {});
        assert_eq!(approvals::get(d.path(), &id).unwrap()["status"], "pending");
        relay.poll(d.path(), &id, &|_| {});
        let a = approvals::get(d.path(), &id).unwrap();
        assert_eq!(
            (a["status"].as_str(), a["decided_by"].as_str()),
            (Some("denied"), Some("telegram:@ana"))
        );
        relay.close("denied", Some("telegram:@ana"), &|_| {});
        assert_eq!(
            *seen.lock().unwrap(),
            [
                format!("sent \"{id}\""),
                "poll 0".to_string(),
                "poll 5".to_string(),
                "closed: Denied by telegram:@ana; the issue was escalated.".to_string(),
            ]
        );
    }

    #[test]
    fn a_failing_channel_is_logged_once_and_dropped() {
        let d = tempfile::tempdir().unwrap();
        let id = pending(d.path());
        let (seen, logs): (Log, Log) = Default::default();
        let log = |m: &str| logs.lock().unwrap().push(m.to_string());
        let conns = vec![FakeChat::boxed(
            vec![Err(ConnectorError::Unsupported("409 webhook set".into()))],
            &seen,
        )];
        let mut relay = Relay::start(conns, &json!({"id": id}), &log).polling_every(Duration::ZERO);
        relay.poll(d.path(), &id, &log);
        relay.poll(d.path(), &id, &log);
        assert!(relay.is_empty());
        let logs = logs.lock().unwrap();
        assert_eq!(logs.len(), 1);
        assert!(
            logs[0].starts_with("stopped reading approval answers from"),
            "{logs:?}"
        );
    }

    #[test]
    fn a_channel_that_cannot_send_is_skipped() {
        let seen: Log = Log::default();
        let mut chat = FakeChat {
            account: Account::new("tg", "telegram"),
            replies: Mutex::new(vec![]),
            fail_send: true,
            seen: seen.clone(),
        };
        chat.account.label = "Phone".into();
        let logs: Log = Log::default();
        let relay = Relay::start(vec![Box::new(chat)], &json!({"id": "apr_1"}), &|m| {
            logs.lock().unwrap().push(m.to_string())
        });
        assert!(relay.is_empty());
        assert!(logs.lock().unwrap()[0]
            .starts_with("could not send the approval request to Phone (telegram)"));
    }

    struct Source(Vec<Account>);

    impl ConnectorSource for Source {
        fn accounts_with_capability(&self, _c: &str) -> Vec<Account> {
            self.0.clone()
        }
        fn load(&self, id: &str) -> Result<Box<dyn Connector>, crate::connectors::LoadFailure> {
            let mut account = Account::new(id, "telegram");
            account.label = id.into();
            Ok(Box::new(FakeChat {
                account,
                replies: Mutex::new(vec![]),
                fail_send: false,
                seen: Log::default(),
            }))
        }
    }

    #[test]
    fn only_enabled_telegram_accounts_with_approvals_on_are_used() {
        let on = |id: &str, kind: &str| Account::new(id, kind).with_setting(SETTING, "on");
        let mut disabled = on("off-account", "telegram");
        disabled.enabled = false;
        let source = Source(vec![
            on("phone", "telegram"),
            Account::new("quiet", "telegram"),
            disabled,
            on("team", "slack"),
        ]);
        let ids: Vec<String> = channels(&source)
            .iter()
            .map(|c| c.account().id.clone())
            .collect();
        assert_eq!(ids, ["phone"]);
    }

    #[test]
    fn polls_are_spaced_out() {
        let d = tempfile::tempdir().unwrap();
        let seen: Log = Log::default();
        let mut relay = Relay::start(
            vec![FakeChat::boxed(vec![], &seen)],
            &json!({"id": "x"}),
            &|_| {},
        );
        relay.poll(d.path(), "x", &|_| {});
        relay.poll(d.path(), "x", &|_| {});
        assert_eq!(
            seen.lock()
                .unwrap()
                .iter()
                .filter(|s| s.starts_with("poll"))
                .count(),
            1
        );
    }
}
