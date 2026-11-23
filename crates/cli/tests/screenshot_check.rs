//! `loopx screenshot check` run as a real process: whatever happens, it
//! writes a report (or refuses before starting anything) and leaves no dev
//! server, Chrome or profile dir behind.

use serde_json::{json, Value};
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
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A fake `yarn` (its `dev` serves `port` through `socketserver.TCPServer`,
/// or exits 1 when `dead`), and a fake Chrome that fills its profile, then
/// hangs without ever opening DevTools.
struct World {
    dir: tempfile::TempDir,
    port: u16,
}

impl World {
    fn new(dead: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["bin", "tmp", "home", "app"] {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        let bin = dir.path().join("bin");
        let tail = if dead {
            "echo 'Error: Cannot find module vite' >&2\nexit 1\n"
        } else {
            "exec python3 \"$SERVER_PY\"\n"
        };
        script(
            &bin.join("yarn"),
            &format!(
                "#!/bin/sh\necho \"$@\" >> \"$YARN_ARGS\"\necho \"telemetry=$NEXT_TELEMETRY_DISABLED\" > \"$YARN_ARGS.env\"\nsleep 300 &\necho $! >> \"$SERVER_PIDS\"\n\
                 echo $$ >> \"$SERVER_PIDS\"\n{tail}"
            ),
        );
        std::fs::write(
            dir.path().join("server.py"),
            "import http.server, os, socketserver\n\
             socketserver.TCPServer(('127.0.0.1', int(os.environ['FAKE_PORT'])), \
             http.server.SimpleHTTPRequestHandler).serve_forever()\n",
        )
        .unwrap();
        script(
            &bin.join("fake-chrome"),
            "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in --user-data-dir=*) d=\"${a#--user-data-dir=}\";; esac; done\n\
             mkdir -p \"$d/Default\" && echo x > \"$d/Default/Cookies\"\n\
             echo $$ >> \"$CHROME_PID\"\nexec sleep 300\n",
        );
        World {
            dir,
            port: free_port(),
        }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn spec(&self, deadline_s: u64, pages: usize) -> Value {
        json!({
            "serve": "yarn dev",
            "origin": format!("http://127.0.0.1:{}", self.port),
            "pages": (0..pages).map(|i| json!({"label": format!("P{i}"), "path": format!("/p{i}"),
                                               "wait_for": null, "dark": false, "viewport": [1280, 800]}))
                               .collect::<Vec<_>>(),
            "cwd": self.path("app"),
            "axe": false,
            "deadline_s": deadline_s,
        })
    }

    fn spawn(&self, spec: &Value) -> Spawned {
        std::fs::write(self.path("spec.json"), spec.to_string()).unwrap();
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(env!("CARGO_BIN_EXE_loopx"))
            .args(["screenshot", "check", "--spec"])
            .arg(self.path("spec.json"))
            .arg("--out")
            .arg(self.path("report.json"))
            .env("PATH", path)
            .env("LOOPX_CHROME", self.path("bin/fake-chrome"))
            .env("TMPDIR", self.path("tmp"))
            .env("HOME", self.path("home"))
            .env("LOOP_ENGINEERING_HOME", self.path("home/.loopx"))
            .env("FAKE_PORT", self.port.to_string())
            .env("SERVER_PY", self.path("server.py"))
            .env("SERVER_PIDS", self.path("server.pids"))
            .env("CHROME_PID", self.path("chrome.pid"))
            .env("YARN_ARGS", self.path("yarn.args"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map(Spawned)
            .unwrap()
    }

    fn report(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.path("report.json")).unwrap()).unwrap()
    }

    /// Every recorded dev-server and Chrome process is gone, and no profile
    /// dir is left in TMPDIR.
    fn assert_nothing_left(&self) {
        let mut all = pids(&self.path("server.pids"));
        all.extend(pids(&self.path("chrome.pid")));
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

impl Drop for World {
    fn drop(&mut self) {
        let mut all = pids(&self.path("server.pids"));
        all.extend(pids(&self.path("chrome.pid")));
        for p in all {
            unsafe {
                libc::kill(-p, libc::SIGKILL);
                libc::kill(p, libc::SIGKILL);
            }
        }
    }
}

struct Spawned(Child);

impl Drop for Spawned {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
            let until = Instant::now() + Duration::from_secs(10);
            while matches!(self.0.try_wait(), Ok(None)) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn wait_exit(child: &mut Spawned, limit: Duration) -> (std::process::ExitStatus, Duration) {
    let started = Instant::now();
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            return (status, started.elapsed());
        }
        if started.elapsed() > limit {
            let _ = child.0.kill();
            panic!("loopx screenshot check still running after {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stderr(child: &mut Spawned) -> String {
    let mut s = String::new();
    use std::io::Read;
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    s
}

#[test]
fn a_dead_dev_server_reports_serve_failed_and_leaves_nothing() {
    let w = World::new(true);
    let mut child = w.spawn(&w.spec(60, 1));
    let (status, _) = wait_exit(&mut child, Duration::from_secs(30));
    assert!(status.success(), "{}", stderr(&mut child));
    let r = w.report();
    assert_eq!(r["serve"]["started"], json!(false), "{r}");
    let tail = r["serve"]["tail"].as_str().unwrap();
    assert!(tail.contains("Cannot find module vite"), "{tail}");
    assert_eq!(r["pages"], json!([]));
    assert!(!w.path("chrome.pid").exists());
    let env = std::fs::read_to_string(w.path("yarn.args.env")).unwrap();
    assert_eq!(
        env.trim(),
        "telemetry=1",
        "the dev server must not phone home"
    );
    w.assert_nothing_left();
}

#[test]
fn a_signal_mid_check_stops_the_dev_server_and_chrome() {
    let w = World::new(false);
    let mut child = w.spawn(&w.spec(120, 2));
    wait_until("Chrome to start", Duration::from_secs(20), || {
        w.path("chrome.pid").exists()
    });
    unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) };
    let (status, _) = wait_exit(&mut child, Duration::from_secs(15));
    assert_eq!(status.code(), Some(128 + libc::SIGTERM));
    assert_eq!(pids(&w.path("server.pids")).len(), 2);
    w.assert_nothing_left();
}

#[test]
fn the_internal_deadline_writes_a_partial_report_and_stops_everything() {
    let w = World::new(false);
    let mut child = w.spawn(&w.spec(3, 2));
    let (status, took) = wait_exit(&mut child, Duration::from_secs(60));
    let err = stderr(&mut child);
    assert!(status.success(), "{err}");
    assert!(took < Duration::from_secs(20), "took {took:?}: {err}");
    let r = w.report();
    assert_eq!(r["serve"]["started"], json!(true), "{r}");
    assert_eq!(r["deadline_hit"], json!(true), "{r}");
    let pages = r["pages"].as_array().unwrap();
    assert!(pages.len() < 2, "{r}");
    assert!(pages.iter().all(|p| p.get("error").is_some()), "{r}");
    w.assert_nothing_left();
}

#[test]
fn an_invalid_spec_is_refused_before_anything_starts() {
    let w = World::new(false);
    let mut bad = Vec::new();
    let mut s = w.spec(60, 1);
    s["serve"] = json!("node evil.js");
    bad.push(s);
    let mut s = w.spec(60, 1);
    s["origin"] = json!("http://example.com:80");
    bad.push(s);
    let mut s = w.spec(60, 1);
    s["cwd"] = json!(w.path("nope"));
    bad.push(s);
    bad.push(w.spec(60, 7));
    let mut s = w.spec(60, 1);
    s["pages"][0]["path"] = json!("//evil");
    bad.push(s);
    for spec in bad {
        let mut child = w.spawn(&spec);
        let (status, _) = wait_exit(&mut child, Duration::from_secs(10));
        assert_eq!(status.code(), Some(2), "{spec}: {}", stderr(&mut child));
        assert!(!w.path("report.json").exists());
        assert!(!w.path("yarn.args").exists());
        assert!(!w.path("chrome.pid").exists());
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn ui_check_never_runs_npm_npx_install_or_build() {
    use loopx_core::loop_verifiers::{IssueWork, Verifier};
    use loopx_core::ui_check::{SubprocessRunner, UiCheckVerifier};
    let w = World::new(false);
    let marker = w.path("installed");
    for tool in ["npm", "npx", "bundle", "pnpm", "corepack"] {
        script(
            &w.path(&format!("bin/{tool}")),
            &format!("#!/bin/sh\necho {tool} \"$@\" >> {}\n", marker.display()),
        );
    }
    // A Chrome that fails at once: this test is about what runs, not the page.
    script(&w.path("bin/fake-chrome"), "#!/bin/sh\nexit 1\n");
    let wt = w.path("wt");
    let work_dir = wt.join("web-issue-7");
    std::fs::create_dir_all(&work_dir).unwrap();
    git(&work_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(work_dir.join("index.html"), "<p>v1</p>").unwrap();
    std::fs::write(
        work_dir.join("package.json"),
        "{\"scripts\": {\"dev\": \"vite\"}}",
    )
    .unwrap();
    git(&work_dir, &["add", "."]);
    git(&work_dir, &["commit", "-qm", "base"]);
    git(&work_dir, &["switch", "-qc", "feature"]);
    std::fs::write(work_dir.join("index.html"), "<p>v2</p>").unwrap();
    git(&work_dir, &["commit", "-qam", "fix"]);
    let handoff = w.path("handoff.json");
    std::fs::write(
        &handoff,
        json!({"action": "fix", "target_branch": "main", "ui_checks": {
            "serve": "yarn dev", "origin": format!("http://127.0.0.1:{}", w.port),
            "pages": [{"label": "Home", "path": "/index.html"}]}})
        .to_string(),
    )
    .unwrap();
    let project = json!({"local_path": "/src/web", "verifiers": ["a11y"]});
    let wt2 = wt.clone();
    let mut work = IssueWork::new("web", 7)
        .with_project_fn(move |_| Ok(project.clone()))
        .with_worktree_root_fn(move || Ok(wt2.clone()));
    work.handoff_path = Some(handoff);
    let runner = SubprocessRunner {
        exe: Some(PathBuf::from(env!("CARGO_BIN_EXE_loopx"))),
        env: [
            ("LOOPX_CHROME", w.path("bin/fake-chrome")),
            ("TMPDIR", w.path("tmp")),
            ("FAKE_PORT", PathBuf::from(w.port.to_string())),
            ("SERVER_PY", w.path("server.py")),
            ("SERVER_PIDS", w.path("server.pids")),
            ("CHROME_PID", w.path("chrome.pid")),
            ("YARN_ARGS", w.path("yarn.args")),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.display().to_string()))
        .collect(),
        ..SubprocessRunner::default()
    };
    // The verifier's own PATH lookup must find the fake tools first.
    let path = format!(
        "{}:{}",
        w.path("bin").display(),
        std::env::var("PATH").unwrap()
    );
    let runner = std::sync::Arc::new(WithPath(runner, path));
    let v = UiCheckVerifier::new("ui_check", work, runner, w.path("loop"))
        .with_find_chrome(|| Some(PathBuf::from("/bin/sh")));
    let def = loopx_core::loop_definition::LoopDefinition::from_value(
        &json!({"name": "t", "version": 1, "trigger": {"type": "manual"}, "goal": {"type": "g"}}),
    )
    .unwrap();
    let r = v.verify(&loopx_core::loop_runtime::LoopContext {
        iteration: 1,
        definition: &def,
        previous: None,
    });
    assert_eq!(
        r.evidence["serve"]["started"],
        json!(true),
        "{}: {}",
        r.output,
        r.evidence
    );
    assert_eq!(
        std::fs::read_to_string(w.path("yarn.args")).unwrap(),
        "dev\n"
    );
    assert!(
        !marker.exists(),
        "{}",
        std::fs::read_to_string(&marker).unwrap()
    );
    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(&work_dir)
        .output()
        .unwrap();
    assert!(
        status.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );
    w.assert_nothing_left();
}

/// `SubprocessRunner` with this test's fake tools first on `PATH`.
struct WithPath(loopx_core::ui_check::SubprocessRunner, String);

impl loopx_core::ui_check::UiCheckRunner for WithPath {
    fn run(&self, spec: &Value, cwd: &Path, _: &str, deadline: Duration) -> Result<Value, String> {
        self.0.run(spec, cwd, &self.1, deadline)
    }
}

#[test]
fn a_check_whose_parent_dies_stops_its_dev_server_and_chrome() {
    let w = World::new(false);
    std::fs::write(w.path("spec.json"), w.spec(120, 2).to_string()).unwrap();
    let path = format!(
        "{}:{}",
        w.path("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // The parent waits until Chrome is up, then exits, orphaning the check.
    let script =
        "\"$LOOPX\" screenshot check --spec \"$SPEC\" --out \"$OUT\" & echo $! > \"$CHECK_PID\"; \
         while [ ! -s \"$CHROME_PID\" ]; do sleep 0.1; done; exit 0";
    let status = Command::new("sh")
        .args(["-c", script])
        .env("LOOPX", env!("CARGO_BIN_EXE_loopx"))
        .env("SPEC", w.path("spec.json"))
        .env("OUT", w.path("report.json"))
        .env("CHECK_PID", w.path("check.pid"))
        .env("PATH", path)
        .env("LOOPX_CHROME", w.path("bin/fake-chrome"))
        .env("TMPDIR", w.path("tmp"))
        .env("HOME", w.path("home"))
        .env("LOOP_ENGINEERING_HOME", w.path("home/.loopx"))
        .env("FAKE_PORT", w.port.to_string())
        .env("SERVER_PY", w.path("server.py"))
        .env("SERVER_PIDS", w.path("server.pids"))
        .env("CHROME_PID", w.path("chrome.pid"))
        .env("YARN_ARGS", w.path("yarn.args"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let check = pids(&w.path("check.pid"))[0];
    let stopped = std::panic::catch_unwind(|| {
        wait_until(
            "the orphaned check to exit",
            Duration::from_secs(10),
            || !alive(check),
        )
    });
    if stopped.is_err() {
        unsafe { libc::kill(check, libc::SIGKILL) };
    }
    assert!(stopped.is_ok(), "the orphaned check kept running");
    w.assert_nothing_left();
}
