//! `loopx metrics` (replaces `bin/metrics.py`'s CLI): the loop's performance
//! report from the event log.

use chrono::{Duration, Utc};
use loopx_core::metrics;
use loopx_core::pydumps::py_repr_str;
use std::path::PathBuf;
use std::process::ExitCode;

/// Python `_parse_flag`: the value after `name`, `None` if absent or last.
fn parse_flag<'a>(argv: &'a [String], name: &str) -> Option<&'a str> {
    let idx = argv.iter().position(|a| a == name)?;
    argv.get(idx + 1).map(String::as_str)
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("{msg}");
    ExitCode::from(1)
}

/// `loopx metrics [--days N] [--project NAME] [--events-dir DIR]`.
pub fn run(argv: &[String]) -> ExitCode {
    let (mut since_date, mut until_date) = (None, None);
    if let Some(raw) = parse_flag(argv, "--days") {
        let Ok(days) = raw.trim().parse::<i64>() else {
            return fail(&format!(
                "metrics: --days must be an integer, got {}",
                py_repr_str(raw)
            ));
        };
        let until = Utc::now().date_naive();
        let since = until - Duration::days(days - 1);
        since_date = Some(since.to_string());
        until_date = Some(until.to_string());
    }
    let project = parse_flag(argv, "--project");
    let events_dir = match parse_flag(argv, "--events-dir").filter(|s| !s.is_empty()) {
        None => None,
        Some(raw) if !std::path::Path::new(raw).exists() => {
            return fail(&format!("metrics: --events-dir {raw} does not exist"));
        }
        Some(raw) => Some(PathBuf::from(raw)),
    };
    let report = metrics::build_report(
        events_dir.as_deref(),
        since_date.as_deref(),
        until_date.as_deref(),
        project,
        None,
    );
    println!("{}", metrics::format_report(&report));
    ExitCode::SUCCESS
}
