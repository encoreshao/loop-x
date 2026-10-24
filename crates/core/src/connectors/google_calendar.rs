//! Google Calendar connector (reads events; the chat can create one after
//! approval). The stored secret is a Google refresh token obtained through the
//! dashboard's Connect with Google flow (`auth = OauthGoogle`); the OAuth
//! client is the one saved on the Inbox Triage setup page. No message built
//! here ever contains the refresh token or an access token.

use super::http::{quote, urlencode, REDACTED, TEST_TIMEOUT_SECONDS};
use super::*;
use serde_json::json;

pub const API_URL: &str = "https://www.googleapis.com/calendar/v3";
pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

const FIELDS: [Field; 1] = [Field::new("calendar_id", "Calendar ID")
    .default("primary")
    .placeholder("primary")
    .help(
        "Use primary for your main calendar, or a calendar's ID from its Google Calendar settings",
    )];

pub const INFO: TypeInfo = TypeInfo {
    name: "google_calendar",
    label: "Google Calendar",
    icon: "calendar_month",
    capabilities: &[CALENDAR],
    fields: &FIELDS,
    secret_label: None,
    external: false,
    brand: "googlecalendar",
    description: "Read your calendar events and, with your approval in chat, create new ones.",
    docs_url: "https://support.google.com/calendar/answer/37082",
    category: "google",
    presets: &[],
    auth: Auth::OauthGoogle,
};

/// Why a refresh failed.
#[derive(Debug, Clone, PartialEq)]
pub enum RefreshError {
    /// The grant is gone: the user must sign in again (`ReauthRequired`).
    Reauth,
    Http(HttpError),
    /// Anything else; only the class name is ever shown.
    Other(String),
}

/// The OAuth client (`{client_id, client_secret}`) saved on the Inbox Triage page.
pub type ClientFn = Arc<dyn Fn() -> Value + Send + Sync>;
/// `(refresh_token, client)` -> access token.
pub type RefreshFn = Arc<dyn Fn(&str, &Value) -> Result<String, RefreshError> + Send + Sync>;

/// The Google client from `mail_oauth.json` (`inbox_config.load_oauth()["google"]`, read from `<home>/mail_oauth.json`).
pub fn default_client() -> Value {
    std::fs::read_to_string(crate::config::home_dir().join("mail_oauth.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("google").filter(|g| g.is_object()).cloned())
        .unwrap_or_else(|| json!({}))
}

/// `mail_auth.google_refresh`: exchange the refresh token for an access token.
pub fn google_refresh(
    http: &dyn HttpClient,
    refresh_token: &str,
    client: &Value,
) -> Result<String, RefreshError> {
    let get = |k: &str| {
        client
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let mut req = HttpRequest::new("POST", GOOGLE_TOKEN_URL);
    req.form = Some(vec![
        ("refresh_token".into(), refresh_token.to_string()),
        ("client_id".into(), get("client_id")),
        ("client_secret".into(), get("client_secret")),
        ("grant_type".into(), "refresh_token".into()),
    ]);
    match http.request_json(&req) {
        Ok(tokens) => tokens
            .get("access_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| RefreshError::Other("KeyError".into())),
        Err(e) if matches!(e.status, Some(400) | Some(401)) => Err(RefreshError::Reauth),
        Err(e) => Err(RefreshError::Http(e)),
    }
}

pub struct GoogleCalendarConnector {
    account: Account,
    secret: Option<String>,
    http: Arc<dyn HttpClient>,
    client_fn: ClientFn,
    refresh_fn: RefreshFn,
}

impl GoogleCalendarConnector {
    pub fn new(account: Account, secret: Option<String>, http: Arc<dyn HttpClient>) -> Self {
        let refresh_http = http.clone();
        Self {
            account,
            secret,
            http,
            client_fn: Arc::new(default_client),
            refresh_fn: Arc::new(move |rt, client| {
                google_refresh(refresh_http.as_ref(), rt, client)
            }),
        }
    }

    pub fn with_client_fn(mut self, f: ClientFn) -> Self {
        self.client_fn = f;
        self
    }

    pub fn with_refresh_fn(mut self, f: RefreshFn) -> Self {
        self.refresh_fn = f;
        self
    }

    fn calendar_path(&self) -> String {
        let id = self.account.setting("calendar_id").trim().to_string();
        let id = if id.is_empty() {
            "primary".to_string()
        } else {
            id
        };
        format!("/calendars/{}", quote(&id))
    }

    /// Replace each token, then any >=8-char prefix or suffix fragment of one
    /// (what is left when a cut lands inside a token).
    fn scrub(&self, text: &str, access_token: Option<&str>) -> String {
        let mut tokens: Vec<String> = [self.secret.as_deref(), access_token]
            .into_iter()
            .flatten()
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect();
        tokens.sort_by_key(|t| std::cmp::Reverse(t.chars().count()));
        let mut out = text.to_string();
        for token in tokens {
            out = out.replace(&token, REDACTED);
            let chars: Vec<char> = token.chars().collect();
            for n in (8..chars.len()).rev() {
                let prefix: String = chars[..n].iter().collect();
                let suffix: String = chars[chars.len() - n..].iter().collect();
                out = out.replace(&prefix, REDACTED).replace(&suffix, REDACTED);
            }
        }
        out
    }

    /// `describe_http_error` with the tokens scrubbed from the body BEFORE its
    /// 200-char truncation, so a token straddling the cut can't survive.
    fn describe(&self, err: &ConnectorError, access_token: Option<&str>) -> String {
        let err = match err {
            ConnectorError::Http(e) if !e.auth_expired => ConnectorError::Http(HttpError::new(
                e.status,
                self.scrub(&e.body, access_token),
                &e.url,
            )),
            other => other.clone(),
        };
        self.scrub(&describe_http_error(&err), access_token)
    }

    /// `(token, None)` or `(None, failure message)`.
    fn access_token(&self) -> Result<String, String> {
        let Some(secret) = self.secret.as_deref().filter(|s| !s.is_empty()) else {
            return Err("Not connected - click Connect with Google".to_string());
        };
        let client = (self.client_fn)();
        let has = |k: &str| {
            client
                .get(k)
                .and_then(Value::as_str)
                .is_some_and(|v| !v.is_empty())
        };
        if !has("client_id") || !has("client_secret") {
            return Err(
                "Add a Google OAuth client on the Inbox Triage setup page first".to_string(),
            );
        }
        match (self.refresh_fn)(secret, &client) {
            Err(RefreshError::Reauth) => {
                Err("Google sign-in expired or was revoked - click Reconnect".to_string())
            }
            Err(RefreshError::Http(e)) => Err(self.describe(&ConnectorError::Http(e), None)),
            Err(RefreshError::Other(class)) => Err(self.describe(
                &ConnectorError::Other {
                    class,
                    detail: String::new(),
                },
                None,
            )),
            Ok(t) if t.is_empty() => {
                Err("Google returned no access token - click Reconnect".to_string())
            }
            Ok(t) => Ok(t),
        }
    }

    fn get(
        &self,
        path: &str,
        token: &str,
        timeout: u64,
        attempts: u32,
    ) -> Result<Value, HttpError> {
        let req = HttpRequest::new("GET", &format!("{API_URL}{path}"))
            .token(Some(token))
            .timeout_secs(timeout)
            .attempts(attempts);
        self.http.request_json(&req)
    }
}

fn attendee(raw: &Value) -> Value {
    let s = |k: &str| {
        if crate::loops_config::py_truthy(raw.get(k).unwrap_or(&Value::Null)) {
            crate::pydumps::py_str(&raw[k])
        } else {
            String::new()
        }
    };
    json!({"name": s("displayName"), "email": s("email"), "response": s("responseStatus"),
           "self": crate::loops_config::py_truthy(raw.get("self").unwrap_or(&Value::Null))})
}

/// The Meet link, else the first video entry point (Zoom, Teams...).
fn join_url(item: &Value) -> String {
    if let Some(l) = item
        .get("hangoutLink")
        .filter(|v| crate::loops_config::py_truthy(v))
    {
        return crate::pydumps::py_str(l);
    }
    item.pointer("/conferenceData/entryPoints")
        .and_then(Value::as_array)
        .and_then(|points| {
            points.iter().find(|p| {
                p.get("entryPointType").and_then(Value::as_str) == Some("video")
                    && p.get("uri").is_some_and(crate::loops_config::py_truthy)
            })
        })
        .map(|p| crate::pydumps::py_str(&p["uri"]))
        .unwrap_or_default()
}

/// `str(value or "")` for a JSON field.
fn text_of(v: Option<&Value>) -> String {
    match v {
        Some(x) if crate::loops_config::py_truthy(x) => crate::pydumps::py_str(x),
        _ => String::new(),
    }
}

impl Connector for GoogleCalendarConnector {
    fn info(&self) -> &'static TypeInfo {
        &INFO
    }

    fn account(&self) -> &Account {
        &self.account
    }

    fn test(&self) -> (bool, String) {
        let token = match self.access_token() {
            Ok(t) => t,
            Err(m) => return (false, m),
        };
        match self.get(&self.calendar_path(), &token, TEST_TIMEOUT_SECONDS, 1) {
            Err(e) => (false, self.describe(&ConnectorError::Http(e), Some(&token))),
            Ok(cal) => {
                let summary = cal
                    .get("summary")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("?");
                (
                    true,
                    format!("Connected: {}", self.scrub(summary, Some(&token))),
                )
            }
        }
    }

    /// Events between two RFC 3339 instants, expanded and in start order.
    fn list_events(
        &self,
        time_min: &str,
        time_max: &str,
        max_results: u32,
    ) -> ConnResult<Vec<Value>> {
        let token = self.access_token().map_err(ConnectorError::msg)?;
        let query = urlencode(&[
            ("timeMin", time_min.to_string()),
            ("timeMax", time_max.to_string()),
            ("maxResults", max_results.to_string()),
            ("singleEvents", "true".to_string()),
            ("orderBy", "startTime".to_string()),
        ]);
        let data = self
            .get(
                &format!("{}/events?{query}", self.calendar_path()),
                &token,
                30,
                3,
            )
            .map_err(|e| {
                ConnectorError::msg(self.describe(&ConnectorError::Http(e), Some(&token)))
            })?;
        let mut events = Vec::new();
        for item in data
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if !item.is_object() {
                continue;
            }
            let start = item
                .get("start")
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or_else(|| json!({}));
            let end = item
                .get("end")
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or_else(|| json!({}));
            let attendees = item.get("attendees").and_then(Value::as_array);
            let people: Vec<Value> = attendees
                .into_iter()
                .flatten()
                .filter(|a| a.is_object())
                .map(attendee)
                .collect();
            let self_response = people
                .iter()
                .find(|a| a["self"] == true)
                .map_or(Value::Null, |a| a["response"].clone());
            let organizer = item
                .get("organizer")
                .filter(|o| o.is_object())
                .map(|o| text_of(o.get("email")))
                .unwrap_or_default();
            let start_text =
                if crate::loops_config::py_truthy(start.get("dateTime").unwrap_or(&Value::Null)) {
                    text_of(start.get("dateTime"))
                } else {
                    text_of(start.get("date"))
                };
            let end_text =
                if crate::loops_config::py_truthy(end.get("dateTime").unwrap_or(&Value::Null)) {
                    text_of(end.get("dateTime"))
                } else {
                    text_of(end.get("date"))
                };
            events.push(json!({
                "summary": text_of(item.get("summary")),
                "start": start_text,
                "end": end_text,
                "html_link": text_of(item.get("htmlLink")),
                "attendees_count": attendees.map_or(0, Vec::len),
                "id": text_of(item.get("id")),
                "recurring_event_id": text_of(item.get("recurringEventId")),
                "description": text_of(item.get("description")),
                "location": text_of(item.get("location")),
                "join_url": join_url(item),
                "organizer": organizer,
                "attendees": people,
                "self_response": self_response,
            }));
        }
        Ok(events)
    }

    /// Creates one event (`{summary, start, end, description?, location?,
    /// attendees?}`; start/end are RFC 3339 with an offset). Invitations go out
    /// only when there are attendees.
    fn create_event(&self, event: &Value) -> ConnResult<Value> {
        let token = self.access_token().map_err(ConnectorError::msg)?;
        let field = |k: &str| {
            event.get(k).cloned().ok_or_else(|| ConnectorError::Other {
                class: "KeyError".into(),
                detail: format!("'{k}'"),
            })
        };
        let mut body = json!({"summary": field("summary")?, "start": {"dateTime": field("start")?},
                              "end": {"dateTime": field("end")?}});
        for key in ["description", "location"] {
            if event.get(key).is_some_and(crate::loops_config::py_truthy) {
                body[key] = event[key].clone();
            }
        }
        let attendees: Vec<Value> = event
            .get("attendees")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !attendees.is_empty() {
            body["attendees"] =
                Value::Array(attendees.iter().map(|a| json!({"email": a})).collect());
        }
        let notify = if attendees.is_empty() { "none" } else { "all" };
        let req = HttpRequest::new(
            "POST",
            &format!(
                "{API_URL}{}/events?sendUpdates={notify}",
                self.calendar_path()
            ),
        )
        .token(Some(&token))
        .json(Some(body));
        match self.http.request_json(&req) {
            Ok(made) => Ok(
                json!({"id": text_of(made.get("id")), "html_link": text_of(made.get("htmlLink"))}),
            ),
            Err(e) if matches!(e.status, Some(401) | Some(403)) => Err(ConnectorError::msg(
                "Reconnect Google Calendar to grant permission to create events",
            )),
            Err(e) => Err(ConnectorError::msg(
                self.describe(&ConnectorError::Http(e), Some(&token)),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::FakeHttp;
    use super::*;

    fn client() -> Value {
        json!({"client_id": "c", "client_secret": "s"})
    }

    fn gcal(
        secret: Option<&str>,
        http: Arc<dyn HttpClient>,
        refresh: RefreshFn,
    ) -> GoogleCalendarConnector {
        GoogleCalendarConnector::new(
            Account::new("g", "google_calendar").with_setting("calendar_id", "primary"),
            secret.map(str::to_string),
            http,
        )
        .with_client_fn(Arc::new(client))
        .with_refresh_fn(refresh)
    }

    fn at(token: &'static str) -> RefreshFn {
        Arc::new(move |_, _| Ok(token.to_string()))
    }

    #[test]
    fn type_metadata() {
        assert_eq!(
            (INFO.label, INFO.brand, INFO.category),
            ("Google Calendar", "googlecalendar", "google")
        );
        assert_eq!(
            (INFO.capabilities, INFO.auth, INFO.secret_label),
            (&[CALENDAR][..], Auth::OauthGoogle, None)
        );
        assert_eq!(INFO.fields.len(), 1);
        assert_eq!(
            (INFO.fields[0].key, INFO.fields[0].default),
            ("calendar_id", "primary")
        );
        assert_eq!(
            INFO.docs_url,
            "https://support.google.com/calendar/answer/37082"
        );
        assert_eq!(github::INFO.auth, Auth::Secret);
    }

    #[test]
    fn test_success_uses_the_probe_settings() {
        let http = FakeHttp::ok(json!({"summary": "Work"}));
        let c = gcal(Some("RT"), http.clone(), at("AT"));
        assert_eq!(c.test(), (true, "Connected: Work".to_string()));
        let call = http.calls.lock().unwrap()[0].clone();
        assert_eq!(
            call.url,
            "https://www.googleapis.com/calendar/v3/calendars/primary"
        );
        assert_eq!(call.token.as_deref(), Some("AT"));
        assert_eq!((call.timeout.as_secs(), call.max_attempts), (10, 1));
    }

    #[test]
    fn test_failure_modes_are_plain() {
        let http = FakeHttp::ok(json!({}));
        let (ok, msg) = gcal(None, http.clone(), at("AT")).test();
        assert!(!ok && msg.contains("Connect with Google"));
        let c = gcal(Some("RT"), http.clone(), at("AT")).with_client_fn(Arc::new(|| json!({})));
        let (ok, msg) = c.test();
        assert!(!ok && msg.contains("Google OAuth client"));
        let reauth: RefreshFn = Arc::new(|_, _| Err(RefreshError::Reauth));
        let (ok, msg) = gcal(Some("RT-SECRET"), http.clone(), reauth).test();
        assert!(!ok && msg.contains("Reconnect") && !msg.contains("RT-SECRET"));
        let (ok, msg) = gcal(Some("RT"), http, Arc::new(|_, _| Ok(String::new()))).test();
        assert!(!ok && msg.contains("no access token"));
    }

    #[test]
    fn errors_never_contain_the_refresh_or_access_token() {
        let bad: RefreshFn = Arc::new(|_, _| {
            Err(RefreshError::Http(HttpError::new(
                Some(400),
                r#"{"error":"invalid_grant","rt":"RT-SECRET"}"#,
                "https://oauth2.googleapis.com/token",
            )))
        });
        let (ok, msg) = gcal(Some("RT-SECRET"), FakeHttp::ok(json!({})), bad).test();
        assert!(!ok && !msg.contains("RT-SECRET"), "{msg}");
        let http = FakeHttp::new(|_| {
            Err(HttpError::new(
                Some(403),
                "denied for AT-SECRET",
                "https://x",
            ))
        });
        let (ok, msg) = gcal(Some("RT"), http, at("AT-SECRET")).test();
        assert!(
            !ok && !msg.contains("AT-SECRET") && msg.contains("403"),
            "{msg}"
        );
    }

    #[test]
    fn a_token_straddling_the_200_char_cut_is_scrubbed() {
        let rt = "1//0gABCDEFGHIJKLMNOPQRSTUVWXYZ-refresh-token-value";
        let body = format!("{}{rt}", "x".repeat(190));
        let bad: RefreshFn = Arc::new(move |_, _| {
            Err(RefreshError::Http(HttpError::new(
                Some(400),
                body.clone(),
                "https://oauth2.googleapis.com/token",
            )))
        });
        let (ok, msg) = gcal(Some(rt), FakeHttp::ok(json!({})), bad).test();
        assert!(!ok);
        let chars: Vec<char> = rt.chars().collect();
        for i in 0..chars.len() - 7 {
            let frag: String = chars[i..i + 8].iter().collect();
            assert!(!msg.contains(&frag), "{frag} in {msg}");
        }
        let http = FakeHttp::new(|_| {
            Err(HttpError::new(
                Some(403),
                format!("{}AT-LONG-ACCESS-TOKEN-123", "y".repeat(195)),
                "https://x",
            ))
        });
        let (ok, msg) = gcal(Some("RT"), http, at("AT-LONG-ACCESS-TOKEN-123")).test();
        assert!(
            !ok && !msg.contains("AT-LONG-") && !msg.contains("TOKEN-123"),
            "{msg}"
        );
    }

    #[test]
    fn calendar_id_is_url_quoted() {
        let http = FakeHttp::ok(json!({"summary": "x"}));
        let c = GoogleCalendarConnector::new(
            Account::new("g", "google_calendar")
                .with_setting("calendar_id", "a@group.calendar.google.com/../x"),
            Some("RT".into()),
            http.clone(),
        )
        .with_client_fn(Arc::new(client))
        .with_refresh_fn(at("AT"));
        c.test();
        assert!(http.calls.lock().unwrap()[0]
            .url
            .ends_with("/calendars/a%40group.calendar.google.com%2F..%2Fx"));
    }

    #[test]
    fn list_events_shapes_items_and_builds_the_query() {
        let http = FakeHttp::ok(json!({"items": [
            {"summary": "Standup", "start": {"dateTime": "2026-10-06T09:00:00Z"}, "end": {"dateTime": "2026-10-06T09:15:00Z"},
             "htmlLink": "https://calendar.google.com/e1", "attendees": [{"email": "a"}, {"email": "b"}]},
            {"start": {"date": "2026-10-07"}, "end": {"date": "2026-10-08"}},
        ]}));
        let events = gcal(Some("RT"), http.clone(), at("AT"))
            .list_events("2026-10-06T00:00:00Z", "2026-10-08T00:00:00Z", 5)
            .unwrap();
        let pick = |e: &Value| {
            ["summary", "start", "end", "html_link", "attendees_count"]
                .iter()
                .map(|k| (k.to_string(), e[*k].clone()))
                .collect::<Map<_, _>>()
        };
        assert_eq!(
            pick(&events[0]),
            json!({"summary": "Standup", "start": "2026-10-06T09:00:00Z", "end": "2026-10-06T09:15:00Z",
                   "html_link": "https://calendar.google.com/e1", "attendees_count": 2}).as_object().unwrap().clone()
        );
        assert_eq!(
            pick(&events[1]),
            json!({"summary": "", "start": "2026-10-07", "end": "2026-10-08", "html_link": "", "attendees_count": 0})
                .as_object().unwrap().clone()
        );
        let call = http.calls.lock().unwrap()[0].clone();
        assert_eq!(
            call.url,
            "https://www.googleapis.com/calendar/v3/calendars/primary/events?timeMin=2026-10-06T00%3A00%3A00Z&timeMax=2026-10-08T00%3A00%3A00Z&maxResults=5&singleEvents=true&orderBy=startTime"
        );
        assert_eq!(call.token.as_deref(), Some("AT"));
    }

    #[test]
    fn list_events_returns_prep_details() {
        let http = FakeHttp::ok(json!({"items": [
            {"id": "ev1", "recurringEventId": "series1", "summary": "Sync",
             "start": {"dateTime": "2026-10-06T09:00:00Z"}, "end": {"dateTime": "2026-10-06T09:30:00Z"},
             "description": "Agenda https://gitlab.example.com/g/p/-/issues/3", "location": "Room 1",
             "conferenceData": {"entryPoints": [{"entryPointType": "phone", "uri": "tel:+1"},
                                                {"entryPointType": "video", "uri": "https://zoom.us/j/1"}]},
             "organizer": {"email": "boss@example.com"},
             "attendees": [{"email": "me@example.com", "self": true, "responseStatus": "accepted"},
                           {"email": "ann@example.com", "displayName": "Ann", "responseStatus": "tentative"}]},
            {"id": "ev2", "summary": "Meet", "hangoutLink": "https://meet.google.com/abc",
             "start": {"dateTime": "2026-10-06T10:00:00Z"}, "end": {"dateTime": "2026-10-06T10:30:00Z"},
             "conferenceData": {"entryPoints": [{"entryPointType": "video", "uri": "https://zoom.us/j/2"}]}},
        ]}));
        let events = gcal(Some("RT"), http, at("AT"))
            .list_events("a", "b", 50)
            .unwrap();
        let (first, second) = (&events[0], &events[1]);
        assert_eq!(
            (first["id"].as_str(), first["recurring_event_id"].as_str()),
            (Some("ev1"), Some("series1"))
        );
        assert_eq!(first["location"], "Room 1");
        assert_eq!(
            (first["join_url"].as_str(), first["organizer"].as_str()),
            (Some("https://zoom.us/j/1"), Some("boss@example.com"))
        );
        assert_eq!(
            first["attendees"],
            json!([{"name": "", "email": "me@example.com", "response": "accepted", "self": true},
                   {"name": "Ann", "email": "ann@example.com", "response": "tentative", "self": false}])
        );
        assert_eq!(first["self_response"], "accepted");
        assert_eq!(second["join_url"], "https://meet.google.com/abc");
        assert_eq!(
            (
                second["recurring_event_id"].as_str(),
                second["self_response"].clone()
            ),
            (Some(""), Value::Null)
        );
        assert_eq!(second["attendees"], json!([]));
    }

    #[test]
    fn create_event_posts_to_the_events_endpoint() {
        let http = FakeHttp::ok(json!({"id": "ev1", "htmlLink": "https://cal.example/ev1"}));
        let c = gcal(Some("RT"), http.clone(), at("AT"));
        let made = c
            .create_event(
                &json!({"summary": "Sync", "start": "2026-10-12T15:00:00+08:00",
                                   "end": "2026-10-12T15:30:00+08:00", "attendees": ["a@x.com"],
                                   "description": "d", "location": "Room"}),
            )
            .unwrap();
        assert_eq!(
            made,
            json!({"id": "ev1", "html_link": "https://cal.example/ev1"})
        );
        let call = http.calls.lock().unwrap()[0].clone();
        assert_eq!(
            (call.method.as_str(), call.token.as_deref()),
            ("POST", Some("AT"))
        );
        assert_eq!(
            call.url,
            "https://www.googleapis.com/calendar/v3/calendars/primary/events?sendUpdates=all"
        );
        assert_eq!(
            call.json_body.unwrap(),
            json!({"summary": "Sync", "start": {"dateTime": "2026-10-12T15:00:00+08:00"},
                   "end": {"dateTime": "2026-10-12T15:30:00+08:00"}, "description": "d",
                   "location": "Room", "attendees": [{"email": "a@x.com"}]})
        );
    }

    #[test]
    fn create_event_without_attendees_sends_no_invites() {
        let http = FakeHttp::ok(json!({"id": "ev1"}));
        gcal(Some("RT"), http.clone(), at("AT"))
            .create_event(&json!({"summary": "Focus", "start": "2026-10-12T15:00:00Z", "end": "2026-10-12T16:00:00Z"}))
            .unwrap();
        let call = http.calls.lock().unwrap()[0].clone();
        assert!(call.url.ends_with("/events?sendUpdates=none"));
        assert!(call.json_body.unwrap().get("attendees").is_none());
    }

    #[test]
    fn create_event_403_asks_to_reconnect() {
        let http = FakeHttp::new(|_| {
            Err(HttpError::new(
                Some(403),
                r#"{"error":"insufficientPermissions"}"#,
                "u",
            ))
        });
        let e = gcal(Some("RT"), http, at("AT"))
            .create_event(&json!({"summary": "x", "start": "2026-10-12T15:00:00Z", "end": "2026-10-12T16:00:00Z"}))
            .unwrap_err();
        assert!(e.to_string().contains("Reconnect"));
    }

    #[test]
    fn google_refresh_posts_a_form_and_maps_rejections_to_reauth() {
        let http = FakeHttp::ok(json!({"access_token": "AT"}));
        assert_eq!(
            google_refresh(http.as_ref(), "RT", &client()).unwrap(),
            "AT"
        );
        let call = http.calls.lock().unwrap()[0].clone();
        assert_eq!(call.url, "https://oauth2.googleapis.com/token");
        assert!(call
            .form
            .unwrap()
            .contains(&("grant_type".into(), "refresh_token".into())));
        let rejected = FakeHttp::new(|r| Err(HttpError::new(Some(400), "{}", &r.url)));
        assert_eq!(
            google_refresh(rejected.as_ref(), "RT", &client()),
            Err(RefreshError::Reauth)
        );
        let down = FakeHttp::new(|r| Err(HttpError::new(Some(503), "", &r.url)));
        assert!(matches!(
            google_refresh(down.as_ref(), "RT", &client()),
            Err(RefreshError::Http(_))
        ));
    }
}
