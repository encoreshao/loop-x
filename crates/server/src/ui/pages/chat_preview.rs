//! `GET /chat/preview`: the chat's file preview panel content, an HTML fragment
//! for one Google Doc, Sheet or Slides deck a chat tool touched.
//!
//! The panel can only open what the chat tools themselves may open (ticked
//! templates, files chat created, and, for docs and sheets, links the user
//! pasted into that same chat session), so this endpoint can never be used to
//! read an arbitrary Drive file. That rule is checked on local state before any
//! connector is loaded. Every file text, name and title is escaped; images load
//! only from Google hosts.

use super::slides::{google_link, slide_cards_html};
use crate::ui::icons;
use crate::ui::pyfmt::escape as h;
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::state::UiState;
use loopx_core::chat_tools::{self, ConfigProvider, Env as ToolsEnv};
use loopx_core::connectors::ConnectorError;
use serde_json::Value;

pub fn register(r: &mut Registry) {
    r.get("/chat/preview", preview);
}

/// What the panel asked for, every field already validated.
#[derive(Debug, Clone, PartialEq)]
struct Target {
    kind: &'static str,
    account: String,
    file: String,
    session: String,
}

/// `^[A-Za-z0-9_-]{1,64}$` or empty (a chat with no session yet).
fn valid_session(s: &str) -> bool {
    s.is_empty()
        || (s.len() <= 64
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
}

fn parse(ctx: &UiCtx) -> Option<Target> {
    let q = ctx.query();
    let kind = match q.first("kind")? {
        "doc" => "doc",
        "sheet" => "sheet",
        "deck" => "deck",
        _ => return None,
    };
    let account = q.first("account")?;
    let file = q.first("file")?;
    let session = q.first("session").unwrap_or("");
    let ok = loopx_core::connectors_config::is_valid_id(account)
        && chat_tools::valid_file_id(file)
        && valid_session(session);
    ok.then(|| Target {
        kind,
        account: account.to_string(),
        file: file.to_string(),
        session: session.to_string(),
    })
}

/// The connector type a file kind is read with.
fn connector_type(kind: &str) -> &'static str {
    match kind {
        "doc" => "google_docs",
        "sheet" => "google_sheets",
        _ => "google_slides",
    }
}

/// A browser marks a request another site made `Sec-Fetch-Site: cross-site`
/// (or `same-site`); only this page's own fetches (or a typed URL) may
/// trigger a read of the user's Google file. The host check every page route
/// gets (`glue::host_allowed`) still applies on top.
fn same_origin(ctx: &UiCtx) -> bool {
    matches!(ctx.req.header("sec-fetch-site"), None | Some("same-origin" | "none"))
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn truthy(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn refresh_button(ctx: &UiCtx) -> String {
    format!(
        "<button type='button' class='btn btn-neutral' data-refresh>{} {}</button>",
        icons::icon("autorenew"),
        h(&ctx.t("Refresh"))
    )
}

/// An inline error line (HTTP 200: the panel shows it in place of the file).
fn error_fragment(ctx: &UiCtx, message: &str, retry: bool) -> String {
    format!(
        "<div class='file-preview file-preview-error' role='alert'>\
<p class='file-preview-error-text'>{} {}</p><div class='file-preview-actions'>{}</div></div>",
        icons::icon("error"),
        h(message),
        if retry { refresh_button(ctx) } else { String::new() }
    )
}

/// A connector failure as the user may see it: a [`ConnectorError::Message`]
/// is written to be shown (and never carries a token); anything else only by
/// its class name.
fn failure_text(ctx: &UiCtx, e: &ConnectorError) -> String {
    match e {
        ConnectorError::Message(m) => m.clone(),
        other => ctx.tf(
            "Could not load this file from Google ({detail})",
            &[("detail", &other.class_name())],
        ),
    }
}

fn doc_body(ctx: &UiCtx, doc: &Value) -> String {
    let paragraphs: String = text(doc, "text")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| format!("<p>{}</p>", h(l)))
        .collect();
    if paragraphs.is_empty() {
        return format!("<p class='file-preview-empty'>{}</p>", h(&ctx.t("This document is empty.")));
    }
    format!("<div class='file-preview-doc'>{paragraphs}</div>")
}

fn cell(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// One table per tab, ragged rows padded to the tab's widest row.
fn sheet_body(ctx: &UiCtx, sheet: &Value) -> (String, bool) {
    let mut cut = false;
    let tabs: String = sheet
        .get("tabs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|tab| {
            cut |= truthy(tab, "truncated");
            let rows: Vec<Vec<String>> = tab
                .get("rows")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|r| r.as_array().map(|c| c.iter().map(cell).collect()).unwrap_or_default())
                .collect();
            let width = rows.iter().map(Vec::len).max().unwrap_or(0);
            let table = if width == 0 {
                format!("<p class='file-preview-empty'>{}</p>", h(&ctx.t("This tab is empty.")))
            } else {
                let body: String = rows
                    .iter()
                    .map(|r| {
                        let cells: String = (0..width)
                            .map(|i| format!("<td>{}</td>", h(r.get(i).map_or("", String::as_str))))
                            .collect();
                        format!("<tr>{cells}</tr>")
                    })
                    .collect();
                format!("<div class='table-wrap'><table class='file-preview-table'><tbody>{body}</tbody></table></div>")
            };
            format!(
                "<section class='file-preview-tab'><h4>{}</h4>{table}</section>",
                h(text(tab, "name"))
            )
        })
        .collect();
    if tabs.is_empty() {
        let empty = format!("<p class='file-preview-empty'>{}</p>", h(&ctx.t("This spreadsheet is empty.")));
        return (empty, cut);
    }
    (tabs, cut)
}

fn deck_body(ctx: &UiCtx, deck: &Value) -> String {
    let cards = slide_cards_html(ctx, deck);
    if cards.is_empty() {
        return format!("<p class='file-preview-empty'>{}</p>", h(&ctx.t("This presentation has no slides.")));
    }
    format!("<div class='slide-grid'>{cards}</div>")
}

fn open_link(ctx: &UiCtx, url: &str) -> String {
    google_link(url)
        .map(|u| {
            format!(
                "<a class='btn btn-neutral' href=\"{}\" target='_blank' rel='noopener noreferrer'>{} {}</a>",
                h(u),
                icons::icon("open_in_new"),
                h(&ctx.t("Open in Google"))
            )
        })
        .unwrap_or_default()
}

fn file_fragment(ctx: &UiCtx, target: &Target, file: &Value) -> String {
    let (body, cut) = match target.kind {
        "doc" => (doc_body(ctx, file), false),
        "sheet" => sheet_body(ctx, file),
        _ => (deck_body(ctx, file), false),
    };
    let note = if cut || truthy(file, "truncated") {
        format!(
            "<p class='file-preview-note' role='note'>{} {}</p>",
            icons::icon("warning"),
            h(&ctx.t("This file is long, so only the first part is shown. Open it in Google to see all of it."))
        )
    } else {
        String::new()
    };
    let title = text(file, "title");
    format!(
        "<div class='file-preview file-preview-{}' data-file-title=\"{}\">\
<div class='file-preview-head'><h3 class='file-preview-title'>{}</h3>\
<div class='file-preview-actions'>{}{}</div></div>{note}{body}</div>",
        target.kind,
        h(title),
        h(title),
        open_link(ctx, text(file, "web_link")),
        refresh_button(ctx)
    )
}

/// Loads and renders the file: access first (local state only), then the
/// account's type, and only then the connector and its one read.
fn render(ctx: &UiCtx, env: &ToolsEnv, target: &Target) -> String {
    if !chat_tools::preview_allowed(env, &target.session, target.kind, &target.account, &target.file) {
        return error_fragment(ctx, &ctx.t("This file is not available to the chat, so it cannot be previewed here."), false);
    }
    let accounts = env.provider.list_accounts().unwrap_or_default();
    let typed = accounts
        .iter()
        .any(|a| a.id == target.account && a.kind == connector_type(target.kind));
    if !typed {
        return error_fragment(ctx, &ctx.t("The Google account for this file is no longer connected."), false);
    }
    let read = env.provider.load(&target.account).and_then(|c| match target.kind {
        "doc" => c.describe_doc(&target.file),
        "sheet" => c.describe_sheet(&target.file),
        _ => c.describe_deck(&target.file, true),
    });
    match read {
        Ok(file) => file_fragment(ctx, target, &file),
        Err(e) => error_fragment(ctx, &failure_text(ctx, &e), true),
    }
}

/// The fragment for the request's file (or the inline error).
fn fragment(ctx: &UiCtx, env: &ToolsEnv) -> String {
    if !same_origin(ctx) {
        return error_fragment(ctx, &ctx.t("This preview can only be opened from the chat."), false);
    }
    match parse(ctx) {
        Some(target) => render(ctx, env, &target),
        None => error_fragment(ctx, &ctx.t("This file cannot be previewed."), false),
    }
}

/// The real wiring: the state's connector store and HTTP client when injected.
fn route_env(state: &UiState) -> ToolsEnv {
    let mut provider = ConfigProvider::new(super::connectors::paths(state));
    if let Some(store) = &state.connector_store {
        provider = provider.with_store(store.clone());
    }
    if let Some(http) = &state.connector_http {
        provider = provider.with_http(http.clone());
    }
    ToolsEnv::new(&state.repo_root, &state.home_dir).with_provider(provider)
}

fn preview(ctx: &UiCtx) -> UiResponse {
    UiResponse::html(fragment(ctx, &route_env(ctx.state)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::routes::UiRequest;
    use loopx_core::chat_tools::StaticProvider;
    use loopx_core::connectors::{Account, ConnResult, Connector, TypeInfo};
    use loopx_core::slides_store as sstore;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<String>>>;

    /// A connector answering `describe_*` from canned values, logging each call.
    struct Fake {
        account: Account,
        log: Log,
        reply: Result<Value, String>,
    }

    impl Connector for Fake {
        fn info(&self) -> &'static TypeInfo {
            &loopx_core::connectors::google_docs::INFO
        }
        fn account(&self) -> &Account {
            &self.account
        }
        fn describe_doc(&self, id: &str) -> ConnResult<Value> {
            self.log.lock().unwrap().push(format!("describe_doc {id}"));
            self.reply.clone().map_err(ConnectorError::msg)
        }
        fn describe_sheet(&self, id: &str) -> ConnResult<Value> {
            self.log.lock().unwrap().push(format!("describe_sheet {id}"));
            self.reply.clone().map_err(ConnectorError::msg)
        }
        fn describe_deck(&self, id: &str, thumbnails: bool) -> ConnResult<Value> {
            self.log
                .lock()
                .unwrap()
                .push(format!("describe_deck {id} {thumbnails}"));
            self.reply.clone().map_err(ConnectorError::msg)
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        log: Log,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
                log: Arc::default(),
            }
        }
        fn home(&self) -> &std::path::Path {
            self.dir.path()
        }
        fn state(&self) -> UiState {
            UiState::explicit(self.home(), self.home(), self.home(), self.home(), "tok", None)
        }
        fn env(&self, reply: Result<Value, String>) -> ToolsEnv {
            let accounts = vec![
                Account::new("gd", "google_docs"),
                Account::new("gs", "google_sheets"),
                Account::new("sl", "google_slides"),
            ];
            let log = self.log.clone();
            let all = accounts.clone();
            ToolsEnv::new(self.home().join("repo"), self.home()).with_provider(StaticProvider::new(
                accounts,
                move |id| {
                    log.lock().unwrap().push(format!("load {id}"));
                    let account = all
                        .iter()
                        .find(|a| a.id == id)
                        .cloned()
                        .ok_or_else(|| ConnectorError::msg("unknown"))?;
                    Ok(Box::new(Fake {
                        account,
                        log: log.clone(),
                        reply: reply.clone(),
                    }) as Box<dyn Connector>)
                },
            ))
        }
        fn created(&self, prefix: &str, account: &str, id: &str) {
            sstore::record_deck(&self.home().join(prefix), account, json!({"id": id})).unwrap();
        }
        fn render(&self, query: &str, reply: Result<Value, String>) -> String {
            let state = self.state();
            let req = UiRequest::get(&format!("/chat/preview?{query}"));
            fragment(&UiCtx::new(&state, &req), &self.env(reply))
        }
        fn calls(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    const DOC: &str = "session=s1&kind=doc&account=gd&file=doc1";

    #[test]
    fn a_doc_renders_escaped_paragraphs_with_open_and_refresh() {
        let fx = Fixture::new();
        fx.created("gdocs", "gd", "doc1");
        let html = fx.render(
            DOC,
            Ok(json!({"id": "doc1", "title": "Plan <b>", "truncated": false,
                "web_link": "https://docs.google.com/document/d/doc1/edit",
                "text": "Hello <script>alert(1)</script>\n\nSecond & last"})),
        );
        assert!(!html.contains("<script>") && !html.contains("<b>"), "{html}");
        assert!(html.contains("Hello &lt;script&gt;alert(1)&lt;/script&gt;"), "{html}");
        assert!(html.contains("<p>Second &amp; last</p>"), "{html}");
        assert!(html.contains("data-file-title=\"Plan &lt;b&gt;\""), "{html}");
        assert!(html.contains("href=\"https://docs.google.com/document/d/doc1/edit\""), "{html}");
        assert!(html.contains("Open in Google"), "{html}");
        assert!(html.contains("data-refresh"), "{html}");
        assert!(!html.contains("file-preview-note"), "{html}");
        assert_eq!(fx.calls(), ["load gd", "describe_doc doc1"]);
    }

    #[test]
    fn a_non_google_link_is_never_rendered() {
        let fx = Fixture::new();
        fx.created("gdocs", "gd", "doc1");
        let html = fx.render(
            DOC,
            Ok(json!({"title": "x", "text": "a", "web_link": "https://evil.example/doc1"})),
        );
        assert!(!html.contains("evil.example"), "{html}");
        assert!(html.contains("data-refresh"), "{html}");
    }

    #[test]
    fn truncated_content_shows_a_note() {
        let fx = Fixture::new();
        fx.created("gdocs", "gd", "doc1");
        let html = fx.render(DOC, Ok(json!({"title": "x", "text": "a", "truncated": true})));
        assert!(html.contains("file-preview-note"), "{html}");
        assert!(html.contains("only the first part is shown"), "{html}");
    }

    #[test]
    fn a_sheet_renders_one_table_per_tab_padding_ragged_rows() {
        let fx = Fixture::new();
        fx.created("gsheets", "gs", "sh1");
        let html = fx.render(
            "session=s1&kind=sheet&account=gs&file=sh1",
            Ok(json!({"title": "Budget", "truncated": false,
                "web_link": "https://docs.google.com/spreadsheets/d/sh1/edit",
                "tabs": [
                    {"name": "Q1 <x>", "rows": [["a", "b", "c"], ["<i>d</i>"]], "truncated": false},
                    {"name": "Empty", "rows": [], "truncated": false}]})),
        );
        assert_eq!(html.matches("<table").count(), 1, "{html}");
        assert!(html.contains("Q1 &lt;x&gt;") && html.contains(">Empty<"), "{html}");
        assert!(html.contains("<tr><td>&lt;i&gt;d&lt;/i&gt;</td><td></td><td></td></tr>"), "{html}");
        assert!(html.contains("This tab is empty."), "{html}");
        assert_eq!(fx.calls(), ["load gs", "describe_sheet sh1"]);
    }

    #[test]
    fn a_truncated_tab_also_shows_the_note() {
        let fx = Fixture::new();
        fx.created("gsheets", "gs", "sh1");
        let html = fx.render(
            "session=s1&kind=sheet&account=gs&file=sh1",
            Ok(json!({"title": "B", "truncated": false,
                "tabs": [{"name": "T", "rows": [["1"]], "truncated": true}]})),
        );
        assert!(html.contains("file-preview-note"), "{html}");
    }

    #[test]
    fn a_deck_renders_thumbnails_from_google_hosts_only_and_escaped_text() {
        let fx = Fixture::new();
        fx.created("slides", "sl", "dk1");
        let html = fx.render(
            "session=s1&kind=deck&account=sl&file=dk1",
            Ok(json!({"title": "Q4", "web_link": "https://docs.google.com/presentation/d/dk1/edit",
                "slides": [
                  {"index": 1, "text": "Hi <i>there</i>", "thumbnail_url": "https://lh3.googleusercontent.com/t?a=1&b=2"},
                  {"index": 2, "text": "", "thumbnail_url": "https://evil.example/t.png"}]})),
        );
        assert!(html.contains("src=\"https://lh3.googleusercontent.com/t?a=1&amp;b=2\""), "{html}");
        assert!(!html.contains("evil.example"), "{html}");
        assert!(html.contains("Hi &lt;i&gt;there&lt;/i&gt;"), "{html}");
        assert!(html.contains("slide-grid"), "{html}");
        assert_eq!(fx.calls(), ["load sl", "describe_deck dk1 true"]);
    }

    #[test]
    fn a_file_the_chat_may_not_open_is_refused_without_any_connector_call() {
        let fx = Fixture::new();
        // A deck id pasted into the chat is still not openable (Slides rule).
        loopx_core::file_grants::record(
            &loopx_core::file_grants::dir(fx.home()),
            "s1",
            &["pasted".into()],
        )
        .unwrap();
        for query in [
            "session=s1&kind=doc&account=gd&file=stranger",
            "session=s1&kind=deck&account=sl&file=pasted",
        ] {
            let html = fx.render(query, Ok(json!({"title": "secret", "text": "secret"})));
            assert!(html.contains("file-preview-error"), "{query}: {html}");
            assert!(!html.contains("secret"), "{query}: {html}");
        }
        assert!(fx.calls().is_empty(), "{:?}", fx.calls());
    }

    #[test]
    fn invalid_parameters_are_rejected_before_any_lookup() {
        let fx = Fixture::new();
        fx.created("gdocs", "gd", "doc1");
        for query in [
            "session=s1&kind=pdf&account=gd&file=doc1",
            "session=s1&kind=doc&account=GD&file=doc1",
            "session=s1&kind=doc&account=gd&file=../doc1",
            "session=s1&kind=doc&account=gd&file=",
            "session=../x&kind=doc&account=gd&file=doc1",
            "kind=doc&file=doc1",
        ] {
            let html = fx.render(query, Ok(json!({"title": "x", "text": "x"})));
            assert!(html.contains("file-preview-error"), "{query}: {html}");
        }
        assert!(fx.calls().is_empty(), "{:?}", fx.calls());
    }

    #[test]
    fn an_account_of_another_type_is_refused_without_loading_it() {
        let fx = Fixture::new();
        fx.created("gdocs", "gs", "doc1");
        let html = fx.render(
            "session=s1&kind=doc&account=gs&file=doc1",
            Ok(json!({"title": "x", "text": "x"})),
        );
        assert!(html.contains("file-preview-error"), "{html}");
        assert!(fx.calls().is_empty(), "{:?}", fx.calls());
    }

    #[test]
    fn a_failing_google_call_returns_the_error_fragment_with_refresh() {
        let fx = Fixture::new();
        fx.created("gdocs", "gd", "doc1");
        let html = fx.render(DOC, Err("Reconnect Google Docs <now>".into()));
        assert!(html.contains("file-preview-error"), "{html}");
        assert!(html.contains("Reconnect Google Docs &lt;now&gt;"), "{html}");
        assert!(html.contains("data-refresh"), "{html}");
    }

    #[test]
    fn a_pasted_doc_opens_in_its_own_session_only() {
        let fx = Fixture::new();
        loopx_core::file_grants::record(
            &loopx_core::file_grants::dir(fx.home()),
            "s1",
            &["pasted".into()],
        )
        .unwrap();
        let ok = Ok(json!({"title": "Shared", "text": "body"}));
        let mine = fx.render("session=s1&kind=doc&account=gd&file=pasted", ok.clone());
        assert!(mine.contains("<p>body</p>"), "{mine}");
        let other = fx.render("session=s2&kind=doc&account=gd&file=pasted", ok);
        assert!(other.contains("file-preview-error"), "{other}");
        assert_eq!(fx.calls(), ["load gd", "describe_doc pasted"]);
    }

    #[test]
    fn the_route_is_a_get_that_answers_200_with_the_error_fragment() {
        let fx = Fixture::new();
        let st = fx.state();
        let r = st
            .registry
            .dispatch(&st, &UiRequest::get("/chat/preview?session=s1&kind=doc&account=gd&file=nope"))
            .unwrap();
        assert_eq!(r.status, 200);
        assert!(r.body_text().contains("file-preview-error"));
        assert_eq!(r.header("Cache-Control"), Some("no-store"));
    }

    #[test]
    fn a_cross_site_request_is_refused() {
        let fx = Fixture::new();
        fx.created("gdocs", "gd", "doc1");
        let state = fx.state();
        let req = UiRequest::get(&format!("/chat/preview?{DOC}"))
            .with_header("sec-fetch-site", "cross-site");
        let html = fragment(&UiCtx::new(&state, &req), &fx.env(Ok(json!({"text": "x"}))));
        assert!(html.contains("file-preview-error"), "{html}");
        assert!(fx.calls().is_empty());
        for site in ["same-origin", "none"] {
            let req = UiRequest::get(&format!("/chat/preview?{DOC}")).with_header("sec-fetch-site", site);
            let html = fragment(&UiCtx::new(&state, &req), &fx.env(Ok(json!({"text": "x"}))));
            assert!(!html.contains("file-preview-error"), "{site}: {html}");
        }
    }
}
