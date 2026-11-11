//! Per-project profile: `<memory root>/<alias>/PROFILE.md`, a markdown file of
//! fixed sections (each with provenance) plus links to other projects. Every
//! save snapshots the previous file into `PROFILE.history/` (newest 20 kept).

use crate::memory_store::{default_memory_root, parse_source_line};
use chrono::{DateTime, SecondsFormat, Utc};
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
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("-->", "->")
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
            out.push_str(&format!("\n{}\n", s.body.trim()));
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
    let (kind, evidence) = rest.split_once(" | ").unwrap_or((rest, ""));
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
    let alias = alias.ok_or("missing alias")?;

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
                    let l = l.trim();
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
                        body: body_lines.join("\n").trim().to_string(),
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

pub fn load(alias: &str, root: Option<&Path>) -> Result<Option<Profile>, String> {
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

pub fn save(p: &Profile, root: Option<&Path>) -> io::Result<()> {
    snapshot_and_write(&p.alias, &render(p), root)
}

/// Make a history file the current profile (the current one is snapshotted first).
pub fn restore(alias: &str, file: &Path, root: Option<&Path>) -> io::Result<()> {
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
    entry.summary = summary.to_string();
    entry.body = body.to_string();
    entry.last_verified = Some(now);
    entry.origin = Origin::Human;
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
}
