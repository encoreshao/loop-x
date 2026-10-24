//! `loopx update`: the update check for this checkout (the app updates through
//! Sparkle, the binary through Homebrew or `install-loopx.sh --upgrade`).
//!
//! State lives in `$LOOP_UPDATE_DIR` (else `<repo root>/outputs/update`):
//! `status.json`, `progress.json`, `notified.json`, `app_path.txt`,
//! `upgrade.lock` (see docs/contract/json-files.md).

use loopx_core::pyjson;
use loopx_core::update_check;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "Usage: loopx update check [--repo DIR] [--state-dir DIR] [--no-fetch]\n       loopx update status [--state-dir DIR]\n       loopx update notification [--state-dir DIR]";

/// `--name VALUE` or `--name=VALUE` anywhere in `args`.
fn flag(args: &[String], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix(&prefix) {
            return Some(v.to_string());
        }
    }
    None
}

fn print_json(v: &Value) {
    println!("{}", pyjson::to_string_pretty(v));
}

pub fn run(args: &[String]) -> ExitCode {
    let Some((cmd, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let repo = flag(rest, "--repo").map(PathBuf::from);
    let state = flag(rest, "--state-dir").map(PathBuf::from);
    match cmd.as_str() {
        "check" => {
            let fetch = !rest.iter().any(|a| a == "--no-fetch");
            let status = update_check::check(repo.as_deref(), state.as_deref(), fetch);
            print_json(&Value::Object(status));
            ExitCode::SUCCESS
        }
        "status" => {
            let status = update_check::read_status(state.as_deref());
            let progress = loopx_server::ui::update::read_progress(
                &state
                    .clone()
                    .unwrap_or_else(update_check::default_state_dir),
            );
            print_json(&json!({"status": status, "progress": progress}));
            ExitCode::SUCCESS
        }
        "notification" => {
            if let Some(text) = update_check::take_notification(state.as_deref()) {
                println!("{text}");
            }
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
