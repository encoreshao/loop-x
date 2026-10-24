//! Connectors: one module per connector type (mirrors `bin/connectors/`).
//!
//! A [`TypeInfo`] declares a type's form fields and capabilities; a
//! [`Connector`] instance wraps one account plus its secret and exposes the
//! operations its type supports (REST `api`, `send`, feed `entries`, calendar
//! events, mailbox search...). Operations a type lacks fail with
//! [`ConnectorError::Unsupported`].
//!
//! Messages are English (the dashboard translates its own copy), and never
//! contain a secret.

pub mod github;
pub mod gitlab;
pub mod google_calendar;
pub mod http;
pub mod jira;
pub mod linear;
pub mod linkedin;
pub mod mailbox;
pub mod notion;
pub mod rss;
pub mod secrets;
pub mod slack;
pub mod telegram;
pub mod webhook;

use crate::loop_notify::{LoadError, NotifyConnector, SendError};
pub use http::{HttpClient, HttpError, HttpRequest, UreqClient};
use serde_json::{Map, Value};
use std::sync::Arc;

pub const ISSUES: &str = "issues";
pub const MERGE_REQUESTS: &str = "merge_requests";
pub const PIPELINES: &str = "pipelines";
pub const NOTIFY: &str = "notify";
pub const FEED: &str = "feed";
pub const MAIL: &str = "mail";
pub const DOCS: &str = "docs";
pub const CALENDAR: &str = "calendar";
pub const SOCIAL: &str = "social";

/// `ConnectorError` and the other failures a connector call can end in.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectorError {
    /// `ConnectorError(message)`: a token-free message meant for display.
    Message(String),
    /// A failed HTTP call (`MailHTTPError` / `AuthExpired`).
    Http(HttpError),
    /// This connector type does not offer the operation.
    Unsupported(String),
    /// Anything else, kept as a Python-style class name plus detail.
    Other { class: String, detail: String },
}

impl ConnectorError {
    pub fn msg(m: impl Into<String>) -> Self {
        ConnectorError::Message(m.into())
    }

    /// Python class name of the equivalent exception (what logs may show:
    /// `str(exc)` can quote URLs and tokens).
    pub fn class_name(&self) -> String {
        match self {
            ConnectorError::Message(_) => "ConnectorError".into(),
            ConnectorError::Http(e) => e.class_name().into(),
            ConnectorError::Unsupported(_) => "AttributeError".into(),
            ConnectorError::Other { class, .. } => class.clone(),
        }
    }

    /// The HTTP status, when this is an HTTP failure that has one.
    pub fn status(&self) -> Option<u16> {
        match self {
            ConnectorError::Http(e) => e.status,
            _ => None,
        }
    }

    /// How a notify result describes it: `HTTP <status>` or the class name.
    pub fn to_send_error(&self) -> SendError {
        match self.status() {
            Some(s) => SendError::Http(s),
            None => SendError::Other(self.class_name()),
        }
    }
}

impl std::fmt::Display for ConnectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectorError::Message(m) => f.write_str(m),
            ConnectorError::Http(e) => write!(f, "{e}"),
            ConnectorError::Unsupported(op) => write!(f, "this connector has no {op}"),
            ConnectorError::Other { class, detail } => write!(f, "{class}: {detail}"),
        }
    }
}

impl std::error::Error for ConnectorError {}

impl From<HttpError> for ConnectorError {
    fn from(e: HttpError) -> Self {
        ConnectorError::Http(e)
    }
}

pub type ConnResult<T> = Result<T, ConnectorError>;

/// One setting field a connector type's form asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    /// `text`, `url`, `email`, `select` or `textarea`.
    pub kind: &'static str,
    pub required: bool,
    pub default: &'static str,
    pub help: &'static str,
    pub options: &'static [&'static str],
    pub placeholder: &'static str,
}

impl Field {
    pub const fn new(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            kind: "text",
            required: true,
            default: "",
            help: "",
            options: &[],
            placeholder: "",
        }
    }
    pub const fn kind(mut self, k: &'static str) -> Self {
        self.kind = k;
        self
    }
    pub const fn optional(mut self) -> Self {
        self.required = false;
        self
    }
    pub const fn default(mut self, d: &'static str) -> Self {
        self.default = d;
        self
    }
    pub const fn help(mut self, h: &'static str) -> Self {
        self.help = h;
        self
    }
    pub const fn options(mut self, o: &'static [&'static str]) -> Self {
        self.options = o;
        self
    }
    pub const fn placeholder(mut self, p: &'static str) -> Self {
        self.placeholder = p;
        self
    }
}

/// A one-click starting point for a type (e.g. a webhook brand).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Preset {
    pub key: &'static str,
    pub label: &'static str,
    pub brand: &'static str,
    pub description: &'static str,
    pub settings: &'static [(&'static str, &'static str)],
    pub docs_url: &'static str,
    pub category: &'static str,
}

/// How a type's secret is obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// A pasted secret (`secret_label`).
    Secret,
    /// A Google refresh token from the dashboard's Connect with Google flow.
    OauthGoogle,
    /// The user's own signed-in browser session (LinkedIn).
    BrowserSession,
}

/// The static description of a connector type (Python class attributes).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TypeInfo {
    pub name: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    pub capabilities: &'static [&'static str],
    pub fields: &'static [Field],
    pub secret_label: Option<&'static str>,
    pub external: bool,
    pub brand: &'static str,
    pub description: &'static str,
    pub docs_url: &'static str,
    pub category: &'static str,
    pub presets: &'static [Preset],
    pub auth: Auth,
}

impl TypeInfo {
    pub fn has_capability(&self, cap: &str) -> bool {
        self.capabilities.contains(&cap)
    }

    /// Form validation errors for `settings`/`secret` (empty = valid).
    pub fn validate(
        &self,
        settings: &Map<String, Value>,
        secret: &str,
        is_new: bool,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        for field in self.fields {
            let value = settings
                .get(field.key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if field.required && value.is_empty() {
                errors.push(format!("{} is required", field.label));
                continue;
            }
            if !value.is_empty()
                && field.kind == "url"
                && !value.starts_with("http://")
                && !value.starts_with("https://")
            {
                errors.push(format!(
                    "{} must start with http:// or https://",
                    field.label
                ));
            }
            if !value.is_empty() && field.kind == "email" && !is_email(value) {
                errors.push(format!("{} must be an email address", field.label));
            }
            if !value.is_empty() && field.kind == "select" && !field.options.contains(&value) {
                errors.push(format!(
                    "{} must be one of: {}",
                    field.label,
                    field.options.join(", ")
                ));
            }
        }
        if let Some(label) = self.secret_label {
            if is_new && secret.trim().is_empty() {
                errors.push(format!("{label} is required"));
            }
        }
        errors
    }
}

fn is_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.contains('@')
        && domain.trim_matches('.').contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

/// Every type, in registry order.
pub fn all_types() -> [&'static TypeInfo; 12] {
    [
        &github::INFO,
        &gitlab::INFO,
        &google_calendar::INFO,
        &jira::INFO,
        &linear::INFO,
        &linkedin::INFO,
        &mailbox::INFO,
        &notion::INFO,
        &rss::INFO,
        &slack::INFO,
        &telegram::INFO,
        &webhook::INFO,
    ]
}

/// `connectors.get_type(name)`.
pub fn get_type(name: &str) -> Option<&'static TypeInfo> {
    all_types().into_iter().find(|t| t.name == name)
}

/// A connector account (`connectors_config` view): never contains a secret.
#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub enabled: bool,
    pub settings: Map<String, Value>,
    /// `native`, `gitlab-config`, `slack-config` or `inboxes`.
    pub managed_by: String,
    pub oauth_connected: bool,
}

impl Account {
    pub fn new(id: &str, kind: &str) -> Self {
        Self {
            id: id.into(),
            kind: kind.into(),
            label: id.into(),
            enabled: true,
            settings: Map::new(),
            managed_by: "native".into(),
            oauth_connected: false,
        }
    }

    pub fn with_setting(mut self, key: &str, value: &str) -> Self {
        self.settings
            .insert(key.into(), Value::String(value.into()));
        self
    }

    pub fn setting(&self, key: &str) -> &str {
        self.settings.get(key).and_then(Value::as_str).unwrap_or("")
    }

    /// The dict `connectors_config` returns (key order as in Python).
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), self.id.clone().into());
        m.insert("type".into(), self.kind.clone().into());
        m.insert("label".into(), self.label.clone().into());
        m.insert("enabled".into(), self.enabled.into());
        m.insert("settings".into(), Value::Object(self.settings.clone()));
        m.insert("managed_by".into(), self.managed_by.clone().into());
        if self.oauth_connected {
            m.insert("oauth_connected".into(), true.into());
        }
        Value::Object(m)
    }
}

/// Options of a REST `api` call.
#[derive(Debug, Clone)]
pub struct ApiOpts {
    pub json_body: Option<Value>,
    pub timeout_secs: u64,
    pub max_attempts: u32,
}

impl Default for ApiOpts {
    fn default() -> Self {
        Self {
            json_body: None,
            timeout_secs: 30,
            max_attempts: 3,
        }
    }
}

impl ApiOpts {
    pub fn json(body: Value) -> Self {
        Self {
            json_body: Some(body),
            ..Self::default()
        }
    }
    pub fn once(mut self) -> Self {
        self.max_attempts = 1;
        self
    }
    pub fn timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }
}

fn unsupported<T>(op: &str) -> ConnResult<T> {
    Err(ConnectorError::Unsupported(op.to_string()))
}

/// One account's live connection. Operations default to "unsupported".
pub trait Connector: Send + Sync {
    fn info(&self) -> &'static TypeInfo;
    fn account(&self) -> &Account;

    /// `(ok, message)`. Never fails and never leaks a secret.
    fn test(&self) -> (bool, String) {
        (
            false,
            format!("Testing is not supported for {}", self.info().label),
        )
    }

    /// Plain text for a notify connector's test message: names the account.
    fn test_message(&self) -> String {
        let a = self.account();
        let name = if a.label == a.id {
            format!("\"{}\"", a.label)
        } else {
            format!("\"{}\" ({})", a.label, a.id)
        };
        format!(
            "Loop X test message from connector {name}. Notifications sent through this connector will appear here."
        )
    }

    /// Notify connectors: send `text` (and Slack blocks where supported).
    fn send(&self, _text: &str, _blocks: Option<&Value>) -> ConnResult<()> {
        unsupported("send")
    }

    /// REST connectors (gitlab, github, jira, notion; linear takes a query
    /// via [`Connector::graphql`]): `method path` with the account's auth.
    fn api(&self, _method: &str, _path: &str, _opts: &ApiOpts) -> ConnResult<Value> {
        unsupported("api")
    }

    /// GitLab: a plain-text endpoint (job trace), LAST 2 MB only.
    fn api_text(&self, _path: &str) -> ConnResult<String> {
        unsupported("api_text")
    }

    /// GitLab: instance base URL without a trailing slash.
    fn base_url(&self) -> ConnResult<String> {
        unsupported("base_url")
    }

    /// Linear: a GraphQL query.
    fn graphql(&self, _query: &str, _variables: Option<&Value>) -> ConnResult<Value> {
        unsupported("graphql")
    }

    /// RSS: the configured feed URLs.
    fn feed_urls(&self) -> Vec<String> {
        Vec::new()
    }

    /// RSS: the entries of one feed (`title`, `link`, `id`, `published`, `summary`).
    fn entries(&self, _url: &str) -> ConnResult<Vec<Value>> {
        unsupported("entries")
    }

    /// Calendar: events between two RFC 3339 instants, in start order.
    fn list_events(
        &self,
        _time_min: &str,
        _time_max: &str,
        _max_results: u32,
    ) -> ConnResult<Vec<Value>> {
        unsupported("list_events")
    }

    /// Calendar: create one event; returns `{id, html_link}`.
    fn create_event(&self, _event: &Value) -> ConnResult<Value> {
        unsupported("create_event")
    }

    /// Mail: search the mailbox.
    fn search_messages(&self, _query: &str, _limit: u32) -> ConnResult<Vec<Value>> {
        unsupported("search_messages")
    }

    /// Mail: read one message.
    fn read_message(&self, _message_id: &str) -> ConnResult<Value> {
        unsupported("read_message")
    }

    /// Mail: send (Gmail only).
    fn send_message(
        &self,
        _to: &[String],
        _cc: &[String],
        _subject: &str,
        _body: &str,
    ) -> ConnResult<Value> {
        unsupported("send_message")
    }
}

/// Instantiate the connector for `account`. `None` for an unknown type.
pub fn build(
    account: Account,
    secret: Option<String>,
    http: Arc<dyn HttpClient>,
) -> Option<Box<dyn Connector>> {
    Some(match account.kind.as_str() {
        "github" => Box::new(github::GitHubConnector::new(account, secret, http)),
        "gitlab" => Box::new(gitlab::GitLabConnector::new(account, secret, http)),
        "google_calendar" => Box::new(google_calendar::GoogleCalendarConnector::new(
            account, secret, http,
        )),
        "jira" => Box::new(jira::JiraConnector::new(account, secret, http)),
        "linear" => Box::new(linear::LinearConnector::new(account, secret, http)),
        "linkedin" => Box::new(linkedin::LinkedInConnector::new(account)),
        "mailbox" => Box::new(mailbox::MailboxConnector::new(account)),
        "notion" => Box::new(notion::NotionConnector::new(account, secret, http)),
        "rss" => Box::new(rss::RssConnector::new(account, http)),
        "slack" => Box::new(slack::SlackConnector::new(account, secret, http)),
        "telegram" => Box::new(telegram::TelegramConnector::new(account, secret, http)),
        "webhook" => Box::new(webhook::WebhookConnector::new(account, secret, http)),
        _ => return None,
    })
}

/// One-line description of a failed probe request (`describe_http_error`).
pub fn describe_http_error(err: &ConnectorError) -> String {
    match err {
        ConnectorError::Http(e) if e.auth_expired => {
            "Token rejected (401) - check it hasn't expired".to_string()
        }
        ConnectorError::Http(e) => match e.status {
            None => format!("Network error: {}", e.body),
            Some(s) => format!("HTTP {s}: {}", e.body.chars().take(200).collect::<String>()),
        },
        // Only the class name: str(exc) can quote request data.
        other => format!("Request failed ({})", other.class_name()),
    }
}

/// Adapts a [`Connector`] to `loop_notify`'s narrower trait.
pub struct NotifyAdapter(pub Box<dyn Connector>);

impl NotifyConnector for NotifyAdapter {
    fn capabilities(&self) -> Vec<String> {
        self.0
            .info()
            .capabilities
            .iter()
            .map(|c| c.to_string())
            .collect()
    }
    fn kind(&self) -> String {
        self.0.info().name.to_string()
    }
    fn send(&self, text: &str, blocks: Option<&Value>) -> Result<(), SendError> {
        self.0.send(text, blocks).map_err(|e| e.to_send_error())
    }
}

/// Why `load_connector` failed (`KeyError`, `ConnectorConfigError`, others).
#[derive(Debug, Clone, PartialEq)]
pub enum LoadFailure {
    Unknown,
    Config(String),
    Other(String),
}

impl From<LoadFailure> for LoadError {
    fn from(f: LoadFailure) -> Self {
        match f {
            LoadFailure::Unknown => LoadError::Unknown,
            LoadFailure::Config(_) => LoadError::Config,
            LoadFailure::Other(class) => LoadError::Other(class),
        }
    }
}

/// What plugins use to find and open connectors (`accounts_with_capability`
/// and `load_connector`); fakes implement it in tests.
pub trait ConnectorSource: Send + Sync {
    fn accounts_with_capability(&self, capability: &str) -> Vec<Account>;
    fn load(&self, account_id: &str) -> Result<Box<dyn Connector>, LoadFailure>;
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::http::{HttpClient, HttpError, HttpRequest};
    use serde_json::Value;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Read one whole HTTP request (headers plus any Content-Length body).
    pub(crate) fn read_request(s: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        let mut data = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = s.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&data).into_owned();
            if let Some(i) = text.find("\r\n\r\n") {
                let len = text[..i]
                    .lines()
                    .find_map(|l| {
                        l.to_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if data.len() >= i + 4 + len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&data).into_owned()
    }

    /// Serialises tests that set process-wide environment variables.
    pub static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub struct Served {
        pub url: String,
        pub requests: Arc<Mutex<Vec<String>>>,
    }

    /// One-thread fake HTTP server answering each connection with the next
    /// canned `(status, extra headers, body)`; records every raw request.
    pub fn serve(responses: Vec<(u16, &'static str, String)>) -> Served {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for (status, headers, body) in responses {
                let Ok((mut s, _)) = listener.accept() else {
                    return;
                };
                let raw = read_request(&mut s);
                seen.lock().unwrap().push(raw);
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Served { url, requests }
    }

    type Handler = dyn Fn(&HttpRequest) -> Result<Value, HttpError> + Send + Sync;

    /// A scripted [`HttpClient`] recording every request it is given.
    pub struct FakeHttp {
        pub calls: Mutex<Vec<HttpRequest>>,
        pub webhooks: Mutex<Vec<(String, Value)>>,
        handler: Box<Handler>,
        pub text: Mutex<Result<String, HttpError>>,
    }

    impl FakeHttp {
        pub fn new(
            handler: impl Fn(&HttpRequest) -> Result<Value, HttpError> + Send + Sync + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                webhooks: Mutex::new(Vec::new()),
                handler: Box::new(handler),
                text: Mutex::new(Ok(String::new())),
            })
        }
        pub fn ok(value: Value) -> Arc<Self> {
            Self::new(move |_| Ok(value.clone()))
        }
    }

    impl HttpClient for FakeHttp {
        fn request_json(&self, req: &HttpRequest) -> Result<Value, HttpError> {
            self.calls.lock().unwrap().push(req.clone());
            (self.handler)(req)
        }
        fn post_webhook(&self, url: &str, body: &Value, _t: Duration) -> Result<(), HttpError> {
            self.webhooks
                .lock()
                .unwrap()
                .push((url.to_string(), body.clone()));
            (self.handler)(&HttpRequest::new("POST", url).json(Some(body.clone())))
                .map(|_| ())
                .map_err(|e| HttpError {
                    url: super::http::REDACTED.into(),
                    ..e
                })
        }
        fn get_text_tail(
            &self,
            _url: &str,
            _h: &[(String, String)],
            _t: Duration,
            _m: usize,
        ) -> Result<String, HttpError> {
            self.text.lock().unwrap().clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn registry_lists_every_type_once() {
        let names: Vec<_> = all_types().iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            [
                "github",
                "gitlab",
                "google_calendar",
                "jira",
                "linear",
                "linkedin",
                "mailbox",
                "notion",
                "rss",
                "slack",
                "telegram",
                "webhook"
            ]
        );
        assert!(get_type("gitlab").is_some() && get_type("nope").is_none());
    }

    #[test]
    fn validate_checks_required_url_email_select_and_secret() {
        const FIELDS: [Field; 3] = [
            Field::new("u", "Site URL").kind("url"),
            Field::new("e", "Email").kind("email"),
            Field::new("s", "Shape")
                .kind("select")
                .options(&["a", "b"])
                .optional(),
        ];
        let info = TypeInfo {
            fields: &FIELDS,
            secret_label: Some("API token"),
            ..gitlab::INFO
        };
        let m = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(
            info.validate(&m(json!({})), "", true),
            [
                "Site URL is required",
                "Email is required",
                "API token is required"
            ]
        );
        assert_eq!(
            info.validate(
                &m(json!({"u": "ftp://x", "e": "nope", "s": "c"})),
                "x",
                true
            ),
            [
                "Site URL must start with http:// or https://",
                "Email must be an email address",
                "Shape must be one of: a, b"
            ]
        );
        assert!(info
            .validate(&m(json!({"u": "https://x", "e": "a@b.co"})), "", false)
            .is_empty());
        assert!(!is_email("a@.b.co") && !is_email("a@b") && !is_email("@b.co"));
    }

    #[test]
    fn describe_http_error_forms() {
        let e = |s, auth| {
            ConnectorError::Http(HttpError {
                auth_expired: auth,
                ..HttpError::new(s, "body", "u")
            })
        };
        assert_eq!(
            describe_http_error(&e(Some(401), true)),
            "Token rejected (401) - check it hasn't expired"
        );
        assert_eq!(describe_http_error(&e(Some(500), false)), "HTTP 500: body");
        assert_eq!(describe_http_error(&e(None, false)), "Network error: body");
        assert_eq!(
            describe_http_error(&ConnectorError::msg("x")),
            "Request failed (ConnectorError)"
        );
    }

    #[test]
    fn errors_map_to_notify_send_errors() {
        let http = ConnectorError::Http(HttpError::new(Some(500), "", ""));
        assert_eq!(http.to_send_error(), SendError::Http(500));
        let net = ConnectorError::Http(HttpError::new(None, "", ""));
        assert_eq!(
            net.to_send_error(),
            SendError::Other("MailHTTPError".into())
        );
        assert_eq!(
            ConnectorError::msg("x").to_send_error(),
            SendError::Other("ConnectorError".into())
        );
    }

    #[test]
    fn account_view_matches_the_python_dict() {
        let mut a = Account::new("g", "gitlab").with_setting("url", "https://x");
        a.managed_by = "gitlab-config".into();
        assert_eq!(
            a.to_value(),
            json!({"id": "g", "type": "gitlab", "label": "g", "enabled": true,
                   "settings": {"url": "https://x"}, "managed_by": "gitlab-config"})
        );
        a.oauth_connected = true;
        assert_eq!(a.to_value()["oauth_connected"], true);
    }
}
