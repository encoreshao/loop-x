//! `GET /readme`: the repo's README rendered in-app (`render_readme_page`).

use crate::ui::badge::default_badge;
use crate::ui::markdown::{self, GitlabPrefixes};
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::shell::{render_shell, ShellOpts};
use crate::ui::template::{Scope, Template};
use crate::ui::{i18n::Lang, pyfmt};
use std::path::PathBuf;

pub fn register(r: &mut Registry) {
    r.get("/readme", page);
}

/// `_localized_readme_path`: `README.<html_lang>.md` for a non-English request
/// when that translation exists, else `README.md`.
fn readme_path(ctx: &UiCtx) -> PathBuf {
    let base = ctx.state.repo_root.join("README.md");
    if ctx.lang != Lang::En {
        let translated = ctx
            .state
            .repo_root
            .join(format!("README.{}.md", ctx.lang.html_lang()));
        if translated.is_file() {
            return translated;
        }
    }
    base
}

fn page(ctx: &UiCtx) -> UiResponse {
    let mut content = match std::fs::read(readme_path(ctx)) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => format!("# README\n\n{}", ctx.t("No README.md found in this repo.")),
    };
    // The GitHub-facing language-switch line atop each README does not resolve in-app.
    let (first, rest) = content.split_once('\n').unwrap_or((&content, ""));
    if first.starts_with("[English](README.md)") || first.starts_with("**English**") {
        content = rest.trim_start_matches('\n').to_string();
    }

    let links: String = markdown::h2_sections(&content)
        .iter()
        .filter(|(title, _)| pyfmt::strip(title).to_lowercase() != "table of contents")
        .map(|(title, slug)| {
            format!(
                "<a href='#{slug}' class='readme-quicknav-link'>{}</a>",
                pyfmt::escape(title)
            )
        })
        .collect();
    let quicknav = if links.is_empty() {
        String::new()
    } else {
        format!(
            "<nav class='readme-quicknav' aria-label='{}'>\
             <p class='readme-quicknav-title'>{}</p>{links}</nav>",
            pyfmt::escape(&ctx.t("Jump to section")),
            pyfmt::escape(&ctx.t("On this page"))
        )
    };

    let prefixes: GitlabPrefixes = crate::ui::gitlab::issue_url_prefixes(ctx.state);
    let mut scope = Scope::new(ctx.lang);
    scope
        .var(
            "readme_subtitle",
            pyfmt::escape_text(&ctx.t("This project's README, rendered here for reference.")),
        )
        .var("quicknav_html", quicknav)
        .set("render_markdown(content)", markdown::render_markdown(&content, &prefixes));
    let body = Template::named("readme_page")
        .render(&scope)
        .expect("readme template");
    UiResponse::html(render_shell(
        ctx,
        "README · Loop X Engineering",
        "readme",
        &default_badge(ctx),
        &body,
        ShellOpts::default(),
    ))
}
