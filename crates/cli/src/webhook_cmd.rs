//! `loopx webhook`: set up, check or switch off the GitLab webhook secret.

use loopx_core::gitlab_webhook::{PATH, TOKEN_ACCOUNT, TOKEN_SERVICE};
use loopx_core::keychain::open_store;
use std::process::ExitCode;

const USAGE: &str = "Usage: loopx webhook setup | status | off";

pub fn run(args: &[String]) -> ExitCode {
    let store = match open_store(TOKEN_SERVICE) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot open the secret store: {e}");
            return ExitCode::from(1);
        }
    };
    match args.first().map(String::as_str) {
        Some("setup") => {
            let token = loopx_core::sha256::hex(&loopx_core::sha256::random_bytes(24));
            if let Err(e) = store.set(TOKEN_ACCOUNT, &token) {
                eprintln!("cannot save the token: {e}");
                return ExitCode::from(1);
            }
            println!(
                "GitLab webhook ready.\n\n\
1. LoopX listens on 127.0.0.1 only, so expose it with a tunnel you run, e.g.\n   cloudflared tunnel --url http://127.0.0.1:8420\n   (only {PATH} answers requests from outside; every other page refuses them).\n\
2. In each GitLab project: Settings > Webhooks > Add new webhook\n   URL:          <your tunnel URL>{PATH}\n   Secret token: {token}\n   Triggers:     Comments, Issues events, Pipeline events\n\n\
A comment on, or assignment of, an issue then queues that issue within a minute;\na failed pipeline queues the Pipeline Doctor. Run `loopx webhook setup` again to\nreplace the token, `loopx webhook off` to stop accepting deliveries."
            );
            ExitCode::SUCCESS
        }
        Some("status") => {
            match store.get(TOKEN_ACCOUNT) {
                Ok(Some(t)) if !t.is_empty() => {
                    println!("On: deliveries to {PATH} with the saved secret token are accepted.")
                }
                _ => println!("Off: run `loopx webhook setup` to accept GitLab webhooks."),
            }
            ExitCode::SUCCESS
        }
        Some("off") => match store.delete(TOKEN_ACCOUNT) {
            Ok(()) => {
                println!("Off: GitLab webhook deliveries are refused.");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("cannot remove the token: {e}");
                ExitCode::from(1)
            }
        },
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
