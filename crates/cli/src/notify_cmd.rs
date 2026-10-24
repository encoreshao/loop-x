//! `loopx notify`: post to the loop's Slack incoming webhook (replaces bin/slack_notify.py).

use loopx_core::slack;
use serde_json::Value;
use std::process::ExitCode;

const USAGE: &str =
    "Usage: loopx notify [--bundle=<name>] [--blocks=<json blocks array>] <message text>";

pub fn run(args: &[String]) -> ExitCode {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match send(args) {
        Ok(status) => {
            println!("Slack response status: {status}");
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
    }
}

fn send(args: &[String]) -> Result<u16, String> {
    let mut bundle = None;
    let mut blocks: Option<Value> = None;
    let mut positional = Vec::new();
    for arg in args {
        if let Some(name) = arg.strip_prefix("--bundle=") {
            bundle = Some(name);
        } else if let Some(json) = arg.strip_prefix("--blocks=") {
            blocks = Some(
                serde_json::from_str(json).map_err(|e| format!("Invalid --blocks JSON: {e}"))?,
            );
        } else {
            positional.push(arg.as_str());
        }
    }
    let Some(text) = positional.first() else {
        return Err(USAGE.to_string());
    };
    let url = slack::load_webhook_url(&slack::default_config_path(), bundle)?;
    slack::post_message(&url, text, blocks.as_ref())
}
