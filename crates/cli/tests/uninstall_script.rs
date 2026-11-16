//! `bin/scripts/uninstall-loopx.sh` removes the Slack listener's agent. Runs
//! the real script against scratch directories with a fake launchctl and
//! `--keep-keychain`, so nothing real is reached.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn script() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bin/scripts/uninstall-loopx.sh")
}

#[test]
fn it_boots_out_and_removes_the_slack_listener_agent_only_among_ours() {
    let d = tempfile::tempdir().unwrap();
    let agents = d.path().join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    for name in [
        "com.hermes.loopx-slack.plist",
        "com.hermes.loop-engineering.plist",
        "com.someone.else.plist",
    ] {
        std::fs::write(agents.join(name), "<plist/>").unwrap();
    }
    let log = d.path().join("calls.log");
    let fake = d.path().join("fake-launchctl");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\necho \"$@\" >> {}\nexit 0\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let out = Command::new("bash")
        .arg(script())
        .args(["--project-dir"])
        .arg(d.path().join("project"))
        .args(["--launch-agents-dir"])
        .arg(&agents)
        .arg("--keep-keychain")
        .env("HOME", d.path().join("home"))
        .env("LOOPX_LAUNCHCTL_CMD", &fake)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(!agents.join("com.hermes.loopx-slack.plist").exists());
    assert!(!agents.join("com.hermes.loop-engineering.plist").exists());
    assert!(
        agents.join("com.someone.else.plist").exists(),
        "someone else's agent was removed"
    );
    let calls = std::fs::read_to_string(&log).unwrap();
    let uid = unsafe { libc::getuid() };
    assert!(
        calls.contains(&format!("bootout gui/{uid}/com.hermes.loopx-slack")),
        "{calls}"
    );
    assert!(!calls.contains("com.someone.else"), "{calls}");
}
