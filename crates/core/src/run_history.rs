//! Run-history files and the unified log, as the Runs hub reads them.
//! Mirrors the matching helpers of `bin/web/dashboard_server.py`:
//! `list_run_history`, `list_topic_history`, `read_history_file`,
//! `delete_history_file`, `extract_history_overview`, `gitlab_history_tags`,
//! `topic_history_tags`, `append_unified_log`, `read_unified_log_tail` and
//! `_parse_unified_log_entries`.

use chrono::NaiveDateTime;
use regex::Regex;
use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;

/// `Path(name).name` for a POSIX path: the part after the last `/`, ignoring
/// trailing slashes; `""` for `""`, `"."` and `"/"`.
pub fn path_name(name: &str) -> String {
    let trimmed = name.trim_end_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or("");
    if last == "." {
        String::new()
    } else {
        last.to_string()
    }
}

/// `Path(name).suffix == ".md"`: the final dot must not be the first or last
/// character of the name.
fn has_md_suffix(name: &str) -> bool {
    match name.rfind('.') {
        Some(i) => i > 0 && i + 1 < name.len() && &name[i..] == ".md",
        None => false,
    }
}

fn md_names(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| has_md_suffix(n))
        .collect()
}

/// `.md` filenames in `dir`, most recent (lexically greatest) first.
pub fn list_run_history(dir: &Path) -> Vec<String> {
    let mut names = md_names(dir);
    names.sort_by(|a, b| b.cmp(a));
    names
}

/// Topic-monitor briefings (`<date>-<topic>.md`), newest first; one topic's
/// only when `topic_name` is given (the whole filename must be a date, a dash,
/// the topic and `.md`, so `news` never claims `ai-news`'s files).
pub fn list_topic_history(topic_name: Option<&str>, dir: &Path) -> Vec<String> {
    let mut names = md_names(dir);
    if let Some(topic) = topic_name {
        let pattern = Regex::new(&format!(
            r"^\d{{4}}-\d{{2}}-\d{{2}}-{}\.md$",
            regex::escape(topic)
        ))
        .expect("topic pattern");
        names.retain(|n| pattern.is_match(n));
    }
    names.sort_by(|a, b| b.cmp(a));
    names
}

/// One history file by name, traversal-safe: only the final path component of
/// `name` is used, and only `.md` names are served.
pub fn read_history_file(name: &str, dir: &Path) -> Option<String> {
    let safe = path_name(name);
    if !safe.ends_with(".md") {
        return None;
    }
    let path = dir.join(&safe);
    if !path.is_file() {
        return None;
    }
    std::fs::read(&path)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryDelete {
    /// The name is not a `.md` filename (carries the original name).
    Invalid(String),
    /// No such file (carries the sanitised name).
    NotFound(String),
    Deleted(String),
}

/// `delete_history_file`: same traversal discipline as [`read_history_file`].
pub fn delete_history_file(name: &str, dir: &Path) -> HistoryDelete {
    let safe = path_name(name);
    if !safe.ends_with(".md") {
        return HistoryDelete::Invalid(name.to_string());
    }
    let path = dir.join(&safe);
    if !path.is_file() {
        return HistoryDelete::NotFound(safe);
    }
    match std::fs::remove_file(&path) {
        Ok(()) => HistoryDelete::Deleted(safe),
        Err(_) => HistoryDelete::NotFound(safe),
    }
}

// ---- markdown helpers ---------------------------------------------------------

fn h2_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^##\s+(.*)$").expect("h2"))
}

/// `str.splitlines()`.
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

fn is_py_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

/// The body of the level-2 section `## <heading>` (case-insensitive), up to the
/// next `## `, stripped of surrounding blank lines; `None` if it never appears.
pub fn markdown_section_body(text: &str, heading: &str) -> Option<String> {
    let lines = splitlines(text);
    let want = heading.to_lowercase();
    let start = lines.iter().position(|l| {
        h2_re()
            .captures(l)
            .is_some_and(|m| m[1].trim_matches(is_py_space).to_lowercase() == want)
    })? + 1;
    let mut body = Vec::new();
    for line in &lines[start..] {
        if h2_re().is_match(line) {
            break;
        }
        body.push(*line);
    }
    Some(body.join("\n").trim_matches(is_py_space).to_string())
}

/// The "at a glance" line of one history entry: its `## Summary` body, else the
/// leading paragraph before the first heading; cut at a word boundary to
/// `max_length` characters with a trailing ellipsis.
pub fn extract_history_overview(content: &str, max_length: usize) -> String {
    let summary = match markdown_section_body(content, "Summary") {
        Some(s) => s,
        None => {
            let mut para: Vec<&str> = Vec::new();
            let mut started = false;
            for line in splitlines(content) {
                if h2_re().is_match(line) {
                    break;
                }
                if line.starts_with("# ") {
                    continue;
                }
                if line.trim_matches(is_py_space).is_empty() {
                    if started {
                        break;
                    }
                    continue;
                }
                started = true;
                para.push(line.trim_matches(is_py_space));
            }
            para.join(" ")
        }
    };
    let summary = summary
        .split(is_py_space)
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if summary.chars().count() <= max_length {
        return summary;
    }
    let cut: String = summary.chars().take(max_length).collect();
    let head = match cut.rfind(' ') {
        Some(i) => &cut[..i],
        None => &cut[..],
    };
    format!("{head}\u{2026}")
}

fn bullet_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*[-*]\s+\S").expect("bullet"))
}

fn count_bullet_items(body: &str) -> usize {
    splitlines(body)
        .iter()
        .filter(|l| bullet_re().is_match(l))
        .count()
}

/// 0 for a missing or `None.` section, else its bullet count (1 with none).
pub fn history_section_count(content: &str, heading: &str) -> usize {
    let Some(body) = markdown_section_body(content, heading) else {
        return 0;
    };
    if body.is_empty()
        || body
            .trim_matches(is_py_space)
            .trim_end_matches('.')
            .to_lowercase()
            == "none"
    {
        return 0;
    }
    count_bullet_items(&body).max(1)
}

/// Highlight tags of one GitLab-loop history entry (English, untranslated).
pub fn gitlab_history_tags(content: &str) -> Vec<String> {
    let mut tags = Vec::new();
    for (heading, singular, plural) in [
        ("MRs opened", "MR", "MRs"),
        ("Escalations", "escalation", "escalations"),
        ("Answered directly", "answered", "answered"),
    ] {
        let count = history_section_count(content, heading);
        if count == 0 {
            continue;
        }
        tags.push(format!(
            "{count} {}",
            if count == 1 { singular } else { plural }
        ));
    }
    if tags.is_empty() {
        tags.push("Quiet day".to_string());
    }
    tags
}

/// Tags of one topic-monitor entry: the topic parsed from the filename, plus
/// "Quiet" when the briefing says nothing notable happened.
pub fn topic_history_tags(name: &str, content: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^\d{4}-\d{2}-\d{2}-(.+)\.md$").expect("topic name"));
    let mut tags: Vec<String> = re
        .captures(name)
        .map(|m| vec![m[1].to_string()])
        .unwrap_or_default();
    let lower = content.to_lowercase();
    if lower.contains("nothing notable") || lower.contains("no notable") {
        tags.push("Quiet".to_string());
    }
    tags
}

// ---- unified log --------------------------------------------------------------

/// `logs/loop-engineering.log` of a checkout.
pub fn unified_log_path(repo_root: &Path) -> std::path::PathBuf {
    repo_root.join("logs").join("loop-engineering.log")
}

/// `append_unified_log`: one `[YYYY-MM-DD HH:MM:SS] ---- source ---- detail ----`
/// header plus the body, best-effort and silent on failure.
pub fn append_unified_log_entry(
    source: &str,
    detail: &str,
    body: Option<&str>,
    log_path: &Path,
    now: NaiveDateTime,
) {
    let header = format!(
        "[{}] ---- {source} ---- {detail} ----\n",
        now.format("%Y-%m-%d %H:%M:%S")
    );
    let write = || -> std::io::Result<()> {
        if let Some(dir) = log_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        f.write_all(header.as_bytes())?;
        if let Some(body) = body.filter(|b| !b.is_empty()) {
            f.write_all(body.as_bytes())?;
            if !body.ends_with('\n') {
                f.write_all(b"\n")?;
            }
        }
        Ok(())
    };
    let _ = write();
}

/// The last `lines` lines of the log, or `None` before it exists.
pub fn read_unified_log_tail(lines: usize, log_path: &Path) -> Option<String> {
    if !log_path.exists() {
        return None;
    }
    let bytes = std::fs::read(log_path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let all = splitlines(&text);
    let from = all.len().saturating_sub(lines);
    Some(all[from..].join("\n"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub timestamp: Option<String>,
    pub source: Option<String>,
    pub detail: Option<String>,
    pub body: String,
}

/// Split a log tail into entries at each `[ts] ---- source ---- detail ----`
/// header; text before the first header becomes a headerless entry.
pub fn parse_unified_log_entries(tail: &str) -> Vec<LogEntry> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^\[([^\]]+)\] ---- (.*?) ---- (.*?) ----\s*$").expect("log header")
    });
    let mut entries: Vec<(LogEntry, Vec<String>)> = Vec::new();
    for line in splitlines(tail) {
        if let Some(m) = re.captures(line) {
            entries.push((
                LogEntry {
                    timestamp: Some(m[1].to_string()),
                    source: Some(m[2].to_string()),
                    detail: Some(m[3].to_string()),
                    body: String::new(),
                },
                Vec::new(),
            ));
        } else if let Some((_, body)) = entries.last_mut() {
            body.push(line.to_string());
        } else {
            entries.push((
                LogEntry {
                    timestamp: None,
                    source: None,
                    detail: None,
                    body: String::new(),
                },
                vec![line.to_string()],
            ));
        }
    }
    entries
        .into_iter()
        .map(|(mut e, body)| {
            e.body = body.join("\n").trim_matches('\n').to_string();
            e
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_name_matches_pathlib() {
        assert_eq!(path_name("a.md"), "a.md");
        assert_eq!(path_name("../x/a.md"), "a.md");
        assert_eq!(path_name("a/"), "a");
        assert_eq!(path_name("."), "");
        assert_eq!(path_name(""), "");
        assert_eq!(path_name("/"), "");
    }

    #[test]
    fn listing_is_newest_first_and_md_only() {
        let d = tempfile::tempdir().unwrap();
        for n in [
            "2026-01-02.md",
            "2026-01-03.md",
            "2026-01-03.log",
            ".md",
            "x",
        ] {
            std::fs::write(d.path().join(n), "x").unwrap();
        }
        assert_eq!(
            list_run_history(d.path()),
            ["2026-01-03.md", "2026-01-02.md"]
        );
        assert!(list_run_history(&d.path().join("missing")).is_empty());
    }

    #[test]
    fn topic_listing_filters_by_exact_topic() {
        let d = tempfile::tempdir().unwrap();
        for n in [
            "2026-01-02-news.md",
            "2026-01-02-ai-news.md",
            "2026-01-03-news.md",
        ] {
            std::fs::write(d.path().join(n), "x").unwrap();
        }
        assert_eq!(
            list_topic_history(Some("news"), d.path()),
            ["2026-01-03-news.md", "2026-01-02-news.md"]
        );
        assert_eq!(list_topic_history(None, d.path()).len(), 3);
    }

    #[test]
    fn read_and_delete_are_traversal_safe() {
        let d = tempfile::tempdir().unwrap();
        let hist = d.path().join("history");
        std::fs::create_dir(&hist).unwrap();
        std::fs::write(hist.join("a.md"), "hello").unwrap();
        std::fs::write(d.path().join("secret.md"), "nope").unwrap();
        assert_eq!(read_history_file("a.md", &hist).as_deref(), Some("hello"));
        assert_eq!(read_history_file("../secret.md", &hist), None);
        assert_eq!(read_history_file("a.txt", &hist), None);
        assert_eq!(read_history_file("missing.md", &hist), None);
        assert_eq!(
            delete_history_file("a.txt", &hist),
            HistoryDelete::Invalid("a.txt".into())
        );
        assert_eq!(
            delete_history_file("../secret.md", &hist),
            HistoryDelete::NotFound("secret.md".into())
        );
        assert!(d.path().join("secret.md").exists());
        assert_eq!(
            delete_history_file("a.md", &hist),
            HistoryDelete::Deleted("a.md".into())
        );
        assert!(!hist.join("a.md").exists());
    }

    #[test]
    fn overview_prefers_the_summary_section_then_the_lead_paragraph() {
        let with = "# T\n\n## Summary\n\nTwo   fixes\nand one answer.\n\n## MRs opened\n- a\n";
        assert_eq!(
            extract_history_overview(with, 240),
            "Two fixes and one answer."
        );
        let without = "# Briefing\n\nFirst line\nsecond line.\n\nmore\n## X\n";
        assert_eq!(
            extract_history_overview(without, 240),
            "First line second line."
        );
        let long = format!("## Summary\n{}", "word ".repeat(100));
        let cut = extract_history_overview(&long, 20);
        assert_eq!(cut, "word word word word\u{2026}");
    }

    #[test]
    fn gitlab_tags_count_non_empty_sections() {
        let c = "## MRs opened\n- a\n- b\n\n## Escalations\nNone.\n\n## Answered directly\nOne reply.\n";
        assert_eq!(gitlab_history_tags(c), ["2 MRs", "1 answered"]);
        assert_eq!(gitlab_history_tags("## Summary\nx"), ["Quiet day"]);
    }

    #[test]
    fn topic_tags_use_the_filename_and_quiet_phrases() {
        assert_eq!(
            topic_history_tags("2026-01-02-ai-news.md", "Nothing notable today"),
            ["ai-news", "Quiet"]
        );
        assert!(topic_history_tags("weird.md", "x").is_empty());
    }

    #[test]
    fn unified_log_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("logs").join("loop-engineering.log");
        assert_eq!(read_unified_log_tail(500, &log), None);
        let now = chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
            .unwrap()
            .and_hms_opt(9, 30, 0)
            .unwrap();
        append_unified_log_entry("chat-assistant", "turn started", None, &log, now);
        append_unified_log_entry("chat-assistant", "reply", Some("Hi\nthere"), &log, now);
        let tail = read_unified_log_tail(500, &log).unwrap();
        assert_eq!(
            tail,
            "[2026-01-15 09:30:00] ---- chat-assistant ---- turn started ----\n\
             [2026-01-15 09:30:00] ---- chat-assistant ---- reply ----\nHi\nthere"
        );
        assert_eq!(read_unified_log_tail(2, &log).unwrap(), "Hi\nthere");
        let entries = parse_unified_log_entries(&format!("leftover\n{tail}"));
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].source, None);
        assert_eq!(entries[0].body, "leftover");
        assert_eq!(entries[2].detail.as_deref(), Some("reply"));
        assert_eq!(entries[2].body, "Hi\nthere");
    }
}
