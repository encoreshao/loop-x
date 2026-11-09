//! `loopx watch`: the terminal view of every loop step. Prints what is running
//! now and the newest steps from the event ledger; `--follow` keeps going.

use loopx_core::live_trace::{self, Filter, Source, TraceLine};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "Usage: loopx watch [--follow|-f] [--lines N] [--scope TEXT] [--agents|--steps] [--attention] [--grep TEXT] [--events-dir DIR]";

fn print(lines: &[TraceLine]) {
    let width = lines
        .iter()
        .map(|l| l.scope.chars().count())
        .max()
        .unwrap_or(0)
        .min(32);
    for l in lines {
        println!("{}", l.render(width));
    }
}

pub fn run(args: &[String]) -> ExitCode {
    let mut follow = false;
    let mut limit = 40usize;
    let mut dir: Option<PathBuf> = None;
    let mut filter = Filter::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--follow" | "-f" => follow = true,
            "--lines" => match it.next().and_then(|v| v.parse().ok()) {
                Some(n) => limit = n,
                None => return usage(),
            },
            "--scope" => match it.next() {
                Some(v) => filter.scope = Some(v.clone()),
                None => return usage(),
            },
            "--grep" => match it.next() {
                Some(v) => filter.text = Some(v.clone()),
                None => return usage(),
            },
            "--agents" => filter.source = Some(Source::Agent),
            "--steps" => filter.source = Some(Source::Step),
            "--attention" => filter.attention = true,
            "--events-dir" => match it.next() {
                Some(v) => dir = Some(PathBuf::from(v)),
                None => return usage(),
            },
            _ => return usage(),
        }
    }
    let events_dir = dir.unwrap_or_else(loopx_core::events::default_events_dir);
    let status = loopx_core::loop_scheduler::status_path_for_loop("issue-loop", None);

    let running = live_trace::running_now(&status);
    if running.is_empty() {
        println!("Nothing is running right now.");
    } else {
        println!("Running now:");
        for r in &running {
            println!("  ▶ {r}");
        }
    }
    println!();
    let mut lines = live_trace::recent(&events_dir, &status, limit, None, &filter);
    print(&lines);
    if !follow {
        return ExitCode::SUCCESS;
    }
    let mut cursor = lines.last().map(|l| l.at.clone());
    loop {
        std::thread::sleep(Duration::from_secs(1));
        lines = live_trace::recent(&events_dir, &status, 500, cursor.as_deref(), &filter);
        if let Some(last) = lines.last() {
            cursor = Some(last.at.clone());
            print(&lines);
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(1)
}
