//! Intake gates: decide whether a Slack `events_api` payload becomes a run.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub user: String,
    pub channel: String,
    /// The thread's root `ts`; the message's own `ts` when it is top-level.
    pub thread_ts: String,
    pub ts: String,
    pub text: String,
    pub is_dm: bool,
    pub event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Run(Request),
    Unauthorized(Request),
    Ignore,
}

pub struct Intake {
    pub allowlist: Vec<String>,
    pub bot_user_id: String,
}

impl Intake {
    /// Decide what to do with an `events_api` body. Slack text is untrusted:
    /// every field is read with `get`/`as_str`, and anything missing or
    /// oddly typed is `Ignore`.
    pub fn judge(&self, payload: &Value) -> Verdict {
        let Some(event) = payload.get("event") else {
            return Verdict::Ignore;
        };
        let field = |k: &str| event.get(k).and_then(Value::as_str);

        let is_dm = match field("type") {
            Some("app_mention") => false,
            // Channel `message` events would double-fire with `app_mention`.
            Some("message") if field("channel_type") == Some("im") => true,
            _ => return Verdict::Ignore,
        };
        if event.get("bot_id").is_some_and(|v| !v.is_null())
            || event.get("subtype").is_some_and(|v| !v.is_null())
        {
            return Verdict::Ignore;
        }
        // Real Slack always sends one; without it we cannot de-dupe.
        let Some(event_id) = payload
            .get("event_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return Verdict::Ignore;
        };
        let (Some(user), Some(channel), Some(ts), Some(raw)) =
            (field("user"), field("channel"), field("ts"), field("text"))
        else {
            return Verdict::Ignore;
        };
        if user == self.bot_user_id {
            return Verdict::Ignore;
        }
        let text = strip_mention(raw, &self.bot_user_id);
        if text.is_empty() {
            return Verdict::Ignore;
        }
        let request = Request {
            user: user.to_string(),
            channel: channel.to_string(),
            thread_ts: field("thread_ts").unwrap_or(ts).to_string(),
            ts: ts.to_string(),
            text,
            is_dm,
            event_id: event_id.to_string(),
        };
        if self.allowlist.iter().any(|u| u == user) {
            Verdict::Run(request)
        } else {
            Verdict::Unauthorized(request)
        }
    }
}

/// Remove `<@BOTID>` tokens and trim.
pub fn strip_mention(text: &str, bot_user_id: &str) -> String {
    text.replace(&format!("<@{bot_user_id}>"), "")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn intake() -> Intake {
        Intake {
            allowlist: vec!["U1".into()],
            bot_user_id: "UBOT".into(),
        }
    }

    fn mention(event: Value) -> Value {
        json!({"event_id": "Ev1", "event": event})
    }

    fn base() -> Value {
        json!({"type":"app_mention","user":"U1","text":"<@UBOT> run issue loop","channel":"C1","ts":"1.1"})
    }

    #[test]
    fn allowlisted_mention_runs() {
        match intake().judge(&mention(base())) {
            Verdict::Run(r) => {
                assert_eq!(r.text, "run issue loop");
                assert_eq!(r.thread_ts, "1.1");
                assert_eq!(r.ts, "1.1");
                assert_eq!(r.user, "U1");
                assert_eq!(r.channel, "C1");
                assert_eq!(r.event_id, "Ev1");
                assert!(!r.is_dm);
            }
            v => panic!("expected Run, got {v:?}"),
        }
    }

    #[test]
    fn thread_ts_wins_when_present() {
        let mut e = base();
        e["thread_ts"] = json!("0.5");
        match intake().judge(&mention(e)) {
            Verdict::Run(r) => assert_eq!(r.thread_ts, "0.5"),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn dm_runs() {
        let e = json!({"type":"message","channel_type":"im","user":"U1","text":"hello","channel":"D1","ts":"2.2"});
        match intake().judge(&mention(e)) {
            Verdict::Run(r) => {
                assert!(r.is_dm);
                assert_eq!(r.text, "hello");
            }
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn bots_and_self_are_ignored() {
        let mut e = base();
        e["bot_id"] = json!("B1");
        assert_eq!(intake().judge(&mention(e)), Verdict::Ignore);
        let mut e = base();
        e["subtype"] = json!("bot_message");
        assert_eq!(intake().judge(&mention(e)), Verdict::Ignore);
        let mut e = base();
        e["user"] = json!("UBOT");
        assert_eq!(intake().judge(&mention(e)), Verdict::Ignore);
    }

    #[test]
    fn non_allowlisted_is_unauthorized() {
        let mut e = base();
        e["user"] = json!("U9");
        assert!(matches!(
            intake().judge(&mention(e)),
            Verdict::Unauthorized(r) if r.user == "U9"
        ));
    }

    #[test]
    fn empty_or_mention_only_text_is_ignored() {
        for text in ["", "   ", "<@UBOT>", "<@UBOT>  "] {
            let mut e = base();
            e["text"] = json!(text);
            assert_eq!(intake().judge(&mention(e)), Verdict::Ignore, "{text:?}");
        }
    }

    #[test]
    fn other_subtypes_and_channel_messages_are_ignored() {
        let mut e = base();
        e["type"] = json!("message");
        e["subtype"] = json!("message_changed");
        e["channel_type"] = json!("im");
        assert_eq!(intake().judge(&mention(e)), Verdict::Ignore);
        let e = json!({"type":"message","channel_type":"channel","user":"U1","text":"hi","channel":"C1","ts":"3.3"});
        assert_eq!(intake().judge(&mention(e)), Verdict::Ignore);
        let e = json!({"type":"reaction_added","user":"U1"});
        assert_eq!(intake().judge(&mention(e)), Verdict::Ignore);
    }

    #[test]
    fn missing_or_bad_event_id_is_ignored() {
        let ok = |id: Option<Value>| {
            let mut p = json!({"event": base()});
            if let Some(id) = id {
                p["event_id"] = id;
            }
            p
        };
        // Sanity: the same payload with a good id runs.
        assert!(matches!(
            intake().judge(&ok(Some(json!("Ev1")))),
            Verdict::Run(_)
        ));
        for id in [None, Some(json!(3)), Some(json!("")), Some(json!(null))] {
            assert_eq!(intake().judge(&ok(id.clone())), Verdict::Ignore, "{id:?}");
        }
    }

    #[test]
    fn odd_payloads_never_panic() {
        for p in [
            json!(null),
            json!({}),
            json!({"event": 5}),
            json!({"event": {"type": "app_mention"}}),
            json!({"event": {"type": "app_mention", "user": 7, "text": ["x"], "channel": null, "ts": 1}}),
            json!({"event_id": 3, "event": {"type": "app_mention", "user": "U1", "text": "x"}}),
        ] {
            assert_eq!(intake().judge(&p), Verdict::Ignore, "{p}");
        }
    }

    #[test]
    fn strip_mention_removes_only_the_bot_tokens() {
        assert_eq!(
            strip_mention("<@UBOT> hi <@UBOT> there", "UBOT"),
            "hi  there"
        );
        assert_eq!(strip_mention("<@UOTHER> hi", "UBOT"), "<@UOTHER> hi");
        assert_eq!(strip_mention("  <@UBOT>  ", "UBOT"), "");
    }
}
