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
//! a secret value is replaced by a notice naming only the kind of secret. The
//! `.mem-actions` containers are where Part B's controls (build, edit, refresh,
//! approve) go; this module registers no POST route yet.

use super::insights::{f_of, fixed, icon as ds_icon, n_of, truthy};
use crate::ui::icons::icon;
use crate::ui::markdown::{self, GitlabPrefixes};
use crate::ui::pyfmt::{escape, escape_text, quote};
use crate::ui::routes::{Registry, UiCtx};
use chrono::{DateTime, Duration, Local, Utc};
use loopx_core::profile_graph::{self, EdgeKind};
use loopx_core::profile_sync::{self, Freshness, Outcome, SyncStore};
use loopx_core::project_profile::{self, contains_secret, Origin, Profile, Section, SectionKey, Status};
use loopx_core::{approvals, learning, memory_store, profile_scan};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

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

/// Part B registers the POST routes (build, section save/refresh, proposal decide,
/// sync settings, sync now) here.
pub fn register(_r: &mut Registry) {}

pub fn render(ctx: &UiCtx) -> String {
    let q = ctx.query();
    let projects = projects(ctx);
    let body = match q.first("project") {
        None => overview(ctx, &projects, q.first("graph") == Some("1")),
        Some(raw) => match projects.iter().find(|p| p.alias == raw) {
            None => unknown_project(ctx),
            Some(p) => match q.first("page") {
                None => project_page(ctx, p, &projects),
                Some(page) => match PAGES.iter().find(|s| **s == page) {
                    Some(slug) => sub_page(ctx, p, slug),
                    None => frame(ctx, p, None, &unknown_page(ctx, p)),
                },
            },
        },
    };
    format!("{STYLE}{body}")
}

// ------------------------------------------------------------------ the data

struct Project {
    alias: String,
    instance: String,
    project_id: String,
    /// The checkout, when the project changes code.
    work_dir: Option<PathBuf>,
}

fn memory_root(ctx: &UiCtx) -> PathBuf {
    ctx.state.home_dir.join("memory")
}

fn approvals_dir(ctx: &UiCtx) -> PathBuf {
    ctx.state.repo_root.join("outputs").join("approvals")
}

/// Every tracked (code-changing) project of `projects.json`, then every other alias
/// with a memory directory (a profile, lessons or setup notes), sorted.
fn projects(ctx: &UiCtx) -> Vec<Project> {
    let config_path = ctx.state.home_dir.join("projects.json");
    let mut out: Vec<Project> = profile_sync::tracked_projects_in(&config_path)
        .into_iter()
        .map(|t| Project {
            alias: t.alias,
            instance: t.instance,
            project_id: t.project_id,
            work_dir: Some(t.work_dir),
        })
        .collect();
    let config = loopx_core::config::Config::load(&config_path).ok();
    for alias in memory_store::all_aliases(Some(&memory_root(ctx))) {
        if alias.starts_with('.') || out.iter().any(|p| p.alias == alias) {
            continue;
        }
        let entry = config.as_ref().and_then(|c| c.project(&alias).ok());
        let text = |k: &str| {
            entry
                .as_ref()
                .and_then(|e| e.get(k))
                .map(n_of)
                .filter(|s| s != "None")
                .unwrap_or_default()
        };
        out.push(Project {
            instance: text("instance"),
            project_id: text("project_id"),
            work_dir: None,
            alias,
        });
    }
    out
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

fn check_for(p: &Project) -> Check {
    let work_dir = p
        .work_dir
        .clone()
        .filter(|w| !w.as_os_str().is_empty() && w.is_dir());
    let head = work_dir.as_deref().and_then(profile_scan::head_sha);
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
/// `profile-section` proposals, newest first.
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
fn neighbors(ctx: &UiCtx, projects: &[Project]) -> BTreeMap<String, Vec<(String, EdgeKind)>> {
    let profiles: Vec<Profile> = projects
        .iter()
        .filter_map(|p| match load(ctx, &p.alias) {
            Loaded::Ready(profile) => Some(profile),
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

fn overview(ctx: &UiCtx, projects: &[Project], graph: bool) -> String {
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
        "<div class='mem-bar'>{}{}<div class='mem-actions'></div></div>",
        segmented(ctx, graph),
        sync_line(ctx, &store)
    );
    let main = if graph {
        format!(
            "<section class='card mem-graph-soon'><div class='section-header'>{}<h2>{}</h2></div><p class='mem-muted'>{}</p></section>",
            icon("hub"),
            escape(&ctx.t("Project graph")),
            escape(&ctx.t("The graph of how your projects connect is coming soon. Each project page already lists its linked projects."))
        )
    } else {
        let all = proposals(ctx);
        let links = neighbors(ctx, projects);
        let status = store.as_ref().ok();
        let cards: String = projects
            .iter()
            .map(|p| project_card(ctx, p, &all, &links, status))
            .collect();
        format!("<div class='mem-grid'>{cards}</div>")
    };
    format!("{title}<div class='mem-stack'>{bar}{main}</div>")
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
    let (ring_class, good, state_html, line, draft) = match load(ctx, &p.alias) {
        Loaded::Missing => ("none", 0, pill("none", &ctx.t("No profile")), ctx.t("No profile yet"), false),
        Loaded::Unreadable(_) => (
            "bad",
            0,
            pill("bad", &ctx.t("Unreadable")),
            ctx.t("The profile file could not be read."),
            false,
        ),
        Loaded::Ready(profile) => {
            let check = check_for(p);
            let states: Vec<State> = SectionKey::ALL
                .into_iter()
                .map(|k| state_of(&profile, k, &check).0)
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
    if draft {
        chips.push(chip(" mem-chip-warn", "edit_note", &ctx.t("Draft"), None));
    }
    if let Some(Outcome::Failed(msg)) = status
        .and_then(|s| s.status.per_project.get(&p.alias))
        .map(|s| &s.outcome)
    {
        chips.push(chip(" mem-chip-bad", "error", &ctx.t("Last sync failed"), Some(msg)));
    }
    let proposals_label = if pending == 1 {
        ctx.t("1 pending proposal")
    } else {
        ctx.tf("{n} pending proposals", &[("n", &pending.to_string())])
    };
    let accent = if pending > 0 { " mem-chip-accent" } else { "" };
    chips.push(chip(accent, "fact_check", &proposals_label, None));
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
fn frame(ctx: &UiCtx, p: &Project, page: Option<&str>, content: &str) -> String {
    let pending = pending_for(&proposals(ctx), &p.alias);
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
        "{}<div class=\"page-title\">\n<h1>{}</h1>{subtitle}\n</div>\n<div class='mem-layout'>{}<div class='mem-content'>{content}</div></div>",
        crumbs(ctx, Some(&p.alias), page),
        escape(&p.alias),
        subnav(ctx, p, page, pending)
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

fn project_page(ctx: &UiCtx, p: &Project, projects: &[Project]) -> String {
    let content = match load(ctx, &p.alias) {
        Loaded::Unreadable(e) => error_card(ctx, &e),
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
            let check = check_for(p);
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
            let links = neighbors(ctx, projects);
            let pending = pending_for(&proposals(ctx), &p.alias);
            let cards: String = SectionKey::ALL
                .into_iter()
                .map(|k| summary_card(ctx, p, &profile, k, &check))
                .collect();
            format!(
                "{banner}{}<div class='mem-pair'>{}{}</div><div class='mem-sections'>{cards}</div>",
                head_card(ctx, p, Some((&profile, &check))),
                neighbors_card(ctx, links.get(&p.alias)),
                proposals_summary(ctx, p, pending)
            )
        }
    };
    frame(ctx, p, None, &content)
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
    format!("<section class='card mem-head'><dl>{dl}</dl><div class='mem-actions'></div></section>")
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

fn sub_page(ctx: &UiCtx, p: &Project, slug: &str) -> String {
    let content = match slug {
        "lessons" => lessons_card(ctx, p),
        "proposals" => proposals_card(ctx, p),
        _ => match load(ctx, &p.alias) {
            Loaded::Unreadable(e) => error_card(ctx, &e),
            Loaded::Missing => empty_card(&ctx.t("This project has no profile yet, so this section has not been written."),
                Some((&project_url(&p.alias, None), ctx.t("Project summary"))),
            ),
            Loaded::Ready(profile) => {
                let check = check_for(p);
                let mut out: String = sections_of(slug)
                    .into_iter()
                    .map(|k| detail_card(ctx, &profile, k, &check))
                    .collect();
                if slug == "stack" {
                    out.push_str(&setup_card(ctx, p));
                }
                out
            }
        },
    };
    frame(ctx, p, Some(slug), &content)
}

fn detail_card(ctx: &UiCtx, profile: &Profile, key: SectionKey, check: &Check) -> String {
    let title = section_title(ctx, key);
    let (state, stale) = state_of(profile, key, check);
    let head = format!(
        "<div class='mem-detail-head'><h2>{}</h2>{}</div>",
        escape(&title),
        state_pill(ctx, state)
    );
    let Some(s) = profile.sections.get(&key) else {
        return format!(
            "<section class='card mem-detail'>{head}<div class='mem-actions'></div><p class='mem-muted'>{}</p></section>",
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
            "<section class='card mem-detail'>{head}{meta}<div class='mem-actions'></div>{}</section>",
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
        "<section class='card mem-detail'>{head}{meta}<div class='mem-actions'></div>{stale_html}{lede}{body}{sources}</section>"
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

fn proposals_card(ctx: &UiCtx, p: &Project) -> String {
    let mut items: Vec<Value> = proposals(ctx)
        .into_iter()
        .filter(|a| proposal_alias(a) == p.alias)
        .collect();
    // Pending first; each group stays newest first.
    items.sort_by_key(|a| a["status"] != "pending");
    let list: String = items.iter().map(|a| proposal_item(ctx, a)).collect();
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

fn proposal_item(ctx: &UiCtx, a: &Value) -> String {
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
        "<article class='mem-proposal'><div class='mem-proposal-head'><h3>{}</h3>{status}{when}</div>{diff}<div class='mem-actions'></div></article>",
        escape(&title)
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
        let category = if truthy(&e["category"]) {
            format!("<span class='pill pill-grey'>{}</span>", escape(&n_of(&e["category"])))
        } else {
            String::new()
        };
        let meta = format!(
            "<div class='pill-row'>{}{category}{}</div>",
            issue_pill(&prefixes, alias, &e["issue_iid"]),
            tag_pills(&e["tags"])
        );
        let description = &e["description"];
        let description_html = if truthy(description) {
            format!("<p class='history-entry-overview'>{}</p>", escape(&n_of(description)))
        } else {
            String::new()
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
            "<li class='learning-item'{data_attr}>{badge}{description_html}<div class='markdown'>{}</div>{meta}{}</li>",
            markdown::render_markdown(e["body"].as_str().unwrap_or(""), &prefixes),
            reuse_stats(&e["lesson_id"])
        )
    };
    let legacy_item = |e: &Value| -> String {
        let pill = match e.get("issue_iid") {
            Some(iid) if !iid.is_null() => issue_pill(&prefixes, alias, iid),
            _ => String::new(),
        };
        let meta = format!("{pill}{}", tag_pills(&e["tags"]));
        let meta_html = if meta.is_empty() {
            String::new()
        } else {
            format!("<div class='pill-row'>{meta}</div>")
        };
        format!(
            "<li class='learning-item'><div class='markdown'>{}</div>{meta_html}</li>",
            markdown::render_markdown(e["lesson"].as_str().unwrap_or(""), &prefixes)
        )
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
        assert!(!html.contains("project=issues-only"), "issues-only project without memory is not listed");
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
                timeout_seconds: 3600,
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
        assert!(html.contains("<div class='mem-actions'></div>"));
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
        assert!(html.contains("Not yet reused") || html.contains("learning-reuse-stats") || !html.contains("lesson_id"));
        // The overview card counts the lessons.
        let overview = sb.get("/insights?view=memory");
        assert!(overview.contains("2 lessons"), "{overview}");
        // A project with none says so.
        let sb2 = Sb::new(&[("beta", true)]);
        assert!(sb2.get("/insights?view=memory&project=beta&page=lessons").contains("(no task memory recorded yet)"));
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
}
