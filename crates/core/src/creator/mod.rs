//! Creator: video-scout catalogs, video files and agent jobs.

pub mod jobs;
pub mod scout;
pub mod spawn;
pub mod state;
pub mod videos;

/// The longest source URL a Creator job accepts.
pub const MAX_SOURCE_URL_CHARS: usize = 2048;

/// Characters a source URL may never contain. The URL rides into an agent prompt
/// and from there, possibly, into a shell command the skill runs, so anything a
/// shell gives meaning to is refused outright (percent-encoded forms are fine).
const SHELL_META: &[char] = &[
    '$', '`', ';', '|', '&', '\'', '"', '<', '>', '(', ')', '{', '}', '\\', '^',
];

/// The one check every Creator entry point (page, chat, job start) applies to a
/// source video address: `https://` only, a host, at most
/// [`MAX_SOURCE_URL_CHARS`], no whitespace or control characters and none of the
/// shell metacharacters above. Returns the URL trimmed, with the scheme and
/// host lower-cased.
pub fn valid_source_url(url: &str) -> Result<String, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("a source URL is required".to_string());
    }
    if url.chars().count() > MAX_SOURCE_URL_CHARS {
        return Err(format!(
            "the source URL is longer than {MAX_SOURCE_URL_CHARS} characters"
        ));
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("the source URL contains spaces or control characters".to_string());
    }
    if url.contains('&') {
        return Err(
            "the source URL contains '&': remove the parameters after the video id \
             (for example &list= or &t=) and try again"
                .to_string(),
        );
    }
    if let Some(c) = url.chars().find(|c| SHELL_META.contains(c)) {
        return Err(format!(
            "the source URL contains {c:?}, which is not allowed (percent-encode it)"
        ));
    }
    let rest = match url.get(..8) {
        Some(scheme) if scheme.eq_ignore_ascii_case("https://") => &url[8..],
        _ => return Err("the source must be an https:// address".to_string()),
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (host, tail) = rest.split_at(end);
    let host_ok = !host.is_empty()
        && !host.starts_with(':')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'));
    if !host_ok {
        return Err("the source URL has no valid host".to_string());
    }
    Ok(format!("https://{}{tail}", host.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_source_url_accepts_plain_https_and_normalizes() {
        for (input, want) in [
            ("https://example.com/talks/a", "https://example.com/talks/a"),
            (
                "  https://example.com/v?id=1  ",
                "https://example.com/v?id=1",
            ),
            ("HTTPS://Example.COM/Path/X", "https://example.com/Path/X"),
            (
                "https://www.youtube.com/watch?v=abc_DEF-1",
                "https://www.youtube.com/watch?v=abc_DEF-1",
            ),
            (
                "https://example.com:8443/a%20b%26c#t=10",
                "https://example.com:8443/a%20b%26c#t=10",
            ),
            ("https://example.com", "https://example.com"),
        ] {
            assert_eq!(valid_source_url(input).as_deref(), Ok(want), "{input}");
        }
    }

    #[test]
    fn valid_source_url_refuses_other_schemes_and_odd_hosts() {
        for bad in [
            "",
            "http://example.com/v",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "ftp://example.com/v",
            "example.com/v",
            "https://",
            "https:///path",
            "https://user:pw@example.com/v",
            "https://exa mple.com/v",
            "https://example.com/a b",
            "https://example.com/\u{7}",
            "https://example.com/a\nb",
        ] {
            assert!(valid_source_url(bad).is_err(), "{bad:?}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_SOURCE_URL_CHARS));
        assert!(valid_source_url(&long).is_err());
        let fits = format!(
            "https://example.com/{}",
            "a".repeat(MAX_SOURCE_URL_CHARS - "https://example.com/".len())
        );
        assert!(valid_source_url(&fits).is_ok());
    }

    #[test]
    fn an_ampersand_says_to_drop_the_extra_parameters() {
        let e = valid_source_url("https://www.youtube.com/watch?v=abc&list=xyz").unwrap_err();
        assert!(
            e.contains("remove the parameters after the video id"),
            "{e}"
        );
        assert!(e.contains("&list=") && e.contains("&t="), "{e}");
        assert!(!e.contains("percent-encode"), "{e}");
    }

    #[test]
    fn valid_source_url_refuses_every_shell_metacharacter() {
        for c in [
            '$', '`', ';', '|', '&', '\'', '"', '<', '>', '(', ')', '{', '}', '\\', '^',
        ] {
            let url = format!("https://example.com/v?x=1{c}y");
            assert!(valid_source_url(&url).is_err(), "{url}");
        }
        for bad in [
            "https://example.com/$(touch /tmp/x)",
            "https://example.com/`id`",
            "https://example.com/v;rm -rf ~",
            "https://example.com/v|sh",
            "https://example.com/watch?v=a&list=b",
        ] {
            assert!(valid_source_url(bad).is_err(), "{bad}");
        }
    }
}
