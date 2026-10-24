//! Connector secrets (tokens, API keys, webhook URLs): the Keychain service
//! `loop-engineering.connectors` (mirrors `bin/secret_store.py`), a separate
//! service from the mailbox tokens so deleting a connector can never touch a
//! mailbox token. The store itself is [`crate::keychain`]'s: the macOS
//! Keychain through `security`, sandbox-suffixed under `LOOP_ENGINEERING_HOME`,
//! or a file store when `LOOPX_KEYCHAIN=file:<dir>` (tests, CI).

pub use crate::keychain::{SecretError, SecretStore};

pub const SERVICE_CONNECTORS: &str = "loop-engineering.connectors";

/// The connector-secret store for this environment.
pub fn open_connector_store() -> Result<Box<dyn SecretStore>, SecretError> {
    crate::keychain::open_store(SERVICE_CONNECTORS)
}
