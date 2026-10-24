//! Inbox Triage mail providers (Gmail, Outlook). Mirrors `bin/mail_providers/`.
//! A provider exposes exactly the [`MailProvider`] methods - nothing that could
//! deliver mail (sending lives in [`crate::mail_sender`], reached only by the
//! chat's approve-gated write tool).

pub mod base;
pub mod gmail;
pub mod outlook;

use crate::mail_http::MailError;
use chrono::{DateTime, FixedOffset};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// One earlier message in a thread, as shown to the model.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PriorMessage {
    pub from: String,
    pub date: String,
    pub body_text: String,
}

/// A normalized message. The last four fields are private threading data that
/// never reaches the model or the dashboard.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MailMessage {
    pub id: String,
    pub thread_id: String,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    /// `datetime.isoformat()` in UTC, e.g. `2026-09-27T08:00:00+00:00`.
    pub date: String,
    pub body_text: String,
    pub prior_thread: Vec<PriorMessage>,
    pub attachments: Vec<String>,
    pub message_id_header: String,
    pub references: String,
    pub reply_to: String,
}

impl MailMessage {
    /// The fields the model sees (`_PROMPT_FIELDS` order).
    pub fn prompt_value(&self) -> Value {
        json!({
            "id": self.id, "from": self.from, "to": self.to, "cc": self.cc,
            "subject": self.subject, "date": self.date, "body_text": self.body_text,
            "prior_thread": self.prior_thread.iter()
                .map(|p| json!({"from": p.from, "date": p.date, "body_text": p.body_text}))
                .collect::<Vec<_>>(),
            "attachments": self.attachments,
        })
    }
}

/// A search hit (`search_recent` / `search_messages`): never a body.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MessageRow {
    /// Empty for `search_recent`, which has no id column.
    pub id: Option<String>,
    pub subject: String,
    pub from: String,
    pub date: String,
    pub snippet: String,
}

impl MessageRow {
    pub fn to_value(&self) -> Value {
        let mut v = json!({"subject": self.subject, "from": self.from, "date": self.date, "snippet": self.snippet});
        if let Some(id) = &self.id {
            let mut with_id = json!({"id": id});
            with_id
                .as_object_mut()
                .expect("object")
                .extend(v.as_object().cloned().unwrap_or_default());
            v = with_id;
        }
        v
    }
}

/// What a provider can do. Every call may refresh the access token once.
pub trait MailProvider {
    fn profile_address(&mut self) -> Result<String, MailError>;
    /// New unread inbox mail since `since`, oldest first, at most `limit`,
    /// skipping `seen_ids` and `exclude`d senders.
    fn fetch_new(
        &mut self,
        since: DateTime<FixedOffset>,
        seen_ids: &HashSet<String>,
        exclude: &[String],
        limit: usize,
    ) -> Result<Vec<MailMessage>, MailError>;
    /// Make sure each label exists; returns `label name -> label id`.
    fn ensure_labels(&mut self, labels: &[String]) -> Result<HashMap<String, String>, MailError>;
    fn apply_label(&mut self, message_id: &str, label_id: &str) -> Result<(), MailError>;
    /// Save a reply draft (never sends); returns a link to open it.
    fn create_reply_draft(
        &mut self,
        message: &MailMessage,
        body: &str,
    ) -> Result<String, MailError>;
    fn search_recent(
        &mut self,
        addresses: &[String],
        days: i64,
        limit: usize,
    ) -> Result<Vec<MessageRow>, MailError>;
    fn search_messages(&mut self, query: &str, limit: usize) -> Result<Vec<MessageRow>, MailError>;
    /// One message with its trimmed body; no threading fields, no prior thread.
    fn read_message(&mut self, message_id: &str) -> Result<MailMessage, MailError>;
}

pub type RefreshFn = Box<dyn FnMut() -> Result<String, MailError>>;

/// `get_provider(inbox, access_token, refresh)`; `base_url` overrides the API
/// root (tests and sandboxes point it at a fake server).
pub fn get_provider(
    inbox: &Value,
    access_token: &str,
    refresh: Option<RefreshFn>,
    base_url: Option<&str>,
) -> Result<Box<dyn MailProvider>, String> {
    let account = inbox
        .get("account")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let base = base::Base::new(access_token, refresh, base_url, account);
    match inbox.get("provider").and_then(Value::as_str) {
        Some("gmail") => Ok(Box::new(gmail::GmailProvider::new(base))),
        Some("outlook") => Ok(Box::new(outlook::OutlookProvider::new(base))),
        other => Err(format!(
            "Unknown provider {}",
            crate::loops_config::py_repr(&other.map_or(Value::Null, |s| Value::String(s.into())))
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_provider_is_refused() {
        let e = get_provider(&json!({"provider": "yahoo"}), "t", None, None)
            .err()
            .unwrap();
        assert_eq!(e, "Unknown provider 'yahoo'");
        assert!(get_provider(
            &json!({"provider": "gmail", "account": "a@b.co"}),
            "t",
            None,
            None
        )
        .is_ok());
        assert!(get_provider(&json!({"provider": "outlook"}), "t", None, None).is_ok());
    }

    #[test]
    fn prompt_value_has_exactly_the_public_fields() {
        let m = MailMessage {
            id: "m1".into(),
            message_id_header: "<x@y>".into(),
            ..Default::default()
        };
        let v = m.prompt_value();
        let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            [
                "id",
                "from",
                "to",
                "cc",
                "subject",
                "date",
                "body_text",
                "prior_thread",
                "attachments"
            ]
        );
    }

    #[test]
    fn row_with_an_id_puts_it_first() {
        let r = MessageRow {
            id: Some("1".into()),
            subject: "s".into(),
            ..Default::default()
        };
        assert_eq!(
            r.to_value().as_object().unwrap().keys().next().unwrap(),
            "id"
        );
        assert!(MessageRow::default().to_value().get("id").is_none());
    }
}
