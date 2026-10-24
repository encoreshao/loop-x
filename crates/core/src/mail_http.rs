//! The one HTTP helper every Inbox Triage mail call goes through (OAuth token
//! endpoints, Gmail, Microsoft Graph). Mirrors `bin/mail_http.py`: JSON in/out,
//! bearer auth, exponential backoff on 429/5xx/network errors (honouring
//! `Retry-After`), and a distinct [`MailError::AuthExpired`] on 401 so callers
//! can refresh once.

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::Value;
use std::io::Read;
use std::time::Duration;

pub const BACKOFF_SECONDS: [u64; 3] = [1, 4, 16];
const RETRY_STATUSES: [u16; 5] = [429, 500, 502, 503, 504];
const MAX_RETRY_AFTER: u64 = 60;

/// Python's `urllib.parse.quote` with `safe=""`: only `A-Za-z0-9_.-~` stay.
const QUOTE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'_')
    .remove(b'.')
    .remove(b'-')
    .remove(b'~');
/// Python's default `quote(value)`: `/` is also safe.
const QUOTE_SAFE_SLASH: &AsciiSet = &QUOTE.remove(b'/');

/// `urllib.parse.quote(s, safe="")`.
pub fn quote(s: &str) -> String {
    utf8_percent_encode(s, QUOTE).to_string()
}

/// `urllib.parse.quote(s)` (default `safe="/"`).
pub fn quote_path(s: &str) -> String {
    utf8_percent_encode(s, QUOTE_SAFE_SLASH).to_string()
}

/// `urllib.parse.urlencode(pairs)` (`quote_plus`: a space is `+`).
pub fn urlencode(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                quote(k).replace("%20", "+"),
                quote(v).replace("%20", "+")
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// `urlencode(..., quote_via=urllib.parse.quote)`: a space is `%20`.
pub fn urlencode_percent(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", quote(k), quote(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// A non-2xx answer (or no answer at all: `status` is `None`).
#[derive(Debug, Clone, PartialEq)]
pub struct MailHttpError {
    pub status: Option<u16>,
    pub body: String,
    pub url: String,
}

fn netloc_and_path(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    rest.to_string()
}

impl std::fmt::Display for MailHttpError {
    /// Host and path only: the query string can carry message ids or tokens.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = self
            .status
            .map_or_else(|| "None".to_string(), |s| s.to_string());
        write!(f, "HTTP {status} from {}", netloc_and_path(&self.url))
    }
}

/// Everything a mail call can fail with. [`MailError::Http`] and
/// [`MailError::AuthExpired`] are what Python's `MailHTTPError` (and its
/// `AuthExpired` subclass) were; the rest are `mail_auth`'s and "anything else".
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MailError {
    #[error("{0}")]
    Http(MailHttpError),
    /// HTTP 401 (a `MailHttpError` too: [`MailError::is_http`] is true).
    #[error("{0}")]
    AuthExpired(MailHttpError),
    /// The refresh token is missing or was rejected: sign in again.
    #[error("{0}")]
    Reauth(String),
    #[error("{0}")]
    Keychain(String),
    /// Anything unexpected (an undecodable reply, a bad header, ...).
    #[error("{0}")]
    Other(String),
}

impl MailError {
    /// `isinstance(exc, MailHTTPError)`.
    pub fn is_http(&self) -> bool {
        matches!(self, MailError::Http(_) | MailError::AuthExpired(_))
    }

    pub fn http(&self) -> Option<&MailHttpError> {
        match self {
            MailError::Http(e) | MailError::AuthExpired(e) => Some(e),
            _ => None,
        }
    }

    pub fn is_auth_expired(&self) -> bool {
        matches!(self, MailError::AuthExpired(_))
    }

    /// The Python exception class name, for the few places that log it.
    pub fn class_name(&self) -> &'static str {
        match self {
            MailError::Http(_) => "MailHTTPError",
            MailError::AuthExpired(_) => "AuthExpired",
            MailError::Reauth(_) => "ReauthRequired",
            MailError::Keychain(_) => "KeychainError",
            MailError::Other(_) => "Exception",
        }
    }
}

pub fn retry_delay(retry_after: Option<&str>, attempt: u32) -> Duration {
    if let Some(n) = retry_after.and_then(|v| v.trim().parse::<i64>().ok()) {
        return Duration::from_secs((n.max(0) as u64).min(MAX_RETRY_AFTER));
    }
    let i = (attempt.saturating_sub(1) as usize).min(BACKOFF_SECONDS.len() - 1);
    Duration::from_secs(BACKOFF_SECONDS[i])
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub token: Option<String>,
    pub json_body: Option<Value>,
    pub form: Option<Vec<(String, String)>>,
    pub headers: Vec<(String, String)>,
    pub max_attempts: u32,
    pub timeout: Duration,
}

impl Request {
    pub fn new(method: &str, url: &str) -> Self {
        Self {
            method: method.into(),
            url: url.into(),
            token: None,
            json_body: None,
            form: None,
            headers: Vec::new(),
            max_attempts: 3,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn token(mut self, token: &str) -> Self {
        if !token.is_empty() {
            self.token = Some(token.into());
        }
        self
    }

    pub fn json(mut self, body: Value) -> Self {
        self.json_body = Some(body);
        self
    }

    pub fn form(mut self, pairs: &[(&str, &str)]) -> Self {
        self.form = Some(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        self
    }

    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }

    pub fn headers(mut self, hs: &[(&str, &str)]) -> Self {
        for (k, v) in hs {
            self.headers.push((k.to_string(), v.to_string()));
        }
        self
    }

    pub fn attempts(mut self, n: u32) -> Self {
        self.max_attempts = n;
        self
    }
}

/// Send `req`, retrying 429/5xx and network errors; `sleep` is injectable.
pub fn request_json_with(req: &Request, sleep: &dyn Fn(Duration)) -> Result<Value, MailError> {
    let agent = ureq::AgentBuilder::new().timeout(req.timeout).build();
    let (body, content_type): (Option<Vec<u8>>, Option<&str>) = if let Some(j) = &req.json_body {
        (Some(j.to_string().into_bytes()), Some("application/json"))
    } else if let Some(form) = &req.form {
        let pairs: Vec<(&str, String)> =
            form.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        (
            Some(urlencode(&pairs).into_bytes()),
            Some("application/x-www-form-urlencoded"),
        )
    } else {
        (None, None)
    };
    for attempt in 1..=req.max_attempts.max(1) {
        let mut r = agent
            .request(&req.method, &req.url)
            .set("Accept", "application/json");
        if let Some(ct) = content_type {
            r = r.set("Content-Type", ct);
        }
        if let Some(t) = &req.token {
            r = r.set("Authorization", &format!("Bearer {t}"));
        }
        for (k, v) in &req.headers {
            r = r.set(k, v);
        }
        let sent = match &body {
            Some(b) => r.send_bytes(b),
            None => r.call(),
        };
        match sent {
            Ok(resp) => {
                let mut raw = Vec::new();
                resp.into_reader()
                    .take(32 * 1024 * 1024)
                    .read_to_end(&mut raw)
                    .map_err(|e| MailError::Other(format!("reading the reply failed: {e}")))?;
                if raw.is_empty() {
                    return Ok(Value::Object(Default::default()));
                }
                return serde_json::from_slice(&raw)
                    .map_err(|e| MailError::Other(format!("reply is not JSON: {e}")));
            }
            Err(ureq::Error::Status(code, resp)) => {
                let retry_after = resp.header("Retry-After").map(str::to_owned);
                let body = resp.into_string().unwrap_or_default();
                let err = MailHttpError {
                    status: Some(code),
                    body,
                    url: req.url.clone(),
                };
                if code == 401 {
                    return Err(MailError::AuthExpired(err));
                }
                if RETRY_STATUSES.contains(&code) && attempt < req.max_attempts {
                    sleep(retry_delay(retry_after.as_deref(), attempt));
                    continue;
                }
                return Err(MailError::Http(err));
            }
            Err(ureq::Error::Transport(t)) => {
                if attempt < req.max_attempts {
                    sleep(retry_delay(None, attempt));
                    continue;
                }
                return Err(MailError::Http(MailHttpError {
                    status: None,
                    body: t.to_string(),
                    url: req.url.clone(),
                }));
            }
        }
    }
    unreachable!("the loop returns on its last attempt")
}

pub fn request_json(req: &Request) -> Result<Value, MailError> {
    request_json_with(req, &std::thread::sleep)
}

#[cfg(test)]
pub(crate) mod testserver {
    //! A real local HTTP stub, like `tests/mail_stub.py`: replays queued JSON
    //! responses per `(method, path)` (the last one repeats) and records requests.
    use serde_json::Value;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone)]
    pub struct Recorded {
        pub method: String,
        pub path: String,
        pub query: String,
        pub headers: HashMap<String, String>,
        pub body: String,
    }

    type Route = (u16, Option<Value>, Vec<(String, String)>);

    #[derive(Default)]
    struct State {
        routes: HashMap<(String, String), Vec<Route>>,
        requests: Vec<Recorded>,
    }

    pub struct Stub {
        pub base_url: String,
        state: Arc<Mutex<State>>,
    }

    impl Stub {
        pub fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let state: Arc<Mutex<State>> = Arc::default();
            let shared = state.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { return };
                    let shared = shared.clone();
                    std::thread::spawn(move || handle(stream, shared));
                }
            });
            Self { base_url, state }
        }

        pub fn add(&self, method: &str, path: &str, status: u16, body: Option<Value>) -> &Self {
            self.add_with(method, path, status, body, &[])
        }

        pub fn add_with(
            &self,
            method: &str,
            path: &str,
            status: u16,
            body: Option<Value>,
            headers: &[(&str, &str)],
        ) -> &Self {
            let hs = headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            self.state
                .lock()
                .unwrap()
                .routes
                .entry((method.into(), path.into()))
                .or_default()
                .push((status, body, hs));
            self
        }

        pub fn requests(&self) -> Vec<Recorded> {
            self.state.lock().unwrap().requests.clone()
        }

        pub fn bodies(&self, method: &str, path: &str) -> Vec<String> {
            self.requests()
                .into_iter()
                .filter(|r| r.method == method && r.path == path)
                .map(|r| r.body)
                .collect()
        }
    }

    fn handle(stream: std::net::TcpStream, state: Arc<Mutex<State>>) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("").to_string();
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let (path, query) = (path.to_string(), query.to_string());
        let mut headers = HashMap::new();
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.insert(k.trim().to_lowercase(), v.trim().to_string());
            }
        }
        let len: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut raw = vec![0u8; len];
        let _ = reader.read_exact(&mut raw);
        let (status, body, extra) = {
            let mut st = state.lock().unwrap();
            st.requests.push(Recorded {
                method: method.clone(),
                path: path.clone(),
                query,
                headers,
                body: String::from_utf8_lossy(&raw).into_owned(),
            });
            match st.routes.get_mut(&(method, path)) {
                None => (404, Some(serde_json::json!({"error": "no stub"})), vec![]),
                Some(q) if q.len() > 1 => q.remove(0),
                Some(q) => q[0].clone(),
            }
        };
        let payload = body.map(|b| b.to_string()).unwrap_or_default();
        let mut out = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            payload.len()
        );
        for (k, v) in extra {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        out.push_str(&payload);
        let mut s = stream;
        let _ = s.write_all(out.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::testserver::Stub;
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    #[test]
    fn json_get_sends_bearer_and_accept() {
        let stub = Stub::new();
        stub.add("GET", "/x", 200, Some(json!({"ok": true})));
        let out = request_json(&Request::new("GET", &format!("{}/x", stub.base_url)).token("tok"))
            .unwrap();
        assert_eq!(out, json!({"ok": true}));
        let r = &stub.requests()[0];
        assert_eq!(r.headers["authorization"], "Bearer tok");
        assert_eq!(r.headers["accept"], "application/json");
    }

    #[test]
    fn json_body_sets_content_type() {
        let stub = Stub::new();
        stub.add("POST", "/x", 200, Some(json!({})));
        request_json(&Request::new("POST", &format!("{}/x", stub.base_url)).json(json!({"a": 1})))
            .unwrap();
        let r = &stub.requests()[0];
        assert_eq!(r.headers["content-type"], "application/json");
        assert_eq!(
            serde_json::from_str::<Value>(&r.body).unwrap(),
            json!({"a": 1})
        );
    }

    #[test]
    fn form_body_is_urlencoded() {
        let stub = Stub::new();
        stub.add("POST", "/t", 200, Some(json!({})));
        request_json(
            &Request::new("POST", &format!("{}/t", stub.base_url))
                .form(&[("a", "b c"), ("d", "é")]),
        )
        .unwrap();
        let r = &stub.requests()[0];
        assert_eq!(
            r.headers["content-type"],
            "application/x-www-form-urlencoded"
        );
        assert_eq!(r.body, "a=b+c&d=%C3%A9");
    }

    #[test]
    fn empty_body_is_an_empty_object() {
        let stub = Stub::new();
        stub.add("DELETE", "/x", 204, None);
        let out = request_json(&Request::new("DELETE", &format!("{}/x", stub.base_url))).unwrap();
        assert_eq!(out, json!({}));
    }

    #[test]
    fn retries_5xx_then_succeeds_with_backoff() {
        let stub = Stub::new();
        stub.add("GET", "/x", 503, Some(json!({})))
            .add("GET", "/x", 500, Some(json!({})))
            .add("GET", "/x", 200, Some(json!({"ok": 1})));
        let sleeps = RefCell::new(Vec::new());
        let out = request_json_with(
            &Request::new("GET", &format!("{}/x", stub.base_url)),
            &|d| sleeps.borrow_mut().push(d),
        )
        .unwrap();
        assert_eq!(out, json!({"ok": 1}));
        assert_eq!(
            *sleeps.borrow(),
            vec![Duration::from_secs(1), Duration::from_secs(4)]
        );
    }

    #[test]
    fn retry_after_header_is_honoured_and_capped() {
        let stub = Stub::new();
        stub.add_with("GET", "/x", 429, Some(json!({})), &[("Retry-After", "7")])
            .add_with("GET", "/x", 429, Some(json!({})), &[("Retry-After", "900")])
            .add("GET", "/x", 200, Some(json!({})));
        let sleeps = RefCell::new(Vec::new());
        request_json_with(
            &Request::new("GET", &format!("{}/x", stub.base_url)),
            &|d| sleeps.borrow_mut().push(d),
        )
        .unwrap();
        assert_eq!(
            *sleeps.borrow(),
            vec![Duration::from_secs(7), Duration::from_secs(60)]
        );
    }

    #[test]
    fn gives_up_after_max_attempts_with_the_last_status() {
        let stub = Stub::new();
        stub.add("GET", "/x", 500, Some(json!({"e": 1})));
        let err = request_json_with(
            &Request::new("GET", &format!("{}/x", stub.base_url)),
            &|_| {},
        )
        .unwrap_err();
        assert_eq!(err.http().unwrap().status, Some(500));
        assert!(!err.is_auth_expired() && err.is_http());
        assert_eq!(stub.requests().len(), 3);
    }

    #[test]
    fn four_hundred_is_not_retried() {
        let stub = Stub::new();
        stub.add("GET", "/x", 404, Some(json!({"e": "gone"})));
        let err = request_json_with(
            &Request::new("GET", &format!("{}/x", stub.base_url)),
            &|_| panic!("no sleep expected"),
        )
        .unwrap_err();
        let h = err.http().unwrap();
        assert_eq!(h.status, Some(404));
        assert!(h.body.contains("gone"));
        assert_eq!(stub.requests().len(), 1);
    }

    #[test]
    fn unauthorized_is_auth_expired_and_not_retried() {
        let stub = Stub::new();
        stub.add("GET", "/x", 401, Some(json!({})));
        let err = request_json_with(
            &Request::new("GET", &format!("{}/x", stub.base_url)),
            &|_| panic!("no sleep expected"),
        )
        .unwrap_err();
        assert!(err.is_auth_expired() && err.is_http());
        assert_eq!(stub.requests().len(), 1);
    }

    #[test]
    fn single_attempt_does_not_retry() {
        let stub = Stub::new();
        stub.add("GET", "/x", 500, Some(json!({})));
        request_json_with(
            &Request::new("GET", &format!("{}/x", stub.base_url)).attempts(1),
            &|_| panic!("no sleep expected"),
        )
        .unwrap_err();
        assert_eq!(stub.requests().len(), 1);
    }

    #[test]
    fn network_error_retries_then_reports_no_status() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", l.local_addr().unwrap());
        drop(l);
        let sleeps = RefCell::new(0);
        let err = request_json_with(&Request::new("GET", &url), &|_| *sleeps.borrow_mut() += 1)
            .unwrap_err();
        assert_eq!(err.http().unwrap().status, None);
        assert_eq!(*sleeps.borrow(), 2);
    }

    #[test]
    fn error_message_has_host_and_path_but_no_query() {
        let e = MailHttpError {
            status: Some(500),
            body: String::new(),
            url: "https://gmail.googleapis.com/gmail/v1/users/me/messages?q=secret".into(),
        };
        assert_eq!(
            e.to_string(),
            "HTTP 500 from gmail.googleapis.com/gmail/v1/users/me/messages"
        );
        let n = MailHttpError {
            status: None,
            body: String::new(),
            url: "http://h:1/p".into(),
        };
        assert_eq!(n.to_string(), "HTTP None from h:1/p");
    }

    #[test]
    fn quoting_matches_python() {
        assert_eq!(quote("a b/c@d"), "a%20b%2Fc%40d");
        assert_eq!(quote_path("a b/c@d"), "a%20b/c%40d");
        assert_eq!(
            urlencode(&[("q", "in:inbox is:unread".into()), ("n", "100".into())]),
            "q=in%3Ainbox+is%3Aunread&n=100"
        );
        assert_eq!(urlencode_percent(&[("$f", "a b".into())]), "%24f=a%20b");
        assert_eq!(quote("~._-"), "~._-");
    }

    #[test]
    fn retry_delay_rules() {
        assert_eq!(retry_delay(Some("5"), 1), Duration::from_secs(5));
        assert_eq!(retry_delay(Some("999"), 1), Duration::from_secs(60));
        assert_eq!(retry_delay(Some("soon"), 2), Duration::from_secs(4));
        assert_eq!(retry_delay(None, 1), Duration::from_secs(1));
        assert_eq!(retry_delay(None, 9), Duration::from_secs(16));
    }

    #[test]
    fn undecodable_reply_is_other_not_http() {
        // A 200 with a non-JSON body: Python lets JSONDecodeError escape (not a MailHTTPError).
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", l.local_addr().unwrap());
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut s, _) = l.accept().unwrap();
            let mut b = [0u8; 2048];
            let _ = s.read(&mut b);
            let _ = s
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnot");
        });
        let err = request_json_with(&Request::new("GET", &url), &|_| {}).unwrap_err();
        assert!(!err.is_http());
    }
}
