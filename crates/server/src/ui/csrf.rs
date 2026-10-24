//! CSRF protection, as in `docs/contract/http-api.md` section 2: one random token
//! per server process, embedded only in pages this server renders, required in
//! every POST body (urlencoded form field `csrf_token`, or a JSON object with a
//! `csrf_token` string for programmatic clients).

use super::pyfmt;
use loopx_core::sha256;
use serde_json::Value;

/// `secrets.token_urlsafe(32)`.
pub fn new_token() -> String {
    sha256::base64url(&sha256::random_bytes(32))
}

/// The hidden form field every page's POST forms carry.
pub fn input_html(token: &str) -> String {
    format!(
        "<input type='hidden' name='csrf_token' value=\"{}\">",
        pyfmt::escape(token)
    )
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Does the request body carry the right token? A missing or empty token never
/// matches; comparison is over bytes in constant time, so non-ASCII input
/// cannot fail loudly or leak how much matched.
pub fn check(expected: &str, body: &[u8], content_type: Option<&str>) -> bool {
    let is_json = content_type.is_some_and(|c| c.to_ascii_lowercase().contains("json"));
    let submitted = if is_json {
        serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| {
                v.get("csrf_token")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default()
    } else {
        pyfmt::Form::from_bytes(body)
            .first("csrf_token")
            .unwrap_or("")
            .to_string()
    };
    !submitted.is_empty()
        && !expected.is_empty()
        && constant_time_eq(submitted.as_bytes(), expected.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_url_safe_and_unique() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn form_and_json_bodies() {
        assert!(check("tok", b"a=1&csrf_token=tok", None));
        assert!(check(
            "tok",
            br#"{"csrf_token": "tok"}"#,
            Some("application/json")
        ));
        assert!(!check("tok", b"csrf_token=nope", None));
        assert!(!check("tok", b"csrf_token=", None));
        assert!(!check("tok", b"", None));
        assert!(!check("tok", b"csrf_token=t%C3%B6k", None));
        assert!(!check(
            "tok",
            br#"{"csrf_token": 1}"#,
            Some("application/json")
        ));
        assert!(!check("", b"csrf_token=", None));
    }

    #[test]
    fn input_is_escaped() {
        assert_eq!(
            input_html("a\"b"),
            "<input type='hidden' name='csrf_token' value=\"a&quot;b\">"
        );
    }
}
