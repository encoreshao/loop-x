//! LinkedIn connector: the user's own signed-in browser session (no API token,
//! no password stored). The session is created by the dashboard's Connect
//! LinkedIn button and used by the LinkedIn Autopilot loop. Only the
//! non-browser part lives here: the connected-marker check and the failure
//! wording; the headless probe itself is a [`LinkedInProbe`] the browser
//! stream registers.

use super::*;
use std::path::PathBuf;
use std::sync::OnceLock;

pub const MARKER: &str = "connected";

pub const INFO: TypeInfo = TypeInfo {
    name: "linkedin",
    label: "LinkedIn",
    icon: "hub",
    capabilities: &[SOCIAL],
    fields: &[],
    secret_label: None,
    external: false,
    brand: "linkedin",
    description: "Read your LinkedIn feed, notifications and messages through your own signed-in browser session.",
    docs_url: "https://www.linkedin.com/help/linkedin",
    category: "other",
    presets: &[],
    auth: Auth::BrowserSession,
};

/// Why the headless probe failed (the `linkedin_browser` exceptions).
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeFailure {
    BrowserUnavailable,
    SessionExpired,
    Challenge,
    /// Any other failure: only the class name is ever shown.
    Other(String),
}

/// Opens the account's session headlessly and checks it is still signed in.
pub trait LinkedInProbe: Send + Sync {
    fn session_ok(&self, account_id: &str) -> Result<(), ProbeFailure>;
}

static PROBE: OnceLock<Arc<dyn LinkedInProbe>> = OnceLock::new();

/// The browser stream plugs in its real probe here (once, at startup).
pub fn register_probe(probe: Arc<dyn LinkedInProbe>) -> bool {
    PROBE.set(probe).is_ok()
}

struct NoBrowser;

impl LinkedInProbe for NoBrowser {
    fn session_ok(&self, _account_id: &str) -> Result<(), ProbeFailure> {
        Err(ProbeFailure::BrowserUnavailable)
    }
}

/// `<LOOP_ENGINEERING_HOME>/linkedin` (`linkedin_session.base_dir`).
pub fn base_dir() -> PathBuf {
    crate::config::home_dir().join("linkedin")
}

/// The per-account session directory; `None` for an invalid account id.
pub fn account_dir(account_id: &str) -> Option<PathBuf> {
    crate::connectors_config::is_valid_id(account_id).then(|| base_dir().join(account_id))
}

/// `linkedin_session.is_connected`: the connected marker exists.
pub fn is_connected(account_id: &str) -> bool {
    account_dir(account_id).is_some_and(|d| d.join(MARKER).is_file())
}

pub struct LinkedInConnector {
    account: Account,
    probe: Option<Arc<dyn LinkedInProbe>>,
}

impl LinkedInConnector {
    pub fn new(account: Account) -> Self {
        Self {
            account,
            probe: None,
        }
    }

    pub fn with_probe(mut self, probe: Arc<dyn LinkedInProbe>) -> Self {
        self.probe = Some(probe);
        self
    }
}

impl Connector for LinkedInConnector {
    fn info(&self) -> &'static TypeInfo {
        &INFO
    }

    fn account(&self) -> &Account {
        &self.account
    }

    fn test(&self) -> (bool, String) {
        if !is_connected(&self.account.id) {
            return (false, "Not connected - use Connect LinkedIn".to_string());
        }
        let default: Arc<dyn LinkedInProbe> =
            PROBE.get().cloned().unwrap_or_else(|| Arc::new(NoBrowser));
        let probe = self.probe.clone().unwrap_or(default);
        match probe.session_ok(&self.account.id) {
            Ok(()) => (true, "Signed in to LinkedIn".to_string()),
            Err(ProbeFailure::BrowserUnavailable) => (
                false,
                "Playwright or Google Chrome is not available - install Playwright (python3 -m pip install playwright) and Google Chrome".to_string(),
            ),
            Err(ProbeFailure::SessionExpired) => (false, "LinkedIn session expired - reconnect".to_string()),
            Err(ProbeFailure::Challenge) => (
                false,
                "LinkedIn showed a security check - open LinkedIn in a normal browser, then reconnect".to_string(),
            ),
            Err(ProbeFailure::Other(class)) => (false, format!("Could not open Chrome ({class})")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::connectors::testutil::ENV_LOCK as ENV;

    struct Fake(Result<(), ProbeFailure>);
    impl LinkedInProbe for Fake {
        fn session_ok(&self, _id: &str) -> Result<(), ProbeFailure> {
            self.0.clone()
        }
    }

    fn with_home<R>(f: impl FnOnce(&std::path::Path) -> R) -> R {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = tempfile::tempdir().unwrap();
        std::env::set_var("LOOP_ENGINEERING_HOME", d.path());
        f(d.path())
    }

    fn connect(home: &std::path::Path) {
        std::fs::create_dir_all(home.join("linkedin").join("li")).unwrap();
        std::fs::write(home.join("linkedin").join("li").join(MARKER), "").unwrap();
    }

    #[test]
    fn type_metadata() {
        assert_eq!(INFO.capabilities, &[SOCIAL]);
        assert_eq!((INFO.auth, INFO.secret_label), (Auth::BrowserSession, None));
        assert!(INFO.fields.is_empty());
        assert!(INFO.validate(&Map::new(), "", true).is_empty());
    }

    #[test]
    fn not_connected_is_reported_without_probing() {
        with_home(|_| {
            let c = LinkedInConnector::new(Account::new("li", "linkedin")).with_probe(Arc::new(
                Fake(Err(ProbeFailure::Other("must not run".into()))),
            ));
            let (ok, msg) = c.test();
            assert!(!ok && msg.contains("Connect LinkedIn"));
        });
    }

    #[test]
    fn connected_and_signed_in() {
        with_home(|h| {
            connect(h);
            let c = LinkedInConnector::new(Account::new("li", "linkedin"))
                .with_probe(Arc::new(Fake(Ok(()))));
            assert_eq!(c.test(), (true, "Signed in to LinkedIn".to_string()));
        });
    }

    #[test]
    fn failures_are_plain_messages() {
        with_home(|h| {
            connect(h);
            for (failure, needle) in [
                (ProbeFailure::SessionExpired, "expired"),
                (ProbeFailure::Challenge, "security check"),
                (ProbeFailure::BrowserUnavailable, "Playwright"),
                (ProbeFailure::Other("RuntimeError".into()), "RuntimeError"),
            ] {
                let c = LinkedInConnector::new(Account::new("li", "linkedin"))
                    .with_probe(Arc::new(Fake(Err(failure))));
                let (ok, msg) = c.test();
                assert!(!ok && msg.contains(needle), "{msg}");
            }
        });
    }

    #[test]
    fn invalid_account_ids_never_reach_the_filesystem() {
        assert!(account_dir("../x").is_none());
        assert!(!is_connected("../x"));
    }
}
