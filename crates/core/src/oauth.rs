//! OAuth for the connectors: Google (desktop client, loopback redirect + PKCE) and
//! LinkedIn (authorization-code flow). See docs/contract/oauth.md.
//!
//! The pieces:
//! - [`Pending`] states (CSRF `state` + PKCE verifier), kept as 0600 files so the process
//!   that starts a flow (`loopx auth ... start`, or the server's `POST /api/oauth/.../start`)
//!   and the one that receives the redirect (`loopx serve`) need not be the same.
//! - [`start`] / [`complete`]: build the authorization URL, then validate the callback,
//!   exchange the code and store the [`TokenSet`] as one JSON secret per account.
//! - [`access_token`]: the refresh-before-use helper every API caller should go through.
//!
//! Every provider endpoint is injectable ([`Endpoints`], env `LOOPX_OAUTH_<PROVIDER>_AUTH_URL`
//! / `_TOKEN_URL`) so tests talk to a fake OAuth server. Tokens are never logged or put in
//! error messages.

use crate::config::home_dir;
use crate::keychain::{self, SecretError, SecretStore, SERVICE_OAUTH};
use crate::sha256::{base64url, random_bytes, sha256};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const LINKEDIN_AUTH_URL: &str = "https://www.linkedin.com/oauth/v2/authorization";
pub const LINKEDIN_TOKEN_URL: &str = "https://www.linkedin.com/oauth/v2/accessToken";

pub const GMAIL_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";
pub const CALENDAR_SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly \
https://www.googleapis.com/auth/calendar.events";
pub const LINKEDIN_SCOPE: &str = "openid profile email";

/// A pending sign-in state is single use and lives this long.
pub const STATE_TTL_SECONDS: i64 = 600;
/// An access token this close to expiry is refreshed before use.
pub const REFRESH_SKEW_SECONDS: i64 = 60;
pub const DEFAULT_ACCOUNT: &str = "default";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, thiserror::Error)]
pub enum OauthError {
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Secret(#[from] SecretError),
    #[error("{0}")]
    Http(String),
    /// The provider refused the request (bad code, revoked client, ...).
    #[error("{0}")]
    Rejected(String),
    /// The stored grant is gone, expired without a refresh token, or was revoked: sign in again.
    #[error("{0}")]
    ReauthRequired(String),
    #[error("{0}")]
    BadState(String),
    #[error("{0}")]
    Io(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Google,
    Linkedin,
}

impl Provider {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "google" => Some(Self::Google),
            "linkedin" => Some(Self::Linkedin),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Linkedin => "linkedin",
        }
    }

    /// The redirect path this provider's flow uses (registered with the provider).
    pub fn callback_path(self) -> &'static str {
        match self {
            Self::Google => "/oauth/google/callback",
            Self::Linkedin => "/oauth/callback",
        }
    }

    pub fn default_scope(self, kind: &str) -> &'static str {
        match (self, kind) {
            (Self::Google, "inbox") => GMAIL_SCOPE,
            (Self::Google, _) => CALENDAR_SCOPE,
            (Self::Linkedin, _) => LINKEDIN_SCOPE,
        }
    }

    fn env_prefix(self) -> &'static str {
        match self {
            Self::Google => "LOOPX_OAUTH_GOOGLE",
            Self::Linkedin => "LOOPX_OAUTH_LINKEDIN",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub auth_url: String,
    pub token_url: String,
}

impl Endpoints {
    pub fn defaults(p: Provider) -> Self {
        match p {
            Provider::Google => Self {
                auth_url: GOOGLE_AUTH_URL.into(),
                token_url: GOOGLE_TOKEN_URL.into(),
            },
            Provider::Linkedin => Self {
                auth_url: LINKEDIN_AUTH_URL.into(),
                token_url: LINKEDIN_TOKEN_URL.into(),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    pub client_id: String,
    pub client_secret: Option<String>,
}

/// Everything a flow needs, resolved once: where pending states live, which secret
/// store holds the tokens, where the OAuth client ids come from and which endpoints to call.
#[derive(Clone)]
pub struct OauthContext {
    pub dir: PathBuf,
    pub store: Arc<dyn SecretStore>,
    /// `mail_oauth.json` (`{"google": {client_id, client_secret}, "linkedin": {...}}`).
    pub clients_path: PathBuf,
    pub google: Endpoints,
    pub linkedin: Endpoints,
    /// Overrides `http://<authority>` in redirect URIs, e.g. `http://localhost:8420`.
    pub redirect_base: Option<String>,
}

impl OauthContext {
    pub fn new(dir: &Path, store: Arc<dyn SecretStore>, clients_path: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            store,
            clients_path: clients_path.to_path_buf(),
            google: Endpoints::defaults(Provider::Google),
            linkedin: Endpoints::defaults(Provider::Linkedin),
            redirect_base: None,
        }
    }

    /// The real configuration: `<home>/oauth/`, the secret store for [`SERVICE_OAUTH`],
    /// `<home>/mail_oauth.json`, plus the `LOOPX_OAUTH_*` overrides.
    pub fn from_env() -> Result<Self, OauthError> {
        let home = home_dir();
        let store: Arc<dyn SecretStore> = Arc::from(keychain::open_store(SERVICE_OAUTH)?);
        let mut ctx = Self::new(&home.join("oauth"), store, &home.join("mail_oauth.json"));
        for p in [Provider::Google, Provider::Linkedin] {
            let get = |suffix: &str| {
                std::env::var(format!("{}_{suffix}", p.env_prefix()))
                    .ok()
                    .filter(|v| !v.is_empty())
            };
            let ep = match p {
                Provider::Google => &mut ctx.google,
                Provider::Linkedin => &mut ctx.linkedin,
            };
            if let Some(v) = get("AUTH_URL") {
                ep.auth_url = v;
            }
            if let Some(v) = get("TOKEN_URL") {
                ep.token_url = v;
            }
        }
        ctx.redirect_base = std::env::var("LOOPX_OAUTH_REDIRECT_BASE")
            .ok()
            .filter(|v| !v.is_empty());
        Ok(ctx)
    }

    pub fn endpoints(&self, p: Provider) -> &Endpoints {
        match p {
            Provider::Google => &self.google,
            Provider::Linkedin => &self.linkedin,
        }
    }

    /// The registered client for `p`. `client_secret` may itself be a `keychain:` reference.
    pub fn client(&self, p: Provider) -> Result<Client, OauthError> {
        let text = std::fs::read_to_string(&self.clients_path).map_err(|_| {
            OauthError::Config(format!(
                "no {} OAuth client configured ({} is missing)",
                p.name(),
                self.clients_path.display()
            ))
        })?;
        let doc: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let entry = &doc[p.name()];
        let field = |k: &str| entry[k].as_str().filter(|s| !s.is_empty());
        let client_id = field("client_id").ok_or_else(|| {
            OauthError::Config(format!(
                "no {} OAuth client configured: add {{\"{}\": {{\"client_id\": ..., \"client_secret\": ...}}}} to {}",
                p.name(),
                p.name(),
                self.clients_path.display()
            ))
        })?;
        let client_secret = field("client_secret")
            .map(keychain::resolve_value)
            .transpose()?;
        if p == Provider::Linkedin && client_secret.is_none() {
            return Err(OauthError::Config(
                "the LinkedIn OAuth client needs a client_secret".into(),
            ));
        }
        Ok(Client {
            client_id: client_id.to_string(),
            client_secret,
        })
    }

    /// `http://<authority><callback path>` unless `redirect_base` overrides it.
    pub fn redirect_uri(&self, p: Provider, authority: &str) -> String {
        match &self.redirect_base {
            Some(base) => format!("{}{}", base.trim_end_matches('/'), p.callback_path()),
            None => format!("http://{authority}{}", p.callback_path()),
        }
    }
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------- PKCE

/// `(verifier, challenge)` with the S256 method (RFC 7636).
pub fn pkce() -> (String, String) {
    let verifier = base64url(&random_bytes(48)); // 64 chars, within 43..=128
    let challenge = base64url(&sha256(verifier.as_bytes()));
    (verifier, challenge)
}

// ------------------------------------------------------------ pending states

#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub provider: Provider,
    /// `connector` (a connector account's sign-in) or `inbox` (an Inbox Triage mailbox).
    pub kind: String,
    /// Connector id or inbox name; the token is stored under `<provider>/<account>`.
    pub account: String,
    pub verifier: String,
    pub redirect_uri: String,
    /// Custom URL scheme the success page hands control back to (ASWebAuthenticationSession).
    pub app_scheme: Option<String>,
    pub created: i64,
}

impl Pending {
    fn to_value(&self) -> Value {
        json!({
            "provider": self.provider.name(), "kind": self.kind, "account": self.account,
            "verifier": self.verifier, "redirect_uri": self.redirect_uri,
            "app_scheme": self.app_scheme, "created": self.created,
        })
    }

    fn from_value(v: &Value) -> Option<Self> {
        Some(Self {
            provider: Provider::parse(v["provider"].as_str()?)?,
            kind: v["kind"].as_str()?.to_string(),
            account: v["account"].as_str()?.to_string(),
            verifier: v["verifier"].as_str()?.to_string(),
            redirect_uri: v["redirect_uri"].as_str()?.to_string(),
            app_scheme: v["app_scheme"].as_str().map(str::to_string),
            created: v["created"].as_i64()?,
        })
    }
}

/// A state is used as a file name, so it must be plain base64url.
fn valid_state(state: &str) -> bool {
    !state.is_empty()
        && state.len() <= 128
        && state
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn write_private(path: &Path, text: &str) -> Result<(), OauthError> {
    use std::io::Write;
    let io = |e: std::io::Error| OauthError::Io(format!("{}: {e}", path.display()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(io)
}

fn sweep(dir: &Path, now: i64) {
    for sub in ["pending", "results"] {
        let Ok(rd) = std::fs::read_dir(dir.join(sub)) else {
            continue;
        };
        for entry in rd.flatten() {
            let created = std::fs::read_to_string(entry.path())
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| v["created"].as_i64().or(v["at"].as_i64()));
            if created.is_none_or(|c| now - c > STATE_TTL_SECONDS) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Store `pending` under a fresh random state and return the state.
pub fn create_pending(dir: &Path, pending: &Pending) -> Result<String, OauthError> {
    sweep(dir, pending.created);
    let state = base64url(&random_bytes(32));
    write_private(
        &dir.join("pending").join(format!("{state}.json")),
        &pending.to_value().to_string(),
    )?;
    Ok(state)
}

/// The pending entry without consuming it. Expired entries are `None` unless `include_expired`.
pub fn peek_pending(dir: &Path, state: &str, now: i64, include_expired: bool) -> Option<Pending> {
    if !valid_state(state) {
        return None;
    }
    let text = std::fs::read_to_string(dir.join("pending").join(format!("{state}.json"))).ok()?;
    let p = Pending::from_value(&serde_json::from_str(&text).ok()?)?;
    (include_expired || now - p.created <= STATE_TTL_SECONDS).then_some(p)
}

/// Take the entry (single use). Expired or unknown states give `None`; an expired one is removed.
pub fn consume_pending(dir: &Path, state: &str, now: i64) -> Option<Pending> {
    let p = peek_pending(dir, state, now, true)?;
    let _ = std::fs::remove_file(dir.join("pending").join(format!("{state}.json")));
    (now - p.created <= STATE_TTL_SECONDS).then_some(p)
}

fn record_result(dir: &Path, state: &str, ok: bool, message: &str, p: Option<&Pending>, now: i64) {
    if !valid_state(state) {
        return;
    }
    let doc = json!({
        "ok": ok, "message": message, "at": now,
        "provider": p.map(|p| p.provider.name()), "account": p.map(|p| p.account.as_str()),
    });
    let _ = write_private(
        &dir.join("results").join(format!("{state}.json")),
        &doc.to_string(),
    );
}

/// `{"ok", "message", "provider", "account", "at"}` once the callback for `state` was handled.
pub fn flow_result(dir: &Path, state: &str) -> Option<Value> {
    if !valid_state(state) {
        return None;
    }
    let text = std::fs::read_to_string(dir.join("results").join(format!("{state}.json"))).ok()?;
    serde_json::from_str(&text).ok()
}

// -------------------------------------------------------------------- tokens

#[derive(Debug, Clone, PartialEq)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds; `None` when the provider gave no lifetime.
    pub expires_at: Option<i64>,
    pub scope: Option<String>,
}

impl TokenSet {
    pub fn to_json(&self) -> String {
        json!({
            "access_token": self.access_token, "refresh_token": self.refresh_token,
            "expires_at": self.expires_at, "scope": self.scope,
        })
        .to_string()
    }

    pub fn from_json(text: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(text).ok()?;
        Some(Self {
            access_token: v["access_token"].as_str()?.to_string(),
            refresh_token: v["refresh_token"].as_str().map(str::to_string),
            expires_at: v["expires_at"].as_i64(),
            scope: v["scope"].as_str().map(str::to_string),
        })
    }

    pub fn expired(&self, now: i64) -> bool {
        self.expires_at
            .is_some_and(|e| e - REFRESH_SKEW_SECONDS <= now)
    }

    fn from_response(v: &Value, now: i64, previous: Option<&TokenSet>) -> Result<Self, OauthError> {
        let access_token = v["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| OauthError::Rejected("the provider returned no access token".into()))?
            .to_string();
        let expires_in = v["expires_in"]
            .as_i64()
            .or_else(|| v["expires_in"].as_str().and_then(|s| s.parse().ok()));
        Ok(Self {
            access_token,
            // A refresh response often omits the refresh token: keep the one we have.
            refresh_token: v["refresh_token"]
                .as_str()
                .map(str::to_string)
                .or_else(|| previous.and_then(|p| p.refresh_token.clone())),
            expires_at: expires_in.map(|s| now + s),
            scope: v["scope"]
                .as_str()
                .map(str::to_string)
                .or_else(|| previous.and_then(|p| p.scope.clone())),
        })
    }
}

fn token_key(p: Provider, account: &str) -> Result<String, OauthError> {
    if account.is_empty() || account.chars().any(char::is_control) {
        return Err(OauthError::Config(format!("bad account name {account:?}")));
    }
    Ok(format!("{}/{account}", p.name()))
}

pub fn load_tokens(
    ctx: &OauthContext,
    p: Provider,
    account: &str,
) -> Result<Option<TokenSet>, OauthError> {
    Ok(ctx
        .store
        .get(&token_key(p, account)?)?
        .and_then(|t| TokenSet::from_json(&t)))
}

fn save_tokens(
    ctx: &OauthContext,
    p: Provider,
    account: &str,
    t: &TokenSet,
) -> Result<(), OauthError> {
    ctx.store.set(&token_key(p, account)?, &t.to_json())?;
    Ok(())
}

// ---------------------------------------------------------------------- HTTP

const QUERY: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

fn encode(s: &str) -> String {
    utf8_percent_encode(s, QUERY).to_string()
}

fn query_string(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// POST a form; `Err((status, error code))` on a non-2xx. Response bodies are never echoed.
fn post_form(url: &str, form: &[(&str, &str)]) -> Result<Value, (Option<u16>, String)> {
    let agent = ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build();
    match agent
        .post(url)
        .set("Accept", "application/json")
        .send_form(form)
    {
        Ok(resp) => resp
            .into_string()
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()))
            .map_err(|e| (None, format!("unreadable response: {e}"))),
        Err(ureq::Error::Status(code, resp)) => {
            let body: Value = resp
                .into_string()
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or(Value::Null);
            let error = body["error"].as_str().unwrap_or("").to_string();
            Err((Some(code), error))
        }
        Err(e) => Err((None, e.to_string())),
    }
}

// --------------------------------------------------------------------- flows

#[derive(Debug, Default, Clone)]
pub struct StartRequest {
    pub account: String,
    /// `connector` (default) or `inbox`.
    pub kind: String,
    pub scope: Option<String>,
    pub login_hint: Option<String>,
    pub app_scheme: Option<String>,
}

/// The URL the user opens, and the `state` that identifies the flow.
#[derive(Debug, Clone, PartialEq)]
pub struct Started {
    pub authorization_url: String,
    pub state: String,
    pub redirect_uri: String,
}

/// Schemes a page must never be told to navigate to.
pub fn valid_app_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "+-.".contains(c))
        && ![
            "http",
            "https",
            "javascript",
            "data",
            "file",
            "vbscript",
            "about",
            "blob",
        ]
        .contains(&s)
}

/// Where the callback page sends an embedding app (an `ASWebAuthenticationSession` watching
/// `scheme`): `<scheme>://oauth?provider=..&status=ok|error&account=..|message=..`.
pub fn app_redirect_url(
    scheme: &str,
    p: Provider,
    account: Option<&str>,
    error: Option<&str>,
) -> String {
    let mut params = vec![("provider", p.name())];
    match error {
        None => params.extend([("status", "ok"), ("account", account.unwrap_or(""))]),
        Some(m) => params.extend([("status", "error"), ("message", m)]),
    }
    format!("{scheme}://oauth?{}", query_string(&params))
}

/// Mint the pending state and build the provider's authorization URL.
/// `authority` is the `host:port` the callback will arrive on (see [`OauthContext::redirect_uri`]).
pub fn start(
    ctx: &OauthContext,
    p: Provider,
    req: &StartRequest,
    authority: &str,
    now: i64,
) -> Result<Started, OauthError> {
    let kind = if req.kind.is_empty() {
        "connector"
    } else {
        &req.kind
    };
    if !["connector", "inbox"].contains(&kind) {
        return Err(OauthError::Config(format!("unknown kind {kind:?}")));
    }
    let account = if req.account.is_empty() {
        DEFAULT_ACCOUNT
    } else {
        &req.account
    };
    token_key(p, account)?;
    if let Some(s) = &req.app_scheme {
        if !valid_app_scheme(s) {
            return Err(OauthError::Config(format!("unusable app_scheme {s:?}")));
        }
    }
    let client = ctx.client(p)?;
    let redirect_uri = ctx.redirect_uri(p, authority);
    let (verifier, challenge) = pkce();
    let state = create_pending(
        &ctx.dir,
        &Pending {
            provider: p,
            kind: kind.to_string(),
            account: account.to_string(),
            verifier,
            redirect_uri: redirect_uri.clone(),
            app_scheme: req.app_scheme.clone(),
            created: now,
        },
    )?;
    let scope = req
        .scope
        .as_deref()
        .unwrap_or_else(|| p.default_scope(kind));
    let mut params = vec![
        ("client_id", client.client_id.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("response_type", "code"),
        ("scope", scope),
        ("state", state.as_str()),
    ];
    if p == Provider::Google {
        params.extend([
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ]);
        if let Some(h) = req.login_hint.as_deref().filter(|h| !h.is_empty()) {
            params.push(("login_hint", h));
        }
    }
    let base = &ctx.endpoints(p).auth_url;
    let sep = if base.contains('?') { '&' } else { '?' };
    Ok(Started {
        authorization_url: format!("{base}{sep}{}", query_string(&params)),
        state,
        redirect_uri,
    })
}

pub fn exchange_code(
    ctx: &OauthContext,
    p: Provider,
    code: &str,
    pending: &Pending,
    now: i64,
) -> Result<TokenSet, OauthError> {
    let client = ctx.client(p)?;
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", pending.redirect_uri.as_str()),
        ("client_id", client.client_id.as_str()),
    ];
    if let Some(secret) = client.client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    if p == Provider::Google {
        form.push(("code_verifier", pending.verifier.as_str()));
    }
    let resp =
        post_form(&ctx.endpoints(p).token_url, &form).map_err(|(status, error)| match status {
            Some(code) => OauthError::Rejected(format!(
                "{} rejected the sign-in ({})",
                p.name(),
                if error.is_empty() {
                    code.to_string()
                } else {
                    error
                }
            )),
            None => OauthError::Http(format!("cannot reach {}: {error}", p.name())),
        })?;
    let tokens = TokenSet::from_response(&resp, now, None)?;
    if p == Provider::Google && tokens.refresh_token.is_none() {
        return Err(OauthError::Rejected(
            "Google returned no refresh token - remove the app's access at \
             myaccount.google.com/permissions and connect again"
                .into(),
        ));
    }
    Ok(tokens)
}

/// Exchange the refresh token for a new access token and store the result.
/// A refused refresh (400/401) means the grant is gone: [`OauthError::ReauthRequired`].
pub fn refresh(
    ctx: &OauthContext,
    p: Provider,
    account: &str,
    now: i64,
) -> Result<TokenSet, OauthError> {
    let current = load_tokens(ctx, p, account)?
        .ok_or_else(|| OauthError::ReauthRequired(format!("{} is not connected", p.name())))?;
    let refresh_token = current.refresh_token.clone().ok_or_else(|| {
        OauthError::ReauthRequired(format!(
            "the {} sign-in expired and has no refresh token - connect again",
            p.name()
        ))
    })?;
    let client = ctx.client(p)?;
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.as_str()),
        ("client_id", client.client_id.as_str()),
    ];
    if let Some(secret) = client.client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    let resp =
        post_form(&ctx.endpoints(p).token_url, &form).map_err(|(status, error)| match status {
            Some(400) | Some(401) => OauthError::ReauthRequired(format!(
                "{} refresh failed ({}) - connect again",
                p.name(),
                if error.is_empty() { "rejected" } else { &error }
            )),
            Some(code) => OauthError::Http(format!("{} refresh failed (HTTP {code})", p.name())),
            None => OauthError::Http(format!("cannot reach {}: {error}", p.name())),
        })?;
    let fresh = TokenSet::from_response(&resp, now, Some(&current))?;
    save_tokens(ctx, p, account, &fresh)?;
    Ok(fresh)
}

/// A usable access token for `account`, refreshing it first when it is (nearly) expired.
/// This is the call API clients should make before every request.
pub fn access_token(
    ctx: &OauthContext,
    p: Provider,
    account: &str,
    now: i64,
) -> Result<String, OauthError> {
    let tokens = load_tokens(ctx, p, account)?
        .ok_or_else(|| OauthError::ReauthRequired(format!("{} is not connected", p.name())))?;
    if !tokens.expired(now) {
        return Ok(tokens.access_token);
    }
    Ok(refresh(ctx, p, account, now)?.access_token)
}

/// What a finished callback tells the page.
#[derive(Debug, Clone, PartialEq)]
pub struct Completed {
    pub provider: Provider,
    pub account: String,
    pub app_scheme: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompleteFailure {
    /// Safe to show: no code, state or token in it.
    pub message: String,
    pub app_scheme: Option<String>,
}

/// Handle the redirect for `state`: validate it, exchange the code and store the tokens.
/// `expected` is the provider the callback path belongs to.
pub fn complete(
    ctx: &OauthContext,
    expected: Provider,
    state: &str,
    code: Option<&str>,
    error: Option<&str>,
    now: i64,
) -> Result<Completed, CompleteFailure> {
    let fail = |message: String, app_scheme: Option<String>| CompleteFailure {
        message,
        app_scheme,
    };
    let Some(pending) = consume_pending(&ctx.dir, state, now) else {
        return Err(fail(
            "That sign-in link expired or was already used - start the sign-in again".into(),
            None,
        ));
    };
    let scheme = pending.app_scheme.clone();
    let outcome = (|| {
        if pending.provider != expected {
            return Err(
                "That sign-in belongs to a different provider - start it again".to_string(),
            );
        }
        if let Some(e) = error.filter(|e| !e.is_empty()) {
            return Err(format!("{} sign-in was cancelled ({e})", expected.name()));
        }
        let code = code
            .filter(|c| !c.is_empty())
            .ok_or_else(|| "The provider sent no authorization code".to_string())?;
        let tokens =
            exchange_code(ctx, expected, code, &pending, now).map_err(|e| e.to_string())?;
        save_tokens(ctx, expected, &pending.account, &tokens).map_err(|e| e.to_string())?;
        Ok(())
    })();
    match outcome {
        Ok(()) => {
            record_result(&ctx.dir, state, true, "connected", Some(&pending), now);
            Ok(Completed {
                provider: expected,
                account: pending.account,
                app_scheme: scheme,
            })
        }
        Err(message) => {
            record_result(&ctx.dir, state, false, &message, Some(&pending), now);
            Err(fail(message, scheme))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub account: String,
    pub connected: bool,
    pub has_refresh_token: bool,
    pub expires_at: Option<i64>,
    pub expired: bool,
}

pub fn status(
    ctx: &OauthContext,
    p: Provider,
    account: &str,
    now: i64,
) -> Result<Status, OauthError> {
    let t = load_tokens(ctx, p, account)?;
    Ok(Status {
        account: account.to_string(),
        connected: t.is_some(),
        has_refresh_token: t.as_ref().is_some_and(|t| t.refresh_token.is_some()),
        expires_at: t.as_ref().and_then(|t| t.expires_at),
        expired: t.as_ref().is_some_and(|t| t.expired(now)),
    })
}

/// Accounts with stored tokens for `p`.
pub fn accounts(ctx: &OauthContext, p: Provider) -> Result<Vec<String>, OauthError> {
    let prefix = format!("{}/", p.name());
    Ok(ctx
        .store
        .list()?
        .into_iter()
        .filter_map(|a| a.strip_prefix(&prefix).map(str::to_string))
        .collect())
}

/// Forget the tokens (the grant stays valid at the provider until revoked there).
pub fn logout(ctx: &OauthContext, p: Provider, account: &str) -> Result<(), OauthError> {
    ctx.store.delete(&token_key(p, account)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MemoryStore;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    /// A tiny token endpoint: answers each request from `replies` (status, body) in order
    /// and records the form bodies it received.
    struct FakeToken {
        url: String,
        bodies: Arc<Mutex<Vec<String>>>,
    }

    fn fake_token(replies: Vec<(u16, Value)>) -> FakeToken {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = bodies.clone();
        std::thread::spawn(move || {
            for (status, body) in replies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut buf = vec![0u8; len];
                reader.read_exact(&mut buf).unwrap();
                seen.lock().unwrap().push(String::from_utf8(buf).unwrap());
                let text = body.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        FakeToken { url, bodies }
    }

    fn ctx(dir: &Path, token_url: &str) -> OauthContext {
        std::fs::write(
            dir.join("mail_oauth.json"),
            r#"{"google":{"client_id":"cid","client_secret":"csecret"},"linkedin":{"client_id":"lid","client_secret":"lsecret"}}"#,
        )
        .unwrap();
        let mut c = OauthContext::new(
            &dir.join("oauth"),
            Arc::new(MemoryStore::new("t")),
            &dir.join("mail_oauth.json"),
        );
        c.google = Endpoints {
            auth_url: "http://127.0.0.1:1/auth".into(),
            token_url: token_url.into(),
        };
        c.linkedin = c.google.clone();
        c
    }

    fn param(url: &str, key: &str) -> String {
        let q = url.split_once('?').unwrap().1;
        q.split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
            .map(|v| {
                percent_encoding::percent_decode_str(v)
                    .decode_utf8_lossy()
                    .into_owned()
            })
            .unwrap_or_default()
    }

    #[test]
    fn pkce_verifier_and_challenge_match_s256() {
        let (v, c) = pkce();
        assert!((43..=128).contains(&v.len()));
        assert_eq!(c, base64url(&sha256(v.as_bytes())));
        assert_ne!(pkce().0, v);
    }

    #[test]
    fn start_builds_the_google_url_and_a_single_use_state() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path(), "http://127.0.0.1:1/token");
        let s = start(
            &c,
            Provider::Google,
            &StartRequest {
                account: "cal".into(),
                login_hint: Some("me@x.com".into()),
                ..Default::default()
            },
            "127.0.0.1:8420",
            1000,
        )
        .unwrap();
        let u = &s.authorization_url;
        assert_eq!(param(u, "client_id"), "cid");
        assert_eq!(
            param(u, "redirect_uri"),
            "http://127.0.0.1:8420/oauth/google/callback"
        );
        assert_eq!(param(u, "code_challenge_method"), "S256");
        assert_eq!(param(u, "access_type"), "offline");
        assert_eq!(param(u, "state"), s.state);
        assert!(param(u, "scope").contains("calendar.readonly"));
        let pending = peek_pending(&c.dir, &s.state, 1000, false).unwrap();
        assert_eq!(
            param(u, "code_challenge"),
            base64url(&sha256(pending.verifier.as_bytes()))
        );
        assert!(consume_pending(&c.dir, &s.state, 1001).is_some());
        assert!(consume_pending(&c.dir, &s.state, 1002).is_none());
    }

    #[test]
    fn linkedin_uses_the_plain_callback_and_no_pkce() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path(), "http://127.0.0.1:1/token");
        let s = start(
            &c,
            Provider::Linkedin,
            &StartRequest::default(),
            "127.0.0.1:8420",
            5,
        )
        .unwrap();
        assert_eq!(
            param(&s.authorization_url, "redirect_uri"),
            "http://127.0.0.1:8420/oauth/callback"
        );
        assert_eq!(param(&s.authorization_url, "code_challenge"), "");
        assert_eq!(param(&s.authorization_url, "scope"), LINKEDIN_SCOPE);
        let mut c2 = c.clone();
        c2.redirect_base = Some("http://localhost:9000/".into());
        assert_eq!(
            c2.redirect_uri(Provider::Linkedin, "x"),
            "http://localhost:9000/oauth/callback"
        );
    }

    #[test]
    fn states_expire_and_reject_path_tricks() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path(), "http://127.0.0.1:1/token");
        let s = start(&c, Provider::Google, &StartRequest::default(), "h:1", 0).unwrap();
        assert!(peek_pending(&c.dir, &s.state, STATE_TTL_SECONDS + 1, false).is_none());
        assert!(peek_pending(&c.dir, &s.state, STATE_TTL_SECONDS + 1, true).is_some());
        assert!(consume_pending(&c.dir, &s.state, STATE_TTL_SECONDS + 1).is_none());
        assert!(peek_pending(&c.dir, "../../etc/passwd", 0, true).is_none());
        assert!(
            !valid_app_scheme("javascript")
                && !valid_app_scheme("HTTP")
                && valid_app_scheme("loopx-auth")
        );
    }

    #[test]
    fn complete_stores_tokens_and_refresh_happens_before_use() {
        let d = tempfile::tempdir().unwrap();
        let fake = fake_token(vec![
            (
                200,
                json!({"access_token":"AT1","refresh_token":"RT1","expires_in":3600,"scope":"s"}),
            ),
            (200, json!({"access_token":"AT2","expires_in":3600})),
        ]);
        let c = ctx(d.path(), &fake.url);
        let s = start(
            &c,
            Provider::Google,
            &StartRequest {
                account: "cal".into(),
                ..Default::default()
            },
            "h:1",
            100,
        )
        .unwrap();
        let done = complete(&c, Provider::Google, &s.state, Some("the-code"), None, 110).unwrap();
        assert_eq!(done.account, "cal");
        assert_eq!(flow_result(&c.dir, &s.state).unwrap()["ok"], true);
        // The state cannot be replayed.
        assert!(complete(&c, Provider::Google, &s.state, Some("the-code"), None, 111).is_err());

        let form = fake.bodies.lock().unwrap()[0].clone();
        assert!(
            form.contains("grant_type=authorization_code") && form.contains("code_verifier="),
            "{form}"
        );

        // Fresh token: no network call.
        assert_eq!(
            access_token(&c, Provider::Google, "cal", 200).unwrap(),
            "AT1"
        );
        assert_eq!(fake.bodies.lock().unwrap().len(), 1);
        // Expired: refreshed, the refresh token is kept, the new token is persisted.
        assert_eq!(
            access_token(&c, Provider::Google, "cal", 110 + 3600).unwrap(),
            "AT2"
        );
        let stored = load_tokens(&c, Provider::Google, "cal").unwrap().unwrap();
        assert_eq!(stored.refresh_token.as_deref(), Some("RT1"));
        assert_eq!(stored.expires_at, Some(110 + 3600 + 3600));
        assert!(fake.bodies.lock().unwrap()[1].contains("grant_type=refresh_token"));
    }

    #[test]
    fn a_refused_refresh_asks_for_a_new_sign_in() {
        let d = tempfile::tempdir().unwrap();
        let fake = fake_token(vec![(400, json!({"error":"invalid_grant"}))]);
        let c = ctx(d.path(), &fake.url);
        save_tokens(
            &c,
            Provider::Google,
            "a",
            &TokenSet {
                access_token: "old".into(),
                refresh_token: Some("rt".into()),
                expires_at: Some(10),
                scope: None,
            },
        )
        .unwrap();
        let e = access_token(&c, Provider::Google, "a", 1000).unwrap_err();
        assert!(matches!(e, OauthError::ReauthRequired(_)), "{e}");
        assert!(e.to_string().contains("invalid_grant") && !e.to_string().contains("rt"));
        // No stored tokens at all, and an expired token without a refresh token.
        assert!(matches!(
            access_token(&c, Provider::Google, "none", 0),
            Err(OauthError::ReauthRequired(_))
        ));
        save_tokens(
            &c,
            Provider::Linkedin,
            "a",
            &TokenSet {
                access_token: "x".into(),
                refresh_token: None,
                expires_at: Some(10),
                scope: None,
            },
        )
        .unwrap();
        assert!(matches!(
            access_token(&c, Provider::Linkedin, "a", 1000),
            Err(OauthError::ReauthRequired(_))
        ));
    }

    #[test]
    fn complete_reports_cancel_wrong_provider_and_missing_refresh_token() {
        let d = tempfile::tempdir().unwrap();
        let fake = fake_token(vec![(200, json!({"access_token":"AT","expires_in":10}))]);
        let c = ctx(d.path(), &fake.url);
        let req = StartRequest {
            app_scheme: Some("loopx-auth".into()),
            ..Default::default()
        };
        let s = start(&c, Provider::Google, &req, "h:1", 0).unwrap();
        let f = complete(
            &c,
            Provider::Google,
            &s.state,
            None,
            Some("access_denied"),
            1,
        )
        .unwrap_err();
        assert!(f.message.contains("cancelled") && f.message.contains("access_denied"));
        assert_eq!(f.app_scheme.as_deref(), Some("loopx-auth"));
        assert_eq!(flow_result(&c.dir, &s.state).unwrap()["ok"], false);

        let s = start(&c, Provider::Google, &req, "h:1", 0).unwrap();
        assert!(
            complete(&c, Provider::Linkedin, &s.state, Some("c"), None, 1)
                .unwrap_err()
                .message
                .contains("different provider")
        );

        let s = start(&c, Provider::Google, &req, "h:1", 0).unwrap();
        let f = complete(&c, Provider::Google, &s.state, Some("c"), None, 1).unwrap_err();
        assert!(f.message.contains("no refresh token"), "{}", f.message);
        assert!(load_tokens(&c, Provider::Google, DEFAULT_ACCOUNT)
            .unwrap()
            .is_none());
    }

    #[test]
    fn status_accounts_and_logout() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path(), "http://127.0.0.1:1/token");
        assert!(!status(&c, Provider::Google, "a", 0).unwrap().connected);
        save_tokens(
            &c,
            Provider::Google,
            "a",
            &TokenSet {
                access_token: "x".into(),
                refresh_token: Some("r".into()),
                expires_at: Some(100),
                scope: None,
            },
        )
        .unwrap();
        let st = status(&c, Provider::Google, "a", 90).unwrap();
        assert!(st.connected && st.has_refresh_token && st.expired);
        assert_eq!(accounts(&c, Provider::Google).unwrap(), vec!["a"]);
        assert!(accounts(&c, Provider::Linkedin).unwrap().is_empty());
        logout(&c, Provider::Google, "a").unwrap();
        assert!(!status(&c, Provider::Google, "a", 0).unwrap().connected);
    }

    #[test]
    fn a_missing_client_is_a_clear_config_error() {
        let d = tempfile::tempdir().unwrap();
        let mut c = ctx(d.path(), "http://x/token");
        std::fs::write(&c.clients_path, "{}").unwrap();
        let e = start(&c, Provider::Google, &StartRequest::default(), "h:1", 0).unwrap_err();
        assert!(matches!(e, OauthError::Config(_)));
        c.clients_path = d.path().join("nope.json");
        assert!(c.client(Provider::Google).is_err());
    }
}
