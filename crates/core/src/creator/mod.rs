//! Creator: video-scout catalogs, video files and agent jobs.

pub mod jobs;
pub mod logfmt;
pub mod notify;
pub mod progress;
pub mod scout;
pub mod spawn;
pub mod state;
pub mod uploads;
pub mod videos;

/// The platforms a video can carry publish text for: the page's key, the skill's id
/// (`config/platforms.yaml`) and whether the copy is English.
pub const PLATFORMS: [(&str, &str, bool); 4] = [
    ("wechat", "wechat_channels", false),
    ("douyin", "douyin", false),
    ("linkedin", "linkedin", true),
    ("facebook", "facebook", true),
];

/// What a new video job is made for unless the user picks: the two Chinese platforms.
pub fn default_platforms() -> Vec<String> {
    vec!["wechat".to_string(), "douyin".to_string()]
}

/// The picked platform keys, de-duplicated and in the page's order. An unknown key or an
/// empty pick is an error.
pub fn parse_platforms<S: AsRef<str>>(picked: &[S]) -> Result<Vec<String>, String> {
    if let Some(bad) = picked
        .iter()
        .map(AsRef::as_ref)
        .find(|p| !PLATFORMS.iter().any(|(k, _, _)| k == p))
    {
        return Err(format!("unknown platform: {bad}"));
    }
    let chosen: Vec<String> = PLATFORMS
        .iter()
        .filter(|(k, _, _)| picked.iter().any(|p| p.as_ref() == *k))
        .map(|(k, _, _)| k.to_string())
        .collect();
    if chosen.is_empty() {
        return Err("pick at least one platform".to_string());
    }
    Ok(chosen)
}

/// Choices that ride with a video job and are kept in its record, so a retry makes the
/// same video.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoOptions {
    /// Platform keys (see [`PLATFORMS`]) the video gets publish text for.
    pub platforms: Vec<String>,
    /// Showcase only: the user's screenshots, staged under the skill's `output/uploads/`
    /// (a path relative to the skill directory; see [`uploads`]).
    pub screenshots: Option<String>,
}

impl Default for VideoOptions {
    fn default() -> Self {
        VideoOptions {
            platforms: default_platforms(),
            screenshots: None,
        }
    }
}

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

/// Tidy a pasted video link before it is validated: share links carry tracking
/// and playback parameters (`&t=`, `&list=`, `?si=`, `?utm_source=`) that the strict
/// check refuses. YouTube keeps only its video id, the other known video hosts lose
/// their query string, and an address that is not `https://` is returned untouched
/// for [`valid_source_url`] to refuse.
pub fn normalize_source_url(url: &str) -> String {
    let url = url.trim();
    let Some(rest) = url
        .get(..8)
        .filter(|s| s.eq_ignore_ascii_case("https://"))
        .map(|_| &url[8..])
    else {
        return url.to_string();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (host, tail) = rest.split_at(end);
    let bare = host.to_ascii_lowercase();
    let bare = bare.trim_start_matches("www.").trim_start_matches("m.");
    let tail = tail.split('#').next().unwrap_or("");
    let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
    let youtube = matches!(bare, "youtube.com" | "music.youtube.com");
    let keep = if youtube && path == "/watch" {
        query
            .split('&')
            .find(|kv| kv.starts_with("v=") && kv.len() > 2)
            .map(|kv| format!("?{kv}"))
    } else {
        None
    };
    let known = youtube
        || matches!(
            bare,
            "youtu.be"
                | "ted.com"
                | "x.com"
                | "twitter.com"
                | "vimeo.com"
                | "bilibili.com"
                | "b23.tv"
                | "tiktok.com"
                | "instagram.com"
                | "facebook.com"
                | "fb.watch"
                | "twitch.tv"
                | "dailymotion.com"
                | "reddit.com"
                | "linkedin.com"
        );
    match (keep, known) {
        (Some(q), _) => format!("https://{host}{path}{q}"),
        (None, true) => format!("https://{host}{path}"),
        (None, false) => {
            // Any other host: only drop the well-known tracking parameters.
            let kept: Vec<&str> = query
                .split('&')
                .filter(|kv| !kv.is_empty())
                .filter(|kv| {
                    let k = kv.split('=').next().unwrap_or("");
                    !(k.starts_with("utm_")
                        || matches!(k, "si" | "fbclid" | "gclid" | "feature" | "ref"))
                })
                .collect();
            if kept.is_empty() {
                format!("https://{host}{path}")
            } else {
                format!("https://{host}{path}?{}", kept.join("&"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn normalize_trims_share_links_down_to_the_video() {
        use super::normalize_source_url as n;
        assert_eq!(
            n("https://www.youtube.com/watch?v=abc123&t=42s&list=PL1"),
            "https://www.youtube.com/watch?v=abc123"
        );
        assert_eq!(
            n("https://youtu.be/abc123?si=xyz&t=9"),
            "https://youtu.be/abc123"
        );
        assert_eq!(
            n("https://x.com/user/status/1?s=20&t=q"),
            "https://x.com/user/status/1"
        );
        assert_eq!(
            n("https://www.ted.com/talks/a_b?utm_source=x&language=en"),
            "https://www.ted.com/talks/a_b"
        );
        assert_eq!(
            n("https://example.com/v?utm_source=a&id=7"),
            "https://example.com/v?id=7"
        );
        assert_eq!(n("  http://x.com/a?b=1&c=2 "), "http://x.com/a?b=1&c=2");
    }

    use super::*;

    #[test]
    fn platforms_are_parsed_in_the_pages_order_without_duplicates_and_never_empty() {
        assert_eq!(default_platforms(), ["wechat", "douyin"]);
        assert_eq!(
            parse_platforms(&["facebook", "wechat", "facebook"]).unwrap(),
            ["wechat", "facebook"]
        );
        assert!(parse_platforms::<&str>(&[])
            .unwrap_err()
            .contains("at least one"));
        assert!(parse_platforms(&["wechat", "tiktok"])
            .unwrap_err()
            .contains("tiktok"));
        assert_eq!(VideoOptions::default().platforms, default_platforms());
    }

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
