//! Where the LinkedIn browser profile (the logged-in session) lives (mirrors
//! `bin/linkedin_session.py`): `<LOOP_ENGINEERING_HOME or ~/.loopx>/
//! linkedin/<account_id>/profile/`, mode 0700, outside the repo. Resolved at
//! call time so a sandboxed run never reaches the real profile. No password is
//! ever stored - only the browser's own session cookies; the 0700 mode and
//! [`wipe`] are the protection.

use std::fs;
use std::path::PathBuf;

pub const MARKER: &str = "connected";

#[derive(Debug, Clone, PartialEq)]
pub enum SessionError {
    InvalidId,
    Io(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::InvalidId => write!(f, "invalid LinkedIn account id"),
            SessionError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<std::io::Error> for SessionError {
    fn from(e: std::io::Error) -> Self {
        SessionError::Io(e.to_string())
    }
}

pub fn base_dir() -> PathBuf {
    crate::config::home_dir().join("linkedin")
}

fn account_dir(account_id: &str) -> Result<PathBuf, SessionError> {
    if crate::connectors_config::is_valid_id(account_id) {
        Ok(base_dir().join(account_id))
    } else {
        Err(SessionError::InvalidId)
    }
}

pub fn profile_dir(account_id: &str) -> Result<PathBuf, SessionError> {
    Ok(account_dir(account_id)?.join("profile"))
}

#[cfg(unix)]
fn private(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn private(_dir: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

pub fn ensure_profile(account_id: &str) -> Result<PathBuf, SessionError> {
    let profile = profile_dir(account_id)?;
    fs::create_dir_all(&profile)?;
    for dir in [
        base_dir(),
        account_dir(account_id)?,
        profile.clone(),
    ] {
        private(&dir)?;
    }
    Ok(profile)
}

pub fn mark_connected(account_id: &str) -> Result<(), SessionError> {
    let account = account_dir(account_id)?;
    fs::create_dir_all(&account)?;
    private(&account)?;
    let stamp = crate::seen_store::py_isoformat(chrono::Utc::now());
    fs::write(account.join(MARKER), stamp)?;
    Ok(())
}

pub fn is_connected(account_id: &str) -> Result<bool, SessionError> {
    Ok(account_dir(account_id)?.join(MARKER).is_file())
}

/// Drop only the connected marker (profile files stay); idempotent.
pub fn mark_disconnected(account_id: &str) -> Result<(), SessionError> {
    match fs::remove_file(account_dir(account_id)?.join(MARKER)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Delete the whole account directory (profile and marker); idempotent.
pub fn wipe(account_id: &str) -> Result<(), SessionError> {
    let _ = fs::remove_dir_all(account_dir(account_id)?);
    Ok(())
}

pub fn rename(old_id: &str, new_id: &str) -> Result<(), SessionError> {
    let (old, new) = (account_dir(old_id)?, account_dir(new_id)?);
    if old.is_dir() && !new.exists() {
        fs::rename(old, new)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::testutil::ENV_LOCK as ENV;

    fn with_home<R>(f: impl FnOnce(&std::path::Path) -> R) -> R {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("le");
        std::env::set_var("LOOP_ENGINEERING_HOME", &home);
        f(&home)
    }

    #[test]
    fn profile_path_follows_loop_engineering_home() {
        with_home(|h| {
            assert_eq!(profile_dir("li").unwrap(), h.join("linkedin/li/profile"));
        });
    }

    #[test]
    fn invalid_ids_are_rejected_before_any_filesystem_access() {
        with_home(|h| {
            for bad in ["../x", "a/b", "", "UPPER", ".", "-x"] {
                assert_eq!(profile_dir(bad), Err(SessionError::InvalidId));
                assert!(ensure_profile(bad).is_err());
                assert!(is_connected(bad).is_err());
                assert!(wipe(bad).is_err());
                assert!(mark_connected(bad).is_err());
                assert!(mark_disconnected(bad).is_err());
            }
            assert!(!h.join("linkedin").exists());
        });
    }

    #[cfg(unix)]
    #[test]
    fn ensure_profile_is_private() {
        use std::os::unix::fs::PermissionsExt;
        with_home(|h| {
            let p = ensure_profile("li").unwrap();
            assert!(p.is_dir());
            for d in [h.join("linkedin"), h.join("linkedin/li"), p] {
                assert_eq!(fs::metadata(d).unwrap().permissions().mode() & 0o777, 0o700);
            }
        });
    }

    #[test]
    fn connected_only_after_mark() {
        with_home(|_| {
            assert!(!is_connected("li").unwrap());
            let p = ensure_profile("li").unwrap();
            fs::write(p.join("Cookies"), "x").unwrap(); // profile files alone do not count
            assert!(!is_connected("li").unwrap());
            mark_connected("li").unwrap();
            assert!(is_connected("li").unwrap());
        });
    }

    #[test]
    fn wipe_removes_everything_and_is_idempotent() {
        with_home(|h| {
            ensure_profile("li").unwrap();
            mark_connected("li").unwrap();
            wipe("li").unwrap();
            wipe("li").unwrap();
            assert!(!is_connected("li").unwrap() && !h.join("linkedin/li").exists());
        });
    }

    #[test]
    fn rename_moves_the_session() {
        with_home(|_| {
            ensure_profile("a").unwrap();
            mark_connected("a").unwrap();
            rename("a", "b").unwrap();
            assert!(is_connected("b").unwrap() && !is_connected("a").unwrap());
            rename("missing", "c").unwrap(); // nothing to move: no error
            assert!(!is_connected("c").unwrap());
        });
    }

    #[test]
    fn mark_disconnected_removes_only_the_marker_and_is_idempotent() {
        with_home(|_| {
            let p = ensure_profile("li").unwrap();
            fs::write(p.join("Cookies"), "x").unwrap();
            mark_connected("li").unwrap();
            mark_disconnected("li").unwrap();
            assert!(!is_connected("li").unwrap() && p.join("Cookies").exists());
            mark_disconnected("li").unwrap();
            mark_disconnected("never-connected").unwrap();
            assert!(mark_disconnected("../x").is_err());
        });
    }
}
