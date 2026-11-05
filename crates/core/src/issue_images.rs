//! Images in an issue's description and comments, saved to disk so the agent can
//! look at them (`loopx issues images`).
//!
//! The issue text is untrusted, so what may be fetched is narrow: only images on
//! the tracker's own host (GitLab uploads, GitHub attachments), only png / jpeg /
//! gif / webp (checked by content, not by name), a few of them, each small. The
//! tracker token goes to the GitLab instance only, never to another host.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The most images saved for one issue.
pub const MAX_IMAGES: usize = 5;
/// The largest image saved, in bytes.
pub const MAX_BYTES: usize = 5 * 1024 * 1024;

/// One image reference found in a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub url: String,
    pub alt: String,
    /// `description`, or `comment by <author>`.
    pub from: String,
}

/// An image saved under the issue's folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub file: PathBuf,
    pub alt: String,
    pub from: String,
    pub bytes: usize,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub saved: Vec<Saved>,
    pub skipped: Vec<(String, String)>,
}

impl Report {
    pub fn to_json(&self) -> Value {
        json!({
            "images": self.saved.iter().map(|s| json!({
                "file": s.file.display().to_string(),
                "alt": s.alt,
                "from": s.from,
                "bytes": s.bytes,
            })).collect::<Vec<_>>(),
            "skipped": self.skipped.iter().map(|(u, why)| json!({"url": u, "reason": why})).collect::<Vec<_>>(),
        })
    }
}

/// Where one issue's images go: `<repo_root>/outputs/issue-images/<key>`.
pub fn images_dir(repo_root: &Path, key: &str) -> PathBuf {
    let safe: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    repo_root.join("outputs").join("issue-images").join(safe)
}

/// Markdown `![alt](url)` and HTML `<img src="url" alt="alt">` images of `text`.
pub fn extract_refs(text: &str, from: &str) -> Vec<ImageRef> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if text[i..].starts_with("![") {
            if let Some(close) = text[i + 2..].find("](") {
                let alt = &text[i + 2..i + 2 + close];
                let url_start = i + 2 + close + 2;
                if let Some(end) = text[url_start..].find(')') {
                    // `(url "title")`: the url is the first word.
                    let url = text[url_start..url_start + end]
                        .split_whitespace()
                        .next()
                        .unwrap_or("");
                    if !url.is_empty() {
                        out.push(ImageRef {
                            url: url.trim_matches(|c| c == '<' || c == '>').to_string(),
                            alt: alt.trim().to_string(),
                            from: from.to_string(),
                        });
                    }
                    i = url_start + end + 1;
                    continue;
                }
            }
        } else if text[i..].to_ascii_lowercase().starts_with("<img") {
            if let Some(end) = text[i..].find('>') {
                let tag = &text[i..i + end];
                if let Some(url) = attr(tag, "src") {
                    out.push(ImageRef {
                        url,
                        alt: attr(tag, "alt").unwrap_or_default(),
                        from: from.to_string(),
                    });
                }
                i += end + 1;
                continue;
            }
        }
        i += text[i..].chars().next().map_or(1, char::len_utf8);
    }
    out
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let at = lower.find(&format!("{name}="))? + name.len() + 1;
    let rest = &tag[at..];
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let inner = &rest[1..];
        Some(inner[..inner.find(quote)?].to_string())
    } else {
        Some(rest.split_whitespace().next()?.to_string())
    }
}

/// Every image of an issue's description then its notes, de-duplicated by URL.
/// `issue` is the GitLab-shaped JSON (`description`, `notes[].body`,
/// `notes[].author.username`) both trackers are served as.
pub fn refs_of_issue(issue: &Value) -> Vec<ImageRef> {
    let mut all = Vec::new();
    if let Some(d) = issue.get("description").and_then(Value::as_str) {
        all.extend(extract_refs(d, "description"));
    }
    for n in issue
        .get("notes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let body = n.get("body").and_then(Value::as_str).unwrap_or("");
        let who = n
            .pointer("/author/username")
            .and_then(Value::as_str)
            .unwrap_or("someone");
        all.extend(extract_refs(body, &format!("comment by {who}")));
    }
    let mut seen = std::collections::HashSet::new();
    all.retain(|r| seen.insert(r.url.clone()));
    all
}

/// The extension of an image's bytes, from its magic number; `None` for anything
/// that is not png, jpeg, gif or webp (an SVG can carry script, an HTML error page
/// is not a picture).
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// Fetch and save up to [`MAX_IMAGES`] of `refs` into `dir` as `1.png`, `2.jpg`, ...
/// `fetch` returns the bytes for one reference or why it may not / could not be
/// fetched. Nothing is saved unless it is a small, real image.
pub fn download_all(
    refs: &[ImageRef],
    dir: &Path,
    fetch: &dyn Fn(&ImageRef) -> Result<Vec<u8>, String>,
) -> Result<Report, String> {
    let mut report = Report::default();
    // A re-run starts clean: no stale image from an earlier attempt.
    let _ = std::fs::remove_dir_all(dir);
    for r in refs {
        if report.saved.len() >= MAX_IMAGES {
            report
                .skipped
                .push((r.url.clone(), format!("more than {MAX_IMAGES} images")));
            continue;
        }
        let bytes = match fetch(r) {
            Ok(b) => b,
            Err(why) => {
                report.skipped.push((r.url.clone(), why));
                continue;
            }
        };
        if bytes.len() > MAX_BYTES {
            report
                .skipped
                .push((r.url.clone(), format!("larger than {} MB", MAX_BYTES >> 20)));
            continue;
        }
        let Some(ext) = sniff(&bytes) else {
            report
                .skipped
                .push((r.url.clone(), "not a png, jpeg, gif or webp image".into()));
            continue;
        };
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let file = dir.join(format!("{}.{ext}", report.saved.len() + 1));
        std::fs::write(&file, &bytes).map_err(|e| format!("{}: {e}", file.display()))?;
        report.saved.push(Saved {
            file,
            alt: r.alt.clone(),
            from: r.from.clone(),
            bytes: bytes.len(),
        });
    }
    Ok(report)
}

/// GET `url` without credentials, at most [`MAX_BYTES`] + 1 bytes of it (so the
/// caller can tell "too large" from "fits").
pub fn fetch_plain(url: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let resp = ureq::get(url).call().map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    Ok(bytes)
}

/// `scheme://host[:port]` of an absolute http(s) URL, lowercased.
fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = rest.split(['/', '?', '#']).next()?;
    if host.is_empty() || host.contains('@') {
        return None;
    }
    Some(format!("{scheme}://{}", host.to_ascii_lowercase()))
}

/// The absolute URL of a GitLab image, or why it must not be fetched. `project_web`
/// is the project's web URL (`https://host/group/project`); only that instance's own
/// host is allowed, so an issue cannot point the token at another server.
pub fn gitlab_target(url: &str, project_web: &str) -> Result<String, String> {
    let instance = origin(project_web).ok_or("the project has no web address")?;
    if url.starts_with('/') && !url.starts_with("//") {
        // `/uploads/<secret>/<file>`: relative to the project.
        return Ok(format!("{}{url}", project_web.trim_end_matches('/')));
    }
    match origin(url) {
        Some(o) if o == instance => Ok(url.to_string()),
        Some(o) => Err(format!("{o} is not the GitLab instance ({instance})")),
        None => Err("not an http(s) address".into()),
    }
}

/// Whether a GitHub image URL is on a host GitHub serves attachments from.
pub fn github_allowed(url: &str) -> Result<(), String> {
    let Some(o) = origin(url) else {
        return Err("not an http(s) address".into());
    };
    let host = o.trim_start_matches("https://");
    let ok = o.starts_with("https://")
        && (host == "github.com"
            || host == "raw.githubusercontent.com"
            || host.ends_with(".githubusercontent.com"));
    if ok {
        Ok(())
    } else {
        Err(format!("{host} is not a GitHub attachment host"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];

    #[test]
    fn markdown_and_html_images_are_found_with_their_alt_text() {
        let text = "Broken:\n![the form](/uploads/abc/form.png)\nand <img src=\"https://h/x.jpg\" alt='second'> \
                    plus ![t](https://h/y.gif \"title\") and a [link](https://h/not-image)";
        let refs = extract_refs(text, "description");
        let got: Vec<(&str, &str)> = refs
            .iter()
            .map(|r| (r.url.as_str(), r.alt.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("/uploads/abc/form.png", "the form"),
                ("https://h/x.jpg", "second"),
                ("https://h/y.gif", "t"),
            ]
        );
    }

    #[test]
    fn multibyte_text_around_images_does_not_panic() {
        let refs = extract_refs("错误 ![图](/uploads/a/b.png) 完", "description");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].alt, "图");
    }

    #[test]
    fn an_issue_yields_description_then_note_images_without_duplicates() {
        let issue = json!({
            "description": "![a](/uploads/1/a.png)",
            "notes": [
                {"body": "see ![a](/uploads/1/a.png) and ![b](/uploads/2/b.png)", "author": {"username": "sam"}},
                {"body": "no image", "author": {"username": "kim"}},
            ]
        });
        let refs = refs_of_issue(&issue);
        let got: Vec<(&str, &str)> = refs
            .iter()
            .map(|r| (r.url.as_str(), r.from.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("/uploads/1/a.png", "description"),
                ("/uploads/2/b.png", "comment by sam")
            ]
        );
    }

    #[test]
    fn only_real_raster_images_are_recognised_by_content() {
        assert_eq!(sniff(PNG), Some("png"));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(sniff(b"GIF89a.."), Some("gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(sniff(b"<svg onload=alert(1)>"), None);
        assert_eq!(sniff(b"<html>404</html>"), None);
    }

    #[test]
    fn download_saves_valid_images_and_reports_the_rest() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("web-5");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stale.png"), PNG).unwrap();
        let refs: Vec<ImageRef> = ["ok", "html", "big", "denied"]
            .iter()
            .map(|n| ImageRef {
                url: format!("/uploads/x/{n}"),
                alt: n.to_string(),
                from: "description".into(),
            })
            .collect();
        let fetch = |r: &ImageRef| -> Result<Vec<u8>, String> {
            match r.alt.as_str() {
                "ok" => Ok(PNG.to_vec()),
                "html" => Ok(b"<html>".to_vec()),
                "big" => Ok(vec![0x89; MAX_BYTES + 1]),
                _ => Err("not allowed".into()),
            }
        };
        let rep = download_all(&refs, &dir, &fetch).unwrap();
        assert_eq!(rep.saved.len(), 1);
        assert_eq!(rep.saved[0].file, dir.join("1.png"));
        assert_eq!(std::fs::read(dir.join("1.png")).unwrap(), PNG);
        assert!(!dir.join("stale.png").exists(), "a re-run starts clean");
        let why: Vec<&str> = rep.skipped.iter().map(|(_, w)| w.as_str()).collect();
        assert_eq!(
            why,
            [
                "not a png, jpeg, gif or webp image",
                "larger than 5 MB",
                "not allowed"
            ]
        );
        assert_eq!(rep.to_json()["images"][0]["alt"], "ok");
    }

    #[test]
    fn at_most_five_images_are_saved() {
        let d = tempfile::tempdir().unwrap();
        let refs: Vec<ImageRef> = (0..7)
            .map(|i| ImageRef {
                url: format!("/uploads/x/{i}.png"),
                alt: String::new(),
                from: "description".into(),
            })
            .collect();
        let rep = download_all(&refs, &d.path().join("k"), &|_| Ok(PNG.to_vec())).unwrap();
        assert_eq!(rep.saved.len(), MAX_IMAGES);
        assert_eq!(rep.skipped.len(), 2);
    }

    #[test]
    fn gitlab_images_stay_on_the_instance() {
        let web = "https://git.example.com/group/proj";
        assert_eq!(
            gitlab_target("/uploads/abc/a.png", web).unwrap(),
            "https://git.example.com/group/proj/uploads/abc/a.png"
        );
        assert_eq!(
            gitlab_target("https://GIT.example.com/group/proj/uploads/a/b.png", web).unwrap(),
            "https://GIT.example.com/group/proj/uploads/a/b.png"
        );
        for bad in [
            "https://evil.example.org/a.png",
            "//evil.example.org/a.png",
            "http://git.example.com/a.png",
            "file:///etc/passwd",
            "https://git.example.com@evil.org/a.png",
        ] {
            assert!(gitlab_target(bad, web).is_err(), "{bad}");
        }
    }

    #[test]
    fn github_images_are_limited_to_github_hosts_over_https() {
        for ok in [
            "https://github.com/user-attachments/assets/1234",
            "https://user-images.githubusercontent.com/1/a.png",
            "https://private-user-images.githubusercontent.com/1/a.png?jwt=x",
            "https://raw.githubusercontent.com/o/r/main/a.png",
        ] {
            assert!(github_allowed(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://github.com/a.png",
            "https://evilgithub.com/a.png",
            "https://github.com.evil.org/a.png",
            "https://example.com/a.png",
            "/relative.png",
        ] {
            assert!(github_allowed(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_folder_name_cannot_escape_outputs() {
        let p = images_dir(Path::new("/r"), "../web 5");
        assert_eq!(p, Path::new("/r/outputs/issue-images/___web_5"));
    }
}
