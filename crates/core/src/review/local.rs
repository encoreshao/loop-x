//! `loopx review`: the MR review run on a local working tree, for you or for a coding
//! agent before it opens a merge request. Same prompt, config, guidance, checks,
//! linters and verification as the MR Review loop; nothing is posted anywhere. The
//! checkout is yours, so config and guidance come from the working tree.

use super::config::{self as review_config, ReviewConfig, Sources};
use super::{checks, context, diff, findings, taxonomy, tools, verify};
use crate::loopkit::{self, WorkItem};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

const OUTPUT_KEYS: [&str; 5] = ["summary", "verdict", "overview", "risks", "findings"];
const MAX_DESCRIPTION: usize = 4096;

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub dir: PathBuf,
    /// Compare with this branch (default: origin's default branch, else `main`).
    pub base: Option<String>,
    /// Only what is not committed yet (`git diff HEAD`).
    pub uncommitted: bool,
    pub no_tools: bool,
    pub no_verify: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Review {
    pub base: String,
    pub summary: String,
    pub verdict: String,
    pub findings: Vec<Value>,
    pub check_results: Vec<Value>,
    pub tool_lines: Vec<String>,
    pub notes: Vec<String>,
    pub cost_usd: f64,
}

/// `(prompt, budget)` -> `(reply, cost)`.
pub type ModelFn<'a> = &'a dyn Fn(&str, f64) -> Result<(String, Option<f64>), String>;

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// The base branch to compare with when none is given.
pub fn default_base(dir: &Path) -> String {
    git(
        dir,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .map(|s| s.trim().to_string())
    .ok()
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| "main".into())
}

/// `(changes, title, description, base label)` for the requested scope.
pub fn collect(opts: &Options) -> Result<(Vec<Value>, String, String, String), String> {
    let dir = &opts.dir;
    if opts.uncommitted {
        let text = git(dir, &["diff", "--no-color", "--no-ext-diff", "HEAD"])?;
        let changes = diff::parse_patch(&text, true)?;
        return Ok((
            changes,
            "Uncommitted changes".into(),
            String::new(),
            "HEAD".into(),
        ));
    }
    let base = opts.base.clone().unwrap_or_else(|| default_base(dir));
    let text = git(
        dir,
        &["diff", "--no-color", "--no-ext-diff", "--merge-base", &base],
    )?;
    let changes = diff::parse_patch(&text, true)?;
    let log = git(dir, &["log", "--format=%s%n%b", &format!("{base}..HEAD")]).unwrap_or_default();
    let title = log
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("Local changes")
        .to_string();
    Ok((
        changes,
        title,
        log.chars().take(MAX_DESCRIPTION).collect(),
        base,
    ))
}

fn read_tree(dir: &Path, rel: &str, max: usize) -> Option<String> {
    let path = dir.join(rel);
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() as usize > max {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// The checks in `<dir>/.loopx/checks/*.md`, with a note per ignored file.
pub fn local_checks(dir: &Path) -> (Vec<checks::Check>, Vec<String>) {
    let mut names: Vec<String> = std::fs::read_dir(dir.join(checks::DIR))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n.ends_with(".md"))
        .collect();
    names.sort();
    let (mut out, mut notes) = (Vec::new(), Vec::new());
    for n in names.into_iter().take(checks::MAX_FILES) {
        let rel = format!("{}/{n}", checks::DIR);
        match read_tree(dir, &rel, checks::MAX_FILE_BYTES).map(|t| checks::parse(&n, &t)) {
            Some(Ok(c)) => out.push(c),
            Some(Err(e)) => notes.push(format!("`{rel}` ignored: {e}")),
            None => notes.push(format!("`{rel}` ignored: unreadable or too large")),
        }
    }
    (out, notes)
}

/// The work item the MR Review prompt expects, plus what cleaning needs.
pub fn build(
    opts: &Options,
    changes: &[Value],
    title: &str,
    description: &str,
) -> (WorkItem, ReviewConfig, Vec<checks::Check>, Vec<String>) {
    let dir = &opts.dir;
    let cfg = review_config::resolve(Sources {
        loop_min_severity: None,
        repo: read_tree(dir, review_config::REPO_FILE, review_config::MAX_FILE_BYTES)
            .map(|t| review_config::parse_repo_file(&t)),
        personal: None,
    });
    let path_of = |c: &Value| c["new_path"].as_str().unwrap_or("").to_string();
    let reviewed: Vec<Value> = changes
        .iter()
        .filter(|c| cfg.reviews_path(&path_of(c)))
        .cloned()
        .collect();
    let reviewed_paths: Vec<String> = reviewed.iter().map(path_of).collect();
    let (text, truncated) = diff::build_diff_text(&reviewed, taxonomy::DIFF_CAP_BYTES);
    let guidance = context::gather(&reviewed_paths, &cfg.path_instructions, "", &mut |p| {
        read_tree(dir, p, review_config::MAX_FILE_BYTES)
    });
    let (all_checks, mut notes) = local_checks(dir);
    let applicable: Vec<checks::Check> = all_checks
        .into_iter()
        .filter(|c| c.applies(&reviewed_paths))
        .collect();
    notes.extend(cfg.notes.iter().cloned());
    let filtered = changes.len() - reviewed.len();
    if filtered > 0 {
        notes.push(format!("{filtered} file(s) not reviewed (path filters)"));
    }
    let changed: Vec<String> = changes.iter().map(path_of).collect();
    let mut payload = json!({
        "account": "local", "project_id": dir.display().to_string(), "iid": 0, "title": title,
        "description": description, "web_url": "",
        "diff_refs": {}, "diff": text, "truncated": truncated,
        "changed_paths": changed, "old_paths": {},
    });
    if !guidance.is_empty() {
        payload["repo_guidance"] = json!(guidance);
    }
    if cfg.walkthrough {
        payload["walkthrough_requested"] = json!(true);
    }
    if !applicable.is_empty() {
        payload["pre_merge_checks"] = json!(applicable
            .iter()
            .map(|c| c.prompt_json())
            .collect::<Vec<_>>());
    }
    (
        WorkItem::new("local", title, payload),
        cfg,
        applicable,
        notes,
    )
}

/// Linter hints on the working tree's changed files (the checkout is yours).
pub fn tool_report(dir: &Path, item: &WorkItem, cfg: &ReviewConfig) -> tools::ToolReport {
    let added = tools::added_lines(item.payload["diff"].as_str().unwrap_or(""));
    let files: Vec<String> = added
        .keys()
        .filter(|p| dir.join(p).is_file())
        .cloned()
        .collect();
    let runner = tools::SystemLintRunner;
    let cache = super::semgrep_rules::cache_dir();
    let rules = if cfg.tool_enabled("semgrep") && tools::LintRunner::installed(&runner, "semgrep") {
        super::semgrep_rules::refresh_stale(
            &cache,
            super::semgrep_rules::DEFAULT_PACKS,
            &super::semgrep_rules::http_fetch,
        );
        super::semgrep_rules::cached(&cache, super::semgrep_rules::DEFAULT_PACKS)
    } else {
        Vec::new()
    };
    let mut budget = tools::RUN_BUDGET;
    tools::run_all(
        &runner,
        dir,
        &files,
        &added,
        &|t| cfg.tool_enabled(t.name()),
        &rules,
        &mut budget,
    )
}

/// Review the working tree with `model` (and `verify` for the verification pass).
pub fn run(
    opts: &Options,
    template: &str,
    budget_usd: f64,
    model: ModelFn<'_>,
    verify_model: Option<ModelFn<'_>>,
    report: Option<tools::ToolReport>,
) -> Result<Review, String> {
    let (changes, title, description, base) = collect(opts)?;
    if changes.is_empty() {
        return Ok(Review {
            base,
            summary: "No changes to review.".into(),
            verdict: "approve".into(),
            ..Review::default()
        });
    }
    let (mut item, cfg, asked, notes) = build(opts, &changes, &title, &description);
    if let Some(r) = report.as_ref().filter(|r| !r.hints.is_empty()) {
        item.payload["tool_hints"] = json!(r.hints.iter().map(|h| h.to_json()).collect::<Vec<_>>());
    }
    let prompt = loopkit::render_prompt(template, &item, &json!({}));
    let (text, cost) = model(&prompt, budget_usd)?;
    let answer = loopkit::parse_answer(Some(&text), &OUTPUT_KEYS)?;
    let allowed: BTreeSet<&str> = item.payload["changed_paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let cleaned = findings::clean(&answer, &allowed, &cfg.min_severity);
    let known: BTreeSet<String> = report
        .iter()
        .flat_map(|r| r.hints.iter().map(|h| h.key()))
        .collect();
    let mut found: Vec<Value> = cleaned
        .findings
        .into_iter()
        .map(|mut f| {
            let hints: Vec<Value> = f["hints"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|h| h.as_str().is_some_and(|h| known.contains(h)))
                .cloned()
                .collect();
            f["hints"] = json!(hints);
            f
        })
        .collect();
    let mut total = cost.unwrap_or(0.0);
    let mut notes = notes;
    if let (Some(v), false) = (verify_model, found.is_empty() || opts.no_verify) {
        let left = (budget_usd - total).clamp(0.0, verify::BUDGET_USD);
        if let Ok((reply, vcost)) = v(
            &verify::prompt(&found, item.payload["diff"].as_str().unwrap_or("")),
            left,
        ) {
            let checked = verify::apply(found, &reply, &cfg.min_severity);
            if checked.dropped > 0 {
                notes.push(format!(
                    "{} candidate finding(s) dropped on verification",
                    checked.dropped
                ));
            }
            found = checked.findings;
            total += vcost.unwrap_or(0.0);
        }
    }
    found.truncate(cfg.finding_cap);
    Ok(Review {
        base,
        summary: cleaned.summary,
        verdict: cleaned.verdict.to_string(),
        findings: found,
        check_results: checks::model_results(&answer, &asked),
        tool_lines: report.map(|r| r.lines).unwrap_or_default(),
        notes,
        cost_usd: total,
    })
}

/// Exit-code gate: does any finding reach `gate` severity?
pub fn blocked(review: &Review, gate: Option<&str>) -> bool {
    gate.is_some_and(|g| {
        review.findings.iter().any(|f| {
            taxonomy::sev_rank(f["severity"].as_str().unwrap_or("")) >= taxonomy::sev_rank(g)
        })
    })
}

/// One JSON object per line: `context`, each `finding`, each `check`, then `complete`.
pub fn ndjson(review: &Review, gate: Option<&str>) -> String {
    let mut lines = vec![
        json!({"type": "context", "base": review.base, "tools": review.tool_lines,
                                "notes": review.notes})
        .to_string(),
    ];
    for f in &review.findings {
        lines.push(
            json!({"type": "finding", "severity": f["severity"], "category": f["category"],
            "path": f["path"], "line": f["line"], "title": f["title"], "body": f["body"],
            "suggestion": f["suggestion"], "fix_prompt": f["fix_prompt"], "hints": f["hints"]})
            .to_string(),
        );
    }
    for c in &review.check_results {
        lines.push(
            json!({"type": "check", "id": c["id"], "name": c["name"], "mode": c["mode"],
                          "result": c["result"], "reason": c["reason"]})
            .to_string(),
        );
    }
    lines.push(
        json!({"type": "complete", "outcome": "ok", "verdict": review.verdict,
                      "summary": review.summary, "findings": review.findings.len(),
                      "blocked": blocked(review, gate), "cost_usd": review.cost_usd})
        .to_string(),
    );
    lines.join("\n")
}

/// Plain text for a terminal.
pub fn text(review: &Review) -> String {
    let mut out = vec![format!(
        "LoopX review against {}: {}",
        review.base, review.verdict
    )];
    if !review.summary.is_empty() {
        out.push(review.summary.clone());
    }
    for n in &review.notes {
        out.push(format!("  note: {n}"));
    }
    for l in &review.tool_lines {
        out.push(format!("  tools: {l}"));
    }
    for f in &review.findings {
        out.push(String::new());
        out.push(format!(
            "{}:{}  [{} \u{b7} {}]  {}",
            f["path"].as_str().unwrap_or(""),
            f["line"],
            f["severity"].as_str().unwrap_or(""),
            f["category"].as_str().unwrap_or(""),
            f["title"].as_str().unwrap_or("")
        ));
        out.push(format!("  {}", f["body"].as_str().unwrap_or("")));
        if let Some(fix) = f["suggestion"].as_str().filter(|s| !s.is_empty()) {
            out.push(format!("  fix: {fix}"));
        }
    }
    for c in &review.check_results {
        out.push(format!(
            "check {}: {} {}",
            c["name"].as_str().unwrap_or(""),
            c["result"].as_str().unwrap_or(""),
            c["reason"].as_str().unwrap_or("")
        ));
    }
    if review.findings.is_empty() {
        out.push("No findings.".into());
    }
    out.join("\n")
}

/// The loop's prompt template: a checkout's `loops/mr-review/prompt.md`, else the
/// copy built into the binary.
pub fn template(repo_root: Option<&Path>) -> Option<String> {
    repo_root
        .and_then(|r| std::fs::read_to_string(r.join("loops/mr-review/prompt.md")).ok())
        .or_else(|| crate::bundled::text("loops/mr-review/prompt.md").map(str::to_string))
}

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(480);

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(
                ok.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&ok.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "T"]);
        std::fs::write(d.path().join("a.py"), "x = 1\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        run(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(d.path().join("a.py"), "x = 1\ny = eval(input())\n").unwrap();
        std::fs::create_dir_all(d.path().join(".loopx/checks")).unwrap();
        std::fs::write(
            d.path().join(".loopx/checks/no-eval.md"),
            "---\nmode: error\n---\nNever eval input.",
        )
        .unwrap();
        std::fs::write(d.path().join("CLAUDE.md"), "Keep it simple.").unwrap();
        run(&["add", "a.py"]);
        run(&["commit", "-q", "-m", "Read y from input"]);
        d
    }

    #[test]
    fn a_branch_is_reviewed_with_the_loops_prompt_guidance_and_checks() {
        let d = repo();
        let opts = Options {
            dir: d.path().to_path_buf(),
            base: Some("main".into()),
            ..Options::default()
        };
        let (changes, title, _, base) = collect(&opts).unwrap();
        assert_eq!(
            (changes.len(), title.as_str(), base.as_str()),
            (1, "Read y from input", "main")
        );
        let model = |p: &str, _b: f64| {
            assert!(p.contains("+y = eval(input())"));
            assert!(p.contains("\"repo_guidance\"") && p.contains("Keep it simple."));
            assert!(p.contains("\"pre_merge_checks\""));
            Ok((json!({"summary": "s", "verdict": "request_changes", "overview": [], "risks": [],
                       "findings": [{"path": "a.py", "line": 2, "severity": "major", "category": "security",
                                     "title": "eval on input", "body": "b", "fix_prompt": "use int()"}],
                       "check_results": [{"id": "no-eval", "result": "failed", "reason": "eval(input())"}]}).to_string(), Some(0.2)))
        };
        let r = run(&opts, "{{item_json}}", 1.0, &model, None, None).unwrap();
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.check_results[0]["result"], "failed");
        assert!(blocked(&r, Some("major")) && !blocked(&r, Some("blocker")) && !blocked(&r, None));
        let lines: Vec<Value> = ndjson(&r, Some("major"))
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let kinds: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["context", "finding", "check", "complete"]);
        assert_eq!(lines[1]["fix_prompt"], "use int()");
        assert_eq!(lines[3]["blocked"], true);
        assert!(text(&r).contains("a.py:2  [major \u{b7} security]  eval on input"));
    }

    #[test]
    fn uncommitted_scope_and_nothing_to_review() {
        let d = repo();
        let opts = Options {
            dir: d.path().to_path_buf(),
            uncommitted: true,
            ..Options::default()
        };
        let r = run(
            &opts,
            "{{item_json}}",
            1.0,
            &|_, _| panic!("no model call"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(r.summary, "No changes to review.");
        std::fs::write(d.path().join("a.py"), "x = 2\ny = eval(input())\n").unwrap();
        let (changes, title, _, _) = collect(&opts).unwrap();
        assert_eq!((changes.len(), title.as_str()), (1, "Uncommitted changes"));
    }

    #[test]
    fn the_template_falls_back_to_the_built_in_copy() {
        assert!(template(None).unwrap().contains("{{item_json}}"));
    }
}
