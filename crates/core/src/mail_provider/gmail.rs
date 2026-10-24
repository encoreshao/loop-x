//! Gmail REST v1 provider (`bin/mail_providers/gmail.py`). Mailbox writes are
//! limited to: creating labels, adding a label to a message, and creating a
//! draft. Nothing here delivers mail, archives, deletes, or changes read state.

use super::base::{plain_addresses, Base};
use super::{MailMessage, MailProvider, MessageRow, PriorMessage};
use crate::inbox_config::{parse_address, sender_matches};
use crate::inbox_triage::{html_to_text, trim, BODY_LIMIT, PRIOR_COUNT, PRIOR_LIMIT};
use crate::mail_http::{quote, urlencode, MailError};
use crate::mail_mime::{build_plain, urlsafe_b64, urlsafe_b64_decode};
use crate::mail_time::iso_from_millis;
use chrono::{DateTime, FixedOffset};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

pub const DEFAULT_BASE_URL: &str = "https://gmail.googleapis.com/gmail/v1";
/// Safety cap: 20 pages of `maxResults=100`.
const LIST_PAGE_LIMIT: usize = 20;

fn header(headers: Option<&Value>, name: &str) -> String {
    headers
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|h| {
            h.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
        .and_then(|h| h.get("value").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

/// Decode bytes the way Python's `bytes.decode(charset, "replace")` would for
/// the charsets mail actually uses; anything else falls back to UTF-8.
fn decode_charset(raw: &[u8], charset: &str) -> String {
    match charset.to_ascii_lowercase().replace('_', "-").as_str() {
        "iso-8859-1" | "latin1" | "latin-1" | "l1" => raw.iter().map(|&b| b as char).collect(),
        "windows-1252" | "cp1252" => raw
            .iter()
            .map(|&b| match b {
                0x80..=0x9f => CP1252_HIGH[(b - 0x80) as usize],
                _ => b as char,
            })
            .collect(),
        _ => String::from_utf8_lossy(raw).into_owned(),
    }
}

const CP1252_HIGH: [char; 32] = [
    '\u{20ac}', '\u{81}', '\u{201a}', '\u{192}', '\u{201e}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{2c6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8d}', '\u{17d}', '\u{8f}',
    '\u{90}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{2dc}', '\u{2122}', '\u{161}', '\u{203a}', '\u{153}', '\u{9d}', '\u{17e}', '\u{178}',
];

fn decode(data: &str, charset: &str) -> String {
    decode_charset(&urlsafe_b64_decode(data).unwrap_or_default(), charset)
}

/// `charset="?([\w.-]+)` from a part's Content-Type, default utf-8.
fn charset(part: &Value) -> String {
    let ct = header(part.get("headers"), "Content-Type");
    let lower = ct.to_ascii_lowercase();
    let Some(p) = lower.find("charset=") else {
        return "utf-8".into();
    };
    let rest = ct[p + 8..].trim_start_matches('"');
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-'))
        .collect();
    if name.is_empty() {
        "utf-8".into()
    } else {
        name
    }
}

fn walk<'a>(part: &'a Value, out: &mut Vec<&'a Value>) {
    out.push(part);
    for child in part
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        walk(child, out);
    }
}

fn truthy_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn body_text(payload: &Value) -> String {
    let (mut plain, mut markup): (Option<String>, Option<String>) = (None, None);
    let mut parts = Vec::new();
    walk(payload, &mut parts);
    for part in parts {
        if truthy_str(part.get("filename")).is_some() {
            continue;
        }
        let Some(data) = truthy_str(part.get("body").and_then(|b| b.get("data"))) else {
            continue;
        };
        match part.get("mimeType").and_then(Value::as_str) {
            Some("text/plain") if plain.is_none() => plain = Some(decode(data, &charset(part))),
            Some("text/html") if markup.is_none() => markup = Some(decode(data, &charset(part))),
            _ => {}
        }
    }
    match plain {
        Some(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => html_to_text(markup.as_deref().unwrap_or("")),
    }
}

fn internal_millis(raw: &Value) -> i64 {
    match raw.get("internalDate") {
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        _ => 0,
    }
}

pub fn parse_gmail_message(raw: &Value) -> Result<MailMessage, MailError> {
    let id = raw
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| MailError::Other("KeyError: 'id'".into()))?;
    let empty = json!({});
    let payload = raw
        .get("payload")
        .filter(|p| !p.is_null())
        .unwrap_or(&empty);
    let headers = payload.get("headers");
    let mut parts = Vec::new();
    walk(payload, &mut parts);
    Ok(MailMessage {
        id: id.to_string(),
        thread_id: raw
            .get("threadId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        from: header(headers, "From"),
        to: header(headers, "To"),
        cc: header(headers, "Cc"),
        subject: header(headers, "Subject"),
        date: iso_from_millis(internal_millis(raw)),
        body_text: trim(&body_text(payload), BODY_LIMIT),
        prior_thread: Vec::new(),
        attachments: parts
            .iter()
            .filter_map(|p| truthy_str(p.get("filename")).map(str::to_owned))
            .collect(),
        message_id_header: header(headers, "Message-ID"),
        references: header(headers, "References"),
        reply_to: header(headers, "Reply-To"),
    })
}

pub fn reply_subject(subject: &str) -> String {
    let subject = subject.trim();
    if subject.len() >= 3 && subject.is_char_boundary(3) && subject[..3].eq_ignore_ascii_case("re:")
    {
        subject.to_string()
    } else {
        format!("Re: {subject}").trim().to_string()
    }
}

pub struct GmailProvider {
    base: Base,
}

impl GmailProvider {
    pub fn new(base: Base) -> Self {
        Self {
            base: base.with_default_url(DEFAULT_BASE_URL),
        }
    }

    fn call(&mut self, method: &str, path: &str, body: Option<Value>) -> Result<Value, MailError> {
        self.base.call(method, path, body, &[])
    }

    fn get(&mut self, path: &str) -> Result<Value, MailError> {
        self.call("GET", path, None)
    }

    fn list(&mut self, params: &[(&str, String)]) -> Result<Value, MailError> {
        self.get(&format!("/users/me/messages?{}", urlencode(params)))
    }

    fn metadata_row(&mut self, id: &str, with_id: bool) -> Result<MessageRow, MailError> {
        let query = urlencode(&[
            ("format", "metadata".into()),
            ("metadataHeaders", "From".into()),
            ("metadataHeaders", "Subject".into()),
        ]);
        let raw = self.get(&format!("/users/me/messages/{id}?{query}"))?;
        let headers = raw.get("payload").and_then(|p| p.get("headers"));
        Ok(MessageRow {
            id: with_id.then(|| id.to_string()),
            subject: header(headers, "Subject"),
            from: header(headers, "From"),
            date: iso_from_millis(internal_millis(&raw)),
            snippet: raw
                .get("snippet")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    fn prior_thread(&mut self, message: &MailMessage) -> Result<Vec<PriorMessage>, MailError> {
        let thread = self.get(&format!(
            "/users/me/threads/{}?format=full",
            message.thread_id
        ))?;
        // Never feed the loop's own earlier drafts in this thread back to the AI.
        let mut earlier = Vec::new();
        for m in thread
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let is_draft = m
                .get("labelIds")
                .and_then(Value::as_array)
                .is_some_and(|l| l.iter().any(|x| x == "DRAFT"));
            if is_draft {
                continue;
            }
            let parsed = parse_gmail_message(m)?;
            if parsed.date < message.date && parsed.id != message.id {
                earlier.push(parsed);
            }
        }
        earlier.sort_by(|a, b| a.date.cmp(&b.date));
        let skip = earlier.len().saturating_sub(PRIOR_COUNT);
        Ok(earlier
            .into_iter()
            .skip(skip)
            .map(|m| PriorMessage {
                from: m.from,
                date: m.date,
                body_text: trim(&m.body_text, PRIOR_LIMIT),
            })
            .collect())
    }
}

impl MailProvider for GmailProvider {
    fn profile_address(&mut self) -> Result<String, MailError> {
        let p = self.get("/users/me/profile")?;
        Ok(p.get("emailAddress")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_lowercase())
    }

    fn fetch_new(
        &mut self,
        since: DateTime<FixedOffset>,
        seen_ids: &HashSet<String>,
        exclude: &[String],
        limit: usize,
    ) -> Result<Vec<MailMessage>, MailError> {
        // One second early: `after:` is exclusive at one-second granularity, so a
        // message in the same second as the high-water mark would be skipped.
        // The seen-id filter below drops anything already triaged.
        let query = format!("in:inbox is:unread after:{}", since.timestamp() - 1);
        let mut ids: Vec<String> = Vec::new();
        let mut page_token: Option<String> = None;
        for _ in 0..LIST_PAGE_LIMIT {
            let mut params = vec![("q", query.clone()), ("maxResults", "100".to_string())];
            if let Some(t) = &page_token {
                params.push(("pageToken", t.clone()));
            }
            let page = self.list(&params)?;
            for m in page
                .get("messages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let id = m
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| MailError::Other("KeyError: 'id'".into()))?;
                if !seen_ids.contains(id) {
                    ids.push(id.to_string());
                }
            }
            page_token = truthy_str(page.get("nextPageToken")).map(str::to_owned);
            if page_token.is_none() {
                break;
            }
        }
        // messages.list is newest-first; walk oldest-first so a backlog larger
        // than one page still yields the true oldest messages.
        ids.reverse();
        let mut messages: Vec<MailMessage> = Vec::new();
        for id in ids {
            if messages.len() >= limit {
                break;
            }
            let parsed =
                parse_gmail_message(&self.get(&format!("/users/me/messages/{id}?format=full"))?)?;
            if sender_matches(&parse_address(&parsed.from), exclude) {
                continue;
            }
            messages.push(parsed);
        }
        messages.sort_by(|a, b| a.date.cmp(&b.date));
        for message in messages.iter_mut() {
            message.prior_thread = self.prior_thread(message)?;
        }
        Ok(messages)
    }

    fn search_recent(
        &mut self,
        addresses: &[String],
        days: i64,
        limit: usize,
    ) -> Result<Vec<MessageRow>, MailError> {
        let addresses = plain_addresses(addresses);
        if addresses.is_empty() {
            return Ok(Vec::new());
        }
        let terms = addresses
            .iter()
            .map(|a| format!("from:{a} to:{a}"))
            .collect::<Vec<_>>()
            .join(" ");
        let page = self.list(&[
            ("q", format!("{{{terms}}} newer_than:{days}d")),
            ("maxResults", limit.to_string()),
        ])?;
        let mut rows = Vec::new();
        for r in page
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(limit)
        {
            let id = r.get("id").and_then(Value::as_str).unwrap_or_default();
            rows.push(self.metadata_row(id, false)?);
        }
        rows.sort_by(|a, b| b.date.cmp(&a.date));
        Ok(rows)
    }

    fn search_messages(&mut self, query: &str, limit: usize) -> Result<Vec<MessageRow>, MailError> {
        let page = self.list(&[("q", query.to_string()), ("maxResults", limit.to_string())])?;
        let mut rows = Vec::new();
        for r in page
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(limit)
        {
            let id = r.get("id").and_then(Value::as_str).unwrap_or_default();
            rows.push(self.metadata_row(id, true)?);
        }
        Ok(rows)
    }

    fn read_message(&mut self, message_id: &str) -> Result<MailMessage, MailError> {
        let raw = self.get(&format!(
            "/users/me/messages/{}?format=full",
            quote(message_id)
        ))?;
        let mut m = parse_gmail_message(&raw)?;
        m.message_id_header.clear();
        m.references.clear();
        m.reply_to.clear();
        m.prior_thread.clear();
        Ok(m)
    }

    fn ensure_labels(&mut self, labels: &[String]) -> Result<HashMap<String, String>, MailError> {
        let listing = self.get("/users/me/labels")?;
        let mut existing: HashMap<String, String> = HashMap::new();
        for l in listing
            .get("labels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let (Some(n), Some(i)) = (
                l.get("name").and_then(Value::as_str),
                l.get("id").and_then(Value::as_str),
            ) {
                existing.insert(n.to_string(), i.to_string());
            }
        }
        let mut mapping = HashMap::new();
        for name in labels {
            if !existing.contains_key(name) {
                let created = self.call(
                    "POST",
                    "/users/me/labels",
                    Some(json!({"name": name, "labelListVisibility": "labelShow", "messageListVisibility": "show"})),
                )?;
                let id = created
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| MailError::Other("KeyError: 'id'".into()))?;
                existing.insert(name.clone(), id.to_string());
            }
            mapping.insert(name.clone(), existing[name].clone());
        }
        Ok(mapping)
    }

    fn apply_label(&mut self, message_id: &str, label_id: &str) -> Result<(), MailError> {
        self.call(
            "POST",
            &format!("/users/me/messages/{message_id}/modify"),
            Some(json!({"addLabelIds": [label_id]})),
        )
        .map(|_| ())
    }

    fn create_reply_draft(
        &mut self,
        message: &MailMessage,
        body: &str,
    ) -> Result<String, MailError> {
        let to = if message.reply_to.is_empty() {
            &message.from
        } else {
            &message.reply_to
        };
        let subject = reply_subject(&message.subject);
        let refs = [
            message.references.as_str(),
            message.message_id_header.as_str(),
        ]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
        let mut headers: Vec<(&str, &str)> = vec![("To", to), ("Subject", &subject)];
        if !message.message_id_header.is_empty() {
            headers.push(("In-Reply-To", &message.message_id_header));
            headers.push(("References", &refs));
        }
        let mime = build_plain(&headers, body).map_err(MailError::Other)?;
        let draft = self.call(
            "POST",
            "/users/me/drafts",
            Some(json!({"message": {"raw": urlsafe_b64(&mime), "threadId": message.thread_id}})),
        )?;
        let draft_message_id = draft
            .get("message")
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok(format!(
            "https://mail.google.com/mail/?authuser={}#drafts?compose={draft_message_id}",
            crate::mail_http::quote_path(&self.base.account)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail_http::testserver::Stub;
    use crate::mail_time::parse_iso;
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    const U: &str = "/gmail/v1/users/me";

    fn b64(text: &str) -> String {
        urlsafe_b64(text.as_bytes())
            .trim_end_matches('=')
            .to_string()
    }

    struct Raw {
        id: &'static str,
        sender: &'static str,
        subject: &'static str,
        body: String,
        ms: i64,
        thread: &'static str,
        mime: &'static str,
        extra_parts: Vec<Value>,
        headers: Vec<Value>,
    }

    fn raw(id: &'static str) -> Raw {
        Raw {
            id,
            sender: "Alice <alice@x.com>",
            subject: "Hello",
            body: "Hi there".into(),
            ms: 1_790_000_000_000,
            thread: "t1",
            mime: "text/plain",
            extra_parts: vec![],
            headers: vec![],
        }
    }

    impl Raw {
        fn value(&self) -> Value {
            let mut hdrs = vec![
                json!({"name": "From", "value": self.sender}),
                json!({"name": "To", "value": "me@example.com"}),
                json!({"name": "Subject", "value": self.subject}),
                json!({"name": "Message-ID", "value": format!("<{}@x.com>", self.id)}),
            ];
            hdrs.extend(self.headers.clone());
            let mut parts = vec![
                json!({"mimeType": self.mime, "body": {"data": b64(&self.body)}, "headers": []}),
            ];
            parts.extend(self.extra_parts.clone());
            json!({"id": self.id, "threadId": self.thread, "internalDate": self.ms.to_string(),
                   "payload": {"mimeType": "multipart/mixed", "headers": hdrs, "parts": parts}})
        }
    }

    fn provider(stub: &Stub, refresh: Option<crate::mail_provider::RefreshFn>) -> GmailProvider {
        GmailProvider::new(Base::new(
            "tok",
            refresh,
            Some(&format!("{}/gmail/v1", stub.base_url)),
            "me@example.com",
        ))
    }

    fn since() -> DateTime<FixedOffset> {
        Utc.with_ymd_and_hms(2026, 9, 25, 0, 0, 0)
            .unwrap()
            .fixed_offset()
    }

    #[test]
    fn parses_a_plain_message() {
        let p = parse_gmail_message(&raw("m1").value()).unwrap();
        assert_eq!((p.id.as_str(), p.thread_id.as_str()), ("m1", "t1"));
        assert_eq!(
            (p.from.as_str(), p.subject.as_str()),
            ("Alice <alice@x.com>", "Hello")
        );
        assert_eq!(p.body_text, "Hi there");
        assert_eq!(p.date, "2026-09-21T14:13:20+00:00");
        assert_eq!(p.message_id_header, "<m1@x.com>");
    }

    #[test]
    fn parses_html_only_and_survives_a_bad_charset() {
        let mut r = raw("m2");
        r.body = "<p>Hello <b>world</b></p>".into();
        r.mime = "text/html";
        assert_eq!(
            parse_gmail_message(&r.value()).unwrap().body_text,
            "Hello world"
        );
        let mut broken = raw("m3").value();
        broken["payload"]["parts"][0]["body"]["data"] = json!(urlsafe_b64(b"caf\xe9 \xff"));
        broken["payload"]["parts"][0]["headers"] =
            json!([{"name": "Content-Type", "value": "text/plain; charset=gb2312"}]);
        assert!(parse_gmail_message(&broken)
            .unwrap()
            .body_text
            .contains("caf"));
        broken["payload"]["parts"][0]["headers"] =
            json!([{"name": "content-type", "value": "text/plain; charset=\"ISO-8859-1\""}]);
        assert_eq!(
            parse_gmail_message(&broken).unwrap().body_text,
            "caf\u{e9} \u{ff}"
        );
    }

    #[test]
    fn lists_attachments_and_trims_the_body() {
        let mut r = raw("m4");
        r.body = "x".repeat(9000);
        r.extra_parts = vec![
            json!({"mimeType": "application/pdf", "filename": "invoice.pdf", "body": {"attachmentId": "a1", "size": 10}}),
        ];
        let p = parse_gmail_message(&r.value()).unwrap();
        assert_eq!(p.attachments, vec!["invoice.pdf"]);
        assert_eq!(p.body_text.chars().count(), 4000);
    }

    #[test]
    fn attachment_parts_never_supply_body_text() {
        let mut r = raw("m5");
        r.extra_parts = vec![
            json!({"mimeType": "text/plain", "filename": "x.txt", "body": {"data": b64("SECRET")}}),
        ];
        assert_eq!(
            parse_gmail_message(&r.value()).unwrap().body_text,
            "Hi there"
        );
    }

    #[test]
    fn reply_subject_is_not_doubled() {
        assert_eq!(reply_subject("Hello"), "Re: Hello");
        assert_eq!(reply_subject("RE: Hello"), "RE: Hello");
        assert_eq!(reply_subject("re:Hello"), "re:Hello");
        assert_eq!(reply_subject(""), "Re:");
        assert_eq!(reply_subject("\u{e9}\u{e9}"), "Re: \u{e9}\u{e9}");
    }

    #[test]
    fn profile_address_is_lowercased() {
        let stub = Stub::new();
        stub.add(
            "GET",
            &format!("{U}/profile"),
            200,
            Some(json!({"emailAddress": "Me@Example.com"})),
        );
        assert_eq!(
            provider(&stub, None).profile_address().unwrap(),
            "me@example.com"
        );
    }

    fn add_thread(stub: &Stub, thread: &str, messages: Vec<Value>) {
        stub.add(
            "GET",
            &format!("{U}/threads/{thread}"),
            200,
            Some(json!({"messages": messages})),
        );
    }

    #[test]
    fn fetch_new_filters_seen_and_excluded_and_sorts_oldest_first() {
        let stub = Stub::new();
        stub.add("GET", &format!("{U}/messages"), 200,
                 Some(json!({"messages": [{"id": "new2"}, {"id": "seen"}, {"id": "new1"}, {"id": "spam"}]})));
        let mut n1 = raw("new1");
        n1.ms = 1_790_000_000_000;
        let mut n2 = raw("new2");
        (n2.ms, n2.thread) = (1_790_000_500_000, "t2");
        let mut spam = raw("spam");
        (spam.sender, spam.thread) = ("bot@spam.com", "t3");
        stub.add("GET", &format!("{U}/messages/new1"), 200, Some(n1.value()));
        stub.add("GET", &format!("{U}/messages/new2"), 200, Some(n2.value()));
        stub.add(
            "GET",
            &format!("{U}/messages/spam"),
            200,
            Some(spam.value()),
        );
        add_thread(&stub, "t1", vec![n1.value()]);
        add_thread(&stub, "t2", vec![n2.value()]);
        let seen: HashSet<String> = ["seen".to_string()].into();
        let got = provider(&stub, None)
            .fetch_new(since(), &seen, &["@spam.com".into()], 10)
            .unwrap();
        assert_eq!(
            got.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["new1", "new2"]
        );
        // One second early: Gmail's after: is exclusive.
        let list = stub
            .requests()
            .into_iter()
            .find(|r| r.path == format!("{U}/messages"))
            .unwrap();
        assert!(list.query.contains(&format!(
            "q=in%3Ainbox+is%3Aunread+after%3A{}",
            since().timestamp() - 1
        )));
        assert!(list.query.contains("maxResults=100"));
        assert!(!stub
            .requests()
            .iter()
            .any(|r| r.path == format!("{U}/messages/seen")));
    }

    #[test]
    fn fetch_new_includes_prior_thread_and_respects_the_limit() {
        let stub = Stub::new();
        stub.add(
            "GET",
            &format!("{U}/messages"),
            200,
            Some(json!({"messages": [{"id": "d"}, {"id": "c"}]})),
        );
        let mk = |id: &'static str, body: &str, ms: i64| {
            let mut r = raw(id);
            (r.body, r.ms, r.thread) = (body.into(), ms, "t");
            r
        };
        stub.add(
            "GET",
            &format!("{U}/messages/c"),
            200,
            Some(mk("c", "Hi there", 1_790_000_900_000).value()),
        );
        stub.add(
            "GET",
            &format!("{U}/messages/d"),
            200,
            Some(mk("d", "Hi there", 1_790_000_990_000).value()),
        );
        add_thread(
            &stub,
            "t",
            vec![
                mk("a", "first", 1_790_000_100_000).value(),
                mk("b", "second", 1_790_000_200_000).value(),
                mk("x", "third", 1_790_000_300_000).value(),
                mk("c", "Hi there", 1_790_000_900_000).value(),
            ],
        );
        let got = provider(&stub, None)
            .fetch_new(since(), &HashSet::new(), &[], 1)
            .unwrap();
        assert_eq!(
            got.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["c"]
        );
        assert_eq!(
            got[0]
                .prior_thread
                .iter()
                .map(|p| p.body_text.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "third"]
        );
        assert!(!stub
            .requests()
            .iter()
            .any(|r| r.path == format!("{U}/messages/d")));
    }

    #[test]
    fn fetch_new_pages_through_everything_and_keeps_the_globally_oldest() {
        let stub = Stub::new();
        stub.add("GET", &format!("{U}/messages"), 200,
                 Some(json!({"messages": [{"id": "f"}, {"id": "e"}, {"id": "d"}, {"id": "g"}], "nextPageToken": "p2"})));
        stub.add(
            "GET",
            &format!("{U}/messages"),
            200,
            Some(json!({"messages": [{"id": "b"}, {"id": "a"}]})),
        );
        for (id, ms, thread) in [
            ("f", 1_790_000_690_000, "tf"),
            ("e", 1_790_000_680_000, "te"),
            ("d", 1_790_000_670_000, "td"),
            ("g", 1_790_000_660_000, "tg"),
            ("b", 1_790_000_200_000, "tb"),
            ("a", 1_790_000_100_000, "ta"),
        ] {
            let mut r = raw(id);
            (r.ms, r.thread) = (ms, thread);
            stub.add("GET", &format!("{U}/messages/{id}"), 200, Some(r.value()));
            add_thread(&stub, thread, vec![r.value()]);
        }
        let got = provider(&stub, None)
            .fetch_new(since(), &HashSet::new(), &[], 2)
            .unwrap();
        assert_eq!(
            got.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        let lists: Vec<_> = stub
            .requests()
            .into_iter()
            .filter(|r| r.path == format!("{U}/messages"))
            .collect();
        assert_eq!(lists.len(), 2);
        assert!(!lists[0].query.contains("pageToken") && lists[1].query.contains("pageToken=p2"));
        let fetched: HashSet<String> = stub
            .requests()
            .iter()
            .filter(|r| r.path.starts_with(&format!("{U}/messages/")))
            .map(|r| r.path.rsplit('/').next().unwrap().to_string())
            .collect();
        assert_eq!(fetched, HashSet::from(["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn prior_thread_skips_the_loops_own_drafts() {
        let stub = Stub::new();
        stub.add(
            "GET",
            &format!("{U}/messages"),
            200,
            Some(json!({"messages": [{"id": "c"}]})),
        );
        let mk = |id: &'static str, body: &str, ms: i64| {
            let mut r = raw(id);
            (r.body, r.ms, r.thread) = (body.into(), ms, "t");
            r
        };
        stub.add(
            "GET",
            &format!("{U}/messages/c"),
            200,
            Some(mk("c", "Hi there", 1_790_000_900_000).value()),
        );
        let mut draft = mk("draftmsg", "draft reply", 1_790_000_400_000).value();
        draft["labelIds"] = json!(["DRAFT"]);
        add_thread(
            &stub,
            "t",
            vec![
                mk("a", "first", 1_790_000_100_000).value(),
                draft,
                mk("b", "second", 1_790_000_200_000).value(),
                mk("c", "Hi there", 1_790_000_900_000).value(),
            ],
        );
        let got = provider(&stub, None)
            .fetch_new(since(), &HashSet::new(), &[], 1)
            .unwrap();
        assert_eq!(
            got[0]
                .prior_thread
                .iter()
                .map(|p| p.body_text.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    #[test]
    fn ensure_labels_creates_only_the_missing_ones() {
        let stub = Stub::new();
        stub.add("GET", &format!("{U}/labels"), 200,
                 Some(json!({"labels": [{"id": "L1", "name": "Loop/Urgent"}, {"id": "INBOX", "name": "INBOX"}]})));
        stub.add(
            "POST",
            &format!("{U}/labels"),
            200,
            Some(json!({"id": "L2", "name": "Loop/FYI"})),
        );
        let m = provider(&stub, None)
            .ensure_labels(&["Loop/Urgent".into(), "Loop/FYI".into()])
            .unwrap();
        assert_eq!(m["Loop/Urgent"], "L1");
        assert_eq!(m["Loop/FYI"], "L2");
        let created: Vec<Value> = stub
            .bodies("POST", &format!("{U}/labels"))
            .iter()
            .map(|b| serde_json::from_str(b).unwrap())
            .collect();
        assert_eq!(
            created
                .iter()
                .map(|c| c["name"].clone())
                .collect::<Vec<_>>(),
            vec![json!("Loop/FYI")]
        );
    }

    #[test]
    fn apply_label_only_adds() {
        let stub = Stub::new();
        stub.add(
            "POST",
            &format!("{U}/messages/m1/modify"),
            200,
            Some(json!({})),
        );
        provider(&stub, None).apply_label("m1", "L1").unwrap();
        let body: Value =
            serde_json::from_str(&stub.bodies("POST", &format!("{U}/messages/m1/modify"))[0])
                .unwrap();
        assert_eq!(body, json!({"addLabelIds": ["L1"]}));
    }

    fn decode_draft(body: &str) -> (Value, String) {
        let sent: Value = serde_json::from_str(body).unwrap();
        let mime = String::from_utf8(
            urlsafe_b64_decode(sent["message"]["raw"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        (sent, mime)
    }

    #[test]
    fn reply_draft_is_threaded() {
        let mut r = raw("m1");
        r.subject = "Prod down";
        r.headers = vec![json!({"name": "References", "value": "<r0@x.com>"})];
        let message = parse_gmail_message(&r.value()).unwrap();
        let stub = Stub::new();
        stub.add(
            "POST",
            &format!("{U}/drafts"),
            200,
            Some(json!({"id": "d1", "message": {"id": "dm1", "threadId": "t1"}})),
        );
        let link = provider(&stub, None)
            .create_reply_draft(&message, "On it - fixing now.")
            .unwrap();
        let (sent, mime) = decode_draft(&stub.bodies("POST", &format!("{U}/drafts"))[0]);
        assert_eq!(sent["message"]["threadId"], "t1");
        assert!(mime.contains("To: Alice <alice@x.com>\n"));
        assert!(mime.contains("Subject: Re: Prod down\n"));
        assert!(mime.contains("In-Reply-To: <m1@x.com>\n"));
        assert!(mime.contains("References: <r0@x.com> <m1@x.com>\n"));
        assert!(mime.ends_with("\n\nOn it - fixing now.\n"));
        assert_eq!(
            link,
            "https://mail.google.com/mail/?authuser=me%40example.com#drafts?compose=dm1"
        );
    }

    #[test]
    fn reply_goes_to_reply_to_when_present() {
        let mut r = raw("m1");
        r.headers = vec![json!({"name": "Reply-To", "value": "team@x.com"})];
        let message = parse_gmail_message(&r.value()).unwrap();
        let stub = Stub::new();
        stub.add(
            "POST",
            &format!("{U}/drafts"),
            200,
            Some(json!({"id": "d1", "message": {"id": "dm1"}})),
        );
        provider(&stub, None)
            .create_reply_draft(&message, "ok")
            .unwrap();
        assert!(
            decode_draft(&stub.bodies("POST", &format!("{U}/drafts"))[0])
                .1
                .contains("To: team@x.com\n")
        );
    }

    #[test]
    fn a_linefeed_in_a_header_makes_the_draft_fail_without_a_request() {
        let mut message = parse_gmail_message(&raw("m1").value()).unwrap();
        message.subject = "a\nBcc: evil@x.com".into();
        let stub = Stub::new();
        assert!(provider(&stub, None)
            .create_reply_draft(&message, "ok")
            .is_err());
        assert!(stub.requests().is_empty());
    }

    #[test]
    fn a_401_refreshes_once_then_succeeds_and_a_second_raises() {
        let stub = Stub::new();
        stub.add("GET", &format!("{U}/profile"), 401, Some(json!({})))
            .add(
                "GET",
                &format!("{U}/profile"),
                200,
                Some(json!({"emailAddress": "me@example.com"})),
            );
        let refresh: crate::mail_provider::RefreshFn = Box::new(|| Ok("tok2".into()));
        assert_eq!(
            provider(&stub, Some(refresh)).profile_address().unwrap(),
            "me@example.com"
        );
        assert_eq!(
            stub.requests().last().unwrap().headers["authorization"],
            "Bearer tok2"
        );

        let always = Stub::new();
        always.add("GET", &format!("{U}/profile"), 401, Some(json!({})));
        let refresh: crate::mail_provider::RefreshFn = Box::new(|| Ok("tok2".into()));
        assert!(provider(&always, Some(refresh))
            .profile_address()
            .unwrap_err()
            .is_auth_expired());
    }

    fn meta(id: &str, sender: &str, subject: &str, ms: i64, snippet: &str) -> Value {
        json!({"id": id, "internalDate": ms.to_string(), "snippet": snippet,
               "payload": {"headers": [{"name": "From", "value": sender}, {"name": "Subject", "value": subject}]}})
    }

    #[test]
    fn search_messages_returns_metadata_rows_without_bodies() {
        let stub = Stub::new();
        stub.add(
            "GET",
            &format!("{U}/messages"),
            200,
            Some(json!({"messages": [{"id": "m1"}]})),
        );
        stub.add(
            "GET",
            &format!("{U}/messages/m1"),
            200,
            Some(meta("m1", "Al <al@x.com>", "Hi", 1_790_000_000_000, "snip")),
        );
        let rows = provider(&stub, None)
            .search_messages("from:al invoice", 5)
            .unwrap();
        let q = &stub.requests()[0].query;
        assert!(q.contains("q=from%3Aal+invoice") && q.contains("maxResults=5"));
        assert_eq!(
            rows[0].to_value(),
            json!({"id": "m1", "subject": "Hi", "from": "Al <al@x.com>",
                                              "date": "2026-09-21T14:13:20+00:00", "snippet": "snip"})
        );
    }

    #[test]
    fn read_message_has_public_fields_only() {
        let stub = Stub::new();
        stub.add(
            "GET",
            &format!("{U}/messages/m1"),
            200,
            Some(raw("m1").value()),
        );
        let m = provider(&stub, None).read_message("m1").unwrap();
        assert_eq!(
            (m.subject.as_str(), m.body_text.as_str()),
            ("Hello", "Hi there")
        );
        assert!(m.message_id_header.is_empty() && m.prior_thread.is_empty());
    }

    #[test]
    fn search_recent_queries_participants_and_returns_metadata_only() {
        let stub = Stub::new();
        stub.add(
            "GET",
            &format!("{U}/messages"),
            200,
            Some(json!({"messages": [{"id": "a"}, {"id": "b"}]})),
        );
        stub.add(
            "GET",
            &format!("{U}/messages/a"),
            200,
            Some(meta(
                "a",
                "Ann <ann@x.com>",
                "Plan",
                1_790_000_900_000,
                "See you then",
            )),
        );
        stub.add(
            "GET",
            &format!("{U}/messages/b"),
            200,
            Some(meta(
                "b",
                "Ann <ann@x.com>",
                "Older",
                1_790_000_100_000,
                "x",
            )),
        );
        let addrs = [
            "ann@x.com".to_string(),
            "bob@y.org".into(),
            "bad addr\" OR x".into(),
        ];
        let rows = provider(&stub, None).search_recent(&addrs, 30, 10).unwrap();
        let reqs = stub.requests();
        let list = reqs
            .iter()
            .find(|r| r.path == format!("{U}/messages"))
            .unwrap();
        assert!(list.query.contains(&format!(
            "q={}",
            &crate::mail_http::urlencode(&[(
                "q",
                "{from:ann@x.com to:ann@x.com from:bob@y.org to:bob@y.org} newer_than:30d".into()
            )])[2..]
        )));
        let get = reqs
            .iter()
            .find(|r| r.path == format!("{U}/messages/a"))
            .unwrap();
        assert!(
            get.query.contains("format=metadata") && get.query.contains("metadataHeaders=From")
        );
        assert_eq!(
            rows.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>(),
            vec!["Plan", "Older"]
        );
        assert_eq!(
            rows[0].to_value(),
            json!({"subject": "Plan", "from": "Ann <ann@x.com>",
                                              "date": "2026-09-21T14:28:20+00:00", "snippet": "See you then"})
        );
    }

    #[test]
    fn search_recent_without_valid_addresses_makes_no_call() {
        let stub = Stub::new();
        assert!(provider(&stub, None)
            .search_recent(&["not an address".into()], 30, 10)
            .unwrap()
            .is_empty());
        assert!(stub.requests().is_empty());
    }

    #[test]
    fn the_source_has_no_way_to_send_mail() {
        // The same promise tests/test_gmail_provider.py checks: no send endpoint.
        let src = include_str!("gmail.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        for forbidden in ["/send", "messages/send", "send_message"] {
            assert!(!code.contains(forbidden), "{forbidden}");
        }
    }

    #[test]
    fn parse_requires_an_id() {
        assert!(parse_gmail_message(&json!({"threadId": "t"})).is_err());
    }

    #[test]
    fn dates_parse_back() {
        let p = parse_gmail_message(&raw("m1").value()).unwrap();
        assert!(parse_iso(&p.date).is_some());
    }
}
