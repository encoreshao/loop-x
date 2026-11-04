//! Google Slides connector: lists the presentations a user can use as templates
//! (Drive), copies one into a new deck, fills its `{{placeholders}}`, and reads
//! a deck back (text plus slide thumbnails) for review. The stored secret is a
//! Google refresh token from the dashboard's Connect with Google flow
//! (`auth = OauthGoogle`); the OAuth client is the one saved on the Inbox Triage
//! setup page. No message built here ever contains a token.

use super::drive::{self, placeholders_in, text_of};
use super::google_auth::GoogleAuth;
pub use super::google_auth::{ClientFn, RefreshError, RefreshFn};
use super::http::{quote, urlencode, TEST_TIMEOUT_SECONDS};
use super::*;
use serde_json::json;

pub const SLIDES_URL: &str = "https://slides.googleapis.com/v1/presentations";
/// Slides whose thumbnail is fetched for a review; later ones are text only.
const MAX_THUMBNAILS: usize = 30;

pub const INFO: TypeInfo = TypeInfo {
    name: "google_slides",
    label: "Google Slides",
    icon: "slideshow",
    capabilities: &[SLIDES],
    fields: &[],
    secret_label: None,
    external: false,
    brand: "googleslides",
    description: "Pick shared slide templates, create a new deck from one with your approval in chat, and review it here.",
    docs_url: "https://support.google.com/docs/answer/1696787",
    category: "google",
    presets: &[],
    auth: Auth::OauthGoogle,
};

pub struct GoogleSlidesConnector {
    account: Account,
    http: Arc<dyn HttpClient>,
    auth: GoogleAuth,
}

pub fn web_link(deck_id: &str) -> String {
    format!("https://docs.google.com/presentation/d/{deck_id}/edit")
}

/// All the text on one slide, one line per text box.
fn slide_text(slide: &Value) -> String {
    let mut lines = Vec::new();
    for el in slide
        .get("pageElements")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let runs = el
            .pointer("/shape/text/textElements")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        let line: String = runs
            .filter_map(|t| t.pointer("/textRun/content").and_then(Value::as_str))
            .collect();
        let line = line.trim();
        if !line.is_empty() {
            lines.push(line.to_string());
        }
    }
    lines.join("\n")
}

/// An edit op is `{"op": "replace", "find": non-empty text, "with": text}`.
pub fn validate_op(op: &Value) -> ConnResult<()> {
    let bad = |m: &str| Err(ConnectorError::msg(m.to_string()));
    if op.get("op").and_then(Value::as_str) != Some("replace") {
        return bad("A deck edit op must be {\"op\": \"replace\", \"find\", \"with\"}");
    }
    match op.get("find").and_then(Value::as_str) {
        Some(f) if !f.is_empty() => {}
        _ => return bad("replace needs a non-empty \"find\" text"),
    }
    if op.get("with").and_then(Value::as_str).is_none() {
        return bad("replace needs a \"with\" text (it may be empty)");
    }
    Ok(())
}

impl GoogleSlidesConnector {
    pub fn new(account: Account, secret: Option<String>, http: Arc<dyn HttpClient>) -> Self {
        Self {
            account,
            auth: GoogleAuth::new(secret, http.clone()),
            http,
        }
    }

    pub fn with_client_fn(mut self, f: ClientFn) -> Self {
        self.auth.client_fn = f;
        self
    }

    pub fn with_refresh_fn(mut self, f: RefreshFn) -> Self {
        self.auth.refresh_fn = f;
        self
    }

    fn fail(&self, e: HttpError, token: &str) -> ConnectorError {
        match e.status {
            Some(401) | Some(403) => {
                ConnectorError::msg("Reconnect Google Slides to grant Slides and Drive access")
            }
            _ => ConnectorError::msg(self.auth.describe(&ConnectorError::Http(e), Some(token))),
        }
    }

    fn call(
        &self,
        method: &str,
        url: &str,
        token: &str,
        body: Option<Value>,
        attempts: u32,
    ) -> ConnResult<Value> {
        let req = HttpRequest::new(method, url)
            .token(Some(token))
            .json(body)
            .timeout_secs(30)
            .attempts(attempts);
        self.http
            .request_json(&req)
            .map_err(|e| self.fail(e, token))
    }
}

impl Connector for GoogleSlidesConnector {
    fn info(&self) -> &'static TypeInfo {
        &INFO
    }

    fn account(&self) -> &Account {
        &self.account
    }

    fn test(&self) -> (bool, String) {
        let token = match self.auth.access_token() {
            Ok(t) => t,
            Err(m) => return (false, m),
        };
        let url = drive::probe_url(drive::PRESENTATION_MIME);
        let req = HttpRequest::new("GET", &url)
            .token(Some(&token))
            .timeout_secs(TEST_TIMEOUT_SECONDS)
            .attempts(1);
        match self.http.request_json(&req) {
            Ok(_) => (
                true,
                "Connected: Google Slides and Drive are reachable".into(),
            ),
            Err(e) => (
                false,
                self.auth.describe(&ConnectorError::Http(e), Some(&token)),
            ),
        }
    }

    fn list_templates(&self, query: &str, limit: u32) -> ConnResult<Vec<Value>> {
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let url = drive::list_url(drive::PRESENTATION_MIME, query, limit);
        let data = self.call("GET", &url, &token, None, 3)?;
        Ok(drive::rows(&data, web_link))
    }

    fn copy_deck(&self, template_id: &str, title: &str) -> ConnResult<Value> {
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let url = drive::copy_url(template_id);
        let made = self.call("POST", &url, &token, Some(json!({"name": title})), 1)?;
        let id = text_of(made.get("id"));
        if id.is_empty() {
            return Err(ConnectorError::msg(
                "Google returned no id for the new deck",
            ));
        }
        Ok(json!({"id": id, "web_link": web_link(&id)}))
    }

    fn fill_deck(&self, deck_id: &str, replacements: &[(String, String)]) -> ConnResult<Value> {
        if replacements.is_empty() {
            return Ok(json!({"replaced": 0}));
        }
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let requests: Vec<Value> = replacements
            .iter()
            .map(|(k, v)| {
                json!({"replaceAllText": {
                    "containsText": {"text": format!("{{{{{k}}}}}"), "matchCase": true},
                    "replaceText": v}})
            })
            .collect();
        let url = format!("{SLIDES_URL}/{}:batchUpdate", quote(deck_id));
        let done = self.call("POST", &url, &token, Some(json!({"requests": requests})), 1)?;
        let replaced: u64 = done
            .get("replies")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| r.pointer("/replaceAllText/occurrencesChanged")?.as_u64())
            .sum();
        Ok(json!({"replaced": replaced}))
    }

    /// Ops run in order, one `replaceAllText` request each (matching case). Every
    /// op is checked before the first request; a later failure says how many ops
    /// were applied.
    fn edit_deck(&self, deck_id: &str, ops: &[Value]) -> ConnResult<Value> {
        for op in ops {
            validate_op(op)?;
        }
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let url = format!("{SLIDES_URL}/{}:batchUpdate", quote(deck_id));
        let mut applied = 0u64;
        for op in ops {
            let find = op.get("find").and_then(Value::as_str).unwrap_or_default();
            let with = op.get("with").and_then(Value::as_str).unwrap_or_default();
            let request = json!({"replaceAllText": {
                "containsText": {"text": find, "matchCase": true}, "replaceText": with}});
            self.call(
                "POST",
                &url,
                &token,
                Some(json!({"requests": [request]})),
                1,
            )
            .map_err(|e| {
                ConnectorError::msg(format!(
                    "{e} ({applied} earlier edit{} applied)",
                    if applied == 1 { "" } else { "s" }
                ))
            })?;
            applied += 1;
        }
        Ok(json!({"applied": applied}))
    }

    fn describe_deck(&self, deck_id: &str, thumbnails: bool) -> ConnResult<Value> {
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let id = quote(deck_id);
        let deck = self.call("GET", &format!("{SLIDES_URL}/{id}"), &token, None, 3)?;
        let mut all_text = String::new();
        let mut slides = Vec::new();
        for (i, slide) in deck
            .get("slides")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let text = slide_text(slide);
            all_text.push_str(&text);
            all_text.push('\n');
            let page = text_of(slide.get("objectId"));
            let thumb = if thumbnails && i < MAX_THUMBNAILS && !page.is_empty() {
                let url = format!(
                    "{SLIDES_URL}/{id}/pages/{}/thumbnail?{}",
                    quote(&page),
                    urlencode(&[
                        ("thumbnailProperties.mimeType", "PNG".to_string()),
                        ("thumbnailProperties.thumbnailSize", "MEDIUM".to_string()),
                    ])
                );
                // A slide without a preview must not fail the whole review.
                self.call("GET", &url, &token, None, 2)
                    .ok()
                    .map(|t| text_of(t.get("contentUrl")))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            slides.push(json!({"index": i + 1, "id": page, "text": text, "thumbnail_url": thumb}));
        }
        Ok(json!({
            "id": deck_id,
            "title": text_of(deck.get("title")),
            "web_link": web_link(deck_id),
            "placeholders": placeholders_in(&all_text),
            "slides": slides,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::FakeHttp;
    use super::*;

    fn slides(http: Arc<dyn HttpClient>, secret: Option<&str>) -> GoogleSlidesConnector {
        GoogleSlidesConnector::new(
            Account::new("s", "google_slides"),
            secret.map(str::to_string),
            http,
        )
        .with_client_fn(Arc::new(|| json!({"client_id": "c", "client_secret": "s"})))
        .with_refresh_fn(Arc::new(|_, _| Ok("AT".to_string())))
    }

    #[test]
    fn type_metadata() {
        assert_eq!(
            (INFO.label, INFO.category, INFO.auth, INFO.secret_label),
            ("Google Slides", "google", Auth::OauthGoogle, None)
        );
        assert_eq!(INFO.capabilities, &[SLIDES]);
        assert!(get_type("google_slides").is_some());
    }

    #[test]
    fn list_templates_queries_drive_and_shapes_rows() {
        let http = FakeHttp::ok(json!({"files": [
            {"id": "d1", "name": "Weekly", "modifiedTime": "2026-10-01T00:00:00Z",
             "webViewLink": "https://docs.google.com/presentation/d/d1/edit",
             "ownedByMe": false, "owners": [{"displayName": "Ana"}]},
            {"id": "d2", "name": "Pitch"}]}));
        let rows = slides(http.clone(), Some("RT"))
            .list_templates("it's", 500)
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["owner"], "Ana");
        assert_eq!(rows[0]["owned_by_me"], false);
        assert_eq!(
            rows[1]["web_link"],
            "https://docs.google.com/presentation/d/d2/edit"
        );
        let call = http.calls.lock().unwrap()[0].clone();
        assert!(call
            .url
            .starts_with("https://www.googleapis.com/drive/v3/files?"));
        assert!(call.url.contains("pageSize=100"), "{}", call.url);
        assert!(
            call.url.contains("name+contains+%27it%5C%27s%27"),
            "{}",
            call.url
        );
        assert_eq!(call.token.as_deref(), Some("AT"));
    }

    #[test]
    fn copy_deck_posts_the_title_and_returns_the_link() {
        let http = FakeHttp::ok(json!({"id": "new1"}));
        let made = slides(http.clone(), Some("RT"))
            .copy_deck("tpl 1", "Q4 review")
            .unwrap();
        assert_eq!(made["id"], "new1");
        assert_eq!(
            made["web_link"],
            "https://docs.google.com/presentation/d/new1/edit"
        );
        let call = http.calls.lock().unwrap()[0].clone();
        assert_eq!(call.method, "POST");
        assert!(call.url.contains("/files/tpl%201/copy"), "{}", call.url);
        assert_eq!(call.json_body.unwrap(), json!({"name": "Q4 review"}));
        assert_eq!(
            call.max_attempts, 1,
            "a copy must never be retried into a duplicate"
        );
    }

    #[test]
    fn fill_deck_sends_one_replace_per_key_and_counts() {
        let http = FakeHttp::ok(json!({"replies": [
            {"replaceAllText": {"occurrencesChanged": 2}}, {"replaceAllText": {}}]}));
        let out = slides(http.clone(), Some("RT"))
            .fill_deck(
                "d",
                &[("name".into(), "Ana".into()), ("date".into(), "Oct".into())],
            )
            .unwrap();
        assert_eq!(out["replaced"], 2);
        let body = http.calls.lock().unwrap()[0].json_body.clone().unwrap();
        assert_eq!(
            body["requests"][0]["replaceAllText"]["containsText"]["text"],
            "{{name}}"
        );
        assert_eq!(body["requests"][1]["replaceAllText"]["replaceText"], "Oct");
        let none = slides(FakeHttp::ok(json!({})), Some("RT"))
            .fill_deck("d", &[])
            .unwrap();
        assert_eq!(none["replaced"], 0);
    }

    #[test]
    fn describe_deck_reads_text_placeholders_and_thumbnails() {
        let http = FakeHttp::new(|req| {
            if req.url.contains("/thumbnail") {
                return Ok(json!({"contentUrl": "https://lh3.googleusercontent.com/t"}));
            }
            Ok(
                json!({"title": "Deck", "slides": [{"objectId": "p1", "pageElements": [
                {"shape": {"text": {"textElements": [
                    {"textRun": {"content": "Hello {{name}}\n"}}]}}}]}]}),
            )
        });
        let deck = slides(http.clone(), Some("RT"))
            .describe_deck("d1", true)
            .unwrap();
        assert_eq!(deck["title"], "Deck");
        assert_eq!(deck["placeholders"], json!(["name"]));
        assert_eq!(deck["slides"][0]["text"], "Hello {{name}}");
        assert_eq!(
            deck["slides"][0]["thumbnail_url"],
            "https://lh3.googleusercontent.com/t"
        );
        let bare = slides(http, Some("RT")).describe_deck("d1", false).unwrap();
        assert_eq!(bare["slides"][0]["thumbnail_url"], "");
    }

    #[test]
    fn a_failed_thumbnail_does_not_fail_the_review() {
        let http = FakeHttp::new(|req| {
            if req.url.contains("/thumbnail") {
                return Err(HttpError::new(Some(500), "boom", &req.url));
            }
            Ok(json!({"slides": [{"objectId": "p1"}]}))
        });
        let deck = slides(http, Some("RT")).describe_deck("d1", true).unwrap();
        assert_eq!(deck["slides"][0]["thumbnail_url"], "");
    }

    #[test]
    fn permission_errors_ask_to_reconnect_and_never_leak_tokens() {
        let http =
            FakeHttp::new(|req| Err(HttpError::new(Some(403), "denied AT-SECRET", &req.url)));
        let c =
            GoogleSlidesConnector::new(Account::new("s", "google_slides"), Some("RT".into()), http)
                .with_client_fn(Arc::new(|| json!({"client_id": "c", "client_secret": "s"})))
                .with_refresh_fn(Arc::new(|_, _| Ok("AT-SECRET".to_string())));
        let msg = c.list_templates("", 10).unwrap_err().to_string();
        assert!(
            msg.contains("Reconnect") && !msg.contains("AT-SECRET"),
            "{msg}"
        );
        let (ok, msg) = c.test();
        assert!(!ok && !msg.contains("AT-SECRET"), "{msg}");
    }

    #[test]
    fn edit_deck_sends_one_case_sensitive_replace_per_op_in_order() {
        let http =
            FakeHttp::ok(json!({"replies": [{"replaceAllText": {"occurrencesChanged": 1}}]}));
        let out = slides(http.clone(), Some("RT"))
            .edit_deck(
                "d 1",
                &[
                    json!({"op": "replace", "find": "Old", "with": "New"}),
                    json!({"op": "replace", "find": "A", "with": ""}),
                ],
            )
            .unwrap();
        assert_eq!(out["applied"], 2);
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].method, "POST");
        assert!(
            calls[0].url.ends_with("/d%201:batchUpdate"),
            "{}",
            calls[0].url
        );
        let b0 = calls[0].json_body.clone().unwrap();
        assert_eq!(b0["requests"].as_array().unwrap().len(), 1);
        let r = &b0["requests"][0]["replaceAllText"];
        assert_eq!(r["containsText"], json!({"text": "Old", "matchCase": true}));
        assert_eq!(r["replaceText"], "New");
        let b1 = calls[1].json_body.clone().unwrap();
        assert_eq!(
            b1["requests"][0]["replaceAllText"]["containsText"]["text"],
            "A"
        );
        assert_eq!(b1["requests"][0]["replaceAllText"]["replaceText"], "");
    }

    #[test]
    fn edit_deck_rejects_bad_ops_before_any_request() {
        for ops in [
            vec![json!({"op": "append", "text": "x"})],
            vec![json!({"op": "replace", "find": "", "with": "x"})],
            vec![json!({"op": "replace", "with": "x"})],
            vec![json!({"op": "replace", "find": "a"})],
            vec![json!({"op": "replace", "find": "a", "with": 1})],
            vec![
                json!({"op": "replace", "find": "a", "with": "b"}),
                json!({"op": "insert_after", "find": "a", "text": "b"}),
            ],
            vec![json!("replace")],
        ] {
            let http = FakeHttp::ok(json!({}));
            let r = slides(http.clone(), Some("RT")).edit_deck("d", &ops);
            assert!(r.is_err(), "{ops:?}");
            assert!(http.calls.lock().unwrap().is_empty(), "{ops:?}");
        }
    }

    #[test]
    fn edit_deck_failure_reports_how_many_ops_were_applied() {
        let n = std::sync::atomic::AtomicUsize::new(0);
        let http =
            FakeHttp::new(
                move |req| match n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => Ok(json!({})),
                    1 => Err(HttpError::new(Some(500), "boom AT-SECRET", &req.url)),
                    _ => Ok(json!({})),
                },
            );
        let op = json!({"op": "replace", "find": "a", "with": "b"});
        let e = slides(http.clone(), Some("RT"))
            .edit_deck("d", &[op.clone(), op.clone(), op.clone()])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("1 earlier edit applied") && !e.contains("AT-SECRET"),
            "{e}"
        );
        assert_eq!(http.calls.lock().unwrap().len(), 2);
        // plural, and a first-op failure says none were applied
        let n = std::sync::atomic::AtomicUsize::new(0);
        let http = FakeHttp::new(move |req| {
            if n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 2 {
                return Err(HttpError::new(Some(500), "boom", &req.url));
            }
            Ok(json!({}))
        });
        let e = slides(http, Some("RT"))
            .edit_deck("d", &[op.clone(), op.clone(), op.clone()])
            .unwrap_err()
            .to_string();
        assert!(e.contains("2 earlier edits applied"), "{e}");
        let http = FakeHttp::new(|req| Err(HttpError::new(Some(500), "boom", &req.url)));
        let e = slides(http, Some("RT"))
            .edit_deck("d", &[op])
            .unwrap_err()
            .to_string();
        assert!(e.contains("0 earlier edits applied"), "{e}");
    }

    #[test]
    fn edit_deck_permission_error_asks_to_reconnect_without_the_token() {
        let http =
            FakeHttp::new(|req| Err(HttpError::new(Some(403), "denied AT-SECRET", &req.url)));
        let c = slides(http, Some("RT"));
        let e = c
            .edit_deck("d", &[json!({"op": "replace", "find": "a", "with": "b"})])
            .unwrap_err()
            .to_string();
        assert!(e.contains("Reconnect") && !e.contains("AT-SECRET"), "{e}");
    }

    #[test]
    fn unconnected_account_says_to_connect() {
        let c = slides(FakeHttp::ok(json!({})), None);
        let (ok, msg) = c.test();
        assert!(!ok && msg.contains("Connect with Google"), "{msg}");
        assert!(c.list_templates("", 10).is_err());
    }
}
