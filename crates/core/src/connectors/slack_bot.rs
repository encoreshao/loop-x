//! Slack agent connector: a bot token (the connector secret) plus an app-level
//! token kept under [`app_token_key`] in the same secret store. This module
//! covers what the rest of the app needs from the account: the allowlist,
//! notifications and approvals delivered as direct messages. Tokens only ever
//! travel through [`SlackWeb`], whose errors carry no token.

use super::*;
use crate::slack_agent::web::{SlackWeb, WebError};
use serde_json::json;

const FIELDS: [Field; 3] = [
    Field::new("allowed_users", "Allowed users")
        .kind("textarea")
        .placeholder("U01ABC2DEF")
        .help("Slack user IDs, one per line or comma-separated. Everyone listed can use your agent as you: they get replies with your private data (mail, calendar) by direct message and can answer your approvals."),
    Field::new(crate::approval_relay::SETTING, "Approvals here")
        .kind("select")
        .options(&["off", "on"])
        .default("off")
        .optional()
        .help("Send approval requests to the first allowed user as a direct message with Allow and Deny buttons."),
    Field::new("rate_per_hour", "Requests per hour")
        .default("10")
        .optional()
        .help("Most Slack requests one person can start per hour"),
];

pub const INFO: TypeInfo = TypeInfo {
    name: "slack_bot",
    label: "Slack agent",
    icon: "forum",
    capabilities: &[NOTIFY],
    fields: &FIELDS,
    secret_label: Some("Bot token (xoxb-)"),
    external: false,
    brand: "slack",
    description: "Use your LoopX agent from Slack mentions and direct messages, and answer approvals there. Everyone on the allowlist uses it as you.",
    docs_url: "https://api.slack.com/apis/socket-mode",
    category: "chat",
    presets: &[],
    auth: Auth::Secret,
};

/// The secret-store entry holding the app-level token (`xapp-`) of account `account_id`.
pub fn app_token_key(account_id: &str) -> String {
    format!("slack_bot:{account_id}:app_token")
}

/// The account the background listener runs as (`loopx slack listen` without
/// `--account`, which is how the launch agent starts it): the first enabled
/// Slack agent. One rule for the listener, preflight and the dashboard.
pub fn listener_account(accounts: impl IntoIterator<Item = Account>) -> Option<Account> {
    accounts
        .into_iter()
        .find(|a| a.kind == INFO.name && a.enabled)
}

/// Slack user IDs from free text: split on commas, semicolons and whitespace,
/// upper-cased, de-duplicated (first wins), keeping only `[UW][A-Z0-9]+`.
pub fn parse_allowlist(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
        let id = part.trim().to_ascii_uppercase();
        let valid = id.len() >= 2
            && id.starts_with(['U', 'W'])
            && id
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
        if valid && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Block Kit for an approval request: the details plus Allow and Deny buttons
/// (`action_id` `loopx_approval`, value `<id>:approve` or `<id>:deny`).
pub fn approval_blocks(approval: &Value) -> Value {
    let id = approval["id"].as_str().unwrap_or("");
    json!([
        {"type": "section", "text": {"type": "mrkdwn", "text": approval_text(approval)}},
        {"type": "actions", "elements": [
            {"type": "button", "action_id": "loopx_approval", "style": "primary",
             "text": {"type": "plain_text", "text": "Allow"},
             "value": format!("{id}:approve")},
            {"type": "button", "action_id": "loopx_approval", "style": "danger",
             "text": {"type": "plain_text", "text": "Deny"},
             "value": format!("{id}:deny")},
        ]},
    ])
}

fn approval_text(approval: &Value) -> String {
    let field = |k: &str| escape_mrkdwn(&crate::pydumps::py_str(&approval[k]));
    let summary = escape_mrkdwn(
        &approval["summary"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(1500)
            .collect::<String>(),
    );
    format!(
        "Approval needed: {} #{}: {}\nOpen a merge request {} → {}?\n\n{summary}\n\nExpires {}.",
        field("project"),
        field("issue_iid"),
        field("title"),
        field("branch"),
        field("target_branch"),
        field("expires_at"),
    )
}

/// Slack's mrkdwn escaping (`&` first), the one implementation for the Slack
/// agent: approval fields and outcomes, agent replies, errors and progress are
/// untrusted, and `<!channel>`, `<@U..>` or `<url|label>` would otherwise ping
/// people or disguise links.
pub use crate::slack_report::escape as escape_mrkdwn;

pub struct SlackBotConnector {
    account: Account,
    secret: Option<String>,
    http: Arc<dyn HttpClient>,
}

impl SlackBotConnector {
    pub fn new(account: Account, secret: Option<String>, http: Arc<dyn HttpClient>) -> Self {
        Self {
            account,
            secret,
            http,
        }
    }

    /// Only the bot token is needed here; the app-level token is for the socket.
    fn web(&self) -> ConnResult<SlackWeb> {
        match self.secret.as_deref().filter(|s| !s.is_empty()) {
            Some(bot) => Ok(SlackWeb::new(
                bot.to_string(),
                String::new(),
                self.http.clone(),
            )),
            None => Err(ConnectorError::msg("The Slack bot token is not set")),
        }
    }

    /// The DM channel with the first allowed user; never a public channel.
    fn dm(&self, web: &SlackWeb) -> ConnResult<String> {
        let allowed = parse_allowlist(self.account.setting("allowed_users"));
        let user = allowed
            .first()
            .ok_or_else(|| ConnectorError::msg("No allowed Slack users are set"))?;
        web.open_dm(user).map_err(failure)
    }
}

/// [`WebError`]'s text is token-free by construction.
fn failure(e: WebError) -> ConnectorError {
    ConnectorError::msg(e.to_string())
}

impl Connector for SlackBotConnector {
    fn info(&self) -> &'static TypeInfo {
        &INFO
    }

    fn account(&self) -> &Account {
        &self.account
    }

    /// A direct message to the first allowed user.
    fn send(&self, text: &str, blocks: Option<&Value>) -> ConnResult<()> {
        let web = self.web()?;
        let channel = self.dm(&web)?;
        let blocks = blocks.filter(|b| crate::slack::is_truthy(b));
        web.post(&channel, None, text, blocks)
            .map(|_| ())
            .map_err(failure)
    }

    fn request_approval(&self, approval: &Value) -> ConnResult<Value> {
        let id = approval["id"].as_str().unwrap_or("");
        let web = self.web()?;
        let channel = self.dm(&web)?;
        let ts = web
            .post(
                &channel,
                None,
                &approval_text(approval),
                Some(&approval_blocks(approval)),
            )
            .map_err(failure)?;
        Ok(json!({"channel": channel, "ts": ts, "approval_id": id}))
    }

    /// Always `(offset, None)`: Slack presses are delivered to the listener
    /// process (Socket Mode), which records them with `approvals_bridge::handle_button`,
    /// so there is nothing to poll here.
    fn poll_approval(
        &self,
        _handle: &Value,
        offset: i64,
    ) -> ConnResult<(i64, Option<ChatDecision>)> {
        Ok((offset, None))
    }

    fn close_approval(&self, handle: &Value, outcome: &str) -> ConnResult<()> {
        let web = self.web()?;
        let channel = handle["channel"].as_str().unwrap_or("");
        let ts = handle["ts"].as_str().unwrap_or("");
        let outcome = escape_mrkdwn(outcome);
        let blocks = json!([{"type": "section", "text": {"type": "mrkdwn", "text": outcome}}]);
        web.update(channel, ts, &outcome, Some(&blocks))
            .map_err(failure)
    }

    fn test(&self) -> (bool, String) {
        let web = match self.web() {
            Ok(w) => w,
            Err(e) => return (false, e.to_string()),
        };
        match web.auth_test() {
            Ok(info) if info.team.is_empty() => (true, "Connected to Slack".to_string()),
            Ok(info) => (true, format!("Connected to {}", info.team)),
            Err(e) => (false, e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::FakeHttp;
    use super::*;

    fn make(http: Arc<dyn HttpClient>) -> SlackBotConnector {
        SlackBotConnector::new(
            Account::new("bot", "slack_bot").with_setting("allowed_users", "u01abc, U02DEF"),
            Some("xoxb-SECRET-bot".into()),
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

    /// Answers each Slack method the way the real API does.
    fn slack() -> Arc<FakeHttp> {
        FakeHttp::new(|req| {
            Ok(match method(req) {
                "conversations.open" => json!({"ok": true, "channel": {"id": "D123"}}),
                "chat.postMessage" => json!({"ok": true, "ts": "1700.01"}),
                _ => json!({"ok": true}),
            })
        })
    }

    #[test]
    fn the_allowlist_keeps_upper_cased_unique_user_ids() {
        assert_eq!(
            parse_allowlist("u01ABC, U02DEF\nU01ABC; bogus!"),
            ["U01ABC", "U02DEF"]
        );
        assert_eq!(parse_allowlist("W9X8 C0123 U ux1-2 x"), ["W9X8"]);
        assert!(parse_allowlist("").is_empty());
    }

    #[test]
    fn the_app_token_has_its_own_key_per_account() {
        assert_eq!(app_token_key("team"), "slack_bot:team:app_token");
    }

    #[test]
    fn approval_blocks_carry_allow_and_deny_buttons() {
        let blocks = approval_blocks(&approval());
        let buttons: Vec<&Value> = blocks
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == "actions")
            .flat_map(|b| b["elements"].as_array().unwrap())
            .collect();
        let values: Vec<&str> = buttons
            .iter()
            .map(|b| b["value"].as_str().unwrap())
            .collect();
        assert_eq!(values, ["apr_1:approve", "apr_1:deny"]);
        assert!(buttons.iter().all(|b| b["action_id"] == "loopx_approval"));
        assert!(blocks.to_string().contains("web #7: Fix login"));
    }

    #[test]
    fn an_approval_request_goes_to_a_dm_with_the_first_allowed_user() {
        let http = slack();
        let handle = make(http.clone()).request_approval(&approval()).unwrap();
        assert_eq!(
            handle,
            json!({"channel": "D123", "ts": "1700.01", "approval_id": "apr_1"})
        );
        let calls = http.calls.lock().unwrap();
        assert_eq!(method(&calls[0]), "conversations.open");
        assert_eq!(
            calls[0].form,
            Some(vec![("users".to_string(), "U01ABC".to_string())])
        );
        assert_eq!(method(&calls[1]), "chat.postMessage");
        let body = calls[1].json_body.clone().unwrap();
        assert_eq!(body["channel"], "D123");
        assert!(body["text"].as_str().unwrap().contains("Fix login"));
        assert_eq!(body["blocks"], approval_blocks(&approval()));
    }

    #[test]
    fn an_approval_request_without_an_allowlist_posts_nothing() {
        let http = slack();
        let conn = SlackBotConnector::new(
            Account::new("bot", "slack_bot"),
            Some("xoxb-SECRET-bot".into()),
            http.clone(),
        );
        assert!(conn.request_approval(&approval()).is_err());
        assert!(http.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn polling_never_decides_and_keeps_the_offset() {
        let http = slack();
        let got = make(http.clone())
            .poll_approval(&json!({"channel": "D1", "ts": "1"}), 42)
            .unwrap();
        assert_eq!(got, (42, None));
        assert!(http.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn closing_updates_the_message_without_buttons() {
        let http = slack();
        make(http.clone())
            .close_approval(
                &json!({"channel": "D123", "ts": "1700.01", "approval_id": "apr_1"}),
                "Approved by slack:U01ABC: opening the merge request.",
            )
            .unwrap();
        let calls = http.calls.lock().unwrap();
        assert_eq!(method(&calls[0]), "chat.update");
        let body = calls[0].json_body.clone().unwrap();
        assert_eq!(
            (&body["channel"], &body["ts"]),
            (&json!("D123"), &json!("1700.01"))
        );
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("Approved by slack:U01ABC"), "{text}");
        assert!(!body["blocks"].to_string().contains("loopx_approval"));
    }

    #[test]
    fn failures_never_mention_the_token() {
        let http = FakeHttp::new(|req| {
            Err(HttpError::new(
                Some(500),
                "boom xoxb-SECRET-bot",
                format!("{} xoxb-SECRET-bot", req.url),
            ))
        });
        let conn = make(http);
        let err = conn.request_approval(&approval()).unwrap_err();
        assert!(!format!("{err} {err:?}").contains("SECRET"), "{err:?}");
        let (ok, msg) = conn.test();
        assert!(!ok && !msg.contains("SECRET"), "{msg}");
        let rejected = FakeHttp::ok(json!({"ok": false, "error": "invalid_auth"}));
        let (ok, msg) = make(rejected).test();
        assert!(!ok && !msg.contains("SECRET"), "{msg}");
    }

    const HOSTILE: &str = "<!channel> <@U123> <https://evil.example|https://gitlab.example/x>";

    fn assert_escaped(body: &Value) {
        let all = body.to_string();
        for raw in ["<!channel>", "<@U123>", "<!here>", "<https://evil"] {
            assert!(!all.contains(raw), "{raw} leaked: {all}");
        }
        for escaped in ["&lt;!channel&gt;", "&lt;@U123&gt;", "&lt;!here&gt;"] {
            assert!(all.contains(escaped), "{escaped} missing: {all}");
        }
    }

    #[test]
    fn untrusted_approval_text_cannot_ping_or_link() {
        let mut a = approval();
        a["title"] = json!(HOSTILE);
        a["project"] = json!("p&q <!here>");
        a["summary"] = json!("<!here> sneaky");
        let http = slack();
        make(http.clone()).request_approval(&a).unwrap();
        let calls = http.calls.lock().unwrap();
        let body = calls[1].json_body.clone().unwrap();
        assert_escaped(&body);
        assert!(body.to_string().contains("p&amp;q"));
        let values: Vec<String> = body["blocks"][1]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["value"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(values, ["apr_1:approve", "apr_1:deny"]);
        assert_escaped(&approval_blocks(&a));
    }

    #[test]
    fn untrusted_outcome_text_cannot_ping_or_link() {
        let http = slack();
        let outcome = format!("Denied: {HOSTILE} <!here>");
        make(http.clone())
            .close_approval(&json!({"channel": "D1", "ts": "1"}), &outcome)
            .unwrap();
        let calls = http.calls.lock().unwrap();
        assert_escaped(&calls[0].json_body.clone().unwrap());
    }

    #[test]
    fn a_connector_without_a_token_says_so() {
        let conn = SlackBotConnector::new(Account::new("bot", "slack_bot"), None, slack());
        let (ok, msg) = conn.test();
        assert!(!ok && msg.contains("token"), "{msg}");
    }

    #[test]
    fn the_test_names_the_workspace() {
        let http = FakeHttp::ok(json!({"ok": true, "user_id": "UBOT", "team": "Acme"}));
        assert_eq!(make(http).test(), (true, "Connected to Acme".to_string()));
    }
}
