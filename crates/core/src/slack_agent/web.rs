//! Slack Web API client. Tokens travel only in the `Authorization` header and
//! never reach an error: failures are mapped to [`WebError`], which carries a
//! sanitized Slack error code at most. Message bodies are untrusted and are
//! never inspected or echoed here.

use crate::connectors::{HttpClient, HttpError, HttpRequest};
use serde_json::{json, Value};
use std::fmt;
use std::sync::Arc;

const BASE_URL: &str = "https://slack.com/api/";
/// Slack accepts far more; this stays well under every client's rendering cap.
pub const SLACK_TEXT_LIMIT: usize = 3500;
/// Used when a 429 carries no usable hint (the HTTP seam hides headers).
const DEFAULT_RETRY_AFTER: u64 = 5;
/// Pages followed by `history` / `member_channels` before giving up.
const MAX_PAGES: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthInfo {
    pub bot_user_id: String,
    pub team: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebError {
    /// The token is invalid, revoked or expired.
    Auth,
    RateLimited {
        retry_after: u64,
    },
    /// The token lacks a scope; carries only the scope name (may be empty).
    MissingScope(String),
    /// Slack's sanitized `error` code.
    Api(String),
    /// Transport failure; deliberately carries no detail.
    Net,
}

impl fmt::Display for WebError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WebError::Auth => write!(f, "Slack rejected the token"),
            WebError::RateLimited { retry_after } => {
                write!(f, "Slack rate limit, retry in {retry_after}s")
            }
            WebError::MissingScope(s) if s.is_empty() => write!(f, "Slack scope missing"),
            WebError::MissingScope(s) => write!(f, "Slack scope missing: {s}"),
            WebError::Api(code) => write!(f, "Slack API error: {code}"),
            WebError::Net => write!(f, "Slack network error"),
        }
    }
}

impl std::error::Error for WebError {}

pub struct SlackWeb {
    bot_token: String,
    app_token: String,
    http: Arc<dyn HttpClient>,
}

impl fmt::Debug for SlackWeb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlackWeb").finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
enum Tok {
    Bot,
    App,
}

/// A Slack error code is `[a-z0-9_]` and short; anything else is not echoed.
fn sanitize_code(s: &str) -> String {
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        s.to_string()
    } else {
        "unknown_error".to_string()
    }
}

/// `retry_after` (seconds) from a 429 body when Slack put one there.
fn body_retry_after(body: &str) -> u64 {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("retry_after").and_then(Value::as_u64))
        .unwrap_or(DEFAULT_RETRY_AFTER)
}

fn map_http_error(e: &HttpError) -> WebError {
    match e.status {
        None => WebError::Net,
        Some(401) => WebError::Auth,
        Some(_) if e.auth_expired => WebError::Auth,
        Some(429) => WebError::RateLimited {
            retry_after: body_retry_after(&e.body),
        },
        Some(s) => WebError::Api(format!("http_{s}")),
    }
}

fn map_api_error(resp: &Value) -> WebError {
    let code = sanitize_code(resp.get("error").and_then(Value::as_str).unwrap_or(""));
    match code.as_str() {
        "invalid_auth" | "not_authed" | "token_revoked" | "token_expired" | "account_inactive" => {
            WebError::Auth
        }
        "ratelimited" => WebError::RateLimited {
            retry_after: DEFAULT_RETRY_AFTER,
        },
        "missing_scope" => WebError::MissingScope(
            resp.get("needed")
                .and_then(Value::as_str)
                .map(|s| {
                    s.split(',')
                        .map(sanitize_scope)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default(),
        ),
        _ => WebError::Api(code),
    }
}

/// Slack's code for a token of the wrong kind in a call (an `xapp-` token
/// where a bot token belongs, or the reverse): the two tokens are swapped.
pub const WRONG_TOKEN_TYPE: &str = "not_allowed_token_type";

/// Whether `e` means the token itself is unusable as set up, so retrying can
/// never help: the wrong kind of token, or (for `apps.connections.open`) an
/// app-level token without `connections:write`.
pub fn is_token_setup_error(e: &WebError) -> bool {
    match e {
        WebError::Api(code) => code == WRONG_TOKEN_TYPE,
        WebError::MissingScope(_) => true,
        _ => false,
    }
}

/// Scope names look like `channels:history`.
fn sanitize_scope(s: &str) -> String {
    let s = s.trim();
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || b == b'_'
                || b == b':'
                || b == b'.'
                || b == b'-'
        });
    if ok {
        s.to_string()
    } else {
        String::new()
    }
}

fn method_is_write(method: &str) -> bool {
    matches!(
        method,
        "chat.postMessage" | "chat.update" | "reactions.add" | "reactions.remove"
    )
}

impl SlackWeb {
    pub fn new(bot_token: String, app_token: String, http: Arc<dyn HttpClient>) -> Self {
        Self {
            bot_token,
            app_token,
            http,
        }
    }

    /// One Web API call. Write methods send JSON, read methods a form.
    fn call(&self, method: &str, tok: Tok, body: CallBody) -> Result<Value, WebError> {
        let token = match tok {
            Tok::Bot => &self.bot_token,
            Tok::App => &self.app_token,
        };
        let mut req = HttpRequest::new("POST", &format!("{BASE_URL}{method}")).token(Some(token));
        match body {
            CallBody::Json(v) => req = req.json(Some(v)),
            CallBody::Form(f) => req.form = Some(f),
        }
        if method_is_write(method) {
            // Not idempotent, and a retry sleep would block the worker: the
            // caller owns the retry policy.
            req = req.attempts(1);
        }
        let resp = self
            .http
            .request_json(&req)
            .map_err(|e| map_http_error(&e))?;
        if resp.get("ok") == Some(&Value::Bool(true)) {
            Ok(resp)
        } else {
            Err(map_api_error(&resp))
        }
    }

    pub fn open_socket_url(&self) -> Result<String, WebError> {
        let r = self.call("apps.connections.open", Tok::App, CallBody::form(&[]))?;
        str_field(&r, "url")
    }

    pub fn auth_test(&self) -> Result<AuthInfo, WebError> {
        let r = self.call("auth.test", Tok::Bot, CallBody::form(&[]))?;
        Ok(AuthInfo {
            bot_user_id: str_field(&r, "user_id")?,
            team: r
                .get("team")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    pub fn post(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
        blocks: Option<&Value>,
    ) -> Result<String, WebError> {
        let mut body = json!({"channel": channel, "text": text});
        if let Some(t) = thread_ts {
            body["thread_ts"] = json!(t);
        }
        if let Some(b) = blocks {
            body["blocks"] = b.clone();
        }
        let r = self.call("chat.postMessage", Tok::Bot, CallBody::Json(body))?;
        str_field(&r, "ts")
    }

    pub fn update(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
        blocks: Option<&Value>,
    ) -> Result<(), WebError> {
        let mut body = json!({"channel": channel, "ts": ts, "text": text});
        if let Some(b) = blocks {
            body["blocks"] = b.clone();
        }
        self.call("chat.update", Tok::Bot, CallBody::Json(body))
            .map(|_| ())
    }

    pub fn react(&self, channel: &str, ts: &str, emoji: &str) -> Result<(), WebError> {
        self.reaction("reactions.add", channel, ts, emoji)
    }

    pub fn unreact(&self, channel: &str, ts: &str, emoji: &str) -> Result<(), WebError> {
        self.reaction("reactions.remove", channel, ts, emoji)
    }

    fn reaction(&self, method: &str, channel: &str, ts: &str, emoji: &str) -> Result<(), WebError> {
        let body = json!({"channel": channel, "timestamp": ts, "name": emoji});
        self.call(method, Tok::Bot, CallBody::Json(body))
            .map(|_| ())
    }

    pub fn open_dm(&self, user: &str) -> Result<String, WebError> {
        let r = self.call(
            "conversations.open",
            Tok::Bot,
            CallBody::form(&[("users", user)]),
        )?;
        r.pointer("/channel/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| WebError::Api("malformed_response".into()))
    }

    pub fn history(&self, channel: &str, oldest: &str) -> Result<Vec<Value>, WebError> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        for _ in 0..MAX_PAGES {
            let mut form = vec![("channel", channel), ("oldest", oldest), ("limit", "200")];
            if !cursor.is_empty() {
                form.push(("cursor", &cursor));
            }
            let r = self.call("conversations.history", Tok::Bot, CallBody::form(&form))?;
            if let Some(msgs) = r.get("messages").and_then(Value::as_array) {
                out.extend(msgs.iter().cloned());
            }
            cursor = next_cursor(&r);
            if cursor.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    /// Exactly one message: `conversations.history` with `latest=<ts>`,
    /// `inclusive=true`, `limit=1`. `None` when that ts is not what came back
    /// (deleted, or not in this conversation).
    pub fn message(&self, channel: &str, ts: &str) -> Result<Option<Value>, WebError> {
        let form = [
            ("channel", channel),
            ("latest", ts),
            ("inclusive", "true"),
            ("limit", "1"),
        ];
        let r = self.call("conversations.history", Tok::Bot, CallBody::form(&form))?;
        Ok(r.get("messages")
            .and_then(Value::as_array)
            .and_then(|m| {
                m.iter()
                    .find(|m| m.get("ts").and_then(Value::as_str) == Some(ts))
            })
            .cloned())
    }

    /// One page (at most 200) of a thread's messages from `oldest` on.
    pub fn replies(
        &self,
        channel: &str,
        thread_ts: &str,
        oldest: &str,
    ) -> Result<Vec<Value>, WebError> {
        let form = [
            ("channel", channel),
            ("ts", thread_ts),
            ("oldest", oldest),
            ("limit", "200"),
        ];
        let r = self.call("conversations.replies", Tok::Bot, CallBody::form(&form))?;
        Ok(r.get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Ids of every conversation the bot is a member of.
    pub fn member_channels(&self) -> Result<Vec<String>, WebError> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        for _ in 0..MAX_PAGES {
            let mut form = vec![
                ("types", "public_channel,private_channel,im,mpim"),
                ("exclude_archived", "true"),
                ("limit", "200"),
            ];
            if !cursor.is_empty() {
                form.push(("cursor", &cursor));
            }
            let r = self.call("users.conversations", Tok::Bot, CallBody::form(&form))?;
            if let Some(chs) = r.get("channels").and_then(Value::as_array) {
                out.extend(
                    chs.iter()
                        .filter_map(|c| c.get("id").and_then(Value::as_str))
                        .map(str::to_string),
                );
            }
            cursor = next_cursor(&r);
            if cursor.is_empty() {
                break;
            }
        }
        Ok(out)
    }
}

enum CallBody {
    Json(Value),
    Form(Vec<(String, String)>),
}

impl CallBody {
    fn form(pairs: &[(&str, &str)]) -> Self {
        CallBody::Form(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }
}

fn next_cursor(r: &Value) -> String {
    r.pointer("/response_metadata/next_cursor")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn str_field(r: &Value, key: &str) -> Result<String, WebError> {
    r.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| WebError::Api("malformed_response".into()))
}

/// Splits `text` into parts of at most [`SLACK_TEXT_LIMIT`] bytes, preferring
/// paragraph then line boundaries and hard-splitting an over-long line on a
/// char boundary that is not inside an `&amp;`/`&lt;`/`&gt;` entity (the
/// executor escapes before it splits). Whitespace-only input yields no parts.
pub fn split_message(text: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, parts: &mut Vec<String>| {
        let t = cur.trim_end();
        if !t.trim().is_empty() {
            parts.push(t.to_string());
        }
        cur.clear();
    };
    for line in text.split_inclusive('\n') {
        if cur.len() + line.len() > SLACK_TEXT_LIMIT {
            // Break at the last blank line in the buffer when it leaves a
            // reasonably full part; otherwise at the line boundary.
            match cur.rfind("\n\n") {
                Some(i) if i + 2 > SLACK_TEXT_LIMIT / 2 => {
                    let rest = cur.split_off(i + 2);
                    flush(&mut cur, &mut parts);
                    cur = rest;
                }
                _ => flush(&mut cur, &mut parts),
            }
        }
        if cur.len() + line.len() <= SLACK_TEXT_LIMIT {
            cur.push_str(line);
            continue;
        }
        // Still too long: the line itself exceeds the limit.
        let mut rest = line;
        while cur.len() + rest.len() > SLACK_TEXT_LIMIT {
            let mut cut = SLACK_TEXT_LIMIT - cur.len();
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            // Never cut an escaped `&amp;` / `&lt;` / `&gt;` in two.
            if let Some(amp) = rest[..cut].rfind('&') {
                if cut - amp < "&amp;".len() && !rest[amp..cut].contains(';') {
                    cut = amp;
                }
            }
            if cut == 0 {
                flush(&mut cur, &mut parts);
                continue;
            }
            cur.push_str(&rest[..cut]);
            rest = &rest[cut..];
            flush(&mut cur, &mut parts);
        }
        cur.push_str(rest);
    }
    flush(&mut cur, &mut parts);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::testutil::FakeHttp;

    const BOT: &str = "xoxb-SECRET123";
    const APP: &str = "xapp-SECRET456";

    fn web(http: Arc<FakeHttp>) -> SlackWeb {
        SlackWeb::new(BOT.into(), APP.into(), http)
    }

    fn api_err(status: Option<u16>, body: &str) -> Arc<FakeHttp> {
        let body = body.to_string();
        FakeHttp::new(move |req| Err(HttpError::new(status, body.clone(), &req.url)))
    }

    fn all_forms(e: &WebError) -> String {
        format!("{e} {e:?}")
    }

    #[test]
    fn post_sends_bearer_and_json_body() {
        let http = FakeHttp::ok(json!({"ok": true, "ts": "1.2"}));
        let ts = web(http.clone())
            .post("C1", Some("9.9"), "hi", None)
            .unwrap();
        assert_eq!(ts, "1.2");
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls[0].url, "https://slack.com/api/chat.postMessage");
        assert_eq!(calls[0].token.as_deref(), Some(BOT));
        assert_eq!(
            calls[0].json_body,
            Some(json!({"channel": "C1", "thread_ts": "9.9", "text": "hi"}))
        );
    }

    #[test]
    fn post_without_thread_omits_thread_ts_and_includes_blocks() {
        let http = FakeHttp::ok(json!({"ok": true, "ts": "1.2"}));
        let blocks = json!([{"type": "divider"}]);
        web(http.clone())
            .post("C1", None, "hi", Some(&blocks))
            .unwrap();
        let body = http.calls.lock().unwrap()[0].json_body.clone().unwrap();
        assert!(body.get("thread_ts").is_none());
        assert_eq!(body["blocks"], blocks);
    }

    #[test]
    fn socket_url_uses_the_app_token_and_everything_else_the_bot_token() {
        let http = FakeHttp::ok(
            json!({"ok": true, "url": "wss://x", "user_id": "U1", "team": "T",
            "channel": {"id": "D1"}, "messages": [], "channels": []}),
        );
        let w = web(http.clone());
        assert_eq!(w.open_socket_url().unwrap(), "wss://x");
        w.auth_test().unwrap();
        w.open_dm("U2").unwrap();
        w.history("C1", "0").unwrap();
        w.update("C1", "1.2", "t", None).unwrap();
        w.react("C1", "1.2", "eyes").unwrap();
        w.unreact("C1", "1.2", "eyes").unwrap();
        let calls = http.calls.lock().unwrap();
        let toks: Vec<_> = calls
            .iter()
            .map(|c| (c.url.rsplit('/').next().unwrap(), c.token.clone().unwrap()))
            .collect();
        assert_eq!(
            toks,
            vec![
                ("apps.connections.open", APP.to_string()),
                ("auth.test", BOT.to_string()),
                ("conversations.open", BOT.to_string()),
                ("conversations.history", BOT.to_string()),
                ("chat.update", BOT.to_string()),
                ("reactions.add", BOT.to_string()),
                ("reactions.remove", BOT.to_string()),
            ]
        );
    }

    #[test]
    fn auth_test_and_open_dm_parse_their_fields() {
        let http = FakeHttp::new(|req| {
            Ok(if req.url.ends_with("auth.test") {
                json!({"ok": true, "user_id": "UBOT", "team": "Acme"})
            } else {
                json!({"ok": true, "channel": {"id": "D42"}})
            })
        });
        let w = web(http);
        assert_eq!(
            w.auth_test().unwrap(),
            AuthInfo {
                bot_user_id: "UBOT".into(),
                team: "Acme".into()
            }
        );
        assert_eq!(w.open_dm("U1").unwrap(), "D42");
    }

    #[test]
    fn message_asks_for_exactly_one_message_and_matches_its_ts() {
        let http = FakeHttp::ok(json!({"ok": true, "messages": [{"ts": "5.000000", "text": "x"}]}));
        let w = web(http.clone());
        assert_eq!(w.message("C1", "5.000000").unwrap().unwrap()["text"], "x");
        assert_eq!(
            w.message("C1", "4.000000").unwrap(),
            None,
            "another ts is not it"
        );
        let calls = http.calls.lock().unwrap();
        assert!(calls[0].url.ends_with("conversations.history"));
        let form = calls[0].form.clone().unwrap();
        for pair in [
            ("channel", "C1"),
            ("latest", "5.000000"),
            ("inclusive", "true"),
            ("limit", "1"),
        ] {
            assert!(form.contains(&(pair.0.into(), pair.1.into())), "{pair:?}");
        }
        assert!(!form.iter().any(|(k, _)| k == "oldest"));
    }

    #[test]
    fn replies_reads_the_thread_from_oldest() {
        let http = FakeHttp::ok(json!({"ok": true, "messages": [{"ts": "5"}, {"ts": "6"}]}));
        let w = web(http.clone());
        assert_eq!(w.replies("C1", "5.0", "5.0").unwrap().len(), 2);
        let calls = http.calls.lock().unwrap();
        assert!(calls[0].url.ends_with("conversations.replies"));
        let form = calls[0].form.clone().unwrap();
        assert!(form.contains(&("ts".into(), "5.0".into())));
        assert!(form.contains(&("oldest".into(), "5.0".into())));
    }

    #[test]
    fn history_sends_oldest_and_returns_messages() {
        let http = FakeHttp::ok(json!({"ok": true, "messages": [{"ts": "1"}, {"ts": "2"}]}));
        let msgs = web(http.clone()).history("C1", "5.0").unwrap();
        assert_eq!(msgs.len(), 2);
        let form = http.calls.lock().unwrap()[0].form.clone().unwrap();
        assert!(form.contains(&("channel".into(), "C1".into())));
        assert!(form.contains(&("oldest".into(), "5.0".into())));
    }

    #[test]
    fn invalid_auth_maps_to_auth() {
        let http = FakeHttp::ok(json!({"ok": false, "error": "invalid_auth"}));
        assert_eq!(web(http).post("C1", None, "x", None), Err(WebError::Auth));
    }

    #[test]
    fn http_401_maps_to_auth() {
        let e = web(api_err(Some(401), "")).auth_test().unwrap_err();
        assert_eq!(e, WebError::Auth);
    }

    #[test]
    fn http_429_maps_to_rate_limited_using_a_body_hint_else_five_seconds() {
        let e = web(api_err(
            Some(429),
            r#"{"ok":false,"error":"ratelimited","retry_after":7}"#,
        ))
        .auth_test()
        .unwrap_err();
        assert_eq!(e, WebError::RateLimited { retry_after: 7 });
        let e = web(api_err(Some(429), "slow down"))
            .auth_test()
            .unwrap_err();
        assert_eq!(e, WebError::RateLimited { retry_after: 5 });
        let http = FakeHttp::ok(json!({"ok": false, "error": "ratelimited"}));
        assert_eq!(
            web(http).auth_test().unwrap_err(),
            WebError::RateLimited { retry_after: 5 }
        );
    }

    #[test]
    fn other_api_errors_carry_only_the_code() {
        let http = FakeHttp::ok(json!({"ok": false, "error": "channel_not_found"}));
        assert_eq!(
            web(http).update("C1", "1", "t", None),
            Err(WebError::Api("channel_not_found".into()))
        );
        assert_eq!(
            web(api_err(Some(500), "boom")).auth_test().unwrap_err(),
            WebError::Api("http_500".into())
        );
    }

    #[test]
    fn missing_scope_is_its_own_variant_with_the_scope_name() {
        let http = FakeHttp::ok(
            json!({"ok": false, "error": "missing_scope", "needed": "im:read", "provided": "chat:write"}),
        );
        let e = web(http).member_channels().unwrap_err();
        assert_eq!(e, WebError::MissingScope("im:read".into()));
    }

    #[test]
    fn member_channels_follows_cursors_and_asks_for_every_type() {
        let http = FakeHttp::new(|req| {
            let form = req.form.clone().unwrap();
            let cursor = form
                .iter()
                .find(|(k, _)| k == "cursor")
                .map(|(_, v)| v.as_str());
            Ok(match cursor {
                None => json!({"ok": true, "channels": [{"id": "C1"}, {"id": "D1"}],
                    "response_metadata": {"next_cursor": "abc"}}),
                Some("abc") => json!({"ok": true, "channels": [{"id": "G1"}],
                    "response_metadata": {"next_cursor": ""}}),
                Some(_) => json!({"ok": false, "error": "bad_cursor"}),
            })
        });
        let ids = web(http.clone()).member_channels().unwrap();
        assert_eq!(ids, vec!["C1", "D1", "G1"]);
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].url, "https://slack.com/api/users.conversations");
        let form = calls[0].form.clone().unwrap();
        assert!(form.contains(&(
            "types".into(),
            "public_channel,private_channel,im,mpim".into()
        )));
    }

    #[test]
    fn member_channels_stops_at_the_page_cap_on_an_endless_cursor() {
        let http = FakeHttp::ok(json!({"ok": true, "channels": [{"id": "C1"}],
            "response_metadata": {"next_cursor": "again"}}));
        let ids = web(http.clone()).member_channels().unwrap();
        assert_eq!(ids.len(), http.calls.lock().unwrap().len());
        assert!(ids.len() <= MAX_PAGES && ids.len() > 1);
    }

    #[test]
    fn no_error_ever_shows_a_token() {
        let leaky = format!("{BOT} {APP}");
        let mut errors = Vec::new();
        // Transport failure whose detail and URL echo the token.
        let net = FakeHttp::new(move |_| {
            Err(HttpError::new(
                None,
                format!("connect failed for {BOT}"),
                format!("https://slack.com/api/x?token={BOT}"),
            ))
        });
        errors.push(web(net).auth_test().unwrap_err());
        // Slack echoing the token in its error code, with and without a status.
        let echo = json!({"ok": false, "error": format!("bad token {BOT}"), "needed": BOT});
        errors.push(web(FakeHttp::ok(echo.clone())).auth_test().unwrap_err());
        errors.push(
            web(FakeHttp::ok(
                json!({"ok": false, "error": "missing_scope", "needed": BOT}),
            ))
            .auth_test()
            .unwrap_err(),
        );
        errors.push(
            web(api_err(Some(400), &echo.to_string()))
                .auth_test()
                .unwrap_err(),
        );
        errors.push(web(api_err(Some(429), &leaky)).auth_test().unwrap_err());
        errors.push(web(api_err(Some(401), &leaky)).auth_test().unwrap_err());
        assert_eq!(errors.len(), 6);
        for e in &errors {
            let s = all_forms(e);
            assert!(!s.contains("SECRET"), "{s}");
        }
        // Debug of the client itself.
        let w = web(FakeHttp::ok(json!({})));
        assert!(!format!("{w:?}").contains("SECRET"));
    }

    #[test]
    fn message_text_is_not_echoed_in_errors() {
        let http = FakeHttp::ok(json!({"ok": false, "error": "msg_too_long"}));
        let e = web(http)
            .post("C1", None, "my-private-message-body", None)
            .unwrap_err();
        assert!(!all_forms(&e).contains("my-private-message-body"));
    }

    #[test]
    fn writes_are_attempted_once_and_reads_keep_the_default_retries() {
        let http = FakeHttp::ok(
            json!({"ok": true, "ts": "1.2", "url": "wss://x", "user_id": "U1",
            "channel": {"id": "D1"}, "messages": [], "channels": []}),
        );
        let w = web(http.clone());
        w.post("C1", None, "hi", None).unwrap();
        w.update("C1", "1.2", "t", None).unwrap();
        w.react("C1", "1.2", "eyes").unwrap();
        w.unreact("C1", "1.2", "eyes").unwrap();
        w.history("C1", "0").unwrap();
        w.member_channels().unwrap();
        w.auth_test().unwrap();
        w.open_socket_url().unwrap();
        let calls = http.calls.lock().unwrap();
        let attempts: Vec<_> = calls
            .iter()
            .map(|c| {
                (
                    c.url.rsplit('/').next().unwrap().to_string(),
                    c.max_attempts,
                )
            })
            .collect();
        let default = HttpRequest::new("POST", "x").max_attempts;
        assert!(default > 1);
        for (m, n) in attempts {
            let write = matches!(
                m.as_str(),
                "chat.postMessage" | "chat.update" | "reactions.add" | "reactions.remove"
            );
            assert_eq!(n, if write { 1 } else { default }, "{m}");
        }
    }

    #[test]
    fn a_short_text_is_one_part() {
        let t = "a".repeat(100);
        assert_eq!(split_message(&t), vec![t]);
    }

    #[test]
    fn long_line_text_splits_into_bounded_parts_that_keep_every_word() {
        let text: String = (0..1500).map(|i| format!("line number {i}\n")).collect();
        assert!(text.len() > 9000);
        let parts = split_message(&text);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|p| p.len() <= SLACK_TEXT_LIMIT));
        let joined = parts.join("\n");
        assert_eq!(
            joined.split_whitespace().collect::<Vec<_>>(),
            text.split_whitespace().collect::<Vec<_>>()
        );
    }

    #[test]
    fn paragraphs_are_kept_whole_when_they_fit() {
        let para = |c: char| c.to_string().repeat(2000);
        let text = format!("{}\n\n{}\n\n{}", para('a'), para('b'), para('c'));
        let parts = split_message(&text);
        assert_eq!(parts, vec![para('a'), para('b'), para('c')]);
    }

    #[test]
    fn one_unbroken_multibyte_line_is_hard_split_on_char_boundaries() {
        let text = "é世".repeat(2500); // 5000 chars, 12500 bytes
        let parts = split_message(&text);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|p| p.len() <= SLACK_TEXT_LIMIT));
        assert_eq!(parts.concat(), text);
    }

    #[test]
    fn blank_text_has_no_parts() {
        assert!(split_message("  \n\n").is_empty());
    }
}
