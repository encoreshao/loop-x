use super::*;
use std::sync::Mutex;

const BOT: &str = "UBOT";
const NOW: f64 = 1_700_000_000.0;

fn allow() -> Vec<String> {
    vec!["U1".to_string()]
}

fn msg(channel: &str, user: &str, ts: f64, text: &str) -> Value {
    json!({"channel": channel, "user": user, "ts": format!("{ts:.6}"), "text": text})
}

fn mention(channel: &str, ts: f64) -> Value {
    msg(channel, "U1", ts, "<@UBOT> check the build")
}

fn missed(history: &[Value]) -> Vec<Request> {
    missed_since(history, BOT, &allow(), NOW - LOOKBACK_SECS as f64)
}

#[test]
fn an_allowlisted_mention_with_no_reply_is_returned() {
    let r = missed(&[mention("C1", NOW - 60.0)]);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].user, "U1");
    assert_eq!(r[0].channel, "C1");
    assert_eq!(r[0].text, "check the build");
    assert_eq!(r[0].thread_ts, r[0].ts);
    assert!(!r[0].is_dm);
    assert_eq!(r[0].event_id, format!("catchup:C1:{}", r[0].ts));
}

#[test]
fn a_dm_needs_no_mention() {
    let r = missed(&[msg("D1", "U1", NOW - 60.0, "what is up")]);
    assert_eq!(r.len(), 1);
    assert!(r[0].is_dm);
    assert_eq!(r[0].text, "what is up");
}

#[test]
fn a_channel_message_without_a_mention_is_not_returned() {
    assert!(missed(&[msg("C1", "U1", NOW - 60.0, "lunch?")]).is_empty());
}

#[test]
fn a_bot_reply_in_the_thread_removes_it() {
    let asked = mention("C1", NOW - 120.0);
    let ts = asked["ts"].as_str().unwrap().to_string();
    let mut reply = msg("C1", BOT, NOW - 100.0, "on it");
    reply["thread_ts"] = json!(ts);
    assert!(missed(&[asked.clone(), reply]).is_empty());
    // Slack lists a replied-to parent with reply_users instead.
    let mut parent = asked.clone();
    parent["reply_users"] = json!(["U7", BOT]);
    parent["latest_reply"] = json!(format!("{:.6}", NOW - 90.0));
    assert!(missed(&[parent]).is_empty());
}

#[test]
fn a_bot_reply_older_than_the_message_does_not_answer_it() {
    let mut first = msg("C1", "U1", NOW - 300.0, "<@UBOT> one");
    first["thread_ts"] = json!(format!("{:.6}", NOW - 300.0));
    let mut bot = msg("C1", BOT, NOW - 290.0, "done");
    bot["thread_ts"] = json!(format!("{:.6}", NOW - 300.0));
    let mut later = msg("C1", "U1", NOW - 60.0, "<@UBOT> two");
    later["thread_ts"] = json!(format!("{:.6}", NOW - 300.0));
    let r = missed(&[first, bot, later]);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].text, "two");
    assert_eq!(r[0].thread_ts, format!("{:.6}", NOW - 300.0));
}

#[test]
fn a_reply_in_another_channel_does_not_answer_it() {
    let asked = mention("C1", NOW - 120.0);
    let mut reply = msg("C2", BOT, NOW - 100.0, "on it");
    reply["thread_ts"] = asked["ts"].clone();
    assert_eq!(missed(&[asked, reply]).len(), 1);
}

#[test]
fn a_user_off_the_allowlist_is_not_returned() {
    assert!(missed(&[msg("C1", "U9", NOW - 60.0, "<@UBOT> hi")]).is_empty());
}

#[test]
fn a_message_older_than_the_cutoff_is_not_returned() {
    let old = mention("C1", NOW - LOOKBACK_SECS as f64 - 5.0);
    assert!(missed(&[old]).is_empty());
    assert_eq!(
        missed(&[mention("C1", NOW - LOOKBACK_SECS as f64 + 5.0)]).len(),
        1
    );
}

#[test]
fn the_bots_own_messages_and_bot_or_subtype_messages_are_not_returned() {
    let own = msg("C1", BOT, NOW - 60.0, "<@UBOT> self");
    let mut other_bot = mention("C1", NOW - 60.0);
    other_bot["bot_id"] = json!("B1");
    let mut edited = mention("C1", NOW - 60.0);
    edited["subtype"] = json!("channel_join");
    assert!(missed(&[own, other_bot, edited]).is_empty());
    // A bot allowlisted by mistake still never answers itself.
    let r = missed_since(
        &[msg("C1", BOT, NOW - 60.0, "<@UBOT> self")],
        BOT,
        &[BOT.to_string()],
        0.0,
    );
    assert!(r.is_empty());
}

#[test]
fn hostile_json_never_panics_and_is_skipped() {
    let hostile = vec![
        Value::Null,
        json!(42),
        json!("text"),
        json!([1, 2]),
        json!({}),
        json!({"user": 1, "ts": 2, "text": 3, "channel": 4}),
        json!({"user": "U1", "ts": "not-a-ts", "text": "<@UBOT> x", "channel": "C1"}),
        json!({"user": "U1", "ts": "1e9.5", "text": "<@UBOT> x", "channel": "C1"}),
        json!({"user": "U1", "ts": "-5.0", "text": "<@UBOT> x", "channel": "C1"}),
        json!({"user": "U1", "ts": "1700000000.5", "text": "<@UBOT> x", "channel": "c1:evil"}),
        json!({"user": "U1", "ts": "1700000000.5", "text": "<@UBOT>", "channel": "C1"}),
        json!({"user": "U1", "ts": format!("{:.6}", NOW - 5.0), "text": "<@UBOT> x",
               "channel": "C1", "reply_users": "x", "thread_ts": 7, "bot_id": null}),
    ];
    let r = missed(&hostile);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].thread_ts, r[0].ts, "an odd thread_ts falls back to ts");
}

#[test]
fn the_offer_text_is_fixed_and_the_blocks_carry_two_parseable_buttons() {
    assert_eq!(
        offer_text(),
        "I was offline when you wrote this. Run it now?"
    );
    let blocks = offer_blocks("C1", "1700000000.000100");
    let elements = blocks[1]["elements"].as_array().unwrap();
    assert_eq!(elements.len(), 2);
    let parsed: Vec<_> = elements
        .iter()
        .map(|e| {
            assert_eq!(e["action_id"], CATCHUP_ACTION);
            parse_catchup_value(e["value"].as_str().unwrap()).unwrap()
        })
        .collect();
    assert_eq!(
        parsed,
        vec![
            ("C1".to_string(), "1700000000.000100".to_string(), true),
            ("C1".to_string(), "1700000000.000100".to_string(), false)
        ]
    );
    assert_eq!(blocks[0]["text"]["text"], offer_text());
}

#[test]
fn parse_catchup_value_is_strict() {
    assert_eq!(
        parse_catchup_value("D9X:1.5:run"),
        Some(("D9X".into(), "1.5".into(), true))
    );
    for bad in [
        "",
        "C1",
        "C1:1.5",
        "C1:1.5:go",
        "C1:1.5:run:extra",
        ":1.5:run",
        "c1:1.5:run",
        "C1::run",
        "C1:15:run",
        "C1:1.5.5:run",
        "C1:1.-5:run",
        "C1:1.5:RUN",
        "C1: 1.5:run",
        "C1:1.5:run\n",
        "C1:1.1234567:run",
    ] {
        assert_eq!(parse_catchup_value(bad), None, "{bad:?}");
    }
}

#[test]
fn escaping_and_ts_helpers() {
    assert_eq!(
        escape_mrkdwn("<!channel> & <@U1>"),
        "&lt;!channel&gt; &amp; &lt;@U1&gt;"
    );
    assert_eq!(
        just_before("1700000000.000100").as_deref(),
        Some("1700000000.000099")
    );
    assert_eq!(
        just_before("1700000001.000000").as_deref(),
        Some("1700000000.999999")
    );
    assert_eq!(
        just_before("1700000000.5").as_deref(),
        Some("1700000000.499999")
    );
    assert_eq!(just_before("junk"), None);
    assert_eq!(just_before("0.000000"), None);
    let r = result_blocks("<!here> Ignored");
    assert_eq!(r[0]["text"]["text"], "&lt;!here&gt; Ignored");
    assert_eq!(r.as_array().unwrap().len(), 1);
}

// ----- offer_missed -----

/// `(channel, thread_ts, text, blocks)` of a posted offer.
type Posted = (String, Option<String>, String, Value);

#[derive(Default)]
struct Fake {
    channels: Vec<String>,
    messages: HashMap<String, Vec<Value>>,
    history_errors: HashMap<String, WebError>,
    /// thread root -> its replies
    replies: HashMap<String, Vec<Value>>,
    message_calls: Mutex<Vec<(String, String)>>,
    channels_error: Option<WebError>,
    post_error: Option<WebError>,
    history_calls: Mutex<Vec<(String, String)>>,
    posts: Mutex<Vec<Posted>>,
}

impl History for Fake {
    fn history(&self, channel: &str, oldest: &str) -> Result<Vec<Value>, WebError> {
        self.history_calls
            .lock()
            .unwrap()
            .push((channel.into(), oldest.into()));
        if let Some(e) = self.history_errors.get(channel) {
            return Err(e.clone());
        }
        Ok(self.messages.get(channel).cloned().unwrap_or_default())
    }
    fn message(&self, channel: &str, ts: &str) -> Result<Option<Value>, WebError> {
        self.message_calls
            .lock()
            .unwrap()
            .push((channel.into(), ts.into()));
        if let Some(e) = self.history_errors.get(channel) {
            return Err(e.clone());
        }
        Ok(self
            .messages
            .get(channel)
            .and_then(|m| m.iter().find(|m| m["ts"] == ts))
            .cloned())
    }
    fn replies(&self, _: &str, thread_ts: &str, _: &str) -> Result<Vec<Value>, WebError> {
        Ok(self.replies.get(thread_ts).cloned().unwrap_or_default())
    }
    fn member_channels(&self) -> Result<Vec<String>, WebError> {
        match &self.channels_error {
            Some(e) => Err(e.clone()),
            None => Ok(self.channels.clone()),
        }
    }
}

impl Poster for Fake {
    fn post(&self, _: &str, _: Option<&str>, _: &str) -> Result<String, WebError> {
        unreachable!()
    }
    fn post_blocks(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
        blocks: &Value,
    ) -> Result<String, WebError> {
        if let Some(e) = &self.post_error {
            return Err(e.clone());
        }
        self.posts.lock().unwrap().push((
            channel.into(),
            thread_ts.map(str::to_string),
            text.into(),
            blocks.clone(),
        ));
        Ok("900.1".into())
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

fn seen(d: &tempfile::TempDir) -> SeenEvents {
    SeenEvents::new(d.path().join("seen.json"))
}

fn raw(message: Value) -> Value {
    // History messages carry no `channel`; offer_missed stamps it.
    let mut m = message;
    m.as_object_mut().unwrap().remove("channel");
    m
}

#[test]
fn one_missed_mention_gets_one_in_thread_offer_and_never_its_text() {
    let d = tempfile::tempdir().unwrap();
    let mut f = Fake {
        channels: vec!["C1".into()],
        ..Fake::default()
    };
    f.messages
        .insert("C1".into(), vec![raw(mention("C1", NOW - 60.0))]);
    let out = offer_missed(&f, &allow(), BOT, &seen(&d), NOW);
    assert_eq!(
        out,
        Outcome {
            offers: 1,
            error: None
        }
    );
    let posts = f.posts.lock().unwrap();
    assert_eq!(posts.len(), 1);
    let (channel, thread, text, blocks) = &posts[0];
    assert_eq!(channel, "C1");
    assert_eq!(
        thread.as_deref(),
        Some(format!("{:.6}", NOW - 60.0).as_str())
    );
    assert_eq!(text, offer_text());
    assert!(!blocks.to_string().contains("check the build"));
    let calls = f.history_calls.lock().unwrap();
    assert_eq!(
        calls[0].1,
        format!("{:.6}", NOW - 600.0),
        "looks back exactly the window"
    );
}

#[test]
fn the_same_message_is_not_offered_again_at_the_next_reconnect() {
    let d = tempfile::tempdir().unwrap();
    let mut f = Fake {
        channels: vec!["C1".into()],
        ..Fake::default()
    };
    f.messages
        .insert("C1".into(), vec![raw(mention("C1", NOW - 60.0))]);
    let s = seen(&d);
    assert_eq!(offer_missed(&f, &allow(), BOT, &s, NOW).offers, 1);
    assert_eq!(offer_missed(&f, &allow(), BOT, &s, NOW + 5.0).offers, 0);
    assert_eq!(f.posts.lock().unwrap().len(), 1);
    // The offer is not the run's event id: a later press still runs.
    assert!(s.first_time(&event_id("C1", &format!("{:.6}", NOW - 60.0))));
}

#[test]
fn at_most_max_channels_are_scanned() {
    let d = tempfile::tempdir().unwrap();
    let f = Fake {
        channels: (0..MAX_CHANNELS + 20).map(|i| format!("C{i}")).collect(),
        ..Fake::default()
    };
    offer_missed(&f, &allow(), BOT, &seen(&d), NOW);
    let calls = f.history_calls.lock().unwrap();
    assert_eq!(calls.len(), MAX_CHANNELS);
    assert_eq!(calls.last().unwrap().0, format!("C{}", MAX_CHANNELS - 1));
}

#[test]
fn at_most_max_offers_are_posted_and_the_rest_stay_offerable() {
    let d = tempfile::tempdir().unwrap();
    let mut f = Fake {
        channels: vec!["C1".into(), "C2".into()],
        ..Fake::default()
    };
    let many: Vec<Value> = (0..MAX_OFFERS + 4)
        .map(|i| raw(mention("C1", NOW - 500.0 + i as f64)))
        .collect();
    f.messages.insert("C1".into(), many);
    f.messages
        .insert("C2".into(), vec![raw(mention("C2", NOW - 10.0))]);
    let s = seen(&d);
    let out = offer_missed(&f, &allow(), BOT, &s, NOW);
    assert_eq!(out.offers, MAX_OFFERS);
    assert_eq!(f.posts.lock().unwrap().len(), MAX_OFFERS);
    assert_eq!(
        f.history_calls.lock().unwrap().len(),
        1,
        "C2 is never read once capped"
    );
    // The next reconnect picks up the four that did not fit, oldest first.
    let out = offer_missed(&f, &allow(), BOT, &s, NOW);
    assert_eq!(out.offers, 5);
}

#[test]
fn a_missing_scope_listing_conversations_ends_the_pass_with_that_error() {
    let d = tempfile::tempdir().unwrap();
    let f = Fake {
        channels_error: Some(WebError::MissingScope("channels:read".into())),
        ..Fake::default()
    };
    let out = offer_missed(&f, &allow(), BOT, &seen(&d), NOW);
    assert_eq!(
        out.error,
        Some(WebError::MissingScope("channels:read".into()))
    );
}

#[test]
fn a_channel_whose_history_fails_is_skipped_and_dms_are_still_scanned() {
    for err in [
        WebError::MissingScope("channels:history".into()),
        WebError::RateLimited { retry_after: 5 },
    ] {
        let d = tempfile::tempdir().unwrap();
        let mut f = Fake {
            channels: vec!["C1".into(), "D2".into()],
            ..Fake::default()
        };
        f.history_errors.insert("C1".into(), err.clone());
        f.messages
            .insert("D2".into(), vec![raw(mention("D2", NOW - 10.0))]);
        let out = offer_missed(&f, &allow(), BOT, &seen(&d), NOW);
        assert_eq!(
            out,
            Outcome {
                offers: 1,
                error: None
            },
            "{err:?}"
        );
    }
}

#[test]
fn other_failures_skip_that_channel_and_go_on() {
    let d = tempfile::tempdir().unwrap();
    let mut f = Fake {
        channels: vec!["C1".into(), "C2".into()],
        ..Fake::default()
    };
    f.history_errors.insert("C1".into(), WebError::Net);
    f.messages
        .insert("C2".into(), vec![raw(mention("C2", NOW - 10.0))]);
    let out = offer_missed(&f, &allow(), BOT, &seen(&d), NOW);
    assert_eq!(
        out,
        Outcome {
            offers: 1,
            error: None
        }
    );
    let mut f = Fake {
        channels: vec!["C1".into()],
        ..Fake::default()
    };
    f.history_errors
        .insert("C1".into(), WebError::Api("channel_not_found".into()));
    assert_eq!(offer_missed(&f, &allow(), BOT, &seen(&d), NOW).error, None);
}

#[test]
fn a_rejected_token_or_a_rate_limit_ends_the_pass() {
    let d = tempfile::tempdir().unwrap();
    let mut f = Fake {
        channels: vec!["C1".into(), "C2".into()],
        ..Fake::default()
    };
    f.history_errors.insert("C1".into(), WebError::Auth);
    assert_eq!(
        offer_missed(&f, &allow(), BOT, &seen(&d), NOW).error,
        Some(WebError::Auth)
    );
    assert_eq!(f.history_calls.lock().unwrap().len(), 1);
    let mut f = Fake {
        channels: vec!["C1".into()],
        ..Fake::default()
    };
    f.messages
        .insert("C1".into(), vec![raw(mention("C1", NOW - 10.0))]);
    f.post_error = Some(WebError::RateLimited { retry_after: 3 });
    let out = offer_missed(&f, &allow(), BOT, &seen(&d), NOW);
    assert_eq!(out.error, Some(WebError::RateLimited { retry_after: 3 }));
}

#[test]
fn original_request_refetches_exactly_one_message() {
    let mut f = Fake::default();
    let target = raw(mention("C1", NOW - 60.0));
    let ts = target["ts"].as_str().unwrap().to_string();
    f.messages
        .insert("C1".into(), vec![raw(mention("C1", NOW - 30.0)), target]);
    let r = original_request(&f, "C1", &ts, BOT, &allow())
        .unwrap()
        .unwrap();
    assert_eq!(r.ts, ts);
    assert_eq!(r.event_id, event_id("C1", &ts));
    assert_eq!(
        f.message_calls.lock().unwrap().clone(),
        vec![("C1".to_string(), ts.clone())]
    );
    assert!(
        f.history_calls.lock().unwrap().is_empty(),
        "no paged history read"
    );
    // Gone, or its author is no longer allowed: nothing to run.
    assert_eq!(
        original_request(&f, "C1", "1.000000", BOT, &allow()).unwrap(),
        None
    );
    assert_eq!(
        original_request(&f, "C1", &ts, BOT, &["U2".to_string()]).unwrap(),
        None
    );
    f.history_errors.insert("C1".into(), WebError::Net);
    assert_eq!(
        original_request(&f, "C1", &ts, BOT, &allow()),
        Err(WebError::Net)
    );
}

#[test]
fn the_message_key_is_one_per_channel_and_ts() {
    assert_eq!(message_key("C1", "1.5"), "msg:C1:1.5");
    assert_ne!(message_key("C1", "1.5"), event_id("C1", "1.5"));
}

#[test]
fn a_message_whose_key_is_seen_is_not_offered() {
    let d = tempfile::tempdir().unwrap();
    let f = Fake {
        channels: vec!["C1".into()],
        messages: HashMap::from([(
            "C1".to_string(),
            vec![
                raw(mention("C1", NOW - 60.0)),
                raw(mention("C1", NOW - 50.0)),
            ],
        )]),
        ..Fake::default()
    };
    let s = seen(&d);
    assert!(s.first_time(&message_key("C1", &format!("{:.6}", NOW - 60.0))));
    let out = offer_missed(&f, &allow(), BOT, &s, NOW);
    assert_eq!(out.offers, 1);
    assert_eq!(f.posts.lock().unwrap()[0].0, "C1");
    assert_eq!(
        f.posts.lock().unwrap()[0].1.as_deref(),
        Some(format!("{:.6}", NOW - 50.0).as_str())
    );
}

const OFFER_TS: &str = "1699999950.000000"; // NOW - 50 s

/// The bot's own catch-up offer as `conversations.replies` returns it.
fn offer_msg(thread_ts: &str, ts: &str) -> Value {
    json!({"user": BOT, "ts": ts, "thread_ts": thread_ts, "text": offer_text(),
           "blocks": offer_blocks("C1", thread_ts)})
}

fn ask_and_req() -> (Value, Request) {
    let asked = raw(mention("C1", NOW - 60.0));
    let mut m = asked.clone();
    m["channel"] = json!("C1");
    (asked, request_for(&m, BOT, &allow()).unwrap())
}

#[test]
fn the_offer_itself_is_not_an_answer() {
    let (asked, req) = ask_and_req();
    let ts = req.ts.clone();
    let mut f = Fake::default();
    f.replies
        .insert(ts.clone(), vec![asked.clone(), offer_msg(&ts, OFFER_TS)]);
    assert!(
        !already_answered(&f, &req, BOT, OFFER_TS).unwrap(),
        "by its ts"
    );
    // An older or duplicate offer (another ts) is recognised by its buttons.
    f.replies.insert(
        ts.clone(),
        vec![
            asked.clone(),
            offer_msg(&ts, "1699999960.000000"),
            offer_msg(&ts, OFFER_TS),
        ],
    );
    assert!(
        !already_answered(&f, &req, BOT, OFFER_TS).unwrap(),
        "by its blocks"
    );
    // Even an offer whose buttons were already replaced by text is skipped by ts.
    let mut plain = offer_msg(&ts, OFFER_TS);
    plain.as_object_mut().unwrap().remove("blocks");
    f.replies.insert(ts, vec![asked, plain]);
    assert!(!already_answered(&f, &req, BOT, OFFER_TS).unwrap());
}

#[test]
fn a_genuine_answer_counts_even_next_to_the_offer() {
    let (asked, req) = ask_and_req();
    let ts = req.ts.clone();
    let mut f = Fake::default();
    f.replies.insert(
        ts.clone(),
        vec![
            asked,
            offer_msg(&ts, OFFER_TS),
            raw(msg("C1", BOT, NOW - 40.0, "done")),
        ],
    );
    assert!(already_answered(&f, &req, BOT, OFFER_TS).unwrap());
}

#[test]
fn odd_reply_json_never_panics_or_counts() {
    let (asked, req) = ask_and_req();
    let ts = req.ts.clone();
    let mut f = Fake::default();
    f.replies.insert(
        ts,
        vec![
            asked,
            json!(7),
            Value::Null,
            json!("x"),
            json!({"user": BOT}),
            json!({"user": BOT, "ts": 5}),
            json!({"user": BOT, "ts": "junk"}),
            json!({"user": 9, "ts": "1699999990.000000"}),
            json!({"user": BOT, "ts": "1699999990.000000", "blocks": 3}),
        ],
    );
    // The last one is a bot message after the ask with odd blocks: genuine.
    assert!(already_answered(&f, &req, BOT, OFFER_TS).unwrap());
    f.replies.get_mut(&req.ts).unwrap().pop();
    assert!(!already_answered(&f, &req, BOT, OFFER_TS).unwrap());
}

#[test]
fn already_answered_needs_a_bot_message_after_the_ask_in_that_thread() {
    let asked = raw(mention("C1", NOW - 60.0));
    let ts = asked["ts"].as_str().unwrap().to_string();
    let req = request_for(
        &{
            let mut m = asked.clone();
            m["channel"] = json!("C1");
            m
        },
        BOT,
        &allow(),
    )
    .unwrap();
    let mut f = Fake::default();
    assert!(!already_answered(&f, &req, BOT, OFFER_TS).unwrap());
    f.replies.insert(
        ts.clone(),
        vec![asked.clone(), raw(msg("C1", "U1", NOW - 40.0, "thanks"))],
    );
    assert!(
        !already_answered(&f, &req, BOT, OFFER_TS).unwrap(),
        "a human reply is not an answer"
    );
    f.replies.insert(
        ts.clone(),
        vec![asked.clone(), raw(msg("C1", BOT, NOW - 70.0, "old"))],
    );
    assert!(
        !already_answered(&f, &req, BOT, OFFER_TS).unwrap(),
        "older than the ask"
    );
    f.replies.insert(
        ts,
        vec![
            asked,
            raw(msg("C1", BOT, NOW - 40.0, "done")),
            json!(7),
            Value::Null,
        ],
    );
    assert!(already_answered(&f, &req, BOT, OFFER_TS).unwrap());
}
