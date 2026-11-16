use super::*;
use crate::approvals::{self, NewApproval};
use crate::chat::{ChatCommand, ChatJobs};
use crate::connectors::testutil::FakeHttp;
use crate::connectors::{HttpError, HttpRequest};
use crate::messages::MessageStore;
use crate::slack_agent::executor::{ON_IT, STILL_WORKING};
use crate::slack_agent::testutil::{fake_socket_server, fake_socket_server_seq, Script};
use serde_json::json;
use std::net::TcpStream;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;
use std::thread::JoinHandle;
use tungstenite::{Message, WebSocket};

const BOT: &str = "xoxb-BOTSECRET";
const APP: &str = "xapp-APPSECRET";
const WAIT: Duration = Duration::from_secs(10);

// ----- fakes -----

fn method(req: &HttpRequest) -> &str {
    req.url.rsplit('/').next().unwrap_or("")
}

/// Slack's Web API: the socket URL is `ws_url`, everything else succeeds
/// unless `override_` answers for that method.
fn slack_http(
    ws_url: &str,
    override_: impl Fn(&str) -> Option<Value> + Send + Sync + 'static,
) -> Arc<FakeHttp> {
    slack_http_req(ws_url, move |req| override_(method(req)))
}

/// Like [`slack_http`], but the override sees the whole request.
fn slack_http_req(
    ws_url: &str,
    override_: impl Fn(&HttpRequest) -> Option<Value> + Send + Sync + 'static,
) -> Arc<FakeHttp> {
    let ws_url = ws_url.to_string();
    let ts = Arc::new(AtomicUsize::new(0));
    FakeHttp::new(move |req: &HttpRequest| -> Result<Value, HttpError> {
        let m = method(req);
        if let Some(v) = override_(req) {
            return Ok(v);
        }
        Ok(match m {
            "apps.connections.open" => json!({"ok": true, "url": ws_url}),
            "auth.test" => json!({"ok": true, "user_id": "UBOT", "team": "T"}),
            "chat.postMessage" => {
                json!({"ok": true, "ts": format!("900.{}", ts.fetch_add(1, Ordering::SeqCst))})
            }
            "conversations.open" => json!({"ok": true, "channel": {"id": "D-DM"}}),
            _ => json!({"ok": true}),
        })
    })
}

fn web(http: &Arc<FakeHttp>) -> SlackWeb {
    SlackWeb::new(BOT.into(), APP.into(), http.clone())
}

fn count(http: &FakeHttp, m: &str) -> usize {
    http.calls
        .lock()
        .unwrap()
        .iter()
        .filter(|r| method(r) == m)
        .count()
}

/// `(channel, thread_ts, text)` of every `chat.postMessage`.
fn posts(http: &FakeHttp) -> Vec<(String, Option<String>, String)> {
    http.calls
        .lock()
        .unwrap()
        .iter()
        .filter(|r| method(r) == "chat.postMessage")
        .filter_map(|r| r.json_body.clone())
        .map(|b| {
            (
                b["channel"].as_str().unwrap_or("").to_string(),
                b["thread_ts"].as_str().map(str::to_string),
                b["text"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

fn posts_saying(http: &FakeHttp, text: &str) -> usize {
    posts(http).iter().filter(|p| p.2 == text).count()
}

fn runtime(dir: &Path) -> ChatRuntime {
    ChatRuntime {
        repo_root: dir.to_path_buf(),
        store: MessageStore::for_root(dir),
        command: ChatCommand::new("loopx chat-tool"),
        jobs: ChatJobs::new(),
        tools_description: Arc::new(|| "tools".to_string()),
        pending_lookup: Arc::new(|_, _| None),
        env: Vec::new(),
    }
}

fn config(dir: &Path) -> Config {
    Config {
        allowlist: vec!["U1".into()],
        rate_per_hour: 10,
        approvals_dir: dir.join("approvals"),
        state_dir: dir.join("slack"),
    }
}

#[derive(Default)]
struct Runs {
    started: AtomicUsize,
    finished: AtomicUsize,
}

impl Runs {
    fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }
    fn finished(&self) -> usize {
        self.finished.load(Ordering::SeqCst)
    }
}

/// A runner that answers at once.
fn quick(runs: &Arc<Runs>) -> impl Fn(&ChatRuntime, &str, &str, &str) + Send + Sync + 'static {
    let runs = runs.clone();
    move |rt, key, _prompt, _sid| {
        runs.started.fetch_add(1, Ordering::SeqCst);
        rt.jobs.finish(key, None, Some("done".into()));
        runs.finished.fetch_add(1, Ordering::SeqCst);
    }
}

fn listener(dir: &Path, http: &Arc<FakeHttp>, runs: &Arc<Runs>) -> Listener<SlackWeb> {
    Listener::new(config(dir), web(http), runtime(dir))
        .with_runner(quick(runs))
        .with_backoff(|_| Duration::from_millis(10))
}

fn start(l: Listener<SlackWeb>) -> (Arc<AtomicBool>, JoinHandle<Exit>) {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    (stop, std::thread::spawn(move || l.run(s)))
}

fn stop_and_join(stop: &AtomicBool, h: JoinHandle<Exit>) -> Exit {
    stop.store(true, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(h.join().unwrap());
    });
    rx.recv_timeout(WAIT).expect("listener did not stop")
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Lets anything that would wrongly happen next show up before asserting it did not.
fn settle() {
    std::thread::sleep(Duration::from_millis(300));
}

fn status_of(dir: &Path) -> Option<Status> {
    status::read_status(&config(dir).status_path())
}

fn state_is(dir: &Path, s: State) -> bool {
    status_of(dir).is_some_and(|st| st.state == s)
}

// ----- socket frames -----

fn hello(ws: &mut WebSocket<TcpStream>) {
    ws.send(Message::text(r#"{"type":"hello"}"#)).unwrap();
}

fn mention_frame(envelope: &str, event_id: &str, user: &str, ts: &str) -> Message {
    Message::text(
        json!({"type": "events_api", "envelope_id": envelope, "payload": {
            "event_id": event_id,
            "event": {"type": "app_mention", "user": user, "text": "<@UBOT> check the build",
                      "channel": "C1", "ts": ts}}})
        .to_string(),
    )
}

fn button_frame(envelope: &str, user: &str, action_id: &str, value: &str) -> Message {
    Message::text(
        json!({"type": "interactive", "envelope_id": envelope, "payload": {
            "type": "block_actions", "user": {"id": user}, "channel": {"id": "D1"},
            "container": {"message_ts": "5.0"}, "message": {"ts": "5.0"},
            "actions": [{"action_id": action_id, "value": value}]}})
        .to_string(),
    )
}

/// The next ack's envelope id (control frames skipped).
fn read_ack(ws: &mut WebSocket<TcpStream>) -> String {
    loop {
        match ws.read().expect("read ack") {
            Message::Text(t) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                return v["envelope_id"].as_str().unwrap().to_string();
            }
            _ => continue,
        }
    }
}

/// Keeps the connection open until the client goes away.
fn hold(ws: &mut WebSocket<TcpStream>) {
    while ws.read().is_ok() {}
}

fn pending_approval(dir: &Path, title: &str) -> String {
    let new = NewApproval {
        kind: "merge_request".into(),
        run_id: "r1".into(),
        project: "web".into(),
        issue_iid: Some(7),
        title: title.into(),
        branch: "loop/issue-7".into(),
        target_branch: "main".into(),
        summary: "Trim the email".into(),
        timeout_seconds: 3600,
        target: None,
    };
    approvals::create(dir, &new, Utc::now()).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

// ----- tests -----

#[test]
fn rate_setting_falls_back_to_ten() {
    assert_eq!(rate_from_setting("5"), 5);
    assert_eq!(rate_from_setting(" 7 "), 7);
    for bad in ["", "0", "-3", "ten", "1.5"] {
        assert_eq!(rate_from_setting(bad), DEFAULT_RATE_PER_HOUR, "{bad:?}");
    }
}

#[test]
fn observed_keeps_the_first_failure_but_never_a_rate_limit() {
    struct Failing(Mutex<Vec<WebError>>);
    impl Poster for Failing {
        fn post(&self, _: &str, _: Option<&str>, _: &str) -> Result<String, WebError> {
            Err(self.0.lock().unwrap().remove(0))
        }
        fn post_blocks(
            &self,
            _: &str,
            _: Option<&str>,
            _: &str,
            _: &Value,
        ) -> Result<String, WebError> {
            unreachable!()
        }
        fn update(&self, _: &str, _: &str, _: &str) -> Result<(), WebError> {
            unreachable!()
        }
        fn react(&self, _: &str, _: &str, _: &str, _: bool) -> Result<(), WebError> {
            unreachable!()
        }
        fn open_dm(&self, _: &str) -> Result<String, WebError> {
            unreachable!()
        }
    }
    let slot = Arc::new(Mutex::new(None));
    let p = Observed::new(
        Failing(Mutex::new(vec![
            WebError::RateLimited { retry_after: 3 },
            WebError::MissingScope("reactions:write".into()),
            WebError::Auth,
        ])),
        slot.clone(),
    );
    let _ = p.post("C", None, "x");
    assert_eq!(*slot.lock().unwrap(), None);
    let _ = p.post("C", None, "x");
    let _ = p.post("C", None, "x");
    assert_eq!(
        slot.lock().unwrap().as_deref(),
        Some("Slack scope missing: reactions:write")
    );
}

#[test]
fn an_empty_allowlist_refuses_before_any_network_call() {
    let d = tempfile::tempdir().unwrap();
    let http = slack_http("ws://127.0.0.1:1", |_| None);
    let mut cfg = config(d.path());
    cfg.allowlist.clear();
    let exit = run_listener(
        cfg,
        web(&http),
        runtime(d.path()),
        Arc::new(AtomicBool::new(false)),
    );
    assert!(
        matches!(exit, Exit::Misconfigured(ref m) if m.contains("allow")),
        "{exit:?}"
    );
    assert!(http.calls.lock().unwrap().is_empty());
}

#[test]
fn an_allowed_mention_is_acked_before_the_agent_finishes_then_runs_once() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (ack_tx, ack_rx) = mpsc::channel();
    let at_ack = runs.clone();
    let (url, server) = fake_socket_server(move |ws| {
        hello(ws);
        ws.send(mention_frame("env1", "Ev1", "U1", "1.0")).unwrap();
        let id = read_ack(ws);
        ack_tx.send((id, at_ack.finished())).unwrap();
        let _ = release_tx.send(());
        hold(ws);
    });
    let http = slack_http(&url, |_| None);
    let released = Arc::new(AtomicBool::new(false));
    let (r, rel) = (runs.clone(), released.clone());
    let release_rx = Mutex::new(release_rx);
    let l = Listener::new(config(d.path()), web(&http), runtime(d.path())).with_runner(
        move |rt, key, _p, _s| {
            r.started.fetch_add(1, Ordering::SeqCst);
            // The agent is "still running" until the server has seen the ack.
            let ok = release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .is_ok();
            rel.store(ok, Ordering::SeqCst);
            rt.jobs.finish(key, None, Some("done".into()));
            r.finished.fetch_add(1, Ordering::SeqCst);
        },
    );
    let (stop, h) = start(l);
    let (acked, finished_at_ack) = ack_rx.recv_timeout(WAIT).expect("no ack");
    assert_eq!(acked, "env1");
    assert_eq!(finished_at_ack, 0, "the ack came after the agent finished");
    wait_until("the run", || runs.finished() == 1);
    assert!(
        released.load(Ordering::SeqCst),
        "the agent never saw the ack"
    );
    wait_until("the reply", || {
        count(&http, "reactions.add") >= 2 // eyes, then the check
    });
    assert_eq!(runs.started(), 1);
    assert_eq!(posts_saying(&http, ON_IT), 1);
    let st = status_of(d.path()).unwrap();
    assert_eq!(st.state, State::Connected);
    assert!(st.last_event.is_some());
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
    assert!(state_is(d.path(), State::Offline));
}

#[test]
fn the_same_event_id_twice_runs_once() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let (url, server) = fake_socket_server(|ws| {
        hello(ws);
        ws.send(mention_frame("env1", "Ev1", "U1", "1.0")).unwrap();
        ws.send(mention_frame("env2", "Ev1", "U1", "1.0")).unwrap();
        ws.send(mention_frame("env3", "Ev2", "U1", "2.0")).unwrap();
        for want in ["env1", "env2", "env3"] {
            assert_eq!(read_ack(ws), want);
        }
        hold(ws);
    });
    let http = slack_http(&url, |_| None);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("two runs", || runs.finished() >= 2);
    settle();
    assert_eq!(runs.started(), 2);
    assert_eq!(posts_saying(&http, STILL_WORKING), 0);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn a_user_off_the_allowlist_gets_one_reply_per_window_and_no_run() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let (url, server) = fake_socket_server(|ws| {
        hello(ws);
        ws.send(mention_frame("e1", "Ev1", "U9", "1.0")).unwrap();
        ws.send(mention_frame("e2", "Ev2", "U9", "2.0")).unwrap();
        // An allowed request after both, as a marker that they were handled.
        ws.send(mention_frame("e3", "Ev3", "U1", "3.0")).unwrap();
        for want in ["e1", "e2", "e3"] {
            assert_eq!(read_ack(ws), want);
        }
        hold(ws);
    });
    let http = slack_http(&url, |_| None);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the allowed run", || runs.finished() == 1);
    wait_until("the reply", || posts_saying(&http, NOT_AUTHORIZED) >= 1);
    settle();
    assert_eq!(posts_saying(&http, NOT_AUTHORIZED), 1);
    let reply = posts(&http)
        .into_iter()
        .find(|p| p.2 == NOT_AUTHORIZED)
        .unwrap();
    assert_eq!((reply.0.as_str(), reply.1.as_deref()), ("C1", Some("1.0")));
    assert_eq!(runs.started(), 1);
    // Logged: who and where, never the text or a token.
    let log = std::fs::read_to_string(crate::runner_util::unified_log_path(d.path())).unwrap();
    assert!(log.contains("U9"), "{log}");
    for secret in ["check the build", BOT, APP] {
        assert!(!log.contains(secret), "{log}");
    }
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn the_unauthorized_reply_comes_again_once_the_window_has_passed() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let (url, server) = fake_socket_server(|ws| {
        hello(ws);
        ws.send(mention_frame("e1", "Ev1", "U9", "1.0")).unwrap();
        ws.send(mention_frame("e2", "Ev2", "U9", "2.0")).unwrap();
        read_ack(ws);
        read_ack(ws);
        hold(ws);
    });
    let http = slack_http(&url, |_| None);
    let mut l = listener(d.path(), &http, &runs);
    l.unauthorized_window = Duration::ZERO;
    let (stop, h) = start(l);
    wait_until("two replies", || posts_saying(&http, NOT_AUTHORIZED) == 2);
    assert_eq!(runs.started(), 0);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn the_eleventh_request_in_an_hour_is_told_to_slow_down() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let (url, server) = fake_socket_server(|ws| {
        hello(ws);
        for i in 0..11 {
            ws.send(mention_frame(
                &format!("e{i}"),
                &format!("Ev{i}"),
                "U1",
                &format!("{i}.0"),
            ))
            .unwrap();
        }
        for _ in 0..11 {
            read_ack(ws);
        }
        hold(ws);
    });
    let http = slack_http(&url, |_| None);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("ten runs", || runs.finished() == 10);
    wait_until("the slow-down reply", || posts_saying(&http, TOO_FAST) == 1);
    settle();
    assert_eq!(runs.started(), 10);
    assert_eq!(posts_saying(&http, TOO_FAST), 1);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn approval_buttons_are_recorded_for_allowed_users_only() {
    let d = tempfile::tempdir().unwrap();
    let apr = d.path().join("approvals");
    let a = pending_approval(&apr, "A");
    let b = pending_approval(&apr, "B");
    let runs = Arc::new(Runs::default());
    let (va, vb) = (format!("{a}:approve"), format!("{b}:approve"));
    let (url, server) = fake_socket_server(move |ws| {
        hello(ws);
        ws.send(button_frame("i1", "U9", APPROVAL_ACTION, &vb))
            .unwrap();
        ws.send(button_frame("i2", "U1", "loopx_catchup", "C1:1.0:run"))
            .unwrap();
        ws.send(button_frame("i3", "U1", APPROVAL_ACTION, &va))
            .unwrap();
        for want in ["i1", "i2", "i3"] {
            assert_eq!(read_ack(ws), want);
        }
        hold(ws);
    });
    let http = slack_http(&url, |_| None);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the approval", || {
        approvals::get(&apr, &a).is_some_and(|v| v["status"] == "approved")
    });
    assert_eq!(approvals::get(&apr, &a).unwrap()["decided_by"], "slack:U1");
    assert_eq!(approvals::get(&apr, &b).unwrap()["status"], "pending");
    wait_until("the outcome reply", || {
        posts(&http)
            .iter()
            .any(|p| p.0 == "D1" && p.1.as_deref() == Some("5.0") && p.2 == "Approved.")
    });
    settle();
    assert_eq!(runs.started(), 0, "a button never starts the agent");
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn a_disconnect_reconnects_and_calls_the_catch_up_hook_once() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let (go1_tx, go1_rx) = mpsc::channel::<()>();
    let (go2_tx, go2_rx) = mpsc::channel::<()>();
    let scripts: Vec<Script> = vec![
        Box::new(move |ws| {
            hello(ws);
            go1_rx.recv().unwrap();
            ws.send(Message::text(
                r#"{"type":"disconnect","reason":"refresh_requested"}"#,
            ))
            .unwrap();
            hold(ws);
        }),
        Box::new(move |ws| {
            go2_rx.recv().unwrap();
            hello(ws);
            hold(ws);
        }),
    ];
    let (url, server) = fake_socket_server_seq(scripts);
    let http = slack_http(&url, |_| None);
    let hooks: Arc<Mutex<Vec<DateTime<Utc>>>> = Arc::default();
    let seen = hooks.clone();
    let l = listener(d.path(), &http, &runs).on_reconnect(move |c| {
        assert_eq!(c.bot_user_id, "UBOT");
        assert_eq!(c.allowlist, ["U1".to_string()]);
        seen.lock().unwrap().push(c.offline_since);
    });
    let (stop, h) = start(l);
    wait_until("connected", || state_is(d.path(), State::Connected));
    go1_tx.send(()).unwrap();
    wait_until("offline", || state_is(d.path(), State::Offline));
    let offline_since = status_of(d.path()).unwrap().since;
    go2_tx.send(()).unwrap();
    wait_until("connected again", || state_is(d.path(), State::Connected));
    wait_until("the hook", || hooks.lock().unwrap().len() == 1);
    settle();
    let hooks = hooks.lock().unwrap().clone();
    assert_eq!(hooks.len(), 1, "only a reconnect calls the hook");
    assert_eq!(hooks[0].to_rfc3339(), offline_since);
    assert_eq!(count(&http, "apps.connections.open"), 2);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn a_silent_socket_is_dropped_and_reconnected_with_catch_up() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let scripts: Vec<Script> = vec![
        // Connection 1: hello, then silence (what a socket looks like after
        // the Mac slept): no ping, no frame, never closed.
        Box::new(move |ws| {
            hello(ws);
            hold(ws);
        }),
        Box::new(move |ws| {
            hello(ws);
            hold(ws);
        }),
    ];
    let (url, server) = fake_socket_server_seq(scripts);
    let http = slack_http(&url, |_| None);
    let hooks: Arc<Mutex<Vec<DateTime<Utc>>>> = Arc::default();
    let seen = hooks.clone();
    let mut l = listener(d.path(), &http, &runs).on_reconnect(move |c| {
        seen.lock().unwrap().push(c.offline_since);
    });
    l.idle_limit = Duration::from_millis(300);
    let (stop, h) = start(l);
    wait_until("a second connection", || {
        count(&http, "apps.connections.open") >= 2
    });
    wait_until("the catch-up hook", || hooks.lock().unwrap().len() == 1);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn a_connection_that_drops_before_hello_is_not_a_reconnect() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let scripts: Vec<Script> = vec![
        // Accepted, then closed without a hello: the listener was never live.
        Box::new(|ws| {
            ws.close(None).unwrap();
            hold(ws);
        }),
        Box::new(|ws| {
            hello(ws);
            hold(ws);
        }),
    ];
    let (url, server) = fake_socket_server_seq(scripts);
    let http = slack_http(&url, |_| None);
    let hooks = Arc::new(AtomicUsize::new(0));
    let n = hooks.clone();
    let l = listener(d.path(), &http, &runs).on_reconnect(move |_| {
        n.fetch_add(1, Ordering::SeqCst);
    });
    let (stop, h) = start(l);
    wait_until("connected", || state_is(d.path(), State::Connected));
    settle();
    assert_eq!(count(&http, "apps.connections.open"), 2);
    assert_eq!(hooks.load(Ordering::SeqCst), 0);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn a_rejected_app_token_stops_after_one_attempt() {
    let d = tempfile::tempdir().unwrap();
    let http = slack_http("ws://127.0.0.1:1", |m| {
        (m == "apps.connections.open").then(|| json!({"ok": false, "error": "invalid_auth"}))
    });
    let (tx, rx) = mpsc::channel();
    let (cfg, w, rt) = (config(d.path()), web(&http), runtime(d.path()));
    std::thread::spawn(move || {
        let _ = tx.send(run_listener(cfg, w, rt, Arc::new(AtomicBool::new(false))));
    });
    assert_eq!(rx.recv_timeout(WAIT).unwrap(), Exit::TokenRejected);
    assert_eq!(count(&http, "apps.connections.open"), 1);
    let st = status_of(d.path()).unwrap();
    assert_eq!(st.state, State::NeedsAttention);
    let err = st.last_error.unwrap();
    assert!(err.contains("rejected"), "{err}");
    let raw = std::fs::read_to_string(config(d.path()).status_path()).unwrap();
    assert!(!raw.contains(BOT) && !raw.contains(APP), "{raw}");
}

#[test]
fn a_bot_token_revoked_while_connected_stops_the_listener() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let rejections = Arc::new(AtomicUsize::new(0));
    let seen = rejections.clone();
    let (url, server) = fake_socket_server(move |ws| {
        hello(ws);
        ws.send(mention_frame("e1", "Ev1", "U1", "1.0")).unwrap();
        read_ack(ws);
        // Wait until a post with the revoked token has failed (the first,
        // "on it", still succeeds), while the listener waits on the socket.
        let deadline = Instant::now() + WAIT;
        while seen.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(200));
        let _ = ws.send(mention_frame("e2", "Ev2", "U1", "2.0"));
        hold(ws);
    });
    let n = rejections.clone();
    let posted = AtomicBool::new(false);
    let http = slack_http(&url, move |m| {
        // The "on it" post goes through; every bot write after it is refused.
        let writes = ["chat.postMessage", "chat.update", "reactions.remove"];
        if !writes.contains(&m) || !posted.swap(true, Ordering::SeqCst) {
            return None;
        }
        n.fetch_add(1, Ordering::SeqCst);
        Some(json!({"ok": false, "error": "token_revoked"}))
    });
    let r = runs.clone();
    // The agent takes a moment, so the token is rejected on the final reply,
    // while the main loop is blocked reading the socket.
    let l = listener(d.path(), &http, &runs).with_runner(move |rt, key, _p, _s| {
        r.started.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(300));
        rt.jobs.finish(key, None, Some("done".into()));
        r.finished.fetch_add(1, Ordering::SeqCst);
    });
    let (tx, rx) = mpsc::channel();
    // No stop flag is ever set: the listener must end on its own.
    std::thread::spawn(move || {
        let _ = tx.send(l.run(Arc::new(AtomicBool::new(false))));
    });
    assert_eq!(rx.recv_timeout(WAIT).unwrap(), Exit::TokenRejected);
    server.join().unwrap();
    settle();
    assert_eq!(runs.started(), 1, "the mention after the rejection ran");
    let st = status_of(d.path()).unwrap();
    assert_eq!(st.state, State::NeedsAttention);
    assert_eq!(
        st.last_error.as_deref(),
        Some("Slack rejected the bot token")
    );
    let raw = std::fs::read_to_string(config(d.path()).status_path()).unwrap();
    assert!(!raw.contains(BOT) && !raw.contains(APP), "{raw}");
}

#[test]
fn a_rejected_bot_token_stops_before_opening_the_socket() {
    let d = tempfile::tempdir().unwrap();
    let http = slack_http("ws://127.0.0.1:1", |m| {
        (m == "auth.test").then(|| json!({"ok": false, "error": "token_revoked"}))
    });
    let exit = run_listener(
        config(d.path()),
        web(&http),
        runtime(d.path()),
        Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(exit, Exit::TokenRejected);
    assert_eq!(count(&http, "auth.test"), 1);
    assert_eq!(count(&http, "apps.connections.open"), 0);
    assert!(state_is(d.path(), State::NeedsAttention));
}

#[test]
fn a_second_listener_on_the_same_state_dir_exits_at_once() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let (url, server) = fake_socket_server(|ws| {
        hello(ws);
        hold(ws);
    });
    let http = slack_http(&url, |_| None);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("connected", || state_is(d.path(), State::Connected));
    let http2 = slack_http(&url, |_| None);
    let started = Instant::now();
    let mut second = Listener::new(config(d.path()), web(&http2), runtime(d.path()));
    second.lock_tries = 3;
    second.lock_retry_every = Duration::from_millis(10);
    let exit = second.run(Arc::new(AtomicBool::new(false)));
    assert_eq!(exit, Exit::LockHeld);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(http2.calls.lock().unwrap().is_empty());
    assert!(state_is(d.path(), State::Connected), "status left alone");
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn stop_is_noticed_while_the_server_keeps_pinging() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let (url, server) = fake_socket_server(|ws| {
        hello(ws);
        // Pings every 20 ms until the client goes away.
        while ws.send(Message::Ping(vec![1])).is_ok() {
            std::thread::sleep(Duration::from_millis(20));
            // Drain the pongs (a read error means the client left).
            let _ = ws
                .get_mut()
                .set_read_timeout(Some(Duration::from_millis(1)));
            match ws.read() {
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return,
            }
        }
    });
    let http = slack_http(&url, |_| None);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("connected", || state_is(d.path(), State::Connected));
    std::thread::sleep(Duration::from_millis(200));
    let asked = Instant::now();
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    assert!(
        asked.elapsed() < Duration::from_secs(3),
        "{:?}",
        asked.elapsed()
    );
    server.join().unwrap();
}

#[test]
fn a_web_api_failure_during_a_run_shows_in_status() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let (url, server) = fake_socket_server(|ws| {
        hello(ws);
        ws.send(mention_frame("e1", "Ev1", "U1", "1.0")).unwrap();
        read_ack(ws);
        hold(ws);
    });
    let http = slack_http(&url, |m| {
        (m == "reactions.add")
            .then(|| json!({"ok": false, "error": "missing_scope", "needed": "reactions:write"}))
    });
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the run", || runs.finished() == 1);
    wait_until("the status error", || {
        status_of(d.path())
            .and_then(|s| s.last_error)
            .is_some_and(|e| e.contains("reactions:write"))
    });
    assert!(state_is(d.path(), State::Connected));
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn an_unwritable_seen_store_fails_open_and_says_so() {
    let d = tempfile::tempdir().unwrap();
    // A directory where the seen-ids file should be: it can never be written.
    std::fs::create_dir_all(config(d.path()).state_dir.join("seen.json")).unwrap();
    let runs = Arc::new(Runs::default());
    let (url, server) = fake_socket_server(|ws| {
        hello(ws);
        ws.send(mention_frame("e1", "Ev1", "U1", "1.0")).unwrap();
        read_ack(ws);
        hold(ws);
    });
    let http = slack_http(&url, |_| None);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the run", || runs.finished() == 1);
    wait_until("the status error", || {
        status_of(d.path())
            .and_then(|s| s.last_error)
            .is_some_and(|e| e.contains("de-dupe"))
    });
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn ignored_events_update_last_event_but_never_run() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let (url, server) = fake_socket_server(|ws| {
        hello(ws);
        ws.send(Message::text(
            json!({"type": "events_api", "envelope_id": "e1", "payload": {
                "event_id": "Ev1", "event": {"type": "reaction_added", "user": "U1"}}})
            .to_string(),
        ))
        .unwrap();
        read_ack(ws);
        hold(ws);
    });
    let http = slack_http(&url, |_| None);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("last_event", || {
        status_of(d.path()).is_some_and(|s| s.last_event.is_some())
    });
    settle();
    assert_eq!(runs.started(), 0);
    assert!(posts(&http).is_empty());
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

// ----- offline catch-up -----

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

/// A workspace where the bot is in C1 and C1's history holds one allowlisted
/// mention at `ts`.
fn catchup_http(ws_url: &str, ts: &str) -> Arc<FakeHttp> {
    let ts = ts.to_string();
    slack_http_req(ws_url, move |req| match method(req) {
        "users.conversations" => Some(json!({"ok": true, "channels": [{"id": "C1"}]})),
        "conversations.history" => Some(json!({"ok": true, "messages": [
            {"type": "message", "user": "U1", "ts": ts, "text": "<@UBOT> check the build"}]})),
        _ => None,
    })
}

fn catchup_press(envelope: &str, user: &str, value: &str) -> Message {
    catchup_press_at(envelope, user, value, "900.0")
}

/// A press on the offer message `offer_ts`.
fn catchup_press_at(envelope: &str, user: &str, value: &str, offer_ts: &str) -> Message {
    Message::text(
        json!({"type": "interactive", "envelope_id": envelope, "payload": {
            "type": "block_actions", "user": {"id": user}, "channel": {"id": "C1"},
            "container": {"message_ts": offer_ts},
            "message": {"ts": offer_ts, "thread_ts": "1.0"},
            "actions": [{"action_id": "loopx_catchup", "value": value}]}})
        .to_string(),
    )
}

/// Connection 1: hello, then Slack asks to reconnect. Connection 2: hello,
/// then every message sent on `tx` (each acked), then held open.
fn outage_scripts() -> (Vec<Script>, mpsc::Sender<Message>) {
    let (scripts, tx, _) = outage_scripts_counting();
    (scripts, tx)
}

/// [`outage_scripts`] plus a count of the frames acked so far.
fn outage_scripts_counting() -> (Vec<Script>, mpsc::Sender<Message>, Arc<AtomicUsize>) {
    let (tx, rx) = mpsc::channel::<Message>();
    let acked = Arc::new(AtomicUsize::new(0));
    let counted = acked.clone();
    let scripts: Vec<Script> = vec![
        Box::new(|ws| {
            hello(ws);
            ws.send(Message::text(
                r#"{"type":"disconnect","reason":"refresh_requested"}"#,
            ))
            .unwrap();
            hold(ws);
        }),
        Box::new(move |ws| {
            hello(ws);
            while let Ok(m) = rx.recv() {
                ws.send(m).unwrap();
                read_ack(ws);
                counted.fetch_add(1, Ordering::SeqCst);
            }
            hold(ws);
        }),
    ];
    (scripts, tx, acked)
}

fn offers(http: &FakeHttp) -> Vec<(String, Option<String>, String)> {
    posts(http)
        .into_iter()
        .filter(|p| p.2 == super::super::catchup::offer_text())
        .collect()
}

fn write_previous_status(dir: &Path, state: State) {
    status::write_status(
        &config(dir).status_path(),
        &Status {
            state,
            since: (Utc::now() - chrono::Duration::hours(1)).to_rfc3339(),
            last_event: None,
            last_error: None,
        },
    );
}

#[test]
fn a_reconnect_offers_the_missed_mention_and_runs_only_after_run_is_pressed() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let missed_ts = format!("{:.6}", now_secs() - 30.0);
    let (scripts, tx) = outage_scripts();
    let (url, server) = fake_socket_server_seq(scripts);
    let http = catchup_http(&url, &missed_ts);
    // A live drop only: nothing was running before this process, so no previous status.
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the offer", || offers(&http).len() == 1);
    settle();
    let sent = offers(&http);
    assert_eq!(sent.len(), 1, "exactly one offer");
    assert_eq!(sent[0].0, "C1");
    assert_eq!(
        sent[0].1.as_deref(),
        Some(missed_ts.as_str()),
        "in the original's thread"
    );
    let body = http
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|r| method(r) == "chat.postMessage")
        .find_map(|r| r.json_body.clone())
        .unwrap();
    let blocks = body["blocks"].to_string();
    assert!(blocks.contains("loopx_catchup"), "{blocks}");
    assert!(blocks.contains(&format!("C1:{missed_ts}:run")), "{blocks}");
    assert!(
        !body.to_string().contains("check the build"),
        "the offer never echoes the message"
    );
    assert_eq!(runs.started(), 0, "nothing runs without a button");
    let oldest: f64 = http
        .calls
        .lock()
        .unwrap()
        .iter()
        .find(|r| method(r) == "conversations.history")
        .and_then(|r| r.form.clone())
        .and_then(|f| f.into_iter().find(|(k, _)| k == "oldest"))
        .map(|(_, v)| v.parse().unwrap())
        .unwrap();
    let ago = now_secs() - oldest;
    assert!(
        (590.0..620.0).contains(&ago),
        "looks back about 600 s, not {ago}"
    );

    tx.send(catchup_press("p1", "U1", &format!("C1:{missed_ts}:run")))
        .unwrap();
    wait_until("the run", || runs.started() == 1);
    tx.send(catchup_press("p2", "U1", &format!("C1:{missed_ts}:run")))
        .unwrap();
    wait_until("the second press to be handled", || {
        count(&http, "conversations.history") >= 2
    });
    settle();
    assert_eq!(runs.started(), 1, "a double press runs once");
    assert_eq!(offers(&http).len(), 1);
    drop(tx);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn a_second_reconnect_does_not_offer_the_same_message_again() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let missed_ts = format!("{:.6}", now_secs() - 30.0);
    let scripts: Vec<Script> = vec![
        Box::new(|ws| {
            hello(ws);
            ws.send(Message::text(r#"{"type":"disconnect","reason":"x"}"#))
                .unwrap();
            hold(ws);
        }),
        Box::new(|ws| {
            hello(ws);
            ws.send(Message::text(r#"{"type":"disconnect","reason":"x"}"#))
                .unwrap();
            hold(ws);
        }),
        Box::new(|ws| {
            hello(ws);
            hold(ws);
        }),
    ];
    let (url, server) = fake_socket_server_seq(scripts);
    let http = catchup_http(&url, &missed_ts);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("three connections", || {
        count(&http, "apps.connections.open") == 3
    });
    wait_until("both scans", || count(&http, "users.conversations") == 2);
    settle();
    assert_eq!(offers(&http).len(), 1);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn the_first_hello_after_a_relaunch_is_an_offline_period_too() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    write_previous_status(d.path(), State::Connected); // a crash leaves this behind
    let missed_ts = format!("{:.6}", now_secs() - 30.0);
    let (url, server) = fake_socket_server(|ws| {
        hello(ws);
        hold(ws);
    });
    let http = catchup_http(&url, &missed_ts);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the offer", || offers(&http).len() == 1);
    settle();
    assert_eq!(offers(&http).len(), 1);
    assert_eq!(runs.started(), 0);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn a_first_ever_start_has_no_offline_period_to_catch_up() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let missed_ts = format!("{:.6}", now_secs() - 30.0);
    let (url, server) = fake_socket_server(|ws| {
        hello(ws);
        hold(ws);
    });
    let http = catchup_http(&url, &missed_ts);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("connected", || state_is(d.path(), State::Connected));
    settle();
    assert_eq!(count(&http, "users.conversations"), 0);
    assert_eq!(count(&http, "conversations.history"), 0);
    assert!(offers(&http).is_empty());
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

/// Reconnect scenario with `frames` sent after the offer is up; returns once
/// each was handled (acked).
fn press_scenario(
    frames: impl Fn(&str) -> Vec<Message>,
) -> (tempfile::TempDir, Arc<FakeHttp>, Arc<Runs>, String) {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let missed_ts = format!("{:.6}", now_secs() - 30.0);
    let (scripts, tx, acked) = outage_scripts_counting();
    let (url, server) = fake_socket_server_seq(scripts);
    let http = catchup_http(&url, &missed_ts);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the offer", || offers(&http).len() == 1);
    let frames = frames(&missed_ts);
    let n = frames.len();
    for f in frames {
        tx.send(f).unwrap();
    }
    wait_until("every press to be acked", || {
        acked.load(Ordering::SeqCst) == n
    });
    settle();
    drop(tx);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
    (d, http, runs, missed_ts)
}

#[test]
fn a_press_by_a_user_off_the_allowlist_is_acked_and_changes_nothing() {
    let (_d, http, runs, _) = press_scenario(|ts| {
        vec![
            catchup_press("p1", "U9", &format!("C1:{ts}:run")),
            catchup_press("p2", "U9", &format!("C1:{ts}:ignore")),
        ]
    });
    assert_eq!(runs.started(), 0);
    assert_eq!(count(&http, "chat.update"), 0, "no state change");
    assert_eq!(
        count(&http, "conversations.history"),
        1,
        "only the scan read history"
    );
    assert_eq!(posts_saying(&http, NOT_AUTHORIZED), 0);
}

#[test]
fn ignore_updates_the_offer_to_ignored_without_buttons_and_never_runs() {
    let ts = format!("{:.6}", now_secs() - 30.0);
    let (scripts, tx) = outage_scripts();
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let (url, server) = fake_socket_server_seq(scripts);
    let http = catchup_http(&url, &ts);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the offer", || offers(&http).len() == 1);
    tx.send(catchup_press("p1", "U1", &format!("C1:{ts}:ignore")))
        .unwrap();
    wait_until("the update", || count(&http, "chat.update") == 1);
    let body = http
        .calls
        .lock()
        .unwrap()
        .iter()
        .find(|r| method(r) == "chat.update")
        .and_then(|r| r.json_body.clone())
        .unwrap();
    assert_eq!(body["channel"], "C1");
    assert_eq!(body["ts"], "900.0");
    assert_eq!(body["text"], "Ignored.");
    assert!(!body["blocks"].to_string().contains("button"), "{body}");
    settle();
    assert_eq!(runs.started(), 0);
    drop(tx);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn a_garbled_press_value_is_acked_and_ignored() {
    let (_d, http, runs, _) = press_scenario(|_| {
        vec![
            catchup_press("p1", "U1", "C1:nope:run"),
            catchup_press("p2", "U1", "garbage"),
            catchup_press("p3", "U1", "C2:1.5:run"), // not the channel the press came from
        ]
    });
    assert_eq!(runs.started(), 0);
    assert_eq!(count(&http, "chat.update"), 0);
    assert_eq!(count(&http, "conversations.history"), 1);
}

#[test]
fn a_missing_scope_is_noted_once_and_the_listener_carries_on() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let (scripts, tx) = outage_scripts();
    let (url, server) = fake_socket_server_seq(scripts);
    let http = slack_http(&url, |m| {
        (m == "users.conversations")
            .then(|| json!({"ok": false, "error": "missing_scope", "needed": "channels:read"}))
    });
    write_previous_status(d.path(), State::Offline);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the note", || {
        status_of(d.path()).is_some_and(|s| {
            s.last_error.as_deref() == Some("catch-up unavailable (missing scope)")
        })
    });
    assert!(state_is(d.path(), State::Connected));
    // Carries on: a live mention still runs.
    tx.send(mention_frame("m1", "Ev1", "U1", "7.0")).unwrap();
    wait_until("the run", || runs.started() == 1);
    drop(tx);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
    let raw = std::fs::read_to_string(config(d.path()).status_path()).unwrap();
    assert!(!raw.contains(BOT) && !raw.contains(APP), "{raw}");
}

#[test]
fn the_missing_scope_note_is_logged_once_across_reconnects() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let scripts: Vec<Script> = vec![
        Box::new(|ws| {
            hello(ws);
            ws.send(Message::text(r#"{"type":"disconnect","reason":"x"}"#))
                .unwrap();
            hold(ws);
        }),
        Box::new(|ws| {
            hello(ws);
            ws.send(Message::text(r#"{"type":"disconnect","reason":"x"}"#))
                .unwrap();
            hold(ws);
        }),
        Box::new(|ws| {
            hello(ws);
            hold(ws);
        }),
    ];
    let (url, server) = fake_socket_server_seq(scripts);
    let http = slack_http(&url, |m| {
        (m == "users.conversations").then(|| json!({"ok": false, "error": "missing_scope"}))
    });
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("both scans", || count(&http, "users.conversations") == 2);
    settle();
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
    let log = std::fs::read_to_string(crate::runner_util::unified_log_path(d.path())).unwrap();
    assert_eq!(
        log.matches("catch-up unavailable (missing scope)").count(),
        1,
        "{log}"
    );
}

#[test]
fn another_failure_during_catch_up_neither_stops_nor_flags_the_listener() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let (scripts, tx) = outage_scripts();
    let (url, server) = fake_socket_server_seq(scripts);
    let http = slack_http(&url, |m| {
        (m == "users.conversations").then(|| json!({"ok": false, "error": "internal_error"}))
    });
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the scan", || count(&http, "users.conversations") == 1);
    tx.send(mention_frame("m1", "Ev1", "U1", "7.0")).unwrap();
    wait_until("the run", || runs.started() == 1);
    assert!(state_is(d.path(), State::Connected));
    drop(tx);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn a_rejected_bot_token_during_catch_up_stops_the_listener() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    write_previous_status(d.path(), State::Offline);
    let (url, server) = fake_socket_server(|ws| {
        hello(ws);
        hold(ws);
    });
    let http = slack_http(&url, |m| {
        (m == "users.conversations").then(|| json!({"ok": false, "error": "token_revoked"}))
    });
    let (stop, h) = start(listener(d.path(), &http, &runs));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(h.join().unwrap());
    });
    assert_eq!(rx.recv_timeout(WAIT).unwrap(), Exit::TokenRejected);
    drop(stop);
    let st = status_of(d.path()).unwrap();
    assert_eq!(st.state, State::NeedsAttention);
    assert_eq!(
        st.last_error.as_deref(),
        Some("Slack rejected the bot token")
    );
    drop(server);
}

#[test]
fn a_pressed_run_goes_through_the_rate_limit_like_a_live_message() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let missed_ts = format!("{:.6}", now_secs() - 30.0);
    let (scripts, tx) = outage_scripts();
    let (url, server) = fake_socket_server_seq(scripts);
    let http = catchup_http(&url, &missed_ts);
    let mut cfg = config(d.path());
    cfg.rate_per_hour = 1;
    let l = Listener::new(cfg, web(&http), runtime(d.path()))
        .with_runner(quick(&runs))
        .with_backoff(|_| Duration::from_millis(10));
    let (stop, h) = start(l);
    wait_until("the offer", || offers(&http).len() == 1);
    // U1's one request this hour is a live mention.
    tx.send(mention_frame("m1", "Ev1", "U1", "7.0")).unwrap();
    wait_until("the live run", || runs.started() == 1);
    tx.send(catchup_press("p1", "U1", &format!("C1:{missed_ts}:run")))
        .unwrap();
    wait_until("the slow-down reply", || posts_saying(&http, TOO_FAST) == 1);
    settle();
    assert_eq!(runs.started(), 1);
    drop(tx);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

// ----- one run per message, and the press fetch off the loop -----

/// [`catchup_http`] whose thread replies (`conversations.replies`) are `replies`.
fn catchup_http_replies(ws_url: &str, ts: &str, replies: Value) -> Arc<FakeHttp> {
    let ts = ts.to_string();
    slack_http_req(ws_url, move |req| match method(req) {
        "users.conversations" => Some(json!({"ok": true, "channels": [{"id": "C1"}]})),
        "conversations.history" => Some(json!({"ok": true, "messages": [
            {"type": "message", "user": "U1", "ts": ts, "text": "<@UBOT> check the build"}]})),
        "conversations.replies" => Some(replies.clone()),
        _ => None,
    })
}

/// [`catchup_http`] whose single-message read (`latest=...`) blocks until a
/// message arrives on the returned sender (or 20 s pass); `inflight` counts
/// the reads that started.
fn gated_http(ws_url: &str, ts: &str) -> (Arc<FakeHttp>, mpsc::Sender<()>, Arc<AtomicUsize>) {
    let (tx, rx) = mpsc::channel::<()>();
    let rx = Mutex::new(rx);
    let inflight = Arc::new(AtomicUsize::new(0));
    let started = inflight.clone();
    let ts = ts.to_string();
    let http = slack_http_req(ws_url, move |req| match method(req) {
        "users.conversations" => Some(json!({"ok": true, "channels": [{"id": "C1"}]})),
        "conversations.history" => {
            let single = req
                .form
                .as_ref()
                .is_some_and(|f| f.iter().any(|(k, _)| k == "latest"));
            if single {
                started.fetch_add(1, Ordering::SeqCst);
                let _ = rx.lock().unwrap().recv_timeout(Duration::from_secs(20));
            }
            Some(json!({"ok": true, "messages": [
                {"type": "message", "user": "U1", "ts": ts, "text": "<@UBOT> check the build"}]}))
        }
        _ => None,
    });
    (http, tx, inflight)
}

#[test]
fn a_message_handled_live_is_not_offered_and_a_stale_press_for_it_runs_nothing() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let ts = format!("{:.6}", now_secs() - 30.0);
    let (tx, rx) = mpsc::channel::<Message>();
    let acked = Arc::new(AtomicUsize::new(0));
    let counted = acked.clone();
    let live = ts.clone();
    let scripts: Vec<Script> = vec![
        Box::new(move |ws| {
            hello(ws);
            ws.send(mention_frame("e1", "Ev1", "U1", &live)).unwrap();
            read_ack(ws);
            ws.send(Message::text(r#"{"type":"disconnect","reason":"x"}"#))
                .unwrap();
            hold(ws);
        }),
        Box::new(move |ws| {
            hello(ws);
            while let Ok(m) = rx.recv() {
                ws.send(m).unwrap();
                read_ack(ws);
                counted.fetch_add(1, Ordering::SeqCst);
            }
            hold(ws);
        }),
    ];
    let (url, server) = fake_socket_server_seq(scripts);
    let http = catchup_http(&url, &ts);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the live run", || runs.started() == 1);
    wait_until("the scan", || count(&http, "users.conversations") == 1);
    settle();
    assert!(
        offers(&http).is_empty(),
        "a message already run live is not offered"
    );
    tx.send(catchup_press("p1", "U1", &format!("C1:{ts}:run")))
        .unwrap();
    wait_until("the press ack", || acked.load(Ordering::SeqCst) == 1);
    settle();
    assert_eq!(runs.started(), 1, "a stale press does not run it again");
    drop(tx);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn the_live_redelivery_of_a_message_a_press_already_ran_does_not_run_again() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let ts = format!("{:.6}", now_secs() - 30.0);
    let (scripts, tx) = outage_scripts();
    let (url, server) = fake_socket_server_seq(scripts);
    let http = catchup_http(&url, &ts);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the offer", || offers(&http).len() == 1);
    tx.send(catchup_press("p1", "U1", &format!("C1:{ts}:run")))
        .unwrap();
    wait_until("the run", || runs.started() == 1);
    // Slack redelivers the original event under a new event id.
    tx.send(mention_frame("e2", "Ev-redelivered", "U1", &ts))
        .unwrap();
    settle();
    assert_eq!(runs.started(), 1);
    drop(tx);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn a_press_for_a_message_the_bot_already_answered_runs_nothing_and_says_so() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let ts = format!("{:.6}", now_secs() - 30.0);
    let later = format!("{:.6}", now_secs() - 20.0);
    let (scripts, tx) = outage_scripts();
    let (url, server) = fake_socket_server_seq(scripts);
    let http = catchup_http_replies(
        &url,
        &ts,
        json!({"ok": true, "messages": [
            {"user": "U1", "ts": ts, "text": "<@UBOT> check the build"},
            {"user": "UBOT", "ts": later, "thread_ts": ts, "text": "done"}]}),
    );
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the offer", || offers(&http).len() == 1);
    tx.send(catchup_press("p1", "U1", &format!("C1:{ts}:run")))
        .unwrap();
    wait_until("the update", || count(&http, "chat.update") == 1);
    let body = http
        .calls
        .lock()
        .unwrap()
        .iter()
        .find(|r| method(r) == "chat.update")
        .and_then(|r| r.json_body.clone())
        .unwrap();
    assert_eq!(body["ts"], "900.0");
    assert_eq!(body["text"], "Already handled.");
    assert!(!body["blocks"].to_string().contains("button"), "{body}");
    settle();
    assert_eq!(runs.started(), 0);
    drop(tx);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn a_press_fetch_that_blocks_does_not_stall_the_loop_and_runs_once_released() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let ts = format!("{:.6}", now_secs() - 30.0);
    let (scripts, tx, acked) = outage_scripts_counting();
    let (url, server) = fake_socket_server_seq(scripts);
    let (http, release, inflight) = gated_http(&url, &ts);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the offer", || offers(&http).len() == 1);
    tx.send(catchup_press("p1", "U1", &format!("C1:{ts}:run")))
        .unwrap();
    wait_until("the fetch to start", || {
        inflight.load(Ordering::SeqCst) == 1
    });
    // The fetch is stuck; the loop still acks and handles a live mention.
    tx.send(mention_frame("m2", "Ev2", "U1", "7.000000"))
        .unwrap();
    wait_until("the second ack", || acked.load(Ordering::SeqCst) == 2);
    wait_until("the live run", || runs.started() == 1);
    release.send(()).unwrap();
    wait_until("the pressed run", || runs.started() == 2);
    settle();
    assert_eq!(runs.started(), 2);
    let form = http
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|r| method(r) == "conversations.history")
        .filter_map(|r| r.form.clone())
        .find(|f| f.iter().any(|(k, _)| k == "latest"))
        .unwrap();
    for pair in [
        ("latest", ts.as_str()),
        ("inclusive", "true"),
        ("limit", "1"),
    ] {
        assert!(
            form.contains(&(pair.0.into(), pair.1.into())),
            "{pair:?} in {form:?}"
        );
    }
    drop(tx);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn stop_is_noticed_while_a_press_fetch_is_blocked() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let ts = format!("{:.6}", now_secs() - 30.0);
    let (scripts, tx) = outage_scripts();
    let (url, server) = fake_socket_server_seq(scripts);
    let (http, release, inflight) = gated_http(&url, &ts);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the offer", || offers(&http).len() == 1);
    tx.send(catchup_press("p1", "U1", &format!("C1:{ts}:run")))
        .unwrap();
    wait_until("the fetch to start", || {
        inflight.load(Ordering::SeqCst) == 1
    });
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    let _ = release.send(());
    drop(tx);
    server.join().unwrap();
}

// ----- the offer is not an answer -----

/// What `conversations.replies` really returns for an offered thread: the ask
/// and the bot's own offer (user = bot, in the thread, with buttons).
fn replies_with_offer(ask_ts: &str, offer_ts: &str, extra: Vec<Value>) -> Value {
    let mut messages = vec![
        json!({"user": "U1", "ts": ask_ts, "text": "<@UBOT> check the build"}),
        json!({"user": "UBOT", "ts": offer_ts, "thread_ts": ask_ts,
               "text": "I was offline when you wrote this. Run it now?",
               "blocks": super::super::catchup::offer_blocks("C1", ask_ts)}),
    ];
    messages.extend(extra);
    json!({"ok": true, "messages": messages})
}

#[test]
fn a_run_press_runs_the_request_although_the_offer_is_in_the_thread() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let ts = format!("{:.6}", now_secs() - 30.0);
    let offer_ts = format!("{:.6}", now_secs() - 25.0);
    let (scripts, tx) = outage_scripts();
    let (url, server) = fake_socket_server_seq(scripts);
    let http = catchup_http_replies(&url, &ts, replies_with_offer(&ts, &offer_ts, vec![]));
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the offer", || offers(&http).len() == 1);
    tx.send(catchup_press_at(
        "p1",
        "U1",
        &format!("C1:{ts}:run"),
        &offer_ts,
    ))
    .unwrap();
    wait_until("the run", || runs.started() == 1);
    wait_until("the offer rewritten", || count(&http, "chat.update") >= 1);
    let update = http
        .calls
        .lock()
        .unwrap()
        .iter()
        .find(|r| method(r) == "chat.update")
        .and_then(|r| r.json_body.clone())
        .unwrap();
    assert_eq!(update["ts"], offer_ts.as_str());
    assert_eq!(update["text"], "Running it now.");
    settle();
    assert_eq!(runs.started(), 1);
    drop(tx);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn an_older_offer_in_the_thread_does_not_block_a_run_either() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let ts = format!("{:.6}", now_secs() - 30.0);
    let other_offer = format!("{:.6}", now_secs() - 25.0);
    let (scripts, tx) = outage_scripts();
    let (url, server) = fake_socket_server_seq(scripts);
    // The press comes from offer 900.0; the thread holds another offer.
    let http = catchup_http_replies(&url, &ts, replies_with_offer(&ts, &other_offer, vec![]));
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the offer", || offers(&http).len() == 1);
    tx.send(catchup_press("p1", "U1", &format!("C1:{ts}:run")))
        .unwrap();
    wait_until("the run", || runs.started() == 1);
    drop(tx);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn a_genuine_answer_next_to_the_offer_still_blocks_the_run() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let ts = format!("{:.6}", now_secs() - 30.0);
    let offer_ts = format!("{:.6}", now_secs() - 25.0);
    let answer_ts = format!("{:.6}", now_secs() - 20.0);
    let (scripts, tx) = outage_scripts();
    let (url, server) = fake_socket_server_seq(scripts);
    let answer = json!({"user": "UBOT", "ts": answer_ts, "thread_ts": ts, "text": "done"});
    let http = catchup_http_replies(&url, &ts, replies_with_offer(&ts, &offer_ts, vec![answer]));
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("the offer", || offers(&http).len() == 1);
    tx.send(catchup_press_at(
        "p1",
        "U1",
        &format!("C1:{ts}:run"),
        &offer_ts,
    ))
    .unwrap();
    wait_until("the update", || count(&http, "chat.update") == 1);
    let body = http
        .calls
        .lock()
        .unwrap()
        .iter()
        .find(|r| method(r) == "chat.update")
        .and_then(|r| r.json_body.clone())
        .unwrap();
    assert_eq!(body["ts"], offer_ts.as_str());
    assert_eq!(body["text"], "Already handled.");
    assert!(!body["blocks"].to_string().contains("button"), "{body}");
    settle();
    assert_eq!(runs.started(), 0);
    drop(tx);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn the_missing_scope_note_clears_after_a_later_pass_succeeds() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let (scripts, tx) = outage_scripts();
    let (url, server) = fake_socket_server_seq(scripts);
    let listed = Arc::new(AtomicUsize::new(0));
    let l = listed.clone();
    let http = slack_http(&url, move |m| {
        (m == "users.conversations" && l.fetch_add(1, Ordering::SeqCst) == 0)
            .then(|| json!({"ok": false, "error": "missing_scope", "needed": "im:read"}))
    });
    write_previous_status(d.path(), State::Offline);
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("both passes", || count(&http, "users.conversations") == 2);
    wait_until("the note cleared", || {
        status_of(d.path()).is_some_and(|s| s.state == State::Connected && s.last_error.is_none())
    });
    drop(tx);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
    let log = std::fs::read_to_string(crate::runner_util::unified_log_path(d.path())).unwrap();
    assert_eq!(
        log.matches("catch-up unavailable (missing scope)").count(),
        1,
        "{log}"
    );
}

#[test]
fn a_one_off_web_api_error_clears_after_a_later_success() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let (tx, rx) = mpsc::channel::<Message>();
    let (url, server) = fake_socket_server(move |ws| {
        hello(ws);
        while let Ok(m) = rx.recv() {
            ws.send(m).unwrap();
            read_ack(ws);
        }
        hold(ws);
    });
    let failed = Arc::new(AtomicUsize::new(0));
    let f = failed.clone();
    let http = slack_http(&url, move |m| {
        (m == "reactions.add" && f.fetch_add(1, Ordering::SeqCst) == 0)
            .then(|| json!({"ok": false, "error": "internal_error"}))
    });
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("connected", || state_is(d.path(), State::Connected));
    tx.send(mention_frame("m1", "Ev1", "U1", "7.0")).unwrap();
    wait_until("the reply", || {
        runs.finished() == 1 && count(&http, "chat.update") >= 1
    });
    settle();
    wait_until("no lasting error", || {
        status_of(d.path()).is_some_and(|s| s.state == State::Connected && s.last_error.is_none())
    });
    drop(tx);
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn a_lock_held_for_a_moment_is_waited_for_not_reported_as_held() {
    let d = tempfile::tempdir().unwrap();
    let runs = Arc::new(Runs::default());
    let (url, server) = fake_socket_server(|ws| {
        hello(ws);
        hold(ws);
    });
    let http = slack_http(&url, |_| None);
    // What the dashboard's liveness probe does: take the lock for an instant.
    std::fs::create_dir_all(d.path().join("slack")).unwrap();
    let probe = crate::file_lock::FileLock::try_exclusive(&d.path().join("slack/listener.lock"))
        .unwrap()
        .unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        drop(probe);
    });
    let (stop, h) = start(listener(d.path(), &http, &runs));
    wait_until("connected or exited", || {
        state_is(d.path(), State::Connected) || h.is_finished()
    });
    assert!(
        !h.is_finished(),
        "the listener gave up on a briefly held lock"
    );
    release.join().unwrap();
    assert_eq!(stop_and_join(&stop, h), Exit::Shutdown);
    server.join().unwrap();
}

#[test]
fn swapped_tokens_or_a_missing_connections_scope_need_attention_not_retries() {
    let cases = [
        (
            "auth.test",
            json!({"ok": false, "error": "not_allowed_token_type"}),
            "bot token",
        ),
        (
            "apps.connections.open",
            json!({"ok": false, "error": "not_allowed_token_type"}),
            "app-level token",
        ),
        (
            "apps.connections.open",
            json!({"ok": false, "error": "missing_scope", "needed": "connections:write"}),
            "connections:write",
        ),
    ];
    for (failing, body, want) in cases {
        let d = tempfile::tempdir().unwrap();
        let runs = Arc::new(Runs::default());
        let http = slack_http("ws://127.0.0.1:1", move |m| {
            (m == failing).then(|| body.clone())
        });
        let (stop, h) = start(listener(d.path(), &http, &runs));
        wait_until("an end or a retry", || {
            h.is_finished() || count(&http, failing) >= 3
        });
        let ended = h.is_finished();
        let exit = stop_and_join(&stop, h);
        assert!(ended, "{failing}: retried instead of stopping");
        assert_eq!(exit, Exit::TokenRejected, "{failing}");
        assert_eq!(count(&http, failing), 1, "{failing}");
        let st = status_of(d.path()).unwrap();
        assert_eq!(st.state, State::NeedsAttention, "{failing}");
        let why = st.last_error.unwrap_or_default();
        assert!(why.contains(want), "{failing}: {why}");
        let raw = std::fs::read_to_string(config(d.path()).status_path()).unwrap();
        assert!(!raw.contains(BOT) && !raw.contains(APP), "{raw}");
    }
}
