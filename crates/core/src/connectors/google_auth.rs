//! The Google sign-in plumbing shared by the Google connectors (Calendar,
//! Slides): the saved OAuth client, refresh-token -> access-token exchange, and
//! scrubbing of tokens out of anything shown. No message built here ever
//! contains the refresh token or an access token.

use super::http::{HttpClient, HttpError, HttpRequest, REDACTED};
use super::{describe_http_error, ConnectorError};
use serde_json::{json, Value};
use std::sync::Arc;

/// What a connector's Test says when Google rejected its refresh token: the
/// sign-in is dead until the person reconnects (`auth_health` alerts at once).
pub const REAUTH_MESSAGE: &str = "Google sign-in expired or was revoked - click Reconnect";

pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Why a refresh failed.
#[derive(Debug, Clone, PartialEq)]
pub enum RefreshError {
    /// The grant is gone: the user must sign in again (`ReauthRequired`).
    Reauth,
    Http(HttpError),
    /// Anything else; only the class name is ever shown.
    Other(String),
}

/// The OAuth client (`{client_id, client_secret}`) saved on the Inbox Triage page.
pub type ClientFn = Arc<dyn Fn() -> Value + Send + Sync>;
/// `(refresh_token, client)` -> access token.
pub type RefreshFn = Arc<dyn Fn(&str, &Value) -> Result<String, RefreshError> + Send + Sync>;

/// The Google client from `mail_oauth.json` (`inbox_config.load_oauth()["google"]`, read from `<home>/mail_oauth.json`).
pub fn default_client() -> Value {
    std::fs::read_to_string(crate::config::home_dir().join("mail_oauth.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("google").filter(|g| g.is_object()).cloned())
        .unwrap_or_else(|| json!({}))
}

/// `mail_auth.google_refresh`: exchange the refresh token for an access token.
pub fn google_refresh(
    http: &dyn HttpClient,
    refresh_token: &str,
    client: &Value,
) -> Result<String, RefreshError> {
    let get = |k: &str| {
        client
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let mut req = HttpRequest::new("POST", GOOGLE_TOKEN_URL);
    req.form = Some(vec![
        ("refresh_token".into(), refresh_token.to_string()),
        ("client_id".into(), get("client_id")),
        ("client_secret".into(), get("client_secret")),
        ("grant_type".into(), "refresh_token".into()),
    ]);
    match http.request_json(&req) {
        Ok(tokens) => tokens
            .get("access_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| RefreshError::Other("KeyError".into())),
        Err(e) if matches!(e.status, Some(400) | Some(401)) => Err(RefreshError::Reauth),
        Err(e) => Err(RefreshError::Http(e)),
    }
}

/// One Google account's credentials: its refresh token plus the seams that
/// resolve the OAuth client and exchange the token.
pub struct GoogleAuth {
    pub secret: Option<String>,
    pub client_fn: ClientFn,
    pub refresh_fn: RefreshFn,
}

impl GoogleAuth {
    pub fn new(secret: Option<String>, http: Arc<dyn HttpClient>) -> Self {
        Self {
            secret,
            client_fn: Arc::new(default_client),
            refresh_fn: Arc::new(move |rt, client| google_refresh(http.as_ref(), rt, client)),
        }
    }

    /// Replace each token, then any >=8-char prefix or suffix fragment of one
    /// (what is left when a cut lands inside a token).
    pub fn scrub(&self, text: &str, access_token: Option<&str>) -> String {
        let mut tokens: Vec<String> = [self.secret.as_deref(), access_token]
            .into_iter()
            .flatten()
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect();
        tokens.sort_by_key(|t| std::cmp::Reverse(t.chars().count()));
        let mut out = text.to_string();
        for token in tokens {
            out = out.replace(&token, REDACTED);
            let chars: Vec<char> = token.chars().collect();
            for n in (8..chars.len()).rev() {
                let prefix: String = chars[..n].iter().collect();
                let suffix: String = chars[chars.len() - n..].iter().collect();
                out = out.replace(&prefix, REDACTED).replace(&suffix, REDACTED);
            }
        }
        out
    }

    /// `describe_http_error` with the tokens scrubbed from the body BEFORE its
    /// 200-char truncation, so a token straddling the cut can't survive.
    pub fn describe(&self, err: &ConnectorError, access_token: Option<&str>) -> String {
        let err = match err {
            ConnectorError::Http(e) if !e.auth_expired => ConnectorError::Http(HttpError::new(
                e.status,
                self.scrub(&e.body, access_token),
                &e.url,
            )),
            other => other.clone(),
        };
        self.scrub(&describe_http_error(&err), access_token)
    }

    /// `(token, None)` or `(None, failure message)`.
    pub fn access_token(&self) -> Result<String, String> {
        let Some(secret) = self.secret.as_deref().filter(|s| !s.is_empty()) else {
            return Err("Not connected - click Connect with Google".to_string());
        };
        let client = (self.client_fn)();
        let has = |k: &str| {
            client
                .get(k)
                .and_then(Value::as_str)
                .is_some_and(|v| !v.is_empty())
        };
        if !has("client_id") || !has("client_secret") {
            return Err(
                "Add a Google OAuth client on the Inbox Triage setup page first".to_string(),
            );
        }
        match (self.refresh_fn)(secret, &client) {
            Err(RefreshError::Reauth) => Err(REAUTH_MESSAGE.to_string()),
            Err(RefreshError::Http(e)) => Err(self.describe(&ConnectorError::Http(e), None)),
            Err(RefreshError::Other(class)) => Err(self.describe(
                &ConnectorError::Other {
                    class,
                    detail: String::new(),
                },
                None,
            )),
            Ok(t) if t.is_empty() => {
                Err("Google returned no access token - click Reconnect".to_string())
            }
            Ok(t) => Ok(t),
        }
    }
}
