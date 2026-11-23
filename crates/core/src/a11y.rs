//! The accessibility half of the UI check: the vendored axe-core (MPL-2.0,
//! unmodified, see `assets/axe/`), the expression that runs it in a page, and
//! the comparison that keeps only violations new against the target branch.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const AXE_VERSION: &str = "4.14.0";
pub const AXE_SHA256: &str = "20c09fe157a8a34a30e241aaa1fcdade657734f08ab379ecfbeb7d45cc46e878";
pub const AXE_JS: &str = include_str!("../assets/axe/axe.min.js");
pub const AXE_LICENSE: &str = include_str!("../assets/axe/LICENSE-axe-core.txt");
pub const AXE_THIRD_PARTY_LICENSE: &str =
    include_str!("../assets/axe/LICENSE-3RD-PARTY-axe-core.txt");
pub const AXE_SOURCE: &str = include_str!("../assets/axe/SOURCE.txt");
pub const WCAG_TAGS: [&str; 5] = ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"];

const MAX_RULES: usize = 50;
const MAX_TARGETS: usize = 10;
const MAX_HELP_CHARS: usize = 120;
const MAX_TARGET_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Impact {
    Minor,
    Moderate,
    Serious,
    Critical,
}

impl Impact {
    pub fn from_axe(s: Option<&str>) -> Impact {
        match s.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("minor") => Impact::Minor,
            Some("serious") => Impact::Serious,
            Some("critical") => Impact::Critical,
            _ => Impact::Moderate,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Impact::Minor => "minor",
            Impact::Moderate => "moderate",
            Impact::Serious => "serious",
            Impact::Critical => "critical",
        }
    }
}

/// One axe rule that failed on a page. `nodes` counts every failing element,
/// `targets` keeps at most [`MAX_TARGETS`] of their selectors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Violation {
    pub id: String,
    pub impact: Impact,
    pub help: String,
    pub targets: Vec<String>,
    pub nodes: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewViolation {
    pub id: String,
    pub impact: Impact,
    pub help: String,
    pub targets: Vec<String>,
}

/// An async expression for `Runtime.evaluate` (`awaitPromise`) that loads
/// axe once per document and resolves to the violations, mapped in the page so
/// no element HTML or failure text ever leaves it.
pub fn run_expression() -> String {
    let options = json!({
        "runOnly": {"type": "tag", "values": WCAG_TAGS},
        "resultTypes": ["violations"],
    });
    format!(
        "(async () => {{\n\
         if (!window.axe) {{ (function () {{\n{AXE_JS}\n}}).call(window); }}\n\
         const r = await window.axe.run(document, {options});\n\
         return r.violations.slice(0, {MAX_RULES}).map((v) => ({{\n\
           id: v.id, impact: v.impact, help: v.help, nodes: v.nodes.length,\n\
           targets: v.nodes.slice(0, {MAX_TARGETS}).map((n) => [].concat(n.target).flat(Infinity).join(' ')),\n\
         }}));\n\
         }})()"
    )
}

fn text(v: &Value, n: usize) -> String {
    crate::pre_review::clip(v.as_str().unwrap_or_default(), n)
}

/// The violations in `v`: the in-page mapping of [`run_expression`], or axe's
/// own `violations` (a bare array or `{violations: [...]}`). Element HTML and
/// failure summaries are never read.
pub fn parse(v: &Value) -> Result<Vec<Violation>, String> {
    let list = match v {
        Value::Array(a) => a,
        Value::Object(o) => o
            .get("violations")
            .and_then(Value::as_array)
            .ok_or("axe returned an object without violations")?,
        _ => return Err("axe returned no list of violations".into()),
    };
    let mut out = Vec::new();
    for item in list.iter().take(MAX_RULES) {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or("an axe violation has no rule id")?;
        let raw_targets: Vec<String> = match (item.get("targets"), item.get("nodes")) {
            (Some(Value::Array(t)), _) => t.iter().map(target_text).collect(),
            (_, Some(Value::Array(nodes))) => nodes
                .iter()
                .map(|n| target_text(n.get("target").unwrap_or(&Value::Null)))
                .collect(),
            _ => Vec::new(),
        };
        let nodes = match item.get("nodes") {
            Some(Value::Array(a)) => a.len(),
            Some(n) => n.as_u64().map_or(raw_targets.len(), |n| n as usize),
            None => raw_targets.len(),
        }
        .max(raw_targets.len().min(MAX_TARGETS));
        out.push(Violation {
            id: crate::pre_review::clip(id, MAX_HELP_CHARS),
            impact: Impact::from_axe(item.get("impact").and_then(Value::as_str)),
            help: text(item.get("help").unwrap_or(&Value::Null), MAX_HELP_CHARS),
            targets: raw_targets
                .into_iter()
                .filter(|t| !t.is_empty())
                .take(MAX_TARGETS)
                .collect(),
            nodes,
        });
    }
    Ok(out)
}

fn target_text(t: &Value) -> String {
    let joined = match t {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|p| match p {
                Value::String(s) => s.clone(),
                Value::Array(_) => target_text(p),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    crate::pre_review::clip(&joined, MAX_TARGET_CHARS)
}

/// Violations on the branch that the target branch did not have. A rule
/// absent from `base` is new on every element; otherwise only when the rule's
/// element count went up, and then only on selectors the base lacks, so
/// selectors that merely churned are never counted.
pub fn new_violations(head: &[Violation], base: &[Violation]) -> Vec<NewViolation> {
    let mut out = Vec::new();
    for h in head {
        let targets: Vec<String> = match base.iter().find(|b| b.id == h.id) {
            None => h.targets.clone(),
            Some(b) if h.nodes > b.nodes => h
                .targets
                .iter()
                .filter(|t| !b.targets.contains(t))
                .cloned()
                .collect(),
            Some(_) => continue,
        };
        if targets.is_empty() && base.iter().any(|b| b.id == h.id) {
            continue;
        }
        out.push(NewViolation {
            id: h.id.clone(),
            impact: h.impact,
            help: h.help.clone(),
            targets,
        });
    }
    out
}

pub fn blocks(v: &NewViolation) -> bool {
    v.impact >= Impact::Serious
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(id: &str, impact: Impact, targets: &[&str]) -> Violation {
        Violation {
            id: id.into(),
            impact,
            help: format!("{id} help"),
            targets: targets.iter().map(|s| s.to_string()).collect(),
            nodes: targets.len(),
        }
    }

    #[test]
    fn axe_is_vendored_pinned_and_unmodified() {
        let digest = crate::sha256::hex(&crate::sha256::sha256(AXE_JS.as_bytes()));
        assert_eq!(digest, AXE_SHA256);
        assert!(
            AXE_JS.starts_with(&format!("/*! axe v{AXE_VERSION}\n")),
            "{}",
            &AXE_JS[..80]
        );
        assert!(AXE_JS[..400].contains("Mozilla Public"));
        let source = include_str!("../assets/axe/SOURCE.txt");
        assert!(source.contains(&format!("axe-core {AXE_VERSION}, unmodified")));
        assert!(source.contains(&format!("tree/v{AXE_VERSION}")));
        assert!(
            include_str!("../assets/axe/LICENSE-axe-core.txt").contains("Mozilla Public License")
        );
        assert!(AXE_JS.contains("wcag22aa"));
    }

    #[test]
    fn run_expression_uses_the_wcag22_aa_tags_and_never_returns_html() {
        let e = run_expression();
        assert!(
            e.contains(r#""values":["wcag2a","wcag2aa","wcag21a","wcag21aa","wcag22aa"]"#),
            "{}",
            &e[e.len() - 600..]
        );
        assert!(e.contains(r#""resultTypes":["violations"]"#));
        assert!(e.contains("if (!window.axe)"));
        let tail = &e[e.find("const r = await").unwrap()..];
        assert!(
            !tail.contains("html") && !tail.contains("failureSummary"),
            "{tail}"
        );
        assert!(e.starts_with("(async () =>") && e.ends_with("})()"));
    }

    #[test]
    fn parses_axe_output_and_normalises_impact() {
        let raw = json!({"violations": [
            {"id": "color-contrast", "impact": "serious", "help": "Elements must meet minimum color contrast ratio thresholds",
             "tags": ["wcag2aa"], "nodes": [
                {"target": ["#a"], "html": "<p id=a>secret@example.com</p>", "failureSummary": "Fix: summary-marker"},
                {"target": [["iframe", "#b"]], "html": "<p>x</p>"}]},
            {"id": "image-alt", "impact": null, "help": "Images must\nhave alternate text", "nodes": [{"target": [".logo"]}]},
            {"id": "label", "impact": "CRITICAL", "help": "Form elements must have labels", "nodes": []},
            {"id": "weird", "impact": "catastrophic", "help": "", "nodes": [{"target": ["x"]}]}
        ]});
        let got = parse(&raw).unwrap();
        assert_eq!(got.len(), 4);
        assert_eq!(got[0].impact, Impact::Serious);
        assert_eq!(got[0].targets, vec!["#a", "iframe #b"]);
        assert_eq!(got[0].nodes, 2);
        assert_eq!(got[1].impact, Impact::Moderate);
        assert_eq!(got[1].help, "Images must have alternate text");
        assert_eq!(got[2].impact, Impact::Critical);
        assert_eq!(got[3].impact, Impact::Moderate);
        let text = serde_json::to_string(&got).unwrap();
        assert!(
            !text.contains("secret@example.com") && !text.contains("summary-marker"),
            "{text}"
        );
        let mapped = json!([{"id": "region", "impact": "moderate", "help": "h", "nodes": 14, "targets": ["main", "footer"]}]);
        let got = parse(&mapped).unwrap();
        assert_eq!((got[0].nodes, got[0].targets.len()), (14, 2));
    }

    #[test]
    fn garbage_or_huge_output_is_capped_or_an_error() {
        for bad in [
            json!(null),
            json!("x"),
            json!(3),
            json!({"x": 1}),
            json!([{"impact": "serious"}]),
            json!([{"id": ""}]),
        ] {
            assert!(parse(&bad).is_err(), "{bad}");
        }
        let many: Vec<Value> = (0..80)
            .map(|i| json!({"id": format!("r{i}"), "impact": "minor", "help": "h".repeat(5000),
                            "targets": (0..40).map(|j| format!("#{}{j}\n", "x".repeat(400))).collect::<Vec<_>>()}))
            .collect();
        let got = parse(&Value::Array(many)).unwrap();
        assert_eq!(got.len(), MAX_RULES);
        for g in &got {
            assert!(g.help.chars().count() <= MAX_HELP_CHARS);
            assert_eq!(g.targets.len(), MAX_TARGETS);
            assert_eq!(g.nodes, 40);
            for t in &g.targets {
                assert!(
                    t.chars().count() <= MAX_TARGET_CHARS && !t.contains('\n'),
                    "{t}"
                );
            }
        }
    }

    #[test]
    fn existing_violations_are_not_new() {
        let base = [v("color-contrast", Impact::Serious, &["#a", "#b"])];
        let head = [v("color-contrast", Impact::Serious, &["#a", "#b"])];
        assert!(new_violations(&head, &base).is_empty());
        let fewer = [v("color-contrast", Impact::Serious, &["#a"])];
        assert!(new_violations(&fewer, &base).is_empty());
        let more = [v("color-contrast", Impact::Serious, &["#a", "#b", "#c"])];
        let got = new_violations(&more, &base);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].targets, vec!["#c"]);
        assert!(blocks(&got[0]));
    }

    #[test]
    fn moved_selectors_with_equal_counts_are_not_new() {
        let base = [v(
            "label",
            Impact::Critical,
            &["form > input:nth-child(2)", "#q"],
        )];
        let head = [v(
            "label",
            Impact::Critical,
            &["form > input:nth-child(3)", "#q"],
        )];
        assert!(new_violations(&head, &base).is_empty());
        // More elements, but every kept selector is already on the base (axe
        // only kept the first ten): nothing nameable, so nothing new.
        let mut base10 = v("region", Impact::Serious, &["#1", "#2"]);
        base10.nodes = 12;
        let mut head10 = base10.clone();
        head10.nodes = 13;
        assert!(new_violations(&[head10], &[base10]).is_empty());
    }

    #[test]
    fn a_new_page_makes_every_violation_new() {
        let head = [
            v("color-contrast", Impact::Serious, &["#a"]),
            v("region", Impact::Moderate, &["main"]),
        ];
        let got = new_violations(&head, &[]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].targets, vec!["main"]);
        let unrelated = [v("image-alt", Impact::Critical, &["img"])];
        assert_eq!(new_violations(&head, &unrelated).len(), 2);
    }

    #[test]
    fn minor_and_moderate_new_violations_never_block() {
        for (impact, blocking) in [
            (Impact::Minor, false),
            (Impact::Moderate, false),
            (Impact::Serious, true),
            (Impact::Critical, true),
        ] {
            let got = new_violations(&[v("r", impact, &["#x"])], &[]);
            assert_eq!(blocks(&got[0]), blocking, "{impact:?}");
        }
        assert!(Impact::Minor < Impact::Moderate && Impact::Serious < Impact::Critical);
    }
}
