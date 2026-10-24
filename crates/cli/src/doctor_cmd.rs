//! `loopx doctor [--json]`: first-launch checks (AI CLIs, credentials, config,
//! backend) with next-step hints. Always exits 0 on a completed check; the
//! `ready` field of the `--json` report says whether the loops could run.

use loopx_core::doctor;
use std::process::ExitCode;

const USAGE: &str = "Usage: loopx doctor [--json]";

pub fn run(args: &[String]) -> ExitCode {
    let json = match args {
        [] => false,
        [a] if a == "--json" => true,
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let report = doctor::check(&doctor::Env::from_process());
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        print!("{}", doctor::render(&report));
    }
    ExitCode::SUCCESS
}
