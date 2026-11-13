//! A project's guide: a small wiki of pages written from its source (Architecture,
//! Data model, Roadmap, one page per feature). One markdown file per page under
//! `<memory root>/<alias>/guide/`. The file format mirrors `PROFILE.md`: parts carry an
//! origin so a hand-edited part survives a rebuild, saves are round-trip checked, and
//! every save snapshots the old file.

use crate::memory_store::parse_source_line;
use crate::project_profile::{
    escape_body_by, one_line, parse_time, ts, unescape_line_by, Origin, Source, Status,
};
use chrono::{DateTime, Timelike, Utc};

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
// Used only by tests until the storage layer (next task) calls it for its round-trip check.
#[allow(dead_code)]
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
}
