//! Which chat sessions have touched the user's private data (mail, calendar).
//!
//! [`super::run_in`] appends one line to `<repo_root>/outputs/chat-private/
//! <session>.jsonl` before a private tool runs, so a reader (the Slack
//! executor) can tell a reply may hold private data whatever CLI produced it.
//! Nothing here changes a tool's args, result or description.

use super::Tool;
use crate::file_lock::{lock_path_for, FileLock};
use serde_json::{json, Value};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Connector types whose data is private to the user.
const PRIVATE_CONNECTORS: &[&str] = &["mailbox", "google_calendar"];

/// The one predicate for "this tool reads or writes private data".
pub fn is_private(tool: &Tool) -> bool {
    tool.connector_type
        .is_some_and(|k| PRIVATE_CONNECTORS.contains(&k))
}

/// [`is_private`] for a registered tool name (`false` for an unknown name).
pub fn is_private_name(name: &str) -> bool {
    super::tools()
        .iter()
        .rev()
        .find(|t| t.name == name)
        .is_some_and(is_private)
}

/// `<repo_root>/outputs/chat-private/<session>.jsonl`, the session reduced to
/// `[A-Za-z0-9_-]` (anything else becomes `_`).
pub fn ledger_path(repo_root: &Path, session: &str) -> PathBuf {
    let safe: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    repo_root
        .join("outputs")
        .join("chat-private")
        .join(format!("{safe}.jsonl"))
}

/// Appends `{tool, created}` under the file lock. A blank session records nothing.
pub fn record(repo_root: &Path, session: &str, tool: &str, created: f64) -> io::Result<()> {
    if session.is_empty() {
        return Ok(());
    }
    let path = ledger_path(repo_root, session);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _lock = FileLock::exclusive(&lock_path_for(&path))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let line = json!({"tool": tool, "created": created}).to_string() + "\n";
    f.write_all(line.as_bytes())
}

/// The session's recorded tool names; a missing ledger is empty, any other
/// read failure is an error (callers fail closed).
pub fn entries(repo_root: &Path, session: &str) -> io::Result<Vec<String>> {
    let text = match std::fs::read_to_string(ledger_path(repo_root, session)) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str::<Value>(l)
                .ok()
                .and_then(|v| v.get("tool").and_then(Value::as_str).map(str::to_string))
                // An unreadable line still counts: it was written for a private call.
                .unwrap_or_default()
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{account, env_with, Fake};
    use super::super::{run_in, Arg, Connector, Env, Handler, Mode, ToolResult};
    use super::*;
    use serde_json::Map;

    static NO_ARGS: [Arg; 0] = [];

    /// Reports how many ledger entries existed when the handler ran.
    fn ledger_seen(e: &Env, _c: Option<&dyn Connector>, _a: &Map<String, Value>) -> ToolResult {
        let seen = entries(&e.repo_root, &e.session).map_or(-1, |v| v.len() as i64);
        Ok(json!({"seen": seen}))
    }

    fn tool(name: &'static str, kind: Option<&'static str>, handler: Handler) -> Tool {
        Tool {
            name,
            description: "d",
            args: &NO_ARGS,
            mode: Mode::Read,
            connector_type: kind,
            handler,
            select: None,
        }
    }

    fn setup(dir: &Path) -> (Env, Vec<Tool>) {
        let accounts = vec![account("m1", "mailbox"), account("g1", "fake")];
        let (env, _) = env_with(dir, accounts, &Fake::new(account("x", "mailbox")));
        let tools = vec![
            tool("fake.mail", Some("mailbox"), ledger_seen),
            tool("fake.other", Some("fake"), ledger_seen),
        ];
        (env, tools)
    }

    #[test]
    fn a_private_tool_is_recorded_before_it_runs() {
        let d = tempfile::tempdir().unwrap();
        let (env, tools) = setup(d.path());
        let r = run_in(&tools, &env, "fake.mail", "{}", "s1");
        assert_eq!(r["result"]["seen"], 1, "{r}");
        assert_eq!(entries(&env.repo_root, "s1").unwrap(), vec!["fake.mail"]);
    }

    #[test]
    fn other_tools_and_blank_sessions_record_nothing() {
        let d = tempfile::tempdir().unwrap();
        let (env, tools) = setup(d.path());
        let r = run_in(&tools, &env, "fake.other", "{}", "s1");
        assert_eq!(r["result"]["seen"], 0, "{r}");
        assert!(entries(&env.repo_root, "s1").unwrap().is_empty());
        run_in(&tools, &env, "fake.mail", "{}", "");
        assert!(!env
            .repo_root
            .join("outputs/chat-private")
            .join(".jsonl")
            .exists());
        assert!(entries(&env.repo_root, "s2").unwrap().is_empty());
    }

    #[test]
    fn a_ledger_that_cannot_be_written_refuses_the_private_tool() {
        let d = tempfile::tempdir().unwrap();
        let (env, tools) = setup(d.path());
        // A file where the ledger directory should be.
        std::fs::create_dir_all(env.repo_root.join("outputs")).unwrap();
        std::fs::write(env.repo_root.join("outputs/chat-private"), "x").unwrap();
        let r = run_in(&tools, &env, "fake.mail", "{}", "s1");
        assert!(r["error"].is_string(), "{r}");
        assert!(r.get("result").is_none(), "{r}");
        // A read error other than NotFound is reported, not treated as empty.
        assert!(entries(&env.repo_root, "s1").is_err());
    }

    #[test]
    fn concurrent_appends_keep_every_line() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    for j in 0..25 {
                        record(&root, "s1", &format!("t{i}.{j}"), 1.0).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let all = entries(&root, "s1").unwrap();
        assert_eq!(all.len(), 200);
        assert!(all.iter().all(|t| t.starts_with('t')), "{all:?}");
    }

    #[test]
    fn session_ids_cannot_escape_the_ledger_dir() {
        let p = ledger_path(Path::new("/r"), "../../etc/x");
        assert_eq!(p, Path::new("/r/outputs/chat-private/______etc_x.jsonl"));
    }

    #[test]
    fn the_predicate_matches_the_registry_mail_and_calendar_tools() {
        let private: Vec<&str> = super::super::tools()
            .iter()
            .filter(|t| is_private(t))
            .map(|t| t.name)
            .collect();
        for name in [
            "mail.search",
            "mail.read",
            "mail.send",
            "calendar.list_events",
            "calendar.free_busy",
            "calendar.create_event",
        ] {
            assert!(private.contains(&name), "{name} missing from {private:?}");
            assert!(is_private_name(name));
        }
        assert!(!is_private_name("gitlab.read_issue"));
        assert!(!is_private_name("no.such_tool"));
    }
}
