//! OAuth routes: the provider redirects (`/oauth/google/callback`, `/oauth/callback`),
//! `POST /api/oauth/{provider}/start`, `POST /api/oauth/{provider}/logout` and
//! `GET /api/oauth/{provider}/status`. See docs/contract/oauth.md.
//!
//! The redirects are plain browser GETs, so they carry no token; they are guarded by the
//! single-use, 10-minute `state` that only an authorised `start` can mint. `start` and
//! `logout` use the same authorisation as `POST /api/approvals/{id}`.

use crate::api::{authorized_to_write, host_allowed, json_error, Shared};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use loopx_core::oauth::{self, OauthContext, Provider};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

type Ctx = Extension<Arc<OauthContext>>;

pub fn routes(ctx: Arc<OauthContext>) -> Router<Shared> {
    Router::new()
        .route("/oauth/google/callback", get(google_callback))
        .route("/oauth/callback", get(linkedin_callback))
        .route("/api/oauth/:provider/start", post(start))
        .route("/api/oauth/:provider/logout", post(logout))
        .route("/api/oauth/:provider/status", get(status))
        .layer(Extension(ctx))
}

fn provider_or_404(name: &str) -> Result<Provider, Box<Response>> {
    Provider::parse(name)
        .ok_or_else(|| Box::new(json_error(StatusCode::NOT_FOUND, "unknown provider")))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// A small page that reads fine in a browser tab and in an `ASWebAuthenticationSession`.
/// With an `app_url` it also navigates there (the session watches for that custom scheme).
fn page(ok: bool, message: &str, app_url: Option<&str>) -> Response {
    let heading = if ok { "Connected" } else { "Sign-in failed" };
    let (link, script) = match app_url {
        Some(u) => (
            format!("<p><a href=\"{}\">Return to LoopX</a></p>", html_escape(u)),
            format!(
                "<script>setTimeout(function(){{location.href={};}},50);</script>",
                serde_json::to_string(u)
                    .unwrap_or_default()
                    .replace('<', "\\u003c")
            ),
        ),
        None => (
            "<p>You can close this tab and go back to LoopX.</p>".to_string(),
            String::new(),
        ),
    };
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"color-scheme\" content=\"light dark\"><title>LoopX - {heading}</title>\
<style>body{{font:16px/1.5 -apple-system,system-ui,sans-serif;display:grid;place-items:center;min-height:100vh;margin:0;padding:0 16px}}\
main{{max-width:28rem;text-align:center}}h1{{font-size:1.4rem}}.{cls}{{font-weight:600}}</style></head>\
<body><main><h1 class=\"{cls}\">{heading}</h1><p>{msg}</p>{link}</main>{script}</body></html>",
        cls = if ok { "ok" } else { "err" },
        msg = html_escape(message),
    );
    (
        if ok {
            StatusCode::OK
        } else {
            StatusCode::BAD_REQUEST
        },
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // The URL that brought us here carries the authorization code.
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        html,
    )
        .into_response()
}

fn loopx_core_lang(headers: &HeaderMap) -> crate::ui::i18n::Lang {
    let get = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    crate::ui::i18n::resolve_language(get("cookie"), get("accept-language"))
}

async fn google_callback(
    s: State<Shared>,
    ctx: Ctx,
    headers: HeaderMap,
    q: Query<HashMap<String, String>>,
) -> Response {
    callback(Provider::Google, s, ctx, headers, q).await
}

async fn linkedin_callback(
    s: State<Shared>,
    ctx: Ctx,
    headers: HeaderMap,
    q: Query<HashMap<String, String>>,
) -> Response {
    callback(Provider::Linkedin, s, ctx, headers, q).await
}

async fn callback(
    provider: Provider,
    State(s): State<Shared>,
    Extension(ctx): Ctx,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !host_allowed(&headers) {
        return json_error(StatusCode::FORBIDDEN, "forbidden host");
    }
    let state = q.get("state").cloned().unwrap_or_default();
    let now = oauth::now_secs();
    // A sign-in started from the dashboard's Connectors page (the legacy `kind: connector`):
    // the refresh token becomes the connector's secret and the browser goes back to
    // /connectors with a flash.
    if provider == Provider::Google
        && oauth::peek_pending(&ctx.dir, &state, now, true)
            .is_some_and(|p| p.kind == crate::ui::pages::connectors::FORM_KIND)
    {
        let ui = s.ui.clone();
        let lang = loopx_core_lang(&headers);
        let (code, error) = (q.get("code").cloned(), q.get("error").cloned());
        let c2 = ctx.clone();
        let (ok, message) = tokio::task::spawn_blocking(move || {
            crate::ui::pages::connectors::google_callback(
                &ui,
                &c2,
                lang,
                &state,
                code.as_deref(),
                error.as_deref(),
            )
        })
        .await
        .unwrap_or((
            false,
            "Internal error while finishing the sign-in".to_string(),
        ));
        return crate::ui::glue::into_response(crate::ui::UiResponse::flash_redirect(
            ok,
            &message,
            "/connectors",
        ));
    }
    let code = q.get("code").cloned();
    let error = q.get("error").cloned();
    let c2 = ctx.clone();
    let result = tokio::task::spawn_blocking(move || {
        oauth::complete(
            &c2,
            provider,
            &state,
            code.as_deref(),
            error.as_deref(),
            now,
        )
    })
    .await;
    match result {
        Ok(Ok(done)) => {
            let url = done
                .app_scheme
                .as_deref()
                .map(|sch| oauth::app_redirect_url(sch, provider, Some(&done.account), None));
            page(
                true,
                &format!("{} is connected.", provider_label(provider)),
                url.as_deref(),
            )
        }
        Ok(Err(fail)) => {
            let url = fail
                .app_scheme
                .as_deref()
                .map(|sch| oauth::app_redirect_url(sch, provider, None, Some(&fail.message)));
            page(false, &fail.message, url.as_deref())
        }
        Err(_) => page(false, "Internal error while finishing the sign-in", None),
    }
}

fn provider_label(p: Provider) -> &'static str {
    match p {
        Provider::Google => "Google",
        Provider::Linkedin => "LinkedIn",
    }
}

/// `127.0.0.1:<port of the Host header>`: the redirect must come back to this very server.
fn authority(headers: &HeaderMap) -> String {
    let port = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.rsplit_once(':'))
        .map(|(_, p)| p.to_string())
        .filter(|p| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty());
    match port {
        Some(p) => format!("127.0.0.1:{p}"),
        None => "127.0.0.1".to_string(),
    }
}

async fn start(
    State(s): State<Shared>,
    Extension(ctx): Ctx,
    UrlPath(provider): UrlPath<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    if !host_allowed(&headers) {
        return json_error(StatusCode::FORBIDDEN, "forbidden host");
    }
    if !authorized_to_write(&headers, &s) {
        return json_error(StatusCode::FORBIDDEN, "missing or wrong token");
    }
    let provider = match provider_or_404(&provider) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let body = body.map(|b| b.0).unwrap_or(Value::Null);
    let text = |k: &str| body[k].as_str().map(str::to_string);
    let req = oauth::StartRequest {
        account: text("account").unwrap_or_default(),
        kind: text("kind").unwrap_or_default(),
        scope: text("scope"),
        login_hint: text("login_hint"),
        app_scheme: text("app_scheme"),
    };
    let authority = authority(&headers);
    let result = tokio::task::spawn_blocking(move || {
        oauth::start(&ctx, provider, &req, &authority, oauth::now_secs())
    })
    .await;
    match result {
        Ok(Ok(started)) => Json(json!({
            "authorization_url": started.authorization_url,
            "state": started.state,
            "redirect_uri": started.redirect_uri,
        }))
        .into_response(),
        Ok(Err(e @ oauth::OauthError::Config(_))) => {
            json_error(StatusCode::BAD_REQUEST, &e.to_string())
        }
        Ok(Err(e)) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    }
}

async fn logout(
    State(s): State<Shared>,
    Extension(ctx): Ctx,
    UrlPath(provider): UrlPath<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    if !host_allowed(&headers) {
        return json_error(StatusCode::FORBIDDEN, "forbidden host");
    }
    if !authorized_to_write(&headers, &s) {
        return json_error(StatusCode::FORBIDDEN, "missing or wrong token");
    }
    let provider = match provider_or_404(&provider) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let account = body
        .and_then(|b| b.0["account"].as_str().map(str::to_string))
        .unwrap_or_else(|| oauth::DEFAULT_ACCOUNT.to_string());
    match tokio::task::spawn_blocking(move || oauth::logout(&ctx, provider, &account)).await {
        Ok(Ok(())) => Json(json!({"ok": true})).into_response(),
        Ok(Err(e)) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    }
}

async fn status(
    Extension(ctx): Ctx,
    UrlPath(provider): UrlPath<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !host_allowed(&headers) {
        return json_error(StatusCode::FORBIDDEN, "forbidden host");
    }
    let provider = match provider_or_404(&provider) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let account = q
        .get("account")
        .cloned()
        .unwrap_or_else(|| oauth::DEFAULT_ACCOUNT.to_string());
    let state = q.get("state").cloned();
    let now = oauth::now_secs();
    let result = tokio::task::spawn_blocking(move || {
        let st = oauth::status(&ctx, provider, &account, now)?;
        let flow = state.map(|state| match oauth::flow_result(&ctx.dir, &state) {
            Some(r) => json!({
                "state": if r["ok"] == true { "done" } else { "failed" },
                "message": r["message"],
            }),
            None if oauth::peek_pending(&ctx.dir, &state, now, false).is_some() => {
                json!({"state": "pending"})
            }
            None => json!({"state": "unknown"}),
        });
        Ok::<_, oauth::OauthError>((st, flow))
    })
    .await;
    match result {
        Ok(Ok((st, flow))) => {
            let mut v = json!({
                "provider": provider.name(), "account": st.account, "connected": st.connected,
                "has_refresh_token": st.has_refresh_token, "expires_at": st.expires_at,
                "expired": st.expired,
            });
            if let Some(f) = flow {
                v["flow"] = f;
            }
            Json(v).into_response()
        }
        Ok(Err(e)) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AppState;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use http_body_util::BodyExt;
    use loopx_core::keychain::MemoryStore;
    use tower::ServiceExt;

    fn setup(dir: &std::path::Path) -> (Router, Arc<OauthContext>) {
        std::fs::write(
            dir.join("mail_oauth.json"),
            r#"{"google":{"client_id":"cid","client_secret":"cs"}}"#,
        )
        .unwrap();
        let ctx = Arc::new(OauthContext::new(
            &dir.join("oauth"),
            Arc::new(MemoryStore::new("t")),
            &dir.join("mail_oauth.json"),
        ));
        let state = AppState::new(dir.to_path_buf(), "tok".into());
        (routes(ctx.clone()).with_state(state), ctx)
    }

    async fn call(app: &Router, req: HttpRequest<Body>) -> (StatusCode, String) {
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into())
    }

    fn start_req(provider: &str, extra: &[(&str, &str)], body: &str) -> HttpRequest<Body> {
        let mut b = HttpRequest::post(format!("/api/oauth/{provider}/start"))
            .header("host", "127.0.0.1:8420")
            .header("content-type", "application/json");
        for (k, v) in extra {
            b = b.header(*k, *v);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn start_needs_the_token_or_same_origin() {
        let d = tempfile::tempdir().unwrap();
        let (app, _) = setup(d.path());
        assert_eq!(call(&app, start_req("google", &[], "{}")).await.0, 403);
        assert_eq!(
            call(&app, start_req("google", &[("x-loopx-token", "bad")], "{}"))
                .await
                .0,
            403
        );
        assert_eq!(
            call(
                &app,
                start_req("google", &[("origin", "http://evil.example")], "{}")
            )
            .await
            .0,
            403
        );
        let (code, body) = call(
            &app,
            start_req(
                "google",
                &[("x-loopx-token", "tok")],
                r#"{"account":"cal"}"#,
            ),
        )
        .await;
        assert_eq!(code, 200, "{body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert!(v["authorization_url"]
            .as_str()
            .unwrap()
            .contains("code_challenge_method=S256"));
        assert!(v["authorization_url"]
            .as_str()
            .unwrap()
            .contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8420%2Foauth%2Fgoogle%2Fcallback"));
        assert!(!v["state"].as_str().unwrap().is_empty());
        let (code, _) = call(
            &app,
            start_req("google", &[("origin", "http://127.0.0.1:8420")], "{}"),
        )
        .await;
        assert_eq!(code, 200);
    }

    #[tokio::test]
    async fn start_validates_provider_scheme_and_client() {
        let d = tempfile::tempdir().unwrap();
        let (app, _) = setup(d.path());
        let auth = [("x-loopx-token", "tok")];
        assert_eq!(call(&app, start_req("myspace", &auth, "{}")).await.0, 404);
        assert_eq!(
            call(
                &app,
                start_req("google", &auth, r#"{"app_scheme":"javascript"}"#)
            )
            .await
            .0,
            400
        );
        // LinkedIn has no client configured in this fixture.
        assert_eq!(call(&app, start_req("linkedin", &auth, "{}")).await.0, 400);
        assert_eq!(
            call(
                &app,
                start_req("google", &auth, r#"{"app_scheme":"loopx-auth"}"#)
            )
            .await
            .0,
            200
        );
    }

    #[tokio::test]
    async fn callback_with_a_bad_state_is_a_failure_page_without_echoing_input() {
        let d = tempfile::tempdir().unwrap();
        let (app, _) = setup(d.path());
        let req = HttpRequest::get(
            "/oauth/google/callback?state=%3Cscript%3Ex%3C/script%3E&code=SECRETCODE",
        )
        .header("host", "127.0.0.1:8420")
        .body(Body::empty())
        .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 400);
        assert_eq!(resp.headers()["cache-control"], "no-store");
        let body = String::from_utf8(
            resp.into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains("expired or was already used"));
        assert!(!body.contains("SECRETCODE") && !body.contains("<script>x"));
        let foreign = HttpRequest::get("/oauth/google/callback?state=a")
            .header("host", "evil.example")
            .body(Body::empty())
            .unwrap();
        assert_eq!(call(&app, foreign).await.0, 403);
    }

    #[tokio::test]
    async fn a_cancelled_sign_in_hands_control_back_to_the_app_scheme() {
        let d = tempfile::tempdir().unwrap();
        let (app, _) = setup(d.path());
        let (_, body) = call(
            &app,
            start_req(
                "google",
                &[("x-loopx-token", "tok")],
                r#"{"app_scheme":"loopx-auth"}"#,
            ),
        )
        .await;
        let state = serde_json::from_str::<Value>(&body).unwrap()["state"]
            .as_str()
            .unwrap()
            .to_string();
        let req = HttpRequest::get(format!(
            "/oauth/google/callback?state={state}&error=access_denied"
        ))
        .header("host", "127.0.0.1:8420")
        .body(Body::empty())
        .unwrap();
        let (code, page) = call(&app, req).await;
        assert_eq!(code, 400);
        assert!(
            page.contains("loopx-auth://oauth?provider=google&amp;status=error"),
            "{page}"
        );
        // The flow result is visible to a polling client.
        let st = HttpRequest::get(format!("/api/oauth/google/status?state={state}"))
            .header("host", "127.0.0.1:8420")
            .body(Body::empty())
            .unwrap();
        let (code, body) = call(&app, st).await;
        assert_eq!(code, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["flow"]["state"], "failed");
        assert_eq!(v["connected"], false);
    }
}
