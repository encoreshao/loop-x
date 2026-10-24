//! Secret storage behind a trait: the macOS Keychain (through `/usr/bin/security`),
//! a file-backed store for tests and CI, and an in-memory double.
//!
//! Which backend `open_store` picks:
//! - `LOOPX_KEYCHAIN=file:<dir>` selects the file store (mode 0600 files under `<dir>`).
//!   Every test and sandbox must set it: nothing below ever reaches the real Keychain then.
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

/// Generic passwords through `/usr/bin/security`. Secrets are written through
/// `security -i` on **stdin**, never on a command line (argv is visible to every
/// local user via `ps`; OAuth refresh tokens rotate and would be exposed again and again).
/// Keychain cannot enumerate by service without a dump, so the account names are
/// also kept in one `__index__` item (names only, never values).
pub struct KeychainStore {
    service: String,
    binary: PathBuf,
}

impl KeychainStore {
    pub fn new(service: &str) -> Self {
        Self::with_binary(service, Path::new(SECURITY_BIN))
    }

    pub fn with_binary(service: &str, binary: &Path) -> Self {
        Self {
            service: service.to_string(),
            binary: binary.to_path_buf(),
        }
    }

    fn security(
        &self,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Result<proc::ProcOutput, SecretError> {
        let mut cmd = std::process::Command::new(&self.binary);
        cmd.args(args);
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

    fn raw_get(&self, account: &str) -> Result<Option<String>, SecretError> {
        let out = self.security(
            &[
                "find-generic-password",
                "-s",
                &self.service,
                "-a",
                account,
                "-w",
            ],
            None,
        )?;
        match out.code {
            Some(0) => Ok(Some(out.stdout.trim_end_matches('\n').to_string())),
            Some(NOT_FOUND) => Ok(None),
            c => Err(SecretError::Failed(format!(
                "Keychain read failed (exit {c:?}): {}",
                out.stderr.trim()
            ))),
        }
    }

    fn raw_set(&self, account: &str, secret: &str) -> Result<(), SecretError> {
        let command = [
            "add-generic-password",
            "-U",
            "-s",
            &quote(&self.service)?,
            "-a",
            &quote(account)?,
            "-w",
            &quote(secret)?,
            "-T",
            SECURITY_BIN,
        ]
        .join(" ");
        let out = self.security(&["-i"], Some(&format!("{command}\n")))?;
        if out.code != Some(0) {
            return Err(SecretError::Failed(format!(
                "Keychain write failed (exit {:?}): {}",
                out.code,
                out.stderr.trim()
            )));
        }
        Ok(())
    }

    fn raw_delete(&self, account: &str) -> Result<(), SecretError> {
        let out = self.security(
            &[
                "delete-generic-password",
                "-s",
                &self.service,
                "-a",
                account,
            ],
            None,
        )?;
        match out.code {
            Some(0) | Some(NOT_FOUND) => Ok(()),
            c => Err(SecretError::Failed(format!(
                "Keychain delete failed (exit {c:?}): {}",
                out.stderr.trim()
            ))),
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
        Some(spec) => match spec.strip_prefix("file:") {
            Some(dir) if !dir.is_empty() => Ok(Box::new(FileStore::new(Path::new(dir), &service))),
            _ => Err(SecretError::Unavailable(format!(
                "LOOPX_KEYCHAIN must be file:<dir>, got {spec:?}"
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
