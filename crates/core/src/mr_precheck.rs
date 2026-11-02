//! Model-free pre-check: which MR discussions need the agent's attention.
//! Pure function over GitLab discussion JSON and the persisted thread state.

use crate::mr_threads::{IssueThreads, ThreadStatus, LOOP_SIGNATURE};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Actionable {
    pub discussion_id: String,
    /// Every non-system note of the discussion, for context.
    pub notes: Vec<Value>,
    /// The inline diff position of the thread, when it has one.
    pub position: Option<Value>,
}

fn note_id(n: &Value) -> i64 {
    n.get("id").and_then(Value::as_i64).unwrap_or(0)
}

fn flag(n: &Value, key: &str) -> bool {
    n.get(key).and_then(Value::as_bool).unwrap_or(false)
}

pub(crate) fn is_human(n: &Value, state: &IssueThreads) -> bool {
    if flag(n, "system") || state.ledger.contains(&note_id(n)) {
        return false;
    }
    if n.get("author")
        .and_then(|a| a.get("bot"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return false;
    }
    !n.get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .contains(LOOP_SIGNATURE)
}

pub fn actionable_threads(discussions: &[Value], state: &IssueThreads) -> Vec<Actionable> {
    let mut out = Vec::new();
    for d in discussions {
        let Some(id) = d.get("id").and_then(Value::as_str) else {
            continue;
        };
        let notes: Vec<Value> = d
            .get("notes")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter(|n| !flag(n, "system")).cloned().collect())
            .unwrap_or_default();
        let resolvable: Vec<&Value> = notes.iter().filter(|n| flag(n, "resolvable")).collect();
        if !resolvable.is_empty() && resolvable.iter().all(|n| flag(n, "resolved")) {
            continue;
        }
        let entry = state.threads.get(id);
        if matches!(entry.map(|e| e.status), Some(ThreadStatus::Resolved)) {
            continue;
        }
        let last = entry.map_or(0, |e| e.last_note_id);
        // The user gave instructions from Needs you: plan again with them.
        let instructed = entry.is_some_and(|e| {
            e.user_instructions.is_some()
                && matches!(e.status, ThreadStatus::New | ThreadStatus::Working)
        });
        if !instructed
            && !notes
                .iter()
                .any(|n| is_human(n, state) && note_id(n) > last)
        {
            continue;
        }
        if let Some(e) = entry {
            if e.status == ThreadStatus::Working && e.attempts >= 2 {
                continue;
            }
        }
        out.push(Actionable {
            discussion_id: id.to_string(),
            position: notes.first().and_then(|n| n.get("position")).cloned(),
            notes,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mr_threads::ThreadState;
    use serde_json::json;

    fn note(id: i64, body: &str) -> Value {
        json!({"id": id, "body": body, "system": false, "resolvable": true,
               "resolved": false, "author": {"username": "rev"}})
    }

    fn disc(id: &str, notes: Vec<Value>) -> Value {
        json!({"id": id, "individual_note": false, "notes": notes})
    }

    fn st(status: ThreadStatus, last: i64, attempts: u8) -> IssueThreads {
        let mut t = IssueThreads::default();
        t.threads.insert(
            "d1".into(),
            ThreadState {
                discussion_id: "d1".into(),
                last_note_id: last,
                status,
                plan_summary: String::new(),
                confidence: None,
                attempts,
                updated_at: String::new(),
                plan: None,
                reason: None,
                base: None,
                user_instructions: None,
                mr_iid: None,
                last_comment: None,
            },
        );
        t
    }

    #[test]
    fn quiet_mr_yields_nothing() {
        assert!(actionable_threads(&[], &IssueThreads::default()).is_empty());
        let d = disc("d1", vec![note(1, "hi")]);
        let s = st(ThreadStatus::WaitingOnReviewer, 1, 0);
        assert!(actionable_threads(&[d], &s).is_empty());
    }

    #[test]
    fn new_human_note_is_actionable() {
        let mut n = note(1, "please rename");
        n["position"] = json!({"new_path": "a.rs", "new_line": 3});
        let r = actionable_threads(&[disc("d1", vec![n])], &IssueThreads::default());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].discussion_id, "d1");
        assert_eq!(r[0].notes.len(), 1);
        assert_eq!(r[0].position.as_ref().unwrap()["new_path"], "a.rs");
    }

    #[test]
    fn reply_after_loop_answer_reopens_thread() {
        let mut s = st(ThreadStatus::Answered, 2, 0);
        s.ledger.insert(2);
        let d = disc(
            "d1",
            vec![
                note(1, "why?"),
                note(2, "because"),
                note(3, "not convinced"),
            ],
        );
        assert_eq!(actionable_threads(&[d], &s).len(), 1);
    }

    #[test]
    fn loops_own_reply_is_ignored_by_ledger() {
        let mut s = st(ThreadStatus::Answered, 1, 0);
        s.ledger.insert(2);
        let d = disc("d1", vec![note(1, "why?"), note(2, "because")]);
        assert!(actionable_threads(&[d], &s).is_empty());
    }

    #[test]
    fn loops_own_reply_is_ignored_by_signature_when_ledger_lost() {
        let s = st(ThreadStatus::Answered, 1, 0);
        let body = format!("because\n\n{LOOP_SIGNATURE}");
        let d = disc("d1", vec![note(1, "why?"), note(2, &body)]);
        assert!(actionable_threads(&[d], &s).is_empty());
    }

    #[test]
    fn system_and_bot_notes_ignored() {
        let mut sys = note(1, "added a commit");
        sys["system"] = json!(true);
        let mut bot = note(2, "pipeline failed");
        bot["author"]["bot"] = json!(true);
        let d = disc("d1", vec![sys, bot]);
        assert!(actionable_threads(&[d], &IssueThreads::default()).is_empty());
    }

    #[test]
    fn resolved_thread_ignored() {
        let mut n = note(1, "fix");
        n["resolved"] = json!(true);
        let d = disc("d1", vec![n]);
        assert!(actionable_threads(&[d], &IssueThreads::default()).is_empty());
        let s = st(ThreadStatus::Resolved, 0, 0);
        assert!(actionable_threads(&[disc("d1", vec![note(5, "x")])], &s).is_empty());
    }

    #[test]
    fn stuck_working_thread_retried_once() {
        let d = disc("d1", vec![note(1, "fix")]);
        assert_eq!(
            actionable_threads(std::slice::from_ref(&d), &st(ThreadStatus::Working, 0, 1)).len(),
            1
        );
        assert!(actionable_threads(&[d], &st(ThreadStatus::Working, 0, 2)).is_empty());
    }

    #[test]
    fn a_thread_given_instructions_is_planned_again_without_a_new_note() {
        let d = disc("d1", vec![note(1, "fix")]);
        let mut s = st(ThreadStatus::New, 1, 0);
        // Seen, no instructions: nothing to do.
        assert!(actionable_threads(std::slice::from_ref(&d), &s).is_empty());
        s.threads.get_mut("d1").unwrap().user_instructions = Some("only a.rs".into());
        assert_eq!(actionable_threads(std::slice::from_ref(&d), &s).len(), 1);
        // A crash mid-plan leaves it working: retried once, like any thread.
        let t = s.threads.get_mut("d1").unwrap();
        t.status = ThreadStatus::Working;
        t.attempts = 1;
        assert_eq!(actionable_threads(std::slice::from_ref(&d), &s).len(), 1);
        s.threads.get_mut("d1").unwrap().attempts = 2;
        assert!(actionable_threads(std::slice::from_ref(&d), &s).is_empty());
        // Instructions left on a settled thread do not reopen it.
        let t = s.threads.get_mut("d1").unwrap();
        t.status = ThreadStatus::Decided;
        assert!(actionable_threads(&[d], &s).is_empty());
    }

    #[test]
    fn users_own_comment_counts_as_feedback() {
        let mut n = note(1, "also handle empty input");
        n["author"]["username"] = json!("me-the-token-owner");
        let r = actionable_threads(&[disc("d1", vec![n])], &IssueThreads::default());
        assert_eq!(r.len(), 1);
    }
}
