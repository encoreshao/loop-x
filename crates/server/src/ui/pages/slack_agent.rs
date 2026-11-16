//! The Slack agent's sections of its connector view
//! (`/connectors?view=add&type=slack_bot&id=<id>`): the app-level token row of
//! the connector form, the "Slack app" card (copy the app manifest, Test
//! connection) and the opt-in "Background listener" card with Install (behind a
//! confirmation sheet), Uninstall and Restart.
//!
//! Install, Uninstall and Restart call `loopx_core::slack_agent::install`
//! directly with this server's own environment copied unchanged into the plist
//! (`install::pinned_env_from`): the dashboard saved the tokens under the
//! Keychain service its own environment selects, so the listener must run with
//! the same one. The CLI's sandbox guard is not involved. The directories and
//! `launchctl` are the ones the Daemons page uses (`UiState::launchctl`, the
//! checkout's `launchd/`, `~/Library/LaunchAgents` under `UiState::user_home`).
//!
//! The listener lock is probed once per page load, never on a timer: the probe
//! takes the lock for an instant (a listener starting in that instant retries
//! for about 2 s before it gives up).

use super::connectors;
use super::loops_common::relative_time;
use crate::ui::icons;
use crate::ui::pyfmt::{self, escape as h};
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::state::UiState;
use loopx_core::connectors::slack_bot::{self, app_token_key};
use loopx_core::connectors::Account;
use loopx_core::connectors_config as cc;
use loopx_core::launchd::{self, Launchctl};
use loopx_core::slack_agent::install::{self, InstallError, Launch};
use loopx_core::slack_agent::listener::{default_state_dir, status_path};
use loopx_core::slack_agent::status::{effective_state, listener_lock_held, read_status, Effective};
use loopx_core::slack_agent::web::{SlackWeb, WebError};
use std::path::PathBuf;

pub const TYPE: &str = "slack_bot";
/// The app-level token's form field (the bot token is the connector's `secret`).
pub const APP_TOKEN_FIELD: &str = "app_token";

pub fn register(r: &mut Registry) {
    r.get("/connectors/slack/manifest", manifest);
    r.post("/connectors/slack/test", test_connection);
    r.post("/connectors/slack/install", install_post);
    r.post("/connectors/slack/uninstall", uninstall_post);
    r.post("/connectors/slack/restart", restart_post);
}

// ---- paths and seams --------------------------------------------------------------------

/// The connector view of account `id`.
pub fn view_href(id: &str) -> String {
    format!("/connectors?view=add&type={TYPE}&id={}", pyfmt::quote(id, ""))
}

/// The account the listener runs as: the first enabled Slack agent (what
/// `install::preflight` and `loopx slack listen` pick).
pub fn listener_account(state: &UiState) -> Option<String> {
    slack_bot::listener_account(cc::list_accounts(&connectors::paths(state)).ok()?).map(|a| a.id)
}

/// Where to manage the listener: its account's view, else a new Slack agent.
pub fn listener_href(state: &UiState) -> String {
    listener_account(state).map_or_else(|| format!("/connectors?view=add&type={TYPE}"), |id| view_href(&id))
}

/// The checkout's `launchd/` and the LaunchAgents folder, as on the Daemons page.
fn dirs(state: &UiState) -> (PathBuf, PathBuf) {
    (
        state.repo_root.join("launchd"),
        launchd::launch_agents_dir(&state.user_home),
    )
}

fn ctl(state: &UiState) -> Launchctl {
    Launchctl {
        command: state.launchctl.clone(),
        list_file: Some(state.repo_root.join("outputs").join("launchctl_list.txt")),
    }
}

/// What the plist points at. The loopx variables of this server's own
/// environment go in unchanged (only those that are set).
fn launch(state: &UiState) -> Result<Launch, String> {
    let bin = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(Launch {
        loopx_bin: bin,
        log_dir: state.repo_root.join("logs"),
        user_home: state.user_home.clone(),
        pinned_env: install::pinned_env_from(&|k| std::env::var(k).ok()),
    })
}

/// Whether this dashboard may run `launchctl` at all. A development sandbox
/// (scratch `HOME`) gets none, so it never reaches the user's real launchd
/// (`launchd::dashboard_launchctl`, picked in `UiState::from_env`).
fn launchd_available(state: &UiState) -> bool {
    !state.launchctl.is_empty()
}

fn no_launchd(ctx: &UiCtx) -> String {
    ctx.t("launchd is not available to this dashboard: it runs with a different home folder than your account (a development sandbox), so it cannot install, remove or restart the listener.")
}

fn is_installed(state: &UiState) -> bool {
    install::is_installed(&dirs(state).1)
}

/// Whether an account (by id) is a Slack agent.
pub fn is_slack_agent(state: &UiState, id: &str) -> bool {
    cc::get_account(id, &connectors::paths(state))
        .ok()
        .flatten()
        .is_some_and(|a| a.kind == TYPE)
}

// ---- messages ---------------------------------------------------------------------------

fn install_error(ctx: &UiCtx, e: &InstallError) -> String {
    match e {
        InstallError::NoAccount => ctx.t("No Slack agent is set up: add one in Settings > Connectors"),
        InstallError::Disabled(id) => ctx.tf("The Slack agent {id} is disabled: enable it first", &[("id", id)]),
        InstallError::NoBotToken => ctx.t("The Slack bot token (xoxb-) is not set"),
        InstallError::NoAppToken => ctx.t("The Slack app-level token (xapp-) is not set"),
        InstallError::EmptyAllowlist => ctx.t("No allowed Slack users are set: add at least one Slack user ID"),
        InstallError::SecretStore => ctx.t("Cannot read the Slack tokens from the secret store"),
        InstallError::Io(e) => ctx.tf("Cannot write the launch agent: {error}", &[("error", e)]),
        InstallError::Launchctl(e) => ctx.tf("launchctl failed: {error}", &[("error", e)]),
    }
}

/// [`WebError`] is token-free by construction; this only translates it.
fn web_error(ctx: &UiCtx, e: &WebError) -> String {
    match e {
        WebError::Auth => ctx.t("Slack rejected the token"),
        WebError::RateLimited { retry_after } => {
            ctx.tf("Slack rate limit, retry in {seconds}s", &[("seconds", &retry_after.to_string())])
        }
        WebError::MissingScope(s) if s.is_empty() => ctx.t("Slack scope missing"),
        WebError::MissingScope(s) => ctx.tf("Slack scope missing: {scope}", &[("scope", s)]),
        WebError::Api(code) => ctx.tf("Slack API error: {code}", &[("code", code)]),
        WebError::Net => ctx.t("Slack network error"),
    }
}

// ---- the connector form: tokens -----------------------------------------------------------

/// The token checks the save runs before storing anything. Messages never
/// carry the token.
pub fn check_tokens(ctx: &UiCtx, bot: &str, app: &str) -> Result<(), String> {
    let (bot, app) = (bot.trim(), app.trim());
    if !bot.is_empty() && !bot.starts_with("xoxb-") {
        return Err(ctx.t("The bot token must start with xoxb-"));
    }
    if !app.is_empty() && !app.starts_with("xapp-") {
        return Err(ctx.t("The app-level token must start with xapp-"));
    }
    if app.chars().any(char::is_control) {
        return Err(ctx.t("The app-level token must not contain control characters or line breaks"));
    }
    Ok(())
}

/// Store a non-blank app-level token for account `id` (blank keeps the saved one).
pub fn store_app_token(ctx: &UiCtx, id: &str, app: &str) -> Result<(), String> {
    let app = app.trim();
    if app.is_empty() {
        return Ok(());
    }
    let failed = || ctx.t("Could not store the secret in the Keychain");
    let store = connectors::store(ctx.state).map_err(|_| failed())?;
    store.set(&app_token_key(id), app).map_err(|_| failed())
}

fn app_token_saved(ctx: &UiCtx, id: &str) -> bool {
    connectors::store(ctx.state)
        .ok()
        .and_then(|s| s.get(&app_token_key(id)).ok().flatten())
        .is_some_and(|v| !v.trim().is_empty())
}

/// The app-level token row of the Credentials fieldset: a write-only input and
/// whether one is saved. Never the token itself.
pub fn app_token_row(ctx: &UiCtx, account_id: Option<&str>) -> String {
    let saved = account_id.is_some_and(|id| app_token_saved(ctx, id));
    let (pill, help) = if saved {
        (
            format!("<span class='pill pill-green'>{}</span>", h(&ctx.t("Saved"))),
            ctx.t("Stored in your macOS Keychain, never shown again. Leave blank to keep it."),
        )
    } else {
        (
            format!("<span class='pill pill-grey'>{}</span>", h(&ctx.t("Not set"))),
            ctx.t("The background listener needs it: in your Slack app, Basic Information, App-Level Tokens, with the connections:write scope."),
        )
    };
    let placeholder = if saved { ctx.t("•••• saved — leave blank to keep") } else { "xapp-…".to_string() };
    format!(
        "<div class='connector-field connector-field-wide'>\
<label for='cf-app-token'>{} <span class='field-optional'>{}</span></label>\
<input type='password' id='cf-app-token' name='{APP_TOKEN_FIELD}' autocomplete='new-password' spellcheck='false' \
autocapitalize='off' placeholder='{}' aria-describedby='cf-app-token-help'>\
<span class='field-help section-subtitle slack-token-help' id='cf-app-token-help'>{pill} {}</span></div>",
        h(&ctx.t("App-level token (xapp-)")),
        h(&ctx.t("(optional)")),
        h(&placeholder),
        h(&help)
    )
}

// ---- the extra cards ----------------------------------------------------------------------

const STYLE: &str = "<style>\
.slack-actions{display:flex;flex-wrap:wrap;gap:.6rem;align-items:center;margin-top:1rem}\
.slack-inline-form{display:inline-flex;margin:0}\
.slack-status{display:flex;flex-wrap:wrap;align-items:center;gap:.4rem .75rem;margin-top:.25rem}\
.slack-status .pill{height:1.75rem;box-sizing:border-box}\
.slack-status-detail{color:var(--md-on-surface-variant);font-size:.9rem;min-width:0;overflow-wrap:anywhere}\
.slack-hint{margin:.6rem 0 0}\
.slack-note{margin:.75rem 0 0;padding:.65rem .85rem;border-radius:12px;background:var(--md-surface-container-high);color:var(--md-on-surface);font-size:.88rem;line-height:1.45}\
.slack-why{margin:.6rem 0 0;color:var(--md-on-surface-variant);font-size:.85rem}\
.slack-pill-warn{background:var(--md-warning-container);color:var(--md-warning)}\
.slack-token-help{display:flex;flex-wrap:wrap;align-items:center;gap:.4rem}\
.slack-dialog{position:fixed;inset:0;z-index:1000;width:min(520px,calc(100vw - 2rem));height:fit-content;max-width:none;margin:auto;\
padding:1.25rem 1.4rem;box-sizing:border-box;border:1px solid var(--md-outline-variant);border-radius:16px;\
background:var(--md-nav-surface);color:inherit;box-shadow:var(--shadow-1),0 24px 60px rgba(0,0,0,.28)}\
.slack-dialog::backdrop{background:rgba(0,0,0,.35);backdrop-filter:blur(4px)}\
.slack-dialog h2{margin:0 0 .5rem;font-size:1.05rem}\
.slack-dialog p{margin:0 0 .5rem}\
.slack-disclosures{margin:.25rem 0 .75rem;padding-left:1.2rem;display:flex;flex-direction:column;gap:.4rem}\
.slack-disclosures code{overflow-wrap:anywhere}\
.slack-dialog-actions{display:flex;justify-content:flex-end;flex-wrap:wrap;gap:.6rem;margin-top:1rem}\
@media (prefers-reduced-motion:no-preference){.slack-dialog[open]{animation:slack-sheet-in 250ms cubic-bezier(0.32,0.72,0,1)}}\
@keyframes slack-sheet-in{from{opacity:0;transform:translateY(12px)}to{opacity:1;transform:none}}\
</style>";

/// What the Slack agent's view adds below the form: the "Slack app" card and,
/// for a saved account, the "Background listener" card.
pub fn extra_cards(ctx: &UiCtx, account: Option<&Account>) -> String {
    // The style sheet goes inside the first card: as a sibling it would take the
    // shared `.wrap > .card + *` gap meant for the card.
    let mut out = setup_card(ctx, account.map(|a| a.id.as_str()));
    if let Some(a) = account {
        let confirm = ctx.query().first("confirm") == Some("install");
        out.push_str(&listener_card(ctx, a, confirm));
    }
    out
}

fn hidden_id(id: &str) -> String {
    format!("<input type='hidden' name='id' value='{}'>", h(id))
}

/// Copy the app manifest, view it, and (for a saved account) Test connection.
fn setup_card(ctx: &UiCtx, id: Option<&str>) -> String {
    let csrf = ctx.csrf_input();
    let test = id.map_or_else(String::new, |id| {
        format!(
            "<form method='post' action='/connectors/slack/test' class='slack-inline-form'>{csrf}{}\
<button type='submit' class='btn btn-neutral'>{} {}</button></form>",
            hidden_id(id),
            icons::icon("check_circle"),
            h(&ctx.t("Test connection"))
        )
    });
    format!(
        "<div class='card slack-setup'>{STYLE}<div class='section-header'>{}<h2>{}</h2></div>\
<p class='section-subtitle'>{}</p>\
<div class='slack-actions'>\
<button type='button' class='btn btn-neutral' data-copy-target='slack-manifest' data-copied-label=\"{}\">\
<svg class='icon' aria-hidden='true'><use href='#i-content_copy'/></svg> <span data-copy-label>{}</span></button>\
<a class='btn btn-neutral' href='/connectors/slack/manifest' target='_blank' rel='noopener noreferrer'>{} {}</a>\
{test}</div><pre id='slack-manifest' hidden>{}</pre></div>",
        icons::icon("forum"),
        h(&ctx.t("Slack app")),
        h(&ctx.t("Create the Slack app from this manifest (api.slack.com/apps, Create New App, From a manifest) and install it to your workspace. Then paste its bot token and an app-level token above. Test connection checks both tokens and starts nothing.")),
        h(&ctx.t("Copied")),
        h(&ctx.t("Copy Slack app manifest")),
        icons::icon("open_in_new"),
        h(&ctx.t("View manifest")),
        h(&install::app_manifest_yaml())
    )
}

fn time_html(ctx: &UiCtx, iso: &str) -> String {
    format!(
        "<time datetime='{}' title='{}'>{}</time>",
        h(iso),
        h(iso),
        h(&relative_time(ctx.state.now(), iso))
    )
}

/// The status of the listener and what can be done about it. Every hint holds
/// for the state it is shown in (Restart only where launchd still has the agent).
pub(super) fn listener_card(ctx: &UiCtx, account: &Account, confirm: bool) -> String {
    let st = ctx.state;
    let (_, agents) = dirs(st);
    let installed = install::is_installed(&agents);
    let state_dir = default_state_dir(&st.repo_root);
    let status = read_status(&status_path(&state_dir));
    // Probed once, for this page load only (see the module doc).
    let lock = listener_lock_held(&state_dir);
    let effective = effective_state(status.as_ref(), lock, installed);
    let preflight = connectors::store(st)
        .map_err(|_| InstallError::SecretStore)
        .and_then(|s| install::preflight(&connectors::paths(st), s.as_ref()));
    let pill = |cls: &str, text: String| format!("<span class='pill {cls}'>{}</span>", h(&text));
    let detail = |html: String| format!("<span class='slack-status-detail'>{html}</span>");
    let log = st.repo_root.join("logs").join("slack-listener.err.log");
    let (badge, details, hint, restart) = match &effective {
        Effective::NotInstalled => (
            pill("pill-grey", ctx.t("Not installed")),
            String::new(),
            ctx.t("Nothing runs in the background until you install it. Saving this form or the tokens never starts it."),
            false,
        ),
        Effective::Stopped => (
            pill("pill-grey", ctx.t("Installed, not running")),
            String::new(),
            ctx.t("It starts at your next login. Restart starts it now; if that fails, uninstall and install again."),
            true,
        ),
        Effective::Offline { since } => (
            pill("slack-pill-warn", ctx.t("Offline")),
            detail(match since {
                Some(t) => h(&ctx.tf("since {time}", &[("time", "\u{1}")])).replace('\u{1}', &time_html(ctx, t)),
                None => h(&ctx.t("starting")),
            }),
            ctx.t("Running, but not connected to Slack. It reconnects by itself."),
            false,
        ),
        Effective::Connected { since, last_event } => (
            pill("pill-green", ctx.t("Connected")),
            format!(
                "{}{}",
                detail(h(&ctx.tf("since {time}", &[("time", "\u{1}")])).replace('\u{1}', &time_html(ctx, since))),
                detail(match last_event {
                    Some(t) => h(&ctx.tf("Last event {time}", &[("time", "\u{1}")])).replace('\u{1}', &time_html(ctx, t)),
                    None => h(&ctx.t("Last event: none yet")),
                })
            ),
            ctx.t("Mention the app in a channel or send it a direct message."),
            false,
        ),
        Effective::NeedsAttention { error } => (
            pill("pill-red", ctx.t("Needs attention")),
            detail(h(&error.clone().unwrap_or_else(|| ctx.t("see the log")))),
            if installed {
                ctx.t("The listener stopped and does not retry on its own. Fix the cause (usually a token: paste a new one above and Save), then Restart.")
            } else {
                ctx.t("Fix the cause (usually a token: paste a new one above and Save), then start the listener again.")
            },
            installed,
        ),
        Effective::Crashed => (
            pill("pill-red", ctx.t("Not running")),
            String::new(),
            ctx.tf(
                "It stopped unexpectedly, or was stopped outside Loop X. Restart it; if that fails, uninstall and install again. Log: {path}",
                &[("path", &log.display().to_string())],
            ),
            true,
        ),
    };
    // Installed but not running and preflight fails: `loopx slack listen` exits
    // 78 before writing any status and launchd relaunches it every 30 s, so
    // neither the login hint nor a stale status tells the truth.
    let hint = match (&effective, &preflight) {
        (Effective::Stopped | Effective::Crashed, Err(e)) if installed => ctx.tf(
            "It cannot start: {reason}. launchd tries again every 30 seconds and it keeps failing until this is fixed; fix it, Save, then Restart.",
            &[("reason", &install_error(ctx, e))],
        ),
        _ => hint,
    };
    let mut notes = String::new();
    let note = |text: String| format!("<p class='slack-note'>{}</p>", h(&text));
    if !installed && lock {
        notes.push_str(&note(ctx.t("Running by hand (loopx slack listen), not as an installed background listener.")));
    }
    if let Some(runs_as) = listener_account(st).filter(|r| *r != account.id) {
        notes.push_str(&note(ctx.tf(
            "The background listener runs as {id}, the first enabled Slack agent.",
            &[("id", &runs_as)],
        )));
    }
    let running = matches!(effective, Effective::Connected { .. } | Effective::Offline { .. });
    if account.setting(loopx_core::approval_relay::SETTING) == "on" && !running {
        notes.push_str(&note(ctx.t(
            "Slack approvals are on, but the Allow and Deny buttons in Slack only work while the listener runs. Approvals still show here in the dashboard and still expire on time.",
        )));
    }
    let csrf = ctx.csrf_input();
    let id = &account.id;
    let mut actions = String::new();
    let mut why = String::new();
    let mut sheet = String::new();
    if !launchd_available(st) {
        notes.push_str(&note(no_launchd(ctx)));
        if !installed {
            actions.push_str(&format!(
                "<button type='button' class='btn btn-primary' disabled>{} {}</button>",
                icons::icon("download"),
                h(&ctx.t("Install background listener"))
            ));
        }
    } else if !installed {
        let label = h(&ctx.t("Install background listener"));
        match &preflight {
            Ok(()) => {
                actions.push_str(&format!(
                    "<a class='btn btn-primary' href='{}&amp;confirm=install' \
onclick=\"document.getElementById('slack-install-sheet').showModal();return false\">{} {label}</a>",
                    h(&view_href(id)),
                    icons::icon("download")
                ));
                sheet = install_sheet(ctx, id, &agents.join(install::PLIST_NAME), confirm);
            }
            Err(e) => {
                actions.push_str(&format!(
                    "<button type='button' class='btn btn-primary' disabled aria-describedby='slack-install-why'>{} {label}</button>",
                    icons::icon("download")
                ));
                why = format!(
                    "<p class='slack-why' id='slack-install-why'>{}</p>",
                    h(&ctx.tf("Install needs this first: {reason}", &[("reason", &install_error(ctx, e))]))
                );
            }
        }
    } else {
        if restart {
            actions.push_str(&format!(
                "<form method='post' action='/connectors/slack/restart' class='slack-inline-form'>{csrf}{}\
<button type='submit' class='btn btn-primary'>{} {}</button></form>",
                hidden_id(id),
                icons::icon("refresh"),
                h(&ctx.t("Restart"))
            ));
        }
        actions.push_str(&format!(
            "<form method='post' action='/connectors/slack/uninstall' class='slack-inline-form'>{csrf}{}\
<button type='submit' class='btn btn-neutral' data-confirm=\"{}\">{} {}</button></form>",
            hidden_id(id),
            h(&ctx.t("Uninstall the background listener? Slack messages stop reaching Loop X until you install it again.")),
            icons::icon("delete"),
            h(&ctx.t("Uninstall"))
        ));
    }
    format!(
        "<div class='card slack-listener' id='slack-listener'><div class='section-header'>{}<h2>{}</h2></div>\
<div class='slack-status'>{badge}{details}</div>\
<p class='section-subtitle slack-hint'>{}</p>{notes}\
<div class='slack-actions'>{actions}</div>{why}{sheet}</div><!--/slack-listener-->",
        icons::icon("settings"),
        h(&ctx.t("Background listener")),
        h(&hint)
    )
}

/// The confirmation sheet: the four things installing means, then Cancel /
/// Install. `open` renders it already open (the no-JavaScript path).
fn install_sheet(ctx: &UiCtx, id: &str, plist: &std::path::Path, open: bool) -> String {
    let code = format!("<code>{}</code>", h(&plist.display().to_string()));
    format!(
        "<dialog id='slack-install-sheet' class='slack-dialog'{} aria-labelledby='slack-install-title'>\
<form method='post' action='/connectors/slack/install'>{}{}\
<h2 id='slack-install-title'>{}</h2><p>{}</p><ul class='slack-disclosures'>\
<li>{}</li><li>{}</li><li>{}</li><li>{}</li></ul>\
<p class='section-subtitle'>{}</p>\
<div class='slack-dialog-actions'>\
<a class='btn btn-neutral' href='{}' onclick=\"this.closest('dialog').close();return false\">{}</a>\
<button type='submit' class='btn btn-primary'>{} {}</button></div></form></dialog>{}",
        if open { " open" } else { "" },
        ctx.csrf_input(),
        hidden_id(id),
        h(&ctx.t("Install the background listener?")),
        h(&ctx.t("Installing it will:")),
        h(&ctx.tf("Write {path} and load it with launchd.", &[("path", "\u{1}")])).replace('\u{1}', &code),
        h(&ctx.t("Run in the background while you are logged in.")),
        h(&ctx.t("It can start agent work from Slack messages from the allowlisted users.")),
        h(&ctx.t("Everyone on the allowlist can use your agent as you: they get replies with your mail and calendar data by direct message and can answer your approvals.")),
        h(&ctx.t("Remove it any time with Uninstall here, or with loopx slack uninstall.")),
        h(&view_href(id)),
        h(&ctx.t("Cancel")),
        icons::icon("download"),
        h(&ctx.t("Install")),
        // Opened from the no-JavaScript link: make it a real modal sheet.
        if open {
            "<script>(function(){var d=document.getElementById('slack-install-sheet');\
if(d&&d.open&&d.showModal){d.close();d.showModal();}})();</script>"
        } else {
            ""
        }
    )
}

// ---- routes -----------------------------------------------------------------------------

fn manifest(_ctx: &UiCtx) -> UiResponse {
    UiResponse::text(200, install::app_manifest_yaml())
}

/// The posted account id, and where to go back to.
fn back(ctx: &UiCtx) -> (String, String) {
    let id = ctx.form().first("id").unwrap_or("").trim().to_string();
    let href = if cc::is_valid_id(&id) { view_href(&id) } else { "/connectors".to_string() };
    (id, href)
}

/// Checks the bot token (`auth.test`) and the app-level token
/// (`apps.connections.open`, whose URL is not kept). Starts nothing.
fn test_connection(ctx: &UiCtx) -> UiResponse {
    let (id, href) = back(ctx);
    if !is_slack_agent(ctx.state, &id) {
        return UiResponse::flash_redirect(false, &ctx.tf("No Slack agent with id {id}", &[("id", &id)]), "/connectors");
    }
    let store = match connectors::store(ctx.state) {
        Ok(s) => s,
        Err(_) => return UiResponse::flash_redirect(false, &ctx.t("Cannot read the Slack tokens from the secret store"), &href),
    };
    let secret = |key: &str| store.get(key).ok().flatten().filter(|v| !v.trim().is_empty());
    let (bot, app) = (secret(&id), secret(&app_token_key(&id)));
    let web = SlackWeb::new(
        bot.clone().unwrap_or_default(),
        app.clone().unwrap_or_default(),
        connectors::http(ctx.state),
    );
    let (bot_ok, bot_line) = match bot {
        None => (false, ctx.t("Bot token: not set")),
        Some(_) => match web.auth_test() {
            Ok(info) => {
                let who = if info.team.is_empty() { info.bot_user_id } else { info.team };
                (true, ctx.tf("Bot token: OK ({workspace})", &[("workspace", &who)]))
            }
            Err(e) => (false, ctx.tf("Bot token: {error}", &[("error", &web_error(ctx, &e))])),
        },
    };
    let (app_ok, app_line) = match app {
        None => (false, ctx.t("App-level token: not set")),
        Some(_) => match web.open_socket_url() {
            Ok(_) => (true, ctx.t("App-level token: OK")),
            Err(e) => (false, ctx.tf("App-level token: {error}", &[("error", &web_error(ctx, &e))])),
        },
    };
    UiResponse::flash_redirect(bot_ok && app_ok, &format!("{bot_line} · {app_line}"), &href)
}

fn install_post(ctx: &UiCtx) -> UiResponse {
    let (_, href) = back(ctx);
    let st = ctx.state;
    let fail = |m: String| UiResponse::flash_redirect(false, &m, &href);
    if !launchd_available(st) {
        return fail(no_launchd(ctx));
    }
    let store = match connectors::store(st) {
        Ok(s) => s,
        Err(_) => return fail(install_error(ctx, &InstallError::SecretStore)),
    };
    let launch = match launch(st) {
        Ok(l) => l,
        Err(e) => return fail(ctx.tf("Cannot find the loopx binary: {error}", &[("error", &e)])),
    };
    let (launchd_dir, agents) = dirs(st);
    match install::install(&connectors::paths(st), store.as_ref(), &launchd_dir, &agents, &ctl(st), &launch) {
        Ok(()) => UiResponse::flash_redirect(true, &ctx.t("Installed the background listener"), &href),
        Err(e) => fail(install_error(ctx, &e)),
    }
}

/// Unload and remove the agent. Also what deleting the account does.
pub fn uninstall(state: &UiState) -> Result<(), String> {
    let (launchd_dir, agents) = dirs(state);
    install::uninstall(&launchd_dir, &agents, &ctl(state))
}

/// Before a Slack agent account is deleted: remove the listener with it
/// (removing the connector removes the agent). `Ok(true)` when one was removed.
/// Without launchd it cannot be removed, so the delete is refused.
pub fn uninstall_for_delete(ctx: &UiCtx, id: &str) -> Result<bool, String> {
    let state = ctx.state;
    // Only a Slack agent takes the listener along, and only the one it runs as.
    // With no Slack agent enabled the listener has no account to run as (it
    // exits 78 and launchd relaunches it), so deleting any Slack agent then
    // removes it too instead of leaving it failing with nothing to manage it.
    if !is_slack_agent(state, id) || !is_installed(state) {
        return Ok(false);
    }
    if listener_account(state).is_some_and(|runs_as| runs_as != id) {
        return Ok(false);
    }
    if !launchd_available(state) {
        return Err(no_launchd(ctx));
    }
    uninstall(state).map(|()| true)
}

/// After a Slack agent's settings were saved: the listener reads its account
/// once at start, so an installed one is restarted for the allowlist, the
/// on/off switch and the rate to take effect. `None` when nothing is installed
/// (nothing to say); otherwise whether it worked and what to tell the user.
pub fn restart_after_save(ctx: &UiCtx) -> Option<(bool, String)> {
    let st = ctx.state;
    if !is_installed(st) {
        return None;
    }
    if !launchd_available(st) {
        return Some((
            true,
            ctx.t("The background listener keeps the old settings until it restarts: restart it with loopx slack restart."),
        ));
    }
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    Some(match install::restart(&dirs(st).1, &ctl(st), uid) {
        Ok(()) => (true, ctx.t("Restarted the background listener so the change takes effect")),
        Err(e) => (false, ctx.tf("Could not restart the background listener: {error}", &[("error", &e)])),
    })
}

fn uninstall_post(ctx: &UiCtx) -> UiResponse {
    let (_, href) = back(ctx);
    if !launchd_available(ctx.state) {
        return UiResponse::flash_redirect(false, &no_launchd(ctx), &href);
    }
    match uninstall(ctx.state) {
        Ok(()) => UiResponse::flash_redirect(true, &ctx.t("Removed the background listener"), &href),
        Err(e) => UiResponse::flash_redirect(
            false,
            &ctx.tf("Could not remove the background listener: {error}", &[("error", &e)]),
            &href,
        ),
    }
}

fn restart_post(ctx: &UiCtx) -> UiResponse {
    let (_, href) = back(ctx);
    if !launchd_available(ctx.state) {
        return UiResponse::flash_redirect(false, &no_launchd(ctx), &href);
    }
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    match install::restart(&dirs(ctx.state).1, &ctl(ctx.state), uid) {
        Ok(()) => UiResponse::flash_redirect(true, &ctx.t("Restarted the background listener"), &href),
        Err(e) => UiResponse::flash_redirect(
            false,
            &ctx.tf("Could not restart the background listener: {error}", &[("error", &e)]),
            &href,
        ),
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::routes::{UiRequest, UiResponse};
    use crate::ui::state::UiState;
    use axum::http::Method;
    use loopx_core::connectors::http::{HttpClient, HttpError, HttpRequest};
    use loopx_core::connectors::slack_bot::app_token_key;
    use loopx_core::keychain::{MemoryStore, SecretStore};
    use loopx_core::slack_agent::install;
    use loopx_core::slack_agent::status::{write_status, State, Status};
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use std::sync::Arc;

    const BOT: &str = "xoxb-TESTSECRET-bot";
    const APP: &str = "xapp-TESTSECRET-app";

    /// Answers the two Slack methods "Test connection" calls.
    struct Slack {
        reject: bool,
    }
    impl HttpClient for Slack {
        fn request_json(&self, req: &HttpRequest) -> Result<Value, HttpError> {
            if self.reject {
                return Ok(json!({"ok": false, "error": "invalid_auth"}));
            }
            Ok(match req.url.rsplit('/').next().unwrap_or("") {
                "auth.test" => json!({"ok": true, "user_id": "UBOT", "team": "Acme"}),
                "apps.connections.open" => json!({"ok": true, "url": "wss://x.example/?ticket=TESTSECRET-ticket"}),
                _ => json!({"ok": true}),
            })
        }
        fn get_text_tail(&self, _: &str, _: &[(String, String)], _: std::time::Duration, _: usize) -> Result<String, HttpError> {
            Ok(String::new())
        }
    }

    struct Sb {
        _d: tempfile::TempDir,
        st: UiState,
        store: Arc<MemoryStore>,
        log: PathBuf,
    }

    impl Sb {
        fn new() -> Self {
            Self::with_http(false)
        }
        fn with_http(reject: bool) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let d = tempfile::tempdir().unwrap();
            let p = |rel: &str| {
                let p = d.path().join(rel);
                std::fs::create_dir_all(&p).unwrap();
                p
            };
            let store = Arc::new(MemoryStore::new("t"));
            let now = chrono::DateTime::parse_from_rfc3339("2026-11-15T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
            let mut st = UiState::explicit(&p("root"), &p("home"), &p("userhome"), &p("update"), "tok", Some(now))
                .with_connectors(store.clone(), Arc::new(Slack { reject }));
            let log = d.path().join("launchctl.log");
            let fake = d.path().join("fake-launchctl");
            // Logs every call; `list` prints nothing (no agent loaded).
            std::fs::write(&fake, format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 0\n", log.display())).unwrap();
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
            st.launchctl = vec![fake.display().to_string()];
            Sb { _d: d, st, store, log }
        }
        fn agents(&self) -> PathBuf {
            loopx_core::launchd::launch_agents_dir(&self.st.user_home)
        }
        fn launchd_dir(&self) -> PathBuf {
            self.st.repo_root.join("launchd")
        }
        fn state_dir(&self) -> PathBuf {
            loopx_core::slack_agent::listener::default_state_dir(&self.st.repo_root)
        }
        fn installed(&self) -> bool {
            install::is_installed(&self.agents())
        }
        /// `launchctl` calls other than `list`.
        fn ctl_calls(&self) -> Vec<String> {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .filter(|l| *l != "list")
                .map(str::to_string)
                .collect()
        }
        fn account(&self, id: &str, bot: Option<&str>, app: Option<&str>, allowed: &str, approvals: &str) {
            let fields = json!({"id": id, "type": "slack_bot", "label": "Team Slack",
                                "allowed_users": allowed, "approvals": approvals, "rate_per_hour": "10"});
            let (ok, m) = loopx_core::connectors_config::upsert_account(
                &fields,
                bot,
                "",
                &super::super::connectors::paths(&self.st),
                self.store.as_ref(),
            );
            assert!(ok, "{m}");
            if let Some(app) = app {
                self.store.set(&app_token_key(id), app).unwrap();
            }
        }
        fn ready(&self) {
            self.account("agent", Some(BOT), Some(APP), "U01ABC", "off");
        }
        fn get(&self, path: &str) -> UiResponse {
            self.st.registry.dispatch(&self.st, &UiRequest::get(path)).expect("a route")
        }
        fn page(&self, path: &str) -> String {
            let r = self.get(path);
            assert_eq!(r.status, 200, "{path}: {}", r.body_text());
            r.body_text()
        }
        fn view(&self) -> String {
            self.page("/connectors?view=add&type=slack_bot&id=agent")
        }
        fn post(&self, path: &str, body: &str) -> UiResponse {
            self.st
                .registry
                .dispatch(&self.st, &UiRequest::new(Method::POST, path).with_body(body.to_string()))
                .expect("a route")
        }
        /// POST with the CSRF token, then follow the redirect.
        fn submit(&self, path: &str, body: &str) -> String {
            let r = self.post(path, &format!("csrf_token=tok&{body}"));
            assert_eq!(r.status, 303, "{path}: {}", r.body_text());
            let location = r.header("location").unwrap().to_string();
            self.page(&location)
        }
        fn status(&self, state: State, error: Option<&str>) {
            write_status(
                &loopx_core::slack_agent::listener::status_path(&self.state_dir()),
                &Status {
                    state,
                    since: "2026-11-15T11:00:00Z".into(),
                    last_event: Some("2026-11-15T11:58:00Z".into()),
                    last_error: error.map(str::to_string),
                },
            );
        }
        fn hold_lock(&self) -> loopx_core::file_lock::FileLock {
            std::fs::create_dir_all(self.state_dir()).unwrap();
            loopx_core::file_lock::FileLock::try_exclusive(&self.state_dir().join("listener.lock"))
                .unwrap()
                .unwrap()
        }
        /// An installed agent without going through launchctl.
        fn fake_installed(&self) {
            std::fs::create_dir_all(self.agents()).unwrap();
            std::fs::create_dir_all(self.launchd_dir()).unwrap();
            let plist = "<?xml version=\"1.0\"?>\n<plist version=\"1.0\"><dict><key>Label</key>\
<string>com.hermes.loopx-slack</string><key>RunAtLoad</key><true/></dict></plist>\n";
            std::fs::write(self.agents().join(install::PLIST_NAME), plist).unwrap();
            std::fs::write(self.launchd_dir().join(install::PLIST_NAME), plist).unwrap();
        }
    }

    fn assert_no_secret(text: &str) {
        assert!(!text.contains("TESTSECRET"), "a secret leaked: {text}");
    }

    /// The listener card alone.
    fn card(html: &str) -> &str {
        let start = html.find("id='slack-listener'").expect("the listener card");
        let end = html[start..].find("<!--/slack-listener-->").expect("the card's end");
        &html[start..start + end]
    }

    // ---- install / uninstall / restart ------------------------------------------------

    #[test]
    fn install_without_csrf_is_rejected_and_writes_nothing() {
        let sb = Sb::new();
        sb.ready();
        let r = sb.post("/connectors/slack/install", "id=agent");
        assert_eq!(r.status, 403);
        assert!(!sb.installed() && sb.ctl_calls().is_empty());
        assert!(!sb.launchd_dir().join(install::PLIST_NAME).exists());
    }

    #[test]
    fn install_with_a_failing_preflight_shows_why_and_writes_nothing() {
        let sb = Sb::new();
        sb.account("agent", Some(BOT), None, "U01ABC", "off");
        let page = sb.submit("/connectors/slack/install", "id=agent");
        assert!(page.contains("The Slack app-level token (xapp-) is not set"), "{page}");
        assert!(!sb.installed() && sb.ctl_calls().is_empty());
        assert!(!sb.launchd_dir().join(install::PLIST_NAME).exists());
        assert!(!sb.st.repo_root.join("logs").exists());
        assert_no_secret(&page);
    }

    #[test]
    fn install_then_uninstall_round_trip_with_a_fake_launchctl() {
        let sb = Sb::new();
        sb.ready();
        assert!(card(&sb.view()).contains("Not installed"));
        let page = sb.submit("/connectors/slack/install", "id=agent");
        assert!(sb.installed(), "{page}");
        let plist = sb.agents().join(install::PLIST_NAME);
        assert_eq!(sb.ctl_calls(), [format!("load -w {}", plist.display())]);
        let text = std::fs::read_to_string(&plist).unwrap();
        assert!(text.contains("<string>listen</string>"));
        assert_no_secret(&text);
        let c = card(&page);
        assert!(c.contains("Installed, not running") && c.contains("/connectors/slack/uninstall"), "{c}");
        assert!(!c.contains("Not installed"), "{c}");

        let page = sb.submit("/connectors/slack/uninstall", "id=agent");
        assert!(!sb.installed() && !sb.launchd_dir().join(install::PLIST_NAME).exists());
        assert_eq!(sb.ctl_calls().last().unwrap(), &format!("unload -w {}", plist.display()));
        assert!(card(&page).contains("Not installed"));
    }

    #[test]
    fn restart_kickstarts_the_installed_agent() {
        let sb = Sb::new();
        sb.ready();
        sb.fake_installed();
        sb.status(State::NeedsAttention, Some("Slack rejected the token (invalid_auth)"));
        sb.submit("/connectors/slack/restart", "id=agent");
        let uid = unsafe { libc::getuid() };
        assert_eq!(sb.ctl_calls(), [format!("kickstart -k gui/{uid}/com.hermes.loopx-slack")]);
    }

    // ---- the connector form ------------------------------------------------------------

    fn save_body(id: &str, original: &str, bot: &str, app: &str) -> String {
        format!(
            "type=slack_bot&label=Team+Slack&id={id}&original_id={original}&allowed_users=U01ABC&approvals=off\
&rate_per_hour=10&secret={bot}&app_token={app}"
        )
    }

    #[test]
    fn saving_the_tokens_stores_both_and_installs_nothing() {
        let sb = Sb::new();
        sb.submit("/connectors/save", &save_body("agent", "", BOT, APP));
        assert_eq!(sb.store.get("agent").unwrap().as_deref(), Some(BOT));
        assert_eq!(sb.store.get(&app_token_key("agent")).unwrap().as_deref(), Some(APP));
        assert!(!sb.installed() && sb.ctl_calls().is_empty());
        assert!(!sb.launchd_dir().join(install::PLIST_NAME).exists());
    }

    #[test]
    fn saving_while_the_listener_is_installed_restarts_it_once() {
        let sb = Sb::new();
        sb.ready();
        sb.fake_installed();
        let page = sb.submit("/connectors/save", &save_body("agent", "agent", "", ""));
        let uid = unsafe { libc::getuid() };
        assert_eq!(sb.ctl_calls(), [format!("kickstart -k gui/{uid}/com.hermes.loopx-slack")]);
        assert!(page.contains("Restarted the background listener so the change takes effect"), "{page}");
    }

    #[test]
    fn saving_without_an_installed_listener_calls_no_launchctl_and_says_nothing() {
        let sb = Sb::new();
        sb.ready();
        let page = sb.submit("/connectors/save", &save_body("agent", "agent", "", ""));
        assert!(sb.ctl_calls().is_empty() && !sb.log.exists());
        assert!(!page.contains("Restart"), "{page}");
        assert!(!page.contains("takes effect"), "{page}");
    }

    #[test]
    fn saving_without_launchd_says_a_restart_is_needed_and_calls_nothing() {
        let sb = without_launchd();
        sb.ready();
        sb.fake_installed();
        let page = sb.submit("/connectors/save", &save_body("agent", "agent", "", ""));
        assert!(!sb.log.exists(), "launchctl ran");
        assert!(page.contains("The background listener keeps the old settings until it restarts"), "{page}");
    }

    #[test]
    fn the_app_token_is_never_shown_only_whether_it_is_saved() {
        let sb = Sb::new();
        sb.account("agent", Some(BOT), None, "U01ABC", "off");
        let page = sb.view();
        assert!(page.contains("name='app_token'"), "{page}");
        assert!(page.contains("App-level token (xapp-)"));
        assert!(page.contains("Not set"));
        sb.store.set(&app_token_key("agent"), APP).unwrap();
        let page = sb.view();
        assert!(page.contains("Saved"), "{page}");
        assert_no_secret(&page);
        // A blank field on save keeps it.
        sb.submit("/connectors/save", &save_body("agent", "agent", "", ""));
        assert_eq!(sb.store.get(&app_token_key("agent")).unwrap().as_deref(), Some(APP));
    }

    #[test]
    fn tokens_with_the_wrong_prefix_are_refused_without_echoing_them() {
        let sb = Sb::new();
        for (bot, app, want) in [
            ("xapp-TESTSECRET-swapped", APP, "The bot token must start with xoxb-"),
            (BOT, "xoxb-TESTSECRET-swapped", "The app-level token must start with xapp-"),
        ] {
            let r = sb.post("/connectors/save", &format!("csrf_token=tok&{}", save_body("agent", "", bot, app)));
            let page = r.body_text();
            assert!(page.contains(want), "{page}");
            assert_no_secret(&page);
            assert_eq!(sb.store.get("agent").unwrap(), None);
            assert_eq!(sb.store.get(&app_token_key("agent")).unwrap(), None);
        }
    }

    #[test]
    fn renaming_moves_the_app_token() {
        let sb = Sb::new();
        sb.ready();
        sb.submit("/connectors/save", &save_body("team", "agent", "", ""));
        assert_eq!(sb.store.get(&app_token_key("team")).unwrap().as_deref(), Some(APP));
        assert_eq!(sb.store.get(&app_token_key("agent")).unwrap(), None);
    }

    #[test]
    fn deleting_the_account_removes_its_tokens_and_the_installed_listener() {
        let sb = Sb::new();
        sb.ready();
        sb.submit("/connectors/slack/install", "id=agent");
        assert!(sb.installed());
        let page = sb.submit("/connectors/delete", "id=agent");
        assert!(!sb.installed(), "{page}");
        assert!(!sb.launchd_dir().join(install::PLIST_NAME).exists());
        assert!(sb.ctl_calls().last().unwrap().starts_with("unload -w"));
        assert_eq!(sb.store.get("agent").unwrap(), None);
        assert_eq!(sb.store.get(&app_token_key("agent")).unwrap(), None);
    }

    #[test]
    fn deleting_another_slack_agent_leaves_the_listener_installed() {
        let sb = Sb::new();
        sb.ready();
        sb.account("other", Some(BOT), Some(APP), "U01ABC", "off");
        sb.fake_installed();
        let page = sb.submit("/connectors/delete", "id=other");
        assert!(sb.installed(), "{page}");
        assert!(sb.ctl_calls().is_empty(), "{:?}", sb.ctl_calls());
        assert_eq!(sb.store.get("other").unwrap(), None);
        // Its own account still takes the listener with it.
        sb.submit("/connectors/delete", "id=agent");
        assert!(!sb.installed());
    }

    /// Switch the account off without touching its tokens.
    fn disable(sb: &Sb, id: &str) {
        let fields = json!({"id": id, "type": "slack_bot", "label": "Team Slack",
                            "enabled": false, "allowed_users": "U01ABC", "approvals": "off",
                            "rate_per_hour": "10"});
        let (ok, m) = loopx_core::connectors_config::upsert_account(
            &fields,
            None,
            id, // the account being edited
            &super::super::connectors::paths(&sb.st),
            sb.store.as_ref(),
        );
        assert!(ok, "{m}");
    }

    #[test]
    fn deleting_the_only_slack_agent_while_it_is_disabled_still_removes_the_listener() {
        let sb = Sb::new();
        sb.ready();
        sb.submit("/connectors/slack/install", "id=agent");
        assert!(sb.installed());
        // Switched off: no enabled Slack agent is left, so the listener has nothing to run as.
        disable(&sb, "agent");
        let page = sb.submit("/connectors/delete", "id=agent");
        assert!(!sb.installed(), "the listener was left behind: {page}");
        assert!(sb.ctl_calls().last().unwrap().starts_with("unload -w"), "{:?}", sb.ctl_calls());
        assert_eq!(sb.store.get("agent").unwrap(), None);
        assert_eq!(sb.store.get(&app_token_key("agent")).unwrap(), None);
    }

    #[test]
    fn deleting_a_disabled_slack_agent_keeps_the_listener_of_another_enabled_one() {
        let sb = Sb::new();
        sb.ready();
        sb.account("other", Some(BOT), Some(APP), "U01ABC", "off");
        disable(&sb, "other");
        sb.fake_installed();
        sb.submit("/connectors/delete", "id=other");
        assert!(sb.installed(), "the enabled account's listener was removed");
        assert!(sb.ctl_calls().is_empty(), "{:?}", sb.ctl_calls());
    }

    #[test]
    fn deleting_another_kind_of_connector_never_touches_the_listener() {
        let sb = Sb::new();
        sb.ready();
        sb.submit("/connectors/slack/install", "id=agent");
        disable(&sb, "agent"); // no enabled Slack agent either
        let (ok, m) = loopx_core::connectors_config::upsert_account(
            &json!({"id": "hook", "type": "slack", "label": "Hook"}),
            Some("https://hooks.slack.com/services/T000/B000/XXXX"),
            "",
            &super::super::connectors::paths(&sb.st),
            sb.store.as_ref(),
        );
        assert!(ok, "{m}");
        let calls_before = sb.ctl_calls().len();
        sb.submit("/connectors/delete", "id=hook");
        assert!(sb.installed(), "an unrelated connector's delete removed the listener");
        assert_eq!(sb.ctl_calls().len(), calls_before, "{:?}", sb.ctl_calls());
    }

    #[test]
    fn deleting_an_account_without_a_listener_calls_no_launchctl() {
        let sb = Sb::new();
        sb.ready();
        sb.submit("/connectors/delete", "id=agent");
        assert!(sb.ctl_calls().is_empty());
    }

    // ---- manifest and test connection --------------------------------------------------

    #[test]
    fn the_manifest_is_served_as_yaml_and_offered_for_copying() {
        let sb = Sb::new();
        let r = sb.get("/connectors/slack/manifest");
        assert_eq!(r.status, 200);
        assert!(r.content_type().unwrap().starts_with("text/plain"));
        let yaml: Value = serde_norway::from_str(&r.body_text()).unwrap();
        assert_eq!(yaml["settings"]["socket_mode_enabled"], true);
        sb.ready();
        let page = sb.view();
        assert!(page.contains("Copy Slack app manifest") && page.contains("data-copy-target='slack-manifest'"), "{page}");
        assert!(page.contains("socket_mode_enabled: true"));
    }

    #[test]
    fn test_connection_checks_both_tokens_and_shows_no_secret() {
        let sb = Sb::new();
        sb.ready();
        let page = sb.submit("/connectors/slack/test", "id=agent");
        assert!(page.contains("Bot token: OK (Acme)"), "{page}");
        assert!(page.contains("App-level token: OK"), "{page}");
        assert_no_secret(&page);
        assert!(sb.ctl_calls().is_empty() && !sb.installed());

        let sb = Sb::with_http(true);
        sb.ready();
        let page = sb.submit("/connectors/slack/test", "id=agent");
        assert!(page.contains("Bot token: Slack rejected the token"), "{page}");
        assert!(page.contains("App-level token: Slack rejected the token"), "{page}");
        assert_no_secret(&page);
    }

    // ---- the status card ---------------------------------------------------------------

    #[test]
    fn the_card_shows_each_state_with_a_hint_that_holds() {
        let sb = Sb::new();
        sb.ready();
        let c = sb.view();
        let c = card(&c);
        assert!(c.contains("Not installed") && c.contains("Install background listener"), "{c}");
        assert!(!c.contains("/connectors/slack/restart"));

        sb.fake_installed();
        let page = sb.view();
        let c = card(&page);
        assert!(c.contains("Installed, not running") && c.contains("/connectors/slack/restart"), "{c}");

        let lock = sb.hold_lock();
        sb.status(State::Connected, None);
        let page = sb.view();
        let c = card(&page);
        assert!(c.contains("Connected") && c.contains("Last event"), "{c}");
        assert!(!c.contains("/connectors/slack/restart"), "{c}");

        sb.status(State::Offline, None);
        let page = sb.view();
        let c = card(&page);
        assert!(c.contains("Offline") && c.contains("reconnects by itself"), "{c}");

        sb.status(State::NeedsAttention, Some("Slack rejected the token (invalid_auth)"));
        drop(lock);
        let page = sb.view();
        let c = card(&page);
        assert!(c.contains("Needs attention") && c.contains("Slack rejected the token (invalid_auth)"), "{c}");
        assert!(c.contains("/connectors/slack/restart"), "{c}");

        sb.status(State::Connected, None);
        let page = sb.view();
        let c = card(&page);
        assert!(c.contains("Not running") && c.contains("/connectors/slack/restart"), "{c}");
        assert!(c.contains("uninstall and install again"), "{c}");
    }

    #[test]
    fn installed_with_a_failing_preflight_shows_the_reason_not_the_login_hint() {
        let sb = Sb::new();
        sb.account("agent", Some(BOT), None, "U01ABC", "off");
        sb.fake_installed();
        let page = sb.view();
        let c = card(&page);
        assert!(c.contains("The Slack app-level token (xapp-) is not set"), "{c}");
        assert!(c.contains("keeps failing until"), "{c}");
        assert!(!c.contains("starts at your next login"), "{c}");
        assert!(c.contains("/connectors/slack/restart"), "{c}");
        // A stale status from an earlier run (it now exits before writing one).
        sb.status(State::Connected, None);
        let page = sb.view();
        let c = card(&page);
        assert!(c.contains("The Slack app-level token (xapp-) is not set"), "{c}");
        assert!(!c.contains("stopped unexpectedly"), "{c}");
    }

    #[test]
    fn a_failing_preflight_disables_install_and_says_why() {
        let sb = Sb::new();
        sb.account("agent", Some(BOT), Some(APP), "not-a-user-id", "off");
        let page = sb.view();
        let c = card(&page);
        assert!(c.contains("No allowed Slack users are set"), "{c}");
        assert!(c.contains("disabled"), "{c}");
        assert!(!c.contains("slack-install-sheet"), "no confirmation sheet to open: {c}");
    }

    #[test]
    fn install_asks_first_in_a_sheet_with_the_four_disclosures() {
        let sb = Sb::new();
        sb.ready();
        let page = sb.view();
        let c = card(&page);
        assert!(c.contains("<dialog id='slack-install-sheet'"), "{c}");
        assert!(!c.contains("slack-dialog' open"), "closed until asked: {c}");
        let plist = sb.agents().join(install::PLIST_NAME);
        assert!(c.contains(&plist.display().to_string()), "{c}");
        assert!(c.contains("while you are logged in"));
        assert!(c.contains("can start agent work from Slack messages from the allowlisted users"));
        assert!(
            c.contains("Everyone on the allowlist can use your agent as you"),
            "the fourth disclosure: {c}"
        );
        let asked = sb.page("/connectors?view=add&type=slack_bot&id=agent&confirm=install");
        assert!(card(&asked).contains("slack-dialog' open"), "{asked}");
        assert!(!sb.installed());
    }

    #[test]
    fn slack_approvals_without_a_running_listener_get_a_note() {
        let sb = Sb::new();
        sb.account("agent", Some(BOT), Some(APP), "U01ABC", "on");
        assert!(card(&sb.view()).contains("Allow and Deny buttons in Slack only work while the listener runs"));
        let sb = Sb::new();
        sb.ready();
        assert!(!card(&sb.view()).contains("Allow and Deny buttons in Slack only work"));
    }

    // ---- elsewhere: Daemons and Needs you ----------------------------------------------

    #[test]
    fn the_daemons_page_shows_the_listener_as_managed_without_controls() {
        let sb = Sb::new();
        sb.fake_installed();
        let page = sb.page("/settings?view=daemons");
        assert!(page.contains("Managed by Slack settings"), "{page}");
        assert!(page.contains("/connectors?view=add&amp;type=slack_bot"), "{page}");
        assert!(!page.contains("/daemons/com.hermes.loopx-slack.plist/"), "{page}");
    }

    #[test]
    fn needs_you_lists_the_listener_only_when_it_needs_attention() {
        let sb = Sb::new();
        sb.ready();
        sb.fake_installed();
        sb.status(State::Connected, None);
        assert!(!sb.page("/needs-you").contains("Slack listener"));
        sb.status(State::NeedsAttention, Some("Slack rejected the token (invalid_auth)"));
        let page = sb.page("/needs-you");
        assert!(page.contains("The Slack listener needs attention: Slack rejected the token (invalid_auth)"), "{page}");
        assert!(page.contains("/connectors?view=add&amp;type=slack_bot&amp;id=agent"), "{page}");
        assert_no_secret(&page);
    }

    // ---- a dashboard without launchd (a development sandbox) ---------------------------

    /// What `UiState::from_env` picks for a scratch `HOME` with no fake injected.
    fn without_launchd() -> Sb {
        let mut sb = Sb::new();
        let elsewhere = sb._d.path().join("real-home");
        std::fs::create_dir_all(&elsewhere).unwrap();
        sb.st.launchctl = loopx_core::launchd::dashboard_launchctl(&sb.st.user_home, Some(&elsewhere), None);
        assert!(sb.st.launchctl.is_empty(), "{:?}", sb.st.launchctl);
        sb
    }

    #[test]
    fn without_launchd_install_says_so_and_writes_nothing() {
        let sb = without_launchd();
        sb.ready();
        let page = sb.view();
        let c = card(&page);
        assert!(c.contains("launchd is not available to this dashboard"), "{c}");
        assert!(!c.contains("slack-install-sheet"), "{c}");
        let page = sb.submit("/connectors/slack/install", "id=agent");
        assert!(page.contains("launchd is not available to this dashboard"), "{page}");
        assert!(!sb.installed() && !sb.launchd_dir().join(install::PLIST_NAME).exists());
        assert!(!sb.st.repo_root.join("logs").exists());
        assert!(!sb.log.exists(), "launchctl ran");
    }

    #[test]
    fn without_launchd_uninstall_and_restart_say_so_and_touch_nothing() {
        let sb = without_launchd();
        sb.ready();
        sb.fake_installed();
        sb.status(State::NeedsAttention, Some("Slack rejected the token (invalid_auth)"));
        let c = sb.view();
        let c = card(&c);
        assert!(c.contains("launchd is not available to this dashboard"), "{c}");
        assert!(!c.contains("/connectors/slack/restart") && !c.contains("/connectors/slack/uninstall"), "{c}");
        for path in ["/connectors/slack/uninstall", "/connectors/slack/restart"] {
            let page = sb.submit(path, "id=agent");
            assert!(page.contains("launchd is not available to this dashboard"), "{path}: {page}");
        }
        assert!(sb.installed() && sb.launchd_dir().join(install::PLIST_NAME).exists());
        assert!(!sb.log.exists(), "launchctl ran");
    }

    #[test]
    fn without_launchd_deleting_the_account_is_refused_while_a_listener_plist_exists() {
        let sb = without_launchd();
        sb.ready();
        sb.fake_installed();
        let page = sb.submit("/connectors/delete", "id=agent");
        assert!(page.contains("launchd is not available to this dashboard"), "{page}");
        assert!(sb.installed());
        assert_eq!(sb.store.get("agent").unwrap().as_deref(), Some(BOT));
        assert!(!sb.log.exists(), "launchctl ran");
    }

}
