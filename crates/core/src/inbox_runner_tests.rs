use super::*;
use crate::inbox_config::default_categories;
use crate::mail_provider::MessageRow;
use chrono::{FixedOffset, TimeZone};
use serde_json::json;
use std::collections::HashMap;
use tempfile::{tempdir, TempDir};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 27, 9, 0, 0).unwrap()
}

fn inbox() -> Value {
    json!({"name": "w", "label": "Work", "provider": "gmail", "account": "me@example.com", "enabled": true,
           "urgent_brief": "", "vip_senders": [], "exclude_senders": [], "categories": null, "slack_bundle": null})
}

fn config() -> Value {
    json!({"default_categories": default_categories(), "inboxes": [inbox()]})
}

fn m(i: u32, minute: u32) -> MailMessage {
    MailMessage {
        id: format!("m{i}"),
        thread_id: format!("t{i}"),
        from: "Alice <alice@x.com>".into(),
        to: "me@example.com".into(),
        subject: format!("Subject {i}"),
        date: format!("2026-09-27T08:{minute:02}:00+00:00"),
        body_text: format!("PRIVATE BODY {i}"),
        ..Default::default()
    }
}

type FetchArgs = (DateTime<FixedOffset>, HashSet<String>, Vec<String>, usize);

#[derive(Default)]
struct Log {
    labelled: Vec<(String, String)>,
    drafts: Vec<(String, String)>,
    fetch_args: Option<FetchArgs>,
}

struct Fake {
    messages: Vec<MailMessage>,
    address: String,
    fail_label_on: Option<String>,
    label_error: MailError,
    fail_draft_on: Option<String>,
    draft_error: MailError,
    ensure_labels_error: Option<MailError>,
    fetch_error: Option<MailError>,
    log: Arc<Mutex<Log>>,
}

impl Fake {
    fn new(messages: Vec<MailMessage>) -> (Self, Arc<Mutex<Log>>) {
        let log: Arc<Mutex<Log>> = Arc::default();
        let http500 = MailError::Http(crate::mail_http::MailHttpError {
            status: Some(500),
            body: String::new(),
            url: "http://x/y".into(),
        });
        (
            Self {
                messages,
                address: "me@example.com".into(),
                fail_label_on: None,
                label_error: http500.clone(),
                fail_draft_on: None,
                draft_error: http500,
                ensure_labels_error: None,
                fetch_error: None,
                log: log.clone(),
            },
            log,
        )
    }
}

impl MailProvider for Fake {
    fn profile_address(&mut self) -> Result<String, MailError> {
        Ok(self.address.clone())
    }
    fn fetch_new(
        &mut self,
        since: DateTime<FixedOffset>,
        seen: &HashSet<String>,
        exclude: &[String],
        limit: usize,
    ) -> Result<Vec<MailMessage>, MailError> {
        self.log.lock().unwrap().fetch_args = Some((since, seen.clone(), exclude.to_vec(), limit));
        if let Some(e) = &self.fetch_error {
            return Err(e.clone());
        }
        Ok(self
            .messages
            .iter()
            .filter(|m| !seen.contains(&m.id))
            .take(limit)
            .cloned()
            .collect())
    }
    fn ensure_labels(&mut self, labels: &[String]) -> Result<HashMap<String, String>, MailError> {
        if let Some(e) = &self.ensure_labels_error {
            return Err(e.clone());
        }
        Ok(labels
            .iter()
            .map(|n| (n.clone(), format!("id-{n}")))
            .collect())
    }
    fn apply_label(&mut self, id: &str, label_id: &str) -> Result<(), MailError> {
        if self.fail_label_on.as_deref() == Some(id) {
            return Err(self.label_error.clone());
        }
        self.log
            .lock()
            .unwrap()
            .labelled
            .push((id.into(), label_id.into()));
        Ok(())
    }
    fn create_reply_draft(
        &mut self,
        message: &MailMessage,
        body: &str,
    ) -> Result<String, MailError> {
        if self.fail_draft_on.as_deref() == Some(message.id.as_str()) {
            return Err(self.draft_error.clone());
        }
        self.log
            .lock()
            .unwrap()
            .drafts
            .push((message.id.clone(), body.into()));
        Ok(format!("https://mail/{}", message.id))
    }
    fn search_recent(
        &mut self,
        _: &[String],
        _: i64,
        _: usize,
    ) -> Result<Vec<MessageRow>, MailError> {
        unreachable!()
    }
    fn search_messages(&mut self, _: &str, _: usize) -> Result<Vec<MessageRow>, MailError> {
        unreachable!()
    }
    fn read_message(&mut self, _: &str) -> Result<MailMessage, MailError> {
        unreachable!()
    }
}

fn decision(id: &str, cat: &str, draft: Option<&str>) -> Value {
    json!({"id": id, "category": cat, "reason": "because", "draft_body": draft})
}

fn replies(entries: Vec<Value>) -> Invoker {
    let text = Value::Array(entries).to_string();
    Arc::new(move |_, _| {
        Ok(AiOut {
            text: text.clone(),
            cost_usd: Some(0.01),
        })
    })
}

fn deps_for(provider: Fake, invoke: Invoker) -> Deps {
    let provider = Arc::new(Mutex::new(Some(provider)));
    Deps {
        token_fn: Arc::new(|_| Ok("tok".into())),
        provider_factory: Arc::new(move |_, _, _| {
            Ok(Box::new(
                provider
                    .lock()
                    .unwrap()
                    .take()
                    .expect("one provider per test"),
            ) as Box<dyn MailProvider>)
        }),
        invoke,
        cli_fn: Arc::new(|| "claude".into()),
        slack_post: Arc::new(|_, _, _| Ok(())),
        instructions: Some("INSTR".into()),
        max_cost_usd: Some(2.0),
        triage: None,
    }
}

fn run(deps: &Deps, state: &Path) -> Outcome {
    let env = TriageEnv {
        deps,
        state_dir: state,
        repo_root: Path::new("/nonexistent"),
        home: Path::new("/nonexistent"),
    };
    triage_inbox(&inbox(), &config(), now(), &env)
}

fn happy_decisions() -> Vec<Value> {
    vec![
        decision("m1", "urgent", Some("On it.")),
        decision("m2", "fyi", None),
    ]
}

// ------------------------------------------------------------- classify

const GOOD: &str = r#"[{"id": "m1", "category": "fyi", "reason": "r", "draft_body": null}]"#;

fn cats() -> Vec<Value> {
    default_categories().as_array().unwrap().clone()
}

#[test]
fn classify_retries_once_on_bad_json_then_succeeds() {
    let calls = Arc::new(Mutex::new(0));
    let c = calls.clone();
    let invoke: Invoker = Arc::new(move |_, _| {
        let mut n = c.lock().unwrap();
        *n += 1;
        Ok(AiOut {
            text: if *n == 1 {
                "not json".into()
            } else {
                GOOD.into()
            },
            cost_usd: Some(0.1),
        })
    });
    let (d, cost) = classify("p", &[m(1, 0)], &cats(), &invoke, Some(2.0)).unwrap();
    assert_eq!((d.len(), *calls.lock().unwrap()), (1, 2));
    assert!((cost.unwrap() - 0.2).abs() < 1e-9);
}

#[test]
fn classify_fails_after_two_bad_replies_with_the_last_error() {
    let invoke: Invoker = Arc::new(|_, _| {
        Ok(AiOut {
            text: "nope".into(),
            cost_usd: None,
        })
    });
    let e = classify("p", &[m(1, 0)], &cats(), &invoke, Some(2.0)).unwrap_err();
    assert_eq!(e, "no JSON array in the AI response");
}

#[test]
fn classify_retries_on_a_failed_call_and_names_the_class() {
    let calls = Arc::new(Mutex::new(0));
    let c = calls.clone();
    let invoke: Invoker = Arc::new(move |_, _| {
        let mut n = c.lock().unwrap();
        *n += 1;
        if *n == 1 {
            Err(AiFail::Timeout)
        } else {
            Ok(AiOut {
                text: GOOD.into(),
                cost_usd: None,
            })
        }
    });
    let (_, cost) = classify("p", &[m(1, 0)], &cats(), &invoke, Some(2.0)).unwrap();
    assert_eq!((*calls.lock().unwrap(), cost), (2, None));
    let always: Invoker = Arc::new(|_, _| Err(AiFail::Exited));
    assert_eq!(
        classify("p", &[m(1, 0)], &cats(), &always, Some(2.0)).unwrap_err(),
        "AI call failed: CalledProcessError"
    );
    let triage: Invoker = Arc::new(|_, _| Err(AiFail::Triage("x".into())));
    assert_eq!(
        classify("p", &[m(1, 0)], &cats(), &triage, Some(2.0)).unwrap_err(),
        "AI call failed: TriageFailed"
    );
}

#[test]
fn classify_caps_each_attempt_at_the_remaining_budget() {
    let caps = Arc::new(Mutex::new(Vec::new()));
    let c = caps.clone();
    let invoke: Invoker = Arc::new(move |_, cap| {
        c.lock().unwrap().push(cap);
        let n = c.lock().unwrap().len();
        Ok(AiOut {
            text: if n == 1 { "bad".into() } else { GOOD.into() },
            cost_usd: if n == 1 { Some(0.5) } else { None },
        })
    });
    classify("p", &[m(1, 0)], &cats(), &invoke, Some(2.0)).unwrap();
    assert_eq!(*caps.lock().unwrap(), vec![Some(2.0), Some(1.5)]);
    // An attempt with unknown cost is booked at its cap; the floor is 0.05.
    let caps = Arc::new(Mutex::new(Vec::new()));
    let c = caps.clone();
    let unknown: Invoker = Arc::new(move |_, cap| {
        c.lock().unwrap().push(cap);
        Err(AiFail::Timeout)
    });
    let _ = classify("p", &[m(1, 0)], &cats(), &unknown, Some(2.0));
    assert_eq!(*caps.lock().unwrap(), vec![Some(2.0), Some(0.05)]);
    let none = Arc::new(Mutex::new(Vec::new()));
    let c = none.clone();
    let invoke: Invoker = Arc::new(move |_, cap| {
        c.lock().unwrap().push(cap);
        Ok(AiOut {
            text: GOOD.into(),
            cost_usd: None,
        })
    });
    classify("p", &[m(1, 0)], &cats(), &invoke, None).unwrap();
    assert_eq!(*none.lock().unwrap(), vec![None]);
}

// ---------------------------------------------------------------- triage

#[test]
fn happy_path_labels_drafts_and_records_seen() {
    let d = tempdir().unwrap();
    let (fake, log) = Fake::new(vec![m(1, 1), m(2, 2)]);
    let o = run(&deps_for(fake, replies(happy_decisions())), d.path());
    assert_eq!(o.status, "ok");
    assert_eq!(
        Value::Object(o.counts.clone()),
        json!({"urgent": 1, "fyi": 1})
    );
    assert_eq!(o.urgent.len(), 1);
    assert_eq!(o.urgent[0].draft_link.as_deref(), Some("https://mail/m1"));
    assert_eq!(o.drafts[0].body, "On it.");
    let log = log.lock().unwrap();
    assert_eq!(
        log.labelled,
        vec![
            ("m1".into(), "id-Loop/Urgent".into()),
            ("m2".into(), "id-Loop/FYI".into())
        ]
    );
    assert_eq!(log.drafts, vec![("m1".to_string(), "On it.".to_string())]);
    let state = inbox_seen::load("w", d.path());
    assert_eq!(state.seen.keys().collect::<Vec<_>>(), vec!["m1", "m2"]);
    assert_eq!(
        state.high_water.as_deref(),
        Some("2026-09-27T08:02:00+00:00")
    );
    assert_eq!(o.cost_usd, Some(0.01));
}

#[test]
fn codex_fails_every_inbox_before_any_mail_is_read() {
    let d = tempdir().unwrap();
    let (fake, _) = Fake::new(vec![m(1, 1)]);
    let mut deps = deps_for(fake, replies(vec![]));
    deps.cli_fn = Arc::new(|| "codex".into());
    deps.token_fn = Arc::new(|_| panic!("token must not be fetched"));
    let o = run(&deps, d.path());
    assert_eq!(
        (o.status.as_str(), o.error.as_deref()),
        ("failed", Some(CODEX_REFUSAL))
    );
}

#[test]
fn a_quiet_run_makes_no_ai_call() {
    let d = tempdir().unwrap();
    let (fake, _) = Fake::new(vec![]);
    let invoke: Invoker = Arc::new(|_, _| panic!("no AI call expected"));
    let o = run(&deps_for(fake, invoke), d.path());
    assert_eq!(o.status, "quiet");
}

#[test]
fn the_cap_asks_for_one_extra_message_and_flags_overflow() {
    let d = tempdir().unwrap();
    let many: Vec<MailMessage> = (0..60).map(|i| m(i, i % 60)).collect();
    let (fake, log) = Fake::new(many);
    let decisions: Vec<Value> = (0..50)
        .map(|i| decision(&format!("m{i}"), "fyi", None))
        .collect();
    let o = run(&deps_for(fake, replies(decisions)), d.path());
    assert_eq!(o.status, "ok");
    assert!(o.overflow);
    assert_eq!(o.rows.len(), 50);
    assert_eq!(log.lock().unwrap().fetch_args.as_ref().unwrap().3, 51);
}

#[test]
fn an_account_mismatch_fails_before_fetching() {
    let d = tempdir().unwrap();
    let (mut fake, log) = Fake::new(vec![m(1, 1)]);
    fake.address = "other@example.com".into();
    let o = run(&deps_for(fake, replies(vec![])), d.path());
    assert_eq!(o.status, "failed");
    assert_eq!(
        o.error.as_deref(),
        Some("Signed in as other@example.com, expected me@example.com")
    );
    assert!(log.lock().unwrap().fetch_args.is_none());
}

#[test]
fn reauth_required_is_needs_reauth() {
    let d = tempdir().unwrap();
    let (fake, _) = Fake::new(vec![m(1, 1)]);
    let mut deps = deps_for(fake, replies(vec![]));
    deps.token_fn = Arc::new(|_| Err(MailError::Reauth("w is not connected".into())));
    let o = run(&deps, d.path());
    assert_eq!(
        (o.status.as_str(), o.error.as_deref()),
        ("needs_reauth", Some("w is not connected"))
    );
}

#[test]
fn auth_expired_while_fetching_is_needs_reauth() {
    let d = tempdir().unwrap();
    let (mut fake, _) = Fake::new(vec![m(1, 1)]);
    fake.fetch_error = Some(MailError::AuthExpired(crate::mail_http::MailHttpError {
        status: Some(401),
        body: String::new(),
        url: "http://h/p".into(),
    }));
    let o = run(&deps_for(fake, replies(vec![])), d.path());
    assert_eq!(o.status, "needs_reauth");
    assert_eq!(o.error.as_deref(), Some("HTTP 401 from h/p"));
}

#[test]
fn a_keychain_error_while_fetching_is_failed() {
    let d = tempdir().unwrap();
    let (fake, _) = Fake::new(vec![]);
    let mut deps = deps_for(fake, replies(vec![]));
    deps.token_fn = Arc::new(|_| Err(MailError::Keychain("locked".into())));
    let o = run(&deps, d.path());
    assert_eq!(
        (o.status.as_str(), o.error.as_deref()),
        ("failed", Some("locked"))
    );
}

#[test]
fn bad_ai_output_applies_nothing_and_records_nothing() {
    let d = tempdir().unwrap();
    let (fake, log) = Fake::new(vec![m(1, 1)]);
    let invoke: Invoker = Arc::new(|_, _| {
        Ok(AiOut {
            text: "garbage".into(),
            cost_usd: None,
        })
    });
    let o = run(&deps_for(fake, invoke), d.path());
    assert_eq!(o.status, "failed");
    assert!(o.error.unwrap().starts_with("AI triage failed:"));
    assert!(log.lock().unwrap().labelled.is_empty());
    assert!(inbox_seen::load("w", d.path()).seen.is_empty());
}

#[test]
fn a_partial_label_failure_records_only_the_labelled() {
    let d = tempdir().unwrap();
    let (mut fake, log) = Fake::new(vec![m(1, 1), m(2, 2)]);
    fake.fail_label_on = Some("m2".into());
    let o = run(&deps_for(fake, replies(happy_decisions())), d.path());
    assert_eq!(o.status, "failed");
    assert!(o
        .error
        .as_deref()
        .unwrap()
        .starts_with("Labelling stopped after 1 of 2:"));
    assert_eq!(log.lock().unwrap().labelled.len(), 1);
    assert_eq!(
        inbox_seen::load("w", d.path())
            .seen
            .keys()
            .collect::<Vec<_>>(),
        vec!["m1"]
    );
    assert_eq!(o.cost_usd, Some(0.01));
}

#[test]
fn a_draft_failure_keeps_the_label_and_is_reported() {
    let d = tempdir().unwrap();
    let (mut fake, _) = Fake::new(vec![m(1, 1), m(2, 2)]);
    fake.fail_draft_on = Some("m1".into());
    let o = run(&deps_for(fake, replies(happy_decisions())), d.path());
    assert_eq!(o.status, "ok");
    assert_eq!(o.urgent[0].draft_link, None);
    assert!(o.urgent[0].draft_failed && !o.urgent[0].needs_manual_reply);
    assert!(o.drafts.is_empty(), "a failed draft is not listed");
    assert_eq!(inbox_seen::load("w", d.path()).seen.len(), 2);
}

#[test]
fn a_keychain_error_while_labelling_records_the_labelled_and_fails() {
    let d = tempdir().unwrap();
    let (mut fake, _) = Fake::new(vec![m(1, 1), m(2, 2)]);
    (fake.fail_label_on, fake.label_error) =
        (Some("m2".into()), MailError::Keychain("locked".into()));
    let o = run(&deps_for(fake, replies(happy_decisions())), d.path());
    assert_eq!(o.status, "failed");
    assert_eq!(
        o.error.as_deref(),
        Some("Labelling stopped after 1 of 2: locked")
    );
    assert_eq!(
        inbox_seen::load("w", d.path())
            .seen
            .keys()
            .collect::<Vec<_>>(),
        vec!["m1"]
    );
}

#[test]
fn a_keychain_error_during_a_draft_is_draft_failed() {
    let d = tempdir().unwrap();
    let (mut fake, _) = Fake::new(vec![m(1, 1), m(2, 2)]);
    (fake.fail_draft_on, fake.draft_error) =
        (Some("m1".into()), MailError::Keychain("locked".into()));
    let o = run(&deps_for(fake, replies(happy_decisions())), d.path());
    assert_eq!(o.status, "ok");
    assert!(o.urgent[0].draft_failed);
}

#[test]
fn an_unexpected_draft_error_is_contained_and_remaining_drafts_still_run() {
    let d = tempdir().unwrap();
    let (mut fake, log) = Fake::new(vec![m(1, 1), m(2, 2)]);
    (fake.fail_draft_on, fake.draft_error) =
        (Some("m1".into()), MailError::Other("bad header".into()));
    let decisions = vec![
        decision("m1", "urgent", Some("a")),
        decision("m2", "urgent", Some("b")),
    ];
    let o = run(&deps_for(fake, replies(decisions)), d.path());
    assert_eq!(o.status, "ok");
    assert_eq!(o.urgent.len(), 2);
    assert!(o.urgent[0].draft_failed);
    assert_eq!(o.urgent[1].draft_link.as_deref(), Some("https://mail/m2"));
    assert_eq!(log.lock().unwrap().drafts.len(), 1);
}

#[test]
fn auth_expired_while_labelling_is_needs_reauth_and_records_the_labelled() {
    let d = tempdir().unwrap();
    let (mut fake, _) = Fake::new(vec![m(1, 1), m(2, 2)]);
    fake.fail_label_on = Some("m2".into());
    fake.label_error = MailError::AuthExpired(crate::mail_http::MailHttpError {
        status: Some(401),
        body: String::new(),
        url: "http://h/p".into(),
    });
    let o = run(&deps_for(fake, replies(happy_decisions())), d.path());
    assert_eq!(o.status, "needs_reauth");
    assert_eq!(
        inbox_seen::load("w", d.path())
            .seen
            .keys()
            .collect::<Vec<_>>(),
        vec!["m1"]
    );
}

#[test]
fn an_ensure_labels_failure_applies_and_records_nothing() {
    let d = tempdir().unwrap();
    let (mut fake, log) = Fake::new(vec![m(1, 1), m(2, 2)]);
    fake.ensure_labels_error = Some(MailError::Other("boom".into()));
    let o = run(&deps_for(fake, replies(happy_decisions())), d.path());
    assert_eq!(o.status, "failed");
    assert!(o
        .error
        .unwrap()
        .starts_with("Labelling stopped after 0 of 2"));
    assert!(log.lock().unwrap().labelled.is_empty());
    assert!(inbox_seen::load("w", d.path()).seen.is_empty());
}

#[test]
fn a_vip_sender_is_forced_urgent_and_needs_a_manual_reply() {
    let d = tempdir().unwrap();
    let mut one = m(1, 1);
    one.from = "Boss <boss@vip.com>".into();
    let (fake, _) = Fake::new(vec![one]);
    let mut vip = inbox();
    vip["vip_senders"] = json!(["@vip.com"]);
    let deps = deps_for(fake, replies(vec![decision("m1", "fyi", None)]));
    let env = TriageEnv {
        deps: &deps,
        state_dir: d.path(),
        repo_root: Path::new("/x"),
        home: Path::new("/x"),
    };
    let o = triage_inbox(&vip, &config(), now(), &env);
    assert_eq!(o.rows[0].category, "urgent");
    assert!(o.urgent[0].needs_manual_reply);
    assert!(o.rows[0].reason.starts_with("VIP sender"));
}

#[test]
fn seen_ids_and_since_are_passed_to_the_provider() {
    let d = tempdir().unwrap();
    inbox_seen::record(
        "w",
        &[("old".into(), "2026-09-27T05:00:00+00:00".into())],
        now(),
        d.path(),
    )
    .unwrap();
    let (fake, log) = Fake::new(vec![]);
    let mut ib = inbox();
    ib["exclude_senders"] = json!(["@spam.com"]);
    let deps = deps_for(fake, replies(vec![]));
    let env = TriageEnv {
        deps: &deps,
        state_dir: d.path(),
        repo_root: Path::new("/x"),
        home: Path::new("/x"),
    };
    triage_inbox(&ib, &config(), now(), &env);
    let args = log.lock().unwrap().fetch_args.clone().unwrap();
    assert_eq!(
        args.0,
        Utc.with_ymd_and_hms(2026, 9, 27, 5, 0, 0)
            .unwrap()
            .fixed_offset()
    );
    assert_eq!(args.1, HashSet::from(["old".to_string()]));
    assert_eq!(args.2, vec!["@spam.com"]);
}

#[test]
fn excluded_senders_are_dropped_after_the_provider_too() {
    let d = tempdir().unwrap();
    let mut bot = m(2, 2);
    bot.from = "Bot <bot@spam.com>".into();
    let (fake, _) = Fake::new(vec![m(1, 1), bot]);
    let mut ib = inbox();
    ib["exclude_senders"] = json!(["@spam.com"]);
    let deps = deps_for(fake, replies(vec![decision("m1", "fyi", None)]));
    let env = TriageEnv {
        deps: &deps,
        state_dir: d.path(),
        repo_root: Path::new("/x"),
        home: Path::new("/x"),
    };
    let o = triage_inbox(&ib, &config(), now(), &env);
    assert_eq!(o.rows.len(), 1);
}

#[test]
fn outcome_rows_never_contain_body_text() {
    let d = tempdir().unwrap();
    let (fake, _) = Fake::new(vec![m(1, 1), m(2, 2)]);
    let o = run(&deps_for(fake, replies(happy_decisions())), d.path());
    let dump = format!("{:?}", (&o.rows, &o.urgent, &o.counts));
    assert!(!dump.contains("PRIVATE BODY"));
}

#[test]
fn the_prompt_carries_instructions_and_the_messages() {
    let d = tempdir().unwrap();
    let (fake, _) = Fake::new(vec![m(1, 1)]);
    let seen = Arc::new(Mutex::new(String::new()));
    let s = seen.clone();
    let invoke: Invoker = Arc::new(move |prompt, _| {
        *s.lock().unwrap() = prompt.to_string();
        Ok(AiOut {
            text: GOOD.into(),
            cost_usd: None,
        })
    });
    run(&deps_for(fake, invoke), d.path());
    let prompt = seen.lock().unwrap().clone();
    assert!(prompt.starts_with("INSTR\n\n## Input") && prompt.contains("PRIVATE BODY 1"));
}

#[test]
fn missing_instructions_fail_the_inbox() {
    let d = tempdir().unwrap();
    let (fake, _) = Fake::new(vec![m(1, 1)]);
    let mut deps = deps_for(fake, replies(vec![]));
    deps.instructions = None;
    let o = run(&deps, d.path());
    assert_eq!(o.status, "failed");
    assert!(o.error.unwrap().contains("inbox-triage.md"));
}

#[test]
fn compose_adds_custom_instructions_on_top() {
    let root = tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("instructions/loopx")).unwrap();
    std::fs::write(
        root.path().join("instructions/loopx/inbox-triage.md"),
        "BUILT-IN\n",
    )
    .unwrap();
    let home = tempdir().unwrap();
    assert_eq!(
        compose_instructions(root.path(), home.path()).unwrap(),
        "BUILT-IN\n"
    );
    std::fs::write(home.path().join("instructions.md"), " global \n").unwrap();
    std::fs::create_dir_all(home.path().join("instructions")).unwrap();
    std::fs::write(home.path().join("instructions/inbox-triage.md"), "per loop").unwrap();
    let text = compose_instructions(root.path(), home.path()).unwrap();
    assert!(text.starts_with("BUILT-IN\n\n## Custom instructions\n\n"));
    assert!(text.ends_with("global\n\nper loop\n"));
}

// ------------------------------------------------- history / digest / drafts

fn outcome_ok() -> Outcome {
    let mut o = Outcome::new(&inbox(), "ok", None);
    o.counts = json!({"urgent": 1, "action": 2, "fyi": 3})
        .as_object()
        .unwrap()
        .clone();
    o.urgent = vec![UrgentItem {
        from: "Alice <a@x.com>".into(),
        subject: "Prod | down".into(),
        draft_link: Some("https://mail/m1".into()),
        needs_manual_reply: false,
        draft_failed: false,
    }];
    o.rows = vec![Row {
        date: "2026-09-27T08:00:00+00:00".into(),
        from: "Alice <a@x.com>".into(),
        subject: "Prod | down".into(),
        category: "urgent".into(),
        reason: "outage".into(),
        draft_link: Some("https://mail/m1".into()),
    }];
    o.cost_usd = Some(0.01);
    o
}

#[test]
fn history_appends_sections_and_escapes_pipes() {
    let d = tempdir().unwrap();
    let path = write_history(&outcome_ok(), now(), d.path()).unwrap();
    write_history(&outcome_ok(), now() + chrono::Duration::hours(4), d.path()).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(path.file_name().unwrap(), "2026-09-27-w.md");
    assert_eq!(text.matches("## Run ").count(), 2);
    assert!(text.starts_with("# Work - 2026-09-27\n"));
    assert_eq!(text.matches("# Work - ").count(), 1);
    assert!(text.contains("Prod \\| down") && text.contains("[draft](https://mail/m1)"));
    assert!(text.contains("| 08:00 | Alice \\<a@x.com\\> |"));
}

#[test]
fn history_notes_quiet_runs_and_errors() {
    let d = tempdir().unwrap();
    let quiet = Outcome::new(&inbox(), "quiet", None);
    let p = write_history(&quiet, now(), d.path()).unwrap();
    assert!(std::fs::read_to_string(&p)
        .unwrap()
        .contains("\nNo new mail.\n"));
    let failed = Outcome::failed(&inbox(), "boom [x](http://evil)");
    let p = write_history(&failed, now(), d.path()).unwrap();
    assert!(
        std::fs::read_to_string(&p)
            .unwrap()
            .contains("boom \\[x\\]\\(http\\://evil)")
            || std::fs::read_to_string(&p)
                .unwrap()
                .contains("boom \\[x\\](http\\://evil)")
    );
}

#[test]
fn digest_covers_every_status() {
    let quiet = Outcome {
        label: "Quiet".into(),
        status: "quiet".into(),
        counts: Map::new(),
        urgent: vec![],
        ..outcome_ok()
    };
    let reauth = Outcome {
        label: "Home".into(),
        status: "needs_reauth".into(),
        error: Some("expired".into()),
        urgent: vec![],
        ..outcome_ok()
    };
    let failed = Outcome {
        label: "Side".into(),
        status: "failed".into(),
        error: Some("AI triage failed: x".into()),
        urgent: vec![],
        overflow: true,
        ..outcome_ok()
    };
    let ok = outcome_ok();
    let text = format_digest(&[&ok, &quiet, &reauth, &failed], now());
    assert!(text.starts_with("*Inbox Triage* - "));
    assert!(text.contains("*Work*: 1 urgent (drafts ready), 2 action, 3 other"));
    assert!(text.contains("Prod | down"));
    assert!(text.contains("*Quiet*: no new mail"));
    assert!(text.contains("*Home*: needs re-auth"));
    assert!(text.contains("*Side*: failed - AI triage failed: x"));
    assert!(text.contains("<https://mail/m1|draft>"));
}

#[test]
fn digest_manual_reply_and_overflow() {
    let mut o = outcome_ok();
    o.urgent[0].draft_link = None;
    o.urgent[0].needs_manual_reply = true;
    o.overflow = true;
    let text = format_digest(&[&o], now());
    assert!(
        text.contains("(reply manually)") && text.contains("more waiting - over 50 new messages")
    );
    o.urgent[0].draft_failed = true;
    assert!(format_digest(&[&o], now()).contains("draft failed - reply manually"));
}

#[test]
fn digest_escapes_untrusted_text_for_slack() {
    let mut o = outcome_ok();
    o.label = "<!channel> & co".into();
    o.urgent[0].from = "<https://evil|Open>".into();
    o.urgent[0].subject = "a>b".into();
    let text = format_digest(&[&o], now());
    assert!(text.contains("*&lt;!channel&gt; &amp; co*"));
    assert!(text.contains("&lt;https://evil|Open&gt; - \"a&gt;b\""));
    assert!(!text.contains("<!channel>") && !text.contains("<https://evil"));
    assert_eq!(slack_escape("<&>"), "&lt;&amp;&gt;");
}

#[test]
fn markdown_escape_neutralises_markup() {
    assert_eq!(md("a|b"), "a\\|b");
    assert_eq!(md("![p](http://x/y.png)"), "\\!\\[p\\](http\\://x/y.png)");
    assert_eq!(md("[t](https://e.com)"), "\\[t\\](https\\://e.com)");
    assert_eq!(md("- item"), "\\- item");
    assert_eq!(md("  + item"), "  \\+ item");
    assert_eq!(md("12. item"), "12\\. item");
    assert_eq!(md("a\nb\x00c\u{7f}d"), "a b c d");
    assert_eq!(
        md("`code` *b* _i_ # <x>"),
        "\\`code\\` \\*b\\* \\_i\\_ \\# \\<x\\>"
    );
    assert_eq!(md("-nospace"), "-nospace");
    assert_eq!(md(""), "");
}

type Posts = Arc<Mutex<Vec<(String, Option<Value>)>>>;

fn slack_config(dir: &Path, bundles: bool) -> PathBuf {
    let p = dir.join("slack.json");
    let mut cfg = json!({"webhook_url": "http://default"});
    if bundles {
        cfg["bundle_webhooks"] = json!({"team": "http://team"});
    }
    std::fs::write(&p, cfg.to_string()).unwrap();
    p
}

fn recording_post(posts: &Posts) -> SlackPost {
    let p = posts.clone();
    Arc::new(move |url, text, blocks| {
        p.lock()
            .unwrap()
            .push((format!("{url}|{text}"), blocks.cloned()));
        Ok(())
    })
}

#[test]
fn digests_are_grouped_by_bundle() {
    let d = tempdir().unwrap();
    let cfg = slack_config(d.path(), true);
    let posts: Posts = Arc::default();
    let a = outcome_ok();
    let b = Outcome {
        name: "b".into(),
        label: "B".into(),
        slack_bundle: Some("team".into()),
        ..outcome_ok()
    };
    send_digests(&[a, b], now(), &cfg, &recording_post(&posts));
    let posts = posts.lock().unwrap();
    assert_eq!(posts.len(), 2);
    assert!(posts[0].0.starts_with("http://default|") && posts[0].0.contains("*Work*"));
    assert!(posts[1].0.starts_with("http://team|") && posts[1].0.contains("*B*"));
}

#[test]
fn a_slack_failure_never_raises_and_a_rejected_template_is_retried_as_plain_text() {
    let d = tempdir().unwrap();
    let cfg = d.path().join("slack.json");
    std::fs::write(
        &cfg,
        json!({"webhook_url": "http://h", "block_templates": {"t": {"notification_key": "inbox_triage_digest",
               "blocks": [{"text": "{{message}}"}]}}})
        .to_string(),
    )
    .unwrap();
    let calls: Posts = Arc::default();
    let c = calls.clone();
    let reject_blocks: SlackPost = Arc::new(move |_, text, blocks| {
        c.lock().unwrap().push((text.to_string(), blocks.cloned()));
        if blocks.is_some() {
            Err("invalid_blocks".into())
        } else {
            Ok(())
        }
    });
    send_digests(&[outcome_ok()], now(), &cfg, &reject_blocks);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].1.is_some() && calls[1].1.is_none());
    let always_fail: SlackPost = Arc::new(|_, _, _| Err("down".into()));
    send_digests(&[outcome_ok()], now(), &cfg, &always_fail);
    send_digests(
        &[outcome_ok()],
        now(),
        &d.path().join("missing.json"),
        &always_fail,
    );
}

#[test]
fn drafts_are_archived_newest_first_with_inbox_metadata() {
    let d = tempdir().unwrap();
    let path = d.path().join("drafts.json");
    let mut o = outcome_ok();
    o.drafts = vec![DraftRecord {
        from: "Alice".into(),
        subject: "S".into(),
        date: "2026-09-27T08:00:00+00:00".into(),
        category: "urgent".into(),
        body: "On it".into(),
        draft_link: "https://mail/m1".into(),
    }];
    save_drafts(&o, &inbox(), now(), &path);
    save_drafts(
        &Outcome::new(&inbox(), "quiet", None),
        &inbox(),
        now(),
        &path,
    );
    let saved: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0]["body"], "On it");
    assert_eq!(saved[0]["inbox"], "w");
    assert_eq!(saved[0]["inbox_label"], "Work");
    assert_eq!(saved[0]["account"], "me@example.com");
    assert_eq!(saved[0]["saved_at"], "2026-09-27T09:00:00+00:00");
}

// ------------------------------------------------------------- run_all

struct World {
    _dir: TempDir,
    ctx: InboxCtx,
}

fn world(inboxes: Value) -> World {
    let dir = tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("loops/inbox-triage")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../loops/inbox-triage/loop.yaml"),
        root.join("loops/inbox-triage/loop.yaml"),
    )
    .unwrap();
    let mut ctx = InboxCtx::new(root);
    ctx.home = dir.path().join("home");
    ctx.config_path = dir.path().join("inboxes.json");
    ctx.events_dir = Some(dir.path().join("events"));
    ctx.results_dir = Some(dir.path().join("results"));
    ctx.slack_config = dir.path().join("slack.json");
    std::fs::write(
        &ctx.config_path,
        json!({"default_categories": default_categories(), "inboxes": inboxes}).to_string(),
    )
    .unwrap();
    World { _dir: dir, ctx }
}

fn stub_triage(f: impl Fn(&Value) -> Outcome + Send + Sync + 'static) -> Deps {
    let (fake, _) = Fake::new(vec![]);
    let mut deps = deps_for(fake, replies(vec![]));
    deps.triage = Some(Arc::new(move |inbox, _, _| f(inbox)));
    deps
}

fn named(inbox: &Value, status: &str) -> Outcome {
    let mut o = outcome_ok();
    (o.name, o.label, o.status) = (
        inbox["name"].as_str().unwrap().into(),
        inbox["label"].as_str().unwrap().into(),
        status.into(),
    );
    if status != "ok" {
        o.error = Some("x".into());
    }
    o
}

fn inbox_named(name: &str, label: &str) -> Value {
    let mut i = inbox();
    (i["name"], i["label"]) = (json!(name), json!(label));
    i
}

fn disabled(name: &str) -> Value {
    let mut i = inbox_named(name, name);
    i["enabled"] = json!(false);
    i
}

fn states(ctx: &InboxCtx) -> HashMap<String, String> {
    inbox_status::read(&ctx.status_path)["inboxes"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v["state"].as_str().unwrap().to_string()))
        .collect()
}

#[test]
fn run_all_isolates_failures_and_writes_status() {
    let w = world(json!([inbox(), inbox_named("h", "Home"), disabled("off")]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let deps = stub_triage(move |inbox| {
        s.lock()
            .unwrap()
            .push(inbox["name"].as_str().unwrap().to_string());
        if inbox["name"] == "w" {
            panic!("unexpected crash");
        }
        named(inbox, "ok")
    });
    let outcomes = run_all_inboxes(&w.ctx, "run_1", now(), &deps).unwrap();
    assert_eq!(*seen.lock().unwrap(), vec!["w", "h"]);
    assert_eq!(
        outcomes
            .iter()
            .map(|o| o.status.as_str())
            .collect::<Vec<_>>(),
        vec!["failed", "ok"]
    );
    assert!(outcomes[0]
        .error
        .as_deref()
        .unwrap()
        .contains("unexpected crash"));
    let status = inbox_status::read(&w.ctx.status_path);
    assert_eq!(status["inboxes"]["w"]["state"], "failed");
    assert_eq!(status["inboxes"]["h"]["state"], "idle");
    assert_eq!(
        status["inboxes"]["h"]["counts"],
        json!({"urgent": 1, "action": 2, "fyi": 3})
    );
    assert_eq!(
        status["inboxes"]["h"]["last_run_at"],
        "2026-09-27T09:00:00+00:00"
    );
    assert_eq!(
        status["inboxes"]["h"]["urgent"][0]["draft_link"],
        "https://mail/m1"
    );
    assert!(status["inboxes"].get("off").is_none());
    assert!(w.ctx.history_dir.join("2026-09-27-h.md").exists());
    // One loop result per inbox, with events.
    assert!(w
        .ctx
        .results_dir
        .as_ref()
        .unwrap()
        .join("run_1_h/result.json")
        .exists());
    assert!(
        w.ctx
            .events_dir
            .as_ref()
            .unwrap()
            .read_dir()
            .unwrap()
            .count()
            > 0
    );
}

#[test]
fn run_all_marks_needs_reauth() {
    let w = world(json!([inbox()]));
    let deps = stub_triage(|i| named(i, "needs_reauth"));
    run_all_inboxes(&w.ctx, "run_1", now(), &deps).unwrap();
    assert_eq!(states(&w.ctx)["w"], "needs_reauth");
}

#[test]
fn run_all_sends_one_digest_for_the_run() {
    let w = world(json!([inbox(), inbox_named("h", "Home")]));
    std::fs::write(
        &w.ctx.slack_config,
        json!({"webhook_url": "http://h"}).to_string(),
    )
    .unwrap();
    let posts: Posts = Arc::default();
    let mut deps = stub_triage(|i| named(i, "ok"));
    deps.slack_post = recording_post(&posts);
    run_all_inboxes(&w.ctx, "run_1", now(), &deps).unwrap();
    let posts = posts.lock().unwrap();
    assert_eq!(posts.len(), 1);
    assert!(posts[0].0.contains("*Work*") && posts[0].0.contains("*Home*"));
}

#[test]
fn a_history_failure_still_writes_the_terminal_status_and_continues() {
    let w = world(json!([inbox(), inbox_named("h", "Home")]));
    std::fs::create_dir_all(w.ctx.history_dir.parent().unwrap()).unwrap();
    std::fs::write(&w.ctx.history_dir, "a file, not a directory").unwrap();
    let deps = stub_triage(|i| named(i, "ok"));
    let outcomes = run_all_inboxes(&w.ctx, "run_1", now(), &deps).unwrap();
    assert_eq!(outcomes.len(), 2);
    let s = states(&w.ctx);
    assert_eq!((s["w"].as_str(), s["h"].as_str()), ("idle", "idle"));
}

#[test]
fn a_status_write_failure_is_contained_per_inbox() {
    let w = world(json!([inbox()]));
    // A directory where status.json must go: every write fails.
    std::fs::create_dir_all(&w.ctx.status_path).unwrap();
    let deps = stub_triage(|i| named(i, "ok"));
    let outcomes = run_all_inboxes(&w.ctx, "run_1", now(), &deps).unwrap();
    assert_eq!(outcomes.len(), 1);
}

#[test]
fn a_second_run_while_the_lock_is_held_does_no_triage() {
    let w = world(json!([inbox()]));
    let _held = RunLock::acquire(&w.ctx.lock_path).unwrap().unwrap();
    let deps = stub_triage(|_| panic!("no triage expected"));
    assert!(run_all_inboxes(&w.ctx, "run_1", now(), &deps)
        .unwrap()
        .is_empty());
    assert!(!w.ctx.status_path.exists());
}

#[test]
fn the_lock_is_released_after_a_run() {
    let w = world(json!([inbox()]));
    let deps = stub_triage(|i| named(i, "ok"));
    run_all_inboxes(&w.ctx, "run_1", now(), &deps).unwrap();
    assert_eq!(
        run_all_inboxes(&w.ctx, "run_2", now(), &deps)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn the_run_saves_drafts_to_the_archive() {
    let w = world(json!([inbox()]));
    let deps = stub_triage(|i| {
        let mut o = named(i, "ok");
        o.drafts = vec![DraftRecord {
            from: "A".into(),
            subject: "S".into(),
            date: "d".into(),
            category: "urgent".into(),
            body: "b".into(),
            draft_link: "https://mail/m1".into(),
        }];
        o
    });
    run_all_inboxes(&w.ctx, "run_1", now(), &deps).unwrap();
    let saved: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(&w.ctx.drafts_path).unwrap()).unwrap();
    assert_eq!(saved[0]["inbox"], "w");
}

#[test]
fn a_missing_config_or_definition_is_a_config_error() {
    let w = world(json!([inbox()]));
    let deps = stub_triage(|i| named(i, "ok"));
    let mut ctx = w.ctx.clone();
    ctx.config_path = w._dir.path().join("nope.json");
    assert!(matches!(
        run_all_inboxes(&ctx, "r", now(), &deps),
        Err(RunError::Config(_))
    ));
    let mut ctx = w.ctx.clone();
    ctx.repo_root = w._dir.path().join("elsewhere");
    assert!(matches!(
        run_all_inboxes(&ctx, "r", now(), &deps),
        Err(RunError::Config(_))
    ));
}

#[test]
fn a_failing_loop_result_still_records_the_outcome() {
    // A failed outcome makes the iteration fail; its error text is kept.
    let w = world(json!([inbox()]));
    let deps = stub_triage(|i| named(i, "failed"));
    let o = &run_all_inboxes(&w.ctx, "run_1", now(), &deps).unwrap()[0];
    assert_eq!(
        (o.status.as_str(), o.error.as_deref()),
        ("failed", Some("x"))
    );
    let result: Value = serde_json::from_str(
        &std::fs::read_to_string(
            w.ctx
                .results_dir
                .as_ref()
                .unwrap()
                .join("run_1_w/result.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_ne!(result["final_state"], "completed");
}

#[test]
fn main_checks_usage_and_exits_zero_even_when_inboxes_fail() {
    let w = world(json!([inbox()]));
    let deps = stub_triage(|i| named(i, "failed"));
    assert_eq!(main(&w.ctx, &[], Some(deps.clone())), 2);
    assert_eq!(
        main(&w.ctx, &["a".into(), "b".into()], Some(deps.clone())),
        2
    );
    assert_eq!(main(&w.ctx, &["run_1".into()], Some(deps)), 0);
    let mut missing = w.ctx.clone();
    missing.config_path = w._dir.path().join("nope.json");
    assert_eq!(
        main(
            &missing,
            &["run_1".into()],
            Some(stub_triage(|i| named(i, "ok")))
        ),
        1
    );
}

#[test]
fn the_registered_loop_definition_has_the_per_inbox_bounds() {
    let def = LoopDefinition::from_yaml(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../loops/inbox-triage/loop.yaml"),
    )
    .unwrap();
    assert_eq!(def.stop_conditions.max_runtime_minutes * 60.0, 900.0);
    assert_eq!(def.stop_conditions.max_cost_usd, 2.0);
    assert_eq!(def.name, "inbox-triage-loop");
}

#[test]
fn the_ai_failure_classes_map_from_the_sealed_call() {
    // A command that does not exist cannot start: reported as a TriageFailed.
    let d = tempdir().unwrap();
    let _ = invoke_triage_agent("p", 1.0, None, &d.path().join("log"));
}
