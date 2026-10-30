//! `loopx screenshot` run as a real process: a signal or a spent budget must
//! never leave the dev server, Chrome or the throwaway profile behind.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

fn pids(file: &Path) -> Vec<i32> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().parse().unwrap())
        .collect()
}

fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A scratch world: a fake `yarn` whose `dev` serves `port` after
/// `delay_s` (leaving a backgrounded child in its group), and a fake Chrome
/// that fills its profile, then hangs without ever opening DevTools.
struct World {
    dir: tempfile::TempDir,
    port: u16,
}

impl World {
    fn new(delay_s: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(dir.path().join("tmp")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        std::fs::create_dir_all(dir.path().join("app")).unwrap();
        script(
            &bin.join("yarn"),
            &format!(
                "#!/bin/sh\nsleep 300 &\necho $! >> \"$SERVER_PIDS\"\necho $$ >> \"$SERVER_PIDS\"\n\
                 sleep {delay_s}\nexec python3 -m http.server \"$FAKE_PORT\" --bind 127.0.0.1\n"
            ),
        );
        script(
            &bin.join("fake-chrome"),
            "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in --user-data-dir=*) d=\"${a#--user-data-dir=}\";; esac; done\n\
             mkdir -p \"$d/Default\" && echo x > \"$d/Default/Cookies\"\n\
             echo $$ > \"$CHROME_PID\"\nexec sleep 300\n",
        );
        World {
            dir,
            port: free_port(),
        }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn spawn(&self, timeout_s: u32) -> Child {
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(env!("CARGO_BIN_EXE_loopx"))
            .args(["screenshot", "--serve", "yarn dev", "--cwd"])
            .arg(self.path("app"))
            .args(["--url", &format!("http://127.0.0.1:{}/", self.port)])
            .arg("--out")
            .arg(self.path("shots/o.png"))
            .args(["--timeout-s", &timeout_s.to_string()])
            .env("PATH", path)
            .env("LOOPX_CHROME", self.path("bin/fake-chrome"))
            .env("TMPDIR", self.path("tmp"))
            .env("HOME", self.path("home"))
            .env("LOOP_ENGINEERING_HOME", self.path("home/.loopx"))
            .env("FAKE_PORT", self.port.to_string())
            .env("SERVER_PIDS", self.path("server.pids"))
            .env("CHROME_PID", self.path("chrome.pid"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// The dev server's group, Chrome and every profile dir are gone.
    fn assert_nothing_left(&self) {
        let mut all = pids(&self.path("server.pids"));
        assert_eq!(all.len(), 2, "{all:?}");
        all.extend(pids(&self.path("chrome.pid")));
        assert_eq!(all.len(), 3, "{all:?}");
        let _ = std::panic::catch_unwind(|| {
            wait_until("every process to exit", Duration::from_secs(5), || {
                all.iter().all(|p| !alive(*p))
            })
        });
        let left: Vec<i32> = all.iter().copied().filter(|p| alive(*p)).collect();
        for p in &left {
            unsafe { libc::kill(*p, libc::SIGKILL) };
        }
        assert!(left.is_empty(), "still running: {left:?}");
        let profiles: Vec<_> = std::fs::read_dir(self.path("tmp"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(profiles.is_empty(), "left in TMPDIR: {profiles:?}");
    }
}

fn wait_exit(child: &mut Child, limit: Duration) -> (std::process::ExitStatus, Duration) {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return (status, started.elapsed());
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            panic!("loopx screenshot still running after {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stderr(child: &mut Child) -> String {
    let mut s = String::new();
    use std::io::Read;
    child.stderr.take().unwrap().read_to_string(&mut s).unwrap();
    s
}

#[test]
fn a_signal_mid_capture_stops_the_dev_server_chrome_and_removes_the_profile() {
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        let w = World::new(0);
        let mut child = w.spawn(60);
        // Chrome is up (the dev server answered first) and its profile exists.
        wait_until("Chrome to start", Duration::from_secs(20), || {
            w.path("chrome.pid").exists()
        });
        unsafe { libc::kill(child.id() as i32, sig) };
        let (status, _) = wait_exit(&mut child, Duration::from_secs(15));
        assert_eq!(status.code(), Some(128 + sig), "signal {sig}");
        w.assert_nothing_left();
    }
}

#[test]
fn the_dev_server_and_the_capture_share_one_deadline() {
    // The server takes 2 s of a 4 s budget, and Chrome never comes up: the
    // capture gets only what is left, not a fresh 4 s plus Chrome's own 30 s.
    let w = World::new(2);
    let mut child = w.spawn(4);
    let (status, took) = wait_exit(&mut child, Duration::from_secs(60));
    let err = stderr(&mut child);
    assert!(!status.success(), "{err}");
    assert!(took < Duration::from_secs(12), "took {took:?}: {err}");
    w.assert_nothing_left();
}
