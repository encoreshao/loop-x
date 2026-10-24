//! Slack incoming-webhook connector. The webhook URL is the secret, so no
//! error path here may ever include it.

use super::http::TEST_TIMEOUT_SECONDS;
use super::*;
use serde_json::json;
use std::time::Duration;

pub const INFO: TypeInfo = TypeInfo {
    name: "slack",
    label: "Slack",
    icon: "forum",
    capabilities: &[NOTIFY],
    fields: &[],
    secret_label: Some("Webhook URL"),
    external: false,
    brand: "slack",
    description: "Post notifications to a Slack channel through an incoming webhook.",
    docs_url: "https://api.slack.com/messaging/webhooks",
    category: "chat",
    presets: &[],
    auth: Auth::Secret,
};

/// Translated-free failure text that never echoes the webhook URL.
pub fn describe_post_error(err: &ConnectorError) -> String {
    match err.status() {
        Some(status) => format!("HTTP {status}"),
        None => "Could not reach the webhook".to_string(),
    }
}

pub struct SlackConnector {
    account: Account,
    secret: Option<String>,
    http: Arc<dyn HttpClient>,
}

impl SlackConnector {
    pub fn new(account: Account, secret: Option<String>, http: Arc<dyn HttpClient>) -> Self {
        Self {
            account,
            secret,
            http,
        }
    }
}

impl Connector for SlackConnector {
    fn info(&self) -> &'static TypeInfo {
        &INFO
    }

    fn account(&self) -> &Account {
        &self.account
    }

    fn send(&self, text: &str, blocks: Option<&Value>) -> ConnResult<()> {
        let mut body = json!({"text": text});
        if let Some(b) = blocks.filter(|b| crate::slack::is_truthy(b)) {
            body["blocks"] = b.clone();
        }
        self.http
            .post_webhook(
                self.secret.as_deref().unwrap_or(""),
                &body,
                Duration::from_secs(TEST_TIMEOUT_SECONDS),
            )
            .map_err(ConnectorError::Http)
    }

    fn test(&self) -> (bool, String) {
        match self.send(&self.test_message(), None) {
            Ok(()) => (true, "Test message sent".to_string()),
            Err(e) => (false, describe_post_error(&e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{serve, FakeHttp};
    use super::*;

    const URL: &str = "https://hooks.slack.com/services/T/SECRET/TOKEN";

    fn make(http: Arc<dyn HttpClient>) -> SlackConnector {
        SlackConnector::new(Account::new("x", "slack"), Some(URL.into()), http)
    }

    #[test]
    fn test_posts_a_message_to_the_secret_url() {
        let http = FakeHttp::ok(json!({}));
        assert!(make(http.clone()).test().0);
        let hooks = http.webhooks.lock().unwrap();
        assert_eq!(hooks[0].0, URL);
        assert!(hooks[0].1["text"]
            .as_str()
            .unwrap()
            .starts_with("Loop X test message from connector \"x\". "));
    }

    #[test]
    fn blocks_are_sent_only_when_non_empty() {
        let http = FakeHttp::ok(json!({}));
        let c = make(http.clone());
        c.send("hi", Some(&json!([{"type": "divider"}]))).unwrap();
        c.send("hi", Some(&json!([]))).unwrap();
        c.send("hi", None).unwrap();
        let hooks = http.webhooks.lock().unwrap();
        assert_eq!(
            hooks[0].1,
            json!({"text": "hi", "blocks": [{"type": "divider"}]})
        );
        assert_eq!(hooks[1].1, json!({"text": "hi"}));
        assert_eq!(hooks[2].1, json!({"text": "hi"}));
    }

    #[test]
    fn errors_never_leak_the_secret_url() {
        for err in [
            HttpError::new(Some(404), "no", "<redacted>"),
            HttpError::new(None, "boom", "<redacted>"),
        ] {
            let http = FakeHttp::new(move |_| Err(err.clone()));
            let (ok, msg) = make(http).test();
            assert!(
                !ok && !msg.contains("SECRET") && !msg.contains(URL),
                "{msg}"
            );
        }
        let http = FakeHttp::new(|_| Err(HttpError::new(Some(500), "x", "u")));
        assert_eq!(make(http).test(), (false, "HTTP 500".to_string()));
    }

    #[test]
    fn real_client_redacts_and_accepts_plain_ok() {
        let f = serve(vec![
            (200, "", "ok".to_string()),
            (500, "", "bad".to_string()),
        ]);
        let c = SlackConnector::new(
            Account::new("x", "slack"),
            Some(format!("{}/SECRETPATH", f.url)),
            Arc::new(UreqClient::default()),
        );
        c.send("x", None).unwrap();
        let e = c.send("x", None).unwrap_err();
        assert_eq!(e.status(), Some(500));
        assert!(!format!("{e}{e:?}").contains("SECRETPATH"));
    }

    #[test]
    fn malformed_url_is_redacted() {
        let c = SlackConnector::new(
            Account::new("s", "slack"),
            Some("https://hooks.example/SECRET PATH".into()),
            Arc::new(UreqClient::default()),
        );
        let e = c.send("hi", None).unwrap_err();
        assert!(!format!("{e}{e:?}").contains("SECRET"));
        let c = SlackConnector::new(
            Account::new("s", "slack"),
            Some("ftp://x/SECRET".into()),
            Arc::new(UreqClient::default()),
        );
        assert!(!format!("{:?}", c.send("hi", None)).contains("SECRET"));
    }

    #[test]
    fn test_message_names_label_and_id() {
        let http = FakeHttp::ok(json!({}));
        let mut a = Account::new("team-alerts", "slack");
        a.label = "Team alerts".into();
        SlackConnector::new(a, Some(URL.into()), http.clone()).test();
        assert_eq!(
            http.webhooks.lock().unwrap()[0].1["text"],
            "Loop X test message from connector \"Team alerts\" (team-alerts). Notifications sent through this connector will appear here."
        );
    }
}
