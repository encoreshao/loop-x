//! The bridge between the axum router and the page registry: claim a request if a
//! registered route (and the kill switches) say this server renders it, otherwise
//! hand it back untouched so the caller can proxy it to the legacy dashboard.

use super::routes::{UiRequest, UiResponse};
use super::state::UiState;
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode};
use std::sync::Arc;

const MAX_BODY: usize = 1024 * 1024;

fn host_name(headers: &HeaderMap) -> Option<String> {
    let host = headers
        .get(header::HOST)?
        .to_str()
        .ok()?
        .to_ascii_lowercase();
    Some(match host.strip_prefix('[') {
        Some(rest) => format!("[{}]", rest.split(']').next()?),
        None => host.split(':').next()?.to_string(),
    })
}

/// Pages carry the CSRF token, so a DNS-rebinding page must not be able to read
/// them: only loopback names (plus `LOOPX_ALLOWED_HOSTS`, comma separated, for a
/// reverse proxy such as `loop.x`) are served.
pub fn host_allowed(headers: &HeaderMap) -> bool {
    let Some(name) = host_name(headers) else {
        return false;
    };
    if matches!(name.as_str(), "127.0.0.1" | "localhost" | "[::1]") {
        return true;
    }
    std::env::var("LOOPX_ALLOWED_HOSTS")
        .unwrap_or_default()
        .split(',')
        .any(|h| !h.trim().is_empty() && h.trim().eq_ignore_ascii_case(&name))
}

pub fn into_response(resp: UiResponse) -> Response<Body> {
    let mut out =
        Response::builder().status(StatusCode::from_u16(resp.status).unwrap_or(StatusCode::OK));
    for (name, value) in &resp.headers {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            out = out.header(n, v);
        }
    }
    out.body(Body::from(resp.body)).expect("valid response")
}

/// `Ok(response)` when a page route handled the request, `Err(request)` (body
/// untouched) when it should be proxied.
pub async fn try_serve(
    ui: &Arc<UiState>,
    req: Request<Body>,
) -> Result<Response<Body>, Request<Body>> {
    let path = req.uri().path().to_string();
    if !ui.registry.serves(ui.as_ref(), req.method(), &path) {
        return Err(req);
    }
    if !host_allowed(req.headers()) {
        return Ok(into_response(UiResponse::text(403, "forbidden host")));
    }
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        return Ok(into_response(UiResponse::text(
            413,
            "request body too large",
        )));
    };
    let ui_req = UiRequest {
        method: parts.method,
        path,
        query: parts.uri.query().unwrap_or("").to_string(),
        headers: parts.headers,
        body: bytes.to_vec(),
    };
    let state = ui.clone();
    let resp = tokio::task::spawn_blocking(move || state.registry.dispatch(&state, &ui_req))
        .await
        .ok()
        .flatten();
    Ok(into_response(resp.unwrap_or_else(|| {
        UiResponse::text(500, "the page handler failed")
    })))
}
