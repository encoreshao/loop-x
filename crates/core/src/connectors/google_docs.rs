//! Google Docs connector: lists document templates (Drive), copies one,
//! fills its `{{placeholders}}`, creates, reads and edits documents. The stored secret is a Google refresh token from the
//! dashboard's Connect with Google flow (`auth = OauthGoogle`); the OAuth client
//! is the one saved on the Inbox Triage setup page. No message built here ever
//! contains a token.

use super::drive::{self, placeholders_in, text_of};
use super::google_auth::GoogleAuth;
pub use super::google_auth::{ClientFn, RefreshError, RefreshFn};
use super::http::{quote, TEST_TIMEOUT_SECONDS};
use super::*;
use serde_json::json;

pub const DOCS_URL: &str = "https://docs.googleapis.com/v1/documents";
/// Document text returned to the caller is cut at this many characters.
const MAX_TEXT_CHARS: usize = 60_000;

pub const INFO: TypeInfo = TypeInfo {
    name: "google_docs",
    label: "Google Docs",
    icon: "description",
    capabilities: &[GDOCS],
    fields: &[],
    secret_label: None,
    external: false,
    brand: "googledocs",
    description: "Pick shared document templates, create a new document from one with your approval in chat, and review it here.",
    docs_url: "https://support.google.com/docs/answer/6000292",
    category: "google",
    presets: &[],
    auth: Auth::OauthGoogle,
};

pub struct GoogleDocsConnector {
    account: Account,
    http: Arc<dyn HttpClient>,
    auth: GoogleAuth,
}

pub fn web_link(id: &str) -> String {
    format!("https://docs.google.com/document/d/{id}/edit")
}

/// The paragraphs of a document body, in order, as `(start, end, text)`.
/// Table cells are walked too.
fn paragraphs(content: Option<&Value>, out: &mut Vec<(u64, u64, String)>) {
    for el in content.and_then(Value::as_array).into_iter().flatten() {
        if let Some(p) = el.get("paragraph") {
            let text: String = p
                .get("elements")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|e| e.pointer("/textRun/content").and_then(Value::as_str))
                .collect();
            let start = el.get("startIndex").and_then(Value::as_u64).unwrap_or(0);
            let end = el.get("endIndex").and_then(Value::as_u64).unwrap_or(0);
            out.push((start, end, text));
        }
        for row in el
            .pointer("/table/tableRows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for cell in row
                .get("tableCells")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                paragraphs(cell.get("content"), out);
            }
        }
    }
}

fn str_field<'a>(op: &'a Value, key: &str) -> &'a str {
    op.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Check one edit op's shape; the error names the op.
fn validate_op(op: &Value) -> ConnResult<()> {
    let kind = str_field(op, "op");
    let ok = match kind {
        "replace" => {
            !str_field(op, "find").is_empty() && op.get("with").is_some_and(Value::is_string)
        }
        "append" => !str_field(op, "text").is_empty(),
        "insert_after" => !str_field(op, "find").is_empty() && !str_field(op, "text").is_empty(),
        other => {
            return Err(ConnectorError::msg(format!(
                "Unknown edit op \"{other}\"; use replace, append or insert_after"
            )))
        }
    };
    if ok {
        Ok(())
    } else {
        Err(ConnectorError::msg(format!(
            "The {kind} edit op is missing a required field"
        )))
    }
}

fn replace_request(find: &str, with: &str) -> Value {
    json!({"replaceAllText": {
        "containsText": {"text": find, "matchCase": true},
        "replaceText": with}})
}

impl GoogleDocsConnector {
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
                ConnectorError::msg("Reconnect Google Docs to grant Docs and Drive access")
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

    fn batch_update(&self, doc_id: &str, token: &str, requests: Vec<Value>) -> ConnResult<Value> {
        let url = format!("{DOCS_URL}/{}:batchUpdate", quote(doc_id));
        self.call("POST", &url, token, Some(json!({"requests": requests})), 1)
    }

    fn fetch(&self, doc_id: &str, token: &str) -> ConnResult<Value> {
        self.call(
            "GET",
            &format!("{DOCS_URL}/{}", quote(doc_id)),
            token,
            None,
            3,
        )
    }
}

impl Connector for GoogleDocsConnector {
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
        let req = HttpRequest::new("GET", &drive::probe_url(drive::DOCUMENT_MIME))
            .token(Some(&token))
            .timeout_secs(TEST_TIMEOUT_SECONDS)
            .attempts(1);
        match self.http.request_json(&req) {
            Ok(_) => (
                true,
                "Connected: Google Docs and Drive are reachable".into(),
            ),
            Err(e) => match e.status {
                Some(401) | Some(403) => (
                    false,
                    "Reconnect Google Docs to grant Docs and Drive access".into(),
                ),
                _ => (
                    false,
                    self.auth.describe(&ConnectorError::Http(e), Some(&token)),
                ),
            },
        }
    }

    fn list_templates(&self, query: &str, limit: u32) -> ConnResult<Vec<Value>> {
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let url = drive::list_url(drive::DOCUMENT_MIME, query, limit);
        let data = self.call("GET", &url, &token, None, 3)?;
        Ok(drive::rows(&data, web_link))
    }

    fn copy_doc(&self, template_id: &str, title: &str) -> ConnResult<Value> {
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let url = drive::copy_url(template_id);
        let made = self.call("POST", &url, &token, Some(json!({"name": title})), 1)?;
        let id = text_of(made.get("id"));
        if id.is_empty() {
            return Err(ConnectorError::msg(
                "Google returned no id for the new document",
            ));
        }
        Ok(json!({"id": id, "web_link": web_link(&id)}))
    }

    fn fill_doc(&self, doc_id: &str, replacements: &[(String, String)]) -> ConnResult<Value> {
        if replacements.is_empty() {
            return Ok(json!({"replaced": 0}));
        }
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let requests = replacements
            .iter()
            .map(|(k, v)| replace_request(&format!("{{{{{k}}}}}"), v))
            .collect();
        let done = self.batch_update(doc_id, &token, requests)?;
        let replaced: u64 = done
            .get("replies")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| r.pointer("/replaceAllText/occurrencesChanged")?.as_u64())
            .sum();
        Ok(json!({"replaced": replaced}))
    }

    fn create_doc(&self, title: &str, text: &str) -> ConnResult<Value> {
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let made = self.call("POST", DOCS_URL, &token, Some(json!({"title": title})), 1)?;
        let id = match text_of(made.get("documentId")) {
            i if i.is_empty() => text_of(made.get("id")),
            i => i,
        };
        if id.is_empty() {
            return Err(ConnectorError::msg(
                "Google returned no id for the new document",
            ));
        }
        if !text.is_empty() {
            let insert = json!({"insertText": {"location": {"index": 1}, "text": text}});
            self.batch_update(&id, &token, vec![insert])?;
        }
        Ok(json!({"id": id, "web_link": web_link(&id)}))
    }

    fn describe_doc(&self, doc_id: &str) -> ConnResult<Value> {
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let doc = self.fetch(doc_id, &token)?;
        let mut paras = Vec::new();
        paragraphs(doc.pointer("/body/content"), &mut paras);
        let full: String = paras.into_iter().map(|p| p.2).collect();
        let truncated = full.chars().count() > MAX_TEXT_CHARS;
        let text: String = if truncated {
            full.chars().take(MAX_TEXT_CHARS).collect()
        } else {
            full.clone()
        };
        Ok(json!({
            "id": doc_id,
            "title": text_of(doc.get("title")),
            "web_link": web_link(doc_id),
            "placeholders": placeholders_in(&full),
            "text": text,
            "truncated": truncated,
        }))
    }

    fn edit_doc(&self, doc_id: &str, ops: &[Value]) -> ConnResult<Value> {
        for op in ops {
            validate_op(op)?;
        }
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let mut applied = 0u64;
        for op in ops {
            let request = match str_field(op, "op") {
                "replace" => replace_request(str_field(op, "find"), str_field(op, "with")),
                kind => {
                    let doc = self.fetch(doc_id, &token)?;
                    let doc_end = doc
                        .pointer("/body/content")
                        .and_then(Value::as_array)
                        .and_then(|c| c.last())
                        .and_then(|e| e.get("endIndex")?.as_u64())
                        .unwrap_or(1);
                    if kind == "append" {
                        json!({"insertText": {
                            "location": {"index": doc_end.saturating_sub(1).max(1)},
                            "text": str_field(op, "text")}})
                    } else {
                        let find = str_field(op, "find");
                        let mut paras = Vec::new();
                        paragraphs(doc.pointer("/body/content"), &mut paras);
                        let Some((_, end, _)) = paras.iter().find(|p| p.2.contains(find)) else {
                            return Err(ConnectorError::msg(format!(
                                "insert_after: no paragraph contains \"{find}\" ({applied} earlier edits applied)"
                            )));
                        };
                        let text = str_field(op, "text");
                        if *end >= doc_end {
                            // Last paragraph: the index at the end is not insertable.
                            json!({"insertText": {
                                "location": {"index": end.saturating_sub(1).max(1)},
                                "text": format!("\n{text}")}})
                        } else {
                            json!({"insertText": {
                                "location": {"index": end},
                                "text": format!("{text}\n")}})
                        }
                    }
                }
            };
            self.batch_update(doc_id, &token, vec![request])?;
            applied += 1;
        }
        Ok(json!({"applied": applied}))
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::FakeHttp;
    use super::*;
    use serde_json::json;

    fn conn(http: Arc<dyn HttpClient>, secret: Option<&str>) -> GoogleDocsConnector {
        GoogleDocsConnector::new(
            Account::new("a", "google_docs"),
            secret.map(str::to_string),
            http,
        )
        .with_client_fn(Arc::new(|| json!({"client_id": "c", "client_secret": "s"})))
        .with_refresh_fn(Arc::new(|_, _| Ok("AT-SECRET".to_string())))
    }

    #[test]
    fn type_metadata() {
        assert_eq!(
            (
                INFO.label,
                INFO.category,
                INFO.auth,
                INFO.secret_label,
                INFO.brand
            ),
            (
                "Google Docs",
                "google",
                Auth::OauthGoogle,
                None,
                "googledocs"
            )
        );
        assert_eq!(INFO.capabilities, &[GDOCS]);
        assert!(INFO.fields.is_empty());
        assert!(get_type("google_docs").is_some());
        assert!(web_link("x1").contains("/x1/"));
    }

    #[test]
    fn test_lists_one_drive_file_of_the_type() {
        let http = FakeHttp::ok(json!({"files": []}));
        let (ok, msg) = conn(http.clone(), Some("RT")).test();
        assert!(ok, "{msg}");
        let calls = http.calls.lock().unwrap();
        assert!(
            calls.iter().any(|c| c.url.starts_with(drive::DRIVE_URL)
                && c.url.contains("google-apps.document")
                && c.url.contains("pageSize=1")),
            "{:?}",
            calls.iter().map(|c| &c.url).collect::<Vec<_>>()
        );
    }

    #[test]
    fn permission_errors_ask_to_reconnect_and_never_leak_tokens() {
        let http =
            FakeHttp::new(|req| Err(HttpError::new(Some(403), "denied AT-SECRET", &req.url)));
        let (ok, msg) = conn(http, Some("RT")).test();
        assert!(
            !ok && msg.contains("Reconnect") && !msg.contains("AT-SECRET"),
            "{msg}"
        );
    }

    fn doc_body() -> Value {
        json!({"title": "Plan", "body": {"content": [
            {"endIndex": 1, "sectionBreak": {}},
            {"startIndex": 1, "endIndex": 17, "paragraph": {"elements": [
                {"textRun": {"content": "Hello {{name}}, "}},
                {"textRun": {"content": "\n"}}]}},
            {"startIndex": 17, "endIndex": 30, "paragraph": {"elements": [
                {"textRun": {"content": "Due {{date}}.\n"}}]}},
            {"startIndex": 30, "endIndex": 38, "paragraph": {"elements": [
                {"textRun": {"content": "Closing\n"}}]}}]}})
    }

    fn doc_http() -> Arc<FakeHttp> {
        FakeHttp::new(|req| {
            if req.method == "GET" {
                Ok(doc_body())
            } else {
                Ok(json!({"id": "n1", "replies": [{"replaceAllText": {"occurrencesChanged": 3}}]}))
            }
        })
    }

    #[test]
    fn list_templates_shapes_document_rows() {
        let http = FakeHttp::ok(json!({"files": [{"id": "d1", "name": "Memo"}]}));
        let rows = conn(http.clone(), Some("RT"))
            .list_templates("memo", 5)
            .unwrap();
        assert_eq!(
            rows[0]["web_link"],
            "https://docs.google.com/document/d/d1/edit"
        );
        assert!(http.calls.lock().unwrap()[0]
            .url
            .contains("google-apps.document"));
    }

    #[test]
    fn describe_doc_flattens_text_and_finds_placeholders() {
        let d = conn(doc_http(), Some("RT")).describe_doc("d1").unwrap();
        assert_eq!(d["title"], "Plan");
        assert_eq!(d["placeholders"], json!(["name", "date"]));
        assert_eq!(d["text"], "Hello {{name}}, \nDue {{date}}.\nClosing\n");
        assert_eq!(d["truncated"], false);
        assert_eq!(d["web_link"], "https://docs.google.com/document/d/d1/edit");
    }

    #[test]
    fn describe_doc_truncates_long_text() {
        let big = "é".repeat(70_000);
        let http = FakeHttp::ok(json!({"title": "T", "body": {"content": [
            {"endIndex": 5, "paragraph": {"elements": [{"textRun": {"content": big}}]}}]}}));
        let d = conn(http, Some("RT")).describe_doc("d1").unwrap();
        assert_eq!(d["truncated"], true);
        assert_eq!(d["text"].as_str().unwrap().chars().count(), 60_000);
    }

    #[test]
    fn copy_doc_posts_title_once_and_returns_link() {
        let http = FakeHttp::ok(json!({"id": "n1"}));
        let made = conn(http.clone(), Some("RT"))
            .copy_doc("t 1", "Brief")
            .unwrap();
        assert_eq!(
            made["web_link"],
            "https://docs.google.com/document/d/n1/edit"
        );
        let c = http.calls.lock().unwrap()[0].clone();
        assert!(c.url.contains("/files/t%201/copy"), "{}", c.url);
        assert_eq!(c.json_body.unwrap(), json!({"name": "Brief"}));
        assert_eq!(c.max_attempts, 1);
    }

    #[test]
    fn fill_doc_sends_one_replace_per_key() {
        let http = doc_http();
        let out = conn(http.clone(), Some("RT"))
            .fill_doc(
                "d1",
                &[("name".into(), "Ana".into()), ("date".into(), "Oct".into())],
            )
            .unwrap();
        assert_eq!(out["replaced"], 3);
        let c = http.calls.lock().unwrap()[0].clone();
        assert!(c.url.ends_with("/documents/d1:batchUpdate"), "{}", c.url);
        let b = c.json_body.unwrap();
        assert_eq!(b["requests"].as_array().unwrap().len(), 2);
        assert_eq!(
            b["requests"][0]["replaceAllText"]["containsText"]["text"],
            "{{name}}"
        );
        assert_eq!(b["requests"][1]["replaceAllText"]["replaceText"], "Oct");
        assert_eq!(
            conn(FakeHttp::ok(json!({})), Some("RT"))
                .fill_doc("d", &[])
                .unwrap()["replaced"],
            0
        );
    }

    #[test]
    fn create_doc_posts_title_then_inserts_text() {
        let http = FakeHttp::ok(json!({"documentId": "n1", "id": "n1"}));
        let made = conn(http.clone(), Some("RT"))
            .create_doc("Notes", "Body text")
            .unwrap();
        assert_eq!(made["id"], "n1");
        assert_eq!(
            made["web_link"],
            "https://docs.google.com/document/d/n1/edit"
        );
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].method, "POST");
        assert_eq!(
            calls[0].json_body.clone().unwrap(),
            json!({"title": "Notes"})
        );
        assert_eq!(calls[0].max_attempts, 1);
        assert!(calls[1].url.ends_with("/documents/n1:batchUpdate"));
        let ins = &calls[1].json_body.clone().unwrap()["requests"][0]["insertText"];
        assert_eq!(ins["text"], "Body text");
        assert_eq!(ins["location"]["index"], 1);
    }

    #[test]
    fn create_doc_without_text_makes_one_call() {
        let http = FakeHttp::ok(json!({"documentId": "n1"}));
        conn(http.clone(), Some("RT"))
            .create_doc("Empty", "")
            .unwrap();
        assert_eq!(http.calls.lock().unwrap().len(), 1);
    }

    fn batch_bodies(http: &FakeHttp) -> Vec<Value> {
        http.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.method == "POST")
            .map(|c| c.json_body.clone().unwrap())
            .collect()
    }

    #[test]
    fn edit_doc_replace_and_append_in_order() {
        let http = doc_http();
        let out = conn(http.clone(), Some("RT"))
            .edit_doc(
                "d1",
                &[
                    json!({"op": "replace", "find": "Closing", "with": "Bye"}),
                    json!({"op": "append", "text": "PS"}),
                ],
            )
            .unwrap();
        assert_eq!(out["applied"], 2);
        let b = batch_bodies(&http);
        assert_eq!(
            b[0]["requests"][0]["replaceAllText"]["containsText"]["text"],
            "Closing"
        );
        assert_eq!(b[0]["requests"][0]["replaceAllText"]["replaceText"], "Bye");
        let ins = &b[1]["requests"][0]["insertText"];
        assert_eq!(ins["text"], "PS");
        assert_eq!(ins["location"]["index"], 37, "one before the final newline");
    }

    #[test]
    fn edit_doc_insert_after_adds_a_paragraph_after_the_match() {
        let http = doc_http();
        let out = conn(http.clone(), Some("RT"))
            .edit_doc(
                "d1",
                &[json!({"op": "insert_after", "find": "Due {{date}}", "text": "Note"})],
            )
            .unwrap();
        assert_eq!(out["applied"], 1);
        let ins = &batch_bodies(&http)[0]["requests"][0]["insertText"];
        assert_eq!(ins["text"], "Note\n");
        assert_eq!(ins["location"]["index"], 30);
    }

    #[test]
    fn edit_doc_insert_after_the_last_paragraph_prepends_a_newline() {
        let http = doc_http();
        conn(http.clone(), Some("RT"))
            .edit_doc(
                "d1",
                &[json!({"op": "insert_after", "find": "Closing", "text": "Z"})],
            )
            .unwrap();
        let ins = &batch_bodies(&http)[0]["requests"][0]["insertText"];
        assert_eq!(ins["text"], "\nZ");
        assert_eq!(ins["location"]["index"], 37);
    }

    #[test]
    fn edit_doc_insert_after_without_a_match_errors_naming_the_op() {
        let http = doc_http();
        let e = conn(http.clone(), Some("RT"))
            .edit_doc(
                "d1",
                &[json!({"op": "insert_after", "find": "nope", "text": "Z"})],
            )
            .unwrap_err()
            .to_string();
        assert!(e.contains("insert_after") && e.contains("nope"), "{e}");
        assert!(batch_bodies(&http).is_empty());
    }

    #[test]
    fn edit_doc_rejects_bad_ops_before_sending_anything() {
        let http = doc_http();
        let c = conn(http.clone(), Some("RT"));
        let e = c
            .edit_doc(
                "d1",
                &[json!({"op": "append", "text": "x"}), json!({"op": "wipe"})],
            )
            .unwrap_err()
            .to_string();
        assert!(e.contains("wipe"), "{e}");
        assert!(c
            .edit_doc("d1", &[json!({"op": "replace", "find": ""})])
            .is_err());
        assert!(http.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn server_errors_never_leak_the_access_token() {
        let http = FakeHttp::new(|req| {
            Err(HttpError::new(
                Some(500),
                "boom Bearer AT-SECRET AT-SECRET",
                &req.url,
            ))
        });
        let c = conn(http.clone(), Some("RT"));
        let errs = [
            c.describe_doc("d").unwrap_err().to_string(),
            c.copy_doc("t", "n").unwrap_err().to_string(),
            c.fill_doc("d", &[("a".into(), "b".into())])
                .unwrap_err()
                .to_string(),
            c.create_doc("t", "x").unwrap_err().to_string(),
            c.edit_doc("d", &[json!({"op": "replace", "find": "a", "with": "b"})])
                .unwrap_err()
                .to_string(),
        ];
        for e in &errs {
            assert!(!e.contains("AT-SECRET") && !e.is_empty(), "{e}");
        }
        assert!(http.calls.lock().unwrap().iter().all(|c| !c
            .json_body
            .clone()
            .unwrap_or_default()
            .to_string()
            .contains("AT-SECRET")));
    }

    #[test]
    fn permission_errors_on_operations_ask_to_reconnect() {
        let http =
            FakeHttp::new(|req| Err(HttpError::new(Some(403), "denied AT-SECRET", &req.url)));
        let e = conn(http, Some("RT"))
            .describe_doc("d")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("Reconnect Google Docs to grant Docs and Drive access")
                && !e.contains("AT-SECRET"),
            "{e}"
        );
    }

    #[test]
    fn unconnected_account_says_to_connect() {
        let (ok, msg) = conn(FakeHttp::ok(json!({})), None).test();
        assert!(!ok && msg.contains("Connect with Google"), "{msg}");
    }
}
