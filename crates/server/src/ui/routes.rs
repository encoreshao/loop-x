//! Request/response types for server-rendered pages and the route registry.
//!
//! A page module (`src/ui/pages/<name>.rs`) exposes `pub fn register(r: &mut
//! Registry)` and calls `r.get("/path", handler)` / `r.post(..)` for the routes
//! it owns. `build.rs` discovers the module, so nothing shared is edited. Any
//! request no registered route claims falls through to the legacy dashboard.
//!
//! Patterns are `/`-separated segments: a literal, `:name` (one segment, URL
//! decoded into [`UiCtx::param`]) or a trailing `*name` (the rest of the path,
//! slashes included). When several patterns match, literal beats `:param` beats
//! `*rest`, then the earlier registration wins.

use super::csrf;
use super::i18n::{self, Lang};
use super::pyfmt::{self, Form};
use super::state::UiState;
use axum::http::{header, HeaderMap, Method};
use serde_json::Value;
use std::collections::HashMap;

/// One incoming request, already split into path and query.
#[derive(Debug, Clone)]
pub struct UiRequest {
    pub method: Method,
    /// The raw (still percent-encoded) path, without the query.
    pub path: String,
    pub query: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl UiRequest {
    /// `GET <path_and_query>` with no headers (tests and golden runs).
    pub fn get(path_and_query: &str) -> Self {
        Self::new(Method::GET, path_and_query)
    }

    pub fn new(method: Method, path_and_query: &str) -> Self {
        let (path, query) = path_and_query
            .split_once('?')
            .unwrap_or((path_and_query, ""));
        Self {
            method,
            path: path.to_string(),
            query: query.to_string(),
            headers: HeaderMap::new(),
            body: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: &'static str, value: &str) -> Self {
        self.headers
            .insert(name, value.parse().expect("header value"));
        self
    }

    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn lang(&self) -> Lang {
        i18n::resolve_language(self.header("cookie"), self.header("accept-language"))
    }
}

/// What a handler gets: the request, shared state, the resolved language and the
/// route's captured parameters.
pub struct UiCtx<'a> {
    pub state: &'a UiState,
    pub req: &'a UiRequest,
    pub lang: Lang,
    params: HashMap<String, String>,
}

impl<'a> UiCtx<'a> {
    pub fn new(state: &'a UiState, req: &'a UiRequest) -> Self {
        Self {
            state,
            req,
            lang: req.lang(),
            params: HashMap::new(),
        }
    }

    /// `i18n.t(text)` in this request's language.
    pub fn t(&self, text: &str) -> String {
        i18n::t(self.lang, text, &[])
    }

    /// `i18n.t(text, **args)`.
    pub fn tf(&self, text: &str, args: &[(&str, &str)]) -> String {
        i18n::t(self.lang, text, args)
    }

    /// A `:name` / `*name` capture, URL-decoded.
    pub fn param(&self, name: &str) -> &str {
        self.params.get(name).map_or("", String::as_str)
    }

    /// The query string, parsed like `urllib.parse.parse_qs`.
    pub fn query(&self) -> Form {
        Form::parse(&self.req.query)
    }

    /// The urlencoded request body.
    pub fn form(&self) -> Form {
        Form::from_bytes(&self.req.body)
    }

    /// `flash` / `ok` from the query: the message (if any) and `ok != "0"`.
    pub fn flash(&self) -> (Option<String>, bool) {
        let q = self.query();
        (
            q.first("flash").map(str::to_string),
            q.first("ok") != Some("0"),
        )
    }

    pub fn csrf_input(&self) -> String {
        csrf::input_html(&self.state.csrf_token)
    }

    pub fn csrf_ok(&self) -> bool {
        csrf::check(
            &self.state.csrf_token,
            &self.req.body,
            self.req.header("content-type"),
        )
    }
}

/// A finished response; the server glue turns it into an HTTP response.
#[derive(Debug, Clone, PartialEq)]
pub struct UiResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl UiResponse {
    fn with(status: u16, headers: &[(&str, &str)], body: Vec<u8>) -> Self {
        Self {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body,
        }
    }

    /// `_send_html`.
    pub fn html(body: impl Into<String>) -> Self {
        Self::with(
            200,
            &[
                ("Content-Type", "text/html; charset=utf-8"),
                ("Cache-Control", "no-store"),
            ],
            body.into().into_bytes(),
        )
    }

    /// `_send_json`: `json.dumps(obj)` with Python's default separators.
    pub fn json(status: u16, value: &Value) -> Self {
        Self::with(
            status,
            &[
                ("Content-Type", "application/json; charset=utf-8"),
                ("Cache-Control", "no-store"),
            ],
            loopx_core::pydumps::dumps(value).into_bytes(),
        )
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self::with(
            status,
            &[("Content-Type", "text/plain; charset=utf-8")],
            body.into().into_bytes(),
        )
    }

    /// A body-less redirect (301 for the legacy table, 303 after a POST).
    pub fn redirect(status: u16, location: &str) -> Self {
        Self::with(
            status,
            &[("Location", location), ("Content-Length", "0")],
            Vec::new(),
        )
    }

    /// `_redirect_with_flash`: 303 to `location?flash=<msg>&ok=1|0`.
    pub fn flash_redirect(ok: bool, message: &str, location: &str) -> Self {
        let sep = if location.contains('?') { '&' } else { '?' };
        let query = format!(
            "flash={}&ok={}",
            pyfmt::quote(message, ""),
            if ok { "1" } else { "0" }
        );
        Self::redirect(303, &format!("{location}{sep}{query}"))
    }

    /// The CSRF failure response.
    pub fn forbidden() -> Self {
        Self::text(403, "Forbidden: missing or invalid CSRF token")
    }

    /// `_not_found`: `Not found`, translated.
    pub fn not_found(lang: Lang) -> Self {
        Self::text(404, i18n::t(lang, "Not found", &[]))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn content_type(&self) -> Option<&str> {
        self.header(header::CONTENT_TYPE.as_str())
    }
}

pub type Handler = fn(&UiCtx) -> UiResponse;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Seg {
    Lit(&'static str),
    Param(&'static str),
    Rest(&'static str),
}

/// One registered route.
pub struct Route {
    pub method: Method,
    pub pattern: &'static str,
    segs: Vec<Seg>,
    handler: Handler,
    csrf: bool,
}

fn parse_pattern(pattern: &'static str) -> Vec<Seg> {
    assert!(
        pattern.starts_with('/'),
        "route pattern must start with /: {pattern}"
    );
    let parts: Vec<&'static str> = pattern[1..].split('/').collect();
    let last = parts.len() - 1;
    parts
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            if let Some(name) = p.strip_prefix(':') {
                Seg::Param(name)
            } else if let Some(name) = p.strip_prefix('*') {
                assert!(i == last, "`*rest` must be the last segment: {pattern}");
                Seg::Rest(name)
            } else {
                Seg::Lit(p)
            }
        })
        .collect()
}

impl Route {
    /// How specifically this route matches `segments`: higher is better.
    fn matches(&self, segments: &[&str]) -> Option<(u32, HashMap<String, String>)> {
        let mut params = HashMap::new();
        let mut score = 0u32;
        for (i, seg) in self.segs.iter().enumerate() {
            match seg {
                Seg::Lit(l) => {
                    if segments.get(i) != Some(l) {
                        return None;
                    }
                    score += 100;
                }
                Seg::Param(name) => {
                    let s = segments.get(i)?;
                    params.insert((*name).to_string(), pyfmt::unquote(s));
                    score += 10;
                }
                Seg::Rest(name) => {
                    if segments.len() <= i {
                        return None;
                    }
                    params.insert(
                        (*name).to_string(),
                        pyfmt::unquote(&segments[i..].join("/")),
                    );
                    return Some((score, params));
                }
            }
        }
        (segments.len() == self.segs.len()).then_some((score + 1, params))
    }
}

/// Every page route this server renders itself.
#[derive(Default)]
pub struct Registry {
    routes: Vec<Route>,
}

impl Registry {
    /// The registry with every `src/ui/pages/*.rs` module registered.
    pub fn with_pages() -> Self {
        let mut r = Registry::default();
        super::pages::register_all(&mut r);
        r
    }

    fn add(&mut self, method: Method, pattern: &'static str, handler: Handler, csrf: bool) {
        if let Some(dup) = self
            .routes
            .iter()
            .find(|r| r.method == method && r.pattern == pattern)
        {
            panic!("route {method} {} registered twice", dup.pattern);
        }
        self.routes.push(Route {
            method,
            pattern,
            segs: parse_pattern(pattern),
            handler,
            csrf,
        });
    }

    pub fn get(&mut self, pattern: &'static str, handler: Handler) {
        self.add(Method::GET, pattern, handler, false);
    }

    /// A POST route; the CSRF token is checked before `handler` runs, and a
    /// failure answers 403 exactly like the Python server.
    pub fn post(&mut self, pattern: &'static str, handler: Handler) {
        self.add(Method::POST, pattern, handler, true);
    }

    /// A POST route with no CSRF check (only for requests that carry their own
    /// proof, never for browser forms).
    pub fn post_unchecked(&mut self, pattern: &'static str, handler: Handler) {
        self.add(Method::POST, pattern, handler, false);
    }

    pub fn routes(&self) -> impl Iterator<Item = &Route> {
        self.routes.iter()
    }

    fn find(&self, method: &Method, path: &str) -> Option<(&Route, HashMap<String, String>)> {
        let segments: Vec<&str> = path.strip_prefix('/')?.split('/').collect();
        let mut best: Option<(u32, &Route, HashMap<String, String>)> = None;
        for route in self.routes.iter().filter(|r| r.method == *method) {
            if let Some((score, params)) = route.matches(&segments) {
                if best.as_ref().is_none_or(|(s, _, _)| score > *s) {
                    best = Some((score, route, params));
                }
            }
        }
        best.map(|(_, r, p)| (r, p))
    }

    /// The pattern that claims `method path`, if any (kill switches ignored).
    pub fn pattern_for(&self, method: &Method, path: &str) -> Option<&'static str> {
        self.find(method, path).map(|(r, _)| r.pattern)
    }

    /// Run the route that claims the request. `None` means no registered route
    /// does (or the kill switch sent it to the legacy dashboard), so the caller
    /// proxies it.
    pub fn dispatch(&self, state: &UiState, req: &UiRequest) -> Option<UiResponse> {
        let (route, params) = self.find(&req.method, &req.path)?;
        if !state.policy.serves(&req.path, route.pattern) {
            return None;
        }
        let mut ctx = UiCtx::new(state, req);
        ctx.params = params;
        if route.csrf && !ctx.csrf_ok() {
            return Some(UiResponse::forbidden());
        }
        Some((route.handler)(&ctx))
    }

    /// Would [`Self::dispatch`] serve this request itself?
    pub fn serves(&self, state: &UiState, method: &Method, path: &str) -> bool {
        self.find(method, path)
            .is_some_and(|(r, _)| state.policy.serves(path, r.pattern))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(_: &UiCtx) -> UiResponse {
        UiResponse::text(200, "ok")
    }
    fn named(ctx: &UiCtx) -> UiResponse {
        UiResponse::text(200, format!("name={}", ctx.param("name")))
    }
    fn rest(ctx: &UiCtx) -> UiResponse {
        UiResponse::text(200, format!("rest={}", ctx.param("rest")))
    }

    fn registry() -> Registry {
        let mut r = Registry::default();
        r.get("/loops/:name", named);
        r.get("/loops/gitlab-loop", ok);
        r.get("/files/*rest", rest);
        r.post("/p", ok);
        r
    }

    fn body(r: &Registry, method: Method, path: &str, st: &UiState) -> Option<String> {
        r.dispatch(st, &UiRequest::new(method, path))
            .map(|x| x.body_text())
    }

    #[test]
    fn literal_beats_param_and_params_decode() {
        let st = UiState::for_tests();
        let r = registry();
        assert_eq!(
            body(&r, Method::GET, "/loops/gitlab-loop", &st).unwrap(),
            "ok"
        );
        assert_eq!(
            body(&r, Method::GET, "/loops/a%20b", &st).unwrap(),
            "name=a b"
        );
        assert_eq!(
            body(&r, Method::GET, "/files/x/y.md", &st).unwrap(),
            "rest=x/y.md"
        );
        assert!(body(&r, Method::GET, "/loops/a/b", &st).is_none());
        assert!(body(&r, Method::GET, "/nope", &st).is_none());
    }

    #[test]
    fn post_without_a_token_is_forbidden() {
        let st = UiState::for_tests();
        let r = registry();
        let resp = r
            .dispatch(&st, &UiRequest::new(Method::POST, "/p"))
            .unwrap();
        assert_eq!(resp.status, 403);
        let req =
            UiRequest::new(Method::POST, "/p").with_body(format!("csrf_token={}", st.csrf_token));
        assert_eq!(r.dispatch(&st, &req).unwrap().status, 200);
    }

    #[test]
    #[should_panic(expected = "registered twice")]
    fn duplicate_routes_are_rejected() {
        let mut r = Registry::default();
        r.get("/a", ok);
        r.get("/a", ok);
    }

    #[test]
    fn flash_redirect_matches_python() {
        let r = UiResponse::flash_redirect(false, "No go & stuff", "/settings?tab=ai-cli");
        assert_eq!(r.status, 303);
        assert_eq!(
            r.header("Location"),
            Some("/settings?tab=ai-cli&flash=No%20go%20%26%20stuff&ok=0")
        );
    }
}
