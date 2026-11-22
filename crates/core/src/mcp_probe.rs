//! The Settings "Test" button: start one registered MCP server, list its
//! tools over stdio JSON-RPC, and stop its whole process group.

use crate::mcp_registry as reg;
use crate::mcp_run::{self, Env};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const DEADLINE: Duration = Duration::from_secs(30);
pub const PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_PAGES: usize = 5;
const MAX_SAVED_TOOLS: usize = 200;
const STDERR_TAIL_CHARS: usize = 2000;
const GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    pub ok: bool,
    pub tools: Vec<String>,
    pub message: String,
    pub install: Option<String>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedResult {
    pub ok: bool,
    pub at: String,
    pub tools: Vec<String>,
    pub message: String,
}

/// `<checkout root>/outputs/connectors/mcp-test-results.json`.
pub fn results_path(repo_root: &Path) -> PathBuf {
    repo_root
        .join("outputs")
        .join("connectors")
        .join("mcp-test-results.json")
}

pub fn probe(name: &str, env: &Env, deadline: Duration) -> ProbeResult {
    probe_inner(name, env, deadline, &|_| {})
}

fn failed(message: String, install: Option<String>, start: Instant) -> ProbeResult {
    ProbeResult {
        ok: false,
        tools: Vec::new(),
        message,
        install,
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

fn probe_inner(name: &str, env: &Env, deadline: Duration, on_spawn: &dyn Fn(u32)) -> ProbeResult {
    let start = Instant::now();
    let launch = match reg::load(&env.registry)
        .map_err(|e| mcp_run::Unresolved {
            reason: "invalid",
            message: e,
            install: None,
        })
        .and_then(|r| mcp_run::resolve_server(name, &r, env))
    {
        Ok(l) => l,
        Err(u) => return failed(u.message, u.install, start),
    };
    let mut cmd = Command::new(&launch.command);
    cmd.args(&launch.args).env_clear();
    for k in ["HOME", "USER", "LOGNAME", "TMPDIR"] {
        if let Some(v) = std::env::var_os(k) {
            cmd.env(k, v);
        }
    }
    cmd.envs(&launch.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return failed(
                format!("could not start {}: {e}", launch.command.display()),
                None,
                start,
            )
        }
    };
    on_spawn(child.id());
    let pgid = child.id() as i32;
    let (tx, rx) = mpsc::channel::<String>();
    if let Some(out) = child.stdout.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    let stderr = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    });
    let stdin = child.stdin.take();
    let outcome = handshake(stdin, &rx, start + deadline, deadline);
    stop(&mut child, pgid);
    let stderr_text = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
    let redact = |t: &str| mcp_run::redact(t, &launch.secrets);
    let duration_ms = start.elapsed().as_millis() as u64;
    match outcome {
        Ok(tools) => ProbeResult {
            ok: true,
            message: format!("{} tools", tools.len()),
            tools,
            install: None,
            duration_ms,
        },
        Err(why) => {
            let tail = crate::runner_util::tail(stderr_text.trim(), STDERR_TAIL_CHARS);
            let message = if tail.is_empty() {
                why
            } else {
                format!("{why}\n{tail}")
            };
            ProbeResult {
                ok: false,
                tools: Vec::new(),
                message: redact(&message),
                install: None,
                duration_ms,
            }
        }
    }
}

/// `initialize`, `notifications/initialized`, then `tools/list` pages.
fn handshake(
    stdin: Option<std::process::ChildStdin>,
    rx: &mpsc::Receiver<String>,
    until: Instant,
    deadline: Duration,
) -> Result<Vec<String>, String> {
    let mut stdin = stdin.ok_or("no stdin")?;
    let mut send = |v: Value| -> Result<(), String> {
        writeln!(stdin, "{v}")
            .and_then(|_| stdin.flush())
            .map_err(|e| format!("the server closed its input: {e}"))
    };
    send(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": PROTOCOL_VERSION, "capabilities": {},
        "clientInfo": {"name": "loopx", "version": env!("CARGO_PKG_VERSION")}}}),
    )?;
    await_result(rx, 1, until, deadline)?;
    send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    for page in 0..MAX_PAGES {
        let id = 2 + page as u64;
        let params = match &cursor {
            Some(c) => json!({"cursor": c}),
            None => json!({}),
        };
        send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/list", "params": params}))?;
        let result = await_result(rx, id, until, deadline)?;
        tools.extend(
            result["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t["name"].as_str().map(str::to_string)),
        );
        cursor = result["nextCursor"]
            .as_str()
            .filter(|c| !c.is_empty())
            .map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    Ok(tools)
}

fn await_result(
    rx: &mpsc::Receiver<String>,
    id: u64,
    until: Instant,
    deadline: Duration,
) -> Result<Value, String> {
    loop {
        let left = until.saturating_duration_since(Instant::now());
        let line = match rx.recv_timeout(left) {
            Ok(l) => l,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(format!(
                    "timed out after {}s waiting for the server",
                    deadline.as_secs()
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("the server exited before answering".into())
            }
        };
        let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if msg.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(err) = msg.get("error") {
            let text = err["message"].as_str().unwrap_or("unknown error");
            return Err(format!("the server answered with an error: {text}"));
        }
        return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
    }
}

fn wait_for(child: &mut std::process::Child, grace: Duration) -> bool {
    let end = Instant::now() + grace;
    while Instant::now() < end {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Stdin is already closed; then SIGTERM and SIGKILL to the whole group.
fn stop(child: &mut std::process::Child, pgid: i32) {
    let exited = wait_for(child, GRACE);
    // SAFETY: signalling the process group this probe created.
    unsafe { libc::kill(-pgid, libc::SIGTERM) };
    if !exited && !wait_for(child, GRACE) {
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
    let _ = child.wait();
}

pub fn save_result(path: &Path, name: &str, r: &ProbeResult, at: &str) -> std::io::Result<()> {
    let saved = SavedResult {
        ok: r.ok,
        at: at.to_string(),
        tools: r.tools.iter().take(MAX_SAVED_TOOLS).cloned().collect(),
        message: r.message.clone(),
    };
    crate::file_lock::with_lock(path, || {
        let mut all = load_results(path);
        all.insert(name.to_string(), saved);
        let value = serde_json::to_value(&all).unwrap_or(Value::Null);
        crate::messages::atomic_write_json(&value, path)
    })?
}

pub fn load_results(path: &Path) -> BTreeMap<String, SavedResult> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn remove_result(path: &Path, name: &str) -> std::io::Result<()> {
    crate::file_lock::with_lock(path, || {
        let mut all = load_results(path);
        if all.remove(name).is_none() {
            return Ok(());
        }
        let value = serde_json::to_value(&all).unwrap_or(Value::Null);
        crate::messages::atomic_write_json(&value, path)
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::{MemoryStore, SecretStore};
    use crate::node_env::Toolchains;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_mcp_server.sh")
    }

    fn exec(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    struct T {
        dir: tempfile::TempDir,
        store: Arc<MemoryStore>,
    }

    impl T {
        fn new() -> T {
            let dir = tempfile::tempdir().unwrap();
            exec(
                &dir.path().join("fake-mcp"),
                &format!("exec sh '{}' \"$@\"", fixture().display()),
            );
            T {
                dir,
                store: Arc::new(MemoryStore::new(reg::SERVICE_MCP)),
            }
        }

        fn register(&self, name: &str, mode: &str) {
            let spec = reg::ServerSpec::from_value(&json!({
                "command": self.dir.path().join("fake-mcp"),
                "env": {"FAKE_MODE": mode},
                "env_secrets": {"DEMO_TOKEN": "keychain:demo-token"}}))
            .unwrap();
            reg::save_entry(&reg::path(self.dir.path()), name, &spec).unwrap();
            self.store.set("demo-token", "s3cret-value").unwrap();
        }

        fn env(&self) -> Env {
            Env {
                registry: reg::path(self.dir.path()),
                secrets: self.store.clone(),
                toolchains: Toolchains {
                    home: self.dir.path().join("home"),
                    nvm_dir: None,
                },
                launch_path: String::new(),
                tmp_root: self.dir.path().join("tmp"),
            }
        }
    }

    #[test]
    fn lists_tools_and_stops_the_server() {
        let t = T::new();
        t.register("demo", "ok");
        let r = probe("demo", &t.env(), Duration::from_secs(10));
        assert!(r.ok, "{r:?}");
        assert_eq!(
            r.tools,
            vec!["browser_navigate", "browser_take_screenshot", "token_ok"]
        );
        assert!(r.install.is_none());
        let pgid_file = t.dir.path().join("pgid");
        let r = probe_with_pgid("demo", &t.env(), Duration::from_secs(10), &pgid_file);
        assert!(r.ok);
        let pgid: i32 = std::fs::read_to_string(&pgid_file)
            .unwrap()
            .parse()
            .unwrap();
        // SAFETY: signal 0 only checks whether the group exists.
        assert_eq!(unsafe { libc::kill(-pgid, 0) }, -1);
    }

    fn probe_with_pgid(name: &str, env: &Env, deadline: Duration, file: &Path) -> ProbeResult {
        probe_inner(name, env, deadline, &|pid| {
            std::fs::write(file, pid.to_string()).unwrap()
        })
    }

    #[test]
    fn follows_next_cursor() {
        let t = T::new();
        t.register("demo", "paged");
        let r = probe("demo", &t.env(), Duration::from_secs(10));
        assert!(r.ok, "{r:?}");
        assert_eq!(r.tools, vec!["page_one", "page_two"]);
    }

    #[test]
    fn skips_non_json_stdout_lines() {
        let t = T::new();
        t.register("demo", "noisy");
        let r = probe("demo", &t.env(), Duration::from_secs(10));
        assert!(r.ok, "{r:?}");
        assert_eq!(r.tools.len(), 3);
    }

    #[test]
    fn a_json_rpc_error_is_a_failed_test() {
        let t = T::new();
        t.register("demo", "error");
        let r = probe("demo", &t.env(), Duration::from_secs(10));
        assert!(!r.ok);
        assert!(r.message.contains("init failed"), "{}", r.message);
        assert!(!r.message.contains("s3cret-value"), "{}", r.message);
    }

    #[test]
    fn a_silent_server_times_out_and_its_process_group_is_killed() {
        let t = T::new();
        t.register("demo", "silent");
        let start = Instant::now();
        let pgid_file = t.dir.path().join("pgid");
        let r = probe_with_pgid("demo", &t.env(), Duration::from_secs(2), &pgid_file);
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "{:?}",
            start.elapsed()
        );
        assert!(!r.ok);
        assert!(r.message.contains("timed out"), "{}", r.message);
        let pgid: i32 = std::fs::read_to_string(&pgid_file)
            .unwrap()
            .parse()
            .unwrap();
        let gone = (0..40).any(|_| {
            // SAFETY: signal 0 only checks whether the group exists.
            let rc = unsafe { libc::kill(-pgid, 0) };
            let esrch =
                rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if !esrch {
                std::thread::sleep(Duration::from_millis(50));
            }
            esrch
        });
        assert!(gone, "process group {pgid} still alive");
    }

    #[test]
    fn the_secret_reaches_the_server_env_but_never_the_message() {
        let t = T::new();
        t.register("demo", "error");
        let r = probe("demo", &t.env(), Duration::from_secs(10));
        assert!(r.message.contains(mcp_run::REDACTED), "{}", r.message);
        assert!(!r.message.contains("s3cret-value"));
        t.register("demo", "ok");
        let r = probe("demo", &t.env(), Duration::from_secs(10));
        assert!(r.tools.contains(&"token_ok".to_string()));
        assert!(!r.message.contains("s3cret-value"));
        let results = t.dir.path().join("out/mcp-test-results.json");
        save_result(&results, "demo", &r, "2026-11-23T10:00:00Z").unwrap();
        let text = std::fs::read_to_string(&results).unwrap();
        assert!(!text.contains("s3cret-value"));
        let saved = load_results(&results);
        assert_eq!(saved["demo"].tools, r.tools);
        assert!(saved["demo"].ok);
        assert_eq!(saved["demo"].at, "2026-11-23T10:00:00Z");
    }

    #[test]
    fn not_installed_or_wrong_version_fails_without_spawning_and_shows_the_install_command() {
        let t = T::new();
        let marker = t.dir.path().join("spawned");
        let chrome = t.dir.path().join("Chrome");
        std::fs::write(&chrome, "").unwrap();
        reg::save_entry(
            &reg::path(t.dir.path()),
            "playwright",
            &reg::playwright_preset(Some(chrome)).unwrap(),
        )
        .unwrap();
        let r = probe("playwright", &t.env(), Duration::from_secs(10));
        assert!(!r.ok);
        assert_eq!(r.install.as_deref(), Some(reg::PLAYWRIGHT_INSTALL_CMD));
        assert!(r.message.contains("not installed"), "{}", r.message);

        let prefix = t.dir.path().join("old");
        let pkg = prefix.join("lib/node_modules/@playwright/mcp");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            json!({"name": reg::PLAYWRIGHT_PACKAGE, "version": "0.0.83"}).to_string(),
        )
        .unwrap();
        exec(
            &pkg.join("cli.js"),
            &format!("touch '{}'", marker.display()),
        );
        exec(
            &prefix.join("bin/node"),
            &format!("touch '{}'; echo v20.0.0", marker.display()),
        );
        std::os::unix::fs::symlink(
            "../lib/node_modules/@playwright/mcp/cli.js",
            prefix.join("bin").join(reg::PLAYWRIGHT_BIN),
        )
        .unwrap();
        let mut env = t.env();
        env.launch_path = prefix.join("bin").to_string_lossy().into_owned();
        let r = probe("playwright", &env, Duration::from_secs(10));
        assert!(!r.ok);
        assert!(r.message.contains("0.0.83"), "{}", r.message);
        assert_eq!(r.install.as_deref(), Some(reg::PLAYWRIGHT_INSTALL_CMD));
        assert!(!marker.exists());
    }

    #[test]
    fn a_playwright_install_runs_through_its_own_node() {
        let t = T::new();
        let chrome = t.dir.path().join("Chrome");
        std::fs::write(&chrome, "").unwrap();
        reg::save_entry(
            &reg::path(t.dir.path()),
            "playwright",
            &reg::playwright_preset(Some(chrome)).unwrap(),
        )
        .unwrap();
        let prefix = t.dir.path().join("home/.nvm/versions/node/v20.15.0");
        let pkg = prefix.join("lib/node_modules/@playwright/mcp");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            json!({"name": reg::PLAYWRIGHT_PACKAGE, "version": reg::PLAYWRIGHT_VERSION})
                .to_string(),
        )
        .unwrap();
        std::fs::write(pkg.join("cli.js"), "").unwrap();
        exec(
            &prefix.join("bin/node"),
            &format!(
                "[ \"$1\" = -v ] && {{ echo v20.15.0; exit 0; }}\nexec sh '{}'",
                fixture().display()
            ),
        );
        std::os::unix::fs::symlink(
            "../lib/node_modules/@playwright/mcp/cli.js",
            prefix.join("bin").join(reg::PLAYWRIGHT_BIN),
        )
        .unwrap();
        let r = probe("playwright", &t.env(), Duration::from_secs(10));
        assert!(r.ok, "{r:?}");
        assert_eq!(r.tools, vec!["browser_navigate", "browser_take_screenshot"]);
    }

    #[test]
    fn an_unregistered_or_invalid_server_fails_without_spawning() {
        let t = T::new();
        let r = probe("nope", &t.env(), Duration::from_secs(10));
        assert!(!r.ok);
        assert!(r.message.contains("not in"), "{}", r.message);
        std::fs::write(reg::path(t.dir.path()), r#"{"bad": {"command": "npx"}}"#).unwrap();
        let r = probe("bad", &t.env(), Duration::from_secs(10));
        assert!(!r.ok);
        assert!(r.message.contains("command"), "{}", r.message);
    }

    #[test]
    fn saved_results_keep_other_servers_and_cap_the_tool_list() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("r.json");
        let many = ProbeResult {
            ok: true,
            tools: (0..250).map(|i| format!("t{i}")).collect(),
            message: String::new(),
            install: None,
            duration_ms: 1,
        };
        save_result(&p, "a", &many, "t1").unwrap();
        save_result(&p, "b", &many, "t2").unwrap();
        let saved = load_results(&p);
        assert_eq!(saved["a"].tools.len(), 200);
        assert_eq!(saved["b"].at, "t2");
        assert!(load_results(&d.path().join("missing.json")).is_empty());
    }
}
