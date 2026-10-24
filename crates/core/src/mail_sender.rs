//! Sends one plain-text Gmail message (`bin/mail_sender.py`). Kept out of
//! `mail_provider` on purpose: Inbox Triage's providers can never deliver mail
//! (their tests enforce it), so the only path that can is this module, reached
//! by the chat's approve-gated `mail.send` write tool. Only the connected Gmail
//! mailbox is used (`gmail.modify` covers `messages.send`), and every recipient
//! must be a plain address.

use crate::mail_http::{self, MailError, Request};
use crate::mail_mime::{build_plain, urlsafe_b64};
use crate::mail_provider::base::is_plain_address;
use serde_json::{json, Value};

pub const GMAIL_BASE_URL: &str = "https://gmail.googleapis.com/gmail/v1";
pub const MAX_RECIPIENTS: usize = 10;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SendError {
    /// A hand-written, secret-free input problem.
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Mail(MailError),
}

fn invalid<T>(m: impl Into<String>) -> Result<T, SendError> {
    Err(SendError::Invalid(m.into()))
}

fn addresses(values: &[String], label: &str) -> Result<Vec<String>, SendError> {
    values
        .iter()
        .map(|v| {
            let v = v.trim();
            if is_plain_address(v) {
                Ok(v.to_string())
            } else {
                invalid(format!("{label} must be plain email addresses"))
            }
        })
        .collect()
}

/// The base64url-encoded RFC 5322 message Gmail's `messages.send` takes.
pub fn build_raw(
    sender: &str,
    to: &[String],
    cc: &[String],
    subject: &str,
    body: &str,
) -> Result<String, SendError> {
    let (to, cc) = (addresses(to, "to")?, addresses(cc, "cc")?);
    if to.is_empty() {
        return invalid("at least one recipient is required");
    }
    if to.len() + cc.len() > MAX_RECIPIENTS {
        return invalid(format!("at most {MAX_RECIPIENTS} recipients"));
    }
    if subject.contains('\r') || subject.contains('\n') {
        return invalid("subject must be a single line");
    }
    let (to_h, cc_h) = (to.join(", "), cc.join(", "));
    let mut headers: Vec<(&str, &str)> = vec![("From", sender), ("To", &to_h)];
    if !cc.is_empty() {
        headers.push(("Cc", &cc_h));
    }
    headers.push(("Subject", subject));
    let mime = build_plain(&headers, body).map_err(SendError::Invalid)?;
    Ok(urlsafe_b64(&mime))
}

/// Send through Gmail; `base_url` overrides the API root. One attempt only: a
/// retried POST could deliver the message twice.
pub fn send_gmail(
    access_token: &str,
    sender: &str,
    to: &[String],
    cc: &[String],
    subject: &str,
    body: &str,
    base_url: Option<&str>,
) -> Result<Value, SendError> {
    let raw = build_raw(sender, to, cc, subject, body)?;
    let url = format!(
        "{}/users/me/messages/send",
        base_url.unwrap_or(GMAIL_BASE_URL).trim_end_matches('/')
    );
    let made = mail_http::request_json(
        &Request::new("POST", &url)
            .token(access_token)
            .json(json!({"raw": raw}))
            .attempts(1),
    )
    .map_err(SendError::Mail)?;
    let get = |k: &str| {
        made.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Ok(json!({"id": get("id"), "thread_id": get("threadId")}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail_http::testserver::Stub;
    use crate::mail_mime::urlsafe_b64_decode;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sends_a_plain_text_message_through_gmail() {
        let stub = Stub::new();
        stub.add(
            "POST",
            "/gmail/v1/users/me/messages/send",
            200,
            Some(json!({"id": "g1", "threadId": "t1"})),
        );
        let out = send_gmail(
            "tok",
            "me@x.com",
            &v(&["a@x.com", "b@x.com"]),
            &v(&["c@x.com"]),
            "H\u{e9}llo",
            "Body \u{2713}",
            Some(&format!("{}/gmail/v1", stub.base_url)),
        )
        .unwrap();
        assert_eq!(out, json!({"id": "g1", "thread_id": "t1"}));
        let sent: Value =
            serde_json::from_str(&stub.bodies("POST", "/gmail/v1/users/me/messages/send")[0])
                .unwrap();
        let mime =
            String::from_utf8(urlsafe_b64_decode(sent["raw"].as_str().unwrap()).unwrap()).unwrap();
        assert!(
            mime.contains("From: me@x.com\nTo: a@x.com, b@x.com\nCc: c@x.com\n"),
            "{mime}"
        );
        assert!(mime.contains("Subject: =?utf-8?b?SMOpbGxv?=\n"));
        assert!(mime.ends_with("\n\nBody \u{2713}\n"));
        assert_eq!(stub.requests()[0].headers["authorization"], "Bearer tok");
    }

    #[test]
    fn bad_recipients_and_header_injection_are_rejected_before_any_request() {
        let long = ["a@x.com"; 11];
        let cases: Vec<(Vec<String>, &str)> = vec![
            (v(&["not-an-address"]), "s"),
            (v(&["a@x.com\nBcc: evil@x.com"]), "s"),
            (vec![], "s"),
            (v(&["a@x.com"]), "line1\r\nBcc: evil@x.com"),
            (long.iter().map(|s| s.to_string()).collect(), "s"),
        ];
        for (to, subject) in cases {
            let e = send_gmail(
                "tok",
                "me@x.com",
                &to,
                &[],
                subject,
                "b",
                Some("http://127.0.0.1:1"),
            )
            .unwrap_err();
            assert!(matches!(e, SendError::Invalid(_)), "{to:?} {subject:?}");
        }
    }

    #[test]
    fn a_failing_send_is_not_retried() {
        let stub = Stub::new();
        stub.add("POST", "/users/me/messages/send", 500, Some(json!({})));
        let e = send_gmail(
            "tok",
            "me@x.com",
            &v(&["a@x.com"]),
            &[],
            "s",
            "b",
            Some(&stub.base_url),
        )
        .unwrap_err();
        assert!(matches!(e, SendError::Mail(m) if m.is_http()));
        assert_eq!(stub.requests().len(), 1);
    }

    #[test]
    fn the_total_recipient_cap_counts_cc() {
        let to: Vec<String> = (0..6).map(|i| format!("a{i}@x.com")).collect();
        let cc: Vec<String> = (0..5).map(|i| format!("c{i}@x.com")).collect();
        assert!(build_raw("me@x.com", &to, &cc, "s", "b").is_err());
        assert!(build_raw("me@x.com", &to[..5], &cc, "s", "b").is_ok());
    }
}
