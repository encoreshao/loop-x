//! `POST /api/voice/transcribe`: the raw recording in, `{"text"}` out. Same-origin pages and the
//! Mac app (with the lock-file token) may call it.

use crate::api::{authorized_to_write, host_allowed, json_error, Shared};
use crate::ui::i18n;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use loopx_core::voice::{self, Setup, VoiceError};
use serde_json::json;

pub fn routes() -> Router<Shared> {
    Router::new().route(
        "/api/voice/transcribe",
        post(transcribe).layer(DefaultBodyLimit::max(voice::MAX_AUDIO_BYTES + 1)),
    )
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn status_for(err: &VoiceError) -> StatusCode {
    match err {
        VoiceError::Off | VoiceError::NotSetUp => StatusCode::SERVICE_UNAVAILABLE,
        VoiceError::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        VoiceError::Unsupported => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        VoiceError::NothingHeard => StatusCode::UNPROCESSABLE_ENTITY,
        VoiceError::Failed(_) => StatusCode::BAD_GATEWAY,
    }
}

async fn transcribe(State(s): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    if !host_allowed(&headers) {
        return json_error(StatusCode::FORBIDDEN, "forbidden host");
    }
    if !authorized_to_write(&headers, &s) {
        return json_error(StatusCode::FORBIDDEN, "missing or wrong token");
    }
    let lang = i18n::resolve_language(
        header_str(&headers, header::COOKIE),
        header_str(&headers, header::ACCEPT_LANGUAGE),
    );
    let content_type = header_str(&headers, header::CONTENT_TYPE)
        .unwrap_or("")
        .to_string();
    let config_path = s.voice_config.clone();
    let store = crate::ui::voice::secret_store(&s.ui);
    let result = tokio::task::spawn_blocking(move || {
        let cfg = voice::load(&config_path);
        if cfg.engine == voice::Engine::Off {
            return Err(VoiceError::Off);
        }
        let key = voice::openai_key(store.as_ref());
        let setup = Setup::detect_cached(&cfg, key);
        voice::transcribe(&body, &content_type, &cfg, &setup)
    })
    .await
    .unwrap_or_else(|_| Err(VoiceError::Failed("internal error".into())));
    match result {
        Ok(t) => Json(json!({"text": t.text, "engine": t.engine.key()})).into_response(),
        Err(e) => json_error(status_for(&e), &crate::ui::voice::error_text(lang, &e)),
    }
}

#[cfg(test)]
mod tests {
    use crate::api::{router, AppState};
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::Value;
    use tower::ServiceExt;

    async fn post(dir: &std::path::Path, headers: &[(&str, &str)], body: Vec<u8>) -> (u16, Value) {
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("voice.json"), r#"{"engine":"off"}"#).unwrap();
        let s = AppState::with_home(dir.to_path_buf(), "secret-token".into(), home);
        let mut req = Request::post("/api/voice/transcribe")
            .header("host", "127.0.0.1:8420")
            .header("content-type", "audio/webm");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = router(s)
            .oneshot(req.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let code = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn a_cross_site_page_cannot_transcribe() {
        let d = tempfile::tempdir().unwrap();
        let (code, _) = post(
            d.path(),
            &[("origin", "http://evil.example")],
            b"x".to_vec(),
        )
        .await;
        assert_eq!(code, 403);
    }

    #[tokio::test]
    async fn the_dashboard_reaches_the_engine_and_gets_a_translated_error() {
        let d = tempfile::tempdir().unwrap();
        let headers = [
            ("origin", "http://127.0.0.1:8420"),
            ("accept-language", "fr"),
        ];
        let (code, body) = post(d.path(), &headers, b"x".to_vec()).await;
        assert_eq!(code, 503);
        let msg = body["error"].as_str().unwrap();
        assert!(
            !msg.is_empty() && msg != "Voice input is turned off in Settings.",
            "{msg}"
        );
    }

    #[tokio::test]
    async fn the_app_token_works_and_recordings_up_to_the_limit_get_through() {
        let d = tempfile::tempdir().unwrap();
        let big = vec![0u8; 3 << 20];
        let (code, _) = post(d.path(), &[("x-loopx-token", "secret-token")], big).await;
        assert_eq!(code, 503);
    }
}
