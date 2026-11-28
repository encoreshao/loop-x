//! Review eval suite (`loop eval --review`): runs the MR Review loop's real prompt
//! and sealed model call against synthetic merge requests in
//! `evals/review/<name>/` (`case.yaml` + `patch.diff`) and grades the cleaned
//! findings: expected defects found (recall), forbidden spots left alone
//! (precision), the finding count and the verdict. Nothing is posted anywhere.
//!
//! Every real case costs money, so the suite has a hard budget, like the golden
//! and chat suites. Parsing and grading are pure; [`run_case_real`] is the paid path.

use crate::golden_eval::{
    checkout_root, evals_dir, run_suite as run_golden_suite, write_summary, CaseCrash, CaseOutcome,
    Summary,
};
use crate::loopkit::{self, WorkItem};
use crate::review::config::{self as review_config, ReviewConfig, Sources};
use crate::review::{context, diff::build_diff_text, findings, taxonomy, verify};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub const DEFAULT_TIMEOUT_SECONDS: f64 = 480.0;
const OUTPUT_KEYS: [&str; 5] = ["summary", "verdict", "overview", "risks", "findings"];

#[derive(Debug, Clone, PartialEq)]
pub struct Spot {
    pub path: String,
    pub start: i64,
    pub end: i64,
    /// Lowest severity that counts as a hit (expected findings only).
    pub min_severity: Option<String>,
    /// Any of these categories counts (expected findings only; empty = any).
    pub categories: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReviewExpect {
    pub findings: Vec<Spot>,
    pub forbidden: Vec<Spot>,
    pub max_findings: Option<usize>,
    pub verdict: Vec<String>,
    /// `tool:rule` hints no finding may confirm.
    pub forbidden_hints: Vec<String>,
    /// Earlier finding ids the model must call addressed / open (incremental cases).
    pub addressed: Vec<String>,
    pub still_open: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReviewCase {
    pub name: String,
    pub title: String,
    pub description: String,
    pub min_severity: String,
    /// GitLab-shaped `changes` rows: `{old_path, new_path, diff, new_file, deleted_file}`.
    pub changes: Vec<Value>,
    /// Files at the base commit (`CLAUDE.md`, `.loopx/review.yaml`, ...).
    pub repo_files: BTreeMap<String, String>,
    /// The reviewer's `custom_instructions` for the project.
    pub custom_instructions: String,
    /// Linter results to hand the model, as the loop's `tool_hints`.
    pub tool_hints: Vec<Value>,
    /// Incremental case: the changes since the last review (`delta.diff`) and the
    /// earlier findings still open.
    pub delta_diff: Option<String>,
    pub previous_findings: Vec<Value>,
    pub expect: ReviewExpect,
}

/// Split a `diff --git` patch into GitLab `/diffs` rows (`diff` starts at the first `@@`).
pub fn parse_patch(text: &str) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for block in text
        .split("\ndiff --git ")
        .map(|b| b.trim_start_matches("diff --git "))
    {
        if block.trim().is_empty() {
            continue;
        }
        let (mut old, mut new) = (None, None);
        let mut hunks: Vec<&str> = Vec::new();
        for line in block.lines() {
            if !hunks.is_empty() {
                hunks.push(line);
            } else if let Some(p) = line.strip_prefix("--- ") {
                old = Some(p.trim_start_matches("a/").to_string());
            } else if let Some(p) = line.strip_prefix("+++ ") {
                new = Some(p.trim_start_matches("b/").to_string());
            } else if line.starts_with("@@") {
                hunks.push(line);
            }
        }
        let (Some(old), Some(new)) = (old, new) else {
            return Err("every file in patch.diff needs --- and +++ lines".into());
        };
        if hunks.is_empty() {
            return Err(format!("{new}: no @@ hunk"));
        }
        let (new_file, deleted) = (old == "/dev/null", new == "/dev/null");
        let new_path = if deleted { old.clone() } else { new };
        let old_path = if new_file { new_path.clone() } else { old };
        out.push(json!({"old_path": old_path, "new_path": new_path,
                        "diff": format!("{}\n", hunks.join("\n")),
                        "new_file": new_file, "deleted_file": deleted}));
    }
    if out.is_empty() {
        return Err("patch.diff has no files".into());
    }
    Ok(out)
}

fn parse_spot(v: &Value, path: &str) -> Result<Spot, String> {
    let file = v
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{path}: every spot needs a path"))?;
    let lines = match v.get("lines") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => return Err(format!("{path}: {file}: lines must be N or \"A-B\"")),
    };
    let (a, b) = lines.split_once('-').unwrap_or((&lines, &lines));
    let num = |s: &str| {
        s.trim()
            .parse::<i64>()
            .map_err(|_| format!("{path}: {file}: bad lines {lines:?}"))
    };
    let (start, end) = (num(a)?, num(b)?);
    if start < 1 || end < start {
        return Err(format!("{path}: {file}: bad lines {lines:?}"));
    }
    let min_severity = v
        .get("min_severity")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(s) = &min_severity {
        if !taxonomy::SEVERITIES.contains(&s.as_str()) {
            return Err(format!("{path}: {file}: unknown min_severity {s:?}"));
        }
    }
    let categories = strings(v.get("category"));
    Ok(Spot {
        path: file.to_string(),
        start,
        end,
        min_severity,
        categories,
    })
}

fn strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

pub fn parse_case(yaml: &str, patch: &str, name: &str) -> Result<ReviewCase, String> {
    let v: Value =
        serde_norway::from_str(yaml).map_err(|e| format!("{name}: invalid YAML: {e}"))?;
    let title = v
        .get("title")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{name}: missing title"))?;
    let expect = v
        .get("expect")
        .ok_or_else(|| format!("{name}: missing expect"))?;
    let spots = |key: &str| -> Result<Vec<Spot>, String> {
        expect
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|s| parse_spot(s, name))
            .collect()
    };
    let verdict = strings(expect.get("verdict"));
    if let Some(bad) = verdict
        .iter()
        .find(|v| !taxonomy::VERDICTS.contains(&v.as_str()))
    {
        return Err(format!("{name}: unknown verdict {bad:?}"));
    }
    let case = ReviewCase {
        name: name.to_string(),
        title: title.to_string(),
        description: v
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        min_severity: v
            .get("min_severity")
            .and_then(Value::as_str)
            .unwrap_or("minor")
            .to_string(),
        changes: parse_patch(patch).map_err(|e| format!("{name}: {e}"))?,
        repo_files: v
            .get("repo_files")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        custom_instructions: v
            .get("custom_instructions")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        tool_hints: v
            .get("tool_hints")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        delta_diff: None,
        previous_findings: v
            .get("previous_findings")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        expect: ReviewExpect {
            findings: spots("findings")?,
            forbidden: spots("forbidden")?,
            max_findings: expect
                .get("max_findings")
                .and_then(Value::as_u64)
                .map(|n| n as usize),
            verdict,
            forbidden_hints: strings(expect.get("forbidden_hints")),
            addressed: strings(expect.get("addressed")),
            still_open: strings(expect.get("still_open")),
        },
    };
    let paths: BTreeSet<String> = case
        .changes
        .iter()
        .filter_map(|c| c["new_path"].as_str().map(str::to_string))
        .collect();
    for s in case.expect.findings.iter().chain(&case.expect.forbidden) {
        if !paths.contains(&s.path) {
            return Err(format!("{name}: {} is not in patch.diff", s.path));
        }
    }
    Ok(case)
}

/// Every `<dir>/*/case.yaml` (with its `patch.diff`), sorted; `names` narrows to those cases.
pub fn load_cases(dir: &Path, names: Option<&[String]>) -> Result<Vec<ReviewCase>, String> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("case.yaml").is_file())
        .collect();
    dirs.sort();
    let mut out = Vec::new();
    for d in dirs {
        let name = d
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if names.is_some_and(|n| !n.contains(&name)) {
            continue;
        }
        let read =
            |f: &str| std::fs::read_to_string(d.join(f)).map_err(|e| format!("{name}/{f}: {e}"));
        let mut case = parse_case(&read("case.yaml")?, &read("patch.diff")?, &name)?;
        if d.join("delta.diff").is_file() {
            let rows =
                parse_patch(&read("delta.diff")?).map_err(|e| format!("{name}/delta.diff: {e}"))?;
            case.delta_diff = Some(build_diff_text(&rows, taxonomy::DIFF_CAP_BYTES).0);
        }
        out.push(case);
    }
    if let Some(wanted) = names {
        if let Some(missing) = wanted.iter().find(|w| !out.iter().any(|c| &c.name == *w)) {
            return Err(format!("no review case named {missing:?}"));
        }
    }
    Ok(out)
}

pub fn default_cases(names: Option<&[String]>) -> Result<Vec<ReviewCase>, String> {
    let root = checkout_root("review").ok_or("no evals/review directory found")?;
    load_cases(&root.join("evals").join("review"), names)
}

/// The work item the MR Review loop would build for this MR (same payload keys).
/// The case's effective review config, from its `.loopx/review.yaml` if any.
pub fn case_config(case: &ReviewCase) -> ReviewConfig {
    review_config::resolve(Sources {
        loop_min_severity: Some(&case.min_severity),
        repo: case
            .repo_files
            .get(review_config::REPO_FILE)
            .map(|t| review_config::parse_repo_file(t)),
        personal: None,
    })
}

pub fn work_item(case: &ReviewCase) -> WorkItem {
    let cfg = case_config(case);
    let reviewed: Vec<Value> = case
        .changes
        .iter()
        .filter(|c| cfg.reviews_path(c["new_path"].as_str().unwrap_or("")))
        .cloned()
        .collect();
    let (diff, truncated) = build_diff_text(&reviewed, taxonomy::DIFF_CAP_BYTES);
    let changed: Vec<&str> = case
        .changes
        .iter()
        .filter_map(|c| c["new_path"].as_str())
        .collect();
    let reviewed_paths: Vec<String> = reviewed
        .iter()
        .filter_map(|c| c["new_path"].as_str().map(str::to_string))
        .collect();
    let guidance = context::gather(
        &reviewed_paths,
        &cfg.path_instructions,
        &case.custom_instructions,
        &mut |p| case.repo_files.get(p).cloned(),
    );
    let url = format!(
        "https://gitlab.example.com/eval/{}/-/merge_requests/1",
        case.name
    );
    let mut payload = json!({
        "account": "eval", "project_id": 1, "iid": 1, "title": case.title,
        "description": case.description, "web_url": url,
        "diff_refs": {"base_sha": "1".repeat(40), "start_sha": "1".repeat(40), "head_sha": "0".repeat(40)},
        "diff": diff, "truncated": truncated,
        "changed_paths": changed, "old_paths": {},
    });
    if !guidance.is_empty() {
        payload["repo_guidance"] = json!(guidance);
    }
    if !case.tool_hints.is_empty() {
        payload["tool_hints"] = json!(case.tool_hints);
    }
    if cfg.walkthrough {
        payload["walkthrough_requested"] = json!(true);
    }
    if let Some(delta) = &case.delta_diff {
        payload["delta_from"] = json!("2".repeat(40));
        payload["delta_diff"] = json!(delta);
        if !case.previous_findings.is_empty() {
            payload["previous_findings"] = json!(case.previous_findings);
        }
    }
    WorkItem::new(
        format!("mr:eval:1!1@{}", "0".repeat(40)),
        case.title.clone(),
        payload,
    )
    .with_url(url)
}

/// The cleaned answer, capped like the loop's (`findings`, `verdict`).
pub fn clean_answer(case: &ReviewCase, answer: &Value) -> (Vec<Value>, String) {
    let allowed: BTreeSet<&str> = case
        .changes
        .iter()
        .filter_map(|c| c["new_path"].as_str())
        .collect();
    let cfg = case_config(case);
    let mut cleaned = findings::clean(answer, &allowed, &cfg.min_severity);
    cleaned.findings.truncate(cfg.finding_cap);
    (cleaned.findings, cleaned.verdict.to_string())
}

fn hits(spot: &Spot, f: &Value, expected: bool) -> bool {
    let line = f["line"].as_i64().unwrap_or(0);
    if f["path"].as_str() != Some(spot.path.as_str()) || line < spot.start || line > spot.end {
        return false;
    }
    if !expected {
        return true;
    }
    let sev = f["severity"].as_str().unwrap_or("");
    let sev_ok = spot
        .min_severity
        .as_deref()
        .is_none_or(|m| taxonomy::sev_rank(sev) >= taxonomy::sev_rank(m));
    let cat = f["category"].as_str().unwrap_or("");
    sev_ok && (spot.categories.is_empty() || spot.categories.iter().any(|c| c == cat))
}

fn label(s: &Spot) -> String {
    if s.start == s.end {
        format!("{}:{}", s.path, s.start)
    } else {
        format!("{}:{}-{}", s.path, s.start, s.end)
    }
}

/// `(passed, reasons)`. The first reason is always the score line.
pub fn grade(expect: &ReviewExpect, found: &[Value], verdict: &str) -> (bool, Vec<String>) {
    let mut fails = Vec::new();
    let matched = expect
        .findings
        .iter()
        .filter(|s| {
            let hit = found.iter().any(|f| hits(s, f, true));
            if !hit {
                fails.push(format!("missed expected finding at {}", label(s)));
            }
            hit
        })
        .count();
    for s in &expect.forbidden {
        for f in found.iter().filter(|f| hits(s, f, false)) {
            fails.push(format!(
                "false positive at {}: {}",
                label(s),
                f["title"].as_str().unwrap_or("")
            ));
        }
    }
    let unexpected = found
        .iter()
        .filter(|f| !expect.findings.iter().any(|s| hits(s, f, true)))
        .count();
    for h in &expect.forbidden_hints {
        if found.iter().any(|f| {
            f["hints"]
                .as_array()
                .is_some_and(|a| a.iter().any(|x| x == h))
        }) {
            fails.push(format!("a finding confirms the noise hint {h}"));
        }
    }
    if let Some(max) = expect.max_findings {
        if found.len() > max {
            fails.push(format!("{} findings, at most {max} expected", found.len()));
        }
    }
    if !expect.verdict.is_empty() && !expect.verdict.iter().any(|v| v == verdict) {
        fails.push(format!(
            "verdict {verdict}, expected {}",
            expect.verdict.join(" or ")
        ));
    }
    let prompted = found
        .iter()
        .filter(|f| f["fix_prompt"].as_str().is_some_and(|p| !p.is_empty()))
        .count();
    let mut reasons = vec![format!(
        "recall {matched}/{}, {unexpected} other finding(s), verdict {verdict}, agent prompt on {prompted}/{}",
        expect.findings.len(),
        found.len()
    )];
    let passed = fails.is_empty();
    reasons.extend(fails);
    (passed, reasons)
}

pub fn run_suite<F>(cases: &[ReviewCase], budget_usd: f64, mut run_case: F) -> Summary
where
    F: FnMut(&ReviewCase, f64) -> Result<CaseOutcome, CaseCrash>,
{
    run_golden_suite(
        cases,
        budget_usd,
        |c| c.name.as_str(),
        |c, r| run_case(c, r),
    )
}

pub use crate::golden_eval::exit_code;

/// `<evals dir>/review-last.json` unless `path` is given.
pub fn write_last_run(
    summary: &Summary,
    budget_usd: f64,
    path: Option<&Path>,
) -> std::io::Result<PathBuf> {
    let p = path.map_or_else(|| evals_dir().join("review-last.json"), Path::to_path_buf);
    write_summary(summary, budget_usd, &p)?;
    Ok(p)
}

/// `(prompt, remaining budget)` → `(answer text, cost)`.
pub type ModelFn<'a> = &'a dyn Fn(&str, f64) -> Result<(String, Option<f64>), CaseCrash>;

/// One case through the loop's prompt template, `model` and the loop's cleaning.
/// One case through the loop's prompt template, `model`, the loop's cleaning and, when
/// given, the loop's verification pass on `verify`.
pub fn run_case(
    case: &ReviewCase,
    template: &str,
    remaining_usd: f64,
    model: ModelFn<'_>,
    verify: Option<ModelFn<'_>>,
) -> Result<CaseOutcome, CaseCrash> {
    let item = work_item(case);
    let prompt = loopkit::render_prompt(template, &item, &json!({}));
    let (text, cost_usd) = model(&prompt, remaining_usd)?;
    let answer = loopkit::parse_answer(Some(&text), &OUTPUT_KEYS).map_err(|e| CaseCrash {
        message: format!("unusable answer: {e}"),
        cost_usd,
    })?;
    let (mut found, verdict) = clean_answer(case, &answer);
    let mut cost = cost_usd;
    let mut note = None;
    if let (Some(verify), false) = (verify, found.is_empty()) {
        let diff = item.payload["diff"].as_str().unwrap_or("");
        let left = (remaining_usd - cost.unwrap_or(0.0)).max(0.0);
        let budget = verify::BUDGET_USD.min(left);
        if let Ok((reply, vcost)) = verify(&verify::prompt(&found, diff), budget) {
            let v = verify::apply(found, &reply, &case_config(case).min_severity);
            note = Some(format!(
                "verification dropped {}, downgraded {}{}",
                v.dropped,
                v.downgraded,
                if v.error.is_some() {
                    " (reply unusable)"
                } else {
                    ""
                }
            ));
            found = v.findings;
            cost = Some(cost.unwrap_or(0.0) + vcost.unwrap_or(0.0));
        }
    }
    let (mut passed, mut reasons) = grade(&case.expect, &found, &verdict);
    if case_config(case).walkthrough {
        let paths: Vec<&str> = case
            .changes
            .iter()
            .filter_map(|c| c["new_path"].as_str())
            .collect();
        match crate::review::walkthrough::clean(&answer, &paths) {
            None => {
                passed = false;
                reasons.push("no usable walkthrough".into());
            }
            Some(w) => reasons.insert(
                1,
                format!(
                    "walkthrough: {} group(s), effort {}, risk {}, diagram {}",
                    w["groups"].as_array().map_or(0, Vec::len),
                    w["effort"],
                    w["merge_risk"],
                    if w["sequence"].is_string() {
                        "yes"
                    } else {
                        "no"
                    }
                ),
            ),
        }
    }
    for (ids, want) in [
        (&case.expect.addressed, "addressed"),
        (&case.expect.still_open, "open"),
    ] {
        for id in ids {
            let got = answer["previous"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|p| p["id"] == id.as_str())
                .and_then(|p| p["status"].as_str())
                .unwrap_or("missing");
            if got != want {
                passed = false;
                reasons.push(format!("earlier finding {id}: {got}, expected {want}"));
            }
        }
    }
    if let Some(n) = note {
        reasons.insert(1, n);
    }
    Ok(CaseOutcome {
        passed,
        reasons,
        cost_usd: cost,
    })
}

fn real_model(
    prompt: &str,
    remaining_usd: f64,
    model: Option<&str>,
) -> Result<(String, Option<f64>), CaseCrash> {
    let hooks = crate::agents::sealed::SealedHooks {
        model,
        ..Default::default()
    };
    let mut sink = |_: &str| {};
    crate::agents::sealed::sealed_call(
        prompt,
        DEFAULT_TIMEOUT_SECONDS,
        &mut sink,
        Some(remaining_usd),
        &hooks,
    )
    .map(|r| (r.text, r.cost_usd))
    .map_err(|e| CaseCrash {
        message: format!("SealedCallFailed: {}", e.0),
        cost_usd: None,
    })
}

/// The paid path: the real `loops/mr-review` prompt and model.
/// `no_verify` skips the verification pass (to measure it).
pub fn run_suite_real(cases: &[ReviewCase], budget_usd: f64, no_verify: bool) -> Summary {
    let root = checkout_root("review").unwrap_or_else(|| PathBuf::from("."));
    let dir = root.join("loops").join("mr-review");
    let template = std::fs::read_to_string(dir.join("prompt.md")).unwrap_or_default();
    let model = crate::loop_definition::LoopDefinition::from_yaml(&dir.join("loop.yaml"))
        .ok()
        .and_then(|d| d.agent.model);
    run_suite(cases, budget_usd, |case, remaining| {
        if template.is_empty() {
            return Err(CaseCrash {
                message: "loops/mr-review/prompt.md not found".into(),
                cost_usd: Some(0.0),
            });
        }
        let call = |p: &str, r: f64| real_model(p, r, model.as_deref());
        let check = |p: &str, r: f64| real_model(p, r, Some(verify::DEFAULT_MODEL));
        let verify: Option<ModelFn<'_>> = (!no_verify).then_some(&check);
        run_case(case, &template, remaining, &call, verify)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "diff --git a/app/a.rb b/app/a.rb\n--- a/app/a.rb\n+++ b/app/a.rb\n@@ -1,2 +1,3 @@\n x\n+y\n z\ndiff --git a/new.rb b/new.rb\n--- /dev/null\n+++ b/new.rb\n@@ -0,0 +1 @@\n+n\n";

    fn case_yaml(expect: &str) -> String {
        format!("title: Add y\ndescription: d\nexpect:\n{expect}")
    }

    fn finding(path: &str, line: i64, sev: &str, cat: &str) -> Value {
        json!({"path": path, "line": line, "severity": sev, "category": cat, "title": "t", "body": "b"})
    }

    #[test]
    fn patch_splits_into_gitlab_rows() {
        let rows = parse_patch(PATCH).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["new_path"], "app/a.rb");
        assert_eq!(rows[0]["diff"], "@@ -1,2 +1,3 @@\n x\n+y\n z\n");
        assert_eq!(rows[1]["new_file"], true);
        assert_eq!(rows[1]["old_path"], "new.rb");
        assert!(parse_patch("").is_err());
        assert!(parse_patch("diff --git a/x b/x\n--- a/x\n+++ b/x\n").is_err());
    }

    #[test]
    fn case_parses_spots_and_rejects_paths_outside_the_patch() {
        let c = parse_case(
            &case_yaml("  findings:\n    - {path: app/a.rb, lines: \"1-3\", min_severity: major, category: [security, data]}\n  forbidden:\n    - {path: new.rb, lines: 1}\n  max_findings: 2\n  verdict: [request_changes]\n"),
            PATCH,
            "c",
        )
        .unwrap();
        assert_eq!(c.expect.findings[0].start, 1);
        assert_eq!(c.expect.findings[0].end, 3);
        assert_eq!(c.expect.findings[0].categories, ["security", "data"]);
        assert_eq!(c.expect.forbidden[0].end, 1);
        assert_eq!(c.min_severity, "minor");
        let bad = parse_case(
            &case_yaml("  findings:\n    - {path: other.rb, lines: 1}\n"),
            PATCH,
            "c",
        );
        assert!(bad.unwrap_err().contains("not in patch.diff"));
        assert!(parse_case(&case_yaml("  verdict: maybe\n"), PATCH, "c").is_err());
        assert!(parse_case(
            &case_yaml("  findings:\n    - {path: app/a.rb, lines: \"3-1\"}\n"),
            PATCH,
            "c"
        )
        .is_err());
    }

    #[test]
    fn grade_scores_recall_precision_count_and_verdict() {
        let expect = ReviewExpect {
            findings: vec![Spot {
                path: "app/a.rb".into(),
                start: 2,
                end: 3,
                min_severity: Some("major".into()),
                categories: vec!["security".into()],
            }],
            forbidden: vec![Spot {
                path: "new.rb".into(),
                start: 1,
                end: 1,
                min_severity: None,
                categories: vec![],
            }],
            max_findings: Some(1),
            verdict: vec!["request_changes".into()],
            forbidden_hints: vec![],
            addressed: vec![],
            still_open: vec![],
        };
        let (ok, reasons) = grade(
            &expect,
            &[finding("app/a.rb", 2, "blocker", "security")],
            "request_changes",
        );
        let noisy = ReviewExpect {
            forbidden_hints: vec!["ruff:S101".into()],
            ..expect.clone()
        };
        let mut hinted = finding("app/a.rb", 2, "blocker", "security");
        hinted["hints"] = json!(["ruff:S101"]);
        let (bad, why) = grade(&noisy, &[hinted], "request_changes");
        assert!(
            !bad && why
                .iter()
                .any(|r| r == "a finding confirms the noise hint ruff:S101")
        );
        assert!(ok, "{reasons:?}");
        assert_eq!(
            reasons[0],
            "recall 1/1, 0 other finding(s), verdict request_changes, agent prompt on 0/1"
        );

        let (ok, reasons) = grade(
            &expect,
            &[finding("app/a.rb", 2, "minor", "security")],
            "comment",
        );
        assert!(!ok);
        assert!(reasons
            .iter()
            .any(|r| r.starts_with("missed expected finding at app/a.rb:2-3")));
        assert!(reasons.iter().any(|r| r.starts_with("verdict comment")));

        let (ok, reasons) = grade(
            &expect,
            &[
                finding("app/a.rb", 2, "major", "security"),
                finding("new.rb", 1, "nit", "tests"),
            ],
            "request_changes",
        );
        assert!(!ok);
        assert!(reasons
            .iter()
            .any(|r| r.starts_with("false positive at new.rb:1")));
        assert!(reasons
            .iter()
            .any(|r| r == "2 findings, at most 1 expected"));
    }

    #[test]
    fn run_case_uses_the_loop_prompt_and_cleaning() {
        let c = parse_case(
            &case_yaml("  findings:\n    - {path: app/a.rb, lines: 2}\n"),
            PATCH,
            "c",
        )
        .unwrap();
        let seen = std::cell::RefCell::new(String::new());
        let model = |p: &str, _r: f64| {
            *seen.borrow_mut() = p.to_string();
            Ok((
                json!({"summary": "s", "verdict": "comment", "overview": [], "risks": [],
                       "findings": [finding("app/a.rb", 2, "minor", "correctness"),
                                    finding("not/in/diff.rb", 1, "major", "security")]})
                .to_string(),
                Some(0.1),
            ))
        };
        let out = run_case(&c, "Data:\n{{item_json}}", 1.0, &model, None).unwrap();
        assert!(out.passed, "{:?}", out.reasons);
        assert_eq!(out.cost_usd, Some(0.1));
        assert!(seen.borrow().contains("+++ b/app/a.rb"));
        assert!(seen.borrow().contains("\"changed_paths\""));

        let bad = |_: &str, _: f64| Ok(("not json".to_string(), Some(0.05)));
        let crash = run_case(&c, "{{item_json}}", 1.0, &bad, None).unwrap_err();
        assert!(crash.message.starts_with("unusable answer"));
        assert_eq!(crash.cost_usd, Some(0.05));
    }

    #[test]
    fn repo_files_drive_config_filters_and_guidance() {
        let yaml = format!(
            "{}repo_files:\n  CLAUDE.md: Use cents.\n  .loopx/review.yaml: \"profile: quiet\\npath_filters: ['!new.rb']\\n\"\ncustom_instructions: Mine.\n",
            case_yaml("  max_findings: 3\n")
        );
        let c = parse_case(&yaml, PATCH, "c").unwrap();
        let item = work_item(&c);
        assert!(!item.payload["diff"].as_str().unwrap().contains("new.rb"));
        let sources: Vec<&str> = item.payload["repo_guidance"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["source"].as_str().unwrap())
            .collect();
        assert_eq!(sources, ["CLAUDE.md", "reviewer's project instructions"]);
        let (found, _) = clean_answer(
            &c,
            &json!({"findings": [finding("app/a.rb", 2, "minor", "tests")]}),
        );
        assert!(found.is_empty(), "quiet profile drops minor findings");
    }

    #[test]
    fn run_case_applies_the_verification_pass() {
        let c = parse_case(&case_yaml("  max_findings: 1\n"), PATCH, "c").unwrap();
        let model = |_: &str, _: f64| {
            Ok((
                json!({"summary": "s", "verdict": "comment", "overview": [], "risks": [],
                       "findings": [finding("app/a.rb", 2, "major", "correctness"),
                                    finding("new.rb", 1, "major", "tests")]})
                .to_string(),
                Some(0.1),
            ))
        };
        let drop_tests = |p: &str, budget: f64| {
            assert!(budget <= verify::BUDGET_USD);
            let id = findings::finding_id("new.rb", "tests", "t");
            assert!(p.contains(&id));
            Ok((
                json!({"decisions": [{"id": id, "decision": "drop"}]}).to_string(),
                Some(0.02),
            ))
        };
        let out = run_case(&c, "{{item_json}}", 1.0, &model, Some(&drop_tests)).unwrap();
        assert!(out.passed, "{:?}", out.reasons);
        assert_eq!(out.reasons[1], "verification dropped 1, downgraded 0");
        assert!((out.cost_usd.unwrap() - 0.12).abs() < 1e-9);
        let without = run_case(&c, "{{item_json}}", 1.0, &model, None).unwrap();
        assert!(!without.passed);
    }

    #[test]
    fn shipped_cases_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/review");
        let cases = load_cases(&dir, None).unwrap();
        assert!(cases.len() >= 6, "{} cases", cases.len());
    }
}
