//! Mail chat tools: search a connected mailbox, read one message, and (after the
//! user approves the card) send one from a Gmail mailbox. Everything in a message
//! is third-party text, so it is wrapped as untrusted. Outlook has no send: its
//! grant lacks Mail.Send and Inbox Triage's grant is kept send-free.

use super::{
    connector_of, get, or_empty, req_str, untrusted_value, Arg, Env, Mode, Tool, ToolError,
    ToolResult,
};
use crate::connectors::{Account, Connector};
use serde_json::{json, Map, Value};

const MAX_LIMIT: i64 = 20;
const DEFAULT_LIMIT: i64 = 10;

/// `min(max(int(args.get("limit") or 10), 1), 20)`.
fn limit(args: &Map<String, Value>) -> Result<u32, ToolError> {
    let n = match args.get("limit") {
        Some(v) if super::truthy(v) => match v {
            Value::Number(n) => n.as_f64().map(|f| f.trunc() as i64),
            Value::String(s) => s.trim().parse::<i64>().ok(),
            _ => None,
        }
        .ok_or_else(|| ToolError::Other("ValueError".into()))?,
        _ => DEFAULT_LIMIT,
    };
    Ok(n.clamp(1, MAX_LIMIT) as u32)
}

fn search(_e: &Env, c: Option<&dyn Connector>, args: &Map<String, Value>) -> ToolResult {
    let c = connector_of(c)?;
    let rows = c
        .search_messages(&req_str(args, "query")?, limit(args)?)
        .map_err(ToolError::from_connector)?;
    let messages: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({"id": get(r, "id"), "subject": untrusted_value(get(r, "subject")),
                   "from": untrusted_value(get(r, "from")), "date": get(r, "date"),
                   "snippet": untrusted_value(get(r, "snippet"))})
        })
        .collect();
    Ok(json!({ "messages": messages }))
}

fn read(_e: &Env, c: Option<&dyn Connector>, args: &Map<String, Value>) -> ToolResult {
    let c = connector_of(c)?;
    let m = c
        .read_message(&req_str(args, "id")?)
        .map_err(ToolError::from_connector)?;
    let attachments: Vec<Value> = get(&m, "attachments")
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| Value::String(untrusted_value(a)))
        .collect();
    Ok(json!({"message": {
        "id": get(&m, "id"), "date": get(&m, "date"),
        "from": untrusted_value(get(&m, "from")), "to": untrusted_value(get(&m, "to")),
        "cc": untrusted_value(get(&m, "cc")), "subject": untrusted_value(get(&m, "subject")),
        "body_text": untrusted_value(get(&m, "body_text")), "attachments": attachments}}))
}

/// A comma/semicolon separated address list; `to` may not be empty.
fn addresses(args: &Map<String, Value>, label: &str) -> Result<Vec<String>, ToolError> {
    let text = args.get(label).map(or_empty).unwrap_or_default();
    let parts: Vec<String> = text
        .replace(';', ",")
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    if parts.is_empty() && label == "to" {
        return Err(ToolError::input("to is required"));
    }
    Ok(parts)
}

/// First mailbox account that is a Gmail inbox (sending is Gmail-only).
fn select_gmail(env: &Env, candidates: &[Account], _args: &Map<String, Value>) -> Option<String> {
    candidates
        .iter()
        .find(|a| (env.inbox_provider)(&a.id).as_deref() == Some("gmail"))
        .map(|a| a.id.clone())
}

fn send(_e: &Env, c: Option<&dyn Connector>, args: &Map<String, Value>) -> ToolResult {
    let c = connector_of(c)?;
    c.send_message(
        &addresses(args, "to")?,
        &addresses(args, "cc")?,
        &req_str(args, "subject")?,
        &req_str(args, "body")?,
    )
    .map_err(ToolError::from_connector)
}

static SEARCH_ARGS: [Arg; 2] = [Arg::req("query"), Arg::opt_int("limit")];
static READ_ARGS: [Arg; 1] = [Arg::req("id")];
static SEND_ARGS: [Arg; 4] = [
    Arg::req("to"),
    Arg::req("subject"),
    Arg::req("body"),
    Arg::opt("cc"),
];

pub(crate) fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "mail.search",
            description: "Search a connected mailbox (query: Gmail search syntax or free text; limit: up to 20) and list matching messages without bodies",
            args: &SEARCH_ARGS,
            mode: Mode::Read,
            connector_type: Some("mailbox"),
            handler: search,
            select: None,
        },
        Tool {
            name: "mail.read",
            description: "Read one message (id from mail.search) with its body text",
            args: &READ_ARGS,
            mode: Mode::Read,
            connector_type: Some("mailbox"),
            handler: read,
            select: None,
        },
        Tool {
            name: "mail.send",
            description: "Send a plain-text email from the user's Gmail mailbox (to/cc: comma-separated plain addresses, at most 10 in total; needs the user's approval)",
            args: &SEND_ARGS,
            mode: Mode::Write,
            connector_type: Some("mailbox"),
            handler: send,
            select: Some(select_gmail),
        },
    ]
}
