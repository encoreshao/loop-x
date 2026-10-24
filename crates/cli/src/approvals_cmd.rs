//! `loopx approvals`: see and answer the loop's pending approvals (the same store
//! the dashboard server and the Mac app use).

use chrono::Utc;
use loopx_core::{approvals, pyjson};
use serde_json::Value;
use std::process::ExitCode;

const USAGE: &str = "Usage: loopx approvals list [--all]\n       loopx approvals show <id>\n       loopx approvals approve <id> [--by NAME]\n       loopx approvals deny <id> [--by NAME]";

pub fn run(args: &[String]) -> ExitCode {
    match dispatch(args) {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
    }
}

fn by(args: &[String]) -> String {
    args.iter()
        .position(|a| a == "--by")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "cli".to_string())
}

fn dispatch(args: &[String]) -> Result<String, String> {
    let dir = approvals::default_dir();
    let now = Utc::now();
    match args.first().map(String::as_str) {
        Some("list") => {
            let status = if args.iter().any(|a| a == "--all") {
                None
            } else {
                Some("pending")
            };
            Ok(pyjson::to_string_pretty(&Value::Array(approvals::list(
                &dir, status, now,
            ))))
        }
        Some("show") => {
            let id = args.get(1).ok_or(USAGE)?;
            approvals::expire_overdue(&dir, now);
            approvals::get(&dir, id)
                .map(|a| pyjson::to_string_pretty(&a))
                .ok_or_else(|| format!("no approval {id}"))
        }
        Some(cmd @ ("approve" | "deny")) => {
            let id = args.get(1).ok_or(USAGE)?;
            approvals::decide(&dir, id, cmd, &by(args), now)
                .map(|a| pyjson::to_string_pretty(&a))
                .map_err(|e| e.to_string())
        }
        _ => Err(USAGE.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_defaults_to_cli() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(by(&a(&["approve", "x"])), "cli");
        assert_eq!(by(&a(&["approve", "x", "--by", "mac-app"])), "mac-app");
    }

    #[test]
    fn unknown_subcommand_is_usage() {
        assert!(dispatch(&[]).unwrap_err().starts_with("Usage:"));
        assert!(dispatch(&["bogus".into()])
            .unwrap_err()
            .starts_with("Usage:"));
        assert!(dispatch(&["approve".into()])
            .unwrap_err()
            .starts_with("Usage:"));
    }
}
