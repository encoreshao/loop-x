//! Cross-process `flock` locks. Parallel runs share files under the loopx home
//! and the checkout, and the agent's own `loopx` subprocesses write them too, so
//! an in-process mutex is not enough: every read-modify-write of a shared file
//! goes through [`update_locked`], and every task a run may start takes a
//! [`FileLock::try_exclusive`] on its own lock file so the same task never runs
//! twice at once. The kernel drops a lock when its holder exits, however it
//! exits, so a killed run can never leave one stuck.

use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// An exclusive `flock` on a lock file, released on drop.
#[derive(Debug)]
pub struct FileLock {
    file: std::fs::File,
}

fn open(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

impl FileLock {
    /// Waits for the lock.
    pub fn exclusive(path: &Path) -> std::io::Result<FileLock> {
        let file = open(path)?;
        loop {
            // SAFETY: flock on a valid descriptor we own.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(FileLock { file });
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }

    /// `Ok(None)` when someone else holds the lock.
    pub fn try_exclusive(path: &Path) -> std::io::Result<Option<FileLock>> {
        let file = open(path)?;
        // SAFETY: flock on a valid descriptor we own.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return if err.kind() == std::io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(err)
            };
        }
        Ok(Some(FileLock { file }))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: unlocking the descriptor we locked.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// The lock file guarding `target`: `.<name>.lock` beside it, so it never
/// shows up as a data file of its own.
pub fn lock_path_for(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!(".{name}.lock"))
}

/// Runs `f` while holding `target`'s lock.
pub fn with_lock<T>(target: &Path, f: impl FnOnce() -> T) -> std::io::Result<T> {
    let _lock = FileLock::exclusive(&lock_path_for(target))?;
    Ok(f())
}

/// Atomically replaces `path` with `bytes` (temp file + rename), so a reader
/// never sees a half-written file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(
        ".{name}.{}.tmp",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    std::fs::write(&tmp, bytes)?;
    let renamed = std::fs::rename(&tmp, path);
    if renamed.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    renamed
}

/// Read-modify-write of `path` under its lock: `f` gets the current text (empty
/// when missing) and returns the new text plus a result; the file is replaced
/// atomically.
pub fn update_locked<T>(path: &Path, f: impl FnOnce(&str) -> (String, T)) -> std::io::Result<T> {
    let _lock = FileLock::exclusive(&lock_path_for(path))?;
    let current = std::fs::read_to_string(path).unwrap_or_default();
    let (next, out) = f(&current);
    write_atomic(path, next.as_bytes())?;
    Ok(out)
}

/// Where the per-task "already running" locks live in a checkout.
pub fn task_locks_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("outputs").join("locks")
}

fn safe(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The lock one GitLab issue holds for as long as any run works on it.
pub fn issue_lock_path(repo_root: &Path, alias: &str, iid: &str) -> PathBuf {
    task_locks_dir(repo_root).join(format!("issue-{}-{}.lock", safe(alias), safe(iid)))
}

/// The lock one topic holds for as long as any run works on it.
pub fn topic_lock_path(repo_root: &Path, topic: &str) -> PathBuf {
    task_locks_dir(repo_root).join(format!("topic-{}.lock", safe(topic)))
}

/// Whether some process currently holds the lock at `path`.
pub fn is_held(path: &Path) -> bool {
    path.exists() && matches!(FileLock::try_exclusive(path), Ok(None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn a_second_try_lock_on_the_same_file_is_refused_until_the_first_drops() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("locks/a.lock");
        let first = FileLock::try_exclusive(&p).unwrap().expect("first lock");
        assert!(FileLock::try_exclusive(&p).unwrap().is_none());
        assert!(is_held(&p));
        drop(first);
        assert!(!is_held(&p));
        assert!(FileLock::try_exclusive(&p).unwrap().is_some());
    }

    #[test]
    fn a_lock_held_by_another_process_is_seen_and_freed_when_it_exits() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.lock");
        std::fs::write(&p, "").unwrap();
        // A real second process holding flock(2) on the file.
        let mut child = Command::new("perl")
            .args([
                "-e",
                "use Fcntl ':flock'; open(my $f, '>>', $ARGV[0]) or die; flock($f, LOCK_EX) or die; $| = 1; print \"locked\\n\"; sleep 30;",
                p.to_str().unwrap(),
            ])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = child.stdout.take().unwrap();
        let mut buf = [0u8; 7];
        std::io::Read::read_exact(&mut out, &mut buf).unwrap();
        assert!(is_held(&p));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!is_held(&p));
    }

    #[test]
    fn concurrent_locked_updates_never_lose_a_write() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("counter.txt");
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..25 {
                        update_locked(&p, |cur| {
                            let n: u64 = cur.trim().parse().unwrap_or(0);
                            ((n + 1).to_string(), ())
                        })
                        .unwrap();
                    }
                });
            }
        });
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "200");
    }

    #[test]
    fn task_lock_paths_are_per_task_and_cannot_escape_the_locks_dir() {
        let root = Path::new("/r");
        assert_eq!(
            issue_lock_path(root, "web", "7"),
            Path::new("/r/outputs/locks/issue-web-7.lock")
        );
        assert_ne!(
            issue_lock_path(root, "web", "7"),
            issue_lock_path(root, "web", "8")
        );
        assert_eq!(
            topic_lock_path(root, "../a b"),
            Path::new("/r/outputs/locks/topic-.._a_b.lock")
        );
    }
}
