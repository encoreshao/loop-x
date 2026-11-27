//! `timer.remind`, `timer.list`, `timer.cancel`: chat sets, shows and cancels
//! reminders (`crate::timers`). A reminder is a message to the user, so these
//! are local writes with no confirmation card; chat never sets a timer that
//! starts a loop.

use super::{req_str, Arg, Env, Mode, Tool, ToolError, ToolResult};
use crate::connectors::Connector;
use crate::timers::{self, Action, Timer};
use chrono::{Local, Utc};
use serde_json::{json, Map, Value};
use std::path::PathBuf;

fn dir(env: &Env) -> PathBuf {
    env.repo_root.join("outputs").join("timers")
}

fn shown(t: &Timer) -> Value {
    let what = match &t.action {
        Action::Remind { .. } => "reminder".to_string(),
        Action::Run { loop_name, args } if args.is_empty() => format!("runs {loop_name}"),
        Action::Run { loop_name, args } => format!("runs {loop_name} {}", args.join(" ")),
    };
    json!({"id": t.id,
           "due": t.due_at.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string(),
           "what": what, "note": t.note, "set_by": t.set_by})
}

fn remind(env: &Env, _c: Option<&dyn Connector>, args: &Map<String, Value>) -> ToolResult {
    let now = Utc::now();
    let due = timers::parse_when(&req_str(args, "when")?, now).map_err(ToolError::input)?;
    let urgent = args.get("urgent").and_then(Value::as_bool).unwrap_or(false);
    let t = timers::add(
        &dir(env),
        due,
        &req_str(args, "note")?,
        Action::Remind { urgent },
        "chat",
        now,
    )
    .map_err(ToolError::input)?;
    Ok(shown(&t))
}

fn list(env: &Env, _c: Option<&dyn Connector>, _args: &Map<String, Value>) -> ToolResult {
    let all: Vec<Value> = timers::list(&dir(env)).iter().map(shown).collect();
    Ok(json!({"timers": all}))
}

fn cancel(env: &Env, _c: Option<&dyn Connector>, args: &Map<String, Value>) -> ToolResult {
    let id = req_str(args, "id")?;
    if timers::cancel(&dir(env), id.trim()) {
        Ok(json!({"cancelled": id}))
    } else {
        Err(ToolError::input(
            "no waiting timer has that id; call timer.list",
        ))
    }
}

static REMIND_ARGS: [Arg; 3] = [Arg::req("when"), Arg::req("note"), Arg::opt("urgent")];
static NO_ARGS: [Arg; 0] = [];
static CANCEL_ARGS: [Arg; 1] = [Arg::req("id")];

pub(crate) fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "timer.remind",
            description: "Send the user a reminder later (when: 30m, 2h, 1d, 09:00 for the next 9 o'clock, or 2026-12-01 09:00 local time; note: what to remind them of; urgent: true to send it even during quiet hours)",
            args: &REMIND_ARGS,
            mode: Mode::Local,
            connector_type: None,
            handler: remind,
            select: None,
        },
        Tool {
            name: "timer.list",
            description: "List the waiting reminders and timers (id, when, what, note)",
            args: &NO_ARGS,
            mode: Mode::Local,
            connector_type: None,
            handler: list,
            select: None,
        },
        Tool {
            name: "timer.cancel",
            description: "Cancel a waiting reminder or timer by its id from timer.list",
            args: &CANCEL_ARGS,
            mode: Mode::Local,
            connector_type: None,
            handler: cancel,
            select: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::super::*;

    fn call(env: &Env, name: &str, args: serde_json::Value) -> ToolResult {
        let tool = tools().iter().find(|t| t.name == name).unwrap();
        (tool.handler)(env, None, args.as_object().unwrap())
    }

    #[test]
    fn chat_sets_lists_and_cancels_a_reminder() {
        let d = tempfile::tempdir().unwrap();
        let env = Env::new(d.path().join("repo"), d.path().join("home"));
        let set = call(
            &env,
            "timer.remind",
            json!({"when": "2h", "note": "call the bank"}),
        )
        .unwrap();
        assert_eq!(set["what"], "reminder");
        assert_eq!(set["set_by"], "chat");
        let listed = call(&env, "timer.list", json!({})).unwrap();
        assert_eq!(listed["timers"][0]["note"], "call the bank");
        let id = set["id"].as_str().unwrap();
        assert!(call(&env, "timer.cancel", json!({"id": id})).is_ok());
        assert!(call(&env, "timer.cancel", json!({"id": id})).is_err());
        assert!(call(
            &env,
            "timer.remind",
            json!({"when": "someday", "note": "x"})
        )
        .is_err());
    }
}
