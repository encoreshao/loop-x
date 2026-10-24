//! Telegram bot connector. The bot token sits in the request URL path, so
//! every error path scrubs it: whatever the HTTP layer raises is replaced with
//! an [`HttpError`] carrying only a status code and redacted text, and any
//! token echoed in Telegram's own error description is removed.

use super::http::{REDACTED, TEST_TIMEOUT_SECONDS};
use super::*;
use serde_json::json;

const FIELDS: [Field; 1] = [Field::new("chat_id", "Chat ID")
    .placeholder("-1001234567890")
    .help("Chat or channel id the bot posts to")];

pub const INFO: TypeInfo = TypeInfo {
    name: "telegram",
    label: "Telegram bot",
    icon: "send",
    capabilities: &[NOTIFY],
    fields: &FIELDS,
    secret_label: Some("Bot token"),
    external: false,
    brand: "telegram",
    description: "Post notifications to a Telegram chat or channel through a bot.",
    docs_url: "https://core.telegram.org/bots/tutorial#obtain-your-bot-token",
    category: "chat",
    presets: &[],
    auth: Auth::Secret,
};

pub struct TelegramConnector {
    account: Account,
    secret: Option<String>,
    http: Arc<dyn HttpClient>,
}

impl TelegramConnector {
    pub fn new(account: Account, secret: Option<String>, http: Arc<dyn HttpClient>) -> Self {
        Self {
            account,
            secret,
            http,
        }
    }

    fn scrub(&self, text: &str) -> String {
        let secret = self.secret.clone().unwrap_or_default();
        let last = secret.rsplit(':').next().unwrap_or("").to_string();
        let mut out = text.to_string();
        let mut fragments = vec![secret];
        if !fragments.contains(&last) {
            fragments.push(last);
        }
        for f in fragments {
            if f.chars().count() >= 4 {
                out = out.replace(&f, REDACTED);
            }
        }
        out
    }

    /// Never fails with anything that mentions the token. Only the connection
    /// test uses the short, single-attempt probe settings.
    fn send_inner(&self, text: &str, probe: bool) -> Result<(), HttpError> {
        let url = format!(
            "https://api.telegram.org/bot{}/sendMessage",
            self.secret.as_deref().unwrap_or("None")
        );
        let body = json!({"chat_id": self.account.setting("chat_id"),
                          "text": text.chars().take(4096).collect::<String>()});
        let mut req = HttpRequest::new("POST", &url).json(Some(body));
        if probe {
            req = req.timeout_secs(TEST_TIMEOUT_SECONDS).attempts(1);
        }
        match self.http.request_json(&req) {
            Err(e) => {
                let mut body = REDACTED.to_string();
                if e.status.is_some() {
                    if let Some(desc) = serde_json::from_str::<Value>(&e.body)
                        .ok()
                        .and_then(|v| {
                            v.get("description")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .filter(|d| !d.is_empty())
                    {
                        body = self.scrub(&desc);
                    }
                }
                Err(HttpError::new(e.status, body, REDACTED))
            }
            Ok(resp) => {
                if resp.get("ok") == Some(&Value::Bool(false)) {
                    let code = resp
                        .get("error_code")
                        .and_then(Value::as_u64)
                        .map(|c| c as u16);
                    let desc = resp
                        .get("description")
                        .map_or(String::new(), crate::pydumps::py_str);
                    return Err(HttpError::new(code, self.scrub(&desc), REDACTED));
                }
                Ok(())
            }
        }
    }
}

impl Connector for TelegramConnector {
    fn info(&self) -> &'static TypeInfo {
        &INFO
    }

    fn account(&self) -> &Account {
        &self.account
    }

    /// `blocks` is accepted for the uniform notify call and ignored.
    fn send(&self, text: &str, _blocks: Option<&Value>) -> ConnResult<()> {
        self.send_inner(text, false).map_err(ConnectorError::Http)
    }

    fn test(&self) -> (bool, String) {
        match self.send_inner(&self.test_message(), true) {
            Ok(()) => (true, "Test message sent".to_string()),
            Err(e) => {
                let detail = if e.body != REDACTED {
                    e.body.as_str()
                } else {
                    ""
                };
                if !detail.is_empty() {
                    (
                        false,
                        format!(
                            "Telegram returned an error: {}",
                            detail.chars().take(200).collect::<String>()
                        ),
                    )
                } else if let Some(status) = e.status {
                    (false, format!("HTTP {status}"))
                } else {
                    (false, "Could not reach Telegram".to_string())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::FakeHttp;
    use super::*;

    fn make(http: Arc<dyn HttpClient>) -> TelegramConnector {
        TelegramConnector::new(
            Account::new("x", "telegram").with_setting("chat_id", "-100"),
            Some("123:ABC".into()),
            http,
        )
    }

    #[test]
    fn send_posts_the_chat_id_and_truncates_with_default_settings() {
        let http = FakeHttp::ok(json!({"ok": true}));
        let c = make(http.clone());
        c.send("hi", None).unwrap();
        c.send(&"x".repeat(5000), None).unwrap();
        c.test();
        let calls = http.calls.lock().unwrap();
        assert_eq!(
            calls[0].url,
            "https://api.telegram.org/bot123:ABC/sendMessage"
        );
        assert_eq!(
            calls[0].json_body,
            Some(json!({"chat_id": "-100", "text": "hi"}))
        );
        assert_eq!(
            calls[1].json_body.as_ref().unwrap()["text"]
                .as_str()
                .unwrap()
                .len(),
            4096
        );
        assert_eq!((calls[1].timeout.as_secs(), calls[1].max_attempts), (30, 3));
        assert_eq!((calls[2].timeout.as_secs(), calls[2].max_attempts), (10, 1));
    }

    #[test]
    fn errors_never_contain_the_token() {
        for err in [
            HttpError::new(
                Some(401),
                "bad",
                "https://api.telegram.org/bot123:ABC/sendMessage",
            ),
            HttpError::new(
                None,
                "boom 123:ABC",
                "https://api.telegram.org/bot123:ABC/sendMessage",
            ),
        ] {
            let http = FakeHttp::new(move |_| Err(err.clone()));
            let c = make(http);
            let (ok, msg) = c.test();
            assert!(!ok && !msg.contains("ABC"), "{msg}");
            let e = c.send("hi", None).unwrap_err();
            let blob = format!("{e} {e:?}");
            assert!(!blob.contains("ABC"), "{blob}");
        }
    }

    #[test]
    fn scrubs_the_token_from_the_api_description() {
        let http = FakeHttp::ok(json!({"ok": false, "description": "Unauthorized bot123:ABC"}));
        let (ok, msg) = make(http).test();
        assert!(!ok && !msg.contains("ABC"));
        assert!(make(FakeHttp::ok(json!({"ok": true}))).test().0);
    }

    #[test]
    fn surfaces_the_api_description_without_the_token() {
        for (body, want) in [
            (
                r#"{"ok":false,"description":"Bad Request: chat not found"}"#,
                "chat not found",
            ),
            (
                r#"{"ok":false,"description":"bad bot123:ABC token"}"#,
                "bad bot",
            ),
        ] {
            let http = FakeHttp::new(move |r| Err(HttpError::new(Some(400), body, &r.url)));
            let (ok, msg) = make(http).test();
            assert!(!ok && msg.contains(want) && !msg.contains("ABC"), "{msg}");
        }
        let http = FakeHttp::new(|r| Err(HttpError::new(Some(502), "<html>ABC</html>", &r.url)));
        assert_eq!(make(http).test(), (false, "HTTP 502".to_string()));
        let http = FakeHttp::new(|r| Err(HttpError::new(None, "x", &r.url)));
        assert_eq!(
            make(http).test(),
            (false, "Could not reach Telegram".to_string())
        );
    }
}
