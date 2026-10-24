//! `rss.read_feed`: entries of one of the user's configured feeds (read-only).
//! Configured feed URLs may embed secrets (userinfo, `?token=`), so they are never
//! returned: the model sees and selects feeds by a sanitized label or 1-based index.

use super::{
    connector_of, get, or_empty, untrusted_value, Arg, Env, Mode, Tool, ToolError, ToolResult,
};
use crate::connectors::Connector;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::OnceLock;

const MAX_ENTRIES: usize = 20;

/// `urllib.parse.urlsplit` for the parts used here.
struct Split<'a> {
    scheme: &'a str,
    netloc: &'a str,
    path: &'a str,
    query: &'a str,
}

fn split(url: &str) -> Split<'_> {
    let url = url.trim();
    let url = url.split('#').next().unwrap_or("");
    let (scheme, rest) = match url.find(':') {
        Some(i)
            if i > 0
                && url[..i]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) =>
        {
            (&url[..i], &url[i + 1..])
        }
        _ => ("", url),
    };
    let (netloc, rest) = match rest.strip_prefix("//") {
        Some(r) => {
            let end = r.find(['/', '?']).unwrap_or(r.len());
            (&r[..end], &r[end..])
        }
        None => ("", rest),
    };
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    Split {
        scheme,
        netloc,
        path,
        query,
    }
}

/// `(hostname, port)`: hostname lowercased, without userinfo; a malformed port is `None`.
fn host_port(netloc: &str) -> (String, Option<u16>) {
    let hostport = netloc.rsplit('@').next().unwrap_or("");
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !hostport.ends_with(']') => (h, p.parse::<u16>().ok()),
        _ => (hostport, None),
    };
    (host.trim_matches(['[', ']']).to_lowercase(), port)
}

/// `host[:port]/path` with scheme, userinfo, query and fragment removed.
pub fn feed_label(url: &str) -> String {
    let s = split(url);
    let (host, port) = host_port(s.netloc);
    let mut out = host;
    if let Some(p) = port.filter(|p| *p != 0) {
        out.push_str(&format!(":{p}"));
    }
    out.push_str(s.path);
    out.trim_end_matches('/').to_string()
}

const SECRET_NAMES: [&str; 9] = [
    "token",
    "key",
    "sig",
    "auth",
    "secret",
    "pass",
    "session",
    "credential",
    "jwt",
];

fn opaque_value() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9_-]{20,}$").expect("static regex"))
}

fn unquote_plus(s: &str) -> String {
    let bytes: Vec<u8> = s.replace('+', " ").into_bytes();
    percent_encoding::percent_decode(&bytes)
        .decode_utf8_lossy()
        .into_owned()
}

fn secret_looking(name: &str, value: &str, feed_values: &HashSet<String>) -> bool {
    let lower = name.to_lowercase();
    SECRET_NAMES.iter().any(|w| lower.contains(w))
        || (!value.is_empty() && feed_values.contains(value))
        || opaque_value().is_match(value)
}

/// Entry links are third-party. Off-host links (or any with userinfo) are
/// reduced to host+path. A same-host link is kept without its fragment and
/// without secret-looking query params, so a feed token echoed into item
/// links cannot reach the model.
pub fn entry_link(link: &str, feed_url: &str) -> String {
    let parts = split(link);
    let feed = split(feed_url);
    let (host, _) = host_port(parts.netloc);
    let (feed_host, _) = host_port(feed.netloc);
    if host.is_empty() || host != feed_host || parts.netloc.contains('@') {
        return feed_label(link);
    }
    let feed_values: HashSet<String> = feed
        .query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| unquote_plus(p.split_once('=').map_or("", |(_, v)| v)))
        .collect();
    let kept: Vec<&str> = parts
        .query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| {
            let (n, v) = p.split_once('=').unwrap_or((p, ""));
            !secret_looking(&unquote_plus(n), &unquote_plus(v), &feed_values)
        })
        .collect();
    let mut out = String::new();
    if !parts.scheme.is_empty() {
        out.push_str(parts.scheme);
        out.push(':');
    }
    out.push_str("//");
    out.push_str(parts.netloc);
    out.push_str(parts.path);
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    out
}

fn pick(feeds: &[String], wanted: &str) -> Result<usize, ToolError> {
    let labels: Vec<String> = feeds.iter().map(|f| feed_label(f)).collect();
    if wanted.is_empty() {
        return Ok(0);
    }
    if let Ok(n) = wanted.parse::<usize>() {
        if wanted.chars().all(|c| c.is_ascii_digit()) && (1..=feeds.len()).contains(&n) {
            return Ok(n - 1);
        }
    }
    if let Some(i) = labels.iter().position(|l| l == wanted) {
        return Ok(i);
    }
    Err(ToolError::input(format!(
        "feed is not a configured feed; choose one of: {}",
        labels.join(", ")
    )))
}

fn read_feed(_e: &Env, c: Option<&dyn Connector>, args: &Map<String, Value>) -> ToolResult {
    let c = connector_of(c)?;
    let feeds = c.feed_urls();
    if feeds.is_empty() {
        return Err(ToolError::input("no feeds are configured"));
    }
    let wanted = args.get("feed").map(or_empty).unwrap_or_default();
    let index = pick(&feeds, wanted.trim())?;
    let url = &feeds[index];
    let entries = c.entries(url).map_err(ToolError::from_connector)?;
    let entries: Vec<Value> = entries
        .iter()
        .take(MAX_ENTRIES)
        .map(|e| {
            json!({"title": untrusted_value(get(e, "title")),
                   "link": entry_link(&or_empty(get(e, "link")), url),
                   "published": untrusted_value(get(e, "published"))})
        })
        .collect();
    Ok(json!({"feed": feed_label(url), "index": index + 1,
              "feeds": feeds.iter().map(|f| feed_label(f)).collect::<Vec<_>>(),
              "entries": entries}))
}

static ARGS: [Arg; 1] = [Arg::opt("feed")];

pub(crate) fn tools() -> Vec<Tool> {
    vec![Tool {
        name: "rss.read_feed",
        description: "Read the latest entries of a configured RSS/Atom feed (feed: its label or 1-based index from the feeds list; default: the first feed)",
        args: &ARGS,
        mode: Mode::Read,
        connector_type: Some("rss"),
        handler: read_feed,
        select: None,
    }]
}
