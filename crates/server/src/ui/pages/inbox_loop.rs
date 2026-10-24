//! The Inbox Triage loop's page `/loops/inbox-triage-loop` (views Live, Drafts and
//! Setup) and the `/inbox/*` POSTs behind it (`/inbox/run-now` is in `runs.rs`).
//! Connect signs a Gmail mailbox in through the Google redirect (`google_callback` finishes
//! it); the Microsoft device-code flow is not available yet.

use super::loops_common::{custom_select, flash_html, render_loop_page};
use crate::ui::badge::py_title;
use crate::ui::hub::{builtin_loop_page, resolve_view};
use crate::ui::pyfmt;
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use chrono::{DateTime, FixedOffset, Local};
use loopx_core::mail_auth::{self, RefreshTokenStore};
use loopx_core::oauth;
use loopx_core::{content_archive, inbox_config, inbox_seen, inbox_status, mail_provider, pydumps};
use serde_json::{json, Value};

const BASE: &str = "/loops/inbox-triage-loop";
const INBOXES_TAB: &str = "/loops/inbox-triage-loop?view=setup&tab=inboxes";

pub fn register(r: &mut Registry) {
    r.get("/loops/inbox-triage-loop", page);
    r.post("/inbox/oauth-client", oauth_client);
    r.post("/inbox/inboxes", save_inbox);
    r.post("/inbox/inboxes/:name/:verb", inbox_action);
}

fn e(s: &str) -> String {
    pyfmt::escape(s)
}

fn page(ctx: &UiCtx) -> UiResponse {
    let (flash, flash_ok) = ctx.flash();
    let hub = builtin_loop_page("inbox-triage-loop").expect("inbox hub");
    let q = ctx.query();
    let view = q.first("view").map(str::to_string);
    let tab = q.first("tab").map(str::to_string);
    let (config, config_error) = config_for_page(ctx);
    let body = match resolve_view(&hub, view.as_deref()).key.as_str() {
        "drafts" => drafts_body(ctx),
        "setup" => format!(
            "{}{config_error}{}",
            flash_html(flash.as_deref(), flash_ok),
            setup_body(ctx, &config, tab.as_deref())
        ),
        _ => format!(
            "{}{config_error}{}",
            flash_html(flash.as_deref(), flash_ok),
            live_body(ctx, &config)
        ),
    };
    UiResponse::html(render_loop_page(ctx, &hub, "inbox-triage-loop", view.as_deref(), &body, None))
}

/// `_inbox_config_for_page`: a malformed inboxes.json renders as an empty config
/// plus a banner naming the file, never a 500.
fn config_for_page(ctx: &UiCtx) -> (Value, String) {
    let path = inbox_config::default_config_path();
    match inbox_config::load_config_or_empty(&path) {
        Ok(c) => (c, String::new()),
        Err(err) => {
            let message = ctx.tf(
                "Could not load {path} - fix or remove that file: {error}",
                &[("path", &path.display().to_string()), ("error", &err.to_string())],
            );
            (
                json!({"default_categories": inbox_config::default_categories(), "inboxes": []}),
                format!("<div class='flash flash-danger'>{}</div>", e(&message)),
            )
        }
    }
}

fn s(v: &Value, key: &str) -> String {
    match v.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(x) => pydumps::py_str(x),
    }
}

fn inboxes_of(config: &Value) -> Vec<Value> {
    config.get("inboxes").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn enabled(inbox: &Value) -> bool {
    inbox.get("enabled").and_then(Value::as_bool).unwrap_or(true)
}

fn status_entry(status: &Value, name: &str) -> Value {
    status
        .get("inboxes")
        .and_then(|i| i.get(name))
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn read_status(ctx: &UiCtx) -> Value {
    inbox_status::read(&inbox_seen::output_dir_for(&ctx.state.repo_root).join("status.json"))
}

/// `_STATE_LABELS` lookup: (label, pill class).
fn state_label(state: Option<&str>) -> (&'static str, &'static str) {
    match state {
        Some("idle") => ("Connected", "pill-green"),
        Some("running") => ("Running", "pill-blue"),
        Some("failed") => ("Failed", "pill-red"),
        Some("needs_reauth") => ("Needs re-auth", "pill-red"),
        None => ("Never run", "pill-grey"),
        Some(_) => ("Unknown", "pill-grey"),
    }
}

fn chip(ctx: &UiCtx, inbox: &Value, entry: &Value) -> String {
    if !enabled(inbox) {
        return format!("<span class='pill pill-grey'>{}</span>", e(&ctx.t("Paused")));
    }
    let (text, kind) = state_label(entry.get("state").and_then(Value::as_str));
    format!("<span class='pill {kind}'>{}</span>", e(&ctx.t(text)))
}

fn post_button(action: &str, label: &str, csrf: &str, extra_class: &str) -> String {
    let cls = if extra_class.is_empty() { "btn btn-neutral".to_string() } else { format!("btn {extra_class}") };
    format!(
        "<form method='POST' action='{}' class='daemon-action-form'>{csrf}<button type='submit' class='{cls}'>{}</button></form>",
        e(action),
        e(label)
    )
}

fn format_run_time(value: Option<&Value>) -> Option<String> {
    let text = value.map(pydumps::py_str).filter(|t| !t.is_empty())?;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() >= 16 && chars[10] == 'T' {
        let date: String = chars[..10].iter().collect();
        let time: String = chars[11..16].iter().collect();
        return Some(format!("{date} {time}"));
    }
    Some(text)
}

fn category_label(key: &str) -> String {
    if key.eq_ignore_ascii_case("fyi") {
        return "FYI".into();
    }
    let t = key.replace(['_', '-'], " ");
    let t = t.trim();
    let mut chars = t.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => String::new(),
    }
}

// ---- Live ---------------------------------------------------------------------------------------

/// `render_inbox_body`.
fn live_body(ctx: &UiCtx, config: &Value) -> String {
    let status = read_status(ctx);
    let inboxes = inboxes_of(config);
    let setup = format!("{BASE}?view=setup");
    let head = format!(
        "<div class='page-title'><h1>{}</h1><p class='subtitle'>{} <a href='{setup}'>{}</a> &middot; <a href='/inbox/history'>{}</a></p></div>",
        e(&ctx.t("Inbox Triage")),
        e(&ctx.t("Status for every connected inbox. Drafts are saved to your mailbox and never delivered automatically - nothing is archived, deleted, or sent on your behalf.")),
        e(&ctx.t("Setup")),
        e(&ctx.t("History")),
    );
    if inboxes.is_empty() {
        return format!(
            "{head}<div class='card'><div class='empty-state'><div class='empty-state-icon'><span class='material-symbols-outlined' aria-hidden='true'>email</span></div><p class='empty-state-message'>{}</p><a class='btn btn-primary empty-state-action' href='{setup}'><span class='material-symbols-outlined' aria-hidden='true'>settings</span> {}</a></div></div>",
            e(&ctx.t("No inboxes yet. Connect Gmail or Outlook from Inbox Setup to start triaging - drafts are saved to your mailbox and never delivered automatically.")),
            e(&ctx.t("Set up an inbox")),
        );
    }
    let csrf = ctx.csrf_input();
    let run_now = post_button("/inbox/run-now", &ctx.t("Run now"), &csrf, "btn-primary");
    let mut cards = format!(
        "<div class='card'><div class='inbox-card-head'><div><h2>{} <span class='inbox-count'>{}</span></h2><p class='section-subtitle'>{}</p></div><div class='daemon-action-form'>{run_now}</div></div></div>",
        e(&ctx.t("Inboxes")),
        inboxes.len(),
        e(&ctx.t("Trigger a triage pass for every connected inbox right now, outside its normal schedule.")),
    );
    for inbox in &inboxes {
        cards.push_str(&status_card(ctx, inbox, &status_entry(&status, &s(inbox, "name")), &csrf));
    }
    format!("{head}<div class='grid'>{cards}</div>")
}

/// `_inbox_status_card`.
fn status_card(ctx: &UiCtx, inbox: &Value, entry: &Value, csrf: &str) -> String {
    let name = s(inbox, "name");
    let pause_label = if enabled(inbox) { ctx.t("Pause") } else { ctx.t("Resume") };
    let pause = post_button(&format!("/inbox/inboxes/{name}/pause"), &pause_label, csrf, "");
    let when = match format_run_time(entry.get("last_run_at")) {
        Some(w) => e(&ctx.tf("last run {when}", &[("when", &w)])),
        None => e(&ctx.t("not run yet")),
    };
    let meta = format!("{} &middot; {} &middot; {when}", e(&s(inbox, "account")), e(&py_title(&s(inbox, "provider"))));

    let counts_html = match entry.get("counts").and_then(Value::as_object).filter(|c| !c.is_empty()) {
        Some(counts) => {
            let tiles: String = counts
                .iter()
                .map(|(k, v)| {
                    format!(
                        "<div class='inbox-stat'><span class='inbox-stat-value'>{}</span><span class='inbox-stat-label'>{}</span></div>",
                        e(&pydumps::py_str(v)),
                        e(&category_label(k))
                    )
                })
                .collect();
            format!("<div class='inbox-stats'>{tiles}</div>")
        }
        None => format!(
            "<p class='section-subtitle'>{}</p>",
            e(&ctx.t("No triage runs yet - counts per category appear after the first run."))
        ),
    };

    let urgent: Vec<Value> = entry.get("urgent").and_then(Value::as_array).cloned().unwrap_or_default();
    let urgent_html = if urgent.is_empty() {
        format!("<p class='section-subtitle'>{}</p>", e(&ctx.t("Nothing urgent in the last run.")))
    } else {
        let items: String = urgent
            .iter()
            .map(|item| {
                let link = s(item, "draft_link");
                let action = if !link.is_empty() {
                    format!(
                        "<a class='btn btn-neutral' href=\"{}\" target='_blank' rel='noopener'>{} &#8599;</a>",
                        e(&link),
                        e(&ctx.t("Open draft"))
                    )
                } else if item.get("draft_failed").is_some_and(loopx_core::loops_config::py_truthy) {
                    format!("<span class='pill pill-red'>{}</span>", e(&ctx.t("Draft failed - reply manually")))
                } else {
                    format!("<span class='pill pill-grey'>{}</span>", e(&ctx.t("Reply manually")))
                };
                format!(
                    "<li><div class='inbox-urgent-text'><span class='inbox-urgent-subject'>{}</span><span class='inbox-urgent-from'>{}</span></div>{action}</li>",
                    e(&s(item, "subject")),
                    e(&s(item, "from"))
                )
            })
            .collect();
        format!("<ul class='inbox-urgent-list'>{items}</ul>")
    };
    let error_html = match entry.get("error").filter(|v| loopx_core::loops_config::py_truthy(v)) {
        Some(err) => format!("<div class='flash flash-danger'>{}</div>", e(&pydumps::py_str(err))),
        None => String::new(),
    };
    format!(
        "<div class='card'><div class='inbox-card-head'><div><div class='section-header'><h2>{}</h2>{}</div><p class='section-subtitle'>{meta}</p></div><div class='daemon-action-form'>{pause}</div></div>{error_html}<div class='inbox-section'><h3>{}</h3>{counts_html}</div><div class='inbox-section'><h3>{} <span class='inbox-count'>{}</span></h3>{urgent_html}</div></div>",
        e(&s(inbox, "label")),
        chip(ctx, inbox, entry),
        e(&ctx.t("Categories")),
        e(&ctx.t("Urgent")),
        urgent.len(),
    )
}

// ---- Drafts -------------------------------------------------------------------------------------

fn saved_label(value: &Value) -> String {
    let text = pydumps::py_str(value).replace('Z', "+00:00");
    DateTime::<FixedOffset>::parse_from_rfc3339(&text)
        .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// `render_drafts_body`.
fn drafts_body(ctx: &UiCtx) -> String {
    let path = inbox_seen::output_dir_for(&ctx.state.repo_root).join("drafts.json");
    let records = content_archive::load(&path);
    if records.is_empty() {
        return format!(
            "<section class='card'><div class='section-header'><h2>{}</h2></div><p class='section-subtitle'>{}</p></section>",
            e(&ctx.t("Drafts")),
            e(&ctx.t("No drafts saved yet. Reply drafts appear here after Inbox Triage writes them."))
        );
    }
    records
        .iter()
        .map(|r| {
            let link = s(r, "draft_link");
            let open = if link.starts_with("http://") || link.starts_with("https://") {
                format!(
                    " <a class='btn btn-neutral' href=\"{}\" target='_blank' rel='noopener'>{}</a>",
                    e(&link),
                    e(&ctx.t("Open draft"))
                )
            } else {
                String::new()
            };
            let meta = [
                s(r, "inbox_label"),
                s(r, "from"),
                s(r, "category"),
                r.get("saved_at").map(saved_label).unwrap_or_default(),
            ]
            .iter()
            .filter(|x| !x.is_empty())
            .map(|x| e(x))
            .collect::<Vec<_>>()
            .join(" \u{b7} ");
            let subject = Some(s(r, "subject")).filter(|x| !x.is_empty()).unwrap_or_else(|| ctx.t("(no subject)"));
            format!(
                "<section class='card'><div class='section-header'><h2>{}</h2>{open}</div><p class='section-subtitle'>{meta}</p><pre class='draft-body'>{}</pre></section>",
                e(&subject),
                e(&s(r, "body"))
            )
        })
        .collect()
}

// ---- Setup --------------------------------------------------------------------------------------

const SETUP_TABS: [(&str, &str, &str); 4] = [
    ("inboxes", "Inboxes", "email"),
    ("add", "Add inbox", "add"),
    ("gmail", "Gmail app", "settings"),
    ("outlook", "Outlook app", "settings"),
];

fn hint(text_html: &str) -> String {
    if text_html.is_empty() {
        String::new()
    } else {
        format!("<small class='field-hint'>{text_html}</small>")
    }
}

fn labelled(label: &str, control: &str, hint_html: &str) -> String {
    format!("<label><span class='field-label'>{}</span>{control}{}</label>", e(label), hint(hint_html))
}

fn field(label_html: &str, control: &str, hint_html: &str) -> String {
    format!("<div class='stack-field'><span class='field-label'>{label_html}</span>{control}{}</div>", hint(hint_html))
}

fn section(title: &str, inner: &str, wide: bool) -> String {
    format!(
        "<div class='inbox-section{}'><h3>{}</h3><div class='section-fields'>{inner}</div></div>",
        if wide { " is-wide" } else { "" },
        e(title)
    )
}

fn lines(inbox: &Value, key: &str) -> String {
    let values: Vec<String> = inbox
        .get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().map(pydumps::py_str).collect())
        .unwrap_or_default();
    e(&values.join("\n"))
}

fn pairs(options: &[(&str, &str)]) -> Vec<(String, String)> {
    options.iter().map(|(v, l)| (v.to_string(), l.to_string())).collect()
}

/// `_inbox_fields`.
fn inbox_fields(ctx: &UiCtx, inbox: &Value, is_new: bool, bundles: &[String]) -> String {
    let readonly = if is_new { "" } else { " readonly" };
    let mut bundle_list = bundles.to_vec();
    let saved_bundle = s(inbox, "slack_bundle");
    if !saved_bundle.is_empty() && !bundle_list.contains(&saved_bundle) {
        bundle_list.push(saved_bundle.clone());
    }
    let sender_format = ctx.tf(
        "One email address per line, or {example} for a whole domain.",
        &[("example", "<code>@domain.com</code>")],
    );
    let provider_select = custom_select(
        "provider",
        &pairs(&[("gmail", "Gmail"), ("outlook", "Outlook")]),
        Some(&s(inbox, "provider")),
        None,
    );
    let account = format!(
        "{}{}{}{}",
        labelled(
            &ctx.t("Inbox ID"),
            &format!("<input type='text' name='name' value=\"{}\"{readonly} required pattern='[a-z0-9][a-z0-9-]*' placeholder='work-gmail'>", e(&s(inbox, "name"))),
            &if is_new { e(&ctx.t("Lowercase letters, numbers and dashes. It can't be changed later.")) } else { String::new() },
        ),
        field(&e(&ctx.t("Email provider")), &provider_select, ""),
        labelled(
            &ctx.t("Email address"),
            &format!("<input type='email' name='account' value=\"{}\" required placeholder='you@example.com'>", e(&s(inbox, "account"))),
            "",
        ),
        labelled(
            &ctx.t("Display name"),
            &format!("<input type='text' name='label' value=\"{}\" required>", e(&s(inbox, "label"))),
            &e(&ctx.t("Shown on the dashboard and in Slack alerts.")),
        ),
    );
    let triage = format!(
        "{}{}{}",
        labelled(
            &ctx.t("What counts as urgent"),
            &format!("<textarea name='urgent_brief' rows='2'>{}</textarea>", e(&s(inbox, "urgent_brief"))),
            &e(&ctx.t("Describe it in plain words, e.g. customer outages or invoices due this week.")),
        ),
        labelled(
            &ctx.t("VIP senders"),
            &format!("<textarea name='vip_senders' rows='2'>{}</textarea>", lines(inbox, "vip_senders")),
            &format!("{} {sender_format}", e(&ctx.t("Always marked urgent."))),
        ),
        labelled(
            &ctx.t("Private senders"),
            &format!("<textarea name='exclude_senders' rows='2'>{}</textarea>", lines(inbox, "exclude_senders")),
            &format!("{} {sender_format}", e(&ctx.t("Their emails are never shown to the AI."))),
        ),
    );
    let bundle_pairs: Vec<(String, String)> = bundle_list.iter().map(|b| (b.clone(), b.clone())).collect();
    let notifications = field(
        &e(&ctx.t("Slack bundle")),
        &custom_select("slack_bundle", &bundle_pairs, Some(&saved_bundle), Some(&ctx.t("(use default webhook)"))),
        &e(&ctx.t("Which Slack webhook gets this inbox's urgent alerts.")),
    );
    format!(
        "{}{}{}",
        section(&ctx.t("Account"), &account, false),
        section(&ctx.t("Triage rules"), &triage, true),
        section(&ctx.t("Notifications"), &notifications, false),
    )
}

/// `_inbox_form`: `inbox` is `None` for the Add form.
fn inbox_form(ctx: &UiCtx, inbox: Option<&Value>, bundles: &[String], form_id: &str, footer: &str) -> String {
    let is_new = inbox.is_none();
    let blank = json!({"name": "", "label": "", "provider": "gmail", "account": "", "urgent_brief": "",
        "vip_senders": [], "exclude_senders": [], "slack_bundle": null});
    let fields = inbox_fields(ctx, inbox.unwrap_or(&blank), is_new, bundles);
    format!(
        "<form method='POST' action='/inbox/inboxes' class='stack-form' id='{}'>{}<input type='hidden' name='is_new' value='{}'>{fields}{footer}</form>",
        e(form_id),
        ctx.csrf_input(),
        if is_new { "1" } else { "0" }
    )
}

fn state_chips(ctx: &UiCtx, inbox: &Value, entry: &Value) -> String {
    let (text, kind) = state_label(entry.get("state").and_then(Value::as_str));
    let mut chips = format!("<span class='pill {kind}'>{}</span>", e(&ctx.t(text)));
    if !enabled(inbox) {
        chips.push_str(&format!(" <span class='pill pill-grey'>{}</span>", e(&ctx.t("Paused"))));
    }
    chips
}

/// `_inbox_card`.
fn inbox_card(ctx: &UiCtx, inbox: &Value, entry: &Value, bundles: &[String]) -> String {
    let csrf = ctx.csrf_input();
    let name = s(inbox, "name");
    let (edit_id, delete_id) = (format!("inbox-edit-{name}"), format!("inbox-delete-{name}"));
    let hidden_forms: String = ["connect", "test", "disconnect", "delete"]
        .iter()
        .map(|verb| {
            format!(
                "<form method='POST' action='/inbox/inboxes/{0}/{verb}' id='inbox-{verb}-{0}' hidden>{csrf}</form>",
                e(&name)
            )
        })
        .collect();
    let buttons: String = [("connect", "Connect"), ("test", "Test connection"), ("disconnect", "Disconnect")]
        .iter()
        .map(|(verb, label)| {
            format!(
                "<button type='submit' form='inbox-{verb}-{}' class='btn btn-neutral'>{}</button>",
                e(&name),
                e(&ctx.t(label))
            )
        })
        .collect();
    let confirm = e(&ctx.tf(
        "Delete inbox {label}? This removes its settings, sign-in and saved state.",
        &[("label", &s(inbox, "label"))],
    ));
    let footer = format!(
        "<div class='inbox-card-footer'><button type='submit' form='{}' class='btn btn-primary'><span class='material-symbols-outlined' aria-hidden='true'>save</span> {}</button><button type='submit' form='{}' class='btn btn-warning' data-confirm=\"{confirm}\"><span class='material-symbols-outlined' aria-hidden='true'>delete</span> {}</button></div>",
        e(&edit_id),
        e(&ctx.t("Save")),
        e(&delete_id),
        e(&ctx.t("Delete")),
    );
    format!(
        "<div class='card'><div class='section-header'><h2>{}</h2>{}</div>{}{}{footer}{hidden_forms}</div>",
        e(&s(inbox, "label")),
        state_chips(ctx, inbox, entry),
        inbox_form(ctx, Some(inbox), bundles, &edit_id, ""),
        section(&ctx.t("Connection"), &format!("<div class='daemon-action-form'>{buttons}</div>"), false),
    )
}

fn google_steps(ctx: &UiCtx, redirect_uri: &str) -> String {
    let t = |text: &str, args: &[(&str, &str)]| ctx.tf(text, args);
    let step1 = t("Open {link} and create (or pick) a project.", &[("link", "<a href='https://console.cloud.google.com/projectcreate' target='_blank' rel='noopener'>console.cloud.google.com</a>")]);
    let step2 = t("Enable the {link}.", &[("link", "<a href='https://console.cloud.google.com/apis/library/gmail.googleapis.com' target='_blank' rel='noopener'>Gmail API</a>")]);
    let step3 = t(
        "Under {consent}, choose {external}, add yourself as a test user, then click {publish} so its status is {production} - a \"Testing\" app's sign-in expires every 7 days. An \"unverified app\" warning during sign-in is expected for personal use.",
        &[("consent", "<em>OAuth consent screen</em>"), ("external", "<strong>External</strong>"), ("publish", "<strong>Publish app</strong>"), ("production", "<strong>In production</strong>")],
    );
    let step4 = t(
        "Under {credentials}, choose {desktop}. Desktop clients accept this dashboard's loopback address automatically: {uri}",
        &[("credentials", "<em>Credentials &rarr; Create credentials &rarr; OAuth client ID</em>"), ("desktop", "<strong>Desktop app</strong>"), ("uri", &format!("<code>{}</code>", e(redirect_uri)))],
    );
    let step5 = t("Paste the client ID and client secret below.", &[]);
    format!("\n<ol class='wizard-steps'>\n  <li>{step1}</li>\n  <li>{step2}</li>\n  <li>{step3}</li>\n  <li>{step4}</li>\n  <li>{step5}</li>\n</ol>")
}

fn microsoft_steps(ctx: &UiCtx) -> String {
    let t = |text: &str, args: &[(&str, &str)]| ctx.tf(text, args);
    let step1 = t("Open {link} &rarr; {button}.", &[("link", "<a href='https://entra.microsoft.com/#view/Microsoft_AAD_RegisteredApps/ApplicationsListBlade' target='_blank' rel='noopener'>entra.microsoft.com &rarr; App registrations</a>"), ("button", "<strong>New registration</strong>")]);
    let step2 = t("Supported account types: {types}. Leave the redirect URI empty.", &[("types", "<strong>Accounts in any organizational directory and personal Microsoft accounts</strong>")]);
    let step3 = t("Under {section}, set {setting} to {value}.", &[("section", "<em>Authentication</em>"), ("setting", "<strong>Allow public client flows</strong>"), ("value", "<strong>Yes</strong>")]);
    let step4 = t(
        "Under {section}, add Microsoft Graph delegated permissions {read_write} and {offline}. Do not add {send}.",
        &[("section", "<em>API permissions</em>"), ("read_write", "<code>Mail.ReadWrite</code>"), ("offline", "<code>offline_access</code>"), ("send", "<code>Mail.Send</code>")],
    );
    let step5 = t("Paste the {field} below. No secret is needed.", &[("field", "<strong>Application (client) ID</strong>")]);
    format!("\n<ol class='wizard-steps'>\n  <li>{step1}</li>\n  <li>{step2}</li>\n  <li>{step3}</li>\n  <li>{step4}</li>\n  <li>{step5}</li>\n</ol>")
}

/// `_client_form`.
fn client_form(ctx: &UiCtx, provider: &str, title: &str, oauth: &serde_json::Map<String, Value>, with_secret: bool) -> String {
    let client_id = oauth.get(provider).map(|c| s(c, "client_id")).unwrap_or_default();
    let configured = if client_id.is_empty() {
        format!("<span class='pill pill-grey'>{}</span>", e(&ctx.t("Not set")))
    } else {
        format!("<span class='pill pill-green'>{}</span>", e(&ctx.t("Saved")))
    };
    let secret = if with_secret {
        labelled(
            &ctx.t("Client secret"),
            "<input type='password' name='client_secret' autocomplete='off'>",
            &e(&ctx.t("Leave blank to keep the saved secret.")),
        )
    } else {
        String::new()
    };
    format!(
        "<form method='POST' action='/inbox/oauth-client' class='stack-form wizard-client'>{}<input type='hidden' name='provider' value='{provider}'><h3>{} {configured}</h3>{}{secret}<button type='submit' class='btn btn-primary'>{}</button></form>",
        ctx.csrf_input(),
        e(title),
        labelled(&ctx.t("Client ID"), &format!("<input type='text' name='client_id' value=\"{}\" required>", e(&client_id)), ""),
        e(&ctx.t("Save")),
    )
}


fn slack_bundles(ctx: &UiCtx) -> Vec<String> {
    std::fs::read_to_string(ctx.state.user_home.join(".gitlab").join("config.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("bundles").and_then(Value::as_object).map(|m| m.keys().cloned().collect()))
        .unwrap_or_default()
}

/// `render_setup_body`.
fn setup_body(ctx: &UiCtx, config: &Value, requested_tab: Option<&str>) -> String {
    let status = read_status(ctx);
    let oauth = inbox_config::load_oauth(&inbox_config::default_oauth_path());
    let inboxes = inboxes_of(config);
    let bundles = slack_bundles(ctx);
    let host = ctx.req.header("host").unwrap_or("127.0.0.1:8420").to_string();
    let redirect_uri = format!("http://{host}/oauth/google/callback");
    let active = match requested_tab {
        Some(t) if SETUP_TABS.iter().any(|(k, _, _)| *k == t) => t.to_string(),
        _ => if inboxes.is_empty() { "gmail" } else { "inboxes" }.to_string(),
    };

    let inboxes_panel = if inboxes.is_empty() {
        format!(
            "<div class='card'><div class='empty-state'><div class='empty-state-icon'><span class='material-symbols-outlined' aria-hidden='true'>email</span></div><p class='empty-state-message'>{}</p><a class='btn btn-primary empty-state-action' href='{BASE}?view=setup&tab=add'><span class='material-symbols-outlined' aria-hidden='true'>add</span> {}</a></div></div>",
            e(&ctx.t("No inboxes yet. First fill in the Gmail app or Outlook app tab, then add an inbox.")),
            e(&ctx.t("Add inbox")),
        )
    } else {
        inboxes
            .iter()
            .map(|i| inbox_card(ctx, i, &status_entry(&status, &s(i, "name")), &bundles))
            .collect()
    };
    let add_footer = format!(
        "<div class='inbox-card-footer'><button type='submit' class='btn btn-primary'><span class='material-symbols-outlined' aria-hidden='true'>add</span> {}</button></div>",
        e(&ctx.t("Add inbox"))
    );
    let add_panel = format!(
        "<div class='card'><div class='section-header'><h2>{}</h2></div><p class='section-subtitle'>{}</p>{}</div>",
        e(&ctx.t("Add an inbox")),
        e(&ctx.t("After adding it, sign in from the Inboxes tab with Connect.")),
        inbox_form(ctx, None, &bundles, "inbox-add-form", &add_footer),
    );
    let gmail_panel = format!(
        "<div class='card'><div class='section-header'><h2>{}</h2></div>{}{}</div>",
        e(&ctx.t("Connect Gmail")),
        google_steps(ctx, &redirect_uri),
        client_form(ctx, "google", &ctx.t("Google OAuth client"), &oauth, true),
    );
    let outlook_panel = format!(
        "<div class='card'><div class='section-header'><h2>{}</h2></div>{}{}</div>",
        e(&ctx.t("Connect Outlook")),
        microsoft_steps(ctx),
        client_form(ctx, "microsoft", &ctx.t("Microsoft app"), &oauth, false),
    );

    let tab_buttons: String = SETUP_TABS
        .iter()
        .map(|(key, label, icon)| {
            let on = *key == active;
            format!(
                "<button type='button' class='tab-button{}' data-tab-target='{key}' role='tab' aria-selected='{}'><span class='material-symbols-outlined' aria-hidden='true'>{icon}</span>{}</button>",
                if on { " is-active" } else { "" },
                if on { "true" } else { "false" },
                e(&ctx.t(label))
            )
        })
        .collect();
    let tab_panels: String = SETUP_TABS
        .iter()
        .map(|(key, _, _)| {
            let panel = match *key {
                "inboxes" => &inboxes_panel,
                "add" => &add_panel,
                "gmail" => &gmail_panel,
                _ => &outlook_panel,
            };
            format!("<div data-tab-panel='{key}'{}>{panel}</div>", if *key == active { "" } else { " hidden" })
        })
        .collect();
    format!(
        "<div class='page-title'><h1>{}</h1><p class='subtitle'>{}</p></div><div data-tabs><div class='tab-list' role='tablist'>{tab_buttons}</div>{tab_panels}</div>",
        e(&ctx.t("Inbox Setup")),
        e(&ctx.t("Connect a Gmail or Outlook inbox for the loop to triage. Nothing is ever sent, archived, or deleted automatically.")),
    )
}

// ---- POSTs --------------------------------------------------------------------------------------

fn split_lines(text: &str) -> Vec<Value> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(|l| json!(l)).collect()
}

/// `POST /inbox/oauth-client`.
fn oauth_client(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let provider = form.first("provider").unwrap_or("").to_string();
    let mut secret = form.first("client_secret").unwrap_or("").to_string();
    let oauth_path = inbox_config::default_oauth_path();
    if provider == "google" && secret.is_empty() {
        secret = inbox_config::load_oauth(&oauth_path)
            .get("google")
            .map(|g| s(g, "client_secret"))
            .unwrap_or_default();
    }
    let (ok, message) = inbox_config::save_oauth_client(&provider, form.first("client_id").unwrap_or(""), &secret, &oauth_path);
    let location = match provider.as_str() {
        "google" => format!("{BASE}?view=setup&tab=gmail"),
        "microsoft" => format!("{BASE}?view=setup&tab=outlook"),
        _ => format!("{BASE}?view=setup"),
    };
    UiResponse::flash_redirect(ok, &message, &location)
}

/// `POST /inbox/inboxes`: add or edit one inbox.
fn save_inbox(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let v = |k: &str| form.first(k).unwrap_or("").to_string();
    let fields = json!({
        "name": v("name"), "label": v("label"), "provider": v("provider"), "account": v("account"),
        "urgent_brief": v("urgent_brief"),
        "vip_senders": split_lines(&v("vip_senders")),
        "exclude_senders": split_lines(&v("exclude_senders")),
        "slack_bundle": v("slack_bundle"), "enabled": true,
    });
    let is_new = v("is_new") == "1";
    let (ok, message) = inbox_config::upsert_inbox(&fields, is_new, &inbox_config::default_config_path());
    let location = if is_new && !ok { format!("{BASE}?view=setup&tab=add") } else { INBOXES_TAB.to_string() };
    UiResponse::flash_redirect(ok, &message, &location)
}

fn valid_inbox_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `POST /inbox/inboxes/<name>/<verb>`.
fn inbox_action(ctx: &UiCtx) -> UiResponse {
    let (name, verb) = (ctx.param("name"), ctx.param("verb"));
    if !valid_inbox_name(name) || !["connect", "disconnect", "test", "pause", "delete"].contains(&verb) {
        return UiResponse::not_found(ctx.lang);
    }
    let config_path = inbox_config::default_config_path();
    let Ok(inbox) = inbox_config::get_inbox(name, &config_path) else {
        return UiResponse::flash_redirect(false, &ctx.tf("No inbox named {name}", &[("name", name)]), INBOXES_TAB);
    };
    let label = s(&inbox, "label");
    match verb {
        "connect" => connect(ctx, name, &inbox),
        "test" => test_connection(ctx, &inbox),
        "pause" => {
            let (ok, message) = inbox_config::set_enabled(name, !enabled(&inbox), &config_path);
            UiResponse::flash_redirect(ok, &message, BASE)
        }
        _ => {
            let deleted = mail_auth::default_store().and_then(|store| store.delete(name));
            if let Err(err) = deleted {
                return UiResponse::flash_redirect(false, &err.to_string(), INBOXES_TAB);
            }
            if verb == "disconnect" {
                return UiResponse::flash_redirect(true, &ctx.tf("Disconnected {label}", &[("label", &label)]), INBOXES_TAB);
            }
            let (ok, message) = inbox_config::delete_inbox(name, &config_path);
            if ok {
                let out = inbox_seen::output_dir_for(&ctx.state.repo_root);
                let _ = inbox_status::remove(name, &out.join("status.json"));
                inbox_seen::forget(name, &out.join("state"));
            }
            UiResponse::flash_redirect(ok, &message, INBOXES_TAB)
        }
    }
}

/// `POST /inbox/inboxes/<name>/connect`: Gmail signs in through Google; Outlook is not built yet.
/// A form with `back=connectors` (the Connectors page) returns there instead of to the inbox list.
fn connect(ctx: &UiCtx, name: &str, inbox: &Value) -> UiResponse {
    let from_connectors = ctx.form().first("back") == Some("connectors");
    let back = if from_connectors { CONNECTORS_PAGE } else { INBOXES_TAB };
    if s(inbox, "provider") == "gmail" {
        return google_connect(ctx, name, inbox, from_connectors);
    }
    UiResponse::flash_redirect(
        false,
        &ctx.tf(
            "{label}: Outlook sign-in isn't available yet - connect a Gmail inbox for now",
            &[("label", &s(inbox, "label"))],
        ),
        back,
    )
}

const CONNECTORS_PAGE: &str = "/connectors";

/// Pending-state kinds for a Gmail inbox sign-in: from the inbox list, or from Connectors.
pub const INBOX_KIND: &str = "inbox";
pub const INBOX_FROM_CONNECTORS_KIND: &str = "inbox-connectors";

pub fn is_inbox_kind(kind: &str) -> bool {
    kind == INBOX_KIND || kind == INBOX_FROM_CONNECTORS_KIND
}

/// Where the browser lands after a Gmail sign-in that started with this pending kind.
pub fn return_location(kind: &str) -> &'static str {
    if kind == INBOX_FROM_CONNECTORS_KIND {
        CONNECTORS_PAGE
    } else {
        INBOXES_TAB
    }
}

/// Whether a Gmail refresh token is stored for this inbox.
pub fn gmail_connected(name: &str) -> bool {
    mail_auth::default_store()
        .ok()
        .and_then(|store| store.get(name).ok().flatten())
        .is_some()
}

/// Send the browser to Google for a Gmail inbox.
fn google_connect(ctx: &UiCtx, name: &str, inbox: &Value, from_connectors: bool) -> UiResponse {
    let back = if from_connectors { CONNECTORS_PAGE } else { INBOXES_TAB };
    let Some((client_id, _)) = super::connectors::google_oauth_client(ctx.state) else {
        return UiResponse::flash_redirect(
            false,
            &ctx.t("Add a Google OAuth client on the Inbox Triage setup page first"),
            "/loops/inbox-triage-loop?view=setup&tab=gmail",
        );
    };
    let redirect_uri = format!("http://{}/oauth/google/callback", super::connectors::authority_port(ctx));
    let (verifier, challenge) = oauth::pkce();
    let pending = oauth::Pending {
        provider: oauth::Provider::Google,
        kind: if from_connectors { INBOX_FROM_CONNECTORS_KIND } else { INBOX_KIND }.to_string(),
        account: name.to_string(),
        verifier,
        redirect_uri: redirect_uri.clone(),
        app_scheme: None,
        created: oauth::now_secs(),
    };
    let Ok(state) = oauth::create_pending(&ctx.state.home_dir.join("oauth"), &pending) else {
        return UiResponse::flash_redirect(false, &ctx.t("Could not start Google sign-in"), back);
    };
    let account = s(inbox, "account");
    let mut params = vec![
        ("client_id", client_id.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("response_type", "code"),
        ("scope", oauth::GMAIL_SCOPE),
        ("access_type", "offline"),
        ("prompt", "consent"),
        ("state", state.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
    ];
    if !account.is_empty() {
        params.push(("login_hint", account.as_str()));
    }
    UiResponse::redirect(303, &format!("{}?{}", oauth::GOOGLE_AUTH_URL, super::connectors::urlencode(&params)))
}

/// Finish a Gmail inbox sign-in: swap the code for a refresh token and keep it under the
/// inbox's name, where `mail_auth` looks it up. The message never carries the code or a token.
pub fn google_callback(
    oauth_ctx: &oauth::OauthContext,
    lang: crate::ui::i18n::Lang,
    query_state: &str,
    code: Option<&str>,
    error: Option<&str>,
) -> (bool, String) {
    let t = |s: &str| crate::ui::i18n::t(lang, s, &[]);
    let now = oauth::now_secs();
    let Some(pending) = oauth::consume_pending(&oauth_ctx.dir, query_state, now).filter(|p| is_inbox_kind(&p.kind)) else {
        return (false, t("That sign-in link expired or was already used - click Connect again"));
    };
    if let Some(e) = error.filter(|e| !e.is_empty()) {
        return (false, crate::ui::i18n::t(lang, "Google sign-in was cancelled ({error})", &[("error", e)]));
    }
    let tokens = match oauth::exchange_code(oauth_ctx, oauth::Provider::Google, code.unwrap_or(""), &pending, now) {
        Ok(t) => t,
        Err(e) => return (false, crate::ui::i18n::t(lang, "Google sign-in failed: {detail}", &[("detail", &e.to_string())])),
    };
    let Some(refresh) = tokens.refresh_token.as_deref() else {
        return (false, t("Could not start Google sign-in"));
    };
    match mail_auth::default_store().and_then(|store| store.set(&pending.account, refresh)) {
        Ok(()) => (true, crate::ui::i18n::t(lang, "Connected {label}", &[("label", &pending.account)])),
        Err(err) => (false, err.to_string()),
    }
}

/// `_test_connection`: fetch the signed-in address and compare it to the account.
fn test_connection(ctx: &UiCtx, inbox: &Value) -> UiResponse {
    let label = s(inbox, "label");
    let address = mail_auth::get_access_token_default(inbox)
        .map_err(|e| e.to_string())
        .and_then(|token| mail_provider::get_provider(inbox, &token, None, None))
        .and_then(|mut provider| provider.profile_address().map_err(|e| e.to_string()));
    let (ok, message) = match address {
        Err(err) => (false, format!("{label}: {err}")),
        Ok(address) if address != s(inbox, "account").trim().to_lowercase() => (
            false,
            ctx.tf(
                "{label}: signed in as {address}, expected {account}",
                &[("label", &label), ("address", &address), ("account", &s(inbox, "account"))],
            ),
        ),
        Ok(address) => (true, ctx.tf("{label}: connected as {address}", &[("label", &label), ("address", &address)])),
    };
    UiResponse::flash_redirect(ok, &message, INBOXES_TAB)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::routes::UiRequest;
    use crate::ui::state::UiState;

    fn sandbox() -> (tempfile::TempDir, UiState) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mail_oauth.json"),
            r#"{"google":{"client_id":"cid","client_secret":"cs"}}"#,
        )
        .unwrap();
        let state = UiState::explicit(dir.path(), dir.path(), dir.path(), dir.path(), "tok", None);
        (dir, state)
    }

    #[test]
    fn connect_on_a_gmail_inbox_goes_to_google_with_the_gmail_scope() {
        let (_dir, state) = sandbox();
        let req = UiRequest::get("/").with_header("host", "127.0.0.1:8420");
        let inbox = json!({"name": "work", "label": "Work", "provider": "gmail", "account": "me@example.com"});
        let resp = connect(&UiCtx::new(&state, &req), "work", &inbox);
        assert_eq!(resp.status, 303);
        let location = resp.header("Location").unwrap().to_string();
        assert!(location.starts_with("https://accounts.google.com/"), "{location}");
        assert!(location.contains("gmail.modify"), "{location}");
        assert!(location.contains("login_hint=me%40example.com"), "{location}");
        assert!(location.contains("127.0.0.1%3A8420%2Foauth%2Fgoogle%2Fcallback"), "{location}");
    }

    #[test]
    fn connect_from_the_connectors_page_comes_back_to_it() {
        let (dir, state) = sandbox();
        let req = UiRequest::get("/").with_header("host", "127.0.0.1:8420").with_body("back=connectors");
        let inbox = json!({"name": "work", "label": "Work", "provider": "gmail", "account": "me@example.com"});
        let resp = connect(&UiCtx::new(&state, &req), "work", &inbox);
        assert_eq!(resp.status, 303);
        let pending = std::fs::read_dir(dir.path().join("oauth/pending")).unwrap().next().unwrap().unwrap();
        let text = std::fs::read_to_string(pending.path()).unwrap();
        assert!(text.contains(INBOX_FROM_CONNECTORS_KIND), "{text}");
        assert_eq!(return_location(INBOX_FROM_CONNECTORS_KIND), "/connectors");
        assert_eq!(return_location(INBOX_KIND), INBOXES_TAB);
    }

    #[test]
    fn connect_on_an_outlook_inbox_says_it_is_not_available() {
        let (_dir, state) = sandbox();
        let req = UiRequest::get("/");
        let inbox = json!({"name": "o", "label": "Outlook", "provider": "outlook", "account": "me@example.com"});
        let resp = connect(&UiCtx::new(&state, &req), "o", &inbox);
        let location = resp.header("Location").unwrap_or_default().to_string();
        assert!(location.contains("flash"), "{location}");
        assert!(!location.contains("accounts.google.com"), "{location}");
    }
}
