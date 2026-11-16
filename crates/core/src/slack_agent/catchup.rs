//! Offline catch-up: after the listener was away, offer (never auto-run) the
//! allowlisted messages that arrived meanwhile and got no reply.
//!
//! The offer is a fixed text with `[Run]` / `[Ignore]` buttons posted in the
//! original message's thread. Slack text is untrusted: the offer never echoes
//! the missed message, and everything read from history goes through `get` /
//! `as_str`, so odd JSON is skipped rather than trusted.

use super::executor::Poster;
use super::intake::{strip_mention, Request};
use super::seen::SeenEvents;
use super::web::{SlackWeb, WebError};
use crate::connectors::slack_bot::escape_mrkdwn;
use serde_json::{json, Value};
use std::collections::HashMap;

/// How far back catch-up looks, however long the listener was away.
pub const LOOKBACK_SECS: u64 = 600;
/// Conversations scanned per reconnect (the first this many the bot is in).
pub const MAX_CHANNELS: usize = 50;
/// Offers posted per reconnect; the rest stay offerable at the next one.
pub const MAX_OFFERS: usize = 10;
/// `action_id` of the Run / Ignore buttons.
pub const CATCHUP_ACTION: &str = "loopx_catchup";
/// The token-free note in the status file when the history scopes are missing.
pub const MISSING_SCOPE_NOTE: &str = "catch-up unavailable (missing scope)";

/// The Slack reads catch-up makes. Implemented by [`SlackWeb`] and by test fakes.
pub trait History {
    fn history(&self, channel: &str, oldest: &str) -> Result<Vec<Value>, WebError>;
    fn member_channels(&self) -> Result<Vec<String>, WebError>;
    /// Exactly one message (`None` when it is gone).
    fn message(&self, channel: &str, ts: &str) -> Result<Option<Value>, WebError>;
    /// A thread's messages from `oldest` on (one page).
    fn replies(&self, channel: &str, thread_ts: &str, oldest: &str)
        -> Result<Vec<Value>, WebError>;
}

impl History for SlackWeb {
    fn history(&self, channel: &str, oldest: &str) -> Result<Vec<Value>, WebError> {
        SlackWeb::history(self, channel, oldest)
    }
    fn member_channels(&self) -> Result<Vec<String>, WebError> {
        SlackWeb::member_channels(self)
    }
    fn message(&self, channel: &str, ts: &str) -> Result<Option<Value>, WebError> {
        SlackWeb::message(self, channel, ts)
    }
    fn replies(
        &self,
        channel: &str,
        thread_ts: &str,
        oldest: &str,
    ) -> Result<Vec<Value>, WebError> {
        SlackWeb::replies(self, channel, thread_ts, oldest)
    }
}

/// The fixed offer text (it never carries the missed message).
pub fn offer_text() -> &'static str {
    "I was offline when you wrote this. Run it now?"
}

/// The offer's blocks: the text and Run / Ignore buttons whose `value` is
/// `<channel>:<ts>:run|ignore` for the ORIGINAL message.
pub fn offer_blocks(channel: &str, req_ts: &str) -> Value {
    let button = |label: &str, verb: &str, style: Option<&str>| {
        let mut b = json!({
            "type": "button", "action_id": CATCHUP_ACTION,
            "text": {"type": "plain_text", "text": label},
            "value": format!("{channel}:{req_ts}:{verb}"),
        });
        if let Some(s) = style {
            b["style"] = json!(s);
        }
        b
    };
    json!([
        {"type": "section", "text": {"type": "mrkdwn", "text": escape_mrkdwn(offer_text())}},
        {"type": "actions", "elements": [
            button("Run", "run", Some("primary")),
            button("Ignore", "ignore", None),
        ]},
    ])
}

/// What replaces the offer once pressed: escaped text, no buttons.
pub fn result_blocks(text: &str) -> Value {
    json!([{"type": "section", "text": {"type": "mrkdwn", "text": escape_mrkdwn(text)}}])
}

/// `(channel, ts, run)` from a button value; strict: anything else is `None`.
pub fn parse_catchup_value(value: &str) -> Option<(String, String, bool)> {
    let mut parts = value.split(':');
    let (channel, ts, verb) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !valid_channel(channel) || parse_ts(ts).is_none() {
        return None;
    }
    let run = match verb {
        "run" => true,
        "ignore" => false,
        _ => return None,
    };
    Some((channel.to_string(), ts.to_string(), run))
}

fn valid_channel(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 32
        && c.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// `(seconds, microseconds)` of a Slack ts such as `1700000000.000100`.
fn parse_ts(ts: &str) -> Option<(u64, u64)> {
    let (s, us) = ts.split_once('.')?;
    let digits = |x: &str| !x.is_empty() && x.len() <= 12 && x.bytes().all(|b| b.is_ascii_digit());
    (digits(s) && digits(us) && us.len() <= 6).then_some(())?;
    Some((s.parse().ok()?, format!("{us:0<6}").parse().ok()?))
}

fn ts_secs(ts: &str) -> Option<f64> {
    let (s, us) = parse_ts(ts)?;
    Some(s as f64 + us as f64 / 1e6)
}

/// A ts one microsecond before `ts` (history's `oldest` is exclusive).
pub fn just_before(ts: &str) -> Option<String> {
    let (s, us) = parse_ts(ts)?;
    let total = (s * 1_000_000 + us).checked_sub(1)?;
    Some(format!("{}.{:06}", total / 1_000_000, total % 1_000_000))
}

/// The id a pressed `Run` goes through [`SeenEvents`] with (Ruling D).
pub fn event_id(channel: &str, ts: &str) -> String {
    format!("catchup:{channel}:{ts}")
}

/// The id an offer goes through [`SeenEvents`] with, so a second reconnect
/// does not offer the same message again. Distinct from [`event_id`].
pub fn offer_event_id(channel: &str, ts: &str) -> String {
    format!("catchup-offer:{channel}:{ts}")
}

/// A request from one history message, with no cutoff and no reply check:
/// the author is allowlisted and not the bot, the message is no bot or
/// subtype message, and it mentions the bot or sits in a DM (`channel` is
/// stamped on the message by the caller; DM ids start with `D`).
pub fn request_for(msg: &Value, bot_user_id: &str, allowlist: &[String]) -> Option<Request> {
    let field = |k: &str| msg.get(k).and_then(Value::as_str);
    if msg.get("bot_id").is_some_and(|v| !v.is_null())
        || msg.get("subtype").is_some_and(|v| !v.is_null())
    {
        return None;
    }
    let (user, channel, ts, raw) = (
        field("user")?,
        field("channel")?,
        field("ts")?,
        field("text")?,
    );
    if user == bot_user_id
        || !allowlist.iter().any(|u| u == user)
        || !valid_channel(channel)
        || parse_ts(ts).is_none()
    {
        return None;
    }
    let is_dm = channel.starts_with('D');
    if !is_dm && !raw.contains(&format!("<@{bot_user_id}>")) {
        return None;
    }
    let text = strip_mention(raw, bot_user_id);
    if text.is_empty() {
        return None;
    }
    let thread_ts = field("thread_ts")
        .filter(|t| parse_ts(t).is_some())
        .unwrap_or(ts);
    Some(Request {
        user: user.to_string(),
        channel: channel.to_string(),
        thread_ts: thread_ts.to_string(),
        ts: ts.to_string(),
        text,
        is_dm,
        event_id: event_id(channel, ts),
    })
}

/// The unanswered requests in `history` (messages stamped with their
/// `channel`) from the last [`LOOKBACK_SECS`].
pub fn missed_requests(history: &[Value], bot_user_id: &str, allowlist: &[String]) -> Vec<Request> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    missed_since(history, bot_user_id, allowlist, now - LOOKBACK_SECS as f64)
}

/// [`missed_requests`] with an explicit cutoff (seconds since the epoch).
pub fn missed_since(
    history: &[Value],
    bot_user_id: &str,
    allowlist: &[String],
    cutoff: f64,
) -> Vec<Request> {
    // thread root -> when the bot last wrote in it
    let mut bot_replies: HashMap<(String, String), f64> = HashMap::new();
    for msg in history {
        let field = |k: &str| msg.get(k).and_then(Value::as_str);
        let channel = field("channel").unwrap_or("").to_string();
        if field("user") == Some(bot_user_id) {
            if let (Some(root), Some(ts)) = (field("thread_ts"), field("ts").and_then(ts_secs)) {
                let e = bot_replies
                    .entry((channel.clone(), root.to_string()))
                    .or_insert(0.0);
                *e = e.max(ts);
            }
        }
        let bot_in_replies = msg
            .get("reply_users")
            .and_then(Value::as_array)
            .is_some_and(|us| us.iter().any(|u| u.as_str() == Some(bot_user_id)));
        if let (true, Some(ts)) = (bot_in_replies, field("ts")) {
            let latest = field("latest_reply")
                .and_then(ts_secs)
                .unwrap_or(f64::INFINITY);
            let e = bot_replies.entry((channel, ts.to_string())).or_insert(0.0);
            *e = e.max(latest);
        }
    }
    history
        .iter()
        .filter_map(|m| request_for(m, bot_user_id, allowlist))
        .filter(|r| ts_secs(&r.ts).is_some_and(|t| t >= cutoff))
        .filter(|r| {
            let asked = ts_secs(&r.ts).unwrap_or(0.0);
            let answered = |root: &str| {
                bot_replies
                    .get(&(r.channel.clone(), root.to_string()))
                    .is_some_and(|at| *at > asked)
            };
            !(answered(&r.thread_ts) || answered(&r.ts))
        })
        .collect()
}

/// The key every path records in [`SeenEvents`] once a message is accepted
/// (live event, or pressed offer), so a message runs at most once whichever
/// way it arrives. Distinct from Slack's `Ev...` event ids.
pub fn message_key(channel: &str, ts: &str) -> String {
    format!("msg:{channel}:{ts}")
}

/// The original message of a pressed button, fetched again (exactly one
/// message): `None` when it is gone or no longer qualifies (author, mention).
pub fn original_request(
    web: &impl History,
    channel: &str,
    ts: &str,
    bot_user_id: &str,
    allowlist: &[String],
) -> Result<Option<Request>, WebError> {
    let Some(mut m) = web.message(channel, ts)? else {
        return Ok(None);
    };
    let Some(o) = m.as_object_mut() else {
        return Ok(None);
    };
    o.insert("channel".into(), json!(channel));
    Ok(request_for(&m, bot_user_id, allowlist))
}

/// True when the bot already wrote in `req`'s thread after the message.
/// The catch-up offer is itself a bot message in that thread, so it never
/// counts: not the message `offer_ts` (the offer being pressed) and not any
/// message whose blocks carry a [`CATCHUP_ACTION`] button (an older or
/// duplicate offer).
pub fn already_answered(
    web: &impl History,
    req: &Request,
    bot_user_id: &str,
    offer_ts: &str,
) -> Result<bool, WebError> {
    let asked = ts_secs(&req.ts).unwrap_or(0.0);
    let replies = web.replies(&req.channel, &req.thread_ts, &req.ts)?;
    Ok(replies.iter().any(|m| {
        let ts = m.get("ts").and_then(Value::as_str);
        m.get("user").and_then(Value::as_str) == Some(bot_user_id)
            && ts != Some(offer_ts)
            && !m
                .get("blocks")
                .is_some_and(|b| b.to_string().contains(CATCHUP_ACTION))
            && ts.and_then(ts_secs).is_some_and(|t| t > asked)
    }))
}

/// What replaces an offer for a message that already ran or was answered.
pub const ALREADY_HANDLED: &str = "Already handled.";

/// What one catch-up pass did.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub offers: usize,
    /// The failure that ended the pass early: listing the conversations
    /// failed (a missing scope), or a rejected token or rate limit; a channel
    /// whose history fails is skipped, another failed offer is dropped.
    pub error: Option<WebError>,
}

fn is_fatal(e: &WebError) -> bool {
    matches!(
        e,
        WebError::Auth | WebError::MissingScope(_) | WebError::RateLimited { .. }
    )
}

/// Posts an offer for each unanswered allowlisted message from the last
/// [`LOOKBACK_SECS`] (as of `now`, seconds since the epoch) in up to
/// [`MAX_CHANNELS`] conversations, at most [`MAX_OFFERS`] in all.
pub fn offer_missed<W: History + Poster>(
    web: &W,
    allowlist: &[String],
    bot_user_id: &str,
    seen: &SeenEvents,
    now: f64,
) -> Outcome {
    let mut offers = 0;
    let channels = match web.member_channels() {
        Ok(c) => c,
        Err(e) => {
            return Outcome {
                offers,
                error: Some(e),
            }
        }
    };
    let cutoff = now - LOOKBACK_SECS as f64;
    let oldest = format!("{cutoff:.6}");
    for channel in channels.into_iter().take(MAX_CHANNELS) {
        if offers >= MAX_OFFERS {
            break;
        }
        // One conversation that cannot be read (a missing history scope above
        // all) is skipped, so the rest, DMs included, are still scanned. Only
        // a rejected token ends the pass (the listener then stops).
        let mut msgs = match web.history(&channel, &oldest) {
            Ok(m) => m,
            Err(WebError::Auth) => {
                return Outcome {
                    offers,
                    error: Some(WebError::Auth),
                }
            }
            Err(_) => continue,
        };
        for m in &mut msgs {
            if let Some(o) = m.as_object_mut() {
                o.insert("channel".into(), json!(channel));
            }
        }
        let mut missed = missed_since(&msgs, bot_user_id, allowlist, cutoff);
        missed.sort_by(|a, b| {
            ts_secs(&a.ts)
                .partial_cmp(&ts_secs(&b.ts))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for req in missed {
            if offers >= MAX_OFFERS {
                break;
            }
            // Already run (live or by an earlier press): nothing to offer.
            if seen.contains(&message_key(&req.channel, &req.ts)) {
                continue;
            }
            if !seen.first_time(&offer_event_id(&req.channel, &req.ts)) {
                continue;
            }
            let blocks = offer_blocks(&req.channel, &req.ts);
            match web.post_blocks(&req.channel, Some(&req.thread_ts), offer_text(), &blocks) {
                Ok(_) => offers += 1,
                Err(e) if is_fatal(&e) => {
                    return Outcome {
                        offers,
                        error: Some(e),
                    }
                }
                Err(_) => {}
            }
        }
    }
    Outcome {
        offers,
        error: None,
    }
}

#[cfg(test)]
#[path = "catchup_tests.rs"]
mod tests;
