//! `loopx timer`: set, list and cancel one-off timers (`loopx_core::timers`).

use chrono::{Local, Utc};
use loopx_core::loops_config;
use loopx_core::timers::{self, Action, Timer};
use std::process::ExitCode;

const USAGE: &str = "Usage:
  loopx timer add <when> <note> [--urgent]
  loopx timer add <when> <note> --run <loop> [--issue <alias>#<iid>]
  loopx timer list [--json]
  loopx timer cancel <id>

<when>: 30m, 2h, 1d, 1h30m, 09:00, 2026-12-01 09:00 or an RFC 3339 time.
Without --run the note is sent to you as a reminder when it is due; with --run
the loop runs then (one issue's run with --issue) and its agent sees the note.";

pub fn run(args: &[String]) -> ExitCode {
    let result = match args.first().map(String::as_str) {
        Some("add") => add(&args[1..]),
        Some("list") => list(&args[1..]),
        Some("cancel") => cancel(&args[1..]),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}

/// Who set a timer: an agent inside a run, else you.
fn set_by() -> String {
    match std::env::var("LOOP_RUN_ID") {
        Ok(id) if !id.is_empty() => "agent".into(),
        _ => "you".into(),
    }
}

fn add(args: &[String]) -> Result<(), String> {
    let mut positional = Vec::new();
    let (mut urgent, mut run, mut issue) = (false, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--urgent" => urgent = true,
            "--run" => run = Some(it.next().ok_or("--run needs a loop name")?.clone()),
            "--issue" => issue = Some(it.next().ok_or("--issue needs <alias>#<iid>")?.clone()),
            _ => positional.push(a.as_str()),
        }
    }
    let [when, note @ ..] = positional.as_slice() else {
        return Err(USAGE.to_string());
    };
    let note = note.join(" ");
    let now = Utc::now();
    let due = timers::parse_when(when, now)?;
    let action = match run {
        None if issue.is_some() => return Err("--issue goes with --run issue-loop".into()),
        None => Action::Remind { urgent },
        Some(name) => {
            let name = loops_config::canonical_loop_name(&name).to_string();
            loops_config::get_loop(&name, &loops_config::default_config_path(), None)
                .map_err(|_| format!("no loop is called \"{name}\""))?;
            let args = match issue {
                None => Vec::new(),
                Some(_) if name != loops_config::ISSUE_LOOP => {
                    return Err("--issue goes with --run issue-loop".into())
                }
                Some(spec) => {
                    let (alias, iid) = spec
                        .split_once('#')
                        .filter(|(a, i)| {
                            !a.is_empty() && i.chars().all(|c| c.is_ascii_digit()) && !i.is_empty()
                        })
                        .ok_or("--issue looks like web#7")?;
                    vec![alias.to_string(), iid.to_string()]
                }
            };
            Action::Run {
                loop_name: name,
                args,
            }
        }
    };
    let t = timers::add(&timers::default_dir(), due, &note, action, &set_by(), now)?;
    println!("Timer {} set for {}: {}", t.id, local(&t), what(&t));
    Ok(())
}

fn local(t: &Timer) -> String {
    t.due_at
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

fn what(t: &Timer) -> String {
    match &t.action {
        Action::Remind { urgent: true } => format!("urgent reminder \"{}\"", t.note),
        Action::Remind { .. } => format!("reminder \"{}\"", t.note),
        Action::Run { loop_name, args } if args.len() == 2 => {
            format!("run {loop_name} on {}#{} \"{}\"", args[0], args[1], t.note)
        }
        Action::Run { loop_name, .. } => format!("run {loop_name} \"{}\"", t.note),
    }
}

fn list(args: &[String]) -> Result<(), String> {
    let all = timers::list(&timers::default_dir());
    if args.iter().any(|a| a == "--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&all).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    if all.is_empty() {
        println!("No timers are waiting.");
    }
    for t in &all {
        println!("{}  {}  {} (set by {})", t.id, local(t), what(t), t.set_by);
    }
    Ok(())
}

fn cancel(args: &[String]) -> Result<(), String> {
    let [id] = args else {
        return Err("Usage: loopx timer cancel <id>".into());
    };
    if timers::cancel(&timers::default_dir(), id) {
        println!("Cancelled {id}.");
        Ok(())
    } else {
        Err(format!("no timer {id} is waiting"))
    }
}
