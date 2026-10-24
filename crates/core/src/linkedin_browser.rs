//! LinkedIn page reading (mirrors `bin/linkedin_browser.py`): pure HTML
//! parsers, challenge detection and the [`Session`] that fetches pages through
//! `linkedin_humanize::HumanPage` over Chrome (see `linkedin_cdp` for how
//! Chrome is driven and why). This is the only module that knows LinkedIn's
//! page structure. It never navigates, scrolls, clicks or types by itself:
//! every such action goes through `HumanPage`, so it is always paced.

use crate::connectors::linkedin::{LinkedInProbe, ProbeFailure};
use crate::linkedin_cdp::{Chrome, CdpPage, LaunchOptions};
use crate::linkedin_extract;
use crate::linkedin_humanize::{HumanPage, Humanizer, Selector};
use crate::linkedin_session;
use crate::loop_plugins::linkedin_autopilot::{
    register_session_factory, BrowserError, LinkedInSession,
};
use crate::loopkit::sha1_hex;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

pub const LINKEDIN: &str = "https://www.linkedin.com";
pub const MAX_TEXT: usize = 400;
pub const MAX_SEARCH_PAGES: usize = 5;
const VOID: [&str; 12] = [
    "br", "img", "input", "meta", "link", "hr", "wbr", "source", "area", "col", "embed", "track",
];
const LOGIN_PATHS: [&str; 4] = ["/login", "/uas/", "/authwall", "/signup"];
const CHALLENGE_WORDS: [&str; 5] = [
    "captcha",
    "security verification",
    "quick security check",
    "unusual activity",
    "restricted your account",
];
pub const SEARCH_BOX: &str = "input.search-global-typeahead__input, input[aria-label*='Search' i], input[placeholder*='Search' i]";

// ---- a forgiving HTML scanner (the role of Python's html.parser) ---------------------------

#[derive(Debug, Clone)]
pub struct Node {
    seq: usize,
    pub attrs: HashMap<String, String>,
    pub hrefs: Vec<String>,
    pub text: String,
}

struct Open {
    seq: usize,
    level: usize,
    attrs: HashMap<String, String>,
    text: String,
    hrefs: Vec<String>,
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let decoded = rest[1..].find(';').filter(|&e| e <= 10).and_then(|e| {
            let name = &rest[1..1 + e];
            let ch = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                _ => name.strip_prefix('#').and_then(|n| {
                    let code = match n.strip_prefix(['x', 'X']) {
                        Some(h) => u32::from_str_radix(h, 16).ok(),
                        None => n.parse().ok(),
                    };
                    code.and_then(char::from_u32)
                }),
            };
            ch.map(|c| (c, e + 2))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

struct Collector<'a> {
    matcher: &'a dyn Fn(&str, &HashMap<String, String>) -> bool,
    found: Vec<Node>,
    open: Vec<Open>,
    stack: Vec<String>,
    seq: usize,
    r#where: HashMap<String, Vec<usize>>,
}

impl Collector<'_> {
    fn start(&mut self, tag: &str, attrs: HashMap<String, String>) {
        if tag == "a" {
            if let Some(h) = attrs.get("href").filter(|h| !h.is_empty()) {
                for node in &mut self.open {
                    node.hrefs.push(h.clone());
                }
            }
        }
        if VOID.contains(&tag) {
            return;
        }
        self.stack.push(tag.to_string());
        self.r#where
            .entry(tag.to_string())
            .or_default()
            .push(self.stack.len() - 1);
        if (self.matcher)(tag, &attrs) {
            self.seq += 1;
            self.open.push(Open {
                seq: self.seq,
                level: self.stack.len(),
                attrs,
                text: String::new(),
                hrefs: Vec::new(),
            });
        }
    }

    fn end(&mut self, tag: &str) {
        if VOID.contains(&tag) {
            return;
        }
        let Some(&index) = self.r#where.get(tag).and_then(|v| v.last()) else {
            return; // stray end tag: ignore, never shift nesting
        };
        while self.open.last().is_some_and(|n| n.level > index) {
            let n = self.open.pop().expect("checked");
            self.found.push(Self::done(n));
        }
        while self.stack.len() > index {
            let t = self.stack.pop().expect("non-empty");
            if let Some(v) = self.r#where.get_mut(&t) {
                v.pop();
            }
        }
    }

    fn data(&mut self, text: &str) {
        for node in &mut self.open {
            node.text.push_str(text);
        }
    }

    fn done(n: Open) -> Node {
        Node {
            seq: n.seq,
            attrs: n.attrs,
            hrefs: n.hrefs,
            text: collapse(&n.text),
        }
    }
}

/// Parse one start tag at `s[0] == '<'`: `(name, attrs, self_closing, bytes consumed)`.
fn parse_start_tag(s: &str) -> Option<(String, HashMap<String, String>, bool, usize)> {
    let b = s.as_bytes();
    let mut i = 1;
    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'/' && b[i] != b'>' {
        i += 1;
    }
    let name = s[1..i].to_ascii_lowercase();
    let mut attrs = HashMap::new();
    let mut self_closing = false;
    loop {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            self_closing = b[i] == b'/';
            i += 1;
        }
        if i >= b.len() {
            return None; // unterminated tag: treated as text by the caller
        }
        if b[i] == b'>' {
            return Some((name, attrs, self_closing, i + 1));
        }
        self_closing = false;
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'>' && b[i] != b'/' {
            i += 1;
        }
        let key = s[start..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let vs = i + 1;
                let end = b[vs..].iter().position(|&c| c == q).map(|p| vs + p)?;
                value = decode_entities(&s[vs..end]);
                i = end + 1;
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                    i += 1;
                }
                value = decode_entities(&s[vs..i]);
            }
        }
        if !key.is_empty() {
            attrs.insert(key, value);
        }
    }
}

/// Every element `matcher(tag, attrs)` accepts, in document order (outermost
/// first), with its collapsed inner text and the hrefs inside it. Broken
/// markup never panics; unclosed elements still count.
pub fn extract_nodes(
    html: &str,
    matcher: &dyn Fn(&str, &HashMap<String, String>) -> bool,
) -> Vec<Node> {
    let mut c = Collector {
        matcher,
        found: Vec::new(),
        open: Vec::new(),
        stack: Vec::new(),
        seq: 0,
        r#where: HashMap::new(),
    };
    let mut rest = html;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            c.data(&decode_entities(rest));
            break;
        };
        if lt > 0 {
            c.data(&decode_entities(&rest[..lt]));
            rest = &rest[lt..];
        }
        let next = rest.as_bytes().get(1).copied();
        if rest.starts_with("<!--") {
            rest = rest[4..].find("-->").map_or("", |e| &rest[4 + e + 3..]);
        } else if matches!(next, Some(b'!' | b'?')) {
            rest = rest.find('>').map_or("", |e| &rest[e + 1..]);
        } else if next == Some(b'/') && rest.as_bytes().get(2).is_some_and(u8::is_ascii_alphabetic) {
            let Some(e) = rest.find('>') else {
                break;
            };
            let name: String = rest[2..e]
                .chars()
                .take_while(|ch| !ch.is_whitespace() && *ch != '/')
                .collect();
            c.end(&name.to_ascii_lowercase());
            rest = &rest[e + 1..];
        } else if next.is_some_and(|b| b.is_ascii_alphabetic()) {
            match parse_start_tag(rest) {
                Some((name, attrs, self_closing, used)) => {
                    rest = &rest[used..];
                    c.start(&name, attrs);
                    if self_closing {
                        c.end(&name);
                    } else if name == "script" || name == "style" {
                        // raw text until the matching end tag
                        let close = format!("</{name}");
                        let lower = rest.to_ascii_lowercase();
                        let end = lower.find(&close).unwrap_or(rest.len());
                        c.data(&rest[..end]);
                        rest = &rest[end..];
                    }
                }
                None => {
                    c.data(&decode_entities(rest));
                    break;
                }
            }
        } else {
            c.data("<");
            rest = &rest[1..];
        }
    }
    while let Some(n) = c.open.pop() {
        let node = Collector::done(n);
        c.found.push(node);
    }
    c.found.sort_by_key(|n| n.seq);
    c.found
}

// ---- row parsers ------------------------------------------------------------------------------

/// Absolute https://www.linkedin.com URL without query/fragment, else "".
pub fn clean_url(href: &str) -> String {
    let h = href.split('?').next().unwrap_or("");
    let h = h.split('#').next().unwrap_or("");
    let full = if h.starts_with('/') {
        format!("{LINKEDIN}{h}")
    } else {
        h.to_string()
    };
    if full.starts_with(&format!("{LINKEDIN}/")) {
        full
    } else {
        String::new()
    }
}

fn first_url(hrefs: &[String], needle: &str) -> String {
    hrefs
        .iter()
        .map(|h| clean_url(h))
        .find(|u| !u.is_empty() && u.contains(needle))
        .unwrap_or_default()
}

fn row_id(kind: &str, text: &str, url: &str) -> String {
    format!("{kind}:{}", &sha1_hex(format!("{url}|{text}").as_bytes())[..12])
}

fn has_class(attrs: &HashMap<String, String>, name: &str) -> bool {
    attrs
        .get("class")
        .is_some_and(|c| c.split_whitespace().any(|x| x == name))
}

fn capped(text: &str) -> String {
    text.chars().take(MAX_TEXT).collect()
}

fn simple_rows(html: &str, kind: &str, needle: &str, m: &dyn Fn(&str, &HashMap<String, String>) -> bool) -> Vec<Value> {
    extract_nodes(html, m)
        .into_iter()
        .filter_map(|n| {
            let text = capped(&n.text);
            if text.is_empty() {
                return None;
            }
            let url = first_url(&n.hrefs, needle);
            Some(json!({"id": row_id(kind, &text, &url), "text": text, "url": url}))
        })
        .collect()
}

pub fn parse_notifications(html: &str) -> Vec<Value> {
    simple_rows(html, "notification", "", &|t, a| {
        t == "article" && has_class(a, "nt-card")
    })
}

pub fn parse_messages(html: &str) -> Vec<Value> {
    simple_rows(html, "message", "/messaging/", &|t, a| {
        t == "li" && has_class(a, "msg-conversation-listitem")
    })
}

pub fn parse_feed(html: &str) -> Vec<Value> {
    let nodes = extract_nodes(html, &|_, a| {
        a.get("data-urn").is_some_and(|u| u.starts_with("urn:li:activity:"))
    });
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();
    for n in nodes {
        let urn = n.attrs["data-urn"].clone();
        let text = capped(&n.text);
        if seen.contains(&urn) || text.is_empty() {
            continue;
        }
        seen.insert(urn.clone());
        rows.push(json!({
            "id": urn, "text": text,
            "url": format!("{LINKEDIN}/feed/update/{urn}/"),
            "profile": first_url(&n.hrefs, "/in/"),
        }));
    }
    rows
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Challenge {
    Login,
    Challenge,
}

fn url_path(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let path = rest.find('/').map_or("", |i| &rest[i..]);
    path.split(['?', '#']).next().unwrap_or("").to_lowercase()
}

/// `None` for a normal page, `Login` for a login wall, `Challenge` for a
/// checkpoint / captcha / security check. Text is matched only against the
/// `<title>`, `<h1>` and `<h2>` of the page, so post bodies, messages and page
/// code can never trigger it.
pub fn detect_challenge(url: &str, html: &str) -> Option<Challenge> {
    let path = url_path(url);
    if path.contains("/checkpoint/") || path.starts_with("/challenge") {
        return Some(Challenge::Challenge);
    }
    if LOGIN_PATHS.iter().any(|p| path.starts_with(p)) {
        return Some(Challenge::Login);
    }
    if html.is_empty() {
        return None;
    }
    let heads = extract_nodes(html, &|t, _| matches!(t, "title" | "h1" | "h2"))
        .iter()
        .map(|n| n.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    CHALLENGE_WORDS
        .iter()
        .any(|w| heads.contains(w))
        .then_some(Challenge::Challenge)
}

// ---- the session ----------------------------------------------------------------------------------

pub type Extractor = Arc<dyn Fn(&str, &Value) -> Vec<Value> + Send + Sync>;

/// How a session is opened; the defaults are the production values.
#[derive(Clone)]
pub struct SessionOptions {
    pub headless: bool,
    pub launch: LaunchOptions,
    /// Origin the pages are fetched from; tests point it at a local fake server.
    pub base_url: String,
    pub extractor: Extractor,
}

impl SessionOptions {
    pub fn new(headless: bool) -> Self {
        Self {
            headless,
            launch: LaunchOptions::default(),
            base_url: LINKEDIN.to_string(),
            extractor: Arc::new(linkedin_extract::extract),
        }
    }
}

fn percent_quote(s: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
    const SET: &AsciiSet = &NON_ALPHANUMERIC.remove(b'_').remove(b'.').remove(b'-').remove(b'~').remove(b'/');
    utf8_percent_encode(s, SET).to_string()
}

pub struct Session {
    pub account_id: String,
    hp: HumanPage<CdpPage>,
    human: Arc<Humanizer>,
    extractor: Extractor,
    base: String,
    // dropped last: closing Chrome releases the profile lock
    chrome: Chrome,
}

impl Session {
    /// Open the account's persistent profile (`~/.loopx/linkedin/<id>/profile`).
    pub fn open(account_id: &str, human: Arc<Humanizer>, opts: &SessionOptions) -> Result<Self, BrowserError> {
        let profile = linkedin_session::ensure_profile(account_id).map_err(|e| BrowserError::Other {
            class: "SessionError".into(),
            detail: e.to_string(),
        })?;
        Self::open_profile(account_id, &profile, human, opts)
    }

    pub fn open_profile(
        account_id: &str,
        profile: &std::path::Path,
        human: Arc<Humanizer>,
        opts: &SessionOptions,
    ) -> Result<Self, BrowserError> {
        let chrome = Chrome::launch(profile, opts.headless, &opts.launch)?;
        let page = chrome.page()?; // on failure `chrome` drops and releases the profile
        Ok(Self {
            account_id: account_id.to_string(),
            hp: HumanPage::new(page, human.clone()),
            human,
            extractor: opts.extractor.clone(),
            base: opts.base_url.trim_end_matches('/').to_string(),
            chrome,
        })
    }

    pub fn close(&mut self) {
        self.chrome.close();
    }

    fn check(&mut self, html: &str) -> Result<(), BrowserError> {
        let url = self.hp.url()?;
        match detect_challenge(&url, html) {
            Some(Challenge::Login) => Err(BrowserError::SessionExpired),
            Some(Challenge::Challenge) => Err(BrowserError::Challenge),
            None => Ok(()),
        }
    }

    fn read(
        &mut self,
        path: &str,
        kind: &str,
        parser: fn(&str) -> Vec<Value>,
        scroll_px: i64,
    ) -> Result<Vec<Value>, BrowserError> {
        let url = format!("{}{path}", self.base);
        self.hp.goto(&url)?;
        self.check("")?;
        self.hp.wait_rendered_default()?;
        self.hp.scroll(scroll_px)?;
        self.hp.wait_rendered_default()?;
        let html = self.hp.content()?;
        self.check(&html)?;
        let mut rows = parser(&html);
        if rows.is_empty() {
            // markup not recognised: let the model read what the page shows
            let dump = self.hp.evaluate(linkedin_extract::PAGE_DUMP_JS)?;
            rows = (self.extractor)(kind, &dump);
        }
        let chars = rows
            .iter()
            .map(|r| r["text"].as_str().map_or(0, |t| t.chars().count()))
            .sum();
        self.hp.dwell(chars);
        Ok(rows)
    }

    pub fn session_ok(&mut self) -> Result<bool, BrowserError> {
        let url = format!("{}/feed/", self.base);
        self.hp.goto(&url)?;
        let html = self.hp.content()?;
        self.check(&html)?;
        Ok(true)
    }
}

impl LinkedInSession for Session {
    fn feed(&mut self) -> Result<Vec<Value>, BrowserError> {
        self.read("/feed/", "feed", parse_feed, 3000)
    }

    fn notifications(&mut self) -> Result<Vec<Value>, BrowserError> {
        self.read("/notifications/", "notifications", parse_notifications, 1200)
    }

    fn messages(&mut self) -> Result<Vec<Value>, BrowserError> {
        self.read("/messaging/", "messages", parse_messages, 400)
    }

    /// A person does not only visit the pages they need: open one to three
    /// extra ones, scroll a little and stay a while.
    fn wander(&mut self) -> Result<(), BrowserError> {
        let urls: Vec<String> = ["/mynetwork/", "/jobs/", "/in/me/", "/feed/"]
            .iter()
            .map(|p| format!("{}{p}", self.base))
            .collect();
        let refs: Vec<&str> = urls.iter().map(String::as_str).collect();
        for url in self.human.wander_pages(&refs) {
            self.hp.goto(url)?;
            self.check("")?;
            let px = self.human.uniform(300.0, 1500.0) as i64;
            self.hp.scroll(px)?;
            self.hp.dwell(0);
        }
        Ok(())
    }

    /// People matching `query`, found through the header search bar like a
    /// person would: type, Enter, People filter, then page through results
    /// until `want` people not in `known` are collected (or the page cap).
    fn search_people(
        &mut self,
        query: &str,
        want: usize,
        known: &BTreeSet<String>,
    ) -> Result<Vec<Value>, BrowserError> {
        let feed = format!("{}/feed/", self.base);
        self.hp.goto(&feed)?;
        self.check("")?;
        self.hp.wait_rendered_default()?;
        let results = format!(
            "{}/search/results/people/?keywords={}&origin=GLOBAL_SEARCH_HEADER",
            self.base,
            percent_quote(query)
        );
        let pause = |s: &Self| s.human.sleep(s.human.pause_seconds(2.0, 4.0));
        if self.hp.type_into(&Selector::Css(SEARCH_BOX.into()), query)? {
            pause(self);
            self.hp.wait_rendered_default()?;
            self.check("")?;
            if !self.hp.url()?.contains("/search/results/people") {
                let filter = Selector::Text {
                    tags: vec!["button".into(), "a".into()],
                    text: "People".into(),
                };
                if self.hp.click(&filter)? {
                    pause(self);
                } else {
                    self.hp.goto(&results)?;
                }
            }
        } else {
            self.hp.goto(&results)?;
        }
        let mut taken: BTreeSet<String> = known.clone();
        let mut found: Vec<Value> = Vec::new();
        for number in 1..=MAX_SEARCH_PAGES {
            if number > 1 {
                self.hp.goto(&format!("{results}&page={number}"))?;
            }
            self.check("")?;
            self.hp.wait_rendered_default()?;
            self.hp.scroll(2500)?;
            self.hp.wait_rendered_default()?;
            let html = self.hp.content()?;
            self.check(&html)?;
            let dump = self.hp.evaluate(linkedin_extract::PAGE_DUMP_JS)?;
            let rows = (self.extractor)("people", &dump);
            if rows.is_empty() {
                break;
            }
            for row in rows {
                let url = row["url"].as_str().unwrap_or_default().to_string();
                if taken.insert(url) {
                    found.push(row);
                }
            }
            if found.len() >= want {
                break;
            }
            self.hp.dwell(0);
        }
        found.truncate(want);
        Ok(found)
    }
}

// ---- wiring --------------------------------------------------------------------------------------

/// The connector's "Test" probe: open the profile headlessly, check the feed loads signed in.
pub struct BrowserProbe {
    pub options: SessionOptions,
}

impl LinkedInProbe for BrowserProbe {
    fn session_ok(&self, account_id: &str) -> Result<(), ProbeFailure> {
        let human = Arc::new(Humanizer::default());
        let mut session = Session::open(account_id, human, &self.options).map_err(to_failure)?;
        session.session_ok().map(|_| ()).map_err(to_failure)
    }
}

fn to_failure(e: BrowserError) -> ProbeFailure {
    match e {
        BrowserError::Unavailable(_) => ProbeFailure::BrowserUnavailable,
        BrowserError::SessionExpired => ProbeFailure::SessionExpired,
        BrowserError::Challenge => ProbeFailure::Challenge,
        other => ProbeFailure::Other(other.class_name()),
    }
}

/// Plug the real browser into the LinkedIn loop and the connector's Test
/// button. Call once at startup; safe to call again (the probe keeps the first).
pub fn register() {
    register_session_factory(Arc::new(|account_id, headless, _human| {
        let session = Session::open(
            account_id,
            Arc::new(Humanizer::default()),
            &SessionOptions::new(headless),
        )?;
        Ok(Box::new(session) as Box<dyn LinkedInSession>)
    }));
    crate::connectors::linkedin::register_probe(Arc::new(BrowserProbe {
        options: SessionOptions::new(true),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const NOTIFICATIONS: &str = r#"
<main>
 <article class="nt-card nt-card--unread">
  <a href="/feed/update/urn:li:activity:111/?trk=x">Ada Lovelace commented on your post:
     <span>  "Great   write-up" </span></a>
 </article>
 <article class="nt-card"><a href="https://www.linkedin.com/in/grace-hopper/?miniProfileUrn=1">Grace Hopper viewed your profile</a></article>
 <article class="other"><a href="/x">not a notification</a></article>
 <article class="nt-card"></article>
</main>"#;

    const FEED: &str = r#"
<div class="scaffold-finite-scroll">
 <div class="feed-shared-update-v2" data-urn="urn:li:activity:7001">
   <a href="/in/ada-lovelace/?trk=feed">Ada Lovelace</a>
   <div>We shipped <b>Rails 9</b> today. <br> Details inside.</div>
   <div data-urn="urn:li:activity:7001"><a href="/in/dup/">dup nested</a></div>
 </div>
 <div data-urn="urn:li:comment:(1,2)">a comment, not a post</div>
 <div class="feed-shared-update-v2" data-urn="urn:li:activity:7002"><span>Hiring a staff engineer</span></div>
</div>"#;

    const MESSAGES: &str = r#"
<ul>
 <li class="msg-conversation-listitem"><a href="/messaging/thread/2-abc/">Linus: are you free <b>Friday</b>?</a></li>
 <li class="msg-conversation-listitem"></li>
 <li class="unrelated">x</li>
</ul>"#;

    fn texts(rows: &[Value]) -> Vec<&str> {
        rows.iter().map(|r| r["text"].as_str().unwrap()).collect()
    }

    #[test]
    fn parse_notifications_rows() {
        let rows = parse_notifications(NOTIFICATIONS);
        assert_eq!(
            texts(&rows),
            ["Ada Lovelace commented on your post: \"Great write-up\"", "Grace Hopper viewed your profile"]
        );
        assert_eq!(rows[0]["url"], "https://www.linkedin.com/feed/update/urn:li:activity:111/");
        assert_eq!(rows[1]["url"], "https://www.linkedin.com/in/grace-hopper/");
        assert!(rows.iter().all(|r| r["id"].as_str().unwrap().starts_with("notification:")));
        assert_ne!(rows[0]["id"], rows[1]["id"]);
    }

    #[test]
    fn parse_feed_dedupes_and_extracts_profile() {
        let rows = parse_feed(FEED);
        let ids: Vec<_> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["urn:li:activity:7001", "urn:li:activity:7002"]);
        assert!(rows[0]["text"].as_str().unwrap().starts_with("Ada Lovelace We shipped Rails 9 today."));
        assert_eq!(rows[0]["url"], "https://www.linkedin.com/feed/update/urn:li:activity:7001/");
        assert_eq!(rows[0]["profile"], "https://www.linkedin.com/in/ada-lovelace/");
        assert_eq!(rows[1]["profile"], "");
    }

    #[test]
    fn parse_messages_rows() {
        let rows = parse_messages(MESSAGES);
        assert_eq!(texts(&rows), ["Linus: are you free Friday?"]);
        assert_eq!(rows[0]["url"], "https://www.linkedin.com/messaging/thread/2-abc/");
    }

    #[test]
    fn ids_match_the_python_form() {
        // sha1("<url>|<text>")[:12] over the same row the Python parser produced.
        let rows = parse_messages(MESSAGES);
        let want = row_id("message", "Linus: are you free Friday?", "https://www.linkedin.com/messaging/thread/2-abc/");
        assert_eq!(rows[0]["id"], want);
        assert_eq!(want.len(), "message:".len() + 12);
    }

    #[test]
    fn text_is_capped() {
        let html = format!(r#"<article class="nt-card"><a href="/x">{}</a></article>"#, "z".repeat(5000));
        assert_eq!(parse_notifications(&html)[0]["text"].as_str().unwrap().len(), MAX_TEXT);
    }

    #[test]
    fn unrecognised_or_broken_markup_gives_no_rows_not_a_crash() {
        for html in [
            "",
            "<html><body><p>LinkedIn changed everything</p></body></html>",
            "<article class='nt-card'><a href='/x'>unclosed",
            "<<<>>>",
            "<div></p></li></article>",
            "<a href=\"unterminated",
            "<!-- never closed",
        ] {
            let _ = (parse_notifications(html), parse_feed(html), parse_messages(html));
        }
        assert!(parse_feed("<p>nothing here</p>").is_empty());
    }

    #[test]
    fn non_linkedin_links_are_dropped() {
        let html = r#"<article class="nt-card"><a href="https://evil.example/x">click</a> hi</article>"#;
        assert_eq!(parse_notifications(html)[0]["url"], "");
        let html = r#"<div data-urn="urn:li:activity:9"><a href="https://evil.example/in/x/">p</a>t</div>"#;
        assert_eq!(parse_feed(html)[0]["profile"], "");
    }

    #[test]
    fn detect_challenge_by_url() {
        let d = |u| detect_challenge(u, "");
        assert_eq!(d("https://www.linkedin.com/checkpoint/challenge/abc"), Some(Challenge::Challenge));
        assert_eq!(d("https://www.linkedin.com/login?session_redirect=x"), Some(Challenge::Login));
        assert_eq!(d("https://www.linkedin.com/uas/login"), Some(Challenge::Login));
        assert_eq!(d("https://www.linkedin.com/authwall?trk=x"), Some(Challenge::Login));
        assert_eq!(d("https://www.linkedin.com/feed/"), None);
    }

    #[test]
    fn detect_challenge_by_headings_only() {
        let url = "https://www.linkedin.com/feed/";
        let d = |h: &str| detect_challenge(url, h);
        assert_eq!(d("<h1>Let's do a quick security check</h1>"), Some(Challenge::Challenge));
        assert_eq!(d("<html><title>Security Verification | LinkedIn</title></html>"), Some(Challenge::Challenge));
        assert_eq!(d("<h2>Please solve this CAPTCHA</h2>"), Some(Challenge::Challenge));
        assert_eq!(d("<script>var captcha = 'unusual activity';</script><style>.captcha{}</style><p>ok</p>"), None);
        assert_eq!(
            d(r#"<div data-urn="urn:li:activity:1"><p>Someone asked me to solve a CAPTCHA, unusual activity indeed</p></div>"#),
            None
        );
        assert_eq!(detect_challenge("https://www.linkedin.com/company/challenge-corp/", ""), None);
        assert_eq!(detect_challenge("https://www.linkedin.com/challenge/abc", ""), Some(Challenge::Challenge));
    }

    #[test]
    fn detect_challenge_is_fast_on_hostile_input() {
        let start = Instant::now();
        assert_eq!(detect_challenge("https://www.linkedin.com/feed/", &"<script>".repeat(40000)), None);
        assert!(start.elapsed().as_secs_f64() < 2.0);
    }

    #[test]
    fn stray_end_tag_does_not_close_a_card_early() {
        let rows = parse_notifications(r#"<article class="nt-card"><a>one</a></p> tail text</article>"#);
        assert_eq!(texts(&rows), ["one tail text"]);
    }

    #[test]
    fn unclosed_inline_tag_does_not_swallow_next_row() {
        let html = r#"<li class="msg-conversation-listitem"><b>A</li><li class="msg-conversation-listitem"><b>B</li>"#;
        assert_eq!(texts(&parse_messages(html)), ["A", "B"]);
    }

    #[test]
    fn top_level_stray_end_tag_is_ignored() {
        let rows = parse_notifications(r#"</div></p><article class="nt-card">hi</article>"#);
        assert_eq!(texts(&rows), ["hi"]);
    }

    #[test]
    fn lookalike_host_is_not_linkedin() {
        assert_eq!(clean_url("https://www.linkedin.com.evil.example/x"), "");
        assert_eq!(clean_url("/in/x/?a=1#b"), "https://www.linkedin.com/in/x/");
    }

    #[test]
    fn end_tag_handling_is_linear_on_hostile_nesting() {
        for html in [
            format!("{}{}", "<div>".repeat(50000), "</span>".repeat(50000)),
            format!("{}{}", "<div>".repeat(30000), "<b></b>".repeat(30000)),
            format!("{}{}", "<div>".repeat(50000), "</div>".repeat(50000)),
        ] {
            let start = Instant::now();
            parse_notifications(&html);
            assert!(start.elapsed().as_secs_f64() < 2.0);
        }
    }

    #[test]
    fn entities_and_attribute_quoting() {
        let html = r#"<article class='nt-card'><a href=/feed/update/x/ title="a &amp; b">Fish &amp; chips &#33; &lt;3 &bogus;</a></article>"#;
        let rows = parse_notifications(html);
        assert_eq!(texts(&rows), ["Fish & chips ! <3 &bogus;"]);
        assert_eq!(rows[0]["url"], "https://www.linkedin.com/feed/update/x/");
    }

    #[test]
    fn percent_quote_matches_python_quote() {
        assert_eq!(percent_quote("AI builder"), "AI%20builder");
        assert_eq!(percent_quote("a/b_c.d-e~f&g"), "a/b_c.d-e~f%26g");
    }
}
