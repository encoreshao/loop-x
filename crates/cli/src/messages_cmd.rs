//! `loopx messages read|add`: the loop's side of the dashboard chat inbox
//! (replaces `dashboard_server.py read-messages` / `add-message`).

use loopx_core::messages::MessageStore;
use loopx_core::{pyjson, runner_util};
use serde_json::Value;
use std::process::ExitCode;

pub fn run(args: &[String]) -> ExitCode {
    let store = MessageStore::for_root(&runner_util::repo_root());
    match args.first().map(String::as_str) {
        Some("read") => {
            println!(
                "{}",
                pyjson::to_string_pretty(&Value::Array(store.pop_unseen_user_messages()))
            );
            ExitCode::SUCCESS
        }
        Some("add") => {
            if args.len() < 3 {
                eprintln!("Usage: loopx messages add <user|loop> <text>");
                return ExitCode::from(1);
            }
            if args[1] != "user" && args[1] != "loop" {
                eprintln!("Usage: loopx messages add <user|loop> <text>");
                return ExitCode::from(1);
            }
            store.append_message(&args[1], &args[2], None, None);
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("Usage: loopx messages read | add <user|loop> <text>");
            ExitCode::from(1)
        }
    }
}
