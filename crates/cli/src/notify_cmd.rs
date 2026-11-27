//! `loopx notify`: post to the loop's Slack incoming webhook.

use loopx_core::outbox::{Delivery, Outbox, Outgoing, Urgency};
use loopx_core::{connectors_config, slack_report};
use serde_json::Value;
use std::process::ExitCode;

const USAGE: &str =
    "Usage: loopx notify [--bundle=<name>] [--blocks=<json blocks array>] [--urgent] [--about=<key>] <message text>";

pub fn run(args: &[String]) -> ExitCode {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if held(std::env::var(slack_report::HOLD_ENV).ok().as_deref()) {
        // A batch agent: the run posts one Slack report at the end instead.
        println!("Held: this batch run posts one Slack report when it finishes.");
        return ExitCode::SUCCESS;
    }
    match send(args) {
        Ok(Delivery::Sent) => {
            println!("Sent.");
            ExitCode::SUCCESS
        }
        Ok(Delivery::Held) => {
            println!("Held for quiet hours: it goes out when they end.");
            ExitCode::SUCCESS
        }
        Ok(Delivery::Retrying(why)) => {
            println!("Not sent yet ({why}): LoopX retries it, so do not send it again.");
            ExitCode::SUCCESS
        }
        Ok(Delivery::AlreadyTold) => {
            println!("Not sent: the same alert went out in the last day.");
            ExitCode::SUCCESS
        }
        Ok(Delivery::Failed(why)) => {
            eprintln!("Slack notification failed: {why}");
            ExitCode::from(1)
        }
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
    }
}

fn send(args: &[String]) -> Result<Delivery, String> {
    let mut bundle = None;
    let mut blocks: Option<Value> = None;
    let mut urgency = Urgency::Routine;
    let mut about = None;
    let mut positional = Vec::new();
    for arg in args {
        if let Some(name) = arg.strip_prefix("--bundle=") {
            bundle = Some(name);
        } else if let Some(json) = arg.strip_prefix("--blocks=") {
            blocks = Some(
                serde_json::from_str(json).map_err(|e| format!("Invalid --blocks JSON: {e}"))?,
            );
        } else if let Some(key) = arg.strip_prefix("--about=") {
            about = Some(key).filter(|k| !k.is_empty());
        } else if arg == "--urgent" {
            urgency = Urgency::Urgent;
        } else {
            positional.push(arg.as_str());
        }
    }
    let Some(text) = positional.first() else {
        return Err(USAGE.to_string());
    };
    let msg = Outgoing {
        urgency,
        ..Outgoing::slack_text(text, blocks, bundle).about(about)
    };
    Ok(Outbox::real().deliver(msg, &connectors_config::live_send))
}

fn held(hold: Option<&str>) -> bool {
    hold.is_some_and(|v| !v.is_empty() && v != "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_agent_s_messages_are_held() {
        assert!(held(Some("1")));
        assert!(!held(None));
        assert!(!held(Some("")));
        assert!(!held(Some("0")));
    }
}
