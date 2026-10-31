//! Telegram bot connector. The bot token sits in the request URL path, so
//! every error path scrubs it: whatever the HTTP layer raises is replaced with
//! an [`HttpError`] carrying only a status code and redacted text, and any
//! token echoed in Telegram's own error description is removed.

use super::http::{REDACTED, TEST_TIMEOUT_SECONDS};
use super::*;
use serde_json::json;

const FIELDS: [Field; 2] = [
    Field::new("chat_id", "Chat ID")
        .placeholder("-1001234567890")
        .help("Chat or channel id the bot posts to"),
    Field::new(crate::approval_relay::SETTING, "Approvals here")
        .kind("select")
        .options(&["off", "on"])
        .default("off")
        .optional()
        .help("Send approval requests to this chat with Allow and Deny buttons; reply to one to request changes. Anyone in the chat can answer."),
];

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
        let body = json!({"chat_id": self.account.setting("chat_id"),
                          "text": text.chars().take(4096).collect::<String>()});
        self.call("sendMessage", body, probe).map(|_| ())
    }

    /// One Bot API call; its `result`. Errors never mention the token.
    fn call(&self, method: &str, body: Value, probe: bool) -> Result<Value, HttpError> {
        let url = format!(
            "https://api.telegram.org/bot{}/{method}",
            self.secret.as_deref().unwrap_or("None")
        );
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
                Ok(resp.get("result").cloned().unwrap_or(Value::Null))
            }
        }
    }

    /// Whether `chat` is the configured chat (ids arrive as numbers).
    fn our_chat(&self, chat: &Value) -> bool {
        let id = match &chat["id"] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        id == self.account.setting("chat_id")
    }
}

/// `telegram:@user`, else the first name.
fn who(from: &Value) -> String {
    match from["username"].as_str() {
        Some(u) => format!("telegram:@{u}"),
        None => format!(
            "telegram:{}",
            from["first_name"].as_str().unwrap_or("unknown")
        ),
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

    fn request_approval(&self, approval: &Value) -> ConnResult<Value> {
        let id = approval["id"].as_str().unwrap_or("");
        let field = |k: &str| crate::pydumps::py_str(&approval[k]);
        let summary: String = approval["summary"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(1500)
            .collect();
        let text = format!(
            "Approval needed: {} #{}: {}\nOpen a merge request {} → {}?\n\n{summary}\n\nReply to this message with what to change to send it back to the agent. Expires {}.",
            field("project"),
            field("issue_iid"),
            field("title"),
            field("branch"),
            field("target_branch"),
            field("expires_at"),
        );
        let body = json!({
            "chat_id": self.account.setting("chat_id"),
            "text": text,
            "reply_markup": {"inline_keyboard": [[
                {"text": "Allow", "callback_data": format!("loopx:{id}:approve")},
                {"text": "Deny", "callback_data": format!("loopx:{id}:deny")},
            ]]},
        });
        let sent = self
            .call("sendMessage", body, false)
            .map_err(ConnectorError::Http)?;
        Ok(json!({"message_id": sent["message_id"], "approval_id": id}))
    }

    fn poll_approval(
        &self,
        handle: &Value,
        offset: i64,
    ) -> ConnResult<(i64, Option<ChatDecision>)> {
        let body = json!({"offset": offset, "timeout": 0,
                          "allowed_updates": ["callback_query", "message"]});
        let updates = self
            .call("getUpdates", body, false)
            .map_err(ConnectorError::Http)?;
        let id = handle["approval_id"].as_str().unwrap_or("");
        let message_id = &handle["message_id"];
        let mut next = offset;
        for update in updates.as_array().into_iter().flatten() {
            next = next.max(update["update_id"].as_i64().unwrap_or(0) + 1);
            let press = &update["callback_query"];
            if !press.is_null() && self.our_chat(&press["message"]["chat"]) {
                let data = press["data"].as_str().unwrap_or("");
                let decision = data
                    .strip_prefix(&format!("loopx:{id}:"))
                    .filter(|d| matches!(*d, "approve" | "deny"));
                if let Some(decision) = decision {
                    let _ = self.call(
                        "answerCallbackQuery",
                        json!({"callback_query_id": press["id"], "text": "Recorded"}),
                        false,
                    );
                    return Ok((
                        next,
                        Some(ChatDecision {
                            decision: decision.into(),
                            notes: None,
                            by: who(&press["from"]),
                        }),
                    ));
                }
            }
            let message = &update["message"];
            let notes = message["text"]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty());
            if let Some(notes) = notes.filter(|_| {
                self.our_chat(&message["chat"])
                    && &message["reply_to_message"]["message_id"] == message_id
            }) {
                return Ok((
                    next,
                    Some(ChatDecision {
                        decision: "request_changes".into(),
                        notes: Some(notes.to_string()),
                        by: who(&message["from"]),
                    }),
                ));
            }
        }
        Ok((next, None))
    }

    fn close_approval(&self, handle: &Value, outcome: &str) -> ConnResult<()> {
        let chat = self.account.setting("chat_id");
        self.call(
            "editMessageReplyMarkup",
            json!({"chat_id": chat, "message_id": handle["message_id"], "reply_markup": {"inline_keyboard": []}}),
            false,
        )
        .map_err(ConnectorError::Http)?;
        self.call(
            "sendMessage",
            json!({"chat_id": chat, "text": outcome, "reply_to_message_id": handle["message_id"]}),
            false,
        )
        .map(|_| ())
        .map_err(ConnectorError::Http)
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

    fn approval() -> Value {
        json!({"id": "apr_1", "project": "web", "issue_iid": 7, "title": "Fix login",
               "branch": "loop/issue-7", "target_branch": "main", "summary": "Trim the email",
               "expires_at": "2026-10-31T10:30:00Z"})
    }

    fn method(req: &HttpRequest) -> &str {
        req.url.rsplit('/').next().unwrap_or("")
    }

    #[test]
    fn an_approval_request_carries_allow_and_deny_buttons() {
        let http = FakeHttp::ok(json!({"ok": true, "result": {"message_id": 55}}));
        let handle = make(http.clone()).request_approval(&approval()).unwrap();
        assert_eq!(handle, json!({"message_id": 55, "approval_id": "apr_1"}));
        let calls = http.calls.lock().unwrap();
        assert_eq!(method(&calls[0]), "sendMessage");
        let body = calls[0].json_body.clone().unwrap();
        assert_eq!(body["chat_id"], "-100");
        let text = body["text"].as_str().unwrap();
        assert!(
            text.contains("web #7: Fix login") && text.contains("loop/issue-7 → main"),
            "{text}"
        );
        assert!(text.contains("Reply to this message"), "{text}");
        assert_eq!(
            body["reply_markup"]["inline_keyboard"],
            json!([[{"text": "Allow", "callback_data": "loopx:apr_1:approve"},
                    {"text": "Deny", "callback_data": "loopx:apr_1:deny"}]])
        );
    }

    fn updates(list: Value) -> Arc<FakeHttp> {
        FakeHttp::new(move |req| {
            Ok(match method(req) {
                "getUpdates" => json!({"ok": true, "result": list.clone()}),
                _ => json!({"ok": true, "result": true}),
            })
        })
    }

    #[test]
    fn a_button_press_in_the_chat_is_the_decision() {
        let handle = json!({"message_id": 55, "approval_id": "apr_1"});
        let press = |id: i64, chat: i64, data: &str| {
            json!({"update_id": id, "callback_query": {"id": format!("cb{id}"),
                "from": {"username": "ana", "first_name": "Ana"},
                "message": {"message_id": 55, "chat": {"id": chat}}, "data": data}})
        };
        let http = updates(json!([
            press(10, -999, "loopx:apr_1:approve"),
            press(11, -100, "loopx:apr_other:approve"),
            press(12, -100, "loopx:apr_1:deny"),
        ]));
        let (offset, decision) = make(http.clone()).poll_approval(&handle, 0).unwrap();
        assert_eq!(offset, 13);
        assert_eq!(
            decision,
            Some(ChatDecision {
                decision: "deny".into(),
                notes: None,
                by: "telegram:@ana".into()
            })
        );
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls[0].json_body.as_ref().unwrap()["offset"], 0);
        assert!(calls.iter().any(|c| method(c) == "answerCallbackQuery"
            && c.json_body.as_ref().unwrap()["callback_query_id"] == "cb12"));
    }

    #[test]
    fn a_reply_to_the_request_asks_for_changes() {
        let handle = json!({"message_id": 55, "approval_id": "apr_1"});
        let reply = |id: i64, to: i64, text: &str| {
            json!({"update_id": id, "message": {"message_id": 60 + id, "chat": {"id": -100},
                "from": {"first_name": "Ana"}, "text": text,
                "reply_to_message": {"message_id": to}}})
        };
        let http = updates(json!([
            reply(20, 54, "unrelated"),
            reply(21, 55, "  Keep the old error text  ")
        ]));
        let (offset, decision) = make(http).poll_approval(&handle, 20).unwrap();
        assert_eq!(offset, 22);
        assert_eq!(
            decision,
            Some(ChatDecision {
                decision: "request_changes".into(),
                notes: Some("Keep the old error text".into()),
                by: "telegram:Ana".into()
            })
        );
        let (offset, decision) = make(updates(json!([]))).poll_approval(&handle, 22).unwrap();
        assert_eq!((offset, decision), (22, None));
    }

    #[test]
    fn closing_replaces_the_buttons_with_the_outcome() {
        let http = updates(json!([]));
        make(http.clone())
            .close_approval(
                &json!({"message_id": 55, "approval_id": "apr_1"}),
                "Approved in LoopX",
            )
            .unwrap();
        let calls = http.calls.lock().unwrap();
        assert_eq!(method(&calls[0]), "editMessageReplyMarkup");
        assert_eq!(
            calls[0].json_body,
            Some(
                json!({"chat_id": "-100", "message_id": 55, "reply_markup": {"inline_keyboard": []}})
            )
        );
        assert_eq!(method(&calls[1]), "sendMessage");
        assert_eq!(
            calls[1].json_body.as_ref().unwrap()["reply_to_message_id"],
            55
        );
        assert_eq!(
            calls[1].json_body.as_ref().unwrap()["text"],
            "Approved in LoopX"
        );
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
