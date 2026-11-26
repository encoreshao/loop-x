//! Secret storage behind a trait: the macOS Keychain, a file-backed store for
//! tests and CI, and an in-memory double.
//!
//! Which backend `open_store` picks:
//! - `LOOPX_KEYCHAIN=file:<dir>` selects the file store (mode 0600 files under `<dir>`).
//!   Every test and sandbox must set it: nothing below ever reaches the real Keychain then.
//! - `LOOPX_KEYCHAIN=keychain:<file>` uses a scratch keychain file (`security
//!   create-keychain`) instead of the login keychain, to check the Keychain backends.
//! - otherwise the macOS Keychain. The service name gets a `.sandbox-<hash>` suffix whenever
//!   `LOOP_ENGINEERING_HOME` is set (same policy as `bin/mail_auth.py`), so a sandboxed run
//!   can never read or overwrite a real secret.
//!
//! A *reference* (`keychain:<service>/<account>`) is what config files keep instead of a
//! plaintext secret; `resolve_value` turns one back into the secret.

use crate::proc;
use crate::sha256::{hex, sha256, unhex};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// Service for tokens/webhooks moved out of `~/.gitlab` and `~/.slack`.
pub const SERVICE_CONFIG: &str = "loop-engineering.config";
/// Service for OAuth token sets (one JSON secret per `<provider>/<account>`).
pub const SERVICE_OAUTH: &str = "loop-engineering.oauth";
/// Only services under this prefix may be named by a reference.
pub const SERVICE_PREFIX: &str = "loop-engineering.";
pub const REF_PREFIX: &str = "keychain:";

const SECURITY_BIN: &str = "/usr/bin/security";
const TIMEOUT: Duration = Duration::from_secs(10);
const NOT_FOUND: i32 = 44;
const INDEX_ACCOUNT: &str = "__index__";

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secret store unavailable: {0}")]
    Unavailable(String),
    #[error("secret store error: {0}")]
    Failed(String),
    #[error("secret not found: {0}")]
    NotFound(String),
    #[error("refusing to store a value containing a line break or NUL in the Keychain")]
    BadValue,
}

pub trait SecretStore: Send + Sync {
    /// The (possibly sandbox-suffixed) service this store writes to.
    fn service(&self) -> String;
    fn get(&self, account: &str) -> Result<Option<String>, SecretError>;
    fn set(&self, account: &str, secret: &str) -> Result<(), SecretError>;
    /// Deleting a missing account is not an error.
    fn delete(&self, account: &str) -> Result<(), SecretError>;
    /// Accounts under this service, sorted (never the values).
    fn list(&self) -> Result<Vec<String>, SecretError>;
}

/// `base` suffixed with `.sandbox-<sha256(realpath(home))[..10]>` when `home` is set.
pub fn sandboxed_service_for(base: &str, home: Option<&Path>) -> String {
    match home {
        None => base.to_string(),
        Some(h) => {
            let real = std::fs::canonicalize(h).unwrap_or_else(|_| h.to_path_buf());
            let digest = hex(&sha256(real.to_string_lossy().as_bytes()));
            format!("{base}.sandbox-{}", &digest[..10])
        }
    }
}

pub fn sandboxed_service(base: &str) -> String {
    let home = std::env::var_os("LOOP_ENGINEERING_HOME").filter(|v| !v.is_empty());
    sandboxed_service_for(base, home.as_deref().map(Path::new))
}

// ---------------------------------------------------------------- in-memory

#[derive(Default)]
pub struct MemoryStore {
    service: String,
    items: Mutex<BTreeMap<String, String>>,
}

impl MemoryStore {
    pub fn new(service: &str) -> Self {
        Self {
            service: service.to_string(),
            items: Mutex::default(),
        }
    }
}

impl SecretStore for MemoryStore {
    fn service(&self) -> String {
        self.service.clone()
    }
    fn get(&self, account: &str) -> Result<Option<String>, SecretError> {
        Ok(self.items.lock().unwrap().get(account).cloned())
    }
    fn set(&self, account: &str, secret: &str) -> Result<(), SecretError> {
        self.items
            .lock()
            .unwrap()
            .insert(account.to_string(), secret.to_string());
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<(), SecretError> {
        self.items.lock().unwrap().remove(account);
        Ok(())
    }
    fn list(&self) -> Result<Vec<String>, SecretError> {
        Ok(self.items.lock().unwrap().keys().cloned().collect())
    }
}

// --------------------------------------------------------------- file-backed

/// `<dir>/<service>/<hex(account)>`, files 0600, directories 0700.
pub struct FileStore {
    dir: PathBuf,
    service: String,
}

impl FileStore {
    pub fn new(dir: &Path, service: &str) -> Self {
        Self {
            dir: dir.to_path_buf(),
            service: service.to_string(),
        }
    }

    fn service_dir(&self) -> PathBuf {
        self.dir.join(&self.service)
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}
#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) {}

impl SecretStore for FileStore {
    fn service(&self) -> String {
        self.service.clone()
    }
    fn get(&self, account: &str) -> Result<Option<String>, SecretError> {
        match std::fs::read_to_string(self.service_dir().join(hex(account.as_bytes()))) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SecretError::Failed(e.to_string())),
        }
    }
    fn set(&self, account: &str, secret: &str) -> Result<(), SecretError> {
        let dir = self.service_dir();
        let fail = |e: std::io::Error| SecretError::Failed(e.to_string());
        std::fs::create_dir_all(&dir).map_err(fail)?;
        set_mode(&self.dir, 0o700);
        set_mode(&dir, 0o700);
        let path = dir.join(hex(account.as_bytes()));
        let tmp = dir.join(format!(".tmp-{}", std::process::id()));
        std::fs::write(&tmp, secret).map_err(fail)?;
        set_mode(&tmp, 0o600);
        std::fs::rename(&tmp, &path).map_err(fail)
    }
    fn delete(&self, account: &str) -> Result<(), SecretError> {
        match std::fs::remove_file(self.service_dir().join(hex(account.as_bytes()))) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SecretError::Failed(e.to_string())),
        }
    }
    fn list(&self) -> Result<Vec<String>, SecretError> {
        let Ok(rd) = std::fs::read_dir(self.service_dir()) else {
            return Ok(Vec::new());
        };
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| unhex(&e.file_name().to_string_lossy()))
            .filter_map(|b| String::from_utf8(b).ok())
            .collect();
        names.sort();
        Ok(names)
    }
}

// ----------------------------------------------------------- macOS Keychain

/// Why a backend could not use an item.
pub(crate) enum RawError {
    /// The item exists but its access list does not trust this backend.
    NotOurs,
    Other(SecretError),
}

impl From<SecretError> for RawError {
    fn from(e: SecretError) -> Self {
        RawError::Other(e)
    }
}

fn plain(e: RawError) -> SecretError {
    match e {
        RawError::NotOurs => {
            SecretError::Failed("the Keychain item belongs to another app".to_string())
        }
        RawError::Other(e) => e,
    }
}

/// Generic passwords in the login keychain, addressed by service and account.
pub(crate) trait RawKeychain: Send + Sync {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, RawError>;
    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), RawError>;
    /// Deleting a missing item is not an error.
    fn delete(&self, service: &str, account: &str) -> Result<(), RawError>;
}

/// Through `/usr/bin/security`, which every item it writes trusts. Secrets are
/// written through `security -i` on **stdin**, never on a command line (argv is
/// visible to every local user via `ps`; OAuth refresh tokens rotate and would be
/// exposed again and again).
struct SecurityCli {
    binary: PathBuf,
    /// A keychain file instead of the default search list.
    keychain: Option<PathBuf>,
}

impl SecurityCli {
    fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<proc::ProcOutput, SecretError> {
        let mut cmd = std::process::Command::new(&self.binary);
        cmd.args(args);
        if let (Some(k), None) = (&self.keychain, stdin) {
            cmd.arg(k);
        }
        let out = proc::run(&mut cmd, stdin, Some(TIMEOUT)).map_err(|e| {
            SecretError::Unavailable(format!("cannot run {}: {e}", self.binary.display()))
        })?;
        if out.timed_out {
            return Err(SecretError::Unavailable(
                "Keychain did not answer in time - is it locked?".into(),
            ));
        }
        Ok(out)
    }
}

impl RawKeychain for SecurityCli {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, RawError> {
        let out = self.run(
            &["find-generic-password", "-s", service, "-a", account, "-w"],
            None,
        )?;
        match out.code {
            Some(0) => Ok(Some(out.stdout.trim_end_matches('\n').to_string())),
            Some(NOT_FOUND) => Ok(None),
            c => Err(SecretError::Failed(format!(
                "Keychain read failed (exit {c:?}): {}",
                out.stderr.trim()
            ))
            .into()),
        }
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), RawError> {
        let command = [
            "add-generic-password",
            "-U",
            "-s",
            &quote(service)?,
            "-a",
            &quote(account)?,
            "-w",
            &quote(secret)?,
            "-T",
            SECURITY_BIN,
        ]
        .join(" ");
        let command = match &self.keychain {
            Some(k) => format!("{command} {}", quote(&k.to_string_lossy())?),
            None => command,
        };
        let out = self.run(&["-i"], Some(&format!("{command}\n")))?;
        if out.code != Some(0) {
            return Err(SecretError::Failed(format!(
                "Keychain write failed (exit {:?}): {}",
                out.code,
                out.stderr.trim()
            ))
            .into());
        }
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), RawError> {
        let out = self.run(
            &["delete-generic-password", "-s", service, "-a", account],
            None,
        )?;
        match out.code {
            Some(0) | Some(NOT_FOUND) => Ok(()),
            c => Err(SecretError::Failed(format!(
                "Keychain delete failed (exit {c:?}): {}",
                out.stderr.trim()
            ))
            .into()),
        }
    }
}

/// Through the Security framework from this process, so a new item trusts only
/// this signed app: any other program asking for it (`security
/// find-generic-password` from an agent's shell included) gets a prompt. Never
/// shows one itself: an item it does not own is [`RawError::NotOurs`].
#[cfg(target_os = "macos")]
struct Framework {
    keychain: Option<PathBuf>,
}

#[cfg(target_os = "macos")]
mod framework {
    use super::{Framework, RawError, RawKeychain, SecretError};
    use security_framework::os::macos::keychain::SecKeychain;
    use security_framework::os::macos::keychain_item::SecKeychainItem;
    use security_framework::os::macos::passwords::{
        find_generic_password, SecKeychainItemPassword,
    };
    use std::sync::Mutex;

    const ITEM_NOT_FOUND: i32 = -25300;
    const INTERACTION_NOT_ALLOWED: i32 = -25308;
    const AUTH_FAILED: i32 = -25293;

    /// Turning user interaction off is process-wide: one call at a time.
    static QUIET: Mutex<()> = Mutex::new(());

    fn raw(e: security_framework::base::Error) -> RawError {
        match e.code() {
            INTERACTION_NOT_ALLOWED | AUTH_FAILED => RawError::NotOurs,
            code => RawError::Other(SecretError::Failed(format!(
                "Keychain error {code}: {}",
                e.message().unwrap_or_default()
            ))),
        }
    }

    fn quietly<T>(f: impl FnOnce() -> Result<T, RawError>) -> Result<T, RawError> {
        let _one = QUIET.lock().unwrap_or_else(|p| p.into_inner());
        let _no_prompt = SecKeychain::disable_user_interaction().map_err(raw)?;
        f()
    }

    impl Framework {
        fn keychain(&self) -> security_framework::base::Result<Option<SecKeychain>> {
            self.keychain.as_ref().map(SecKeychain::open).transpose()
        }

        fn find(
            &self,
            service: &str,
            account: &str,
        ) -> security_framework::base::Result<(SecKeychainItemPassword, SecKeychainItem)> {
            match self.keychain()? {
                Some(k) => k.find_generic_password(service, account),
                None => find_generic_password(None, service, account),
            }
        }

        fn add(
            &self,
            service: &str,
            account: &str,
            secret: &str,
        ) -> security_framework::base::Result<()> {
            let keychain = match self.keychain()? {
                Some(k) => k,
                None => SecKeychain::default()?,
            };
            keychain.add_generic_password(service, account, secret.as_bytes())
        }
    }

    impl RawKeychain for Framework {
        fn get(&self, service: &str, account: &str) -> Result<Option<String>, RawError> {
            quietly(|| match self.find(service, account) {
                Ok((password, _)) => String::from_utf8(password.to_vec())
                    .map(Some)
                    .map_err(|_| RawError::Other(SecretError::Failed("not UTF-8".into()))),
                Err(e) if e.code() == ITEM_NOT_FOUND => Ok(None),
                Err(e) => Err(raw(e)),
            })
        }

        fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), RawError> {
            quietly(|| match self.find(service, account) {
                Ok((_, mut item)) => item.set_password(secret.as_bytes()).map_err(raw),
                Err(e) if e.code() == ITEM_NOT_FOUND => {
                    self.add(service, account, secret).map_err(raw)
                }
                Err(e) => Err(raw(e)),
            })
        }

        fn delete(&self, service: &str, account: &str) -> Result<(), RawError> {
            quietly(|| match self.find(service, account) {
                Ok((_, item)) => {
                    item.delete();
                    Ok(())
                }
                Err(e) if e.code() == ITEM_NOT_FOUND => Ok(()),
                Err(e) => Err(raw(e)),
            })
        }
    }

    pub(super) fn signed_by_apple_issued_identity() -> bool {
        use security_framework::os::macos::code_signing::{Flags, SecCode, SecRequirement};
        let Ok(requirement) = "anchor apple generic".parse::<SecRequirement>() else {
            return false;
        };
        SecCode::for_self(Flags::NONE)
            .and_then(|code| code.check_validity(Flags::NONE, &requirement))
            .is_ok()
    }
}

/// Is this binary signed with an Apple-issued identity (Apple Development or
/// Developer ID)? Only then does a code signature survive a rebuild, so only then
/// can the Keychain trust this app alone without prompting after every update.
/// `LOOPX_KEYCHAIN_LOCK=off` turns the lock off.
pub fn locked_to_this_app() -> bool {
    static LOCKED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *LOCKED.get_or_init(|| {
        if std::env::var("LOOPX_KEYCHAIN_LOCK").is_ok_and(|v| v == "off") {
            return false;
        }
        #[cfg(target_os = "macos")]
        {
            framework::signed_by_apple_issued_identity()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    })
}

/// Generic passwords in the login keychain. Keychain cannot enumerate by service
/// without a dump, so the account names are also kept in one `__index__` item
/// (names only, never values).
///
/// Locked (see [`locked_to_this_app`]): items live under the Security framework
/// and trust only this app. An item still written the old way (it trusts
/// `/usr/bin/security`) is read through `security` once and moved under the lock.
pub struct KeychainStore {
    service: String,
    primary: Box<dyn RawKeychain>,
    legacy: Option<Box<dyn RawKeychain>>,
}

impl KeychainStore {
    pub fn new(service: &str) -> Self {
        Self::in_file(service, None)
    }

    /// In the keychain file `keychain` instead of the default search list.
    pub fn in_file(service: &str, keychain: Option<&Path>) -> Self {
        let cli = Box::new(SecurityCli {
            binary: PathBuf::from(SECURITY_BIN),
            keychain: keychain.map(Path::to_path_buf),
        });
        #[cfg(target_os = "macos")]
        if locked_to_this_app() {
            let framework = Framework {
                keychain: keychain.map(Path::to_path_buf),
            };
            return Self::with_backends(service, Box::new(framework), Some(cli));
        }
        Self::with_backends(service, cli, None)
    }

    pub fn with_binary(service: &str, binary: &Path) -> Self {
        Self::with_backends(
            service,
            Box::new(SecurityCli {
                binary: binary.to_path_buf(),
                keychain: None,
            }),
            None,
        )
    }

    pub(crate) fn with_backends(
        service: &str,
        primary: Box<dyn RawKeychain>,
        legacy: Option<Box<dyn RawKeychain>>,
    ) -> Self {
        Self {
            service: service.to_string(),
            primary,
            legacy,
        }
    }

    fn raw_get(&self, account: &str) -> Result<Option<String>, SecretError> {
        let legacy = match (self.primary.get(&self.service, account), &self.legacy) {
            (Err(RawError::NotOurs), Some(legacy)) => legacy,
            (other, _) => return other.map_err(plain),
        };
        let found = legacy.get(&self.service, account).map_err(plain)?;
        if let Some(secret) = &found {
            self.adopt(legacy.as_ref(), account, secret);
        }
        Ok(found)
    }

    /// Re-writes an old item under the lock; best-effort, and the old item is put
    /// back if the new one cannot be written.
    fn adopt(&self, legacy: &dyn RawKeychain, account: &str, secret: &str) {
        if legacy.delete(&self.service, account).is_err() {
            return;
        }
        if self.primary.set(&self.service, account, secret).is_err() {
            let _ = legacy.set(&self.service, account, secret);
        }
    }

    fn raw_set(&self, account: &str, secret: &str) -> Result<(), SecretError> {
        match (
            self.primary.set(&self.service, account, secret),
            &self.legacy,
        ) {
            (Err(RawError::NotOurs), Some(legacy)) => {
                legacy.delete(&self.service, account).map_err(plain)?;
                if let Err(e) = self.primary.set(&self.service, account, secret) {
                    let _ = legacy.set(&self.service, account, secret);
                    return Err(plain(e));
                }
                Ok(())
            }
            (other, _) => other.map_err(plain),
        }
    }

    fn raw_delete(&self, account: &str) -> Result<(), SecretError> {
        match (self.primary.delete(&self.service, account), &self.legacy) {
            (Err(RawError::NotOurs), Some(legacy)) => {
                legacy.delete(&self.service, account).map_err(plain)
            }
            (other, _) => other.map_err(plain),
        }
    }

    fn index(&self) -> Result<Vec<String>, SecretError> {
        Ok(self
            .raw_get(INDEX_ACCOUNT)?
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default())
    }

    fn write_index(&self, mut names: Vec<String>) -> Result<(), SecretError> {
        names.sort();
        names.dedup();
        self.raw_set(
            INDEX_ACCOUNT,
            &serde_json::to_string(&names).unwrap_or_default(),
        )
    }
}

/// A double-quoted argument for `security -i`'s own tokenizer (it honours backslash
/// escapes inside double quotes). A line break would end the command, so it is refused.
fn quote(value: &str) -> Result<String, SecretError> {
    if value.contains(['\n', '\r', '\0']) {
        return Err(SecretError::BadValue);
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

impl SecretStore for KeychainStore {
    fn service(&self) -> String {
        self.service.clone()
    }
    fn get(&self, account: &str) -> Result<Option<String>, SecretError> {
        self.raw_get(account)
    }
    fn set(&self, account: &str, secret: &str) -> Result<(), SecretError> {
        self.raw_set(account, secret)?;
        if account != INDEX_ACCOUNT {
            let mut names = self.index()?;
            if !names.iter().any(|n| n == account) {
                names.push(account.to_string());
                self.write_index(names)?;
            }
        }
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<(), SecretError> {
        self.raw_delete(account)?;
        let mut names = self.index()?;
        if names.iter().any(|n| n == account) {
            names.retain(|n| n != account);
            self.write_index(names)?;
        }
        Ok(())
    }
    fn list(&self) -> Result<Vec<String>, SecretError> {
        let mut names = self.index()?;
        names.sort();
        Ok(names)
    }
}

// ------------------------------------------------------------------ selection

/// The store for `base_service` (e.g. [`SERVICE_CONFIG`]); see the module docs.
pub fn open_store(base_service: &str) -> Result<Box<dyn SecretStore>, SecretError> {
    let service = sandboxed_service(base_service);
    match std::env::var("LOOPX_KEYCHAIN")
        .ok()
        .filter(|v| !v.is_empty())
    {
        Some(spec) => match (spec.strip_prefix("file:"), spec.strip_prefix("keychain:")) {
            (Some(dir), _) if !dir.is_empty() => {
                Ok(Box::new(FileStore::new(Path::new(dir), &service)))
            }
            (_, Some(file)) if !file.is_empty() => Ok(Box::new(KeychainStore::in_file(
                &service,
                Some(Path::new(file)),
            ))),
            _ => Err(SecretError::Unavailable(format!(
                "LOOPX_KEYCHAIN must be file:<dir> or keychain:<keychain file>, got {spec:?}"
            ))),
        },
        None => Ok(Box::new(KeychainStore::new(&service))),
    }
}

// ----------------------------------------------------------------- references

pub fn format_ref(service: &str, account: &str) -> String {
    format!("{REF_PREFIX}{service}/{account}")
}

/// `keychain:<service>/<account>` -> (service, account). The service must live under
/// [`SERVICE_PREFIX`], so a config file cannot name an arbitrary Keychain item.
pub fn parse_ref(value: &str) -> Option<(&str, &str)> {
    let rest = value.strip_prefix(REF_PREFIX)?;
    let (service, account) = rest.split_once('/')?;
    (service.starts_with(SERVICE_PREFIX) && !account.is_empty()).then_some((service, account))
}

pub fn is_ref(value: &str) -> bool {
    value.starts_with(REF_PREFIX)
}

/// A config value that is either a plain secret (returned as is) or a reference
/// (looked up in the store it names).
pub fn resolve_value(value: &str) -> Result<String, SecretError> {
    resolve_value_with(value, &open_store)
}

pub type StoreOpener<'a> = dyn Fn(&str) -> Result<Box<dyn SecretStore>, SecretError> + 'a;

/// [`resolve_value`] with the store opener injected (tests).
pub fn resolve_value_with(value: &str, open: &StoreOpener) -> Result<String, SecretError> {
    if !is_ref(value) {
        return Ok(value.to_string());
    }
    let (service, account) = parse_ref(value)
        .ok_or_else(|| SecretError::Failed("malformed secret reference".to_string()))?;
    open(service)?
        .get(account)?
        .ok_or_else(|| SecretError::NotFound(value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn sandbox_suffix_is_stable_and_home_specific() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        assert_eq!(sandboxed_service_for("svc", None), "svc");
        let sa = sandboxed_service_for("svc", Some(a.path()));
        assert!(sa.starts_with("svc.sandbox-") && sa.len() == "svc.sandbox-".len() + 10);
        assert_eq!(sa, sandboxed_service_for("svc", Some(a.path())));
        assert_ne!(sa, sandboxed_service_for("svc", Some(b.path())));
    }

    fn exercise(store: &dyn SecretStore) {
        assert_eq!(store.get("a/b").unwrap(), None);
        store.set("a/b", "s3cret \"q\" \\ x").unwrap();
        store.set("c", "other").unwrap();
        assert_eq!(store.get("a/b").unwrap().unwrap(), "s3cret \"q\" \\ x");
        assert_eq!(store.list().unwrap(), vec!["a/b", "c"]);
        store.set("a/b", "new").unwrap();
        assert_eq!(store.get("a/b").unwrap().unwrap(), "new");
        store.delete("a/b").unwrap();
        store.delete("a/b").unwrap();
        assert_eq!(store.list().unwrap(), vec!["c"]);
    }

    #[test]
    fn memory_and_file_stores_behave_the_same() {
        exercise(&MemoryStore::new("svc"));
        let d = tempfile::tempdir().unwrap();
        exercise(&FileStore::new(d.path(), "svc"));
    }

    #[cfg(unix)]
    #[test]
    fn file_store_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let s = FileStore::new(d.path(), "svc");
        s.set("acct", "v").unwrap();
        let f = d.path().join("svc").join(hex(b"acct"));
        assert_eq!(
            std::fs::metadata(f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(d.path().join("svc"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn resolve_follows_references_and_reports_missing_ones() {
        let store = std::sync::Arc::new(MemoryStore::new("s"));
        store
            .set("gitlab/instances/work/token", "glpat-real")
            .unwrap();
        struct Shared(std::sync::Arc<MemoryStore>);
        impl SecretStore for Shared {
            fn service(&self) -> String {
                self.0.service()
            }
            fn get(&self, a: &str) -> Result<Option<String>, SecretError> {
                self.0.get(a)
            }
            fn set(&self, a: &str, s: &str) -> Result<(), SecretError> {
                self.0.set(a, s)
            }
            fn delete(&self, a: &str) -> Result<(), SecretError> {
                self.0.delete(a)
            }
            fn list(&self) -> Result<Vec<String>, SecretError> {
                self.0.list()
            }
        }
        let open = |_: &str| -> Result<Box<dyn SecretStore>, SecretError> {
            Ok(Box::new(Shared(store.clone())))
        };
        let r = format_ref(SERVICE_CONFIG, "gitlab/instances/work/token");
        assert_eq!(resolve_value_with(&r, &open).unwrap(), "glpat-real");
        assert_eq!(resolve_value_with("plain", &open).unwrap(), "plain");
        let missing = format_ref(SERVICE_CONFIG, "nope");
        assert!(matches!(
            resolve_value_with(&missing, &open),
            Err(SecretError::NotFound(_))
        ));
        assert!(resolve_value_with("keychain:bad", &open).is_err());
    }

    #[test]
    fn references_round_trip_and_reject_foreign_services() {
        let r = format_ref(SERVICE_CONFIG, "gitlab/instances/work/token");
        assert_eq!(
            parse_ref(&r),
            Some((SERVICE_CONFIG, "gitlab/instances/work/token"))
        );
        assert_eq!(parse_ref("keychain:Chrome Safe Storage/x"), None);
        assert_eq!(parse_ref("keychain:loop-engineering.x/"), None);
        assert_eq!(parse_ref("glpat-plain"), None);
        assert_eq!(resolve_value("glpat-plain").unwrap(), "glpat-plain");
    }

    /// One login keychain shared by fake backends; each item remembers which
    /// backend wrote it, and a backend can only use its own items.
    type Shelf = Arc<Mutex<BTreeMap<(String, String), (&'static str, String)>>>;

    struct Fake {
        me: &'static str,
        shelf: Shelf,
        fail_set: bool,
    }

    impl RawKeychain for Fake {
        fn get(&self, service: &str, account: &str) -> Result<Option<String>, RawError> {
            match self
                .shelf
                .lock()
                .unwrap()
                .get(&(service.into(), account.into()))
            {
                None => Ok(None),
                Some((owner, _)) if *owner != self.me => Err(RawError::NotOurs),
                Some((_, v)) => Ok(Some(v.clone())),
            }
        }
        fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), RawError> {
            if self.fail_set {
                return Err(RawError::Other(SecretError::Failed("full".into())));
            }
            let mut shelf = self.shelf.lock().unwrap();
            let key = (service.to_string(), account.to_string());
            if shelf.get(&key).is_some_and(|(o, _)| *o != self.me) {
                return Err(RawError::NotOurs);
            }
            shelf.insert(key, (self.me, secret.to_string()));
            Ok(())
        }
        fn delete(&self, service: &str, account: &str) -> Result<(), RawError> {
            let mut shelf = self.shelf.lock().unwrap();
            let key = (service.to_string(), account.to_string());
            match shelf.get(&key) {
                Some((o, _)) if *o != self.me => Err(RawError::NotOurs),
                _ => {
                    shelf.remove(&key);
                    Ok(())
                }
            }
        }
    }

    fn locked(shelf: &Shelf, fail_set: bool) -> KeychainStore {
        KeychainStore::with_backends(
            "svc",
            Box::new(Fake {
                me: "loopx",
                shelf: shelf.clone(),
                fail_set,
            }),
            Some(Box::new(Fake {
                me: "security",
                shelf: shelf.clone(),
                fail_set: false,
            })),
        )
    }

    fn owner(shelf: &Shelf, account: &str) -> Option<&'static str> {
        shelf
            .lock()
            .unwrap()
            .get(&("svc".to_string(), account.to_string()))
            .map(|(o, _)| *o)
    }

    fn put_legacy(shelf: &Shelf, account: &str, secret: &str) {
        shelf
            .lock()
            .unwrap()
            .insert(("svc".into(), account.into()), ("security", secret.into()));
    }

    #[test]
    fn a_locked_store_keeps_its_own_items() {
        let shelf = Shelf::default();
        exercise(&locked(&shelf, false));
        assert_eq!(owner(&shelf, "c"), Some("loopx"));
    }

    #[test]
    fn an_item_written_the_old_way_is_read_once_and_moved_under_the_lock() {
        let shelf = Shelf::default();
        put_legacy(&shelf, "gitlab/token", "glpat-1");
        let store = locked(&shelf, false);
        assert_eq!(
            store.get("gitlab/token").unwrap().as_deref(),
            Some("glpat-1")
        );
        assert_eq!(owner(&shelf, "gitlab/token"), Some("loopx"));
        assert_eq!(
            store.get("gitlab/token").unwrap().as_deref(),
            Some("glpat-1")
        );
    }

    #[test]
    fn writing_over_or_deleting_an_old_item_takes_it_over() {
        let shelf = Shelf::default();
        put_legacy(&shelf, "a", "old");
        put_legacy(&shelf, "b", "old");
        let store = locked(&shelf, false);
        store.set("a", "new").unwrap();
        assert_eq!(owner(&shelf, "a"), Some("loopx"));
        assert_eq!(store.get("a").unwrap().as_deref(), Some("new"));
        store.delete("b").unwrap();
        assert_eq!(owner(&shelf, "b"), None);
    }

    #[test]
    fn a_move_that_cannot_write_the_locked_item_puts_the_old_one_back() {
        let shelf = Shelf::default();
        put_legacy(&shelf, "a", "keep-me");
        let store = locked(&shelf, true);
        assert_eq!(store.get("a").unwrap().as_deref(), Some("keep-me"));
        assert_eq!(owner(&shelf, "a"), Some("security"));
    }

    /// A fake `security` that records argv and stdin and keeps items in a directory.
    #[cfg(unix)]
    fn fake_security(dir: &Path) -> PathBuf {
        let script = dir.join("security");
        std::fs::write(
            &script,
            r#"#!/bin/sh
D="$(dirname "$0")"
echo "$@" >> "$D/argv.log"
case "$1" in
  -i)
    cat > "$D/stdin.log"
    # add-generic-password -U -s "svc" -a "acct" -w "secret" -T ...
    ACCT=$(sed -n 's/.* -a "\([^"]*\)".*/\1/p' "$D/stdin.log")
    SEC=$(sed -n 's/.* -w "\(.*\)" -T .*/\1/p' "$D/stdin.log")
    printf '%s' "$SEC" | tr -d '\\' > "$D/item-$(printf '%s' "$ACCT" | tr '/' '_')"
    ;;
  find-generic-password)
    while [ $# -gt 0 ]; do [ "$1" = "-a" ] && A="$2"; shift; done
    F="$D/item-$(printf '%s' "$A" | tr '/' '_')"
    [ -f "$F" ] && { cat "$F"; echo; exit 0; }
    exit 44 ;;
  delete-generic-password)
    while [ $# -gt 0 ]; do [ "$1" = "-a" ] && A="$2"; shift; done
    F="$D/item-$(printf '%s' "$A" | tr '/' '_')"
    [ -f "$F" ] && { rm "$F"; exit 0; }
    exit 44 ;;
esac
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    #[test]
    fn keychain_store_keeps_secrets_off_the_command_line() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_security(d.path());
        let s = KeychainStore::with_binary("loop-engineering.test", &bin);
        assert_eq!(s.get("acct").unwrap(), None);
        s.set("acct", "hunter2-token").unwrap();
        assert_eq!(s.get("acct").unwrap().as_deref(), Some("hunter2-token"));
        assert_eq!(s.list().unwrap(), vec!["acct"]);
        s.delete("acct").unwrap();
        assert_eq!(s.get("acct").unwrap(), None);
        assert!(s.list().unwrap().is_empty());
        let argv = std::fs::read_to_string(d.path().join("argv.log")).unwrap();
        assert!(!argv.contains("hunter2"), "secret leaked into argv: {argv}");
        assert!(argv.contains("find-generic-password"));
    }

    #[test]
    fn keychain_values_with_line_breaks_are_refused() {
        assert!(matches!(quote("a\nb"), Err(SecretError::BadValue)));
        assert_eq!(quote("a\"b\\c").unwrap(), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn a_missing_security_binary_is_unavailable_not_a_panic() {
        let s = KeychainStore::with_binary("svc", Path::new("/nonexistent/security"));
        assert!(matches!(s.get("a"), Err(SecretError::Unavailable(_))));
    }
}
