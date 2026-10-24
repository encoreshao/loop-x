//! The routes native clients (the Mac app) and the web UI rely on:
//! `/api/version`, `/api/snapshot`, `/api/events` (SSE), `/api/approvals`.
//! Everything else is a plain 404.
//!
//! Auth: the server only listens on loopback and rejects any `Host` that is not
//! a loopback name (DNS rebinding). Reads need nothing more. Writes need either
//! the token from `loopx.lock` in an `X-LoopX-Token` header (native clients) or a
//! same-origin browser request (`Origin` equal to our own origin, which a
//! cross-site page cannot forge).

use crate::snapshot::{build_snapshot, Paths};
use axum::body::Body;
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{SecondsFormat, Utc};
use futures_util::stream::{self, Stream, StreamExt};
use loopx_core::approvals::{self, ApprovalError};
use serde_json::{json, Value};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

pub struct AppState {
    pub root: PathBuf,
    pub paths: Paths,
    pub token: String,
    pub version: String,
    pub started_at: String,
    /// Server-rendered pages (see [`crate::ui`]); anything they do not claim is proxied.
    pub ui: Arc<crate::ui::UiState>,
    /// Serialised snapshots; one per change.
    pub tx: broadcast::Sender<Arc<String>>,
}

pub type Shared = Arc<AppState>;

impl AppState {
    pub fn new(root: PathBuf, token: String) -> Shared {
        let (tx, _) = broadcast::channel(16);
        let root_for_ui = root.clone();
        Arc::new(Self {
            paths: Paths::for_root(&root),
            root,
            token,
            version: loopx_core::VERSION.to_string(),
            started_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            ui: Arc::new(crate::ui::UiState::from_env(&root_for_ui)),
            tx,
        })
    }

    pub fn snapshot_text(&self) -> String {
        build_snapshot(&self.paths, Utc::now()).to_string()
    }
}

pub fn router(state: Shared) -> Router {
    let mut app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/version", get(version))
        .route("/api/snapshot", get(snapshot))
        .route("/api/events", get(events))
        .route("/static/loopx-notify.js", get(notify_script))
        .route("/api/approvals", get(list_approvals))
        .route("/api/approvals/:id", post(decide));
    match loopx_core::oauth::OauthContext::from_env() {
        Ok(ctx) => app = app.merge(crate::oauth_routes::routes(std::sync::Arc::new(ctx))),
        Err(e) => eprintln!("oauth routes disabled: {e}"),
    }
    app.fallback(fallback).with_state(state)
}

pub(crate) fn json_error(code: StatusCode, message: &str) -> Response {
    (code, Json(json!({"error": message}))).into_response()
}

/// The `Host` header's name part, lowercased, without a port.
fn host_name(headers: &HeaderMap) -> Option<String> {
    let host = headers
        .get(header::HOST)?
        .to_str()
        .ok()?
        .to_ascii_lowercase();
    Some(match host.strip_prefix('[') {
        Some(rest) => format!("[{}", rest.split(']').next()?) + "]",
        None => host.split(':').next()?.to_string(),
    })
}

pub(crate) fn host_allowed(headers: &HeaderMap) -> bool {
    matches!(
        host_name(headers).as_deref(),
        Some("127.0.0.1") | Some("localhost") | Some("[::1]")
    )
}

pub(crate) fn authorized_to_write(headers: &HeaderMap, state: &AppState) -> bool {
    if headers
        .get("x-loopx-token")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| !state.token.is_empty() && constant_time_eq(t, &state.token))
    {
        return true;
    }
    let (Some(origin), Some(host)) = (
        headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()),
        headers.get(header::HOST).and_then(|v| v.to_str().ok()),
    ) else {
        return false;
    };
    origin.eq_ignore_ascii_case(&format!("http://{host}"))
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

async fn version(State(s): State<Shared>, headers: HeaderMap) -> Response {
    if !host_allowed(&headers) {
        return json_error(StatusCode::FORBIDDEN, "forbidden host");
    }
    Json(json!({
        "name": "loopx",
        "version": s.version,
        "pid": std::process::id(),
        "started_at": s.started_at,
        "root": s.root.display().to_string(),
    }))
    .into_response()
}

async fn snapshot(State(s): State<Shared>, headers: HeaderMap) -> Response {
    if !host_allowed(&headers) {
        return json_error(StatusCode::FORBIDDEN, "forbidden host");
    }
    (
        [(header::CONTENT_TYPE, "application/json")],
        s.snapshot_text(),
    )
        .into_response()
}

/// SSE: one `snapshot` event on connect, then one per change. A snapshot is the
/// whole state, so a client that reconnects (or wakes from sleep) is fully
/// caught up by the first event it receives; there is nothing to replay.
async fn events(State(s): State<Shared>, headers: HeaderMap) -> Response {
    if !host_allowed(&headers) {
        return json_error(StatusCode::FORBIDDEN, "forbidden host");
    }
    let rx = s.tx.subscribe();
    let first = s.snapshot_text();
    let stream: std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = Box::pin(
        stream::once(async move { Ok(Event::default().event("snapshot").data(first)) }).chain(
            BroadcastStream::new(rx).filter_map(|item| async move {
                // A lagged receiver just skips ahead: the next snapshot is complete.
                item.ok()
                    .map(|text| Ok(Event::default().event("snapshot").data(text.as_str())))
            }),
        ),
    );
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("ping"),
        )
        .into_response()
}

async fn list_approvals(State(s): State<Shared>, headers: HeaderMap) -> Response {
    if !host_allowed(&headers) {
        return json_error(StatusCode::FORBIDDEN, "forbidden host");
    }
    let now = Utc::now();
    let dir = &s.paths.approvals_dir;
    let pending = approvals::list(dir, Some("pending"), now);
    let recent: Vec<Value> = approvals::list(dir, None, now)
        .into_iter()
        .filter(|a| a["status"] != "pending")
        .take(20)
        .collect();
    let snap = build_snapshot(&s.paths, now);
    let escalations: Vec<Value> = snap["recent_events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["type"] == "issue.escalated")
        .cloned()
        .collect();
    Json(json!({"pending": pending, "recent": recent, "escalations": escalations})).into_response()
}

async fn decide(
    State(s): State<Shared>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    if !host_allowed(&headers) {
        return json_error(StatusCode::FORBIDDEN, "forbidden host");
    }
    if !authorized_to_write(&headers, &s) {
        return json_error(StatusCode::FORBIDDEN, "missing or wrong token");
    }
    let body = body.map(|b| b.0).unwrap_or(Value::Null);
    let decision = body["decision"].as_str().unwrap_or("");
    let by = body["by"].as_str().unwrap_or("api");
    match approvals::decide(&s.paths.approvals_dir, &id, decision, by, Utc::now()) {
        Ok(a) => Json(a).into_response(),
        Err(e @ ApprovalError::Unknown(_)) => json_error(StatusCode::NOT_FOUND, &e.to_string()),
        Err(e @ ApprovalError::AlreadyDecided(..)) => {
            json_error(StatusCode::CONFLICT, &e.to_string())
        }
        Err(e @ ApprovalError::BadDecision(_)) => {
            json_error(StatusCode::BAD_REQUEST, &e.to_string())
        }
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

const NOTIFY_JS: &str = include_str!("notify.js");
const NOTIFY_TAG: &str = "<script src=\"/static/loopx-notify.js\" defer></script>";

async fn notify_script() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        NOTIFY_JS,
    )
        .into_response()
}

/// Add the notification script to a rendered page. It is done here, on the
/// served response, so the page registry (and the golden UI comparison against
/// the Python dashboard) stays byte-identical.
async fn inject_notify(resp: Response) -> Response {
    let is_html = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"));
    if !is_html || resp.status() != StatusCode::OK {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 8 * 1024 * 1024).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let out = match std::str::from_utf8(&bytes) {
        Ok(html) if html.contains("</body>") => html
            .replacen("</body>", &format!("{NOTIFY_TAG}</body>"), 1)
            .into_bytes(),
        _ => bytes.to_vec(),
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(out))
}

/// Everything not handled above: a server-rendered page, or a plain 404.
pub(crate) async fn fallback(State(s): State<Shared>, req: Request<Body>) -> Response {
    if let Ok(resp) = crate::ui::glue::try_serve(&s.ui, req).await {
        return inject_notify(resp).await;
    }
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "not found",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use chrono::Utc;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn state(dir: &std::path::Path) -> Shared {
        AppState::new(dir.to_path_buf(), "secret-token".into())
    }

    async fn call(app: Router, req: Request<Body>) -> (StatusCode, Value, HeaderMap) {
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v = serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into()));
        (status, v, headers)
    }

    fn get_req(path: &str) -> Request<Body> {
        Request::get(path)
            .header("host", "127.0.0.1:8420")
            .body(Body::empty())
            .unwrap()
    }

    fn pending(s: &Shared) -> String {
        approvals::create(
            &s.paths.approvals_dir,
            &approvals::NewApproval {
                kind: "open_mr".into(),
                title: "Fix #1".into(),
                timeout_seconds: 3600,
                ..Default::default()
            },
            Utc::now(),
        )
        .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn decide_req(id: &str, body: Value, extra: &[(&str, &str)]) -> Request<Body> {
        let mut b = Request::post(format!("/api/approvals/{id}"))
            .header("host", "127.0.0.1:8420")
            .header("content-type", "application/json");
        for (k, v) in extra {
            b = b.header(*k, *v);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn notify_script_is_served() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state(dir.path()));
        let resp = app
            .oneshot(get_req("/static/loopx-notify.js"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let b = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&b).contains("/api/events"));
    }

    #[tokio::test]
    async fn inject_notify_adds_the_script_to_html_only() {
        let html = Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from("<html><body>hi</body></html>"))
            .unwrap();
        let out = inject_notify(html).await;
        let b = out.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&b).contains("loopx-notify.js\" defer></script></body>"));
        let json = Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("</body>"))
            .unwrap();
        let b = inject_notify(json)
            .await
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(&b[..], b"</body>");
    }

    #[tokio::test]
    async fn version_reports_name_version_and_root() {
        let d = tempfile::tempdir().unwrap();
        let (code, v, _) = call(router(state(d.path())), get_req("/api/version")).await;
        assert_eq!(code, 200);
        assert_eq!(v["name"], "loopx");
        assert_eq!(v["version"], loopx_core::VERSION);
        assert_eq!(v["pid"], std::process::id());
    }

    #[tokio::test]
    async fn foreign_hosts_are_refused_to_stop_dns_rebinding() {
        let d = tempfile::tempdir().unwrap();
        for host in [
            "evil.example.com",
            "evil.example.com:8420",
            "127.0.0.1.evil.com",
        ] {
            let req = Request::get("/api/snapshot")
                .header("host", host)
                .body(Body::empty())
                .unwrap();
            let (code, _, _) = call(router(state(d.path())), req).await;
            assert_eq!(code, 403, "{host}");
        }
        for host in ["localhost:1", "[::1]:9", "127.0.0.1"] {
            let req = Request::get("/api/snapshot")
                .header("host", host)
                .body(Body::empty())
                .unwrap();
            assert_eq!(call(router(state(d.path())), req).await.0, 200, "{host}");
        }
    }

    #[tokio::test]
    async fn snapshot_is_json_state() {
        let d = tempfile::tempdir().unwrap();
        let (code, v, headers) = call(router(state(d.path())), get_req("/api/snapshot")).await;
        assert_eq!(code, 200);
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(v["app_state"], "idle");
    }

    #[tokio::test]
    async fn approvals_list_pending_and_decided() {
        let d = tempfile::tempdir().unwrap();
        let s = state(d.path());
        let id = pending(&s);
        let (_, v, _) = call(router(s.clone()), get_req("/api/approvals")).await;
        assert_eq!(v["pending"][0]["id"], id);
        assert_eq!(v["recent"], json!([]));
        let (code, v, _) = call(
            router(s.clone()),
            decide_req(
                &id,
                json!({"decision": "approve", "by": "mac"}),
                &[("x-loopx-token", "secret-token")],
            ),
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(v["status"], "approved");
        assert_eq!(v["decided_by"], "mac");
        let (_, v, _) = call(router(s), get_req("/api/approvals")).await;
        assert_eq!(v["pending"], json!([]));
        assert_eq!(v["recent"][0]["status"], "approved");
    }

    #[tokio::test]
    async fn writes_need_the_token_or_a_same_origin_browser() {
        let d = tempfile::tempdir().unwrap();
        let s = state(d.path());
        let id = pending(&s);
        let body = json!({"decision": "deny"});
        let none = call(router(s.clone()), decide_req(&id, body.clone(), &[]))
            .await
            .0;
        let wrong = call(
            router(s.clone()),
            decide_req(&id, body.clone(), &[("x-loopx-token", "nope")]),
        )
        .await
        .0;
        let cross = call(
            router(s.clone()),
            decide_req(&id, body.clone(), &[("origin", "http://evil.example.com")]),
        )
        .await
        .0;
        assert_eq!(
            (none.as_u16(), wrong.as_u16(), cross.as_u16()),
            (403, 403, 403)
        );
        assert_eq!(
            approvals::get(&s.paths.approvals_dir, &id).unwrap()["status"],
            "pending"
        );
        let same = call(
            router(s),
            decide_req(&id, body, &[("origin", "http://127.0.0.1:8420")]),
        )
        .await
        .0;
        assert_eq!(same, 200);
    }

    #[tokio::test]
    async fn decision_errors_map_to_http_statuses() {
        let d = tempfile::tempdir().unwrap();
        let s = state(d.path());
        let id = pending(&s);
        let tok = [("x-loopx-token", "secret-token")];
        let (c, _, _) = call(
            router(s.clone()),
            decide_req("apr_nope", json!({"decision": "deny"}), &tok),
        )
        .await;
        assert_eq!(c, 404);
        let (c, _, _) = call(
            router(s.clone()),
            decide_req(&id, json!({"decision": "maybe"}), &tok),
        )
        .await;
        assert_eq!(c, 400);
        let (c, _, _) = call(
            router(s.clone()),
            decide_req(&id, json!({"decision": "deny"}), &tok),
        )
        .await;
        assert_eq!(c, 200);
        let (c, v, _) = call(
            router(s),
            decide_req(&id, json!({"decision": "approve"}), &tok),
        )
        .await;
        assert_eq!(c, 409);
        assert!(v["error"].as_str().unwrap().contains("already denied"));
    }

    #[tokio::test]
    async fn unknown_paths_are_404() {
        let d = tempfile::tempdir().unwrap();
        let (code, _, _) = call(router(state(d.path())), get_req("/no-such-page")).await;
        assert_eq!(code, 404);
    }

    #[test]
    fn token_comparison_is_exact() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
    }

    #[tokio::test]
    async fn the_event_stream_starts_with_a_snapshot() {
        let d = tempfile::tempdir().unwrap();
        let s = state(d.path());
        let resp = router(s.clone())
            .oneshot(get_req("/api/events"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
        let mut body = resp.into_body();
        let frame = body.frame().await.unwrap().unwrap();
        let text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        assert!(text.starts_with("event: snapshot\n"), "{text}");
        let data = text.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(data).unwrap()["app_state"],
            "idle"
        );
        // A later change reaches the open stream.
        s.tx.send(Arc::new(r#"{"app_state":"running"}"#.to_string()))
            .unwrap();
        let frame = body.frame().await.unwrap().unwrap();
        let text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        assert!(text.contains(r#"data: {"app_state":"running"}"#), "{text}");
    }
}
