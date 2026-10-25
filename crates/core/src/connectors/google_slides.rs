//! Google Slides connector: lists the presentations a user can use as templates
//! (Drive), copies one into a new deck, fills its `{{placeholders}}`, and reads
//! a deck back (text plus slide thumbnails) for review. The stored secret is a
//! Google refresh token from the dashboard's Connect with Google flow
//! (`auth = OauthGoogle`); the OAuth client is the one saved on the Inbox Triage
//! setup page. No message built here ever contains a token.

use super::google_auth::GoogleAuth;
pub use super::google_auth::{ClientFn, RefreshError, RefreshFn};
use super::http::{quote, urlencode, TEST_TIMEOUT_SECONDS};
use super::*;
use regex::Regex;
use serde_json::json;
use std::sync::OnceLock;

pub const SLIDES_URL: &str = "https://slides.googleapis.com/v1/presentations";
pub const DRIVE_URL: &str = "https://www.googleapis.com/drive/v3";
const PRESENTATION_MIME: &str = "application/vnd.google-apps.presentation";
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

fn placeholder_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\{\{\s*([A-Za-z0-9_][A-Za-z0-9_. -]{0,59}?)\s*\}\}").expect("static regex")
    })
}

/// The distinct `{{name}}` placeholders in `text`, in first-seen order.
pub fn placeholders_in(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for c in placeholder_re().captures_iter(text) {
        let name = c[1].to_string();
        if !found.contains(&name) {
            found.push(name);
        }
    }
    found
}

pub fn web_link(deck_id: &str) -> String {
    format!("https://docs.google.com/presentation/d/{deck_id}/edit")
}

fn text_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
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
        let url = format!(
            "{DRIVE_URL}/files?{}",
            urlencode(&[
                (
                    "q",
                    format!("mimeType='{PRESENTATION_MIME}' and trashed=false")
                ),
                ("pageSize", "1".to_string()),
                ("fields", "files(id)".to_string()),
            ])
        );
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
        let mut q = format!("mimeType='{PRESENTATION_MIME}' and trashed=false");
        let term = query.trim().replace('\\', "").replace('\'', "\\'");
        if !term.is_empty() {
            q.push_str(&format!(" and name contains '{term}'"));
        }
        let url = format!(
            "{DRIVE_URL}/files?{}",
            urlencode(&[
                ("q", q),
                ("pageSize", limit.min(100).to_string()),
                ("orderBy", "modifiedTime desc".to_string()),
                (
                    "fields",
                    "files(id,name,modifiedTime,webViewLink,ownedByMe,owners(displayName))".into()
                ),
                ("supportsAllDrives", "true".to_string()),
                ("includeItemsFromAllDrives", "true".to_string()),
            ])
        );
        let data = self.call("GET", &url, &token, None, 3)?;
        Ok(data
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|f| f.is_object())
            .map(|f| {
                let id = text_of(f.get("id"));
                let link = match text_of(f.get("webViewLink")) {
                    l if l.is_empty() => web_link(&id),
                    l => l,
                };
                json!({
                    "id": id,
                    "name": text_of(f.get("name")),
                    "modified": text_of(f.get("modifiedTime")),
                    "web_link": link,
                    "owned_by_me": f.get("ownedByMe").and_then(Value::as_bool).unwrap_or(false),
                    "owner": text_of(f.pointer("/owners/0/displayName")),
                })
            })
            .collect())
    }

    fn copy_deck(&self, template_id: &str, title: &str) -> ConnResult<Value> {
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let url = format!(
            "{DRIVE_URL}/files/{}/copy?supportsAllDrives=true&fields=id",
            quote(template_id)
        );
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
    fn placeholders_are_distinct_and_trimmed() {
        assert_eq!(
            placeholders_in("Hi {{ name }}, {{date}} and {{name}} {{ }} {single}"),
            vec!["name", "date"]
        );
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
    fn unconnected_account_says_to_connect() {
        let c = slides(FakeHttp::ok(json!({})), None);
        let (ok, msg) = c.test();
        assert!(!ok && msg.contains("Connect with Google"), "{msg}");
        assert!(c.list_templates("", 10).is_err());
    }
}
