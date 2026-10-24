//! A minimal plain-text RFC 5322 message builder: what Python's
//! `email.message.EmailMessage` + `set_content(text)` produce for the reply
//! drafts ([`crate::mail_provider::gmail`]) and the chat's send tool
//! ([`crate::mail_sender`]). Header values are refused when they contain a line
//! break (header injection), long headers are folded, non-ASCII header text is
//! RFC 2047 encoded, and the body is 7bit/8bit when its lines are short, base64
//! otherwise.

use base64::engine::general_purpose::{STANDARD, URL_SAFE};
use base64::Engine;

const FOLD_AT: usize = 78;

/// `base64.urlsafe_b64encode` (padded).
pub fn urlsafe_b64(bytes: &[u8]) -> String {
    URL_SAFE.encode(bytes)
}

/// `base64.urlsafe_b64decode` as the Gmail payloads need it: characters outside
/// the alphabet and any padding are ignored; `None` on a truncated final group.
pub fn urlsafe_b64_decode(data: &str) -> Option<Vec<u8>> {
    let mut clean: Vec<u8> = data
        .bytes()
        .filter_map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => Some(b),
            b'+' => Some(b'-'),
            b'/' => Some(b'_'),
            _ => None,
        })
        .collect();
    if clean.len() % 4 == 1 {
        return None;
    }
    while clean.len() % 4 != 0 {
        clean.push(b'=');
    }
    URL_SAFE.decode(&clean).ok()
}

fn encoded_words(text: &str) -> String {
    // 45 raw bytes -> 60 base64 chars; with the 12-char wrapper that is 72 <= 75.
    let mut words = Vec::new();
    let mut chunk = String::new();
    for c in text.chars() {
        if chunk.len() + c.len_utf8() > 45 {
            words.push(std::mem::take(&mut chunk));
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(chunk);
    }
    words
        .iter()
        .map(|w| format!("=?utf-8?b?{}?=", STANDARD.encode(w.as_bytes())))
        .collect::<Vec<_>>()
        .join(" ")
}

const ADDRESS_HEADERS: [&str; 5] = ["from", "to", "cc", "reply-to", "bcc"];

fn encode_value(name: &str, value: &str) -> String {
    if value.is_ascii() {
        return value.to_string();
    }
    if !ADDRESS_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
        return encoded_words(value);
    }
    value
        .split(", ")
        .map(|item| match item.rfind('<') {
            Some(p) if !item.is_ascii() => {
                let phrase = item[..p].trim().trim_matches('"');
                format!("{} {}", encoded_words(phrase), &item[p..])
            }
            _ if !item.is_ascii() => encoded_words(item),
            _ => item.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Fold at spaces so no line (name included) exceeds [`FOLD_AT`] where possible.
fn fold(name: &str, value: &str) -> String {
    let mut out = String::new();
    let mut line = format!("{name}:");
    for (i, token) in value.split(' ').enumerate() {
        if i > 0 && line.len() + 1 + token.len() > FOLD_AT {
            out.push_str(&line);
            out.push('\n');
            line = String::new();
        }
        line.push(' ');
        line.push_str(token);
    }
    out.push_str(&line);
    out
}

/// Build `headers` + a `text/plain; charset="utf-8"` body.
pub fn build_plain(headers: &[(&str, &str)], body: &str) -> Result<Vec<u8>, String> {
    let mut out = String::new();
    for (name, value) in headers {
        if value.contains('\n') || value.contains('\r') {
            return Err(format!(
                "Header values may not contain linefeed or carriage return characters ({name})"
            ));
        }
        out.push_str(&fold(name, &encode_value(name, value)));
        out.push('\n');
    }
    let lines: Vec<&str> = body.lines().collect();
    let mut text = lines.join("\n");
    text.push('\n');
    let max_line = lines.iter().map(|l| l.len()).max().unwrap_or(0);
    let (cte, payload) = if max_line <= FOLD_AT {
        (if text.is_ascii() { "7bit" } else { "8bit" }, text)
    } else {
        let b64 = STANDARD.encode(text.as_bytes());
        let wrapped: Vec<&str> = b64
            .as_bytes()
            .chunks(76)
            .map(|c| std::str::from_utf8(c).unwrap_or(""))
            .collect();
        ("base64", wrapped.join("\n") + "\n")
    };
    out.push_str("Content-Type: text/plain; charset=\"utf-8\"\n");
    out.push_str(&format!("Content-Transfer-Encoding: {cte}\n"));
    out.push_str("MIME-Version: 1.0\n\n");
    out.push_str(&payload);
    Ok(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(b: Vec<u8>) -> String {
        String::from_utf8(b).unwrap()
    }

    #[test]
    fn plain_ascii_message() {
        let m = text(build_plain(&[("To", "a@x.com"), ("Subject", "Hi")], "Hello\nthere").unwrap());
        assert_eq!(
            m,
            "To: a@x.com\nSubject: Hi\nContent-Type: text/plain; charset=\"utf-8\"\nContent-Transfer-Encoding: 7bit\nMIME-Version: 1.0\n\nHello\nthere\n"
        );
    }

    #[test]
    fn non_ascii_body_is_8bit_and_long_lines_are_base64() {
        let m = text(build_plain(&[("Subject", "s")], "Body \u{2713}").unwrap());
        assert!(m.contains("Content-Transfer-Encoding: 8bit") && m.ends_with("Body \u{2713}\n"));
        let long = "x".repeat(200);
        let m = text(build_plain(&[("Subject", "s")], &long).unwrap());
        assert!(m.contains("Content-Transfer-Encoding: base64"));
        let payload = m.split("\n\n").nth(1).unwrap().replace('\n', "");
        assert_eq!(
            STANDARD.decode(payload).unwrap(),
            format!("{long}\n").into_bytes()
        );
    }

    #[test]
    fn non_ascii_subject_and_display_names_are_encoded() {
        let m = text(
            build_plain(
                &[
                    ("Subject", "H\u{e9}llo"),
                    ("To", "Jos\u{e9} <j@x.com>, b@x.com"),
                ],
                "b",
            )
            .unwrap(),
        );
        assert!(m.contains("Subject: =?utf-8?b?SMOpbGxv?="));
        assert!(m.contains("To: =?utf-8?b?Sm9zw6k=?= <j@x.com>, b@x.com"));
    }

    #[test]
    fn line_breaks_in_headers_are_refused() {
        assert!(build_plain(&[("Subject", "a\r\nBcc: evil@x.com")], "b").is_err());
        assert!(build_plain(&[("To", "a\nb")], "b").is_err());
    }

    #[test]
    fn long_headers_are_folded() {
        let refs = (0..12)
            .map(|i| format!("<ref{i}@example.com>"))
            .collect::<Vec<_>>()
            .join(" ");
        let m = text(build_plain(&[("References", &refs)], "b").unwrap());
        assert!(m
            .lines()
            .take_while(|l| !l.is_empty())
            .all(|l| l.len() <= 78));
        let unfolded = m.split("Content-Type").next().unwrap().replace("\n ", " ");
        assert_eq!(unfolded.trim_end(), format!("References: {refs}"));
    }

    #[test]
    fn urlsafe_base64_roundtrips_and_tolerates_missing_padding() {
        assert_eq!(urlsafe_b64(b"a?>"), "YT8-");
        assert_eq!(urlsafe_b64_decode("YT8-").unwrap(), b"a?>");
        assert_eq!(urlsafe_b64_decode("aGk").unwrap(), b"hi");
        assert_eq!(urlsafe_b64_decode("aGk=").unwrap(), b"hi");
        assert_eq!(urlsafe_b64_decode("aG k\n").unwrap(), b"hi");
        assert!(urlsafe_b64_decode("a").is_none());
        assert_eq!(urlsafe_b64_decode("").unwrap(), b"");
    }
}
