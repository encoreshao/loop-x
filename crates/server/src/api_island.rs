//! Token-authenticated JSON routes for the Mac app's island: start a chat reply, decide a chat
//! approval card, start a loop. They call the same functions the dashboard pages use.

use crate::api::{authorized_to_write, host_allowed, json_error, Shared};
use crate::ui::i18n::Lang;
use crate::ui::pages::chat;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/chat", post(chat_start))
        .route("/api/chat/approve", post(chat_decide_approve))
        .route("/api/chat/reject", post(chat_decide_reject))
        .route("/api/loops/:name/run", post(run_loop))
}

/// `Some(error response)` unless the request is from loopback with the token (or same origin).
fn guard(headers: &HeaderMap, s: &Shared) -> Option<Response> {
    if !host_allowed(headers) {
        return Some(json_error(StatusCode::FORBIDDEN, "forbidden host"));
    }
    if !authorized_to_write(headers, s) {
        return Some(json_error(StatusCode::FORBIDDEN, "missing or wrong token"));
    }
    None
}

fn str_field<'a>(body: &'a Value, key: &str) -> Option<&'a str> {
    body.get(key).and_then(Value::as_str)
}

async fn chat_start(
    State(s): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(denied) = guard(&headers, &s) {
        return denied;
    }
    let text = str_field(&body, "text").unwrap_or("").to_string();
    let session = str_field(&body, "session").map(str::to_string);
    let ui = s.ui.clone();
    let result = tokio::task::spawn_blocking(move || {
        chat::start_reply(&ui, Lang::En, &text, session.as_deref(), None)
    })
    .await
    .unwrap_or_else(|_| Err("internal error".into()));
    match result {
        Ok((reply_key, session)) => {
            Json(json!({"reply_key": reply_key, "session": session})).into_response()
        }
        Err(message) => json_error(StatusCode::BAD_REQUEST, &message),
    }
}

fn id_and_session(body: &Value) -> Option<(String, String)> {
    let id = str_field(body, "id").filter(|v| !v.is_empty())?;
    let session = str_field(body, "session").filter(|v| !v.is_empty())?;
    Some((id.to_string(), session.to_string()))
}

async fn chat_decide_approve(
    State(s): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(denied) = guard(&headers, &s) {
        return denied;
    }
    let Some((id, session)) = id_and_session(&body) else {
        return json_error(StatusCode::BAD_REQUEST, "id and session are required");
    };
    let ui = s.ui.clone();
    let out = tokio::task::spawn_blocking(move || chat::chat_approve(&ui, Lang::En, &id, &session))
        .await
        .unwrap_or_else(|_| json!({"ok": false, "error": "internal error"}));
    Json(out).into_response()
}

async fn chat_decide_reject(
    State(s): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(denied) = guard(&headers, &s) {
        return denied;
    }
    let Some((id, session)) = id_and_session(&body) else {
        return json_error(StatusCode::BAD_REQUEST, "id and session are required");
    };
    let ok = chat::chat_reject(&s.ui, &id, &session);
    let out = if ok {
        json!({"ok": true})
    } else {
        json!({"ok": false, "error": "This action expired or was already handled."})
    };
    Json(out).into_response()
}

async fn run_loop(
    State(s): State<Shared>,
    headers: HeaderMap,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    use loopx_core::run_control::TriggerOutcome;
    if let Some(denied) = guard(&headers, &s) {
        return denied;
    }
    let ui = s.ui.clone();
    let outcome =
        tokio::task::spawn_blocking(move || crate::ui::pages::runs::trigger_named(&ui, &name))
            .await
            .ok()
            .flatten();
    match outcome {
        None => json_error(StatusCode::NOT_FOUND, "unknown loop"),
        Some(TriggerOutcome::Started) => {
            (StatusCode::ACCEPTED, Json(json!({"started": true}))).into_response()
        }
        Some(TriggerOutcome::AlreadyRunning) => {
            json_error(StatusCode::CONFLICT, "A run is already in progress")
        }
        Some(TriggerOutcome::LauncherMissing(path)) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("run-loop-now.sh not found at {path}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{router, AppState};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn state(dir: &std::path::Path) -> Shared {
        AppState::new(dir.to_path_buf(), "secret-token".into())
    }

    async fn post(s: Shared, path: &str, body: Value, headers: &[(&str, &str)]) -> (u16, Value) {
        let mut req = Request::post(path)
            .header("host", "127.0.0.1:8420")
            .header("content-type", "application/json");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = router(s)
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let code = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    const TOKEN: [(&str, &str); 1] = [("x-loopx-token", "secret-token")];

    #[tokio::test]
    async fn chat_needs_the_token() {
        let d = tempfile::tempdir().unwrap();
        let (code, _) = post(state(d.path()), "/api/chat", json!({"text": "hi"}), &[]).await;
        assert_eq!(code, 403);
    }

    #[tokio::test]
    async fn chat_rejects_blank_text() {
        let d = tempfile::tempdir().unwrap();
        let (code, v) = post(state(d.path()), "/api/chat", json!({"text": "   "}), &TOKEN).await;
        assert_eq!(code, 400);
        assert!(v["error"].as_str().is_some());
    }

    #[tokio::test]
    async fn chat_starts_a_reply_and_returns_its_key_and_session() {
        // The reply thread runs the assistant through this wrapper; `true` keeps the test off the real `claude`.
        std::env::set_var("LOOPX_CHAT_SHELL", "/usr/bin/true");
        let d = tempfile::tempdir().unwrap();
        let (code, v) = post(
            state(d.path()),
            "/api/chat",
            json!({"text": "hello"}),
            &TOKEN,
        )
        .await;
        assert_eq!(code, 200);
        assert!(!v["reply_key"].as_str().unwrap().is_empty());
        assert!(!v["session"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn approve_expired_says_so() {
        let d = tempfile::tempdir().unwrap();
        let (code, v) = post(
            state(d.path()),
            "/api/chat/approve",
            json!({"id": "nope", "session": "s"}),
            &TOKEN,
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "This action expired or was already handled.");
    }

    #[tokio::test]
    async fn reject_unknown_says_so() {
        let d = tempfile::tempdir().unwrap();
        let (_, v) = post(
            state(d.path()),
            "/api/chat/reject",
            json!({"id": "nope", "session": "s"}),
            &TOKEN,
        )
        .await;
        assert_eq!(v["ok"], false);
    }

    #[tokio::test]
    async fn approve_and_reject_need_id_session_and_token() {
        let d = tempfile::tempdir().unwrap();
        let s = state(d.path());
        assert_eq!(
            post(s.clone(), "/api/chat/approve", json!({"id": "x"}), &TOKEN)
                .await
                .0,
            400
        );
        assert_eq!(
            post(
                s.clone(),
                "/api/chat/reject",
                json!({"session": "x"}),
                &TOKEN
            )
            .await
            .0,
            400
        );
        assert_eq!(
            post(
                s,
                "/api/chat/approve",
                json!({"id": "x", "session": "s"}),
                &[]
            )
            .await
            .0,
            403
        );
    }

    #[tokio::test]
    async fn run_unknown_loop_is_404() {
        let d = tempfile::tempdir().unwrap();
        let (code, _) = post(state(d.path()), "/api/loops/nope/run", json!({}), &TOKEN).await;
        assert_eq!(code, 404);
    }

    #[tokio::test]
    async fn run_needs_the_token() {
        let d = tempfile::tempdir().unwrap();
        let (code, _) = post(state(d.path()), "/api/loops/topic-loop/run", json!({}), &[]).await;
        assert_eq!(code, 403);
    }

    #[tokio::test]
    async fn run_conflict_is_409() {
        let d = tempfile::tempdir().unwrap();
        let topics = d.path().join("outputs/topic-monitor");
        std::fs::create_dir_all(&topics).unwrap();
        std::fs::write(
            topics.join("status.json"),
            r#"{"topics":{"a":{"state":"running"}}}"#,
        )
        .unwrap();
        let (code, v) = post(
            state(d.path()),
            "/api/loops/topic-loop/run",
            json!({}),
            &TOKEN,
        )
        .await;
        assert_eq!(code, 409);
        assert_eq!(v["error"], "A run is already in progress");
    }
}
