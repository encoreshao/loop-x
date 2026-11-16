//! `loopx slack`: real subprocesses against a scratch home. Nothing here
//! reaches Slack: every case stops before a network call.

use std::path::Path;
use std::process::{Command, Output};

fn loopx(home: &Path, args: &[&str]) -> Output {
    loopx_env(home, args, &[])
}

fn loopx_env(home: &Path, args: &[&str], extra: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loopx"))
        .args(args)
        .envs(extra.iter().copied())
        .stdin(std::process::Stdio::null())
        .env("LOOP_ENGINEERING_HOME", home.join("loopx"))
        .env("HOME", home.join("home"))
        .env("LOOPX_ROOT", home.join("root"))
        .env("LOOP_UPDATE_DIR", home.join("update"))
        .env("LOOP_EVENTS_DIR", home.join("events"))
        .env(
            "LOOPX_KEYCHAIN",
            format!("file:{}", home.join("keys").display()),
        )
        .output()
        .unwrap()
}

fn text(o: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).to_string(),
        String::from_utf8_lossy(&o.stderr).to_string(),
    )
}

fn connectors(home: &Path, entries: serde_json::Value) {
    let dir = home.join("loopx");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("connectors.json"), entries.to_string()).unwrap();
}

#[test]
fn top_level_help_lists_every_slack_subcommand() {
    let d = tempfile::tempdir().unwrap();
    let o = loopx(d.path(), &["--help"]);
    assert_eq!(o.status.code(), Some(0));
    let out = text(&o).0;
    let line = out
        .lines()
        .find(|l| l.trim_start().starts_with("slack "))
        .unwrap_or_else(|| panic!("no slack line in:\n{out}"));
    for sub in [
        "listen",
        "status",
        "test",
        "install",
        "uninstall",
        "restart",
        "manifest",
    ] {
        assert!(
            line.split(|c: char| !c.is_ascii_alphanumeric())
                .any(|w| w == sub),
            "`{sub}` missing from: {line}"
        );
    }
}

#[test]
fn status_without_a_status_file_is_not_installed() {
    let d = tempfile::tempdir().unwrap();
    let o = loopx(d.path(), &["slack", "status"]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(text(&o).0.trim(), "Slack listener: not installed");
}

fn agents_dir(home: &Path) -> std::path::PathBuf {
    home.join("home/Library/LaunchAgents")
}

fn write_status(home: &Path, state: &str) {
    let dir = home.join("root/outputs/slack");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("status.json"),
        format!(
            r#"{{"state":"{state}","since":"2026-11-15T10:00:00+00:00","last_event":null,"last_error":null}}"#
        ),
    )
    .unwrap();
}

fn mark_installed(home: &Path) {
    let dir = agents_dir(home);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("com.hermes.loopx-slack.plist"), "x").unwrap();
}

#[test]
fn status_of_a_live_installed_listener_is_connected() {
    let d = tempfile::tempdir().unwrap();
    write_status(d.path(), "connected");
    mark_installed(d.path());
    // A listener is running while something holds its lock.
    let _lock = loopx_core::file_lock::FileLock::try_exclusive(
        &d.path().join("root/outputs/slack/listener.lock"),
    )
    .unwrap()
    .unwrap();
    let o = loopx(d.path(), &["slack", "status"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(text(&o)
        .0
        .starts_with("Slack listener: installed, connected since 2026-11-15T10:00:00+00:00"));
}

#[test]
fn a_status_file_with_no_listener_behind_it_is_a_crash() {
    let d = tempfile::tempdir().unwrap();
    write_status(d.path(), "connected");
    mark_installed(d.path());
    let o = loopx(d.path(), &["slack", "status"]);
    assert!(
        text(&o)
            .0
            .starts_with("Slack listener: installed, not running"),
        "{:?}",
        text(&o)
    );
}

#[test]
fn a_stale_status_file_without_the_agent_reads_as_not_installed() {
    let d = tempfile::tempdir().unwrap();
    write_status(d.path(), "connected");
    let o = loopx(d.path(), &["slack", "status"]);
    assert_eq!(text(&o).0.trim(), "Slack listener: not installed");
}

#[test]
fn manifest_prints_the_slack_app_manifest() {
    let d = tempfile::tempdir().unwrap();
    let o = loopx(d.path(), &["slack", "manifest"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(text(&o).0.contains("socket_mode_enabled: true"));
}

/// A scratch install target: explicit dirs and a fake launchctl that logs argv.
struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        let script = dir.path().join("fake-launchctl");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" >> {}\nexit 0\n",
                dir.path().join("calls.log").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        Sandbox { dir }
    }
    fn agents(&self) -> String {
        self.dir.path().join("agents").display().to_string()
    }
    fn launchd(&self) -> String {
        self.dir.path().join("launchd").display().to_string()
    }
    fn run(&self, args: &[&str]) -> Output {
        let mut full = args.to_vec();
        let (a, l) = (self.agents(), self.launchd());
        full.extend(["--launch-agents-dir", &a, "--launchd-dir", &l]);
        let fake = self.dir.path().join("fake-launchctl").display().to_string();
        loopx_env(self.dir.path(), &full, &[("LOOPX_LAUNCHCTL_CMD", &fake)])
    }
    fn calls(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("calls.log")).unwrap_or_default()
    }
}

fn save_tokens(home: &Path, id: &str) {
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let keys = format!("file:{}", home.join("keys").display());
    std::env::set_var("LOOPX_KEYCHAIN", &keys);
    std::env::set_var("LOOP_ENGINEERING_HOME", home.join("loopx"));
    let store = loopx_core::connectors::secrets::open_connector_store().unwrap();
    store.set(id, "xoxb-TESTSECRET").unwrap();
    store
        .set(
            &loopx_core::connectors::slack_bot::app_token_key(id),
            "xapp-TESTSECRET",
        )
        .unwrap();
    std::env::remove_var("LOOPX_KEYCHAIN");
    std::env::remove_var("LOOP_ENGINEERING_HOME");
}

fn agent_account(home: &Path) {
    connectors(
        home,
        serde_json::json!([{"id": "agent", "type": "slack_bot", "label": "Agent",
                            "enabled": true, "settings": {"allowed_users": "U01ABC"}}]),
    );
}

#[test]
fn install_without_yes_and_without_a_terminal_refuses_and_writes_nothing() {
    let sb = Sandbox::new();
    agent_account(sb.dir.path());
    save_tokens(sb.dir.path(), "agent");
    let o = sb.run(&["slack", "install"]);
    assert_eq!(o.status.code(), Some(2), "{:?}", text(&o));
    let (out, err) = text(&o);
    // It says what it would do before refusing.
    assert!(out.contains("com.hermes.loopx-slack.plist"), "{out}");
    assert!(out.contains("while you are logged in"));
    assert!(out.contains("can start agent work from Slack messages from the allowlisted users"));
    assert!(err.contains("--yes"), "{err}");
    assert!(!Path::new(&sb.agents()).exists());
    assert!(!Path::new(&sb.launchd()).exists());
    assert_eq!(sb.calls(), "");
}

#[test]
fn install_status_restart_uninstall_round_trip_in_a_scratch_dir() {
    let sb = Sandbox::new();
    agent_account(sb.dir.path());
    save_tokens(sb.dir.path(), "agent");

    let o = sb.run(&["slack", "install", "--yes"]);
    assert_eq!(o.status.code(), Some(0), "{:?}", text(&o));
    let plist = Path::new(&sb.agents()).join("com.hermes.loopx-slack.plist");
    let body = std::fs::read_to_string(&plist).unwrap();
    assert!(body.contains("<string>listen</string>"));
    assert!(body.contains("<key>SuccessfulExit</key>"));
    assert!(!body.contains("TESTSECRET"));
    assert_eq!(sb.calls().trim(), format!("load -w {}", plist.display()));

    let o = sb.run(&["slack", "status"]);
    assert!(
        text(&o).0.starts_with("Slack listener: installed, stopped"),
        "{:?}",
        text(&o)
    );

    let o = sb.run(&["slack", "restart"]);
    assert_eq!(o.status.code(), Some(0), "{:?}", text(&o));
    let uid = unsafe { libc::getuid() };
    assert!(sb
        .calls()
        .trim_end()
        .ends_with(&format!("kickstart -k gui/{uid}/com.hermes.loopx-slack")));

    let o = sb.run(&["slack", "uninstall"]);
    assert_eq!(o.status.code(), Some(0), "{:?}", text(&o));
    assert!(!plist.exists());
    assert!(!Path::new(&sb.launchd())
        .join("com.hermes.loopx-slack.plist")
        .exists());
    // Twice is fine and calls nothing more.
    let n = sb.calls().lines().count();
    assert_eq!(sb.run(&["slack", "uninstall"]).status.code(), Some(0));
    assert_eq!(sb.calls().lines().count(), n);
    // And restart now says why it cannot.
    let o = sb.run(&["slack", "restart"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).1.contains("not installed"));
}

#[test]
fn install_with_a_failing_preflight_writes_nothing() {
    let sb = Sandbox::new();
    agent_account(sb.dir.path()); // no tokens saved
    let o = sb.run(&["slack", "install", "--yes"]);
    assert_eq!(o.status.code(), Some(1), "{:?}", text(&o));
    assert!(text(&o).1.contains("bot token"), "{:?}", text(&o));
    assert!(!Path::new(&sb.agents()).exists());
    assert_eq!(sb.calls(), "");
}

/// The guard: with a development sandbox active and no explicit directories,
/// nothing may reach the real launchctl or write under the home's LaunchAgents.
#[test]
fn a_sandbox_without_explicit_dirs_never_reaches_launchd() {
    let d = tempfile::tempdir().unwrap();
    agent_account(d.path());
    save_tokens(d.path(), "agent");
    let fake = d.path().join("fake-launchctl").display().to_string();
    for sub in [
        &["slack", "install", "--yes"][..],
        &["slack", "restart"],
        &["slack", "uninstall"],
    ] {
        // Even with a fake launchctl injected.
        let o = loopx_env(d.path(), sub, &[("LOOPX_LAUNCHCTL_CMD", &fake)]);
        assert_eq!(o.status.code(), Some(2), "{sub:?}: {:?}", text(&o));
        assert!(text(&o).1.contains("--launch-agents-dir"), "{:?}", text(&o));
    }
    assert!(!d.path().join("home/Library").exists());
    assert!(!d.path().join("root/launchd").exists());
}

#[test]
fn listen_without_an_account_is_a_config_error() {
    let d = tempfile::tempdir().unwrap();
    let o = loopx(d.path(), &["slack", "listen"]);
    assert_eq!(o.status.code(), Some(78));
    assert!(
        text(&o).1.contains("No Slack agent is set up"),
        "{:?}",
        text(&o)
    );
    assert!(!d.path().join("root/outputs/slack/listener.lock").exists());
}

#[test]
fn listen_and_test_without_tokens_are_config_errors() {
    let d = tempfile::tempdir().unwrap();
    connectors(
        d.path(),
        serde_json::json!([{"id": "agent", "type": "slack_bot", "label": "Agent",
                            "enabled": true, "settings": {"allowed_users": "U01ABC"}}]),
    );
    for sub in ["listen", "test"] {
        let o = loopx(d.path(), &["slack", sub, "--account", "agent"]);
        assert_eq!(o.status.code(), Some(78), "{sub}");
        assert!(
            text(&o).1.contains("bot token is not set"),
            "{sub}: {:?}",
            text(&o)
        );
    }
}

#[test]
fn bad_usage_exits_2() {
    let d = tempfile::tempdir().unwrap();
    for args in [
        &["slack"][..],
        &["slack", "nope"],
        &["slack", "listen", "--account"],
    ] {
        let o = loopx(d.path(), args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(text(&o).1.contains("Usage: loopx slack"), "{args:?}");
    }
}
