//! RSS / Atom feed connector (read-only, no secret).
//!
//! Feeds are untrusted: a body containing a DOCTYPE or ENTITY declaration is
//! refused before anything is parsed, so no entity is ever expanded. The XML
//! parser is a small purpose-built one (elements, attributes, text, CDATA,
//! the five predefined entities and numeric character references,
//! namespaces); anything else in the document is an error.

use super::http::TEST_TIMEOUT_SECONDS;
use super::*;
use serde_json::json;
use std::io::Read;

pub const MAX_BYTES: u64 = 2 * 1024 * 1024;
const ATOM: &str = "{http://www.w3.org/2005/Atom}";

const FIELDS: [Field; 1] = [Field::new("feeds", "Feed URLs")
    .kind("textarea")
    .help("One feed URL per line")
    .placeholder("https://example.com/feed.xml")];

pub const INFO: TypeInfo = TypeInfo {
    name: "rss",
    label: "RSS / Atom feeds",
    icon: "rss_feed",
    capabilities: &[FEED],
    fields: &FIELDS,
    secret_label: None,
    external: false,
    brand: "rss",
    description: "Follow RSS and Atom feeds for the topic monitor.",
    docs_url: "",
    category: "feeds",
    presets: &[],
    auth: Auth::Secret,
};

/// `(url, timeout_seconds)` -> the feed body (at most [`MAX_BYTES`]).
pub type Fetcher = Arc<dyn Fn(&str, u64) -> Result<Vec<u8>, ConnectorError> + Send + Sync>;

/// The real fetcher: http(s) only, `User-Agent: LoopX/1.0`, at most 2 MB.
pub fn fetch(url: &str, timeout_secs: u64) -> Result<Vec<u8>, ConnectorError> {
    let lower = url.to_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(ConnectorError::msg(
            "Only http:// and https:// feed URLs are supported",
        ));
    }
    let resp = super::http::agent()
        .get(url)
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .set("User-Agent", "LoopX/1.0")
        .call();
    match resp {
        Ok(r) => {
            let mut body = Vec::new();
            let _ = r.into_reader().take(MAX_BYTES).read_to_end(&mut body);
            Ok(body)
        }
        Err(ureq::Error::Status(code, r)) => Err(ConnectorError::Other {
            class: "HTTPError".into(),
            detail: format!("HTTP Error {code}: {}", r.status_text()),
        }),
        Err(ureq::Error::Transport(t)) => Err(ConnectorError::Other {
            class: "URLError".into(),
            detail: format!("<urlopen error {}>", t.message().unwrap_or("network error")),
        }),
    }
}

// ---- a minimal XML parser -------------------------------------------------------

#[derive(Debug, Default)]
struct Node {
    /// ElementTree style: `local` or `{uri}local`.
    tag: String,
    attrs: Vec<(String, String)>,
    /// Text before the first child element.
    text: String,
    children: Vec<Node>,
}

impl Node {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    fn find(&self, tag: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.tag == tag)
    }
    /// `_text(node, tag)`: the stripped text of a direct child, or `""`.
    fn child_text(&self, tag: &str) -> String {
        self.find(tag)
            .map_or(String::new(), |c| c.text.trim().to_string())
    }
    fn iter<'a>(&'a self, tag: &str, out: &mut Vec<&'a Node>) {
        if self.tag == tag {
            out.push(self);
        }
        for c in &self.children {
            c.iter(tag, out);
        }
    }
}

struct Parser<'a> {
    s: &'a str,
    pos: usize,
}

type XmlResult<T> = Result<T, String>;

impl<'a> Parser<'a> {
    fn rest(&self) -> &'a str {
        &self.s[self.pos..]
    }
    fn line_col(&self) -> String {
        let before = &self.s[..self.pos];
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count());
        format!("line {line}, column {col}")
    }
    fn err<T>(&self, what: &str) -> XmlResult<T> {
        Err(format!("{what}: {}", self.line_col()))
    }
    fn skip_ws(&mut self) {
        let t = self.rest().trim_start();
        self.pos = self.s.len() - t.len();
    }
    fn starts(&self, p: &str) -> bool {
        self.rest().starts_with(p)
    }
    fn skip_until(&mut self, end: &str, what: &str) -> XmlResult<()> {
        match self.rest().find(end) {
            Some(i) => {
                self.pos += i + end.len();
                Ok(())
            }
            None => self.err(what),
        }
    }
    /// Comments, PIs and whitespace between nodes.
    fn skip_misc(&mut self) -> XmlResult<()> {
        loop {
            self.skip_ws();
            if self.starts("<!--") {
                self.skip_until("-->", "unclosed comment")?;
            } else if self.starts("<?") {
                self.skip_until("?>", "unclosed token")?;
            } else {
                return Ok(());
            }
        }
    }
    fn name(&mut self) -> XmlResult<String> {
        let rest = self.rest();
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | ':' | '-' | '.')))
            .unwrap_or(rest.len());
        if end == 0 {
            return self.err("not well-formed (invalid token)");
        }
        self.pos += end;
        Ok(rest[..end].to_string())
    }
    fn decode(&self, raw: &str) -> XmlResult<String> {
        let mut out = String::with_capacity(raw.len());
        let mut rest = raw;
        while let Some(i) = rest.find('&') {
            out.push_str(&rest[..i]);
            let after = &rest[i + 1..];
            let Some(end) = after.find(';') else {
                return self.err("not well-formed (invalid token)");
            };
            let ent = &after[..end];
            match ent {
                "lt" => out.push('<'),
                "gt" => out.push('>'),
                "amp" => out.push('&'),
                "quot" => out.push('"'),
                "apos" => out.push('\''),
                _ => {
                    let code = ent
                        .strip_prefix("#x")
                        .or_else(|| ent.strip_prefix("#X"))
                        .map(|h| u32::from_str_radix(h, 16))
                        .or_else(|| ent.strip_prefix('#').map(|d| d.parse::<u32>()));
                    match code.and_then(Result::ok).and_then(char::from_u32) {
                        Some(c) => out.push(c),
                        None => return self.err("undefined entity"),
                    }
                }
            }
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn element(&mut self, ns: &[(String, String)]) -> XmlResult<Node> {
        // at '<'
        self.pos += 1;
        let raw_tag = self.name()?;
        let mut attrs_raw: Vec<(String, String)> = Vec::new();
        let self_closing;
        loop {
            self.skip_ws();
            if self.starts("/>") {
                self.pos += 2;
                self_closing = true;
                break;
            }
            if self.starts(">") {
                self.pos += 1;
                self_closing = false;
                break;
            }
            let key = self.name()?;
            self.skip_ws();
            if !self.starts("=") {
                return self.err("not well-formed (invalid token)");
            }
            self.pos += 1;
            self.skip_ws();
            let quote = self.rest().chars().next();
            if !matches!(quote, Some('"') | Some('\'')) {
                return self.err("not well-formed (invalid token)");
            }
            let q = quote.unwrap();
            self.pos += 1;
            let Some(end) = self.rest().find(q) else {
                return self.err("unclosed token");
            };
            let value = self.decode(&self.rest()[..end])?;
            self.pos += end + 1;
            if attrs_raw.iter().any(|(k, _)| *k == key) {
                return self.err("duplicate attribute");
            }
            attrs_raw.push((key, value));
        }
        // Namespace scope for this element.
        let mut scope: Vec<(String, String)> = ns.to_vec();
        for (k, v) in &attrs_raw {
            if k == "xmlns" {
                scope.push((String::new(), v.clone()));
            } else if let Some(p) = k.strip_prefix("xmlns:") {
                scope.push((p.to_string(), v.clone()));
            }
        }
        let resolve = |qname: &str, is_attr: bool| -> String {
            let (prefix, local) = match qname.split_once(':') {
                Some((p, l)) => (p, l),
                None => ("", qname),
            };
            if is_attr && prefix.is_empty() {
                return local.to_string();
            }
            match scope.iter().rev().find(|(p, _)| p == prefix) {
                Some((_, uri)) if !uri.is_empty() => format!("{{{uri}}}{local}"),
                _ => local.to_string(),
            }
        };
        let mut node = Node {
            tag: resolve(&raw_tag, false),
            attrs: attrs_raw
                .iter()
                .filter(|(k, _)| k != "xmlns" && !k.starts_with("xmlns:"))
                .map(|(k, v)| (resolve(k, true), v.clone()))
                .collect(),
            ..Node::default()
        };
        if self_closing {
            return Ok(node);
        }
        loop {
            let rest = self.rest();
            let Some(lt) = rest.find('<') else {
                return self.err("no element found");
            };
            if lt > 0 {
                let text = self.decode(&rest[..lt])?;
                if node.children.is_empty() {
                    node.text.push_str(&text);
                }
                self.pos += lt;
            }
            if self.starts("</") {
                self.pos += 2;
                let end_name = self.name()?;
                self.skip_ws();
                if !self.starts(">") {
                    return self.err("not well-formed (invalid token)");
                }
                self.pos += 1;
                if end_name != raw_tag {
                    return self.err("mismatched tag");
                }
                return Ok(node);
            } else if self.starts("<!--") {
                self.skip_until("-->", "unclosed comment")?;
            } else if self.starts("<![CDATA[") {
                self.pos += 9;
                let Some(end) = self.rest().find("]]>") else {
                    return self.err("unclosed CDATA section");
                };
                if node.children.is_empty() {
                    node.text.push_str(&self.rest()[..end]);
                }
                self.pos += end + 3;
            } else if self.starts("<?") {
                self.skip_until("?>", "unclosed token")?;
            } else if self.starts("<!") {
                return self.err("not well-formed (invalid token)");
            } else {
                let child = self.element(&scope)?;
                node.children.push(child);
            }
        }
    }
}

fn parse_document(text: &str) -> XmlResult<Node> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut p = Parser { s: text, pos: 0 };
    p.skip_misc()?;
    if !p.starts("<") {
        return p.err("syntax error");
    }
    let root = p.element(&[])?;
    p.skip_misc()?;
    if !p.rest().is_empty() {
        return p.err("junk after document element");
    }
    Ok(root)
}

/// Decode the body (UTF-8, or UTF-16 with a byte-order mark).
fn decode_body(body: &[u8]) -> Result<String, String> {
    let utf16 = |units: Vec<u16>| {
        String::from_utf16(&units).map_err(|_| "not well-formed (invalid token)".to_string())
    };
    if body.starts_with(&[0xff, 0xfe]) {
        return utf16(
            body[2..]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect(),
        );
    }
    if body.starts_with(&[0xfe, 0xff]) {
        return utf16(
            body[2..]
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect(),
        );
    }
    String::from_utf8(body.to_vec()).map_err(|_| "not well-formed (invalid token)".to_string())
}

const DOCTYPE_MSG: &str = "Feed contains a DOCTYPE or ENTITY declaration, which is not allowed";

/// Parse a feed body into entries (`title`, `link`, `id`, `published`,
/// `summary`); RSS `item`s first, then Atom `entry`s.
pub fn parse_feed(body: &[u8]) -> Result<Vec<Value>, ConnectorError> {
    let lower = String::from_utf8_lossy(body).to_lowercase();
    if lower.contains("<!doctype") || lower.contains("<!entity") {
        return Err(ConnectorError::msg(DOCTYPE_MSG));
    }
    let invalid = |detail: String| ConnectorError::msg(format!("Feed is not valid XML: {detail}"));
    let text = decode_body(body).map_err(invalid)?;
    let decoded = text.to_lowercase();
    if decoded.contains("<!doctype") || decoded.contains("<!entity") {
        return Err(ConnectorError::msg(DOCTYPE_MSG));
    }
    let root = parse_document(&text).map_err(invalid)?;
    let mut out = Vec::new();
    let mut items = Vec::new();
    root.iter("item", &mut items);
    for item in items {
        out.push(
            json!({"title": item.child_text("title"), "link": item.child_text("link"),
                        "id": item.child_text("guid"), "published": item.child_text("pubDate"),
                        "summary": item.child_text("description")}),
        );
    }
    let mut entries = Vec::new();
    root.iter(&format!("{ATOM}entry"), &mut entries);
    for entry in entries {
        let link = entry
            .children
            .iter()
            .filter(|c| c.tag == format!("{ATOM}link"))
            .find(|c| {
                c.attr("rel").unwrap_or("alternate") == "alternate"
                    && c.attr("href").is_some_and(|h| !h.is_empty())
            })
            .and_then(|c| c.attr("href"))
            .unwrap_or("")
            .to_string();
        let published = Some(entry.child_text(&format!("{ATOM}published")))
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| entry.child_text(&format!("{ATOM}updated")));
        out.push(
            json!({"title": entry.child_text(&format!("{ATOM}title")), "link": link,
                        "id": entry.child_text(&format!("{ATOM}id")), "published": published,
                        "summary": entry.child_text(&format!("{ATOM}summary"))}),
        );
    }
    Ok(out)
}

pub struct RssConnector {
    account: Account,
    fetcher: Fetcher,
}

impl RssConnector {
    pub fn new(account: Account, _http: Arc<dyn HttpClient>) -> Self {
        Self {
            account,
            fetcher: Arc::new(fetch),
        }
    }

    /// Replace the network fetch (tests).
    pub fn with_fetcher(mut self, fetcher: Fetcher) -> Self {
        self.fetcher = fetcher;
        self
    }

    /// One non-empty trimmed line per configured feed URL.
    pub fn feed_urls(&self) -> Vec<String> {
        self.account
            .setting("feeds")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }
}

impl Connector for RssConnector {
    fn info(&self) -> &'static TypeInfo {
        &INFO
    }

    fn account(&self) -> &Account {
        &self.account
    }

    fn feed_urls(&self) -> Vec<String> {
        RssConnector::feed_urls(self)
    }

    fn entries(&self, url: &str) -> ConnResult<Vec<Value>> {
        parse_feed(&(self.fetcher)(url, TEST_TIMEOUT_SECONDS)?)
    }

    fn test(&self) -> (bool, String) {
        let urls = self.feed_urls();
        if urls.is_empty() {
            return (false, "Feed URLs is required".to_string());
        }
        let mut total = 0;
        for url in &urls {
            match self.entries(url) {
                Ok(e) => total += e.len(),
                Err(e) => return (false, format!("{url}: {e}")),
            }
        }
        (true, format!("{} feeds, {total} entries", urls.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn conn(feeds: &str, body: &'static [u8]) -> RssConnector {
        RssConnector::new(
            Account::new("r", "rss").with_setting("feeds", feeds),
            Arc::new(UreqClient::default()),
        )
        .with_fetcher(Arc::new(move |_, _| Ok(body.to_vec())))
    }

    #[test]
    fn counts_entries_across_feeds() {
        let xml = b"<rss><channel><item><title>a</title></item><item><title>b</title></item></channel></rss>";
        let c = conn("https://a/feed\nhttps://b/feed", xml);
        assert_eq!(c.test(), (true, "2 feeds, 4 entries".to_string()));
        let titles: Vec<_> = c
            .entries("https://a/feed")
            .unwrap()
            .iter()
            .map(|e| e["title"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(titles, ["a", "b"]);
    }

    #[test]
    fn rss_item_fields() {
        let xml = b"<rss><channel><item><title> T </title><link>https://x/1</link><guid>g</guid>\
            <pubDate>Mon, 06 Oct 2026 09:00:00 GMT</pubDate><description><![CDATA[<b>hi</b> &amp;]]></description></item></channel></rss>";
        let e = conn("https://a", xml).entries("https://a").unwrap();
        assert_eq!(
            e[0],
            json!({"title": "T", "link": "https://x/1", "id": "g",
                   "published": "Mon, 06 Oct 2026 09:00:00 GMT", "summary": "<b>hi</b> &amp;"})
        );
    }

    #[test]
    fn atom_is_supported() {
        let xml = b"<feed xmlns='http://www.w3.org/2005/Atom'><entry><title>t</title><link href='https://x/1'/><id>1</id></entry></feed>";
        assert_eq!(
            conn("https://a", xml).entries("https://a").unwrap(),
            vec![
                json!({"title": "t", "link": "https://x/1", "id": "1", "published": "", "summary": ""})
            ]
        );
    }

    #[test]
    fn atom_link_prefers_alternate_and_published_falls_back_to_updated() {
        let xml = b"<a:feed xmlns:a='http://www.w3.org/2005/Atom'><a:entry>\
            <a:link rel='self' href='https://x/self'/><a:link href='https://x/alt'/>\
            <a:updated>2026-10-06</a:updated></a:entry></a:feed>";
        let e = conn("https://a", xml).entries("https://a").unwrap();
        assert_eq!(
            (e[0]["link"].as_str(), e[0]["published"].as_str()),
            (Some("https://x/alt"), Some("2026-10-06"))
        );
    }

    #[test]
    fn doctype_and_entity_declarations_are_refused() {
        let c = conn("https://a", b"<!DOCTYPE x [<!ENTITY a 'b'>]><rss/>");
        let (ok, msg) = c.test();
        assert!(!ok && msg.contains("DOCTYPE"));
        let body = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><!DOCTYPE x [<!ENTITY a \"pwned\">]><rss><channel><item><title>&a;</title></item></channel></rss>";
        let mut utf16: Vec<u8> = vec![0xff, 0xfe];
        for u in body.encode_utf16() {
            utf16.extend_from_slice(&u.to_le_bytes());
        }
        let leaked: &'static [u8] = Box::leak(utf16.into_boxed_slice());
        let c = conn("https://a", leaked);
        assert!(c.test().1.contains("DOCTYPE"));
        assert!(
            matches!(c.entries("https://a"), Err(ConnectorError::Message(m)) if m.contains("DOCTYPE"))
        );
    }

    #[test]
    fn malformed_xml_is_reported_not_raised() {
        for bad in [
            &b"<rss><item></rss>"[..],
            b"not xml",
            b"<rss/><rss/>",
            b"<rss>&nope;</rss>",
        ] {
            let (ok, msg) = conn("https://a", Box::leak(bad.to_vec().into_boxed_slice())).test();
            assert!(!ok && msg.contains("Feed is not valid XML"), "{msg}");
        }
    }

    #[test]
    fn character_references_and_comments_are_handled() {
        let xml = b"<?xml version='1.0'?><!-- c --><rss><channel><item><title>A&#66;&#x43; &lt;ok&gt;</title><!-- x --></item></channel></rss>";
        let e = conn("https://a", xml).entries("https://a").unwrap();
        assert_eq!(e[0]["title"], "ABC <ok>");
    }

    #[test]
    fn empty_feed_list_and_fetch_errors() {
        assert_eq!(
            conn("  \n ", b"").test(),
            (false, "Feed URLs is required".to_string())
        );
        let c = RssConnector::new(
            Account::new("r", "rss").with_setting("feeds", "file:///etc/passwd"),
            Arc::new(UreqClient::default()),
        );
        let (ok, msg) = c.test();
        assert!(!ok && msg.contains("Only http:// and https://"));
        assert!(matches!(
            fetch("file:///etc/passwd", 5),
            Err(ConnectorError::Message(_))
        ));
    }

    #[test]
    fn real_fetch_reads_at_most_2mb_and_reports_http_errors() {
        let big = "a".repeat(3 * 1024 * 1024);
        let f = super::super::testutil::serve(vec![(200, "", big), (404, "", "x".into())]);
        assert_eq!(fetch(&f.url, 5).unwrap().len(), 2 * 1024 * 1024);
        let err = fetch(&f.url, 5).unwrap_err();
        assert_eq!(err.class_name(), "HTTPError");
        assert!(f.requests.lock().unwrap()[0]
            .to_lowercase()
            .contains("user-agent: loopx/1.0"));
        let _ = std::io::stdout().flush();
    }
}
