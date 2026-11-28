//! `loopx review config`: validate a `.loopx/review.yaml` and print the effective MR
//! review configuration with the source of each value. Reads local files only.

use loopx_core::config::{default_config_path, Config};
use loopx_core::loops_config;
use loopx_core::review::config::{self as review_config, Sources, REPO_FILE};
use serde_json::Value;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "Usage: loopx review config [--file <path>] [--project <alias>]

Prints the MR review configuration the MR Review loop would use, with where each
value comes from (default, loop settings, repo, projects.json). --file defaults to
.loopx/review.yaml in the current directory; --project adds that project's
personal `review` block from projects.json.";

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

pub fn run(args: &[String]) -> ExitCode {
    if args.first().map(String::as_str) != Some("config") || args.iter().any(|a| a == "--help") {
        println!("{USAGE}");
        return ExitCode::from(if args.iter().any(|a| a == "--help") {
            0
        } else {
            2
        });
    }
    let file = flag(args, "--file").map_or_else(|| PathBuf::from(REPO_FILE), PathBuf::from);
    let repo = match std::fs::read_to_string(&file) {
        Ok(text) => Some(review_config::parse_repo_file(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("No {} found; showing defaults.", file.display());
            None
        }
        Err(e) => Some(Err(format!("could not read {}: {e}", file.display()))),
    };
    let personal = match flag(args, "--project") {
        None => None,
        Some(alias) => {
            let project = Config::load(&default_config_path())
                .map_err(|e| e.to_string())
                .and_then(|c| c.project(alias).map_err(|e| e.to_string()));
            match project {
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(1);
                }
                Ok(p) => p.get("review").map(review_config::parse_personal),
            }
        }
    };
    let loop_settings: Option<Value> =
        loops_config::get_loop("mr-review-loop", &loops_config::default_config_path(), None)
            .ok()
            .and_then(|l| l.get("settings").cloned());
    let loop_min = loop_settings
        .as_ref()
        .and_then(|s| s.get("min_severity"))
        .and_then(Value::as_str);
    let cfg = review_config::resolve(Sources {
        loop_min_severity: loop_min,
        repo,
        personal,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&cfg.explain()).unwrap_or_default()
    );
    if cfg.notes.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
