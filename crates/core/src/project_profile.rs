//! Per-project profile: `<memory root>/<alias>/PROFILE.md`, a markdown file of
//! fixed sections (each with provenance) plus links to other projects. Every
//! save snapshots the previous file into `PROFILE.history/` (newest 20 kept).

use crate::memory_store::{default_memory_root, parse_source_line};
use chrono::{DateTime, SecondsFormat, Timelike, Utc};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

const HISTORY_KEEP: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SectionKey {
    Overview,
    Structure,
    Features,
    Stack,
    Integrations,
    Secrets,
    Conventions,
    Gotchas,
    Activity,
    People,
}

impl SectionKey {
    pub const ALL: [SectionKey; 10] = [
        SectionKey::Overview,
        SectionKey::Structure,
        SectionKey::Features,
        SectionKey::Stack,
        SectionKey::Integrations,
        SectionKey::Secrets,
        SectionKey::Conventions,
        SectionKey::Gotchas,
        SectionKey::Activity,
        SectionKey::People,
    ];

    pub fn slug(self) -> &'static str {
        match self {
            SectionKey::Overview => "overview",
            SectionKey::Structure => "structure",
            SectionKey::Features => "features",
            SectionKey::Stack => "stack",
            SectionKey::Integrations => "integrations",
            SectionKey::Secrets => "secrets",
            SectionKey::Conventions => "conventions",
            SectionKey::Gotchas => "gotchas",
            SectionKey::Activity => "activity",
            SectionKey::People => "people",
        }
    }

    fn from_slug(slug: &str) -> Option<SectionKey> {
        Self::ALL.into_iter().find(|k| k.slug() == slug)
    }

    fn heading(self) -> &'static str {
        match self {
            SectionKey::Overview => "Overview",
            SectionKey::Structure => "Structure",
            SectionKey::Features => "Features",
            SectionKey::Stack => "Stack",
            SectionKey::Integrations => "Integrations",
            SectionKey::Secrets => "Secrets",
            SectionKey::Conventions => "Conventions",
            SectionKey::Gotchas => "Gotchas",
            SectionKey::Activity => "Activity",
            SectionKey::People => "People",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Agent,
    Human,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Draft,
    Approved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub summary: String,
    pub body: String,
    pub last_verified: Option<DateTime<Utc>>,
    pub sources: Vec<Source>,
    pub origin: Origin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub to: String,
    pub kind: String,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub alias: String,
    pub generated_at: Option<DateTime<Utc>>,
    pub head_sha: Option<String>,
    pub status: Status,
    pub links: Vec<Link>,
    pub sections: BTreeMap<SectionKey, Section>,
}

fn one_line(s: &str) -> String {
    let mut cur = s.split_whitespace().collect::<Vec<_>>().join(" ");
    // Idempotent: replacing can create a new `-->` (e.g. `--->`), so loop.
    while cur.contains("-->") {
        cur = cur.replace("-->", "->");
    }
    cur
}

/// A body line that the parser would treat as structure: a section marker, the
/// links marker or the sources heading.
fn is_structural(line: &str) -> bool {
    line.starts_with("<!-- section: ")
        || line.trim_end() == "<!-- links -->"
        || line.trim_end() == "### Sources"
}

/// Body escaping: a structural line, with any number of leading backslashes,
/// gets one more backslash on render; parse strips one. Round-trips exactly.
fn escape_body(body: &str) -> String {
    body.lines()
        .map(|l| {
            if is_structural(l.trim_start_matches('\\')) {
                format!("\\{l}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn unescape_line(line: &str) -> &str {
    if line.starts_with('\\') && is_structural(line.trim_start_matches('\\')) {
        &line[1..]
    } else {
        line
    }
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn render(p: &Profile) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("alias: {}\n", p.alias));
    if let Some(t) = p.generated_at {
        out.push_str(&format!("generated_at: {}\n", ts(t)));
    }
    if let Some(h) = &p.head_sha {
        out.push_str(&format!("head_sha: {h}\n"));
    }
    out.push_str(match p.status {
        Status::Draft => "status: draft\n",
        Status::Approved => "status: approved\n",
    });
    out.push_str("---\n");
    for key in SectionKey::ALL {
        let Some(s) = p.sections.get(&key) else {
            continue;
        };
        let origin = match s.origin {
            Origin::Agent => "agent",
            Origin::Human => "human",
        };
        let verified = s.last_verified.map_or("-".to_string(), ts);
        out.push_str(&format!(
            "\n<!-- section: {} origin: {origin} verified: {verified} summary: {} -->\n## {}\n",
            key.slug(),
            one_line(&s.summary),
            key.heading()
        ));
        if !s.body.trim().is_empty() {
            out.push_str(&format!("\n{}\n", escape_body(s.body.trim())));
        }
        if !s.sources.is_empty() {
            out.push_str("\n### Sources\n");
            for src in &s.sources {
                out.push_str(&format!("- `{}` sha256:{}\n", src.path, src.sha256));
            }
        }
    }
    if !p.links.is_empty() {
        out.push_str("\n<!-- links -->\n## Links\n\n");
        for l in &p.links {
            out.push_str(&format!(
                "- `{}` | {} | {}\n",
                l.to,
                one_line(&l.kind),
                one_line(&l.evidence)
            ));
        }
    }
    out
}

fn parse_time(v: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(v.trim())
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| format!("bad timestamp {v:?}: {e}"))
}

type Marker = (SectionKey, Origin, Option<DateTime<Utc>>, String);

fn parse_marker(rest: &str) -> Result<Marker, String> {
    // rest: "<slug> origin: X verified: V summary: S"
    let bad = || format!("malformed section marker: {rest:?}");
    let (slug, rest) = rest.split_once(" origin: ").ok_or_else(bad)?;
    let (origin, rest) = rest.split_once(" verified: ").ok_or_else(bad)?;
    let (verified, summary) = rest.split_once(" summary: ").ok_or_else(bad)?;
    let key =
        SectionKey::from_slug(slug.trim()).ok_or_else(|| format!("unknown section {slug:?}"))?;
    let origin = match origin.trim() {
        "agent" => Origin::Agent,
        "human" => Origin::Human,
        o => return Err(format!("unknown origin {o:?}")),
    };
    let verified = match verified.trim() {
        "-" => None,
        v => Some(parse_time(v)?),
    };
    Ok((key, origin, verified, summary.trim().to_string()))
}

fn parse_link(line: &str) -> Result<Link, String> {
    let bad = || format!("malformed link line: {line:?}");
    let rest = line.strip_prefix("- `").ok_or_else(bad)?;
    let (to, rest) = rest.split_once("` | ").ok_or_else(bad)?;
    let (kind, evidence) = match rest.split_once(" | ") {
        Some(pair) => pair,
        None => (rest.strip_suffix(" |").unwrap_or(rest), ""),
    };
    Ok(Link {
        to: to.to_string(),
        kind: kind.to_string(),
        evidence: evidence.to_string(),
    })
}

enum Block {
    Section(
        SectionKey,
        Origin,
        Option<DateTime<Utc>>,
        String,
        Vec<String>,
    ),
    Links(Vec<String>),
}

pub fn parse(text: &str) -> Result<Profile, String> {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Err("missing frontmatter".into());
    }
    let (mut alias, mut generated_at, mut head_sha, mut status) = (None, None, None, Status::Draft);
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "alias" => alias = Some(v.to_string()),
            "generated_at" => generated_at = Some(parse_time(v)?),
            "head_sha" => head_sha = Some(v.to_string()),
            "status" => {
                status = match v {
                    "draft" => Status::Draft,
                    "approved" => Status::Approved,
                    o => return Err(format!("unknown status {o:?}")),
                }
            }
            _ => {}
        }
    }
    if !closed {
        return Err("unterminated frontmatter".into());
    }
    let alias = alias.filter(|a| !a.is_empty()).ok_or("missing alias")?;

    let mut blocks: Vec<Block> = Vec::new();
    for line in lines {
        if let Some(rest) = line
            .strip_prefix("<!-- section: ")
            .and_then(|r| r.strip_suffix(" -->"))
        {
            let (k, o, v, s) = parse_marker(rest)?;
            blocks.push(Block::Section(k, o, v, s, Vec::new()));
        } else if line.trim_end() == "<!-- links -->" {
            blocks.push(Block::Links(Vec::new()));
        } else {
            match blocks.last_mut() {
                Some(Block::Section(.., body)) | Some(Block::Links(body)) => {
                    body.push(line.to_string())
                }
                None => {}
            }
        }
    }

    let mut sections = BTreeMap::new();
    let mut links = Vec::new();
    for b in blocks {
        match b {
            Block::Links(ls) => {
                for l in ls {
                    // Keep trailing whitespace: an empty evidence renders as `kind | `.
                    let l = l.trim_start();
                    if l.starts_with("- `") {
                        links.push(parse_link(l)?);
                    }
                }
            }
            Block::Section(key, origin, last_verified, summary, ls) => {
                let mut ls: &[String] = &ls;
                if ls.first().is_some_and(|l| l.starts_with("## ")) {
                    ls = &ls[1..];
                }
                let split = ls.iter().position(|l| l.trim_end() == "### Sources");
                let (body_lines, src_lines) = match split {
                    Some(i) => (&ls[..i], &ls[i + 1..]),
                    None => (ls, &[][..]),
                };
                let sources = src_lines
                    .iter()
                    .filter_map(|l| parse_source_line(l))
                    .map(|(p, h)| Source {
                        path: p.to_string(),
                        sha256: h.to_string(),
                    })
                    .collect();
                sections.insert(
                    key,
                    Section {
                        summary,
                        body: body_lines
                            .iter()
                            .map(|l| unescape_line(l))
                            .collect::<Vec<_>>()
                            .join("\n")
                            .trim()
                            .to_string(),
                        last_verified,
                        sources,
                        origin,
                    },
                );
            }
        }
    }
    Ok(Profile {
        alias,
        generated_at,
        head_sha,
        status,
        links,
        sections,
    })
}

fn project_dir(alias: &str, root: Option<&Path>) -> PathBuf {
    root.map(Path::to_path_buf)
        .unwrap_or_else(default_memory_root)
        .join(alias)
}

fn history_dir(alias: &str, root: Option<&Path>) -> PathBuf {
    project_dir(alias, root).join("PROFILE.history")
}

pub fn profile_path(alias: &str, root: Option<&Path>) -> PathBuf {
    project_dir(alias, root).join("PROFILE.md")
}

/// Aliases become a directory name: no separators, no `..`, not empty.
fn check_alias(alias: &str) -> Result<(), String> {
    if alias.is_empty() || alias.contains(['/', '\\', '\0']) || alias.contains("..") {
        return Err(format!("invalid project alias: {alias:?}"));
    }
    Ok(())
}

pub fn load(alias: &str, root: Option<&Path>) -> Result<Option<Profile>, String> {
    check_alias(alias)?;
    let path = profile_path(alias, root);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    parse(&text)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Newest first.
pub fn history(alias: &str, root: Option<&Path>) -> Vec<PathBuf> {
    if check_alias(alias).is_err() {
        return Vec::new();
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(history_dir(alias, root))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "md"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files.reverse();
    files
}

fn snapshot_and_write(alias: &str, text: &str, root: Option<&Path>) -> io::Result<()> {
    let path = profile_path(alias, root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.is_file() {
        let hdir = history_dir(alias, root);
        std::fs::create_dir_all(&hdir)?;
        let mut now = Utc::now();
        let dest = loop {
            let name = format!("{}.md", now.format("%Y%m%dT%H%M%S%9fZ"));
            let candidate = hdir.join(name);
            if !candidate.exists() {
                break candidate;
            }
            now += chrono::Duration::nanoseconds(1);
        };
        std::fs::copy(&path, dest)?;
        for old in history(alias, root).into_iter().skip(HISTORY_KEEP) {
            let _ = std::fs::remove_file(old);
        }
    }
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Fields the line-based format cannot represent are rejected, never dropped.
fn validate(p: &Profile) -> io::Result<()> {
    let has_nl = |v: &str| v.contains('\n') || v.contains('\r');
    for l in &p.links {
        if l.to.is_empty() || l.to.contains('`') || has_nl(&l.to) {
            return Err(invalid(format!(
                "link target not representable: {:?}",
                l.to
            )));
        }
        if l.kind.contains('|') || has_nl(&l.kind) || has_nl(&l.evidence) {
            return Err(invalid(format!(
                "link to {:?} has an unrepresentable kind/evidence",
                l.to
            )));
        }
    }
    for (key, s) in &p.sections {
        for src in &s.sources {
            if src.path.is_empty() || src.path.contains('`') || has_nl(&src.path) {
                return Err(invalid(format!(
                    "{}: source path not representable: {:?}",
                    key.slug(),
                    src.path
                )));
            }
            if src.sha256.is_empty() || src.sha256.contains(char::is_whitespace) {
                return Err(invalid(format!(
                    "{}: bad sha256 for {:?}",
                    key.slug(),
                    src.path
                )));
            }
        }
    }
    Ok(())
}

/// `p` as `render` + `parse` will give it back: whole-second timestamps,
/// bodies trimmed with LF line ends, one-line summaries/kinds/evidence.
fn normalised(p: &Profile) -> Profile {
    let secs = |t: DateTime<Utc>| t.with_nanosecond(0).unwrap_or(t);
    let mut n = p.clone();
    n.generated_at = n.generated_at.map(secs);
    for l in &mut n.links {
        l.kind = one_line(&l.kind);
        l.evidence = one_line(&l.evidence);
    }
    for s in n.sections.values_mut() {
        s.summary = one_line(&s.summary);
        s.body = s.body.trim().lines().collect::<Vec<_>>().join("\n");
        s.last_verified = s.last_verified.map(secs);
    }
    n
}

/// Refuses (writes nothing) anything that would not read back as `p`, up to
/// the normalisation `render` applies.
pub fn save(p: &Profile, root: Option<&Path>) -> io::Result<()> {
    check_alias(&p.alias).map_err(invalid)?;
    validate(p)?;
    let text = render(p);
    let back = parse(&text).map_err(invalid)?;
    if back != normalised(p) {
        return Err(invalid(
            "profile does not round-trip through its markdown form".into(),
        ));
    }
    snapshot_and_write(&p.alias, &text, root)
}

/// Make a history file the current profile (the current one is snapshotted first).
pub fn restore(alias: &str, file: &Path, root: Option<&Path>) -> io::Result<()> {
    check_alias(alias).map_err(invalid)?;
    let text = std::fs::read_to_string(file)?;
    parse(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    snapshot_and_write(alias, &text, root)
}

pub fn edit_section(
    p: &mut Profile,
    key: SectionKey,
    summary: &str,
    body: &str,
    now: DateTime<Utc>,
) {
    let entry = p.sections.entry(key).or_insert_with(|| Section {
        summary: String::new(),
        body: String::new(),
        last_verified: None,
        sources: Vec::new(),
        origin: Origin::Human,
    });
    entry.summary = one_line(summary);
    entry.body = body.replace("\r\n", "\n");
    entry.last_verified = Some(now);
    entry.origin = Origin::Human;
}

// ---- secret guard ----------------------------------------------------

/// Describes the first secret-looking thing in `text` by kind only. The
/// returned string never contains any part of the matched text.
pub fn contains_secret(text: &str) -> Option<String> {
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<Vec<(&'static str, regex::Regex)>> = OnceLock::new();
    static ASSIGN: OnceLock<regex::Regex> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        let table: [(&str, &str); 7] = [
            ("private key block", r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY"),
            ("GitLab token", r"glpat-[A-Za-z0-9_\-]{10,}"),
            ("Slack token", r"xox[baprs]-[A-Za-z0-9-]{10,}"),
            ("AWS access key", r"\bAKIA[0-9A-Z]{16}\b"),
            (
                "GitHub token",
                r"\bgh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}",
            ),
            ("API key", r"\bsk-[A-Za-z0-9_\-]{20,}"),
            (
                "Slack webhook URL",
                r"hooks\.slack\.com/services/[A-Za-z0-9/]{10,}",
            ),
        ];
        table
            .into_iter()
            .map(|(k, r)| (k, regex::Regex::new(r).expect("static regex")))
            .collect()
    });
    for (kind, re) in patterns {
        if re.is_match(text) {
            return Some((*kind).to_string());
        }
    }
    let assign = ASSIGN.get_or_init(|| {
        regex::Regex::new(
            r#"(?i)([A-Za-z0-9_.\-]*(?:token|secret|password|passwd|api_?key|private_?key|credential|auth)[A-Za-z0-9_.\-]*)\s*[=:]\s*["']?([A-Za-z0-9+/_\-]{20,})"#,
        )
        .expect("static regex")
    });
    for c in assign.captures_iter(text) {
        let v = &c[2];
        if v.chars().any(|ch| ch.is_ascii_alphabetic()) && v.chars().any(|ch| ch.is_ascii_digit()) {
            return Some("secret assignment with a long value".to_string());
        }
    }
    None
}

/// Rejects a profile whose text fields would persist a secret value. Errors
/// name the field and the kind of secret, never the matched text.
pub fn validate_draft(p: &Profile) -> Result<(), String> {
    let check = |field: String, text: &str| -> Result<(), String> {
        match contains_secret(text) {
            Some(kind) => Err(format!(
                "{field} contains what looks like a secret ({kind})"
            )),
            None => Ok(()),
        }
    };
    if let Some(sha) = &p.head_sha {
        check("head_sha".into(), sha)?;
    }
    for (key, sec) in &p.sections {
        let slug = key.slug();
        check(format!("section {slug} summary"), &sec.summary)?;
        check(format!("section {slug} body"), &sec.body)?;
    }
    for (i, l) in p.links.iter().enumerate() {
        let n = i + 1;
        check(format!("link {n} evidence"), &l.evidence)?;
        check(format!("link {n} to"), &l.to)?;
        check(format!("link {n} kind"), &l.kind)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 11, h, 0, 0).unwrap()
    }

    fn sample(alias: &str) -> Profile {
        let mut sections = BTreeMap::new();
        for (i, key) in SectionKey::ALL.into_iter().enumerate() {
            sections.insert(
                key,
                Section {
                    summary: format!("summary of {}", key.slug()),
                    body: format!("Body for {}.\n\n- a bullet\n- another", key.slug()),
                    last_verified: if i % 2 == 0 { Some(t(i as u32)) } else { None },
                    sources: vec![Source {
                        path: format!("src/{i}.rs"),
                        sha256: format!("{:064x}", i),
                    }],
                    origin: if i % 3 == 0 {
                        Origin::Human
                    } else {
                        Origin::Agent
                    },
                },
            );
        }
        Profile {
            alias: alias.into(),
            generated_at: Some(t(9)),
            head_sha: Some("abc123".into()),
            status: Status::Approved,
            links: vec![Link {
                to: "other".into(),
                kind: "calls".into(),
                evidence: "src/client.rs uses other's API".into(),
            }],
            sections,
        }
    }

    #[test]
    fn parse_render_round_trips_all_sections() {
        let p = sample("web");
        let text = render(&p);
        assert_eq!(parse(&text).unwrap(), p);
        assert_eq!(render(&parse(&text).unwrap()), text);
        assert_eq!(SectionKey::ALL.len(), 10);
        assert_eq!(p.sections.len(), 10);
    }

    #[test]
    fn load_absent_is_none() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(load("nope", Some(root.path())), Ok(None));
    }

    #[test]
    fn load_corrupt_is_err_and_leaves_file_untouched() {
        let root = tempfile::tempdir().unwrap();
        let path = profile_path("web", Some(root.path()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let garbage =
            "---\nalias: web\n---\n<!-- section: bogus origin: agent verified: - summary: x -->\n";
        std::fs::write(&path, garbage).unwrap();
        assert!(load("web", Some(root.path())).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), garbage);
        assert!(history("web", Some(root.path())).is_empty());
    }

    #[test]
    fn save_keeps_history_capped_at_20() {
        let root = tempfile::tempdir().unwrap();
        let r = Some(root.path());
        let mut p = sample("web");
        for i in 0..25 {
            p.head_sha = Some(format!("sha{i}"));
            save(&p, r).unwrap();
        }
        let h = history("web", r);
        assert_eq!(h.len(), 20);
        // newest first; the newest snapshot is the previous (24th) save
        let newest = parse(&std::fs::read_to_string(&h[0]).unwrap()).unwrap();
        assert_eq!(newest.head_sha.as_deref(), Some("sha23"));
        assert_eq!(
            load("web", r).unwrap().unwrap().head_sha.as_deref(),
            Some("sha24")
        );
    }

    #[test]
    fn edit_section_marks_human() {
        let mut p = sample("web");
        assert_eq!(p.sections[&SectionKey::Features].origin, Origin::Agent);
        edit_section(&mut p, SectionKey::Features, "new sum", "new body", t(12));
        let s = &p.sections[&SectionKey::Features];
        assert_eq!(s.origin, Origin::Human);
        assert_eq!(s.summary, "new sum");
        assert_eq!(s.body, "new body");
        assert_eq!(s.last_verified, Some(t(12)));
    }

    fn hostile_round_trip(body: &str) {
        let mut p = sample("web");
        edit_section(&mut p, SectionKey::Gotchas, "s", body, t(1));
        let back = parse(&render(&p)).unwrap();
        assert_eq!(back, p);
        assert_eq!(back.sections[&SectionKey::Gotchas].body, body);
    }

    #[test]
    fn hostile_bodies_round_trip_exactly() {
        hostile_round_trip("a\n<!-- section: stack origin: agent verified: - summary: x -->\nb");
        hostile_round_trip("a\n<!-- section: bogus -->\nb");
        hostile_round_trip("a\n<!-- links -->\n- `x` | y | z\nb");
        hostile_round_trip("a\n### Sources\n- `p` sha256:abc\nb");
        hostile_round_trip("a\n\\### Sources\n\\\\<!-- links -->\nb");
    }

    #[test]
    fn multiline_summary_round_trips_after_edit_section() {
        let mut p = sample("web");
        edit_section(
            &mut p,
            SectionKey::Gotchas,
            "one\ntwo  --> three",
            "b",
            t(1),
        );
        assert_eq!(p.sections[&SectionKey::Gotchas].summary, "one two -> three");
        assert_eq!(parse(&render(&p)).unwrap(), p);
    }

    #[test]
    fn save_refuses_unreadable_output() {
        let root = tempfile::tempdir().unwrap();
        let r = Some(root.path());
        let mutations: Vec<fn(&mut Profile)> = vec![
            |p| p.sections.get_mut(&SectionKey::Stack).unwrap().sources[0].path = "a`b".into(),
            |p| p.sections.get_mut(&SectionKey::Stack).unwrap().sources[0].path = "a\nb".into(),
            |p| p.links[0].to = "a`b".into(),
            |p| p.links[0].to = "a\nb".into(),
            |p| p.links[0].kind = "a\nb".into(),
            |p| p.links[0].kind = "a | b".into(),
            |p| p.links[0].evidence = "a\nb".into(),
            |p| p.alias = String::new(),
            |p| p.alias = "web\nstatus: draft".into(),
            |p| p.alias = " web".into(),
            |p| p.alias = "web ".into(),
            |p| p.head_sha = Some("abc\ndef".into()),
            |p| p.alias = "../x".into(),
            |p| p.alias = "a/b".into(),
            |p| p.alias = "a\\b".into(),
        ];
        for m in mutations {
            let mut p = sample("web");
            m(&mut p);
            assert!(save(&p, r).is_err());
            assert!(!profile_path(&p.alias, r).exists());
        }
        save(&sample("web"), r).unwrap();
    }

    #[test]
    fn one_line_is_idempotent_and_crlf_is_normalised() {
        for s in ["--->", "a ---> b", "----->", "x\n-->\ny"] {
            assert_eq!(one_line(&one_line(s)), one_line(s));
            assert!(!one_line(s).contains("-->"));
        }
        let mut p = sample("web");
        edit_section(
            &mut p,
            SectionKey::Gotchas,
            "s",
            "a\r\nb\r\n### Sources  \r\nc",
            t(1),
        );
        assert_eq!(
            p.sections[&SectionKey::Gotchas].body,
            "a\nb\n### Sources  \nc"
        );
        assert_eq!(parse(&render(&p)).unwrap(), p);
    }

    #[test]
    fn trailing_whitespace_hostile_lines_round_trip() {
        hostile_round_trip("a\n### Sources  \n- `p` sha256:abc\nb");
        hostile_round_trip("a\n<!-- links -->  \n- `x` | y | z\nb");
        hostile_round_trip("a\n### Sources\t\n\\<!-- links -->\t\nb");
    }

    #[test]
    fn empty_kind_and_evidence_links_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let r = Some(root.path());
        let mut p = sample("web");
        p.links = vec![
            Link {
                to: "a".into(),
                kind: "calls".into(),
                evidence: String::new(),
            },
            Link {
                to: "b".into(),
                kind: String::new(),
                evidence: "ev".into(),
            },
            Link {
                to: "c".into(),
                kind: String::new(),
                evidence: String::new(),
            },
        ];
        save(&p, r).unwrap();
        assert_eq!(load("web", r).unwrap().unwrap().links, p.links);
        // an editor stripping the trailing space is tolerated
        let text = render(&p).replace("| calls | \n", "| calls |\n");
        assert_eq!(parse(&text).unwrap().links, p.links);
    }

    #[test]
    fn save_normalises_lossless_differences_and_rejects_traversal() {
        let root = tempfile::tempdir().unwrap();
        let r = Some(root.path());
        let mut p = sample("web");
        p.sections.get_mut(&SectionKey::Stack).unwrap().body = "  padded \r\n body\n\n".into();
        p.generated_at = Some(t(9) + chrono::Duration::milliseconds(5));
        save(&p, r).unwrap();
        let back = load("web", r).unwrap().unwrap();
        assert_eq!(back.sections[&SectionKey::Stack].body, "padded \n body");
        for bad in ["../x", "a/b", "a\\b", "..", ""] {
            assert!(load(bad, r).is_err());
            assert!(history(bad, r).is_empty());
            assert!(restore(bad, &profile_path("web", r), r).is_err());
        }
        assert!(!root
            .path()
            .parent()
            .unwrap()
            .join("x")
            .join("PROFILE.md")
            .exists());
    }

    #[test]
    fn restore_brings_back_previous_version() {
        let root = tempfile::tempdir().unwrap();
        let r = Some(root.path());
        let mut p = sample("web");
        p.head_sha = Some("v1".into());
        save(&p, r).unwrap();
        p.head_sha = Some("v2".into());
        save(&p, r).unwrap();
        let h = history("web", r);
        assert_eq!(h.len(), 1);
        restore("web", &h[0], r).unwrap();
        assert_eq!(
            load("web", r).unwrap().unwrap().head_sha.as_deref(),
            Some("v1")
        );
        // the v2 file was snapshotted by the restore
        assert_eq!(history("web", r).len(), 2);
    }

    fn fake(parts: &[&str]) -> String {
        parts.concat()
    }

    #[test]
    fn rejects_private_key_block() {
        let t = fake(&["-----BEGIN RSA ", "PRIVATE KEY-----\nMIIE"]);
        assert_eq!(contains_secret(&t).as_deref(), Some("private key block"));
    }

    #[test]
    fn rejects_gitlab_pat_glpat_prefix() {
        let t = fake(&["token is glpat-", "abcDEF123456ghiJKL78"]);
        assert_eq!(contains_secret(&t).as_deref(), Some("GitLab token"));
    }

    #[test]
    fn rejects_aws_style_key() {
        let t = fake(&["key AKIA", "IOSFODNN7EXAMPLE here"]);
        assert_eq!(contains_secret(&t).as_deref(), Some("AWS access key"));
    }

    #[test]
    fn rejects_long_high_entropy_assignment() {
        let t = fake(&["TOKEN=", "a1b2c3d4e5f6a7b8c9d0a1b2c3d4e5f6a7b8c9d0"]);
        assert!(contains_secret(&t).is_some());
        assert!(contains_secret("api_key: \"Zx9Qw8Er7Ty6Ui5Op4As3Df2Gh1Jk\"").is_some());
    }

    #[test]
    fn accepts_secret_name_without_value() {
        assert_eq!(
            contains_secret("GITLAB_TOKEN \u{2013} set in ~/.gitlab/config.json"),
            None
        );
        assert_eq!(contains_secret("export GITLAB_TOKEN=$GITLAB_TOKEN"), None);
    }

    #[test]
    fn accepts_commit_sha_paths_and_prose() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(contains_secret(&format!("head {sha}")), None);
        assert_eq!(
            contains_secret("see crates/core/src/project_profile.rs"),
            None
        );
        assert_eq!(contains_secret("The auth module handles login."), None);
    }

    #[test]
    fn error_message_never_contains_the_value() {
        let value = "a1b2c3d4e5f6a7b8c9d0a1b2c3d4e5f6";
        let mut p = sample("web");
        p.sections.get_mut(&SectionKey::ALL[0]).unwrap().body = format!("SECRET_TOKEN={value}");
        let err = validate_draft(&p).unwrap_err();
        assert!(!err.contains(value) && !err.contains("a1b2c3"), "{err}");
        assert!(err.contains(SectionKey::ALL[0].slug()));
        let glpat = fake(&["glpat-", "abcDEF123456ghiJKL78"]);
        let d = contains_secret(&glpat).unwrap();
        assert!(!d.contains("abcDEF"));
    }

    #[test]
    fn validate_draft_checks_summary_links_and_passes_clean() {
        assert!(validate_draft(&sample("web")).is_ok());
        let secret = fake(&["glpat-", "abcDEF123456ghiJKL78"]);
        let mut p = sample("web");
        p.sections.get_mut(&SectionKey::ALL[1]).unwrap().summary = secret.clone();
        assert!(validate_draft(&p).unwrap_err().contains("summary"));
        let mut p = sample("web");
        p.links.clear();
        p.links.push(Link {
            to: "x".into(),
            kind: "uses".into(),
            evidence: secret,
        });
        assert!(validate_draft(&p).unwrap_err().contains("link 1 evidence"));
    }
}
