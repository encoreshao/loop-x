//! Google Sheets connector: sign-in and connection test (the operations arrive
//! with the chat tools). The stored secret is a Google refresh token from the
//! dashboard's Connect with Google flow (`auth = OauthGoogle`); the OAuth client
//! is the one saved on the Inbox Triage setup page. No message built here ever
//! contains a token.

use super::drive;
use super::google_auth::GoogleAuth;
pub use super::google_auth::{ClientFn, RefreshError, RefreshFn};
use super::http::TEST_TIMEOUT_SECONDS;
use super::*;

pub const INFO: TypeInfo = TypeInfo {
    name: "google_sheets",
    label: "Google Sheets",
    icon: "fact_check",
    capabilities: &[SHEETS],
    fields: &[],
    secret_label: None,
    external: false,
    brand: "googlesheets",
    description: "Pick shared spreadsheet templates, create a new sheet from one with your approval in chat, and review it here.",
    docs_url: "https://support.google.com/docs/answer/6000292",
    category: "google",
    presets: &[],
    auth: Auth::OauthGoogle,
};

pub struct GoogleSheetsConnector {
    account: Account,
    http: Arc<dyn HttpClient>,
    auth: GoogleAuth,
}

pub fn web_link(id: &str) -> String {
    format!("https://docs.google.com/spreadsheets/d/{id}/edit")
}

impl GoogleSheetsConnector {
    pub fn new(account: Account, secret: Option<String>, http: Arc<dyn HttpClient>) -> Self {
        Self {
            account,
            auth: GoogleAuth::new(secret, http.clone()),
            http,
        }
    }

    pub fn with_client_fn(mut self, f: ClientFn) -> Self {
        self.auth.client_fn = f;
        self
    }

    pub fn with_refresh_fn(mut self, f: RefreshFn) -> Self {
        self.auth.refresh_fn = f;
        self
    }
}

impl Connector for GoogleSheetsConnector {
    fn info(&self) -> &'static TypeInfo {
        &INFO
    }

    fn account(&self) -> &Account {
        &self.account
    }

    fn test(&self) -> (bool, String) {
        let token = match self.auth.access_token() {
            Ok(t) => t,
            Err(m) => return (false, m),
        };
        let req = HttpRequest::new("GET", &drive::probe_url(drive::SPREADSHEET_MIME))
            .token(Some(&token))
            .timeout_secs(TEST_TIMEOUT_SECONDS)
            .attempts(1);
        match self.http.request_json(&req) {
            Ok(_) => (
                true,
                "Connected: Google Sheets and Drive are reachable".into(),
            ),
            Err(e) => match e.status {
                Some(401) | Some(403) => (
                    false,
                    "Reconnect Google Sheets to grant Sheets and Drive access".into(),
                ),
                _ => (
                    false,
                    self.auth.describe(&ConnectorError::Http(e), Some(&token)),
                ),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::FakeHttp;
    use super::*;
    use serde_json::json;

    fn conn(http: Arc<dyn HttpClient>, secret: Option<&str>) -> GoogleSheetsConnector {
        GoogleSheetsConnector::new(
            Account::new("a", "google_sheets"),
            secret.map(str::to_string),
            http,
        )
        .with_client_fn(Arc::new(|| json!({"client_id": "c", "client_secret": "s"})))
        .with_refresh_fn(Arc::new(|_, _| Ok("AT-SECRET".to_string())))
    }

    #[test]
    fn type_metadata() {
        assert_eq!(
            (
                INFO.label,
                INFO.category,
                INFO.auth,
                INFO.secret_label,
                INFO.brand
            ),
            (
                "Google Sheets",
                "google",
                Auth::OauthGoogle,
                None,
                "googlesheets"
            )
        );
        assert_eq!(INFO.capabilities, &[SHEETS]);
        assert!(INFO.fields.is_empty());
        assert!(get_type("google_sheets").is_some());
        assert!(web_link("x1").contains("/x1/"));
    }

    #[test]
    fn test_lists_one_drive_file_of_the_type() {
        let http = FakeHttp::ok(json!({"files": []}));
        let (ok, msg) = conn(http.clone(), Some("RT")).test();
        assert!(ok, "{msg}");
        let calls = http.calls.lock().unwrap();
        assert!(
            calls.iter().any(|c| c.url.starts_with(drive::DRIVE_URL)
                && c.url.contains("google-apps.spreadsheet")
                && c.url.contains("pageSize=1")),
            "{:?}",
            calls.iter().map(|c| &c.url).collect::<Vec<_>>()
        );
    }

    #[test]
    fn permission_errors_ask_to_reconnect_and_never_leak_tokens() {
        let http =
            FakeHttp::new(|req| Err(HttpError::new(Some(403), "denied AT-SECRET", &req.url)));
        let (ok, msg) = conn(http, Some("RT")).test();
        assert!(
            !ok && msg.contains("Reconnect") && !msg.contains("AT-SECRET"),
            "{msg}"
        );
    }

    #[test]
    fn unconnected_account_says_to_connect() {
        let (ok, msg) = conn(FakeHttp::ok(json!({})), None).test();
        assert!(!ok && msg.contains("Connect with Google"), "{msg}");
    }
}
