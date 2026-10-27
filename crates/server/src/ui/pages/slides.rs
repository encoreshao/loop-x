//! `/connectors/slides`: a Google Slides account's templates (tick the ones the
//! chat may copy, refresh the list from Drive) and the decks created from them,
//! with an in-app review (slide thumbnails and text) next to an Open in Google
//! Slides link. The template list is cached under `<home>/slides/`.

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

pub fn register(r: &mut Registry) {
    r.get("/connectors/slides", page);
    r.post("/connectors/slides/refresh", refresh);
    r.post("/connectors/slides/select", select);
}

fn dir(state: &UiState) -> PathBuf {
    state.home_dir.join("slides")
}

fn http(state: &UiState) -> Arc<dyn HttpClient> {
    state
        .connector_http
        .clone()
        .unwrap_or_else(|| Arc::new(UreqClient::default()))
}

fn back(id: &str) -> String {
    format!("/connectors/slides?id={}", pyfmt::quote(id, ""))
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Only Google's own hosts are ever linked or loaded as an image.
fn google_link(url: &str) -> Option<&str> {
    url.strip_prefix("https://docs.google.com/").map(|_| url)
}

fn google_image(url: &str) -> Option<&str> {
    let host = url.strip_prefix("https://")?.split('/').next()?;
    (host.ends_with(".googleusercontent.com") || host.ends_with(".google.com")).then_some(url)
}

fn account_for(ctx: &UiCtx, id: &str) -> Result<Account, String> {
    match cc::get_account(id, &paths(ctx.state)) {
        Ok(Some(a)) if a.kind == "google_slides" && a.managed_by == "native" => Ok(a),
        Ok(_) => Err(ctx.tf("No Google Slides connector with id {id}", &[("id", id)])),
        Err(e) => Err(ctx.tf("Could not read connectors: {detail}", &[("detail", &e.0)])),
    }
}

fn load(ctx: &UiCtx, id: &str) -> Result<Box<dyn Connector>, String> {
    let secrets = store(ctx.state)?;
    cc::load_connector(id, secrets.as_ref(), &paths(ctx.state), http(ctx.state))
        .map_err(|_| ctx.t("Could not open the Google Slides connector"))
}

/// Fetch the template list and cache it; the message names the count.
fn refresh_cache(c: &dyn Connector, dir: &std::path::Path, id: &str, now: &str) -> Result<usize, String> {
    let rows = c.list_templates("", TEMPLATE_LIMIT).map_err(|e| e.to_string())?;
    sstore::cache_templates(dir, id, &rows, now)?;
    Ok(rows.len())
}

fn refresh(ctx: &UiCtx) -> UiResponse {
    let id = ctx.form().first("id").unwrap_or("").to_string();
    let to = back(&id);
    if let Err(m) = account_for(ctx, &id) {
        return UiResponse::flash_redirect(false, &m, "/connectors");
    }
    let connector = match load(ctx, &id) {
        Ok(c) => c,
        Err(m) => return UiResponse::flash_redirect(false, &m, &to),
    };
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    match refresh_cache(connector.as_ref(), &dir(ctx.state), &id, &now) {
        Ok(n) => UiResponse::flash_redirect(
            true,
            &ctx.tf("Found {count} templates in Drive", &[("count", &n.to_string())]),
            &to,
        ),
        Err(m) => UiResponse::flash_redirect(false, &m, &to),
    }
}

fn select(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let id = form.first("id").unwrap_or("").to_string();
    let to = back(&id);
    if let Err(m) = account_for(ctx, &id) {
        return UiResponse::flash_redirect(false, &m, "/connectors");
    }
    // Only ids the last Drive listing returned can be ticked.
    let known: Vec<String> = sstore::cached_templates(&dir(ctx.state), &id)
        .iter()
        .map(|t| text(t, "id"))
        .collect();
    let mut keep: Vec<String> = sstore::selected_ids(&dir(ctx.state), &id)
        .into_iter()
        .filter(|s| !known.contains(s))
        .collect();
    keep.extend(form.all("template").iter().filter(|t| known.contains(t)).cloned());
    match sstore::set_selected(&dir(ctx.state), &id, &keep) {
        Ok(()) => {
            let n = sstore::selected_templates(&dir(ctx.state), &id).len();
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

fn open_link(ctx: &UiCtx, url: &str) -> String {
    match google_link(url) {
        Some(u) => format!(
            "<a class='btn btn-neutral' href=\"{}\" target='_blank' rel='noopener noreferrer'>{} {}</a>",
            h(u),
            icons::icon("open_in_new"),
            h(&ctx.t("Open in Google Slides"))
        ),
        None => String::new(),
    }
}

fn templates_card(ctx: &UiCtx, account: &Account) -> String {
    let d = dir(ctx.state);
    let cached = sstore::cached_templates(&d, &account.id);
    let ticked = sstore::selected_ids(&d, &account.id);
    let csrf = ctx.csrf_input();
    let id = h(&account.id);
    let fetched = sstore::fetched_at(&d, &account.id);
    let status = if fetched.is_empty() {
        ctx.t("Not fetched from Drive yet")
    } else {
        ctx.tf("Last refreshed {when}", &[("when", &when(&fetched))])
    };
    let refresh_form = format!(
        "<form method='post' action='/connectors/slides/refresh' class='daemon-action-form'>{csrf}\
<input type='hidden' name='id' value=\"{id}\">\
<button type='submit' class='btn btn-primary'>{} {}</button></form>",
        icons::icon("autorenew"),
        h(&ctx.t("Refresh from Drive"))
    );
    let list = if cached.is_empty() {
        format!(
            "<p class='section-subtitle'>{}</p>",
            h(&ctx.t("No templates yet. Refresh to list the presentations you own or that were shared with you."))
        )
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
                    h(&[owner, when(&text(t, "modified"))]
                        .iter()
                        .filter(|s| !s.is_empty())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" · "))
                )
            })
            .collect();
        format!(
            "<form method='post' action='/connectors/slides/select'>{csrf}\
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
        icons::icon("slideshow"),
        h(&ctx.t("Templates")),
        h(&ctx.t("Tick the presentations the chat may copy. Put {{placeholders}} in a template's text and the chat fills them in.")),
        h(&status)
    )
}

fn decks_card(ctx: &UiCtx, account: &Account) -> String {
    let decks = sstore::decks(&dir(ctx.state), &account.id);
    let body = if decks.is_empty() {
        format!(
            "<p class='section-subtitle'>{}</p>",
            h(&ctx.t("No decks yet. Ask in Chat to create one from an enabled template."))
        )
    } else {
        decks
            .iter()
            .map(|d| {
                let deck_id = text(d, "id");
                let review = format!(
                    "/connectors/slides?id={}&amp;deck={}",
                    h(&pyfmt::quote(&account.id, "")),
                    h(&pyfmt::quote(&deck_id, ""))
                );
                format!(
                    "<div class='project-block account-row'><div class='account-main'><strong>{}</strong>\
<div class='section-subtitle'>{}</div></div><div class='account-actions'>\
<a class='btn btn-primary' href=\"{review}\">{} {}</a>{}</div></div>",
                    h(&text(d, "title")),
                    h(&[text(d, "template_name"), when(&text(d, "created"))]
                        .iter()
                        .filter(|s| !s.is_empty())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" · ")),
                    icons::icon("visibility"),
                    h(&ctx.t("Review")),
                    open_link(ctx, &text(d, "web_link"))
                )
            })
            .collect()
    };
    format!(
        "<div class='card'><div class='section-header'>{}<h2>{}</h2></div>{body}</div>",
        icons::icon("description"),
        h(&ctx.t("Decks"))
    )
}

/// The review of one deck as `describe_deck` returned it.
fn review_card(ctx: &UiCtx, account: &Account, deck: &Value) -> String {
    let pending: Vec<String> = deck
        .get("placeholders")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(|s| format!("<span class='pill pill-red'>{{{{{}}}}}</span>", h(s))))
        .collect();
    let notice = if pending.is_empty() {
        format!("<span class='pill pill-green'>{}</span>", h(&ctx.t("No placeholders left")))
    } else {
        format!(
            "<span class='pill pill-grey'>{}</span>{}",
            h(&ctx.t("Still unfilled:")),
            pending.join("")
        )
    };
    let slides: String = deck
        .get("slides")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|s| {
            let n = s.get("index").and_then(Value::as_u64).unwrap_or(0);
            let thumb = google_image(&text(s, "thumbnail_url"))
                .map(|u| {
                    format!(
                        "<img class='slide-thumb' loading='lazy' referrerpolicy='no-referrer' src=\"{}\" alt=\"{}\">",
                        h(u),
                        h(&ctx.tf("Slide {n}", &[("n", &n.to_string())]))
                    )
                })
                .unwrap_or_else(|| {
                    format!("<div class='slide-thumb slide-thumb-empty'>{}</div>", h(&ctx.t("No preview")))
                });
            format!(
                "<figure class='slide-card'>{thumb}<figcaption><strong>{}</strong>\
<pre class='slide-text'>{}</pre></figcaption></figure>",
                h(&ctx.tf("Slide {n}", &[("n", &n.to_string())])),
                h(&text(s, "text"))
            )
        })
        .collect();
    format!(
        "<div class='card'><div class='section-header'>{}<h2>{}</h2></div>\
<div class='pill-row'>{notice}</div>\
<div class='account-actions'><a class='btn btn-neutral' href=\"{}\">{} {}</a>{}</div>\
<div class='slide-grid'>{slides}</div></div>",
        icons::icon("visibility"),
        h(&text(deck, "title")),
        h(&back(&account.id)),
        icons::icon("arrow_back"),
        h(&ctx.t("Back to templates")),
        open_link(ctx, &text(deck, "web_link"))
    )
}

fn shell(ctx: &UiCtx, title: &str, body: &str) -> UiResponse {
    UiResponse::html(render_shell(
        ctx,
        &format!("{title} · LoopX Engineering"),
        "connectors",
        &default_badge(ctx),
        body,
        ShellOpts::default(),
    ))
}

fn page(ctx: &UiCtx) -> UiResponse {
    let q = ctx.query();
    let id = q.first("id").unwrap_or("").to_string();
    let (flash, ok) = ctx.flash();
    let account = match account_for(ctx, &id) {
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
    if let Some(deck_id) = q.first("deck").filter(|d| !d.is_empty()) {
        let known = sstore::decks(&dir(ctx.state), &id)
            .iter()
            .any(|d| text(d, "id") == deck_id);
        if !known {
            return UiResponse::flash_redirect(false, &ctx.t("That deck was not created here"), &back(&id));
        }
        let review = load(ctx, &id)
            .and_then(|c| c.describe_deck(deck_id, true).map_err(|e| e.to_string()));
        let body = match review {
            Ok(deck) => review_card(ctx, &account, &deck),
            Err(m) => format!("<div class='card'>{}</div>", flash_html(Some(&m), false)),
        };
        return shell(ctx, &ctx.t("Review deck"), &format!("{head}{body}"));
    }
    let body = format!("{head}{}{}", templates_card(ctx, &account), decks_card(ctx, &account));
    shell(ctx, &ctx.t("Google Slides"), &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::routes::UiRequest;
    use axum::http::Method;
    use loopx_core::connectors::{ConnResult, ConnectorError, TypeInfo};
    use serde_json::json;

    fn state(dir: &std::path::Path) -> UiState {
        UiState::explicit(dir, dir, dir, &dir.join("update"), "tok", None)
    }

    fn with_account(dir: &std::path::Path) {
        std::fs::write(
            dir.join("connectors.json"),
            json!([{"id": "sl", "type": "google_slides", "label": "My Slides"}]).to_string(),
        )
        .unwrap();
    }

    fn cache(dir: &std::path::Path) {
        sstore::cache_templates(
            &dir.join("slides"),
            "sl",
            &[
                json!({"id": "t1", "name": "Weekly <b>", "owner": "Ana", "modified": "2026-10-01T09:30:00Z",
                       "web_link": "https://docs.google.com/presentation/d/t1/edit"}),
                json!({"id": "t2", "name": "Pitch", "owned_by_me": true}),
            ],
            "2026-10-25T08:00:00Z",
        )
        .unwrap();
    }

    fn post(st: &UiState, path: &str, body: &str) -> crate::ui::routes::UiResponse {
        st.registry
            .dispatch(st, &UiRequest::new(Method::POST, path).with_body(body))
            .unwrap()
    }

    fn get_page(st: &UiState, path: &str) -> crate::ui::routes::UiResponse {
        st.registry.dispatch(st, &UiRequest::get(path)).unwrap()
    }

    #[test]
    fn page_lists_cached_templates_with_ticks_and_escapes_names() {
        let d = tempfile::tempdir().unwrap();
        with_account(d.path());
        cache(d.path());
        sstore::set_selected(&d.path().join("slides"), "sl", &["t1".into()]).unwrap();
        let st = state(d.path());
        let body = get_page(&st, "/connectors/slides?id=sl").body_text();
        assert!(body.contains("Weekly &lt;b&gt;") && !body.contains("Weekly <b>"), "{body}");
        assert!(body.contains("value=\"t1\" checked") && body.contains("value=\"t2\">"), "{body}");
        assert!(body.contains("Last refreshed 2026-10-25 08:00 UTC"));
        assert!(body.contains("name='csrf_token'") || body.contains("csrf_token"));
    }

    #[test]
    fn unknown_or_other_type_accounts_bounce_back_to_connectors() {
        let d = tempfile::tempdir().unwrap();
        with_account(d.path());
        let st = state(d.path());
        for path in ["/connectors/slides?id=nope", "/connectors/slides?id="] {
            let r = get_page(&st, path);
            assert_eq!(r.status, 303, "{path}");
            assert!(r.header("Location").unwrap().starts_with("/connectors?flash="));
        }
    }

    #[test]
    fn posts_need_csrf() {
        let d = tempfile::tempdir().unwrap();
        with_account(d.path());
        let st = state(d.path());
        for path in ["/connectors/slides/refresh", "/connectors/slides/select"] {
            let r = st.registry.dispatch(&st, &UiRequest::new(Method::POST, path)).unwrap();
            assert_eq!(r.status, 403, "{path}");
        }
    }

    #[test]
    fn saving_a_selection_ticks_only_known_ids_and_keeps_unlisted_ones() {
        let d = tempfile::tempdir().unwrap();
        with_account(d.path());
        cache(d.path());
        let slides = d.path().join("slides");
        sstore::set_selected(&slides, "sl", &["old-not-listed".into(), "t1".into()]).unwrap();
        let st = state(d.path());
        let r = post(&st, "/connectors/slides/select", "csrf_token=tok&id=sl&template=t2&template=bogus");
        assert_eq!(r.status, 303);
        assert_eq!(sstore::selected_ids(&slides, "sl"), ["old-not-listed", "t2"]);
        let r = post(&st, "/connectors/slides/select", "csrf_token=tok&id=sl");
        assert_eq!(r.status, 303);
        assert_eq!(sstore::selected_ids(&slides, "sl"), ["old-not-listed"]);
    }

    struct Listing(Result<Vec<Value>, ConnectorError>);
    impl Connector for Listing {
        fn info(&self) -> &'static TypeInfo {
            &loopx_core::connectors::google_slides::INFO
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
        assert_eq!(refresh_cache(&ok, d.path(), "sl", "now"), Ok(1));
        assert_eq!(sstore::cached_templates(d.path(), "sl").len(), 1);
        let bad = Listing(Err(ConnectorError::msg("Reconnect Google Slides")));
        assert_eq!(refresh_cache(&bad, d.path(), "sl", "later"), Err("Reconnect Google Slides".into()));
        assert_eq!(sstore::fetched_at(d.path(), "sl"), "now");
    }

    #[test]
    fn review_shows_thumbnails_text_and_unfilled_placeholders_safely() {
        let d = tempfile::tempdir().unwrap();
        with_account(d.path());
        let st = state(d.path());
        let ctx_html = {
            let req = UiRequest::get("/connectors/slides?id=sl");
            let ctx = crate::ui::routes::UiCtx::new(&st, &req);
            let account = Account::new("sl", "google_slides");
            review_card(
                &ctx,
                &account,
                &json!({"title": "Q4 <script>", "web_link": "https://docs.google.com/presentation/d/x/edit",
                    "placeholders": ["client"],
                    "slides": [
                      {"index": 1, "text": "Hi <i>{{client}}</i>", "thumbnail_url": "https://lh3.googleusercontent.com/t?x=1&y=2"},
                      {"index": 2, "text": "", "thumbnail_url": "https://evil.example/t.png"}]}),
            )
        };
        assert!(ctx_html.contains("Q4 &lt;script&gt;") && !ctx_html.contains("<script>"));
        assert!(ctx_html.contains("src=\"https://lh3.googleusercontent.com/t?x=1&amp;y=2\""));
        assert!(!ctx_html.contains("evil.example"));
        assert!(ctx_html.contains("{{client}}") && ctx_html.contains("pill-red"));
        assert!(ctx_html.contains("rel='noopener noreferrer'"));
    }

    #[test]
    fn reviewing_a_deck_this_app_did_not_create_is_refused() {
        let d = tempfile::tempdir().unwrap();
        with_account(d.path());
        let st = state(d.path());
        let r = get_page(&st, "/connectors/slides?id=sl&deck=stranger");
        assert_eq!(r.status, 303);
        assert!(r.header("Location").unwrap().starts_with("/connectors/slides?id=sl&flash="));
    }

    #[test]
    fn decks_list_links_review_and_open() {
        let d = tempfile::tempdir().unwrap();
        with_account(d.path());
        sstore::record_deck(
            &d.path().join("slides"),
            "sl",
            json!({"id": "d1", "title": "Acme", "web_link": "https://docs.google.com/presentation/d/d1/edit",
                   "template_name": "Weekly", "created": "2026-10-25T10:00:00Z"}),
        )
        .unwrap();
        let st = state(d.path());
        let body = get_page(&st, "/connectors/slides?id=sl").body_text();
        assert!(body.contains("/connectors/slides?id=sl&amp;deck=d1"), "{body}");
        assert!(body.contains("https://docs.google.com/presentation/d/d1/edit"));
    }
}
