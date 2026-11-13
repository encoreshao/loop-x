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

use super::{
    day, escape, hidden, icon, markdown, memory_root, pill, prefixes, project_url, redacted,
    short_sha, spinner, Model, Project,
};
use crate::ui::routes::UiCtx;
use loopx_core::guide_ops;
use loopx_core::guide_plan::{self, Plan};
use loopx_core::profile_build;
use loopx_core::project_guide::{self, check_slug, Kind, Page};
use loopx_core::project_profile::{contains_secret, Origin, Status};
use std::collections::BTreeSet;
use std::path::Path;

/// The guide page of `alias` showing `slug`. Both are checked values (a known alias,
/// a slug that passed `check_slug`).
fn page_url(alias: &str, slug: &str) -> String {
    format!("{}&guide={slug}", project_url(alias, Some("guide")))
}

/// Whether a guide build or page operation holds the project's guide slot.
fn guide_busy(alias: &str) -> bool {
    profile_build::is_running(&format!("guide:{alias}"))
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
    if g.pages.is_empty() && g.unreadable.is_empty() && g.plan.is_none() {
        return format!(
            "{GUIDE_STYLE}{}{}",
            empty_state(ctx, p),
            deleted_card(ctx, p, &g.deleted)
        );
    }
    let groups = rows(&p.alias, &root, &g);
    let first = groups.iter().flatten().find_map(Row::slug);
    // The selected slug: a checked `guide` value naming a listed page, or the first page.
    let query = ctx.query();
    let raw = query.first("guide");
    let selected = match raw {
        None => first,
        Some(raw) => groups
            .iter()
            .flatten()
            .filter_map(Row::slug)
            .find(|s| check_slug(raw).is_ok() && *s == raw),
    };
    let right = match selected {
        None if raw.is_some() => muted_page(&ctx.t("This page does not exist.")),
        None => muted_page(&ctx.t("No page has been written yet.")),
        Some(slug) => match g.pages.iter().find(|x| x.slug == slug) {
            Some(page) => page_view(ctx, p, page, &g.pages),
            None => muted_page(&ctx.t("This page could not be read.")),
        },
    };
    format!(
        "{GUIDE_STYLE}<div class='gd-layout'>{}<div class='gd-main'>{right}</div></div>",
        list_view(ctx, p, &groups, selected, &g.deleted)
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
        } else if guide_busy(&p.alias) {
            format!(
                "<button class='mem-btn mem-btn-primary' type='button' disabled>{}{}</button>",
                spinner(),
                escape(&ctx.t("Working…"))
            )
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
    let mut out = String::new();
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
                escape(&format!(
                    "{}&history={s}",
                    project_url(&p.alias, Some("guide"))
                )),
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
fn link_pages(text: &str, alias: &str, existing: &BTreeSet<&str>) -> (String, Vec<String>) {
    let mut out = String::with_capacity(text.len());
    let mut urls = Vec::new();
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
    (out, urls)
}

fn part_body(ctx: &UiCtx, alias: &str, key: &str, body: &str, all: &[Page]) -> String {
    if body.trim().is_empty() {
        return format!(
            "<p class='mem-muted'>{}</p>",
            escape(&ctx.t("No details written yet."))
        );
    }
    if key != "associations" {
        return format!(
            "<div class='markdown mem-body'>{}</div>",
            markdown::render_markdown(body, &prefixes(ctx))
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

fn page_view(ctx: &UiCtx, p: &Project, page: &Page, all: &[Page]) -> String {
    let head = format!(
        "<div class='gd-page-head'><h2>{}</h2>{}</div>",
        escape(shown_title(page)),
        status_pill(ctx, page.status)
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
    if let Some(kind) = page_secret(page) {
        return format!(
            "<article class='card gd-page'>{head}{banner}{}</article>",
            redacted(ctx, &kind)
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
                "<section class='gd-part'><div class='gd-part-head'><h3>{}</h3>{tag}</div>{}</section>",
                escape(&part_heading(ctx, heading)),
                part_body(ctx, &p.alias, key, &part.body, all)
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

const GUIDE_STYLE: &str = "<style>
.gd-layout{display:grid;grid-template-columns:minmax(220px,280px) minmax(0,1fr);gap:1.25rem;align-items:start}
.gd-main{display:flex;flex-direction:column;gap:1.25rem;min-width:0}
.card.gd-list{position:sticky;top:1rem;display:flex;flex-direction:column;gap:.2rem;min-width:0;padding:.6rem;border-radius:14px}
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
@media (prefers-reduced-motion:no-preference){
.gd-row,.gd-deleted a{transition:background-color .25s cubic-bezier(0.32,0.72,0,1),color .25s cubic-bezier(0.32,0.72,0,1)}
}
@media (max-width:760px){
.gd-layout{grid-template-columns:minmax(0,1fr)}
.card.gd-list{position:static}
}
</style>";
