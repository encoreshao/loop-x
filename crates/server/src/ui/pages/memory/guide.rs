//! Insights → Memory → project → **Guide** (`&page=guide`): the project guide, read-only.
//!
//! A list of the guide's pages on the left (Project: Architecture, Data model, Roadmap;
//! Features in plan order; then pages the current plan no longer names) and the
//! selected page on the right (`&guide=<slug>`, the first page by default).
//!
//! Every query value is untrusted: `guide` must pass `project_guide::check_slug` and
//! name a page that exists, otherwise the list is shown with a fixed "does not exist"
//! line, and the value is never echoed. Page text is escaped and rendered through the
//! dashboard's markdown renderer; a page whose stored text trips the secret guard
//! shows only its title, its status and the redaction notice, and its list row only
//! its title.
//!
//! The page actions (Approve, Approve all drafts, Edit a part, Delete, History and
//! Restore) post to the routes at the end of this file. Each takes a known alias and a
//! slug that passed `check_slug`, calls `guide_ops` (which takes the guide slot and
//! the guide lock), and redirects with checked values and one fixed notice code.

use super::{
    approvals_dir, back, day, escape, guide_running, hidden, icon, known_alias, markdown,
    memory_root, pill, prefixes, project_url, secret_in, short_sha, spinner, valid_id, Back,
    Model, Notice, Project,
};
use crate::ui::routes::{UiCtx, UiResponse};
use chrono::{DateTime, NaiveDateTime, Utc};
use loopx_core::guide_ops::{self, MAX_PART_BYTES};

/// The most a part edited here may hold, in characters (UTF-16 units, what the
/// textarea's `maxlength` counts). At 3 UTF-8 bytes per unit at most, any text the
/// browser accepts stays under the core's 64 KB byte limit, which remains the backstop.
const MAX_PART_CHARS: usize = 20_000;
use loopx_core::guide_plan::{self, Plan};
use loopx_core::project_guide::{self, check_slug, Kind, Page};
use loopx_core::project_profile::{contains_secret, Origin, Status};
use loopx_core::{approvals, guide_proposal, proposals as core_proposals};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

/// The guide page of `alias` showing `slug`. Both are checked values (a known alias,
/// a slug that passed `check_slug`).
pub(super) fn page_url(alias: &str, slug: &str) -> String {
    format!("{}&guide={slug}", project_url(alias, Some("guide")))
}

/// A page's history view: `alias` known, `slug` checked.
fn history_url(alias: &str, slug: &str) -> String {
    format!("{}&history={slug}", project_url(alias, Some("guide")))
}

/// The secret kind a page's stored text trips, if any: the whole rendered file, so a
/// title, summary, part, source path or frontmatter value all count.
fn page_secret(page: &Page) -> Option<String> {
    contains_secret(&project_guide::render(page))
}

/// A title that is safe to show: the page's own, or its (checked) slug when the
/// title itself looks like a secret.
fn shown_title(page: &Page) -> &str {
    if contains_secret(&page.title).is_some() {
        &page.slug
    } else {
        &page.title
    }
}

fn status_pill(ctx: &UiCtx, status: Status) -> String {
    match status {
        Status::Draft => pill("review", &ctx.t("Draft")),
        Status::Approved => pill("fresh", &ctx.t("Approved")),
    }
}

/// The translated heading of a part, from the English heading in `Kind::parts()`.
fn part_heading(ctx: &UiCtx, heading: &str) -> String {
    match heading {
        "What it does" => ctx.t("What it does"),
        "Business value and metrics" => ctx.t("Business value and metrics"),
        "How it works" => ctx.t("How it works"),
        "Associations" => ctx.t("Associations"),
        "Key points" => ctx.t("Key points"),
        "Overview" => ctx.t("Overview"),
        "Components" => ctx.t("Components"),
        "Request and data flows" => ctx.t("Request and data flows"),
        "Runtime and deployment" => ctx.t("Runtime and deployment"),
        "Entities" => ctx.t("Entities"),
        "Relationships" => ctx.t("Relationships"),
        "Rules and invariants" => ctx.t("Rules and invariants"),
        "Recently shipped" => ctx.t("Recently shipped"),
        "In progress" => ctx.t("In progress"),
        "Planned" => ctx.t("Planned"),
        _ => ctx.t("Not scheduled"),
    }
}

/// What the guide folder holds for one project, read once per request.
struct Guide {
    pages: Vec<Page>,
    /// Slugs (checked) whose file did not parse or belongs elsewhere.
    unreadable: Vec<String>,
    plan: Option<Plan>,
    deleted: Vec<String>,
    /// The folder itself could not be listed.
    failed: bool,
}

fn read_guide(alias: &str, root: &Path) -> Guide {
    let (pages, unreadable, failed) = match project_guide::list(alias, Some(root)) {
        Ok(l) => (l.pages, l.skipped, false),
        Err(_) => (Vec::new(), Vec::new(), true),
    };
    Guide {
        pages,
        unreadable,
        plan: guide_plan::load(alias, Some(root)).ok().flatten(),
        deleted: guide_ops::deleted_pages(alias, Some(root)),
        failed,
    }
}

/// One row of the list, in display order.
enum Row<'a> {
    Page {
        page: &'a Page,
        off_plan: bool,
    },
    Unreadable(&'a str),
    /// In the plan, not written yet: (title from the plan, summary from the plan).
    Missing(String, String),
}

impl Row<'_> {
    fn slug(&self) -> Option<&str> {
        match self {
            Row::Page { page, .. } => Some(&page.slug),
            Row::Unreadable(s) => Some(s),
            Row::Missing(..) => None,
        }
    }
}

/// The list in its three groups: Project, Features, Not in the current plan.
fn rows<'a>(alias: &str, root: &Path, g: &'a Guide) -> [Vec<Row<'a>>; 3] {
    // With no plan at all nothing is "outside" it.
    let off_plan = |slug: &str| g.plan.is_some() && !guide_ops::in_plan(alias, slug, Some(root));
    let page_row = |p: &'a Page| Row::Page {
        page: p,
        off_plan: off_plan(&p.slug),
    };
    let (mut project, mut features, mut outside) = (Vec::new(), Vec::new(), Vec::new());
    let fixed = [
        (Kind::Architecture, "architecture"),
        (Kind::DataModel, "data-model"),
        (Kind::Roadmap, "roadmap"),
    ];
    for (kind, slug) in fixed {
        if let Some(p) = g.pages.iter().find(|p| p.slug == slug) {
            project.push(page_row(p));
        } else if g.unreadable.iter().any(|s| s == slug) {
            project.push(Row::Unreadable(slug));
        } else if g.plan.is_some() && guide_ops::in_plan(alias, slug, Some(root)) {
            let title = match kind {
                Kind::Architecture => "Architecture",
                Kind::DataModel => "Data model",
                _ => "Roadmap",
            };
            project.push(Row::Missing(title.to_string(), String::new()));
        }
    }
    let mut seen = BTreeSet::new();
    if let Some(plan) = &g.plan {
        for f in &plan.features {
            seen.insert(f.slug.as_str());
            if let Some(p) = g.pages.iter().find(|p| p.slug == f.slug) {
                features.push(page_row(p));
            } else if g.unreadable.contains(&f.slug) {
                features.push(Row::Unreadable(&f.slug));
            } else {
                features.push(Row::Missing(f.title.clone(), f.summary.clone()));
            }
        }
    }
    for p in g
        .pages
        .iter()
        .filter(|p| p.kind == Kind::Feature && !seen.contains(p.slug.as_str()))
    {
        if g.plan.is_some() {
            outside.push(page_row(p));
        } else {
            features.push(page_row(p));
        }
    }
    for s in &g.unreadable {
        if check_slug(s) == Ok(Kind::Feature) && !seen.contains(s.as_str()) {
            if g.plan.is_some() {
                outside.push(Row::Unreadable(s));
            } else {
                features.push(Row::Unreadable(s));
            }
        }
    }
    [project, features, outside]
}

/// The guide sub-page's content (the caller wraps it in the project frame).
pub(super) fn render(ctx: &UiCtx, p: &Project, _m: &Model) -> String {
    let root = memory_root(ctx);
    let g = read_guide(&p.alias, &root);
    if g.failed {
        return format!(
            "{GUIDE_STYLE}<section class='card gd-empty'><p class='mem-muted'>{}</p></section>",
            escape(&ctx.t("The guide folder could not be read."))
        );
    }
    let query = ctx.query();
    // `history=<slug>`: a checked slug of a page that exists, could not be read, or
    // was deleted (has snapshots but no file). Anything else is "does not exist".
    let history = query.first("history").map(|raw| {
        g.pages
            .iter()
            .map(|x| x.slug.as_str())
            .chain(g.unreadable.iter().map(String::as_str))
            .chain(g.deleted.iter().map(String::as_str))
            .find(|s| check_slug(raw).is_ok() && *s == raw)
    });
    let history_view = |slug: &str| {
        let exists = g.pages.iter().any(|x| x.slug == slug) || g.unreadable.iter().any(|x| x == slug);
        history_view(ctx, p, slug, exists, &root)
    };
    if g.pages.is_empty() && g.unreadable.is_empty() && g.plan.is_none() {
        let main = match history {
            Some(Some(slug)) => history_view(slug),
            Some(None) => muted_page(&ctx.t("This page does not exist.")),
            None => empty_state(ctx, p),
        };
        return format!("{GUIDE_STYLE}{main}{}", deleted_card(ctx, p, &g.deleted));
    }
    let groups = rows(&p.alias, &root, &g);
    let first = groups.iter().flatten().find_map(Row::slug);
    let listed = |raw: &str| {
        groups
            .iter()
            .flatten()
            .filter_map(Row::slug)
            .find(|s| check_slug(raw).is_ok() && *s == raw)
    };
    // The selected slug: a checked `guide` value naming a listed page, or the first page.
    let raw = query.first("guide");
    let selected = match (history, raw) {
        (Some(h), _) => h.and_then(listed),
        (None, None) => first,
        (None, Some(raw)) => listed(raw),
    };
    let shown = selected.and_then(|slug| g.pages.iter().find(|x| x.slug == slug));
    let right = match (history, selected, shown) {
        (Some(Some(slug)), ..) => history_view(slug),
        (Some(None), ..) => muted_page(&ctx.t("This page does not exist.")),
        (None, None, _) if raw.is_some() => muted_page(&ctx.t("This page does not exist.")),
        (None, None, _) => muted_page(&ctx.t("No page has been written yet.")),
        (None, Some(_), Some(page)) => page_view(ctx, p, page, &g.pages, &root),
        (None, Some(_), None) => muted_page(&ctx.t("This page could not be read.")),
    };
    format!(
        "{GUIDE_STYLE}{}<div class='gd-layout'>{}<div class='gd-main'>{right}</div></div>",
        actions(ctx, p, !g.pages.is_empty(), shown.map(|x| x.slug.as_str())),
        list_view(ctx, p, &groups, selected, &g.deleted)
    )
}

/// Whether "Rebuild this page" can rewrite `slug`: a valid slug the saved plan names
/// that a build writes (architecture, the data model when planned, a planned
/// feature). Never `roadmap`: a build does not write it. Used by the button and the
/// route alike.
pub(super) fn rebuildable(alias: &str, slug: &str, root: &Path) -> bool {
    matches!(check_slug(slug), Ok(Kind::Architecture | Kind::DataModel | Kind::Feature))
        && guide_ops::in_plan(alias, slug, Some(root))
}

/// The disabled button a guide button becomes while a guide run works on the project.
fn working(ctx: &UiCtx) -> String {
    format!(
        "<button class='mem-btn mem-btn-primary' type='button' disabled>{}{}</button>",
        spinner(),
        escape(&ctx.t("Working…"))
    )
}

/// One guide action: a CSRF-checked form posting the checked alias (and slug).
fn action_form(ctx: &UiCtx, action: &str, fields: &str, primary: bool, icon_name: &str, label: &str) -> String {
    format!(
        "<form method='post' action='{action}' class='mem-inline'>{}{fields}\
         <button class='mem-btn{}' type='submit'>{}{}</button></form>",
        ctx.csrf_input(),
        if primary { " mem-btn-primary" } else { "" },
        icon(icon_name),
        escape(label)
    )
}

/// The header row over the list: Build guide (no page written yet) or Rebuild guide,
/// and on a page the plan names, Rebuild this page. While a guide run works, one
/// disabled Working… instead. Nothing without a checkout (the route would refuse).
fn actions(ctx: &UiCtx, p: &Project, has_pages: bool, page: Option<&str>) -> String {
    if !p.checkout {
        return String::new();
    }
    let buttons = if guide_running(ctx, &p.alias) {
        working(ctx)
    } else {
        let project = hidden("project", &p.alias);
        let (icon_name, label) = if has_pages {
            ("autorenew", ctx.t("Rebuild guide"))
        } else {
            ("auto_awesome", ctx.t("Build guide"))
        };
        let build = action_form(ctx, "/insights/memory/guide/build", &project, !has_pages, icon_name, &label);
        let root = memory_root(ctx);
        let one = page
            .filter(|slug| rebuildable(&p.alias, slug, &root))
            .map_or_else(String::new, |slug| {
                let fields = format!("{project}{}", hidden("slug", slug));
                action_form(ctx, "/insights/memory/guide/rebuild-page", &fields, false, "autorenew", &ctx.t("Rebuild this page"))
            });
        format!("{one}{build}")
    };
    format!(
        "<div class='gd-actions'><p class='mem-muted mem-hint'>{}</p><div class='gd-actions-buttons'>{buttons}</div></div>",
        escape(&ctx.t("A rebuild writes new drafts. Approved pages are not changed: their updates wait for your approval."))
    )
}

fn muted_page(text: &str) -> String {
    format!(
        "<article class='card gd-page'><p class='mem-muted'>{}</p></article>",
        escape(text)
    )
}

fn empty_state(ctx: &UiCtx, p: &Project) -> String {
    let action =
        if !p.checkout {
            format!(
                "<p class='mem-muted'>{}</p>",
                escape(&ctx.t(
                    "Set this project's local checkout on the Projects page to build its guide."
                ))
            )
        } else if guide_running(ctx, &p.alias) {
            working(ctx)
        } else {
            format!(
                "<form method='post' action='/insights/memory/guide/build' class='gd-build'>{}{}\
             <button class='mem-btn mem-btn-primary' type='submit'>{}{}</button></form>",
                ctx.csrf_input(),
                hidden("project", &p.alias),
                icon("auto_awesome"),
                escape(&ctx.t("Build guide"))
            )
        };
    format!(
        "<section class='card gd-empty'><div class='gd-empty-icon'>{}</div><h2>{}</h2><p>{}</p>{action}</section>",
        icon("menu_book"),
        escape(&ctx.t("No guide yet")),
        escape(&ctx.t("A build reads the project's code and writes about 15 pages: Architecture, Data model and one page per major feature. It makes one AI call per page (about 15) and takes a few minutes. Nothing is used by agents until you approve a page."))
    )
}

fn list_view(
    ctx: &UiCtx,
    p: &Project,
    groups: &[Vec<Row<'_>>; 3],
    selected: Option<&str>,
    deleted: &[String],
) -> String {
    let names = [
        ctx.t("Project"),
        ctx.t("Features"),
        ctx.t("Not in the current plan"),
    ];
    let drafts = groups
        .iter()
        .flatten()
        .any(|r| matches!(r, Row::Page { page, .. } if page.status == Status::Draft));
    let mut out = if drafts {
        format!(
            "<form method='post' action='/insights/memory/guide/approve-all' class='gd-approve-all'>{}{}\
             <button class='mem-btn' type='submit'>{}{}</button></form>",
            ctx.csrf_input(),
            hidden("project", &p.alias),
            icon("task_alt"),
            escape(&ctx.t("Approve all drafts"))
        )
    } else {
        String::new()
    };
    for (name, rows) in names.iter().zip(groups.iter()) {
        if rows.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "<h3 class='gd-group'>{}</h3><div class='gd-rows'>",
            escape(name)
        ));
        for row in rows {
            out.push_str(&row_html(ctx, p, row, selected));
        }
        out.push_str("</div>");
    }
    out.push_str(&deleted_list(ctx, p, deleted));
    format!(
        "<nav class='card gd-list' aria-label='{}'>{out}</nav>",
        escape(&ctx.t("Guide pages"))
    )
}

fn row_html(ctx: &UiCtx, p: &Project, row: &Row<'_>, selected: Option<&str>) -> String {
    let link = |slug: &str, inner: String| {
        let active = selected == Some(slug);
        format!(
            "<a class='gd-row{}' href='{}'{}>{inner}</a>",
            if active { " is-active" } else { "" },
            escape(&page_url(&p.alias, slug)),
            if active { " aria-current='page'" } else { "" }
        )
    };
    match row {
        Row::Page { page, off_plan } => {
            let secret = page_secret(page).is_some();
            let summary = if secret || page.summary.trim().is_empty() {
                String::new()
            } else {
                format!("<span class='gd-row-sum'>{}</span>", escape(&page.summary))
            };
            let marker = if *off_plan {
                format!("<span class='gd-off-plan'>{}</span>", escape(&ctx.t("Not in the current plan")))
            } else {
                String::new()
            };
            link(
                &page.slug,
                format!(
                    "<span class='gd-row-title'>{}</span>{summary}<span class='gd-row-tags'>{}{marker}</span>",
                    escape(shown_title(page)),
                    status_pill(ctx, page.status)
                ),
            )
        }
        Row::Unreadable(slug) => link(
            slug,
            format!(
                "<span class='gd-row-title'><code>{}</code></span><span class='gd-row-tags'>{}</span>",
                escape(slug),
                pill("bad", &ctx.t("Could not be read"))
            ),
        ),
        Row::Missing(title, summary) => {
            let title = match title.as_str() {
                "Architecture" => ctx.t("Architecture"),
                "Data model" => ctx.t("Data model"),
                "Roadmap" => ctx.t("Roadmap"),
                other => other.to_string(),
            };
            let summary = if summary.is_empty() {
                String::new()
            } else {
                format!("<span class='gd-row-sum'>{}</span>", escape(summary))
            };
            format!(
                "<div class='gd-row gd-row-missing'><span class='gd-row-title'>{}</span>{summary}<span class='gd-row-tags'>{}</span></div>",
                escape(&title),
                pill("none", &ctx.t("Not written yet"))
            )
        }
    }
}

/// Pages that have history but no current file, each linking to its history.
fn deleted_list(ctx: &UiCtx, p: &Project, deleted: &[String]) -> String {
    if deleted.is_empty() {
        return String::new();
    }
    let items: String = deleted
        .iter()
        .filter(|s| check_slug(s).is_ok())
        .map(|s| {
            format!(
                "<li><a href='{}'>{}<code>{}</code></a></li>",
                escape(&history_url(&p.alias, s)),
                icon("history"),
                escape(s)
            )
        })
        .collect();
    format!(
        "<div class='gd-deleted'><h3 class='gd-group'>{}</h3><ul>{items}</ul></div>",
        escape(&ctx.t("Deleted pages"))
    )
}

/// The deleted pages under the empty state (no list to put them in).
fn deleted_card(ctx: &UiCtx, p: &Project, deleted: &[String]) -> String {
    let list = deleted_list(ctx, p, deleted);
    if list.is_empty() {
        String::new()
    } else {
        format!("<section class='card gd-list'>{list}</section>")
    }
}

/// `[label](slug)` → `[label](<guide url>)` when `slug` is a page in `existing`, and just
/// `label` when it is a page slug with no page (a link to nowhere); any other link is
/// left for the markdown renderer's own rules. Returns the text and the URLs it wrote.
/// Text inside a fenced code block or an inline code span is left as it is.
fn link_pages(text: &str, alias: &str, existing: &BTreeSet<&str>) -> (String, Vec<String>) {
    let mut out = String::with_capacity(text.len());
    let mut urls = Vec::new();
    let mut fenced = false;
    for line in text.split_inclusive('\n') {
        // The markdown renderer's own fence rule.
        if line.starts_with("```") {
            fenced = !fenced;
            out.push_str(line);
            continue;
        }
        if fenced {
            out.push_str(line);
            continue;
        }
        // Even pieces are outside a code span, odd ones inside.
        for (n, piece) in line.split('`').enumerate() {
            if n > 0 {
                out.push('`');
            }
            if n % 2 == 1 {
                out.push_str(piece);
            } else {
                out.push_str(&link_text(piece, alias, existing, &mut urls));
            }
        }
    }
    (out, urls)
}

/// [`link_pages`] for a piece of text with no code in it.
fn link_text(text: &str, alias: &str, existing: &BTreeSet<&str>, urls: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        rest = &rest[open..];
        let link = rest.find("](").and_then(|mid| {
            let label = &rest[1..mid];
            let close = mid + 2 + rest[mid + 2..].find(')')?;
            let target = &rest[mid + 2..close];
            let plain = |s: &str| !s.contains(['[', ']', '\n']);
            (plain(label) && plain(target) && check_slug(target).is_ok())
                .then_some((label, target, close))
        });
        match link {
            Some((label, target, close)) => {
                if existing.contains(target) {
                    let url = page_url(alias, target);
                    out.push_str(&format!("[{label}]({url})"));
                    urls.push(url);
                } else {
                    out.push_str(label);
                }
                rest = &rest[close + 1..];
            }
            None => {
                out.push('[');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn part_body(ctx: &UiCtx, alias: &str, body: &str, all: &[Page]) -> String {
    if body.trim().is_empty() {
        return format!(
            "<p class='mem-muted'>{}</p>",
            escape(&ctx.t("No details written yet."))
        );
    }
    let existing: BTreeSet<&str> = all.iter().map(|p| p.slug.as_str()).collect();
    let (text, urls) = link_pages(body, alias, &existing);
    let mut html = markdown::render_markdown(&text, &prefixes(ctx));
    // A link to another guide page stays in this tab.
    for url in urls {
        let url = escape(&url);
        html = html.replace(
            &format!("<a href=\"{url}\" rel=\"noopener\" target=\"_blank\">"),
            &format!("<a href=\"{url}\">"),
        );
    }
    format!("<div class='markdown mem-body'>{html}</div>")
}

/// The redaction notice for a guide page: same look as the profile's, but it names the
/// page file (`guide/<slug>.md`), which is where the value has to be removed.
fn redacted_page(ctx: &UiCtx, kind: &str) -> String {
    format!(
        "<p class='mem-redacted' role='note'>{}<span>{}</span></p>",
        icon("warning"),
        escape(&ctx.tf(
            "Hidden: this page looks like it contains a secret value ({kind}). Remove the value from the page file, or delete the page and rebuild it.",
            &[("kind", kind)]
        ))
    )
}

fn page_view(ctx: &UiCtx, p: &Project, page: &Page, all: &[Page], root: &Path) -> String {
    let secret = page_secret(page);
    // A build holds the guide slot, so a Save would only be refused: show it disabled.
    let running = guide_running(ctx, &p.alias);
    let head = format!(
        "<div class='gd-page-head'><h2>{}</h2>{}</div>{}",
        escape(shown_title(page)),
        status_pill(ctx, page.status),
        page_tools(ctx, p, page, secret.is_none(), root)
    );
    let banner = if page.status == Status::Draft {
        format!(
            "<div class='mem-banner gd-banner' role='note'>{}<p>{}</p></div>",
            icon("edit_note"),
            escape(&ctx.t("Draft: agents do not read this page until it is approved."))
        )
    } else {
        String::new()
    };
    if let Some(kind) = secret {
        // The parts can still be rewritten (that is how a secret is removed), but the
        // editors start empty: no stored text of this page reaches the browser.
        let fix: String = page
            .kind
            .parts()
            .iter()
            .filter(|(key, _)| page.parts.iter().any(|x| x.key == *key))
            .map(|(key, heading)| {
                format!(
                    "<section class='gd-fix'><div class='gd-fix-head'><h3>{}</h3>{}</div></section>",
                    escape(&part_heading(ctx, heading)),
                    part_editor(ctx, p, page, key, heading, "", running)
                )
            })
            .collect();
        return format!(
            "<article class='card gd-page'>{head}{banner}{}{fix}</article>",
            redacted_page(ctx, &kind)
        );
    }
    let human = page.parts.iter().any(|x| x.origin == Origin::Human);
    let agent = page.parts.iter().any(|x| x.origin == Origin::Agent);
    let mut meta = vec![match (agent, human) {
        (true, true) => ctx.t("Written by the agent and a person"),
        (false, true) => ctx.t("Written by a person"),
        _ => ctx.t("Written by the agent"),
    }];
    meta.push(page.generated_at.map_or_else(
        || ctx.t("Never verified"),
        |t| ctx.tf("Last verified {date}", &[("date", &day(t))]),
    ));
    let meta: Vec<String> = meta.iter().map(|m| escape(m)).collect();
    let commit = page.head_sha.as_deref().map_or_else(String::new, |sha| {
        format!(
            " · {} <code>{}</code>",
            escape(&ctx.t("Commit")),
            escape(&short_sha(sha))
        )
    });
    let meta = format!(
        "<p class='mem-meta gd-meta'>{}{commit}</p>",
        meta.join(" · ")
    );
    let lede = if page.summary.trim().is_empty() {
        String::new()
    } else {
        format!("<p class='mem-lede'>{}</p>", escape(&page.summary))
    };
    let parts: String = page
        .kind
        .parts()
        .iter()
        .filter_map(|(key, heading)| {
            let part = page.parts.iter().find(|x| x.key == *key)?;
            let tag = if part.origin == Origin::Human {
                pill("human", &ctx.t("Written by a person"))
            } else {
                String::new()
            };
            Some(format!(
                "<section class='gd-part'><div class='gd-part-head'><h3>{}</h3>{tag}{}</div>{}</section>",
                escape(&part_heading(ctx, heading)),
                part_editor(ctx, p, page, key, heading, &part.body, running),
                part_body(ctx, &p.alias, &part.body, all)
            ))
        })
        .collect();
    let parts = if parts.is_empty() {
        format!(
            "<p class='mem-muted'>{}</p>",
            escape(&ctx.t("No details written yet."))
        )
    } else {
        parts
    };
    let sources = if page.sources.is_empty() {
        format!(
            "<p class='mem-sources mem-muted'>{}</p>",
            escape(&ctx.t("No sources recorded."))
        )
    } else {
        let items: String = page
            .sources
            .iter()
            .map(|s| {
                format!(
                    "<li><code>{}</code><span class='mem-sha'>{}</span></li>",
                    escape(&s.path),
                    escape(&short_sha(&s.sha256))
                )
            })
            .collect();
        format!(
            "<details class='mem-sources'><summary>{}</summary><ul>{items}</ul></details>",
            escape(&ctx.tf("Sources ({n})", &[("n", &page.sources.len().to_string())]))
        )
    };
    format!("<article class='card gd-page'>{head}{banner}{meta}{lede}{parts}{sources}</article>")
}

/// Under the page title: Approve (a draft whose text is shown), History, and Delete
/// with its confirmation (only for a page the current plan does not name).
fn page_tools(ctx: &UiCtx, p: &Project, page: &Page, readable: bool, root: &Path) -> String {
    let fields = format!("{}{}", hidden("project", &p.alias), hidden("slug", &page.slug));
    let approve = if readable && page.status == Status::Draft {
        action_form(ctx, "/insights/memory/guide/approve", &fields, true, "check", &ctx.t("Approve"))
    } else {
        String::new()
    };
    let history = format!(
        "<a class='mem-btn' href='{}'>{}{}</a>",
        escape(&history_url(&p.alias, &page.slug)),
        icon("history"),
        escape(&ctx.t("History"))
    );
    let delete = if guide_ops::in_plan(&p.alias, &page.slug, Some(root)) {
        String::new()
    } else {
        format!(
            "<details class='gd-delete'><summary class='mem-btn'>{}{}</summary>\
             <div class='gd-confirm'><p>{}</p>\
             <form method='post' action='/insights/memory/guide/delete' class='mem-inline'>{}{fields}\
             <button class='mem-btn gd-btn-danger' type='submit'>{}{}</button></form></div></details>",
            icon("delete"),
            escape(&ctx.t("Delete page")),
            escape(&ctx.t("Delete this page? The current plan no longer names it. A copy stays under Deleted pages, so you can restore it.")),
            ctx.csrf_input(),
            icon("delete"),
            escape(&ctx.t("Delete"))
        )
    };
    format!("<div class='gd-tools'>{approve}{history}{delete}</div>")
}

/// One part's editor: one form, one Save. `text` prefills it (empty for a page whose
/// text is hidden). `key` and `heading` come from `Kind::parts()`. While a guide run
/// works on the project (`running`), Save is disabled and a note says why.
fn part_editor(ctx: &UiCtx, p: &Project, page: &Page, key: &str, heading: &str, text: &str, running: bool) -> String {
    let id = format!(
        "gd-body-{}-{}-{key}",
        p.alias.bytes().filter(u8::is_ascii_alphanumeric).map(char::from).collect::<String>(),
        page.slug
    );
    let note = if page.status == Status::Approved {
        format!(
            "<p class='gd-editor-note'>{}{}</p>",
            icon("visibility"),
            escape(&ctx.t("This page is approved, so agents see your change immediately."))
        )
    } else {
        String::new()
    };
    let (busy_note, disabled) = if running {
        (
            format!(
                "<p class='gd-editor-note gd-editor-busy' role='note'>{}{}</p>",
                icon("schedule"),
                escape(&ctx.t("A build is running, so this page cannot be changed right now."))
            ),
            " disabled aria-disabled='true'",
        )
    } else {
        (String::new(), "")
    };
    format!(
        "<details class='gd-edit'><summary class='mem-btn'>{}{}</summary>\
         <form method='post' action='/insights/memory/guide/edit' class='gd-editor'>{}{}{}{}{note}{busy_note}\
         <label class='gd-label' for='{id}'>{}</label>\
         <textarea class='gd-textarea' id='{id}' name='body' maxlength='{MAX_PART_CHARS}' required>{}</textarea>\
         <div class='gd-editor-foot'><p class='mem-muted'>{}</p>\
         <button class='mem-btn mem-btn-primary' type='submit'{disabled}>{}{}</button></div></form></details>",
        icon("edit"),
        escape(&ctx.t("Edit")),
        ctx.csrf_input(),
        hidden("project", &p.alias),
        hidden("slug", &page.slug),
        hidden("part", key),
        escape(&part_heading(ctx, heading)),
        escape(text),
        escape(&ctx.t("Saving marks this part as written by a person; rebuilds keep it.")),
        icon("save"),
        escape(&ctx.t("Save"))
    )
}

/// The time in a snapshot file name `<slug>.<%Y%m%dT%H%M%S%9fZ>.md`.
fn snapshot_time(slug: &str, name: &str) -> Option<DateTime<Utc>> {
    let ts = name.strip_prefix(slug)?.strip_prefix('.')?.strip_suffix(".md")?;
    NaiveDateTime::parse_from_str(ts, "%Y%m%dT%H%M%S%9fZ")
        .ok()
        .map(|t| t.and_utc())
}

/// A page's snapshots, newest first: (bare file name, time). Only names that parse
/// as a snapshot of `slug`; the names are what a Restore form may post back.
fn snapshots(alias: &str, slug: &str, root: &Path) -> Vec<(String, DateTime<Utc>)> {
    project_guide::history(alias, slug, Some(root))
        .iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?;
            Some((name.to_string(), snapshot_time(slug, name)?))
        })
        .collect()
}

/// The history of `slug` (checked; a page, an unreadable page or a deleted one): one
/// row per snapshot with its time and a Restore button. No page text at all.
fn history_view(ctx: &UiCtx, p: &Project, slug: &str, exists: bool, root: &Path) -> String {
    let (back_url, back_label) = if exists {
        (page_url(&p.alias, slug), ctx.t("Back to the page"))
    } else {
        (project_url(&p.alias, Some("guide")), ctx.t("Back to the guide"))
    };
    let list = snapshots(&p.alias, slug, root);
    let rows = if list.is_empty() {
        format!(
            "<p class='mem-muted'>{}</p>",
            escape(&ctx.t("No earlier versions of this page are kept."))
        )
    } else {
        let fields = format!("{}{}", hidden("project", &p.alias), hidden("slug", slug));
        let items: String = list
            .iter()
            .map(|(name, at)| {
                format!(
                    "<li><time class='gd-history-time' datetime='{}'>{}</time>\
                     <form method='post' action='/insights/memory/guide/restore' class='mem-inline'>{}{fields}{}\
                     <button class='mem-btn' type='submit'>{}{}</button></form></li>",
                    at.to_rfc3339(),
                    at.format("%Y-%m-%d %H:%M:%S UTC"),
                    ctx.csrf_input(),
                    hidden("snapshot", name),
                    icon("history"),
                    escape(&ctx.t("Restore"))
                )
            })
            .collect();
        format!("<ol class='gd-history'>{items}</ol>")
    };
    format!(
        "<article class='card gd-page gd-history-view'>\
         <div class='gd-history-head'><div class='gd-page-head'><h2>{}</h2><code class='gd-slug'>{}</code></div>\
         <a class='mem-btn' href='{}'>{}{}</a></div>\
         <p class='mem-muted mem-hint'>{}</p>{rows}</article>",
        escape(&ctx.t("Page history")),
        escape(slug),
        escape(&back_url),
        icon("arrow_back"),
        escape(&back_label),
        escape(&ctx.t("Restoring makes that version the current page. The version it replaces is kept here too, so a restore can be undone."))
    )
}

// ------------------------------------------------------------------ page actions

/// The form's `slug` when it passes `check_slug` (never the raw value otherwise).
fn form_slug(raw: Option<&str>) -> Option<(String, Kind)> {
    let raw = raw?;
    check_slug(raw).ok().map(|kind| (raw.to_string(), kind))
}

/// What a refused `guide_ops` call tells the page. Core errors are content-free
/// sentences; only their fixed wording is matched, never shown.
fn op_notice(err: &str) -> Notice {
    if err.contains("is in progress") {
        Notice::GuideBusy
    } else if err == "the page is in the current plan" {
        Notice::GuideInPlan
    } else if err.contains("looks like a secret") {
        Notice::Secret
    } else if err.contains("does not exist") || err.starts_with("no such snapshot") {
        Notice::BadInput
    } else {
        Notice::GuideFailed
    }
}

/// The checked alias and slug of a page-action form, or the redirect that refuses it:
/// an unknown project goes to the overview, a bad slug to the project's guide.
fn page_form(ctx: &UiCtx) -> Result<(String, String, Kind), UiResponse> {
    let form = ctx.form();
    let Some(alias) = known_alias(ctx, form.first("project")) else {
        return Err(back(Back::Overview, Notice::BadInput));
    };
    match form_slug(form.first("slug")) {
        Some((slug, kind)) => Ok((alias, slug, kind)),
        None => Err(back(Back::Project(&alias, Some("guide")), Notice::BadInput)),
    }
}

pub(super) fn approve_route(ctx: &UiCtx) -> UiResponse {
    let (alias, slug, _) = match page_form(ctx) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let root = memory_root(ctx);
    let notice = match guide_ops::approve_page(&alias, &slug, ctx.state.now(), Some(&root)) {
        Ok(_) => Notice::GuideApproved,
        Err(e) => op_notice(&e),
    };
    back(Back::GuidePage(&alias, &slug), notice)
}

/// Approve every draft. The count goes back as a number; a page that could not be
/// approved turns the notice into a failure (the rest are approved all the same).
pub(super) fn approve_all_route(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let Some(alias) = known_alias(ctx, form.first("project")) else {
        return back(Back::Overview, Notice::BadInput);
    };
    let to = Back::Project(&alias, Some("guide"));
    match guide_ops::approve_all(&alias, ctx.state.now(), Some(&memory_root(ctx))) {
        Ok(r) if r.failed.is_empty() => UiResponse::redirect(
            303,
            &format!(
                "{}&approved={}&notice={}",
                project_url(&alias, Some("guide")),
                r.approved,
                Notice::GuideAllApproved.code()
            ),
        ),
        // The rest were approved all the same: say how many, with the failure.
        Ok(r) => UiResponse::redirect(
            303,
            &format!(
                "{}&approved={}&notice={}",
                project_url(&alias, Some("guide")),
                r.approved,
                Notice::GuideFailed.code()
            ),
        ),
        Err(e) => back(to, op_notice(&e)),
    }
}

/// Rewrite one part. Checked here before the core sees it (so the notice is exact):
/// the part is one of the page kind's, the text is not empty, at most
/// [`MAX_PART_CHARS`] characters (and 64 KB) and holds no secret. A secret the core
/// still finds is then elsewhere on the page. The text is never echoed back.
pub(super) fn edit_route(ctx: &UiCtx) -> UiResponse {
    let (alias, slug, kind) = match page_form(ctx) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let to = || Back::GuidePage(&alias, &slug);
    let form = ctx.form();
    let Some(key) = form
        .first("part")
        .and_then(|raw| kind.parts().iter().map(|(k, _)| *k).find(|k| *k == raw))
    else {
        return back(to(), Notice::BadInput);
    };
    let body = form.first("body").unwrap_or("");
    if body.len() > 4 * MAX_PART_BYTES {
        return back(to(), Notice::GuideTooLong);
    }
    let text = body.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim();
    if text.is_empty() {
        return back(to(), Notice::BadInput);
    }
    if text.encode_utf16().count() > MAX_PART_CHARS || text.len() > MAX_PART_BYTES {
        return back(to(), Notice::GuideTooLong);
    }
    if contains_secret(text).is_some() {
        return back(to(), Notice::Secret);
    }
    let root = memory_root(ctx);
    let notice = match guide_ops::edit_part(&alias, &slug, key, text, ctx.state.now(), Some(&root)) {
        Ok(()) => Notice::GuideEdited,
        Err(e) => match op_notice(&e) {
            // The text was checked above, so the secret is in the stored page.
            Notice::Secret => Notice::GuideSecretElsewhere,
            n => n,
        },
    };
    back(to(), notice)
}

/// Delete a page the plan does not name (the core refuses one it does, and keeps a
/// snapshot of what it removes). Back to the guide, or to the page when refused.
pub(super) fn delete_route(ctx: &UiCtx) -> UiResponse {
    let (alias, slug, _) = match page_form(ctx) {
        Ok(v) => v,
        Err(r) => return r,
    };
    match guide_ops::delete_page(&alias, &slug, Some(&memory_root(ctx))) {
        Ok(()) => back(Back::Project(&alias, Some("guide")), Notice::GuideDeleted),
        Err(e) => back(Back::GuidePage(&alias, &slug), op_notice(&e)),
    }
}

/// Restore a snapshot. The posted name is only used to pick one of the names this
/// page's history lists; the core gets that listed name, never the form value.
pub(super) fn restore_route(ctx: &UiCtx) -> UiResponse {
    let (alias, slug, _) = match page_form(ctx) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let to = || Back::GuidePage(&alias, &slug);
    let root = memory_root(ctx);
    let form = ctx.form();
    let Some((name, _)) = form
        .first("snapshot")
        .and_then(|raw| snapshots(&alias, &slug, &root).into_iter().find(|(n, _)| n == raw))
    else {
        return back(to(), Notice::BadInput);
    };
    let notice = match guide_ops::restore(&alias, &slug, &name, ctx.state.now(), Some(&root)) {
        Ok(()) => Notice::GuideRestored,
        Err(e) => op_notice(&e),
    };
    back(to(), notice)
}

// ------------------------------------------------------------------ proposals

const DECIDE: &str = "/insights/memory/guide/proposal/decide";

/// The pending and approved `guide-page` proposals of every project, read once per
/// request (the profile's own list, `super::proposals`, holds `profile-section` only).
pub(super) fn proposals(ctx: &UiCtx) -> Vec<Value> {
    approvals::all(&approvals_dir(ctx))
        .into_iter()
        .filter(|a| a["kind"] == guide_proposal::KIND)
        .filter(|a| a["status"] == "pending" || a["status"] == "approved")
        .collect()
}

/// A guide proposal's checked id and slug, when it is a well-formed proposal of
/// `alias`: a valid id, `project` and `target.alias` both `alias`, a slug that passes
/// `check_slug`. Anything else is neither listed nor decided here.
fn checked<'a>(a: &'a Value, alias: &str) -> Option<(&'a str, &'a str)> {
    if a["kind"] != guide_proposal::KIND || a["project"] != alias || a["target"]["alias"] != alias {
        return None;
    }
    let id = a["id"].as_str().filter(|id| valid_id(id))?;
    let slug = a["target"]["slug"].as_str().filter(|s| check_slug(s).is_ok())?;
    Some((id, slug))
}

/// The redaction notice for a proposal: it names the proposal, not a file, and says
/// what to do instead of approving it.
fn redacted_proposal(ctx: &UiCtx, kind: &str) -> String {
    format!(
        "<p class='mem-redacted' role='note'>{}<span>{}</span></p>",
        icon("warning"),
        escape(&ctx.tf(
            "Hidden: this proposal looks like it contains a secret value ({kind}). It cannot be approved; dismiss it and rebuild the page.",
            &[("kind", kind)]
        ))
    )
}

/// The guide pages' proposals of `p` for the Proposals page, pending first, under a
/// **Guide pages** heading; empty when there are none.
pub(super) fn proposals_list(ctx: &UiCtx, p: &Project, all: &[Value]) -> String {
    let mut items: Vec<(&Value, &str, &str)> = all
        .iter()
        .filter_map(|a| checked(a, &p.alias).map(|(id, slug)| (a, id, slug)))
        .collect();
    if items.is_empty() {
        return String::new();
    }
    items.sort_by_key(|(a, _, _)| a["status"] != "pending");
    let root = memory_root(ctx);
    let list: String = items
        .iter()
        .map(|(a, id, slug)| proposal_item(ctx, &p.alias, a, id, slug, &root))
        .collect();
    format!(
        "{GP_STYLE}<div class='gp-group'><h3 class='gp-heading'>{}<span>{}</span></h3><div class='mem-proposals'>{list}</div></div>",
        icon("menu_book"),
        escape(&ctx.t("Guide pages"))
    )
}

fn proposal_item(ctx: &UiCtx, alias: &str, a: &Value, id: &str, slug: &str, root: &Path) -> String {
    let pending = a["status"] == "pending";
    let applied = !pending && guide_proposal::is_applied(alias, id, Some(root));
    let deleted = matches!(project_guide::load(alias, slug, Some(root)), Ok(None));
    let applies = !applied && guide_proposal::still_applies(a, Some(root));
    let text = |k: &str| a["target"][k].as_str().unwrap_or("").to_string();
    let (old, new) = (text("old"), text("new"));
    let secret = secret_in(&[&old, &new]);
    let status = if applied {
        pill("fresh", &ctx.t("Approved"))
    } else if !applies {
        pill("stale", &ctx.t("Stale"))
    } else if pending {
        pill("review", &ctx.t("Pending"))
    } else {
        pill("stale", &ctx.t("Approved, not applied"))
    };
    let note = |t: String| format!("<p class='mem-muted mem-unapplied'>{}</p>", escape(&t));
    let note = if applied {
        String::new()
    } else if deleted {
        note(ctx.t("The page was deleted after this was proposed."))
    } else if !applies && pending {
        note(ctx.t("The page changed after this was proposed, so it can no longer be applied. Dismiss it and rebuild the page for a new proposal."))
    } else if !applies {
        note(ctx.t("The page changed after this was approved, so it was not applied. Rebuild the page for a new proposal."))
    } else if !pending {
        note(ctx.t("Approved, but not applied yet. Apply it to write it into the guide page."))
    } else {
        String::new()
    };
    let created = a["created_at"].as_str().and_then(|s| s.get(..10)).unwrap_or("");
    let when = if created.is_empty() {
        String::new()
    } else {
        format!("<span class='mem-muted'>{}</span>", escape(&ctx.tf("Proposed {date}", &[("date", created)])))
    };
    let diff = match &secret {
        Some(kind) => redacted_proposal(ctx, kind),
        None => {
            let col = |class: &str, label: String, body: &str| {
                let body = if body.trim().is_empty() {
                    format!("<p class='mem-muted'>{}</p>", escape(&ctx.t("(empty)")))
                } else {
                    format!("<pre class='mem-pre'>{}</pre>", escape(body))
                };
                format!("<div class='mem-diff-col mem-diff-{class}'><h4>{}</h4>{body}</div>", escape(&label))
            };
            format!(
                "<details class='gp-diff'><summary>{}<span>{}</span></summary><div class='mem-diff'>{}{}</div></details>",
                icon("expand_more"),
                escape(&ctx.t("Show the change")),
                col("old", ctx.t("Current"), &old),
                col("new", ctx.t("Proposed"), &new)
            )
        }
    };
    // Approve (or Apply) only while the page still holds the text it was raised
    // against and the proposal holds no secret; Dismiss while it is pending.
    let can_apply = applies && secret.is_none();
    let button = |decision: &str, primary: bool, icon_name: &str, label: String| {
        format!(
            "<button class='mem-btn{}' type='submit' name='decision' value='{decision}'>{}{}</button>",
            if primary { " mem-btn-primary" } else { "" },
            icon(icon_name),
            escape(&label)
        )
    };
    let mut buttons = String::new();
    if pending {
        buttons.push_str(&button("dismiss", false, "close", ctx.t("Dismiss")));
        if can_apply {
            buttons.push_str(&button("approve", true, "check", ctx.t("Approve")));
        }
    } else if !applied && can_apply {
        buttons.push_str(&button("approve", true, "check", ctx.t("Apply")));
    }
    let actions = if buttons.is_empty() {
        String::new()
    } else {
        format!(
            "<div class='mem-actions'><form method='post' action='{DECIDE}' class='mem-inline mem-decide'>{}{}{}{buttons}</form></div>",
            ctx.csrf_input(),
            hidden("project", alias),
            hidden("id", id)
        )
    };
    format!(
        "<article class='mem-proposal' data-proposal='{id}'><div class='mem-proposal-head'><h3>{}</h3>{status}{when}</div>{diff}{note}{actions}</article>",
        escape(&ctx.tf("Update the {slug} guide page", &[("slug", slug)]))
    )
}

/// Approve or dismiss a guide page proposal of this project. The approval must be a
/// `guide-page` proposal whose project is the (known) alias; anything else is
/// `bad-input` and nothing is decided. Approving goes through the core's
/// `proposals::decide`, which takes the guide slot, checks the page still holds the
/// text the proposal was raised against, and applies it; its errors are matched on
/// their fixed wording and never shown.
pub(super) fn proposal_decide_route(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let Some(alias) = known_alias(ctx, form.first("project")) else {
        return back(Back::Overview, Notice::BadInput);
    };
    let to = || Back::Project(&alias, Some("proposals"));
    let id = form.first("id").unwrap_or("");
    let decision = match form.first("decision") {
        Some("approve") => "approve",
        Some("dismiss") => "deny",
        _ => return back(to(), Notice::BadInput),
    };
    if !valid_id(id) {
        return back(to(), Notice::BadInput);
    }
    let dir = approvals_dir(ctx);
    let Some(a) = approvals::get(&dir, id).filter(|a| a["id"] == id && checked(a, &alias).is_some()) else {
        return back(to(), Notice::BadInput);
    };
    let status = a["status"].as_str().unwrap_or("");
    if status != "pending" && (decision == "deny" || status != "approved") {
        return back(to(), Notice::AlreadyDecided);
    }
    let root = memory_root(ctx);
    let notice = match core_proposals::decide(id, decision, None, "dashboard", Some(&root), &dir, ctx.state.now()) {
        Ok(_) if decision == "deny" => Notice::Dismissed,
        Ok(_) => Notice::GuideProposalApproved,
        Err(e) => {
            let e = e.to_string();
            if e.contains("changed since") {
                Notice::GuideProposalChanged
            } else if e.contains("in progress") {
                Notice::GuideBusy
            } else if e.contains("already decided") || decision == "deny" {
                Notice::AlreadyDecided
            } else {
                Notice::GuideProposalFailed
            }
        }
    };
    back(to(), notice)
}

/// The guide proposals' own styles on the Proposals page.
const GP_STYLE: &str = "<style>
.gp-group{margin-top:1.25rem;padding-top:1rem;border-top:1px solid var(--md-outline-variant)}
.gp-heading{display:flex;align-items:center;gap:.45rem;margin:0 0 .25rem;font-size:.95rem;font-weight:600}
.gp-heading .icon{width:17px;height:17px;flex:0 0 17px;color:var(--md-primary)}
.gp-diff{margin:.5rem 0 .25rem}
.gp-diff>summary{display:inline-flex;align-items:center;gap:.35rem;height:30px;box-sizing:border-box;padding:0 .8rem;border-radius:999px;border:1px solid var(--md-outline-variant);background:transparent;color:var(--md-on-surface-variant);font:500 .8rem -apple-system,BlinkMacSystemFont,'SF Pro Text',system-ui,sans-serif;cursor:pointer;list-style:none;user-select:none}
.gp-diff>summary::-webkit-details-marker{display:none}
.gp-diff>summary:hover{background:var(--md-surface-container-high);color:var(--md-on-surface)}
.gp-diff>summary:focus-visible{outline:2px solid var(--md-primary);outline-offset:2px}
.gp-diff>summary .icon{width:15px;height:15px;flex:0 0 15px}
.gp-diff[open]>summary{background:var(--md-surface-container-high);color:var(--md-on-surface)}
.gp-diff[open]>summary .icon{transform:rotate(180deg)}
.gp-diff>.mem-diff{margin-top:.6rem}
@media (prefers-reduced-motion:no-preference){
.gp-diff>summary{transition:background-color .25s cubic-bezier(0.32,0.72,0,1),color .25s cubic-bezier(0.32,0.72,0,1)}
.gp-diff>summary .icon{transition:transform .25s cubic-bezier(0.32,0.72,0,1)}
}
</style>";

const GUIDE_STYLE: &str = "<style>
.gd-layout{display:grid;grid-template-columns:minmax(220px,280px) minmax(0,1fr);gap:1.25rem;align-items:start}
.gd-main{display:flex;flex-direction:column;gap:1.25rem;min-width:0}
.card.gd-list{position:sticky;top:1rem;max-height:calc(100vh - 7rem);overflow:auto;overscroll-behavior:contain;box-sizing:border-box;display:flex;flex-direction:column;gap:.2rem;min-width:0;padding:.6rem;border-radius:14px}
.gd-group{margin:.55rem .55rem .3rem;font-size:.72rem;font-weight:600;letter-spacing:.04em;text-transform:uppercase;color:var(--md-on-surface-variant)}
.gd-group:first-child{margin-top:.2rem}
.gd-rows{display:flex;flex-direction:column;gap:2px}
.gd-row{display:flex;flex-direction:column;align-items:flex-start;gap:.2rem;min-width:0;box-sizing:border-box;padding:.5rem .65rem;border-radius:10px;color:var(--md-on-surface);text-decoration:none}
a.gd-row:hover{background:var(--md-surface-container-high)}
a.gd-row:focus-visible{outline:2px solid var(--md-primary);outline-offset:1px}
.gd-row.is-active{background:var(--md-nav-active-surface);color:var(--md-nav-active-on-surface)}
.gd-row-missing{opacity:.75}
.gd-row-title{max-width:100%;font-size:.9rem;font-weight:600;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.gd-row-title code{font:.82rem ui-monospace,SFMono-Regular,Menlo,monospace}
.gd-row-sum{max-width:100%;font-size:.8rem;line-height:1.4;color:var(--md-on-surface-variant);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.gd-row.is-active .gd-row-sum{color:inherit;opacity:.8}
.gd-row-tags{display:flex;flex-wrap:wrap;align-items:center;gap:.3rem}
.gd-off-plan{display:inline-flex;align-items:center;height:22px;box-sizing:border-box;padding:0 .55rem;border-radius:999px;border:1px solid var(--md-outline-variant);font-size:.7rem;font-weight:500;color:var(--md-on-surface-variant);white-space:nowrap}
.gd-deleted{margin-top:.35rem;padding-top:.35rem;border-top:1px solid var(--md-outline-variant)}
.gd-deleted ul{list-style:none;margin:0;padding:0 .2rem;display:flex;flex-direction:column;gap:2px}
.gd-deleted a{display:flex;align-items:center;gap:.45rem;height:30px;box-sizing:border-box;padding:0 .45rem;border-radius:8px;color:var(--md-on-surface-variant);text-decoration:none;font-size:.82rem}
.gd-deleted a:hover{background:var(--md-surface-container-high);color:var(--md-on-surface)}
.gd-deleted .icon{width:14px;height:14px;flex:0 0 14px}
.gd-page{min-width:0}
.gd-page-head{display:flex;flex-wrap:wrap;align-items:center;gap:.6rem;margin-bottom:.4rem}
.gd-page-head h2{margin:0;min-width:0;overflow-wrap:anywhere}
.gd-banner{margin:.35rem 0 .85rem}
.gd-meta code{font:.78rem ui-monospace,SFMono-Regular,Menlo,monospace}
.gd-part{padding:1rem 0 .25rem;border-top:1px solid var(--md-outline-variant)}
.gd-part-head{display:flex;flex-wrap:wrap;align-items:center;gap:.5rem;margin-bottom:.4rem}
.gd-part-head h3{margin:0;font-size:1rem}
.gd-part .mem-body>:first-child{margin-top:0}
.gd-part .mem-body a{color:var(--md-primary)}
.gd-empty{display:flex;flex-direction:column;align-items:center;gap:.7rem;padding:2.5rem 1.5rem;text-align:center}
.gd-empty h2{margin:0;font-size:1.1rem}
.gd-empty p{margin:0;max-width:36rem;line-height:1.5;color:var(--md-on-surface-variant)}
.gd-empty-icon .icon{width:34px;height:34px;color:var(--md-primary)}
.gd-build{margin:.35rem 0 0}
.gd-actions{display:flex;flex-wrap:wrap;align-items:center;justify-content:space-between;gap:.6rem 1rem;min-width:0}
.gd-actions .mem-hint{flex:1 1 16rem;min-width:0;margin:0}
.gd-actions-buttons{display:flex;flex-wrap:wrap;align-items:center;gap:.5rem;margin-left:auto}
.gd-actions form{margin:0}
.gd-tools{display:flex;flex-wrap:wrap;align-items:center;gap:.5rem;margin:.15rem 0 .75rem}
.gd-tools form{margin:0}
.gd-delete{min-width:0}
.gd-delete[open]{flex:1 1 100%}
.gd-delete[open]>summary,.gd-edit[open]>summary{background:var(--md-surface-container-high)}
.gd-confirm{display:flex;flex-wrap:wrap;align-items:center;justify-content:space-between;gap:.6rem 1rem;margin-top:.6rem;padding:.8rem 1rem;box-sizing:border-box;border:1px solid color-mix(in srgb,var(--md-error) 35%,transparent);border-radius:12px;background:color-mix(in srgb,var(--md-error-container) 45%,transparent)}
.gd-confirm p{flex:1 1 16rem;min-width:0;margin:0;font-size:.85rem;line-height:1.45;color:var(--md-on-surface)}
.gd-confirm form{margin:0}
.mem-btn.gd-btn-danger{background:var(--md-error);border-color:var(--md-error);color:var(--md-on-error)}
.mem-btn.gd-btn-danger:hover{background:var(--md-error);filter:brightness(1.08)}
.gd-approve-all{margin:.15rem .2rem .4rem}
.gd-approve-all .mem-btn{width:100%;justify-content:center}
.gd-edit{margin-left:auto;min-width:0}
.gd-edit[open]{flex:1 1 100%;margin-left:0}
.gd-editor{display:flex;flex-direction:column;gap:.5rem;margin:.6rem 0 .4rem;padding:1rem;box-sizing:border-box;border:1px solid color-mix(in srgb,var(--md-outline) 30%,transparent);border-radius:14px;background:transparent}
.gd-editor-note{display:flex;align-items:center;gap:.45rem;margin:0;padding:.5rem .75rem;border-radius:10px;background:var(--md-primary-container);color:var(--md-on-primary-container);font-size:.82rem;line-height:1.4}
.gd-editor-note .icon{width:15px;height:15px;flex:0 0 15px}
.gd-editor-busy{background:var(--md-surface-container-highest);color:var(--md-on-surface-variant)}
.gd-label{font-size:.78rem;font-weight:600;color:var(--md-on-surface-variant)}
.gd-textarea{display:block;width:100%;height:220px;min-height:120px;box-sizing:border-box;flex:none;margin:0;padding:.6rem .75rem;border:1px solid color-mix(in srgb,var(--md-outline) 45%,transparent);border-radius:10px;background:var(--md-surface-container-lowest);color:var(--md-on-surface);font:.84rem/1.5 ui-monospace,SFMono-Regular,Menlo,monospace;resize:vertical}
.gd-textarea:focus{outline:2px solid var(--md-primary);outline-offset:0;border-color:transparent}
.gd-editor-foot{display:flex;flex-wrap:wrap;align-items:center;justify-content:space-between;gap:.6rem 1rem}
.gd-editor-foot p{flex:1 1 220px;min-width:0;margin:0;font-size:.82rem}
.gd-fix{padding:.85rem 0 .25rem;border-top:1px solid var(--md-outline-variant)}
.gd-fix-head{display:flex;flex-wrap:wrap;align-items:center;gap:.5rem}
.gd-fix-head h3{margin:0;font-size:1rem}
.gd-history-head{display:flex;flex-wrap:wrap;align-items:center;justify-content:space-between;gap:.6rem 1rem;margin-bottom:.35rem}
.gd-history-head .gd-page-head{margin:0}
.gd-slug{font:.82rem ui-monospace,SFMono-Regular,Menlo,monospace;color:var(--md-on-surface-variant)}
.gd-history{list-style:none;margin:.6rem 0 0;padding:0}
.gd-history li{display:flex;flex-wrap:wrap;align-items:center;justify-content:space-between;gap:.5rem 1rem;min-height:48px;box-sizing:border-box;padding:.4rem 0;border-top:1px solid var(--md-outline-variant)}
.gd-history-time{font-size:.9rem;font-variant-numeric:tabular-nums}
.gd-history form{margin:0}
@media (prefers-reduced-motion:no-preference){
.gd-row,.gd-deleted a{transition:background-color .25s cubic-bezier(0.32,0.72,0,1),color .25s cubic-bezier(0.32,0.72,0,1)}
}
@media (max-width:760px){
.gd-layout{grid-template-columns:minmax(0,1fr)}
.card.gd-list{position:static;max-height:none;overflow:visible}
}
</style>";
