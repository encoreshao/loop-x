//! Skills and Chat tools views.

use super::super::loops_common::flash_html;
use crate::ui::consts;
use crate::ui::pyfmt;
use crate::ui::routes::UiCtx;
use crate::ui::template::{Scope, Template};
use loopx_core::settings_store as store;

const SKILLS: [(&str, &str, &str, &[&str]); 1] = [(
    "gitlab-config",
    "GitLab API access - instances, tokens, project aliases, and the local instance/project/issue cache every gitlab_api.py/gitlab_cache.py call reads from.",
    "skills/gitlab-config/scripts/gitlab_api.py",
    &[
        "bin/web/dashboard_server.py",
        "bin/project_memory.py",
        "bin/track_new_comments.py",
        "bin/list_assigned_issues.py",
        "instructions/gitlab-issue.md",
    ],
)];

pub fn skills_path(ctx: &UiCtx) -> std::path::PathBuf {
    ctx.state.user_home.join(".encore-skills")
}

pub fn install_status_path(ctx: &UiCtx) -> std::path::PathBuf {
    ctx.state.repo_root.join("outputs").join("skills_install_status.json")
}

pub fn skills_body(ctx: &UiCtx, flash: Option<&str>, flash_ok: bool) -> String {
    let status = loopx_core::status::read_status(&install_status_path(ctx));
    let installing = status.get("state").and_then(|s| s.as_str()) == Some("installing");
    let csrf = ctx.csrf_input();
    let mut rows = String::new();
    for (name, description, check, used_by) in SKILLS {
        let path = skills_path(ctx).join(check);
        let status_cell = if path.exists() {
            format!(
                "<span class='pill pill-green'>{}{}</span>",
                consts::ds("_CHECK_ICON"),
                pyfmt::escape(&ctx.t("installed"))
            )
        } else if installing {
            format!(
                "<span class='pill pill-blue'>{}{}</span>",
                consts::ds("_SPINNER_ICON"),
                pyfmt::escape(&ctx.t("setup in progress…"))
            )
        } else {
            let confirm = pyfmt::escape(&ctx.tf(
                "Set up {name}? This installs it in the background and restarts the dashboard when done - the page may briefly go offline.",
                &[("name", name)],
            ));
            format!(
                "<span class='pill pill-grey'>{}</span>\
<form method='post' action='/skills/install' class='daemon-action-form'>{csrf}\
<button type='submit' class='btn btn-primary' data-confirm=\"{confirm}\">{}</button></form>",
                pyfmt::escape(&ctx.t("not installed")),
                pyfmt::escape(&ctx.t("Set up & restart dashboard"))
            )
        };
        let used: String = used_by
            .iter()
            .map(|u| format!("<li><code>{}</code></li>", pyfmt::escape(u)))
            .collect();
        rows.push_str(&format!(
            "<tr class='skill-row' tabindex='0' role='button' aria-expanded='false' \
onclick=\"this.classList.toggle('is-expanded'); this.setAttribute('aria-expanded', this.classList.contains('is-expanded'))\" \
onkeydown=\"if (event.key === 'Enter' || event.key === ' ') {{ event.preventDefault(); this.click(); }}\">\
<td>{} {}</td><td>{status_cell}</td><td>{}</td></tr>\
<tr class='skill-detail-row'><td colspan='3'>\
<p><strong>{}</strong></p><ul class='plain'>{used}</ul>\
<p><strong>{}</strong></p><code>{}</code></td></tr>",
            pyfmt::escape(name),
            consts::ds("_EXPAND_ICON"),
            pyfmt::escape(description),
            pyfmt::escape(&ctx.t("Used by")),
            pyfmt::escape(&ctx.t("Path")),
            pyfmt::escape(&path.display().to_string())
        ));
    }
    let skills_html = if rows.is_empty() {
        format!("<p>{}</p>", pyfmt::escape(&ctx.t("(no skill dependencies registered)")))
    } else {
        format!(
            "<div class='table-wrap'><table class='daemons skills'>\
<thead><tr><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>{rows}</tbody></table></div>",
            pyfmt::escape(&ctx.t("Skill")),
            pyfmt::escape(&ctx.t("Status")),
            pyfmt::escape(&ctx.t("What it does"))
        )
    };
    let mut scope = Scope::new(ctx.lang);
    scope
        .var("flash_html", flash_html(flash, flash_ok))
        .var("skills_html", skills_html);
    Template::named("settings_skills_body")
        .render(&scope)
        .unwrap_or_else(|e| panic!("{e}"))
}

pub fn chat_tools_path(ctx: &UiCtx) -> std::path::PathBuf {
    ctx.state.home_dir.join("chat_tools.json")
}

pub fn chat_body(ctx: &UiCtx, flash: Option<&str>, flash_ok: bool) -> String {
    let chosen = store::auto_approved(&chat_tools_path(ctx));
    let rows: String = store::CHAT_WRITE_TOOLS
        .iter()
        .map(|(name, description)| {
            format!(
                "<li><label class='md-checkbox'><input type='checkbox' name='auto_approve' value='{n}'{}> <code>{n}</code> <span class='muted'>{}</span></label></li>",
                if chosen.iter().any(|c| c == name) { " checked" } else { "" },
                pyfmt::escape(description),
                n = pyfmt::escape(name)
            )
        })
        .collect();
    let mut scope = Scope::new(ctx.lang);
    scope
        .var("flash_html", flash_html(flash, flash_ok))
        .var("csrf_input", ctx.csrf_input())
        .var("rows", rows);
    Template::named("settings_chat_body")
        .render(&scope)
        .unwrap_or_else(|e| panic!("{e}"))
}
