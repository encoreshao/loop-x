//! Microsoft Graph v1.0 provider (`bin/mail_providers/outlook.py`). The token
//! this loop obtains cannot deliver mail at all (the `Mail.Send` scope is never
//! requested). Mailbox writes are limited to: creating master categories,
//! appending a category to a message, and creating a reply draft via
//! `createReply`. Nothing here archives, deletes, or changes read state.

use super::base::{plain_addresses, Base};
use super::{MailMessage, MailProvider, MessageRow, PriorMessage};
use crate::inbox_config::{parse_address, sender_matches};
use crate::inbox_triage::{html_to_text, trim, BODY_LIMIT, PRIOR_COUNT, PRIOR_LIMIT};
use crate::mail_http::{quote, urlencode, urlencode_percent, MailError};
use crate::mail_time::{isoformat, parse_iso};
use chrono::{DateTime, Duration, FixedOffset, SecondsFormat, Utc};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

pub const DEFAULT_BASE_URL: &str = "https://graph.microsoft.com/v1.0";
const PREFER_TEXT: [(&str, &str); 1] = [("Prefer", "outlook.body-content-type=\"text\"")];
const SELECT: &str =
    "id,conversationId,from,toRecipients,ccRecipients,subject,receivedDateTime,body,hasAttachments,internetMessageId";
/// Safety cap: 20 pages of `$top=100`, mirroring Gmail's own list-page cap.
const LIST_PAGE_LIMIT: usize = 20;

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn person(entry: Option<&Value>) -> String {
    let Some(addr) = entry.and_then(|e| e.get("emailAddress")) else {
        return String::new();
    };
    let (address, name) = (str_of(addr, "address"), str_of(addr, "name"));
    if address.is_empty() {
        String::new()
    } else if !name.is_empty() && name != address {
        format!("{name} <{address}>")
    } else {
        address.to_string()
    }
}

fn people(list: Option<&Value>) -> String {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|r| person(Some(r)))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

fn body(raw: &Value) -> String {
    let Some(b) = raw.get("body").filter(|b| b.is_object()) else {
        return String::new();
    };
    let content = str_of(b, "content");
    if str_of(b, "contentType").eq_ignore_ascii_case("html") {
        html_to_text(content)
    } else {
        content.trim().to_string()
    }
}

/// `datetime.fromisoformat(value.replace("Z", "+00:00")).isoformat()`.
fn iso(value: &str) -> Result<String, MailError> {
    parse_iso(value)
        .map(|d| isoformat(&d))
        .ok_or_else(|| MailError::Other(format!("Invalid isoformat string: {value:?}")))
}

pub struct Parsed {
    pub message: MailMessage,
    pub has_attachments: bool,
}

pub fn parse_graph_message(raw: &Value) -> Result<Parsed, MailError> {
    let key = |k: &str| MailError::Other(format!("KeyError: '{k}'"));
    let id = raw
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| key("id"))?;
    let received = raw
        .get("receivedDateTime")
        .and_then(Value::as_str)
        .ok_or_else(|| key("receivedDateTime"))?;
    Ok(Parsed {
        message: MailMessage {
            id: id.to_string(),
            thread_id: str_of(raw, "conversationId").to_string(),
            from: person(raw.get("from")),
            to: people(raw.get("toRecipients")),
            cc: people(raw.get("ccRecipients")),
            subject: str_of(raw, "subject").to_string(),
            date: iso(received)?,
            body_text: trim(&body(raw), BODY_LIMIT),
            ..Default::default()
        },
        has_attachments: raw
            .get("hasAttachments")
            .is_some_and(crate::loops_config::py_truthy),
    })
}

pub struct OutlookProvider {
    base: Base,
}

impl OutlookProvider {
    pub fn new(base: Base) -> Self {
        Self {
            base: base.with_default_url(DEFAULT_BASE_URL),
        }
    }

    fn get(&mut self, path: &str, headers: &[(&str, &str)]) -> Result<Value, MailError> {
        self.base.call("GET", path, None, headers)
    }

    fn attachment_names(&mut self, id_path: &str) -> Result<Vec<String>, MailError> {
        let atts = self.get(
            &format!("/me/messages/{id_path}/attachments?$select=name"),
            &[],
        )?;
        Ok(atts
            .get("value")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|a| str_of(a, "name").to_string())
            .collect())
    }

    fn prior_thread(&mut self, message: &MailMessage) -> Result<Vec<PriorMessage>, MailError> {
        let filter = format!(
            "conversationId eq '{}'",
            message.thread_id.replace('\'', "''")
        );
        let params = [
            ("$filter", filter),
            (
                "$select",
                "id,from,receivedDateTime,body,isDraft".to_string(),
            ),
            ("$top", "10".to_string()),
        ];
        let page = self.get(
            &format!("/me/messages?{}", urlencode_percent(&params)),
            &PREFER_TEXT,
        )?;
        // Never feed the loop's own earlier draft reply in this thread back to the AI.
        let mut earlier = Vec::new();
        for raw in page
            .get("value")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let received = raw
                .get("receivedDateTime")
                .is_some_and(crate::loops_config::py_truthy);
            if str_of(raw, "id") == message.id
                || !received
                || raw
                    .get("isDraft")
                    .is_some_and(crate::loops_config::py_truthy)
            {
                continue;
            }
            let mut copy = raw.clone();
            copy["subject"] = json!("");
            let parsed = parse_graph_message(&copy)?.message;
            if parsed.date < message.date {
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

    /// [`MailProvider::search_recent`] with an injectable clock.
    pub fn search_recent_at(
        &mut self,
        addresses: &[String],
        days: i64,
        limit: usize,
        now: DateTime<Utc>,
    ) -> Result<Vec<MessageRow>, MailError> {
        let addresses = plain_addresses(addresses);
        if addresses.is_empty() {
            return Ok(Vec::new());
        }
        let search = addresses
            .iter()
            .map(|a| format!("participants:{a}"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let params = [
            ("$search", format!("\"{search}\"")),
            (
                "$select",
                "subject,from,receivedDateTime,bodyPreview".to_string(),
            ),
            ("$top", "50".to_string()),
        ];
        let page = self.get(&format!("/me/messages?{}", urlencode(&params)), &[])?;
        let cutoff = now - Duration::days(days);
        let mut rows = Vec::new();
        for raw in page
            .get("value")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(received) = raw
                .get("receivedDateTime")
                .and_then(Value::as_str)
                .and_then(parse_iso)
            else {
                continue;
            };
            if received < cutoff {
                continue;
            }
            rows.push(MessageRow {
                id: None,
                subject: str_of(raw, "subject").to_string(),
                from: person(raw.get("from")),
                date: isoformat(&received),
                snippet: str_of(raw, "bodyPreview").to_string(),
            });
        }
        rows.sort_by(|a, b| b.date.cmp(&a.date));
        rows.truncate(limit);
        Ok(rows)
    }
}

impl MailProvider for OutlookProvider {
    fn profile_address(&mut self) -> Result<String, MailError> {
        let me = self.get("/me", &[])?;
        let mail = Some(str_of(&me, "mail")).filter(|s| !s.is_empty());
        Ok(mail
            .unwrap_or_else(|| str_of(&me, "userPrincipalName"))
            .trim()
            .to_lowercase())
    }

    fn search_recent(
        &mut self,
        addresses: &[String],
        days: i64,
        limit: usize,
    ) -> Result<Vec<MessageRow>, MailError> {
        self.search_recent_at(addresses, days, limit, Utc::now())
    }

    fn search_messages(&mut self, query: &str, limit: usize) -> Result<Vec<MessageRow>, MailError> {
        let text = query.replace('"', " ");
        let params = [
            ("$search", format!("\"{}\"", text.trim())),
            (
                "$select",
                "id,subject,from,receivedDateTime,bodyPreview".to_string(),
            ),
            ("$top", limit.to_string()),
        ];
        let page = self.get(&format!("/me/messages?{}", urlencode(&params)), &[])?;
        let mut rows = Vec::new();
        for raw in page
            .get("value")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = raw
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| MailError::Other("KeyError: 'id'".into()))?;
            let received = Some(str_of(raw, "receivedDateTime"))
                .filter(|s| !s.is_empty())
                .unwrap_or("1970-01-01T00:00:00Z");
            rows.push(MessageRow {
                id: Some(id.to_string()),
                subject: str_of(raw, "subject").to_string(),
                from: person(raw.get("from")),
                date: iso(received)?,
                snippet: str_of(raw, "bodyPreview").to_string(),
            });
        }
        rows.sort_by(|a, b| b.date.cmp(&a.date));
        rows.truncate(limit);
        Ok(rows)
    }

    fn read_message(&mut self, message_id: &str) -> Result<MailMessage, MailError> {
        let id = quote(message_id);
        let raw = self.get(&format!("/me/messages/{id}?$select={SELECT}"), &PREFER_TEXT)?;
        let parsed = parse_graph_message(&raw)?;
        let mut message = parsed.message;
        if parsed.has_attachments {
            message.attachments = self.attachment_names(&id)?;
        }
        Ok(message)
    }

    fn fetch_new(
        &mut self,
        since: DateTime<FixedOffset>,
        seen_ids: &HashSet<String>,
        exclude: &[String],
        limit: usize,
    ) -> Result<Vec<MailMessage>, MailError> {
        let since_utc = since
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Secs, true);
        // Graph rejects $orderby on a property that isn't also the leading
        // clause of $filter, in the same order (400 InefficientFilter).
        let params = [
            (
                "$filter",
                format!("receivedDateTime ge {since_utc} and isRead eq false"),
            ),
            ("$orderby", "receivedDateTime asc".to_string()),
            ("$top", "100".to_string()),
            ("$select", SELECT.to_string()),
        ];
        let mut url = format!(
            "/me/mailFolders/inbox/messages?{}",
            urlencode_percent(&params)
        );
        let mut found: Vec<Parsed> = Vec::new();
        for _ in 0..LIST_PAGE_LIMIT {
            let page = self.get(&url, &PREFER_TEXT)?;
            for raw in page
                .get("value")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let id = raw
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| MailError::Other("KeyError: 'id'".into()))?;
                if seen_ids.contains(id) {
                    continue;
                }
                let parsed = parse_graph_message(raw)?;
                if sender_matches(&parse_address(&parsed.message.from), exclude) {
                    continue;
                }
                found.push(parsed);
                if found.len() >= limit {
                    break;
                }
            }
            if found.len() >= limit {
                break;
            }
            // @odata.nextLink is an absolute URL; Base::call uses it as-is.
            match page
                .get("@odata.nextLink")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                Some(next) => url = next.to_string(),
                None => break,
            }
        }
        found.sort_by(|a, b| a.message.date.cmp(&b.message.date));
        let mut messages = Vec::new();
        for mut p in found {
            if p.has_attachments {
                p.message.attachments = self.attachment_names(&p.message.id)?;
            }
            p.message.prior_thread = self.prior_thread(&p.message)?;
            messages.push(p.message);
        }
        Ok(messages)
    }

    fn ensure_labels(&mut self, labels: &[String]) -> Result<HashMap<String, String>, MailError> {
        let listing = self.get("/me/outlook/masterCategories", &[])?;
        let mut existing: HashSet<String> = listing
            .get("value")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| str_of(c, "displayName").to_string())
            .collect();
        for name in labels {
            if !existing.contains(name) {
                self.base.call(
                    "POST",
                    "/me/outlook/masterCategories",
                    Some(json!({"displayName": name, "color": "preset7"})),
                    &[],
                )?;
                existing.insert(name.clone());
            }
        }
        Ok(labels.iter().map(|n| (n.clone(), n.clone())).collect())
    }

    fn apply_label(&mut self, message_id: &str, label_id: &str) -> Result<(), MailError> {
        let got = self.get(
            &format!("/me/messages/{message_id}?$select=categories"),
            &[],
        )?;
        let mut current: Vec<Value> = got
            .get("categories")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if current.iter().any(|c| c == label_id) {
            return Ok(());
        }
        current.push(json!(label_id));
        self.base
            .call(
                "PATCH",
                &format!("/me/messages/{message_id}"),
                Some(json!({"categories": current})),
                &[],
            )
            .map(|_| ())
    }

    fn create_reply_draft(
        &mut self,
        message: &MailMessage,
        body: &str,
    ) -> Result<String, MailError> {
        let draft = self.base.call(
            "POST",
            &format!("/me/messages/{}/createReply", message.id),
            Some(json!({"comment": body})),
            &[],
        )?;
        Ok(str_of(&draft, "webLink").to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail_http::testserver::Stub;
    use chrono::TimeZone;

    fn provider(stub: &Stub) -> OutlookProvider {
        OutlookProvider::new(Base::new(
            "tok",
            None,
            Some(&format!("{}/v1.0", stub.base_url)),
            "me@example.com",
        ))
    }

    fn graph(
        id: &str,
        sender: &str,
        subject: &str,
        received: &str,
        body: &str,
        conv: &str,
    ) -> Value {
        json!({"id": id, "conversationId": conv, "from": {"emailAddress": {"name": "Ann", "address": sender}},
               "toRecipients": [{"emailAddress": {"name": "Me", "address": "me@example.com"}}],
               "ccRecipients": [], "subject": subject, "receivedDateTime": received,
               "body": {"contentType": "text", "content": body}, "hasAttachments": false,
               "internetMessageId": format!("<{id}@x>")})
    }

    fn since() -> DateTime<FixedOffset> {
        Utc.with_ymd_and_hms(2026, 9, 25, 0, 0, 0)
            .unwrap()
            .fixed_offset()
    }

    const M: &str = "/v1.0/me/messages";
    const INBOX: &str = "/v1.0/me/mailFolders/inbox/messages";

    #[test]
    fn parses_people_dates_and_text_bodies() {
        let p = parse_graph_message(&graph(
            "m1",
            "ann@x.com",
            "Hi",
            "2026-09-27T08:00:00Z",
            "Body",
            "c1",
        ))
        .unwrap();
        assert_eq!(p.message.from, "Ann <ann@x.com>");
        assert_eq!(p.message.to, "Me <me@example.com>");
        assert_eq!(p.message.date, "2026-09-27T08:00:00+00:00");
        assert_eq!(
            (p.message.thread_id.as_str(), p.message.body_text.as_str()),
            ("c1", "Body")
        );
        assert!(!p.has_attachments);
    }

    #[test]
    fn html_bodies_become_text_and_names_equal_to_the_address_collapse() {
        let mut raw = graph(
            "m1",
            "ann@x.com",
            "Hi",
            "2026-09-27T08:00:00Z",
            "<p>Hello <b>you</b></p>",
            "c",
        );
        raw["body"]["contentType"] = json!("html");
        raw["from"]["emailAddress"]["name"] = json!("ann@x.com");
        let p = parse_graph_message(&raw).unwrap().message;
        assert_eq!(
            (p.body_text.as_str(), p.from.as_str()),
            ("Hello you", "ann@x.com")
        );
    }

    #[test]
    fn profile_address_prefers_mail_then_upn() {
        let stub = Stub::new();
        stub.add(
            "GET",
            "/v1.0/me",
            200,
            Some(json!({"mail": "Me@Example.com", "userPrincipalName": "x@y"})),
        );
        assert_eq!(provider(&stub).profile_address().unwrap(), "me@example.com");
        let stub = Stub::new();
        stub.add(
            "GET",
            "/v1.0/me",
            200,
            Some(json!({"mail": null, "userPrincipalName": "UPN@y.com"})),
        );
        assert_eq!(provider(&stub).profile_address().unwrap(), "upn@y.com");
    }

    #[test]
    fn fetch_new_filters_orders_and_adds_thread_context_and_attachments() {
        let stub = Stub::new();
        let mut with_att = graph(
            "a2",
            "ann@x.com",
            "Two",
            "2026-09-27T09:00:00Z",
            "later",
            "c2",
        );
        with_att["hasAttachments"] = json!(true);
        stub.add(
            "GET",
            INBOX,
            200,
            Some(json!({"value": [
            with_att,
            graph("seen", "ann@x.com", "Seen", "2026-09-27T08:30:00Z", "x", "c3"),
            graph("a1", "ann@x.com", "One", "2026-09-27T08:00:00Z", "earlier", "c1"),
            graph("sp", "bot@spam.com", "Spam", "2026-09-27T08:10:00Z", "x", "c4")]})),
        );
        stub.add("GET", INBOX, 200, Some(json!({"value": []})));
        stub.add(
            "GET",
            &format!("{M}/a2/attachments"),
            200,
            Some(json!({"value": [{"name": "inv.pdf"}]})),
        );
        stub.add("GET", M, 200, Some(json!({"value": [
            graph("p0", "ann@x.com", "ignored", "2026-09-27T07:00:00Z", "prior one", "c1"),
            {"id": "d0", "isDraft": true, "receivedDateTime": "2026-09-27T07:30:00Z", "body": {"content": "my draft"}},
            graph("a1", "ann@x.com", "One", "2026-09-27T08:00:00Z", "earlier", "c1")]})));
        let seen: HashSet<String> = ["seen".to_string()].into();
        let got = provider(&stub)
            .fetch_new(since(), &seen, &["@spam.com".into()], 10)
            .unwrap();
        assert_eq!(
            got.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["a1", "a2"]
        );
        assert_eq!(
            got[0]
                .prior_thread
                .iter()
                .map(|p| p.body_text.as_str())
                .collect::<Vec<_>>(),
            vec!["prior one"]
        );
        let list = stub
            .requests()
            .into_iter()
            .find(|r| r.path == INBOX)
            .unwrap();
        assert!(list.query.contains("%24filter=receivedDateTime%20ge%202026-09-25T00%3A00%3A00Z%20and%20isRead%20eq%20false"), "{}", list.query);
        assert!(
            list.query.contains("%24orderby=receivedDateTime%20asc")
                && list.query.contains("%24top=100")
        );
        assert_eq!(list.headers["prefer"], "outlook.body-content-type=\"text\"");
    }

    #[test]
    fn fetch_new_follows_next_link_and_stops_at_the_limit() {
        let stub = Stub::new();
        stub.add(
            "GET",
            INBOX,
            200,
            Some(json!({
            "value": [graph("a", "ann@x.com", "A", "2026-09-27T08:00:00Z", "x", "ca")],
            "@odata.nextLink": format!("{}/v1.0/next-page", stub.base_url)})),
        );
        stub.add(
            "GET",
            "/v1.0/next-page",
            200,
            Some(json!({
            "value": [graph("b", "ann@x.com", "B", "2026-09-27T09:00:00Z", "x", "cb"),
                      graph("c", "ann@x.com", "C", "2026-09-27T10:00:00Z", "x", "cc")]})),
        );
        stub.add("GET", M, 200, Some(json!({"value": []})));
        let got = provider(&stub)
            .fetch_new(since(), &HashSet::new(), &[], 2)
            .unwrap();
        assert_eq!(
            got.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn prior_thread_filter_escapes_quotes() {
        let stub = Stub::new();
        stub.add("GET", INBOX, 200, Some(json!({"value": [graph("a", "ann@x.com", "A", "2026-09-27T08:00:00Z", "x", "c'x")]})));
        stub.add("GET", M, 200, Some(json!({"value": []})));
        provider(&stub)
            .fetch_new(since(), &HashSet::new(), &[], 5)
            .unwrap();
        let q = stub
            .requests()
            .into_iter()
            .find(|r| r.path == M)
            .unwrap()
            .query;
        assert!(q.contains("conversationId%20eq%20%27c%27%27x%27"), "{q}");
    }

    #[test]
    fn ensure_labels_creates_missing_categories_and_maps_names_to_themselves() {
        let stub = Stub::new();
        stub.add(
            "GET",
            "/v1.0/me/outlook/masterCategories",
            200,
            Some(json!({"value": [{"displayName": "Loop/Urgent"}]})),
        );
        stub.add(
            "POST",
            "/v1.0/me/outlook/masterCategories",
            201,
            Some(json!({})),
        );
        let m = provider(&stub)
            .ensure_labels(&["Loop/Urgent".into(), "Loop/FYI".into()])
            .unwrap();
        assert_eq!(m["Loop/FYI"], "Loop/FYI");
        let created: Vec<Value> = stub
            .bodies("POST", "/v1.0/me/outlook/masterCategories")
            .iter()
            .map(|b| serde_json::from_str(b).unwrap())
            .collect();
        assert_eq!(
            created,
            vec![json!({"displayName": "Loop/FYI", "color": "preset7"})]
        );
    }

    #[test]
    fn apply_label_appends_once() {
        let stub = Stub::new();
        stub.add(
            "GET",
            &format!("{M}/m1"),
            200,
            Some(json!({"categories": ["Mine"]})),
        );
        stub.add("PATCH", &format!("{M}/m1"), 200, Some(json!({})));
        provider(&stub).apply_label("m1", "Loop/FYI").unwrap();
        let body: Value =
            serde_json::from_str(&stub.bodies("PATCH", &format!("{M}/m1"))[0]).unwrap();
        assert_eq!(body, json!({"categories": ["Mine", "Loop/FYI"]}));
        let again = Stub::new();
        again.add(
            "GET",
            &format!("{M}/m1"),
            200,
            Some(json!({"categories": ["Loop/FYI"]})),
        );
        provider(&again).apply_label("m1", "Loop/FYI").unwrap();
        assert!(again.bodies("PATCH", &format!("{M}/m1")).is_empty());
    }

    #[test]
    fn reply_draft_uses_create_reply_and_returns_the_web_link() {
        let stub = Stub::new();
        stub.add(
            "POST",
            &format!("{M}/m1/createReply"),
            201,
            Some(json!({"webLink": "https://outlook/x"})),
        );
        let message = MailMessage {
            id: "m1".into(),
            ..Default::default()
        };
        assert_eq!(
            provider(&stub)
                .create_reply_draft(&message, "On it")
                .unwrap(),
            "https://outlook/x"
        );
        let body: Value =
            serde_json::from_str(&stub.bodies("POST", &format!("{M}/m1/createReply"))[0]).unwrap();
        assert_eq!(body, json!({"comment": "On it"}));
    }

    #[test]
    fn search_recent_applies_the_window_client_side() {
        let stub = Stub::new();
        stub.add("GET", M, 200, Some(json!({"value": [
            {"subject": "New", "from": {"emailAddress": {"name": "Ann", "address": "ann@x.com"}}, "receivedDateTime": "2026-09-26T10:00:00Z", "bodyPreview": "p1"},
            {"subject": "Old", "from": {"emailAddress": {"address": "ann@x.com"}}, "receivedDateTime": "2026-06-01T10:00:00Z", "bodyPreview": "p2"},
            {"subject": "Bad", "receivedDateTime": "garbage"}]})));
        let now = Utc.with_ymd_and_hms(2026, 9, 27, 0, 0, 0).unwrap();
        let rows = provider(&stub)
            .search_recent_at(&["ann@x.com".into(), "bad addr".into()], 30, 10, now)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].to_value(),
            json!({"subject": "New", "from": "Ann <ann@x.com>", "date": "2026-09-26T10:00:00+00:00", "snippet": "p1"})
        );
        let q = &stub.requests()[0].query;
        assert!(
            q.contains("%24search=%22participants%3Aann%40x.com%22") && q.contains("%24top=50"),
            "{q}"
        );
        let none = Stub::new();
        assert!(provider(&none)
            .search_recent_at(&["nope".into()], 30, 10, now)
            .unwrap()
            .is_empty());
        assert!(none.requests().is_empty());
    }

    #[test]
    fn search_messages_strips_quotes_and_sorts_newest_first() {
        let stub = Stub::new();
        stub.add("GET", M, 200, Some(json!({"value": [
            {"id": "o", "subject": "Old", "receivedDateTime": "2026-01-01T00:00:00Z", "bodyPreview": ""},
            {"id": "n", "subject": "New", "receivedDateTime": "2026-02-01T00:00:00Z", "bodyPreview": "x"}]})));
        let rows = provider(&stub)
            .search_messages("say \"hi\" now", 5)
            .unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r.id.clone().unwrap())
                .collect::<Vec<_>>(),
            vec!["n", "o"]
        );
        assert!(
            stub.requests()[0]
                .query
                .contains("%24search=%22say++hi++now%22"),
            "{}",
            stub.requests()[0].query
        );
    }

    #[test]
    fn read_message_includes_attachment_names() {
        let stub = Stub::new();
        let mut raw = graph(
            "m1",
            "ann@x.com",
            "Hi",
            "2026-09-27T08:00:00Z",
            "Body",
            "c1",
        );
        raw["hasAttachments"] = json!(true);
        stub.add("GET", &format!("{M}/m1"), 200, Some(raw));
        stub.add(
            "GET",
            &format!("{M}/m1/attachments"),
            200,
            Some(json!({"value": [{"name": "a.pdf"}, {"name": "b.png"}]})),
        );
        let m = provider(&stub).read_message("m1").unwrap();
        assert_eq!(m.attachments, vec!["a.pdf", "b.png"]);
        assert!(m.prior_thread.is_empty());
    }

    #[test]
    fn the_source_has_no_way_to_send_mail() {
        let src = include_str!("outlook.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        for forbidden in ["/send", "sendMail", "Mail.Send"] {
            assert!(
                !code.replace("`Mail.Send`", "").contains(forbidden),
                "{forbidden}"
            );
        }
    }
}
