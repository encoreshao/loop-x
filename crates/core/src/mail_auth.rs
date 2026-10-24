//! The Inbox Triage side of mail OAuth: turn an inbox's stored refresh token into
//! an access token (`bin/mail_auth.py`'s `get_access_token` / `google_refresh` /
//! `ms_refresh`). The interactive sign-in (browser redirect, device code, pending
//! states) is not here: it lives with the OAuth module.
//!
//! Refresh tokens are kept behind [`RefreshTokenStore`]. The production store is
//! the secret store of [`crate::keychain`] under the `loop-engineering.mail`
//! service (account = inbox name, secret = the raw refresh token - the same
//! layout the Python used, so tokens survive the migration). That store is the
//! macOS Keychain, or a 0600 file store under `LOOPX_KEYCHAIN=file:<dir>`, which
//! every test and sandbox sets: nothing here can reach the real Keychain then.

use crate::inbox_config;
use crate::keychain::{self, SecretStore};
use crate::mail_http::{self, MailError, Request};
use serde_json::{Map, Value};

pub const KEYCHAIN_SERVICE: &str = "loop-engineering.mail";
pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const MS_TOKEN_URL: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/token";
pub const GMAIL_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";
/// `Mail.Send` is deliberately never requested: an Outlook token issued here
/// cannot send mail at all.
pub const OUTLOOK_SCOPE: &str = "offline_access https://graph.microsoft.com/Mail.ReadWrite";

/// Where the per-inbox refresh tokens live.
pub trait RefreshTokenStore {
    fn get(&self, inbox_name: &str) -> Result<Option<String>, MailError>;
    fn set(&self, inbox_name: &str, token: &str) -> Result<(), MailError>;
    fn delete(&self, inbox_name: &str) -> Result<(), MailError>;
}

/// [`RefreshTokenStore`] over a [`SecretStore`].
pub struct SecretTokenStore(pub Box<dyn SecretStore>);

fn keychain_err(e: keychain::SecretError) -> MailError {
    MailError::Keychain(e.to_string())
}

impl RefreshTokenStore for SecretTokenStore {
    fn get(&self, inbox_name: &str) -> Result<Option<String>, MailError> {
        self.0
            .get(inbox_name)
            .map(|t| t.filter(|t| !t.trim().is_empty()))
            .map_err(keychain_err)
    }

    fn set(&self, inbox_name: &str, token: &str) -> Result<(), MailError> {
        self.0.set(inbox_name, token).map_err(keychain_err)
    }

    fn delete(&self, inbox_name: &str) -> Result<(), MailError> {
        self.0.delete(inbox_name).map_err(keychain_err)
    }
}

/// The store for this process: see the module docs.
pub fn default_store() -> Result<SecretTokenStore, MailError> {
    keychain::open_store(KEYCHAIN_SERVICE)
        .map(SecretTokenStore)
        .map_err(keychain_err)
}

/// Token endpoints; tests and sandboxes point them at a fake server with
/// `LOOPX_MAIL_GOOGLE_TOKEN_URL` / `LOOPX_MAIL_MS_TOKEN_URL`.
#[derive(Debug, Clone)]
pub struct TokenEndpoints {
    pub google: String,
    pub microsoft: String,
}

impl Default for TokenEndpoints {
    fn default() -> Self {
        Self {
            google: GOOGLE_TOKEN_URL.into(),
            microsoft: MS_TOKEN_URL.into(),
        }
    }
}

impl TokenEndpoints {
    pub fn from_env() -> Self {
        let pick = |var: &str, default: &str| {
            std::env::var(var)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Self {
            google: pick("LOOPX_MAIL_GOOGLE_TOKEN_URL", GOOGLE_TOKEN_URL),
            microsoft: pick("LOOPX_MAIL_MS_TOKEN_URL", MS_TOKEN_URL),
        }
    }
}

/// The `error` field of an OAuth error body (`""` when absent or not JSON).
fn oauth_error(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default()
}

fn refresh_failure(who: &str, e: MailError) -> MailError {
    match e.http() {
        Some(h) if matches!(h.status, Some(400) | Some(401)) => {
            let detail = Some(oauth_error(&h.body))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| h.status.map(|s| s.to_string()).unwrap_or_default());
            MailError::Reauth(format!("{who} refresh failed ({detail})"))
        }
        _ => e,
    }
}

fn access_token_of(tokens: &Value) -> Result<String, MailError> {
    tokens
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| MailError::Other("KeyError: 'access_token'".into()))
}

/// A new Google access token. 400/401 mean the grant is gone: sign in again.
pub fn google_refresh(
    refresh_token: &str,
    client: &Value,
    token_url: &str,
) -> Result<String, MailError> {
    let get = |k: &str| {
        client
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let (id, secret) = (get("client_id"), get("client_secret"));
    let req = Request::new("POST", token_url).form(&[
        ("refresh_token", refresh_token),
        ("client_id", &id),
        ("client_secret", &secret),
        ("grant_type", "refresh_token"),
    ]);
    let tokens = mail_http::request_json(&req).map_err(|e| refresh_failure("Google", e))?;
    access_token_of(&tokens)
}

/// `(access_token, rotated refresh token if any)`; Microsoft rotates on every refresh.
pub fn ms_refresh(
    refresh_token: &str,
    client_id: &str,
    token_url: &str,
) -> Result<(String, Option<String>), MailError> {
    let req = Request::new("POST", token_url).form(&[
        ("grant_type", "refresh_token"),
        ("client_id", client_id),
        ("refresh_token", refresh_token),
        ("scope", OUTLOOK_SCOPE),
    ]);
    let tokens = mail_http::request_json(&req).map_err(|e| refresh_failure("Microsoft", e))?;
    let new_refresh = tokens
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Ok((access_token_of(&tokens)?, new_refresh))
}

/// A fresh access token for `inbox` (`{"name", "provider", ...}`): read its
/// refresh token, exchange it with the app credentials in `oauth`
/// (`mail_oauth.json`), and persist a rotated refresh token.
pub fn get_access_token(
    inbox: &Value,
    oauth: &Map<String, Value>,
    store: &dyn RefreshTokenStore,
    endpoints: &TokenEndpoints,
) -> Result<String, MailError> {
    let name = inbox
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(refresh_token) = store.get(name)? else {
        return Err(MailError::Reauth(format!("{name} is not connected")));
    };
    if inbox.get("provider").and_then(Value::as_str) == Some("gmail") {
        let client = oauth.get("google").cloned().unwrap_or(Value::Null);
        let has = |k: &str| {
            client
                .get(k)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
        };
        if !has("client_id") || !has("client_secret") {
            return Err(MailError::Reauth(
                "Google OAuth client is not configured".into(),
            ));
        }
        return google_refresh(&refresh_token, &client, &endpoints.google);
    }
    let client_id = oauth
        .get("microsoft")
        .and_then(|m| m.get("client_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| MailError::Reauth("Microsoft OAuth client is not configured".into()))?;
    let (access, rotated) = ms_refresh(&refresh_token, client_id, &endpoints.microsoft)?;
    if let Some(new) = rotated.filter(|n| *n != refresh_token) {
        store.set(name, &new)?;
    }
    Ok(access)
}

/// [`get_access_token`] with the production defaults: `mail_oauth.json` from the
/// loop-engineering home, the process's token store and endpoints.
pub fn get_access_token_default(inbox: &Value) -> Result<String, MailError> {
    let oauth = inbox_config::load_oauth(&inbox_config::default_oauth_path());
    get_access_token(
        inbox,
        &oauth,
        &default_store()?,
        &TokenEndpoints::from_env(),
    )
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// An in-memory [`RefreshTokenStore`].
    #[derive(Default)]
    pub struct MemTokens(pub Mutex<HashMap<String, String>>);

    impl MemTokens {
        pub fn with(name: &str, token: &str) -> Self {
            let s = Self::default();
            s.0.lock().unwrap().insert(name.into(), token.into());
            s
        }
    }

    impl RefreshTokenStore for MemTokens {
        fn get(&self, n: &str) -> Result<Option<String>, MailError> {
            Ok(self.0.lock().unwrap().get(n).cloned())
        }
        fn set(&self, n: &str, t: &str) -> Result<(), MailError> {
            self.0.lock().unwrap().insert(n.into(), t.into());
            Ok(())
        }
        fn delete(&self, n: &str) -> Result<(), MailError> {
            self.0.lock().unwrap().remove(n);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::MemTokens;
    use super::*;
    use crate::mail_http::testserver::Stub;
    use serde_json::json;

    fn oauth() -> Map<String, Value> {
        json!({"google": {"client_id": "gid", "client_secret": "gsecret"}, "microsoft": {"client_id": "mid"}})
            .as_object()
            .unwrap()
            .clone()
    }

    fn endpoints(stub: &Stub) -> TokenEndpoints {
        TokenEndpoints {
            google: format!("{}/g/token", stub.base_url),
            microsoft: format!("{}/ms/token", stub.base_url),
        }
    }

    fn gmail() -> Value {
        json!({"name": "w", "provider": "gmail"})
    }

    fn outlook() -> Value {
        json!({"name": "o", "provider": "outlook"})
    }

    #[test]
    fn google_refresh_posts_the_grant_and_returns_the_access_token() {
        let stub = Stub::new();
        stub.add(
            "POST",
            "/g/token",
            200,
            Some(json!({"access_token": "at1"})),
        );
        let t = get_access_token(
            &gmail(),
            &oauth(),
            &MemTokens::with("w", "rt"),
            &endpoints(&stub),
        )
        .unwrap();
        assert_eq!(t, "at1");
        let body = &stub.requests()[0].body;
        for needle in [
            "refresh_token=rt",
            "client_id=gid",
            "client_secret=gsecret",
            "grant_type=refresh_token",
        ] {
            assert!(body.contains(needle), "{body}");
        }
    }

    #[test]
    fn google_rejection_means_reauth() {
        let stub = Stub::new();
        stub.add(
            "POST",
            "/g/token",
            400,
            Some(json!({"error": "invalid_grant"})),
        );
        let e = get_access_token(
            &gmail(),
            &oauth(),
            &MemTokens::with("w", "rt"),
            &endpoints(&stub),
        )
        .unwrap_err();
        assert_eq!(
            e,
            MailError::Reauth("Google refresh failed (invalid_grant)".into())
        );
        let stub = Stub::new();
        stub.add("POST", "/g/token", 401, None);
        let e = get_access_token(
            &gmail(),
            &oauth(),
            &MemTokens::with("w", "rt"),
            &endpoints(&stub),
        )
        .unwrap_err();
        assert_eq!(e, MailError::Reauth("Google refresh failed (401)".into()));
    }

    #[test]
    fn a_server_error_is_not_a_reauth() {
        let stub = Stub::new();
        stub.add("POST", "/g/token", 403, Some(json!({})));
        let e = get_access_token(
            &gmail(),
            &oauth(),
            &MemTokens::with("w", "rt"),
            &endpoints(&stub),
        )
        .unwrap_err();
        assert!(e.is_http());
    }

    #[test]
    fn a_missing_token_or_client_means_reauth() {
        let stub = Stub::new();
        let e = get_access_token(&gmail(), &oauth(), &MemTokens::default(), &endpoints(&stub))
            .unwrap_err();
        assert_eq!(e, MailError::Reauth("w is not connected".into()));
        let e = get_access_token(
            &gmail(),
            &Map::new(),
            &MemTokens::with("w", "rt"),
            &endpoints(&stub),
        )
        .unwrap_err();
        assert_eq!(
            e,
            MailError::Reauth("Google OAuth client is not configured".into())
        );
        let e = get_access_token(
            &outlook(),
            &Map::new(),
            &MemTokens::with("o", "rt"),
            &endpoints(&stub),
        )
        .unwrap_err();
        assert_eq!(
            e,
            MailError::Reauth("Microsoft OAuth client is not configured".into())
        );
        assert!(stub.requests().is_empty());
    }

    #[test]
    fn microsoft_rotation_is_persisted_and_scope_has_no_send() {
        let stub = Stub::new();
        stub.add(
            "POST",
            "/ms/token",
            200,
            Some(json!({"access_token": "at2", "refresh_token": "rt2"})),
        );
        let store = MemTokens::with("o", "rt1");
        assert_eq!(
            get_access_token(&outlook(), &oauth(), &store, &endpoints(&stub)).unwrap(),
            "at2"
        );
        assert_eq!(store.get("o").unwrap().as_deref(), Some("rt2"));
        let body = &stub.requests()[0].body;
        assert!(body.contains("Mail.ReadWrite") && !body.contains("Mail.Send"));
        assert!(body.contains("client_id=mid") && body.contains("refresh_token=rt1"));
    }

    #[test]
    fn microsoft_without_rotation_leaves_the_stored_token_alone() {
        let stub = Stub::new();
        stub.add(
            "POST",
            "/ms/token",
            200,
            Some(json!({"access_token": "at2"})),
        );
        let store = MemTokens::with("o", "rt1");
        get_access_token(&outlook(), &oauth(), &store, &endpoints(&stub)).unwrap();
        assert_eq!(store.get("o").unwrap().as_deref(), Some("rt1"));
    }

    #[test]
    fn a_reply_without_an_access_token_is_an_error() {
        let stub = Stub::new();
        stub.add("POST", "/g/token", 200, Some(json!({})));
        assert!(get_access_token(
            &gmail(),
            &oauth(),
            &MemTokens::with("w", "rt"),
            &endpoints(&stub)
        )
        .is_err());
    }

    #[test]
    fn the_file_store_backs_the_default_token_store() {
        // LOOPX_KEYCHAIN=file:<dir> keeps every test off the real Keychain.
        let dir = tempfile::tempdir().unwrap();
        let service = keychain::sandboxed_service(KEYCHAIN_SERVICE);
        let store = SecretTokenStore(Box::new(keychain::FileStore::new(dir.path(), &service)));
        assert_eq!(store.get("w").unwrap(), None);
        store.set("w", "rt").unwrap();
        assert_eq!(store.get("w").unwrap().as_deref(), Some("rt"));
        store.delete("w").unwrap();
        assert_eq!(store.get("w").unwrap(), None);
    }

    #[test]
    fn endpoints_default_to_the_real_providers() {
        let d = TokenEndpoints::default();
        assert_eq!(
            (d.google.as_str(), d.microsoft.as_str()),
            (GOOGLE_TOKEN_URL, MS_TOKEN_URL)
        );
    }
}
