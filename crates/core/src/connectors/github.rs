//! GitHub connector (github.com or GitHub Enterprise).

use super::http::TEST_TIMEOUT_SECONDS;

/// Job logs are cut to their last 2 MB (failures sit at the end).
const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
use super::*;

const FIELDS: [Field; 2] = [
    Field::new("api_url", "API URL")
        .kind("url")
        .default("https://api.github.com"),
    Field::new("username", "Username")
        .optional()
        .placeholder("octocat"),
];

pub const INFO: TypeInfo = TypeInfo {
    name: "github",
    label: "GitHub",
    icon: "code",
    capabilities: &[ISSUES, MERGE_REQUESTS, PIPELINES],
    fields: &FIELDS,
    secret_label: Some("Personal access token"),
    external: false,
    brand: "github",
    description: "Read issues, pull requests and workflow runs from GitHub or GitHub Enterprise.",
    docs_url: "https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens",
    category: "code",
    presets: &[],
    auth: Auth::Secret,
};

pub struct GitHubConnector {
    account: Account,
    secret: Option<String>,
    http: Arc<dyn HttpClient>,
}

impl GitHubConnector {
    pub fn new(account: Account, secret: Option<String>, http: Arc<dyn HttpClient>) -> Self {
        Self {
            account,
            secret,
            http,
        }
    }
}

impl Connector for GitHubConnector {
    fn info(&self) -> &'static TypeInfo {
        &INFO
    }

    fn account(&self) -> &Account {
        &self.account
    }

    fn api(&self, method: &str, path: &str, opts: &ApiOpts) -> ConnResult<Value> {
        let base = self
            .account
            .setting("api_url")
            .trim_end_matches('/')
            .to_string();
        let req = HttpRequest {
            json_body: opts.json_body.clone(),
            max_attempts: opts.max_attempts,
            ..HttpRequest::new(method, &format!("{base}{path}"))
                .token(self.secret.as_deref())
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .timeout_secs(opts.timeout_secs)
        };
        Ok(self.http.request_json(&req)?)
    }

    /// GET a plain-text endpoint (an Actions job log) and return its last
    /// 2 MB. Errors carry only the status, never the URL or token.
    fn api_text(&self, path: &str) -> ConnResult<String> {
        let base = self
            .account
            .setting("api_url")
            .trim_end_matches('/')
            .to_string();
        let mut headers = vec![
            (
                "Accept".to_string(),
                "application/vnd.github+json".to_string(),
            ),
            ("X-GitHub-Api-Version".to_string(), "2022-11-28".to_string()),
        ];
        if let Some(t) = &self.secret {
            headers.push(("Authorization".to_string(), format!("Bearer {t}")));
        }
        self.http
            .get_text_tail(
                &format!("{base}{path}"),
                &headers,
                std::time::Duration::from_secs(30),
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
                    me.get("login").and_then(Value::as_str).unwrap_or("?")
                ),
            ),
            Err(e) => (false, describe_http_error(&e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::FakeHttp;
    use super::*;
    use serde_json::json;

    fn make(http: Arc<dyn HttpClient>) -> GitHubConnector {
        GitHubConnector::new(
            Account::new("x", "github")
                .with_setting("api_url", "https://api.github.com/")
                .with_setting("username", "u"),
            Some("s".into()),
            http,
        )
    }

    #[test]
    fn test_uses_bearer_and_the_probe_settings() {
        let http = FakeHttp::ok(json!({"login": "u"}));
        assert_eq!(
            make(http.clone()).test(),
            (true, "Connected as u".to_string())
        );
        let call = http.calls.lock().unwrap()[0].clone();
        assert_eq!(call.token.as_deref(), Some("s"));
        assert_eq!(call.url, "https://api.github.com/user");
        assert!(call
            .headers
            .contains(&("Accept".into(), "application/vnd.github+json".into())));
        assert_eq!((call.timeout.as_secs(), call.max_attempts), (10, 1));
    }

    #[test]
    fn test_never_raises_on_http_error() {
        let http = FakeHttp::new(|r| Err(HttpError::new(Some(500), "boom", &r.url)));
        let (ok, msg) = make(http).test();
        assert!(!ok && msg.contains("500"));
    }
    #[test]
    fn api_text_sends_the_token_to_the_api_host() {
        let served = crate::connectors::testutil::serve(vec![(200, "", "line1\nboom".into())]);
        let c = GitHubConnector::new(
            Account::new("x", "github").with_setting("api_url", &served.url),
            Some("s".into()),
            Arc::new(crate::connectors::http::UreqClient::default()),
        );
        assert_eq!(
            c.api_text("/repos/o/r/actions/jobs/5/logs").unwrap(),
            "line1\nboom"
        );
        let raw = served.requests.lock().unwrap()[0].to_lowercase();
        assert!(raw.starts_with("get /repos/o/r/actions/jobs/5/logs "));
        assert!(raw.contains("authorization: bearer s"));
    }
}
