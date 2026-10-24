//! `loopx.lock`: tells other clients (the Mac app, a second `loopx serve`) that a
//! backend is running, where, which version, and the token native clients use to
//! call its API. Written at startup, removed on a clean exit, and recognised as
//! stale (and replaced) when its pid is no longer alive.

use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub fn default_path() -> PathBuf {
    loopx_core::config::home_dir().join("loopx.lock")
}

#[derive(Debug, Clone, PartialEq)]
pub struct LockInfo {
    pub pid: u32,
    pub port: u16,
    pub version: String,
    pub started_at: String,
    pub token: String,
}

impl LockInfo {
    pub fn new(port: u16, version: &str, token: &str) -> Self {
        Self {
            pid: std::process::id(),
            port,
            version: version.to_string(),
            started_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            token: token.to_string(),
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "pid": self.pid,
            "port": self.port,
            "version": self.version,
            "started_at": self.started_at,
            "token": self.token,
        })
    }

    fn from_value(v: &Value) -> Option<Self> {
        Some(Self {
            pid: u32::try_from(v.get("pid")?.as_u64()?).ok()?,
            port: u16::try_from(v.get("port")?.as_u64()?).ok()?,
            version: v.get("version")?.as_str()?.to_string(),
            started_at: v.get("started_at")?.as_str()?.to_string(),
            token: v.get("token")?.as_str()?.to_string(),
        })
    }
}

/// The lock file's contents, or `None` when missing or unparseable.
pub fn read(path: &Path) -> Option<LockInfo> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    LockInfo::from_value(&v)
}

#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    // Signal 0 only checks existence/permission. EPERM still means "alive".
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub fn pid_alive(_pid: u32) -> bool {
    true
}

#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    #[error("another loopx backend is already running (pid {}, port {}, version {})", .0.pid, .0.port, .0.version)]
    AlreadyRunning(LockInfo),
    #[error("cannot write {0}: {1}")]
    Io(PathBuf, std::io::Error),
}

/// Removes the lock file on drop, but only if it is still ours.
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
    pid: u32,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if read(&self.path).is_some_and(|l| l.pid == self.pid) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn write_private(path: &Path, info: &LockInfo) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("lock.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut f = opts.open(&tmp)?;
    f.write_all(loopx_core::pyjson::to_string_pretty(&info.to_value()).as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Claim the lock for this process. A lock whose pid is dead is replaced; one
/// whose pid is alive (and not us) is an error carrying its details.
pub fn acquire(path: &Path, info: &LockInfo) -> Result<LockGuard, AcquireError> {
    if let Some(existing) = read(path) {
        if existing.pid != info.pid && pid_alive(existing.pid) {
            return Err(AcquireError::AlreadyRunning(existing));
        }
    }
    write_private(path, info).map_err(|e| AcquireError::Io(path.to_path_buf(), e))?;
    Ok(LockGuard {
        path: path.to_path_buf(),
        pid: info.pid,
    })
}

/// Rewrite our own lock (e.g. once the real port is known).
pub fn update(path: &Path, info: &LockInfo) -> std::io::Result<()> {
    write_private(path, info)
}

/// True when the lock file should be written again for `info`: it is gone (something
/// removed the data directory under a running backend) or points at a dead process.
/// A lock owned by another live process is never taken over.
pub fn needs_rewrite(path: &Path, info: &LockInfo) -> bool {
    match read(path) {
        None => true,
        Some(l) => l.pid != info.pid && !pid_alive(l.pid),
    }
}

/// Keep `loopx.lock` in place for as long as this backend lives, so the Mac app
/// can always find a backend that is still serving.
pub fn spawn_keeper(path: PathBuf, info: LockInfo) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if needs_rewrite(&path, &info) {
                let _ = update(&path, &info);
            }
        }
    });
}

/// 256 bits of randomness as hex.
pub fn new_token() -> String {
    format!(
        "{}{}",
        loopx_core::events::new_uuid_hex(),
        loopx_core::events::new_uuid_hex()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_is_private() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("loopx.lock");
        let info = LockInfo::new(8420, "0.4.0", "tok");
        let _guard = acquire(&p, &info).unwrap();
        assert_eq!(read(&p), Some(info));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn rewrites_a_lock_that_vanished_or_is_dead_but_not_a_live_foreign_one() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("loopx.lock");
        let info = LockInfo::new(8420, "0.4.0", "tok");
        assert!(needs_rewrite(&p, &info), "missing");
        let _g = acquire(&p, &info).unwrap();
        assert!(!needs_rewrite(&p, &info), "ours");
        let mut dead = info.clone();
        dead.pid = 4_000_000;
        write_private(&p, &dead).unwrap();
        assert!(needs_rewrite(&p, &info), "dead owner");
        let mut live = info.clone();
        live.pid = 1;
        write_private(&p, &live).unwrap();
        assert!(!needs_rewrite(&p, &info), "live foreign owner");
    }

    #[test]
    fn drop_removes_only_our_own_lock() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("loopx.lock");
        let guard = acquire(&p, &LockInfo::new(1, "v", "t")).unwrap();
        drop(guard);
        assert!(!p.exists());

        let guard = acquire(&p, &LockInfo::new(1, "v", "t")).unwrap();
        let mut other = LockInfo::new(2, "v", "t");
        other.pid = 1;
        write_private(&p, &other).unwrap();
        drop(guard);
        assert!(p.exists(), "someone else's lock must survive");
    }

    #[test]
    fn a_live_foreign_lock_blocks_a_second_backend() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("loopx.lock");
        let mut other = LockInfo::new(9000, "0.3.0", "t");
        other.pid = 1; // init is always alive
        write_private(&p, &other).unwrap();
        match acquire(&p, &LockInfo::new(1, "v", "t")) {
            Err(AcquireError::AlreadyRunning(l)) => {
                assert_eq!((l.port, l.version.as_str()), (9000, "0.3.0"))
            }
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
    }

    #[test]
    fn a_stale_lock_is_replaced() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("loopx.lock");
        let mut dead = LockInfo::new(9000, "0.3.0", "old");
        dead.pid = 0x3fff_fffe; // beyond any real pid
        write_private(&p, &dead).unwrap();
        assert!(!pid_alive(dead.pid));
        let _g = acquire(&p, &LockInfo::new(8420, "0.4.0", "new")).unwrap();
        assert_eq!(read(&p).unwrap().token, "new");
    }

    #[test]
    fn unreadable_locks_are_treated_as_absent() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("loopx.lock");
        std::fs::write(&p, "not json").unwrap();
        assert_eq!(read(&p), None);
        std::fs::write(&p, r#"{"pid": 1}"#).unwrap();
        assert_eq!(read(&p), None);
        assert!(acquire(&p, &LockInfo::new(1, "v", "t")).is_ok());
    }

    #[test]
    fn tokens_are_long_and_unique() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }
}
