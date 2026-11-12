//! Skills and Chat tools views.

use super::super::loops_common::flash_html;
use crate::ui::consts;
use crate::ui::pyfmt;
use crate::ui::routes::UiCtx;
use crate::ui::template::{Scope, Template};
use loopx_core::settings_store as store;

/// (name, description, installed-marker path relative to the user's home, code-path users).
/// An empty users list marks a skill the Creator page and chat use (see `creator_skill_detail`):
/// it installs in several ways, so `creator_skill_file` finds it and the marker is only where
/// a hand install would put it.
const SKILLS: [(&str, &str, &str, &[&str]); 4] = [
    (
        "gitlab-config",
        "GitLab API access - instances, tokens, project aliases, and the local instance/project/issue cache every gitlab_api.py/gitlab_cache.py call reads from.",
        ".encore-skills/skills/gitlab-config/scripts/gitlab_api.py",
        &[
            "bin/web/dashboard_server.py",
            "bin/project_memory.py",
            "bin/track_new_comments.py",
            "bin/list_assigned_issues.py",
            "instructions/gitlab-issue.md",
        ],
    ),
    (
        "video-scout",
        "Finds, ranks and curates high-quality public videos (TED, YouTube, universities) for the Creator page.",
        ".claude/skills/video-scout/SKILL.md",
        &[],
    ),
    (
        "inspire-video",
        "Turns a talk, interview or video URL into a polished 9:16 bilingual inspirational short video.",
        ".inspire-video/skills/inspire-video/SKILL.md",
        &[],
    ),
    (
        "inspire-showcase",
        "Turns a website or a GitHub or GitLab repository into a short bilingual promo video (a camera flying over the real page, or a framed walkthrough).",
        ".inspire-video/skills/inspire-showcase/SKILL.md",
        &[],
    ),
];

/// A Creator skill's `SKILL.md`, wherever it is installed: a Claude Code plugin, a clone
/// or a link under `~/.claude/skills`. `None` when it is not installed.
fn creator_skill_file(user_home: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    use loopx_core::creator::skills::{self, Engine};
    if name == "video-scout" {
        return skills::scout_skill(user_home).map(|dir| dir.join("SKILL.md"));
    }
    skills::engine(user_home)
        .filter(|engine| engine.has_skill(name))
        .map(|engine| engine.root.join(Engine::skill_file(name)))
}

/// Used-by list, requirements and the repository to install as a plugin for the Creator
/// skills. They are not installed from the page: adding a plugin stays a deliberate action
/// in Claude Code.
fn creator_skill_detail(ctx: &UiCtx, name: &str) -> (String, String, &'static str) {
    let li = |t: String| format!("<li>{}</li>", pyfmt::escape(&t));
    let used = format!("{}{}", li(ctx.t("Creator page")), li(ctx.t("Chat")));
    if name == "video-scout" {
        (used, ctx.t("Web search and fetch (no extra tools)"), "video-scout")
    } else if name == "inspire-showcase" {
        (
            used,
            ctx.t("The inspire-video plugin (it holds both skills), plus Chrome and Playwright for website promos"),
            "inspire-video",
        )
    } else {
        (
            used,
            ctx.t("python 3, node + npm and ffmpeg/ffprobe; yt-dlp, Remotion and its other packages are installed on first use"),
            "inspire-video",
        )
    }
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
        let creator = used_by.is_empty();
        let found = if creator { creator_skill_file(&ctx.state.user_home, name) } else { None };
        let installed = found.is_some() || (!creator && ctx.state.user_home.join(check).exists());
        let path = found.unwrap_or_else(|| ctx.state.user_home.join(check));
        let status_cell = if installed {
            format!(
                "<span class='pill pill-green'>{}{}</span>",
                consts::ds("_CHECK_ICON"),
                pyfmt::escape(&ctx.t("installed"))
            )
        } else if creator {
            format!("<span class='pill pill-grey'>{}</span>", pyfmt::escape(&ctx.t("not installed")))
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
        let mut used: String = used_by
            .iter()
            .map(|u| format!("<li><code>{}</code></li>", pyfmt::escape(u)))
            .collect();
        let mut extra = String::new();
        if creator {
            let (users, requirements, repo) = creator_skill_detail(ctx, name);
            used = users;
            extra.push_str(&format!(
                "<p><strong>{}</strong> {}</p>",
                pyfmt::escape(&ctx.t("Requires")),
                pyfmt::escape(&requirements)
            ));
            if !installed {
                extra.push_str(&format!(
                    "<p><strong>{}</strong></p>\
<p><code>/plugin marketplace add encoreshao/{repo}</code></p>\
<p><code>/plugin install {repo}@{repo}</code></p>",
                    pyfmt::escape(&ctx.t("Set up in Claude Code"))
                ));
            }
        }
        rows.push_str(&format!(
            "<tr class='skill-row' tabindex='0' role='button' aria-expanded='false' \
onclick=\"this.classList.toggle('is-expanded'); this.setAttribute('aria-expanded', this.classList.contains('is-expanded'))\" \
onkeydown=\"if (event.key === 'Enter' || event.key === ' ') {{ event.preventDefault(); this.click(); }}\">\
<td>{} {}</td><td>{status_cell}</td><td>{}</td></tr>\
<tr class='skill-detail-row'><td colspan='3'>\
<p><strong>{}</strong></p><ul class='plain'>{used}</ul>\
<p><strong>{}</strong></p><code>{}</code>{extra}</td></tr>",
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
        .map(|(name, _)| {
            let (title, description) = match *name {
                "calendar.create_event" => (
                    ctx.t("Create calendar event"),
                    ctx.t("Adds an event to your Google Calendar and invites the attendees."),
                ),
                "github.comment" => (
                    ctx.t("Comment on GitHub"),
                    ctx.t("Posts a comment on a GitHub issue or pull request."),
                ),
                "gitlab.comment" => (
                    ctx.t("Comment on GitLab"),
                    ctx.t("Posts a comment on a GitLab issue."),
                ),
                "mail.send" => (
                    ctx.t("Send email"),
                    ctx.t("Sends a plain-text email from your Gmail mailbox, to at most 10 recipients."),
                ),
                other => (other.to_string(), String::new()),
            };
            format!(
                "<li class='chat-tool-row'><label class='md-checkbox'><input type='checkbox' name='auto_approve' value='{n}'{}> \
<span class='chat-tool-text'><span class='chat-tool-title'>{}<code>{n}</code></span><span class='chat-tool-desc'>{}</span></span></label></li>",
                if chosen.iter().any(|c| c == name) { " checked" } else { "" },
                pyfmt::escape(&title),
                pyfmt::escape(&description),
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

#[cfg(test)]
mod tests {
    use crate::ui::routes::UiRequest;
    use crate::ui::state::UiState;

    fn page(user: &std::path::Path) -> String {
        let d = tempfile::tempdir().unwrap();
        let st = UiState::explicit(d.path(), d.path(), user, &d.path().join("update"), "tok", None);
        st.registry
            .dispatch(&st, &UiRequest::get("/settings?view=skills"))
            .unwrap()
            .body_text()
    }

    #[test]
    fn chat_settings_never_offer_to_skip_review_for_creator_writes() {
        let d = tempfile::tempdir().unwrap();
        let st = UiState::explicit(d.path(), d.path(), d.path(), &d.path().join("update"), "tok", None);
        let html = st
            .registry
            .dispatch(&st, &UiRequest::get("/settings?view=chat"))
            .unwrap()
            .body_text();
        // Creator writes always need the Approve card, so review can't be turned off here.
        assert_eq!(html.matches("name='auto_approve'").count(), 4, "{html}");
        assert!(html.contains("value='mail.send'"), "{html}");
        assert!(!html.contains("value='creator.") && !html.contains("<code>creator."), "{html}");
    }

    #[test]
    fn skills_page_lists_video_skills() {
        let user = tempfile::tempdir().unwrap();
        let none = page(user.path());
        assert!(none.contains("video-scout") && none.contains("inspire-video"), "{none}");
        assert!(none.contains("not installed"), "{none}");
        // Both are Claude Code plugins now: the page shows the two commands for each.
        for repo in ["video-scout", "inspire-video"] {
            assert!(none.contains(&format!("/plugin marketplace add encoreshao/{repo}")), "{none}");
            assert!(none.contains(&format!("/plugin install {repo}@{repo}")), "{none}");
        }
        assert!(!none.contains("git clone") && !none.contains("install.sh"), "{none}");
        assert!(none.contains("Creator page"), "{none}");
        assert_eq!(none.matches("action='/skills/install'").count(), 1, "{none}");

        let scout = user.path().join(".claude/skills/video-scout");
        std::fs::create_dir_all(&scout).unwrap();
        std::fs::write(scout.join("SKILL.md"), "x").unwrap();
        let inspire = user.path().join(".inspire-video/skills");
        std::fs::create_dir_all(inspire.join("inspire-video")).unwrap();
        std::fs::write(inspire.join("inspire-video/SKILL.md"), "x").unwrap();
        // An older clone has no showcase skill yet.
        let older = page(user.path());
        assert_eq!(older.matches("pill pill-green").count(), 2, "{older}");
        assert!(older.contains("/plugin install inspire-video@inspire-video"), "{older}");
        assert!(!older.contains("/plugin install video-scout@video-scout"), "{older}");
        std::fs::create_dir_all(inspire.join("inspire-showcase")).unwrap();
        std::fs::write(inspire.join("inspire-showcase/SKILL.md"), "x").unwrap();
        let both = page(user.path());
        assert_eq!(both.matches("pill pill-green").count(), 3, "{both}");
        assert!(!both.contains("/plugin install"), "{both}");
        // gitlab-config keeps its restart-the-dashboard setup button.
        assert!(both.contains("Set up &amp; restart dashboard"), "{both}");
    }

    #[test]
    fn skills_installed_as_plugins_are_installed_and_show_where_they_are() {
        let user = tempfile::tempdir().unwrap();
        let config = user.path().join(".claude");
        let scout = config.join("plugins/cache/video-scout/video-scout/1.0.0");
        let inspire = config.join("plugins/cache/inspire-video/inspire-video/abc123");
        for file in [
            scout.join("skills/video-scout/SKILL.md"),
            inspire.join("skills/inspire-video/SKILL.md"),
            inspire.join("skills/inspire-showcase/SKILL.md"),
        ] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "x").unwrap();
        }
        let record = serde_json::json!({"version": 2, "plugins": {
            "video-scout@video-scout": [{"installPath": scout}],
            "inspire-video@inspire-video": [{"installPath": inspire}],
        }});
        std::fs::write(config.join("plugins/installed_plugins.json"), record.to_string()).unwrap();
        let html = page(user.path());
        assert_eq!(html.matches("pill pill-green").count(), 3, "{html}");
        assert!(!html.contains("/plugin install"), "{html}");
        for file in ["1.0.0/skills/video-scout/SKILL.md", "abc123/skills/inspire-showcase/SKILL.md"] {
            assert!(html.contains(file), "{file}: {html}");
        }
    }
}
