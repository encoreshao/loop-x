//! GitLab connector. External accounts come from `~/.gitlab/config.json`
//! (owned by the gitlab-config skill); this type can also be created natively
//! for an instance you don't want in that shared file.

use super::http::TEST_TIMEOUT_SECONDS;
use super::*;
use std::time::Duration;

pub const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;

const FIELDS: [Field; 1] = [Field::new("url", "Instance URL")
    .kind("url")
    .default("https://gitlab.com")
    .placeholder("https://gitlab.com")];

pub const INFO: TypeInfo = TypeInfo {
    name: "gitlab",
    label: "GitLab",
    icon: "code",
    capabilities: &[ISSUES, MERGE_REQUESTS, PIPELINES],
    fields: &FIELDS,
    secret_label: Some("Personal access token"),
    external: false,
    brand: "gitlab",
    description: "Read issues, merge requests and pipelines from a GitLab instance.",
    docs_url: "https://docs.gitlab.com/ee/user/profile/personal_access_tokens.html",
    category: "code",
    presets: &[],
    auth: Auth::Secret,
};

pub struct GitLabConnector {
    account: Account,
    secret: Option<String>,
    http: Arc<dyn HttpClient>,
}

impl GitLabConnector {
    pub fn new(account: Account, secret: Option<String>, http: Arc<dyn HttpClient>) -> Self {
        Self {
            account,
            secret,
            http,
        }
    }

    fn base(&self) -> String {
        self.account
            .setting("url")
            .trim_end_matches('/')
            .to_string()
    }

    fn token_header(&self) -> (String, String) {
        (
            "PRIVATE-TOKEN".to_string(),
            self.secret.clone().unwrap_or_default(),
        )
    }
}

impl Connector for GitLabConnector {
    fn info(&self) -> &'static TypeInfo {
        &INFO
    }

    fn account(&self) -> &Account {
        &self.account
    }

    fn base_url(&self) -> ConnResult<String> {
        Ok(self.base())
    }

    fn api(&self, method: &str, path: &str, opts: &ApiOpts) -> ConnResult<Value> {
        let (k, v) = self.token_header();
        let req = HttpRequest {
            json_body: opts.json_body.clone(),
            max_attempts: opts.max_attempts,
            ..HttpRequest::new(method, &format!("{}/api/v4{path}", self.base()))
                .header(&k, &v)
                .timeout_secs(opts.timeout_secs)
        };
        Ok(self.http.request_json(&req)?)
    }

    /// GET a plain-text endpoint (e.g. a job trace) and return its LAST 2 MB
    /// (failures sit at the end of a log). Errors carry only the status: the
    /// URL and token are never put in the error.
    fn api_text(&self, path: &str) -> ConnResult<String> {
        let (k, v) = self.token_header();
        self.http
            .get_text_tail(
                &format!("{}/api/v4{path}", self.base()),
                &[(k, v)],
                Duration::from_secs(30),
                MAX_TEXT_BYTES,
            )
            .map_err(ConnectorError::Http)
    }

    fn test(&self) -> (bool, String) {
        match self.api(
            "GET",
            "/user",
            &ApiOpts::default().timeout(TEST_TIMEOUT_SECONDS).once(),
        ) {
            Ok(me) => (
                true,
                format!(
                    "Connected as {}",
                    me.get("username").and_then(Value::as_str).unwrap_or("?")
                ),
            ),
            Err(e) => (false, describe_http_error(&e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{serve, FakeHttp};
    use super::*;
    use serde_json::json;

    fn make(url: &str, http: Arc<dyn HttpClient>) -> GitLabConnector {
        GitLabConnector::new(
            Account::new("x", "gitlab").with_setting("url", url),
            Some("s".into()),
            http,
        )
    }

    #[test]
    fn test_calls_user_endpoint_with_private_token_header() {
        let http = FakeHttp::ok(json!({"username": "enc"}));
        let c = make("https://gl.example/", http.clone());
        assert_eq!(c.test(), (true, "Connected as enc".to_string()));
        let call = http.calls.lock().unwrap()[0].clone();
        assert_eq!(call.url, "https://gl.example/api/v4/user");
        assert!(call.headers.contains(&("PRIVATE-TOKEN".into(), "s".into())));
        assert_eq!(call.token, None);
        assert_eq!((call.timeout.as_secs(), call.max_attempts), (10, 1));
    }

    #[test]
    fn auth_expired_message_and_never_raises() {
        let http = FakeHttp::new(|r| {
            Err(HttpError {
                auth_expired: true,
                ..HttpError::new(Some(401), "", &r.url)
            })
        });
        assert_eq!(
            make("https://gl", http).test(),
            (
                false,
                "Token rejected (401) - check it hasn't expired".to_string()
            )
        );
        let http = FakeHttp::new(|r| Err(HttpError::new(Some(500), "boom", &r.url)));
        let (ok, msg) = make("https://gl", http).test();
        assert!(!ok && msg.contains("500"));
    }

    #[test]
    fn api_passes_body_attempts_and_prefixes_the_url() {
        let http = FakeHttp::ok(json!([]));
        let c = make("https://gl.example", http.clone());
        c.api(
            "POST",
            "/projects/1/x",
            &ApiOpts::json(json!({"a": 1})).once(),
        )
        .unwrap();
        let call = http.calls.lock().unwrap()[0].clone();
        assert_eq!(call.method, "POST");
        assert_eq!(call.url, "https://gl.example/api/v4/projects/1/x");
        assert_eq!(call.json_body, Some(json!({"a": 1})));
        assert_eq!((call.timeout.as_secs(), call.max_attempts), (30, 1));
        assert_eq!(c.base_url().unwrap(), "https://gl.example");
    }

    #[test]
    fn api_text_returns_the_tail_and_sends_the_token_and_range() {
        let f = serve(vec![(200, "", "boom \u{2713}\n".to_string())]);
        let c = make(&f.url, Arc::new(UreqClient::default()));
        assert_eq!(
            c.api_text("/projects/9/jobs/5/trace").unwrap(),
            "boom \u{2713}\n"
        );
        let raw = f.requests.lock().unwrap()[0].to_lowercase();
        assert!(raw.starts_with("get /api/v4/projects/9/jobs/5/trace"));
        assert!(raw.contains("private-token: s") && raw.contains("range: bytes=-2097152"));
    }

    #[test]
    fn api_text_errors_redact_url_and_token() {
        let f = serve(vec![(404, "", "secret-body s".to_string())]);
        let c = make(&f.url, Arc::new(UreqClient::default()));
        let e = c.api_text("/x").unwrap_err();
        assert_eq!(e.status(), Some(404));
        let text = format!("{e}{e:?}");
        assert!(!text.contains("secret-body") && !text.contains(&f.url));
    }

    #[test]
    fn api_text_416_is_empty() {
        let f = serve(vec![(416, "", String::new())]);
        let c = make(&f.url, Arc::new(UreqClient::default()));
        assert_eq!(c.api_text("/t").unwrap(), "");
    }
}
