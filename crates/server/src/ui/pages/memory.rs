//! Insights → Memory (`/insights?view=memory`): what LoopX knows about each project.
//!
//! Three levels, all read-only views of `<home>/memory/<alias>/` (`PROFILE.md`, the
//! task memories, `SETUP.md`) plus the `profile-section` proposals in the approvals
//! store and the weekly sync status in `profile_sync.json`:
//!
//! 1. overview (`/insights?view=memory`): a card per project, with a Memory/Graph toggle;
//! 2. project page (`&project=<alias>`): one summary card per section;
//! 3. sub-pages (`&project=<alias>&page=<slug>`): the full section, the lessons
//!    (per-issue task memories, formerly this whole view) and the proposals.
//!
//! Every query value is untrusted: `project` must be one of the known aliases and
//! `page` one of [`PAGES`], and nothing from the query is echoed back. Profile text
//! is escaped, and a section (or proposal, or parse error) that looks like it holds
//! a secret value is replaced by a notice naming only the kind of secret.
//!
//! The POST routes (all CSRF-checked by the registry) live at the end of this
//! module: build a profile (one or all, in the background), save a section by hand,
//! refresh one section into a proposal (background), approve or dismiss a proposal,
//! and the weekly sync switch, schedule and "Sync now". Every form value is
//! untrusted: aliases, slugs, ids and choices are checked against fixed or known
//! sets, and a redirect is built only from checked values plus a `notice` code from
//! a fixed list ([`Notice`]), which the page maps to a translated message. Nothing
//! from the query or the form is echoed back.

use super::insights::{f_of, fixed, icon as ds_icon, n_of, truthy};
use crate::ui::icons::icon;
use crate::ui::markdown::{self, GitlabPrefixes};
use crate::ui::pyfmt::{escape, escape_text, quote};
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::state::ProfileAgent;
use chrono::{DateTime, Duration, Local, Utc};
use loopx_core::profile_graph::{self, Edge, EdgeKind, Graph};
use loopx_core::profile_build;
use loopx_core::profile_progress::{failure_reason, JobKind as ProfileJobKind, Progress, ProfileJob};
use loopx_core::{guide_build, guide_proposal};
use loopx_core::profile_notify::{self, BuildDone, Finished, RowResult};
use loopx_core::profile_sync::{self, Freshness, Outcome, SyncStore, TrackedProject};
use loopx_core::project_profile::{self, contains_secret, Origin, Profile, Section, SectionKey, Status};
use loopx_core::profile_scan::IssueDigest;
use loopx_core::{approvals, learning, memory_store, profile_scan};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::cell::OnceCell;
use std::sync::{Arc, Mutex};

// An explicit path: `build.rs` registers every `pages/<name>.rs` as a page, and a
// child module resolved by default would sit beside it in `pages/`.
#[path = "memory/guide.rs"]
mod guide;

const BASE: &str = "/insights?view=memory";

/// The sub-page slugs, in sub-nav order. `activity` holds Recent work and People;
/// `guide` is the project guide (its own module, [`guide`]).
pub const PAGES: [&str; 12] = [
    "overview",
    "structure",
    "features",
    "stack",
    "integrations",
    "secrets",
    "conventions",
    "gotchas",
    "activity",
    "guide",
    "lessons",
    "proposals",
];

pub fn register(r: &mut Registry) {
    r.post("/insights/memory/build", build);
    r.post("/insights/memory/approve", approve_profile);
    r.post("/insights/memory/section/save", save_section);
    r.post("/insights/memory/section/refresh", refresh_section);
    r.post("/insights/memory/proposal/decide", decide);
    r.post("/insights/memory/sync/settings", sync_settings);
    r.post("/insights/memory/sync/now", sync_now);
    r.post("/insights/memory/sync/slack", slack_settings);
    r.post("/insights/memory/sync/clone", clone_settings);
    r.post("/insights/memory/checkout/reclone", reclone);
    r.post("/insights/memory/wiki/add", wiki_add);
    r.post("/insights/memory/wiki/remove", wiki_remove);
    r.post("/insights/memory/guide/build", guide_build_route);
    r.post("/insights/memory/guide/rebuild-page", guide_rebuild_page);
    r.post("/insights/memory/guide/approve", guide::approve_route);
    r.post("/insights/memory/guide/approve-all", guide::approve_all_route);
    r.post("/insights/memory/guide/edit", guide::edit_route);
    r.post("/insights/memory/guide/delete", guide::delete_route);
    r.post("/insights/memory/guide/restore", guide::restore_route);
    r.get("/insights/memory/jobs", jobs_status);
}

pub fn render(ctx: &UiCtx) -> String {
    let q = ctx.query();
    let m = model(ctx);
    let mut watched = None;
    let body = match q.first("project") {
        None => overview(ctx, &m, q.first("graph") == Some("1")),
        Some(raw) => match m.projects.iter().find(|p| p.alias == raw) {
            None => unknown_project(ctx),
            Some(p) => {
                watched = Some(p.alias.as_str());
                match q.first("page") {
                    None => project_page(ctx, p, &m),
                    Some(page) => match PAGES.iter().find(|s| **s == page) {
                        Some(slug) => sub_page(ctx, p, slug, &m),
                        None => frame(ctx, p, None, &unknown_page(ctx, p), &m),
                    },
                }
            }
        },
    };
    format!("{STYLE}{body}{}", live_marker(ctx, watched))
}

// ------------------------------------------------------------------ live updates

/// The marker `live.js` starts from, and the script itself, on every Memory page.
/// The script only polls when the page shows a running job (a `data-mem-job`
/// element), or briefly after a Build or Refresh click sent the user here
/// (`data-mem-watch`). `project` is a checked alias, never the raw query value.
fn live_marker(ctx: &UiCtx, project: Option<&str>) -> String {
    let started = ctx
        .query()
        .first("notice")
        .and_then(Notice::from_code)
        .is_some_and(|n| {
            matches!(
                n,
                Notice::BuildStarted | Notice::BuildAllStarted | Notice::RefreshStarted | Notice::GuideBuildStarted
            )
        });
    let project = project.map_or_else(String::new, |a| format!(" data-mem-project='{}'", escape(a)));
    let watch = if started { " data-mem-watch" } else { "" };
    let script = crate::ui::assets::text("memory/live.js").unwrap_or("");
    format!(
        "<div hidden data-mem-live data-mem-url='/insights/memory/jobs'{project}{watch} data-mem-finished='{}' data-mem-reload='{}'></div><script>{script}</script>",
        escape(&ctx.t("Finished — reload to see the result")),
        escape(&ctx.t("Reload"))
    )
}

/// How far a job is, 0 to 100, for a progress bar.
fn job_percent(job: &loopx_core::profile_progress::JobView) -> usize {
    if job.steps == 0 {
        0
    } else {
        (job.step * 100 / job.steps).min(100)
    }
}

/// A thin progress bar `live.js` moves in place; `label` is what a screen reader hears.
fn progress_bar(class: &str, percent: usize, label: &str) -> String {
    format!(
        "<div class='mem-progress{class}' role='progressbar' aria-valuemin='0' aria-valuemax='100' aria-valuenow='{percent}' aria-label='{}' data-mem-progress><span style='width:{percent}%'></span></div>",
        escape(label)
    )
}

/// `GET /insights/memory/jobs`: what the page polls while a job runs. The same
/// sources and filters as the page: live job records (live pid, not stale) and this
/// process's own single-flight slots, the weekly sync's record, and the last failed
/// build or refresh (dropped after 24h). Steps are translated for the request. An
/// unknown `project` filter matches nothing and is never echoed.
fn jobs_status(ctx: &UiCtx) -> UiResponse {
    let config = loopx_core::config::Config::load(&ctx.state.home_dir.join("projects.json")).ok();
    let aliases = alias_list(ctx, config.as_ref());
    let wanted = ctx.query().first("project").map(str::to_string);
    let shown = |alias: &str| match &wanted {
        None => true,
        Some(w) => w == alias && aliases.iter().any(|a| a == alias),
    };
    // Judged at the real time, like the page's own `live_job`.
    let jobs = loopx_core::profile_progress::running(&crate::ui::profile_jobs::base(ctx));
    let mut running: Vec<Value> = Vec::new();
    for job in jobs.iter().filter(|j| !j.is_sync() && !j.is_guide() && shown(&j.alias)) {
        running.push(serde_json::json!({
            "alias": job.alias,
            "kind": job.kind,
            "step": job.step,
            "steps": job.steps,
            "code": job.step_code,
            "label": crate::ui::profile_jobs::step_detail(ctx, job),
        }));
    }
    // A build this process started whose record is not written yet.
    for alias in aliases.iter().filter(|a| shown(a)) {
        if profile_build::is_running(alias) && !running.iter().any(|r| r["alias"] == alias.as_str()) {
            running.push(serde_json::json!({
                "alias": alias, "kind": "build", "step": 0, "steps": 0, "code": "start",
                "label": ctx.t("Starting"),
            }));
        }
    }
    // Guide runs, apart from the profile jobs (independent slots).
    let mut guide: Vec<Value> = Vec::new();
    for job in jobs.iter().filter(|j| j.is_guide() && shown(&j.alias)) {
        guide.push(serde_json::json!({
            "alias": job.alias,
            "step": job.step,
            "steps": job.steps,
            "code": job.step_code,
            "label": crate::ui::profile_jobs::step_detail(ctx, job),
        }));
    }
    for alias in aliases.iter().filter(|a| shown(a)) {
        if guide_slot_held(alias) && !guide.iter().any(|g| g["alias"] == alias.as_str()) {
            guide.push(serde_json::json!({
                "alias": alias, "step": 0, "steps": 0, "code": "start", "label": ctx.t("Starting"),
            }));
        }
    }
    let sync = if wanted.is_some() {
        None
    } else {
        jobs.iter().find(|j| j.is_sync()).map(|j| {
            serde_json::json!({
                "step": j.step,
                "steps": j.steps,
                "code": j.step_code,
                "label": crate::ui::profile_jobs::progress_text(ctx, j),
            })
        })
    };
    let mut failed: Vec<Value> = Vec::new();
    for alias in aliases.iter().filter(|a| shown(a)) {
        if busy(ctx, alias) {
            continue;
        }
        if let Some(Job { at, outcome: JobOutcome::Failed(e), .. }) = last_job(&ctx.state.home_dir, alias) {
            if ctx.state.now() - at < FAILURE_SHOWN_FOR {
                failed.push(serde_json::json!({"alias": alias, "reason": shown_failure(ctx, &e)}));
            }
        }
    }
    UiResponse::bytes(
        200,
        "application/json",
        &[("X-Content-Type-Options", "nosniff"), ("Cache-Control", "no-store")],
        serde_json::json!({
            "running": running,
            "guide": guide,
            "sync": sync,
            "build_all": wanted.is_none() && build_all_running(&ctx.state.home_dir),
            "failed": failed,
        })
        .to_string()
        .into_bytes(),
    )
}

// ------------------------------------------------------------------ the data

/// Everything one request needs, read once: each project with its parsed profile
/// and freshness inputs, and the profile proposals.
struct Model {
    projects: Vec<Project>,
    proposals: Vec<Value>,
    /// The banner for a whitelisted `notice` code, or empty.
    notice: String,
    /// Each project's issue digest, read (through the process-wide digest cache)
    /// on first use.
    digests: OnceCell<BTreeMap<String, IssueDigest>>,
    /// Who links to whom, without a layout: the overview and project pages.
    links: OnceCell<BTreeMap<String, Vec<(String, EdgeKind)>>>,
    /// The laid-out graph: only the graph view asks for it.
    graph: OnceCell<Graph>,
}

struct Project {
    alias: String,
    instance: String,
    project_id: String,
    loaded: Loaded,
    /// A tracked project whose checkout exists (or that Memory clones itself on the
    /// first build): Build and Refresh can scan it.
    checkout: bool,
    /// No `local_path`: Memory keeps its own read-only clone (see `profile_checkout`).
    managed: bool,
    /// Only computed (from the checkout) when the profile parsed.
    check: Check,
}

fn memory_root(ctx: &UiCtx) -> PathBuf {
    ctx.state.home_dir.join("memory")
}

fn approvals_dir(ctx: &UiCtx) -> PathBuf {
    ctx.state.repo_root.join("outputs").join("approvals")
}

fn model(ctx: &UiCtx) -> Model {
    Model {
        projects: projects(ctx),
        proposals: proposals(ctx),
        notice: notice_html(ctx),
        digests: OnceCell::new(),
        links: OnceCell::new(),
        graph: OnceCell::new(),
    }
}

fn parsed_profiles(m: &Model) -> Vec<Profile> {
    m.projects
        .iter()
        .filter_map(|p| match &p.loaded {
            Loaded::Ready(profile) => Some(profile.clone()),
            _ => None,
        })
        .collect()
}

fn aliases(m: &Model) -> Vec<String> {
    m.projects.iter().map(|p| p.alias.clone()).collect()
}

/// This request's issue digests, read once, from the dashboard's own cache roots
/// (`user_home`'s `.gitlab/cache` and `<home>/data`), never the process's
/// `HOME`, and memoised across requests until a project's issue cache changes.
fn digests<'a>(ctx: &UiCtx, m: &'a Model) -> &'a BTreeMap<String, IssueDigest> {
    m.digests.get_or_init(|| {
        let gitlab_root = ctx.state.user_home.join(".gitlab").join("cache");
        let data_root = ctx.state.home_dir.join("data");
        let memory = memory_root(ctx);
        m.projects
            .iter()
            .filter(|p| !p.instance.is_empty() && !p.project_id.is_empty())
            .map(|p| {
                let d = profile_scan::digest_issues_cached(
                    &p.instance,
                    &p.project_id,
                    &p.alias,
                    &gitlab_root,
                    &data_root,
                    Some(&memory),
                );
                (p.alias.clone(), d)
            })
            .collect()
    })
}

/// Per alias, its directly linked projects: edges only, no layout.
fn links<'a>(ctx: &UiCtx, m: &'a Model) -> &'a BTreeMap<String, Vec<(String, EdgeKind)>> {
    m.links.get_or_init(|| {
        let edges = profile_graph::derive_edges(&parsed_profiles(m), digests(ctx, m), &aliases(m));
        neighbors(&edges)
    })
}

/// The laid-out graph of every known project (the graph view only).
fn graph<'a>(ctx: &UiCtx, m: &'a Model) -> &'a Graph {
    m.graph
        .get_or_init(|| profile_graph::derive(&parsed_profiles(m), digests(ctx, m), &aliases(m)))
}

/// Like the old Memory view, every alias of `projects.json` in its order (the
/// code-changing ones with their checkout), then every other alias with a memory
/// directory (a profile, lessons or setup notes), sorted.
fn projects(ctx: &UiCtx) -> Vec<Project> {
    let config_path = ctx.state.home_dir.join("projects.json");
    let tracked = profile_sync::tracked_projects_in(&config_path);
    let config = loopx_core::config::Config::load(&config_path).ok();
    alias_list(ctx, config.as_ref())
        .into_iter()
        .map(|alias| {
            let managed = tracked.iter().any(|t| t.alias == alias && t.managed);
            let (instance, project_id, work_dir) = match tracked.iter().find(|t| t.alias == alias) {
                Some(t) => (t.instance.clone(), t.project_id.clone(), Some(t.work_dir.clone())),
                None => {
                    let entry = config.as_ref().and_then(|c| c.project(&alias).ok());
                    let text = |k: &str| {
                        entry
                            .as_ref()
                            .and_then(|e| e.get(k))
                            .map(n_of)
                            .filter(|s| s != "None")
                            .unwrap_or_default()
                    };
                    (text("instance"), text("project_id"), None)
                }
            };
            let loaded = load(ctx, &alias);
            let checkout = managed || work_dir.as_ref().is_some_and(|w| !w.as_os_str().is_empty() && w.is_dir());
            let check = match loaded {
                Loaded::Ready(_) => check_for(work_dir),
                _ => Check { work_dir: None, head: None },
            };
            Project {
                alias,
                instance,
                project_id,
                loaded,
                checkout,
                managed,
                check,
            }
        })
        .collect()
}

/// The aliases of `projects.json` in order, then the other memory directories.
fn alias_list(ctx: &UiCtx, config: Option<&loopx_core::config::Config>) -> Vec<String> {
    let mut aliases: Vec<String> = config.and_then(|c| c.aliases().ok()).unwrap_or_default();
    for alias in memory_store::all_aliases(Some(&memory_root(ctx))) {
        if !alias.starts_with('.') && !aliases.contains(&alias) {
            aliases.push(alias);
        }
    }
    aliases
}

enum Loaded {
    Missing,
    Unreadable(String),
    Ready(Profile),
}

fn load(ctx: &UiCtx, alias: &str) -> Loaded {
    match project_profile::load(alias, Some(&memory_root(ctx))) {
        Ok(None) => Loaded::Missing,
        Ok(Some(p)) => Loaded::Ready(p),
        Err(e) => Loaded::Unreadable(e),
    }
}

/// What a freshness check compares against: the checkout and its HEAD.
struct Check {
    work_dir: Option<PathBuf>,
    head: Option<String>,
}

/// HEAD is read from the git files (`head_sha_fast`), never by spawning git:
/// this runs for every project on every page load.
fn check_for(work_dir: Option<PathBuf>) -> Check {
    let work_dir = work_dir.filter(|w| !w.as_os_str().is_empty() && w.is_dir());
    let head = work_dir.as_deref().and_then(profile_scan::head_sha_fast);
    Check { work_dir, head }
}

/// Worst last, so the worst section of a profile is the `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum State {
    Fresh,
    Human,
    Unchecked,
    Review,
    Stale,
}

fn state_of(p: &Profile, key: SectionKey, check: &Check) -> (State, Vec<String>) {
    let Some(work_dir) = &check.work_dir else {
        return match p.sections.get(&key) {
            None => (State::Review, Vec::new()),
            Some(s) if s.origin == Origin::Human => (State::Human, Vec::new()),
            Some(_) => (State::Unchecked, Vec::new()),
        };
    };
    match profile_sync::section_freshness(p, key, work_dir, check.head.as_deref()) {
        Freshness::Fresh => (State::Fresh, Vec::new()),
        Freshness::Stale(paths) => (State::Stale, paths),
        Freshness::NeedsReview => (State::Review, Vec::new()),
        Freshness::HumanEdited => (State::Human, Vec::new()),
    }
}

fn state_pill(ctx: &UiCtx, state: State) -> String {
    let (class, label) = match state {
        State::Fresh => ("fresh", ctx.t("Fresh")),
        State::Human => ("human", ctx.t("Human-edited")),
        State::Unchecked => ("none", ctx.t("No checkout")),
        State::Review => ("review", ctx.t("Needs review")),
        State::Stale => ("stale", ctx.t("Stale")),
    };
    pill(class, &label)
}

/// A project's freshness verdict as the overview shows it: the worst section
/// state of its profile (with how many sections are fresh or human-edited), or
/// why there is none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Missing,
    Unreadable,
    Ready { worst: State, good: usize },
}

fn verdict(p: &Project) -> Verdict {
    match &p.loaded {
        Loaded::Missing => Verdict::Missing,
        Loaded::Unreadable(_) => Verdict::Unreadable,
        Loaded::Ready(profile) => {
            let states: Vec<State> = SectionKey::ALL
                .into_iter()
                .map(|k| state_of(profile, k, &p.check).0)
                .collect();
            Verdict::Ready {
                worst: states.iter().copied().max().unwrap_or(State::Fresh),
                good: states
                    .iter()
                    .filter(|s| matches!(s, State::Fresh | State::Human))
                    .count(),
            }
        }
    }
}

fn verdict_pill(ctx: &UiCtx, v: Verdict) -> String {
    match v {
        Verdict::Missing => pill("none", &ctx.t("No profile")),
        Verdict::Unreadable => pill("bad", &ctx.t("Unreadable")),
        Verdict::Ready { worst, .. } => state_pill(ctx, worst),
    }
}

/// The graph node's ring class: `mg-v-<this>`.
fn verdict_class(v: Verdict) -> &'static str {
    match v {
        Verdict::Missing => "none",
        Verdict::Unreadable => "bad",
        Verdict::Ready { worst, .. } => match worst {
            State::Fresh => "fresh",
            State::Human => "human",
            State::Unchecked => "unchecked",
            State::Review => "review",
            State::Stale => "stale",
        },
    }
}

fn pill(class: &str, label: &str) -> String {
    format!("<span class='mem-pill mem-pill-{class}'>{}</span>", escape(label))
}

/// The pending (and approved, not yet applied or kept for the record)
/// `profile-section` proposals, newest first. Read once per request.
fn proposals(ctx: &UiCtx) -> Vec<Value> {
    approvals::all(&approvals_dir(ctx))
        .into_iter()
        .filter(|a| a["kind"] == profile_sync::KIND)
        .filter(|a| a["status"] == "pending" || a["status"] == "approved")
        .collect()
}

fn proposal_alias(a: &Value) -> &str {
    a["target"]["alias"]
        .as_str()
        .or_else(|| a["project"].as_str())
        .unwrap_or("")
}

fn pending_for(all: &[Value], alias: &str) -> usize {
    all.iter()
        .filter(|a| a["status"] == "pending" && proposal_alias(a) == alias)
        .count()
}

/// Directly linked projects per alias, from every edge of the graph except the
/// weak shared-people guess (hidden by default in the graph too).
fn neighbors(edges: &[Edge]) -> BTreeMap<String, Vec<(String, EdgeKind)>> {
    let mut out: BTreeMap<String, Vec<(String, EdgeKind)>> = BTreeMap::new();
    for e in edges.iter().filter(|e| e.kind != EdgeKind::SharesPeople) {
        for (a, b) in [(&e.from, &e.to), (&e.to, &e.from)] {
            let list = out.entry(a.clone()).or_default();
            if a != b && !list.iter().any(|(o, k)| o == b && *k == e.kind) {
                list.push((b.clone(), e.kind));
            }
        }
    }
    out
}

fn distinct_neighbors(list: Option<&Vec<(String, EdgeKind)>>) -> usize {
    let mut names: Vec<&String> = list.into_iter().flatten().map(|(n, _)| n).collect();
    names.sort();
    names.dedup();
    names.len()
}

fn sync_store(ctx: &UiCtx) -> Result<SyncStore, String> {
    profile_sync::load(&ctx.state.home_dir.join("profile_sync.json"))
}

// ------------------------------------------------------------------ helpers

/// A project's Memory page (`page`: one of its sub-pages).
pub(crate) fn project_url(alias: &str, page: Option<&str>) -> String {
    let mut url = format!("{BASE}&project={}", quote(alias, ""));
    if let Some(page) = page {
        url.push_str("&page=");
        url.push_str(page);
    }
    url
}

fn day(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d").to_string()
}

fn ago(ctx: &UiCtx, then: DateTime<Utc>) -> String {
    let d = ctx.state.now() - then;
    if d < Duration::minutes(1) {
        ctx.t("just now")
    } else if d < Duration::hours(1) {
        ctx.tf("{n}m ago", &[("n", &d.num_minutes().to_string())])
    } else if d < Duration::days(1) {
        ctx.tf("{n}h ago", &[("n", &d.num_hours().to_string())])
    } else {
        ctx.tf("{n}d ago", &[("n", &d.num_days().to_string())])
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(12).collect()
}

fn section_title(ctx: &UiCtx, key: SectionKey) -> String {
    match key {
        SectionKey::Overview => ctx.t("Overview"),
        SectionKey::Structure => ctx.t("Structure"),
        SectionKey::Features => ctx.t("Features"),
        SectionKey::Stack => ctx.t("Stack & setup"),
        SectionKey::Integrations => ctx.t("Integrations"),
        SectionKey::Secrets => ctx.t("Secrets inventory"),
        SectionKey::Conventions => ctx.t("Conventions"),
        SectionKey::Gotchas => ctx.t("Gotchas"),
        SectionKey::Activity => ctx.t("Recent work"),
        SectionKey::People => ctx.t("People"),
    }
}

fn page_label(ctx: &UiCtx, slug: &str) -> String {
    match slug {
        "overview" => ctx.t("Overview"),
        "structure" => ctx.t("Structure"),
        "features" => ctx.t("Features"),
        "stack" => ctx.t("Stack & setup"),
        "integrations" => ctx.t("Integrations"),
        "secrets" => ctx.t("Secrets"),
        "conventions" => ctx.t("Conventions"),
        "gotchas" => ctx.t("Gotchas"),
        "activity" => ctx.t("Activity"),
        "guide" => ctx.t("Guide"),
        "lessons" => ctx.t("Lessons"),
        _ => ctx.t("Proposals"),
    }
}

fn page_icon(slug: &str) -> &'static str {
    match slug {
        "overview" => "summarize",
        "structure" => "folder",
        "features" => "widgets",
        "stack" => "terminal",
        "integrations" => "link",
        "secrets" => "visibility",
        "conventions" => "tune",
        "gotchas" => "warning",
        "activity" => "history",
        "guide" => "menu_book",
        "lessons" => "lightbulb",
        _ => "fact_check",
    }
}

/// The sub-page a section's **More** link opens.
fn page_of(key: SectionKey) -> &'static str {
    match key {
        SectionKey::People => "activity",
        k => k.slug(),
    }
}

fn sections_of(page: &str) -> Vec<SectionKey> {
    match page {
        "activity" => vec![SectionKey::Activity, SectionKey::People],
        other => SectionKey::from_slug(other).into_iter().collect(),
    }
}

fn edge_label(ctx: &UiCtx, kind: EdgeKind) -> String {
    match kind {
        EdgeKind::DependsOn => ctx.t("depends on"),
        EdgeKind::Calls => ctx.t("calls"),
        EdgeKind::SharesSecret => ctx.t("shares a secret"),
        EdgeKind::SharesService => ctx.t("shares a service"),
        EdgeKind::IssueLink => ctx.t("linked issues"),
        EdgeKind::SharesPeople => ctx.t("shares people"),
        EdgeKind::Declared => ctx.t("declared link"),
    }
}

/// The first secret-looking text among `texts`, by kind only.
fn secret_in(texts: &[&str]) -> Option<String> {
    texts.iter().find_map(|t| contains_secret(t))
}

fn redacted(ctx: &UiCtx, kind: &str) -> String {
    format!(
        "<p class='mem-redacted' role='note'>{}<span>{}</span></p>",
        icon("warning"),
        escape(&ctx.tf(
            "Hidden: this looks like it contains a secret value ({kind}). Only secret names belong in a profile; remove the value from PROFILE.md.",
            &[("kind", kind)]
        ))
    )
}

fn section_texts(s: &Section) -> Vec<&str> {
    let mut texts = vec![s.summary.as_str(), s.body.as_str()];
    texts.extend(s.sources.iter().map(|x| x.path.as_str()));
    texts
}

/// A section's summary, or `None` when it is blank or must not be shown.
fn safe_summary(s: &Section) -> Option<&str> {
    if s.summary.trim().is_empty() || secret_in(&section_texts(s)).is_some() {
        None
    } else {
        Some(s.summary.as_str())
    }
}

fn prefixes(ctx: &UiCtx) -> GitlabPrefixes {
    crate::ui::gitlab::issue_url_prefixes(ctx.state)
}

fn error_card(ctx: &UiCtx, err: &str) -> String {
    let detail = if contains_secret(err).is_some() {
        format!(
            "<p class='mem-muted'>{}</p>",
            escape(&ctx.t("The error quotes text that looks like a secret, so it is not shown."))
        )
    } else {
        format!("<pre class='mem-pre'>{}</pre>", escape(err))
    };
    format!(
        "<section class='card mem-error' role='alert'><div class='section-header'>{}<h2>{}</h2></div>\
         <p class='mem-muted'>{}</p>{detail}</section>",
        icon("error"),
        escape(&ctx.t("This profile could not be read")),
        escape(&ctx.t("The file was left exactly as it is. Fix or remove PROFILE.md, then reload this page."))
    )
}

fn empty_card(message: &str, link: Option<(&str, String)>) -> String {
    let action = link.map_or_else(String::new, |(href, label)| {
        format!(
            "<a class='mem-more' href='{}'>{}{}</a>",
            escape(href),
            escape(&label),
            icon("arrow_forward")
        )
    });
    format!(
        "<section class='card mem-empty'><div class='mem-empty-icon'>{}</div><p>{}</p>{action}</section>",
        icon("folder_off"),
        escape(message)
    )
}

// ------------------------------------------------------------------ level 1

fn overview(ctx: &UiCtx, m: &Model, graph: bool) -> String {
    let projects = &m.projects;
    let title = format!(
        "<div class=\"page-title\">\n<h1>{}</h1>\n<p class=\"subtitle\">{}</p>\n</div>\n",
        escape(&ctx.t("Project Memory")),
        escape(&ctx.t("What LoopX knows about each project: its profile, the lessons from past runs and the updates waiting for review."))
    );
    if projects.is_empty() {
        return format!(
            "{title}<div class='empty-state'>\n<div class='empty-state-icon'>{}</div>\n\
             <p class='empty-state-message'>{}</p>\n\
             <a class='btn btn-primary empty-state-action' href='/loops/issue-loop?view=projects'>\n{} {}\n</a>\n</div>\n",
            icon("folder_off"),
            escape_text(&ctx.t("No projects configured yet, so there is no memory recorded.")),
            icon("settings"),
            escape_text(&ctx.t("Set up a project"))
        );
    }
    let store = sync_store(ctx);
    let bar = format!(
        "<div class='mem-bar'>{}{}<div class='mem-actions'>{}</div></div>",
        segmented(ctx, graph),
        sync_line(ctx, &store),
        overview_controls(ctx, &store)
    );
    let main = if graph {
        graph_view(ctx, m)
    } else {
        let all = &m.proposals;
        let links = links(ctx, m);
        let status = store.as_ref().ok();
        let cards: String = projects
            .iter()
            .map(|p| project_card(ctx, p, all, links, status))
            .collect();
        format!("<div class='mem-grid'>{cards}</div>")
    };
    format!("{title}<div class='mem-stack'>{}{bar}{main}</div>", m.notice)
}

fn segmented(ctx: &UiCtx, graph: bool) -> String {
    let active = " class='is-active' aria-current='page'";
    format!(
        "<nav class='mem-seg' aria-label='{}'><a href='{}'{}>{}</a><a href='{}&amp;graph=1'{}>{}</a></nav>",
        escape(&ctx.t("Memory view")),
        escape(BASE),
        if graph { "" } else { active },
        escape(&ctx.t("Memory")),
        escape(BASE),
        if graph { active } else { "" },
        escape(&ctx.t("Graph"))
    )
}

fn sync_line(ctx: &UiCtx, store: &Result<SyncStore, String>) -> String {
    let parts = match store {
        Err(_) => vec![ctx.t("The sync status file could not be read.")],
        Ok(s) => {
            let last = s.status.last_run.map_or_else(
                || ctx.t("Weekly sync has not run yet"),
                |t| ctx.tf("Last weekly sync {when}", &[("when", &ago(ctx, t))]),
            );
            let next = if s.settings.enabled {
                let at = profile_sync::next_run(&s.settings, ctx.state.now()).with_timezone(&Local);
                ctx.tf("Next run {when}", &[("when", &at.format("%Y-%m-%d %H:%M").to_string())])
            } else {
                ctx.t("Weekly sync is off")
            };
            vec![last, next]
        }
    };
    let mut spans: Vec<String> = parts
        .iter()
        .map(|p| format!("<span>{}</span>", escape(p)))
        .collect();
    if let Some(job) = loopx_core::profile_progress::running(&crate::ui::profile_jobs::base(ctx)).iter().find(|j| j.is_sync()) {
        spans.push(format!(
            "<span class='mem-sync-run' data-mem-job='sync'>{}<span data-mem-step>{}</span></span>",
            spinner(),
            escape(&crate::ui::profile_jobs::progress_text(ctx, job))
        ));
    }
    format!(
        "<div class='mem-sync'>{}{}</div>",
        icon("schedule"),
        spans.join("<span class='mem-dot' aria-hidden='true'>·</span>")
    )
}

fn chip(class: &str, icon_name: &str, label: &str, title: Option<&str>) -> String {
    let title = title.map_or_else(String::new, |t| format!(" title='{}'", escape(t)));
    format!(
        "<span class='mem-chip{class}'{title}>{}{}</span>",
        icon(icon_name),
        escape(label)
    )
}

/// The one-line overview of a project, as its card shows it.
fn overview_line(ctx: &UiCtx, p: &Project) -> String {
    match &p.loaded {
        Loaded::Missing => ctx.t("No profile yet"),
        Loaded::Unreadable(_) => ctx.t("The profile file could not be read."),
        Loaded::Ready(profile) => profile
            .sections
            .get(&SectionKey::Overview)
            .and_then(safe_summary)
            .map_or_else(|| ctx.t("No overview yet"), str::to_string),
    }
}

fn project_card(
    ctx: &UiCtx,
    p: &Project,
    all: &[Value],
    links: &BTreeMap<String, Vec<(String, EdgeKind)>>,
    status: Option<&SyncStore>,
) -> String {
    let total = SectionKey::ALL.len();
    let v = verdict(p);
    let (ring_class, good) = match v {
        Verdict::Missing => ("none", 0),
        Verdict::Unreadable => ("bad", 0),
        Verdict::Ready { worst: State::Stale, good } => ("stale", good),
        Verdict::Ready { worst: State::Review, good } => ("review", good),
        Verdict::Ready { good, .. } => ("fresh", good),
    };
    let state_html = verdict_pill(ctx, v);
    let line = overview_line(ctx, p);
    let draft = matches!(&p.loaded, Loaded::Ready(profile) if profile.status == Status::Draft);
    let pct = good * 100 / total;
    let ring = format!(
        "<span class='mem-ring mem-ring-{ring_class}' style='--p:{pct}' role='img' aria-label='{}'><span>{good}</span></span>",
        escape(&ctx.tf(
            "{n} of {total} sections up to date",
            &[("n", &good.to_string()), ("total", &total.to_string())]
        ))
    );
    let pending = pending_for(all, &p.alias);
    let n_links = distinct_neighbors(links.get(&p.alias));
    let tasks = memory_store::list_task_memories(&p.alias, Some(&memory_root(ctx)));
    let review = tasks
        .iter()
        .filter(|t| t.to_value()["flag"].as_str() == Some("needs_review"))
        .count();
    let mut chips = Vec::new();
    let live = busy(ctx, &p.alias).then(|| live_job(ctx, &p.alias));
    if live.is_some() {
        let working = ctx.t("Working…");
        chips.push(format!(
            "<span class='mem-chip mem-chip-accent'>{}<span data-mem-chip data-mem-base='{}'>{}</span></span>",
            icon("autorenew"),
            escape(&working),
            escape(&working)
        ));
    }
    if draft {
        chips.push(chip(" mem-chip-warn", "edit_note", &ctx.t("Draft"), None));
    }
    if let Some(Outcome::Failed(msg)) = status
        .and_then(|s| s.status.per_project.get(&p.alias))
        .map(|s| &s.outcome)
    {
        let msg = shown_failure(ctx, msg);
        chips.push(chip(" mem-chip-bad", "error", &ctx.t("Last sync failed"), Some(&msg)));
    }
    // Only when something waits; the project page always shows the count.
    if pending > 0 {
        let proposals_label = if pending == 1 {
            ctx.t("1 pending proposal")
        } else {
            ctx.tf("{n} pending proposals", &[("n", &pending.to_string())])
        };
        chips.push(chip(" mem-chip-accent", "fact_check", &proposals_label, None));
    }
    let links_label = if n_links == 1 {
        ctx.t("1 linked project")
    } else {
        ctx.tf("{n} linked projects", &[("n", &n_links.to_string())])
    };
    chips.push(chip("", "hub", &links_label, None));
    if !tasks.is_empty() {
        let lessons = if tasks.len() == 1 {
            ctx.t("1 lesson")
        } else {
            ctx.tf("{n} lessons", &[("n", &tasks.len().to_string())])
        };
        chips.push(chip("", "lightbulb", &lessons, None));
    }
    if review > 0 {
        chips.push(chip(
            " mem-chip-bad",
            "warning",
            &format!("{} ({review})", ctx.t("Needs review")),
            None,
        ));
    }
    let (job_attr, bar) = match &live {
        Some(j) => {
            let step = j.as_ref().map_or_else(|| ctx.t("Starting"), |j| crate::ui::profile_jobs::step_detail(ctx, j));
            let percent = j.as_ref().map_or(0, job_percent);
            (format!(" data-mem-job='{}'", escape(&p.alias)), progress_bar(" mem-progress-card", percent, &step))
        }
        None => (String::new(), String::new()),
    };
    format!(
        "<a class='card mem-card' href='{}'{job_attr}><div class='mem-card-top'>{ring}<div class='mem-card-name'><h3>{}</h3>{state_html}</div></div>\
         <p class='mem-card-overview'>{}</p><div class='mem-chips'>{}</div>{bar}</a>",
        escape(&project_url(&p.alias, None)),
        escape(&p.alias),
        escape(&line),
        chips.concat()
    )
}


// ------------------------------------------------------------------ the graph view

/// Above this many links only the strongest are drawn, with a notice.
const MAX_DRAWN_EDGES: usize = 2000;
/// Above this many projects the nodes carry no text label (the name is still in
/// the node's tooltip, its accessible name and the inspector).
const MAX_LABELLED_NODES: usize = 60;
const VIEW_W: f32 = 1600.0;
const VIEW_H: f32 = 1000.0;
/// Chip order: the strong, deterministic kinds first, the weak guess last.
const KINDS: [EdgeKind; 7] = [
    EdgeKind::DependsOn,
    EdgeKind::Calls,
    EdgeKind::Declared,
    EdgeKind::IssueLink,
    EdgeKind::SharesSecret,
    EdgeKind::SharesService,
    EdgeKind::SharesPeople,
];

fn directed(kind: EdgeKind) -> bool {
    !matches!(
        kind,
        EdgeKind::SharesSecret | EdgeKind::SharesService | EdgeKind::SharesPeople
    )
}

/// `layout_with_edges` places nodes in a 0..1000 square; the view is wider than
/// tall, so x is stretched to use a dashboard-width card.
fn place(n: &profile_graph::Node) -> (f32, f32) {
    (100.0 + n.x * 1.4, 50.0 + n.y * 0.9)
}

fn radius(activity: usize) -> f32 {
    20.0 + activity.min(10) as f32 * 1.6
}

/// A coordinate with at most one decimal and no trailing `.0`.
fn num(v: f32) -> String {
    let r = (v * 10.0).round() / 10.0;
    if r.fract() == 0.0 {
        format!("{}", r as i64)
    } else {
        format!("{r:.1}")
    }
}

fn short_label(alias: &str) -> String {
    const MAX: usize = 18;
    if alias.chars().count() <= MAX {
        alias.to_string()
    } else {
        let mut out: String = alias.chars().take(MAX - 1).collect();
        out.push('…');
        out
    }
}

/// The arrow for an edge seen from one end, with its spoken name.
fn direction(ctx: &UiCtx, e: &Edge, from_here: bool) -> String {
    let (arrow, label) = if !directed(e.kind) {
        ("↔", ctx.t("shared with"))
    } else if from_here {
        ("→", ctx.t("links to"))
    } else {
        ("←", ctx.t("linked from"))
    };
    format!("<span class='mg-dir' role='img' aria-label='{}'>{arrow}</span>", escape(&label))
}

/// The drawn edges: all of them, or the [`MAX_DRAWN_EDGES`] most confident
/// (ties keep the derive order), in derive order.
fn drawn_edges(g: &Graph) -> Vec<&Edge> {
    let mut order: Vec<usize> = (0..g.edges.len()).collect();
    if order.len() > MAX_DRAWN_EDGES {
        order.sort_by(|&a, &b| {
            g.edges[b]
                .confidence
                .partial_cmp(&g.edges[a].confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.cmp(&b))
        });
        order.truncate(MAX_DRAWN_EDGES);
        order.sort_unstable();
    }
    order.into_iter().map(|i| &g.edges[i]).collect()
}

fn hide_classes(hidden: &[EdgeKind]) -> String {
    hidden
        .iter()
        .map(|k| format!(" mg-hide-{}", k.as_str()))
        .collect()
}

fn graph_view(ctx: &UiCtx, m: &Model) -> String {
    use std::fmt::Write as _;
    let g = graph(ctx, m);
    let edges = drawn_edges(g);
    let by_alias: BTreeMap<&str, &Project> = m.projects.iter().map(|p| (p.alias.as_str(), p)).collect();
    // alias -> (index, x, y, r)
    let index: BTreeMap<&str, (usize, f32, f32, f32)> = g
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let (x, y) = place(n);
            (n.alias.as_str(), (i, x, y, radius(n.activity)))
        })
        .collect();
    let hidden = [EdgeKind::SharesPeople];
    let hide = hide_classes(&hidden);

    // --- edges: parallel links between one pair bow apart so each stays visible.
    let mut pairs: BTreeMap<(&str, &str), Vec<usize>> = BTreeMap::new();
    for (i, e) in edges.iter().enumerate() {
        let key = if e.from <= e.to { (e.from.as_str(), e.to.as_str()) } else { (e.to.as_str(), e.from.as_str()) };
        pairs.entry(key).or_default().push(i);
    }
    let mut edge_svg = String::new();
    for ((lo, hi), list) in &pairs {
        let (Some(&(_, lx, ly, _)), Some(&(_, hx, hy, _))) = (index.get(lo), index.get(hi)) else {
            continue;
        };
        let (dx, dy) = (hx - lx, hy - ly);
        let len = (dx * dx + dy * dy).sqrt().max(1e-3);
        let (nx, ny) = (-dy / len, dx / len);
        for (k, &i) in list.iter().enumerate() {
            let e = edges[i];
            let (Some(&(_, x1, y1, r1)), Some(&(_, x2, y2, r2))) = (index.get(e.from.as_str()), index.get(e.to.as_str())) else {
                continue;
            };
            let off = (k as f32 - (list.len() as f32 - 1.0) / 2.0) * 26.0;
            let (cx, cy) = ((x1 + x2) / 2.0 + nx * off * 2.0, (y1 + y2) / 2.0 + ny * off * 2.0);
            let toward = |px: f32, py: f32, qx: f32, qy: f32, d: f32| {
                let (vx, vy) = (qx - px, qy - py);
                let l = (vx * vx + vy * vy).sqrt().max(1e-3);
                (px + vx / l * d, py + vy / l * d)
            };
            let arrow = directed(e.kind);
            let (sx, sy) = toward(x1, y1, cx, cy, r1 + 2.0);
            let (ex, ey) = toward(x2, y2, cx, cy, r2 + if arrow { 4.0 } else { 2.0 });
            let kind = e.kind.as_str();
            let marker = if arrow { format!(" marker-end='url(#mg-a-{kind})'") } else { String::new() };
            let _ = write!(
                edge_svg,
                "<path class='mg-edge mg-k-{kind}' data-edge='{i}' data-from='{}' data-to='{}' data-kind='{kind}' data-ev='{}' data-conf='{:.2}' d='M{} {} Q{} {} {} {}'{marker}><title>{}</title></path>",
                escape(&e.from),
                escape(&e.to),
                escape(&e.evidence),
                e.confidence,
                num(sx),
                num(sy),
                num(cx),
                num(cy),
                num(ex),
                num(ey),
                escape(&format!("{} {} {}: {}", e.from, if arrow { "→" } else { "↔" }, e.to, edge_label(ctx, e.kind)))
            );
        }
    }

    // --- nodes, and one inspector template per node.
    let labelled = g.nodes.len() <= MAX_LABELLED_NODES;
    let total_sections = SectionKey::ALL.len();
    let mut node_svg = String::new();
    let mut templates = String::new();
    for (i, n) in g.nodes.iter().enumerate() {
        let Some(p) = by_alias.get(n.alias.as_str()) else { continue };
        let v = verdict(p);
        let (x, y, r) = index.get(n.alias.as_str()).map(|&(_, x, y, r)| (x, y, r)).unwrap_or((800.0, 500.0, 16.0));
        let good = match v {
            Verdict::Ready { good, .. } => Some(good),
            _ => None,
        };
        let circ = std::f32::consts::TAU * r;
        let arc = circ * good.unwrap_or(0) as f32 / total_sections as f32;
        let mine: Vec<(usize, &Edge)> = edges
            .iter()
            .enumerate()
            .filter(|(_, e)| e.from == n.alias || e.to == n.alias)
            .map(|(i, e)| (i, *e))
            .collect();
        let linked = {
            let mut names: Vec<&str> = mine
                .iter()
                .filter(|(_, e)| e.kind != EdgeKind::SharesPeople)
                .map(|(_, e)| if e.from == n.alias { e.to.as_str() } else { e.from.as_str() })
                .collect();
            names.sort_unstable();
            names.dedup();
            names.len()
        };
        let state_label = match v {
            Verdict::Missing => ctx.t("No profile"),
            Verdict::Unreadable => ctx.t("Unreadable"),
            Verdict::Ready { worst, .. } => match worst {
                State::Fresh => ctx.t("Fresh"),
                State::Human => ctx.t("Human-edited"),
                State::Unchecked => ctx.t("No checkout"),
                State::Review => ctx.t("Needs review"),
                State::Stale => ctx.t("Stale"),
            },
        };
        let aria = ctx.tf(
            "{project}: {state}, linked projects: {n}",
            &[("project", &n.alias), ("state", &state_label), ("n", &linked.to_string())],
        );
        let label = if labelled {
            format!("<text class='mg-label' y='{}'>{}</text>", num(r + 24.0), escape(&short_label(&n.alias)))
        } else {
            String::new()
        };
        let center = good.map_or_else(|| "–".to_string(), |g| g.to_string());
        let _ = write!(
            node_svg,
            "<g class='mg-node mg-v-{}' data-node='{i}' data-alias='{}' data-activity='{}' transform='translate({} {})' tabindex='0' role='button' aria-label='{}'>\
             <title>{}</title><circle class='mg-halo' r='{}'/><circle class='mg-core' r='{}'/><circle class='mg-track' r='{}'/>\
             <circle class='mg-ring' r='{}' stroke-dasharray='{} {}' transform='rotate(-90)'/><text class='mg-count' dy='.35em'>{center}</text>{label}</g>",
            verdict_class(v),
            escape(&n.alias),
            n.activity,
            num(x),
            num(y),
            escape(&aria),
            escape(&n.alias),
            num(r + 7.0),
            num(r),
            num(r),
            num(r),
            num(arc),
            num(circ),
        );
        templates.push_str(&node_template(ctx, m, p, v, i, n.activity, &mine, &index));
    }

    // --- the toolbar chips (one per kind, with its count) and the notices.
    let chips: String = KINDS
        .iter()
        .map(|&k| {
            let count = edges.iter().filter(|e| e.kind == k).count();
            format!(
                "<button type='button' class='mg-chip mg-k-{kind}' data-kind='{kind}' aria-pressed='{}'>\
                 <svg class='mg-swatch' viewBox='0 0 24 8' aria-hidden='true'><line x1='2' y1='4' x2='22' y2='4'/></svg>\
                 <span class='mg-chip-label'>{}</span><span class='mg-chip-count'>{count}</span></button>",
                !hidden.contains(&k),
                escape(&edge_label(ctx, k)),
                kind = k.as_str(),
            )
        })
        .collect();
    let mut notes = String::new();
    if g.edges.len() > edges.len() {
        let _ = write!(
            notes,
            "<p class='mg-note' role='status'>{}{}</p>",
            icon("warning"),
            escape(&ctx.tf(
                "Showing the {shown} strongest of {total} links.",
                &[("shown", &edges.len().to_string()), ("total", &g.edges.len().to_string())]
            ))
        );
    }
    if edges.is_empty() {
        let _ = write!(
            notes,
            "<p class='mg-note'>{}<span><strong>{}</strong> {}</span></p>",
            icon("hub"),
            escape(&ctx.t("No links between projects yet.")),
            escape(&ctx.t("Links appear when profiles or issues show that projects depend on, call or mention each other."))
        );
    }
    let markers: String = KINDS
        .iter()
        .filter(|k| directed(**k))
        .map(|k| {
            format!(
                "<marker id='mg-a-{kind}' viewBox='0 0 10 10' refX='8.5' refY='5' markerWidth='13' markerHeight='13' markerUnits='userSpaceOnUse' orient='auto'>\
                 <path class='mg-arrow mg-k-{kind}' d='M0 1L9 5L0 9z'/></marker>",
                kind = k.as_str()
            )
        })
        .collect();
    let legend: String = [
        ("fresh", ctx.t("Fresh")),
        ("stale", ctx.t("Stale")),
        ("review", ctx.t("Needs review")),
        ("human", ctx.t("Human-edited")),
        ("unchecked", ctx.t("No checkout")),
        ("none", ctx.t("No profile")),
    ]
    .iter()
    .map(|(c, l)| format!("<span class='mg-v-{c}'><i aria-hidden='true'></i>{}</span>", escape(l)))
    .collect();
    let edge_tpl = format!(
        "<template class='mg-tpl-edge'><div class='mg-insp-head'><span class='mg-kind' data-slot='kind'></span></div>\
         <p class='mg-insp-pair'><button type='button' class='mg-goto' data-slot='from'></button><span class='mg-dir' data-slot='dir' aria-hidden='true'></span>\
         <button type='button' class='mg-goto' data-slot='to'></button></p>\
         <h4>{}</h4><p class='mg-insp-ev' data-slot='ev'></p>\
         <dl class='mg-facts'><div><dt>{}</dt><dd data-slot='conf'></dd></div></dl></template>",
        escape(&ctx.t("Evidence")),
        escape(&ctx.t("Confidence"))
    );
    let graph_card = format!(
        "<section class='card mem-graph' data-mg>\
         <div class='mg-head'><div class='mg-title'><div class='section-header'>{}<h2>{}</h2></div>\
         <p class='mem-muted mg-hint' data-mg-js hidden>{}</p>\
         <p class='mem-muted mg-narrow-note'>{}</p></div>\
         <div class='mg-filters' role='group' aria-label='{}' data-mg-js hidden>{chips}</div></div>{notes}\
         <div class='mg-stage' data-wheel-hint='{}'><svg class='mg-svg{hide}' viewBox='0 0 {} {}' preserveAspectRatio='xMidYMid meet' role='group' aria-label='{}'>\
         <defs>{markers}</defs><g class='mg-edges'>{edge_svg}</g><g class='mg-nodes'>{node_svg}</g></svg>\
         <div class='mg-zoom' role='group' aria-label='{}' data-mg-js hidden>\
         <button type='button' data-zoom='in' aria-label='{}' title='{}'>+</button>\
         <button type='button' data-zoom='out' aria-label='{}' title='{}'>−</button>\
         <button type='button' data-zoom='fit' aria-label='{}' title='{}'>{}</button></div>\
         <aside class='mg-inspector{hide}' aria-live='polite' aria-label='{}' tabindex='-1' hidden>\
         <button type='button' class='mg-close' aria-label='{}' title='{}'>{}</button><div class='mg-insp-body'></div></aside></div>\
         <div class='mg-legend' aria-label='{}'><span class='mg-legend-title'>{}</span>{legend}</div>\
         {templates}{edge_tpl}</section>",
        icon("hub"),
        escape(&ctx.t("Project graph")),
        escape(&ctx.t("Drag to move; Ctrl/⌘ + scroll or pinch to zoom. Select a project or a link to see why.")),
        escape(&ctx.t("The graph needs a wider window; the same links are listed below.")),
        escape(&ctx.t("Link types")),
        escape(&ctx.t("Use Ctrl/⌘ + scroll to zoom")),
        num(VIEW_W),
        num(VIEW_H),
        escape(&ctx.t("Project graph")),
        escape(&ctx.t("Zoom")),
        escape(&ctx.t("Zoom in")),
        escape(&ctx.t("Zoom in")),
        escape(&ctx.t("Zoom out")),
        escape(&ctx.t("Zoom out")),
        escape(&ctx.t("Fit the whole graph")),
        escape(&ctx.t("Fit the whole graph")),
        icon("unfold_more"),
        escape(&ctx.t("Details")),
        escape(&ctx.t("Close")),
        escape(&ctx.t("Close")),
        icon("close"),
        escape(&ctx.t("Ring colour")),
        escape(&ctx.t("Ring:")),
    );
    let script = crate::ui::assets::text("memory/graph.js").unwrap_or("");
    format!(
        "{GRAPH_STYLE}<div class='mem-stack mg-wrap'>{graph_card}{}</div><script>{script}</script>",
        graph_list(ctx, m, g, &edges, &hide)
    )
}

#[allow(clippy::too_many_arguments)]
fn node_template(
    ctx: &UiCtx,
    m: &Model,
    p: &Project,
    v: Verdict,
    i: usize,
    activity: usize,
    mine: &[(usize, &Edge)],
    index: &BTreeMap<&str, (usize, f32, f32, f32)>,
) -> String {
    let draft = match &p.loaded {
        Loaded::Ready(profile) if profile.status == Status::Draft => chip(" mem-chip-warn", "edit_note", &ctx.t("Draft"), None),
        _ => String::new(),
    };
    let mut facts = Vec::new();
    if let Verdict::Ready { good, .. } = v {
        facts.push((
            ctx.t("Sections up to date"),
            ctx.tf("{n} of {total}", &[("n", &good.to_string()), ("total", &SectionKey::ALL.len().to_string())]),
        ));
    }
    facts.push((ctx.t("Recent issues"), activity.to_string()));
    facts.push((ctx.t("Pending proposals"), pending_for(&m.proposals, &p.alias).to_string()));
    let facts: String = facts
        .iter()
        .map(|(k, v)| format!("<div><dt>{}</dt><dd>{}</dd></div>", escape(k), escape(v)))
        .collect();
    let rows: String = mine
        .iter()
        .map(|(edge, e)| {
            let from_here = e.from == p.alias;
            let other = if from_here { &e.to } else { &e.from };
            // A neighbour without a node gets its name only, never a jump to a
            // wrong node.
            let target = match index.get(other.as_str()) {
                Some(t) => format!(
                    "<button type='button' class='mg-goto' data-goto='{}'>{}</button>",
                    t.0,
                    escape(other)
                ),
                None => format!("<span class='mg-other'>{}</span>", escape(other)),
            };
            format!(
                "<li class='mg-row mg-row-{kind}'><button type='button' class='mg-kind mg-k-{kind}' data-edge='{edge}'>{}</button>{}\
                 {target}<small>{}</small></li>",
                escape(&edge_label(ctx, e.kind)),
                direction(ctx, e, from_here),
                escape(&e.evidence),
                kind = e.kind.as_str()
            )
        })
        .collect();
    let links = if rows.is_empty() {
        format!("<p class='mem-muted'>{}</p>", escape(&ctx.t("No linked projects yet.")))
    } else {
        format!("<ul class='mg-insp-links'>{rows}</ul>")
    };
    format!(
        "<template class='mg-tpl-node' data-node='{i}'><div class='mg-insp-head'><h3>{}</h3><div class='mg-insp-pills'>{}{draft}</div></div>\
         <p class='mg-insp-sum'>{}</p><dl class='mg-facts'>{facts}</dl><h4>{}</h4>{links}\
         <a class='mem-btn mem-btn-primary mg-open' href='{}'>{}{}</a></template>",
        escape(&p.alias),
        verdict_pill(ctx, v),
        escape(&overview_line(ctx, p)),
        escape(&ctx.t("Linked projects")),
        escape(&project_url(&p.alias, None)),
        escape(&ctx.t("Open full page")),
        icon("arrow_forward")
    )
}

/// The same graph as a list: every project with its links, kinds and evidence.
/// Open without JavaScript and on narrow screens; the script folds it away on
/// wide ones, where the graph is the main view.
fn graph_list(ctx: &UiCtx, m: &Model, g: &Graph, edges: &[&Edge], hide: &str) -> String {
    let items: String = g
        .nodes
        .iter()
        .filter_map(|n| by_alias_of(m, &n.alias))
        .map(|p| {
            let rows: String = edges
                .iter()
                .filter(|e| e.from == p.alias || e.to == p.alias)
                .map(|e| {
                    let from_here = e.from == p.alias;
                    let other = if from_here { &e.to } else { &e.from };
                    format!(
                        "<li class='mg-row mg-row-{kind}'><span class='mg-kind mg-k-{kind}'>{}</span>{}\
                         <a href='{}'>{}</a><small>{}</small></li>",
                        escape(&edge_label(ctx, e.kind)),
                        direction(ctx, e, from_here),
                        escape(&project_url(other, None)),
                        escape(other),
                        escape(&e.evidence),
                        kind = e.kind.as_str()
                    )
                })
                .collect();
            let links = if rows.is_empty() {
                format!("<p class='mem-muted'>{}</p>", escape(&ctx.t("No linked projects yet.")))
            } else {
                format!("<ul class='mg-list-links'>{rows}</ul>")
            };
            format!(
                "<li><div class='mg-list-head'><a href='{}'>{}</a>{}</div>{links}</li>",
                escape(&project_url(&p.alias, None)),
                escape(&p.alias),
                verdict_pill(ctx, verdict(p))
            )
        })
        .collect();
    format!(
        "<section class='card mem-graph-listcard'><details class='mem-graph-list{hide}' open>\
         <summary><span>{}</span><small>{}</small></summary><ul class='mg-list'>{items}</ul></details></section>",
        escape(&ctx.t("List view")),
        escape(&ctx.tf(
            "{projects} projects, {links} links",
            &[("projects", &g.nodes.len().to_string()), ("links", &edges.len().to_string())]
        ))
    )
}

fn by_alias_of<'a>(m: &'a Model, alias: &str) -> Option<&'a Project> {
    m.projects.iter().find(|p| p.alias == alias)
}

// ------------------------------------------------------------------ levels 2 and 3

fn unknown_project(ctx: &UiCtx) -> String {
    format!(
        "{}<div class=\"page-title\">\n<h1>{}</h1>\n</div>\n{}",
        crumbs(ctx, None, None),
        escape(&ctx.t("Project Memory")),
        empty_card(&ctx.t("No project by that name has a profile or memory here."),
            Some((BASE, ctx.t("All projects")))
        )
    )
}

fn unknown_page(ctx: &UiCtx, p: &Project) -> String {
    empty_card(&ctx.t("That page does not exist."),
        Some((&project_url(&p.alias, None), ctx.t("Project summary"))),
    )
}

fn crumbs(ctx: &UiCtx, alias: Option<&str>, page: Option<&str>) -> String {
    let mut parts = vec![format!(
        "<a href='{}'>{}</a>",
        escape(BASE),
        escape(&ctx.t("Project Memory"))
    )];
    if let Some(alias) = alias {
        parts.push(if page.is_some() {
            format!("<a href='{}'>{}</a>", escape(&project_url(alias, None)), escape(alias))
        } else {
            format!("<span>{}</span>", escape(alias))
        });
    }
    if let Some(page) = page {
        parts.push(format!("<span>{}</span>", escape(&page_label(ctx, page))));
    }
    format!(
        "<nav class='mem-crumbs' aria-label='{}'>{}</nav>",
        escape(&ctx.t("Breadcrumb")),
        parts.join("<span class='mem-crumb-sep' aria-hidden='true'>/</span>")
    )
}

fn subnav(ctx: &UiCtx, p: &Project, current: Option<&str>, pending: usize) -> String {
    let link = |href: String, icon_name: &str, label: String, active: bool, badge: String| {
        format!(
            "<a href='{}'{}>{}<span>{}</span>{badge}</a>",
            escape(&href),
            if active { " class='is-active' aria-current='page'" } else { "" },
            icon(icon_name),
            escape(&label)
        )
    };
    let mut out = vec![link(
        project_url(&p.alias, None),
        "space_dashboard",
        ctx.t("Summary"),
        current.is_none(),
        String::new(),
    )];
    for slug in PAGES {
        if slug == "lessons" {
            out.push("<span class='mem-subnav-sep' aria-hidden='true'></span>".to_string());
        }
        let badge = if slug == "proposals" && pending > 0 {
            format!("<span class='mem-subnav-badge'>{pending}</span>")
        } else {
            String::new()
        };
        out.push(link(
            project_url(&p.alias, Some(slug)),
            page_icon(slug),
            page_label(ctx, slug),
            current == Some(slug),
            badge,
        ));
    }
    format!(
        "<nav class='card mem-subnav' aria-label='{}'>{}</nav>",
        escape(&ctx.t("Project pages")),
        out.concat()
    )
}

/// The project frame: breadcrumb, title, the sub-nav and `content` beside it.
fn frame(ctx: &UiCtx, p: &Project, page: Option<&str>, content: &str, m: &Model) -> String {
    let pending = pending_for(&m.proposals, &p.alias);
    let subtitle = [p.instance.as_str(), p.project_id.as_str()]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" · ");
    let subtitle = if subtitle.is_empty() {
        String::new()
    } else {
        format!("\n<p class=\"subtitle\">{}</p>", escape(&subtitle))
    };
    format!(
        "{}<div class=\"page-title\">\n<h1>{}</h1>{subtitle}\n</div>\n<div class='mem-layout'>{}<div class='mem-content'>{}{}{content}</div></div>",
        crumbs(ctx, Some(&p.alias), page),
        escape(&p.alias),
        subnav(ctx, p, page, pending),
        m.notice,
        job_line(ctx, &p.alias)
    )
}

fn head_line(ctx: &UiCtx, profile: &Profile, check: &Check) -> String {
    let short = |s: &str| s.chars().take(8).collect::<String>();
    if check.work_dir.is_none() {
        return ctx.t("No checkout to compare with");
    }
    let Some(now) = check.head.as_deref() else {
        return ctx.t("Could not read the checkout's HEAD");
    };
    match profile.head_sha.as_deref() {
        None => ctx.tf("Checkout at {sha}", &[("sha", &short(now))]),
        Some(then) if then == now => ctx.tf("Up to date with HEAD {sha}", &[("sha", &short(now))]),
        Some(then) => ctx.tf(
            "HEAD moved since the profile was written ({old} → {new})",
            &[("old", &short(then)), ("new", &short(now))],
        ),
    }
}

fn project_page(ctx: &UiCtx, p: &Project, m: &Model) -> String {
    let content = match &p.loaded {
        Loaded::Unreadable(e) => error_card(ctx, e),
        Loaded::Missing => {
            let head = format!("{}{}", head_card(ctx, p, None), wiki_card(ctx, p));
            format!(
                "{head}{}",
                empty_card(&ctx.t("No profile has been built for this project yet. The weekly sync drafts one for every tracked project."),
                    Some((&project_url(&p.alias, Some("lessons")), ctx.t("See its lessons")))
                )
            )
        }
        Loaded::Ready(profile) => {
            let check = &p.check;
            let banner = if profile.status == Status::Draft {
                format!(
                    "<div class='mem-banner' role='status'>{}<p><strong>{}</strong> {}</p>\
                     <form method='post' action='/insights/memory/approve' class='mem-inline mem-banner-action'>{}{}\
                     <button class='mem-btn mem-btn-primary' type='submit'>{}{}</button></form></div>",
                    icon("edit_note"),
                    escape(&ctx.t("This profile is a draft.")),
                    escape(&ctx.t("Read each section before agents rely on it.")),
                    ctx.csrf_input(),
                    hidden("project", &p.alias),
                    icon("check_circle"),
                    escape(&ctx.t("Approve profile"))
                )
            } else {
                String::new()
            };
            let links = links(ctx, m);
            let pending = pending_for(&m.proposals, &p.alias);
            let cards: String = SectionKey::ALL
                .into_iter()
                .map(|k| summary_card(ctx, p, profile, k, check))
                .collect();
            let head = format!("{}{}", head_card(ctx, p, Some((profile, check))), wiki_card(ctx, p));
            format!(
                "{banner}{head}<div class='mem-pair'>{}{}</div><div class='mem-sections'>{cards}</div>",
                neighbors_card(ctx, links.get(&p.alias)),
                proposals_summary(ctx, p, pending)
            )
        }
    };
    frame(ctx, p, None, &content, m)
}

fn head_card(ctx: &UiCtx, p: &Project, profile: Option<(&Profile, &Check)>) -> String {
    let dash = "—".to_string();
    let mut rows = vec![(
        ctx.t("Instance"),
        if p.instance.is_empty() { dash.clone() } else { p.instance.clone() },
    )];
    if let Some((profile, check)) = profile {
        rows.push((
            ctx.t("Last reconciled"),
            profile.generated_at.map_or_else(|| ctx.t("Never"), day),
        ));
        rows.push((ctx.t("HEAD"), head_line(ctx, profile, check)));
    }
    if let Ok(store) = sync_store(ctx) {
        if let Some(s) = store.status.per_project.get(&p.alias) {
            let text = match &s.outcome {
                Outcome::Failed(msg) => ctx.tf(
                    "Last sync failed {when}: {error}",
                    &[("when", &ago(ctx, s.at)), ("error", &shown_failure(ctx, msg))],
                ),
                Outcome::Skipped => ctx.tf("Checked {when}, nothing to update", &[("when", &ago(ctx, s.at))]),
                Outcome::Drafted => ctx.tf("Drafted {when}", &[("when", &ago(ctx, s.at))]),
                Outcome::Proposed(1) => ctx.tf("Proposed 1 update {when}", &[("when", &ago(ctx, s.at))]),
                Outcome::Proposed(n) => ctx.tf(
                    "Proposed {n} updates {when}",
                    &[("n", &n.to_string()), ("when", &ago(ctx, s.at))],
                ),
            };
            rows.push((ctx.t("Weekly sync"), text));
        }
    }
    let dl: String = rows
        .iter()
        .map(|(k, v)| format!("<dt>{}</dt><dd>{}</dd>", escape(k), escape(v)))
        .collect();
    format!(
        "<section class='card mem-head'><dl>{dl}</dl><div class='mem-actions'>{}</div></section>",
        head_actions(ctx, p, profile.map(|(p, _)| p))
    )
}

fn summary_card(ctx: &UiCtx, p: &Project, profile: &Profile, key: SectionKey, check: &Check) -> String {
    let title = section_title(ctx, key);
    let (state, _) = state_of(profile, key, check);
    let summary = match profile.sections.get(&key) {
        None => format!("<p class='mem-summary mem-summary-empty'>{}</p>", escape(&ctx.t("Not written yet"))),
        Some(s) => match secret_in(&section_texts(s)) {
            Some(_) => format!(
                "<p class='mem-summary mem-summary-empty'>{}</p>",
                escape(&ctx.t("Hidden: looks like it contains a secret value."))
            ),
            None => match safe_summary(s) {
                Some(text) => format!("<p class='mem-summary'>{}</p>", escape(text)),
                None => format!("<p class='mem-summary mem-summary-empty'>{}</p>", escape(&ctx.t("No summary"))),
            },
        },
    };
    format!(
        "<article class='card mem-section'><div class='mem-section-head'><h3>{}</h3>{}</div>{summary}\
         <a class='mem-more' href='{}' aria-label='{}'>{}{}</a></article>",
        escape(&title),
        state_pill(ctx, state),
        escape(&project_url(&p.alias, Some(page_of(key)))),
        escape(&ctx.tf("More about {section}", &[("section", &title)])),
        escape(&ctx.t("More")),
        icon("arrow_forward")
    )
}

/// The wiki pages saved for a project and the form that adds one from a link.
fn wiki_card(ctx: &UiCtx, p: &Project) -> String {
    let csrf = ctx.csrf_input();
    let ids: String = p.alias.bytes().filter(u8::is_ascii_alphanumeric).map(char::from).collect();
    let pages = loopx_core::profile_wiki::list(&p.alias, Some(&memory_root(ctx)));
    let list = if pages.is_empty() {
        format!("<p class='mem-muted'>{}</p>", escape(&ctx.t("No wiki pages added yet.")))
    } else {
        let rows: String = pages
            .iter()
            .map(|w| {
                format!(
                    "<li><a href='{}' target='_blank' rel='noopener noreferrer'>{}</a>\
                     <span class='mem-muted'>{}</span>\
                     <form method='post' action='/insights/memory/wiki/remove' class='mem-inline'>{csrf}{}{}\
                     <button class='mem-btn' type='submit' title='{}'>{}</button></form></li>",
                    escape(&w.url),
                    escape(&w.title),
                    escape(&ago(ctx, w.fetched_at)),
                    hidden("project", &p.alias),
                    hidden("page", &w.id),
                    escape(&ctx.t("Forget this page; the profile keeps what it already says")),
                    escape(&ctx.t("Remove"))
                )
            })
            .collect();
        format!("<ul class='mem-wiki-list'>{rows}</ul>")
    };
    format!(
        "<section class='card mem-wiki'><div class='section-header'>{}<h2>{}</h2></div>{list}\
         <form method='post' action='/insights/memory/wiki/add' class='mem-editor'>{csrf}{}\
         <label class='mem-label' for='mem-wiki-{ids}'>{}</label>\
         <input class='mem-input' id='mem-wiki-{ids}' name='url' type='url' maxlength='500' required \
          placeholder='https://gitlab.example.com/group/project/-/wikis/Page'>\
         <div class='mem-editor-foot'><p class='mem-muted'>{}</p>\
         <button class='mem-btn mem-btn-primary' type='submit'>{}{}</button></div></form></section>",
        icon("link"),
        escape(&ctx.t("Wiki pages")),
        hidden("project", &p.alias),
        escape(&ctx.t("Wiki page link")),
        escape(&ctx.t("Paste the link of one wiki page. It is read through GitLab, kept as a short note, and the profile sections it can change are proposed for your approval. The weekly sync checks it again.")),
        icon("auto_awesome"),
        escape(&ctx.t("Add wiki page"))
    )
}

fn neighbors_card(ctx: &UiCtx, list: Option<&Vec<(String, EdgeKind)>>) -> String {
    let items: String = list
        .into_iter()
        .flatten()
        .map(|(other, kind)| {
            format!(
                "<a class='mem-link' href='{}'>{}<span>{}</span><small>{}</small></a>",
                escape(&project_url(other, None)),
                icon("hub"),
                escape(other),
                escape(&edge_label(ctx, *kind))
            )
        })
        .collect();
    let body = if items.is_empty() {
        format!("<p class='mem-muted'>{}</p>", escape(&ctx.t("No linked projects yet.")))
    } else {
        format!("<div class='mem-links'>{items}</div>")
    };
    format!(
        "<section class='card mem-neighbors'><div class='section-header'>{}<h2>{}</h2></div>{body}</section>",
        icon("hub"),
        escape(&ctx.t("Linked projects"))
    )
}

fn proposals_summary(ctx: &UiCtx, p: &Project, pending: usize) -> String {
    let note = if pending == 0 {
        ctx.t("Nothing is waiting for review.")
    } else {
        ctx.t("Updates to this profile wait here until someone reviews them.")
    };
    format!(
        "<section class='card mem-proposals-card'><div class='section-header'>{}<h2>{}</h2></div>\
         <p class='mem-count'>{pending}</p><p class='mem-muted'>{}</p>\
         <a class='mem-more' href='{}'>{}{}</a></section>",
        icon("fact_check"),
        escape(&ctx.t("Pending proposals")),
        escape(&note),
        escape(&project_url(&p.alias, Some("proposals"))),
        escape(&ctx.t("Open proposals")),
        icon("arrow_forward")
    )
}

fn sub_page(ctx: &UiCtx, p: &Project, slug: &str, m: &Model) -> String {
    let content = match slug {
        "lessons" => lessons_card(ctx, p),
        "proposals" => proposals_card(ctx, p, &m.proposals),
        "guide" => guide::render(ctx, p, m),
        _ => match &p.loaded {
            Loaded::Unreadable(e) => error_card(ctx, e),
            Loaded::Missing => empty_card(&ctx.t("This project has no profile yet, so this section has not been written."),
                Some((&project_url(&p.alias, None), ctx.t("Project summary"))),
            ),
            Loaded::Ready(profile) => {
                let mut out: String = sections_of(slug)
                    .into_iter()
                    .map(|k| detail_card(ctx, p, profile, k))
                    .collect();
                if slug == "stack" {
                    out.push_str(&setup_card(ctx, p));
                }
                out
            }
        },
    };
    frame(ctx, p, Some(slug), &content, m)
}

fn detail_card(ctx: &UiCtx, p: &Project, profile: &Profile, key: SectionKey) -> String {
    let check = &p.check;
    let title = section_title(ctx, key);
    let (state, stale) = state_of(profile, key, check);
    let head = format!(
        "<div class='mem-detail-head'><h2>{}</h2>{}</div>",
        escape(&title),
        state_pill(ctx, state)
    );
    let Some(s) = profile.sections.get(&key) else {
        return format!(
            "<section class='card mem-detail'>{head}<div class='mem-actions'>{}</div><p class='mem-muted'>{}</p></section>",
            section_actions(ctx, p, key, None),
            escape(&ctx.t("Not written yet"))
        );
    };
    let mut meta = vec![s.last_verified.map_or_else(
        || ctx.t("Never verified"),
        |t| ctx.tf("Last verified {date}", &[("date", &day(t))]),
    )];
    meta.push(match s.origin {
        Origin::Human => ctx.t("Written by a person"),
        Origin::Agent => ctx.t("Written by the agent"),
    });
    let meta = format!("<p class='mem-meta'>{}</p>", escape(&meta.join(" · ")));
    if let Some(kind) = secret_in(&section_texts(s)) {
        return format!(
            "<section class='card mem-detail'>{head}{meta}<div class='mem-actions'>{}</div>{}</section>",
            section_actions(ctx, p, key, None),
            redacted(ctx, &kind)
        );
    }
    let stale_html = if stale.is_empty() {
        String::new()
    } else {
        let n = stale.len().to_string();
        let label = match (s.last_verified, stale.len()) {
            (Some(t), 1) => ctx.tf("1 source changed since {date}", &[("date", &day(t))]),
            (Some(t), _) => ctx.tf("{n} sources changed since {date}", &[("n", &n), ("date", &day(t))]),
            (None, 1) => ctx.t("1 source changed"),
            (None, _) => ctx.tf("{n} sources changed", &[("n", &n)]),
        };
        let paths: Vec<String> = stale
            .iter()
            .map(|p| format!("<code>{}</code>", escape(p)))
            .collect();
        format!(
            "<p class='mem-stale'>{}<span>{}: {}</span></p>",
            icon("warning"),
            escape(&label),
            paths.join(", ")
        )
    };
    let lede = if s.summary.trim().is_empty() {
        String::new()
    } else {
        format!("<p class='mem-lede'>{}</p>", escape(&s.summary))
    };
    let body = if s.body.trim().is_empty() {
        format!("<p class='mem-muted'>{}</p>", escape(&ctx.t("No details written yet.")))
    } else {
        format!(
            "<div class='markdown mem-body'>{}</div>",
            markdown::render_markdown(&s.body, &prefixes(ctx))
        )
    };
    let sources = if s.sources.is_empty() {
        format!(
            "<p class='mem-sources mem-muted'>{}</p>",
            escape(&ctx.t("No sources recorded."))
        )
    } else {
        let items: String = s
            .sources
            .iter()
            .map(|src| {
                let changed = if stale.contains(&src.path) {
                    pill("stale", &ctx.t("Changed"))
                } else {
                    String::new()
                };
                format!(
                    "<li><code>{}</code><span class='mem-sha' title='sha256:{}'>{}</span>{changed}</li>",
                    escape(&src.path),
                    escape(&src.sha256),
                    escape(&short_sha(&src.sha256))
                )
            })
            .collect();
        format!(
            "<details class='mem-sources' open><summary>{}</summary><ul>{items}</ul></details>",
            escape(&ctx.tf("Sources ({n})", &[("n", &s.sources.len().to_string())]))
        )
    };
    format!(
        "<section class='card mem-detail'>{head}{meta}<div class='mem-actions'>{}</div>{stale_html}{lede}{body}{sources}</section>",
        section_actions(ctx, p, key, Some(s))
    )
}

fn setup_card(ctx: &UiCtx, p: &Project) -> String {
    let body = match memory_store::read_setup_knowledge(&p.alias, Some(&memory_root(ctx))) {
        None => format!("<p class='mem-muted'>{}</p>", escape(&ctx.t("No setup notes recorded yet."))),
        Some(text) => match contains_secret(&text) {
            Some(kind) => redacted(ctx, &kind),
            None => format!(
                "<div class='markdown mem-body'>{}</div>",
                markdown::render_markdown(&text, &prefixes(ctx))
            ),
        },
    };
    format!(
        "<section class='card mem-detail'><div class='section-header'>{}<h2>{}</h2><span class='mem-file'>SETUP.md</span></div>{body}</section>",
        icon("description"),
        escape(&ctx.t("Setup notes"))
    )
}

fn proposals_card(ctx: &UiCtx, p: &Project, all: &[Value]) -> String {
    let mut items: Vec<&Value> = all
        .iter()
        .filter(|a| proposal_alias(a) == p.alias)
        .collect();
    // Pending first; each group stays newest first.
    items.sort_by_key(|a| a["status"] != "pending");
    let profile = match &p.loaded {
        Loaded::Ready(profile) => Some(profile),
        _ => None,
    };
    let list: String = items.iter().map(|a| proposal_item(ctx, &p.alias, a, profile)).collect();
    let body = if list.is_empty() {
        format!("<p class='mem-muted'>{}</p>", escape(&ctx.t("No proposals for this project.")))
    } else {
        format!("<div class='mem-proposals'>{list}</div>")
    };
    format!(
        "<section class='card'><div class='section-header'>{}<h2>{}</h2></div><p class='mem-meta'>{}</p>{body}</section>",
        icon("fact_check"),
        escape(&ctx.t("Proposals")),
        escape(&ctx.t("Proposed updates to this profile. Nothing changes until a proposal is approved."))
    )
}

fn proposal_item(ctx: &UiCtx, alias: &str, a: &Value, profile: Option<&Profile>) -> String {
    let t = &a["target"];
    let text = |k: &str| t[k].as_str().unwrap_or("").to_string();
    let slug = text("section");
    let title = SectionKey::from_slug(&slug).map_or_else(|| slug.clone(), |k| section_title(ctx, k));
    let id = a["id"].as_str().unwrap_or("");
    let key = proposal_key(a, alias);
    let unapplied = a["status"] == "approved" && !profile_sync::is_applied(alias, id, Some(&memory_root(ctx)));
    let status = if a["status"] == "pending" {
        pill("review", &ctx.t("Pending"))
    } else if unapplied {
        pill("stale", &ctx.t("Approved, not applied"))
    } else {
        pill("fresh", &ctx.t("Approved"))
    };
    // Why an approved proposal is not in the profile, and what to do about it.
    let note = match (unapplied, key) {
        (true, Some(key)) if profile.is_some_and(|p| still_applies(p, key, a)) => format!(
            "<p class='mem-muted mem-unapplied'>{}</p>",
            escape(&ctx.t("Approved, but not applied yet. Apply it to write it into the profile."))
        ),
        (true, Some(key)) => format!(
            "<p class='mem-muted mem-unapplied'>{} <a href='{}'>{}</a></p>",
            escape(&ctx.t("The section changed after this was approved, so it was not applied.")),
            escape(&project_url(alias, Some(page_of(key)))),
            escape(&ctx.t("Refresh this section"))
        ),
        _ => String::new(),
    };
    let created = a["created_at"].as_str().and_then(|s| s.get(..10)).unwrap_or("");
    let when = if created.is_empty() {
        String::new()
    } else {
        format!(
            "<span class='mem-muted'>{}</span>",
            escape(&ctx.tf("Proposed {date}", &[("date", created)]))
        )
    };
    let (old, new, old_sum, new_sum) = (text("old"), text("new"), text("old_summary"), text("new_summary"));
    let diff = match secret_in(&[&old, &new, &old_sum, &new_sum]) {
        Some(kind) => redacted(ctx, &kind),
        None => {
            let col = |class: &str, label: String, summary: &str, body: &str| {
                let summary = if summary.trim().is_empty() {
                    String::new()
                } else {
                    format!("<p class='mem-diff-sum'>{}</p>", escape(summary))
                };
                let body = if body.trim().is_empty() {
                    format!("<p class='mem-muted'>{}</p>", escape(&ctx.t("(empty)")))
                } else {
                    format!("<pre class='mem-pre'>{}</pre>", escape(body))
                };
                format!(
                    "<div class='mem-diff-col mem-diff-{class}'><h4>{}</h4>{summary}{body}</div>",
                    escape(&label)
                )
            };
            format!(
                "<div class='mem-diff'>{}{}</div>",
                col("old", ctx.t("Current"), &old_sum, &old),
                col("new", ctx.t("Proposed"), &new_sum, &new)
            )
        }
    };
    format!(
        "<article class='mem-proposal'><div class='mem-proposal-head'><h3>{}</h3>{status}{when}</div>{diff}{note}<div class='mem-actions'>{}</div></article>",
        escape(&title),
        proposal_actions(ctx, alias, a, profile)
    )
}

// ------------------------------------------------------------------ lessons
// The per-issue task memories (and the legacy `fix_learnings` of the GitLab
// cache), exactly as the old Insights → Memory view showed them, for one project.

fn legacy_learnings(ctx: &UiCtx, instance: &str, project_id: &str) -> Vec<Value> {
    if project_id.is_empty() {
        return Vec::new();
    }
    let path = ctx
        .state
        .user_home
        .join(".gitlab")
        .join("cache")
        .join(instance)
        .join("projects")
        .join(project_id.replace('/', "__"))
        .join("project.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("_memory")?.get("fix_learnings")?.as_array().cloned())
        .unwrap_or_default()
}

fn issue_pill(prefixes: &GitlabPrefixes, alias: &str, iid: &Value) -> String {
    let safe = escape(&n_of(iid));
    match prefixes.get(alias).filter(|b| !b.is_empty()) {
        Some(base) => format!(
            "<a class='pill pill-link' href='{base}/-/issues/{}' rel='noopener' target='_blank'>\
             <svg class='icon' aria-hidden='true'><use href='#i-open_in_new'/></svg>#{safe}</a>",
            n_of(iid)
        ),
        None => format!("<span class='pill pill-grey'>#{safe}</span>"),
    }
}

fn tag_pills(tags: &Value) -> String {
    tags.as_array()
        .map(|a| {
            a.iter()
                .map(|t| format!("<span class='pill pill-grey'>{}</span>", escape(&n_of(t))))
                .collect()
        })
        .unwrap_or_default()
}

fn lessons_card(ctx: &UiCtx, p: &Project) -> String {
    let st = ctx.state;
    let alias = p.alias.as_str();
    let legacy = legacy_learnings(ctx, &p.instance, &p.project_id);
    let tasks: Vec<Value> = memory_store::list_task_memories(alias, Some(&memory_root(ctx)))
        .iter()
        .map(memory_store::TaskMemory::to_value)
        .collect();
    let report = learning::build_learning_report(Some(&st.events_dir), None, None, None, None);
    let stats: std::collections::HashMap<String, Value> = report
        .lessons
        .iter()
        .filter_map(|l| Some((l.lesson_id.as_str()?.to_string(), l.to_value())))
        .collect();
    let prefixes = prefixes(ctx);

    let reuse_stats = |lesson_id: &Value| -> String {
        if !truthy(lesson_id) {
            return String::new();
        }
        let s = lesson_id.as_str().and_then(|id| stats.get(id));
        match s.filter(|s| s["times_reused"].as_i64().unwrap_or(0) != 0) {
            None => format!(
                "<p class='learning-reuse-stats'>{}</p>",
                escape(&ctx.t("Not yet reused"))
            ),
            Some(s) => {
                let eff = f_of(&s["effectiveness_rate"])
                    .map_or_else(|| ctx.t("pending"), |e| format!("{}%", fixed(e * 100.0, 0)));
                let text = ctx.tf(
                    "Reused {times}\u{d7} \u{b7} {successful} successful, {failed} failed (effectiveness: {effectiveness})",
                    &[
                        ("times", &n_of(&s["times_reused"])),
                        ("successful", &n_of(&s["successful_reuses"])),
                        ("failed", &n_of(&s["failed_reuses"])),
                        ("effectiveness", &eff),
                    ],
                );
                format!("<p class='learning-reuse-stats'>{}</p>", escape_text(&text))
            }
        }
    };

    let task_item = |e: &Value| -> String {
        let description = &e["description"];
        let body = e["body"].as_str().unwrap_or("");
        // Lessons are agent-written text too: the same secret guard as the profile.
        let secret = secret_in(&[&n_of(description), body, &n_of(&e["tags"])]);
        let tags = if secret.is_some() { Value::Null } else { e["tags"].clone() };
        let category = if truthy(&e["category"]) {
            format!("<span class='pill pill-grey'>{}</span>", escape(&n_of(&e["category"])))
        } else {
            String::new()
        };
        let meta = format!(
            "<div class='pill-row'>{}{category}{}</div>",
            issue_pill(&prefixes, alias, &e["issue_iid"]),
            tag_pills(&tags)
        );
        let description_html = if secret.is_some() || !truthy(description) {
            String::new()
        } else {
            format!("<p class='history-entry-overview'>{}</p>", escape(&n_of(description)))
        };
        let body_html = match &secret {
            Some(kind) => redacted(ctx, kind),
            None => format!("<div class='markdown'>{}</div>", markdown::render_markdown(body, &prefixes)),
        };
        let needs_review = e["flag"].as_str() == Some("needs_review");
        let badge = if needs_review {
            format!(
                "<span class='pill pill-red' title='{}'><svg class='icon' aria-hidden='true'><use href='#i-warning'/></svg>{}</span>",
                escape(&ctx.t("Reuse of this lesson preceded repeated failures; check it is still correct.")),
                escape(&ctx.t("Needs review"))
            )
        } else {
            String::new()
        };
        let data_attr = if needs_review { " data-needs-review='1'" } else { "" };
        format!(
            "<li class='learning-item'{data_attr}>{badge}{description_html}{body_html}{meta}{}</li>",
            reuse_stats(&e["lesson_id"])
        )
    };
    let legacy_item = |e: &Value| -> String {
        let pill = match e.get("issue_iid") {
            Some(iid) if !iid.is_null() => issue_pill(&prefixes, alias, iid),
            _ => String::new(),
        };
        let lesson = e["lesson"].as_str().unwrap_or("");
        let secret = secret_in(&[lesson, &n_of(&e["tags"])]);
        let tags = if secret.is_some() { Value::Null } else { e["tags"].clone() };
        let meta = format!("{pill}{}", tag_pills(&tags));
        let meta_html = if meta.is_empty() {
            String::new()
        } else {
            format!("<div class='pill-row'>{meta}</div>")
        };
        let body_html = match &secret {
            Some(kind) => redacted(ctx, kind),
            None => format!("<div class='markdown'>{}</div>", markdown::render_markdown(lesson, &prefixes)),
        };
        format!("<li class='learning-item'>{body_html}{meta_html}</li>")
    };

    let needs_review_count = tasks
        .iter()
        .filter(|e| e["flag"].as_str() == Some("needs_review"))
        .count();
    let tasks_html = if tasks.is_empty() {
        format!("<p>{}</p>", escape(&ctx.t("(no task memory recorded yet)")))
    } else {
        format!(
            "<ul class='plain'>{}</ul>",
            tasks.iter().map(task_item).collect::<String>()
        )
    };
    let legacy_html = if legacy.is_empty() {
        String::new()
    } else {
        format!(
            "<h4>{}</h4><ul class='plain'>{}</ul>",
            escape(&ctx.t("Legacy learnings")),
            legacy.iter().map(legacy_item).collect::<String>()
        )
    };
    let style = "\n<style>\n\
#memory-filter-needs-review:checked ~ .memory-list li.learning-item:not([data-needs-review]) { display: none; }\n\
#memory-filter-needs-review { position: absolute; opacity: 0; pointer-events: none; }\n\
#memory-filter-needs-review:checked + label.pill { outline: 2px solid var(--md-error, currentColor); }\n\
#memory-filter-needs-review:focus-visible + label.pill { outline: 2px solid currentColor; }\n\
label.memory-filter-chip { cursor: pointer; }\n\
</style>\n";
    format!(
        "{style}<section class=\"card\">\n<div class=\"section-header\">{}<h2>{}</h2></div>\n\
         <p class='mem-meta'>{}</p>\n\
         <input type=\"checkbox\" id=\"memory-filter-needs-review\">\n\
         <label class=\"pill pill-red memory-filter-chip\" for=\"memory-filter-needs-review\">{} ({needs_review_count})</label>\n\
         <div class=\"memory-list\">\n<div class='project-block'>{tasks_html}{legacy_html}</div>\n</div>\n</section>\n",
        ds_icon("_SECTION_ICON_MEMORY"),
        escape(&ctx.t("Lessons")),
        escape(&ctx.t("Task memory recorded per project by the automated review loop.")),
        escape(&ctx.t("Needs review"))
    )
}

// ------------------------------------------------------------------ actions
// The POST routes. Each answers with a 303 back to the page it belongs to, built
// only from checked values, plus one `notice` code from [`NOTICES`].

/// A summary is one line; a body is a page of Markdown, not a dump.
const SUMMARY_MAX: usize = 300;
const BODY_MAX: usize = 20_000;

const WEEKDAYS: [(&str, chrono::Weekday); 7] = [
    ("mon", chrono::Weekday::Mon),
    ("tue", chrono::Weekday::Tue),
    ("wed", chrono::Weekday::Wed),
    ("thu", chrono::Weekday::Thu),
    ("fri", chrono::Weekday::Fri),
    ("sat", chrono::Weekday::Sat),
    ("sun", chrono::Weekday::Sun),
];

/// What a redirect may tell the page. Only these codes are ever read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Notice {
    BuildStarted,
    BuildAllStarted,
    Busy,
    NoCheckout,
    NothingToBuild,
    NoAgent,
    Saved,
    Secret,
    TooLong,
    NoProfile,
    Unreadable,
    SaveFailed,
    BadInput,
    RefreshStarted,
    Approved,
    Dismissed,
    Changed,
    AlreadyDecided,
    ApplyFailed,
    SyncOn,
    SyncOff,
    ScheduleSaved,
    SyncRequested,
    StoreBad,
    ProfileApproved,
    ApproveSecret,
    SlackOn,
    SlackOff,
    CloneOn,
    CloneOff,
    Recloned,
    WikiAdded,
    WikiUnchanged,
    WikiRemoved,
    WikiBadLink,
    WikiOtherProject,
    WikiSecret,
    WikiFailed,
    GuideBuildStarted,
    GuideBusy,
    GuideApproved,
    GuideAllApproved,
    GuideEdited,
    GuideDeleted,
    GuideRestored,
    GuideInPlan,
    GuideFailed,
    GuideTooLong,
    GuideSecretElsewhere,
}

const NOTICES: [(Notice, &str); 49] = [
    (Notice::BuildStarted, "build-started"),
    (Notice::BuildAllStarted, "build-all-started"),
    (Notice::Busy, "busy"),
    (Notice::NoCheckout, "no-checkout"),
    (Notice::NothingToBuild, "nothing-to-build"),
    (Notice::NoAgent, "no-agent"),
    (Notice::Saved, "saved"),
    (Notice::Secret, "secret"),
    (Notice::TooLong, "too-long"),
    (Notice::NoProfile, "no-profile"),
    (Notice::Unreadable, "unreadable"),
    (Notice::SaveFailed, "save-failed"),
    (Notice::BadInput, "bad-input"),
    (Notice::RefreshStarted, "refresh-started"),
    (Notice::Approved, "approved"),
    (Notice::Dismissed, "dismissed"),
    (Notice::Changed, "changed"),
    (Notice::AlreadyDecided, "already-decided"),
    (Notice::ApplyFailed, "apply-failed"),
    (Notice::SyncOn, "sync-on"),
    (Notice::SyncOff, "sync-off"),
    (Notice::ScheduleSaved, "schedule-saved"),
    (Notice::SyncRequested, "sync-requested"),
    (Notice::StoreBad, "store-bad"),
    (Notice::ProfileApproved, "profile-approved"),
    (Notice::ApproveSecret, "approve-secret"),
    (Notice::SlackOn, "slack-on"),
    (Notice::SlackOff, "slack-off"),
    (Notice::CloneOn, "clone-on"),
    (Notice::CloneOff, "clone-off"),
    (Notice::Recloned, "recloned"),
    (Notice::WikiAdded, "wiki-added"),
    (Notice::WikiUnchanged, "wiki-unchanged"),
    (Notice::WikiRemoved, "wiki-removed"),
    (Notice::WikiBadLink, "wiki-bad-link"),
    (Notice::WikiOtherProject, "wiki-other-project"),
    (Notice::WikiSecret, "wiki-secret"),
    (Notice::WikiFailed, "wiki-failed"),
    (Notice::GuideBuildStarted, "guide-build-started"),
    (Notice::GuideBusy, "guide-busy"),
    (Notice::GuideApproved, "guide-approved"),
    (Notice::GuideAllApproved, "guide-all-approved"),
    (Notice::GuideEdited, "guide-edited"),
    (Notice::GuideDeleted, "guide-deleted"),
    (Notice::GuideRestored, "guide-restored"),
    (Notice::GuideInPlan, "guide-in-plan"),
    (Notice::GuideFailed, "guide-failed"),
    (Notice::GuideTooLong, "guide-too-long"),
    (Notice::GuideSecretElsewhere, "guide-secret-elsewhere"),
];

impl Notice {
    fn code(self) -> &'static str {
        NOTICES
            .iter()
            .find(|(n, _)| *n == self)
            .map_or("bad-input", |(_, c)| c)
    }

    fn from_code(code: &str) -> Option<Self> {
        NOTICES.iter().find(|(_, c)| *c == code).map(|(n, _)| *n)
    }

    fn ok(self) -> bool {
        matches!(
            self,
            Notice::BuildStarted
                | Notice::BuildAllStarted
                | Notice::Saved
                | Notice::RefreshStarted
                | Notice::Approved
                | Notice::Dismissed
                | Notice::SyncOn
                | Notice::SyncOff
                | Notice::ScheduleSaved
                | Notice::SyncRequested
                | Notice::ProfileApproved
                | Notice::SlackOn
                | Notice::SlackOff
                | Notice::CloneOn
                | Notice::CloneOff
                | Notice::Recloned
                | Notice::WikiAdded
                | Notice::WikiUnchanged
                | Notice::WikiRemoved
                | Notice::GuideBuildStarted
                | Notice::GuideApproved
                | Notice::GuideAllApproved
                | Notice::GuideEdited
                | Notice::GuideDeleted
                | Notice::GuideRestored
        )
    }

    fn message(self, ctx: &UiCtx) -> String {
        match self {
            Notice::BuildStarted => ctx.t("Building the profile in the background. It takes a few minutes; this page updates by itself."),
            Notice::BuildAllStarted => ctx.t("Building every tracked project's profile in the background, one after another."),
            Notice::Busy => ctx.t("A profile run for this project is already in progress. Try again when it finishes."),
            Notice::NoCheckout => ctx.t("This project has no checkout to scan. Set its local path on the Projects page first."),
            Notice::NothingToBuild => ctx.t("No tracked project has a checkout to build a profile from."),
            Notice::NoAgent => ctx.t("This dashboard does not start AI runs, so nothing was started."),
            Notice::Saved => ctx.t("Section saved. It is now marked as written by a person."),
            Notice::Secret => ctx.t("Nothing was saved: the text looks like it contains a secret value. Remove it and save again."),
            Notice::TooLong => ctx.t("Nothing was saved: a summary can be up to 300 characters and the details up to 20,000. Your text is still in the editor below."),
            Notice::NoProfile => ctx.t("This project has no profile yet. Build a profile first."),
            Notice::Unreadable => ctx.t("The profile file could not be read, so nothing was changed."),
            Notice::SaveFailed => ctx.t("The section could not be saved."),
            Notice::BadInput => ctx.t("That request was not understood, so nothing was changed."),
            Notice::RefreshStarted => ctx.t("Refreshing this section in the background. The update will wait under Proposals for your review."),
            Notice::Approved => ctx.t("Proposal approved and applied to the profile."),
            Notice::Dismissed => ctx.t("Proposal dismissed. It is not raised again until its sources change."),
            Notice::Changed => ctx.t("This section changed since the proposal was raised, so it was not applied. Refresh the section to get a new proposal."),
            Notice::AlreadyDecided => ctx.t("That proposal was already decided."),
            Notice::ApplyFailed => ctx.t("The proposal could not be applied to the profile."),
            Notice::SyncOn => ctx.t("Weekly sync is on."),
            Notice::SyncOff => ctx.t("Weekly sync is off. Sync now still runs it once."),
            Notice::ScheduleSaved => ctx.t("Weekly sync schedule saved."),
            Notice::SyncRequested => ctx.t("Sync requested. It starts within a minute."),
            Notice::StoreBad => ctx.t("The sync settings file could not be read, so nothing was changed."),
            Notice::ProfileApproved => ctx.t("Profile approved. It is no longer marked as a draft."),
            Notice::ApproveSecret => ctx.t("Not approved: a section looks like it contains a secret value. Edit it out, then approve again."),
            Notice::SlackOn => ctx.t("Slack messages for profile jobs are on."),
            Notice::SlackOff => ctx.t("Slack messages for profile jobs are off."),
            Notice::CloneOn => ctx.t("Memory will clone projects that have no local checkout."),
            Notice::CloneOff => ctx.t("Memory no longer clones projects that have no local checkout."),
            Notice::Recloned => ctx.t("The clone was removed. The next build or refresh clones it again."),
            Notice::WikiAdded => ctx.t("Wiki page saved. Updates to the profile appear under Proposals shortly."),
            Notice::WikiUnchanged => ctx.t("That wiki page is already saved with the same text."),
            Notice::WikiRemoved => ctx.t("Wiki page removed. The profile keeps what it already says."),
            Notice::WikiBadLink => ctx.t("That is not a link to one wiki page of this project."),
            Notice::WikiOtherProject => ctx.t("That wiki page belongs to another project."),
            Notice::WikiSecret => ctx.t("Not saved: the wiki page looks like it contains a secret value."),
            Notice::WikiFailed => ctx.t("The wiki page could not be fetched or saved. Check the link and the GitLab token."),
            Notice::GuideBuildStarted => ctx.t("Building the guide in the background. It takes a few minutes; this page updates by itself."),
            Notice::GuideBusy => ctx.t("A guide build for this project is already running. Try again when it finishes."),
            Notice::GuideApproved => ctx.t("Page approved. Agents now read it."),
            Notice::GuideAllApproved => match approved_count(ctx) {
                Some(n) => ctx.tf("Draft pages approved: {n}. Agents now read them.", &[("n", &n.to_string())]),
                None => ctx.t("Every draft page was approved."),
            },
            Notice::GuideEdited => ctx.t("Part saved. It is now marked as written by a person; rebuilds keep it."),
            Notice::GuideDeleted => ctx.t("Page deleted. A copy is kept under Deleted pages, where you can restore it."),
            Notice::GuideRestored => ctx.t("Page restored from its history. The version it replaced is kept in the history too."),
            Notice::GuideInPlan => ctx.t("This page is in the current plan, so it was not deleted. A rebuild would write it again."),
            Notice::GuideFailed => match approved_count(ctx) {
                Some(n) => ctx.tf("Approved {n} pages; some pages could not be read or saved.", &[("n", &n.to_string())]),
                None => ctx.t("The guide could not be changed: a page file could not be read or saved. Reload the page and try again."),
            },
            Notice::GuideTooLong => ctx.t("Nothing was saved: a part can hold up to 20,000 characters. Shorten it and save again."),
            Notice::GuideSecretElsewhere => ctx.t("Nothing was saved: another part of this page looks like it contains a secret value. Remove it from the page file, or delete the page and rebuild it."),
        }
    }
}

/// Approve all's count: the only value besides the notice code an approve-all
/// redirect carries. Read back only as a plain number up to 1000 (more pages than a
/// guide ever has); anything else gives `None` and the notice's plain wording.
fn approved_count(ctx: &UiCtx) -> Option<usize> {
    ctx.query()
        .first("approved")
        .filter(|v| !v.is_empty() && v.len() <= 4 && v.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n <= 1000)
}

/// The banner for the request's `notice` code; unknown codes show nothing.
fn notice_html(ctx: &UiCtx) -> String {
    let Some(n) = ctx.query().first("notice").and_then(Notice::from_code) else {
        return String::new();
    };
    let (class, icon_name) = if n.ok() { ("ok", "check_circle") } else { ("bad", "error") };
    format!(
        "<div class='mem-notice mem-notice-{class}' role='status'>{}<p>{}</p></div>",
        icon(icon_name),
        escape(&n.message(ctx))
    )
}

/// Where a handler sends the user back to.
enum Back<'a> {
    Overview,
    Project(&'a str, Option<&'static str>),
    /// One guide page: a known alias and a slug that passed `check_slug`.
    GuidePage(&'a str, &'a str),
}

fn back(to: Back<'_>, notice: Notice) -> UiResponse {
    let base = match to {
        Back::Overview => BASE.to_string(),
        Back::Project(alias, page) => project_url(alias, page),
        Back::GuidePage(alias, slug) => guide::page_url(alias, slug),
    };
    UiResponse::redirect(303, &format!("{base}&notice={}", notice.code()))
}

/// The form's `project` when it is a known alias (never the raw value).
fn known_alias(ctx: &UiCtx, raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    let config = loopx_core::config::Config::load(&ctx.state.home_dir.join("projects.json")).ok();
    alias_list(ctx, config.as_ref()).into_iter().find(|a| a == raw)
}

fn tracked_list(ctx: &UiCtx) -> Vec<TrackedProject> {
    profile_sync::tracked_projects_in(&ctx.state.home_dir.join("projects.json"))
}

/// A project Build and Refresh can scan: it has a checkout, or Memory clones one
/// the first time it runs.
fn buildable(t: &TrackedProject) -> bool {
    t.managed || (!t.work_dir.as_os_str().is_empty() && t.work_dir.is_dir())
}

fn checkout_of(ctx: &UiCtx, alias: &str) -> Option<TrackedProject> {
    tracked_list(ctx)
        .into_iter()
        .find(|t| t.alias == alias && buildable(t))
}

fn store_path(ctx: &UiCtx) -> PathBuf {
    ctx.state.home_dir.join("profile_sync.json")
}

/// Hand edits and approvals in this process take turns, so an approval's
/// "is the section still what the proposal replaces" check cannot race a save.
static WRITES: Mutex<()> = Mutex::new(());

// ---- background jobs

/// The last finished build or refresh per (home, alias), for the page to show, and
/// apart from it the last guide build or page rebuild (the guide has its own slot,
/// so neither result replaces the other). In memory only: a restart forgets it
/// (the profile, the guide and the proposals are on disk).
#[derive(Clone)]
enum JobKind {
    Build,
    Refresh(SectionKey),
    /// A guide build or a one-page rebuild.
    Guide,
}

#[derive(Clone)]
enum JobOutcome {
    Built,
    Proposed,
    NothingNew,
    /// A guide run that wrote something: how many pages were written, held for
    /// approval (an approved page gets a proposal instead) and failed. Counts only.
    GuideBuilt { saved: usize, held: usize, failed: usize },
    Failed(String),
}

#[derive(Clone)]
struct Job {
    at: DateTime<Utc>,
    kind: JobKind,
    outcome: JobOutcome,
}

/// (home, alias, guide lane).
type JobKey = (PathBuf, String, bool);
static JOBS: Mutex<Vec<(JobKey, Job)>> = Mutex::new(Vec::new());
/// The homes with a "Build all profiles" pass running.
static BUILD_ALL: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn record(home: &Path, alias: &str, job: Job) {
    let guide = matches!(job.kind, JobKind::Guide);
    forget_in(home, alias, guide);
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    jobs.push(((home.to_path_buf(), alias.to_string(), guide), job));
}

fn forget_in(home: &Path, alias: &str, guide: bool) {
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    jobs.retain(|((h, a, g), _)| !(h == home && a == alias && *g == guide));
}

fn last_in(home: &Path, alias: &str, guide: bool) -> Option<Job> {
    let jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    jobs.iter()
        .find(|((h, a, g), _)| h == home && a == alias && *g == guide)
        .map(|(_, j)| j.clone())
}

/// Forget the last profile result for `alias`: a new build makes it out of date.
fn forget(home: &Path, alias: &str) {
    forget_in(home, alias, false);
}

/// The last profile build or refresh result for `alias` (never a guide run's).
fn last_job(home: &Path, alias: &str) -> Option<Job> {
    last_in(home, alias, false)
}

struct BuildAllGuard(PathBuf);

impl BuildAllGuard {
    fn acquire(home: &Path) -> Option<Self> {
        let mut running = BUILD_ALL.lock().unwrap_or_else(|e| e.into_inner());
        if running.iter().any(|h| h == home) {
            return None;
        }
        running.push(home.to_path_buf());
        Some(Self(home.to_path_buf()))
    }
}

impl Drop for BuildAllGuard {
    fn drop(&mut self) {
        let mut running = BUILD_ALL.lock().unwrap_or_else(|e| e.into_inner());
        running.retain(|h| h != &self.0);
    }
}

fn build_all_running(home: &Path) -> bool {
    BUILD_ALL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|h| h == home)
}

/// One line, bounded: what a failed job shows (secret-checked before the cut).
fn failure_line(e: &str) -> String {
    profile_sync::safe_failure_line(e)
}

/// A stored failure message as shown: files written before failures were
/// guarded may still quote something secret-shaped.
fn shown_failure(ctx: &UiCtx, msg: &str) -> String {
    if contains_secret(msg).is_some() {
        ctx.t("details hidden")
    } else {
        crate::ui::profile_jobs::failure_text(ctx, msg)
    }
}

/// Everything a background job needs, owned (the request is gone by then).
struct JobEnv {
    home: PathBuf,
    root: PathBuf,
    approvals: PathBuf,
    /// Where each job's live record goes (the Agents and Terminal pages, the Mac app).
    activity: PathBuf,
    fixed_now: Option<DateTime<Utc>>,
    agent: ProfileAgent,
    /// The sync store holding the `notify_slack` switch.
    store: PathBuf,
    /// Where a finished job's Slack message goes; `None` sends nothing.
    notifier: Option<profile_notify::Notifier>,
}

impl JobEnv {
    fn now(&self) -> DateTime<Utc> {
        self.fixed_now.unwrap_or_else(Utc::now)
    }

    /// The job's one Slack message: sent after its live record is gone, when the
    /// switch is on. Never fails or panics (see `profile_notify::send_if_enabled`).
    fn notify(&self, f: &Finished) {
        if let Some(n) = &self.notifier {
            profile_notify::send_if_enabled(&self.store, f, &**n);
        }
    }
}

/// The injected agent, else the real sealed AI CLI when this dashboard may start
/// processes; `None` (tests, golden pages) starts nothing.
fn job_env(ctx: &UiCtx) -> Option<JobEnv> {
    let agent = ctx.state.profile_agent.clone().or_else(|| {
        ctx.state
            .spawn_processes
            .then(|| Arc::new(profile_build::SealedAgent) as ProfileAgent)
    })?;
    Some(JobEnv {
        home: ctx.state.home_dir.clone(),
        root: memory_root(ctx),
        approvals: approvals_dir(ctx),
        activity: crate::ui::profile_jobs::base(ctx),
        fixed_now: ctx.state.fixed_now,
        agent,
        store: store_path(ctx),
        notifier: ctx.state.profile_notifier.clone().or_else(|| {
            ctx.state
                .spawn_processes
                .then(|| profile_notify::slack_sender(slack_config(ctx)))
        }),
    })
}

/// This dashboard's Slack config (`~/.slack/config.json`).
fn slack_config(ctx: &UiCtx) -> PathBuf {
    ctx.state.user_home.join(".slack").join("config.json")
}

/// Build one profile; its live record is gone by the time this returns. Gives
/// what it wrote, or the fixed reason it failed with.
fn run_build(env: &JobEnv, t: &TrackedProject, aliases: &[String]) -> Result<BuildDone, &'static str> {
    forget(&env.home, &t.alias);
    let job = ProfileJob::start(&env.activity, ProfileJobKind::Build, &t.alias);
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let dir = loopx_core::profile_checkout::prepare(t, &env.home, std::time::SystemTime::now())?.dir;
        profile_build::build_profile_with_progress(
            &t.alias,
            &dir,
            &t.instance,
            &t.project_id,
            aliases,
            &*env.agent,
            env.now(),
            Some(&env.root),
            Some(&job),
        )
    }))
    .unwrap_or_else(|_| Err("internal error".into()));
    let fixed = job.failure();
    drop(job);
    let (outcome, done) = match res {
        Ok(p) => (JobOutcome::Built, Ok(BuildDone::of(&p))),
        Err(e) => (JobOutcome::Failed(failure_line(&e)), Err(fixed.unwrap_or_else(|| profile_notify::reason(&e)))),
    };
    record(&env.home, &t.alias, Job { at: env.now(), kind: JobKind::Build, outcome });
    done
}

// ---- handlers

fn build(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    if form.first("project") == Some("all") {
        return build_all(ctx);
    }
    let Some(alias) = known_alias(ctx, form.first("project")) else {
        return back(Back::Overview, Notice::BadInput);
    };
    let to = || {
        if form.first("from") == Some("overview") {
            Back::Overview
        } else {
            Back::Project(&alias, None)
        }
    };
    let Some(t) = checkout_of(ctx, &alias) else {
        return back(to(), Notice::NoCheckout);
    };
    let Some(env) = job_env(ctx) else {
        return back(to(), Notice::NoAgent);
    };
    // Taken on the click, so a second click is refused at once.
    let Some(slot) = profile_build::try_acquire(&alias) else {
        return back(to(), Notice::Busy);
    };
    let aliases: Vec<String> = tracked_list(ctx).into_iter().map(|t| t.alias).collect();
    // Cleared on the click, so the page it redirects to never shows the old failure.
    forget(&ctx.state.home_dir, &alias);
    std::thread::spawn(move || {
        let _slot = slot;
        let began = std::time::Instant::now();
        let result = run_build(&env, &t, &aliases);
        env.notify(&Finished::Build { alias: t.alias.clone(), result, took: began.elapsed() });
    });
    back(to(), Notice::BuildStarted)
}

/// Every tracked project with a checkout, one after another; a project another
/// run is busy with is skipped. Human-written sections are kept by the build.
fn build_all(ctx: &UiCtx) -> UiResponse {
    let all = tracked_list(ctx);
    let aliases: Vec<String> = all.iter().map(|t| t.alias.clone()).collect();
    let list: Vec<TrackedProject> = all
        .into_iter()
        .filter(buildable)
        .collect();
    if list.is_empty() {
        return back(Back::Overview, Notice::NothingToBuild);
    }
    let Some(env) = job_env(ctx) else {
        return back(Back::Overview, Notice::NoAgent);
    };
    let Some(guard) = BuildAllGuard::acquire(&ctx.state.home_dir) else {
        return back(Back::Overview, Notice::Busy);
    };
    std::thread::spawn(move || {
        let _guard = guard;
        let began = std::time::Instant::now();
        let mut rows = Vec::new();
        for t in &list {
            let Some(_slot) = profile_build::try_acquire(&t.alias) else {
                rows.push((t.alias.clone(), RowResult::Busy));
                continue;
            };
            let row = match run_build(&env, t, &aliases) {
                Ok(_) => RowResult::Built,
                Err(why) => RowResult::Failed(why),
            };
            rows.push((t.alias.clone(), row));
        }
        // One message for the whole pass, not one per project.
        env.notify(&Finished::BuildAll { rows, took: began.elapsed() });
    });
    back(Back::Overview, Notice::BuildAllStarted)
}

fn save_section(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let Some(alias) = known_alias(ctx, form.first("project")) else {
        return back(Back::Overview, Notice::BadInput);
    };
    let Some(key) = form.first("section").and_then(SectionKey::from_slug) else {
        return back(Back::Project(&alias, None), Notice::BadInput);
    };
    let to = || Back::Project(&alias, Some(page_of(key)));
    // Browsers send CRLF: count and check the text as it will be stored.
    let summary = form.first("summary").unwrap_or("").replace("\r\n", "\n");
    let body = form.first("body").unwrap_or("").replace("\r\n", "\n");
    let (summary, body) = (summary.as_str(), body.as_str());
    // The profile's own save does not reject secrets; a person's paste must not
    // land in a file agents read. Checked first, so rejected secret text is never kept.
    if contains_secret(summary).is_some() || contains_secret(body).is_some() {
        return back(to(), Notice::Secret);
    }
    if summary.chars().count() > SUMMARY_MAX || body.chars().count() > BODY_MAX {
        keep_draft(&ctx.state.home_dir, &alias, key, summary, body, ctx.state.now());
        return back(to(), Notice::TooLong);
    }
    let root = memory_root(ctx);
    let _w = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    match project_profile::load(&alias, Some(&root)) {
        Ok(Some(_)) => {}
        Ok(None) => return back(to(), Notice::NoProfile),
        Err(_) => return back(to(), Notice::Unreadable),
    }
    let now = ctx.state.now();
    let saved = project_profile::update_locked(&alias, Some(&root), |existing| {
        let mut p = existing.ok_or_else(|| "no profile".to_string())?;
        project_profile::edit_section(&mut p, key, summary, body, now);
        Ok(p)
    });
    match saved {
        Ok(_) => {
            drop_draft(&ctx.state.home_dir, &alias, key);
            back(to(), Notice::Saved)
        }
        Err(_) => back(to(), Notice::SaveFailed),
    }
}

fn refresh_section(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let Some(alias) = known_alias(ctx, form.first("project")) else {
        return back(Back::Overview, Notice::BadInput);
    };
    let Some(key) = form.first("section").and_then(SectionKey::from_slug) else {
        return back(Back::Project(&alias, None), Notice::BadInput);
    };
    let to = || Back::Project(&alias, Some(page_of(key)));
    match project_profile::load(&alias, Some(&memory_root(ctx))) {
        Ok(Some(_)) => {}
        Ok(None) => return back(to(), Notice::NoProfile),
        Err(_) => return back(to(), Notice::Unreadable),
    }
    let Some(t) = checkout_of(ctx, &alias) else {
        return back(to(), Notice::NoCheckout);
    };
    let Some(env) = job_env(ctx) else {
        return back(to(), Notice::NoAgent);
    };
    let Some(slot) = profile_build::try_acquire(&alias) else {
        return back(to(), Notice::Busy);
    };
    // The same issue digest (and lessons) the page reads, from this dashboard's roots.
    let gitlab_root = ctx.state.user_home.join(".gitlab").join("cache");
    let data_root = ctx.state.home_dir.join("data");
    std::thread::spawn(move || {
        let _slot = slot;
        let began = std::time::Instant::now();
        let job = ProfileJob::start(&env.activity, ProfileJobKind::Refresh(key), &t.alias);
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let dir = loopx_core::profile_checkout::prepare(&t, &env.home, std::time::SystemTime::now())?.dir;
            let digest = if t.instance.is_empty() || t.project_id.is_empty() {
                profile_scan::digest_for("", "", &t.alias, Some(&env.root))
            } else {
                profile_scan::digest_issues_cached(&t.instance, &t.project_id, &t.alias, &gitlab_root, &data_root, Some(&env.root))
            };
            profile_sync::propose_refresh_held_with_progress(
                &t.alias,
                key,
                &dir,
                &digest,
                &*env.agent,
                env.now(),
                Some(&env.root),
                &env.approvals,
                Some(&job),
            )
        }))
        .unwrap_or_else(|_| Err("internal error".into()));
        let fixed = job.failure();
        drop(job);
        let (outcome, result) = match res {
            Ok(Some(_)) => (JobOutcome::Proposed, Ok(true)),
            Ok(None) => (JobOutcome::NothingNew, Ok(false)),
            Err(e) => (JobOutcome::Failed(failure_line(&e)), Err(fixed.unwrap_or_else(|| profile_notify::reason(&e)))),
        };
        record(&env.home, &t.alias, Job { at: env.now(), kind: JobKind::Refresh(key), outcome });
        env.notify(&Finished::Refresh { alias: t.alias.clone(), section: key, result, took: began.elapsed() });
    });
    back(to(), Notice::RefreshStarted)
}

// ---- guide builds

/// What a guide run writes: the whole guide, or one page of the saved plan (a slug
/// that passed `check_slug` and that the plan names).
enum GuideRun {
    All,
    Page(String),
}

/// Run one guide build or page rebuild and record its result for the page: page
/// counts, or a fixed reason (never the error's own text). Pages an approved page
/// holds back are raised as proposals. A panic is an "internal error".
fn run_guide(env: &JobEnv, t: &TrackedProject, what: &GuideRun, tracked: &[String]) {
    forget_in(&env.home, &t.alias, true);
    let job = ProfileJob::start(&env.activity, ProfileJobKind::Guide, &t.alias);
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // A clone that cannot be made fails the job (its record says failed, with a
        // fixed reason), not "interrupted".
        let dir = match loopx_core::profile_checkout::prepare(t, &env.home, std::time::SystemTime::now()) {
            Ok(p) => p.dir,
            Err(e) => {
                Progress::fail(&job, &e);
                return Err(e);
            }
        };
        let (agent, now, root) = (&*env.agent, env.now(), Some(env.root.as_path()));
        let (alias, instance, project) = (t.alias.as_str(), t.instance.as_str(), t.project_id.as_str());
        let report = match what {
            GuideRun::All => guide_build::build_guide(alias, &dir, instance, project, tracked, agent, now, root, Some(&job)),
            GuideRun::Page(slug) => {
                guide_build::rebuild_page(alias, slug, &dir, instance, project, tracked, agent, now, root, Some(&job))
            }
        }?;
        // "Waiting for approval" counts the proposals actually raised: a held page whose
        // update is already pending (or cannot be raised) is not counted.
        let raised = guide_proposal::raise_all(alias, &report, &env.approvals, root, now).len();
        Ok::<_, String>((report, raised))
    }))
    .unwrap_or_else(|_| Err("internal error".into()));
    let outcome = match res {
        Ok((r, raised)) => JobOutcome::GuideBuilt { saved: r.saved.len(), held: raised, failed: r.failed.len() },
        Err(e) => JobOutcome::Failed(job.failure().unwrap_or_else(|| failure_reason(None, &e)).to_string()),
    };
    record(&env.home, &t.alias, Job { at: env.now(), kind: JobKind::Guide, outcome });
    drop(job);
}

/// Build guide / Rebuild this page. In order: a known alias (else back to the
/// overview), for a page a slug of the saved plan, a checkout, an agent, and the
/// guide slot, taken on the click so a second click is refused at once. The slot
/// is the guide's own: a profile build of the same project does not block it.
fn start_guide(ctx: &UiCtx, page: bool) -> UiResponse {
    let form = ctx.form();
    let Some(alias) = known_alias(ctx, form.first("project")) else {
        return back(Back::Overview, Notice::BadInput);
    };
    let what = if page {
        let root = memory_root(ctx);
        match form.first("slug").filter(|s| guide::rebuildable(&alias, s, &root)) {
            Some(s) => GuideRun::Page(s.to_string()),
            None => return back(Back::Project(&alias, Some("guide")), Notice::BadInput),
        }
    } else {
        GuideRun::All
    };
    // A page rebuild goes back to its page (a slug checked above), a build to the guide.
    let page_slug = match &what {
        GuideRun::Page(slug) => Some(slug.clone()),
        GuideRun::All => None,
    };
    let to = || match &page_slug {
        Some(slug) => Back::GuidePage(&alias, slug),
        None => Back::Project(&alias, Some("guide")),
    };
    let Some(t) = checkout_of(ctx, &alias) else {
        return back(to(), Notice::NoCheckout);
    };
    let Some(env) = job_env(ctx) else {
        return back(to(), Notice::NoAgent);
    };
    let Some(slot) = guide_build::try_acquire(&alias) else {
        return back(to(), Notice::GuideBusy);
    };
    let tracked: Vec<String> = tracked_list(ctx).into_iter().map(|t| t.alias).collect();
    // Cleared on the click, so the page it redirects to never shows the old result.
    forget_in(&ctx.state.home_dir, &alias, true);
    std::thread::spawn(move || {
        let _slot = slot;
        run_guide(&env, &t, &what, &tracked);
    });
    back(to(), Notice::GuideBuildStarted)
}

fn guide_build_route(ctx: &UiCtx) -> UiResponse {
    start_guide(ctx, false)
}

fn guide_rebuild_page(ctx: &UiCtx) -> UiResponse {
    start_guide(ctx, true)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The proposal's section key, when it is a well-formed proposal for `alias`.
fn proposal_key(a: &Value, alias: &str) -> Option<SectionKey> {
    if a["kind"] != profile_sync::KIND || a["target"]["alias"] != alias || a["project"] != alias {
        return None;
    }
    a["target"]["section"].as_str().and_then(SectionKey::from_slug)
}

/// Whether the section still holds the text the proposal replaces (the same
/// comparison `apply_proposal` makes under the lock).
fn still_applies(profile: &Profile, key: SectionKey, a: &Value) -> bool {
    let t = &a["target"];
    let (old, old_summary) = (t["old"].as_str().unwrap_or(""), t["old_summary"].as_str().unwrap_or(""));
    match profile.sections.get(&key) {
        Some(cur) => cur.body == old && cur.summary == old_summary,
        None => old.is_empty() && old_summary.is_empty(),
    }
}

/// Approve or dismiss. A decision is final in the approvals store, so an approval
/// is recorded only after checking that the section still holds the proposal's
/// old text; then the proposal is applied. Should the apply still fail (another
/// process wrote in between), the approved proposal keeps an Apply button while
/// it remains applicable.
fn decide(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let Some(alias) = known_alias(ctx, form.first("project")) else {
        return back(Back::Overview, Notice::BadInput);
    };
    let to = || Back::Project(&alias, Some("proposals"));
    let id = form.first("id").unwrap_or("");
    let decision = form.first("decision");
    if !valid_id(id) || !matches!(decision, Some("approve" | "dismiss")) {
        return back(to(), Notice::BadInput);
    }
    let dir = approvals_dir(ctx);
    let Some((a, key)) = approvals::get(&dir, id)
        .filter(|a| a["id"] == id)
        .and_then(|a| proposal_key(&a, &alias).map(|k| (a, k)))
    else {
        return back(to(), Notice::BadInput);
    };
    let now = ctx.state.now();
    if decision == Some("dismiss") {
        if a["status"] != "pending" {
            return back(to(), Notice::AlreadyDecided);
        }
        return match approvals::decide(&dir, id, "deny", "dashboard", now) {
            Ok(_) => back(to(), Notice::Dismissed),
            Err(_) => back(to(), Notice::AlreadyDecided),
        };
    }
    let root = memory_root(ctx);
    let _w = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    // No build or refresh may rewrite the profile between the check and the apply.
    let Some(_slot) = profile_build::try_acquire(&alias) else {
        return back(to(), Notice::Busy);
    };
    let profile = match project_profile::load(&alias, Some(&root)) {
        Ok(Some(p)) => p,
        Ok(None) => return back(to(), Notice::NoProfile),
        Err(_) => return back(to(), Notice::Unreadable),
    };
    match a["status"].as_str() {
        Some("pending") => {
            if !still_applies(&profile, key, &a) {
                return back(to(), Notice::Changed);
            }
            if approvals::decide(&dir, id, "approve", "dashboard", now).is_err() {
                return back(to(), Notice::AlreadyDecided);
            }
        }
        // Approved earlier but not applied: apply it now (a no-op when it was).
        Some("approved") => {}
        _ => return back(to(), Notice::AlreadyDecided),
    }
    match profile_sync::apply_proposal(id, Some(&root), &dir) {
        Ok(_) => back(to(), Notice::Approved),
        Err(e) if e.contains("changed since") => back(to(), Notice::Changed),
        Err(_) => back(to(), Notice::ApplyFailed),
    }
}

/// Approve a draft profile: it stops being marked a draft. Refused (value-free)
/// while any of its text looks like a secret.
fn approve_profile(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let Some(alias) = known_alias(ctx, form.first("project")) else {
        return back(Back::Overview, Notice::BadInput);
    };
    let to = || Back::Project(&alias, None);
    let root = memory_root(ctx);
    let _w = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    match project_profile::load(&alias, Some(&root)) {
        Ok(Some(p)) if project_profile::validate_draft(&p).is_err() => return back(to(), Notice::ApproveSecret),
        Ok(Some(_)) => {}
        Ok(None) => return back(to(), Notice::NoProfile),
        Err(_) => return back(to(), Notice::Unreadable),
    }
    let saved = project_profile::update_locked(&alias, Some(&root), |existing| {
        let mut p = existing.ok_or_else(|| "no profile".to_string())?;
        project_profile::validate_draft(&p)?;
        p.status = Status::Approved;
        Ok(p)
    });
    match saved {
        Ok(_) => back(to(), Notice::ProfileApproved),
        Err(_) => back(to(), Notice::SaveFailed),
    }
}

fn sync_settings(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let enabled = matches!(form.first("enabled"), Some("on" | "1"));
    let Some(weekday) = form
        .first("weekday")
        .and_then(|w| WEEKDAYS.iter().find(|(code, _)| *code == w))
        .map(|(_, d)| *d)
    else {
        return back(Back::Overview, Notice::BadInput);
    };
    let Some(hour) = form
        .first("hour")
        .filter(|h| !h.is_empty() && h.len() <= 2 && h.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|h| h.parse::<u32>().ok())
        .filter(|h| *h <= 23)
    else {
        return back(Back::Overview, Notice::BadInput);
    };
    let changed = profile_sync::with_store(&store_path(ctx), |s| {
        let was = s.settings.enabled;
        s.settings.enabled = enabled;
        s.settings.weekday = weekday;
        s.settings.hour = hour;
        was != enabled
    });
    match changed {
        Err(_) => back(Back::Overview, Notice::StoreBad),
        Ok(true) if enabled => back(Back::Overview, Notice::SyncOn),
        Ok(true) => back(Back::Overview, Notice::SyncOff),
        Ok(false) => back(Back::Overview, Notice::ScheduleSaved),
    }
}

/// The "Slack messages for profile jobs" switch: `notify=on` or `notify=0`,
/// anything else is refused. Only that setting changes.
fn slack_settings(ctx: &UiCtx) -> UiResponse {
    let on = match ctx.form().first("notify") {
        Some("on" | "1") => true,
        Some("0") => false,
        _ => return back(Back::Overview, Notice::BadInput),
    };
    match profile_sync::with_store(&store_path(ctx), |s| s.settings.notify_slack = on) {
        Err(_) => back(Back::Overview, Notice::StoreBad),
        Ok(()) if on => back(Back::Overview, Notice::SlackOn),
        Ok(()) => back(Back::Overview, Notice::SlackOff),
    }
}

/// The "Clone projects without a local checkout" switch: `clone=on` or `clone=0`,
/// anything else is refused. Only that setting changes.
fn clone_settings(ctx: &UiCtx) -> UiResponse {
    let on = match ctx.form().first("clone") {
        Some("on" | "1") => true,
        Some("0") => false,
        _ => return back(Back::Overview, Notice::BadInput),
    };
    match profile_sync::with_store(&store_path(ctx), |s| s.settings.clone_for_memory = on) {
        Err(_) => back(Back::Overview, Notice::StoreBad),
        Ok(()) if on => back(Back::Overview, Notice::CloneOn),
        Ok(()) => back(Back::Overview, Notice::CloneOff),
    }
}

/// Delete a managed project's clone so the next build or refresh clones it afresh.
/// Only ever the loopx-owned directory: a project with its own `local_path` is refused.
fn reclone(ctx: &UiCtx) -> UiResponse {
    let Some(alias) = known_alias(ctx, ctx.form().first("project")) else {
        return back(Back::Overview, Notice::BadInput);
    };
    if !tracked_list(ctx).iter().any(|t| t.alias == alias && t.managed) {
        return back(Back::Overview, Notice::BadInput);
    }
    let Some(_slot) = profile_build::try_acquire(&alias) else {
        return back(Back::Project(&alias, None), Notice::Busy);
    };
    // A guide build uses the same clone: refused while one runs, and both slots
    // are held until the clone is gone.
    let Some(_guide_slot) = guide_build::try_acquire(&alias) else {
        return back(Back::Project(&alias, None), Notice::Busy);
    };
    let dir = loopx_core::profile_checkout::managed_dir(&ctx.state.home_dir, &alias);
    let lock = loopx_core::profile_checkout::clone_lock(&dir);
    let _turn = lock.lock().unwrap_or_else(|e| e.into_inner());
    let _ = std::fs::remove_dir_all(&dir);
    back(Back::Project(&alias, None), Notice::Recloned)
}

/// The wiki notice for an `add_page` failure (never the raw text).
fn wiki_failure(e: &str) -> Notice {
    if e.contains("another project") {
        Notice::WikiOtherProject
    } else if e.contains("secret") {
        Notice::WikiSecret
    } else if e.contains("not a web link") || e.contains("not a wiki link") || e.contains("open one wiki page") {
        Notice::WikiBadLink
    } else {
        Notice::WikiFailed
    }
}

/// Save the wiki page behind a pasted link for a project and queue the sections it
/// can change as proposals (processed even while the weekly switch is off).
fn wiki_add(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let Some(alias) = known_alias(ctx, form.first("project")) else {
        return back(Back::Overview, Notice::BadInput);
    };
    let to = || Back::Project(&alias, None);
    let project = loopx_core::config::Config::load(&ctx.state.home_dir.join("projects.json"))
        .ok()
        .and_then(|c| c.project(&alias).ok());
    let text = |k: &str| project.as_ref().map(|p| n_of(&p[k])).filter(|v| v != "None").unwrap_or_default();
    let (instance, project_id) = (text("instance"), text("project_id"));
    if instance.is_empty() {
        return back(to(), Notice::WikiFailed);
    }
    let root = memory_root(ctx);
    let fetch = loopx_core::profile_wiki::gitlab_fetch(&instance);
    let url = form.first("url").unwrap_or("");
    match loopx_core::profile_wiki::add_page(&alias, url, &project_id, Some(&root), Utc::now(), &fetch) {
        Ok(a) if a.changed => {
            profile_sync::queue_wiki_refresh(&alias, Some(&root));
            back(to(), Notice::WikiAdded)
        }
        Ok(_) => back(to(), Notice::WikiUnchanged),
        Err(e) => back(to(), wiki_failure(&e)),
    }
}

/// Forget one saved wiki page (the profile keeps what it already says).
fn wiki_remove(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let Some(alias) = known_alias(ctx, form.first("project")) else {
        return back(Back::Overview, Notice::BadInput);
    };
    match loopx_core::profile_wiki::remove(&alias, form.first("page").unwrap_or(""), Some(&memory_root(ctx))) {
        Ok(_) => back(Back::Project(&alias, None), Notice::WikiRemoved),
        Err(_) => back(Back::Project(&alias, None), Notice::BadInput),
    }
}

/// Runs once on the next server tick, whether or not the weekly switch is on.
fn sync_now(ctx: &UiCtx) -> UiResponse {
    match profile_sync::with_store(&store_path(ctx), |s| s.sync_requested = true) {
        Ok(()) => back(Back::Overview, Notice::SyncRequested),
        Err(_) => back(Back::Overview, Notice::StoreBad),
    }
}

// ---- controls

/// The button that asks before a costly build. It is a link to the same page with
/// `confirm=...`, which renders the dialog already open, so it works without
/// JavaScript; with JavaScript it opens the dialog in place.
fn confirm_opener(dialog: &str, href: &str, icon_name: &str, label: &str) -> String {
    format!(
        "<a class='mem-btn mem-btn-primary' href='{}' onclick=\"document.getElementById('{dialog}').showModal();return false\">{}{}</a>",
        escape(href),
        icon(icon_name),
        escape(label)
    )
}

/// A confirm sheet whose primary button posts the build form; Cancel closes it
/// (or, without JavaScript, goes back to `cancel`).
fn confirm_dialog(ctx: &UiCtx, id: &str, open: bool, title: &str, text: &str, fields: &str, cancel: &str) -> String {
    format!(
        "<dialog id='{id}' class='mem-dialog'{}><form method='post' action='/insights/memory/build'>{}{fields}\
         <h2>{}</h2><p>{}</p><div class='mem-dialog-actions'>\
         <a class='mem-btn' href='{}' onclick=\"this.closest('dialog').close();return false\">{}</a>\
         <button class='mem-btn mem-btn-primary' type='submit'>{}{}</button></div></form></dialog>",
        if open { " open" } else { "" },
        ctx.csrf_input(),
        escape(title),
        escape(text),
        escape(cancel),
        escape(&ctx.t("Cancel")),
        icon("auto_awesome"),
        escape(&ctx.t("Build"))
    )
}

/// A refused (too long) edit, kept for a while so the editor shows it again.
/// Never holds text that looked like a secret: that check runs first. Bounded:
/// at most [`DRAFT_MAX_ENTRIES`] entries and [`DRAFT_MAX_TOTAL_BYTES`] in all
/// (oldest evicted first); an edit over [`DRAFT_MAX_ENTRY_BYTES`] is not kept.
/// Expired entries are pruned on every read and write.
type DraftKey = (PathBuf, String, SectionKey);
const DRAFT_MINUTES: i64 = 30;
const DRAFT_MAX_ENTRIES: usize = 32;
const DRAFT_MAX_ENTRY_BYTES: usize = 64 * 1024;
const DRAFT_MAX_TOTAL_BYTES: usize = 512 * 1024;

struct KeptEdit {
    key: DraftKey,
    at: DateTime<Utc>,
    summary: String,
    body: String,
}

impl KeptEdit {
    fn bytes(&self) -> usize {
        self.summary.len() + self.body.len()
    }
}

/// Oldest first.
#[derive(Default)]
struct DraftStore {
    entries: Vec<KeptEdit>,
}

impl DraftStore {
    fn prune(&mut self, now: DateTime<Utc>) {
        self.entries.retain(|e| now - e.at < Duration::minutes(DRAFT_MINUTES));
    }

    fn keep(&mut self, key: DraftKey, summary: &str, body: &str, now: DateTime<Utc>) {
        self.prune(now);
        // A newer refusal replaces the older one, kept or not: never show stale text.
        self.entries.retain(|e| e.key != key);
        if summary.len() + body.len() > DRAFT_MAX_ENTRY_BYTES {
            return;
        }
        self.entries.push(KeptEdit { key, at: now, summary: summary.to_string(), body: body.to_string() });
        while self.entries.len() > DRAFT_MAX_ENTRIES || self.bytes() > DRAFT_MAX_TOTAL_BYTES {
            self.entries.remove(0);
        }
    }

    fn get(&mut self, key: &DraftKey, now: DateTime<Utc>) -> Option<(String, String)> {
        self.prune(now);
        self.entries
            .iter()
            .find(|e| &e.key == key)
            .map(|e| (e.summary.clone(), e.body.clone()))
    }

    fn drop_key(&mut self, key: &DraftKey) {
        self.entries.retain(|e| &e.key != key);
    }

    fn bytes(&self) -> usize {
        self.entries.iter().map(KeptEdit::bytes).sum()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

static DRAFTS: Mutex<DraftStore> = Mutex::new(DraftStore { entries: Vec::new() });

fn draft_key(home: &Path, alias: &str, key: SectionKey) -> DraftKey {
    (home.to_path_buf(), alias.to_string(), key)
}

fn keep_draft(home: &Path, alias: &str, key: SectionKey, summary: &str, body: &str, now: DateTime<Utc>) {
    let mut drafts = DRAFTS.lock().unwrap_or_else(|e| e.into_inner());
    drafts.keep(draft_key(home, alias, key), summary, body, now);
}

fn kept_draft(home: &Path, alias: &str, key: SectionKey, now: DateTime<Utc>) -> Option<(String, String)> {
    let mut drafts = DRAFTS.lock().unwrap_or_else(|e| e.into_inner());
    drafts.get(&draft_key(home, alias, key), now)
}

fn drop_draft(home: &Path, alias: &str, key: SectionKey) {
    let mut drafts = DRAFTS.lock().unwrap_or_else(|e| e.into_inner());
    drafts.drop_key(&draft_key(home, alias, key));
}

fn spinner() -> &'static str {
    "<span class='mem-spin' aria-hidden='true'></span>"
}

fn hidden(name: &str, value: &str) -> String {
    format!("<input type='hidden' name='{name}' value='{}'>", escape(value))
}

fn weekday_label(ctx: &UiCtx, d: chrono::Weekday) -> String {
    match d {
        chrono::Weekday::Mon => ctx.t("Monday"),
        chrono::Weekday::Tue => ctx.t("Tuesday"),
        chrono::Weekday::Wed => ctx.t("Wednesday"),
        chrono::Weekday::Thu => ctx.t("Thursday"),
        chrono::Weekday::Fri => ctx.t("Friday"),
        chrono::Weekday::Sat => ctx.t("Saturday"),
        chrono::Weekday::Sun => ctx.t("Sunday"),
    }
}

/// The overview's controls: the weekly sync switch (saves on click, like the
/// Todos switch), its day and hour (each saves on change; a Save button stands in
/// without JavaScript), "Sync now" and "Build all profiles".
fn overview_controls(ctx: &UiCtx, store: &Result<SyncStore, String>) -> String {
    let csrf = ctx.csrf_input();
    let mut out = String::new();
    if let Ok(s) = store {
        let set = &s.settings;
        let day = WEEKDAYS.iter().find(|(_, d)| *d == set.weekday).map_or("mon", |(c, _)| c);
        let hour = set.hour.min(23).to_string();
        let label = escape(&ctx.t("Weekly sync"));
        // The switch posts the opposite of what it shows; the schedule keeps it.
        let (class, checked, next, keep) = if set.enabled {
            (" is-on", "true", "0", "on")
        } else {
            ("", "false", "on", "0")
        };
        out.push_str(&format!(
            "<form method='post' action='/insights/memory/sync/settings' class='mem-switch'>{csrf}{}{}{}\
             <button type='submit' class='switch{class}' role='switch' aria-checked='{checked}' aria-label='{label}' title='{label}'><span class='switch-thumb'></span></button>\
             <span>{label}</span></form>",
            hidden("enabled", next),
            hidden("weekday", day),
            hidden("hour", &hour)
        ));
        let days: String = WEEKDAYS
            .iter()
            .map(|(code, d)| {
                format!(
                    "<option value='{code}'{}>{}</option>",
                    if *d == set.weekday { " selected" } else { "" },
                    escape(&weekday_label(ctx, *d))
                )
            })
            .collect();
        let hours: String = (0..24)
            .map(|h| {
                format!(
                    "<option value='{h}'{}>{h:02}:00</option>",
                    if h == set.hour { " selected" } else { "" }
                )
            })
            .collect();
        out.push_str(&format!(
            "<form method='post' action='/insights/memory/sync/settings' class='mem-schedule'>{csrf}{}\
             <select class='mem-select' name='weekday' aria-label='{}' onchange='this.form.requestSubmit()'>{days}</select>\
             <select class='mem-select' name='hour' aria-label='{}' onchange='this.form.requestSubmit()'>{hours}</select>\
             <noscript><button class='mem-btn' type='submit'>{}</button></noscript></form>",
            hidden("enabled", keep),
            escape(&ctx.t("Sync day")),
            escape(&ctx.t("Sync hour")),
            escape(&ctx.t("Save"))
        ));
        out.push_str(&format!(
            "<form method='post' action='/insights/memory/sync/now' class='mem-inline'>{csrf}\
             <button class='mem-btn' type='submit' title='{}'>{}{}</button></form>",
            escape(&ctx.t("Run the weekly sync once, within a minute")),
            icon("autorenew"),
            escape(&ctx.t("Sync now"))
        ));
        out.push_str(&slack_switch(ctx, set.notify_slack));
        out.push_str(&clone_switch(ctx, set.clone_for_memory));
    }
    let build = if build_all_running(&ctx.state.home_dir) {
        format!(
            "<button class='mem-btn mem-btn-primary' type='button' disabled data-mem-job='all'>{}{}</button>",
            spinner(),
            escape(&ctx.t("Building profiles…"))
        )
    } else {
        // One AI pass per project, and approved profiles go back to Draft: ask first.
        let n = tracked_list(ctx)
            .iter()
            .filter(|t| buildable(t))
            .count();
        let text = if n == 1 {
            ctx.t("This runs one AI pass for the 1 tracked project and puts an approved profile back to Draft for review. Sections you edited are kept.")
        } else {
            ctx.tf(
                "This runs one AI pass for each of the {n} tracked projects and puts approved profiles back to Draft for review. Sections you edited are kept.",
                &[("n", &n.to_string())],
            )
        };
        format!(
            "{}{}",
            confirm_opener("mem-build-all", &format!("{BASE}&confirm=build-all"), "auto_awesome", &ctx.t("Build all profiles")),
            confirm_dialog(
                ctx,
                "mem-build-all",
                ctx.query().first("confirm") == Some("build-all"),
                &ctx.t("Build all profiles?"),
                &text,
                &format!("{}{}", hidden("project", "all"), hidden("from", "overview")),
                BASE
            )
        )
    };
    out.push_str(&build);
    out
}

/// The "Slack messages for profile jobs" switch (saves on click, like the
/// weekly one), and a hint pointing to Connectors when no webhook is set up.
fn slack_switch(ctx: &UiCtx, on: bool) -> String {
    let label = escape(&ctx.t("Slack messages for profile jobs"));
    let (class, checked, next) = if on { (" is-on", "true", "0") } else { ("", "false", "on") };
    let hint = if profile_notify::webhook_configured(&slack_config(ctx)) {
        String::new()
    } else {
        format!(
            "<a class='mem-slack-hint' href='/connectors'>{}</a>",
            escape(&ctx.t("No Slack webhook configured — set it up in Connectors"))
        )
    };
    format!(
        "<div class='mem-slack'><form method='post' action='/insights/memory/sync/slack' class='mem-switch'>{}{}\
         <button type='submit' class='switch{class}' role='switch' aria-checked='{checked}' aria-label='{label}' title='{label}'><span class='switch-thumb'></span></button>\
         <span>{label}</span></form>{hint}</div>",
        ctx.csrf_input(),
        hidden("notify", next)
    )
}

/// The "Clone projects without a local checkout" switch (saves on click).
fn clone_switch(ctx: &UiCtx, on: bool) -> String {
    let label = escape(&ctx.t("Clone projects without a local checkout"));
    let (class, checked, next) = if on { (" is-on", "true", "0") } else { ("", "false", "on") };
    format!(
        "<div class='mem-slack'><form method='post' action='/insights/memory/sync/clone' class='mem-switch'>{}{}\
         <button type='submit' class='switch{class}' role='switch' aria-checked='{checked}' aria-label='{label}' title='{label}'><span class='switch-thumb'></span></button>\
         <span>{label}</span></form></div>",
        ctx.csrf_input(),
        hidden("clone", next)
    )
}

/// What a managed project's header adds: that Memory reads its own clone, and a
/// Re-clone button that deletes it.
fn managed_note(ctx: &UiCtx, p: &Project) -> String {
    if !p.managed {
        return String::new();
    }
    format!(
        "<p class='mem-muted mem-hint'>{}</p>\
         <form method='post' action='/insights/memory/checkout/reclone' class='mem-inline'>{}{}\
         <button class='mem-btn' type='submit' title='{}'>{}{}</button></form>",
        escape(&ctx.t("Cloned for Memory: a read-only copy of this repository that the AI CLI reads like any checkout.")),
        ctx.csrf_input(),
        hidden("project", &p.alias),
        escape(&ctx.t("Delete the copy; the next build or refresh clones it again")),
        icon("autorenew"),
        escape(&ctx.t("Re-clone"))
    )
}

/// The project header's actions, plus the managed-clone note.
fn head_actions(ctx: &UiCtx, p: &Project, profile: Option<&Profile>) -> String {
    format!("{}{}", head_actions_inner(ctx, p, profile), managed_note(ctx, p))
}

/// The project header's Build (no profile yet) or Refresh (rebuild) button.
fn head_actions_inner(ctx: &UiCtx, p: &Project, profile: Option<&Profile>) -> String {
    if !p.checkout {
        return format!(
            "<p class='mem-muted'>{}</p>",
            escape(&ctx.t("Set this project's local checkout on the Projects page to build its profile."))
        );
    }
    if busy(ctx, &p.alias) {
        return format!(
            "<button class='mem-btn mem-btn-primary' type='button' disabled>{}{}</button>",
            spinner(),
            escape(&ctx.t("Working…"))
        );
    }
    let (icon_name, label) = if profile.is_some() {
        ("autorenew", ctx.t("Refresh profile"))
    } else {
        ("auto_awesome", ctx.t("Build profile"))
    };
    let hint = format!(
        "<p class='mem-muted mem-hint'>{}</p>",
        escape(&ctx.t("Scans the checkout and asks the AI CLI for a draft. Sections written by a person are kept."))
    );
    let fields = format!("{}{}", hidden("project", &p.alias), hidden("from", "project"));
    // Rebuilding an approved profile puts it back to Draft: ask first.
    if profile.is_some_and(|p| p.status == Status::Approved) {
        let back_to = project_url(&p.alias, None);
        return format!(
            "{}{hint}{}",
            confirm_opener("mem-build-project", &format!("{back_to}&confirm=build"), icon_name, &label),
            confirm_dialog(
                ctx,
                "mem-build-project",
                ctx.query().first("confirm") == Some("build"),
                &ctx.t("Rebuild this profile?"),
                &ctx.t("This runs one AI pass on this project and puts its approved profile back to Draft for review. Sections you edited are kept."),
                &fields,
                &back_to
            )
        );
    }
    format!(
        "<form method='post' action='/insights/memory/build' class='mem-inline'>{}{fields}\
         <button class='mem-btn mem-btn-primary' type='submit'>{}{}</button></form>{hint}",
        ctx.csrf_input(),
        icon(icon_name),
        escape(&label)
    )
}

/// The live record of a job working on `alias` (a build or refresh in any
/// process whose pid is alive), read fresh on every render.
fn live_job(ctx: &UiCtx, alias: &str) -> Option<loopx_core::profile_progress::JobView> {
    loopx_core::profile_progress::running_for(&crate::ui::profile_jobs::base(ctx), alias)
}

/// Whether a job works on `alias`: its live record, or this process's own guard
/// (held from the click until the job ends, also before the record is written).
fn busy(ctx: &UiCtx, alias: &str) -> bool {
    profile_build::is_running(alias) || live_job(ctx, alias).is_some()
}

/// Whether this process holds `alias`'s guide slot (from the click until the run ends).
fn guide_slot_held(alias: &str) -> bool {
    profile_build::is_running(&format!("guide:{alias}"))
}

/// The live record of a guide build or page rebuild working on `alias`.
fn live_guide_job(ctx: &UiCtx, alias: &str) -> Option<loopx_core::profile_progress::JobView> {
    loopx_core::profile_progress::running_guide_for(&crate::ui::profile_jobs::base(ctx), alias)
}

/// Whether a guide run works on `alias`. Apart from [`busy`]: a guide run only
/// disables the guide's own buttons, never the profile's, and the reverse.
fn guide_running(ctx: &UiCtx, alias: &str) -> bool {
    guide_slot_held(alias) || live_guide_job(ctx, alias).is_some()
}

/// A failed build or refresh is no longer shown once it is this old.
const FAILURE_SHOWN_FOR: Duration = Duration::hours(24);

/// "Working…" while a job works on the project (its live record first, so a page
/// opened again mid-run still says so), else the last build or refresh result;
/// a failure is dropped after [`FAILURE_SHOWN_FOR`].
fn job_line(ctx: &UiCtx, alias: &str) -> String {
    format!("{}{}", profile_job_line(ctx, alias), guide_job_line(ctx, alias))
}

/// The running strip both job lines use: `data-mem-job` or `data-mem-guide-job`
/// marks it for `live.js`, which moves the step text and the bar in place.
fn running_strip(ctx: &UiCtx, marker: &str, alias: &str, text: &str, live: Option<&loopx_core::profile_progress::JobView>) -> String {
    // Before the record is written, just "Starting".
    let (step, percent) = match live {
        Some(j) => (crate::ui::profile_jobs::step_detail(ctx, j), job_percent(j)),
        None => (ctx.t("Starting"), 0),
    };
    format!(
        "<div class='mem-job' role='status' {marker}='{}'>{}<div class='mem-job-body'><p>{}<br><span data-mem-step>{}</span></p>{}</div></div>",
        escape(alias),
        spinner(),
        escape(text),
        escape(&step),
        progress_bar("", percent, &step)
    )
}

/// A finished job's one line: a check or an error icon and the text.
fn finished_strip(ok: bool, text: &str) -> String {
    format!(
        "<div class='mem-job{}' role='status'>{}<p>{}</p></div>",
        if ok { "" } else { " mem-job-bad" },
        icon(if ok { "check_circle" } else { "error" }),
        escape(text)
    )
}

/// The guide's line: its running strip (the step it is at), else the last guide
/// run's page counts or fixed failure reason; a failure is dropped after
/// [`FAILURE_SHOWN_FOR`]. Failed pages are only counted, never named.
fn guide_job_line(ctx: &UiCtx, alias: &str) -> String {
    let live = live_guide_job(ctx, alias);
    if live.is_some() || guide_slot_held(alias) {
        let text = ctx.t("Building the guide… This page updates by itself while it runs.");
        return running_strip(ctx, "data-mem-guide-job", alias, &text, live.as_ref());
    }
    let Some(job) = last_in(&ctx.state.home_dir, alias, true) else {
        return String::new();
    };
    let when = ago(ctx, job.at);
    match &job.outcome {
        JobOutcome::Failed(e) if ctx.state.now() - job.at < FAILURE_SHOWN_FOR => finished_strip(
            false,
            &ctx.tf("The last guide build failed {when}: {error}", &[("when", &when), ("error", &shown_failure(ctx, e))]),
        ),
        JobOutcome::GuideBuilt { saved, held, failed } => finished_strip(
            true,
            &ctx.tf(
                "Guide built {when}: {saved} pages written, {held} waiting for approval, {failed} failed.",
                &[
                    ("when", &when),
                    ("saved", &saved.to_string()),
                    ("held", &held.to_string()),
                    ("failed", &failed.to_string()),
                ],
            ),
        ),
        _ => String::new(),
    }
}

/// The profile's line (see [`job_line`]).
fn profile_job_line(ctx: &UiCtx, alias: &str) -> String {
    let live = live_job(ctx, alias);
    if live.is_some() || profile_build::is_running(alias) {
        // What the running job is doing now, from its live record.
        let text = ctx.t("Working on this profile… This page updates by itself while it runs.");
        return running_strip(ctx, "data-mem-job", alias, &text, live.as_ref());
    }
    let Some(job) = last_job(&ctx.state.home_dir, alias) else {
        return String::new();
    };
    if matches!(job.outcome, JobOutcome::Failed(_)) && ctx.state.now() - job.at >= FAILURE_SHOWN_FOR {
        return String::new();
    }
    let when = ago(ctx, job.at);
    let section = |k: SectionKey| section_title(ctx, k);
    let error = |e: &str| shown_failure(ctx, e);
    let (ok, text) = match (&job.kind, &job.outcome) {
        (JobKind::Build, JobOutcome::Failed(e)) => (
            false,
            ctx.tf("The last build failed {when}: {error}", &[("when", &when), ("error", &error(e))]),
        ),
        (JobKind::Build, _) => (true, ctx.tf("Profile built {when}. Read it before agents rely on it.", &[("when", &when)])),
        (JobKind::Refresh(k), JobOutcome::Proposed) => (
            true,
            ctx.tf("Proposal raised for {section} {when}. Review it under Proposals.", &[("section", &section(*k)), ("when", &when)]),
        ),
        (JobKind::Refresh(k), JobOutcome::Failed(e)) => (
            false,
            ctx.tf(
                "Refreshing {section} failed {when}: {error}",
                &[("section", &section(*k)), ("when", &when), ("error", &error(e))],
            ),
        ),
        (JobKind::Refresh(k), _) => (
            true,
            ctx.tf("Nothing new to propose for {section} ({when}).", &[("section", &section(*k)), ("when", &when)]),
        ),
        // Guide results live in their own lane (see `guide_job_line`).
        (JobKind::Guide, _) => return String::new(),
    };
    finished_strip(ok, &text)
}

/// A section card's Edit (inline editor) and Refresh this section. `s` is `None`
/// for a missing section and for one whose text is hidden as a secret: the editor
/// then starts empty and never carries the hidden text.
fn section_actions(ctx: &UiCtx, p: &Project, key: SectionKey, s: Option<&Section>) -> String {
    let csrf = ctx.csrf_input();
    let slug = key.slug();
    let ids = format!("{}-{slug}", p.alias.bytes().filter(u8::is_ascii_alphanumeric).map(char::from).collect::<String>());
    let refresh = if !p.checkout {
        String::new()
    } else if busy(ctx, &p.alias) {
        format!(
            "<button class='mem-btn' type='button' disabled>{}{}</button>",
            spinner(),
            escape(&ctx.t("Working…"))
        )
    } else {
        format!(
            "<form method='post' action='/insights/memory/section/refresh' class='mem-inline mem-refresh'>{csrf}{}{}\
             <button class='mem-btn' type='submit' title='{}'>{}{}</button></form>",
            hidden("project", &p.alias),
            hidden("section", slug),
            escape(&ctx.t("Ask the AI CLI for an update. It waits under Proposals; nothing changes until you approve it.")),
            icon("autorenew"),
            escape(&ctx.t("Refresh this section"))
        )
    };
    let draft = kept_draft(&ctx.state.home_dir, &p.alias, key, ctx.state.now());
    let (summary, body) = match &draft {
        Some((summary, body)) => (summary.as_str(), body.as_str()),
        None => s.map_or(("", ""), |s| (s.summary.as_str(), s.body.as_str())),
    };
    format!(
        "<details class='mem-edit'{}><summary class='mem-btn'>{}{}</summary>\
         <form method='post' action='/insights/memory/section/save' class='mem-editor'>{csrf}{}{}\
         <label class='mem-label' for='mem-sum-{ids}'>{}</label>\
         <input class='mem-input' id='mem-sum-{ids}' name='summary' maxlength='{SUMMARY_MAX}' value='{}'>\
         <label class='mem-label' for='mem-body-{ids}'>{}</label>\
         <textarea class='mem-textarea' id='mem-body-{ids}' name='body' maxlength='{BODY_MAX}'>{}</textarea>\
         <div class='mem-editor-foot'><p class='mem-muted'>{}</p>\
         <button class='mem-btn mem-btn-primary' type='submit'>{}{}</button></div></form></details>{refresh}",
        if draft.is_some() { " open" } else { "" },
        icon("edit"),
        escape(&ctx.t("Edit")),
        hidden("project", &p.alias),
        hidden("section", slug),
        escape(&ctx.t("Summary (one line)")),
        escape(summary),
        escape(&ctx.t("Details (Markdown)")),
        escape(body),
        escape(&ctx.t("Saving marks this section as written by a person; later syncs only propose changes to it.")),
        icon("save"),
        escape(&ctx.t("Save"))
    )
}

/// Approve / Dismiss on a pending proposal; Apply on an approved one that was not
/// applied and still fits the section.
fn proposal_actions(ctx: &UiCtx, alias: &str, a: &Value, profile: Option<&Profile>) -> String {
    let Some(id) = a["id"].as_str().filter(|id| valid_id(id)) else {
        return String::new();
    };
    let Some(key) = proposal_key(a, alias) else {
        return String::new();
    };
    let buttons = match a["status"].as_str() {
        Some("pending") => format!(
            "<button class='mem-btn' type='submit' name='decision' value='dismiss'>{}{}</button>\
             <button class='mem-btn mem-btn-primary' type='submit' name='decision' value='approve'>{}{}</button>",
            icon("close"),
            escape(&ctx.t("Dismiss")),
            icon("check"),
            escape(&ctx.t("Approve"))
        ),
        Some("approved") => {
            let applied = profile_sync::is_applied(alias, id, Some(&memory_root(ctx)));
            if applied || !profile.is_some_and(|p| still_applies(p, key, a)) {
                return String::new();
            }
            format!(
                "<button class='mem-btn mem-btn-primary' type='submit' name='decision' value='approve'>{}{}</button>",
                icon("check"),
                escape(&ctx.t("Apply"))
            )
        }
        _ => return String::new(),
    };
    format!(
        "<form method='post' action='/insights/memory/proposal/decide' class='mem-inline mem-decide'>{}{}{}{buttons}</form>",
        ctx.csrf_input(),
        hidden("project", alias),
        hidden("id", id)
    )
}

/// The graph view's own styles (only on `&graph=1`). The stage has its own
/// light and dark palette (`--mg-*`) that follows the card around it: the named
/// accents wash every card light in both modes (the `dark` accent, dark in
/// both), and only the `default` accent follows the colour mode, so the stage is
/// never a black well in a light card or a white one in a dark card.
const GRAPH_STYLE: &str = "<style>
.mem-graph{--mg-bg:#141416;--mg-fg:#F5F5F7;--mg-muted:#AEAEB2;--mg-hairline:rgba(255,255,255,.1);--mg-node:#2C2C2E;--mg-node-off:#232325;--mg-track:rgba(255,255,255,.16);--mg-panel:rgba(40,40,42,.78);--mg-focus:#0A84FF;
--mg-fresh:#30D158;--mg-stale:#FF9F0A;--mg-review:#0A84FF;--mg-human:#BF5AF2;--mg-unchecked:#8E8E93;--mg-bad:#FF453A;--mg-none:#636366;
--mg-depends-on:#0A84FF;--mg-calls:#64D2FF;--mg-declared:#AEAEB2;--mg-issue-link:#FFD60A;--mg-shares-secret:#FF6961;--mg-shares-service:#DA8FFF;--mg-shares-people:#8E8E93;
display:flex;flex-direction:column;gap:.9rem;min-width:0}
@media (prefers-color-scheme:light){:root:is([data-accent=\"default\"],:not([data-accent])):not([data-color-mode=\"dark\"]) .mem-graph{--mg-bg:#FFFFFF;--mg-fg:#1D1D1F;--mg-muted:#6E6E73;--mg-hairline:rgba(0,0,0,.1);--mg-node:#F5F5F7;--mg-node-off:#ECECEF;--mg-track:rgba(0,0,0,.1);--mg-panel:rgba(255,255,255,.78);--mg-focus:#007AFF;
--mg-fresh:#248A3D;--mg-stale:#C93400;--mg-review:#007AFF;--mg-human:#8944AB;--mg-unchecked:#8E8E93;--mg-bad:#D70015;--mg-none:#AEAEB2;
--mg-depends-on:#007AFF;--mg-calls:#0071A4;--mg-declared:#6E6E73;--mg-issue-link:#B25000;--mg-shares-secret:#D70015;--mg-shares-service:#8944AB;--mg-shares-people:#8E8E93}}
:root:is([data-accent=\"default\"],:not([data-accent]))[data-color-mode=\"light\"] .mem-graph,:root[data-accent=\"indigo\"] .mem-graph,:root[data-accent=\"blue\"] .mem-graph,:root[data-accent=\"green\"] .mem-graph,:root[data-accent=\"red\"] .mem-graph,:root[data-accent=\"gray\"] .mem-graph{--mg-bg:#FFFFFF;--mg-fg:#1D1D1F;--mg-muted:#6E6E73;--mg-hairline:rgba(0,0,0,.1);--mg-node:#F5F5F7;--mg-node-off:#ECECEF;--mg-track:rgba(0,0,0,.1);--mg-panel:rgba(255,255,255,.78);--mg-focus:#007AFF;
--mg-fresh:#248A3D;--mg-stale:#C93400;--mg-review:#007AFF;--mg-human:#8944AB;--mg-unchecked:#8E8E93;--mg-bad:#D70015;--mg-none:#AEAEB2;
--mg-depends-on:#007AFF;--mg-calls:#0071A4;--mg-declared:#6E6E73;--mg-issue-link:#B25000;--mg-shares-secret:#D70015;--mg-shares-service:#8944AB;--mg-shares-people:#8E8E93}
.mg-k-depends-on{--k:var(--mg-depends-on)}.mg-k-calls{--k:var(--mg-calls)}.mg-k-declared{--k:var(--mg-declared)}.mg-k-issue-link{--k:var(--mg-issue-link)}
.mg-k-shares-secret{--k:var(--mg-shares-secret)}.mg-k-shares-service{--k:var(--mg-shares-service)}.mg-k-shares-people{--k:var(--mg-shares-people)}
.mg-v-fresh{--v:var(--mg-fresh)}.mg-v-stale{--v:var(--mg-stale)}.mg-v-review{--v:var(--mg-review)}.mg-v-human{--v:var(--mg-human)}.mg-v-unchecked{--v:var(--mg-unchecked)}.mg-v-bad{--v:var(--mg-bad)}.mg-v-none{--v:var(--mg-none)}
.mg-head{display:flex;flex-direction:column;gap:.7rem;min-width:0}
.mg-title{min-width:0}
.mg-title .section-header{margin-bottom:.2rem}
.mg-hint,.mg-narrow-note{font-size:.84rem}
.mg-narrow-note{display:none}
.mg-filters{display:flex;flex-wrap:wrap;gap:.4rem;min-width:0}
.mg-filters[hidden],.mg-zoom[hidden],.mg-hint[hidden]{display:none}
.mg-chip{display:inline-flex;align-items:center;gap:.4rem;height:28px;box-sizing:border-box;flex:0 0 auto;margin:0;padding:0 .7rem 0 .55rem;border:1px solid var(--md-outline-variant);border-radius:999px;background:transparent;color:var(--md-on-surface);font:inherit;font-size:.8rem;font-weight:500;line-height:1;white-space:nowrap;cursor:pointer}
.mg-chip:hover{background:var(--md-surface-container-high)}
.mg-chip[aria-pressed='true']{background:var(--md-surface-container-highest);border-color:transparent}
.mg-chip[aria-pressed='false']{color:var(--md-on-surface-variant)}
.mg-chip[aria-pressed='false'] .mg-swatch{opacity:.35}
.mg-chip:focus-visible,.mg-zoom button:focus-visible,.mg-close:focus-visible,.mg-goto:focus-visible{outline:2px solid var(--md-primary);outline-offset:2px}
.mg-chip-count{min-width:1.1rem;padding:0 .3rem;height:18px;box-sizing:border-box;display:inline-grid;place-items:center;border-radius:999px;background:var(--md-surface);color:var(--md-on-surface-variant);font-size:.7rem;font-variant-numeric:tabular-nums}
.mg-swatch{width:24px;height:8px;flex:0 0 24px}
.mg-swatch line,.mg-edge{fill:none;stroke:var(--k);stroke-linecap:round}
.mg-swatch line{stroke-width:2.5}
.mg-note{display:flex;align-items:flex-start;gap:.5rem;margin:0;padding:.6rem .85rem;border-radius:12px;background:var(--md-surface-container-high);color:var(--md-on-surface);font-size:.85rem;line-height:1.45}
.mg-note .icon{width:16px;height:16px;flex:0 0 16px;margin-top:.1rem}
.mg-stage{position:relative;height:clamp(420px,66vh,760px);min-width:0;overflow:hidden;border-radius:14px;border:1px solid var(--mg-hairline);background:var(--mg-bg);color:var(--mg-fg)}
.mg-svg{display:block;width:100%;height:100%;touch-action:none;user-select:none;-webkit-user-select:none;cursor:grab}
.mg-svg.is-panning{cursor:grabbing}
.mg-svg:focus{outline:none}
.mg-edge{stroke-width:2.2;opacity:.78}
.mg-edge.mg-k-issue-link{stroke-dasharray:8 6}
.mg-chip.mg-k-issue-link .mg-swatch line{stroke-dasharray:6 4}
.mg-edge.mg-k-shares-secret,.mg-edge.mg-k-shares-service,.mg-edge.mg-k-shares-people{stroke-width:2.8;stroke-dasharray:.1 7}
.mg-chip.mg-k-shares-secret .mg-swatch line,.mg-chip.mg-k-shares-service .mg-swatch line,.mg-chip.mg-k-shares-people .mg-swatch line{stroke-width:3;stroke-dasharray:.1 5}
.mg-arrow{fill:var(--k);stroke:none}
.mg-hit{fill:none;stroke:transparent;stroke-width:16;pointer-events:stroke;cursor:pointer}
.mg-hide-depends-on .mg-k-depends-on:is(.mg-edge,.mg-row,.mg-hit),.mg-hide-calls .mg-k-calls:is(.mg-edge,.mg-row,.mg-hit),.mg-hide-declared .mg-k-declared:is(.mg-edge,.mg-row,.mg-hit),.mg-hide-issue-link .mg-k-issue-link:is(.mg-edge,.mg-row,.mg-hit),
.mg-hide-shares-secret .mg-k-shares-secret:is(.mg-edge,.mg-row,.mg-hit),.mg-hide-shares-service .mg-k-shares-service:is(.mg-edge,.mg-row,.mg-hit),.mg-hide-shares-people .mg-k-shares-people:is(.mg-edge,.mg-row,.mg-hit){display:none}
.mg-hide-depends-on .mg-row-depends-on,.mg-hide-calls .mg-row-calls,.mg-hide-declared .mg-row-declared,.mg-hide-issue-link .mg-row-issue-link,.mg-hide-shares-secret .mg-row-shares-secret,.mg-hide-shares-service .mg-row-shares-service,.mg-hide-shares-people .mg-row-shares-people{display:none}
.mg-node{cursor:pointer;outline:none}
.mg-halo{fill:none;stroke:transparent;stroke-width:5}
.mg-core{fill:var(--mg-node)}
.mg-track{fill:none;stroke:var(--mg-track);stroke-width:4.5}
.mg-ring{fill:none;stroke:var(--v);stroke-width:4.5;stroke-linecap:round}
.mg-count{fill:var(--mg-fg);font:600 15px var(--font-family-stack);text-anchor:middle;pointer-events:none}
.mg-label{fill:var(--mg-fg);font:500 19px var(--font-family-stack);text-anchor:middle;paint-order:stroke;stroke:var(--mg-bg);stroke-width:6px;stroke-linejoin:round;pointer-events:none}
.mg-v-none .mg-core,.mg-v-bad .mg-core{fill:var(--mg-node-off)}
.mg-v-none .mg-track{stroke:var(--v);stroke-dasharray:3 5}
.mg-v-bad .mg-track{stroke:var(--v)}
.mg-v-none .mg-count,.mg-v-none .mg-label{fill:var(--mg-muted)}
.mg-node:hover .mg-halo,.mg-node.is-lit .mg-halo{stroke:var(--mg-hairline)}
.mg-node:focus-visible .mg-halo{stroke:var(--mg-focus)}
.mg-node.is-selected .mg-halo{stroke:var(--mg-focus);stroke-width:4}
.mg-svg.is-focusing .mg-edge{opacity:.1}
.mg-svg.is-focusing .mg-edge.is-lit{opacity:1;stroke-width:3.4}
.mg-svg.is-focusing .mg-node{opacity:.32}
.mg-svg.is-focusing .mg-node.is-lit{opacity:1}
.mg-wheel-hint{position:absolute;left:50%;top:50%;transform:translate(-50%,-50%);padding:.55rem 1rem;border-radius:999px;background:var(--mg-panel);-webkit-backdrop-filter:blur(20px) saturate(1.6);backdrop-filter:blur(20px) saturate(1.6);border:1px solid var(--mg-hairline);color:var(--mg-fg);font-size:.88rem;font-weight:500;pointer-events:none;opacity:0;box-shadow:0 8px 24px rgba(0,0,0,.18)}
.mg-wheel-hint.is-on{opacity:1}
.mg-zoom{position:absolute;left:12px;bottom:12px;display:flex;flex-direction:column;overflow:hidden;border-radius:12px;border:1px solid var(--mg-hairline);background:var(--mg-panel);-webkit-backdrop-filter:blur(20px) saturate(1.6);backdrop-filter:blur(20px) saturate(1.6);box-shadow:0 4px 16px rgba(0,0,0,.14)}
.mg-zoom button{display:grid;place-items:center;width:36px;height:36px;box-sizing:border-box;margin:0;padding:0;border:0;border-top:1px solid var(--mg-hairline);background:transparent;color:var(--mg-fg);font:500 1.15rem/1 var(--font-family-stack);cursor:pointer}
.mg-zoom button:first-child{border-top:0}
.mg-zoom button:hover{background:var(--mg-hairline)}
.mg-zoom .icon{width:16px;height:16px}
.mg-inspector{position:absolute;top:12px;right:12px;width:min(340px,calc(100% - 24px));max-height:calc(100% - 24px);box-sizing:border-box;overflow:auto;padding:1rem 1.1rem 1.1rem;border-radius:16px;border:1px solid var(--mg-hairline);background:var(--mg-panel);-webkit-backdrop-filter:blur(24px) saturate(1.6);backdrop-filter:blur(24px) saturate(1.6);box-shadow:0 18px 48px rgba(0,0,0,.24);color:var(--mg-fg);opacity:0;transform:translateX(12px)}
.mg-inspector[hidden]{display:none}
.mg-inspector.is-open{opacity:1;transform:none}
.mg-inspector:focus:not(:focus-visible){outline:none}
.mg-close{position:absolute;top:10px;right:10px;display:grid;place-items:center;width:28px;height:28px;box-sizing:border-box;margin:0;padding:0;border:0;border-radius:999px;background:var(--mg-hairline);color:var(--mg-fg);cursor:pointer}
.mg-close .icon{width:14px;height:14px}
.mg-insp-head{display:flex;flex-direction:column;align-items:flex-start;gap:.4rem;padding-right:2rem;min-width:0}
.mg-insp-head h3{margin:0;max-width:100%;font-size:1.1rem;overflow-wrap:anywhere}
.mg-insp-pills{display:flex;flex-wrap:wrap;gap:.35rem}
.mg-insp-sum,.mg-insp-ev{margin:.7rem 0 0;font-size:.88rem;line-height:1.45;overflow-wrap:anywhere}
.mg-insp-pair{display:flex;flex-wrap:wrap;align-items:center;gap:.4rem;margin:.7rem 0 0;font-size:.95rem;font-weight:600}
.mg-inspector h4{margin:1rem 0 .4rem;font-size:.72rem;font-weight:600;letter-spacing:.04em;text-transform:uppercase;color:var(--mg-muted)}
.mg-facts{display:grid;grid-template-columns:repeat(auto-fit,minmax(90px,1fr));gap:.5rem;margin:.85rem 0 0}
.mg-facts div{padding:.5rem .6rem;border-radius:10px;background:var(--mg-hairline)}
.mg-facts dt{font-size:.72rem;color:var(--mg-muted)}
.mg-facts dd{margin:.15rem 0 0;font-size:.95rem;font-weight:600;font-variant-numeric:tabular-nums}
.mg-insp-links,.mg-list-links{display:flex;flex-direction:column;gap:.45rem;margin:0;padding:0;list-style:none}
.mg-row{display:flex;flex-wrap:wrap;align-items:center;gap:.25rem .45rem;min-width:0;font-size:.85rem}
.mg-row small{flex:1 1 100%;color:var(--mg-muted);font-size:.78rem;line-height:1.4;overflow-wrap:anywhere}
.mg-kind{display:inline-flex;align-items:center;gap:.3rem;height:20px;box-sizing:border-box;padding:0 .5rem;border-radius:999px;border:1px solid var(--k);color:inherit;font-size:.7rem;font-weight:600;white-space:nowrap}
button.mg-kind{margin:0;background:transparent;font-family:inherit;cursor:pointer}
button.mg-kind:hover{background:color-mix(in srgb,var(--k) 14%,transparent)}
button.mg-kind:focus-visible{outline:2px solid var(--mg-focus);outline-offset:2px}
.mg-inspector:focus-visible{outline:2px solid var(--mg-focus);outline-offset:-2px}
.mg-kind::before{content:'';width:6px;height:6px;border-radius:50%;background:var(--k)}
.mg-dir{color:var(--mg-muted)}
.mg-goto{margin:0;padding:0;border:0;background:none;color:inherit;font:inherit;font-weight:600;cursor:pointer;text-decoration:underline;text-decoration-color:var(--mg-hairline);text-underline-offset:3px;overflow-wrap:anywhere}
.mg-goto:hover{text-decoration-color:currentColor}
.mg-open{margin-top:1.1rem}
.mg-legend{display:flex;flex-wrap:wrap;align-items:center;gap:.35rem 1rem;font-size:.8rem;color:var(--md-on-surface-variant)}
.mg-legend-title{font-weight:600}
.mg-legend span{display:inline-flex;align-items:center;gap:.35rem}
.mg-legend i{width:10px;height:10px;box-sizing:border-box;border-radius:50%;border:2.5px solid var(--v)}
.mg-legend .mg-v-none i{border-style:dashed}
.mem-graph-list summary{display:flex;flex-wrap:wrap;align-items:baseline;gap:.25rem .75rem;cursor:pointer;font-weight:600}
.mem-graph-list summary small{font-weight:400;color:var(--md-on-surface-variant)}
.mg-list{display:grid;gap:.25rem;margin:.75rem 0 0;padding:0;list-style:none}
.mg-list>li{display:flex;flex-direction:column;gap:.45rem;padding:.75rem 0;border-top:1px solid var(--md-outline-variant);min-width:0}
.mg-list>li:first-child{border-top:0}
.mg-list-head{display:flex;flex-wrap:wrap;align-items:center;gap:.5rem;min-width:0}
.mg-list-head a{font-weight:600;color:var(--md-on-surface);text-decoration:none;overflow-wrap:anywhere}
.mg-list-head a:hover{text-decoration:underline}
.mg-list-links .mg-row a{color:var(--md-primary);font-weight:500;text-decoration:none;overflow-wrap:anywhere}
.mg-list-links .mg-row small,.mg-list-links .mg-dir{color:var(--md-on-surface-variant)}
.mem-graph-list .mg-kind{border-color:color-mix(in srgb,var(--k) 70%,transparent)}
.mem-graph-listcard{--mg-depends-on:#0A84FF;--mg-calls:#5AC8DA;--mg-declared:#8E8E93;--mg-issue-link:#FF9F0A;--mg-shares-secret:#FF6961;--mg-shares-service:#BF5AF2;--mg-shares-people:#8E8E93}
@media (prefers-reduced-motion:no-preference){
.mg-inspector{transition:opacity .28s cubic-bezier(.32,.72,0,1),transform .28s cubic-bezier(.32,.72,0,1)}
.mg-edge,.mg-node{transition:opacity .2s cubic-bezier(.32,.72,0,1)}
.mg-wheel-hint{transition:opacity .25s cubic-bezier(.32,.72,0,1)}
.mg-chip{transition:background-color .2s cubic-bezier(.32,.72,0,1),color .2s cubic-bezier(.32,.72,0,1)}
}
@media (max-width:639px){
.mg-stage,.mg-legend,.mg-hint{display:none}
.mg-narrow-note{display:block}
.mg-chip{height:26px;padding:0 .55rem 0 .45rem;font-size:.75rem}
.mg-swatch{width:16px;flex-basis:16px}
}
</style>";

const STYLE: &str = "<style>
.mem-stack{display:flex;flex-direction:column;gap:1.25rem;min-width:0}
.mem-bar{display:flex;flex-wrap:wrap;align-items:center;gap:.75rem 1rem;min-width:0}
.mem-sync{display:inline-flex;flex-wrap:wrap;align-items:center;gap:.3rem .5rem;min-width:0;font-size:.85rem;color:var(--md-on-surface-variant)}
.mem-sync .icon{width:16px;height:16px;flex:0 0 16px}
.mem-dot{opacity:.6}
.mem-seg{display:inline-flex;flex:0 0 auto;padding:2px;border-radius:999px;background:var(--md-surface-container-highest)}
.mem-seg a{display:inline-flex;align-items:center;height:30px;box-sizing:border-box;padding:0 1rem;border-radius:999px;color:var(--md-on-surface-variant);text-decoration:none;font-size:.85rem;font-weight:500}
.mem-seg a.is-active{background:var(--md-surface);color:var(--md-on-surface);box-shadow:0 1px 3px rgba(0,0,0,.18)}
.mem-grid{display:grid;gap:1.25rem;grid-template-columns:repeat(auto-fill,minmax(min(100%,280px),1fr))}
a.mem-card{display:flex;flex-direction:column;gap:.75rem;color:inherit;text-decoration:none;border:1px solid transparent}
a.mem-card:hover{border-color:var(--md-outline-variant);box-shadow:var(--shadow-2)}
a.mem-card:focus-visible{outline:2px solid var(--md-primary);outline-offset:2px}
.mem-card-top{display:flex;align-items:center;gap:.85rem;min-width:0}
.mem-card-name{display:flex;flex-direction:column;align-items:flex-start;gap:.3rem;min-width:0;flex:1 1 auto}
.mem-card-name h3{margin:0;max-width:100%;font-size:1.05rem;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.mem-card-overview{margin:0;min-height:2.9em;font-size:.9rem;line-height:1.45;color:var(--md-on-surface-variant);display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;overflow:hidden;overflow-wrap:anywhere}
.mem-chips{display:flex;flex-wrap:wrap;gap:.35rem;margin-top:auto}
.mem-chip{display:inline-flex;align-items:center;gap:.3rem;height:22px;box-sizing:border-box;padding:0 .6rem;border-radius:999px;background:var(--md-surface-container-highest);color:var(--md-on-surface-variant);font-size:.72rem;font-weight:500;white-space:nowrap}
.mem-chip .icon{width:13px;height:13px;flex:0 0 13px}
.mem-chip-accent{background:var(--md-primary-container);color:var(--md-on-primary-container)}
.mem-chip-warn{background:var(--md-warning-container);color:var(--md-on-warning-container)}
.mem-chip-bad{background:var(--md-error-container);color:var(--md-on-error-container)}
.mem-ring{--p:0;--mem-ring:var(--md-success);position:relative;flex:0 0 42px;width:42px;height:42px;border-radius:50%;display:grid;place-items:center;background:conic-gradient(var(--mem-ring) calc(var(--p) * 1%),var(--md-surface-container-highest) 0)}
.mem-ring::before{content:'';position:absolute;inset:4px;border-radius:50%;background:var(--md-nav-surface)}
.mem-ring span{position:relative;font-size:.78rem;font-weight:600;color:var(--md-on-surface)}
.mem-ring-stale{--mem-ring:var(--md-warning)}
.mem-ring-review{--mem-ring:var(--md-primary)}
.mem-ring-bad{--mem-ring:var(--md-error)}
.mem-pill{display:inline-flex;align-items:center;height:22px;box-sizing:border-box;padding:0 .6rem;border-radius:999px;font-size:.72rem;font-weight:600;white-space:nowrap;flex:0 0 auto}
.mem-pill-fresh{background:var(--md-success-container);color:var(--md-on-success-container)}
.mem-pill-stale{background:var(--md-warning-container);color:var(--md-on-warning-container)}
.mem-pill-review{background:var(--md-primary-container);color:var(--md-on-primary-container)}
.mem-pill-human{background:var(--md-nav-active-surface);color:var(--md-nav-active-on-surface)}
.mem-pill-none{background:var(--md-surface-container-highest);color:var(--md-on-surface-variant)}
.mem-pill-bad{background:var(--md-error-container);color:var(--md-on-error-container)}
.mem-crumbs{display:flex;flex-wrap:wrap;align-items:center;gap:.4rem;margin:0 0 .35rem;font-size:.85rem;color:var(--md-on-surface-variant)}
.mem-crumbs a{color:inherit;font-weight:500;text-decoration:none}
.mem-crumbs a:hover{text-decoration:underline}
.mem-crumb-sep{opacity:.5}
.mem-layout{display:grid;grid-template-columns:210px minmax(0,1fr);gap:1.25rem;align-items:start}
.card.mem-subnav{position:sticky;top:1rem;display:flex;flex-direction:column;gap:2px;min-width:0;padding:.45rem;border-radius:14px}
.mem-subnav a{display:flex;align-items:center;gap:.55rem;height:32px;box-sizing:border-box;padding:0 .65rem;border-radius:8px;color:var(--md-on-surface);text-decoration:none;font-size:.88rem;white-space:nowrap}
.mem-subnav a span{min-width:0;overflow:hidden;text-overflow:ellipsis}
.mem-subnav a:hover{background:var(--md-surface-container-high)}
.mem-subnav a.is-active{background:var(--md-nav-active-surface);color:var(--md-nav-active-on-surface);font-weight:600}
.mem-subnav .icon{width:16px;height:16px;flex:0 0 16px;opacity:.85}
.mem-subnav-badge{margin-left:auto;min-width:20px;height:20px;box-sizing:border-box;padding:0 .4rem;border-radius:999px;display:inline-grid;place-items:center;background:var(--md-primary);color:var(--md-on-primary);font-size:.7rem;font-weight:600}
.mem-subnav-sep{display:block;height:1px;margin:.3rem .5rem;background:var(--md-outline-variant)}
.mem-content{display:flex;flex-direction:column;gap:1.25rem;min-width:0}
.mem-banner{display:flex;align-items:flex-start;gap:.7rem;padding:.85rem 1.1rem;border-radius:14px;background:var(--md-warning-container);color:var(--md-on-warning-container)}
.mem-banner p{margin:0;line-height:1.45}
.mem-banner .icon{width:20px;height:20px;flex:0 0 20px}
.mem-banner p{flex:1 1 auto}
.mem-banner-action{flex:0 0 auto;align-self:center}
.mem-banner-action .icon{width:16px;height:16px;flex:0 0 16px}
.mem-head dl{display:grid;grid-template-columns:max-content minmax(0,1fr);gap:.5rem 1.25rem;margin:0;font-size:.9rem}
.mem-head dt{color:var(--md-on-surface-variant)}
.mem-head dd{margin:0;overflow-wrap:anywhere}
.mem-pair{display:grid;gap:1.25rem;grid-template-columns:repeat(auto-fit,minmax(min(100%,280px),1fr))}
.mem-sections{display:grid;gap:1.25rem;grid-template-columns:repeat(auto-fill,minmax(min(100%,260px),1fr))}
.mem-section{display:flex;flex-direction:column;gap:.5rem}
.mem-section-head{display:flex;align-items:center;justify-content:space-between;gap:.5rem;min-width:0}
.mem-section-head h3{margin:0;min-width:0;font-size:1rem;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.mem-summary{margin:0;min-height:2.9em;font-size:.9rem;line-height:1.45;color:var(--md-on-surface-variant);display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;overflow:hidden;overflow-wrap:anywhere}
.mem-summary-empty{font-style:italic;opacity:.8}
.mem-more{display:inline-flex;align-items:center;align-self:flex-start;gap:.25rem;height:28px;margin-top:auto;color:var(--md-primary);font-size:.85rem;font-weight:500;text-decoration:none}
.mem-more:hover{text-decoration:underline}
.mem-more .icon{width:14px;height:14px}
.mem-links{display:flex;flex-wrap:wrap;gap:.5rem}
.mem-link{display:inline-flex;align-items:center;gap:.4rem;height:30px;box-sizing:border-box;max-width:100%;padding:0 .8rem;border-radius:999px;border:1px solid var(--md-outline-variant);color:var(--md-on-surface);text-decoration:none;font-size:.85rem}
.mem-link:hover{background:var(--md-surface-container-high)}
.mem-link .icon{width:14px;height:14px;color:var(--md-primary)}
.mem-link span{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.mem-link small{color:var(--md-on-surface-variant);font-size:.75rem;white-space:nowrap}
.mem-count{margin:0 0 .25rem;font-size:2rem;font-weight:600;line-height:1}
.mem-muted{margin:0;color:var(--md-on-surface-variant);font-size:.88rem;line-height:1.45}
.mem-meta{margin:0 0 .75rem;color:var(--md-on-surface-variant);font-size:.82rem}
.mem-detail-head{display:flex;flex-wrap:wrap;align-items:center;gap:.6rem;margin-bottom:.35rem}
.mem-detail-head h2{margin:0}
.mem-lede{margin:0 0 .75rem;font-weight:500;line-height:1.45;overflow-wrap:anywhere}
.mem-body{overflow-wrap:anywhere}
.mem-file{margin-left:auto;font:.78rem ui-monospace,SFMono-Regular,Menlo,monospace;color:var(--md-on-surface-variant)}
.mem-stale,.mem-redacted{display:flex;align-items:flex-start;gap:.5rem;margin:0 0 .75rem;padding:.6rem .85rem;border-radius:12px;font-size:.85rem;line-height:1.45;overflow-wrap:anywhere}
.mem-stale{background:var(--md-warning-container);color:var(--md-on-warning-container)}
.mem-redacted{background:var(--md-error-container);color:var(--md-on-error-container)}
.mem-stale .icon,.mem-redacted .icon{width:16px;height:16px;flex:0 0 16px;margin-top:.1rem}
.mem-sources{margin:1rem 0 0;padding-top:.75rem;border-top:1px solid var(--md-outline-variant);font-size:.85rem}
.mem-sources summary{cursor:pointer;font-weight:500;color:var(--md-on-surface-variant)}
.mem-sources ul{display:grid;gap:.35rem;margin:.55rem 0 0;padding:0;list-style:none}
.mem-sources li{display:flex;flex-wrap:wrap;align-items:center;gap:.3rem .75rem;min-width:0}
.mem-sources code{overflow-wrap:anywhere}
.mem-sha{font:.78rem ui-monospace,SFMono-Regular,Menlo,monospace;color:var(--md-on-surface-variant)}
.mem-error{border:1px solid var(--md-error-container)}
.mem-error .section-header .icon{color:var(--md-error)}
.mem-pre{margin:.5rem 0 0;padding:.65rem .8rem;max-height:18rem;overflow:auto;border-radius:10px;background:var(--md-surface-container);font:.8rem/1.5 ui-monospace,SFMono-Regular,Menlo,monospace;white-space:pre-wrap;overflow-wrap:anywhere}
.mem-empty{display:flex;flex-direction:column;align-items:center;gap:.6rem;padding:2.5rem 1.5rem;text-align:center}
.mem-empty p{margin:0;max-width:34rem;color:var(--md-on-surface-variant)}
.mem-empty-icon .icon{width:32px;height:32px;color:var(--md-outline)}
.mem-empty .mem-more{align-self:center}
.mem-proposals{display:flex;flex-direction:column}
.mem-proposal{padding:1rem 0;border-top:1px solid var(--md-outline-variant)}
.mem-proposal:first-child{padding-top:.25rem;border-top:0}
.mem-proposal-head{display:flex;flex-wrap:wrap;align-items:center;gap:.4rem .75rem;margin-bottom:.6rem}
.mem-proposal-head h3{margin:0;font-size:1rem}
.mem-diff{display:grid;gap:.75rem;grid-template-columns:repeat(auto-fit,minmax(min(100%,260px),1fr))}
.mem-diff-col{min-width:0;padding:.75rem .9rem;border-radius:12px;border:1px solid var(--md-outline-variant)}
.mem-diff-old{background:color-mix(in srgb,var(--md-error-container) 30%,transparent)}
.mem-diff-new{background:color-mix(in srgb,var(--md-success-container) 30%,transparent)}
.mem-diff-col h4{margin:0 0 .35rem;font-size:.72rem;font-weight:600;letter-spacing:.04em;text-transform:uppercase;color:var(--md-on-surface-variant)}
.mem-diff-sum{margin:0;font-size:.88rem;font-weight:500;overflow-wrap:anywhere}
.mem-actions{display:flex;flex-wrap:wrap;align-items:center;gap:.5rem .6rem;min-width:0}
.mem-actions form{margin:0}
.mem-bar .mem-actions{margin-left:auto}
.mem-inline{display:inline-flex;align-items:center;gap:.5rem}
.mem-switch{display:inline-flex;align-items:center;gap:.5rem;height:32px;font-size:.85rem;font-weight:500;color:var(--md-on-surface)}
.mem-schedule{display:inline-flex;align-items:center;gap:.4rem}
.mem-slack{display:inline-flex;flex-wrap:wrap;align-items:center;gap:.15rem .6rem;min-width:0}
.mem-slack-hint{display:inline-flex;align-items:center;min-height:24px;font-size:.78rem;line-height:1.3;color:var(--md-on-surface-variant);text-decoration:none;border-radius:6px;transition:color .25s cubic-bezier(0.32,0.72,0,1)}
.mem-slack-hint:hover,.mem-slack-hint:focus-visible{color:var(--md-primary);text-decoration:underline}
.mem-select{width:auto;height:32px;box-sizing:border-box;flex:0 0 auto;margin:0;padding:0 .6rem;border:1px solid var(--md-outline-variant);border-radius:10px;background:var(--md-surface);color:var(--md-on-surface);font:inherit;font-size:.85rem;cursor:pointer}
.mem-btn{display:inline-flex;align-items:center;gap:.4rem;height:32px;box-sizing:border-box;flex:0 0 auto;margin:0;padding:0 .95rem;border:1px solid var(--md-outline-variant);border-radius:999px;background:var(--md-surface);color:var(--md-on-surface);font:inherit;font-size:.85rem;font-weight:500;line-height:1;white-space:nowrap;cursor:pointer;list-style:none;user-select:none}
.mem-btn::-webkit-details-marker{display:none}
.mem-btn:hover{background:var(--md-surface-container-high)}
.mem-btn .icon{width:15px;height:15px;flex:0 0 15px}
.mem-btn-primary{background:var(--md-primary);border-color:var(--md-primary);color:var(--md-on-primary)}
.mem-btn-primary:hover{background:var(--md-primary);filter:brightness(1.08)}
.mem-btn[disabled]{opacity:.7;cursor:default;filter:none}
.mem-btn:focus-visible,.mem-select:focus-visible{outline:2px solid var(--md-primary);outline-offset:2px}
.mem-head .mem-actions{margin-top:1rem;padding-top:.9rem;border-top:1px solid var(--md-outline-variant)}
.mem-hint{font-size:.82rem}
.mem-wiki-list{list-style:none;margin:0 0 .75rem;padding:0;display:flex;flex-direction:column;gap:.4rem}
.mem-wiki-list li{display:flex;flex-wrap:wrap;align-items:center;gap:.4rem .75rem}
.mem-wiki-list a{color:var(--md-primary);text-decoration:none;font-weight:500}
.mem-wiki-list a:hover{text-decoration:underline}
.mem-detail .mem-actions{margin:0 0 .9rem}
.mem-refresh{order:1}
.mem-edit{order:2;min-width:0}
.mem-edit[open]{flex:1 1 100%}
.mem-edit[open]>summary{background:var(--md-surface-container-high)}
.mem-editor{display:flex;flex-direction:column;gap:.45rem;margin-top:.75rem;padding:1rem;border:1px solid color-mix(in srgb,var(--md-outline) 30%,transparent);border-radius:14px;background:transparent}
.mem-label{font-size:.78rem;font-weight:600;color:var(--md-on-surface-variant)}
.mem-label+.mem-input,.mem-label+.mem-textarea{margin-bottom:.35rem}
.mem-input{display:block;width:100%;height:36px;box-sizing:border-box;flex:none;margin:0;padding:0 .75rem;border:1px solid color-mix(in srgb,var(--md-outline) 45%,transparent);border-radius:10px;background:var(--md-surface-container-lowest);color:var(--md-on-surface);font:inherit;font-size:.9rem}
.mem-textarea{display:block;width:100%;height:260px;min-height:120px;box-sizing:border-box;flex:none;margin:0;padding:.6rem .75rem;border:1px solid color-mix(in srgb,var(--md-outline) 45%,transparent);border-radius:10px;background:var(--md-surface-container-lowest);color:var(--md-on-surface);font:.84rem/1.5 ui-monospace,SFMono-Regular,Menlo,monospace;resize:vertical}
.mem-input:focus,.mem-textarea:focus{outline:2px solid var(--md-primary);outline-offset:0;border-color:transparent}
.mem-editor-foot{display:flex;flex-wrap:wrap;align-items:center;justify-content:space-between;gap:.6rem 1rem}
.mem-editor-foot p{flex:1 1 220px;font-size:.82rem}
.mem-proposal .mem-actions{margin-top:.75rem;justify-content:flex-end}
.mem-notice,.mem-job{display:flex;align-items:flex-start;gap:.6rem;padding:.75rem 1rem;border-radius:14px;font-size:.88rem;line-height:1.45}
.mem-notice p,.mem-job p{margin:0;min-width:0;overflow-wrap:anywhere}
.mem-notice .icon,.mem-job .icon{width:18px;height:18px;flex:0 0 18px}
.mem-notice-ok{background:var(--md-success-container);color:var(--md-on-success-container)}
.mem-notice-bad{background:var(--md-error-container);color:var(--md-on-error-container)}
.mem-job{border:1px solid var(--md-outline-variant);background:var(--md-surface-container);color:var(--md-on-surface)}
.mem-job-bad{border-color:transparent;background:var(--md-error-container);color:var(--md-on-error-container)}
.mem-spin{display:inline-block;width:14px;height:14px;flex:0 0 14px;box-sizing:border-box;border:2px solid currentColor;border-right-color:transparent;border-radius:50%;animation:mem-spin .8s linear infinite}
.mem-job .mem-spin{margin-top:.15rem}
.mem-job-body{flex:1 1 auto;min-width:0;display:flex;flex-direction:column;gap:.55rem}
.mem-progress{height:6px;border-radius:999px;background:var(--md-outline-variant);overflow:hidden;width:100%;box-sizing:border-box}
.mem-progress>span{display:block;height:100%;border-radius:999px;background:var(--md-primary);transition:width 350ms cubic-bezier(0.32,0.72,0,1)}
.mem-progress-card{margin-top:.75rem;height:4px}
.mem-sync-run{display:inline-flex;align-items:center;gap:.4rem;color:var(--md-on-surface)}
.mem-live-banner{margin-bottom:.75rem}
.mem-live-banner a{color:inherit;font-weight:600;margin-left:.35rem;text-decoration:underline}
@keyframes mem-spin{to{transform:rotate(360deg)}}
.mem-dialog{position:fixed;inset:0;z-index:1000;width:min(480px,calc(100vw - 2rem));height:fit-content;max-width:none;margin:auto;padding:1.25rem 1.4rem;box-sizing:border-box;border:1px solid var(--md-outline-variant);border-radius:16px;background:var(--md-surface);color:var(--md-on-surface);box-shadow:0 24px 60px rgba(0,0,0,.28)}
.mem-dialog::backdrop{background:rgba(0,0,0,.35);backdrop-filter:blur(4px)}
.mem-dialog h2{margin:0 0 .5rem;font-size:1.05rem}
.mem-dialog p{margin:0;font-size:.9rem;line-height:1.5;color:var(--md-on-surface-variant)}
.mem-dialog-actions{display:flex;flex-wrap:wrap;justify-content:flex-end;gap:.5rem;margin-top:1.1rem}
a.mem-btn{text-decoration:none}
.mem-unapplied{margin:.6rem 0 0;font-size:.85rem}
.mem-unapplied a{color:var(--md-primary);font-weight:500}
.mem-actions:empty{display:none}
@media (prefers-reduced-motion:no-preference){
a.mem-card{transition:transform .3s cubic-bezier(.32,.72,0,1),box-shadow .3s cubic-bezier(.32,.72,0,1),border-color .3s cubic-bezier(.32,.72,0,1)}
a.mem-card:hover{transform:translateY(-2px)}
.mem-seg a,.mem-subnav a,.mem-link{transition:background-color .25s cubic-bezier(.32,.72,0,1),color .25s cubic-bezier(.32,.72,0,1)}
}
@media (max-width:760px){
.mem-layout{grid-template-columns:minmax(0,1fr)}
.card.mem-subnav{position:static;flex-direction:row;overflow-x:auto;scrollbar-width:none}
.mem-subnav::-webkit-scrollbar{display:none}
.mem-subnav a{flex:0 0 auto}
.mem-subnav-sep{width:1px;height:auto;margin:.35rem .2rem;flex:0 0 1px}
}
</style>";

#[cfg(test)]
mod tests {
    use crate::ui::state::UiState;
    use chrono::{TimeZone, Utc};
    use loopx_core::approvals::{self, NewApproval};
    use loopx_core::project_profile::{self, Link, Origin, Profile, Section, SectionKey, Source, Status};
    use serde_json::json;
    use std::collections::BTreeMap;
    use serde_json::Value;
    use std::path::{Path, PathBuf};

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap()
    }

    struct Sb {
        dir: tempfile::TempDir,
        state: UiState,
    }

    impl Sb {
        /// `projects`: (alias, Some(checkout) for a code-changing project, None for issues-only).
        fn new(projects: &[(&str, bool)]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("loopx");
            std::fs::create_dir_all(&home).unwrap();
            let mut map = serde_json::Map::new();
            for (alias, code) in projects {
                let mut p = json!({"project_id": format!("group/{alias}"), "target_branch": "main"});
                if *code {
                    let wd = dir.path().join("repos").join(alias);
                    std::fs::create_dir_all(&wd).unwrap();
                    std::fs::write(wd.join("README.md"), "hello\n").unwrap();
                    p["local_path"] = json!(wd.to_string_lossy());
                } else {
                    // Issues-only here means "no checkout at all"; managed clones have their own tests.
                    p["memory_clone"] = json!(false);
                }
                map.insert((*alias).to_string(), p);
            }
            std::fs::write(
                home.join("projects.json"),
                json!({"gitlab_instance": "main", "projects": map}).to_string(),
            )
            .unwrap();
            let state = UiState::explicit(dir.path(), &home, &dir.path().join("userhome"), dir.path(), "tok", Some(now()));
            Self { dir, state }
        }
        fn root(&self) -> PathBuf {
            self.state.home_dir.join("memory")
        }
        fn approvals(&self) -> PathBuf {
            self.state.repo_root.join("outputs").join("approvals")
        }
        fn save(&self, p: &Profile) {
            project_profile::save(p, Some(&self.root())).unwrap();
        }
        fn get(&self, url: &str) -> String {
            let r = self.state.registry.dispatch(&self.state, &crate::ui::routes::UiRequest::get(url)).unwrap();
            assert_eq!(r.status, 200, "{url}");
            String::from_utf8(r.body).unwrap()
        }
    }

    fn section(summary: &str, body: &str) -> Section {
        Section {
            summary: summary.into(),
            body: body.into(),
            last_verified: Some(Utc.with_ymd_and_hms(2026, 11, 1, 8, 0, 0).unwrap()),
            sources: vec![],
            origin: Origin::Agent,
        }
    }

    fn profile(alias: &str, sections: &[(SectionKey, Section)]) -> Profile {
        Profile {
            alias: alias.into(),
            generated_at: Some(Utc.with_ymd_and_hms(2026, 11, 1, 8, 0, 0).unwrap()),
            head_sha: None,
            status: Status::Approved,
            links: vec![],
            sections: sections.iter().cloned().collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn overview_lists_every_tracked_project() {
        let sb = Sb::new(&[("alpha", true), ("beta", true), ("issues-only", false)]);
        sb.save(&profile("alpha", &[(SectionKey::Overview, section("Alpha serves the shop.", "long"))]));
        // An alias with only a memory directory (no projects.json entry) is listed too.
        sb.save(&profile("orphan", &[(SectionKey::Overview, section("Orphan summary.", "x"))]));
        let html = sb.get("/insights?view=memory");
        assert!(html.contains("class='mem-grid'"), "{html}");
        for alias in ["alpha", "beta", "orphan"] {
            assert!(html.contains(&format!("project={alias}'")), "missing {alias}");
        }
        // Like the old view: every projects.json alias, issues-only ones too.
        assert!(html.contains("project=issues-only'"), "issues-only project is listed");
        // No proposals anywhere: no "0 pending proposals" noise.
        assert!(!html.contains("0 pending proposals"));
        assert!(html.contains("Alpha serves the shop."));
        assert!(html.contains("No profile yet"));
        // The Memory/Graph segmented toggle and the weekly sync line.
        assert!(html.contains("class='mem-seg'") && html.contains("graph=1"));
        assert!(html.contains("Next run"));
    }

    #[test]
    fn overview_marks_failed_sync_and_counts_proposals_and_neighbors() {
        let sb = Sb::new(&[("alpha", true), ("beta", true)]);
        let mut a = profile("alpha", &[(SectionKey::Overview, section("A.", "a"))]);
        a.links.push(Link { to: "beta".into(), kind: "calls".into(), evidence: "config/x.yml:1".into() });
        sb.save(&a);
        sb.save(&profile("beta", &[(SectionKey::Overview, section("B.", "b"))]));
        loopx_core::profile_sync::with_store(&sb.state.home_dir.join("profile_sync.json"), |s| {
            s.status.last_run = Some(now() - chrono::Duration::days(2));
            s.status.per_project.insert(
                "beta".into(),
                loopx_core::profile_sync::ProjectSync { at: now(), outcome: loopx_core::profile_sync::Outcome::Failed("checkout not found".into()) },
            );
        })
        .unwrap();
        propose(&sb, "alpha", "features", "old", "new");
        let html = sb.get("/insights?view=memory");
        assert!(html.contains("Last sync failed"), "{html}");
        assert!(html.contains("checkout not found"));
        assert!(html.contains("1 pending proposal"), "{html}");
        assert!(html.contains("1 linked project"), "{html}");
        assert!(html.contains("2d ago"));
    }

    fn propose(sb: &Sb, alias: &str, slug: &str, old: &str, new: &str) -> String {
        let a = approvals::create(
            &sb.approvals(),
            &NewApproval {
                kind: loopx_core::profile_sync::KIND.into(),
                project: alias.into(),
                title: format!("Update the {slug} section of the {alias} profile"),
                summary: "s".into(),
                // Never expired by a page that looks at approvals with the real clock.
                timeout_seconds: 10 * 365 * 24 * 3600,
                target: Some(json!({
                    "section": slug, "alias": alias, "old": old, "new": new,
                    "old_summary": "old summary", "new_summary": "new summary",
                })),
                ..Default::default()
            },
            now(),
        )
        .unwrap();
        a["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn project_page_shows_summaries_not_bodies() {
        let sb = Sb::new(&[("alpha", true), ("beta", true)]);
        let mut a = profile(
            "alpha",
            &[
                (SectionKey::Overview, section("Alpha serves the shop.", "OVERVIEW-BODY-TEXT")),
                (SectionKey::Features, section("Checkout and carts.", "FEATURES-BODY-TEXT")),
            ],
        );
        a.links.push(Link { to: "beta".into(), kind: "calls".into(), evidence: "config/x.yml:1".into() });
        sb.save(&a);
        let html = sb.get("/insights?view=memory&project=alpha");
        assert!(html.contains("Checkout and carts."));
        assert!(!html.contains("FEATURES-BODY-TEXT") && !html.contains("OVERVIEW-BODY-TEXT"));
        assert!(html.contains("project=alpha&amp;page=features"), "More link to the sub-page");
        assert!(html.contains("More"));
        // Neighbor strip and the proposals card.
        assert!(html.contains("project=beta'"), "neighbor link");
        assert!(html.contains("Pending proposals"));
        assert!(html.contains("class='card mem-subnav'"));
        assert!(html.contains("class='mem-actions'"));
        assert!(!html.contains("This profile is a draft"));
        // Missing sections still get a card.
        assert!(html.contains("Not written yet"));
    }

    #[test]
    fn draft_profile_shows_a_banner() {
        let sb = Sb::new(&[("alpha", true)]);
        let mut a = profile("alpha", &[(SectionKey::Overview, section("A.", "a"))]);
        a.status = Status::Draft;
        sb.save(&a);
        assert!(sb.get("/insights?view=memory&project=alpha").contains("This profile is a draft"));
    }

    #[test]
    fn subpage_shows_full_body_and_sources() {
        let sb = Sb::new(&[("alpha", true)]);
        let wd = sb.dir.path().join("repos/alpha");
        let mut f = section("Checkout and carts.", "## Cart\n\nThe **cart** lives in `app/cart`.");
        f.sources = vec![
            Source { path: "README.md".into(), sha256: loopx_core::sha256::hex(&loopx_core::sha256::sha256(b"hello\n")) },
            Source { path: "app/cart.rb".into(), sha256: "ab".repeat(32) },
        ];
        sb.save(&profile("alpha", &[(SectionKey::Features, f)]));
        assert!(wd.join("README.md").exists());
        let html = sb.get("/insights?view=memory&project=alpha&page=features");
        assert!(html.contains("<strong>cart</strong>"), "{html}");
        assert!(html.contains("<code>app/cart</code>"));
        assert!(html.contains("README.md") && html.contains("app/cart.rb"));
        assert!(html.contains("abababababab"), "short sha");
        assert!(html.contains("2026-11-01"), "last verified");
        // app/cart.rb is missing from the checkout: the section is stale.
        assert!(html.contains("Stale"));
        assert!(html.contains("class='card mem-subnav'") && html.contains("aria-current='page'"));
    }

    #[test]
    fn stack_page_shows_setup_md_inline() {
        let sb = Sb::new(&[("alpha", true)]);
        sb.save(&profile("alpha", &[(SectionKey::Stack, section("Rails 7.", "Ruby **3.3**."))]));
        loopx_core::memory_store::save_setup_knowledge("alpha", "# Setup\n\nRun `bin/setup` first.\n", Some(&sb.root())).unwrap();
        let html = sb.get("/insights?view=memory&project=alpha&page=stack");
        assert!(html.contains("Ruby <strong>3.3</strong>."));
        assert!(html.contains("<code>bin/setup</code>"), "{html}");
    }

    #[test]
    fn activity_page_shows_recent_work_and_people() {
        let sb = Sb::new(&[("alpha", true)]);
        sb.save(&profile(
            "alpha",
            &[(SectionKey::Activity, section("Busy.", "RECENT-WORK-BODY")), (SectionKey::People, section("Two people.", "PEOPLE-BODY"))],
        ));
        let html = sb.get("/insights?view=memory&project=alpha&page=activity");
        assert!(html.contains("RECENT-WORK-BODY") && html.contains("PEOPLE-BODY"));
    }

    #[test]
    fn corrupt_profile_shows_error_card_and_keeps_file() {
        let sb = Sb::new(&[("alpha", true)]);
        let path = project_profile::profile_path("alpha", Some(&sb.root()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = "not a profile <b>at all</b>\n";
        std::fs::write(&path, bytes).unwrap();
        for url in ["/insights?view=memory&project=alpha", "/insights?view=memory&project=alpha&page=features"] {
            let html = sb.get(url);
            assert!(html.contains("mem-error"), "{html}");
            assert!(html.contains("missing frontmatter"));
            assert!(!html.contains("<b>at all</b>"));
        }
        let overview = sb.get("/insights?view=memory");
        assert!(overview.contains("Unreadable"), "{overview}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
    }

    #[test]
    fn a_malformed_line_or_failure_with_a_token_is_never_shown() {
        let sb = Sb::new(&[("alpha", true), ("beta", true)]);
        let token = ["glpat-", "abcDEF123456ghiJKL78"].concat();
        let path = project_profile::profile_path("alpha", Some(&sb.root()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("---\nalias: alpha\n---\n<!-- section: stack origin: {token} verified: - summary: x -->\n")).unwrap();
        sb.save(&profile("beta", &[(SectionKey::Overview, section("B.", "b"))]));
        // A failure stored by an older version, before failures were guarded.
        loopx_core::profile_sync::with_store(&sb.state.home_dir.join("profile_sync.json"), |s| {
            s.status.per_project.insert(
                "beta".into(),
                loopx_core::profile_sync::ProjectSync { at: now(), outcome: loopx_core::profile_sync::Outcome::Failed(format!("bad {token}")) },
            );
        })
        .unwrap();
        for url in ["/insights?view=memory", "/insights?view=memory&project=alpha", "/insights?view=memory&project=beta"] {
            let html = sb.get(url);
            assert!(!html.contains("glpat-") && !html.contains("abcDEF"), "{url} leaked the token");
        }
        assert!(sb.get("/insights?view=memory&project=alpha").contains("on line 4"));
        assert!(sb.get("/insights?view=memory&project=beta").contains("details hidden"));
    }

    #[test]
    fn secrets_page_never_renders_a_value() {
        let sb = Sb::new(&[("alpha", true)]);
        let secret = "glpat-abcdefghijklmnop1234";
        sb.save(&profile(
            "alpha",
            &[(SectionKey::Secrets, section(&format!("GITLAB_TOKEN is {secret}"), &format!("- `GITLAB_TOKEN` = {secret}")))],
        ));
        for url in ["/insights?view=memory&project=alpha&page=secrets", "/insights?view=memory&project=alpha", "/insights?view=memory"] {
            let html = sb.get(url);
            assert!(!html.contains("glpat-"), "{url} leaked a value");
        }
        let html = sb.get("/insights?view=memory&project=alpha&page=secrets");
        assert!(html.contains("looks like it contains a secret"), "{html}");
        assert!(html.contains("GitLab token"), "names the kind only");
        // A clean inventory renders normally.
        sb.save(&profile("alpha", &[(SectionKey::Secrets, section("Two names.", "- `GITLAB_TOKEN`: env, from 1Password"))]));
        assert!(sb.get("/insights?view=memory&project=alpha&page=secrets").contains("<code>GITLAB_TOKEN</code>"));
    }

    #[test]
    fn unknown_alias_and_page_are_safe_and_escaped() {
        let sb = Sb::new(&[("alpha", true)]);
        sb.save(&profile("alpha", &[(SectionKey::Overview, section("A.", "a"))]));
        let html = sb.get("/insights?view=memory&project=%3Cscript%3Ealert(1)%3C%2Fscript%3E");
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("mem-empty"), "{html}");
        let html = sb.get("/insights?view=memory&project=..%2F..%2Fetc");
        assert!(html.contains("mem-empty") && !html.contains("../../etc"));
        let html = sb.get("/insights?view=memory&project=alpha&page=%3Cimg%20src%3Dx%3E");
        assert!(!html.contains("<img src=x>"));
        assert!(html.contains("That page does not exist"), "{html}");
    }

    #[test]
    fn old_memory_url_still_renders_overview() {
        let sb = Sb::new(&[("alpha", true)]);
        let html = sb.get("/insights?view=memory");
        assert!(html.contains("<h1>Project Memory</h1>"));
        assert!(html.contains("class='mem-grid'"));
        assert!(html.contains("aria-current='page'>Memory</a>"));
    }

    #[test]
    fn no_projects_keeps_the_set_up_empty_state() {
        let sb = Sb::new(&[]);
        let html = sb.get("/insights?view=memory");
        assert!(html.contains("No projects configured yet, so there is no memory recorded."));
        assert!(html.contains("/loops/issue-loop?view=projects"));
    }

    #[test]
    fn graph_toggle_renders_the_graph_not_the_cards() {
        let sb = Sb::new(&[("alpha", true)]);
        let html = sb.get("/insights?view=memory&graph=1");
        assert!(html.contains("class='mg-svg"), "{html}");
        assert!(!html.contains("mem-graph-soon"));
        assert!(!html.contains("class='mem-grid'"));
    }

    #[test]
    fn lessons_subpage_keeps_existing_memory_content() {
        let sb = Sb::new(&[("alpha", true)]);
        let root = sb.root();
        loopx_core::memory_store::add_task_memory("alpha", 101, "Run the migration before the specs. It fixes a flaky **setup** failure.", &["testing".into()], Some("testing"), Some(&root)).unwrap();
        let added = loopx_core::memory_store::add_task_memory("alpha", 103, "A lesson with no category & <b>markup</b>.", &[], None, Some(&root)).unwrap();
        // Flag one as needs_review the way the learning loop does.
        let id = added.lesson_id.unwrap();
        assert!(loopx_core::memory_store::set_score(&id, -3, "needs_review", Some("alpha"), Some(&root)).unwrap());
        let html = sb.get("/insights?view=memory&project=alpha&page=lessons");
        assert!(html.contains("<input type=\"checkbox\" id=\"memory-filter-needs-review\">"), "{html}");
        assert!(html.contains("for=\"memory-filter-needs-review\">Needs review (1)</label>"), "{html}");
        assert!(html.contains("<li class='learning-item' data-needs-review='1'>"));
        assert!(html.contains("<strong>setup</strong>"));
        assert!(html.contains("&lt;b&gt;markup&lt;/b&gt;") && !html.contains("<b>markup</b>"));
        assert!(html.contains("<span class='pill pill-grey'>#101</span>"));
        assert!(html.contains("<span class='pill pill-grey'>testing</span>"));
        // Both lessons have a lesson_id and no reuse events yet.
        assert_eq!(html.matches("<p class='learning-reuse-stats'>Not yet reused</p>").count(), 2, "{html}");
        // The overview card counts the lessons.
        let overview = sb.get("/insights?view=memory");
        assert!(overview.contains("2 lessons"), "{overview}");
        // A project with none says so.
        let sb2 = Sb::new(&[("beta", true)]);
        assert!(sb2.get("/insights?view=memory&project=beta&page=lessons").contains("(no task memory recorded yet)"));
    }


    #[test]
    fn issues_only_alias_with_legacy_learnings_still_shows_them() {
        let sb = Sb::new(&[("legacy", false)]);
        let cache = sb.dir.path().join("userhome/.gitlab/cache/main/projects/group__legacy");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(
            cache.join("project.json"),
            json!({"_memory": {"fix_learnings": [{"lesson": "LEGACY-LESSON keep **this**", "issue_iid": 7, "tags": ["old"]}]}}).to_string(),
        )
        .unwrap();
        let overview = sb.get("/insights?view=memory");
        assert!(overview.contains("project=legacy'"), "{overview}");
        let html = sb.get("/insights?view=memory&project=legacy&page=lessons");
        assert!(html.contains("Legacy learnings"), "{html}");
        assert!(html.contains("LEGACY-LESSON keep <strong>this</strong>"));
        assert!(html.contains("<span class='pill pill-grey'>#7</span>") && html.contains("<span class='pill pill-grey'>old</span>"));
    }

    #[test]
    fn lessons_never_render_a_secret_value() {
        let sb = Sb::new(&[("alpha", true)]);
        let root = sb.root();
        loopx_core::memory_store::add_task_memory("alpha", 5, "Use token glpat-abcdefghijklmnop1234 for the API.", &[], None, Some(&root)).unwrap();
        loopx_core::memory_store::add_task_memory("alpha", 6, "A clean lesson.", &[], None, Some(&root)).unwrap();
        let cache = sb.dir.path().join("userhome/.gitlab/cache/main/projects/group__alpha");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(
            cache.join("project.json"),
            json!({"_memory": {"fix_learnings": [{"lesson": "old xoxb-1234567890-abcdefghij token"}]}}).to_string(),
        )
        .unwrap();
        let html = sb.get("/insights?view=memory&project=alpha&page=lessons");
        assert!(!html.contains("glpat-") && !html.contains("xoxb-"), "{html}");
        assert!(html.contains("looks like it contains a secret"));
        assert!(html.contains("A clean lesson."));
    }

    #[test]
    fn project_page_reads_head_from_the_checkout() {
        let sb = Sb::new(&[("alpha", true)]);
        let wd = sb.dir.path().join("repos/alpha");
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&wd)
                .args(["-c", "user.name=t", "-c", "user.email=t@e", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success());
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "init", "--no-verify"]);
        let head = git(&["rev-parse", "HEAD"]);
        let mut p = profile("alpha", &[(SectionKey::Overview, section("A.", "a"))]);
        p.head_sha = Some(head.clone());
        sb.save(&p);
        let html = sb.get("/insights?view=memory&project=alpha");
        assert!(html.contains(&format!("Up to date with HEAD {}", &head[..8])), "{html}");
    }

    #[test]
    fn proposals_subpage_lists_profile_section_approvals() {
        let sb = Sb::new(&[("alpha", true), ("beta", true)]);
        sb.save(&profile("alpha", &[(SectionKey::Overview, section("A.", "a"))]));
        propose(&sb, "alpha", "features", "OLD-FEATURES <i>x</i>", "NEW-FEATURES");
        let approved = propose(&sb, "alpha", "stack", "OLD-STACK", "NEW-STACK");
        approvals::decide(&sb.approvals(), &approved, "approve", "me", now()).unwrap();
        let denied = propose(&sb, "alpha", "gotchas", "OLD-GOTCHAS", "NEW-GOTCHAS");
        approvals::decide(&sb.approvals(), &denied, "deny", "me", now()).unwrap();
        propose(&sb, "beta", "features", "BETA-OLD", "BETA-NEW");
        approvals::create(
            &sb.approvals(),
            &NewApproval { kind: "mr".into(), project: "alpha".into(), title: "MR-APPROVAL".into(), ..Default::default() },
            now(),
        )
        .unwrap();
        let html = sb.get("/insights?view=memory&project=alpha&page=proposals");
        assert!(html.contains("OLD-FEATURES &lt;i&gt;x&lt;/i&gt;") && html.contains("NEW-FEATURES"), "{html}");
        assert!(html.contains("NEW-STACK"));
        assert!(!html.contains("NEW-GOTCHAS"), "denied ones are not listed");
        assert!(!html.contains("BETA-NEW") && !html.contains("MR-APPROVAL"));
        assert!(html.contains("Pending") && html.contains("Approved"));
        assert!(html.contains("new summary"));
        let main = sb.get("/insights?view=memory&project=alpha");
        assert!(main.contains("project=alpha&amp;page=proposals"));
    }

    // ---------------------------------------------------------------- Part B: actions

    use axum::http::Method;
    use loopx_core::profile_build::{self, Agent};
    use loopx_core::profile_sync;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex};

    /// An agent that blocks until released (if gated) and then answers `reply`.
    struct FakeAgent {
        reply: String,
        gate: Mutex<Option<mpsc::Receiver<()>>>,
        calls: AtomicUsize,
    }

    impl FakeAgent {
        fn new(reply: String) -> Arc<Self> {
            Arc::new(Self { reply, gate: Mutex::new(None), calls: AtomicUsize::new(0) })
        }
        fn gated(reply: String) -> (Arc<Self>, mpsc::Sender<()>) {
            let (tx, rx) = mpsc::channel();
            (Arc::new(Self { reply, gate: Mutex::new(Some(rx)), calls: AtomicUsize::new(0) }), tx)
        }
    }

    impl Agent for FakeAgent {
        fn run(&self, _prompt: &str) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let gate = self.gate.lock().unwrap().take();
            if let Some(rx) = gate {
                let _ = rx.recv_timeout(std::time::Duration::from_secs(20));
            }
            Ok(self.reply.clone())
        }
    }

    /// A whole profile as the agent would reply with it.
    fn full_reply(alias: &str, text: &str) -> String {
        let all: Vec<(SectionKey, Section)> =
            SectionKey::ALL.into_iter().map(|k| (k, section(&format!("{text} {}", k.slug()), text))).collect();
        let mut p = profile(alias, &all);
        p.status = Status::Draft;
        project_profile::render(&p)
    }

    fn one_section_reply(alias: &str, key: SectionKey, text: &str) -> String {
        let mut p = profile(alias, &[(key, section(&format!("{text} summary"), text))]);
        p.status = Status::Draft;
        project_profile::render(&p)
    }

    impl Sb {
        fn with_agent(mut self, agent: Arc<FakeAgent>) -> Self {
            self.state = self.state.with_profile_agent(agent);
            self
        }
        fn post(&self, url: &str, body: &str) -> crate::ui::routes::UiResponse {
            let req = crate::ui::routes::UiRequest::new(Method::POST, url)
                .with_header("content-type", "application/x-www-form-urlencoded")
                .with_body(format!("csrf_token=tok&{body}"));
            self.state.registry.dispatch(&self.state, &req).unwrap()
        }
        fn post_to(&self, url: &str, body: &str) -> String {
            let r = self.post(url, body);
            assert_eq!(r.status, 303, "{url} {body}");
            r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("location")).unwrap().1.clone()
        }
        fn store(&self) -> PathBuf {
            self.state.home_dir.join("profile_sync.json")
        }
        /// Add an issues-only project `alias` that Memory clones from a local bare
        /// repository holding `README.md`; returns the clone directory.
        fn add_managed(&self, alias: &str) -> PathBuf {
            let git = |dir: &Path, args: &[&str]| {
                let out = std::process::Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(["-c", "user.email=t@t", "-c", "user.name=t"])
                    .args(args)
                    .env_remove("GIT_DIR")
                    .output()
                    .unwrap();
                assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            };
            let bare = self.dir.path().join(format!("{alias}.git"));
            let work = self.dir.path().join(format!("{alias}-work"));
            std::fs::create_dir_all(&bare).unwrap();
            std::fs::create_dir_all(&work).unwrap();
            git(&bare, &["init", "--bare", "-b", "main"]);
            git(&work, &["init", "-b", "main"]);
            std::fs::write(work.join("README.md"), "hello\n").unwrap();
            git(&work, &["add", "."]);
            git(&work, &["commit", "-m", "one"]);
            let url = format!("file://{}", bare.display());
            git(&work, &["push", &url, "main"]);
            let project_id = format!("grp/{alias}");
            loopx_core::profile_checkout::register_test_remote(
                "work",
                &project_id,
                loopx_core::profile_checkout::Remote { url, branch: "main".into(), auth_env: vec![] },
            );
            let path = self.state.home_dir.join("projects.json");
            let mut cfg: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            cfg["projects"][alias] = json!({"instance": "work", "project_id": project_id});
            std::fs::write(&path, cfg.to_string()).unwrap();
            loopx_core::profile_checkout::managed_dir(&self.state.home_dir, alias)
        }
        fn load(&self, alias: &str) -> Profile {
            project_profile::load(alias, Some(&self.root())).unwrap().unwrap()
        }
    }

    fn wait_idle(alias: &str) {
        for _ in 0..400 {
            if !profile_build::is_running(alias) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("{alias} is still running");
    }

    const ROUTES: [&str; 19] = [
        "/insights/memory/build",
        "/insights/memory/approve",
        "/insights/memory/section/save",
        "/insights/memory/section/refresh",
        "/insights/memory/proposal/decide",
        "/insights/memory/sync/settings",
        "/insights/memory/sync/now",
        "/insights/memory/sync/slack",
        "/insights/memory/sync/clone",
        "/insights/memory/checkout/reclone",
        "/insights/memory/wiki/add",
        "/insights/memory/wiki/remove",
        "/insights/memory/guide/build",
        "/insights/memory/guide/rebuild-page",
        "/insights/memory/guide/approve",
        "/insights/memory/guide/approve-all",
        "/insights/memory/guide/edit",
        "/insights/memory/guide/delete",
        "/insights/memory/guide/restore",
    ];

    #[test]
    fn a_draft_profile_can_be_approved_from_its_banner() {
        let sb = Sb::new(&[("appr-a", true)]);
        let mut a = profile("appr-a", &[(SectionKey::Overview, section("A.", "a"))]);
        a.status = Status::Draft;
        sb.save(&a);
        let html = sb.get("/insights?view=memory&project=appr-a");
        assert!(html.contains("action='/insights/memory/approve'"), "{html}");
        assert!(html.contains("Approve profile"));
        let loc = sb.post_to("/insights/memory/approve", "project=appr-a");
        assert!(loc.ends_with("notice=profile-approved"), "{loc}");
        let back = sb.load("appr-a");
        assert_eq!(back.status, Status::Approved);
        assert_eq!(back.sections, a.sections);
        let html = sb.get("/insights?view=memory&project=appr-a");
        assert!(!html.contains("This profile is a draft") && !html.contains("Approve profile"), "{html}");
        // Unknown project: nothing changes.
        assert!(sb.post_to("/insights/memory/approve", "project=nope").ends_with("notice=bad-input"));
    }

    #[test]
    fn a_draft_holding_a_secret_is_not_approved() {
        let sb = Sb::new(&[("appr-b", true)]);
        let mut a = profile("appr-b", &[(SectionKey::Overview, section("A.", "a"))]);
        a.status = Status::Draft;
        sb.save(&a);
        // Written by hand into the file (a save through the page refuses secrets).
        let path = project_profile::profile_path("appr-b", Some(&sb.root()));
        let text = std::fs::read_to_string(&path).unwrap();
        let token = ["glpat-", "abcDEF123456ghiJKL78"].concat();
        std::fs::write(&path, text.replace("\na\n", &format!("\nuse {token}\n"))).unwrap();
        let loc = sb.post_to("/insights/memory/approve", "project=appr-b");
        assert!(loc.ends_with("notice=approve-secret"), "{loc}");
        assert!(!loc.contains("glpat"));
        assert_eq!(sb.load("appr-b").status, Status::Draft);
    }

    #[test]
    fn every_memory_post_route_requires_csrf() {
        let sb = Sb::new(&[("alpha", true)]);
        for url in ROUTES {
            let req = crate::ui::routes::UiRequest::new(Method::POST, url)
                .with_header("content-type", "application/x-www-form-urlencoded")
                .with_body("project=alpha");
            let r = sb.state.registry.dispatch(&sb.state, &req).unwrap();
            assert_eq!(r.status, 403, "{url}");
            let wrong = crate::ui::routes::UiRequest::new(Method::POST, url)
                .with_header("content-type", "application/x-www-form-urlencoded")
                .with_body("csrf_token=nope&project=alpha");
            assert_eq!(sb.state.registry.dispatch(&sb.state, &wrong).unwrap().status, 403, "{url}");
        }
        assert!(!sb.store().exists());
    }

    #[test]
    fn save_section_marks_human_and_rejects_secret_text() {
        let sb = Sb::new(&[("save-a", true)]);
        sb.save(&profile("save-a", &[(SectionKey::Features, section("Old sum", "Old body"))]));
        let loc = sb.post_to(
            "/insights/memory/section/save",
            "project=save-a&section=features&summary=New+sum&body=Line+one%0D%0ALine+two",
        );
        assert_eq!(loc, "/insights?view=memory&project=save-a&page=features&notice=saved");
        let s = &sb.load("save-a").sections[&SectionKey::Features];
        assert_eq!(s.origin, Origin::Human);
        assert_eq!((s.summary.as_str(), s.body.as_str()), ("New sum", "Line one\nLine two"));
        let html = sb.get(&loc);
        assert!(html.contains("mem-notice") && html.contains("Section saved"), "{html}");

        let secret = "glpat-abcdefghijklmnop1234";
        for body in [format!("summary=x&body=key+{secret}"), format!("summary=key+{secret}&body=x")] {
            let loc = sb.post_to("/insights/memory/section/save", &format!("project=save-a&section=features&{body}"));
            assert!(loc.ends_with("&notice=secret"), "{loc}");
            assert!(!loc.contains("glpat"), "{loc}");
            let html = sb.get(&loc);
            assert!(!html.contains("glpat-"), "never echoed");
            assert!(html.contains("looks like it contains a secret"), "{html}");
        }
        let s = &sb.load("save-a").sections[&SectionKey::Features];
        assert_eq!(s.summary, "New sum", "a refused save writes nothing");
    }

    #[test]
    fn save_rejects_unknown_alias_and_slug_and_oversize() {
        let sb = Sb::new(&[("save-b", true), ("save-none", true)]);
        sb.save(&profile("save-b", &[(SectionKey::Stack, section("S", "B"))]));
        let before = std::fs::read_to_string(project_profile::profile_path("save-b", Some(&sb.root()))).unwrap();
        let unknown = sb.post_to("/insights/memory/section/save", "project=ghost&section=stack&summary=a&body=b");
        assert_eq!(unknown, "/insights?view=memory&notice=bad-input");
        let slug = sb.post_to("/insights/memory/section/save", "project=save-b&section=lessons&summary=a&body=b");
        assert_eq!(slug, "/insights?view=memory&project=save-b&notice=bad-input");
        let long_sum = "a".repeat(301);
        let loc = sb.post_to("/insights/memory/section/save", &format!("project=save-b&section=stack&summary={long_sum}&body=b"));
        assert!(loc.ends_with("page=stack&notice=too-long"), "{loc}");
        let long_body = "b".repeat(20_001);
        let loc = sb.post_to("/insights/memory/section/save", &format!("project=save-b&section=stack&summary=a&body={long_body}"));
        assert!(loc.ends_with("notice=too-long"), "{loc}");
        let after = std::fs::read_to_string(project_profile::profile_path("save-b", Some(&sb.root()))).unwrap();
        assert_eq!(before, after);
        let none = sb.post_to("/insights/memory/section/save", "project=save-none&section=stack&summary=a&body=b");
        assert!(none.ends_with("notice=no-profile"), "{none}");
        assert!(project_profile::load("save-none", Some(&sb.root())).unwrap().is_none(), "no profile is created");
    }

    #[test]
    fn build_starts_in_background_and_returns_immediately() {
        let (agent, release) = FakeAgent::gated(full_reply("build-a", "Built"));
        let sb = Sb::new(&[("build-a", true)]).with_agent(agent.clone());
        let started = std::time::Instant::now();
        let loc = sb.post_to("/insights/memory/build", "project=build-a&from=project");
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "the request did not wait for the agent");
        assert_eq!(loc, "/insights?view=memory&project=build-a&notice=build-started");
        assert!(profile_build::is_running("build-a"));
        let page = sb.get(&loc);
        assert!(page.contains("Building the profile"), "{page}");
        assert!(page.contains("Working on this profile"), "{page}");
        // A second click while the first runs.
        let again = sb.post_to("/insights/memory/build", "project=build-a&from=overview");
        assert_eq!(again, "/insights?view=memory&notice=busy");
        release.send(()).unwrap();
        wait_idle("build-a");
        let p = sb.load("build-a");
        assert_eq!(p.status, Status::Draft);
        assert_eq!(agent.calls.load(Ordering::SeqCst), 1);
        let page = sb.get("/insights?view=memory&project=build-a");
        assert!(page.contains("Profile built"), "{page}");
    }

    #[test]
    fn a_running_build_is_a_live_agent_with_its_step_until_it_ends() {
        let (agent, release) = FakeAgent::gated(full_reply("live-a", "Built"));
        let sb = Sb::new(&[("live-a", true)]).with_agent(agent);
        let base = loopx_core::agent_activity::beside(&sb.state.status_path_for_loop("issue-loop"));
        sb.post_to("/insights/memory/build", "project=live-a&from=project");
        let mut job = None;
        for _ in 0..400 {
            job = loopx_core::profile_progress::running_for(&base, "live-a").filter(|j| j.step_code == "ai");
            if job.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let job = job.expect("the build reports that it is asking the AI");
        assert_eq!((job.kind.as_str(), job.step, job.steps), ("build", 3, 5));
        let agents = sb.get("/queue/fragment");
        assert!(agents.contains("Asking the AI \u{b7} step 3 of 5"), "{agents}");
        let page = sb.get("/insights?view=memory&project=live-a");
        assert!(page.contains("Working on this profile") && page.contains("Asking the AI \u{b7} step 3 of 5"), "{page}");
        release.send(()).unwrap();
        wait_idle("live-a");
        assert!(loopx_core::profile_progress::running(&base).is_empty());
        let trace = std::fs::read_to_string(base.with_file_name("agent-trace.jsonl")).unwrap();
        assert!(trace.lines().last().unwrap().contains("\"phase\":\"done\""), "{trace}");
    }

    use super::{last_job, record, Job, JobKind, JobOutcome, ProfileJob, ProfileJobKind};

    /// An agent whose AI call fails with `err` (what `todos_draft::run_sealed` returns).
    struct FailingAgent(&'static str);

    impl Agent for FailingAgent {
        fn run(&self, _prompt: &str) -> Result<String, String> {
            Err(self.0.to_string())
        }
    }

    fn failed_build(sb: &Sb, alias: &str, at: chrono::DateTime<Utc>) {
        record(&sb.state.home_dir, alias, Job { at, kind: JobKind::Build, outcome: JobOutcome::Failed("TimeoutExpired".into()) });
    }

    #[test]
    fn a_build_whose_cli_could_not_start_says_so_on_the_memory_page() {
        let alias = "nocli-a";
        let mut sb = Sb::new(&[(alias, true)]);
        sb.state = sb.state.with_profile_agent(Arc::new(FailingAgent("FileNotFoundError")));
        sb.post_to("/insights/memory/build", &format!("project={alias}&from=project"));
        wait_idle(alias);
        let page = sb.get(&format!("/insights?view=memory&project={alias}"));
        assert!(
            page.contains("The last build failed just now: the AI command could not be started \u{2014} check Settings \u{2192} AI CLI"),
            "{page}"
        );
        assert!(!page.contains("FileNotFoundError"), "{page}");
        let req = crate::ui::routes::UiRequest::get(&format!("/insights?view=memory&project={alias}")).with_header("cookie", "loop_lang=fr");
        let fr = String::from_utf8(sb.state.registry.dispatch(&sb.state, &req).unwrap().body).unwrap();
        assert!(fr.contains("La commande IA n&#x27;a pas pu"), "{fr}");
    }

    #[test]
    fn a_live_job_record_shows_as_running_over_an_older_failure() {
        let alias = "rec-a";
        let sb = Sb::new(&[(alias, true)]);
        failed_build(&sb, alias, now() - chrono::Duration::minutes(8));
        // A job this page did not start (no in-process guard), alive by its pid.
        let base = loopx_core::agent_activity::beside(&sb.state.status_path_for_loop("issue-loop"));
        let job = ProfileJob::start(&base, ProfileJobKind::Build, alias);
        loopx_core::profile_progress::Progress::step(&job, 3, 5, loopx_core::profile_progress::Step::Ai);
        let page = sb.get(&format!("/insights?view=memory&project={alias}"));
        assert!(page.contains("Working on this profile") && page.contains("Asking the AI \u{b7} step 3 of 5"), "{page}");
        assert!(!page.contains("The last build failed"), "{page}");
        drop(job);
        let page = sb.get(&format!("/insights?view=memory&project={alias}"));
        assert!(page.contains("The last build failed 8m ago"), "{page}");
        assert!(!page.contains("Working on this profile"), "{page}");
    }

    #[test]
    fn starting_a_build_clears_the_last_failure() {
        let alias = "clear-a";
        let (agent, release) = FakeAgent::gated(full_reply(alias, "Built"));
        let sb = Sb::new(&[(alias, true)]).with_agent(agent);
        failed_build(&sb, alias, now() - chrono::Duration::minutes(8));
        sb.post_to("/insights/memory/build", &format!("project={alias}&from=project"));
        assert!(last_job(&sb.state.home_dir, alias).is_none(), "the old failure is gone once a build starts");
        let page = sb.get(&format!("/insights?view=memory&project={alias}"));
        assert!(page.contains("Working on this profile") && !page.contains("The last build failed"), "{page}");
        release.send(()).unwrap();
        wait_idle(alias);
    }

    #[test]
    fn a_failure_older_than_a_day_is_no_longer_shown() {
        let alias = "old-a";
        let sb = Sb::new(&[(alias, true)]);
        failed_build(&sb, alias, now() - chrono::Duration::hours(23));
        assert!(sb.get(&format!("/insights?view=memory&project={alias}")).contains("The last build failed 23h ago"));
        failed_build(&sb, alias, now() - chrono::Duration::hours(25));
        let page = sb.get(&format!("/insights?view=memory&project={alias}"));
        assert!(!page.contains("The last build failed"), "{page}");
    }

    #[test]
    fn a_section_refresh_is_a_live_agent_too() {
        let alias = "live-r";
        let (agent, release) = FakeAgent::gated(one_section_reply(alias, SectionKey::Stack, "Fresh"));
        let sb = Sb::new(&[(alias, true)]).with_agent(agent);
        sb.save(&profile(alias, &[(SectionKey::Stack, section("Old", "Old body"))]));
        let base = loopx_core::agent_activity::beside(&sb.state.status_path_for_loop("issue-loop"));
        sb.post_to("/insights/memory/section/refresh", &format!("project={alias}&section=stack"));
        let mut job = None;
        for _ in 0..400 {
            job = loopx_core::profile_progress::running_for(&base, alias).filter(|j| j.step_code == "ai");
            if job.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let job = job.expect("the refresh reports that it is asking the AI");
        assert_eq!((job.kind.as_str(), job.section.as_deref(), job.step, job.steps), ("refresh", Some("stack"), 2, 4));
        release.send(()).unwrap();
        wait_idle(alias);
        assert!(loopx_core::profile_progress::running(&base).is_empty());
    }

    #[test]
    fn build_all_builds_every_tracked_checkout_and_refuses_a_second_click() {
        let (agent, release) = FakeAgent::gated(full_reply("x", "Built"));
        let sb = Sb::new(&[("all-a", true), ("all-b", true), ("all-issues", false)]).with_agent(agent.clone());
        let loc = sb.post_to("/insights/memory/build", "project=all&from=overview");
        assert_eq!(loc, "/insights?view=memory&notice=build-all-started");
        assert_eq!(sb.post_to("/insights/memory/build", "project=all"), "/insights?view=memory&notice=busy");
        assert!(sb.get("/insights?view=memory").contains("Building profiles"));
        release.send(()).unwrap();
        for _ in 0..400 {
            if project_profile::load("all-b", Some(&sb.root())).ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        wait_idle("all-b");
        assert!(sb.load("all-a").sections.len() == 10 && sb.load("all-b").sections.len() == 10);
        assert!(project_profile::load("all-issues", Some(&sb.root())).unwrap().is_none());
    }

    #[test]
    fn build_without_a_checkout_or_agent_is_refused() {
        let sb = Sb::new(&[("nock", false)]).with_agent(FakeAgent::new(String::new()));
        let loc = sb.post_to("/insights/memory/build", "project=nock&from=project");
        assert_eq!(loc, "/insights?view=memory&project=nock&notice=no-checkout");
        let plain = Sb::new(&[("noagent", true)]);
        let loc = plain.post_to("/insights/memory/build", "project=noagent&from=project");
        assert!(loc.ends_with("notice=no-agent"), "tests and goldens never start the real AI CLI: {loc}");
    }

    #[test]
    fn refresh_section_creates_proposal_not_overwrite_for_human_section() {
        let agent = FakeAgent::new(one_section_reply("refresh-h", SectionKey::Features, "Agent text"));
        let sb = Sb::new(&[("refresh-h", true)]).with_agent(agent);
        let mut human = section("Mine", "Written by me");
        human.origin = Origin::Human;
        sb.save(&profile("refresh-h", &[(SectionKey::Features, human)]));
        let loc = sb.post_to("/insights/memory/section/refresh", "project=refresh-h&section=features");
        assert_eq!(loc, "/insights?view=memory&project=refresh-h&page=features&notice=refresh-started");
        wait_idle("refresh-h");
        let s = &sb.load("refresh-h").sections[&SectionKey::Features];
        assert_eq!((s.body.as_str(), s.origin), ("Written by me", Origin::Human), "not overwritten");
        let all = approvals::all(&sb.approvals());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0]["kind"], profile_sync::KIND);
        assert_eq!(all[0]["target"]["new"], "Agent text");
        let page = sb.get("/insights?view=memory&project=refresh-h&page=proposals");
        assert!(page.contains("Agent text") && page.contains("action='/insights/memory/proposal/decide'"), "{page}");
        assert!(page.contains("Proposal raised"), "{page}");
        let bad = sb.post_to("/insights/memory/section/refresh", "project=refresh-h&section=nope");
        assert!(bad.ends_with("notice=bad-input"), "{bad}");
    }

    fn real_proposal(sb: &Sb, alias: &str, key: SectionKey, new: &str) -> String {
        let agent = FakeAgent::new(one_section_reply(alias, key, new));
        let work = sb.dir.path().join("repos").join(alias);
        profile_sync::propose_refresh(alias, key, &work, &Default::default(), &*agent, now(), Some(&sb.root()), &sb.approvals())
            .unwrap()
            .unwrap()
            .id
    }

    #[test]
    fn decide_approve_applies_and_dismiss_does_not_write() {
        let sb = Sb::new(&[("decide-a", true)]);
        sb.save(&profile("decide-a", &[(SectionKey::Stack, section("Old", "Old stack")), (SectionKey::Gotchas, section("G", "Old gotchas"))]));
        let id = real_proposal(&sb, "decide-a", SectionKey::Stack, "New stack");
        let loc = sb.post_to("/insights/memory/proposal/decide", &format!("project=decide-a&id={id}&decision=approve"));
        assert_eq!(loc, "/insights?view=memory&project=decide-a&page=proposals&notice=approved");
        let p = sb.load("decide-a");
        assert_eq!(p.sections[&SectionKey::Stack].body, "New stack");
        assert_eq!(approvals::get(&sb.approvals(), &id).unwrap()["status"], "approved");

        let id2 = real_proposal(&sb, "decide-a", SectionKey::Gotchas, "New gotchas");
        let before = std::fs::read_to_string(project_profile::profile_path("decide-a", Some(&sb.root()))).unwrap();
        let loc = sb.post_to("/insights/memory/proposal/decide", &format!("project=decide-a&id={id2}&decision=dismiss"));
        assert!(loc.ends_with("notice=dismissed"), "{loc}");
        assert_eq!(approvals::get(&sb.approvals(), &id2).unwrap()["status"], "denied");
        let after = std::fs::read_to_string(project_profile::profile_path("decide-a", Some(&sb.root()))).unwrap();
        assert_eq!(before, after);
        let again = sb.post_to("/insights/memory/proposal/decide", &format!("project=decide-a&id={id2}&decision=approve"));
        assert!(again.ends_with("notice=already-decided"), "{again}");
        let bad = sb.post_to("/insights/memory/proposal/decide", &format!("project=decide-a&id={id2}&decision=yes"));
        assert!(bad.ends_with("notice=bad-input"), "{bad}");
    }

    #[test]
    fn decide_approve_after_section_changed_shows_notice_and_does_not_clobber() {
        let sb = Sb::new(&[("decide-c", true)]);
        sb.save(&profile("decide-c", &[(SectionKey::Stack, section("Old", "Old stack"))]));
        let id = real_proposal(&sb, "decide-c", SectionKey::Stack, "Agent stack");
        // A person edits the section after the proposal was raised.
        sb.post_to("/insights/memory/section/save", "project=decide-c&section=stack&summary=Mine&body=Hand+written");
        let loc = sb.post_to("/insights/memory/proposal/decide", &format!("project=decide-c&id={id}&decision=approve"));
        assert!(loc.ends_with("page=proposals&notice=changed"), "{loc}");
        assert_eq!(sb.load("decide-c").sections[&SectionKey::Stack].body, "Hand written");
        assert_eq!(approvals::get(&sb.approvals(), &id).unwrap()["status"], "pending", "not marked approved");
        assert!(sb.get(&loc).contains("changed since the proposal was raised"));
    }

    #[test]
    fn decide_rejects_id_of_other_alias_or_kind() {
        let sb = Sb::new(&[("rej-a", true), ("rej-b", true)]);
        sb.save(&profile("rej-a", &[(SectionKey::Stack, section("S", "B"))]));
        let other = propose(&sb, "rej-b", "stack", "B", "x");
        let mr = approvals::create(
            &sb.approvals(),
            &NewApproval { kind: "mr".into(), project: "rej-a".into(), title: "t".into(), ..Default::default() },
            now(),
        )
        .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        for id in [other.as_str(), mr.as_str(), "../../etc/passwd", "apr_missing"] {
            let loc = sb.post_to("/insights/memory/proposal/decide", &format!("project=rej-a&id={}&decision=dismiss", crate::ui::pyfmt::quote(id, "")));
            assert!(loc.ends_with("notice=bad-input"), "{id}: {loc}");
        }
        assert_eq!(approvals::get(&sb.approvals(), &other).unwrap()["status"], "pending");
        assert_eq!(approvals::get(&sb.approvals(), &mr).unwrap()["status"], "pending");
    }

    #[test]
    fn sync_settings_toggle_persists_and_corrupt_store_is_not_overwritten() {
        let sb = Sb::new(&[("alpha", true)]);
        let loc = sb.post_to("/insights/memory/sync/settings", "enabled=0&weekday=fri&hour=7");
        assert_eq!(loc, "/insights?view=memory&notice=sync-off");
        let s = profile_sync::load(&sb.store()).unwrap().settings;
        assert_eq!((s.enabled, s.weekday, s.hour), (false, chrono::Weekday::Fri, 7));
        let loc = sb.post_to("/insights/memory/sync/settings", "enabled=on&weekday=fri&hour=7");
        assert!(loc.ends_with("notice=sync-on"), "{loc}");
        let loc = sb.post_to("/insights/memory/sync/settings", "enabled=on&weekday=tue&hour=23");
        assert!(loc.ends_with("notice=schedule-saved"), "{loc}");
        let s = profile_sync::load(&sb.store()).unwrap().settings;
        assert_eq!((s.enabled, s.weekday, s.hour), (true, chrono::Weekday::Tue, 23));
        for bad in ["enabled=on&weekday=funday&hour=7", "enabled=on&weekday=mon&hour=24", "enabled=on&weekday=mon&hour=x"] {
            assert!(sb.post_to("/insights/memory/sync/settings", bad).ends_with("notice=bad-input"), "{bad}");
        }
        std::fs::write(sb.store(), "{corrupt").unwrap();
        let loc = sb.post_to("/insights/memory/sync/settings", "enabled=0&weekday=mon&hour=9");
        assert!(loc.ends_with("notice=store-bad"), "{loc}");
        assert_eq!(std::fs::read_to_string(sb.store()).unwrap(), "{corrupt");
        assert!(sb.post_to("/insights/memory/sync/now", "").ends_with("notice=store-bad"));
        assert_eq!(std::fs::read_to_string(sb.store()).unwrap(), "{corrupt");
    }

    #[test]
    fn sync_now_sets_flag() {
        let sb = Sb::new(&[("alpha", true)]);
        sb.post_to("/insights/memory/sync/settings", "enabled=0&weekday=mon&hour=9");
        let loc = sb.post_to("/insights/memory/sync/now", "");
        assert_eq!(loc, "/insights?view=memory&notice=sync-requested");
        let store = profile_sync::load(&sb.store()).unwrap();
        assert!(store.sync_requested && !store.settings.enabled, "works with the switch off");
    }

    #[test]
    fn redirects_never_contain_raw_input() {
        let sb = Sb::new(&[("alpha", true)]);
        sb.save(&profile("alpha", &[(SectionKey::Stack, section("S", "B"))]));
        let evil = "%22%3E%3Cscript%3Ealert(1)%3C%2Fscript%3E%2F%2Fevil.example";
        let mut seen = Vec::new();
        for (url, body) in [
            ("/insights/memory/build", format!("project={evil}&from={evil}")),
            ("/insights/memory/build", format!("project=alpha&from={evil}")),
            ("/insights/memory/section/save", format!("project=alpha&section={evil}&summary=a&body=b")),
            ("/insights/memory/section/save", format!("project={evil}&section=stack&summary=a&body=b")),
            ("/insights/memory/section/refresh", format!("project=alpha&section={evil}")),
            ("/insights/memory/proposal/decide", format!("project=alpha&id={evil}&decision={evil}")),
            ("/insights/memory/sync/settings", format!("enabled={evil}&weekday={evil}&hour={evil}")),
            ("/insights/memory/sync/now", format!("project={evil}")),
            ("/insights/memory/sync/slack", format!("notify={evil}")),
            ("/insights/memory/approve", format!("project={evil}")),
        ] {
            seen.push(sb.post_to(url, &body));
        }
        for loc in seen {
            assert!(loc.starts_with("/insights?view=memory"), "{loc}");
            for bad in ["<", ">", "\"", "script", "evil", "//"] {
                assert!(!loc.contains(bad), "{loc}");
            }
        }
    }

    #[test]
    fn notices_are_whitelisted_and_translated() {
        let sb = Sb::new(&[("alpha", true)]);
        let html = sb.get("/insights?view=memory&notice=sync-requested");
        assert!(html.contains("Sync requested"), "{html}");
        let html = sb.get("/insights?view=memory&notice=%3Cb%3Ehello%3C%2Fb%3E");
        assert!(!html.contains("class='mem-notice") && !html.contains("hello"), "unknown codes show nothing");
        let req = crate::ui::routes::UiRequest::get("/insights?view=memory&notice=sync-requested").with_header("cookie", "loop_lang=fr");
        let r = sb.state.registry.dispatch(&sb.state, &req).unwrap();
        let fr = String::from_utf8(r.body).unwrap();
        assert!(fr.contains("class='mem-notice") && !fr.contains("Sync requested. It starts within a minute."), "{fr}");
    }

    #[test]
    fn pages_carry_the_controls() {
        let sb = Sb::new(&[("alpha", true), ("beta", true)]);
        sb.save(&profile("alpha", &[(SectionKey::Stack, section("S <b>", "Body"))]));
        let id = propose(&sb, "alpha", "stack", "Body", "New");
        let overview = sb.get("/insights?view=memory");
        for needle in [
            "action='/insights/memory/sync/settings'",
            "role='switch'",
            "aria-checked='true'",
            "name='weekday'",
            "name='hour'",
            "action='/insights/memory/sync/now'",
            "name='project' value='all'",
        ] {
            assert!(overview.contains(needle), "{needle}: {overview}");
        }
        let bar = &overview[overview.find("class='mem-bar'").unwrap()..];
        let bar = &bar[..bar.find("class='mem-grid'").unwrap()];
        assert!(!bar.contains("daemon-action-form"), "{bar}");
        let main = sb.get("/insights?view=memory&project=alpha");
        assert!(main.contains("action='/insights/memory/build'") && main.contains("Refresh profile"), "{main}");
        assert!(sb.get("/insights?view=memory&project=beta").contains("Build profile"));
        let sub = sb.get("/insights?view=memory&project=alpha&page=stack");
        assert!(sub.contains("action='/insights/memory/section/save'") && sub.contains("class='mem-input'") && sub.contains("class='mem-textarea'"), "{sub}");
        assert!(sub.contains("value='S &lt;b&gt;'"), "prefilled and escaped: {sub}");
        assert!(sub.contains("action='/insights/memory/section/refresh'"), "{sub}");
        let props = sb.get("/insights?view=memory&project=alpha&page=proposals");
        assert!(props.contains(&format!("name='id' value='{id}'")) && props.contains("value='approve'") && props.contains("value='dismiss'"), "{props}");
    }

    #[test]
    fn a_redacted_section_editor_is_not_prefilled() {
        let sb = Sb::new(&[("alpha", true)]);
        sb.save(&profile("alpha", &[(SectionKey::Secrets, section("names", "TOKEN glpat-abcdefghijklmnop1234"))]));
        let html = sb.get("/insights?view=memory&project=alpha&page=secrets");
        assert!(!html.contains("glpat-"), "{html}");
        assert!(html.contains("action='/insights/memory/section/save'"));
    }

    // ---------------------------------------------------------------- fix round 1

    #[test]
    fn build_all_asks_for_confirmation_first() {
        let sb = Sb::new(&[("conf-a", true), ("conf-b", true), ("conf-issues", false)]);
        let html = sb.get("/insights?view=memory");
        let bar = &html[html.find("class='mem-bar'").unwrap()..];
        let bar = &bar[..bar.find("class='mem-grid'").unwrap()];
        // The opener is a link (no JS: it opens the dialog server-side), not a POST.
        assert!(bar.contains("href='/insights?view=memory&amp;confirm=build-all'"), "{bar}");
        let dialog = &bar[bar.find("<dialog").expect("a confirm dialog")..];
        let before_dialog = &bar[..bar.find("<dialog").unwrap()];
        assert!(!before_dialog.contains("name='project' value='all'"), "Build all does not post directly");
        assert!(dialog.contains("name='project' value='all'") && dialog.contains("action='/insights/memory/build'"));
        assert!(dialog.contains("2 tracked projects"), "{dialog}");
        assert!(dialog.contains("Cancel"));
        assert!(!bar.contains("<dialog id='mem-build-all' class='mem-dialog' open"), "closed until asked");
        let asked = sb.get("/insights?view=memory&confirm=build-all");
        assert!(asked.contains("<dialog id='mem-build-all' class='mem-dialog' open"), "{asked}");
        // The route itself is unchanged and still needs CSRF.
        let req = crate::ui::routes::UiRequest::new(Method::POST, "/insights/memory/build")
            .with_header("content-type", "application/x-www-form-urlencoded")
            .with_body("project=all");
        assert_eq!(sb.state.registry.dispatch(&sb.state, &req).unwrap().status, 403);
    }

    #[test]
    fn rebuilding_an_approved_profile_asks_first_and_a_draft_does_not() {
        let sb = Sb::new(&[("conf-ok", true), ("conf-draft", true), ("conf-none", true)]);
        sb.save(&profile("conf-ok", &[(SectionKey::Stack, section("S", "B"))]));
        let mut d = profile("conf-draft", &[(SectionKey::Stack, section("S", "B"))]);
        d.status = Status::Draft;
        sb.save(&d);
        let approved = sb.get("/insights?view=memory&project=conf-ok");
        assert!(approved.contains("id='mem-build-project'") && approved.contains("confirm=build'"), "{approved}");
        assert!(approved.contains("back to Draft"), "{approved}");
        assert!(sb.get("/insights?view=memory&project=conf-ok&confirm=build").contains("<dialog id='mem-build-project' class='mem-dialog' open"));
        for alias in ["conf-draft", "conf-none"] {
            let html = sb.get(&format!("/insights?view=memory&project={alias}"));
            assert!(!html.contains("id='mem-build-project'"), "{alias}");
            assert!(html.contains(&format!("name='project' value='{alias}'")), "{alias} posts directly");
        }
    }

    #[test]
    fn an_approved_proposal_that_was_not_applied_says_so() {
        let sb = Sb::new(&[("unapplied", true)]);
        sb.save(&profile("unapplied", &[(SectionKey::Stack, section("Old", "Old stack")), (SectionKey::Gotchas, section("G", "Old gotchas"))]));
        let fits = real_proposal(&sb, "unapplied", SectionKey::Stack, "New stack");
        let stale = real_proposal(&sb, "unapplied", SectionKey::Gotchas, "New gotchas");
        for id in [&fits, &stale] {
            approvals::decide(&sb.approvals(), id, "approve", "me", now()).unwrap();
        }
        sb.post_to("/insights/memory/section/save", "project=unapplied&section=gotchas&summary=Mine&body=Hand");
        let html = sb.get("/insights?view=memory&project=unapplied&page=proposals");
        assert_eq!(html.matches("Approved, not applied").count(), 2, "{html}");
        assert!(html.contains("value='approve'"), "the one that still fits keeps Apply");
        assert!(html.contains("changed after this was approved"), "{html}");
        assert!(html.contains("href='/insights?view=memory&amp;project=unapplied&amp;page=gotchas'"), "{html}");
        // Applied ones are plain Approved.
        sb.post_to("/insights/memory/proposal/decide", &format!("project=unapplied&id={fits}&decision=approve"));
        let html = sb.get("/insights?view=memory&project=unapplied&page=proposals");
        assert_eq!(html.matches("Approved, not applied").count(), 1, "{html}");
    }

    #[test]
    fn save_counts_length_after_crlf_normalisation() {
        let sb = Sb::new(&[("crlf", true)]);
        sb.save(&profile("crlf", &[(SectionKey::Stack, section("S", "B"))]));
        // 24,000 characters as submitted, 18,000 once CRLF becomes LF.
        let body = "ab%0D%0A".repeat(6000);
        let loc = sb.post_to("/insights/memory/section/save", &format!("project=crlf&section=stack&summary=s&body={body}"));
        assert!(loc.ends_with("notice=saved"), "{loc}");
        assert_eq!(sb.load("crlf").sections[&SectionKey::Stack].body.matches('\n').count(), 5999);
    }

    #[test]
    fn a_too_long_save_keeps_the_typed_text_but_a_secret_is_never_kept() {
        let sb = Sb::new(&[("keep", true)]);
        sb.save(&profile("keep", &[(SectionKey::Stack, section("Stored summary", "Stored body"))]));
        let long = "x".repeat(301);
        let loc = sb.post_to("/insights/memory/section/save", &format!("project=keep&section=stack&summary={long}&body=Typed+body"));
        assert!(loc.ends_with("notice=too-long"), "{loc}");
        let html = sb.get(&loc);
        assert!(html.contains(&long) && html.contains("Typed body"), "the typed text is back in the editor");
        assert!(html.contains("<details class='mem-edit' open>"), "{html}");
        assert!(html.contains("Nothing was saved"), "{html}");
        // A secret is refused first and never kept, even when it is also too long.
        let secret = "glpat-abcdefghijklmnop1234";
        let loc = sb.post_to("/insights/memory/section/save", &format!("project=keep&section=stack&summary={long}&body={secret}"));
        assert!(loc.ends_with("notice=secret"), "{loc}");
        assert!(!sb.get(&loc).contains("glpat-"));
        // A successful save drops the kept draft.
        sb.post_to("/insights/memory/section/save", "project=keep&section=stack&summary=Done&body=Saved");
        let html = sb.get("/insights?view=memory&project=keep&page=stack");
        assert!(!html.contains(&long) && !html.contains("<details class='mem-edit' open>"), "{html}");
    }

    // ---------------------------------------------------------------- fix round 2: kept-edit caps

    fn dk(alias: &str, key: SectionKey) -> super::DraftKey {
        (PathBuf::from("/h"), alias.to_string(), key)
    }

    #[test]
    fn kept_edits_evict_the_oldest_past_the_entry_cap() {
        let mut store = super::DraftStore::default();
        for i in 0..=super::DRAFT_MAX_ENTRIES {
            store.keep(dk(&format!("p{i}"), SectionKey::Stack), "s", "b", now() + chrono::Duration::seconds(i as i64));
        }
        let at = now() + chrono::Duration::seconds(super::DRAFT_MAX_ENTRIES as i64);
        assert_eq!(store.len(), super::DRAFT_MAX_ENTRIES);
        assert!(store.get(&dk("p0", SectionKey::Stack), at).is_none(), "the oldest went");
        assert!(store.get(&dk("p1", SectionKey::Stack), at).is_some());
        assert!(store.get(&dk(&format!("p{}", super::DRAFT_MAX_ENTRIES), SectionKey::Stack), at).is_some());
    }

    #[test]
    fn kept_edits_stay_under_the_total_byte_cap() {
        let mut store = super::DraftStore::default();
        let big = "x".repeat(super::DRAFT_MAX_ENTRY_BYTES - 10);
        for i in 0..20 {
            store.keep(dk(&format!("b{i}"), SectionKey::Stack), "s", &big, now() + chrono::Duration::seconds(i));
        }
        assert!(store.bytes() <= super::DRAFT_MAX_TOTAL_BYTES, "{}", store.bytes());
        let at = now() + chrono::Duration::seconds(19);
        assert!(store.get(&dk("b19", SectionKey::Stack), at).is_some(), "the newest is kept");
        assert!(store.get(&dk("b0", SectionKey::Stack), at).is_none(), "the oldest went");
    }

    #[test]
    fn an_oversize_edit_is_not_kept_and_drops_an_older_one_for_the_section() {
        let mut store = super::DraftStore::default();
        store.keep(dk("o", SectionKey::Stack), "old typed", "b", now());
        let huge = "y".repeat(super::DRAFT_MAX_ENTRY_BYTES + 1);
        store.keep(dk("o", SectionKey::Stack), "s", &huge, now());
        assert!(store.get(&dk("o", SectionKey::Stack), now()).is_none(), "no stale text either");
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn expired_kept_edits_are_removed_on_read() {
        let mut store = super::DraftStore::default();
        store.keep(dk("e", SectionKey::Stack), "s", "b", now());
        store.keep(dk("f", SectionKey::Stack), "s", "b", now());
        let later = now() + chrono::Duration::minutes(super::DRAFT_MINUTES + 1);
        assert!(store.get(&dk("e", SectionKey::Stack), later).is_none());
        assert_eq!(store.len(), 0, "every expired entry is pruned, not just the one asked for");
    }

    #[test]
    fn an_oversize_save_reopens_the_editor_with_the_stored_text() {
        let sb = Sb::new(&[("oversize", true)]);
        sb.save(&profile("oversize", &[(SectionKey::Stack, section("Stored summary", "Stored body"))]));
        let small = "z".repeat(301);
        sb.post_to("/insights/memory/section/save", &format!("project=oversize&section=stack&summary={small}&body=b"));
        assert!(sb.get("/insights?view=memory&project=oversize&page=stack").contains(&small));
        let huge = "q".repeat(super::DRAFT_MAX_ENTRY_BYTES + 1);
        let loc = sb.post_to("/insights/memory/section/save", &format!("project=oversize&section=stack&summary=s&body={huge}"));
        assert!(loc.ends_with("notice=too-long"), "{loc}");
        let html = sb.get(&loc);
        assert!(!html.contains(&small) && !html.contains("qqqqqqqqqq"), "neither the old nor the oversize text");
        assert!(html.contains("value='Stored summary'") && !html.contains("<details class='mem-edit' open>"), "{html}");
        // A secret is still never stored.
        let secret = "glpat-abcdefghijklmnop1234";
        sb.post_to("/insights/memory/section/save", &format!("project=oversize&section=stack&summary={small}&body={secret}"));
        assert!(!sb.get("/insights?view=memory&project=oversize&page=stack").contains("glpat-"));
    }

    // ------------------------------------------------------------------ graph view

    fn count(html: &str, needle: &str) -> usize {
        html.matches(needle).count()
    }

    /// The `<svg …>…</svg>` part of the graph page.
    fn svg_of(html: &str) -> &str {
        let start = html.find("<svg class='mg-svg").expect("graph svg");
        let end = start + html[start..].find("</svg>").expect("svg end");
        &html[start..end]
    }

    fn link(to: &str, kind: &str, evidence: &str) -> Link {
        Link { to: to.into(), kind: kind.into(), evidence: evidence.into() }
    }

    fn put_issue(sb: &Sb, project_id: &str, iid: i64, issue: serde_json::Value) {
        let dir = sb
            .state
            .user_home
            .join(".gitlab/cache/main/projects")
            .join(project_id.replace('/', "__"))
            .join("issues");
        std::fs::create_dir_all(&dir).unwrap();
        let mut issue = issue;
        issue["iid"] = json!(iid);
        std::fs::write(dir.join(format!("{iid}.json")), issue.to_string()).unwrap();
    }

    #[test]
    fn graph_svg_has_one_node_per_tracked_project() {
        let sb = Sb::new(&[("alpha", true), ("beta", true), ("gamma", false)]);
        sb.save(&profile("alpha", &[(SectionKey::Overview, section("A.", "a"))]));
        sb.save(&profile("orphan", &[(SectionKey::Overview, section("O.", "o"))]));
        let html = sb.get("/insights?view=memory&graph=1");
        let svg = svg_of(&html);
        assert_eq!(count(svg, "<g class='mg-node "), 4, "{svg}");
        for alias in ["alpha", "beta", "gamma", "orphan"] {
            assert!(svg.contains(&format!("data-alias='{alias}'")), "missing {alias}");
        }
        // A project without a profile is a greyed node.
        assert!(svg.contains("mg-v-none"), "{svg}");
        // Every node is focusable and named for assistive tech.
        assert_eq!(count(svg, "tabindex='0'"), 4);
        // The inspector has a template per node, with the full page link.
        assert_eq!(count(&html, "<template class='mg-tpl-node'"), 4);
        assert!(html.contains("project=alpha'"));
        assert!(html.contains("Open full page"));
    }

    #[test]
    fn unevidenced_edges_not_rendered() {
        let sb = Sb::new(&[("alpha", true), ("beta", true), ("gamma", true)]);
        let mut a = profile("alpha", &[(SectionKey::Overview, section("A.", "a"))]);
        a.links.push(link("beta", "depends-on", "Gemfile: gem 'beta', path: '../beta'"));
        a.links.push(link("gamma", "calls", "   "));
        sb.save(&a);
        sb.save(&profile("beta", &[(SectionKey::Overview, section("B.", "b"))]));
        sb.save(&profile("gamma", &[(SectionKey::Overview, section("G.", "g"))]));
        let html = sb.get("/insights?view=memory&graph=1");
        let svg = svg_of(&html);
        assert_eq!(count(svg, "<path class='mg-edge "), 1, "{svg}");
        assert!(svg.contains("data-kind='depends-on'"));
        assert!(!svg.contains("data-kind='calls'"));
        assert!(svg.contains("marker-end="), "directed kinds get an arrowhead");
    }

    #[test]
    fn people_edges_hidden_by_default() {
        let sb = Sb::new(&[("alpha", true), ("beta", true)]);
        sb.save(&profile("alpha", &[(SectionKey::Overview, section("A.", "a"))]));
        sb.save(&profile("beta", &[(SectionKey::Overview, section("B.", "b"))]));
        for (pid, iid) in [("group/alpha", 1), ("group/beta", 2)] {
            put_issue(&sb, pid, iid, json!({"title": "t", "author": {"username": "ann"},
                "assignees": [{"username": "bob"}], "updated_at": "2026-11-01T00:00:00Z"}));
        }
        let html = sb.get("/insights?view=memory&graph=1");
        let svg = svg_of(&html);
        assert!(svg.contains("data-kind='shares-people'"), "{svg}");
        // The svg starts with the people kind hidden, and its chip is off.
        assert!(html.contains("<svg class='mg-svg mg-hide-shares-people'"), "{html}");
        assert!(html.contains("data-kind='shares-people' aria-pressed='false'"), "{html}");
        assert!(html.contains("data-kind='depends-on' aria-pressed='true'"));
        // The list rows for people links are hidden the same way.
        assert!(html.contains("class='mem-graph-list mg-hide-shares-people'"), "{html}");
    }

    #[test]
    fn fallback_list_present() {
        let sb = Sb::new(&[("alpha", true), ("beta", true)]);
        let mut a = profile("alpha", &[(SectionKey::Overview, section("A.", "a"))]);
        a.links.push(link("beta", "calls", "config/services.yml: BETA_URL"));
        sb.save(&a);
        let html = sb.get("/insights?view=memory&graph=1");
        let start = html.find("class='mem-graph-list").expect("list view");
        let list = &html[start..];
        assert!(list.starts_with("class='mem-graph-list"));
        // Open without JS; the page script closes it on wide screens.
        assert!(html[..start].ends_with("<details "), "{}", &html[start - 40..start + 60]);
        assert!(list.contains("config/services.yml: BETA_URL"));
        assert!(list.contains(">calls<"), "{list}");
        assert!(list.contains("project=beta'"));
        assert!(list.contains(" open>"));
    }

    #[test]
    fn graph_uses_issue_digests_from_the_state_home_not_the_real_home() {
        let sb = Sb::new(&[("alpha", true), ("beta", true)]);
        sb.save(&profile("beta", &[(SectionKey::Overview, section("B.", "b"))]));
        put_issue(&sb, "group/alpha", 7, json!({"title": "Checkout fails",
            "description": "Blocked by group/beta#4", "updated_at": "2026-11-01T00:00:00Z"}));
        let html = sb.get("/insights?view=memory&graph=1");
        let svg = svg_of(&html);
        assert!(svg.contains("data-kind='issue-link'"), "{svg}");
        assert!(svg.contains("data-from='alpha' data-to='beta'"), "{svg}");
        assert!(html.contains("issues #7"));
        // The overview and the project page count the issue link too.
        assert!(sb.get("/insights?view=memory").contains("1 linked project"));
        let page = sb.get("/insights?view=memory&project=beta");
        assert!(page.contains("linked issues"), "{page}");
        // Activity sizes the node: alpha has one recent issue, beta none.
        assert!(svg.contains("data-alias='alpha' data-activity='1'"), "{svg}");
        assert!(svg.contains("data-alias='beta' data-activity='0'"), "{svg}");
    }

    #[test]
    fn node_freshness_comes_from_the_overview_verdict_not_the_approximation() {
        let sb = Sb::new(&[("alpha", true), ("beta", true), ("gamma", true)]);
        // Only one section: the overview says Needs review (the rest are missing);
        // `derive` alone would call it Fresh.
        sb.save(&profile("alpha", &[(SectionKey::Overview, section("A.", "a"))]));
        // A source that no longer matches the checkout: Stale.
        let mut s = section("B.", "b");
        s.sources.push(Source { path: "README.md".into(), sha256: "0".repeat(64) });
        let full: Vec<(SectionKey, Section)> = SectionKey::ALL.into_iter().map(|k| (k, s.clone())).collect();
        sb.save(&profile("beta", &full));
        // All ten sections written by a person: Human-edited.
        let mut h = section("G.", "g");
        h.origin = Origin::Human;
        let human: Vec<(SectionKey, Section)> = SectionKey::ALL.into_iter().map(|k| (k, h.clone())).collect();
        sb.save(&profile("gamma", &human));
        let html = sb.get("/insights?view=memory&graph=1");
        let svg = svg_of(&html);
        let class_of = |alias: &str| {
            let at = svg.find(&format!("data-alias='{alias}'")).unwrap();
            let open = svg[..at].rfind("<g class='mg-node ").unwrap();
            svg[open..at].to_string()
        };
        assert!(class_of("alpha").contains("mg-v-review"), "{}", class_of("alpha"));
        assert!(class_of("beta").contains("mg-v-stale"), "{}", class_of("beta"));
        assert!(class_of("gamma").contains("mg-v-human"), "{}", class_of("gamma"));
        // The same verdict pills as the overview, in the inspector templates.
        assert!(html.contains("mem-pill-review") && html.contains("mem-pill-stale") && html.contains("mem-pill-human"));
    }

    #[test]
    fn evidence_is_escaped_and_secret_free() {
        let sb = Sb::new(&[("alpha", true), ("beta", true)]);
        let secret = "glpat-abcdefghijklmnop1234";
        let mut a = profile(
            "alpha",
            &[
                (SectionKey::Overview, section("<script>alert(1)</script>", "x")),
                (SectionKey::Secrets, section("names", &format!("- `GITLAB_TOKEN` = {secret}\n- `SHARED_API_KEY`"))),
            ],
        );
        a.links.push(link("beta", "calls", "<img src=x onerror=alert(1)> in config"));
        a.links.push(link("beta", "depends-on", &format!("token {secret}")));
        sb.save(&a);
        sb.save(&profile(
            "beta",
            &[
                (SectionKey::Overview, section(&format!("Uses {secret}"), "y")),
                (SectionKey::Secrets, section("names", "- `SHARED_API_KEY`")),
            ],
        ));
        let html = sb.get("/insights?view=memory&graph=1");
        assert!(!html.contains("glpat-"), "a secret value leaked");
        assert!(!html.contains("<script>alert(1)"));
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt; in config"), "{html}");
        // The shared secret NAME is evidence; the value is not.
        assert!(svg_of(&html).contains("data-kind='shares-secret'"));
        assert!(html.contains("SHARED_API_KEY"));
    }

    #[test]
    fn graph_with_no_projects_shows_the_empty_state() {
        let sb = Sb::new(&[]);
        let html = sb.get("/insights?view=memory&graph=1");
        assert!(!html.contains("<svg class='mg-svg"));
        assert!(html.contains("No projects configured yet"), "{html}");
    }

    #[test]
    fn graph_with_one_project_centres_it_and_has_no_edges() {
        let sb = Sb::new(&[("solo", true)]);
        sb.save(&profile("solo", &[(SectionKey::Overview, section("S.", "s"))]));
        let html = sb.get("/insights?view=memory&graph=1");
        let svg = svg_of(&html);
        assert_eq!(count(svg, "<g class='mg-node "), 1);
        assert!(svg.contains("transform='translate(800 500)'"), "{svg}");
        assert_eq!(count(svg, "<path class='mg-edge "), 0);
        assert!(html.contains("No links between projects yet."), "{html}");
    }

    #[test]
    fn graph_caps_drawn_edges_with_a_notice() {
        let aliases: Vec<String> = (0..50).map(|i| format!("p{i:02}")).collect();
        let spec: Vec<(&str, bool)> = aliases.iter().map(|a| (a.as_str(), false)).collect();
        let sb = Sb::new(&spec);
        for a in &aliases {
            let mut p = profile(a, &[(SectionKey::Overview, section("x", "x"))]);
            for b in aliases.iter().filter(|b| *b != a) {
                p.links.push(link(b, "declared", &format!("{a} mentions {b}")));
            }
            sb.save(&p);
        }
        let html = sb.get("/insights?view=memory&graph=1");
        let svg = svg_of(&html);
        assert_eq!(count(svg, "<path class='mg-edge "), super::MAX_DRAWN_EDGES, "cap");
        assert!(html.contains("Showing the 2000 strongest of 2450 links."), "notice");
        assert_eq!(count(svg, "<g class='mg-node "), 50);
    }

    #[test]
    fn graph_ignores_unknown_query_values() {
        let sb = Sb::new(&[("alpha", true)]);
        sb.save(&profile("alpha", &[(SectionKey::Overview, section("A.", "a"))]));
        let html = sb.get("/insights?view=memory&graph=1&kind=%3Cscript%3E&zoom=NaN&select=..%2F..");
        assert!(html.contains("<svg class='mg-svg"));
        assert!(!html.contains("<script>'") && !html.contains("../.."));
        // Not "1": the cards, not the graph.
        assert!(sb.get("/insights?view=memory&graph=%3Cb%3E").contains("class='mem-grid'"));
    }

    #[test]
    fn graph_strings_are_translated() {
        let sb = Sb::new(&[("alpha", true), ("beta", true)]);
        let mut a = profile("alpha", &[(SectionKey::Overview, section("A.", "a"))]);
        a.links.push(link("beta", "calls", "x"));
        sb.save(&a);
        let req = crate::ui::routes::UiRequest::get("/insights?view=memory&graph=1")
            .with_header("cookie", "loop_lang=fr");
        let r = sb.state.registry.dispatch(&sb.state, &req).unwrap();
        let html = String::from_utf8(r.body).unwrap();
        assert!(!html.contains(">Open full page<"), "untranslated");
        assert!(!html.contains("aria-label='Zoom in'"));
    }

    #[test]
    fn overview_and_project_page_run_no_layout() {
        use loopx_core::profile_graph::layout_runs_on_this_thread as runs;
        let sb = Sb::new(&[("alpha", true), ("beta", true)]);
        let mut a = profile("alpha", &[(SectionKey::Overview, section("A.", "a"))]);
        a.links.push(link("beta", "calls", "x"));
        sb.save(&a);
        let before = runs();
        assert!(sb.get("/insights?view=memory").contains("1 linked project"));
        assert!(sb.get("/insights?view=memory&project=alpha").contains("project=beta'"));
        assert_eq!(runs(), before, "counts need edges, not a layout");
        sb.get("/insights?view=memory&graph=1");
        assert_eq!(runs(), before + 1, "the graph lays out once per request");
    }

    #[test]
    fn overview_with_200_projects_renders_quickly() {
        let aliases: Vec<String> = (0..200).map(|i| format!("s{i:03}")).collect();
        let spec: Vec<(&str, bool)> = aliases.iter().map(|a| (a.as_str(), false)).collect();
        let sb = Sb::new(&spec);
        for (i, a) in aliases.iter().enumerate() {
            let mut p = profile(a, &[(SectionKey::Overview, section("x", "x"))]);
            for j in [i + 1, i + 7, i + 31] {
                p.links.push(link(&aliases[j % 200], "calls", "cfg"));
            }
            sb.save(&p);
            if i % 3 == 0 {
                put_issue(&sb, &format!("group/{a}"), 1, json!({"title": "t",
                    "description": format!("see group/{}#2", aliases[(i + 11) % 200])}));
            }
        }
        let t = std::time::Instant::now();
        let html = sb.get("/insights?view=memory");
        let took = t.elapsed();
        assert!(html.contains("class='mem-grid'"));
        eprintln!("overview, 200 projects: {took:?}");
        assert!(took < std::time::Duration::from_secs(3), "{took:?}");
    }

    #[test]
    fn graph_is_keyboard_reachable_and_wheel_zoom_is_opt_in() {
        let sb = Sb::new(&[("alpha", true), ("beta", true)]);
        let mut a = profile("alpha", &[(SectionKey::Overview, section("A.", "a"))]);
        a.links.push(link("beta", "calls", "x"));
        sb.save(&a);
        let html = sb.get("/insights?view=memory&graph=1");
        // The inspector announces its content and takes focus (tabindex -1).
        assert!(html.contains("class='mg-inspector mg-hide-shares-people' aria-live='polite'"), "{html}");
        // In a node's inspector, each link's kind is a button that opens that link.
        assert!(html.contains("<button type='button' class='mg-kind mg-k-calls' data-edge='0'>"), "{html}");
        assert!(svg_of(&html).contains("data-edge='0'"));
        // Plain scrolling scrolls the page; the stage carries the translated hint.
        assert!(html.contains("data-wheel-hint='Use Ctrl/⌘ + scroll to zoom'"), "{html}");
        let script = crate::ui::assets::text("memory/graph.js").unwrap();
        assert!(script.contains("ev.ctrlKey || ev.metaKey"));
    }

    // ---------------------------------------------------------------- Slack messages

    use loopx_core::slack_report::Report;

    /// Every message a job handed over, with whether its live record was already
    /// gone at that moment.
    #[derive(Default)]
    struct Outbox(Mutex<Vec<(Report, bool)>>);

    impl Outbox {
        fn titles(&self) -> Vec<String> {
            self.0.lock().unwrap().iter().map(|(r, _)| r.title.clone()).collect()
        }
        fn text(&self, i: usize) -> String {
            loopx_core::slack_report::plain_text(&self.0.lock().unwrap()[i].0)
        }
    }

    impl Sb {
        fn with_outbox(mut self) -> (Self, Arc<Outbox>) {
            let out = Arc::new(Outbox::default());
            let (o, base) = (out.clone(), loopx_core::agent_activity::beside(&self.state.status_path_for_loop("issue-loop")));
            self.state = self.state.with_profile_notifier(Arc::new(move |r: &Report| {
                let clean = loopx_core::profile_progress::running(&base).is_empty();
                o.0.lock().unwrap().push((r.clone(), clean));
                true
            }));
            (self, out)
        }
    }

    #[test]
    fn a_finished_build_sends_one_message_after_its_record_is_gone() {
        let alias = "slack-a";
        let (sb, out) = Sb::new(&[(alias, true)]).with_agent(FakeAgent::new(full_reply(alias, "Built"))).with_outbox();
        sb.post_to("/insights/memory/build", &format!("project={alias}&from=project"));
        wait_idle(alias);
        assert_eq!(out.titles(), [format!("Project profile built \u{b7} {alias}")]);
        assert!(out.0.lock().unwrap()[0].1, "the pill was cleared first");
        let text = out.text(0);
        assert!(text.contains("10 sections written") && text.contains("Draft ready for review"), "{text}");
        assert!(!text.contains("Built overview"), "no profile text: {text}");
    }

    #[test]
    fn a_failed_build_sends_the_fixed_reason_only() {
        let alias = "slack-b";
        let (mut sb, out) = Sb::new(&[(alias, true)]).with_outbox();
        sb.state = sb.state.with_profile_agent(Arc::new(FailingAgent("FileNotFoundError")));
        sb.post_to("/insights/memory/build", &format!("project={alias}&from=project"));
        wait_idle(alias);
        assert_eq!(out.titles(), [format!("Project profile build failed \u{b7} {alias}")]);
        let text = out.text(0);
        assert!(text.contains("The AI command could not be started"), "{text}");
        assert!(!text.contains("FileNotFoundError"), "{text}");
    }

    #[test]
    fn a_section_refresh_sends_one_message() {
        let alias = "slack-r";
        let (sb, out) = Sb::new(&[(alias, true)])
            .with_agent(FakeAgent::new(one_section_reply(alias, SectionKey::Stack, "Fresh")))
            .with_outbox();
        sb.save(&profile(alias, &[(SectionKey::Stack, section("Old", "Old body"))]));
        sb.post_to("/insights/memory/section/refresh", &format!("project={alias}&section=stack"));
        wait_idle(alias);
        assert_eq!(out.titles(), [format!("Profile section proposed \u{b7} {alias} \u{b7} Stack")]);
        assert!(out.0.lock().unwrap()[0].1);
        assert!(!out.text(0).contains("Fresh"), "no section text");
    }

    #[test]
    fn build_all_sends_one_summary_at_the_end() {
        let (sb, out) = Sb::new(&[("slack-all-a", true), ("slack-all-b", true)])
            .with_agent(FakeAgent::new(full_reply("x", "Built")))
            .with_outbox();
        sb.post_to("/insights/memory/build", "project=all&from=overview");
        for _ in 0..400 {
            if !super::build_all_running(&sb.state.home_dir) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(out.titles(), ["Project profiles built: 2 ok, 0 failed"]);
        let text = out.text(0);
        assert!(text.contains("slack-all-a \u{2014} ok") && text.contains("slack-all-b \u{2014} ok"), "{text}");
    }

    #[test]
    fn a_sender_that_panics_changes_nothing_about_the_job() {
        let alias = "slack-p";
        let mut sb = Sb::new(&[(alias, true)]).with_agent(FakeAgent::new(full_reply(alias, "Built")));
        sb.state = sb.state.with_profile_notifier(Arc::new(|_: &Report| panic!("slack exploded")));
        sb.post_to("/insights/memory/build", &format!("project={alias}&from=project"));
        wait_idle(alias);
        assert_eq!(sb.load(alias).status, Status::Draft);
        assert!(matches!(last_job(&sb.state.home_dir, alias).map(|j| j.outcome), Some(JobOutcome::Built)));
        let base = loopx_core::agent_activity::beside(&sb.state.status_path_for_loop("issue-loop"));
        assert!(loopx_core::profile_progress::running(&base).is_empty());
    }

    #[test]
    fn with_no_webhook_the_real_sender_skips_and_the_job_is_unchanged() {
        let alias = "slack-n";
        let mut sb = Sb::new(&[(alias, true)]).with_agent(FakeAgent::new(full_reply(alias, "Built")));
        let config = sb.state.user_home.join(".slack").join("config.json");
        sb.state = sb.state.with_profile_notifier(loopx_core::profile_notify::slack_sender(config));
        sb.post_to("/insights/memory/build", &format!("project={alias}&from=project"));
        wait_idle(alias);
        assert!(matches!(last_job(&sb.state.home_dir, alias).map(|j| j.outcome), Some(JobOutcome::Built)));
    }

    #[test]
    fn the_slack_switch_saves_on_click_and_turns_messages_off() {
        let alias = "slack-s";
        let (sb, out) = Sb::new(&[(alias, true)]).with_agent(FakeAgent::new(full_reply(alias, "Built"))).with_outbox();
        let loc = sb.post_to("/insights/memory/sync/slack", "notify=0");
        assert_eq!(loc, "/insights?view=memory&notice=slack-off");
        let s = profile_sync::load(&sb.store()).unwrap().settings;
        assert!(!s.notify_slack && s.enabled, "only the Slack switch changed");
        assert!(sb.get(&loc).contains("Slack messages for profile jobs are off."));
        sb.post_to("/insights/memory/build", &format!("project={alias}&from=project"));
        wait_idle(alias);
        assert!(out.titles().is_empty(), "switched off: nothing sent");
        assert_eq!(sb.post_to("/insights/memory/sync/slack", "notify=on"), "/insights?view=memory&notice=slack-on");
        assert!(profile_sync::load(&sb.store()).unwrap().settings.notify_slack);
        for bad in ["notify=maybe", "", "notify="] {
            assert!(sb.post_to("/insights/memory/sync/slack", bad).ends_with("notice=bad-input"), "{bad}");
        }
        std::fs::write(sb.store(), "{corrupt").unwrap();
        assert!(sb.post_to("/insights/memory/sync/slack", "notify=0").ends_with("notice=store-bad"));
        assert_eq!(std::fs::read_to_string(sb.store()).unwrap(), "{corrupt");
    }

    #[test]
    fn the_overview_shows_the_slack_switch_and_a_hint_without_a_webhook() {
        let sb = Sb::new(&[("slack-v", true)]);
        let html = sb.get("/insights?view=memory");
        assert!(html.contains("action='/insights/memory/sync/slack'"), "{html}");
        assert!(html.contains("aria-label='Slack messages for profile jobs'"), "{html}");
        assert!(html.contains("No Slack webhook configured"), "{html}");
        assert!(html.contains("href='/connectors'"), "{html}");
        let slack = sb.state.user_home.join(".slack");
        std::fs::create_dir_all(&slack).unwrap();
        std::fs::write(slack.join("config.json"), r#"{"webhook_url": "keychain:loop/slack"}"#).unwrap();
        let html = sb.get("/insights?view=memory");
        assert!(!html.contains("No Slack webhook configured"), "{html}");
        sb.post_to("/insights/memory/sync/slack", "notify=0");
        let off = sb.get("/insights?view=memory");
        let form = off.split("action='/insights/memory/sync/slack'").nth(1).unwrap();
        assert!(form.split("</form>").next().unwrap().contains("aria-checked='false'"), "{off}");
    }

    #[test]
    fn the_slack_switch_and_hint_are_translated() {
        let sb = Sb::new(&[("slack-fr", true)]);
        let req = crate::ui::routes::UiRequest::get("/insights?view=memory").with_header("cookie", "loop_lang=fr");
        let fr = String::from_utf8(sb.state.registry.dispatch(&sb.state, &req).unwrap().body).unwrap();
        assert!(!fr.contains("Slack messages for profile jobs"), "{fr}");
        assert!(!fr.contains("No Slack webhook configured"), "{fr}");
        assert!(fr.contains("aria-label='Messages Slack pour les tâches de profil'"), "{fr}");
        assert!(fr.contains("Aucun webhook Slack configuré — configurez-le dans Connecteurs"), "{fr}");
        sb.post_to("/insights/memory/sync/slack", "notify=0");
        let req = crate::ui::routes::UiRequest::get("/insights?view=memory&notice=slack-off").with_header("cookie", "loop_lang=fr");
        let fr = String::from_utf8(sb.state.registry.dispatch(&sb.state, &req).unwrap().body).unwrap();
        assert!(fr.contains("Les messages Slack pour les tâches de profil sont désactivés."), "{fr}");
    }

    // ---------------------------------------------------------------- live updates

    fn jobs_json(sb: &Sb, url: &str, lang: Option<&str>) -> (crate::ui::routes::UiResponse, serde_json::Value) {
        let mut req = crate::ui::routes::UiRequest::get(url);
        if let Some(l) = lang {
            req = req.with_header("cookie", &format!("loop_lang={l}"));
        }
        let r = sb.state.registry.dispatch(&sb.state, &req).unwrap();
        let v = serde_json::from_slice(&r.body).unwrap_or(serde_json::Value::Null);
        (r, v)
    }

    fn activity_base(sb: &Sb) -> PathBuf {
        loopx_core::agent_activity::beside(&sb.state.status_path_for_loop("issue-loop"))
    }

    #[test]
    fn the_jobs_endpoint_lists_running_jobs_with_translated_steps() {
        let sb = Sb::new(&[("live-j", true), ("other-j", true)]);
        let base = activity_base(&sb);
        let job = ProfileJob::start(&base, ProfileJobKind::Build, "live-j");
        loopx_core::profile_progress::Progress::step(&job, 3, 5, loopx_core::profile_progress::Step::Ai);
        let (r, v) = jobs_json(&sb, "/insights/memory/jobs", None);
        assert_eq!(r.status, 200);
        let ct = r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-type")).unwrap().1.clone();
        assert!(ct.starts_with("application/json"), "{ct}");
        assert!(r.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("cache-control") && v.contains("no-store")));
        let running = v["running"].as_array().unwrap();
        assert_eq!(running.len(), 1, "{v}");
        assert_eq!(running[0]["alias"], "live-j");
        assert_eq!(running[0]["kind"], "build");
        assert_eq!((running[0]["step"].as_u64(), running[0]["steps"].as_u64()), (Some(3), Some(5)));
        assert_eq!(running[0]["code"], "ai");
        assert_eq!(running[0]["label"], "Asking the AI \u{b7} step 3 of 5");
        assert!(v["sync"].is_null() && v["failed"].as_array().unwrap().is_empty(), "{v}");
        let (_, fr) = jobs_json(&sb, "/insights/memory/jobs", Some("fr"));
        assert_eq!(fr["running"][0]["label"], "Interrogation de l'IA \u{b7} étape 3 sur 5");
        // Filtered to a known project.
        let (_, only) = jobs_json(&sb, "/insights/memory/jobs?project=other-j", None);
        assert!(only["running"].as_array().unwrap().is_empty(), "{only}");
        let (_, mine) = jobs_json(&sb, "/insights/memory/jobs?project=live-j", None);
        assert_eq!(mine["running"].as_array().unwrap().len(), 1);
        drop(job);
        let (_, gone) = jobs_json(&sb, "/insights/memory/jobs", None);
        assert!(gone["running"].as_array().unwrap().is_empty(), "{gone}");
    }

    #[test]
    fn the_jobs_endpoint_does_not_list_a_guide_build_as_a_profile_job() {
        let sb = Sb::new(&[("live-g", true)]);
        let base = activity_base(&sb);
        let guide = ProfileJob::start(&base, ProfileJobKind::Guide, "live-g");
        loopx_core::profile_progress::Progress::step(&guide, 2, 5, loopx_core::profile_progress::Step::Page);
        let (_, v) = jobs_json(&sb, "/insights/memory/jobs", None);
        assert!(v["running"].as_array().unwrap().is_empty(), "{v}");
        drop(guide);
    }

    #[test]
    fn the_jobs_endpoint_ignores_dead_and_stale_records() {
        let sb = Sb::new(&[("dead-j", true), ("stale-j", true)]);
        let base = activity_base(&sb);
        let dead = ProfileJob::start(&base, ProfileJobKind::Build, "dead-j");
        let stale = ProfileJob::start(&base, ProfileJobKind::Build, "stale-j");
        let rewrite = |job: &ProfileJob, f: &dyn Fn(&mut serde_json::Value)| {
            let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(job.file()).unwrap()).unwrap();
            f(&mut v);
            std::fs::write(job.file(), v.to_string()).unwrap();
        };
        let pid = {
            let mut c = std::process::Command::new("true").spawn().unwrap();
            c.wait().unwrap();
            c.id()
        };
        rewrite(&dead, &|v| v["pid"] = serde_json::json!(pid));
        rewrite(&stale, &|v| {
            v["at"] = serde_json::json!((Utc::now() - chrono::Duration::hours(30)).to_rfc3339_opts(chrono::SecondsFormat::Secs, false))
        });
        let (_, v) = jobs_json(&sb, "/insights/memory/jobs", None);
        assert!(v["running"].as_array().unwrap().is_empty(), "{v}");
        drop((dead, stale));
    }

    #[test]
    fn the_jobs_endpoint_reports_a_sync_and_failures_with_fixed_text() {
        let sb = Sb::new(&[("fail-j", true), ("old-j", true)]);
        let base = activity_base(&sb);
        failed_build(&sb, "fail-j", now() - chrono::Duration::minutes(5));
        failed_build(&sb, "old-j", now() - chrono::Duration::hours(25));
        record(&sb.state.home_dir, "fail-j", Job { at: now(), kind: JobKind::Build, outcome: JobOutcome::Failed(loopx_core::profile_progress::CLI_NOT_STARTED.into()) });
        let sync = ProfileJob::start(&base, ProfileJobKind::Sync, "");
        sync.project(2, 3, "fail-j");
        let (_, v) = jobs_json(&sb, "/insights/memory/jobs", None);
        assert_eq!(v["sync"]["step"], 2, "{v}");
        assert_eq!(v["sync"]["steps"], 3);
        assert_eq!(v["sync"]["label"], "Profile sync \u{2014} fail-j (2/3)");
        let failed = v["failed"].as_array().unwrap();
        assert_eq!(failed.len(), 1, "the 25h old failure is dropped: {v}");
        assert_eq!(failed[0]["alias"], "fail-j");
        assert_eq!(failed[0]["reason"], "the AI command could not be started \u{2014} check Settings \u{2192} AI CLI");
        drop(sync);
        let (_, v) = jobs_json(&sb, "/insights/memory/jobs", None);
        assert!(v["sync"].is_null(), "{v}");
    }

    #[test]
    fn the_jobs_endpoint_never_echoes_an_unknown_project() {
        let sb = Sb::new(&[("known-j", true)]);
        let base = activity_base(&sb);
        let job = ProfileJob::start(&base, ProfileJobKind::Build, "known-j");
        let (r, v) = jobs_json(&sb, "/insights/memory/jobs?project=%3Cscript%3Ealert(1)%3C/script%3E", None);
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body.clone()).unwrap();
        assert!(!body.contains("script") && !body.contains("alert"), "{body}");
        assert!(v["running"].as_array().unwrap().is_empty(), "{v}");
        let (_, v) = jobs_json(&sb, "/insights/memory/jobs?project=../../etc", None);
        assert!(v["running"].as_array().unwrap().is_empty(), "{v}");
        drop(job);
    }

    #[test]
    fn the_jobs_endpoint_counts_a_build_whose_record_is_not_written_yet() {
        let (agent, release) = FakeAgent::gated(full_reply("guard-j", "Built"));
        let sb = Sb::new(&[("guard-j", true)]).with_agent(agent);
        sb.post_to("/insights/memory/build", "project=guard-j&from=project");
        let (_, v) = jobs_json(&sb, "/insights/memory/jobs?project=guard-j", None);
        assert_eq!(v["running"][0]["alias"], "guard-j", "{v}");
        release.send(()).unwrap();
        wait_idle("guard-j");
        let (_, v) = jobs_json(&sb, "/insights/memory/jobs?project=guard-j", None);
        assert!(v["running"].as_array().unwrap().is_empty(), "{v}");
    }

    #[test]
    fn memory_pages_carry_the_live_script_and_the_running_markers() {
        let sb = Sb::new(&[("mark-j", true)]);
        let base = activity_base(&sb);
        let idle = sb.get("/insights?view=memory&project=mark-j");
        assert!(idle.contains("data-mem-live") && idle.contains("data-mem-project='mark-j'"), "{idle}");
        assert!(!idle.contains("data-mem-job="), "{idle}");
        let job = ProfileJob::start(&base, ProfileJobKind::Build, "mark-j");
        loopx_core::profile_progress::Progress::step(&job, 2, 5, loopx_core::profile_progress::Step::Issues);
        let project = sb.get("/insights?view=memory&project=mark-j");
        assert!(project.contains("data-mem-job='mark-j'"), "{project}");
        assert!(project.contains("data-mem-step"), "{project}");
        assert!(project.contains("role='progressbar' aria-valuemin='0' aria-valuemax='100' aria-valuenow='40'"), "{project}");
        assert!(project.contains("Reading issues \u{b7} step 2 of 5"), "{project}");
        assert!(project.contains("This page updates by itself while it runs."), "{project}");
        assert!(!project.contains("reload the page to see the result"), "{project}");
        let sub = sb.get("/insights?view=memory&project=mark-j&page=stack");
        assert!(sub.contains("data-mem-job='mark-j'") && sub.contains("data-mem-live"), "{sub}");
        let overview = sb.get("/insights?view=memory");
        assert!(overview.contains("data-mem-job='mark-j'") && overview.contains("data-mem-live"), "{overview}");
        assert!(overview.contains("data-mem-progress"), "{overview}");
        // The overview has no per-project filter.
        assert!(!overview.contains("data-mem-project='"), "{overview}");
        let sync = ProfileJob::start(&base, ProfileJobKind::Sync, "");
        sync.project(1, 2, "mark-j");
        let overview = sb.get("/insights?view=memory");
        assert!(overview.contains("data-mem-job='sync'") && overview.contains("Profile sync \u{2014} mark-j (1/2)"), "{overview}");
        drop((job, sync));
        // Other pages never get it.
        let other = sb.get("/insights");
        assert!(!other.contains("data-mem-live"), "{other}");
    }

    #[test]
    fn a_started_notice_asks_the_script_to_watch() {
        let sb = Sb::new(&[("watch-j", true)]);
        let page = sb.get("/insights?view=memory&project=watch-j&notice=build-started");
        assert!(page.contains("data-mem-watch"), "{page}");
        assert!(!sb.get("/insights?view=memory&project=watch-j&notice=saved").contains("data-mem-watch data-mem-finished"));
        assert!(!sb.get("/insights?view=memory&project=watch-j").contains("data-mem-watch data-mem-finished"));
        let page = sb.get("/insights?view=memory&notice=build-all-started");
        assert!(page.contains("data-mem-watch"), "{page}");
    }

    #[test]
    fn the_live_script_is_safe_and_only_reloads_without_losing_typing() {
        let js = crate::ui::assets::text("memory/live.js").unwrap();
        for banned in ["innerHTML", "outerHTML", "insertAdjacentHTML", "eval(", "new Function", "document.write"] {
            assert!(!js.contains(banned), "{banned}");
        }
        assert!(js.contains("document.hidden") && js.contains("visibilitychange"));
        assert!(js.contains("textContent"));
        // The reload goes through the dirty-input check; the check is what keeps typing.
        let finish = js.split("function finish").nth(1).unwrap();
        assert!(finish.find("isDirty()").unwrap() < finish.find("location.reload()").unwrap());
        assert!(js.contains("dialog[open]") && js.contains("defaultValue"));
        assert!(js.contains("2 * 60 * 60 * 1000") && js.contains("15000"));
    }

    #[test]
    fn a_managed_project_is_built_from_a_clone_made_on_the_click() {
        let alias = "mgd-build";
        let sb = Sb::new(&[("plain", true)]).with_agent(FakeAgent::new(full_reply(alias, "Built")));
        let dir = sb.add_managed(alias);
        assert!(!dir.exists());
        let loc = sb.post_to("/insights/memory/build", &format!("project={alias}&from=project"));
        assert!(loc.ends_with("notice=build-started"), "{loc}");
        wait_idle(alias);
        assert!(dir.join("README.md").is_file(), "cloned into the managed dir");
        assert_eq!(sb.load(alias).sections.len(), 10);
    }

    #[test]
    fn a_managed_project_without_a_reachable_remote_fails_the_job_not_the_click() {
        let alias = "mgd-bad";
        let sb = Sb::new(&[]).with_agent(FakeAgent::new(full_reply(alias, "Built")));
        let dir = sb.add_managed(alias);
        let path = sb.state.home_dir.join("projects.json");
        let mut cfg: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        cfg["projects"][alias]["project_id"] = json!("grp/no-such-remote");
        std::fs::write(&path, cfg.to_string()).unwrap();
        let loc = sb.post_to("/insights/memory/build", &format!("project={alias}&from=project"));
        assert!(loc.ends_with("notice=build-started"), "{loc}");
        wait_idle(alias);
        assert!(!dir.exists());
        assert!(project_profile::load(alias, Some(&sb.root())).unwrap().is_none());
    }

    #[test]
    fn build_all_includes_a_managed_project() {
        let alias = "mgd-all";
        let sb = Sb::new(&[("mgd-all-user", true)]).with_agent(FakeAgent::new(full_reply("x", "Built")));
        sb.add_managed(alias);
        assert_eq!(sb.post_to("/insights/memory/build", "project=all"), "/insights?view=memory&notice=build-all-started");
        for _ in 0..400 {
            if project_profile::load(alias, Some(&sb.root())).ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        wait_idle(alias);
        assert!(project_profile::load(alias, Some(&sb.root())).unwrap().is_some());
    }

    #[test]
    fn the_project_page_labels_a_managed_clone_and_offers_reclone_only_there() {
        let sb = Sb::new(&[("mgd-plain", true)]);
        sb.add_managed("mgd-label");
        let managed = sb.get("/insights?view=memory&project=mgd-label");
        assert!(managed.contains("Cloned for Memory"), "{managed}");
        assert!(managed.contains("action='/insights/memory/checkout/reclone'"), "{managed}");
        let plain = sb.get("/insights?view=memory&project=mgd-plain");
        assert!(!plain.contains("Cloned for Memory") && !plain.contains("checkout/reclone"), "{plain}");
    }

    #[test]
    fn reclone_deletes_only_the_managed_dir() {
        let sb = Sb::new(&[("mgd-user", true)]);
        let dir = sb.add_managed("mgd-rc");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stale"), "x").unwrap();
        let loc = sb.post_to("/insights/memory/checkout/reclone", "project=mgd-rc");
        assert!(loc.ends_with("notice=recloned"), "{loc}");
        assert!(!dir.exists());
        // A user-owned checkout is never removed; unknown aliases are refused.
        let user = sb.dir.path().join("repos").join("mgd-user");
        assert!(sb.post_to("/insights/memory/checkout/reclone", "project=mgd-user").ends_with("notice=bad-input"));
        assert!(user.join("README.md").is_file());
        assert!(sb.post_to("/insights/memory/checkout/reclone", "project=nope").ends_with("notice=bad-input"));
    }

    #[test]
    fn the_clone_switch_saves_on_click_and_hides_managed_projects_when_off() {
        let sb = Sb::new(&[("sw-user", true)]);
        sb.add_managed("sw-mgd");
        let html = sb.get("/insights?view=memory");
        assert!(html.contains("action='/insights/memory/sync/clone'"), "{html}");
        assert!(html.contains("aria-label='Clone projects without a local checkout'"), "{html}");
        let loc = sb.post_to("/insights/memory/sync/clone", "clone=0");
        assert_eq!(loc, "/insights?view=memory&notice=clone-off");
        let s = profile_sync::load(&sb.store()).unwrap().settings;
        assert!(!s.clone_for_memory && s.enabled && s.notify_slack, "only the clone switch changed");
        assert!(!profile_sync::tracked_projects_in(&sb.state.home_dir.join("projects.json")).iter().any(|t| t.managed));
        assert_eq!(sb.post_to("/insights/memory/sync/clone", "clone=on"), "/insights?view=memory&notice=clone-on");
        assert!(profile_sync::load(&sb.store()).unwrap().settings.clone_for_memory);
        for bad in ["clone=maybe", "", "clone="] {
            assert!(sb.post_to("/insights/memory/sync/clone", bad).ends_with("notice=bad-input"), "{bad}");
        }
        std::fs::write(sb.store(), "{corrupt").unwrap();
        assert!(sb.post_to("/insights/memory/sync/clone", "clone=0").ends_with("notice=store-bad"));
        assert_eq!(std::fs::read_to_string(sb.store()).unwrap(), "{corrupt");
    }

    /// Point project `alias` at instance `inst` and answer every wiki fetch for it with `body`.
    fn wiki_sandbox(sb: &Sb, alias: &str, inst: &str, body: &'static str) {
        let path = sb.state.home_dir.join("projects.json");
        let mut cfg: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        cfg["projects"][alias]["instance"] = json!(inst);
        std::fs::write(&path, cfg.to_string()).unwrap();
        loopx_core::profile_wiki::register_test_fetch(
            inst,
            Box::new(move |r| {
                if body == "down" {
                    Err("down".into())
                } else {
                    Ok((format!("Page {}", r.slug), body.to_string()))
                }
            }),
        );
    }

    fn wiki_link(alias: &str) -> String {
        format!("https://gl.example.com/group/{alias}/-/wikis/Runbook")
    }

    fn enc(url: &str) -> String {
        url.replace(':', "%3A").replace('/', "%2F")
    }

    #[test]
    fn a_pasted_wiki_link_is_saved_and_queues_the_sections_it_can_change() {
        let alias = "wiki-ok";
        let sb = Sb::new(&[(alias, true)]);
        sb.save(&profile(alias, &[(SectionKey::Overview, section("S.", "B"))]));
        wiki_sandbox(&sb, alias, "wiki-inst-ok", "Restart with make up.");
        let url = enc(&wiki_link(alias));
        let loc = sb.post_to("/insights/memory/wiki/add", &format!("project={alias}&url={url}"));
        assert!(loc.ends_with("notice=wiki-added"), "{loc}");
        let pages = loopx_core::profile_wiki::list(alias, Some(&sb.root()));
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].content, "Restart with make up.");
        let queued = profile_sync::load(&sb.store()).unwrap().queued;
        assert!(queued[alias].iter().any(|q| q.section == "overview" && q.forced), "{queued:?}");
        let html = sb.get(&format!("/insights?view=memory&project={alias}"));
        assert!(html.contains("Page Runbook"), "{html}");
        assert!(html.contains("action='/insights/memory/wiki/remove'"), "{html}");
        // The same text again changes nothing.
        let loc = sb.post_to("/insights/memory/wiki/add", &format!("project={alias}&url={url}"));
        assert!(loc.ends_with("notice=wiki-unchanged"), "{loc}");
    }

    #[test]
    fn a_wiki_link_that_cannot_be_used_says_why_and_saves_nothing() {
        let alias = "wiki-bad";
        let sb = Sb::new(&[(alias, true)]);
        wiki_sandbox(&sb, alias, "wiki-inst-bad", "text");
        let post = |url: &str| sb.post_to("/insights/memory/wiki/add", &format!("project={alias}&url={}", enc(url)));
        assert!(post("https://gl.example.com/group/wiki-bad").ends_with("notice=wiki-bad-link"));
        assert!(post("https://gl.example.com/other/proj/-/wikis/x").ends_with("notice=wiki-other-project"));
        let token = ["glpat-", "abcDEF123456ghiJKL78"].concat();
        wiki_sandbox(&sb, alias, "wiki-inst-bad", Box::leak(format!("use {token}").into_boxed_str()));
        assert!(post(&wiki_link(alias)).ends_with("notice=wiki-secret"));
        wiki_sandbox(&sb, alias, "wiki-inst-bad", "down");
        assert!(post(&wiki_link(alias)).ends_with("notice=wiki-failed"));
        assert!(loopx_core::profile_wiki::list(alias, Some(&sb.root())).is_empty());
        assert!(sb.post_to("/insights/memory/wiki/add", "project=nope&url=x").ends_with("notice=bad-input"));
    }

    #[test]
    fn a_saved_wiki_page_can_be_removed_and_odd_ids_are_refused() {
        let alias = "wiki-rm";
        let sb = Sb::new(&[(alias, true)]);
        wiki_sandbox(&sb, alias, "wiki-inst-rm", "text");
        sb.post_to("/insights/memory/wiki/add", &format!("project={alias}&url={}", enc(&wiki_link(alias))));
        let id = loopx_core::profile_wiki::list(alias, Some(&sb.root()))[0].id.clone();
        assert!(sb.post_to("/insights/memory/wiki/remove", &format!("project={alias}&page=..%2Fx")).ends_with("notice=bad-input"));
        let loc = sb.post_to("/insights/memory/wiki/remove", &format!("project={alias}&page={id}"));
        assert!(loc.ends_with("notice=wiki-removed"), "{loc}");
        assert!(loopx_core::profile_wiki::list(alias, Some(&sb.root())).is_empty());
        assert!(sb.post_to("/insights/memory/wiki/remove", &format!("project=nope&page={id}")).ends_with("notice=bad-input"));
    }

    #[test]
    fn the_project_page_offers_the_wiki_form_with_or_without_a_profile() {
        let sb = Sb::new(&[("wiki-form", true)]);
        let html = sb.get("/insights?view=memory&project=wiki-form");
        assert!(html.contains("action='/insights/memory/wiki/add'"), "{html}");
        assert!(html.contains("name='url'"), "{html}");
        sb.save(&profile("wiki-form", &[(SectionKey::Overview, section("S.", "B"))]));
        assert!(sb.get("/insights?view=memory&project=wiki-form").contains("action='/insights/memory/wiki/add'"));
    }

    // ------------------------------------------------------------------ the guide reader

    fn gd_page(alias: &str, slug: &str, title: &str, status: Status, parts: &[(&str, &str, Origin)]) -> loopx_core::project_guide::Page {
        loopx_core::project_guide::Page {
            alias: alias.into(),
            slug: slug.into(),
            kind: loopx_core::project_guide::check_slug(slug).unwrap(),
            title: title.into(),
            status,
            generated_at: Some(Utc.with_ymd_and_hms(2026, 11, 2, 9, 0, 0).unwrap()),
            head_sha: Some("0123456789abcdef0123".into()),
            summary: format!("Summary of {slug}."),
            related: vec![],
            parts: parts
                .iter()
                .map(|(k, b, o)| loopx_core::project_guide::Part { key: (*k).into(), body: (*b).into(), origin: *o })
                .collect(),
            sources: vec![Source { path: format!("app/{slug}.rb"), sha256: "cd".repeat(32) }],
        }
    }

    fn put_page(sb: &Sb, alias: &str, slug: &str, title: &str, status: Status, parts: &[(&str, &str, Origin)]) {
        loopx_core::project_guide::save(&gd_page(alias, slug, title, status, parts), Some(&sb.root())).unwrap();
    }

    fn put_plan(sb: &Sb, alias: &str, features: &[&str]) {
        let plan = loopx_core::guide_plan::Plan {
            features: features
                .iter()
                .map(|s| loopx_core::guide_plan::FeaturePlan {
                    slug: (*s).into(),
                    title: format!("Planned {s}"),
                    summary: format!("Plan summary of {s}."),
                    files: vec![],
                    related: vec![],
                })
                .collect(),
            ..Default::default()
        };
        loopx_core::guide_plan::save(alias, &plan, Some(&sb.root())).unwrap();
    }

    fn guide_url(alias: &str, slug: Option<&str>) -> String {
        let mut url = format!("/insights?view=memory&project={alias}&page=guide");
        if let Some(s) = slug {
            url.push_str("&guide=");
            url.push_str(s);
        }
        url
    }

    /// The `<a>` row of `slug` in the guide list.
    fn row_of<'a>(html: &'a str, slug: &str) -> &'a str {
        let href = html.find(&format!("guide={slug}'")).unwrap_or_else(|| panic!("no row for {slug}: {html}"));
        let at = html[..href].rfind("<a ").unwrap();
        let end = href + html[href..].find("</a>").unwrap();
        &html[at..end]
    }

    fn get_fr(sb: &Sb, url: &str) -> String {
        let req = crate::ui::routes::UiRequest::get(url).with_header("cookie", "loop_lang=fr");
        let r = sb.state.registry.dispatch(&sb.state, &req).unwrap();
        assert_eq!(r.status, 200, "{url}");
        String::from_utf8(r.body).unwrap()
    }

    #[test]
    fn guide_appears_in_the_sub_nav_and_the_empty_state_explains_a_build() {
        let sb = Sb::new(&[("gd-empty", true), ("gd-nock", false)]);
        let html = sb.get(&guide_url("gd-empty", None));
        // The sub-nav: …Activity, Guide, | Lessons, Proposals; Guide is the current page.
        let nav_at = html.find("class='card mem-subnav'").unwrap();
        let nav = &html[nav_at..nav_at + html[nav_at..].find("</nav>").unwrap()];
        let pos = |needle: &str| nav.find(needle).unwrap_or_else(|| panic!("{needle} missing: {nav}"));
        assert!(pos("page=activity'") < pos("page=guide'"));
        assert!(pos("page=guide'") < pos("mem-subnav-sep"));
        assert!(pos("mem-subnav-sep") < pos("page=lessons'"));
        assert!(nav.contains("<span>Guide</span>"), "{nav}");
        assert!(nav.contains("href='#i-menu_book'"), "{nav}");
        assert!(nav[pos("page=guide'")..].starts_with("page=guide' class='is-active' aria-current='page'"), "{nav}");
        // The empty state says what a build does and costs, with a Build guide form.
        assert!(html.contains("A build reads the project&#x27;s code and writes about 15 pages"), "{html}");
        assert!(html.contains("It makes one AI call per page (about 15) and takes a few minutes."));
        assert!(html.contains("Nothing is used by agents until you approve a page."));
        assert!(html.contains("action='/insights/memory/guide/build'"), "{html}");
        let form_at = html.find("action='/insights/memory/guide/build'").unwrap();
        let form = &html[form_at..form_at + html[form_at..].find("</form>").unwrap()];
        assert!(form.contains("name='csrf_token'"), "{form}");
        assert!(form.contains("<input type='hidden' name='project' value='gd-empty'>"), "{form}");
        assert!(form.contains("Build guide"), "{form}");
        // No profile is needed, and no list is drawn when there is nothing to list.
        assert!(!html.contains("class='gd-layout'"));
        // Without a checkout: no form, the reason instead.
        let html = sb.get(&guide_url("gd-nock", None));
        assert!(html.contains("about 15 pages"));
        assert!(!html.contains("action='/insights/memory/guide/build'"), "{html}");
        assert!(html.contains("Set this project&#x27;s local checkout on the Projects page to build its guide."), "{html}");
    }

    #[test]
    fn the_list_groups_project_and_feature_pages_with_status_pills_and_plan_markers() {
        let sb = Sb::new(&[("gd-list", true)]);
        put_plan(&sb, "gd-list", &["feature-orders", "feature-pay"]);
        put_page(&sb, "gd-list", "architecture", "Architecture", Status::Approved, &[("overview", "Layers.", Origin::Agent)]);
        put_page(&sb, "gd-list", "feature-orders", "Orders", Status::Draft, &[("what", "Takes orders.", Origin::Agent)]);
        put_page(&sb, "gd-list", "feature-old", "Old export", Status::Draft, &[("what", "Exports.", Origin::Agent)]);
        let html = sb.get(&guide_url("gd-list", None));
        assert!(html.contains("class='gd-layout'"), "{html}");
        let list_at = html.find("class='card gd-list'").unwrap();
        let list = &html[list_at..list_at + html[list_at..].find("</nav>").unwrap()];
        let pos = |needle: &str| list.find(needle).unwrap_or_else(|| panic!("{needle} missing: {list}"));
        // Groups, in order: Project, Features, Not in the current plan.
        assert!(pos("<h3 class='gd-group'>Project</h3>") < pos("guide=architecture'"));
        assert!(pos("guide=architecture'") < pos("<h3 class='gd-group'>Features</h3>"));
        assert!(pos("<h3 class='gd-group'>Features</h3>") < pos("guide=feature-orders'"));
        assert!(pos("guide=feature-orders'") < pos("<h3 class='gd-group'>Not in the current plan</h3>"));
        assert!(pos("<h3 class='gd-group'>Not in the current plan</h3>") < pos("guide=feature-old'"));
        // Each row: title, summary, a status pill; the off-plan marker only on feature-old.
        let arch = row_of(list, "architecture");
        assert!(arch.contains("Architecture") && arch.contains("Summary of architecture."), "{arch}");
        assert!(arch.contains("<span class='mem-pill mem-pill-fresh'>Approved</span>"), "{arch}");
        let orders = row_of(list, "feature-orders");
        assert!(orders.contains("Orders") && orders.contains("<span class='mem-pill mem-pill-review'>Draft</span>"), "{orders}");
        assert!(!orders.contains("gd-off-plan"), "{orders}");
        let old = row_of(list, "feature-old");
        assert!(old.contains("Old export") && old.contains("<span class='mem-pill mem-pill-review'>Draft</span>"), "{old}");
        assert!(old.contains("<span class='gd-off-plan'>Not in the current plan</span>"), "{old}");
        assert_eq!(list.matches("gd-off-plan").count(), 1, "{list}");
        // A planned feature with no page yet is listed as not written, and is not a link.
        assert!(list.contains("Planned feature-pay") && list.contains("Not written yet"), "{list}");
        assert!(!list.contains("guide=feature-pay'"));
        // The first page is selected by default and shown on the right.
        assert!(arch.contains("is-active"), "{arch}");
        assert!(!orders.contains("is-active"));
        let page_at = html.find("class='card gd-page'").unwrap();
        assert!(html[page_at..].contains("<h2>Architecture</h2>"));
        assert!(html[page_at..].contains("Layers."));
        assert!(!html[page_at..].contains("Takes orders."));
    }

    #[test]
    fn a_page_renders_parts_sources_verified_commit_and_human_tag() {
        let sb = Sb::new(&[("gd-view", true)]);
        // Parts given out of order: they render in the kind's order.
        put_page(
            &sb,
            "gd-view",
            "feature-orders",
            "Orders",
            Status::Approved,
            &[
                ("key-points", "Totals are in **cents**.", Origin::Agent),
                ("how", "A job settles payments.", Origin::Human),
                ("what", "Buyers place orders.", Origin::Agent),
            ],
        );
        let html = sb.get(&guide_url("gd-view", Some("feature-orders")));
        let page_at = html.find("class='card gd-page'").unwrap();
        let page = &html[page_at..];
        let pos = |needle: &str| page.find(needle).unwrap_or_else(|| panic!("{needle} missing: {page}"));
        assert!(pos("<h2>Orders</h2>") < pos("<h3>What it does</h3>"));
        assert!(pos("<h3>What it does</h3>") < pos("<h3>How it works</h3>"));
        assert!(pos("<h3>How it works</h3>") < pos("<h3>Key points</h3>"));
        assert!(page.contains("<span class='mem-pill mem-pill-fresh'>Approved</span>"));
        assert!(page.contains("<strong>cents</strong>"), "{page}");
        assert!(page.contains("Summary of feature-orders."));
        // Each part is its own section; only the human one carries the person tag.
        let parts: Vec<&str> = page.split("<section class='gd-part'>").skip(1).collect();
        assert_eq!(parts.len(), 3, "{page}");
        assert!(!parts[0].split("</section>").next().unwrap().contains("Written by a person"));
        assert!(parts[1].split("</section>").next().unwrap().contains("<span class='mem-pill mem-pill-human'>Written by a person</span>"));
        assert!(!parts[2].split("</section>").next().unwrap().contains("Written by a person"));
        assert!(parts[0].contains("<div class='markdown mem-body'>"));
        // Meta: who wrote it, verified date, short commit.
        assert!(page.contains("Written by the agent and a person"), "{page}");
        assert!(page.contains("Last verified 2026-11-02"), "{page}");
        assert!(page.contains("<code>0123456789ab</code>"), "{page}");
        assert!(!page.contains("0123456789abcdef"));
        // Sources.
        assert!(page.contains("<details class='mem-sources'"), "{page}");
        assert!(page.contains("Sources (1)"));
        assert!(page.contains("<code>app/feature-orders.rb</code>"));
        assert!(page.contains("cdcdcdcdcdcd"));
        // An all-agent page says so.
        put_page(&sb, "gd-view", "architecture", "Architecture", Status::Approved, &[("overview", "Layers.", Origin::Agent)]);
        let html = sb.get(&guide_url("gd-view", Some("architecture")));
        assert!(html.contains("Written by the agent ·") || html.contains("Written by the agent<"), "{html}");
        assert!(!html.contains("Written by the agent and a person"));
    }

    #[test]
    fn associations_link_only_to_existing_pages() {
        let sb = Sb::new(&[("gd-assoc", true)]);
        put_page(&sb, "gd-assoc", "feature-cart", "Cart", Status::Approved, &[("what", "Holds items.", Origin::Agent)]);
        put_page(
            &sb,
            "gd-assoc",
            "feature-orders",
            "Orders",
            Status::Approved,
            &[(
                "associations",
                "Uses [Cart](feature-cart), not [Ghost](feature-ghost) or [x](javascript:alert(1)).",
                Origin::Agent,
            )],
        );
        let html = sb.get(&guide_url("gd-assoc", Some("feature-orders")));
        let page_at = html.find("class='card gd-page'").unwrap();
        let page = &html[page_at..];
        assert!(page.contains("<h3>Associations</h3>"), "{page}");
        assert!(
            page.contains("<a href=\"/insights?view=memory&amp;project=gd-assoc&amp;page=guide&amp;guide=feature-cart\">Cart</a>"),
            "{page}"
        );
        // A slug with no page is not turned into a guide link (only its label is kept);
        // a javascript: link stays text.
        assert!(!page.contains("guide=feature-ghost"), "{page}");
        assert!(!page.contains("href=\"feature-ghost\""), "{page}");
        assert!(page.contains("not Ghost or"), "{page}");
        assert!(!page.contains("href=\"javascript"), "{page}");
        assert!(page.contains("[x](javascript:alert(1))"), "{page}");
    }

    #[test]
    fn a_page_with_a_secret_is_redacted_everywhere() {
        let alias = "gd-secret";
        let sb = Sb::new(&[(alias, true)]);
        put_page(&sb, alias, "architecture", "Architecture", Status::Approved, &[("overview", "Layers.", Origin::Agent)]);
        put_page(&sb, alias, "feature-orders", "Orders", Status::Draft, &[("what", "Body text here.", Origin::Agent)]);
        let file = loopx_core::project_guide::guide_dir(alias, Some(&sb.root())).join("feature-orders.md");
        let token = ["glpat-", "abcDEF123456ghiJKL78"].concat();
        let text = std::fs::read_to_string(&file).unwrap().replace("Body text here.", &format!("Body text here {token}."));
        std::fs::write(&file, text).unwrap();
        for url in [guide_url(alias, Some("feature-orders")), guide_url(alias, None)] {
            let html = sb.get(&url);
            assert!(!html.contains("glpat-"), "{html}");
            assert!(!html.contains("Body text here"), "{html}");
            assert!(!html.contains("Summary of feature-orders."), "{html}");
            assert!(!html.contains("app/feature-orders.rb"), "{html}");
            // The list row still shows the title.
            assert!(row_of(&html, "feature-orders").contains("Orders"));
            let layout_at = html.find("class='gd-layout'").unwrap();
            let layout_end = html.find("data-mem-live").unwrap();
            assert!(!html[layout_at..layout_end].contains("title="), "{html}");
        }
        let html = sb.get(&guide_url(alias, Some("feature-orders")));
        let page_at = html.find("class='card gd-page'").unwrap();
        let page = &html[page_at..];
        assert!(page.contains("<h2>Orders</h2>"), "{page}");
        assert!(page.contains("mem-pill-review'>Draft</span>"), "{page}");
        assert!(page.contains("class='mem-redacted'"), "{page}");
        assert!(page.contains("GitLab token"), "{page}");
        assert!(!page.contains("gd-part"), "{page}");
        // The notice names the page file, not the profile.
        assert!(!page.contains("PROFILE.md"), "{page}");
        assert!(
            page.contains("Hidden: this page looks like it contains a secret value (GitLab token). Remove the value from the page file, or delete the page and rebuild it."),
            "{page}"
        );
    }

    /// Store `feature-orders` with `from` replaced by `to` in its file (a hand edit).
    fn hand_edit(sb: &Sb, alias: &str, from: &str, to: &str) {
        let file = loopx_core::project_guide::guide_dir(alias, Some(&sb.root())).join("feature-orders.md");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains(from), "{text}");
        std::fs::write(&file, text.replace(from, to)).unwrap();
    }

    #[test]
    fn a_secret_only_in_the_title_summary_or_a_source_path_is_hidden() {
        let token = ["glpat-", "abcDEF123456ghiJKL78"].concat();
        for (i, (from, to)) in [
            ("title: Orders", format!("title: Orders {token}")),
            ("summary: Summary of feature-orders.", format!("summary: Summary of feature-orders. {token}")),
            ("`app/feature-orders.rb`", format!("`app/{token}.rb`")),
        ]
        .into_iter()
        .enumerate()
        {
            let alias = format!("gd-sec{i}");
            let sb = Sb::new(&[(alias.as_str(), true)]);
            put_page(&sb, &alias, "feature-orders", "Orders", Status::Draft, &[("what", "Body text here.", Origin::Agent)]);
            hand_edit(&sb, &alias, from, &to);
            for url in [guide_url(&alias, Some("feature-orders")), guide_url(&alias, None)] {
                let html = sb.get(&url);
                assert!(!html.contains("glpat-"), "{from}: {html}");
                assert!(!html.contains("Body text here"), "{from}: {html}");
                assert!(!html.contains("Summary of feature-orders."), "{from}: {html}");
                assert!(html.contains("class='mem-redacted'"), "{from}: {html}");
                let row = row_of(&html, "feature-orders");
                assert!(!row.contains("gd-row-sum"), "{from}: {row}");
                if i == 0 {
                    // The title itself is the secret: the checked slug stands in for it.
                    assert!(row.contains("<span class='gd-row-title'>feature-orders</span>"), "{row}");
                    assert!(html.contains("<h2>feature-orders</h2>"), "{html}");
                } else {
                    assert!(row.contains("<span class='gd-row-title'>Orders</span>"), "{row}");
                }
            }
        }
    }

    #[test]
    fn page_links_are_rewritten_in_every_part_but_not_inside_code() {
        let alias = "gd-links";
        let sb = Sb::new(&[(alias, true)]);
        put_page(&sb, alias, "feature-cart", "Cart", Status::Approved, &[("what", "Holds items.", Origin::Agent)]);
        put_page(
            &sb,
            alias,
            "feature-orders",
            "Orders",
            Status::Approved,
            &[
                ("what", "Fills a [Cart](feature-cart) or a [Ghost](feature-ghost).", Origin::Agent),
                (
                    "how",
                    "Write `[x](feature-cart)` or `[z](feature-ghost)` in text.\n\n```\n[y](feature-cart) and [w](feature-ghost)\n```\n\nAfter the fence: [Cart](feature-cart).",
                    Origin::Agent,
                ),
            ],
        );
        let html = sb.get(&guide_url(alias, Some("feature-orders")));
        let page = &html[html.find("class='card gd-page'").unwrap()..];
        let link = "<a href=\"/insights?view=memory&amp;project=gd-links&amp;page=guide&amp;guide=feature-cart\">Cart</a>";
        // A part that is not Associations is rewritten too: twice (What it does, after the fence).
        assert_eq!(page.matches(link).count(), 2, "{page}");
        assert!(page.contains("Fills a ") && page.contains(" or a Ghost."), "{page}");
        assert!(!page.contains("href=\"feature-ghost\""), "{page}");
        // Code spans and fenced blocks stay literal.
        assert!(page.contains("<code>[x](feature-cart)</code>"), "{page}");
        assert!(page.contains("<code>[z](feature-ghost)</code>"), "{page}");
        assert!(page.contains("[y](feature-cart) and [w](feature-ghost)"), "{page}");
    }

    #[test]
    fn hostile_guide_values_are_not_echoed_and_do_not_crash() {
        let alias = "gd-host";
        let sb = Sb::new(&[(alias, true)]);
        put_page(&sb, alias, "architecture", "Architecture", Status::Approved, &[("overview", "Layers.", Origin::Agent)]);
        let long = "q".repeat(10 * 1024);
        for (raw, shown) in [
            ("%3Cscript%3Ex", "<script>x"),
            ("..%2Fx", "../x"),
            (long.as_str(), long.as_str()),
            ("Feature-A", "Feature-A"),
            ("feature-nope", "feature-nope"),
        ] {
            let html = sb.get(&guide_url(alias, Some(raw)));
            assert!(html.contains("This page does not exist."), "{raw}");
            assert!(!html.contains(shown), "{raw} echoed");
            assert!(!html.contains(raw), "{raw} echoed");
            assert!(!html.contains("&lt;script&gt;x"), "{raw}");
            // The list is still there.
            assert!(html.contains("guide=architecture'"), "{raw}");
        }
        let html = sb.get("/insights?view=memory&project=nope&page=guide&guide=%3Cscript%3Ex");
        assert!(html.contains("No project by that name"), "{html}");
        assert!(!html.contains("<script>x") && !html.contains("&lt;script&gt;x"));
    }

    #[test]
    fn a_draft_page_says_agents_do_not_read_it() {
        let alias = "gd-draft";
        let sb = Sb::new(&[(alias, true)]);
        put_page(&sb, alias, "architecture", "Architecture", Status::Approved, &[("overview", "Layers.", Origin::Agent)]);
        put_page(&sb, alias, "roadmap", "Roadmap", Status::Draft, &[("planned", "Refunds.", Origin::Agent)]);
        let banner = "Draft: agents do not read this page until it is approved.";
        assert!(sb.get(&guide_url(alias, Some("roadmap"))).contains(banner));
        assert!(!sb.get(&guide_url(alias, Some("architecture"))).contains(banner));
    }

    #[test]
    fn an_unreadable_page_file_shows_a_neutral_row_and_the_rest_still_renders() {
        let alias = "gd-bad";
        let sb = Sb::new(&[(alias, true)]);
        put_page(&sb, alias, "architecture", "Architecture", Status::Approved, &[("overview", "Layers.", Origin::Agent)]);
        let dir = loopx_core::project_guide::guide_dir(alias, Some(&sb.root()));
        std::fs::write(dir.join("feature-x.md"), "not a page").unwrap();
        let html = sb.get(&guide_url(alias, None));
        let row = row_of(&html, "feature-x");
        assert!(row.contains("Could not be read"), "{row}");
        assert!(!html.contains("not a page"), "{html}");
        assert!(!html.contains(&*sb.root().to_string_lossy()), "{html}");
        // The readable page is still listed and shown.
        assert!(html.contains("guide=architecture'") && html.contains("Layers."));
        let html = sb.get(&guide_url(alias, Some("feature-x")));
        assert!(html.contains("This page could not be read."), "{html}");
        assert!(!html.contains("not a page") && !html.contains("missing frontmatter"), "{html}");
    }

    #[test]
    fn the_guide_page_is_translated() {
        let sb = Sb::new(&[("gd-fr", true), ("gd-fr2", true)]);
        let html = get_fr(&sb, &guide_url("gd-fr", None));
        assert!(html.contains("<span>Guide</span>"), "{html}");
        assert!(html.contains("Elle fait un appel IA par page (environ 15)"), "{html}");
        assert!(html.contains("Construire le guide"), "{html}");
        assert!(!html.contains("It makes one AI call per page"));
        put_page(&sb, "gd-fr2", "feature-orders", "Orders", Status::Draft, &[("key-points", "Cents.", Origin::Human)]);
        let html = get_fr(&sb, &guide_url("gd-fr2", Some("feature-orders")));
        assert!(html.contains("mem-pill-review'>Brouillon</span>"), "{html}");
        assert!(html.contains("<h3>Points clés</h3>"), "{html}");
        assert!(html.contains("Fonctionnalités"), "{html}");
        assert!(html.contains("Brouillon : les agents ne lisent pas cette page tant qu"), "{html}");
        assert!(!html.contains("Draft: agents do not read"));
    }

    // ------------------------------------------------------------------ guide builds

    /// The plan a guide build gets: the architecture page and one feature, both read
    /// from the checkout's `README.md` (no data model, so two pages in all).
    const GUIDE_PLAN: &str = r#"{"features":[{"slug":"feature-a","title":"Alpha feature","summary":"Does alpha.","files":["README.md"],"related":[]}],"architecture":{"files":["README.md"]},"data_model":{"files":[]}}"#;

    /// A guide-capable agent, shaped like the real prompts: the plan prompt (it holds
    /// `"features"` and `Reply with the JSON only`) gets [`GUIDE_PLAN`], a page prompt
    /// gets every part its marker lines ask for. Gated on its first call, or failing
    /// (or panicking) on every call.
    struct GuideAgent {
        gate: Mutex<Option<mpsc::Receiver<()>>>,
        prompts: Mutex<Vec<String>>,
        err: Option<String>,
        panics: bool,
        /// Only page calls fail with `err` (the plan call answers).
        pages_only: bool,
    }

    impl GuideAgent {
        fn make(gate: Option<mpsc::Receiver<()>>, err: Option<String>, panics: bool) -> Arc<Self> {
            Arc::new(Self { gate: Mutex::new(gate), prompts: Mutex::new(Vec::new()), err, panics, pages_only: false })
        }
        fn new() -> Arc<Self> {
            Self::make(None, None, false)
        }
        fn gated() -> (Arc<Self>, mpsc::Sender<()>) {
            let (tx, rx) = mpsc::channel();
            (Self::make(Some(rx), None, false), tx)
        }
        fn failing(err: &str) -> Arc<Self> {
            Self::make(None, Some(err.to_string()), false)
        }
        fn failing_pages(err: &str) -> Arc<Self> {
            Arc::new(Self {
                gate: Mutex::new(None),
                prompts: Mutex::new(Vec::new()),
                err: Some(err.to_string()),
                panics: false,
                pages_only: true,
            })
        }
        fn calls(&self) -> usize {
            self.prompts.lock().unwrap().len()
        }
        fn plan_calls(&self) -> usize {
            self.prompts.lock().unwrap().iter().filter(|p| is_plan_prompt(p)).count()
        }
    }

    fn is_plan_prompt(prompt: &str) -> bool {
        prompt.contains("\"features\"") && prompt.contains("Reply with the JSON only")
    }

    fn guide_parts(keys: &[(&str, &str)]) -> String {
        keys.iter()
            .map(|(k, h)| format!("<!-- part: {k} origin: agent -->\n## {h}\n\nBody of {k}.\n\n"))
            .collect()
    }

    /// The page reply for a page prompt: the parts its marker lines name.
    fn guide_page_reply(prompt: &str) -> String {
        if prompt.contains("<!-- part: what origin: agent -->") {
            guide_parts(&[
                ("what", "What it does"),
                ("value", "Business value and metrics"),
                ("how", "How it works"),
                ("associations", "Associations"),
                ("key-points", "Key points"),
            ])
        } else if prompt.contains("<!-- part: entities origin: agent -->") {
            guide_parts(&[("entities", "Entities"), ("relationships", "Relationships"), ("rules", "Rules and invariants")])
        } else {
            guide_parts(&[
                ("overview", "Overview"),
                ("components", "Components"),
                ("flows", "Request and data flows"),
                ("runtime", "Runtime and deployment"),
            ])
        }
    }

    impl Agent for GuideAgent {
        fn run(&self, prompt: &str) -> Result<String, String> {
            self.prompts.lock().unwrap().push(prompt.to_string());
            let gate = self.gate.lock().unwrap().take();
            if let Some(rx) = gate {
                let _ = rx.recv_timeout(std::time::Duration::from_secs(20));
            }
            if self.panics {
                panic!("the guide agent fell over");
            }
            if let Some(e) = &self.err {
                if !(self.pages_only && is_plan_prompt(prompt)) {
                    return Err(e.clone());
                }
            }
            if is_plan_prompt(prompt) {
                Ok(GUIDE_PLAN.to_string())
            } else {
                Ok(guide_page_reply(prompt))
            }
        }
    }

    fn with_guide_agent(sb: Sb, agent: Arc<GuideAgent>) -> Sb {
        let Sb { dir, state } = sb;
        Sb { dir, state: state.with_profile_agent(agent) }
    }

    fn guide_slot_held(alias: &str) -> bool {
        profile_build::is_running(&format!("guide:{alias}"))
    }

    fn wait_guide_idle(alias: &str) {
        for _ in 0..400 {
            if !guide_slot_held(alias) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("the guide of {alias} is still building");
    }

    fn guide_back(alias: &str, notice: &str) -> String {
        format!("/insights?view=memory&project={alias}&page=guide&notice={notice}")
    }

    /// Where a page rebuild sends the user back to: the page itself.
    fn page_back(alias: &str, slug: &str, notice: &str) -> String {
        format!("/insights?view=memory&project={alias}&page=guide&guide={slug}&notice={notice}")
    }

    #[test]
    fn building_starts_a_job_and_writes_pages_as_drafts() {
        let alias = "gb-build";
        let (agent, release) = GuideAgent::gated();
        let sb = with_guide_agent(Sb::new(&[("gb-plain", true)]), agent.clone());
        sb.add_managed(alias);
        let started = std::time::Instant::now();
        let loc = sb.post_to("/insights/memory/guide/build", &format!("project={alias}"));
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "the request did not wait for the agent");
        assert_eq!(loc, guide_back(alias, "guide-build-started"));
        assert!(guide_slot_held(alias), "the slot is taken on the click");
        assert!(!profile_build::is_running(alias), "the profile slot is not touched");
        // Mid-run: the plan call is waiting on the gate.
        let base = activity_base(&sb);
        let mut job = None;
        for _ in 0..400 {
            job = loopx_core::profile_progress::running_guide_for(&base, alias).filter(|j| j.step_code == "plan");
            if job.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let job = job.expect("the build reports that it is planning");
        assert_eq!(job.kind, "guide");
        let page = sb.get(&loc);
        assert!(page.contains("Building the guide in the background"), "{page}");
        assert!(page.contains(&format!("data-mem-guide-job='{alias}'")), "{page}");
        assert!(page.contains("Building the guide… This page updates by itself while it runs."), "{page}");
        assert!(page.contains("Planning the guide"), "{page}");
        assert!(page.contains("data-mem-watch"), "the script is asked to watch: {page}");
        assert!(!page.contains("action='/insights/memory/guide/build'"), "no second Build while it runs: {page}");
        assert!(page.contains("Working…"), "{page}");
        // Another frame page shows the strip too.
        let project = sb.get(&format!("/insights?view=memory&project={alias}"));
        assert!(project.contains(&format!("data-mem-guide-job='{alias}'")), "{project}");
        release.send(()).unwrap();
        wait_guide_idle(alias);
        let root = sb.root();
        let listed = loopx_core::project_guide::list(alias, Some(&root)).unwrap();
        let mut slugs: Vec<&str> = listed.pages.iter().map(|p| p.slug.as_str()).collect();
        slugs.sort_unstable();
        assert_eq!(slugs, ["architecture", "feature-a"]);
        assert!(listed.pages.iter().all(|p| p.status == Status::Draft));
        assert!(loopx_core::guide_plan::load(alias, Some(&root)).unwrap().is_some(), "plan.json saved");
        assert_eq!((agent.plan_calls(), agent.calls()), (1, 3), "one plan call, one call per page");
        assert!(loopx_core::profile_progress::running_guide_for(&base, alias).is_none());
        let page = sb.get(&guide_url(alias, None));
        assert!(page.contains("Guide built just now: 2 pages written, 0 waiting for approval, 0 failed."), "{page}");
        assert!(!page.contains("data-mem-guide-job="), "{page}");
        // With pages written the header offers a rebuild, and a page view rebuilds one page.
        assert!(page.contains("Rebuild guide") && page.contains("action='/insights/memory/guide/build'"), "{page}");
        let one = sb.get(&guide_url(alias, Some("feature-a")));
        let form_at = one.find("action='/insights/memory/guide/rebuild-page'").expect("a Rebuild this page form");
        let form = &one[form_at..form_at + one[form_at..].find("</form>").unwrap()];
        assert!(form.contains("name='csrf_token'"), "{form}");
        assert!(form.contains(&format!("<input type='hidden' name='project' value='{alias}'>")), "{form}");
        assert!(form.contains("<input type='hidden' name='slug' value='feature-a'>"), "{form}");
        assert!(form.contains("Rebuild this page"), "{form}");
    }

    #[test]
    fn a_second_click_while_running_is_refused() {
        let alias = "gb-twice";
        let (agent, release) = GuideAgent::gated();
        let sb = with_guide_agent(Sb::new(&[(alias, true)]), agent.clone());
        assert_eq!(sb.post_to("/insights/memory/guide/build", &format!("project={alias}")), guide_back(alias, "guide-build-started"));
        let again = sb.post_to("/insights/memory/guide/build", &format!("project={alias}"));
        assert_eq!(again, guide_back(alias, "guide-busy"));
        let page = sb.get(&again);
        assert!(page.contains("A guide build for this project is already running."), "{page}");
        assert!(page.contains("mem-notice-bad"), "{page}");
        release.send(()).unwrap();
        wait_guide_idle(alias);
        assert_eq!(agent.plan_calls(), 1, "only the first click started a build");
        assert_eq!(agent.calls(), 3);
    }

    #[test]
    fn a_missing_checkout_or_agent_or_unknown_alias_starts_nothing() {
        let agent = GuideAgent::new();
        let sb = with_guide_agent(Sb::new(&[("gb-nock", false), ("gb-known", true)]), agent.clone());
        put_plan(&sb, "gb-nock", &["feature-a"]);
        for (url, extra) in [("/insights/memory/guide/build", ""), ("/insights/memory/guide/rebuild-page", "&slug=architecture")] {
            let loc = sb.post_to(url, &format!("project=gb-nock{extra}"));
            let want = if extra.is_empty() {
                guide_back("gb-nock", "no-checkout")
            } else {
                page_back("gb-nock", "architecture", "no-checkout")
            };
            assert_eq!(loc, want, "{url}");
            let loc = sb.post_to(url, &format!("project=ghost{extra}"));
            assert_eq!(loc, "/insights?view=memory&notice=bad-input", "{url}");
            let loc = sb.post_to(url, extra.trim_start_matches('&'));
            assert_eq!(loc, "/insights?view=memory&notice=bad-input", "{url}");
        }
        let page = sb.get(&guide_back("gb-nock", "no-checkout"));
        assert!(page.contains("This project has no checkout to scan."), "{page}");
        let plain = Sb::new(&[("gb-noagent", true)]);
        put_plan(&plain, "gb-noagent", &["feature-a"]);
        let loc = plain.post_to("/insights/memory/guide/build", "project=gb-noagent");
        assert_eq!(loc, guide_back("gb-noagent", "no-agent"), "tests never start the real AI CLI");
        let loc = plain.post_to("/insights/memory/guide/rebuild-page", "project=gb-noagent&slug=architecture");
        assert_eq!(loc, page_back("gb-noagent", "architecture", "no-agent"));
        for alias in ["gb-nock", "ghost", "gb-noagent"] {
            assert!(!guide_slot_held(alias), "{alias}");
        }
        assert_eq!(agent.calls(), 0);
        assert!(loopx_core::project_guide::list("gb-nock", Some(&sb.root())).unwrap().pages.is_empty());
        assert!(loopx_core::project_guide::list("gb-noagent", Some(&plain.root())).unwrap().pages.is_empty());
    }

    #[test]
    fn rebuilding_one_page_calls_the_agent_once_and_raises_a_proposal_for_an_approved_page() {
        let alias = "gb-rebuild";
        let agent = GuideAgent::new();
        let sb = with_guide_agent(Sb::new(&[(alias, true)]), agent.clone());
        put_plan(&sb, alias, &["feature-a"]);
        put_page(&sb, alias, "feature-a", "Alpha", Status::Draft, &[("what", "Stored text.", Origin::Agent)]);
        let root = sb.root();
        assert!(loopx_core::guide_ops::approve_page(alias, "feature-a", now(), Some(&root)).unwrap());
        let path = loopx_core::project_guide::guide_dir(alias, Some(&root)).join("feature-a.md");
        let before = std::fs::read_to_string(&path).unwrap();
        let loc = sb.post_to("/insights/memory/guide/rebuild-page", &format!("project={alias}&slug=feature-a"));
        assert_eq!(loc, page_back(alias, "feature-a", "guide-build-started"), "back to the page itself");
        wait_guide_idle(alias);
        assert_eq!((agent.calls(), agent.plan_calls()), (1, 0), "one page call, no plan call");
        let raised: Vec<Value> = approvals::list(&sb.approvals(), None, now())
            .into_iter()
            .filter(|a| a["kind"] == "guide-page")
            .collect();
        assert_eq!(raised.len(), 1, "{raised:?}");
        assert_eq!(raised[0]["project"], alias);
        assert_eq!(raised[0]["target"]["slug"], "feature-a");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "an approved page is never written by a build");
        let page = sb.get(&guide_url(alias, Some("feature-a")));
        assert!(page.contains("Guide built just now: 0 pages written, 1 waiting for approval, 0 failed."), "{page}");
        // The same rebuild again: the page is held, but its proposal is already
        // pending, so nothing new waits for approval and the count says so.
        sb.post_to("/insights/memory/guide/rebuild-page", &format!("project={alias}&slug=feature-a"));
        wait_guide_idle(alias);
        assert_eq!(agent.calls(), 2);
        let raised = approvals::list(&sb.approvals(), None, now()).into_iter().filter(|a| a["kind"] == "guide-page").count();
        assert_eq!(raised, 1, "no second proposal");
        let page = sb.get(&guide_url(alias, Some("feature-a")));
        assert!(page.contains("Guide built just now: 0 pages written, 0 waiting for approval, 0 failed."), "{page}");
    }

    #[test]
    fn rebuild_page_rejects_bad_and_out_of_plan_slugs() {
        let alias = "gb-slugs";
        let agent = GuideAgent::new();
        let sb = with_guide_agent(Sb::new(&[(alias, true)]), agent.clone());
        put_plan(&sb, alias, &["feature-a"]);
        // `data-model` is a valid slug but this plan has no schema files.
        // `roadmap` is always in a plan, but a build never writes it (it is not rebuildable).
        for slug in ["..%2Fx", "Feature-A", "feature-nope", "data-model", "roadmap", "", "feature-a%0A"] {
            let loc = sb.post_to("/insights/memory/guide/rebuild-page", &format!("project={alias}&slug={slug}"));
            assert_eq!(loc, guide_back(alias, "bad-input"), "{slug}");
        }
        let loc = sb.post_to("/insights/memory/guide/rebuild-page", &format!("project={alias}"));
        assert_eq!(loc, guide_back(alias, "bad-input"), "no slug at all");
        assert!(!guide_slot_held(alias));
        assert_eq!(agent.calls(), 0, "the agent is never called for a refused slug");
        assert!(loopx_core::project_guide::list(alias, Some(&sb.root())).unwrap().pages.is_empty());
    }

    #[test]
    fn a_failed_build_shows_a_fixed_reason_and_no_agent_text() {
        let alias = "gb-fail";
        let secret = ["glpat-", "SECRETsecret1234567890"].concat();
        let agent = GuideAgent::failing(&format!("boom: the CLI said {secret} and crashed"));
        let sb = with_guide_agent(Sb::new(&[(alias, true)]), agent);
        let loc = sb.post_to("/insights/memory/guide/build", &format!("project={alias}"));
        assert!(!loc.contains("glpat") && !loc.contains("boom"), "{loc}");
        wait_guide_idle(alias);
        let page = sb.get(&guide_url(alias, None));
        assert!(page.contains("The last guide build failed just now: the AI call failed"), "{page}");
        assert!(page.contains("mem-job-bad"), "{page}");
        assert_no_leak(&sb, alias, &page);
        assert!(loopx_core::project_guide::list(alias, Some(&sb.root())).unwrap().pages.is_empty());
    }

    /// No page, job line, jobs JSON or trace line of `alias` holds the secret or the
    /// agent's own words. The trace must exist: a run always writes one.
    fn assert_no_leak(sb: &Sb, alias: &str, page: &str) {
        let project = sb.get(&format!("/insights?view=memory&project={alias}"));
        let (_, jobs) = jobs_json(sb, &format!("/insights/memory/jobs?project={alias}"), None);
        let (_, all) = jobs_json(sb, "/insights/memory/jobs", None);
        let trace = std::fs::read_to_string(activity_base(sb).with_file_name("agent-trace.jsonl")).expect("the run wrote a trace");
        assert!(trace.contains(&format!("\"alias\":\"{alias}\"")), "{trace}");
        for text in [page, &project, &jobs.to_string(), &all.to_string(), &trace] {
            assert!(!text.contains("glpat") && !text.contains("SECRETsecret") && !text.contains("boom"), "{text}");
        }
    }

    /// The trace's last line for `alias`'s guide job.
    fn last_guide_trace(sb: &Sb, alias: &str) -> Value {
        let trace = std::fs::read_to_string(activity_base(sb).with_file_name("agent-trace.jsonl")).unwrap();
        trace
            .lines()
            .rev()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v["kind"] == "guide" && v["alias"] == alias)
            .expect("a guide trace line")
    }

    #[test]
    fn a_failing_page_call_shows_no_agent_text() {
        let alias = "gb-pagefail";
        let secret = ["glpat-", "SECRETsecret1234567890"].concat();
        let agent = GuideAgent::failing_pages(&format!("boom: page call leaked {secret}"));
        let sb = with_guide_agent(Sb::new(&[(alias, true)]), agent.clone());
        sb.post_to("/insights/memory/guide/build", &format!("project={alias}"));
        wait_guide_idle(alias);
        assert_eq!((agent.plan_calls(), agent.calls()), (1, 3));
        let page = sb.get(&guide_url(alias, None));
        assert!(page.contains("The last guide build failed just now: the AI call failed"), "{page}");
        assert_no_leak(&sb, alias, &page);
        assert_eq!(last_guide_trace(&sb, alias)["phase"], "failed");
    }

    #[test]
    fn a_failed_clone_marks_the_job_failed_and_shows_no_remote_text() {
        let alias = "gb-badclone";
        let agent = GuideAgent::new();
        let sb = with_guide_agent(Sb::new(&[]), agent.clone());
        let dir = sb.add_managed(alias);
        // A remote whose (unreachable) address holds a secret-looking value.
        let leaky = format!("file://{}/glpat-SECRETsecret1234567890.git", sb.dir.path().display());
        loopx_core::profile_checkout::register_test_remote(
            "work",
            "grp/gb-leaky",
            loopx_core::profile_checkout::Remote { url: leaky, branch: "main".into(), auth_env: vec![] },
        );
        let path = sb.state.home_dir.join("projects.json");
        let mut cfg: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        cfg["projects"][alias]["project_id"] = json!("grp/gb-leaky");
        std::fs::write(&path, cfg.to_string()).unwrap();
        let loc = sb.post_to("/insights/memory/guide/build", &format!("project={alias}"));
        assert_eq!(loc, guide_back(alias, "guide-build-started"));
        wait_guide_idle(alias);
        assert_eq!(agent.calls(), 0, "nothing to scan, no AI call");
        assert!(!dir.join("README.md").exists());
        let page = sb.get(&guide_url(alias, None));
        assert!(page.contains("The last guide build failed just now:"), "{page}");
        assert_no_leak(&sb, alias, &page);
        // The record says it failed, not that it was interrupted.
        let last = last_guide_trace(&sb, alias);
        assert_eq!(last["phase"], "failed", "{last}");
        assert!(loopx_core::profile_progress::FAIL_REASONS.contains(&last["reason"].as_str().unwrap()), "{last}");
    }

    #[test]
    fn reclone_is_refused_while_a_guide_run_holds_the_clone() {
        let sb = Sb::new(&[]);
        let alias = "gb-reclone";
        let dir = sb.add_managed(alias);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("in-use"), "x").unwrap();
        let slot = loopx_core::guide_build::try_acquire(alias).unwrap();
        let loc = sb.post_to("/insights/memory/checkout/reclone", &format!("project={alias}"));
        assert!(loc.ends_with("notice=busy"), "{loc}");
        assert!(dir.join("in-use").is_file(), "the clone a guide build uses is kept");
        drop(slot);
        let loc = sb.post_to("/insights/memory/checkout/reclone", &format!("project={alias}"));
        assert!(loc.ends_with("notice=recloned"), "{loc}");
        assert!(!dir.exists());
        assert!(!guide_slot_held(alias) && !profile_build::is_running(alias), "both slots are let go");
    }

    #[test]
    fn rebuild_this_page_is_offered_only_for_pages_a_build_writes() {
        let alias = "gb-roadmap";
        let sb = Sb::new(&[(alias, true)]);
        put_plan(&sb, alias, &["feature-a"]);
        let roadmap_key = loopx_core::project_guide::Kind::Roadmap.parts()[0].0;
        put_page(&sb, alias, "roadmap", "Roadmap", Status::Draft, &[(roadmap_key, "Next up.", Origin::Agent)]);
        put_page(&sb, alias, "architecture", "Architecture", Status::Draft, &[("overview", "A web app.", Origin::Agent)]);
        put_page(&sb, alias, "feature-off", "Off plan", Status::Draft, &[("what", "Gone.", Origin::Agent)]);
        let roadmap = sb.get(&guide_url(alias, Some("roadmap")));
        assert!(roadmap.contains("action='/insights/memory/guide/build'"), "{roadmap}");
        assert!(!roadmap.contains("action='/insights/memory/guide/rebuild-page'"), "{roadmap}");
        let off = sb.get(&guide_url(alias, Some("feature-off")));
        assert!(!off.contains("action='/insights/memory/guide/rebuild-page'"), "{off}");
        let arch = sb.get(&guide_url(alias, Some("architecture")));
        assert!(arch.contains("action='/insights/memory/guide/rebuild-page'"), "{arch}");
    }

    #[test]
    fn a_panicking_build_is_an_internal_error() {
        let alias = "gb-panic";
        let sb = with_guide_agent(Sb::new(&[(alias, true)]), GuideAgent::make(None, None, true));
        sb.post_to("/insights/memory/guide/build", &format!("project={alias}"));
        wait_guide_idle(alias);
        let page = sb.get(&guide_url(alias, None));
        assert!(page.contains("The last guide build failed just now: internal error"), "{page}");
        assert!(!page.contains("fell over"), "{page}");
        assert!(loopx_core::profile_progress::running_guide_for(&activity_base(&sb), alias).is_none());
    }

    #[test]
    fn jobs_status_lists_a_running_guide_job_and_not_as_a_profile_build() {
        let alias = "gj-alpha";
        let sb = Sb::new(&[(alias, true), ("gj-other", true)]);
        let base = activity_base(&sb);
        let job = ProfileJob::start(&base, ProfileJobKind::Guide, alias);
        loopx_core::profile_progress::Progress::step(&job, 5, 14, loopx_core::profile_progress::Step::Page);
        let (r, v) = jobs_json(&sb, &format!("/insights/memory/jobs?project={alias}"), None);
        assert_eq!(r.status, 200);
        assert!(v["running"].as_array().unwrap().is_empty(), "never a profile build: {v}");
        let guide = v["guide"].as_array().unwrap();
        assert_eq!(guide.len(), 1, "{v}");
        assert_eq!(guide[0]["alias"], alias);
        assert_eq!((guide[0]["step"].as_u64(), guide[0]["steps"].as_u64()), (Some(5), Some(14)));
        assert_eq!(guide[0]["code"], "page");
        assert_eq!(guide[0]["label"], "Writing a page \u{b7} step 5 of 14");
        let (_, fr) = jobs_json(&sb, &format!("/insights/memory/jobs?project={alias}"), Some("fr"));
        assert_ne!(fr["guide"][0]["label"], "Writing a page \u{b7} step 5 of 14", "translated: {fr}");
        let (_, other) = jobs_json(&sb, "/insights/memory/jobs?project=gj-other", None);
        assert!(other["guide"].as_array().unwrap().is_empty(), "{other}");
        let (_, all) = jobs_json(&sb, "/insights/memory/jobs", None);
        assert_eq!(all["guide"].as_array().unwrap().len(), 1, "{all}");
        // The page shows the same step in its strip.
        let page = sb.get(&guide_url(alias, None));
        assert!(page.contains(&format!("data-mem-guide-job='{alias}'")) && page.contains("Writing a page \u{b7} step 5 of 14"), "{page}");
        drop(job);
        let (_, gone) = jobs_json(&sb, &format!("/insights/memory/jobs?project={alias}"), None);
        assert!(gone["guide"].as_array().unwrap().is_empty(), "{gone}");
        // A build this process started whose record is not written yet: the slot alone.
        let slot = loopx_core::guide_build::try_acquire(alias).unwrap();
        let (_, v) = jobs_json(&sb, &format!("/insights/memory/jobs?project={alias}"), None);
        assert_eq!(v["guide"][0]["alias"], alias, "{v}");
        assert_eq!(v["guide"][0]["code"], "start");
        assert!(v["running"].as_array().unwrap().is_empty(), "{v}");
        drop(slot);
    }

    #[test]
    fn the_guide_build_does_not_disable_profile_refresh_and_vice_versa() {
        let alias = "gb-indep";
        let sb = with_guide_agent(Sb::new(&[(alias, true)]), GuideAgent::new());
        sb.save(&profile(alias, &[(SectionKey::Stack, section("Rust.", "Rust body"))]));
        // The guide slot held: the profile's Refresh buttons are still there.
        let guide_slot = loopx_core::guide_build::try_acquire(alias).unwrap();
        let project = sb.get(&format!("/insights?view=memory&project={alias}"));
        assert!(project.contains("action='/insights/memory/build'") && project.contains("Refresh profile"), "{project}");
        assert!(!project.contains("data-mem-job="), "no profile strip: {project}");
        assert!(project.contains(&format!("data-mem-guide-job='{alias}'")), "{project}");
        let stack = sb.get(&format!("/insights?view=memory&project={alias}&page=stack"));
        assert!(stack.contains("action='/insights/memory/section/refresh'"), "{stack}");
        let guide = sb.get(&guide_url(alias, None));
        assert!(!guide.contains("action='/insights/memory/guide/build'") && guide.contains("Working…"), "{guide}");
        drop(guide_slot);
        // The profile slot held: Build guide is still offered and still starts.
        let profile_slot = profile_build::try_acquire(alias).unwrap();
        let guide = sb.get(&guide_url(alias, None));
        assert!(guide.contains("action='/insights/memory/guide/build'"), "{guide}");
        let project = sb.get(&format!("/insights?view=memory&project={alias}"));
        assert!(!project.contains("action='/insights/memory/build'"), "the profile itself is busy: {project}");
        let loc = sb.post_to("/insights/memory/guide/build", &format!("project={alias}"));
        assert_eq!(loc, guide_back(alias, "guide-build-started"));
        wait_guide_idle(alias);
        drop(profile_slot);
        assert_eq!(loopx_core::project_guide::list(alias, Some(&sb.root())).unwrap().pages.len(), 2);
    }

    #[test]
    fn the_guide_build_strings_are_translated() {
        let alias = "gb-fr";
        let sb = Sb::new(&[(alias, true)]);
        let slot = loopx_core::guide_build::try_acquire(alias).unwrap();
        let html = get_fr(&sb, &guide_back(alias, "guide-build-started"));
        assert!(!html.contains("Building the guide"), "{html}");
        assert!(!html.contains("Working…"), "{html}");
        drop(slot);
        let html = get_fr(&sb, &guide_back(alias, "guide-busy"));
        assert!(!html.contains("A guide build for this project is already running."), "{html}");
        record(
            &sb.state.home_dir,
            alias,
            Job { at: now(), kind: JobKind::Guide, outcome: JobOutcome::GuideBuilt { saved: 3, held: 1, failed: 2 } },
        );
        let html = get_fr(&sb, &guide_url(alias, None));
        assert!(!html.contains("Guide built"), "{html}");
        assert!(html.contains('3') && html.contains("mem-job"), "{html}");
        // A guide result does not replace the profile's own last result, and vice versa.
        failed_build(&sb, alias, now());
        assert!(last_job(&sb.state.home_dir, alias).is_some_and(|j| matches!(j.kind, JobKind::Build)));
        let html = sb.get(&format!("/insights?view=memory&project={alias}"));
        assert!(html.contains("The last build failed") && html.contains("Guide built just now: 3 pages written, 1 waiting for approval, 2 failed."), "{html}");
    }

    // ------------------------------------------------------------------ guide page actions

    fn gd_file(sb: &Sb, alias: &str, slug: &str) -> PathBuf {
        loopx_core::project_guide::guide_dir(alias, Some(&sb.root())).join(format!("{slug}.md"))
    }

    fn gd_load(sb: &Sb, alias: &str, slug: &str) -> Option<loopx_core::project_guide::Page> {
        loopx_core::project_guide::load(alias, slug, Some(&sb.root())).unwrap()
    }

    fn gd_history(sb: &Sb, alias: &str, slug: &str) -> Vec<String> {
        loopx_core::project_guide::history(alias, slug, Some(&sb.root()))
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// The right-hand side of the guide page (the selected page or the history view).
    fn gd_main(html: &str) -> &str {
        let at = html.find("class='gd-main'").unwrap_or_else(|| panic!("no gd-main: {html}"));
        let end = html.find("data-mem-live").unwrap();
        &html[at..end]
    }

    fn history_url(alias: &str, slug: &str) -> String {
        format!("{}&history={slug}", guide_url(alias, None))
    }

    /// Every `<textarea …>…</textarea>` body in `html`.
    fn textareas(html: &str) -> Vec<&str> {
        html.split("<textarea").skip(1).map(|t| {
            let open = t.find('>').unwrap() + 1;
            &t[open..t.find("</textarea>").unwrap()]
        }).collect()
    }

    const APPROVE: &str = "/insights/memory/guide/approve";
    const APPROVE_ALL: &str = "/insights/memory/guide/approve-all";
    const EDIT: &str = "/insights/memory/guide/edit";
    const DELETE: &str = "/insights/memory/guide/delete";
    const RESTORE: &str = "/insights/memory/guide/restore";

    #[test]
    fn approving_a_draft_flips_it_and_the_page_shows_approved() {
        let alias = "ga-appr";
        let sb = Sb::new(&[(alias, true)]);
        put_page(&sb, alias, "architecture", "Architecture", Status::Approved, &[("overview", "Layers.", Origin::Agent)]);
        put_page(&sb, alias, "feature-orders", "Orders", Status::Draft, &[("what", "Takes orders.", Origin::Agent)]);
        let html = sb.get(&guide_url(alias, Some("feature-orders")));
        let main = gd_main(&html);
        assert!(main.contains(&format!("action='{APPROVE}'")), "{main}");
        assert!(main.contains("name='slug' value='feature-orders'"), "{main}");
        // An approved page has no Approve button.
        let html = sb.get(&guide_url(alias, Some("architecture")));
        assert!(!gd_main(&html).contains(&format!("action='{APPROVE}'")), "{html}");

        let loc = sb.post_to(APPROVE, &format!("project={alias}&slug=feature-orders"));
        assert_eq!(loc, page_back(alias, "feature-orders", "guide-approved"));
        let page = gd_load(&sb, alias, "feature-orders").unwrap();
        assert_eq!(page.status, Status::Approved);
        assert_eq!(page.parts[0].body, "Takes orders.");
        let html = sb.get(&loc);
        assert!(html.contains("Page approved. Agents now read it."), "{html}");
        let main = gd_main(&html);
        assert!(main.contains("mem-pill-fresh'>Approved</span>"), "{main}");
        assert!(!main.contains("Draft: agents do not read this page"), "{main}");
        assert!(!main.contains(&format!("action='{APPROVE}'")), "{main}");

        // A second approve is harmless: same notice, nothing written, no new snapshot.
        let bytes = std::fs::read(gd_file(&sb, alias, "feature-orders")).unwrap();
        let snaps = gd_history(&sb, alias, "feature-orders").len();
        let loc = sb.post_to(APPROVE, &format!("project={alias}&slug=feature-orders"));
        assert_eq!(loc, page_back(alias, "feature-orders", "guide-approved"));
        assert_eq!(std::fs::read(gd_file(&sb, alias, "feature-orders")).unwrap(), bytes);
        assert_eq!(gd_history(&sb, alias, "feature-orders").len(), snaps);
    }

    #[test]
    fn approve_all_approves_every_draft_and_reports_the_count() {
        let alias = "ga-all";
        let sb = Sb::new(&[(alias, true)]);
        put_page(&sb, alias, "architecture", "Architecture", Status::Approved, &[("overview", "Layers.", Origin::Agent)]);
        for slug in ["feature-a", "feature-b", "roadmap"] {
            let part = if slug == "roadmap" { "planned" } else { "what" };
            put_page(&sb, alias, slug, slug, Status::Draft, &[(part, "Text.", Origin::Agent)]);
        }
        let html = sb.get(&guide_url(alias, None));
        let list_at = html.find("class='card gd-list'").unwrap();
        let list = &html[list_at..list_at + html[list_at..].find("</nav>").unwrap()];
        assert!(list.contains(&format!("action='{APPROVE_ALL}'")), "{list}");
        assert!(list.contains("Approve all drafts"), "{list}");

        let loc = sb.post_to(APPROVE_ALL, &format!("project={alias}"));
        assert_eq!(loc, format!("/insights?view=memory&project={alias}&page=guide&approved=3&notice=guide-all-approved"));
        for slug in ["architecture", "feature-a", "feature-b", "roadmap"] {
            assert_eq!(gd_load(&sb, alias, slug).unwrap().status, Status::Approved, "{slug}");
        }
        let html = sb.get(&loc);
        assert!(html.contains("Draft pages approved: 3. Agents now read them."), "{html}");
        // No draft left: no Approve all button.
        assert!(!html.contains(&format!("action='{APPROVE_ALL}'")), "{html}");
        // Nothing to approve: still a notice, with a zero count.
        let loc = sb.post_to(APPROVE_ALL, &format!("project={alias}"));
        assert!(loc.ends_with("&approved=0&notice=guide-all-approved"), "{loc}");
    }

    #[test]
    fn editing_a_part_marks_it_human_and_shows_the_approved_note() {
        let alias = "ga-edit";
        let sb = Sb::new(&[(alias, true)]);
        put_page(
            &sb,
            alias,
            "feature-orders",
            "Orders",
            Status::Approved,
            &[("what", "Old <b>text</b>.", Origin::Agent), ("how", "Queues.", Origin::Agent)],
        );
        put_page(&sb, alias, "roadmap", "Roadmap", Status::Draft, &[("planned", "Refunds.", Origin::Agent)]);
        let html = sb.get(&guide_url(alias, Some("feature-orders")));
        let main = gd_main(&html);
        // One editor per part, each one form with one Save.
        assert_eq!(main.matches("<details class='gd-edit'>").count(), 2, "{main}");
        assert_eq!(main.matches(&format!("action='{EDIT}'")).count(), 2, "{main}");
        for editor in main.split("<details class='gd-edit'>").skip(1) {
            let editor = &editor[..editor.find("</details>").unwrap()];
            assert_eq!((editor.matches("<form").count(), editor.matches("type='submit'").count()), (1, 1), "{editor}");
        }
        assert!(main.contains("name='part' value='what'") && main.contains("name='part' value='how'"), "{main}");
        assert!(main.contains("name='body' maxlength='20000'"), "{main}");
        // Prefilled with the stored text, escaped.
        assert_eq!(textareas(main), vec!["Old &lt;b&gt;text&lt;/b&gt;.", "Queues."]);
        assert!(main.contains("This page is approved, so agents see your change immediately."), "{main}");
        assert!(main.contains("Saving marks this part as written by a person; rebuilds keep it."), "{main}");
        // A draft has no "approved" note.
        let html = sb.get(&guide_url(alias, Some("roadmap")));
        assert!(!html.contains("This page is approved, so agents see your change immediately."), "{html}");

        let loc = sb.post_to(EDIT, &format!("project={alias}&slug=feature-orders&part=what&body=Line+one%0D%0ALine+two%0D%0A"));
        assert_eq!(loc, page_back(alias, "feature-orders", "guide-edited"));
        let page = gd_load(&sb, alias, "feature-orders").unwrap();
        let what = page.parts.iter().find(|x| x.key == "what").unwrap();
        assert_eq!((what.body.as_str(), what.origin), ("Line one\nLine two", Origin::Human));
        let how = page.parts.iter().find(|x| x.key == "how").unwrap();
        assert_eq!((how.body.as_str(), how.origin), ("Queues.", Origin::Agent));
        assert_eq!(page.status, Status::Approved);
        assert!(!std::fs::read_to_string(gd_file(&sb, alias, "feature-orders")).unwrap().contains('\r'));
        let html = sb.get(&loc);
        assert!(html.contains("Part saved. It is now marked as written by a person; rebuilds keep it."), "{html}");
        let main = gd_main(&html);
        let what_at = main.find("<h3>What it does</h3>").unwrap();
        let what_head = &main[what_at..what_at + main[what_at..].find("</div>").unwrap()];
        assert!(what_head.contains("mem-pill-human'>Written by a person</span>"), "{what_head}");
        assert!(main.contains("Written by the agent and a person"), "{main}");
    }

    #[test]
    fn editing_refuses_secrets_empty_text_and_unknown_parts_without_writing() {
        let alias = "ga-refuse";
        let sb = Sb::new(&[(alias, true)]);
        put_page(&sb, alias, "feature-orders", "Orders", Status::Approved, &[("what", "Old text.", Origin::Agent)]);
        let file = gd_file(&sb, alias, "feature-orders");
        let bytes = std::fs::read(&file).unwrap();
        let token = ["glpat-", "abcDEF123456ghiJKL78"].concat();
        let big = "x".repeat(64 * 1024 + 1);
        for (part, body, notice) in [
            ("what", format!("Use+{token}+here"), "secret"),
            ("what", String::new(), "bad-input"),
            ("what", "+%0D%0A+".to_string(), "bad-input"),
            ("nope", "Fine.".to_string(), "bad-input"),
            // A part of another kind of page.
            ("overview", "Fine.".to_string(), "bad-input"),
            ("what", big, "guide-too-long"),
        ] {
            let loc = sb.post_to(EDIT, &format!("project={alias}&slug=feature-orders&part={part}&body={body}"));
            assert_eq!(loc, page_back(alias, "feature-orders", notice), "{part} {notice}");
            assert!(!loc.contains("glpat") && !loc.contains("Use"), "{loc}");
            let html = sb.get(&loc);
            assert!(!html.contains("glpat"), "{html}");
            assert!(!html.contains("xxxxxxxxxx"), "the refused text is not kept");
            assert_eq!(std::fs::read(&file).unwrap(), bytes, "{part} {notice}");
        }
        assert!(gd_history(&sb, alias, "feature-orders").is_empty());
        assert!(sb.get(&page_back(alias, "feature-orders", "secret")).contains("Nothing was saved: the text looks like it contains a secret value."));
        // The editor still shows the stored text, not the refused one.
        assert_eq!(textareas(gd_main(&sb.get(&page_back(alias, "feature-orders", "secret")))), vec!["Old text."]);
    }

    #[test]
    fn the_editor_never_carries_text_of_a_redacted_page() {
        let alias = "ga-redact";
        let sb = Sb::new(&[(alias, true)]);
        put_page(
            &sb,
            alias,
            "feature-orders",
            "Orders",
            Status::Draft,
            &[("what", "Body text here.", Origin::Agent), ("how", "Plain words.", Origin::Agent)],
        );
        let token = ["glpat-", "abcDEF123456ghiJKL78"].concat();
        hand_edit(&sb, alias, "Body text here.", &format!("Body text here {token}."));
        let html = sb.get(&guide_url(alias, Some("feature-orders")));
        let main = gd_main(&html);
        assert!(main.contains("class='mem-redacted'"), "{main}");
        let areas = textareas(main);
        assert_eq!(areas.len(), 2, "{main}");
        assert!(areas.iter().all(|t| t.is_empty()), "{areas:?}");
        for hidden_text in ["glpat", "Body text here", "Plain words", "Summary of feature-orders"] {
            assert!(!html.contains(hidden_text), "{hidden_text}: {html}");
        }
        // Approving a page that holds a secret is not offered.
        assert!(!main.contains(&format!("action='{APPROVE}'")), "{main}");
        assert!(main.contains("history=feature-orders"), "{main}");
        // Overwriting the part that holds the secret clears the redaction.
        let loc = sb.post_to(EDIT, &format!("project={alias}&slug=feature-orders&part=what&body=Clean."));
        assert_eq!(loc, page_back(alias, "feature-orders", "guide-edited"));
        let html = sb.get(&loc);
        assert!(!html.contains("class='mem-redacted'") && html.contains("Clean.") && !html.contains("glpat"), "{html}");
    }

    #[test]
    fn deleting_is_offered_and_allowed_only_outside_the_plan() {
        let alias = "ga-del";
        let sb = Sb::new(&[(alias, true)]);
        put_plan(&sb, alias, &["feature-a"]);
        put_page(&sb, alias, "architecture", "Architecture", Status::Approved, &[("overview", "Layers.", Origin::Agent)]);
        put_page(&sb, alias, "feature-a", "A", Status::Approved, &[("what", "In plan.", Origin::Agent)]);
        put_page(&sb, alias, "feature-old", "Old", Status::Approved, &[("what", "Gone from plan.", Origin::Agent)]);
        for slug in ["architecture", "feature-a"] {
            let html = sb.get(&guide_url(alias, Some(slug)));
            assert!(!html.contains(&format!("action='{DELETE}'")), "{slug}: {html}");
            let loc = sb.post_to(DELETE, &format!("project={alias}&slug={slug}"));
            assert_eq!(loc, page_back(alias, slug, "guide-in-plan"));
            assert!(gd_file(&sb, alias, slug).is_file(), "{slug}");
        }
        assert!(sb.get(&page_back(alias, "feature-a", "guide-in-plan")).contains("This page is in the current plan, so it was not deleted."));

        let html = sb.get(&guide_url(alias, Some("feature-old")));
        let main = gd_main(&html);
        let del_at = main.find("<details class='gd-delete'>").unwrap_or_else(|| panic!("{main}"));
        let del = &main[del_at..del_at + main[del_at..].find("</details>").unwrap()];
        assert!(del.contains(&format!("action='{DELETE}'")) && del.contains("name='slug' value='feature-old'"), "{del}");
        assert!(del.contains("gd-btn-danger"), "{del}");
        // The app shell carries its own confirm script; the guide never uses it.
        assert!(!main.contains("confirm(") && !main.contains("data-confirm") && !main.contains("onclick"), "no browser dialog: {main}");

        let loc = sb.post_to(DELETE, &format!("project={alias}&slug=feature-old"));
        assert_eq!(loc, guide_back(alias, "guide-deleted"));
        assert!(!gd_file(&sb, alias, "feature-old").exists());
        let snaps = gd_history(&sb, alias, "feature-old");
        assert_eq!(snaps.len(), 1);
        let html = sb.get(&loc);
        assert!(html.contains("Page deleted."), "{html}");
        assert!(!html.contains("guide=feature-old'"), "the row is gone: {html}");
        let deleted_at = html.find("class='gd-deleted'").unwrap();
        assert!(html[deleted_at..].contains("history=feature-old"), "{html}");
        // Deleting it again: nothing there to delete.
        assert_eq!(sb.post_to(DELETE, &format!("project={alias}&slug=feature-old")), page_back(alias, "feature-old", "bad-input"));
        assert_eq!(gd_history(&sb, alias, "feature-old").len(), 1);
    }

    #[test]
    fn history_lists_snapshots_and_restore_brings_back_an_older_version_or_a_deleted_page() {
        let alias = "ga-hist";
        let sb = Sb::new(&[(alias, true)]);
        put_page(&sb, alias, "feature-orders", "Orders", Status::Approved, &[("what", "Version one.", Origin::Agent)]);
        for body in ["Version+two.", "Version+three."] {
            let loc = sb.post_to(EDIT, &format!("project={alias}&slug=feature-orders&part=what&body={body}"));
            assert_eq!(loc, page_back(alias, "feature-orders", "guide-edited"));
        }
        let snaps = gd_history(&sb, alias, "feature-orders");
        assert_eq!(snaps.len(), 2);
        // The page links to its history.
        assert!(gd_main(&sb.get(&guide_url(alias, Some("feature-orders")))).contains("history=feature-orders"));
        let html = sb.get(&history_url(alias, "feature-orders"));
        let main = gd_main(&html);
        assert_eq!(main.matches(&format!("action='{RESTORE}'")).count(), 2, "{main}");
        // Newest first, each with the time from its file name, and no page text.
        let (newer, older) = (main.find(&snaps[0]).unwrap(), main.find(&snaps[1]).unwrap());
        assert!(newer < older, "{main}");
        let ts = snaps[1].trim_start_matches("feature-orders.").trim_end_matches(".md");
        let shown = format!("{}-{}-{} {}:{}:{} UTC", &ts[0..4], &ts[4..6], &ts[6..8], &ts[9..11], &ts[11..13], &ts[13..15]);
        assert!(main.contains(&shown), "{shown}: {main}");
        assert!(!main.contains("Version"), "{main}");
        assert!(main.contains(&format!("href='/insights?view=memory&amp;project={alias}&amp;page=guide&amp;guide=feature-orders'")), "back link: {main}");

        let loc = sb.post_to(RESTORE, &format!("project={alias}&slug=feature-orders&snapshot={}", snaps[1]));
        assert_eq!(loc, page_back(alias, "feature-orders", "guide-restored"));
        assert_eq!(gd_load(&sb, alias, "feature-orders").unwrap().parts[0].body, "Version one.");
        let html = sb.get(&loc);
        assert!(html.contains("Page restored from its history.") && gd_main(&html).contains("Version one."), "{html}");

        // A deleted page (no plan: nothing is in it) comes back from the Deleted pages history.
        assert_eq!(sb.post_to(DELETE, &format!("project={alias}&slug=feature-orders")), guide_back(alias, "guide-deleted"));
        assert!(gd_load(&sb, alias, "feature-orders").is_none());
        let html = sb.get(&guide_url(alias, None));
        assert!(html.contains(&format!("href='/insights?view=memory&amp;project={alias}&amp;page=guide&amp;history=feature-orders'")), "{html}");
        let html = sb.get(&history_url(alias, "feature-orders"));
        let snaps = gd_history(&sb, alias, "feature-orders");
        assert_eq!(html.matches(&format!("action='{RESTORE}'")).count(), snaps.len(), "{html}");
        assert!(!html.contains("Version") && !html.contains("This page does not exist."), "{html}");
        let loc = sb.post_to(RESTORE, &format!("project={alias}&slug=feature-orders&snapshot={}", snaps[0]));
        assert_eq!(loc, page_back(alias, "feature-orders", "guide-restored"));
        assert_eq!(gd_load(&sb, alias, "feature-orders").unwrap().parts[0].body, "Version one.");
        assert!(!sb.get(&guide_url(alias, None)).contains("class='gd-deleted'"));
    }

    #[test]
    fn hostile_form_values_are_refused() {
        let alias = "ga-host";
        let sb = Sb::new(&[(alias, true), ("ga-host-empty", true)]);
        put_page(&sb, alias, "feature-orders", "Orders", Status::Draft, &[("what", "Old text.", Origin::Agent)]);
        let file = gd_file(&sb, alias, "feature-orders");
        let bytes = std::fs::read(&file).unwrap();
        let index = guide_back(alias, "bad-input");
        let on_page = page_back(alias, "feature-orders", "bad-input");
        let huge = "y".repeat(1024 * 1024);
        let cases: Vec<(&str, String, String)> = vec![
            (APPROVE, format!("project={alias}&slug=..%2Fx"), index.clone()),
            (APPROVE, format!("project={alias}&slug=%3Cscript%3E"), index.clone()),
            (APPROVE, format!("project={alias}"), index.clone()),
            (EDIT, format!("project={alias}&slug=..%2Fx&part=what&body=Hi"), index.clone()),
            (EDIT, format!("project={alias}&slug=feature-orders&part=%3Cscript%3E&body=Hi"), on_page.clone()),
            (EDIT, format!("project={alias}&slug=feature-orders&part=what&body={huge}"), page_back(alias, "feature-orders", "guide-too-long")),
            (DELETE, format!("project={alias}&slug=..%2F..%2Fx"), index.clone()),
            (RESTORE, format!("project={alias}&slug=feature-orders&snapshot=..%2F..%2Fx"), on_page.clone()),
            (RESTORE, format!("project={alias}&slug=feature-orders&snapshot=feature-orders.20260101T000000000000000Z.md"), on_page.clone()),
            (RESTORE, format!("project={alias}&slug=feature-orders"), on_page.clone()),
            (RESTORE, format!("project={alias}&slug=..%2Fx&snapshot=x.md"), index.clone()),
        ];
        let unknown = "/insights?view=memory&notice=bad-input".to_string();
        let mut all = cases;
        for route in [APPROVE, APPROVE_ALL, EDIT, DELETE, RESTORE] {
            all.push((route, "project=nope&slug=feature-orders&part=what&body=Hi&snapshot=x.md".into(), unknown.clone()));
            all.push((route, "project=%3Cscript%3E&slug=feature-orders".into(), unknown.clone()));
        }
        for (route, body, want) in &all {
            let loc = sb.post_to(route, body);
            assert_eq!(&loc, want, "{route} {body:.80}");
            assert!(!loc.contains("script") && !loc.contains("..") && !loc.contains("yyyy") && !loc.contains("nope"), "{loc}");
            let html = sb.get(&loc);
            assert!(!html.contains("<script>x") && !html.contains("&lt;script&gt;") && !html.contains("yyyyyyyy"), "{route}");
            assert_eq!(std::fs::read(&file).unwrap(), bytes, "{route} {body:.80}");
        }
        assert!(gd_history(&sb, alias, "feature-orders").is_empty());
        // A valid slug with no page creates nothing: no guide folder for a project without one.
        let empty = "ga-host-empty";
        for (route, body) in [
            (APPROVE, format!("project={empty}&slug=feature-x")),
            (EDIT, format!("project={empty}&slug=feature-x&part=what&body=Hi")),
            (DELETE, format!("project={empty}&slug=feature-x")),
            (RESTORE, format!("project={empty}&slug=feature-x&snapshot=feature-x.20260101T000000000000000Z.md")),
        ] {
            assert_eq!(sb.post_to(route, &body), page_back(empty, "feature-x", "bad-input"), "{route}");
        }
        assert!(!loopx_core::project_guide::guide_dir(empty, Some(&sb.root())).exists());
        // A hostile history value shows the fixed line and is not echoed.
        let html = sb.get(&format!("{}&history=..%2F%3Cscript%3E", guide_url(alias, None)));
        assert!(html.contains("This page does not exist.") && !html.contains("&lt;script&gt;") && !html.contains("../"), "{html}");
        let html = sb.get(&format!("{}&history=feature-ghost", guide_url(alias, None)));
        assert!(html.contains("This page does not exist.") && !html.contains(&format!("action='{RESTORE}'")), "{html}");
    }

    #[test]
    fn every_guide_post_route_requires_csrf_and_a_busy_guide_refuses_changes() {
        for route in [APPROVE, APPROVE_ALL, EDIT, DELETE, RESTORE] {
            assert!(ROUTES.contains(&route), "{route} is in the CSRF list");
        }
        let alias = "ga-busy";
        let sb = Sb::new(&[(alias, true)]);
        put_page(&sb, alias, "feature-orders", "Orders", Status::Draft, &[("what", "Old text.", Origin::Agent)]);
        let loc = sb.post_to(EDIT, &format!("project={alias}&slug=feature-orders&part=what&body=Second."));
        assert_eq!(loc, page_back(alias, "feature-orders", "guide-edited"));
        let snap = gd_history(&sb, alias, "feature-orders")[0].clone();
        let file = gd_file(&sb, alias, "feature-orders");
        let bytes = std::fs::read(&file).unwrap();
        // No CSRF token: refused before anything runs.
        for route in [APPROVE, APPROVE_ALL, EDIT, DELETE, RESTORE] {
            let req = crate::ui::routes::UiRequest::new(Method::POST, route)
                .with_header("content-type", "application/x-www-form-urlencoded")
                .with_body(format!("project={alias}&slug=feature-orders&part=what&body=X&snapshot={snap}"));
            assert_eq!(sb.state.registry.dispatch(&sb.state, &req).unwrap().status, 403, "{route}");
        }
        assert_eq!(std::fs::read(&file).unwrap(), bytes);

        let slot = loopx_core::guide_build::try_acquire(alias).unwrap();
        let busy_page = page_back(alias, "feature-orders", "guide-busy");
        for (route, body, want) in [
            (APPROVE, format!("project={alias}&slug=feature-orders"), busy_page.clone()),
            (APPROVE_ALL, format!("project={alias}"), guide_back(alias, "guide-busy")),
            (EDIT, format!("project={alias}&slug=feature-orders&part=what&body=Third."), busy_page.clone()),
            (DELETE, format!("project={alias}&slug=feature-orders"), busy_page.clone()),
            (RESTORE, format!("project={alias}&slug=feature-orders&snapshot={snap}"), busy_page.clone()),
        ] {
            assert_eq!(sb.post_to(route, &body), want, "{route}");
            assert_eq!(std::fs::read(&file).unwrap(), bytes, "{route}");
        }
        assert_eq!(gd_history(&sb, alias, "feature-orders").len(), 1);
        drop(slot);
        assert_eq!(sb.post_to(APPROVE, &format!("project={alias}&slug=feature-orders")), page_back(alias, "feature-orders", "guide-approved"));
    }

    #[test]
    fn the_guide_page_actions_are_translated() {
        let alias = "ga-fr";
        let sb = Sb::new(&[(alias, true)]);
        put_page(&sb, alias, "feature-orders", "Orders", Status::Approved, &[("what", "Old text.", Origin::Agent)]);
        let html = get_fr(&sb, &guide_url(alias, Some("feature-orders")));
        for english in [
            "This page is approved, so agents see your change immediately.",
            "Saving marks this part as written by a person; rebuilds keep it.",
            "Delete page",
            ">History<",
        ] {
            assert!(!html.contains(english), "{english}");
        }
        let html = get_fr(&sb, &history_url(alias, "feature-orders"));
        assert!(!html.contains("No earlier versions of this page are kept."), "{html}");
        for code in ["guide-approved", "guide-edited", "guide-deleted", "guide-restored", "guide-in-plan", "guide-failed"] {
            let html = get_fr(&sb, &guide_back(alias, code));
            assert!(html.contains("class='mem-notice"), "{code}");
        }
        let html = get_fr(&sb, &format!("{}&approved=2&notice=guide-all-approved", guide_url(alias, None)));
        assert!(html.contains('2') && !html.contains("Draft pages approved"), "{html}");
    }

    #[test]
    fn a_part_holds_up_to_20000_characters_like_its_textarea() {
        let alias = "ga-limit";
        let sb = Sb::new(&[(alias, true)]);
        put_page(&sb, alias, "feature-orders", "Orders", Status::Approved, &[("what", "Old text.", Origin::Agent)]);
        let file = gd_file(&sb, alias, "feature-orders");
        let bytes = std::fs::read(&file).unwrap();
        // 3 UTF-8 bytes each: 20,001 of them fit in 64 KB, but not in the textarea.
        let han = "%E6%BC%A2";
        let loc = sb.post_to(EDIT, &format!("project={alias}&slug=feature-orders&part=what&body={}", han.repeat(20_001)));
        assert_eq!(loc, page_back(alias, "feature-orders", "guide-too-long"));
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        let html = sb.get(&loc);
        assert!(html.contains("Nothing was saved: a part can hold up to 20,000 characters. Shorten it and save again."), "{html}");
        assert!(!html.contains("Your text is still in the editor below"), "{html}");
        let loc = sb.post_to(EDIT, &format!("project={alias}&slug=feature-orders&part=what&body={}", han.repeat(20_000)));
        assert_eq!(loc, page_back(alias, "feature-orders", "guide-edited"));
        let what = &gd_load(&sb, alias, "feature-orders").unwrap().parts[0];
        assert_eq!((what.body.chars().count(), what.body.len()), (20_000, 60_000));
    }

    #[test]
    fn a_secret_elsewhere_on_the_page_is_not_blamed_on_the_edit() {
        let alias = "ga-elsewhere";
        let sb = Sb::new(&[(alias, true)]);
        put_page(
            &sb,
            alias,
            "feature-orders",
            "Orders",
            Status::Draft,
            &[("what", "Plain words.", Origin::Agent), ("how", "Body text here.", Origin::Agent)],
        );
        let token = ["glpat-", "abcDEF123456ghiJKL78"].concat();
        hand_edit(&sb, alias, "Body text here.", &format!("Body text here {token}."));
        let file = gd_file(&sb, alias, "feature-orders");
        let bytes = std::fs::read(&file).unwrap();
        // A clean text for another part: the page itself is what holds the secret.
        let loc = sb.post_to(EDIT, &format!("project={alias}&slug=feature-orders&part=what&body=Clean."));
        assert_eq!(loc, page_back(alias, "feature-orders", "guide-secret-elsewhere"));
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        let html = sb.get(&loc);
        assert!(html.contains("Nothing was saved: another part of this page looks like it contains a secret value. Remove it from the page file, or delete the page and rebuild it."), "{html}");
        assert!(!html.contains("glpat"), "{html}");
        // A text that itself holds a secret is still blamed on the text.
        let loc = sb.post_to(EDIT, &format!("project={alias}&slug=feature-orders&part=what&body=Use+{token}"));
        assert_eq!(loc, page_back(alias, "feature-orders", "secret"));
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
    }

    #[test]
    fn the_editors_cannot_save_while_a_build_runs() {
        let alias = "ga-running";
        let sb = Sb::new(&[(alias, true)]);
        put_page(&sb, alias, "feature-orders", "Orders", Status::Approved, &[("what", "Old text.", Origin::Agent), ("how", "Queues.", Origin::Agent)]);
        let note = "A build is running, so this page cannot be changed right now.";
        let editors = |html: &str| -> Vec<String> {
            gd_main(html)
                .split("<details class='gd-edit'>")
                .skip(1)
                .map(|e| e[..e.find("</details>").unwrap()].to_string())
                .collect()
        };
        let slot = loopx_core::guide_build::try_acquire(alias).unwrap();
        let html = sb.get(&guide_url(alias, Some("feature-orders")));
        let list = editors(&html);
        assert_eq!(list.len(), 2, "{html}");
        for e in &list {
            assert!(e.contains("type='submit' disabled aria-disabled='true'"), "{e}");
            assert!(e.contains(note), "{e}");
        }
        drop(slot);
        let html = sb.get(&guide_url(alias, Some("feature-orders")));
        for e in editors(&html) {
            assert!(!e.contains("disabled") && !e.contains(note), "{e}");
        }
    }

    #[test]
    fn approve_all_keeps_the_count_when_a_page_cannot_be_approved() {
        let alias = "ga-partial";
        let sb = Sb::new(&[(alias, true)]);
        put_page(&sb, alias, "feature-a", "A", Status::Draft, &[("what", "Text.", Origin::Agent)]);
        put_page(&sb, alias, "feature-b", "B", Status::Draft, &[("what", "Text.", Origin::Agent)]);
        std::fs::write(gd_file(&sb, alias, "feature-x"), "not a page").unwrap();
        let loc = sb.post_to(APPROVE_ALL, &format!("project={alias}"));
        assert_eq!(loc, format!("/insights?view=memory&project={alias}&page=guide&approved=2&notice=guide-failed"));
        for slug in ["feature-a", "feature-b"] {
            assert_eq!(gd_load(&sb, alias, slug).unwrap().status, Status::Approved, "{slug}");
        }
        let html = sb.get(&loc);
        assert!(html.contains("Approved 2 pages; some pages could not be read or saved."), "{html}");
        // The count is read back only as a small number; anything else gets the plain text.
        let generic = "The guide could not be changed: a page file could not be read or saved. Reload the page and try again.";
        for bad in ["1001", "99999999999999999999", "abc", "%3Cscript%3E", "-1"] {
            let html = sb.get(&format!("{}&approved={bad}&notice=guide-failed", guide_url(alias, None)));
            assert!(html.contains(generic), "{bad}");
            assert!(!html.contains("Approved 1001") && !html.contains("&lt;script&gt;") && !html.contains("abc pages"), "{bad}");
        }
        let html = sb.get(&format!("{}&approved=5000&notice=guide-all-approved", guide_url(alias, None)));
        assert!(html.contains("Every draft page was approved."), "{html}");
    }
}
