//! Python-compatible string primitives the Python dashboard's output depends on:
//! `html.escape`, `json.dumps` of a string, `urllib.parse.quote`/`parse_qs`,
//! `str.format` with named fields, `str.splitlines`. Byte-for-byte equality with
//! the Python pages is the whole point, so these mirror the stdlib exactly.

use loopx_core::pydumps;

/// `html.escape(s, quote=True)`.
pub fn escape(s: &str) -> String {
    escape_with(s, true)
}

/// `html.escape(s, quote=False)`: only `&`, `<`, `>`.
pub fn escape_text(s: &str) -> String {
    escape_with(s, false)
}

pub fn escape_with(s: &str, quote: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if quote => out.push_str("&quot;"),
            '\'' if quote => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

/// `json.dumps(s)` for a string (quotes included, non-ASCII as `\uXXXX`).
pub fn json_str(s: &str) -> String {
    pydumps::dumps(&s)
}

/// `urllib.parse.quote(s, safe=safe)`.
pub fn quote(s: &str, safe: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(b, b'_' | b'.' | b'-' | b'~')
            || (b.is_ascii() && safe.as_bytes().contains(&b));
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `urllib.parse.unquote(s)` (UTF-8, invalid sequences become U+FFFD).
pub fn unquote(s: &str) -> String {
    percent_decode(s.as_bytes(), false)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(bytes: &[u8], plus_is_space: bool) -> String {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len()
                && hex_val(bytes[i + 1]).is_some()
                && hex_val(bytes[i + 2]).is_some() =>
            {
                out.push(hex_val(bytes[i + 1]).unwrap() << 4 | hex_val(bytes[i + 2]).unwrap());
                i += 3;
            }
            b'+' if plus_is_space => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `urllib.parse.parse_qs(s)` (blank values dropped, as the stdlib does by
/// default): `(name, values)` pairs in first-seen order.
pub fn parse_qs(s: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for pair in s.split('&') {
        if pair.is_empty() {
            continue;
        }
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        let name = percent_decode(name.as_bytes(), true);
        let value = percent_decode(value.as_bytes(), true);
        match out.iter_mut().find(|(n, _)| *n == name) {
            Some((_, values)) => values.push(value),
            None => out.push((name, vec![value])),
        }
    }
    out
}

/// A parsed query string or form body with the lookups pages need.
#[derive(Debug, Clone, Default)]
pub struct Form(Vec<(String, Vec<String>)>);

impl Form {
    pub fn parse(s: &str) -> Self {
        Self(parse_qs(s))
    }

    pub fn from_bytes(b: &[u8]) -> Self {
        Self::parse(&String::from_utf8_lossy(b))
    }

    /// First value of `name` (`query.get(name, [None])[0]`).
    pub fn first(&self, name: &str) -> Option<&str> {
        self.all(name).first().map(String::as_str)
    }

    pub fn all(&self, name: &str) -> &[String] {
        self.0
            .iter()
            .find(|(n, _)| n == name)
            .map_or(&[][..], |(_, v)| v.as_slice())
    }
}

/// `text.format(**kwargs)` for the `{name}` / `{{` / `}}` subset the dashboard
/// uses. An unknown `{name}` is left as written (Python would raise).
pub fn format_named(text: &str, args: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(['{', '}']) {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        if let Some(after) = tail.strip_prefix("{{") {
            out.push('{');
            rest = after;
        } else if let Some(after) = tail.strip_prefix("}}") {
            out.push('}');
            rest = after;
        } else if tail.starts_with('{') {
            match tail.find('}') {
                Some(end) => {
                    let name = &tail[1..end];
                    match args.iter().find(|(n, _)| *n == name) {
                        Some((_, v)) => out.push_str(v),
                        None => out.push_str(&tail[..=end]),
                    }
                    rest = &tail[end + 1..];
                }
                None => {
                    out.push_str(tail);
                    rest = "";
                }
            }
        } else {
            out.push('}');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

/// `str.splitlines()`: splits on every Unicode line boundary Python knows,
/// `\r\n` counting once, without keeping the terminators.
pub fn splitlines(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
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
            let mut next = i + c.len_utf8();
            if c == '\r' && matches!(chars.peek(), Some((_, '\n'))) {
                chars.next();
                next += 1;
            }
            start = next;
        }
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

/// Python's `str.isspace()`-style whitespace for `\s` in `re` patterns and `strip()`.
pub fn is_py_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

/// `str.strip()`.
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_matches_python() {
        assert_eq!(
            escape("<a href=\"x\">'&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&#x27;&amp;&#x27;&lt;/a&gt;"
        );
        assert_eq!(escape_text("\"'<"), "\"'&lt;");
    }

    #[test]
    fn json_str_is_ascii_only() {
        assert_eq!(
            json_str("a\"b\n日本😀"),
            "\"a\\\"b\\n\\u65e5\\u672c\\ud83d\\ude00\""
        );
    }

    #[test]
    fn quote_and_unquote() {
        assert_eq!(quote("a b/ü?", "/"), "a%20b/%C3%BC%3F");
        assert_eq!(quote("a/b", ""), "a%2Fb");
        assert_eq!(unquote("a%20b%C3%BC%zz"), "a bü%zz");
    }

    #[test]
    fn parse_qs_drops_blanks_and_decodes_plus() {
        let f = Form::parse("a=1&b=&c=x+y%21&a=2&d");
        assert_eq!(f.all("a"), ["1", "2"]);
        assert_eq!(f.first("b"), None);
        assert_eq!(f.first("c"), Some("x y!"));
        assert_eq!(f.first("d"), None);
    }

    #[test]
    fn format_named_handles_braces() {
        assert_eq!(format_named("{a} {{b}} {c}", &[("a", "1")]), "1 {b} {c}");
    }

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(splitlines("a\nb\r\nc\rd"), ["a", "b", "c", "d"]);
        assert_eq!(splitlines("a\n"), ["a"]);
        assert_eq!(splitlines("a\n\nb"), ["a", "", "b"]);
        assert_eq!(splitlines(""), Vec::<&str>::new());
    }
}
