//! Builds a project's guide: one plan call, then one call per page, each reply checked,
//! merged with the stored page (human parts kept, approved pages held) and saved.
//!
//! Files are read only through an inventory taken now, so a hand-edited `plan.json`
//! cannot widen what is read. Failure reasons are fixed phrases: no file text, reply
//! text or model-supplied path ever reaches an error or the report. No lock is held
//! across an agent call; each page's save is its own locked section.

use crate::guide_inventory::{inventory, Inventory};
use crate::guide_plan::{self, Plan};
use crate::guide_prompt::{page_prompt, page_summary, plan_prompt, PageSpec};
use crate::guide_source::{load_files, PAGE_CAP};
use crate::profile_build::{self, Agent, RunGuard};
use crate::profile_progress::{report, settle, Progress, Step};
use crate::profile_scan::{digest_for, scan_repo, IssueDigest, Scan};
use crate::project_guide::{self, check_slug, update_if_locked, Kind, Page, Part};
use crate::project_profile::{self, Origin, Status};
use chrono::{DateTime, Utc};
use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

const AI_FAILED: &str = "the AI call failed";
const NO_PARSE: &str = "the reply did not parse";
const SECRET: &str = "secret-shaped text found";
const NOT_SAVED: &str = "could not save";
const TOO_LARGE: &str = "the prompt for this page is too large";
const NOT_STATED: &str = "not stated";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageFailure {
    pub slug: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct BuildReport {
    /// Slugs written, in build order.
    pub saved: Vec<String>,
    /// Fresh drafts for pages that are approved: never written.
    pub held: Vec<Page>,
    pub failed: Vec<PageFailure>,
    /// `(slug, paths)` of files left out of a page because they tripped the secret guard.
    pub skipped_files: Vec<(String, Vec<String>)>,
}

/// The guide's single-flight slot, apart from the profile's.
pub fn try_acquire(alias: &str) -> Option<RunGuard> {
    profile_build::try_acquire(&format!("guide:{alias}"))
}

/// One page to write, all of it taken from the validated plan.
struct Job {
    kind: Kind,
    slug: String,
    title: String,
    summary: String,
    related: Vec<String>,
    files: Vec<String>,
}

fn fixed_job(kind: Kind, title: &str, files: &[String]) -> Job {
    Job {
        kind,
        slug: kind.as_str().to_string(),
        title: title.to_string(),
        summary: page_summary(kind).to_string(),
        related: Vec::new(),
        files: files.to_vec(),
    }
}

/// Architecture, data model (only when the plan names files for it), then the features.
fn jobs(plan: &Plan) -> Vec<Job> {
    let mut out = vec![fixed_job(
        Kind::Architecture,
        "Architecture",
        &plan.architecture.files,
    )];
    if !plan.data_model.files.is_empty() {
        out.push(fixed_job(
            Kind::DataModel,
            "Data model",
            &plan.data_model.files,
        ));
    }
    out.extend(plan.features.iter().map(|f| Job {
        kind: Kind::Feature,
        slug: f.slug.clone(),
        title: f.title.clone(),
        summary: f.summary.clone(),
        related: f.related.clone(),
        files: f.files.clone(),
    }));
    out
}

/// A markdown link whose target looks like a page slug but names no known page (and not
/// this one) becomes its plain text; every other link is left alone.
fn unlink_unknown(body: &str, own: &str, known: &[String]) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\[([^\]\n]+)\]\(([^)\s]+)\)").expect("static regex"));
    re.replace_all(body, |c: &regex::Captures| {
        let target = &c[2];
        let unknown = target != own && !known.iter().any(|k| k == target);
        if check_slug(target).is_ok() && unknown {
            c[1].to_string()
        } else {
            c[0].to_string()
        }
    })
    .into_owned()
}

/// The parts of a page reply: every part of `kind`, in order, missing ones `not stated`,
/// all of agent origin. Unknown or repeated parts, or none at all, fail.
fn check_reply(
    kind: Kind,
    alias: &str,
    slug: &str,
    title: &str,
    reply: &str,
    known: &[String],
) -> Result<Vec<Part>, String> {
    let text = format!(
        "---\nalias: {alias}\nslug: {slug}\nkind: {}\ntitle: {title}\n---\n{reply}",
        kind.as_str()
    );
    let parsed = project_guide::parse(&text).map_err(|_| NO_PARSE.to_string())?;
    if parsed.parts.is_empty() {
        return Err(NO_PARSE.into());
    }
    Ok(kind
        .parts()
        .iter()
        .map(|(key, _)| {
            let body = parsed
                .parts
                .iter()
                .find(|p| p.key == *key)
                .map(|p| p.body.clone())
                .filter(|b| !b.trim().is_empty())
                .unwrap_or_else(|| NOT_STATED.to_string());
            let body = if *key == "associations" {
                unlink_unknown(&body, slug, known)
            } else {
                body
            };
            Part {
                key: key.to_string(),
                body,
                origin: Origin::Agent,
            }
        })
        .collect())
}

/// What to write over `existing`: `fresh` with the stored page's human parts kept, or
/// `None` when the stored page is approved (it is held, never written).
fn merge(existing: Option<Page>, mut fresh: Page) -> Option<Page> {
    let Some(cur) = existing else {
        return Some(fresh);
    };
    if cur.status == Status::Approved {
        return None;
    }
    for part in &mut fresh.parts {
        if let Some(h) = cur
            .parts
            .iter()
            .find(|x| x.key == part.key && x.origin == Origin::Human)
        {
            *part = h.clone();
        }
    }
    Some(fresh)
}

fn save_reason(err: &str) -> &'static str {
    if err.contains("looks like a secret") {
        SECRET
    } else {
        NOT_SAVED
    }
}

fn prompt_reason(err: &str) -> &'static str {
    if err.contains("looks like a secret") {
        SECRET
    } else {
        TOO_LARGE
    }
}

/// Everything a page call reads, gathered once per run.
struct Ctx<'a> {
    alias: &'a str,
    work_dir: &'a Path,
    agent: &'a dyn Agent,
    now: DateTime<Utc>,
    root: Option<&'a Path>,
    plan: &'a Plan,
    inv: &'a Inventory,
    scan: &'a Scan,
    digest: &'a IssueDigest,
    known: Vec<String>,
}

enum Outcome {
    Saved,
    Held(Box<Page>),
}

fn write_page(ctx: &Ctx, job: &Job, out: &mut BuildReport) -> Result<Outcome, &'static str> {
    let loaded = load_files(ctx.work_dir, ctx.inv, &job.files, PAGE_CAP);
    if !loaded.skipped_secret.is_empty() {
        out.skipped_files
            .push((job.slug.clone(), loaded.skipped_secret.clone()));
    }
    let spec = PageSpec {
        kind: job.kind,
        title: &job.title,
        summary: &job.summary,
        plan: ctx.plan,
    };
    let prompt = page_prompt(ctx.alias, &spec, &loaded, ctx.scan, ctx.digest)
        .map_err(|e| prompt_reason(&e))?;
    let reply = ctx.agent.run(&prompt).map_err(|_| AI_FAILED)?;
    let parts = check_reply(
        job.kind, ctx.alias, &job.slug, &job.title, &reply, &ctx.known,
    )
    .map_err(|_| NO_PARSE)?;
    let fresh = Page {
        alias: ctx.alias.to_string(),
        slug: job.slug.clone(),
        kind: job.kind,
        title: job.title.clone(),
        status: Status::Draft,
        generated_at: Some(ctx.now),
        head_sha: ctx.scan.head_sha.clone(),
        summary: job.summary.clone(),
        related: job.related.clone(),
        parts,
        sources: loaded.sources,
    };
    // Checked here too, so a held draft is as clean as a saved one.
    project_guide::validate(&fresh).map_err(|e| save_reason(&e))?;
    let mut held = None;
    let written = update_if_locked(ctx.alias, &job.slug, ctx.root, |cur| {
        let merged = merge(cur, fresh.clone());
        if merged.is_none() {
            held = Some(fresh);
        }
        Ok(merged)
    })
    .map_err(|e| save_reason(&e))?;
    match (written, held) {
        (Some(_), _) => Ok(Outcome::Saved),
        (None, Some(p)) => Ok(Outcome::Held(Box::new(p))),
        (None, None) => Err(NOT_SAVED),
    }
}

fn run_job(ctx: &Ctx, job: &Job, out: &mut BuildReport) {
    match write_page(ctx, job, out) {
        Ok(Outcome::Saved) => out.saved.push(job.slug.clone()),
        Ok(Outcome::Held(p)) => out.held.push(*p),
        Err(reason) => out.failed.push(PageFailure {
            slug: job.slug.clone(),
            reason: reason.to_string(),
        }),
    }
}

/// What both entry points read before any AI call. Fails on a missing checkout or a
/// corrupt profile or plan; gives the saved plan, if any.
fn prepare(
    alias: &str,
    work_dir: &Path,
    instance: &str,
    project_id: &str,
    tracked: &[String],
    root: Option<&Path>,
) -> Result<(Option<Plan>, Scan, Inventory, IssueDigest), String> {
    if !work_dir.is_dir() {
        return Err(format!(
            "the checkout for {alias} is missing ({}); clone it first",
            work_dir.display()
        ));
    }
    project_profile::load(alias, root)?;
    let saved = guide_plan::load(alias, root)?;
    let scan = scan_repo(work_dir, tracked)?;
    let inv = inventory(work_dir);
    let digest = digest_for(instance, project_id, alias, root);
    Ok((saved, scan, inv, digest))
}

fn known_slugs(jobs: &[Job]) -> Vec<String> {
    jobs.iter().map(|j| j.slug.clone()).collect()
}

/// Plan the guide and write every page in it as a draft. `Err` only when nothing could
/// be built (missing checkout, corrupt state, the plan call failed or its plan was
/// invalid); a page that fails is reported in `failed` and the build goes on.
#[allow(clippy::too_many_arguments)]
pub fn build_guide(
    alias: &str,
    work_dir: &Path,
    instance: &str,
    project_id: &str,
    tracked: &[String],
    agent: &dyn Agent,
    now: DateTime<Utc>,
    root: Option<&Path>,
    progress: Option<&dyn Progress>,
) -> Result<BuildReport, String> {
    let res = (|| {
        let (_, scan, inv, digest) = prepare(alias, work_dir, instance, project_id, tracked, root)?;
        let prompt = plan_prompt(alias, &inv, &scan, &digest)?;
        let reply = agent.run(&prompt)?;
        let plan = guide_plan::parse_plan(&reply, &inv)
            .map_err(|e| format!("the plan reply did not parse: {e}"))?;
        let jobs = jobs(&plan);
        let total = 1 + jobs.len();
        report(progress, 1, total, Step::Plan);
        guide_plan::save(alias, &plan, root)?;
        let ctx = Ctx {
            alias,
            work_dir,
            agent,
            now,
            root,
            plan: &plan,
            inv: &inv,
            scan: &scan,
            digest: &digest,
            known: known_slugs(&jobs),
        };
        let mut out = BuildReport::default();
        for (i, job) in jobs.iter().enumerate() {
            report(progress, i + 2, total, Step::Page);
            run_job(&ctx, job, &mut out);
        }
        Ok(out)
    })();
    settle(progress, &res);
    res
}

/// Rewrite one page (`architecture`, `data-model` or a feature) from the saved plan, with
/// one agent call. `Err` when there is no plan or the slug is not in it.
#[allow(clippy::too_many_arguments)]
pub fn rebuild_page(
    alias: &str,
    slug: &str,
    work_dir: &Path,
    instance: &str,
    project_id: &str,
    tracked: &[String],
    agent: &dyn Agent,
    now: DateTime<Utc>,
    root: Option<&Path>,
    progress: Option<&dyn Progress>,
) -> Result<BuildReport, String> {
    let res = (|| {
        check_slug(slug)?;
        let (saved, scan, inv, digest) =
            prepare(alias, work_dir, instance, project_id, tracked, root)?;
        let plan = saved.ok_or("there is no guide plan yet; build the guide first")?;
        let jobs = jobs(&plan);
        let job = jobs
            .iter()
            .find(|j| j.slug == slug)
            .ok_or_else(|| format!("{slug} is not in the guide plan"))?;
        report(progress, 1, 1, Step::Page);
        let ctx = Ctx {
            alias,
            work_dir,
            agent,
            now,
            root,
            plan: &plan,
            inv: &inv,
            scan: &scan,
            digest: &digest,
            known: known_slugs(&jobs),
        };
        let mut out = BuildReport::default();
        run_job(&ctx, job, &mut out);
        Ok(out)
    })();
    settle(progress, &res);
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_profile::{Origin, Status};
    use chrono::TimeZone;
    use std::cell::RefCell;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap()
    }

    struct Fake {
        prompts: RefCell<Vec<String>>,
        plan: String,
        fail_on: Option<&'static str>, // a page title (or "PLAN") whose call errors
        page: fn(&str) -> String,
    }

    fn parts_reply(keys: &[(&str, &str)]) -> String {
        keys.iter()
            .map(|(k, h)| format!("<!-- part: {k} origin: agent -->\n## {h}\n\nBody of {k}.\n\n"))
            .collect()
    }

    fn default_page(prompt: &str) -> String {
        if prompt.contains("<!-- part: what origin: agent -->") {
            parts_reply(&[
                ("what", "What it does"),
                ("value", "Business value and metrics"),
                ("how", "How it works"),
                ("associations", "Associations"),
                ("key-points", "Key points"),
            ])
        } else if prompt.contains("<!-- part: entities origin: agent -->") {
            parts_reply(&[
                ("entities", "Entities"),
                ("relationships", "Relationships"),
                ("rules", "Rules and invariants"),
            ])
        } else {
            parts_reply(&[
                ("overview", "Overview"),
                ("components", "Components"),
                ("flows", "Request and data flows"),
                ("runtime", "Runtime and deployment"),
            ])
        }
    }

    impl Agent for Fake {
        fn run(&self, prompt: &str) -> Result<String, String> {
            self.prompts.borrow_mut().push(prompt.to_string());
            if prompt.contains("\"features\"") && prompt.contains("Reply with the JSON only") {
                return if self.fail_on == Some("PLAN") {
                    Err("boom".into())
                } else {
                    Ok(self.plan.clone())
                };
            }
            if let Some(t) = self.fail_on {
                if prompt.contains(&format!("Page title: {t}")) {
                    return Err("boom".into());
                }
            }
            Ok((self.page)(prompt))
        }
    }

    const PLAN: &str = r#"{"features":[
      {"slug":"feature-orders","title":"Orders","summary":"Taking orders.","files":["app/order.rb"],"related":["feature-cart"]},
      {"slug":"feature-cart","title":"Cart","summary":"Holding items.","files":["app/cart.rb","app/leak.rb"],"related":[]}],
      "architecture":{"files":["config/routes.rb"]},"data_model":{"files":["db/schema.rb"]}}"#;

    fn fake(plan: &str, fail_on: Option<&'static str>) -> Fake {
        Fake {
            prompts: RefCell::new(vec![]),
            plan: plan.into(),
            fail_on,
            page: default_page,
        }
    }

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let w = |rel: &str, t: &str| {
            let p = d.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, t).unwrap();
        };
        w("README.md", "# Shop\nSells things.\n");
        w(
            "app/order.rb",
            "class Order\n  api_key = \"abcd1234efgh5678ijkl\"\nend\n",
        );
        w("app/cart.rb", "class Cart\nend\n");
        w(
            "app/leak.rb",
            "# glpat-AAAAAAAAAAAAAAAAAAAA\nclass Leak\nend\n",
        );
        w("config/routes.rb", "get '/orders'\n");
        w(
            "db/schema.rb",
            "create_table \"orders\" do |t|\n  t.integer \"total_cents\"\nend\n",
        );
        Command::new("git")
            .arg("-C")
            .arg(d.path())
            .args(["init", "-q"])
            .status()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(d.path())
            .args(["add", "-A"])
            .status()
            .unwrap();
        d
    }

    fn build(d: &Path, f: &Fake, root: &Path) -> Result<BuildReport, String> {
        build_guide("shop", d, "", "", &[], f, now(), Some(root), None)
    }

    #[test]
    fn builds_a_plan_and_every_page_as_drafts() {
        let d = repo();
        let root = tempfile::tempdir().unwrap();
        let f = fake(PLAN, None);
        let r = build(d.path(), &f, root.path()).unwrap();
        assert_eq!(
            r.saved,
            [
                "architecture",
                "data-model",
                "feature-orders",
                "feature-cart"
            ]
        );
        assert!(r.failed.is_empty() && r.held.is_empty());
        assert_eq!(f.prompts.borrow().len(), 5); // plan + 4 pages
        assert!(crate::guide_plan::load("shop", Some(root.path()))
            .unwrap()
            .is_some());
        let p = crate::project_guide::load("shop", "feature-orders", Some(root.path()))
            .unwrap()
            .unwrap();
        assert_eq!(p.status, Status::Draft);
        assert_eq!(p.related, ["feature-cart"]);
        assert_eq!(p.generated_at, Some(now()));
        assert_eq!(p.parts.len(), 5);
        assert!(p.parts.iter().all(|x| x.origin == Origin::Agent));
        assert_eq!(
            p.sources
                .iter()
                .map(|s| s.path.as_str())
                .collect::<Vec<_>>(),
            ["app/order.rb"]
        );
    }

    #[test]
    fn no_secret_reaches_any_prompt_and_a_leaking_file_is_reported() {
        let d = repo();
        let root = tempfile::tempdir().unwrap();
        let f = fake(PLAN, None);
        let r = build(d.path(), &f, root.path()).unwrap();
        for p in f.prompts.borrow().iter() {
            assert!(!p.contains("glpat-") && !p.contains("abcd1234efgh5678ijkl"));
        }
        assert_eq!(
            r.skipped_files,
            [("feature-cart".to_string(), vec!["app/leak.rb".to_string()])]
        );
        let cart = crate::project_guide::load("shop", "feature-cart", Some(root.path()))
            .unwrap()
            .unwrap();
        assert_eq!(
            cart.sources
                .iter()
                .map(|s| s.path.as_str())
                .collect::<Vec<_>>(),
            ["app/cart.rb"]
        );
    }

    #[test]
    fn an_invalid_plan_writes_nothing_and_calls_no_page() {
        let d = repo();
        let root = tempfile::tempdir().unwrap();
        let bad = PLAN.replace("app/order.rb", "../../etc/passwd");
        let f = fake(&bad, None);
        assert!(build(d.path(), &f, root.path()).is_err());
        assert_eq!(f.prompts.borrow().len(), 1);
        assert!(crate::guide_plan::load("shop", Some(root.path()))
            .unwrap()
            .is_none());
        assert!(crate::project_guide::list("shop", Some(root.path()))
            .unwrap()
            .pages
            .is_empty());
    }

    #[test]
    fn a_failing_page_keeps_the_pages_that_finished_and_the_ones_after_it() {
        let d = repo();
        let root = tempfile::tempdir().unwrap();
        let f = fake(PLAN, Some("Orders"));
        let r = build(d.path(), &f, root.path()).unwrap();
        assert_eq!(r.saved, ["architecture", "data-model", "feature-cart"]);
        assert_eq!(r.failed.len(), 1);
        assert_eq!(r.failed[0].slug, "feature-orders");
        assert!(!r.failed[0].reason.contains("boom"));
        assert!(
            crate::project_guide::load("shop", "feature-orders", Some(root.path()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_failing_plan_call_is_an_error_and_writes_nothing() {
        let d = repo();
        let root = tempfile::tempdir().unwrap();
        assert!(build(d.path(), &fake(PLAN, Some("PLAN")), root.path()).is_err());
        assert!(crate::project_guide::list("shop", Some(root.path()))
            .unwrap()
            .pages
            .is_empty());
    }

    #[test]
    fn a_rebuild_keeps_human_parts_and_replaces_agent_parts() {
        let d = repo();
        let root = tempfile::tempdir().unwrap();
        build(d.path(), &fake(PLAN, None), root.path()).unwrap();
        crate::project_guide::update_locked("shop", "feature-orders", Some(root.path()), |cur| {
            let mut p = cur.unwrap();
            let k = p.parts.iter_mut().find(|x| x.key == "key-points").unwrap();
            k.body = "Totals are in cents (written by a person).".into();
            k.origin = Origin::Human;
            Ok(p)
        })
        .unwrap();
        let mut f = fake(PLAN, None);
        f.page = |p| default_page(p).replace("Body of", "Rewritten body of");
        let r = build(d.path(), &f, root.path()).unwrap();
        assert!(r.saved.contains(&"feature-orders".to_string()));
        let p = crate::project_guide::load("shop", "feature-orders", Some(root.path()))
            .unwrap()
            .unwrap();
        let k = p.parts.iter().find(|x| x.key == "key-points").unwrap();
        assert_eq!(
            (k.body.as_str(), k.origin),
            ("Totals are in cents (written by a person).", Origin::Human)
        );
        assert!(p
            .parts
            .iter()
            .find(|x| x.key == "what")
            .unwrap()
            .body
            .starts_with("Rewritten body of"));
    }

    #[test]
    fn an_approved_page_is_never_overwritten_and_comes_back_held() {
        let d = repo();
        let root = tempfile::tempdir().unwrap();
        build(d.path(), &fake(PLAN, None), root.path()).unwrap();
        crate::project_guide::update_locked("shop", "architecture", Some(root.path()), |cur| {
            let mut p = cur.unwrap();
            p.status = Status::Approved;
            Ok(p)
        })
        .unwrap();
        let before = fs::read_to_string(
            crate::project_guide::guide_dir("shop", Some(root.path())).join("architecture.md"),
        )
        .unwrap();
        let history_before =
            crate::project_guide::history("shop", "architecture", Some(root.path())).len();
        let mut f = fake(PLAN, None);
        f.page = |p| default_page(p).replace("Body of", "New body of");
        let r = build(d.path(), &f, root.path()).unwrap();
        assert!(!r.saved.contains(&"architecture".to_string()));
        assert_eq!(r.held.len(), 1);
        assert_eq!(r.held[0].slug, "architecture");
        assert!(r.held[0].parts[0].body.starts_with("New body of"));
        let after = fs::read_to_string(
            crate::project_guide::guide_dir("shop", Some(root.path())).join("architecture.md"),
        )
        .unwrap();
        assert_eq!(before, after);
        // Approving it above took one snapshot; the held rebuild must add none.
        assert_eq!(
            crate::project_guide::history("shop", "architecture", Some(root.path())).len(),
            history_before
        );
    }

    #[test]
    fn a_page_missing_from_the_new_plan_is_kept() {
        let d = repo();
        let root = tempfile::tempdir().unwrap();
        build(d.path(), &fake(PLAN, None), root.path()).unwrap();
        let smaller = r#"{"features":[{"slug":"feature-cart","title":"Cart","summary":"Holding items.","files":["app/cart.rb"],"related":[]}],"architecture":{"files":["config/routes.rb"]},"data_model":{"files":[]}}"#;
        build(d.path(), &fake(smaller, None), root.path()).unwrap();
        assert!(
            crate::project_guide::load("shop", "feature-orders", Some(root.path()))
                .unwrap()
                .is_some()
        );
        assert!(
            crate::project_guide::load("shop", "data-model", Some(root.path()))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn replies_that_are_wrong_fail_that_page_only() {
        let d = repo();
        let root = tempfile::tempdir().unwrap();
        let mut f = fake(PLAN, None);
        f.page = |p| {
            if p.contains("Page title: Cart") {
                "no markers at all".into()
            } else if p.contains("Page title: Orders") {
                format!(
                    "{}<!-- part: bogus origin: agent -->\n## X\n\nx\n",
                    default_page(p)
                )
            } else {
                default_page(p)
            }
        };
        let r = build(d.path(), &f, root.path()).unwrap();
        let slugs: Vec<&str> = r.failed.iter().map(|x| x.slug.as_str()).collect();
        assert_eq!(slugs, ["feature-orders", "feature-cart"]);
        assert!(r
            .failed
            .iter()
            .all(|x| x.reason == "the reply did not parse"));
    }

    #[test]
    fn a_missing_part_becomes_not_stated_and_unknown_association_links_are_unlinked() {
        let d = repo();
        let root = tempfile::tempdir().unwrap();
        let mut f = fake(PLAN, None);
        f.page = |p| {
            if p.contains("Page title: Orders") {
                "<!-- part: what origin: agent -->\n## What it does\n\nIt takes orders.\n\n<!-- part: associations origin: agent -->\n## Associations\n\nNeeds [Cart](feature-cart), [Ghost](feature-ghost) and [docs](https://x.example).\n".into()
            } else {
                default_page(p)
            }
        };
        build(d.path(), &f, root.path()).unwrap();
        let p = crate::project_guide::load("shop", "feature-orders", Some(root.path()))
            .unwrap()
            .unwrap();
        assert_eq!(
            p.parts.iter().find(|x| x.key == "value").unwrap().body,
            "not stated"
        );
        let a = &p
            .parts
            .iter()
            .find(|x| x.key == "associations")
            .unwrap()
            .body;
        assert!(
            a.contains("[Cart](feature-cart)")
                && a.contains("Ghost")
                && !a.contains("(feature-ghost)")
                && a.contains("(https://x.example)")
        );
    }

    #[test]
    fn a_page_with_a_secret_in_the_reply_is_refused() {
        let d = repo();
        let root = tempfile::tempdir().unwrap();
        let mut f = fake(PLAN, None);
        f.page = |p| {
            if p.contains("Page title: Cart") {
                default_page(p).replace("Body of what", "token glpat-AAAAAAAAAAAAAAAAAAAA")
            } else {
                default_page(p)
            }
        };
        let r = build(d.path(), &f, root.path()).unwrap();
        assert_eq!(r.failed.len(), 1);
        assert_eq!(r.failed[0].reason, "secret-shaped text found");
        assert!(
            crate::project_guide::load("shop", "feature-cart", Some(root.path()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rebuild_page_reuses_the_saved_plan_and_calls_the_agent_once() {
        let d = repo();
        let root = tempfile::tempdir().unwrap();
        build(d.path(), &fake(PLAN, None), root.path()).unwrap();
        let f = fake("unused", None);
        let r = rebuild_page(
            "shop",
            "feature-cart",
            d.path(),
            "",
            "",
            &[],
            &f,
            now(),
            Some(root.path()),
            None,
        )
        .unwrap();
        assert_eq!(r.saved, ["feature-cart"]);
        assert_eq!(f.prompts.borrow().len(), 1);
        assert!(rebuild_page(
            "shop",
            "feature-nope",
            d.path(),
            "",
            "",
            &[],
            &f,
            now(),
            Some(root.path()),
            None
        )
        .is_err());
        let none = tempfile::tempdir().unwrap();
        assert!(rebuild_page(
            "shop",
            "feature-cart",
            d.path(),
            "",
            "",
            &[],
            &f,
            now(),
            Some(none.path()),
            None
        )
        .is_err());
    }

    #[test]
    fn a_missing_checkout_or_empty_repo_is_an_error_not_a_panic() {
        let root = tempfile::tempdir().unwrap();
        assert!(build(
            Path::new("/nonexistent/checkout"),
            &fake(PLAN, None),
            root.path()
        )
        .is_err());
        let empty = tempfile::tempdir().unwrap();
        let _ = build(empty.path(), &fake(PLAN, None), root.path()); // must not panic
    }

    #[test]
    fn the_guide_slot_is_independent_of_the_profile_slot() {
        let g = try_acquire("shop").unwrap();
        assert!(try_acquire("shop").is_none());
        assert!(crate::profile_build::try_acquire("shop").is_some());
        drop(g);
        assert!(try_acquire("shop").is_some());
    }

    #[test]
    fn progress_counts_the_plan_call_and_every_page() {
        use crate::profile_progress::{Progress, Step};
        struct Rec(RefCell<Vec<(usize, usize, Step)>>, RefCell<u8>);
        impl Progress for Rec {
            fn step(&self, n: usize, t: usize, s: Step) {
                self.0.borrow_mut().push((n, t, s));
            }
            fn done(&self) {
                *self.1.borrow_mut() += 1;
            }
            fn fail(&self, _: &str) {}
        }
        let d = repo();
        let root = tempfile::tempdir().unwrap();
        let rec = Rec(RefCell::new(vec![]), RefCell::new(0));
        build_guide(
            "shop",
            d.path(),
            "",
            "",
            &[],
            &fake(PLAN, None),
            now(),
            Some(root.path()),
            Some(&rec),
        )
        .unwrap();
        let steps = rec.0.borrow();
        // total = 1 (the plan call) + 4 pages (architecture, data model, 2 features).
        assert_eq!(
            steps.first().map(|s| (s.0, s.1, s.2)),
            Some((1, 5, Step::Plan))
        );
        assert_eq!(
            steps.last().map(|s| (s.0, s.1, s.2)),
            Some((5, 5, Step::Page))
        );
        assert_eq!(*rec.1.borrow(), 1);
    }
}
