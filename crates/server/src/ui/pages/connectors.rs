//! The Connectors hub (`/connectors`, views `accounts` and `add`) and its POST routes
//! (`bin/web/dashboard_server.py`, "Connectors page"): save, delete, test, the Connect
//! with Google sign-in start and the LinkedIn browser sign-in actions.

use super::loops_common::flash_html;
use crate::ui::badge::default_badge;
use crate::ui::hub::{self, render_hub_page};
use crate::ui::i18n::Lang;
use crate::ui::logos::{brand_color, brand_logo_svg};
use crate::ui::pyfmt::{self, escape as h};
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::state::UiState;
use crate::ui::consts;
use loopx_core::connectors::secrets::{open_connector_store, SecretStore};
use loopx_core::connectors::{self, Account, Auth, HttpClient, TypeInfo, UreqClient};
use loopx_core::connectors_config::{self as cc, Paths};
use loopx_core::oauth;
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// The Pending `kind` of a sign-in started from the Connectors page (the legacy
/// `kind: connector`): the callback stores the refresh token as the connector's secret
/// (`set_oauth_secret`) and redirects back to `/connectors` with a flash.
pub const FORM_KIND: &str = "connector-form";

pub fn register(r: &mut Registry) {
    r.get("/connectors", page);
    r.post("/connectors/save", save);
    r.post("/connectors/oauth/google/start", google_start);
    r.post("/connectors/linkedin/connect", linkedin_connect);
    r.post("/connectors/linkedin/disconnect", linkedin_disconnect);
    r.post("/connectors/delete", delete);
    r.post("/connectors/test", test);
}

// ---- state plumbing -----------------------------------------------------------------

pub fn paths(state: &UiState) -> Paths {
    Paths {
        config_path: Some(state.home_dir.join("connectors.json")),
        gitlab_config_path: Some(state.user_home.join(".gitlab").join("config.json")),
        slack_config_path: Some(state.user_home.join(".slack").join("config.json")),
        inbox_config_path: Some(state.home_dir.join("inboxes.json")),
    }
}

pub fn store(state: &UiState) -> Result<Arc<dyn SecretStore>, String> {
    match &state.connector_store {
        Some(s) => Ok(s.clone()),
        None => open_connector_store()
            .map(Arc::from)
            .map_err(|_| "Could not open the Keychain".to_string()),
    }
}

fn http(state: &UiState) -> Arc<dyn HttpClient> {
    state
        .connector_http
        .clone()
        .unwrap_or_else(|| Arc::new(UreqClient::default()))
}

pub fn results_path(state: &UiState) -> PathBuf {
    state
        .repo_root
        .join("outputs")
        .join("connectors")
        .join("test-results.json")
}

fn linkedin_dir(state: &UiState, id: &str) -> Option<PathBuf> {
    cc::is_valid_id(id).then(|| state.home_dir.join("linkedin").join(id))
}

fn linkedin_connected(state: &UiState, id: &str) -> bool {
    linkedin_dir(state, id).is_some_and(|d| d.join("connected").is_file())
}

// ---- small helpers ------------------------------------------------------------------

fn type_or_none(name: &str) -> Option<&'static TypeInfo> {
    connectors::get_type(name)
}

fn is_google(cls: Option<&TypeInfo>) -> bool {
    cls.is_some_and(|c| c.auth == Auth::OauthGoogle)
}

fn is_linkedin(cls: Option<&TypeInfo>) -> bool {
    cls.is_some_and(|c| c.auth == Auth::BrowserSession)
}

fn capability_label(ctx: &UiCtx, cap: &str) -> String {
    let label = consts::value("dashboard_server", "_CAPABILITY_LABELS")
        .and_then(|v| v.get(cap))
        .and_then(Value::as_str)
        .unwrap_or(cap);
    ctx.t(label)
}

fn chips(ctx: &UiCtx, caps: &[&str]) -> String {
    let mut caps = caps.to_vec();
    caps.sort_unstable();
    caps.iter()
        .map(|c| {
            format!(
                "<span class='pill pill-grey'>{}</span>",
                h(&capability_label(ctx, c))
            )
        })
        .collect()
}

fn owner_href(managed_by: &str) -> Option<&'static str> {
    consts::value("dashboard_server", "_CONNECTOR_OWNER_PAGES")
        .and_then(|v| v.get(managed_by))
        .and_then(Value::as_str)
}

fn owner_badge_text(ctx: &UiCtx, managed_by: &str) -> String {
    match managed_by {
        "gitlab-config" => ctx.t("Managed on GitLab page"),
        "slack-config" => ctx.t("Managed on Notification settings"),
        "inboxes" => ctx.t("Managed on Inbox Triage"),
        _ => ctx.t("Native"),
    }
}

fn str_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn brand_key(account: &Account) -> String {
    let cls = type_or_none(&account.kind);
    let fmt = account.setting("format");
    if account.kind == "mailbox" {
        return match account.setting("provider").to_lowercase().as_str() {
            "gmail" => "gmail",
            "outlook" => "microsoftoutlook",
            _ => "",
        }
        .to_string();
    }
    let Some(cls) = cls else {
        return String::new();
    };
    if !fmt.is_empty() {
        for p in cls.presets {
            if p.settings.iter().any(|(k, v)| *k == "format" && *v == fmt) {
                return p.brand.to_string();
            }
        }
    }
    cls.brand.to_string()
}

fn preset_for_account<'a>(cls: &'a TypeInfo, settings: &Map<String, Value>) -> Option<&'a connectors::Preset> {
    let fmt = str_of(settings.get("format"));
    if fmt.is_empty() {
        return None;
    }
    cls.presets
        .iter()
        .find(|p| p.settings.iter().any(|(k, v)| *k == "format" && *v == fmt))
}

/// The Google OAuth client saved on Inbox Triage setup (both fields set), else `None`.
pub fn google_oauth_client(state: &UiState) -> Option<(String, String)> {
    let text = std::fs::read_to_string(state.home_dir.join("mail_oauth.json")).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    let g = doc.get("google")?.as_object()?;
    let id = g.get("client_id")?.as_str().filter(|s| !s.is_empty())?;
    let secret = g.get("client_secret")?.as_str().filter(|s| !s.is_empty())?;
    Some((id.to_string(), secret.to_string()))
}

// ---- accounts view ------------------------------------------------------------------

fn account_row_html(ctx: &UiCtx, account: &Account, csrf: &str, result: Option<&Value>) -> String {
    let safe_id = h(&account.id);
    let quoted_id = pyfmt::quote(&account.id, "");
    let cls = type_or_none(&account.kind);
    let caps = cls.map(|c| chips(ctx, c.capabilities)).unwrap_or_default();
    let managed_by = account.managed_by.as_str();
    let badge = match owner_href(managed_by) {
        Some(href) => format!(
            "<a class='pill pill-link' href=\"{}\">{}</a>",
            h(href),
            h(&owner_badge_text(ctx, managed_by))
        ),
        None => format!(
            "<span class='pill pill-blue'>{}</span>",
            h(&owner_badge_text(ctx, managed_by))
        ),
    };
    let mut result_html = String::new();
    if let Some(Value::Object(r)) = result {
        let ok = r.get("ok").is_some_and(loopx_core::loops_config::py_truthy);
        let pill = if ok { "pill-green" } else { "pill-red" };
        result_html = format!(
            "<span class='pill {pill}' title=\"{}\">{}</span>",
            h(&r.get("at").map_or(String::new(), loopx_core::pydumps::py_str)),
            h(&r.get("message").map_or(String::new(), loopx_core::pydumps::py_str))
        );
    }
    let notify = cls.is_some_and(|c| c.has_capability("notify"));
    let test_label = if notify {
        ctx.t("Send test message")
    } else {
        ctx.t("Test")
    };
    let mut oauth_html = String::new();
    let mut connect_html = String::new();
    let connected_pill = |connected: bool| {
        format!(
            "<span class='pill {}'>{}</span>",
            if connected { "pill-green" } else { "pill-grey" },
            h(&if connected {
                ctx.t("Connected")
            } else {
                ctx.t("Not connected")
            })
        )
    };
    let btn = |connected: bool| if connected { "btn-neutral" } else { "btn-primary" };
    if is_google(cls) && managed_by == "native" {
        let connected = account.oauth_connected;
        oauth_html = connected_pill(connected);
        connect_html = format!(
            "<form method='post' action='/connectors/oauth/google/start' class='daemon-action-form'>{csrf}\
<input type='hidden' name='id' value=\"{safe_id}\">\
<button type='submit' class='btn {}'>\
<span class='material-symbols-outlined' aria-hidden='true'>login</span> {}</button></form>",
            btn(connected),
            h(&if connected {
                ctx.t("Reconnect")
            } else {
                ctx.t("Connect with Google")
            })
        );
    } else if is_linkedin(cls) && managed_by == "native" {
        let connected = linkedin_connected(ctx.state, &account.id);
        oauth_html = connected_pill(connected);
        connect_html = format!(
            "<form method='post' action='/connectors/linkedin/connect' class='daemon-action-form'>{csrf}\
<input type='hidden' name='id' value=\"{safe_id}\">\
<button type='submit' class='btn {}'>\
<span class='material-symbols-outlined' aria-hidden='true'>login</span> {}</button></form>",
            btn(connected),
            h(&if connected {
                ctx.t("Reconnect")
            } else {
                ctx.t("Connect LinkedIn")
            })
        );
        if connected {
            connect_html.push_str(&format!(
                "<form method='post' action='/connectors/linkedin/disconnect' class='daemon-action-form'>{csrf}\
<input type='hidden' name='id' value=\"{safe_id}\">\
<button type='submit' class='btn btn-neutral'>\
<span class='material-symbols-outlined' aria-hidden='true'>delete</span> {}</button></form>",
                h(&ctx.t("Disconnect"))
            ));
        }
    }
    let mut buttons = format!(
        "{connect_html}<form method='post' action='/connectors/test' class='daemon-action-form'>{csrf}\
<input type='hidden' name='id' value=\"{safe_id}\">\
<button type='submit' class='btn btn-neutral'><span class='material-symbols-outlined' aria-hidden='true'>\
{}</span> {}</button></form>",
        if notify { "send" } else { "check_circle" },
        h(&test_label)
    );
    if managed_by == "native" {
        let confirm = h(&ctx.tf("Delete connector {id}?", &[("id", &account.id)]));
        buttons.push_str(&format!(
            "<a class='btn btn-neutral' href='/connectors?view=add&amp;type={}&amp;id={}'>\
<span class='material-symbols-outlined' aria-hidden='true'>edit</span> {}</a>\
<form method='post' action='/connectors/delete' class='daemon-action-form'>{csrf}\
<input type='hidden' name='id' value=\"{safe_id}\">\
<button type='submit' class='btn btn-warning' data-confirm=\"{confirm}\">\
<span class='material-symbols-outlined' aria-hidden='true'>delete</span> {}</button></form>",
            pyfmt::quote(&account.kind, ""),
            h(&quoted_id),
            h(&ctx.t("Edit")),
            h(&ctx.t("Delete"))
        ));
    }
    let disabled = if account.enabled {
        String::new()
    } else {
        format!(" <span class='pill pill-grey'>{}</span>", h(&ctx.t("Disabled")))
    };
    format!(
        "<div class='project-block account-row'><div class='account-main'>\
<div class='connector-row-title'>{}<strong>{}</strong> <code>{safe_id}</code>{disabled}</div>\
<div class='pill-row'>{caps}{badge}{oauth_html}{result_html}</div></div>\
<div class='account-actions'>{buttons}</div></div>",
        brand_logo_svg(&brand_key(account), &account.label, "24", "brand-logo"),
        h(&account.label)
    )
}

fn accounts_body(ctx: &UiCtx, flash: Option<&str>, flash_ok: bool) -> String {
    let csrf = ctx.csrf_input();
    let (accounts, error_html) = match cc::list_accounts(&paths(ctx.state)) {
        Ok(a) => (a, String::new()),
        Err(e) => (Vec::new(), flash_html(Some(&e.0), false)),
    };
    let results = cc::read_test_results(&results_path(ctx.state));
    let mut order: Vec<String> = Vec::new();
    for a in &accounts {
        if !order.contains(&a.kind) {
            order.push(a.kind.clone());
        }
    }
    let mut cards = Vec::new();
    for type_name in &order {
        let cls = type_or_none(type_name);
        let label = cls.map_or_else(|| type_name.clone(), |c| ctx.t(c.label));
        let icon = cls.map_or("hub", |c| c.icon);
        let rows: String = accounts
            .iter()
            .filter(|a| &a.kind == type_name)
            .map(|a| account_row_html(ctx, a, &csrf, results.get(&a.id)))
            .collect();
        cards.push(format!(
            "<div class='card'><div class='section-header'>\
<span class='material-symbols-outlined' aria-hidden='true'>{}</span>\
<h2>{}</h2></div>{rows}</div>",
            h(icon),
            h(&label)
        ));
    }
    if cards.is_empty() && error_html.is_empty() {
        cards.push(format!(
            "<div class='card'><p class='section-subtitle'>{} <a href='/connectors?view=add'>{}</a></p></div>",
            h(&ctx.t("No connectors yet.")),
            h(&ctx.t("Add a connector"))
        ));
    }
    format!(
        "{}{}<p class='section-subtitle'>{}</p><div class='grid connector-accounts'>{}</div>",
        flash_html(flash, flash_ok),
        error_html,
        h(&ctx.t("Accounts the loops can read from or notify through.")),
        cards.join("")
    )
}

// ---- add view: the gallery ------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn tile_html(
    ctx: &UiCtx,
    href: &str,
    brand: &str,
    label: &str,
    description: &str,
    caps: &[&str],
    type_key: &str,
    preset_key: &str,
) -> String {
    let name = ctx.t(label);
    let alias = consts::value("dashboard_server", "_CONNECTOR_SEARCH_ALIASES")
        .and_then(|v| v.get(brand))
        .and_then(Value::as_str)
        .unwrap_or("");
    let search = [name.as_str(), label, type_key, preset_key, brand, alias]
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let desc = if description.is_empty() {
        String::new()
    } else {
        h(&ctx.t(description))
    };
    format!(
        "<a class='connector-tile' href='{href}' data-name=\"{}\" style='--brand:{}'>\
<span class='connector-tile-head'><span class='connector-tile-mark'>{}</span>\
<strong class='connector-tile-name'>{}</strong></span>\
<span class='connector-tile-desc' title=\"{desc}\">{desc}</span>\
<span class='connector-tile-caps'>{}</span></a>",
        h(&search),
        brand_color(brand),
        brand_logo_svg(brand, &name, "26", "brand-logo"),
        h(&name),
        chips(ctx, caps)
    )
}

fn type_picker_html(ctx: &UiCtx) -> String {
    let mut by_category: Vec<(String, Vec<(String, String)>)> = Vec::new();
    let mut push = |cat: &str, brand: &str, tile: String| {
        match by_category.iter_mut().find(|(c, _)| c == cat) {
            Some((_, v)) => v.push((brand.to_string(), tile)),
            None => by_category.push((cat.to_string(), vec![(brand.to_string(), tile)])),
        }
    };
    let mailbox_tiles = consts::list("dashboard_server", "_MAILBOX_TILES");
    for cls in connectors::all_types() {
        let quoted = pyfmt::quote(cls.name, "");
        if cls.external {
            for t in mailbox_tiles {
                let a = t.as_array().cloned().unwrap_or_default();
                let g = |i: usize| a.get(i).and_then(Value::as_str).unwrap_or("").to_string();
                let tile = tile_html(
                    ctx,
                    &format!("/loops/inbox-triage-loop?view=setup&amp;tab={}", g(4)),
                    &g(1),
                    &g(2),
                    &g(3),
                    cls.capabilities,
                    cls.name,
                    "",
                );
                push(&g(0), &g(1), tile);
            }
        } else if !cls.presets.is_empty() {
            for p in cls.presets {
                let href = format!(
                    "/connectors?view=add&amp;type={quoted}&amp;preset={}",
                    pyfmt::quote(p.key, "")
                );
                let desc = if p.description.is_empty() { cls.description } else { p.description };
                let tile = tile_html(ctx, &href, p.brand, p.label, desc, cls.capabilities, cls.name, p.key);
                let cat = if p.category.is_empty() { cls.category } else { p.category };
                push(cat, p.brand, tile);
            }
        } else {
            let tile = tile_html(
                ctx,
                &format!("/connectors?view=add&amp;type={quoted}"),
                cls.brand,
                cls.label,
                cls.description,
                cls.capabilities,
                cls.name,
                "",
            );
            push(cls.category, cls.brand, tile);
        }
    }
    let categories = consts::list("dashboard_server", "_CONNECTOR_CATEGORIES");
    let known: Vec<String> = categories
        .iter()
        .map(|c| c[0].as_str().unwrap_or("").to_string())
        .collect();
    let unknown: Vec<(String, Vec<(String, String)>)> = {
        let mut u = Vec::new();
        let mut kept = Vec::new();
        for entry in by_category.drain(..) {
            if known.contains(&entry.0) {
                kept.push(entry);
            } else {
                u.push(entry);
            }
        }
        by_category = kept;
        u
    };
    for (_, tiles) in unknown {
        match by_category.iter_mut().find(|(c, _)| c == "other") {
            Some((_, v)) => v.extend(tiles),
            None => by_category.push(("other".into(), tiles)),
        }
    }
    let google_order: Vec<String> = consts::list("dashboard_server", "_GOOGLE_TILE_ORDER")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let mut sections = String::new();
    for cat in categories {
        let key = cat[0].as_str().unwrap_or("");
        let heading = cat[1].as_str().unwrap_or("");
        let Some((_, tiles)) = by_category.iter().find(|(c, _)| c == key) else {
            continue;
        };
        if tiles.is_empty() {
            continue;
        }
        let mut tiles = tiles.clone();
        let mark = if key == "google" {
            tiles.sort_by_key(|(brand, _)| {
                google_order
                    .iter()
                    .position(|b| b == brand)
                    .unwrap_or(google_order.len())
            });
            brand_logo_svg("google", "", "20", "brand-logo")
        } else {
            String::new()
        };
        sections.push_str(&format!(
            "<section class='connector-category' data-category='{key}'>\
<h2>{mark}<span>{}</span>\
<span class='connector-count'>{}</span></h2>\
<div class='connector-grid'>{}</div></section>",
            h(&ctx.t(heading)),
            tiles.len(),
            tiles.iter().map(|(_, t)| t.as_str()).collect::<String>()
        ));
    }
    let search_label = h(&ctx.t("Search connectors"));
    let search = format!(
        "<div class='connector-search-wrap'>\
<span class='material-symbols-outlined' aria-hidden='true'>search</span>\
<input type='search' class='connector-search' id='connector-search' \
placeholder=\"{search_label}\" \
aria-label=\"{search_label}\"></div>"
    );
    let script = "<script>(function(){var q=document.getElementById('connector-search');if(!q)return;\
q.addEventListener('input',function(){var v=q.value.trim().toLowerCase();\
document.querySelectorAll('.connector-tile').forEach(function(t){\
t.hidden=!(!v||(t.getAttribute('data-name')||'').indexOf(v)!==-1);});\
document.querySelectorAll('.connector-category').forEach(function(s){\
var any=s.querySelector('.connector-tile:not([hidden])');\
s.hidden=!any;});});})();</script>";
    format!(
        "<div class='connector-gallery'>\
<p class='section-subtitle connector-gallery-intro'>{}</p>\
{search}{sections}</div>{script}",
        h(&ctx.t("Choose a connector type."))
    )
}

// ---- add view: the form -----------------------------------------------------------------

const TECH_INPUT_ATTRS: &str = " spellcheck='false' autocapitalize='off' autocomplete='off'";

/// A failed save's non-secret values, re-rendered.
#[derive(Debug, Clone, Default)]
pub struct Submitted {
    pub label: String,
    pub id: String,
    pub settings: Map<String, Value>,
    pub original_id: String,
    pub preset: String,
}

fn form_script(ctx: &UiCtx) -> String {
    format!(
        "<script>(function(){{var f=document.getElementById('connector-form');if(!f)return;\
var l=f.querySelector('[name=label]'),i=f.querySelector('[name=id]');\
if(l&&i&&f.getAttribute('data-suggest-id')==='1'){{var edited=i.value!=='';\
i.addEventListener('input',function(){{edited=true;}});\
l.addEventListener('input',function(){{if(edited)return;\
i.value=l.value.toLowerCase().replace(/[^a-z0-9]+/g,'-').replace(/^-+|-+$/g,'')\
.slice(0,48).replace(/-+$/,'');}});}}\
var b=f.querySelector('.secret-toggle'),s=f.querySelector('[name=secret]');\
if(b&&s){{var SHOW={},HIDE={};b.hidden=false;\
b.addEventListener('click',function(){{var show=s.type==='password';\
s.type=show?'text':'password';b.setAttribute('aria-pressed',show?'true':'false');\
b.textContent=show?HIDE:SHOW;}});}}\
}})();</script>",
        pyfmt::json_str(&ctx.t("Show")),
        pyfmt::json_str(&ctx.t("Hide"))
    )
}

fn form_row(ctx: &UiCtx, field_id: &str, label: &str, control: &str, required: bool, help: &str, wide: bool) -> String {
    let marker = if required {
        " <span class='field-required' aria-hidden='true'>*</span>".to_string()
    } else {
        format!(" <span class='field-optional'>{}</span>", h(&ctx.t("(optional)")))
    };
    let help_html = if help.is_empty() {
        String::new()
    } else {
        format!(
            "<span class='field-help section-subtitle' id='{field_id}-help'>{}</span>",
            h(help)
        )
    };
    let cls = if wide {
        "connector-field connector-field-wide"
    } else {
        "connector-field"
    };
    format!(
        "<div class='{cls}'><label for='{field_id}'>{}{marker}</label>{control}{help_html}</div>",
        h(label)
    )
}

fn described(field_id: &str, help: &str) -> String {
    if help.is_empty() {
        String::new()
    } else {
        format!(" aria-describedby='{field_id}-help'")
    }
}

fn setting_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => loopx_core::pydumps::py_str(other),
    }
}

fn form_body(
    ctx: &UiCtx,
    type_name: &str,
    account: Option<&Account>,
    preset_key: Option<&str>,
    submitted: Option<&Submitted>,
    google_client_missing: bool,
) -> String {
    let cls = type_or_none(type_name).expect("connector type");
    let csrf = ctx.csrf_input();
    let (original_id, label_value, id_value, settings, preset_key) = match submitted {
        Some(s) => (
            s.original_id.clone(),
            s.label.clone(),
            s.id.clone(),
            s.settings.clone(),
            Some(s.preset.clone()).filter(|p| !p.is_empty()),
        ),
        None => (
            account.map(|a| a.id.clone()).unwrap_or_default(),
            account.map(|a| a.label.clone()).unwrap_or_default(),
            account.map(|a| a.id.clone()).unwrap_or_default(),
            account.map(|a| a.settings.clone()).unwrap_or_default(),
            preset_key.map(str::to_string),
        ),
    };
    let editing = !original_id.is_empty();
    let preset = if editing {
        preset_for_account(cls, &settings)
    } else if let Some(k) = &preset_key {
        cls.presets.iter().find(|p| p.key == k.as_str())
    } else {
        None
    };
    let preset_settings: &[(&str, &str)] = match (preset, editing) {
        (Some(p), false) => p.settings,
        _ => &[],
    };
    let use_settings = editing || submitted.is_some();

    let type_title = match preset {
        Some(p) => ctx.t(p.label),
        None => ctx.t(cls.label),
    };
    let mut id_help = ctx.t("Lowercase letters, digits and dashes");
    if editing {
        id_help.push(' ');
        id_help.push_str(&ctx.t("Changing the id renames this connector."));
    }
    let account_rows = [form_row(
            ctx,
            "cf-label",
            &ctx.t("Label"),
            &format!(
                "<input type='text' id='cf-label' name='label' value='{}' placeholder=\"{}\" required>",
                h(&label_value),
                h(&ctx.tf("e.g. Work {name}", &[("name", &type_title)]))
            ),
            true,
            "",
            false,
        ),
        form_row(
            ctx,
            "cf-id",
            &ctx.t("Connector id"),
            &format!(
                "<input type='text' id='cf-id' name='id' value='{}' pattern='[a-z0-9][a-z0-9-]{{0,47}}' maxlength='48'{TECH_INPUT_ATTRS}{} required>",
                h(&id_value),
                described("cf-id", &id_help)
            ),
            true,
            &id_help,
            false,
        )];
    let mut hidden_format = String::new();
    let mut field_rows: Vec<String> = Vec::new();
    for field in cls.fields {
        let value = if use_settings {
            setting_text(settings.get(field.key))
        } else {
            preset_settings
                .iter()
                .find(|(k, _)| *k == field.key)
                .map_or(field.default, |(_, v)| *v)
                .to_string()
        };
        let field_id = format!(
            "cf-{}",
            field
                .key
                .to_lowercase()
                .chars()
                .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' { c } else { '-' })
                .collect::<String>()
        );
        let name = h(field.key);
        let req = if field.required { " required" } else { "" };
        let help_text = if field.help.is_empty() { String::new() } else { ctx.t(field.help) };
        let placeholder = if field.placeholder.contains(' ') {
            ctx.t(field.placeholder)
        } else {
            field.placeholder.to_string()
        };
        let ph = if placeholder.is_empty() {
            String::new()
        } else {
            format!(" placeholder='{}'", h(&placeholder))
        };
        if field.key == "format" {
            if let Some((_, fixed)) = preset_settings.iter().find(|(k, _)| *k == "format") {
                hidden_format = format!("<input type='hidden' name='format' value='{}'>", h(fixed));
                field_rows.push(format!(
                    "<div class='connector-field'><span class='connector-field-label'>{}</span>\
<span class='connector-fixed-value'><code>{}</code> \
<a href='/connectors?view=add'>{}</a></span></div>",
                    h(&ctx.t(field.label)),
                    h(fixed),
                    h(&ctx.t("Change"))
                ));
                continue;
            }
        }
        let attrs = format!("name='{name}' id='{field_id}'{}{req}", described(&field_id, &help_text));
        let mut wide = false;
        let control = match field.kind {
            "select" => {
                let opts: String = field
                    .options
                    .iter()
                    .map(|o| {
                        format!(
                            "<option value='{}'{}>{}</option>",
                            h(o),
                            if *o == value { " selected" } else { "" },
                            h(o)
                        )
                    })
                    .collect();
                format!("<select {attrs}>{opts}</select>")
            }
            "textarea" => {
                wide = true;
                format!("<textarea {attrs} rows='3'{ph}{TECH_INPUT_ATTRS}>{}</textarea>", h(&value))
            }
            kind => {
                let kind_attrs = match kind {
                    "url" => "type='url' inputmode='url'",
                    "email" => "type='email' inputmode='email'",
                    _ => "type='text'",
                };
                format!("<input {kind_attrs} {attrs} value='{}'{ph}{TECH_INPUT_ATTRS}>", h(&value))
            }
        };
        field_rows.push(form_row(ctx, &field_id, &ctx.t(field.label), &control, field.required, &help_text, wide));
    }
    let mut secret_html = String::new();
    if let Some(secret_label) = cls.secret_label {
        let placeholder = h(&if editing {
            ctx.t("•••• saved — leave blank to keep")
        } else {
            String::new()
        });
        let note = ctx.t("Stored in your macOS Keychain, never shown again.");
        let secret_input = format!(
            "<span class='secret-input-row'><input type='password' id='cf-secret' name='secret' \
autocomplete='new-password' spellcheck='false' autocapitalize='off' placeholder='{placeholder}' \
aria-describedby='cf-secret-help'{}>\
<button type='button' class='btn btn-neutral secret-toggle' aria-pressed='false' \
aria-controls='cf-secret' hidden>{}</button></span>",
            if editing { "" } else { " required" },
            h(&ctx.t("Show"))
        );
        secret_html = format!(
            "<fieldset class='connector-section'><legend>{}</legend>\
<div class='connector-fields'>{}</div></fieldset>",
            h(&ctx.t("Credentials")),
            form_row(ctx, "cf-secret", &ctx.t(secret_label), &secret_input, !editing, &note, true)
        );
    }
    let oauth = is_google(Some(cls));
    if oauth {
        let google_client_missing = google_client_missing || google_oauth_client(ctx.state).is_none();
        let connected = account.is_some_and(|a| a.oauth_connected);
        let status = if connected { ctx.t("Connected") } else { ctx.t("Not connected") };
        let missing_html = if google_client_missing {
            let href = consts::str("dashboard_server", "_GOOGLE_CLIENT_SETUP_HREF");
            format!(
                "<p class='connector-oauth-missing'>{} <a href=\"{}\">{}</a></p>",
                h(&ctx.t("Add a Google OAuth client on the Inbox Triage setup page first")),
                h(href),
                h(&ctx.t("Add a Google OAuth client"))
            )
        } else {
            String::new()
        };
        secret_html = format!(
            "<fieldset class='connector-section'><legend>{}</legend>\
<p class='section-subtitle'><span class='pill {}'>{}</span> {}</p>{missing_html}</fieldset>",
            h(&ctx.t("Google account")),
            if connected { "pill-green" } else { "pill-grey" },
            h(&status),
            h(&ctx.t("Sign in with Google to grant calendar access. The refresh token is stored in your macOS Keychain, never shown again."))
        );
    }
    let original = if editing {
        format!("<input type='hidden' name='original_id' value='{}'>", h(&original_id))
    } else {
        String::new()
    };
    let preset_input = match (preset, editing) {
        (Some(p), false) => format!("<input type='hidden' name='preset' value='{}'>", h(p.key)),
        _ => String::new(),
    };
    let (title, description, brand_key, logo) = match preset {
        Some(p) => {
            let title = ctx.t(p.label);
            let d = if p.description.is_empty() { cls.description } else { p.description };
            let logo = brand_logo_svg(p.brand, &title, "32", "brand-logo");
            (title, d.to_string(), p.brand.to_string(), logo)
        }
        None => {
            let title = ctx.t(cls.label);
            let bk = if cls.brand.is_empty() { type_name } else { cls.brand };
            let logo = if cls.brand.is_empty() {
                format!("<span class='material-symbols-outlined' aria-hidden='true'>{}</span>", h(cls.icon))
            } else {
                brand_logo_svg(cls.brand, &title, "32", "brand-logo")
            };
            (title, cls.description.to_string(), bk.to_string(), logo)
        }
    };
    let docs_url = match preset {
        Some(p) if !p.docs_url.is_empty() => p.docs_url,
        _ => cls.docs_url,
    };
    let docs_html = if docs_url.starts_with("https://") {
        format!(
            "<a class='connector-docs-link' href='{}' target='_blank' rel='noopener noreferrer'>{}\
<span class='material-symbols-outlined' aria-hidden='true'>open_in_new</span></a>",
            h(docs_url),
            h(&ctx.t("Where do I get this?"))
        )
    } else {
        String::new()
    };
    let desc_html = if description.is_empty() {
        String::new()
    } else {
        format!("<p class='section-subtitle'>{}</p>", h(&ctx.t(&description)))
    };
    let header = format!(
        "<div class='connector-hero' style='--brand:{}'>\
<span class='connector-tile-mark'>{logo}</span>\
<div class='connector-hero-text'><h2>{}</h2>{desc_html}</div></div>\
<div class='pill-row'>{}{docs_html}</div>",
        brand_color(&brand_key),
        h(&title),
        chips(ctx, cls.capabilities)
    );
    let connection_html = if field_rows.is_empty() {
        String::new()
    } else {
        format!(
            "<fieldset class='connector-section'><legend>{}</legend>\
<div class='connector-fields'>{}</div></fieldset>",
            h(&ctx.t("Connection")),
            field_rows.join("")
        )
    };
    let suggest = if editing { "" } else { " data-suggest-id='1'" };
    let save_and_test = format!(
        "<button type='submit' class='btn btn-neutral' name='then_test' value='1'>\
<span class='material-symbols-outlined' aria-hidden='true'>check_circle</span> {}</button>",
        h(&ctx.t("Save and test"))
    );
    let actions = if oauth {
        format!(
            "<button type='submit' class='btn btn-primary' formaction='/connectors/oauth/google/start'>\
<span class='material-symbols-outlined' aria-hidden='true'>login</span> {}</button>\
<button type='submit' class='btn btn-neutral'><span class='material-symbols-outlined' \
aria-hidden='true'>save</span> {}</button>{}",
            h(&ctx.t("Connect with Google")),
            h(&ctx.t("Save")),
            if editing { save_and_test.as_str() } else { "" }
        )
    } else {
        format!(
            "<button type='submit' class='btn btn-primary'><span class='material-symbols-outlined' \
aria-hidden='true'>save</span> {}</button>{save_and_test}",
            h(&ctx.t("Save"))
        )
    };
    format!(
        "<div class='card connector-form-card'>{header}\
<form method='post' action='/connectors/save' class='project-form connector-form' id='connector-form' \
autocomplete='off'{suggest}>\
{csrf}<input type='hidden' name='type' value='{}'>\
{hidden_format}{preset_input}{original}\
<fieldset class='connector-section'><legend>{}</legend>\
<div class='connector-fields'>{}</div></fieldset>\
{connection_html}{secret_html}\
<div class='connector-actions'>{actions}\
<a class='btn connector-cancel' href='/connectors'>{}</a></div>\
</form>{}</div>",
        h(type_name),
        h(&ctx.t("Account")),
        account_rows.join(""),
        h(&ctx.t("Cancel")),
        form_script(ctx)
    )
}

pub struct AddParams<'a> {
    pub type_name: Option<&'a str>,
    pub account_id: Option<&'a str>,
    pub preset: Option<&'a str>,
    pub flash: Option<&'a str>,
    pub flash_ok: bool,
    pub submitted: Option<&'a Submitted>,
    pub google_client_missing: bool,
}

fn add_body(ctx: &UiCtx, p: &AddParams) -> String {
    let flash = flash_html(p.flash, p.flash_ok);
    if let Some(sub) = p.submitted {
        let cls = p.type_name.and_then(type_or_none);
        return match cls {
            Some(c) if !c.external => format!(
                "{flash}{}",
                form_body(ctx, c.name, None, None, Some(sub), p.google_client_missing)
            ),
            _ => format!("{flash}{}", type_picker_html(ctx)),
        };
    }
    let mut type_name = p.type_name.filter(|t| !t.is_empty()).map(str::to_string);
    let mut account: Option<Account> = None;
    if let Some(id) = p.account_id.filter(|i| !i.is_empty()) {
        let list = match cc::list_accounts(&paths(ctx.state)) {
            Ok(l) => l,
            Err(e) => return format!("{}{}", flash_html(Some(&e.0), false), type_picker_html(ctx)),
        };
        let Some(found) = list.into_iter().find(|a| a.id == id) else {
            return format!(
                "{}{}",
                flash_html(Some(&ctx.tf("No connector with id {id}", &[("id", id)])), false),
                type_picker_html(ctx)
            );
        };
        if found.managed_by != "native" {
            let href = owner_href(&found.managed_by).unwrap_or("/connectors");
            return format!(
                "<div class='card'><p>{} <code>{}</code> <a class='pill pill-link' href=\"{}\">{}</a></p></div>",
                h(&found.label),
                h(id),
                h(href),
                h(&owner_badge_text(ctx, &found.managed_by))
            );
        }
        type_name = Some(found.kind.clone());
        account = Some(found);
    }
    let Some(type_name) = type_name else {
        return type_picker_html(ctx);
    };
    match type_or_none(&type_name) {
        Some(c) if !c.external => form_body(ctx, c.name, account.as_ref(), p.preset, None, false),
        _ => format!(
            "{}{}",
            flash_html(Some(&ctx.tf("Unknown connector type {type}", &[("type", &type_name)])), false),
            type_picker_html(ctx)
        ),
    }
}

fn render_page(ctx: &UiCtx, view: Option<&str>, p: &AddParams, flash: Option<&str>, flash_ok: bool) -> String {
    let hub_page = hub::hub_by_key("connectors").expect("connectors hub");
    let current = hub::resolve_view(&hub_page, view);
    let body = match current.key.as_str() {
        "add" => add_body(ctx, p),
        _ => accounts_body(ctx, flash, flash_ok),
    };
    render_hub_page(ctx, &hub_page, view, &body, &default_badge(ctx))
}

fn page(ctx: &UiCtx) -> UiResponse {
    let q = ctx.query();
    let (flash, ok) = ctx.flash();
    let p = AddParams {
        type_name: q.first("type"),
        account_id: q.first("id"),
        preset: q.first("preset"),
        flash: flash.as_deref(),
        flash_ok: ok,
        submitted: None,
        google_client_missing: false,
    };
    UiResponse::html(render_page(ctx, q.first("view"), &p, flash.as_deref(), ok))
}

// ---- POSTs ------------------------------------------------------------------------------

/// `parse_qs(keep_blank_values=True)` with the first value of each key.
fn form_first_keep_blank(body: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(body);
    let mut out: Vec<(String, String)> = Vec::new();
    for pair in text.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let k = pyfmt::unquote(&k.replace('+', " "));
        let v = pyfmt::unquote(&v.replace('+', " "));
        if !out.iter().any(|(n, _)| *n == k) {
            out.push((k, v));
        }
    }
    out
}

fn get<'a>(form: &'a [(String, String)], k: &str) -> &'a str {
    form.iter().find(|(n, _)| n == k).map_or("", |(_, v)| v.as_str())
}

fn submitted_values(form: &[(String, String)]) -> Submitted {
    let cls = type_or_none(get(form, "type"));
    let settings: Map<String, Value> = cls
        .map(|c| {
            c.fields
                .iter()
                .map(|f| (f.key.to_string(), Value::String(get(form, f.key).to_string())))
                .collect()
        })
        .unwrap_or_default();
    Submitted {
        label: get(form, "label").to_string(),
        id: get(form, "id").to_string(),
        settings,
        original_id: get(form, "original_id").trim().to_string(),
        preset: get(form, "preset").to_string(),
    }
}

fn redirect(ok: bool, message: &str) -> UiResponse {
    UiResponse::flash_redirect(ok, message, "/connectors")
}

pub fn run_test(ctx: &UiCtx, id: &str) -> (bool, String) {
    match store(ctx.state) {
        Ok(store) => cc::run_connector_test(
            id,
            store.as_ref(),
            &paths(ctx.state),
            http(ctx.state),
            &results_path(ctx.state),
        ),
        Err(_) => (false, ctx.tf("Request failed ({kind})", &[("kind", "KeychainError")])),
    }
}

fn renamed_or_deleted(ctx: &UiCtx, old: &str, new: Option<&str>) {
    // The loops' notify lists live in `<home>/loops.json`.
    cc::account_renamed_or_deleted_in(old, new, &results_path(ctx.state), &ctx.state.loops_config_path());
}

fn save(ctx: &UiCtx) -> UiResponse {
    let form = form_first_keep_blank(&ctx.req.body);
    let fields: Map<String, Value> = form
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "csrf_token" | "secret" | "original_id" | "preset" | "then_test"))
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    let secret = get(&form, "secret").to_string();
    let original_id = get(&form, "original_id").to_string();
    let store = match store(ctx.state) {
        Ok(s) => s,
        Err(m) => return rerender_failed(ctx, &form, &m),
    };
    let (ok, message) = cc::upsert_account(
        &Value::Object(fields),
        Some(&secret),
        &original_id,
        &paths(ctx.state),
        store.as_ref(),
    );
    if !ok {
        return rerender_failed(ctx, &form, &message);
    }
    let old_id = original_id.trim().to_string();
    let new_id = get(&form, "id").trim().to_string();
    if !old_id.is_empty() && !new_id.is_empty() && old_id != new_id {
        renamed_or_deleted(ctx, &old_id, Some(&new_id));
        if cc::is_valid_id(&old_id) && cc::is_valid_id(&new_id) {
            let (old, new) = (
                ctx.state.home_dir.join("linkedin").join(&old_id),
                ctx.state.home_dir.join("linkedin").join(&new_id),
            );
            if old.is_dir() && !new.exists() {
                let _ = std::fs::rename(old, new);
            }
        }
    }
    let (mut ok, mut message) = (ok, message);
    if get(&form, "then_test") == "1" {
        let (test_ok, test_message) = run_test(ctx, &new_id);
        ok = test_ok;
        message = format!("{message} — {test_message}");
    }
    redirect(ok, &message)
}

fn rerender_failed(ctx: &UiCtx, form: &[(String, String)], message: &str) -> UiResponse {
    let sub = submitted_values(form);
    let type_name = get(form, "type").to_string();
    let p = AddParams {
        type_name: Some(&type_name),
        account_id: None,
        preset: None,
        flash: Some(message),
        flash_ok: false,
        submitted: Some(&sub),
        google_client_missing: false,
    };
    UiResponse::html(render_page(ctx, Some("add"), &p, None, true))
}

fn id_param(ctx: &UiCtx) -> String {
    ctx.form().first("id").unwrap_or("").to_string()
}

fn delete(ctx: &UiCtx) -> UiResponse {
    let id = id_param(ctx);
    let store = match store(ctx.state) {
        Ok(s) => s,
        Err(m) => return redirect(false, &m),
    };
    let (ok, message) = cc::delete_account(&id, Some(&paths(ctx.state).config()), store.as_ref());
    if ok {
        renamed_or_deleted(ctx, &id, None);
        if let Some(d) = linkedin_dir(ctx.state, &id) {
            let _ = std::fs::remove_dir_all(d);
        }
    }
    redirect(ok, &message)
}

fn test(ctx: &UiCtx) -> UiResponse {
    let (ok, message) = run_test(ctx, &id_param(ctx));
    redirect(ok, &message)
}

fn linkedin_action(ctx: &UiCtx, action: &str) -> (bool, String) {
    let id = id_param(ctx);
    let account = cc::get_account(&id, &paths(ctx.state));
    let no_connector = || (false, ctx.tf("No connector with id {id}", &[("id", &id)]));
    let account = match account {
        Ok(Some(a)) => a,
        _ => return no_connector(),
    };
    let cls = type_or_none(&account.kind);
    if account.managed_by != "native" || !is_linkedin(cls) || !cc::is_valid_id(&id) {
        return (
            false,
            ctx.tf("Connector {id} does not use a browser sign-in", &[("id", &id)]),
        );
    }
    if action == "disconnect" {
        if let Some(d) = linkedin_dir(ctx.state, &id) {
            let _ = std::fs::remove_dir_all(d);
        }
        return (true, ctx.tf("Disconnected {id}", &[("id", &id)]));
    }
    if ctx.state.spawn_processes {
        let script = ctx.state.repo_root.join("bin").join("linkedin_login.py");
        let spawned = std::process::Command::new("python3")
            .arg(script)
            .arg(&id)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if spawned.is_err() {
            return (false, ctx.t("Could not open the sign-in window"));
        }
    }
    (
        true,
        ctx.t("A Chrome window opened - sign in to LinkedIn, then return here and press Test"),
    )
}

fn linkedin_connect(ctx: &UiCtx) -> UiResponse {
    let (ok, m) = linkedin_action(ctx, "connect");
    redirect(ok, &m)
}

fn linkedin_disconnect(ctx: &UiCtx) -> UiResponse {
    let (ok, m) = linkedin_action(ctx, "disconnect");
    redirect(ok, &m)
}

// ---- Connect with Google ---------------------------------------------------------------

fn urlencode(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", pyfmt::quote(k, "").replace("%20", "+"), pyfmt::quote(v, "").replace("%20", "+")))
        .collect::<Vec<_>>()
        .join("&")
}

fn authority_port(ctx: &UiCtx) -> String {
    ctx.req
        .header("host")
        .and_then(|h| h.rsplit_once(':'))
        .map(|(_, p)| p.to_string())
        .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        .map_or_else(|| "127.0.0.1".to_string(), |p| format!("127.0.0.1:{p}"))
}

enum Start {
    Redirect(String),
    Html(String),
    Flash(bool, String, Option<String>),
}

fn google_start_inner(ctx: &UiCtx) -> Start {
    let form = form_first_keep_blank(&ctx.req.body);
    let has_type = form.iter().any(|(k, _)| k == "type");
    let paths = paths(ctx.state);
    let (type_name, account_id);
    if has_type {
        type_name = get(&form, "type").to_string();
        let cls = type_or_none(&type_name);
        if !is_google(cls) {
            return Start::Flash(
                false,
                ctx.tf("{type} does not use Google sign-in", &[("type", &type_name)]),
                None,
            );
        }
        let fields: Map<String, Value> = form
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "csrf_token" | "secret" | "original_id" | "preset" | "then_test"))
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let store = match store(ctx.state) {
            Ok(s) => s,
            Err(m) => return Start::Flash(false, m, None),
        };
        let (ok, message) = cc::upsert_account(
            &Value::Object(fields),
            None,
            get(&form, "original_id"),
            &paths,
            store.as_ref(),
        );
        if !ok {
            let sub = submitted_values(&form);
            let p = AddParams {
                type_name: Some(&type_name),
                account_id: None,
                preset: None,
                flash: Some(&message),
                flash_ok: false,
                submitted: Some(&sub),
                google_client_missing: false,
            };
            return Start::Html(render_page(ctx, Some("add"), &p, None, true));
        }
        let old_id = get(&form, "original_id").trim().to_string();
        account_id = get(&form, "id").trim().to_string();
        if !old_id.is_empty() && old_id != account_id {
            renamed_or_deleted(ctx, &old_id, Some(&account_id));
        }
    } else {
        account_id = get(&form, "id").trim().to_string();
        let account = match cc::get_account(&account_id, &paths) {
            Ok(Some(a)) => a,
            Ok(None) => {
                return Start::Flash(
                    false,
                    ctx.tf("No connector with id {id}", &[("id", &account_id)]),
                    None,
                )
            }
            Err(e) => {
                return Start::Flash(
                    false,
                    ctx.tf("Could not read connectors: {detail}", &[("detail", &e.0)]),
                    None,
                )
            }
        };
        type_name = account.kind.clone();
        if account.managed_by != "native" || !is_google(type_or_none(&type_name)) {
            return Start::Flash(
                false,
                ctx.tf("Connector {id} does not use Google sign-in", &[("id", &account_id)]),
                None,
            );
        }
    }
    let Some((client_id, _)) = google_oauth_client(ctx.state) else {
        return Start::Flash(
            false,
            ctx.t("Add a Google OAuth client on the Inbox Triage setup page first"),
            Some(format!(
                "/connectors?view=add&type={}&id={}",
                pyfmt::quote(&type_name, ""),
                pyfmt::quote(&account_id, "")
            )),
        );
    };
    let redirect_uri = format!("http://{}/oauth/google/callback", authority_port(ctx));
    let (verifier, challenge) = oauth::pkce();
    let pending = oauth::Pending {
        provider: oauth::Provider::Google,
        kind: FORM_KIND.to_string(),
        account: account_id.clone(),
        verifier,
        redirect_uri: redirect_uri.clone(),
        app_scheme: None,
        created: oauth::now_secs(),
    };
    let state = match oauth::create_pending(&ctx.state.home_dir.join("oauth"), &pending) {
        Ok(s) => s,
        Err(_) => return Start::Flash(false, ctx.t("Could not start Google sign-in"), None),
    };
    let url = format!(
        "{}?{}",
        oauth::GOOGLE_AUTH_URL,
        urlencode(&[
            ("client_id", &client_id),
            ("redirect_uri", &redirect_uri),
            ("response_type", "code"),
            ("scope", oauth::CALENDAR_SCOPE),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("state", &state),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ])
    );
    Start::Redirect(url)
}

fn google_start(ctx: &UiCtx) -> UiResponse {
    match google_start_inner(ctx) {
        Start::Redirect(url) => UiResponse::redirect(303, &url),
        Start::Html(page) => UiResponse::html(page),
        Start::Flash(ok, message, location) => {
            UiResponse::flash_redirect(ok, &message, location.as_deref().unwrap_or("/connectors"))
        }
    }
}

/// The legacy `_connector_google_callback`: consume the single-use state, exchange the
/// code and store the refresh token as the connector's secret. The message never
/// carries the code or a token.
pub fn google_callback(
    state: &UiState,
    oauth_ctx: &oauth::OauthContext,
    lang: Lang,
    query_state: &str,
    code: Option<&str>,
    error: Option<&str>,
) -> (bool, String) {
    let t = |s: &str| crate::ui::i18n::t(lang, s, &[]);
    let tf = |s: &str, a: &[(&str, &str)]| crate::ui::i18n::t(lang, s, a);
    let now = oauth::now_secs();
    let dir = state.home_dir.join("oauth");
    let Some(pending) = oauth::consume_pending(&dir, query_state, now).filter(|p| p.kind == FORM_KIND) else {
        return (
            false,
            t("That sign-in link expired or was already used - click Connect again"),
        );
    };
    if let Some(e) = error.filter(|e| !e.is_empty()) {
        return (false, tf("Google sign-in was cancelled ({error})", &[("error", e)]));
    }
    let account_id = pending.account.clone();
    let paths = paths(state);
    let account = match cc::get_account(&account_id, &paths) {
        Ok(Some(a)) => a,
        _ => {
            return (
                false,
                tf("Connector {id} no longer exists - nothing was saved", &[("id", &account_id)]),
            )
        }
    };
    if account.managed_by != "native" || !is_google(type_or_none(&account.kind)) {
        return (
            false,
            tf("Connector {id} does not use Google sign-in", &[("id", &account_id)]),
        );
    }
    if google_oauth_client(state).is_none() {
        return (
            false,
            t("Add a Google OAuth client on the Inbox Triage setup page first"),
        );
    }
    let tokens = match oauth::exchange_code(
        oauth_ctx,
        oauth::Provider::Google,
        code.unwrap_or(""),
        &pending,
        now,
    ) {
        Ok(t) => t,
        Err(e) => return (false, e.to_string()),
    };
    if let Some(granted) = tokens.scope.as_deref().filter(|g| !g.is_empty()) {
        if !granted
            .split_whitespace()
            .any(|s| s == "https://www.googleapis.com/auth/calendar.readonly")
        {
            return (
                false,
                t("Google didn't grant calendar access - reconnect and keep the Calendar box ticked"),
            );
        }
    }
    let store = match store(state) {
        Ok(s) => s,
        Err(m) => return (false, m),
    };
    cc::set_oauth_secret(&account_id, tokens.refresh_token.as_deref(), &paths, store.as_ref())
}
