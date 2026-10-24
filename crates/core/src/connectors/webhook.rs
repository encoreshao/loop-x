//! Generic chat webhook connector (Feishu, DingTalk, Teams, Discord, other).
//! The webhook URL is the secret, so no error path may include it.

use super::http::TEST_TIMEOUT_SECONDS;
use super::slack::describe_post_error;
use super::*;
use serde_json::json;
use std::time::Duration;

const FORMATS: [&str; 7] = [
    "feishu",
    "dingtalk",
    "teams",
    "discord",
    "wecom",
    "googlechat",
    "generic",
];
const FIELDS: [Field; 1] = [Field::new("format", "Payload format")
    .kind("select")
    .default("generic")
    .options(&FORMATS)
    .help("Message shape the receiving service expects")];

const fn preset(
    key: &'static str,
    label: &'static str,
    brand: &'static str,
    description: &'static str,
    settings: &'static [(&'static str, &'static str)],
    docs_url: &'static str,
    category: &'static str,
) -> Preset {
    Preset {
        key,
        label,
        brand,
        description,
        settings,
        docs_url,
        category,
    }
}

const PRESETS: [Preset; 7] = [
    preset("feishu", "Feishu", "feishu",
        "Post notifications to a Feishu group through a custom bot webhook.",
        &[("format", "feishu")],
        "https://open.feishu.cn/document/client-docs/bot-v3/add-custom-bot", ""),
    preset("dingtalk", "DingTalk", "dingtalk",
        "Post notifications to a DingTalk group through a custom robot webhook.",
        &[("format", "dingtalk")],
        "https://open.dingtalk.com/document/robots/custom-robot-access", ""),
    preset("wecom", "WeCom \u{4f01}\u{4e1a}\u{5fae}\u{4fe1}", "wecom",
        "Group bot webhook. Personal WeChat has no bot API.",
        &[("format", "wecom")],
        "https://developer.work.weixin.qq.com/document/path/91770", ""),
    preset("microsoftteams", "Microsoft Teams", "microsoftteams",
        "Teams Workflows webhooks may require Adaptive Cards.",
        &[("format", "teams")],
        "https://learn.microsoft.com/en-us/microsoftteams/platform/webhooks-and-connectors/how-to/add-incoming-webhook", ""),
    preset("discord", "Discord", "discord",
        "Post notifications to a Discord channel through an incoming webhook.",
        &[("format", "discord")],
        "https://support.discord.com/hc/en-us/articles/228383668-Intro-to-Webhooks", ""),
    preset("googlechat", "Google Chat", "googlechat",
        "Post notifications to a Google Chat space through an incoming webhook.",
        &[("format", "googlechat")],
        "https://developers.google.com/workspace/chat/quickstart/webhooks", "google"),
    preset("generic", "Generic webhook", "webhook",
        "Post JSON notifications to any webhook URL.",
        &[("format", "generic")], "", ""),
];

pub const INFO: TypeInfo = TypeInfo {
    name: "webhook",
    label: "Chat webhook",
    icon: "webhook",
    capabilities: &[NOTIFY],
    fields: &FIELDS,
    secret_label: Some("Webhook URL"),
    external: false,
    brand: "webhook",
    description:
        "Post notifications to Feishu, DingTalk, WeCom, Teams, Discord, Google Chat or any webhook.",
    docs_url: "",
    category: "chat",
    presets: &PRESETS,
    auth: Auth::Secret,
};

/// The JSON body each format expects.
pub fn payload(format: &str, text: &str) -> Value {
    match format {
        "feishu" => json!({"msg_type": "text", "content": {"text": text}}),
        "dingtalk" | "wecom" => json!({"msgtype": "text", "text": {"content": text}}),
        "discord" => json!({"content": text}),
        _ => json!({"text": text}),
    }
}

pub struct WebhookConnector {
    account: Account,
    secret: Option<String>,
    http: Arc<dyn HttpClient>,
}

impl WebhookConnector {
    pub fn new(account: Account, secret: Option<String>, http: Arc<dyn HttpClient>) -> Self {
        Self {
            account,
            secret,
            http,
        }
    }
}

impl Connector for WebhookConnector {
    fn info(&self) -> &'static TypeInfo {
        &INFO
    }

    fn account(&self) -> &Account {
        &self.account
    }

    /// `blocks` (Slack Block Kit) is accepted for the uniform notify call and
    /// ignored - these services take text.
    fn send(&self, text: &str, _blocks: Option<&Value>) -> ConnResult<()> {
        let fmt = Some(self.account.setting("format"))
            .filter(|f| !f.is_empty())
            .unwrap_or("generic");
        self.http
            .post_webhook(
                self.secret.as_deref().unwrap_or(""),
                &payload(fmt, text),
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
    use super::super::testutil::FakeHttp;
    use super::*;

    fn sent(format: &str, text: &str) -> Value {
        let http = FakeHttp::ok(json!({}));
        let c = WebhookConnector::new(
            Account::new("x", "webhook").with_setting("format", format),
            Some("https://example.com/h".into()),
            http.clone(),
        );
        c.send(text, Some(&json!([{"type": "divider"}]))).unwrap();
        let body = http.webhooks.lock().unwrap()[0].1.clone();
        body
    }

    #[test]
    fn every_format_has_its_payload_shape() {
        assert_eq!(
            sent("feishu", "hello"),
            json!({"msg_type": "text", "content": {"text": "hello"}})
        );
        for (fmt, expected) in [
            (
                "dingtalk",
                json!({"msgtype": "text", "text": {"content": "hi"}}),
            ),
            (
                "wecom",
                json!({"msgtype": "text", "text": {"content": "hi"}}),
            ),
            ("teams", json!({"text": "hi"})),
            ("discord", json!({"content": "hi"})),
            ("googlechat", json!({"text": "hi"})),
            ("generic", json!({"text": "hi"})),
            ("unknown", json!({"text": "hi"})),
            ("", json!({"text": "hi"})),
        ] {
            assert_eq!(sent(fmt, "hi"), expected, "{fmt}");
        }
    }

    #[test]
    fn presets_cover_brands_with_distinct_descriptions() {
        let keys: Vec<_> = INFO.presets.iter().map(|p| p.key).collect();
        for k in [
            "feishu",
            "dingtalk",
            "wecom",
            "microsoftteams",
            "discord",
            "googlechat",
            "generic",
        ] {
            assert!(keys.contains(&k), "{k}");
        }
        for p in INFO.presets {
            assert!(INFO.fields[0].options.contains(&p.settings[0].1));
            assert!(!p.description.trim().is_empty());
            if p.key != "generic" {
                assert!(p.docs_url.starts_with("https://"), "{}", p.key);
            }
            assert_eq!(p.category == "google", p.key == "googlechat");
        }
        let mut d: Vec<_> = INFO.presets.iter().map(|p| p.description).collect();
        d.sort();
        d.dedup();
        assert_eq!(d.len(), INFO.presets.len());
        assert!(!INFO
            .presets
            .iter()
            .any(|p| p.description == INFO.description));
        assert!(INFO
            .presets
            .iter()
            .find(|p| p.key == "microsoftteams")
            .unwrap()
            .description
            .contains("Adaptive Cards"));
    }

    #[test]
    fn errors_never_leak_the_secret_url() {
        let url = "https://hooks.slack.com/services/T/SECRET/TOKEN";
        let http = FakeHttp::new(|_| Err(HttpError::new(Some(404), "no", "<redacted>")));
        let c = WebhookConnector::new(Account::new("x", "webhook"), Some(url.into()), http);
        let (ok, msg) = c.test();
        assert!(!ok && msg == "HTTP 404");
    }
}
