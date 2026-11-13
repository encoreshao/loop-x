//! A project's guide: a small wiki of pages written from its source (Architecture,
//! Data model, Roadmap, one page per feature). One markdown file per page under
//! `<memory root>/<alias>/guide/`. The file format mirrors `PROFILE.md`: parts carry an
//! origin so a hand-edited part survives a rebuild, saves are round-trip checked, and
//! every save snapshots the old file.

use crate::memory_store::parse_source_line;
use crate::project_profile::{
    check_alias, contains_secret, escape_body_by, invalid, one_line, parse_time, project_dir, ts,
    unescape_line_by, Origin, Source, Status, HISTORY_KEEP,
};
use chrono::{DateTime, Timelike, Utc};
use std::io;
use std::path::{Path, PathBuf};

pub const MAX_SLUG: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Feature,
    Architecture,
    DataModel,
    Roadmap,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Feature => "feature",
            Kind::Architecture => "architecture",
            Kind::DataModel => "data-model",
            Kind::Roadmap => "roadmap",
        }
    }

    pub fn parse_kind(s: &str) -> Option<Kind> {
        [
            Kind::Feature,
            Kind::Architecture,
            Kind::DataModel,
            Kind::Roadmap,
        ]
        .into_iter()
        .find(|k| k.as_str() == s)
    }

    /// (part key, heading) in render order.
    pub fn parts(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Kind::Feature => &[
                ("what", "What it does"),
                ("value", "Business value and metrics"),
                ("how", "How it works"),
                ("associations", "Associations"),
                ("key-points", "Key points"),
            ],
            Kind::Architecture => &[
                ("overview", "Overview"),
                ("components", "Components"),
                ("flows", "Request and data flows"),
                ("runtime", "Runtime and deployment"),
            ],
            Kind::DataModel => &[
                ("entities", "Entities"),
                ("relationships", "Relationships"),
                ("rules", "Rules and invariants"),
            ],
            Kind::Roadmap => &[
                ("shipped", "Recently shipped"),
                ("in-progress", "In progress"),
                ("planned", "Planned"),
                ("unscheduled", "Not scheduled"),
            ],
        }
    }

    fn part_index(self, key: &str) -> Option<usize> {
        self.parts().iter().position(|(k, _)| *k == key)
    }
}

/// Validates a slug and gives the page kind it names. Runs before any path is built.
pub fn check_slug(slug: &str) -> Result<Kind, String> {
    let bad = || format!("invalid guide page slug: {slug:?}");
    if slug.is_empty() || slug.len() > MAX_SLUG {
        return Err(bad());
    }
    if !slug
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(bad());
    }
    match slug {
        "architecture" => return Ok(Kind::Architecture),
        "data-model" => return Ok(Kind::DataModel),
        "roadmap" => return Ok(Kind::Roadmap),
        _ => {}
    }
    let rest = slug.strip_prefix("feature-").ok_or_else(bad)?;
    // `--` is checked on the whole slug: "feature--a" has a clean `rest` ("-a" has no `--`).
    if rest.is_empty() || slug.contains("--") || rest.ends_with('-') {
        return Err(bad());
    }
    Ok(Kind::Feature)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub key: String,
    pub body: String,
    pub origin: Origin,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub alias: String,
    pub slug: String,
    pub kind: Kind,
    pub title: String,
    pub status: Status,
    pub generated_at: Option<DateTime<Utc>>,
    pub head_sha: Option<String>,
    pub summary: String,
    pub related: Vec<String>,
    pub parts: Vec<Part>,
    pub sources: Vec<Source>,
}

/// A body line the parser would treat as structure.
fn is_structural(line: &str) -> bool {
    line.starts_with("<!-- part: ") || line.trim_end() == "### Sources"
}

fn origin_str(o: Origin) -> &'static str {
    match o {
        Origin::Agent => "agent",
        Origin::Human => "human",
    }
}

pub fn render(p: &Page) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("alias: {}\n", p.alias));
    out.push_str(&format!("slug: {}\n", p.slug));
    out.push_str(&format!("kind: {}\n", p.kind.as_str()));
    out.push_str(&format!("title: {}\n", one_line(&p.title)));
    out.push_str(match p.status {
        Status::Draft => "status: draft\n",
        Status::Approved => "status: approved\n",
    });
    if let Some(t) = p.generated_at {
        out.push_str(&format!("generated_at: {}\n", ts(t)));
    }
    if let Some(h) = &p.head_sha {
        out.push_str(&format!("head_sha: {h}\n"));
    }
    out.push_str(&format!("summary: {}\n", one_line(&p.summary)));
    if !p.related.is_empty() {
        out.push_str(&format!("related: {}\n", p.related.join(", ")));
    }
    out.push_str("---\n");
    for (key, heading) in p.kind.parts() {
        let Some(part) = p.parts.iter().find(|x| x.key == *key) else {
            continue;
        };
        out.push_str(&format!(
            "\n<!-- part: {key} origin: {} -->\n## {heading}\n",
            origin_str(part.origin)
        ));
        if !part.body.trim().is_empty() {
            out.push_str(&format!(
                "\n{}\n",
                escape_body_by(part.body.trim(), is_structural)
            ));
        }
    }
    if !p.sources.is_empty() {
        out.push_str("\n### Sources\n");
        for s in &p.sources {
            out.push_str(&format!("- `{}` sha256:{}\n", s.path, s.sha256));
        }
    }
    out
}

/// Parse errors name the line and the problem, never the text: a page line can carry
/// anything (a pasted secret included) and these messages reach logs and the chat.
pub fn parse(text: &str) -> Result<Page, String> {
    let mut lines = text.lines().enumerate().map(|(i, l)| (i + 1, l));
    if lines.next().map(|(_, l)| l.trim()) != Some("---") {
        return Err("missing frontmatter".into());
    }
    let (mut alias, mut slug, mut kind, mut title) = (None, None, None, None);
    let (mut status, mut generated_at, mut head_sha) = (Status::Draft, None, None);
    let (mut summary, mut related) = (String::new(), Vec::new());
    let mut closed = false;
    for (n, line) in lines.by_ref() {
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
            "slug" => slug = Some(v.to_string()),
            "kind" => {
                kind = Some(Kind::parse_kind(v).ok_or_else(|| format!("unknown kind on line {n}"))?)
            }
            "title" => title = Some(v.to_string()),
            "status" => {
                status = match v {
                    "draft" => Status::Draft,
                    "approved" => Status::Approved,
                    _ => return Err(format!("unknown status on line {n}")),
                }
            }
            "generated_at" => {
                generated_at =
                    Some(parse_time(v).map_err(|_| format!("bad generated_at on line {n}"))?)
            }
            "head_sha" => head_sha = Some(v.to_string()),
            "summary" => summary = v.to_string(),
            "related" => {
                related = v
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            }
            _ => {}
        }
    }
    if !closed {
        return Err("unterminated frontmatter".into());
    }
    let alias = alias.filter(|a| !a.is_empty()).ok_or("missing alias")?;
    let slug = slug.ok_or("missing slug")?;
    let slug_kind = check_slug(&slug).map_err(|_| "invalid slug in frontmatter".to_string())?;
    let kind = kind.ok_or("missing kind")?;
    if kind != slug_kind {
        return Err("kind does not match the slug".into());
    }
    let title = title.filter(|t| !t.is_empty()).ok_or("missing title")?;

    let body: Vec<(usize, &str)> = lines.collect();
    let split = body.iter().position(|(_, l)| l.trim_end() == "### Sources");
    let (part_lines, src_lines) = match split {
        Some(i) => (&body[..i], &body[i + 1..]),
        None => (&body[..], &body[body.len()..]),
    };
    let sources = src_lines
        .iter()
        .filter_map(|(_, l)| parse_source_line(l))
        .map(|(p, h)| Source {
            path: p.to_string(),
            sha256: h.to_string(),
        })
        .collect();

    let mut blocks: Vec<(String, Origin, Vec<&str>)> = Vec::new();
    for (n, line) in part_lines {
        if let Some(rest) = line
            .strip_prefix("<!-- part: ")
            .and_then(|r| r.strip_suffix(" -->"))
        {
            let (key, origin) = rest
                .split_once(" origin: ")
                .ok_or_else(|| format!("malformed part marker on line {n}"))?;
            let origin = match origin.trim() {
                "agent" => Origin::Agent,
                "human" => Origin::Human,
                _ => return Err(format!("unknown origin on line {n}")),
            };
            if kind.part_index(key.trim()).is_none() {
                return Err(format!("unknown part on line {n}"));
            }
            if blocks.iter().any(|b| b.0 == key.trim()) {
                return Err(format!("duplicate part on line {n}"));
            }
            blocks.push((key.trim().to_string(), origin, Vec::new()));
        } else if let Some(b) = blocks.last_mut() {
            b.2.push(line);
        }
    }
    let parts = blocks
        .into_iter()
        .map(|(key, origin, ls)| {
            let mut ls: &[&str] = &ls;
            if ls.first().is_some_and(|l| l.starts_with("## ")) {
                ls = &ls[1..];
            }
            let body = ls
                .iter()
                .map(|l| unescape_line_by(l, is_structural))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
            Part { key, body, origin }
        })
        .collect();
    Ok(Page {
        alias,
        slug,
        kind,
        title,
        status,
        generated_at,
        head_sha,
        summary,
        related,
        parts,
        sources,
    })
}

/// `p` as `render` + `parse` give it back: whole-second timestamps, one-line title and
/// summary, trimmed LF bodies, parts in the kind's order.
pub(crate) fn normalised(p: &Page) -> Page {
    let mut n = p.clone();
    n.generated_at = n.generated_at.map(|t| t.with_nanosecond(0).unwrap_or(t));
    n.title = one_line(&n.title);
    n.summary = one_line(&n.summary);
    for part in &mut n.parts {
        part.body = part.body.trim().lines().collect::<Vec<_>>().join("\n");
    }
    n.parts
        .sort_by_key(|x| p.kind.part_index(&x.key).unwrap_or(usize::MAX));
    n
}

pub fn guide_dir(alias: &str, root: Option<&Path>) -> PathBuf {
    project_dir(alias, root).join("guide")
}

/// `<guide dir>/plan.json`, the last accepted plan.
pub fn plan_path(alias: &str, root: Option<&Path>) -> PathBuf {
    guide_dir(alias, root).join("plan.json")
}

fn history_dir(alias: &str, root: Option<&Path>) -> PathBuf {
    guide_dir(alias, root).join(".history")
}

fn page_path(alias: &str, slug: &str, root: Option<&Path>) -> PathBuf {
    guide_dir(alias, root).join(format!("{slug}.md"))
}

/// Fields the line-based format cannot represent or that would persist a secret are
/// rejected, never dropped. Errors name the field and kind, never the matched text.
pub fn validate(p: &Page) -> Result<(), String> {
    let has_nl = |v: &str| v.contains('\n') || v.contains('\r');
    let kind = check_slug(&p.slug).map_err(|_| "slug is not valid".to_string())?;
    if kind != p.kind {
        return Err("kind does not match the slug".into());
    }
    if p.title.trim().is_empty() || has_nl(&p.title) {
        return Err("title must be one non-empty line".into());
    }
    if p.head_sha
        .as_deref()
        .is_some_and(|h| has_nl(h) || h.contains(char::is_whitespace))
    {
        return Err("head_sha is not representable".into());
    }
    for (i, r) in p.related.iter().enumerate() {
        check_slug(r).map_err(|_| format!("related slug #{} is not valid", i + 1))?;
    }
    let mut seen = Vec::new();
    for (i, part) in p.parts.iter().enumerate() {
        if p.kind.part_index(&part.key).is_none() {
            return Err(format!(
                "part #{} does not belong on a {} page",
                i + 1,
                p.kind.as_str()
            ));
        }
        // The key is a known one here, so naming it echoes nothing a caller supplied.
        if seen.contains(&part.key) {
            return Err(format!("part {} appears twice", part.key));
        }
        seen.push(part.key.clone());
    }
    for (i, s) in p.sources.iter().enumerate() {
        if s.path.is_empty() || s.path.contains('`') || has_nl(&s.path) {
            return Err(format!("source #{} path is not representable", i + 1));
        }
        if s.sha256.is_empty() || s.sha256.contains(char::is_whitespace) {
            return Err(format!("source #{} has a bad sha256", i + 1));
        }
    }
    let check = |field: String, text: &str| match contains_secret(text) {
        Some(k) => Err(format!("{field} contains what looks like a secret ({k})")),
        None => Ok(()),
    };
    check("title".into(), &p.title)?;
    check("summary".into(), &p.summary)?;
    for part in &p.parts {
        check(format!("part {}", part.key), &part.body)?;
    }
    for s in &p.sources {
        check("source path".into(), &s.path)?;
    }
    Ok(())
}

fn snapshot_and_write(alias: &str, slug: &str, text: &str, root: Option<&Path>) -> io::Result<()> {
    let path = page_path(alias, slug, root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.is_file() {
        let hdir = history_dir(alias, root);
        std::fs::create_dir_all(&hdir)?;
        let mut now = Utc::now();
        let dest = loop {
            let candidate = hdir.join(format!("{slug}.{}.md", now.format("%Y%m%dT%H%M%S%9fZ")));
            if !candidate.exists() {
                break candidate;
            }
            now += chrono::Duration::nanoseconds(1);
        };
        std::fs::copy(&path, dest)?;
        for old in history(alias, slug, root).into_iter().skip(HISTORY_KEEP) {
            let _ = std::fs::remove_file(old);
        }
    }
    crate::file_lock::write_atomic(&path, text.as_bytes())
}

/// Refuses (writes nothing) anything that would not read back as `p`. Takes no lock:
/// writers that read-modify-write a page must go through `update_locked`.
pub fn save(p: &Page, root: Option<&Path>) -> io::Result<()> {
    check_alias(&p.alias).map_err(invalid)?;
    validate(p).map_err(invalid)?;
    let text = render(p);
    let back = parse(&text).map_err(invalid)?;
    if back != normalised(p) {
        return Err(invalid(
            "page does not round-trip through its markdown form".into(),
        ));
    }
    snapshot_and_write(&p.alias, &p.slug, &text, root)
}

fn same_page(p: &Page, alias: &str, slug: &str) -> Result<(), String> {
    if p.alias != alias {
        return Err(format!(
            "the guide page in the {alias} folder belongs to another project (its alias does not match the folder); move or fix it by hand"
        ));
    }
    if p.slug != slug {
        return Err(format!(
            "the file for {slug} holds a different page (its slug does not match)"
        ));
    }
    Ok(())
}

pub fn load(alias: &str, slug: &str, root: Option<&Path>) -> Result<Option<Page>, String> {
    check_alias(alias)?;
    check_slug(slug)?;
    let path = page_path(alias, slug, root);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let p = parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    same_page(&p, alias, slug)?;
    Ok(Some(p))
}

#[derive(Debug, Default)]
pub struct GuideList {
    pub pages: Vec<Page>,
    pub skipped: Vec<String>,
}

fn order_key(slug: &str) -> (u8, String) {
    let rank = match slug {
        "architecture" => 0,
        "data-model" => 1,
        "roadmap" => 2,
        _ => 3,
    };
    (rank, slug.to_string())
}

pub fn list(alias: &str, root: Option<&Path>) -> Result<GuideList, String> {
    check_alias(alias)?;
    let rd = match std::fs::read_dir(guide_dir(alias, root)) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(GuideList::default()),
        Err(e) => return Err(e.to_string()),
    };
    let mut out = GuideList::default();
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|x| x != "md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if check_slug(stem).is_err() {
            continue;
        }
        match load(alias, stem, root) {
            Ok(Some(p)) => out.pages.push(p),
            _ => out.skipped.push(stem.to_string()),
        }
    }
    out.pages.sort_by_key(|p| order_key(&p.slug));
    out.skipped.sort();
    Ok(out)
}

/// Newest first.
pub fn history(alias: &str, slug: &str, root: Option<&Path>) -> Vec<PathBuf> {
    if check_alias(alias).is_err() || check_slug(slug).is_err() {
        return Vec::new();
    }
    let prefix = format!("{slug}.");
    let mut files: Vec<PathBuf> = std::fs::read_dir(history_dir(alias, root))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "md"))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(&prefix))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files.reverse();
    files
}

/// Make a snapshot the current page (the current one is snapshotted first), under the
/// guide lock. Refused when the file is for another project or page, or holds a secret.
pub fn restore(alias: &str, slug: &str, file: &Path, root: Option<&Path>) -> io::Result<()> {
    check_alias(alias).map_err(invalid)?;
    check_slug(slug).map_err(invalid)?;
    let text = std::fs::read_to_string(file)?;
    let p = parse(&text).map_err(invalid)?;
    same_page(&p, alias, slug).map_err(invalid)?;
    validate(&p).map_err(invalid)?;
    let dir = guide_dir(alias, root);
    std::fs::create_dir_all(&dir)?;
    crate::file_lock::with_lock(&dir, || snapshot_and_write(alias, slug, &text, root))?
}

/// Load, change and save one page under the guide directory's lock. Keep `f` quick:
/// never hold this across a model call.
pub fn update_locked(
    alias: &str,
    slug: &str,
    root: Option<&Path>,
    f: impl FnOnce(Option<Page>) -> Result<Page, String>,
) -> Result<Page, String> {
    check_alias(alias)?;
    check_slug(slug)?;
    let dir = guide_dir(alias, root);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    crate::file_lock::with_lock(&dir, || {
        let p = f(load(alias, slug, root)?)?;
        same_page(&p, alias, slug)?;
        save(&p, root).map_err(|e| e.to_string())?;
        Ok(p)
    })
    .map_err(|e| e.to_string())?
}

/// Like [`update_locked`], but `f` may return `Ok(None)` to leave the page exactly as it
/// is (no write, no history snapshot).
pub fn update_if_locked(
    alias: &str,
    slug: &str,
    root: Option<&Path>,
    f: impl FnOnce(Option<Page>) -> Result<Option<Page>, String>,
) -> Result<Option<Page>, String> {
    check_alias(alias)?;
    check_slug(slug)?;
    let dir = guide_dir(alias, root);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    crate::file_lock::with_lock(&dir, || {
        let Some(p) = f(load(alias, slug, root)?)? else {
            return Ok(None);
        };
        same_page(&p, alias, slug)?;
        save(&p, root).map_err(|e| e.to_string())?;
        Ok(Some(p))
    })
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_profile::{Origin, Source, Status};

    fn part(key: &str, body: &str, origin: Origin) -> Part {
        Part {
            key: key.into(),
            body: body.into(),
            origin,
        }
    }

    pub(super) fn sample() -> Page {
        Page {
            alias: "shop".into(),
            slug: "feature-checkout".into(),
            kind: Kind::Feature,
            title: "Checkout".into(),
            status: Status::Draft,
            generated_at: None,
            head_sha: Some("abc123".into()),
            summary: "How an order is paid for.".into(),
            related: vec!["feature-cart".into()],
            parts: vec![
                part("what", "Buyers pay for a cart.", Origin::Agent),
                part("key-points", "Totals are in cents.", Origin::Human),
            ],
            sources: vec![Source {
                path: "app/models/order.rb".into(),
                sha256: "ff00".into(),
            }],
        }
    }

    #[test]
    fn slugs_map_to_kinds() {
        assert_eq!(check_slug("architecture"), Ok(Kind::Architecture));
        assert_eq!(check_slug("data-model"), Ok(Kind::DataModel));
        assert_eq!(check_slug("roadmap"), Ok(Kind::Roadmap));
        assert_eq!(check_slug("feature-meetings-2"), Ok(Kind::Feature));
    }

    #[test]
    fn bad_slugs_are_refused() {
        let long = format!("feature-{}", "a".repeat(41)); // 49 chars
        for bad in [
            "",
            "../x",
            "feature-",
            "Feature-A",
            "feature--a",
            "feature-a--b",
            "feature-a-",
            "feature_a",
            "feature-a/b",
            "other",
            "architecture2",
            long.as_str(),
        ] {
            assert!(check_slug(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(check_slug(&format!("feature-{}", "a".repeat(40))).is_ok()); // 48 chars
    }

    #[test]
    fn render_then_parse_round_trips() {
        let p = sample();
        let back = parse(&render(&p)).unwrap();
        assert_eq!(back, normalised(&p));
        assert_eq!(back.parts[1].origin, Origin::Human);
    }

    #[test]
    fn parts_render_in_the_kinds_order_whatever_the_vec_order() {
        let mut p = sample();
        p.parts.reverse();
        let text = render(&p);
        assert!(
            text.find("<!-- part: what").unwrap() < text.find("<!-- part: key-points").unwrap()
        );
    }

    #[test]
    fn a_body_that_looks_like_structure_does_not_split_the_page() {
        let mut p = sample();
        p.parts[0].body =
            "intro\n### Sources\n- `x` sha256:00\n<!-- part: key-points origin: agent -->\nmore"
                .into();
        let back = parse(&render(&p)).unwrap();
        assert_eq!(back.parts.len(), 2);
        assert_eq!(back.parts[0].body, p.parts[0].body);
        assert_eq!(back.sources, p.sources);
    }

    #[test]
    fn a_page_with_no_parts_or_sources_round_trips() {
        let mut p = sample();
        p.parts.clear();
        p.sources.clear();
        p.related.clear();
        assert_eq!(parse(&render(&p)).unwrap(), normalised(&p));
    }

    #[test]
    fn parse_refuses_a_kind_that_does_not_match_the_slug() {
        let text = render(&sample()).replace("kind: feature", "kind: roadmap");
        assert!(parse(&text).unwrap_err().contains("kind"));
    }

    #[test]
    fn parse_refuses_an_unknown_part_and_never_echoes_text() {
        let text = render(&sample()).replace("<!-- part: what ", "<!-- part: nope ");
        let err = parse(&text).unwrap_err();
        assert!(err.contains("unknown part"), "{err}");
        assert!(!err.contains("Buyers pay"), "{err}");
    }

    #[test]
    fn parse_errors_never_echo_a_bad_slug() {
        for bad in ["feature-glpat-AAAA1234", "../x"] {
            let text = render(&sample()).replace("slug: feature-checkout", &format!("slug: {bad}"));
            let err = parse(&text).unwrap_err();
            assert!(!err.contains(bad), "{err}");
        }
    }

    #[test]
    fn parse_refuses_a_duplicate_part() {
        let text = render(&sample()).replace("<!-- part: key-points ", "<!-- part: what ");
        assert!(parse(&text).unwrap_err().contains("duplicate part"));
    }

    use tempfile::tempdir;

    fn secret() -> String {
        format!("glpat-{}", "a".repeat(24))
    }

    #[test]
    fn save_then_load_round_trips_and_a_missing_page_is_none() {
        let dir = tempdir().unwrap();
        let root = Some(dir.path());
        assert_eq!(load("shop", "feature-checkout", root).unwrap(), None);
        let p = sample();
        save(&p, root).unwrap();
        assert_eq!(
            load("shop", "feature-checkout", root).unwrap(),
            Some(normalised(&p))
        );
        assert!(guide_dir("shop", root)
            .join("feature-checkout.md")
            .is_file());
    }

    #[test]
    fn a_bad_slug_or_alias_creates_nothing() {
        let dir = tempdir().unwrap();
        let root = Some(dir.path());
        assert!(load("shop", "../evil", root).is_err());
        assert!(load("../shop", "roadmap", root).is_err());
        assert!(!dir.path().join("shop").exists());
        let mut p = sample();
        p.slug = "../evil".into();
        assert!(save(&p, root).is_err());
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn a_page_with_a_secret_is_refused_without_echoing_it() {
        let dir = tempdir().unwrap();
        let mut p = sample();
        p.parts[0].body = format!("token {}", secret());
        let err = save(&p, Some(dir.path())).unwrap_err().to_string();
        assert!(err.contains("part what"), "{err}");
        assert!(!err.contains("glpat"), "{err}");
        assert_eq!(
            load("shop", "feature-checkout", Some(dir.path())).unwrap(),
            None
        );
    }

    #[test]
    fn unrepresentable_fields_are_rejected_not_dropped() {
        let dir = tempdir().unwrap();
        let root = Some(dir.path());
        let mut p = sample();
        p.title = "two\nlines".into();
        assert!(save(&p, root).is_err());
        let mut p = sample();
        p.related = vec!["Not A Slug".into()];
        assert!(save(&p, root).is_err());
        let mut p = sample();
        p.parts.push(part("what", "dup", Origin::Agent));
        assert!(save(&p, root).is_err());
        let mut p = sample();
        p.parts = vec![part("shipped", "wrong kind", Origin::Agent)];
        assert!(save(&p, root).is_err());
        let mut p = sample();
        p.sources[0].path = "a`b".into();
        assert!(save(&p, root).is_err());
    }

    #[test]
    fn validation_errors_never_echo_a_rejected_value() {
        let dir = tempdir().unwrap();
        let root = Some(dir.path());
        let s = secret();
        let mut p = sample();
        p.related = vec!["feature-ok".into(), s.clone()];
        let e = save(&p, root).unwrap_err().to_string();
        assert!(!e.contains("glpat") && e.contains("related"), "{e}");
        let mut p = sample();
        p.parts[0].key = s.clone();
        let e = save(&p, root).unwrap_err().to_string();
        assert!(!e.contains("glpat"), "{e}");
        let mut p = sample();
        p.sources[0].path = format!("{s}`");
        let e = save(&p, root).unwrap_err().to_string();
        assert!(!e.contains("glpat"), "{e}");
        let mut p = sample();
        p.sources[0].sha256 = format!("{s} x");
        let e = save(&p, root).unwrap_err().to_string();
        assert!(!e.contains("glpat"), "{e}");
        let mut p = sample();
        p.slug = s;
        let e = save(&p, root).unwrap_err().to_string();
        assert!(!e.contains("glpat"), "{e}");
    }

    #[test]
    fn saving_over_a_page_snapshots_it_and_keeps_twenty() {
        let dir = tempdir().unwrap();
        let root = Some(dir.path());
        let mut p = sample();
        for i in 0..23 {
            p.parts[0].body = format!("version {i}");
            save(&p, root).unwrap();
        }
        let h = history("shop", "feature-checkout", root);
        assert_eq!(h.len(), 20);
        // newest first: the newest snapshot is the one made before the last save.
        let newest = parse(&std::fs::read_to_string(&h[0]).unwrap()).unwrap();
        assert_eq!(newest.parts[0].body, "version 21");
    }

    #[test]
    fn history_is_per_page() {
        let dir = tempdir().unwrap();
        let root = Some(dir.path());
        let a = sample();
        let mut b = sample();
        b.slug = "feature-cart".into();
        b.related.clear();
        save(&a, root).unwrap();
        save(&a, root).unwrap();
        save(&b, root).unwrap();
        assert_eq!(history("shop", "feature-checkout", root).len(), 1);
        assert!(history("shop", "feature-cart", root).is_empty());
    }

    #[test]
    fn restore_makes_a_snapshot_current_and_snapshots_the_replaced_page() {
        let dir = tempdir().unwrap();
        let root = Some(dir.path());
        let mut p = sample();
        p.parts[0].body = "first".into();
        save(&p, root).unwrap();
        p.parts[0].body = "second".into();
        save(&p, root).unwrap();
        let snap = history("shop", "feature-checkout", root).remove(0);
        restore("shop", "feature-checkout", &snap, root).unwrap();
        let now = load("shop", "feature-checkout", root).unwrap().unwrap();
        assert_eq!(now.parts[0].body, "first");
        assert_eq!(history("shop", "feature-checkout", root).len(), 2);
    }

    #[test]
    fn restore_refuses_another_projects_or_pages_file() {
        let dir = tempdir().unwrap();
        let root = Some(dir.path());
        save(&sample(), root).unwrap();
        let other = dir.path().join("other.md");
        let mut o = sample();
        o.alias = "elsewhere".into();
        std::fs::write(&other, render(&o)).unwrap();
        assert!(restore("shop", "feature-checkout", &other, root).is_err());
        let mut o = sample();
        o.slug = "feature-cart".into();
        std::fs::write(&other, render(&o)).unwrap();
        assert!(restore("shop", "feature-checkout", &other, root).is_err());
    }

    #[test]
    fn load_refuses_a_file_that_belongs_to_another_project() {
        let dir = tempdir().unwrap();
        let root = Some(dir.path());
        let mut p = sample();
        p.alias = "elsewhere".into();
        let gd = guide_dir("shop", root);
        std::fs::create_dir_all(&gd).unwrap();
        std::fs::write(gd.join("feature-checkout.md"), render(&p)).unwrap();
        assert!(load("shop", "feature-checkout", root)
            .unwrap_err()
            .contains("another project"));
    }

    #[test]
    fn list_orders_pages_and_ignores_non_pages_and_names_broken_ones() {
        let dir = tempdir().unwrap();
        let root = Some(dir.path());
        let mut roadmap = sample();
        roadmap.slug = "roadmap".into();
        roadmap.kind = Kind::Roadmap;
        roadmap.parts = vec![part("planned", "x", Origin::Agent)];
        roadmap.related.clear();
        let mut arch = roadmap.clone();
        arch.slug = "architecture".into();
        arch.kind = Kind::Architecture;
        arch.parts = vec![part("overview", "x", Origin::Agent)];
        let mut zed = sample();
        zed.slug = "feature-zed".into();
        zed.related.clear();
        let mut abe = zed.clone();
        abe.slug = "feature-abe".into();
        for p in [&zed, &roadmap, &abe, &arch] {
            save(p, root).unwrap();
        }
        save(&zed, root).unwrap(); // creates .history/
        let gd = guide_dir("shop", root);
        std::fs::write(gd.join("plan.json"), "{}").unwrap();
        std::fs::write(gd.join("feature-x.md.tmp"), "junk").unwrap();
        std::fs::write(gd.join("feature-broken.md"), "not a page").unwrap();
        std::fs::write(gd.join("Not A Slug.md"), "x").unwrap();
        let l = list("shop", root).unwrap();
        let slugs: Vec<&str> = l.pages.iter().map(|p| p.slug.as_str()).collect();
        assert_eq!(
            slugs,
            ["architecture", "roadmap", "feature-abe", "feature-zed"]
        );
        assert_eq!(l.skipped, ["feature-broken"]);
    }

    #[test]
    fn list_of_a_project_with_no_guide_is_empty() {
        let dir = tempdir().unwrap();
        let l = list("shop", Some(dir.path())).unwrap();
        assert!(l.pages.is_empty() && l.skipped.is_empty());
    }

    #[test]
    fn update_locked_sees_the_current_page_and_saves_the_result() {
        let dir = tempdir().unwrap();
        let root = Some(dir.path());
        let first = update_locked("shop", "feature-checkout", root, |cur| {
            assert!(cur.is_none());
            Ok(sample())
        })
        .unwrap();
        assert_eq!(first.slug, "feature-checkout");
        let second = update_locked("shop", "feature-checkout", root, |cur| {
            let mut p = cur.unwrap();
            p.status = Status::Approved;
            Ok(p)
        })
        .unwrap();
        assert_eq!(second.status, Status::Approved);
        assert_eq!(
            load("shop", "feature-checkout", root)
                .unwrap()
                .unwrap()
                .status,
            Status::Approved
        );
    }

    #[test]
    fn update_locked_refuses_a_page_for_another_slug() {
        let dir = tempdir().unwrap();
        let r = update_locked("shop", "feature-cart", Some(dir.path()), |_| Ok(sample()));
        assert!(r.is_err());
    }

    #[test]
    fn update_if_locked_writes_nothing_when_told_to_leave_the_page() {
        let dir = tempdir().unwrap();
        let root = Some(dir.path());
        let none = update_if_locked("shop", "feature-checkout", root, |_| Ok(None)).unwrap();
        assert!(none.is_none());
        assert!(load("shop", "feature-checkout", root).unwrap().is_none());
        update_if_locked("shop", "feature-checkout", root, |_| Ok(Some(sample()))).unwrap();
        update_if_locked("shop", "feature-checkout", root, |cur| {
            assert!(cur.is_some());
            Ok(None)
        })
        .unwrap();
        assert!(history("shop", "feature-checkout", root).is_empty());
        assert!(load("shop", "feature-checkout", root).unwrap().is_some());
        let wrong = update_if_locked("shop", "feature-cart", root, |_| Ok(Some(sample())));
        assert!(wrong.is_err());
    }
}
