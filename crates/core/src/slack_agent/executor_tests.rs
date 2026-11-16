use super::*;
use crate::chat::{ChatCommand, ChatJobs};
use crate::messages::MessageStore;
use serde_json::Value;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
enum Call {
    Post {
        channel: String,
        thread: Option<String>,
        text: String,
    },
    Update {
        channel: String,
        ts: String,
        text: String,
    },
    React {
        ts: String,
        emoji: String,
        on: bool,
    },
    OpenDm(String),
}

#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<Call>>,
    next_ts: AtomicUsize,
    /// Popped by each `update`; empty means success.
    update_errors: Mutex<VecDeque<WebError>>,
}

impl Recorder {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
    fn push(&self, c: Call) {
        self.calls.lock().unwrap().push(c);
    }
    fn posts_to(&self, channel: &str) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Post {
                    channel: ch, text, ..
                } if ch == channel => Some(text),
                _ => None,
            })
            .collect()
    }
    fn updates(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Update { text, .. } => Some(text),
                _ => None,
            })
            .collect()
    }
}

impl Poster for Recorder {
    fn post(&self, channel: &str, thread_ts: Option<&str>, text: &str) -> Result<String, WebError> {
        self.push(Call::Post {
            channel: channel.into(),
            thread: thread_ts.map(str::to_string),
            text: text.into(),
        });
        Ok(format!(
            "200.{}",
            self.next_ts.fetch_add(1, Ordering::SeqCst)
        ))
    }
    fn post_blocks(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
        _blocks: &Value,
    ) -> Result<String, WebError> {
        self.post(channel, thread_ts, text)
    }
    fn update(&self, channel: &str, ts: &str, text: &str) -> Result<(), WebError> {
        self.push(Call::Update {
            channel: channel.into(),
            ts: ts.into(),
            text: text.into(),
        });
        match self.update_errors.lock().unwrap().pop_front() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
    fn react(&self, _channel: &str, ts: &str, emoji: &str, on: bool) -> Result<(), WebError> {
        self.push(Call::React {
            ts: ts.into(),
            emoji: emoji.into(),
            on,
        });
        Ok(())
    }
    fn open_dm(&self, user: &str) -> Result<String, WebError> {
        self.push(Call::OpenDm(user.into()));
        Ok("D-DM".into())
    }
}

/// A fake `claude` that prints `stdout` (written to a file, so any JSON is
/// safe) after an optional `before` shell snippet, counting each reply run.
/// The background title request is refused so it never counts as a run.
fn fake_bin(dir: &Path, before: &str, stdout: &str, exit: i32) -> std::path::PathBuf {
    fake_cli_bin(dir, "claude", before, stdout, exit)
}

/// [`fake_bin`] for any CLI program name.
fn fake_cli_bin(
    dir: &Path,
    program: &str,
    before: &str,
    stdout: &str,
    exit: i32,
) -> std::path::PathBuf {
    let bin = dir.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let out = dir.join("claude-stdout");
    std::fs::write(&out, stdout).unwrap();
    let runs = dir.join("runs");
    let body = format!(
        "case \"$*\" in *\"Write a short title\"*) exit 1;; esac\n\
         echo run >> '{}'\n{before}\ncat '{}'\nexit {exit}",
        runs.display(),
        out.display()
    );
    crate::agents::testutil::script(&bin, program, &body);
    crate::agents::testutil::script(&bin, "timeout", "shift\nexec \"$@\"");
    bin
}

fn runtime(dir: &Path, bin: &Path) -> ChatRuntime {
    let mut command = ChatCommand::new("loopx chat-tool");
    command.wrapper = vec!["sh".into(), "-c".into()];
    ChatRuntime {
        repo_root: dir.to_path_buf(),
        store: MessageStore::for_root(dir),
        command,
        jobs: ChatJobs::new(),
        tools_description: Arc::new(|| "tools".to_string()),
        pending_lookup: Arc::new(|_, _| None),
        env: vec![("PATH".into(), format!("{}:/usr/bin:/bin", bin.display()))],
    }
}

type Sleeps = Arc<Mutex<Vec<Duration>>>;

fn executor(dir: &Path, before: &str, stdout: &str, exit: i32) -> (Executor<Recorder>, Sleeps) {
    let bin = fake_bin(dir, before, stdout, exit);
    let sleeps: Sleeps = Arc::default();
    let s = sleeps.clone();
    let mut ex = Executor::new(
        Recorder::default(),
        runtime(dir, &bin),
        ThreadMap::new(dir.join("slack/threads.json")),
    )
    .with_sleep(move |d| s.lock().unwrap().push(d));
    ex.min_edit_interval = Duration::from_millis(500);
    (ex, sleeps)
}

fn req(text: &str, thread_ts: &str, ts: &str) -> Request {
    Request {
        user: "U1".into(),
        channel: "C1".into(),
        thread_ts: thread_ts.into(),
        ts: ts.into(),
        text: text.into(),
        is_dm: false,
        event_id: format!("Ev{ts}"),
    }
}

fn result_line(text: &str) -> String {
    serde_json::json!({"type": "result", "result": text, "is_error": false}).to_string() + "\n"
}

fn tool_line(id: &str, tool: &str) -> String {
    serde_json::json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": id,
         "input": {"command": format!("loopx chat-tool call {tool} {{}}")}}]}})
    .to_string()
        + "\n"
}

fn runs(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("runs"))
        .unwrap_or_default()
        .lines()
        .count()
}

fn user_texts(ex: &Executor<Recorder>, session: &str) -> Vec<String> {
    ex.runtime()
        .store
        .chat_session_messages(session)
        .iter()
        .filter(|m| m["from"] == "user")
        .map(|m| m["text"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_first_message_acks_runs_and_replaces_the_progress_message() {
    let d = tempfile::tempdir().unwrap();
    let (ex, _) = executor(d.path(), "", &result_line("Hello from LoopX"), 0);
    ex.run(&req("check the build", "1.0", "1.0"));

    let progress = "200.0".to_string();
    assert_eq!(
        ex.poster().calls(),
        vec![
            Call::React {
                ts: "1.0".into(),
                emoji: "eyes".into(),
                on: true
            },
            Call::Post {
                channel: "C1".into(),
                thread: Some("1.0".into()),
                text: ON_IT.into()
            },
            Call::Update {
                channel: "C1".into(),
                ts: progress,
                text: "Hello from LoopX".into()
            },
            Call::React {
                ts: "1.0".into(),
                emoji: "eyes".into(),
                on: false
            },
            Call::React {
                ts: "1.0".into(),
                emoji: "white_check_mark".into(),
                on: true
            },
        ]
    );
    let sid = ex.threads().session_for("C1", "1.0").expect("thread bound");
    let msgs = ex.runtime().store.chat_session_messages(&sid);
    assert_eq!(msgs.len(), 2, "{msgs:?}");
    // Only the stripped text is stored: no Slack user or channel id.
    assert_eq!(msgs[0]["from"], "user");
    assert_eq!(msgs[0]["text"], "check the build");
    assert_eq!(msgs[1]["from"], "loop");
    assert_eq!(msgs[1]["text"], "Hello from LoopX");
    assert_eq!(runs(d.path()), 1);
}

#[test]
fn a_follow_up_in_the_same_thread_reuses_its_session() {
    let d = tempfile::tempdir().unwrap();
    let (ex, _) = executor(d.path(), "", &result_line("ok"), 0);
    ex.run(&req("first", "1.0", "1.0"));
    let sid = ex.threads().session_for("C1", "1.0").unwrap();
    ex.run(&req("second", "1.0", "1.5"));
    assert_eq!(
        ex.threads().session_for("C1", "1.0").as_deref(),
        Some(sid.as_str())
    );
    assert_eq!(user_texts(&ex, &sid), vec!["first", "second"]);
    // A different thread gets its own session.
    ex.run(&req("other", "2.0", "2.0"));
    let other = ex.threads().session_for("C1", "2.0").unwrap();
    assert_ne!(other, sid);
    assert_eq!(user_texts(&ex, &other), vec!["other"]);
}

#[test]
fn a_failing_cli_posts_a_short_error_without_paths_and_reacts_x() {
    let d = tempfile::tempdir().unwrap();
    let (ex, _) = executor(
        d.path(),
        "echo 'cannot read /Users/someone/.loopx/secret.json' >&2",
        "",
        3,
    );
    ex.run(&req("do it", "1.0", "1.0"));
    let updates = ex.poster().updates();
    assert_eq!(updates.len(), 1, "{updates:?}");
    let text = &updates[0];
    assert!(text.contains("exited with code 3"), "{text}");
    assert!(!text.contains('/'), "{text}");
    assert!(!text.contains("secret"), "{text}");
    assert!(text.len() < 300, "{text}");
    let calls = ex.poster().calls();
    assert!(calls.contains(&Call::React {
        ts: "1.0".into(),
        emoji: "x".into(),
        on: true
    }));
    assert!(calls.contains(&Call::React {
        ts: "1.0".into(),
        emoji: "eyes".into(),
        on: false
    }));
    assert!(!calls
        .iter()
        .any(|c| matches!(c, Call::React { emoji, .. } if emoji == "white_check_mark")));
}

#[test]
fn a_long_reply_is_split_first_part_replacing_progress() {
    let d = tempfile::tempdir().unwrap();
    let long: String = (0..900).map(|i| format!("line {i:04}\n")).collect();
    assert!(long.len() >= 9000);
    let (ex, _) = executor(d.path(), "", &result_line(&long), 0);
    ex.run(&req("long please", "1.0", "1.0"));
    let updates = ex.poster().updates();
    assert_eq!(updates.len(), 1);
    let rest: Vec<String> = ex
        .poster()
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Post {
                thread: Some(t),
                text,
                ..
            } if t == "1.0" && text != ON_IT => Some(text),
            _ => None,
        })
        .collect();
    assert!(!rest.is_empty());
    let mut all = vec![updates[0].clone()];
    all.extend(rest);
    assert!(all.iter().all(|p| p.len() <= SLACK_TEXT_LIMIT));
    assert_eq!(all.concat().replace('\n', ""), long.replace('\n', ""));
}

#[test]
fn a_second_request_in_an_active_thread_is_refused_without_a_run() {
    let d = tempfile::tempdir().unwrap();
    let (ex, _) = executor(d.path(), "sleep 1", &result_line("done"), 0);
    let ex = Arc::new(ex);
    let first = {
        let ex = ex.clone();
        std::thread::spawn(move || ex.run(&req("first", "1.0", "1.0")))
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ex.poster().posts_to("C1").iter().any(|t| t == ON_IT) {
        assert!(Instant::now() < deadline, "first run never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    ex.run(&req("second", "1.0", "1.1"));
    assert!(ex
        .poster()
        .posts_to("C1")
        .contains(&STILL_WORKING.to_string()));
    first.join().unwrap();
    assert_eq!(runs(d.path()), 1);
    let sid = ex.threads().session_for("C1", "1.0").unwrap();
    assert_eq!(user_texts(&ex, &sid), vec!["first"]);
    // Once finished, the thread takes new requests again.
    ex.run(&req("third", "1.0", "1.2"));
    assert_eq!(runs(d.path()), 2);
}

#[test]
fn progress_edits_are_throttled() {
    let d = tempfile::tempdir().unwrap();
    let burst: String = (0..20)
        .map(|i| tool_line(&format!("t{i}"), "gitlab.read_issue"))
        .collect();
    let (ex, _) = executor(d.path(), "", &(burst + &result_line("final")), 0);
    ex.run(&req("go", "1.0", "1.0"));
    let updates = ex.poster().updates();
    assert_eq!(updates.last().map(String::as_str), Some("final"));
    assert!(updates.len() - 1 <= 2, "{updates:?}");
}

#[test]
fn progress_shows_tool_activity_and_skips_rate_limited_edits() {
    let d = tempfile::tempdir().unwrap();
    let out = d.path().join("slow");
    std::fs::write(
        &out,
        format!(
            "{}{}",
            tool_line("t1", "gitlab.read_issue"),
            tool_line("t2", "github.read_pr")
        ),
    )
    .unwrap();
    let lines = format!(
        "head -n1 '{0}'; sleep 0.4; tail -n1 '{0}'; sleep 0.4",
        out.display()
    );
    let (mut ex, sleeps) = executor(d.path(), &lines, &result_line("final"), 0);
    ex.min_edit_interval = Duration::from_millis(200);
    // The first progress edit is rate limited: it is dropped, never retried.
    ex.poster()
        .update_errors
        .lock()
        .unwrap()
        .push_back(WebError::RateLimited { retry_after: 9 });
    ex.run(&req("go", "1.0", "1.0"));
    let updates = ex.poster().updates();
    assert!(updates.len() >= 3, "{updates:?}");
    assert!(
        updates[..updates.len() - 1]
            .iter()
            .any(|u| u.contains("github.read_pr")),
        "{updates:?}"
    );
    assert_eq!(updates.last().map(String::as_str), Some("final"));
    assert!(sleeps.lock().unwrap().is_empty());
}

#[test]
fn the_final_reply_retries_rate_limits_with_clamped_waits() {
    let d = tempfile::tempdir().unwrap();
    let (ex, sleeps) = executor(d.path(), "", &result_line("final"), 0);
    ex.poster().update_errors.lock().unwrap().extend([
        WebError::RateLimited { retry_after: 0 },
        WebError::RateLimited { retry_after: 120 },
    ]);
    ex.run(&req("go", "1.0", "1.0"));
    assert_eq!(ex.poster().updates(), vec!["final"; 3]);
    assert_eq!(
        *sleeps.lock().unwrap(),
        vec![Duration::from_secs(1), Duration::from_secs(60)]
    );
    assert!(ex.poster().calls().contains(&Call::React {
        ts: "1.0".into(),
        emoji: "white_check_mark".into(),
        on: true
    }));
}

#[test]
fn the_final_reply_gives_up_after_three_retries() {
    let d = tempfile::tempdir().unwrap();
    let (ex, sleeps) = executor(d.path(), "", &result_line("final"), 0);
    ex.poster()
        .update_errors
        .lock()
        .unwrap()
        .extend(std::iter::repeat_n(
            WebError::RateLimited { retry_after: 5 },
            50,
        ));
    ex.run(&req("go", "1.0", "1.0"));
    assert_eq!(ex.poster().updates().len(), 4);
    assert_eq!(sleeps.lock().unwrap().len(), 3);
    assert!(ex.poster().calls().contains(&Call::React {
        ts: "1.0".into(),
        emoji: "x".into(),
        on: true
    }));
}

#[test]
fn a_reply_built_from_mail_goes_to_a_dm() {
    let d = tempfile::tempdir().unwrap();
    let stdout = tool_line("t1", "mail.search") + &result_line("Your inbox: SECRET-SUBJECT");
    let (ex, _) = executor(d.path(), "", &stdout, 0);
    ex.run(&req("what's in my mail", "1.0", "1.0"));
    let calls = ex.poster().calls();
    assert!(calls.contains(&Call::OpenDm("U1".into())), "{calls:?}");
    assert_eq!(
        ex.poster().posts_to("D-DM"),
        vec!["Your inbox: SECRET-SUBJECT"]
    );
    assert!(calls.contains(&Call::Post {
        channel: "D-DM".into(),
        thread: None,
        text: "Your inbox: SECRET-SUBJECT".into()
    }));
    assert_eq!(
        ex.poster().updates().last().map(String::as_str),
        Some(DM_NOTICE)
    );
    for c in &calls {
        let text = match c {
            Call::Post { channel, text, .. } if channel == "C1" => text,
            Call::Update { text, .. } => text,
            _ => continue,
        };
        assert!(!text.contains("SECRET"), "leaked into the channel: {c:?}");
        assert!(
            !text.contains("mail.search"),
            "private tool named in the channel: {c:?}"
        );
    }
}

#[test]
fn a_private_reply_to_a_dm_stays_in_that_dm() {
    let d = tempfile::tempdir().unwrap();
    let stdout = tool_line("t1", "calendar.list_events") + &result_line("You have 3 meetings");
    let (ex, _) = executor(d.path(), "", &stdout, 0);
    let mut r = req("my day?", "1.0", "1.0");
    r.channel = "D-OWN".into();
    r.is_dm = true;
    ex.run(&r);
    assert!(!ex
        .poster()
        .calls()
        .iter()
        .any(|c| matches!(c, Call::OpenDm(_))));
    assert_eq!(
        ex.poster().updates().last().map(String::as_str),
        Some("You have 3 meetings")
    );
}

#[test]
fn reply_in_dm_matches_private_tools_only() {
    let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(reply_in_dm(&names(&["gitlab.read_issue", "mail.read"])));
    assert!(reply_in_dm(&names(&["calendar.free_busy"])));
    assert!(!reply_in_dm(&names(&[
        "gitlab.read_issue",
        "web.search",
        "docs.save"
    ])));
    assert!(!reply_in_dm(&[]));
}

#[test]
fn every_mailbox_and_calendar_tool_counts_as_private() {
    use crate::chat_tools::private_ledger::is_private;
    let (private, public): (Vec<&crate::chat_tools::Tool>, Vec<_>) = crate::chat_tools::tools()
        .iter()
        .partition(|t| is_private(t));
    assert!(private.len() >= 6, "{}", private.len());
    // The prefix list (which also covers names the stream reports for tools
    // this build does not know) agrees with the shared predicate both ways.
    for t in &private {
        assert!(
            has_private_prefix(t.name),
            "{} has no private prefix",
            t.name
        );
    }
    for t in &public {
        assert!(
            !has_private_prefix(t.name),
            "{} matches a private prefix",
            t.name
        );
    }
    let names: Vec<String> = public.iter().map(|t| t.name.to_string()).collect();
    assert!(!reply_in_dm(&names), "{names:?}");
}

#[test]
fn short_errors_drop_detail_and_paths() {
    assert_eq!(
        short_error(
            "Claude Code exited with code 3: boom at /tmp/x. Switch CLI in Settings -> AI CLI."
        ),
        "Sorry, that didn't work: Claude Code exited with code 3."
    );
    for (raw, want) in [
        (
            "Codex CLI timed out after 90 s. Switch CLI in Settings -> AI CLI.",
            "Codex CLI timed out after 90 s",
        ),
        (
            "Gemini CLI was stopped by a signal. Switch CLI in Settings -> AI CLI.",
            "Gemini CLI was stopped by a signal",
        ),
        (
            "Kimi CLI didn't return a reply. Switch CLI in Settings -> AI CLI.",
            "Kimi CLI didn't return a reply",
        ),
        (
            "could not start /opt/bin/claude (claude CLI): No such file. Switch CLI in Settings -> AI CLI.",
            "could not start",
        ),
        (
            "Could not prepare the Cursor CLI workspace: outputs/x resolves outside /r/outputs. Switch",
            "Could not prepare the Cursor CLI workspace",
        ),
    ] {
        assert_eq!(short_error(raw), format!("Sorry, that didn't work: {want}."), "{raw}");
    }
    // Anything unrecognized (model text above all) is never echoed.
    for raw in [
        "Your inbox: SECRET",
        "Could not run /opt/bin/claude now\nmore",
        "SECRET exited with code 1",
        "  ",
    ] {
        assert_eq!(short_error(raw), "Sorry, that didn't work.", "{raw}");
    }
}

/// Every text that reached the request's channel (posts there, and every edit:
/// edits only ever target the thread's progress message).
fn channel_texts(ex: &Executor<Recorder>) -> Vec<String> {
    let mut out = ex.poster().posts_to("C1");
    out.extend(ex.poster().updates());
    out
}

/// Shell that records a private call in the ledger the way `loopx chat-tool`
/// does for the session the chat engine passes in `LOOP_CHAT_SESSION`.
fn ledger_write(dir: &Path, tool: &str) -> String {
    let ledger = dir.join("outputs/chat-private");
    format!(
        "mkdir -p '{0}' && printf '{{\"tool\":\"{tool}\",\"created\":%s}}\\n' \"$(date +%s)\" >> '{0}'/\"$LOOP_CHAT_SESSION\".jsonl",
        ledger.display()
    )
}

fn assert_private_delivery(ex: &Executor<Recorder>, reply: &str, secret: &str) {
    assert_eq!(ex.poster().posts_to("D-DM"), vec![reply.to_string()]);
    assert_eq!(
        ex.poster().updates().last().map(String::as_str),
        Some(DM_NOTICE)
    );
    for t in channel_texts(ex) {
        assert!(!t.contains(secret), "leaked into the channel: {t}");
    }
}

#[test]
fn a_single_reply_cli_that_read_mail_answers_by_dm() {
    let d = tempfile::tempdir().unwrap();
    let bin = fake_cli_bin(
        d.path(),
        "codex",
        &ledger_write(d.path(), "mail.read"),
        "Codex says SECRET-CODEX\n",
        0,
    );
    let mut rt = runtime(d.path(), &bin);
    rt.command.cli = "codex".into();
    let ex = Executor::new(
        Recorder::default(),
        rt,
        ThreadMap::new(d.path().join("slack/threads.json")),
    );
    ex.run(&req("mail?", "1.0", "1.0"));
    assert_private_delivery(&ex, "Codex says SECRET-CODEX", "SECRET");
}

#[test]
fn a_tool_command_the_stream_parser_misses_still_answers_by_dm() {
    let d = tempfile::tempdir().unwrap();
    let odd = serde_json::json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": "t1",
         "input": {"command": "/opt/loopx-dev/bin/loopx-dev chat-tool call mail.read {}"}}]}})
    .to_string()
        + "\n";
    let (ex, _) = executor(
        d.path(),
        &ledger_write(d.path(), "mail.read"),
        &(odd + &result_line("Mail: SECRET-ODD")),
        0,
    );
    ex.run(&req("mail?", "1.0", "1.0"));
    assert_private_delivery(&ex, "Mail: SECRET-ODD", "SECRET");
}

#[test]
fn a_thread_that_touched_private_data_stays_private() {
    let d = tempfile::tempdir().unwrap();
    let (ex, _) = executor(
        d.path(),
        &ledger_write(d.path(), "mail.search"),
        // Only the ledger knows: no tool line in the stream (as on SingleReply CLIs).
        &result_line("You have 2 mails"),
        0,
    );
    ex.run(&req("mail?", "1.0", "1.0"));
    assert_eq!(ex.poster().posts_to("D-DM"), vec!["You have 2 mails"]);
    // Turn 2 calls no tool and answers from history.
    fake_bin(d.path(), "", &result_line("The second is SECRET-2"), 0);
    ex.run(&req("and the second one?", "1.0", "1.1"));
    assert_eq!(
        ex.poster().posts_to("D-DM"),
        vec!["You have 2 mails", "The second is SECRET-2"]
    );
    assert_eq!(
        ex.poster().updates().last().map(String::as_str),
        Some(DM_NOTICE)
    );
    for t in channel_texts(&ex) {
        assert!(!t.contains("SECRET"), "leaked into the channel: {t}");
    }
}

#[test]
fn an_unreadable_ledger_fails_closed_to_a_dm() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("outputs")).unwrap();
    std::fs::write(d.path().join("outputs/chat-private"), "not a dir").unwrap();
    let (ex, _) = executor(d.path(), "", &result_line("Plain SECRET-X"), 0);
    ex.run(&req("hi", "1.0", "1.0"));
    assert_private_delivery(&ex, "Plain SECRET-X", "SECRET");
}

#[test]
fn a_private_turn_that_fails_posts_only_the_generic_error() {
    let d = tempfile::tempdir().unwrap();
    let failing =
        serde_json::json!({"type": "result", "result": "Your inbox: SECRET", "is_error": true})
            .to_string()
            + "\n";
    let (ex, _) = executor(
        d.path(),
        &ledger_write(d.path(), "mail.search"),
        &(tool_line("t1", "mail.search") + &failing),
        0,
    );
    ex.run(&req("mail?", "1.0", "1.0"));
    let texts = channel_texts(&ex);
    assert!(texts.iter().all(|t| !t.contains("SECRET")), "{texts:?}");
    assert_eq!(
        ex.poster().updates().last().map(String::as_str),
        Some(GENERIC_ERROR)
    );
    assert!(ex.poster().calls().contains(&Call::React {
        ts: "1.0".into(),
        emoji: "x".into(),
        on: true
    }));
}

#[test]
fn slack_turns_are_never_handed_to_the_loop() {
    let d = tempfile::tempdir().unwrap();
    let (ex, _) = executor(d.path(), "", &result_line("ok"), 0);
    ex.run(&req("please merge everything", "1.0", "1.0"));
    assert!(ex.runtime().store.pop_unseen_user_messages().is_empty());
    let sid = ex.threads().session_for("C1", "1.0").unwrap();
    assert_eq!(user_texts(&ex, &sid), vec!["please merge everything"]);
}

fn assert_failed_and_released(ex: &Executor<Recorder>, started: Instant, limit: Duration) {
    assert!(started.elapsed() < limit, "took {:?}", started.elapsed());
    assert!(ex.poster().calls().contains(&Call::React {
        ts: "1.0".into(),
        emoji: "x".into(),
        on: true
    }));
    // The thread is free again: a new request starts a run, not "still working".
    ex.run(&req("again", "1.0", "1.9"));
    assert!(!ex
        .poster()
        .posts_to("C1")
        .contains(&STILL_WORKING.to_string()));
}

#[test]
fn a_job_that_never_finishes_hits_the_deadline_and_is_cancelled() {
    let d = tempfile::tempdir().unwrap();
    let (ex, _) = executor(d.path(), "", &result_line("ok"), 0);
    let keys: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = keys.clone();
    let mut ex = ex.with_runner(move |_rt, key, _prompt, _sid| {
        seen.lock().unwrap().push(key.to_string());
        std::thread::sleep(Duration::from_secs(30));
    });
    ex.turn_deadline = Duration::from_millis(300);
    ex.cancel_grace = Duration::from_millis(100);
    let started = Instant::now();
    ex.run(&req("hi", "1.0", "1.0"));
    let key = keys.lock().unwrap()[0].clone();
    assert!(ex.runtime().jobs.is_cancelled(&key));
    assert_eq!(
        ex.poster().updates().last().map(String::as_str),
        Some(short_error(TIMED_OUT).as_str())
    );
    assert_failed_and_released(&ex, started, Duration::from_secs(3));
}

#[test]
fn a_worker_that_exits_without_finishing_ends_the_run() {
    let d = tempfile::tempdir().unwrap();
    let (ex, _) = executor(d.path(), "", &result_line("ok"), 0);
    let ex = ex.with_runner(|_rt, _key, _prompt, _sid| {});
    let started = Instant::now();
    ex.run(&req("hi", "1.0", "1.0"));
    assert_eq!(
        ex.poster().updates().last().map(String::as_str),
        Some(short_error(UNEXPECTED_STOP).as_str())
    );
    assert_failed_and_released(&ex, started, Duration::from_secs(2));
}

#[test]
fn a_private_turn_shows_even_engine_errors_only_generically() {
    let d = tempfile::tempdir().unwrap();
    let (ex, _) = executor(
        d.path(),
        &ledger_write(d.path(), "calendar.list_events"),
        "",
        3,
    );
    ex.run(&req("my day?", "1.0", "1.0"));
    assert_eq!(
        ex.poster().updates().last().map(String::as_str),
        Some(GENERIC_ERROR)
    );
    // The same failure on a non-private turn names the engine's reason.
    let d2 = tempfile::tempdir().unwrap();
    let (ex2, _) = executor(d2.path(), "", "", 3);
    ex2.run(&req("build?", "1.0", "1.0"));
    let last = ex2.poster().updates().last().cloned().unwrap();
    assert!(last.contains("exited with code 3"), "{last}");
}

// ---- mrkdwn escaping: agent text never pings or disguises links ------------------------

const HOSTILE: &str =
    "<!channel> <!here> <@U123> <https://evil.example|https://gitlab.example/x> & \"x\"";
const RAW: &[&str] = &[
    "<!channel>",
    "<!here>",
    "<@U123>",
    "<https://evil.example|",
    "& \"x\"",
];
const ESCAPED: &[&str] = &[
    "&lt;!channel&gt;",
    "&lt;!here&gt;",
    "&lt;@U123&gt;",
    "&lt;https://evil.example|https://gitlab.example/x&gt;",
    "&amp; \"x\"",
];

/// Every text the executor sent (posts and edits), with the fixed strings it
/// sends itself left out.
fn sent_texts(ex: &Executor<Recorder>) -> Vec<String> {
    ex.poster()
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Post { text, .. } | Call::Update { text, .. } => Some(text),
            _ => None,
        })
        .filter(|t| ![ON_IT, DM_NOTICE].contains(&t.as_str()))
        .collect()
}

fn assert_escaped_everywhere(texts: &[String]) {
    assert!(!texts.is_empty());
    for t in texts {
        for raw in RAW {
            assert!(!t.contains(raw), "raw {raw:?} sent: {t}");
        }
        for esc in ESCAPED {
            assert!(t.contains(esc), "{esc:?} missing: {t}");
        }
    }
}

#[test]
fn a_reply_is_escaped_before_it_reaches_the_channel() {
    let d = tempfile::tempdir().unwrap();
    let (ex, _) = executor(d.path(), "", &result_line(HOSTILE), 0);
    ex.run(&req("summarize the issue", "1.0", "1.0"));
    assert_escaped_everywhere(&sent_texts(&ex));
    // The chat history keeps the model's own text.
    let sid = ex.threads().session_for("C1", "1.0").unwrap();
    let msgs = ex.runtime().store.chat_session_messages(&sid);
    assert_eq!(msgs[1]["text"], HOSTILE);
}

#[test]
fn a_private_reply_is_escaped_in_the_dm_too() {
    let d = tempfile::tempdir().unwrap();
    let stdout = tool_line("t1", "mail.search") + &result_line(HOSTILE);
    let (ex, _) = executor(d.path(), "", &stdout, 0);
    ex.run(&req("my mail", "1.0", "1.0"));
    let dm = ex.poster().posts_to("D-DM");
    assert_eq!(dm.len(), 1, "{dm:?}");
    assert_escaped_everywhere(&dm);
}

#[test]
fn an_error_line_built_from_engine_text_is_escaped() {
    let d = tempfile::tempdir().unwrap();
    let (ex, _) = executor(d.path(), "", "", 0);
    let ex = ex.with_runner(|rt, key, _prompt, _sid| {
        rt.jobs.finish(
            key,
            Some("Could not start <!channel> & <@U123> now".into()),
            None,
        );
    });
    ex.run(&req("hi", "1.0", "1.0"));
    let last = ex.poster().updates().last().cloned().unwrap();
    assert!(
        !last.contains("<!channel>") && !last.contains("<@U123>"),
        "{last}"
    );
    assert!(
        last.contains("&lt;!channel&gt; &amp; &lt;@U123&gt;"),
        "{last}"
    );
}

#[test]
fn the_progress_message_escapes_tool_names() {
    let text = progress_text(&["gitlab.<!here>&x".to_string()]);
    assert!(!text.contains("<!here>"), "{text}");
    assert!(text.contains("gitlab.&lt;!here&gt;&amp;x"), "{text}");
}

/// Escaping comes first (Slack's limit counts the escaped text), and the
/// split never cuts an entity in two.
#[test]
fn a_long_escaped_reply_fits_the_limit_and_keeps_entities_whole() {
    let d = tempfile::tempdir().unwrap();
    // One line: 3498 bytes, then a `<` whose `&lt;` straddles the 3500 cut,
    // then 3000 more `<` (12000 escaped bytes).
    let reply = format!("{}<{}", "a".repeat(3498), "<".repeat(3000));
    let (ex, _) = executor(d.path(), "", &result_line(&reply), 0);
    ex.run(&req("long", "1.0", "1.0"));
    let parts = sent_texts(&ex);
    assert!(parts.len() >= 4, "{}", parts.len());
    for p in &parts {
        assert!(p.len() <= SLACK_TEXT_LIMIT, "part of {} bytes", p.len());
        assert!(!p.contains('<'), "raw `<` sent");
        let mut rest = p.as_str();
        while let Some(i) = rest.find('&') {
            rest = &rest[i..];
            assert!(
                rest.starts_with("&lt;") || rest.starts_with("&gt;") || rest.starts_with("&amp;"),
                "a cut entity: {:?}",
                &rest[..rest.len().min(8)]
            );
            rest = &rest[1..];
        }
    }
    assert_eq!(parts.concat(), escape_mrkdwn(&reply));
}

#[test]
fn to_mrkdwn_converts_markdown_to_slack_syntax() {
    use super::to_mrkdwn;
    assert_eq!(to_mrkdwn("**#1361: Title** (x)"), "*#1361: Title* (x)");
    assert_eq!(to_mrkdwn("## Heading"), "*Heading*");
    assert_eq!(
        to_mrkdwn("see [docs](https://a.io/x?a=1)"),
        "see <https://a.io/x?a=1|docs>"
    );
    assert_eq!(
        to_mrkdwn("`**keep**` and **bold**"),
        "`**keep**` and *bold*"
    );
    assert_eq!(
        to_mrkdwn("```\n**raw** <b>\n```"),
        "```\n**raw** &lt;b&gt;\n```"
    );
    assert_eq!(
        to_mrkdwn("a < b & <!channel>"),
        "a &lt; b &amp; &lt;!channel&gt;"
    );
}
