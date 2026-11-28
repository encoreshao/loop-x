//! Review configuration: built-in defaults, the loop's settings, the repository's
//! `.loopx/review.yaml` (read at the merge request's base commit, so an MR cannot
//! change the rules that review it) and the personal `review` block of a project in
//! `projects.json`, which wins. Every effective value records where it came from.

use super::context::PathInstruction;
use super::glob;
use super::taxonomy::{MAX_FINDINGS, SEVERITIES};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const REPO_FILE: &str = ".loopx/review.yaml";
pub const MAX_FILE_BYTES: usize = 64 * 1024;

/// Files never worth a review comment; a user include pattern brings one back.
pub const DEFAULT_DENY: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "Gemfile.lock",
    "Cargo.lock",
    "poetry.lock",
    "Pipfile.lock",
    "uv.lock",
    "composer.lock",
    "go.sum",
    "**/node_modules/**",
    "**/vendor/**",
    "**/dist/**",
    "**/generated/**",
    "**/__generated__/**",
    "*.min.js",
    "*.min.css",
    "*.map",
    "*.snap",
    "*.pb.go",
    "*_pb2.py",
    "*.png",
    "*.jpg",
    "*.jpeg",
    "*.gif",
    "*.ico",
    "*.webp",
    "*.pdf",
    "*.zip",
    "*.gz",
    "*.woff",
    "*.woff2",
    "*.ttf",
    "*.eot",
    "*.mp4",
    "*.mov",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Quiet,
    Chill,
    Assertive,
}

impl Profile {
    fn min_severity(self) -> &'static str {
        match self {
            Profile::Quiet => "major",
            Profile::Chill => "minor",
            Profile::Assertive => "nit",
        }
    }
    fn finding_cap(self) -> usize {
        match self {
            Profile::Quiet => 8,
            Profile::Chill => MAX_FINDINGS,
            Profile::Assertive => 25,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Profile::Quiet => "quiet",
            Profile::Chill => "chill",
            Profile::Assertive => "assertive",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoReviewLayer {
    pub enabled: Option<bool>,
    pub ignore_title_keywords: Option<Vec<String>>,
    pub labels: Option<Vec<String>>,
    pub ignore_authors: Option<Vec<String>>,
    pub base_branches: Option<Vec<String>>,
}

/// One layer as written by a person: the repo file or the `projects.json` block.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewLayer {
    pub version: Option<u32>,
    pub profile: Option<Profile>,
    pub path_filters: Option<Vec<String>>,
    pub path_instructions: Option<Vec<PathInstruction>>,
    pub auto_review: Option<AutoReviewLayer>,
}

impl ReviewLayer {
    fn validate(self) -> Result<Self, String> {
        if let Some(v) = self.version.filter(|v| *v != 1) {
            return Err(format!("version {v} is not supported (use 1)"));
        }
        for re in self
            .auto_review
            .iter()
            .flat_map(|a| a.base_branches.iter().flatten())
        {
            Regex::new(&format!("^(?:{re})$"))
                .map_err(|_| format!("auto_review.base_branches: invalid regex {re:?}"))?;
        }
        Ok(self)
    }
}

/// The repository file. `Err` explains why it is ignored.
pub fn parse_repo_file(text: &str) -> Result<ReviewLayer, String> {
    if text.len() > MAX_FILE_BYTES {
        return Err(format!("larger than {} KB", MAX_FILE_BYTES / 1024));
    }
    if text.trim().is_empty() {
        return Ok(ReviewLayer::default());
    }
    serde_norway::from_str::<ReviewLayer>(text)
        .map_err(|e| e.to_string())?
        .validate()
}

/// A project's `review` block in `projects.json`. `Err` explains why it is ignored.
pub fn parse_personal(v: &Value) -> Result<ReviewLayer, String> {
    serde_json::from_value::<ReviewLayer>(v.clone())
        .map_err(|e| e.to_string())?
        .validate()
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AutoReview {
    pub enabled: bool,
    pub ignore_title_keywords: Vec<String>,
    pub labels: Vec<String>,
    pub ignore_authors: Vec<String>,
    pub base_branches: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReviewConfig {
    pub profile: &'static str,
    pub min_severity: String,
    pub finding_cap: usize,
    pub path_filters: Vec<String>,
    pub path_instructions: Vec<PathInstruction>,
    pub auto_review: AutoReview,
    /// key -> `default` | `loop settings` | `repo` | `projects.json`.
    pub provenance: BTreeMap<String, String>,
    /// Layers that were ignored, with why (shown in the review).
    pub notes: Vec<String>,
}

pub struct Sources<'a> {
    pub loop_min_severity: Option<&'a str>,
    pub repo: Option<Result<ReviewLayer, String>>,
    pub personal: Option<Result<ReviewLayer, String>>,
}

pub fn resolve(src: Sources<'_>) -> ReviewConfig {
    let mut cfg = ReviewConfig {
        profile: Profile::Chill.name(),
        min_severity: Profile::Chill.min_severity().into(),
        finding_cap: Profile::Chill.finding_cap(),
        path_filters: Vec::new(),
        path_instructions: Vec::new(),
        auto_review: AutoReview {
            enabled: true,
            ..AutoReview::default()
        },
        provenance: BTreeMap::new(),
        notes: Vec::new(),
    };
    for k in [
        "profile",
        "min_severity",
        "finding_cap",
        "path_filters",
        "path_instructions",
        "auto_review",
    ] {
        cfg.provenance.insert(k.into(), "default".into());
    }
    if let Some(s) = src.loop_min_severity.filter(|s| SEVERITIES.contains(s)) {
        cfg.min_severity = s.to_string();
        cfg.provenance
            .insert("min_severity".into(), "loop settings".into());
    }
    let layers = [
        (REPO_FILE, "repo", src.repo),
        ("projects.json review", "projects.json", src.personal),
    ];
    for (label, source, layer) in layers {
        let layer = match layer {
            None => continue,
            Some(Err(e)) => {
                cfg.notes.push(format!("`{label}` ignored: {e}"));
                continue;
            }
            Some(Ok(l)) => l,
        };
        apply(&mut cfg, layer, source);
    }
    cfg
}

fn apply(cfg: &mut ReviewConfig, layer: ReviewLayer, source: &str) {
    let mark = |cfg: &mut ReviewConfig, k: &str| {
        cfg.provenance.insert(k.into(), source.into());
    };
    if let Some(p) = layer.profile {
        cfg.profile = p.name();
        cfg.min_severity = p.min_severity().into();
        cfg.finding_cap = p.finding_cap();
        for k in ["profile", "min_severity", "finding_cap"] {
            mark(cfg, k);
        }
    }
    if let Some(f) = layer.path_filters {
        cfg.path_filters.extend(f);
        mark(cfg, "path_filters");
    }
    if let Some(p) = layer.path_instructions {
        cfg.path_instructions.extend(p);
        mark(cfg, "path_instructions");
    }
    if let Some(a) = layer.auto_review {
        let ar = &mut cfg.auto_review;
        if let Some(v) = a.enabled {
            ar.enabled = v;
        }
        if let Some(v) = a.ignore_title_keywords {
            ar.ignore_title_keywords = v;
        }
        if let Some(v) = a.labels {
            ar.labels = v;
        }
        if let Some(v) = a.ignore_authors {
            ar.ignore_authors = v;
        }
        if let Some(v) = a.base_branches {
            ar.base_branches = v;
        }
        mark(cfg, "auto_review");
    }
}

impl ReviewConfig {
    /// Is `path` reviewed? A user `!` exclude always wins; a user include brings back a
    /// default-denied file; any include turns the list into an allowlist.
    pub fn reviews_path(&self, path: &str) -> bool {
        let (excludes, includes): (Vec<&String>, Vec<&String>) =
            self.path_filters.iter().partition(|p| p.starts_with('!'));
        if excludes.iter().any(|p| glob::matches(&p[1..], path)) {
            return false;
        }
        if includes.iter().any(|p| glob::matches(p, path)) {
            return true;
        }
        includes.is_empty() && !DEFAULT_DENY.iter().any(|p| glob::matches(p, path))
    }

    /// Why an MR is left to "Review now" only, or `None` when auto review takes it.
    pub fn auto_review_skip(
        &self,
        title: &str,
        labels: &[String],
        author: &str,
        target_branch: &str,
    ) -> Option<String> {
        let ar = &self.auto_review;
        if !ar.enabled {
            return Some("auto review is off".into());
        }
        if !author.is_empty() && ar.ignore_authors.iter().any(|a| a == author) {
            return Some(format!("author {author} is ignored"));
        }
        let lower = title.to_lowercase();
        if let Some(k) = ar
            .ignore_title_keywords
            .iter()
            .find(|k| !k.trim().is_empty() && lower.contains(&k.to_lowercase()))
        {
            return Some(format!("title contains {k:?}"));
        }
        let (excluded, included): (Vec<&String>, Vec<&String>) =
            ar.labels.iter().partition(|l| l.starts_with('!'));
        if let Some(l) = excluded
            .iter()
            .find(|l| labels.iter().any(|x| *x == l[1..]))
        {
            return Some(format!("label {} excludes it", &l[1..]));
        }
        if !included.is_empty() && !included.iter().any(|l| labels.contains(l)) {
            return Some("none of the auto review labels is set".into());
        }
        let branch_ok = ar.base_branches.is_empty()
            || ar.base_branches.iter().any(|re| {
                Regex::new(&format!("^(?:{re})$")).is_ok_and(|r| r.is_match(target_branch))
            });
        if !branch_ok {
            return Some(format!("target branch {target_branch} is not listed"));
        }
        None
    }

    /// The effective configuration with each value's source, for `--explain`.
    pub fn explain(&self) -> Value {
        let src = |k: &str| self.provenance.get(k).cloned().unwrap_or_default();
        json!({
            "profile": {"value": self.profile, "from": src("profile")},
            "min_severity": {"value": self.min_severity, "from": src("min_severity")},
            "finding_cap": {"value": self.finding_cap, "from": src("finding_cap")},
            "path_filters": {"value": self.path_filters, "from": src("path_filters")},
            "path_instructions": {"value": self.path_instructions.iter()
                .map(|p| json!({"path": p.path, "instructions": p.instructions}))
                .collect::<Vec<_>>(), "from": src("path_instructions")},
            "auto_review": {"value": {
                "enabled": self.auto_review.enabled,
                "ignore_title_keywords": self.auto_review.ignore_title_keywords,
                "labels": self.auto_review.labels,
                "ignore_authors": self.auto_review.ignore_authors,
                "base_branches": self.auto_review.base_branches,
            }, "from": src("auto_review")},
            "notes": self.notes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(repo: &str, personal: Option<Value>, loop_sev: Option<&str>) -> ReviewConfig {
        resolve(Sources {
            loop_min_severity: loop_sev,
            repo: Some(parse_repo_file(repo)),
            personal: personal.map(|v| parse_personal(&v)),
        })
    }

    #[test]
    fn defaults_match_todays_behaviour() {
        let c = resolve(Sources {
            loop_min_severity: None,
            repo: None,
            personal: None,
        });
        assert_eq!(
            (c.profile, c.min_severity.as_str(), c.finding_cap),
            ("chill", "minor", 15)
        );
        assert!(c.auto_review.enabled);
        assert!(c.notes.is_empty());
        let c = resolve(Sources {
            loop_min_severity: Some("major"),
            repo: None,
            personal: None,
        });
        assert_eq!(c.min_severity, "major");
        assert_eq!(c.provenance["min_severity"], "loop settings");
    }

    #[test]
    fn repo_overrides_loop_settings_and_personal_overrides_repo() {
        let c = cfg(
            "profile: assertive\npath_filters: ['!docs/**']\n",
            None,
            Some("major"),
        );
        assert_eq!((c.min_severity.as_str(), c.finding_cap), ("nit", 25));
        assert_eq!(c.provenance["profile"], "repo");
        let c = cfg(
            "profile: assertive\npath_filters: ['!docs/**']\nauto_review: {ignore_authors: [bot]}\n",
            Some(json!({"profile": "quiet", "path_filters": ["!spec/**"], "auto_review": {"enabled": false}})),
            None,
        );
        assert_eq!(
            (c.profile, c.min_severity.as_str(), c.finding_cap),
            ("quiet", "major", 8)
        );
        assert_eq!(c.provenance["profile"], "projects.json");
        assert_eq!(c.path_filters, ["!docs/**", "!spec/**"]);
        assert!(!c.auto_review.enabled);
        assert_eq!(c.auto_review.ignore_authors, ["bot"]);
    }

    #[test]
    fn a_bad_layer_is_ignored_with_a_note_not_an_error() {
        let c = cfg("profile: loud\n", None, None);
        assert_eq!(c.profile, "chill");
        assert!(
            c.notes[0].starts_with("`.loopx/review.yaml` ignored:"),
            "{:?}",
            c.notes
        );
        assert!(cfg("walkthrough: true\n", None, None).notes[0].contains("unknown field"));
        assert!(cfg("version: 2\n", None, None).notes[0].contains("version 2"));
        assert!(
            cfg("auto_review: {base_branches: ['(']}\n", None, None).notes[0]
                .contains("invalid regex")
        );
        assert!(cfg(&"#".repeat(MAX_FILE_BYTES + 1), None, None).notes[0].contains("larger than"));
        let c = cfg("", Some(json!({"profile": 3})), None);
        assert!(c.notes[0].starts_with("`projects.json review` ignored:"));
        assert!(cfg("", None, None).notes.is_empty());
    }

    #[test]
    fn path_filters_default_deny_allowlists_and_excludes() {
        let c = cfg("", None, None);
        assert!(c.reviews_path("app/models/user.rb"));
        assert!(!c.reviews_path("yarn.lock"));
        assert!(!c.reviews_path("web/dist/app.js"));
        assert!(!c.reviews_path("public/a.min.js"));
        let c = cfg(
            "path_filters: ['yarn.lock', 'src/**', '!src/gen/**']\n",
            None,
            None,
        );
        assert!(
            c.reviews_path("yarn.lock"),
            "an include brings a default back"
        );
        assert!(c.reviews_path("src/a.ts"));
        assert!(!c.reviews_path("src/gen/a.ts"), "exclude wins");
        assert!(
            !c.reviews_path("package.json"),
            "any include makes an allowlist"
        );
        let c = cfg("path_filters: ['!docs/**']\n", None, None);
        assert!(!c.reviews_path("docs/a.md"));
        assert!(c.reviews_path("README.md"));
    }

    #[test]
    fn auto_review_rules() {
        let c = cfg(
            "auto_review:\n  ignore_title_keywords: [WIP]\n  labels: [review-me, '!no-review']\n  ignore_authors: [renovate]\n  base_branches: [main, 'release/.*']\n",
            None,
            None,
        );
        let ok = |t: &str, l: &[&str], a: &str, b: &str| {
            let labels: Vec<String> = l.iter().map(|s| s.to_string()).collect();
            c.auto_review_skip(t, &labels, a, b)
        };
        assert_eq!(ok("Add x", &["review-me"], "ann", "main"), None);
        assert_eq!(ok("Add x", &["review-me"], "ann", "release/1.2"), None);
        assert!(ok("Add x", &["review-me"], "ann", "feature/x")
            .unwrap()
            .contains("target branch"));
        assert!(
            ok("Add x", &["review-me"], "ann", "mainline").is_some(),
            "anchored"
        );
        assert!(ok("wip: add x", &["review-me"], "ann", "main")
            .unwrap()
            .contains("WIP"));
        assert!(ok("Add x", &["review-me", "no-review"], "ann", "main")
            .unwrap()
            .contains("no-review"));
        assert!(ok("Add x", &[], "ann", "main").unwrap().contains("labels"));
        assert!(ok("Add x", &["review-me"], "renovate", "main")
            .unwrap()
            .contains("renovate"));
        let off = cfg("auto_review: {enabled: false}\n", None, None);
        assert_eq!(
            off.auto_review_skip("t", &[], "a", "main").unwrap(),
            "auto review is off"
        );
    }

    #[test]
    fn path_instructions_from_both_layers_add_up() {
        let c = cfg(
            "path_instructions:\n  - {path: 'db/**', instructions: 'Set lock_timeout.'}\n",
            Some(json!({"path_instructions": [{"path": "app/**", "instructions": "Mine."}]})),
            None,
        );
        let paths: Vec<&str> = c
            .path_instructions
            .iter()
            .map(|p| p.path.as_str())
            .collect();
        assert_eq!(paths, ["db/**", "app/**"]);
        assert_eq!(c.provenance["path_instructions"], "projects.json");
        assert!(
            cfg("path_instructions: [{path: a}]\n", None, None).notes[0].contains("instructions")
        );
    }

    #[test]
    fn explain_names_each_source() {
        let c = cfg("profile: quiet\n", None, Some("nit"));
        let e = c.explain();
        assert_eq!(e["profile"]["from"], "repo");
        assert_eq!(e["path_filters"]["from"], "default");
        assert_eq!(e["finding_cap"]["value"], 8);
    }
}
