//! Jira Cloud connector (email + API token, HTTP Basic).

use super::http::TEST_TIMEOUT_SECONDS;
use super::*;

const FIELDS: [Field; 2] = [
    Field::new("site_url", "Site URL")
        .kind("url")
        .placeholder("https://your-team.atlassian.net"),
    Field::new("email", "Account email")
        .kind("email")
        .placeholder("you@company.com"),
];

pub const INFO: TypeInfo = TypeInfo {
    name: "jira",
    label: "Jira",
    icon: "task_alt",
    capabilities: &[ISSUES],
    fields: &FIELDS,
    secret_label: Some("API token"),
    external: false,
    brand: "jira",
    description: "Read issues from Jira Cloud with your account email and an API token.",
    docs_url: "https://support.atlassian.com/atlassian-account/docs/manage-api-tokens-for-your-atlassian-account/",
    category: "tracking",
    presets: &[],
    auth: Auth::Secret,
};

/// Standard base64 with padding.
pub fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub struct JiraConnector {
    account: Account,
    secret: Option<String>,
    http: Arc<dyn HttpClient>,
}

impl JiraConnector {
    pub fn new(account: Account, secret: Option<String>, http: Arc<dyn HttpClient>) -> Self {
        Self {
            account,
            secret,
            http,
        }
    }
}

impl Connector for JiraConnector {
    fn info(&self) -> &'static TypeInfo {
        &INFO
    }

    fn account(&self) -> &Account {
        &self.account
    }

    fn api(&self, method: &str, path: &str, opts: &ApiOpts) -> ConnResult<Value> {
        let creds = format!(
            "{}:{}",
            self.account.setting("email"),
            self.secret.as_deref().unwrap_or("")
        );
        let base = self
            .account
            .setting("site_url")
            .trim_end_matches('/')
            .to_string();
        let req = HttpRequest {
            json_body: opts.json_body.clone(),
            max_attempts: opts.max_attempts,
            ..HttpRequest::new(method, &format!("{base}{path}"))
                .header(
                    "Authorization",
                    &format!("Basic {}", base64_encode(creds.as_bytes())),
                )
                .header("Accept", "application/json")
                .timeout_secs(opts.timeout_secs)
        };
        Ok(self.http.request_json(&req)?)
    }

    fn test(&self) -> (bool, String) {
        match self.api(
            "GET",
            "/rest/api/3/myself",
            &ApiOpts::default().timeout(TEST_TIMEOUT_SECONDS).once(),
        ) {
            Ok(me) => (
                true,
                format!(
                    "Connected as {}",
                    me.get("displayName").and_then(Value::as_str).unwrap_or("?")
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
    fn base64_matches_known_values() {
        assert_eq!(base64_encode(b"a@b.co:s"), "YUBiLmNvOnM=");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn uses_basic_auth_prefixes_urls_and_probes_once() {
        let http = FakeHttp::ok(json!({"displayName": "A"}));
        let c = JiraConnector::new(
            Account::new("x", "jira")
                .with_setting("site_url", "https://acme.atlassian.net/")
                .with_setting("email", "a@b.co"),
            Some("s".into()),
            http.clone(),
        );
        assert_eq!(c.test(), (true, "Connected as A".to_string()));
        let call = http.calls.lock().unwrap()[0].clone();
        assert_eq!(call.url, "https://acme.atlassian.net/rest/api/3/myself");
        assert!(call.headers.contains(&(
            "Authorization".into(),
            format!("Basic {}", base64_encode(b"a@b.co:s"))
        )));
        assert_eq!((call.timeout.as_secs(), call.max_attempts), (10, 1));
    }
}
