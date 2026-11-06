//! `loopx mr-fix`: real subprocesses against a scratch home.

use std::path::Path;
use std::process::{Command, Output};

fn loopx(home: &Path, args: &[&str]) -> Output {
    loopx_with(home, args, &[])
}

/// [`loopx`] with extra environment variables.
fn loopx_with(home: &Path, args: &[&str], envs: &[(&str, &std::ffi::OsStr)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loopx"))
        .args(args)
        .envs(envs.iter().copied())
        .env("LOOP_ENGINEERING_HOME", home.join("loopx"))
        .env("HOME", home.join("home"))
        .env("LOOPX_ROOT", home.join("root"))
        .env("LOOP_UPDATE_DIR", home.join("update"))
        .env("LOOP_EVENTS_DIR", home.join("events"))
        .env_remove("GITLAB_TOKEN")
        .env_remove("GITLAB_URL")
        .output()
        .unwrap()
}

#[test]
fn mr_fix_run_rejects_a_malformed_ref() {
    let d = tempfile::tempdir().unwrap();
    for bad in ["nonsense", "a:1!", "a:o/n/m!1", "a:1!2;rm"] {
        let out = loopx(d.path(), &["mr-fix", "run", bad]);
        assert_eq!(out.status.code(), Some(1), "{bad}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("not a valid merge request reference"), "{err}");
    }
    // No side effects: nothing was created in the scratch loopx home.
    assert!(!d.path().join("loopx").join("mr-fix").exists());
}

#[test]
fn mr_fix_announce_rejects_invalid_plan_json() {
    let d = tempfile::tempdir().unwrap();
    let draft = d.path().join("plan-draft.json");
    let plan = d.path().join("plan.json");
    for bad in [
        "not json",
        r#"{"changes":[]}"#,
        r#"{"changes":[{"severity":"low","text":"x"}],"extra":1}"#,
    ] {
        std::fs::write(&draft, bad).unwrap();
        let out = loopx_with(
            d.path(),
            &["mr-fix", "announce", draft.to_str().unwrap()],
            &[("LOOPX_MR_FIX_PLAN", plan.as_os_str())],
        );
        assert_eq!(out.status.code(), Some(1), "{bad}");
        assert!(!String::from_utf8_lossy(&out.stderr).trim().is_empty());
        assert!(!plan.exists(), "an invalid plan must not reach the runner");
    }
}

#[test]
fn mr_fix_announce_needs_the_plan_env_and_a_readable_draft() {
    let d = tempfile::tempdir().unwrap();
    let out = loopx(
        d.path(),
        &["mr-fix", "announce", "/nonexistent/plan-draft.json"],
    );
    assert_eq!(out.status.code(), Some(1));
}
