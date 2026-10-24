//! Linear connector (GraphQL, personal API key sent raw).

use super::http::TEST_TIMEOUT_SECONDS;
use super::*;
use serde_json::json;

pub const API_URL: &str = "https://api.linear.app/graphql";

pub const INFO: TypeInfo = TypeInfo {
    name: "linear",
    label: "Linear",
    icon: "task_alt",
    capabilities: &[ISSUES],
    fields: &[],
    secret_label: Some("API key"),
    external: false,
    brand: "linear",
    description: "Read issues from Linear with a personal API key.",
    docs_url: "https://linear.app/docs/api-and-webhooks",
    category: "tracking",
    presets: &[],
    auth: Auth::Secret,
};

pub struct LinearConnector {
    account: Account,
    secret: Option<String>,
    http: Arc<dyn HttpClient>,
    api_url: String,
}

impl LinearConnector {
    pub fn new(account: Account, secret: Option<String>, http: Arc<dyn HttpClient>) -> Self {
        Self {
            account,
            secret,
            http,
            api_url: API_URL.to_string(),
        }
    }

    /// Point at another GraphQL endpoint (tests).
    pub fn with_api_url(mut self, url: &str) -> Self {
        self.api_url = url.to_string();
        self
    }

    fn call(
        &self,
        query: &str,
        variables: Option<&Value>,
        timeout: u64,
        attempts: u32,
    ) -> ConnResult<Value> {
        let mut body = json!({"query": query});
        if let Some(v) = variables.filter(|v| v.as_object().is_some_and(|m| !m.is_empty())) {
            body["variables"] = v.clone();
        }
        let req = HttpRequest {
            max_attempts: attempts,
            ..HttpRequest::new("POST", &self.api_url)
                .json(Some(body))
                .header("Authorization", self.secret.as_deref().unwrap_or(""))
                .timeout_secs(timeout)
        };
        Ok(self.http.request_json(&req)?)
    }
}

impl Connector for LinearConnector {
    fn info(&self) -> &'static TypeInfo {
        &INFO
    }

    fn account(&self) -> &Account {
        &self.account
    }

    fn graphql(&self, query: &str, variables: Option<&Value>) -> ConnResult<Value> {
        self.call(query, variables, 30, 3)
    }

    fn test(&self) -> (bool, String) {
        match self.call("{ viewer { id name } }", None, TEST_TIMEOUT_SECONDS, 1) {
            Err(e) => (false, describe_http_error(&e)),
            Ok(data) => {
                if let Some(first) = data
                    .get("errors")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                {
                    let detail = match first {
                        Value::Object(o) => o
                            .get("message")
                            .map_or("?".to_string(), crate::pydumps::py_str),
                        other => crate::pydumps::py_str(other),
                    };
                    return (false, format!("Linear returned an error: {detail}"));
                }
                let name = data
                    .pointer("/data/viewer/name")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                (true, format!("Connected as {name}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::FakeHttp;
    use super::*;

    fn make(http: Arc<dyn HttpClient>) -> LinearConnector {
        LinearConnector::new(Account::new("x", "linear"), Some("s".into()), http)
    }

    #[test]
    fn graphql_viewer_sends_the_key_raw() {
        let http = FakeHttp::ok(json!({"data": {"viewer": {"name": "A"}}}));
        assert_eq!(
            make(http.clone()).test(),
            (true, "Connected as A".to_string())
        );
        let call = http.calls.lock().unwrap()[0].clone();
        assert!(call.headers.contains(&("Authorization".into(), "s".into())));
        assert!(call.json_body.unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("viewer"));
        assert_eq!((call.timeout.as_secs(), call.max_attempts), (10, 1));
        assert_eq!(call.token, None);
    }

    #[test]
    fn errors_list_is_a_failure() {
        let http = FakeHttp::ok(json!({"errors": [{"message": "bad key"}], "data": null}));
        let (ok, msg) = make(http).test();
        assert!(!ok && msg.contains("bad key"));
    }

    #[test]
    fn graphql_passes_variables_only_when_present() {
        let http = FakeHttp::ok(json!({}));
        let c = make(http.clone());
        c.graphql("q", Some(&json!({"a": 1}))).unwrap();
        c.graphql("q", Some(&json!({}))).unwrap();
        let calls = http.calls.lock().unwrap();
        assert_eq!(
            calls[0].json_body,
            Some(json!({"query": "q", "variables": {"a": 1}}))
        );
        assert_eq!(calls[1].json_body, Some(json!({"query": "q"})));
    }
}
