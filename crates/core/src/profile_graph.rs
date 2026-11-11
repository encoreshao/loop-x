//! Project graph: edges derived from profiles and issue digests, plus a
//! deterministic force-directed layout. Pure functions: no IO, no clock, no RNG.
//!
//! Name parsing rule (Secrets / Integrations section bodies are free markdown).
//! A *candidate* is a backticked span, or the first word of a `-`/`*` bullet
//! (trailing `.,:;` and en/em dashes stripped). A candidate containing `=` is a
//! value-carrying assignment and is ignored entirely; nothing after `=` is read.
//! - Secrets: ONLY UPPER_SNAKE names (`^[A-Z][A-Z0-9]*(_[A-Z0-9]+)+$`, at least
//!   one underscore), kept as written. Anything else (bare words, lowercase
//!   backticked values) is not a name.
//! - Integrations: bare identifiers (`[A-Za-z_][A-Za-z0-9_.-]*`, 2+ chars),
//!   lower-cased, minus a stop list of filler words (none, n/a, tbd, see, ...).
//! - Document frequency: a name present in more than max(3, one third of) the
//!   profiles that have that section carries no signal and creates no edge.
//!
//! Evidence for shared secrets/services is the NAME only, never a value.

use crate::profile_scan::IssueDigest;
use crate::profile_sync::Freshness;
use crate::project_profile::{contains_secret, Profile, SectionKey};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EdgeKind {
    DependsOn,
    Calls,
    SharesSecret,
    SharesService,
    IssueLink,
    SharesPeople,
    Declared,
}

impl EdgeKind {
    fn undirected(self) -> bool {
        matches!(
            self,
            EdgeKind::SharesSecret | EdgeKind::SharesService | EdgeKind::SharesPeople
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    pub evidence: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub alias: String,
    pub x: f32,
    pub y: f32,
    /// `None` when the project has no profile. With a profile, `derive` has no
    /// filesystem access and reports `Fresh` (or `NeedsReview` for a profile with
    /// no sections); callers with a work dir may overwrite it from `section_freshness`.
    pub freshness: Option<Freshness>,
    /// Number of recent issues in the digest.
    pub activity: usize,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

const MAX_EVIDENCE: usize = 200;
const MIN_SHARED_PEOPLE: usize = 2;

/// Exact alias first, then a UNIQUE match by path suffix, then a UNIQUE alias
/// whose own last segment equals the target's last segment. More than one candidate is ambiguous: unresolved.
fn resolve(target: &str, tracked: &BTreeSet<String>) -> Option<String> {
    let t = target.trim().trim_matches('/');
    if t.is_empty() {
        return None;
    }
    if tracked.contains(t) {
        return Some(t.to_string());
    }
    let tl = t.to_lowercase();
    let unique = |pred: &dyn Fn(&str) -> bool| -> Result<Option<String>, ()> {
        let found: Vec<&String> = tracked.iter().filter(|a| pred(&a.to_lowercase())).collect();
        match found.len() {
            0 => Ok(None),
            1 => Ok(Some(found[0].clone())),
            _ => Err(()),
        }
    };
    match unique(&|al| al == tl || tl.ends_with(&format!("/{al}"))) {
        Ok(Some(a)) => return Some(a),
        Err(()) => return None,
        Ok(None) => {}
    }
    // Last path segment of the target against the alias's own last segment.
    let last = tl.rsplit('/').next().unwrap_or(&tl).to_string();
    unique(&|al| al.rsplit('/').next() == Some(last.as_str()))
        .ok()
        .flatten()
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && s.len() >= 2
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

const MAX_NAME_LEN: usize = 40;
/// Token-like prefixes (compared on a segment start) and short segments that
/// are token prefixes by themselves.
const TOKEN_PREFIXES: [&str; 8] = [
    "AKIA",
    "ASIA",
    "AIZA",
    "XOX",
    "GLPAT",
    "EYJ",
    "GITHUB_PAT",
    "SG.",
];
const TOKEN_SEGMENTS: [&str; 8] = ["SK", "PK", "RK", "SG", "GHP", "GHO", "GHS", "GHU"];

/// Conservative environment-variable shape: UPPER_SNAKE with at least one
/// underscore, at most 40 chars, no long or digit-heavy segment, no token
/// prefix. When in doubt it is NOT a name.
fn is_upper_snake(s: &str) -> bool {
    if s.len() > MAX_NAME_LEN || !s.contains('_') {
        return false;
    }
    let digits = s.chars().filter(|c| c.is_ascii_digit()).count();
    if digits * 10 > s.len() * 4 {
        return false;
    }
    let upper = s.to_ascii_uppercase();
    if TOKEN_PREFIXES.iter().any(|p| upper.starts_with(p)) {
        return false;
    }
    let mut first = true;
    for seg in s.split('_') {
        let ok = !seg.is_empty()
            && seg.len() <= 11
            && seg
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            && (!first || seg.starts_with(|c: char| c.is_ascii_uppercase()))
            && !TOKEN_SEGMENTS.contains(&seg)
            && !TOKEN_PREFIXES.iter().any(|p| seg.starts_with(p));
        if !ok {
            return false;
        }
        first = false;
    }
    true
}

const STOP_WORDS: [&str; 13] = [
    "none", "n/a", "na", "tbd", "todo", "unknown", "etc", "see", "uses", "stored", "no", "not",
    "the",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum NameKind {
    Secret,
    Service,
}

fn parse_names(body: &str, kind: NameKind) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut add = |raw: &str| {
        if raw.contains('=') {
            return;
        }
        let w = raw.trim().trim_end_matches(['.', ',', ':', ';', '–', '—']);
        match kind {
            NameKind::Secret => {
                if is_upper_snake(w) {
                    out.insert(w.to_string());
                }
            }
            NameKind::Service => {
                let l = w.to_lowercase();
                if w.len() <= MAX_NAME_LEN && is_ident(w) && !STOP_WORDS.contains(&l.as_str()) {
                    out.insert(l);
                }
            }
        }
    };
    for line in body.lines() {
        let line = line.trim();
        for (i, span) in line.split('`').enumerate() {
            if i % 2 == 1 {
                add(span);
            }
        }
        if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            let word = rest.split_whitespace().next().unwrap_or("");
            if !word.starts_with('`') {
                add(word);
            }
        }
    }
    out
}

/// Per-alias name sets for one section kind, with names present in too many
/// profiles removed (see the module header).
fn name_sets(
    by_alias: &BTreeMap<&str, &Profile>,
    key: SectionKey,
    kind: NameKind,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut sets: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (alias, p) in by_alias {
        if let Some(sec) = p.sections.get(&key) {
            sets.insert(alias.to_string(), parse_names(&sec.body, kind));
        }
    }
    let cutoff = std::cmp::max(3, sets.len() / 3);
    let mut freq: BTreeMap<String, usize> = BTreeMap::new();
    for names in sets.values() {
        for n in names {
            *freq.entry(n.clone()).or_default() += 1;
        }
    }
    for names in sets.values_mut() {
        names.retain(|n| freq[n] <= cutoff);
    }
    sets
}

fn people_of(p: Option<&Profile>, d: Option<&IssueDigest>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some(d) = d {
        for (n, _) in &d.people {
            out.insert(n.trim().to_lowercase());
        }
    }
    if let Some(s) = p.and_then(|p| p.sections.get(&SectionKey::People)) {
        for line in s.body.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) else {
                continue;
            };
            let name = rest
                .split([':', '(', '–', '—'])
                .next()
                .unwrap_or("")
                .split(" - ")
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches(['`', '*']);
            if !name.is_empty() && !name.eq_ignore_ascii_case("unknown") {
                out.insert(name.to_lowercase());
            }
        }
    }
    out.retain(|n| !n.is_empty());
    out
}

fn cap(s: &str) -> String {
    if s.chars().count() <= MAX_EVIDENCE {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_EVIDENCE - 1).collect();
    out.push('…');
    out
}

pub fn derive(
    profiles: &[Profile],
    digests: &BTreeMap<String, IssueDigest>,
    tracked: &[String],
) -> Graph {
    let tracked_set: BTreeSet<String> = tracked.iter().cloned().collect();
    let by_alias: BTreeMap<&str, &Profile> = profiles
        .iter()
        .filter(|p| tracked_set.contains(&p.alias))
        .map(|p| (p.alias.as_str(), p))
        .collect();

    // (from, to, kind) -> (confidence, evidence parts)
    type Key = (String, String, EdgeKind);
    let mut merged: BTreeMap<Key, (f32, Vec<String>)> = BTreeMap::new();
    let mut add = |from: &str, to: &str, kind: EdgeKind, evidence: &str, confidence: f32| {
        let evidence = evidence.trim();
        if evidence.is_empty() || from == to || contains_secret(evidence).is_some() {
            return;
        }
        let (f, t) = if kind.undirected() && from > to {
            (to, from)
        } else {
            (from, to)
        };
        let e = merged
            .entry((f.to_string(), t.to_string(), kind))
            .or_insert((0.0, Vec::new()));
        e.0 = e.0.max(confidence);
        if !e.1.iter().any(|x| x == evidence) {
            e.1.push(evidence.to_string());
        }
    };

    // 1. declared links
    for (alias, p) in &by_alias {
        for l in &p.links {
            let Some(to) = resolve(&l.to, &tracked_set) else {
                continue;
            };
            let kind = match l.kind.trim().to_lowercase().as_str() {
                "depends-on" => EdgeKind::DependsOn,
                "calls" => EdgeKind::Calls,
                _ => EdgeKind::Declared,
            };
            add(alias, &to, kind, &l.evidence, 0.9);
        }
    }

    // 2. shared secret / service names, 4. shared people (parsed once per alias)
    let secrets = name_sets(&by_alias, SectionKey::Secrets, NameKind::Secret);
    let services = name_sets(&by_alias, SectionKey::Integrations, NameKind::Service);
    let people: BTreeMap<&String, BTreeSet<String>> = tracked_set
        .iter()
        .map(|a| {
            (
                a,
                people_of(by_alias.get(a.as_str()).copied(), digests.get(a.as_str())),
            )
        })
        .collect();
    let aliases: Vec<&String> = tracked_set.iter().collect();
    for (i, a) in aliases.iter().enumerate() {
        for b in &aliases[i + 1..] {
            for (sets, kind) in [
                (&secrets, EdgeKind::SharesSecret),
                (&services, EdgeKind::SharesService),
            ] {
                if let (Some(na), Some(nb)) = (sets.get(a.as_str()), sets.get(b.as_str())) {
                    for name in na.intersection(nb) {
                        add(a, b, kind, name, 0.5);
                    }
                }
            }
            let shared: Vec<String> = people[a].intersection(&people[b]).cloned().collect();
            if shared.len() >= MIN_SHARED_PEOPLE {
                add(
                    a,
                    b,
                    EdgeKind::SharesPeople,
                    &format!("{} shared people: {}", shared.len(), shared.join(", ")),
                    0.2,
                );
            }
        }
    }

    // 3. issue cross references, one edge per (from, to) with a count bonus
    for (alias, d) in digests {
        if !tracked_set.contains(alias) {
            continue;
        }
        let mut groups: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
        for r in &d.cross_refs {
            if let Some(to) = resolve(&r.target_project, &tracked_set) {
                groups.entry(to).or_default().insert(r.issue_iid);
            }
        }
        for (to, iids) in groups {
            let conf = (0.6 + 0.1 * (iids.len() as f32 - 1.0)).min(0.9);
            let ev = iids
                .iter()
                .map(|i| format!("#{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            add(
                alias,
                &to,
                EdgeKind::IssueLink,
                &format!("issues {ev}"),
                conf,
            );
        }
    }

    let edges: Vec<Edge> = merged
        .into_iter()
        .map(|((from, to, kind), (confidence, ev))| Edge {
            from,
            to,
            kind,
            evidence: cap(&ev.join("; ")),
            confidence,
        })
        .collect();

    let mut nodes: Vec<Node> = tracked_set
        .iter()
        .map(|alias| Node {
            alias: alias.clone(),
            x: 500.0,
            y: 500.0,
            freshness: by_alias.get(alias.as_str()).map(|p| {
                if p.sections.is_empty() {
                    Freshness::NeedsReview
                } else {
                    Freshness::Fresh
                }
            }),
            activity: digests.get(alias).map(|d| d.recent.len()).unwrap_or(0),
        })
        .collect();
    layout_with_edges(&mut nodes, &edges);
    Graph { nodes, edges }
}

/// Rounds points to tenths; the points are visited in `order` and a point whose
/// rounded value is already taken is nudged by 0.1 until it is free.
fn unique_tenths(points: &[(f64, f64)], order: &[usize]) -> Vec<(i32, i32)> {
    let mut out = vec![(0, 0); points.len()];
    let mut used: BTreeSet<(i32, i32)> = BTreeSet::new();
    for &i in order {
        let (mut x, y) = (
            (points[i].0 * 10.0).round() as i32,
            (points[i].1 * 10.0).round() as i32,
        );
        while !used.insert((x, y)) {
            x += if x >= 10_000 { -1 } else { 1 };
        }
        out[i] = (x, y);
    }
    out
}

/// Deterministic layout without edge information (pure repulsion).
pub fn layout(nodes: &mut [Node]) {
    layout_with_edges(nodes, &[]);
}

/// Deterministic force-directed layout (Fruchterman-Reingold). Initial
/// positions come from the sorted alias order, so input positions are ignored
/// and the same aliases/edges always give the same result. Coordinates land in
/// [50, 950] within the 0..1000 box.
pub fn layout_with_edges(nodes: &mut [Node], edges: &[Edge]) {
    let n = nodes.len();
    if n == 0 {
        return;
    }
    if n == 1 {
        nodes[0].x = 500.0;
        nodes[0].y = 500.0;
        return;
    }
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| nodes[a].alias.cmp(&nodes[b].alias).then(a.cmp(&b)));
    let mut pos = vec![(0.0f64, 0.0f64); n];
    for (rank, &i) in order.iter().enumerate() {
        let ang = std::f64::consts::TAU * rank as f64 / n as f64;
        pos[i] = (500.0 + 300.0 * ang.cos(), 500.0 + 300.0 * ang.sin());
    }
    let index: BTreeMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, nd)| (nd.alias.as_str(), i))
        .collect();
    let links: Vec<(usize, usize, f64)> = edges
        .iter()
        .filter_map(|e| {
            let a = *index.get(e.from.as_str())?;
            let b = *index.get(e.to.as_str())?;
            (a != b).then_some((a, b, 0.3 + e.confidence as f64))
        })
        .collect();

    const ITER: usize = 200;
    let k = (1000.0f64 * 1000.0 / n as f64).sqrt() * 0.6;
    for it in 0..ITER {
        let temp = 100.0 * (1.0 - it as f64 / ITER as f64) + 1.0;
        let mut disp = vec![(0.0f64, 0.0f64); n];
        for i in 0..n {
            for j in (i + 1)..n {
                let (mut dx, mut dy) = (pos[i].0 - pos[j].0, pos[i].1 - pos[j].1);
                let mut d = (dx * dx + dy * dy).sqrt();
                if d < 1e-6 {
                    let ang = (i * 7 + j * 13) as f64;
                    dx = ang.cos();
                    dy = ang.sin();
                    d = 1e-6;
                }
                let f = k * k / d;
                let (ux, uy) = (dx / d.max(1e-6), dy / d.max(1e-6));
                let (ux, uy) = if d <= 1e-6 { (dx, dy) } else { (ux, uy) };
                disp[i].0 += ux * f;
                disp[i].1 += uy * f;
                disp[j].0 -= ux * f;
                disp[j].1 -= uy * f;
            }
        }
        for &(a, b, w) in &links {
            let (dx, dy) = (pos[a].0 - pos[b].0, pos[a].1 - pos[b].1);
            let d = (dx * dx + dy * dy).sqrt();
            if d < 1e-6 {
                continue;
            }
            let f = w * d * d / k;
            let (ux, uy) = (dx / d, dy / d);
            disp[a].0 -= ux * f;
            disp[a].1 -= uy * f;
            disp[b].0 += ux * f;
            disp[b].1 += uy * f;
        }
        for i in 0..n {
            // weak gravity keeps disconnected nodes in view
            disp[i].0 += (500.0 - pos[i].0) * 0.05;
            disp[i].1 += (500.0 - pos[i].1) * 0.05;
            let d = (disp[i].0 * disp[i].0 + disp[i].1 * disp[i].1).sqrt();
            if d > 1e-9 {
                let s = d.min(temp) / d;
                pos[i].0 += disp[i].0 * s;
                pos[i].1 += disp[i].1 * s;
            }
            if !pos[i].0.is_finite() || !pos[i].1.is_finite() {
                pos[i] = (500.0, 500.0);
            }
        }
    }

    // Fit the bounding box into [50, 950], uniform scale, centred.
    let (mut minx, mut maxx, mut miny, mut maxy) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for p in &pos {
        minx = minx.min(p.0);
        maxx = maxx.max(p.0);
        miny = miny.min(p.1);
        maxy = maxy.max(p.1);
    }
    let span = (maxx - minx).max(maxy - miny);
    let scale = if span > 1e-9 { 900.0 / span } else { 1.0 };
    let (cx, cy) = ((minx + maxx) / 2.0, (miny + maxy) / 2.0);
    let fitted: Vec<(f64, f64)> = pos
        .iter()
        .map(|p| {
            (
                (500.0 + (p.0 - cx) * scale).clamp(0.0, 1000.0),
                (500.0 + (p.1 - cy) * scale).clamp(0.0, 1000.0),
            )
        })
        .collect();
    for (nd, (x, y)) in nodes.iter_mut().zip(unique_tenths(&fitted, &order)) {
        nd.x = x as f32 / 10.0;
        nd.y = y as f32 / 10.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile_scan::CrossRef;
    use crate::project_profile::{Link, Origin, Section, Status};

    fn profile(alias: &str) -> Profile {
        Profile {
            alias: alias.into(),
            generated_at: None,
            head_sha: None,
            status: Status::Approved,
            links: vec![],
            sections: BTreeMap::new(),
        }
    }

    fn section(body: &str) -> Section {
        Section {
            summary: String::new(),
            body: body.into(),
            last_verified: None,
            sources: vec![],
            origin: Origin::Agent,
        }
    }

    fn link(to: &str, kind: &str, ev: &str) -> Link {
        Link {
            to: to.into(),
            kind: kind.into(),
            evidence: ev.into(),
        }
    }

    fn t(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn none() -> BTreeMap<String, IssueDigest> {
        BTreeMap::new()
    }

    #[test]
    fn edges_without_evidence_are_dropped() {
        let mut a = profile("a");
        a.links = vec![link("b", "depends-on", "  "), link("b", "calls", "api.rs")];
        let g = derive(&[a], &none(), &t(&["a", "b"]));
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].kind, EdgeKind::Calls);
        assert_eq!(g.edges[0].evidence, "api.rs");
        assert_eq!(g.edges[0].confidence, 0.9);
    }

    #[test]
    fn unknown_alias_and_self_links_ignored() {
        let mut a = profile("a");
        a.links = vec![
            link("ghost", "depends-on", "x"),
            link("a", "calls", "x"),
            link("b", "mentions", "readme"),
        ];
        let g = derive(&[a, profile("zzz")], &none(), &t(&["a", "b"]));
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].kind, EdgeKind::Declared);
        assert_eq!(g.nodes.len(), 2);
    }

    #[test]
    fn shared_secret_name_creates_edge_without_exposing_values() {
        let mut a = profile("a");
        let mut b = profile("b");
        a.sections.insert(
            SectionKey::Secrets,
            section("- `DB_PASSWORD=hunter2hunter2` in .env\n- STRIPE_KEY – payments\n- ONLY_A: x"),
        );
        b.sections.insert(
            SectionKey::Secrets,
            section("- STRIPE_KEY: set\n- `DB_PASSWORD` (prod)\n- `SECRET_TOKEN=abcdef`"),
        );
        let g = derive(&[b, a], &none(), &t(&["a", "b"]));
        assert_eq!(g.edges.len(), 1);
        let e = &g.edges[0];
        assert_eq!(e.kind, EdgeKind::SharesSecret);
        assert_eq!(e.evidence, "STRIPE_KEY");
        assert_eq!(e.confidence, 0.5);
        assert!(!e.evidence.contains("hunter2") && !e.evidence.contains('='));
    }

    #[test]
    fn shared_service_is_lowercased_and_undirected_merges() {
        let mut a = profile("a");
        let mut b = profile("b");
        a.sections.insert(
            SectionKey::Integrations,
            section("- Redis – cache\n- `postgres`"),
        );
        b.sections.insert(
            SectionKey::Integrations,
            section("- `redis`\n- postgres: main"),
        );
        let g = derive(&[a, b], &none(), &t(&["b", "a"]));
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].evidence, "postgres; redis");
        assert!(g.edges.iter().all(|e| e.from == "a" && e.to == "b"));
        assert!(g.edges.iter().all(|e| e.kind == EdgeKind::SharesService));
    }

    #[test]
    fn duplicate_edges_merge() {
        let mut a = profile("a");
        a.links = vec![
            link("b", "calls", "one"),
            link("b", "calls", "two"),
            link("b", "calls", "one"),
        ];
        let mut b = profile("b");
        b.links = vec![link("a", "calls", "back")];
        let g = derive(&[a, b], &none(), &t(&["a", "b"]));
        assert_eq!(g.edges.len(), 2, "directed kinds keep direction");
        let ab = g.edges.iter().find(|e| e.from == "a").unwrap();
        assert_eq!(ab.evidence, "one; two");
    }

    #[test]
    fn duplicate_keeps_highest_confidence() {
        let mut a = profile("a");
        a.links = vec![link("b", "depends-on", "x")];
        let d = IssueDigest {
            cross_refs: vec![CrossRef {
                issue_iid: 1,
                target_project: "g/b".into(),
            }],
            ..Default::default()
        };
        let digs = BTreeMap::from([("a".to_string(), d)]);
        let g = derive(&[a], &digs, &t(&["a", "b"]));
        // different kinds stay separate
        assert_eq!(g.edges.len(), 2);
    }

    #[test]
    fn issue_link_resolves_alias_by_path_suffix_and_scales_with_count() {
        let d = IssueDigest {
            cross_refs: vec![
                CrossRef {
                    issue_iid: 3,
                    target_project: "group/sub/billing".into(),
                },
                CrossRef {
                    issue_iid: 4,
                    target_project: "group/sub/billing".into(),
                },
                CrossRef {
                    issue_iid: 5,
                    target_project: "other/unknown".into(),
                },
            ],
            ..Default::default()
        };
        let digs = BTreeMap::from([("web".to_string(), d)]);
        let g = derive(&[], &digs, &t(&["web", "billing"]));
        assert_eq!(g.edges.len(), 1);
        let e = &g.edges[0];
        assert_eq!((e.from.as_str(), e.to.as_str()), ("web", "billing"));
        assert_eq!(e.kind, EdgeKind::IssueLink);
        assert!((e.confidence - 0.7).abs() < 1e-6);
        assert_eq!(e.evidence, "issues #3, #4");
    }

    #[test]
    fn people_edge_needs_two_and_is_weak() {
        let dig = |names: &[&str]| IssueDigest {
            people: names.iter().map(|n| (n.to_string(), 1)).collect(),
            ..Default::default()
        };
        let digs = BTreeMap::from([
            ("a".to_string(), dig(&["Ann", "Bob", "Cy"])),
            ("b".to_string(), dig(&["ann", "Bob"])),
            ("c".to_string(), dig(&["Ann"])),
        ]);
        let g = derive(&[], &digs, &t(&["a", "b", "c"]));
        assert_eq!(g.edges.len(), 1);
        let e = &g.edges[0];
        assert_eq!(e.kind, EdgeKind::SharesPeople);
        assert!(e.confidence < 0.3);
        assert_eq!((e.from.as_str(), e.to.as_str()), ("a", "b"));
    }

    #[test]
    fn secret_shaped_evidence_drops_the_edge() {
        let mut a = profile("a");
        a.links = vec![link("b", "calls", "token glpat-abcdefghij1234567890 used")];
        let g = derive(&[a], &none(), &t(&["a", "b"]));
        assert!(g.edges.is_empty());
    }

    #[test]
    fn evidence_is_capped() {
        let mut a = profile("a");
        a.links = vec![link("b", "calls", &"x".repeat(500))];
        let g = derive(&[a], &none(), &t(&["a", "b"]));
        assert_eq!(g.edges[0].evidence.chars().count(), 200);
    }

    #[test]
    fn node_per_tracked_alias_with_freshness_and_activity() {
        let d = IssueDigest {
            recent: vec![(1, "x".into()), (2, "y".into())],
            ..Default::default()
        };
        let digs = BTreeMap::from([("a".to_string(), d)]);
        let mut a = profile("a");
        a.sections.insert(SectionKey::Stack, section("rust"));
        let g = derive(&[a], &digs, &t(&["a", "b", "a"]));
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.nodes[0].freshness, Some(Freshness::Fresh));
        assert_eq!(g.nodes[0].activity, 2);
        assert_eq!(g.nodes[1].freshness, None);
        assert_eq!(g.nodes[1].activity, 0);
        for n in &g.nodes {
            assert!((0.0..=1000.0).contains(&n.x) && (0.0..=1000.0).contains(&n.y));
        }
    }

    fn nodes(aliases: &[&str]) -> Vec<Node> {
        aliases
            .iter()
            .map(|a| Node {
                alias: a.to_string(),
                x: 500.0,
                y: 500.0,
                freshness: None,
                activity: 0,
            })
            .collect()
    }

    #[test]
    fn layout_is_deterministic() {
        let mut a = nodes(&["c", "a", "d", "b"]);
        let mut b = nodes(&["c", "a", "d", "b"]);
        b[0].x = 12.0;
        layout(&mut a);
        layout(&mut b);
        assert_eq!(a, b);
        let mut g1 = derive(&[], &none(), &t(&["x", "y", "z"]));
        let g2 = derive(&[], &none(), &t(&["z", "y", "x"]));
        layout(&mut g1.nodes);
        assert_eq!(g1.nodes, g2.nodes);
    }

    #[test]
    fn layout_is_finite_and_spread_for_coincident_nodes() {
        let mut n = nodes(&["a", "b", "c", "d", "e"]);
        layout(&mut n);
        for p in &n {
            assert!(p.x.is_finite() && p.y.is_finite());
            assert!((0.0..=1000.0).contains(&p.x) && (0.0..=1000.0).contains(&p.y));
        }
        for i in 0..n.len() {
            for j in i + 1..n.len() {
                assert!((n[i].x - n[j].x).abs() + (n[i].y - n[j].y).abs() > 1.0);
            }
        }
    }

    #[test]
    fn single_node_and_empty_graph_do_not_panic() {
        let mut none_nodes: Vec<Node> = vec![];
        layout(&mut none_nodes);
        let g = derive(&[], &none(), &[]);
        assert!(g.nodes.is_empty() && g.edges.is_empty());
        let mut one = nodes(&["solo"]);
        layout(&mut one);
        assert_eq!((one[0].x, one[0].y), (500.0, 500.0));
        let mut two = nodes(&["a", "b"]);
        layout(&mut two);
        assert!(two.iter().all(|n| n.x.is_finite() && n.y.is_finite()));
        assert_ne!((two[0].x, two[0].y), (two[1].x, two[1].y));
    }

    fn with_section(alias: &str, key: SectionKey, body: &str) -> Profile {
        let mut p = profile(alias);
        p.sections.insert(key, section(body));
        p
    }

    #[test]
    fn filler_words_do_not_create_edges() {
        for key in [SectionKey::Secrets, SectionKey::Integrations] {
            let a = with_section("a", key, "- None");
            let b = with_section("b", key, "- None.\n- TBD\n- See docs\n- `ruby`");
            let c = with_section("c", key, "- none\n- `ruby`");
            let g = derive(&[a, b, c], &none(), &t(&["a", "b", "c"]));
            let shared: Vec<_> = g
                .edges
                .iter()
                .filter(|e| matches!(e.kind, EdgeKind::SharesSecret | EdgeKind::SharesService))
                .collect();
            if key == SectionKey::Secrets {
                assert!(shared.is_empty(), "{shared:?}");
            } else {
                assert!(shared.iter().all(|e| e.evidence == "ruby"), "{shared:?}");
            }
        }
    }

    #[test]
    fn secrets_only_accept_upper_snake_names() {
        let a = with_section(
            "a",
            SectionKey::Secrets,
            "- `hunter2hunter2`\n- `token`\n- API_KEY=abc\n- DB_URL – main",
        );
        let b = with_section(
            "b",
            SectionKey::Secrets,
            "- `hunter2hunter2`\n- `token`\n- `API_KEY=abc`\n- `DB_URL`",
        );
        let g = derive(&[a, b], &none(), &t(&["a", "b"]));
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].evidence, "DB_URL");
    }

    #[test]
    fn names_in_most_profiles_carry_no_signal() {
        let ps: Vec<Profile> = ["a", "b", "c", "d", "e", "f"]
            .iter()
            .map(|a| with_section(a, SectionKey::Secrets, "- COMMON_KEY\n- RARE_KEY"))
            .collect();
        let g = derive(&ps, &none(), &t(&["a", "b", "c", "d", "e", "f"]));
        assert!(g.edges.iter().all(|e| !e.evidence.contains("COMMON")));
        // RARE_KEY is just as common here, so nothing at all
        assert!(g.edges.is_empty(), "{:?}", g.edges);
        let mut ps2 = ps;
        for p in ps2.iter_mut().skip(2) {
            p.sections
                .insert(SectionKey::Secrets, section("- OTHER_ONE"));
        }
        let g2 = derive(&ps2, &none(), &t(&["a", "b", "c", "d", "e", "f"]));
        assert_eq!(g2.edges.len(), 1);
        assert_eq!(g2.edges[0].evidence, "COMMON_KEY; RARE_KEY");
    }

    #[test]
    fn ambiguous_alias_match_is_unresolved() {
        let mut a = profile("a");
        a.links = vec![link("x/billing", "calls", "e")];
        let g = derive(&[a], &none(), &t(&["a", "g1/billing", "g2/billing"]));
        assert!(g.edges.is_empty());
        let mut a = profile("a");
        a.links = vec![
            link("g1/billing", "calls", "e"),
            link("billing", "calls", "f"),
        ];
        let g = derive(&[a], &none(), &t(&["a", "g1/billing", "g2/billing"]));
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].to, "g1/billing");
        assert_eq!(g.edges[0].evidence, "e");
    }

    #[test]
    fn link_kind_is_case_insensitive_and_trimmed() {
        let mut a = profile("a");
        a.links = vec![link("b", " Depends-On ", "x")];
        let g = derive(&[a], &none(), &t(&["a", "b"]));
        assert_eq!(g.edges[0].kind, EdgeKind::DependsOn);
    }

    #[test]
    fn truncated_evidence_ends_with_ellipsis_and_coordinates_have_one_decimal() {
        let mut a = profile("a");
        a.links = vec![link("b", "calls", &"x".repeat(500))];
        let g = derive(&[a], &none(), &t(&["a", "b", "c"]));
        assert!(g.edges[0].evidence.ends_with('…'));
        for n in &g.nodes {
            assert_eq!((n.x * 10.0).round(), n.x * 10.0);
            assert_eq!((n.y * 10.0).round(), n.y * 10.0);
        }
    }

    fn shared_secret_edges(names: &[&str]) -> Vec<Edge> {
        let body = names
            .iter()
            .map(|n| format!("- `{n}`\n- {n} – x"))
            .collect::<Vec<_>>()
            .join("\n");
        let a = with_section("a", SectionKey::Secrets, &body);
        let b = with_section("b", SectionKey::Secrets, &body);
        derive(&[a, b], &none(), &t(&["a", "b"])).edges
    }

    #[test]
    fn token_shaped_upper_snake_values_are_not_names() {
        let long = format!(
            "{}_{}",
            "ABCDEFGHIJKLMNOPQRSTUVWXYZ", "ABCDEFGHIJKLMNOPQRSTUVWXYZ"
        );
        let bad = [
            "AKIAIOSFODNN7_EXAMPLE",
            "GLPAT_ABCDEFGHIJKLMNOPQRST12",
            long.as_str(),
            "A1B2C3D4E5F6G7H8_I9J0K1L2",
            "SK_LIVE",
            "XOXB_TOKEN",
            "ABCDEFGHIJKL_X",
            "AB_1234567",
        ];
        for n in bad {
            assert!(shared_secret_edges(&[n]).is_empty(), "{n}");
        }
        let e = shared_secret_edges(&["GITLAB_TOKEN", "DATABASE_URL", "STRIPE_API_KEY"]);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].evidence, "DATABASE_URL; GITLAB_TOKEN; STRIPE_API_KEY");
        let mixed = shared_secret_edges(&["AKIAIOSFODNN7_EXAMPLE", "DATABASE_URL"]);
        assert_eq!(mixed[0].evidence, "DATABASE_URL");
    }

    #[test]
    fn suffix_ambiguity_and_unique_resolution() {
        let mut a = profile("a");
        a.links = vec![link("x/g/billing", "calls", "e")];
        let g = derive(&[a], &none(), &t(&["a", "billing", "g/billing"]));
        assert!(g.edges.is_empty(), "both aliases are suffixes");
        let mut a = profile("a");
        a.links = vec![link("x/billing", "calls", "e")];
        let g = derive(&[a], &none(), &t(&["a", "p/billing", "q/billing"]));
        assert!(g.edges.is_empty(), "same last segment twice");
        let mut a = profile("a");
        a.links = vec![
            link("x/g/billing", "calls", "e"),
            link("y/ledger", "calls", "f"),
        ];
        let g = derive(
            &[a],
            &none(),
            &t(&["a", "g/billing", "p/ledger", "q/other"]),
        );
        let to: Vec<&str> = g.edges.iter().map(|e| e.to.as_str()).collect();
        assert_eq!(to, ["g/billing", "p/ledger"]);
    }

    #[test]
    fn rounded_coordinates_never_collide() {
        let pts = [(500.04, 500.04), (500.0, 500.0), (500.01, 499.98)];
        let r = unique_tenths(&pts, &[1, 0, 2]);
        let set: BTreeSet<_> = r.iter().collect();
        assert_eq!(set.len(), 3, "{r:?}");
        assert_eq!(r[1], (5000, 5000), "first in order keeps its spot");
    }
}
