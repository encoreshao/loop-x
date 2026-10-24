//! Notion connector (internal integration token).

use super::http::TEST_TIMEOUT_SECONDS;
use super::*;

pub const API_URL: &str = "https://api.notion.com/v1";
pub const API_VERSION: &str = "2022-06-28";

pub const INFO: TypeInfo = TypeInfo {
    name: "notion",
    label: "Notion",
    icon: "description",
    capabilities: &[DOCS],
    fields: &[],
    secret_label: Some("Integration token"),
    external: false,
    brand: "notion",
    description: "Read pages and databases shared with a Notion integration.",
    docs_url: "https://developers.notion.com/docs/create-a-notion-integration",
    category: "knowledge",
    presets: &[],
    auth: Auth::Secret,
};

pub struct NotionConnector {
    account: Account,
    secret: Option<String>,
    http: Arc<dyn HttpClient>,
}

impl NotionConnector {
    pub fn new(account: Account, secret: Option<String>, http: Arc<dyn HttpClient>) -> Self {
        Self {
            account,
            secret,
            http,
        }
    }
}

impl Connector for NotionConnector {
    fn info(&self) -> &'static TypeInfo {
        &INFO
    }

    fn account(&self) -> &Account {
        &self.account
    }

    fn api(&self, method: &str, path: &str, opts: &ApiOpts) -> ConnResult<Value> {
        let req = HttpRequest {
            json_body: opts.json_body.clone(),
            max_attempts: opts.max_attempts,
            ..HttpRequest::new(method, &format!("{API_URL}{path}"))
                .token(self.secret.as_deref())
                .header("Notion-Version", API_VERSION)
                .timeout_secs(opts.timeout_secs)
        };
        Ok(self.http.request_json(&req)?)
    }

    fn test(&self) -> (bool, String) {
        match self.api(
            "GET",
            "/users/me",
            &ApiOpts::default().timeout(TEST_TIMEOUT_SECONDS).once(),
        ) {
            Ok(me) => (
                true,
                format!(
                    "Connected as {}",
                    me.get("name").and_then(Value::as_str).unwrap_or("?")
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

    #[test]
    fn test_uses_the_version_header_and_bearer() {
        let http = FakeHttp::ok(json!({"name": "Loop bot"}));
        let c = NotionConnector::new(Account::new("x", "notion"), Some("s".into()), http.clone());
        assert_eq!(c.test(), (true, "Connected as Loop bot".to_string()));
        let call = http.calls.lock().unwrap()[0].clone();
        assert_eq!(call.url, "https://api.notion.com/v1/users/me");
        assert!(call
            .headers
            .contains(&("Notion-Version".into(), "2022-06-28".into())));
        assert_eq!(call.token.as_deref(), Some("s"));
    }
}
