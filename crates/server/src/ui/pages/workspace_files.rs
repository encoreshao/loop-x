//! `/connectors/gdocs` and `/connectors/gsheets`: a Google Docs or Google Sheets
//! account's templates (tick the ones the chat may copy, refresh the list from
//! Drive) and the files the chat created from them, each with an Open in Google
//! link. The ticks and the created files live in the same store the chat tools
//! read: `<home>/gdocs/` and `<home>/gsheets/`.

use super::connectors::{paths, store};
use super::loops_common::flash_html;
use crate::ui::badge::default_badge;
use crate::ui::icons;
use crate::ui::pyfmt::{self, escape as h};
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::shell::{render_shell, ShellOpts};
use crate::ui::state::UiState;
use loopx_core::connectors::{Account, Connector, HttpClient, UreqClient};
use loopx_core::connectors_config as cc;
use loopx_core::slides_store as sstore;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

const TEMPLATE_LIMIT: u32 = 100;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Docs,
    Sheets,
}

impl Kind {
    fn account_kind(self) -> &'static str {
        match self {
            Kind::Docs => "google_docs",
            Kind::Sheets => "google_sheets",
        }
    }
    fn route(self) -> &'static str {
        match self {
            Kind::Docs => "/connectors/gdocs",
            Kind::Sheets => "/connectors/gsheets",
        }
    }
    fn dir_name(self) -> &'static str {
        match self {
            Kind::Docs => "gdocs",
            Kind::Sheets => "gsheets",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Kind::Docs => "description",
            Kind::Sheets => "fact_check",
        }
    }
    fn product(self, ctx: &UiCtx) -> String {
        match self {
            Kind::Docs => ctx.t("Google Docs"),
            Kind::Sheets => ctx.t("Google Sheets"),
        }
    }
    fn no_connector(self, ctx: &UiCtx, id: &str) -> String {
        match self {
            Kind::Docs => ctx.tf("No Google Docs connector with id {id}", &[("id", id)]),
            Kind::Sheets => ctx.tf("No Google Sheets connector with id {id}", &[("id", id)]),
        }
    }
    fn cannot_open(self, ctx: &UiCtx) -> String {
        match self {
            Kind::Docs => ctx.t("Could not open the Google Docs connector"),
            Kind::Sheets => ctx.t("Could not open the Google Sheets connector"),
        }
    }
    fn open_label(self, ctx: &UiCtx) -> String {
        match self {
            Kind::Docs => ctx.t("Open in Google Docs"),
            Kind::Sheets => ctx.t("Open in Google Sheets"),
        }
    }
    fn no_templates(self, ctx: &UiCtx) -> String {
        match self {
            Kind::Docs => ctx.t(
                "No templates yet. Refresh to list the documents you own or that were shared with you.",
            ),
            Kind::Sheets => ctx.t(
                "No templates yet. Refresh to list the spreadsheets you own or that were shared with you.",
            ),
        }
    }
    fn hint(self, ctx: &UiCtx) -> String {
        match self {
            Kind::Docs => ctx.t(
                "Tick the documents the chat may copy. Put {{placeholders}} in a template's text and the chat fills them in.",
            ),
            Kind::Sheets => ctx.t(
                "Tick the spreadsheets the chat may copy. Put {{placeholders}} in a template's cells and the chat fills them in.",
            ),
        }
    }
    fn files_title(self, ctx: &UiCtx) -> String {
        match self {
            Kind::Docs => ctx.t("Documents"),
            Kind::Sheets => ctx.t("Spreadsheets"),
        }
    }
    fn no_files(self, ctx: &UiCtx) -> String {
        match self {
            Kind::Docs => ctx.t("No documents yet. Ask in Chat to create one from an enabled template."),
            Kind::Sheets => ctx.t("No spreadsheets yet. Ask in Chat to create one from an enabled template."),
        }
    }
}

pub fn register(r: &mut Registry) {
    r.get("/connectors/gdocs", docs_page);
    r.post("/connectors/gdocs/refresh", docs_refresh);
    r.post("/connectors/gdocs/select", docs_select);
    r.get("/connectors/gsheets", sheets_page);
    r.post("/connectors/gsheets/refresh", sheets_refresh);
    r.post("/connectors/gsheets/select", sheets_select);
}

fn docs_page(ctx: &UiCtx) -> UiResponse {
    page(ctx, Kind::Docs)
}
fn docs_refresh(ctx: &UiCtx) -> UiResponse {
    refresh(ctx, Kind::Docs)
}
fn docs_select(ctx: &UiCtx) -> UiResponse {
    select(ctx, Kind::Docs)
}
fn sheets_page(ctx: &UiCtx) -> UiResponse {
    page(ctx, Kind::Sheets)
}
fn sheets_refresh(ctx: &UiCtx) -> UiResponse {
    refresh(ctx, Kind::Sheets)
}
fn sheets_select(ctx: &UiCtx) -> UiResponse {
    select(ctx, Kind::Sheets)
}

fn dir(state: &UiState, kind: Kind) -> PathBuf {
    state.home_dir.join(kind.dir_name())
}

fn http(state: &UiState) -> Arc<dyn HttpClient> {
    state
        .connector_http
        .clone()
        .unwrap_or_else(|| Arc::new(UreqClient::default()))
}

fn back(kind: Kind, id: &str) -> String {
    format!("{}?id={}", kind.route(), pyfmt::quote(id, ""))
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Only Google's own hosts are ever linked.
fn google_link(url: &str) -> Option<&str> {
    (url.starts_with("https://docs.google.com/") || url.starts_with("https://drive.google.com/")).then_some(url)
}

fn account_for(ctx: &UiCtx, kind: Kind, id: &str) -> Result<Account, String> {
    match cc::get_account(id, &paths(ctx.state)) {
        Ok(Some(a)) if a.kind == kind.account_kind() && a.managed_by == "native" => Ok(a),
        Ok(_) => Err(kind.no_connector(ctx, id)),
        Err(e) => Err(ctx.tf("Could not read connectors: {detail}", &[("detail", &e.0)])),
    }
}

fn load(ctx: &UiCtx, kind: Kind, id: &str) -> Result<Box<dyn Connector>, String> {
    let secrets = store(ctx.state)?;
    cc::load_connector(id, secrets.as_ref(), &paths(ctx.state), http(ctx.state))
        .map_err(|_| kind.cannot_open(ctx))
}

/// Fetch the template list and cache it; the result is the count.
fn refresh_cache(c: &dyn Connector, dir: &std::path::Path, id: &str, now: &str) -> Result<usize, String> {
    let rows = c.list_templates("", TEMPLATE_LIMIT).map_err(|e| e.to_string())?;
    sstore::cache_templates(dir, id, &rows, now)?;
    Ok(rows.len())
}

fn refresh(ctx: &UiCtx, kind: Kind) -> UiResponse {
    let id = ctx.form().first("id").unwrap_or("").to_string();
    let to = back(kind, &id);
    if let Err(m) = account_for(ctx, kind, &id) {
        return UiResponse::flash_redirect(false, &m, "/connectors");
    }
    let connector = match load(ctx, kind, &id) {
        Ok(c) => c,
        Err(m) => return UiResponse::flash_redirect(false, &m, &to),
    };
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    match refresh_cache(connector.as_ref(), &dir(ctx.state, kind), &id, &now) {
        Ok(n) => UiResponse::flash_redirect(
            true,
            &ctx.tf("Found {count} templates in Drive", &[("count", &n.to_string())]),
            &to,
        ),
        Err(m) => UiResponse::flash_redirect(false, &m, &to),
    }
}

fn select(ctx: &UiCtx, kind: Kind) -> UiResponse {
    let form = ctx.form();
    let id = form.first("id").unwrap_or("").to_string();
    let to = back(kind, &id);
    if let Err(m) = account_for(ctx, kind, &id) {
        return UiResponse::flash_redirect(false, &m, "/connectors");
    }
    let d = dir(ctx.state, kind);
    // Only ids the last Drive listing returned can be ticked.
    let known: Vec<String> = sstore::cached_templates(&d, &id).iter().map(|t| text(t, "id")).collect();
    let mut keep: Vec<String> = sstore::selected_ids(&d, &id)
        .into_iter()
        .filter(|s| !known.contains(s))
        .collect();
    keep.extend(form.all("template").iter().filter(|t| known.contains(t)).cloned());
    match sstore::set_selected(&d, &id, &keep) {
        Ok(()) => {
            let n = sstore::selected_templates(&d, &id).len();
            UiResponse::flash_redirect(
                true,
                &ctx.tf("{count} templates enabled", &[("count", &n.to_string())]),
                &to,
            )
        }
        Err(m) => UiResponse::flash_redirect(false, &ctx.t(&m), &to),
    }
}

// ---- rendering -------------------------------------------------------------------------

fn when(stamp: &str) -> String {
    if stamp.len() >= 16 {
        format!("{} UTC", stamp[..16].replace('T', " "))
    } else {
        stamp.to_string()
    }
}

fn join_dot(parts: &[String]) -> String {
    parts.iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" · ")
}

fn open_link(ctx: &UiCtx, kind: Kind, url: &str) -> String {
    match google_link(url) {
        Some(u) => format!(
            "<a class='btn btn-neutral' href=\"{}\" target='_blank' rel='noopener noreferrer'>{} {}</a>",
            h(u),
            icons::icon("open_in_new"),
            h(&kind.open_label(ctx))
        ),
        None => String::new(),
    }
}

fn templates_card(ctx: &UiCtx, kind: Kind, account: &Account) -> String {
    let d = dir(ctx.state, kind);
    let cached = sstore::cached_templates(&d, &account.id);
    let ticked = sstore::selected_ids(&d, &account.id);
    let csrf = ctx.csrf_input();
    let id = h(&account.id);
    let route = kind.route();
    let fetched = sstore::fetched_at(&d, &account.id);
    let status = if fetched.is_empty() {
        ctx.t("Not fetched from Drive yet")
    } else {
        ctx.tf("Last refreshed {when}", &[("when", &when(&fetched))])
    };
    let refresh_form = format!(
        "<form method='post' action='{route}/refresh' class='daemon-action-form'>{csrf}\
<input type='hidden' name='id' value=\"{id}\">\
<button type='submit' class='btn btn-primary'>{} {}</button></form>",
        icons::icon("autorenew"),
        h(&ctx.t("Refresh from Drive"))
    );
    let list = if cached.is_empty() {
        format!("<p class='section-subtitle'>{}</p>", h(&kind.no_templates(ctx)))
    } else {
        let rows: String = cached
            .iter()
            .map(|t| {
                let tid = text(t, "id");
                let checked = if ticked.contains(&tid) { " checked" } else { "" };
                let owner = if t.get("owned_by_me").and_then(Value::as_bool).unwrap_or(false) {
                    ctx.t("You")
                } else {
                    text(t, "owner")
                };
                let link = google_link(&text(t, "web_link"))
                    .map(|u| {
                        format!(
                            " <a href=\"{}\" target='_blank' rel='noopener noreferrer'>{}</a>",
                            h(u),
                            h(&ctx.t("Open"))
                        )
                    })
                    .unwrap_or_default();
                format!(
                    "<label class='project-block account-row'><span class='account-main'>\
<input type='checkbox' name='template' value=\"{}\"{checked}> <strong>{}</strong> \
<span class='section-subtitle'>{}</span>{link}</span></label>",
                    h(&tid),
                    h(&text(t, "name")),
                    h(&join_dot(&[owner, when(&text(t, "modified"))]))
                )
            })
            .collect();
        format!(
            "<form method='post' action='{route}/select'>{csrf}\
<input type='hidden' name='id' value=\"{id}\">{rows}\
<div class='account-actions'><button type='submit' class='btn btn-primary'>{} {}</button></div></form>",
            icons::icon("check_circle"),
            h(&ctx.t("Save selection"))
        )
    };
    format!(
        "<div class='card'><div class='section-header'>{}<h2>{}</h2></div>\
<p class='section-subtitle'>{}</p><div class='pill-row'><span class='pill pill-grey'>{}</span></div>\
<div class='account-actions'>{refresh_form}</div>{list}</div>",
        icons::icon(kind.icon()),
        h(&ctx.t("Templates")),
        h(&kind.hint(ctx)),
        h(&status)
    )
}

fn files_card(ctx: &UiCtx, kind: Kind, account: &Account) -> String {
    let files = sstore::decks(&dir(ctx.state, kind), &account.id);
    let body = if files.is_empty() {
        format!("<p class='section-subtitle'>{}</p>", h(&kind.no_files(ctx)))
    } else {
        files
            .iter()
            .map(|f| {
                format!(
                    "<div class='project-block account-row'><div class='account-main'><strong>{}</strong>\
<div class='section-subtitle'>{}</div></div><div class='account-actions'>{}</div></div>",
                    h(&text(f, "title")),
                    h(&join_dot(&[text(f, "template_name"), when(&text(f, "created"))])),
                    open_link(ctx, kind, &text(f, "web_link"))
                )
            })
            .collect()
    };
    format!(
        "<div class='card'><div class='section-header'>{}<h2>{}</h2></div>{body}</div>",
        icons::icon(kind.icon()),
        h(&kind.files_title(ctx))
    )
}

fn page(ctx: &UiCtx, kind: Kind) -> UiResponse {
    let id = ctx.query().first("id").unwrap_or("").to_string();
    let (flash, ok) = ctx.flash();
    let account = match account_for(ctx, kind, &id) {
        Ok(a) => a,
        Err(m) => return UiResponse::flash_redirect(false, &m, "/connectors"),
    };
    let head = format!(
        "<p class='section-subtitle'><a href='/connectors'>{} {}</a> · {}</p>{}",
        icons::icon("arrow_back"),
        h(&ctx.t("Connectors")),
        h(&account.label),
        flash_html(flash.as_deref(), ok)
    );
    let body = format!(
        "{head}{}{}",
        templates_card(ctx, kind, &account),
        files_card(ctx, kind, &account)
    );
    UiResponse::html(render_shell(
        ctx,
        &format!("{} · LoopX Engineering", kind.product(ctx)),
        "connectors",
        &default_badge(ctx),
        &body,
        ShellOpts::default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::routes::UiRequest;
    use axum::http::Method;
    use loopx_core::connectors::{ConnResult, ConnectorError, TypeInfo};
    use serde_json::json;

    const KINDS: [(Kind, &str, &str); 2] = [
        (Kind::Docs, "google_docs", "gdocs"),
        (Kind::Sheets, "google_sheets", "gsheets"),
    ];

    fn state(dir: &std::path::Path) -> UiState {
        UiState::explicit(dir, dir, dir, &dir.join("update"), "tok", None)
    }

    fn with_account(dir: &std::path::Path, ty: &str) {
        std::fs::write(
            dir.join("connectors.json"),
            json!([{"id": "ws", "type": ty, "label": "My Workspace"}]).to_string(),
        )
        .unwrap();
    }

    fn cache(dir: &std::path::Path, sub: &str) {
        sstore::cache_templates(
            &dir.join(sub),
            "ws",
            &[
                json!({"id": "t1", "name": "<script>x</script>", "owner": "Ana <i>", "modified": "2026-10-01T09:30:00Z",
                       "web_link": "https://docs.google.com/document/d/t1/edit"}),
                json!({"id": "t2", "name": "Pitch", "owned_by_me": true}),
            ],
            "2026-10-25T08:00:00Z",
        )
        .unwrap();
    }

    fn post(st: &UiState, path: &str, body: &str) -> UiResponse {
        st.registry
            .dispatch(st, &UiRequest::new(Method::POST, path).with_body(body))
            .unwrap()
    }

    fn get_page(st: &UiState, path: &str) -> UiResponse {
        st.registry.dispatch(st, &UiRequest::get(path)).unwrap()
    }

    #[test]
    fn page_escapes_third_party_names_and_shows_ticks() {
        for (_, ty, sub) in KINDS {
            let d = tempfile::tempdir().unwrap();
            with_account(d.path(), ty);
            cache(d.path(), sub);
            sstore::set_selected(&d.path().join(sub), "ws", &["t1".into()]).unwrap();
            let st = state(d.path());
            let body = get_page(&st, &format!("/connectors/{sub}?id=ws")).body_text();
            assert!(body.contains("&lt;script&gt;x&lt;/script&gt;") && !body.contains("<script>x"), "{body}");
            assert!(body.contains("Ana &lt;i&gt;"), "{body}");
            assert!(body.contains("value=\"t1\" checked") && body.contains("value=\"t2\">"), "{body}");
            assert!(body.contains("Last refreshed 2026-10-25 08:00 UTC"));
            assert!(body.contains(&format!("action='/connectors/{sub}/select'")));
        }
    }

    #[test]
    fn wrong_type_or_unknown_accounts_bounce_back() {
        let d = tempfile::tempdir().unwrap();
        with_account(d.path(), "google_docs");
        let st = state(d.path());
        for path in [
            "/connectors/gdocs?id=nope",
            "/connectors/gdocs?id=",
            "/connectors/gsheets?id=ws",
        ] {
            let r = get_page(&st, path);
            assert_eq!(r.status, 303, "{path}");
            assert!(r.header("Location").unwrap().starts_with("/connectors?flash="));
        }
    }

    #[test]
    fn posts_need_csrf() {
        let d = tempfile::tempdir().unwrap();
        with_account(d.path(), "google_docs");
        let st = state(d.path());
        for (_, _, sub) in KINDS {
            for action in ["refresh", "select"] {
                let path = format!("/connectors/{sub}/{action}");
                let r = st.registry.dispatch(&st, &UiRequest::new(Method::POST, &path)).unwrap();
                assert_eq!(r.status, 403, "{path}");
            }
        }
    }

    #[test]
    fn ticks_only_known_ids_and_use_each_kinds_own_store() {
        for (_, ty, sub) in KINDS {
            let d = tempfile::tempdir().unwrap();
            with_account(d.path(), ty);
            cache(d.path(), sub);
            let store = d.path().join(sub);
            sstore::set_selected(&store, "ws", &["old-not-listed".into(), "t1".into()]).unwrap();
            let st = state(d.path());
            let r = post(
                &st,
                &format!("/connectors/{sub}/select"),
                "csrf_token=tok&id=ws&template=t2&template=bogus",
            );
            assert_eq!(r.status, 303);
            assert_eq!(sstore::selected_ids(&store, "ws"), ["old-not-listed", "t2"]);
            // What the chat tools read.
            let names: Vec<String> = sstore::selected_templates(&store, "ws").iter().map(|t| text(t, "id")).collect();
            assert_eq!(names, ["t2"]);
            let other = if sub == "gdocs" { "gsheets" } else { "gdocs" };
            assert!(sstore::selected_ids(&d.path().join(other), "ws").is_empty());
        }
    }

    #[test]
    fn a_page_for_the_other_kind_cannot_tick_this_accounts_store() {
        let d = tempfile::tempdir().unwrap();
        with_account(d.path(), "google_docs");
        cache(d.path(), "gsheets");
        let st = state(d.path());
        let r = post(&st, "/connectors/gsheets/select", "csrf_token=tok&id=ws&template=t1");
        assert_eq!(r.status, 303);
        assert!(r.header("Location").unwrap().starts_with("/connectors?flash="));
        assert!(sstore::selected_ids(&d.path().join("gsheets"), "ws").is_empty());
    }

    struct Listing(Result<Vec<Value>, ConnectorError>);
    impl Connector for Listing {
        fn info(&self) -> &'static TypeInfo {
            &loopx_core::connectors::google_docs::INFO
        }
        fn account(&self) -> &Account {
            unreachable!()
        }
        fn list_templates(&self, _q: &str, limit: u32) -> ConnResult<Vec<Value>> {
            assert_eq!(limit, TEMPLATE_LIMIT);
            self.0.clone()
        }
    }

    #[test]
    fn refresh_caches_the_listing_and_a_failure_leaves_the_cache_alone() {
        let d = tempfile::tempdir().unwrap();
        let ok = Listing(Ok(vec![json!({"id": "a", "name": "A"})]));
        assert_eq!(refresh_cache(&ok, d.path(), "ws", "now"), Ok(1));
        assert_eq!(sstore::cached_templates(d.path(), "ws").len(), 1);
        let bad = Listing(Err(ConnectorError::msg("Reconnect Google Docs")));
        assert_eq!(refresh_cache(&bad, d.path(), "ws", "later"), Err("Reconnect Google Docs".into()));
        assert_eq!(sstore::fetched_at(d.path(), "ws"), "now");
    }

    #[test]
    fn created_files_list_with_open_in_google_links_and_escape_titles() {
        for (_, ty, sub) in KINDS {
            let d = tempfile::tempdir().unwrap();
            with_account(d.path(), ty);
            sstore::record_deck(
                &d.path().join(sub),
                "ws",
                json!({"id": "f1", "title": "Acme <b>", "web_link": "https://docs.google.com/document/d/f1/edit",
                       "template_name": "Weekly", "created": "2026-10-25T10:00:00Z"}),
            )
            .unwrap();
            sstore::record_deck(
                &d.path().join(sub),
                "ws",
                json!({"id": "f2", "title": "Evil", "web_link": "https://evil.example/x"}),
            )
            .unwrap();
            let st = state(d.path());
            let body = get_page(&st, &format!("/connectors/{sub}?id=ws")).body_text();
            assert!(body.contains("Acme &lt;b&gt;") && !body.contains("Acme <b>"), "{body}");
            assert!(body.contains("href=\"https://docs.google.com/document/d/f1/edit\""));
            assert!(body.contains("rel='noopener noreferrer'"));
            assert!(!body.contains("evil.example"));
            assert!(body.contains(if sub == "gdocs" { "Open in Google Docs" } else { "Open in Google Sheets" }));
        }
    }

    #[test]
    fn renaming_or_removing_an_account_moves_or_removes_its_store_files() {
        let d = tempfile::tempdir().unwrap();
        with_account(d.path(), "google_docs");
        for sub in ["gdocs", "gsheets"] {
            cache(d.path(), sub);
        }
        let st = state(d.path());
        let req = UiRequest::get("/connectors");
        let ctx = UiCtx::new(&st, &req);
        super::super::connectors::renamed_or_deleted(&ctx, "ws", Some("ws2"));
        for sub in ["gdocs", "gsheets"] {
            assert!(sstore::cached_templates(&d.path().join(sub), "ws").is_empty());
            assert_eq!(sstore::cached_templates(&d.path().join(sub), "ws2").len(), 2, "{sub}");
        }
        super::super::connectors::renamed_or_deleted(&ctx, "ws2", None);
        for sub in ["gdocs", "gsheets"] {
            assert!(sstore::cached_templates(&d.path().join(sub), "ws2").is_empty(), "{sub}");
        }
    }

    #[test]
    fn connectors_page_links_each_kind_to_its_templates_page() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("connectors.json"),
            json!([{"id": "ws", "type": "google_docs", "label": "D"},
                   {"id": "sh", "type": "google_sheets", "label": "S"}])
            .to_string(),
        )
        .unwrap();
        let st = state(d.path());
        let body = get_page(&st, "/connectors").body_text();
        assert!(body.contains("href='/connectors/gdocs?id=ws'"), "{body}");
        assert!(body.contains("href='/connectors/gsheets?id=sh'"), "{body}");
    }
}
