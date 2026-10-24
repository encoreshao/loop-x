//! Mailbox connector. Accounts are managed on the Inbox Triage page; this
//! type reports whether a refresh token is stored for the inbox and gives the
//! loops read access (search / read) and, for Gmail, send.
//!
//! The mail plumbing (inbox config, Keychain tokens, provider calls) sits
//! behind [`MailBackend`]; the connector only maps its failures to
//! token-free [`ConnectorError`]s.

use super::*;
use crate::mail_http::MailError;
use crate::mail_provider::{self, MailProvider, MessageRow};
use crate::{inbox_config, mail_auth, mail_sender};
use serde_json::json;
use std::sync::OnceLock;

pub const INFO: TypeInfo = TypeInfo {
    name: "mailbox",
    label: "Mailbox",
    icon: "mail",
    capabilities: &[MAIL],
    fields: &[],
    secret_label: None,
    external: true,
    brand: "",
    description: "Gmail or Outlook inbox managed on the Inbox Triage page.",
    docs_url: "",
    category: "mail",
    presets: &[],
    auth: Auth::Secret,
};

/// One opened mailbox (token already resolved).
pub trait MailAccess {
    fn search_messages(&mut self, query: &str, limit: u32) -> ConnResult<Vec<Value>>;
    fn read_message(&mut self, message_id: &str) -> ConnResult<Value>;
    /// Gmail only: Outlook's grant has no send scope.
    fn send_message(
        &mut self,
        to: &[String],
        cc: &[String],
        subject: &str,
        body: &str,
    ) -> ConnResult<Value>;
}

/// The mailbox plumbing behind a mailbox account.
pub trait MailBackend: Send + Sync {
    /// Is a refresh token stored for the inbox? `Err((class, detail))` when
    /// the Keychain could not be read.
    fn has_token(&self, account_id: &str) -> Result<bool, (String, String)>;
    /// Open the mailbox. Failures carry a token-free message.
    fn open(&self, account_id: &str) -> ConnResult<Box<dyn MailAccess>>;
}

fn mail_error(e: MailError) -> ConnectorError {
    match e {
        MailError::Http(h) => ConnectorError::Http(HttpError::new(h.status, h.body, h.url)),
        MailError::AuthExpired(h) => ConnectorError::Http(HttpError {
            auth_expired: true,
            ..HttpError::new(h.status, h.body, h.url)
        }),
        other => ConnectorError::Other {
            class: other.class_name().to_string(),
            detail: String::new(),
        },
    }
}

/// The production backend: `inboxes.json`, the mail Keychain tokens and the
/// Gmail / Outlook providers (`bin/connectors/mailbox.py`'s `_open`).
pub struct DefaultMailBackend;

struct ProviderAccess {
    inbox: Value,
    token: String,
    provider: Box<dyn MailProvider>,
}

impl MailAccess for ProviderAccess {
    fn search_messages(&mut self, query: &str, limit: u32) -> ConnResult<Vec<Value>> {
        self.provider
            .search_messages(query, limit as usize)
            .map(|rows| rows.iter().map(MessageRow::to_value).collect())
            .map_err(|e| ConnectorError::msg(describe_http_error(&mail_error(e))))
    }

    fn read_message(&mut self, message_id: &str) -> ConnResult<Value> {
        let m = self
            .provider
            .read_message(message_id)
            .map_err(|e| ConnectorError::msg(describe_http_error(&mail_error(e))))?;
        Ok(
            json!({"id": m.id, "thread_id": m.thread_id, "from": m.from, "to": m.to, "cc": m.cc,
                  "subject": m.subject, "date": m.date, "body_text": m.body_text,
                  "attachments": m.attachments}),
        )
    }

    fn send_message(
        &mut self,
        to: &[String],
        cc: &[String],
        subject: &str,
        body: &str,
    ) -> ConnResult<Value> {
        if self.inbox.get("provider").and_then(Value::as_str) != Some("gmail") {
            return Err(ConnectorError::msg(
                "Sending is only supported for Gmail mailboxes",
            ));
        }
        let sender = self
            .inbox
            .get("account")
            .and_then(Value::as_str)
            .unwrap_or("");
        mail_sender::send_gmail(&self.token, sender, to, cc, subject, body, None).map_err(|e| {
            match e {
                mail_sender::SendError::Invalid(m) => ConnectorError::msg(m),
                mail_sender::SendError::Mail(e) => {
                    ConnectorError::msg(describe_http_error(&mail_error(e)))
                }
            }
        })
    }
}

impl MailBackend for DefaultMailBackend {
    fn has_token(&self, account_id: &str) -> Result<bool, (String, String)> {
        let store =
            mail_auth::default_store().map_err(|e| (e.class_name().to_string(), e.to_string()))?;
        use crate::mail_auth::RefreshTokenStore;
        store
            .get(account_id)
            .map(|t| t.is_some())
            .map_err(|e| (e.class_name().to_string(), e.to_string()))
    }

    fn open(&self, account_id: &str) -> ConnResult<Box<dyn MailAccess>> {
        let inbox = inbox_config::get_inbox(account_id, &inbox_config::default_config_path())
            .map_err(|_| ConnectorError::msg("Mailbox is not configured"))?;
        let token = mail_auth::get_access_token_default(&inbox).map_err(|e| match e {
            MailError::Reauth(m) if !m.is_empty() => ConnectorError::msg(m),
            MailError::Reauth(_) | MailError::AuthExpired(_) => {
                ConnectorError::msg("Sign-in expired - reconnect this inbox")
            }
            MailError::Keychain(_) => ConnectorError::msg("Could not read the mailbox credentials"),
            other => mail_error(other),
        })?;
        let refresh_inbox = inbox.clone();
        let refresh: mail_provider::RefreshFn =
            Box::new(move || mail_auth::get_access_token_default(&refresh_inbox));
        let provider = mail_provider::get_provider(&inbox, &token, Some(refresh), None)
            .map_err(|_| ConnectorError::msg("Mailbox is not configured"))?;
        Ok(Box::new(ProviderAccess {
            inbox,
            token,
            provider,
        }))
    }
}

static BACKEND: OnceLock<Arc<dyn MailBackend>> = OnceLock::new();

/// The inbox stream registers the real backend here (once, at startup).
pub fn register_backend(backend: Arc<dyn MailBackend>) -> bool {
    BACKEND.set(backend).is_ok()
}

pub struct MailboxConnector {
    account: Account,
    backend: Option<Arc<dyn MailBackend>>,
}

impl MailboxConnector {
    pub fn new(account: Account) -> Self {
        Self {
            account,
            backend: None,
        }
    }

    pub fn with_backend(mut self, backend: Arc<dyn MailBackend>) -> Self {
        self.backend = Some(backend);
        self
    }

    fn backend(&self) -> Arc<dyn MailBackend> {
        self.backend
            .clone()
            .or_else(|| BACKEND.get().cloned())
            .unwrap_or_else(|| Arc::new(DefaultMailBackend))
    }
}

impl Connector for MailboxConnector {
    fn info(&self) -> &'static TypeInfo {
        &INFO
    }

    fn account(&self) -> &Account {
        &self.account
    }

    fn test(&self) -> (bool, String) {
        match self.backend().has_token(&self.account.id) {
            Err((kind, detail)) => (false, format!("{kind}: {detail}")),
            Ok(false) => (
                false,
                "Not authorized - finish setup on the Inbox Triage page".to_string(),
            ),
            Ok(true) => (true, "Authorized".to_string()),
        }
    }

    fn search_messages(&self, query: &str, limit: u32) -> ConnResult<Vec<Value>> {
        self.backend()
            .open(&self.account.id)?
            .search_messages(query, limit)
    }

    fn read_message(&self, message_id: &str) -> ConnResult<Value> {
        self.backend()
            .open(&self.account.id)?
            .read_message(message_id)
    }

    fn send_message(
        &self,
        to: &[String],
        cc: &[String],
        subject: &str,
        body: &str,
    ) -> ConnResult<Value> {
        self.backend()
            .open(&self.account.id)?
            .send_message(to, cc, subject, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    use crate::connectors::testutil::ENV_LOCK as ENV;

    #[derive(Default)]
    struct Log(Mutex<Vec<String>>);

    struct Access(Arc<Log>);
    impl MailAccess for Access {
        fn search_messages(&mut self, q: &str, l: u32) -> ConnResult<Vec<Value>> {
            self.0 .0.lock().unwrap().push(format!("search {q} {l}"));
            Ok(vec![json!({"id": "m1"})])
        }
        fn read_message(&mut self, id: &str) -> ConnResult<Value> {
            self.0 .0.lock().unwrap().push(format!("read {id}"));
            Ok(json!({"id": id}))
        }
        fn send_message(
            &mut self,
            to: &[String],
            _cc: &[String],
            s: &str,
            b: &str,
        ) -> ConnResult<Value> {
            self.0
                 .0
                .lock()
                .unwrap()
                .push(format!("send {} {s} {b}", to.join(",")));
            Ok(json!({"id": "g1", "thread_id": "t"}))
        }
    }

    struct Backend {
        log: Arc<Log>,
        token: Result<bool, (String, String)>,
        open_err: Option<String>,
    }

    impl MailBackend for Backend {
        fn has_token(&self, _id: &str) -> Result<bool, (String, String)> {
            self.token.clone()
        }
        fn open(&self, _id: &str) -> ConnResult<Box<dyn MailAccess>> {
            match &self.open_err {
                Some(m) => Err(ConnectorError::msg(m.clone())),
                None => Ok(Box::new(Access(self.log.clone()))),
            }
        }
    }

    fn make(
        token: Result<bool, (String, String)>,
        open_err: Option<&str>,
    ) -> (MailboxConnector, Arc<Log>) {
        let log = Arc::new(Log::default());
        let backend = Backend {
            log: log.clone(),
            token,
            open_err: open_err.map(str::to_string),
        };
        (
            MailboxConnector::new(Account::new("m1", "mailbox")).with_backend(Arc::new(backend)),
            log,
        )
    }

    #[test]
    fn test_reports_the_token_state() {
        assert_eq!(
            make(Ok(true), None).0.test(),
            (true, "Authorized".to_string())
        );
        let (ok, msg) = make(Ok(false), None).0.test();
        assert!(!ok && msg.starts_with("Not authorized"));
        let (ok, msg) = make(Err(("KeychainError".into(), "nope".into())), None)
            .0
            .test();
        assert!(!ok && msg == "KeychainError: nope");
    }

    #[test]
    fn search_read_and_send_go_through_the_opened_mailbox() {
        let (c, log) = make(Ok(true), None);
        assert_eq!(
            c.search_messages("invoice", 5).unwrap(),
            vec![json!({"id": "m1"})]
        );
        assert_eq!(c.read_message("m1").unwrap(), json!({"id": "m1"}));
        assert_eq!(
            c.send_message(&["a@x.com".into()], &[], "S", "B").unwrap(),
            json!({"id": "g1", "thread_id": "t"})
        );
        assert_eq!(
            *log.0.lock().unwrap(),
            ["search invoice 5", "read m1", "send a@x.com S B"]
        );
    }

    #[test]
    fn open_failures_surface_as_token_free_connector_errors() {
        let (c, _) = make(Ok(true), Some("work is not connected"));
        let e = c.search_messages("x", 1).unwrap_err();
        assert!(matches!(&e, ConnectorError::Message(m) if m.contains("not connected")));
    }

    #[test]
    fn the_default_backend_reports_an_unconfigured_mailbox() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = tempfile::tempdir().unwrap();
        std::env::set_var("LOOP_ENGINEERING_HOME", d.path());
        std::env::set_var(
            "LOOPX_KEYCHAIN",
            format!("file:{}", d.path().join("kc").display()),
        );
        let c = MailboxConnector::new(Account::new("m1", "mailbox"));
        assert_eq!(
            c.test(),
            (
                false,
                "Not authorized - finish setup on the Inbox Triage page".to_string()
            )
        );
        let e = c.read_message("x").unwrap_err();
        assert_eq!(e.to_string(), "Mailbox is not configured");
        std::env::remove_var("LOOPX_KEYCHAIN");
    }
}
