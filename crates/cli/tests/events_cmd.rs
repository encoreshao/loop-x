//! `loopx events emit`: real subprocesses against a scratch events dir.

use std::path::Path;
use std::process::{Command, Output};

fn emit(events: &Path, event_type: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loopx"))
        .args(["events", "emit", "--type", event_type, "--run-id", "r1"])
        .stdin(std::process::Stdio::null())
        .env("LOOP_ENGINEERING_HOME", events.join("home"))
        .env("LOOP_EVENTS_DIR", events)
        .output()
        .unwrap()
}

fn logged(events: &Path) -> String {
    std::fs::read_dir(events)
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
                .map(|e| std::fs::read_to_string(e.path()).unwrap())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn runner_only_event_types_are_refused() {
    let d = tempfile::tempdir().unwrap();
    for t in ["mr_autofix.escalated", "mr_autofix.completed"] {
        let o = emit(d.path(), t);
        assert!(!o.status.success(), "{t} was accepted");
        assert!(
            String::from_utf8_lossy(&o.stderr).contains("written only by LoopX"),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    assert!(!logged(d.path()).contains("mr_autofix"));
}

#[test]
fn agent_event_types_are_still_written() {
    let d = tempfile::tempdir().unwrap();
    let o = emit(d.path(), "issue.started");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(logged(d.path()).contains("issue.started"));
}
