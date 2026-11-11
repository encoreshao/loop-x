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
use loopx_core::profile_graph::{self, EdgeKind};
use loopx_core::profile_build;
use loopx_core::profile_sync::{self, Freshness, Outcome, SyncStore, TrackedProject};
use loopx_core::project_profile::{self, contains_secret, Origin, Profile, Section, SectionKey, Status};
use loopx_core::{approvals, learning, memory_store, profile_scan};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const BASE: &str = "/insights?view=memory";

/// The sub-page slugs, in sub-nav order. `activity` holds Recent work and People.
pub const PAGES: [&str; 11] = [
    "overview",
    "structure",
    "features",
    "stack",
    "integrations",
    "secrets",
    "conventions",
    "gotchas",
    "activity",
    "lessons",
    "proposals",
];

pub fn register(r: &mut Registry) {
    r.post("/insights/memory/build", build);
    r.post("/insights/memory/section/save", save_section);
    r.post("/insights/memory/section/refresh", refresh_section);
    r.post("/insights/memory/proposal/decide", decide);
    r.post("/insights/memory/sync/settings", sync_settings);
    r.post("/insights/memory/sync/now", sync_now);
}

pub fn render(ctx: &UiCtx) -> String {
    let q = ctx.query();
    let m = model(ctx);
    let body = match q.first("project") {
        None => overview(ctx, &m, q.first("graph") == Some("1")),
        Some(raw) => match m.projects.iter().find(|p| p.alias == raw) {
            None => unknown_project(ctx),
            Some(p) => match q.first("page") {
                None => project_page(ctx, p, &m),
                Some(page) => match PAGES.iter().find(|s| **s == page) {
                    Some(slug) => sub_page(ctx, p, slug, &m),
                    None => frame(ctx, p, None, &unknown_page(ctx, p), &m),
                },
            },
        },
    };
    format!("{STYLE}{body}")
}

// ------------------------------------------------------------------ the data

/// Everything one request needs, read once: each project with its parsed profile
/// and freshness inputs, and the profile proposals.
struct Model {
    projects: Vec<Project>,
    proposals: Vec<Value>,
    /// The banner for a whitelisted `notice` code, or empty.
    notice: String,
}

struct Project {
    alias: String,
    instance: String,
    project_id: String,
    loaded: Loaded,
    /// A tracked project whose checkout exists: Build and Refresh can scan it.
    checkout: bool,
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
    }
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
            let checkout = work_dir.as_ref().is_some_and(|w| !w.as_os_str().is_empty() && w.is_dir());
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

/// Directly linked projects per alias (profile-derived edges only; the issue
/// digests the full graph adds come with the Graph view).
fn neighbors(projects: &[Project]) -> BTreeMap<String, Vec<(String, EdgeKind)>> {
    let profiles: Vec<Profile> = projects
        .iter()
        .filter_map(|p| match &p.loaded {
            Loaded::Ready(profile) => Some(profile.clone()),
            _ => None,
        })
        .collect();
    let aliases: Vec<String> = projects.iter().map(|p| p.alias.clone()).collect();
    let graph = profile_graph::derive(&profiles, &BTreeMap::new(), &aliases);
    let mut out: BTreeMap<String, Vec<(String, EdgeKind)>> = BTreeMap::new();
    for e in graph.edges {
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

fn project_url(alias: &str, page: Option<&str>) -> String {
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
        format!(
            "<section class='card mem-graph-soon'><div class='section-header'>{}<h2>{}</h2></div><p class='mem-muted'>{}</p></section>",
            icon("hub"),
            escape(&ctx.t("Project graph")),
            escape(&ctx.t("The graph of how your projects connect is coming soon. Each project page already lists its linked projects."))
        )
    } else {
        let all = &m.proposals;
        let links = neighbors(projects);
        let status = store.as_ref().ok();
        let cards: String = projects
            .iter()
            .map(|p| project_card(ctx, p, all, &links, status))
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
    let spans: Vec<String> = parts
        .iter()
        .map(|p| format!("<span>{}</span>", escape(p)))
        .collect();
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

fn project_card(
    ctx: &UiCtx,
    p: &Project,
    all: &[Value],
    links: &BTreeMap<String, Vec<(String, EdgeKind)>>,
    status: Option<&SyncStore>,
) -> String {
    let total = SectionKey::ALL.len();
    let (ring_class, good, state_html, line, draft) = match &p.loaded {
        Loaded::Missing => ("none", 0, pill("none", &ctx.t("No profile")), ctx.t("No profile yet"), false),
        Loaded::Unreadable(_) => (
            "bad",
            0,
            pill("bad", &ctx.t("Unreadable")),
            ctx.t("The profile file could not be read."),
            false,
        ),
        Loaded::Ready(profile) => {
            let check = &p.check;
            let states: Vec<State> = SectionKey::ALL
                .into_iter()
                .map(|k| state_of(profile, k, check).0)
                .collect();
            let worst = states.iter().copied().max().unwrap_or(State::Fresh);
            let good = states
                .iter()
                .filter(|s| matches!(s, State::Fresh | State::Human))
                .count();
            let line = profile
                .sections
                .get(&SectionKey::Overview)
                .and_then(safe_summary)
                .map_or_else(|| ctx.t("No overview yet"), str::to_string);
            let ring = match worst {
                State::Stale => "stale",
                State::Review => "review",
                _ => "fresh",
            };
            (ring, good, state_pill(ctx, worst), line, profile.status == Status::Draft)
        }
    };
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
    if profile_build::is_running(&p.alias) {
        chips.push(chip(" mem-chip-accent", "autorenew", &ctx.t("Working…"), None));
    }
    if draft {
        chips.push(chip(" mem-chip-warn", "edit_note", &ctx.t("Draft"), None));
    }
    if let Some(Outcome::Failed(msg)) = status
        .and_then(|s| s.status.per_project.get(&p.alias))
        .map(|s| &s.outcome)
    {
        chips.push(chip(" mem-chip-bad", "error", &ctx.t("Last sync failed"), Some(msg)));
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
    format!(
        "<a class='card mem-card' href='{}'><div class='mem-card-top'>{ring}<div class='mem-card-name'><h3>{}</h3>{state_html}</div></div>\
         <p class='mem-card-overview'>{}</p><div class='mem-chips'>{}</div></a>",
        escape(&project_url(&p.alias, None)),
        escape(&p.alias),
        escape(&line),
        chips.concat()
    )
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
            let head = head_card(ctx, p, None);
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
                    "<div class='mem-banner' role='status'>{}<p><strong>{}</strong> {}</p></div>",
                    icon("edit_note"),
                    escape(&ctx.t("This profile is a draft.")),
                    escape(&ctx.t("Read each section before agents rely on it."))
                )
            } else {
                String::new()
            };
            let links = neighbors(&m.projects);
            let pending = pending_for(&m.proposals, &p.alias);
            let cards: String = SectionKey::ALL
                .into_iter()
                .map(|k| summary_card(ctx, p, profile, k, check))
                .collect();
            format!(
                "{banner}{}<div class='mem-pair'>{}{}</div><div class='mem-sections'>{cards}</div>",
                head_card(ctx, p, Some((profile, check))),
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
                Outcome::Failed(msg) => ctx.tf("Last sync failed {when}: {error}", &[("when", &ago(ctx, s.at)), ("error", msg)]),
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
        head_actions(ctx, p, profile.is_some())
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
    let status = if a["status"] == "pending" {
        pill("review", &ctx.t("Pending"))
    } else {
        pill("fresh", &ctx.t("Approved"))
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
        "<article class='mem-proposal'><div class='mem-proposal-head'><h3>{}</h3>{status}{when}</div>{diff}<div class='mem-actions'>{}</div></article>",
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
}

const NOTICES: [(Notice, &str); 24] = [
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
        )
    }

    fn message(self, ctx: &UiCtx) -> String {
        match self {
            Notice::BuildStarted => ctx.t("Building the profile in the background. It takes a few minutes; reload this page to see it."),
            Notice::BuildAllStarted => ctx.t("Building every tracked project's profile in the background, one after another."),
            Notice::Busy => ctx.t("A profile run for this project is already in progress. Try again when it finishes."),
            Notice::NoCheckout => ctx.t("This project has no checkout to scan. Set its local path on the Projects page first."),
            Notice::NothingToBuild => ctx.t("No tracked project has a checkout to build a profile from."),
            Notice::NoAgent => ctx.t("This dashboard does not start AI runs, so nothing was started."),
            Notice::Saved => ctx.t("Section saved. It is now marked as written by a person."),
            Notice::Secret => ctx.t("Not saved: the text looks like it contains a secret value. Remove it and save again."),
            Notice::TooLong => ctx.t("Not saved: a summary can be up to 300 characters and the details up to 20,000."),
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
        }
    }
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
}

fn back(to: Back<'_>, notice: Notice) -> UiResponse {
    let base = match to {
        Back::Overview => BASE.to_string(),
        Back::Project(alias, page) => project_url(alias, page),
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

fn checkout_of(ctx: &UiCtx, alias: &str) -> Option<TrackedProject> {
    tracked_list(ctx)
        .into_iter()
        .find(|t| t.alias == alias && !t.work_dir.as_os_str().is_empty() && t.work_dir.is_dir())
}

fn store_path(ctx: &UiCtx) -> PathBuf {
    ctx.state.home_dir.join("profile_sync.json")
}

/// Hand edits and approvals in this process take turns, so an approval's
/// "is the section still what the proposal replaces" check cannot race a save.
static WRITES: Mutex<()> = Mutex::new(());

// ---- background jobs

/// The last finished build or refresh per (home, alias), for the page to show.
/// In memory only: a restart forgets it (the profile and proposals are on disk).
#[derive(Clone)]
enum JobKind {
    Build,
    Refresh(SectionKey),
}

#[derive(Clone)]
enum JobOutcome {
    Built,
    Proposed,
    NothingNew,
    Failed(String),
}

#[derive(Clone)]
struct Job {
    at: DateTime<Utc>,
    kind: JobKind,
    outcome: JobOutcome,
}

type JobKey = (PathBuf, String);
static JOBS: Mutex<Vec<(JobKey, Job)>> = Mutex::new(Vec::new());
/// The homes with a "Build all profiles" pass running.
static BUILD_ALL: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn record(home: &Path, alias: &str, job: Job) {
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    jobs.retain(|((h, a), _)| !(h == home && a == alias));
    jobs.push(((home.to_path_buf(), alias.to_string()), job));
}

fn last_job(home: &Path, alias: &str) -> Option<Job> {
    let jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    jobs.iter()
        .find(|((h, a), _)| h == home && a == alias)
        .map(|(_, j)| j.clone())
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

/// One line, bounded: what a failed job shows.
fn failure_line(e: &str) -> String {
    e.lines().next().unwrap_or("").trim().chars().take(200).collect()
}

/// Everything a background job needs, owned (the request is gone by then).
struct JobEnv {
    home: PathBuf,
    root: PathBuf,
    approvals: PathBuf,
    fixed_now: Option<DateTime<Utc>>,
    agent: ProfileAgent,
}

impl JobEnv {
    fn now(&self) -> DateTime<Utc> {
        self.fixed_now.unwrap_or_else(Utc::now)
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
        fixed_now: ctx.state.fixed_now,
        agent,
    })
}

fn run_build(env: &JobEnv, t: &TrackedProject, aliases: &[String]) {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        profile_build::build_profile(
            &t.alias,
            &t.work_dir,
            &t.instance,
            &t.project_id,
            aliases,
            &*env.agent,
            env.now(),
            Some(&env.root),
        )
    }))
    .unwrap_or_else(|_| Err("internal error".into()));
    let outcome = match res {
        Ok(_) => JobOutcome::Built,
        Err(e) => JobOutcome::Failed(failure_line(&e)),
    };
    record(&env.home, &t.alias, Job { at: env.now(), kind: JobKind::Build, outcome });
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
    std::thread::spawn(move || {
        let _slot = slot;
        run_build(&env, &t, &aliases);
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
        .filter(|t| !t.work_dir.as_os_str().is_empty() && t.work_dir.is_dir())
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
        for t in &list {
            let Some(_slot) = profile_build::try_acquire(&t.alias) else {
                continue;
            };
            run_build(&env, t, &aliases);
        }
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
    let summary = form.first("summary").unwrap_or("");
    let body = form.first("body").unwrap_or("");
    if summary.chars().count() > SUMMARY_MAX || body.chars().count() > BODY_MAX {
        return back(to(), Notice::TooLong);
    }
    // The profile's own save does not reject secrets; a person's paste must not
    // land in a file agents read.
    if contains_secret(summary).is_some() || contains_secret(body).is_some() {
        return back(to(), Notice::Secret);
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
        Ok(_) => back(to(), Notice::Saved),
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
    std::thread::spawn(move || {
        let _slot = slot;
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            profile_sync::propose_refresh_held(
                &t.alias,
                key,
                &t.work_dir,
                &*env.agent,
                env.now(),
                Some(&env.root),
                &env.approvals,
            )
        }))
        .unwrap_or_else(|_| Err("internal error".into()));
        let outcome = match res {
            Ok(Some(_)) => JobOutcome::Proposed,
            Ok(None) => JobOutcome::NothingNew,
            Err(e) => JobOutcome::Failed(failure_line(&e)),
        };
        record(&env.home, &t.alias, Job { at: env.now(), kind: JobKind::Refresh(key), outcome });
    });
    back(to(), Notice::RefreshStarted)
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

/// Runs once on the next server tick, whether or not the weekly switch is on.
fn sync_now(ctx: &UiCtx) -> UiResponse {
    match profile_sync::with_store(&store_path(ctx), |s| s.sync_requested = true) {
        Ok(()) => back(Back::Overview, Notice::SyncRequested),
        Err(_) => back(Back::Overview, Notice::StoreBad),
    }
}

// ---- controls

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
    }
    let build = if build_all_running(&ctx.state.home_dir) {
        format!(
            "<button class='mem-btn mem-btn-primary' type='button' disabled>{}{}</button>",
            spinner(),
            escape(&ctx.t("Building profiles…"))
        )
    } else {
        format!(
            "<button class='mem-btn mem-btn-primary' type='submit'>{}{}</button>",
            icon("auto_awesome"),
            escape(&ctx.t("Build all profiles"))
        )
    };
    out.push_str(&format!(
        "<form method='post' action='/insights/memory/build' class='mem-inline'>{csrf}{}{}{build}</form>",
        hidden("project", "all"),
        hidden("from", "overview")
    ));
    out
}

/// The project header's Build (no profile yet) or Refresh (rebuild) button.
fn head_actions(ctx: &UiCtx, p: &Project, built: bool) -> String {
    if !p.checkout {
        return format!(
            "<p class='mem-muted'>{}</p>",
            escape(&ctx.t("Set this project's local checkout on the Projects page to build its profile."))
        );
    }
    if profile_build::is_running(&p.alias) {
        return format!(
            "<button class='mem-btn mem-btn-primary' type='button' disabled>{}{}</button>",
            spinner(),
            escape(&ctx.t("Working…"))
        );
    }
    let (icon_name, label) = if built {
        ("autorenew", ctx.t("Refresh profile"))
    } else {
        ("auto_awesome", ctx.t("Build profile"))
    };
    format!(
        "<form method='post' action='/insights/memory/build' class='mem-inline'>{}{}{}\
         <button class='mem-btn mem-btn-primary' type='submit'>{}{}</button></form>\
         <p class='mem-muted mem-hint'>{}</p>",
        ctx.csrf_input(),
        hidden("project", &p.alias),
        hidden("from", "project"),
        icon(icon_name),
        escape(&label),
        escape(&ctx.t("Scans the checkout and asks the AI CLI for a draft. Sections written by a person are kept."))
    )
}

/// "Working…" while a run holds the project, else the last build or refresh result.
fn job_line(ctx: &UiCtx, alias: &str) -> String {
    if profile_build::is_running(alias) {
        return format!(
            "<div class='mem-job' role='status'>{}<p>{}</p></div>",
            spinner(),
            escape(&ctx.t("Working on this profile… This takes a few minutes; reload the page to see the result."))
        );
    }
    let Some(job) = last_job(&ctx.state.home_dir, alias) else {
        return String::new();
    };
    let when = ago(ctx, job.at);
    let section = |k: SectionKey| section_title(ctx, k);
    let error = |e: &str| match contains_secret(e) {
        Some(_) => ctx.t("details hidden"),
        None => e.to_string(),
    };
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
    };
    format!(
        "<div class='mem-job{}' role='status'>{}<p>{}</p></div>",
        if ok { "" } else { " mem-job-bad" },
        icon(if ok { "check_circle" } else { "error" }),
        escape(&text)
    )
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
    } else if profile_build::is_running(&p.alias) {
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
    let (summary, body) = s.map_or(("", ""), |s| (s.summary.as_str(), s.body.as_str()));
    format!(
        "<details class='mem-edit'><summary class='mem-btn'>{}{}</summary>\
         <form method='post' action='/insights/memory/section/save' class='mem-editor'>{csrf}{}{}\
         <label class='mem-label' for='mem-sum-{ids}'>{}</label>\
         <input class='mem-input' id='mem-sum-{ids}' name='summary' maxlength='{SUMMARY_MAX}' value='{}'>\
         <label class='mem-label' for='mem-body-{ids}'>{}</label>\
         <textarea class='mem-textarea' id='mem-body-{ids}' name='body' maxlength='{BODY_MAX}'>{}</textarea>\
         <div class='mem-editor-foot'><p class='mem-muted'>{}</p>\
         <button class='mem-btn mem-btn-primary' type='submit'>{}{}</button></div></form></details>{refresh}",
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
            let t = &a["target"];
            let applied = profile.and_then(|p| p.sections.get(&key)).is_some_and(|cur| {
                cur.body == t["new"].as_str().unwrap_or("") && cur.summary == t["new_summary"].as_str().unwrap_or("")
            });
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
@keyframes mem-spin{to{transform:rotate(360deg)}}
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
    use std::path::PathBuf;

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
    fn graph_toggle_renders_a_placeholder() {
        let sb = Sb::new(&[("alpha", true)]);
        let html = sb.get("/insights?view=memory&graph=1");
        assert!(html.contains("mem-graph-soon"), "{html}");
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

    const ROUTES: [&str; 6] = [
        "/insights/memory/build",
        "/insights/memory/section/save",
        "/insights/memory/section/refresh",
        "/insights/memory/proposal/decide",
        "/insights/memory/sync/settings",
        "/insights/memory/sync/now",
    ];

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
        profile_sync::propose_refresh(alias, key, &work, &*agent, now(), Some(&sb.root()), &sb.approvals())
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
}
