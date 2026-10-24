//! Pure logic for the Inbox Triage loop: turn normalized messages into the AI
//! prompt, and turn the AI's reply back into validated decisions. No I/O. Mirrors
//! `bin/inbox_triage.py`; orchestration is in [`crate::inbox_runner`].

use crate::inbox_config::{parse_address, sender_matches, string_list};
use crate::loops_config::py_repr;
use crate::mail_provider::MailMessage;
use serde_json::{json, Map, Value};

pub const BODY_LIMIT: usize = 4000;
pub const PRIOR_LIMIT: usize = 1500;
pub const PRIOR_COUNT: usize = 2;
pub const REASON_LIMIT: usize = 200;
pub const DRAFT_LIMIT: usize = 4000;
const ERROR_VALUE_LIMIT: usize = 40;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct TriageResponseError(pub String);

fn err<T>(m: impl Into<String>) -> Result<T, TriageResponseError> {
    Err(TriageResponseError(m.into()))
}

/// One validated verdict for one message.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub id: String,
    pub category: String,
    pub reason: String,
    pub draft_body: Option<String>,
    /// Set by [`apply_rules`]: urgent, but no draft to hand over.
    pub needs_manual_reply: bool,
}

// ---------------------------------------------------------------------- html

const BLOCK_TAGS: [&str; 15] = [
    "br",
    "p",
    "div",
    "li",
    "tr",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "blockquote",
    "table",
    "ul",
    "ol",
];
const SKIP_TAGS: [&str; 4] = ["script", "style", "head", "title"];
/// Elements whose content is raw text (no nested tags): like `HTMLParser`.
const RAW_TEXT_TAGS: [&str; 2] = ["script", "style"];

fn find_ci(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    (from..h.len().saturating_sub(n.len().saturating_sub(1)))
        .find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// The visible text of an HTML fragment: tags dropped, `script`/`style`/`head`/
/// `title` content skipped, block elements become line breaks, entities decoded,
/// runs of blanks collapsed and empty lines removed.
pub fn html_to_text(markup: &str) -> String {
    let b = markup.as_bytes();
    let mut parts = String::new();
    let (mut skip, mut i) = (0usize, 0usize);
    while i < b.len() {
        if b[i] != b'<' {
            let end = markup[i..].find('<').map_or(b.len(), |p| i + p);
            if skip == 0 {
                parts.push_str(&markup[i..end]);
            }
            i = end;
            continue;
        }
        let rest = &markup[i..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(b.len(), |p| i + p + 3);
        } else if rest.starts_with("<!") || rest.starts_with("<?") {
            i = rest.find('>').map_or(b.len(), |p| i + p + 1);
        } else if rest.len() > 1 && rest.as_bytes()[1].is_ascii_alphabetic() {
            let name_end = rest[1..]
                .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
                .map_or(rest.len(), |p| p + 1);
            let name = rest[1..name_end].to_ascii_lowercase();
            // Skip the attributes up to the closing '>', honouring quotes.
            let (mut j, mut quote) = (name_end, None::<u8>);
            let rb = rest.as_bytes();
            while j < rb.len() {
                match (quote, rb[j]) {
                    (Some(q), c) if c == q => quote = None,
                    (None, b'"') | (None, b'\'') => quote = Some(rb[j]),
                    (None, b'>') => break,
                    _ => {}
                }
                j += 1;
            }
            let self_closing = j > 0 && j < rb.len() && rb[j - 1] == b'/';
            i += (j + 1).min(rest.len());
            if SKIP_TAGS.contains(&name.as_str()) {
                skip += 1;
            } else if BLOCK_TAGS.contains(&name.as_str()) && skip == 0 {
                parts.push('\n');
            }
            if RAW_TEXT_TAGS.contains(&name.as_str()) && !self_closing {
                let close = format!("</{name}");
                match find_ci(markup, &close, i) {
                    Some(p) => i = p,
                    None => i = b.len(),
                }
            } else if self_closing {
                // handle_startendtag = start + end.
                if SKIP_TAGS.contains(&name.as_str()) {
                    skip = skip.saturating_sub(1);
                } else if BLOCK_TAGS.contains(&name.as_str()) && skip == 0 {
                    parts.push('\n');
                }
            }
        } else if rest.starts_with("</")
            && rest.len() > 2
            && rest.as_bytes()[2].is_ascii_alphabetic()
        {
            let close = rest.find('>');
            let name = rest[2..close.unwrap_or(rest.len())]
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            i += close.map_or(rest.len(), |c| c + 1);
            if SKIP_TAGS.contains(&name.as_str()) {
                skip = skip.saturating_sub(1);
            } else if BLOCK_TAGS.contains(&name.as_str()) && skip == 0 {
                parts.push('\n');
            }
        } else {
            if skip == 0 {
                parts.push('<');
            }
            i += 1;
        }
    }
    let text = unescape(&unescape(&parts));
    let lines: Vec<String> = py_splitlines(&text)
        .into_iter()
        .map(|l| collapse_blanks(l).trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    lines.join("\n")
}

/// `re.sub(r"[ \t]+", " ", line)`.
fn collapse_blanks(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut prev_blank = false;
    for c in line.chars() {
        let blank = c == ' ' || c == '\t';
        if blank {
            if !prev_blank {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
        prev_blank = blank;
    }
    out
}

/// `str.splitlines()` (without keeping line ends).
fn py_splitlines(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let (mut start, mut chars) = (0, s.char_indices().peekable());
    while let Some((i, c)) = chars.next() {
        let is_break = matches!(
            c,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if is_break {
            lines.push(&s[start..i]);
            start = i + c.len_utf8();
            if c == '\r' && matches!(chars.peek(), Some((_, '\n'))) {
                chars.next();
                start += 1;
            }
        }
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

const NAMED: [(&str, &str); 56] = [
    ("amp", "&"),
    ("lt", "<"),
    ("gt", ">"),
    ("quot", "\""),
    ("apos", "'"),
    ("nbsp", "\u{a0}"),
    ("copy", "\u{a9}"),
    ("reg", "\u{ae}"),
    ("trade", "\u{2122}"),
    ("hellip", "\u{2026}"),
    ("mdash", "\u{2014}"),
    ("ndash", "\u{2013}"),
    ("lsquo", "\u{2018}"),
    ("rsquo", "\u{2019}"),
    ("ldquo", "\u{201c}"),
    ("rdquo", "\u{201d}"),
    ("sbquo", "\u{201a}"),
    ("bdquo", "\u{201e}"),
    ("bull", "\u{2022}"),
    ("middot", "\u{b7}"),
    ("laquo", "\u{ab}"),
    ("raquo", "\u{bb}"),
    ("euro", "\u{20ac}"),
    ("pound", "\u{a3}"),
    ("yen", "\u{a5}"),
    ("cent", "\u{a2}"),
    ("sect", "\u{a7}"),
    ("deg", "\u{b0}"),
    ("plusmn", "\u{b1}"),
    ("times", "\u{d7}"),
    ("divide", "\u{f7}"),
    ("para", "\u{b6}"),
    ("iexcl", "\u{a1}"),
    ("iquest", "\u{bf}"),
    ("eacute", "\u{e9}"),
    ("egrave", "\u{e8}"),
    ("agrave", "\u{e0}"),
    ("aacute", "\u{e1}"),
    ("ccedil", "\u{e7}"),
    ("ouml", "\u{f6}"),
    ("uuml", "\u{fc}"),
    ("auml", "\u{e4}"),
    ("szlig", "\u{df}"),
    ("ntilde", "\u{f1}"),
    ("oacute", "\u{f3}"),
    ("iacute", "\u{ed}"),
    ("uacute", "\u{fa}"),
    ("ecirc", "\u{ea}"),
    ("acirc", "\u{e2}"),
    ("larr", "\u{2190}"),
    ("rarr", "\u{2192}"),
    ("uarr", "\u{2191}"),
    ("darr", "\u{2193}"),
    ("check", "\u{2713}"),
    ("zwnj", "\u{200c}"),
    ("shy", "\u{ad}"),
];
/// Names HTML5 also recognises without a trailing `;`.
const LEGACY: [&str; 7] = ["amp", "lt", "gt", "quot", "nbsp", "copy", "reg"];
const CP1252: [char; 32] = [
    '\u{20ac}', '\u{81}', '\u{201a}', '\u{192}', '\u{201e}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{2c6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8d}', '\u{17d}', '\u{8f}',
    '\u{90}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{2dc}', '\u{2122}', '\u{161}', '\u{203a}', '\u{153}', '\u{9d}', '\u{17e}', '\u{178}',
];

fn numeric_char(code: u32) -> char {
    match code {
        0x80..=0x9f => CP1252[(code - 0x80) as usize],
        0 | 0xd800..=0xdfff => '\u{fffd}',
        c => char::from_u32(c).unwrap_or('\u{fffd}'),
    }
}

/// `html.unescape` for numeric references and the common named entities.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let tail = &rest[p + 1..];
        if let Some(num) = tail.strip_prefix('#') {
            let (radix, digits) = match num.strip_prefix(['x', 'X']) {
                Some(h) => (16, h),
                None => (10, num),
            };
            let len = digits.chars().take_while(|c| c.is_digit(radix)).count();
            if len > 0 {
                let code = u32::from_str_radix(&digits[..len], radix).unwrap_or(0x11_0000);
                out.push(numeric_char(code));
                let used = (num.len() - digits.len()) + len;
                let after = &num[used..];
                rest = after.strip_prefix(';').unwrap_or(after);
                continue;
            }
        } else {
            let len = tail
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .count();
            let name = &tail[..len];
            let has_semicolon = tail[len..].starts_with(';');
            let hit = NAMED
                .iter()
                .find(|(n, _)| *n == name)
                .filter(|(n, _)| has_semicolon || LEGACY.contains(n));
            if let Some((_, text)) = hit {
                out.push_str(text);
                rest = &tail[len + usize::from(has_semicolon)..];
                continue;
            }
        }
        out.push('&');
        rest = tail;
    }
    out.push_str(rest);
    out
}

// -------------------------------------------------------------------- prompt

/// `text[:limit - 1] + "…"` when longer than `limit` characters.
pub fn trim(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

pub fn filter_excluded(messages: Vec<MailMessage>, exclude: &[String]) -> Vec<MailMessage> {
    messages
        .into_iter()
        .filter(|m| !sender_matches(&parse_address(&m.from), exclude))
        .collect()
}

pub fn build_prompt(
    instructions: &str,
    inbox: &Value,
    categories: &[Value],
    messages: &[MailMessage],
) -> String {
    let cats: Vec<Value> = categories
        .iter()
        .map(|c| {
            json!({
                "key": c.get("key").cloned().unwrap_or(Value::Null),
                "description": c.get("description").cloned().unwrap_or(Value::Null),
                "draft": c.get("draft").is_some_and(crate::loops_config::py_truthy),
            })
        })
        .collect();
    let payload = json!({
        "account": inbox.get("account").cloned().unwrap_or(Value::Null),
        "urgent_brief": inbox.get("urgent_brief").cloned().unwrap_or_else(|| json!("")),
        "categories": cats,
        "messages": messages.iter().map(MailMessage::prompt_value).collect::<Vec<_>>(),
    });
    format!(
        "{}\n\n## Input\n\n```json\n{}\n```\n",
        instructions.trim_end(),
        serde_json::to_string_pretty(&payload).unwrap_or_default()
    )
}

// ------------------------------------------------------------------ response

/// The model's JSON array: from a fenced block, else the outermost `[...]`.
pub fn extract_json_array(text: &str) -> Result<Vec<Value>, TriageResponseError> {
    let text = text.trim();
    let inner = fenced_block(text).unwrap_or(text);
    let (Some(start), Some(end)) = (inner.find('['), inner.rfind(']')) else {
        return err("no JSON array in the AI response");
    };
    if end < start {
        return err("no JSON array in the AI response");
    }
    match serde_json::from_str::<Value>(&inner[start..=end]) {
        Ok(Value::Array(a)) => Ok(a),
        Ok(_) => err("AI response is not a JSON array"),
        Err(_) => err("AI response is not valid JSON: Expecting value"),
    }
}

/// `re.search(r"```(?:json)?\s*\n(.*?)\n```", text, re.DOTALL)`, stripped.
fn fenced_block(text: &str) -> Option<&str> {
    let mut from = 0;
    while let Some(p) = text[from..].find("```") {
        let open = from + p + 3;
        let mut starts = Vec::new();
        if text[open..].starts_with("json") {
            starts.push(open + 4);
        }
        starts.push(open);
        for start in starts {
            // `\s*\n` backtracks to the last newline of the whitespace run.
            let ws_len: usize = text[start..]
                .chars()
                .take_while(|c| c.is_whitespace())
                .map(char::len_utf8)
                .sum();
            if let Some(nl) = text[start..start + ws_len].rfind('\n') {
                let body = start + nl + 1;
                if let Some(q) = text[body..].find("\n```") {
                    return Some(text[body..body + q].trim());
                }
            }
        }
        from += p + 1;
    }
    None
}

/// An AI-supplied value, safe to put in an error message: its repr, bounded.
fn brief(value: &Value) -> String {
    trim(&py_repr(value), ERROR_VALUE_LIMIT)
}

pub fn parse_response(
    text: &str,
    messages: &[MailMessage],
    categories: &[Value],
) -> Result<Vec<Decision>, TriageResponseError> {
    let entries = extract_json_array(text)?;
    let by_key: Map<String, Value> = categories
        .iter()
        .filter_map(|c| Some((c.get("key")?.as_str()?.to_string(), c.clone())))
        .collect();
    let expected: Vec<&str> = messages.iter().map(|m| m.id.as_str()).collect();
    let mut decisions: Vec<Decision> = Vec::new();
    for entry in &entries {
        let Some(entry) = entry.as_object() else {
            return err("every entry must be an object");
        };
        let id_val = entry.get("id").cloned().unwrap_or(Value::Null);
        let Some(id) = id_val.as_str().filter(|i| expected.contains(i)) else {
            return err(format!("unknown id {}", brief(&id_val)));
        };
        if decisions.iter().any(|d| d.id == id) {
            return err(format!("duplicate id {}", brief(&id_val)));
        }
        let cat_val = entry.get("category").cloned().unwrap_or(Value::Null);
        let Some(category) = cat_val.as_str().filter(|c| by_key.contains_key(*c)) else {
            return err(format!("unknown category {} for {id}", brief(&cat_val)));
        };
        let reason = match entry.get("reason") {
            Some(v) if crate::loops_config::py_truthy(v) => match v.as_str() {
                Some(s) => s.to_string(),
                None => return err(reason_error(id)),
            },
            _ => String::new(),
        };
        if reason.chars().count() > REASON_LIMIT {
            return err(reason_error(id));
        }
        let mut draft = entry.get("draft_body").cloned().filter(|v| !v.is_null());
        if draft
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(|s| s.trim().is_empty())
        {
            draft = None;
        }
        let draft_body = match draft {
            None => None,
            Some(v) => {
                let Some(s) = v.as_str().filter(|s| s.chars().count() <= DRAFT_LIMIT) else {
                    return err(format!(
                        "draft_body for {id} must be a string of at most {DRAFT_LIMIT} chars"
                    ));
                };
                if !by_key[category]
                    .get("draft")
                    .is_some_and(crate::loops_config::py_truthy)
                {
                    return err(format!(
                        "draft_body is not allowed for category {} ({id})",
                        brief(&cat_val)
                    ));
                }
                Some(s.to_string())
            }
        };
        decisions.push(Decision {
            id: id.to_string(),
            category: category.to_string(),
            reason,
            draft_body,
            needs_manual_reply: false,
        });
    }
    let missing: Vec<&str> = expected
        .iter()
        .copied()
        .filter(|id| !decisions.iter().any(|d| d.id == *id))
        .collect();
    if !missing.is_empty() {
        return err(format!("missing decisions for {}", missing.join(", ")));
    }
    Ok(expected
        .iter()
        .map(|id| {
            decisions
                .iter()
                .find(|d| d.id == *id)
                .expect("present")
                .clone()
        })
        .collect())
}

fn reason_error(id: &str) -> String {
    format!("reason for {id} must be a string of at most {REASON_LIMIT} chars")
}

/// Deterministic overrides the prompt is not trusted with: VIP senders are
/// always urgent, and the user's own mail never gets a reply draft.
pub fn apply_rules(
    decisions: Vec<Decision>,
    messages: &[MailMessage],
    inbox: &Value,
) -> Vec<Decision> {
    let own = inbox
        .get("account")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let vips = string_list(inbox, "vip_senders");
    decisions
        .into_iter()
        .map(|mut d| {
            let sender = messages
                .iter()
                .find(|m| m.id == d.id)
                .map(|m| parse_address(&m.from))
                .unwrap_or_default();
            if sender == own {
                d.draft_body = None;
                d.needs_manual_reply = false;
                return d;
            }
            if sender_matches(&sender, &vips) && d.category != "urgent" {
                d.category = "urgent".into();
                d.reason = trim(&format!("VIP sender. {}", d.reason), REASON_LIMIT);
            }
            d.needs_manual_reply = d.category == "urgent" && d.draft_body.is_none();
            d
        })
        .collect()
}

/// `{category: n}` in first-seen order.
pub fn count_by_category(decisions: &[Decision]) -> Map<String, Value> {
    let mut counts = Map::new();
    for d in decisions {
        let n = counts.get(&d.category).and_then(Value::as_i64).unwrap_or(0);
        counts.insert(d.category.clone(), json!(n + 1));
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox_config::default_categories;

    fn cats() -> Vec<Value> {
        default_categories().as_array().unwrap().clone()
    }

    fn inbox() -> Value {
        json!({"name": "w", "account": "me@example.com", "urgent_brief": "Clients are urgent.",
               "vip_senders": ["@vip.com"], "exclude_senders": ["@spam.com"]})
    }

    fn msg(i: u32, sender: &str) -> MailMessage {
        MailMessage {
            id: format!("m{i}"),
            thread_id: format!("t{i}"),
            from: sender.into(),
            to: "me@example.com".into(),
            subject: format!("Subject {i}"),
            date: "2026-09-27T08:00:00+00:00".into(),
            body_text: format!("Body {i}"),
            message_id_header: "<x@y>".into(),
            ..Default::default()
        }
    }

    fn m(i: u32) -> MailMessage {
        msg(i, "Alice <alice@x.com>")
    }

    #[test]
    fn html_to_text_strips_tags_scripts_and_unescapes() {
        let markup = "<html><style>p{}</style><script>alert(1)</script><p>Hello&nbsp;<b>there</b></p><br>Line&amp;2<div>Three</div></html>";
        let text = html_to_text(markup);
        assert!(!text.contains("alert") && !text.contains("p{}"));
        assert!(text.contains("Hello\u{a0}there") || text.contains("Hello there"));
        assert!(text.contains("Line&2"));
        assert_eq!(text.lines().last().unwrap().trim(), "Three");
    }

    #[test]
    fn html_to_text_edge_cases() {
        assert_eq!(html_to_text(""), "");
        assert_eq!(html_to_text("a < b and c > d"), "a < b and c > d");
        assert_eq!(
            html_to_text("<head><title>T</title></head><body>Hi</body>"),
            "Hi"
        );
        assert_eq!(html_to_text("<!-- c --><p>x</p><!DOCTYPE html>"), "x");
        assert_eq!(html_to_text("<a href=\"x>y\">link</a>"), "link");
        assert_eq!(html_to_text("a<br/>b"), "a\nb");
        assert_eq!(
            html_to_text("  many   spaces\t\there  "),
            "many spaces here"
        );
        assert_eq!(
            html_to_text("<p>&lt;b&gt; &#65;&#x42; &amp;amp; &copy;</p>"),
            "<b> AB & ©"
        );
        assert_eq!(html_to_text("<script>never</script>after"), "after");
        assert_eq!(html_to_text("<SCRIPT>x</SCRIPT>ok"), "ok");
    }

    #[test]
    fn unescape_handles_numeric_named_and_unknown() {
        assert_eq!(
            unescape("&amp;&lt;&gt;&quot;&#39;&#x41;&unknown; &amp"),
            "&<>\"'A&unknown; &"
        );
        assert_eq!(unescape("&#0;&#xD800;&#150;"), "\u{fffd}\u{fffd}\u{2013}");
        assert_eq!(unescape("a & b"), "a & b");
    }

    #[test]
    fn trim_counts_characters() {
        assert_eq!(trim("abc", 5), "abc");
        assert_eq!(trim("abcdef", 4), "abc\u{2026}");
        assert_eq!(trim(&"x".repeat(10_000), 4000).chars().count(), 4000);
        assert_eq!(trim("ééééé", 3), "éé\u{2026}");
    }

    #[test]
    fn filter_excluded_drops_matching_senders() {
        let kept = filter_excluded(
            vec![m(1), msg(2, "Bot <bot@spam.com>")],
            &["@spam.com".into()],
        );
        assert_eq!(
            kept.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["m1"]
        );
    }

    #[test]
    fn build_prompt_contains_payload_but_no_private_keys() {
        let prompt = build_prompt("INSTRUCTIONS\n\n", &inbox(), &cats(), &[m(1)]);
        assert!(prompt.starts_with("INSTRUCTIONS\n\n## Input\n\n```json\n"));
        assert!(prompt.ends_with("\n```\n"));
        let body = prompt
            .split_once("```json\n")
            .unwrap()
            .1
            .rsplit_once("\n```")
            .unwrap()
            .0;
        let payload: Value = serde_json::from_str(body).unwrap();
        assert_eq!(payload["account"], "me@example.com");
        assert_eq!(payload["urgent_brief"], "Clients are urgent.");
        assert_eq!(payload["categories"].as_array().unwrap().len(), 5);
        assert_eq!(
            payload["categories"][0],
            json!({"key": "urgent", "description": cats()[0]["description"], "draft": true})
        );
        assert_eq!(payload["messages"][0]["id"], "m1");
        assert!(payload["messages"][0].get("_message_id_header").is_none());
        assert!(prompt.contains("Body 1"));
    }

    #[test]
    fn prompt_keeps_non_ascii_unescaped() {
        let mut one = m(1);
        one.subject = "Réunion 会议".into();
        assert!(build_prompt("I", &inbox(), &cats(), &[one]).contains("Réunion 会议"));
    }

    #[test]
    fn extract_json_array_handles_fences_and_prose() {
        let raw = r#"[{"id": "m1"}]"#;
        let want = vec![json!({"id": "m1"})];
        assert_eq!(extract_json_array(raw).unwrap(), want);
        assert_eq!(
            extract_json_array(&format!("```json\n{raw}\n```")).unwrap(),
            want
        );
        assert_eq!(
            extract_json_array(&format!("```\n{raw}\n```")).unwrap(),
            want
        );
        assert_eq!(
            extract_json_array(&format!("Here you go:\n{raw}\nDone.")).unwrap(),
            want
        );
        assert_eq!(
            extract_json_array(&format!("x ```json\n  \n{raw}\n``` y")).unwrap(),
            want
        );
        for bad in [r#"[{"id": "m1""#, "no json here", r#"{"id": "m1"}"#] {
            assert!(extract_json_array(bad).is_err(), "{bad}");
        }
    }

    fn resp(entries: Vec<Value>) -> String {
        Value::Array(entries).to_string()
    }

    fn entry(id: &str, cat: &str, reason: &str, draft: Value) -> Value {
        json!({"id": id, "category": cat, "reason": reason, "draft_body": draft})
    }

    #[test]
    fn parse_response_valid_in_message_order() {
        let text = resp(vec![
            entry("m2", "fyi", "update", Value::Null),
            entry("m1", "urgent", "deadline", json!("On it.")),
        ]);
        let d = parse_response(&text, &[m(1), m(2)], &cats()).unwrap();
        assert_eq!(
            d.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["m1", "m2"]
        );
        assert_eq!(d[0].draft_body.as_deref(), Some("On it."));
    }

    #[test]
    fn parse_response_rejects_invalid() {
        let ok2 = entry("m2", "fyi", "r", Value::Null);
        let cases: Vec<(Vec<Value>, &str)> = vec![
            (vec![entry("m1", "fyi", "r", Value::Null)], "missing"),
            (
                vec![
                    entry("m1", "fyi", "r", Value::Null),
                    ok2.clone(),
                    entry("zzz", "fyi", "r", Value::Null),
                ],
                "unknown id",
            ),
            (
                vec![
                    entry("m1", "fyi", "r", Value::Null),
                    entry("m1", "fyi", "r", Value::Null),
                ],
                "duplicate",
            ),
            (
                vec![entry("m1", "spam", "r", Value::Null), ok2.clone()],
                "category",
            ),
            (
                vec![entry("m1", "fyi", "r", json!("hi")), ok2.clone()],
                "draft",
            ),
            (
                vec![
                    entry("m1", "fyi", &"r".repeat(201), Value::Null),
                    ok2.clone(),
                ],
                "reason",
            ),
            (
                vec![entry("m1", "urgent", "r", json!("d".repeat(4001))), ok2],
                "draft",
            ),
        ];
        for (entries, fragment) in cases {
            let e = parse_response(&resp(entries), &[m(1), m(2)], &cats()).unwrap_err();
            assert!(e.0.contains(fragment), "{e}");
        }
    }

    #[test]
    fn blank_draft_becomes_none_and_missing_reason_is_empty() {
        let d = parse_response(
            &resp(vec![
                json!({"id": "m1", "category": "urgent", "draft_body": "   "}),
            ]),
            &[m(1)],
            &cats(),
        )
        .unwrap();
        assert_eq!(
            (d[0].draft_body.clone(), d[0].reason.clone()),
            (None, String::new())
        );
    }

    #[test]
    fn non_object_entries_and_bad_types_are_response_errors() {
        assert!(parse_response("[1]", &[m(1)], &cats()).is_err());
        for (field, value) in [
            ("id", json!(["m1"])),
            ("id", json!({"a": 1})),
            ("category", json!(["fyi"])),
            ("category", json!({"k": "v"})),
            ("id", json!(1)),
            ("reason", json!(5)),
        ] {
            let mut e = entry("m1", "fyi", "r", Value::Null);
            e[field] = value;
            assert!(
                parse_response(&resp(vec![e]), &[m(1)], &cats()).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn ai_values_are_bounded_in_error_messages() {
        for e in [
            entry(&"x".repeat(500), "fyi", "r", Value::Null),
            entry("m1", &"y".repeat(500), "r", Value::Null),
        ] {
            let err = parse_response(&resp(vec![e]), &[m(1)], &cats()).unwrap_err();
            assert!(err.0.len() < 120, "{err}");
        }
    }

    fn d(id: &str, cat: &str, reason: &str, draft: Option<&str>) -> Decision {
        Decision {
            id: id.into(),
            category: cat.into(),
            reason: reason.into(),
            draft_body: draft.map(Into::into),
            needs_manual_reply: false,
        }
    }

    #[test]
    fn vip_forces_urgent_and_flags_manual_reply() {
        let out = apply_rules(
            vec![d("m1", "fyi", "update", None)],
            &[msg(1, "Boss <boss@vip.com>")],
            &inbox(),
        );
        assert_eq!(out[0].category, "urgent");
        assert!(out[0].reason.starts_with("VIP sender"));
        assert!(out[0].needs_manual_reply);
    }

    #[test]
    fn own_address_never_gets_a_draft() {
        let out = apply_rules(
            vec![d("m1", "urgent", "r", Some("Reply to myself"))],
            &[msg(1, "Me <ME@example.com>")],
            &inbox(),
        );
        assert_eq!(out[0].draft_body, None);
        assert!(!out[0].needs_manual_reply);
    }

    #[test]
    fn urgent_without_draft_needs_manual_reply() {
        let out = apply_rules(vec![d("m1", "urgent", "r", None)], &[m(1)], &inbox());
        assert!(out[0].needs_manual_reply);
        let out = apply_rules(vec![d("m1", "urgent", "r", Some("hi"))], &[m(1)], &inbox());
        assert!(!out[0].needs_manual_reply);
    }

    #[test]
    fn vip_reason_is_capped() {
        let long = "r".repeat(200);
        let out = apply_rules(
            vec![d("m1", "fyi", &long, None)],
            &[msg(1, "b@vip.com")],
            &inbox(),
        );
        assert_eq!(out[0].reason.chars().count(), REASON_LIMIT);
    }

    #[test]
    fn counts_by_category_in_first_seen_order() {
        let c = count_by_category(&[
            d("a", "fyi", "", None),
            d("b", "fyi", "", None),
            d("c", "urgent", "", None),
        ]);
        assert_eq!(Value::Object(c), json!({"fyi": 2, "urgent": 1}));
    }
}
