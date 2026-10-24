//! The one HTTP helper every connector call goes through (mirrors
//! `bin/mail_http.py`): JSON in/out, bearer auth, exponential backoff on
//! 429/5xx/network errors (honouring `Retry-After`), and a distinct
//! `auth_expired` flag on 401 so callers can refresh once.
//!
//! [`HttpClient`] is a trait so connector and plugin tests can inject a fake;
//! [`UreqClient`] is the real implementation.

use serde_json::Value;
use std::io::Read;
use std::time::Duration;

pub const BACKOFF_SECONDS: [u64; 3] = [1, 4, 16];
pub const MAX_RETRY_AFTER: u64 = 60;
const RETRY_STATUSES: [u16; 5] = [429, 500, 502, 503, 504];

pub const TEST_TIMEOUT_SECONDS: u64 = 10;
pub const REDACTED: &str = "<redacted>";

/// `MailHTTPError` / `AuthExpired`: `status` is `None` for a network error.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpError {
    pub status: Option<u16>,
    pub body: String,
    pub url: String,
    /// The `AuthExpired` subclass: a 401.
    pub auth_expired: bool,
}

impl HttpError {
    pub fn new(status: Option<u16>, body: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            url: url.into(),
            auth_expired: false,
        }
    }

    /// Python class name, used where only the class may be shown.
    pub fn class_name(&self) -> &'static str {
        if self.auth_expired {
            "AuthExpired"
        } else {
            "MailHTTPError"
        }
    }
}

impl std::fmt::Display for HttpError {
    /// `HTTP <status> from <host><path>` (never the query string).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (host, path) = split_host_path(&self.url);
        match self.status {
            Some(s) => write!(f, "HTTP {s} from {host}{path}"),
            None => write!(f, "HTTP None from {host}{path}"),
        }
    }
}

impl std::error::Error for HttpError {}

fn split_host_path(url: &str) -> (&str, &str) {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    let rest = &rest[..end];
    match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    }
}

/// One JSON request.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub token: Option<String>,
    pub json_body: Option<Value>,
    pub form: Option<Vec<(String, String)>>,
    pub headers: Vec<(String, String)>,
    pub timeout: Duration,
    pub max_attempts: u32,
}

impl HttpRequest {
    pub fn new(method: &str, url: &str) -> Self {
        Self {
            method: method.to_string(),
            url: url.to_string(),
            token: None,
            json_body: None,
            form: None,
            headers: Vec::new(),
            timeout: Duration::from_secs(30),
            max_attempts: 3,
        }
    }
    pub fn token(mut self, token: Option<&str>) -> Self {
        self.token = token.filter(|t| !t.is_empty()).map(str::to_string);
        self
    }
    pub fn json(mut self, body: Option<Value>) -> Self {
        self.json_body = body;
        self
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
    pub fn timeout_secs(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }
    pub fn attempts(mut self, n: u32) -> Self {
        self.max_attempts = n;
        self
    }
}

pub trait HttpClient: Send + Sync {
    /// JSON request with the retry policy above. An empty body is `{}`.
    fn request_json(&self, req: &HttpRequest) -> Result<Value, HttpError>;

    /// POST JSON to a webhook: any 2xx is success (the body is ignored); the
    /// error carries a redacted URL, never the real one (it is the secret).
    fn post_webhook(&self, url: &str, body: &Value, timeout: Duration) -> Result<(), HttpError> {
        let req = HttpRequest {
            timeout,
            max_attempts: 1,
            ..HttpRequest::new("POST", url).json(Some(body.clone()))
        };
        self.request_json(&req).map(|_| ()).map_err(|e| HttpError {
            url: REDACTED.to_string(),
            ..e
        })
    }

    /// GET a plain-text endpoint, returning the LAST `max_bytes` of the body.
    /// Errors carry only the status (the URL and token are never exposed).
    fn get_text_tail(
        &self,
        url: &str,
        headers: &[(String, String)],
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<String, HttpError>;
}

/// The production client (blocking, `ureq`).
#[derive(Clone)]
pub struct UreqClient {
    sleep: fn(Duration),
}

impl Default for UreqClient {
    fn default() -> Self {
        Self {
            sleep: std::thread::sleep,
        }
    }
}

impl UreqClient {
    /// Replace the backoff sleeper (tests).
    pub fn with_sleep(sleep: fn(Duration)) -> Self {
        Self { sleep }
    }
}

/// Delay before retry number `attempt` (1-based): `Retry-After` seconds when
/// it parses as an integer (capped), else the exponential backoff.
pub fn retry_delay(retry_after: Option<&str>, attempt: u32) -> Duration {
    if let Some(n) = retry_after.and_then(|s| s.trim().parse::<i64>().ok()) {
        return Duration::from_secs(n.clamp(0, MAX_RETRY_AFTER as i64) as u64);
    }
    let i = (attempt.saturating_sub(1) as usize).min(BACKOFF_SECONDS.len() - 1);
    Duration::from_secs(BACKOFF_SECONDS[i])
}

pub(crate) fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().build()
}

impl HttpClient for UreqClient {
    fn request_json(&self, req: &HttpRequest) -> Result<Value, HttpError> {
        let data = match (&req.json_body, &req.form) {
            (Some(j), _) => Some((
                "application/json",
                serde_json::to_vec(j).unwrap_or_default(),
            )),
            (None, Some(f)) => Some((
                "application/x-www-form-urlencoded",
                form_encode(f).into_bytes(),
            )),
            _ => None,
        };
        let attempts = req.max_attempts.max(1);
        for attempt in 1..=attempts {
            let mut r = agent()
                .request(&req.method, &req.url)
                .timeout(req.timeout)
                .set("Accept", "application/json");
            if let Some((ct, _)) = &data {
                r = r.set("Content-Type", ct);
            }
            if let Some(t) = &req.token {
                r = r.set("Authorization", &format!("Bearer {t}"));
            }
            for (k, v) in &req.headers {
                r = r.set(k, v);
            }
            let result = match &data {
                Some((_, bytes)) => r.send_bytes(bytes),
                None => r.call(),
            };
            match result {
                Ok(resp) => {
                    let mut raw = String::new();
                    let _ = resp.into_reader().take(64 << 20).read_to_string(&mut raw);
                    if raw.trim().is_empty() {
                        return Ok(Value::Object(Default::default()));
                    }
                    return serde_json::from_str(&raw)
                        .map_err(|e| HttpError::new(None, format!("invalid JSON: {e}"), &req.url));
                }
                Err(ureq::Error::Status(code, resp)) => {
                    let retry_after = resp.header("Retry-After").map(str::to_string);
                    let mut body = String::new();
                    let _ = resp.into_reader().take(1 << 20).read_to_string(&mut body);
                    if code == 401 {
                        return Err(HttpError {
                            auth_expired: true,
                            ..HttpError::new(Some(401), body, &req.url)
                        });
                    }
                    if RETRY_STATUSES.contains(&code) && attempt < attempts {
                        (self.sleep)(retry_delay(retry_after.as_deref(), attempt));
                        continue;
                    }
                    return Err(HttpError::new(Some(code), body, &req.url));
                }
                Err(ureq::Error::Transport(t)) => {
                    if attempt < attempts {
                        (self.sleep)(retry_delay(None, attempt));
                        continue;
                    }
                    return Err(HttpError::new(None, t.to_string(), &req.url));
                }
            }
        }
        unreachable!("max_attempts >= 1")
    }

    fn post_webhook(&self, url: &str, body: &Value, timeout: Duration) -> Result<(), HttpError> {
        let red = |status: Option<u16>, text: &str| HttpError::new(status, text, REDACTED);
        if !(url.to_lowercase().starts_with("http://")
            || url.to_lowercase().starts_with("https://"))
        {
            return Err(red(None, "unsupported URL scheme"));
        }
        let bytes = serde_json::to_vec(body).unwrap_or_default();
        let result = agent()
            .post(url)
            .timeout(timeout)
            .set("Content-Type", "application/json")
            .set("User-Agent", "LoopX/1.0")
            .send_bytes(&bytes);
        match result {
            Ok(resp) if (200..300).contains(&resp.status()) => Ok(()),
            Ok(resp) => Err(red(Some(resp.status()), "")),
            Err(ureq::Error::Status(code, resp)) => {
                let mut excerpt = String::new();
                let _ = resp.into_reader().take(200).read_to_string(&mut excerpt);
                Err(red(Some(code), &excerpt))
            }
            Err(ureq::Error::Transport(t)) => {
                if matches!(
                    t.kind(),
                    ureq::ErrorKind::InvalidUrl | ureq::ErrorKind::UnknownScheme
                ) {
                    Err(red(None, "invalid URL"))
                } else {
                    Err(red(None, "network error"))
                }
            }
        }
    }

    fn get_text_tail(
        &self,
        url: &str,
        headers: &[(String, String)],
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<String, HttpError> {
        let mut r = agent()
            .get(url)
            .timeout(timeout)
            .set("Range", &format!("bytes=-{max_bytes}"));
        for (k, v) in headers {
            r = r.set(k, v);
        }
        match r.call() {
            Ok(resp) => {
                // Keep only a rolling tail, in case the server ignored the Range.
                let mut reader = resp.into_reader();
                let mut tail: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 64 * 1024];
                loop {
                    let n = reader.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    tail.extend_from_slice(&chunk[..n]);
                    if tail.len() > max_bytes + chunk.len() {
                        let cut = tail.len() - max_bytes;
                        tail.drain(..cut);
                    }
                }
                let start = tail.len().saturating_sub(max_bytes);
                Ok(String::from_utf8_lossy(&tail[start..]).into_owned())
            }
            Err(ureq::Error::Status(416, _)) => Ok(String::new()),
            Err(ureq::Error::Status(code, _)) => Err(HttpError::new(Some(code), "", "")),
            Err(ureq::Error::Transport(_)) => Err(HttpError::new(None, "", "")),
        }
    }
}

/// `application/x-www-form-urlencoded` (Python `urllib.parse.urlencode`).
pub fn form_encode(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Python `quote_plus`: unreserved bytes kept, space as `+`, the rest `%XX`.
pub fn quote_plus(s: &str) -> String {
    quote_with(s, true)
}

fn quote_with(s: &str, plus: bool) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(b as char)
            }
            b' ' if plus => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Python `urllib.parse.urlencode` of `(key, value)` pairs.
pub fn urlencode(pairs: &[(&str, String)]) -> String {
    form_encode(
        &pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect::<Vec<_>>(),
    )
}

/// Python `urllib.parse.quote(s, safe="")`.
pub fn quote(s: &str) -> String {
    quote_with(s, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A one-thread fake server: each connection is answered with the next
    /// canned `(status, extra headers, body)`; records each raw request.
    pub struct Fake {
        pub url: String,
        pub requests: Arc<Mutex<Vec<String>>>,
    }

    pub fn serve(responses: Vec<(u16, &'static str, &'static str)>) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for (status, headers, body) in responses {
                let Ok((mut s, _)) = listener.accept() else {
                    return;
                };
                let raw = crate::connectors::testutil::read_request(&mut s);
                seen.lock().unwrap().push(raw);
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Fake { url, requests }
    }

    fn no_sleep(_: Duration) {}

    #[test]
    fn retry_delay_honours_retry_after_and_caps() {
        assert_eq!(retry_delay(Some("5"), 1), Duration::from_secs(5));
        assert_eq!(retry_delay(Some("999"), 1), Duration::from_secs(60));
        assert_eq!(retry_delay(Some("soon"), 2), Duration::from_secs(4));
        assert_eq!(retry_delay(None, 1), Duration::from_secs(1));
        assert_eq!(retry_delay(None, 9), Duration::from_secs(16));
    }

    #[test]
    fn quoting_matches_python() {
        assert_eq!(quote("a b/c~"), "a%20b%2Fc~");
        assert_eq!(quote_plus("a b&c"), "a+b%26c");
        assert_eq!(
            urlencode(&[("q", "x y".into()), ("n", "1".into())]),
            "q=x+y&n=1"
        );
    }

    #[test]
    fn display_hides_query_string() {
        let e = HttpError::new(Some(500), "x", "https://h.io/a/b?token=SECRET");
        assert_eq!(e.to_string(), "HTTP 500 from h.io/a/b");
    }

    #[test]
    fn returns_json_and_sends_bearer_and_headers() {
        let f = serve(vec![(200, "", r#"{"ok": 1}"#)]);
        let req = HttpRequest::new("POST", &format!("{}/x", f.url))
            .token(Some("tok"))
            .header("X-A", "b")
            .json(Some(serde_json::json!({"a": 1})));
        let v = UreqClient::with_sleep(no_sleep).request_json(&req).unwrap();
        assert_eq!(v["ok"], 1);
        let raw = f.requests.lock().unwrap()[0].to_lowercase();
        assert!(raw.contains("authorization: bearer tok"));
        assert!(raw.contains("x-a: b"));
        assert!(raw.contains("content-type: application/json"));
    }

    #[test]
    fn empty_body_is_empty_object() {
        let f = serve(vec![(200, "", "")]);
        let v = UreqClient::with_sleep(no_sleep)
            .request_json(&HttpRequest::new("GET", &f.url))
            .unwrap();
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    fn retries_429_then_succeeds_and_gives_up_after_max_attempts() {
        let f = serve(vec![(429, "Retry-After: 0\r\n", "slow"), (200, "", "{}")]);
        UreqClient::with_sleep(no_sleep)
            .request_json(&HttpRequest::new("GET", &f.url))
            .unwrap();
        assert_eq!(f.requests.lock().unwrap().len(), 2);
        let f = serve(vec![(503, "", "a"), (503, "", "b")]);
        let e = UreqClient::with_sleep(no_sleep)
            .request_json(&HttpRequest::new("GET", &f.url).attempts(2))
            .unwrap_err();
        assert_eq!((e.status, e.body.as_str()), (Some(503), "b"));
    }

    #[test]
    fn a_401_is_auth_expired_and_never_retried() {
        let f = serve(vec![(401, "", "nope"), (200, "", "{}")]);
        let e = UreqClient::with_sleep(no_sleep)
            .request_json(&HttpRequest::new("GET", &f.url))
            .unwrap_err();
        assert!(e.auth_expired);
        assert_eq!(e.class_name(), "AuthExpired");
        assert_eq!(f.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_404_is_not_retried() {
        let f = serve(vec![(404, "", "gone")]);
        let e = UreqClient::with_sleep(no_sleep)
            .request_json(&HttpRequest::new("GET", &f.url))
            .unwrap_err();
        assert_eq!((e.status, e.body.as_str()), (Some(404), "gone"));
    }

    #[test]
    fn network_error_retries_then_reports_none_status() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        let e = UreqClient::with_sleep(no_sleep)
            .request_json(&HttpRequest::new("GET", &url).attempts(2))
            .unwrap_err();
        assert_eq!(e.status, None);
    }

    #[test]
    fn webhook_post_redacts_the_url_in_errors() {
        let f = serve(vec![(500, "", "boom")]);
        let url = format!("{}/secret-path", f.url);
        let e = UreqClient::default()
            .post_webhook(&url, &serde_json::json!({}), Duration::from_secs(5))
            .unwrap_err();
        assert_eq!((e.status, e.url.as_str()), (Some(500), REDACTED));
        assert!(!e.to_string().contains("secret-path"));
        let f = serve(vec![(200, "", "ok")]);
        UreqClient::default()
            .post_webhook(
                &f.url,
                &serde_json::json!({"text": "x"}),
                Duration::from_secs(5),
            )
            .unwrap();
        let e = UreqClient::default()
            .post_webhook("ftp://x", &serde_json::json!({}), Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(e.body, "unsupported URL scheme");
    }

    #[test]
    fn text_tail_keeps_only_the_end() {
        let f = serve(vec![(200, "", "0123456789")]);
        let t = UreqClient::default()
            .get_text_tail(&f.url, &[], Duration::from_secs(5), 4)
            .unwrap();
        assert_eq!(t, "6789");
        assert!(f.requests.lock().unwrap()[0]
            .to_lowercase()
            .contains("range: bytes=-4"));
        let f = serve(vec![(416, "", "")]);
        assert_eq!(
            UreqClient::default()
                .get_text_tail(&f.url, &[], Duration::from_secs(5), 4)
                .unwrap(),
            ""
        );
    }
}
