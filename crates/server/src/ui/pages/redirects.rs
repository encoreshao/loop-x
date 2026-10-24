//! The legacy dashboard paths that 301 to their hub or loop page
//! (`_LEGACY_REDIRECTS` and `/learnings`), plus `/favicon.ico`.

use crate::ui::consts;
use crate::ui::routes::{Registry, UiCtx, UiResponse};

pub fn register(r: &mut Registry) {
    if let Some(table) = consts::value("dashboard_server", "_LEGACY_REDIRECTS").and_then(|v| v.as_object()) {
        for path in table.keys() {
            // The route table is built once at startup; leaking the few dozen
            // path strings keeps `&'static str` patterns simple.
            r.get(Box::leak(path.clone().into_boxed_str()), redirect);
        }
    }
    r.get("/learnings", learnings);
    r.get("/favicon.ico", favicon);
}

/// `legacy_redirect_target`: the target with the request's query appended.
fn redirect(ctx: &UiCtx) -> UiResponse {
    let target = consts::value("dashboard_server", "_LEGACY_REDIRECTS")
        .and_then(|t| t.get(&ctx.req.path))
        .and_then(|v| v.as_str())
        .unwrap_or("/");
    if ctx.req.query.is_empty() {
        return UiResponse::redirect(301, target);
    }
    let sep = if target.contains('?') { '&' } else { '?' };
    UiResponse::redirect(301, &format!("{target}{sep}{}", ctx.req.query))
}

fn learnings(_: &UiCtx) -> UiResponse {
    UiResponse::redirect(301, "/insights?view=memory")
}

fn favicon(_: &UiCtx) -> UiResponse {
    let icon = crate::ui::assets::favicon();
    if icon.is_empty() {
        return UiResponse::not_found(crate::ui::i18n::Lang::En);
    }
    UiResponse {
        status: 200,
        headers: vec![("Content-Type".into(), "image/x-icon".into())],
        body: icon.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::routes::UiRequest;
    use crate::ui::state::UiState;

    #[test]
    fn legacy_paths_redirect_and_keep_the_query() {
        let st = UiState::for_tests();
        let go = |p: &str| st.registry.dispatch(&st, &UiRequest::get(p)).unwrap();
        assert_eq!(go("/gitlab").header("Location"), Some("/loops/gitlab-loop"));
        assert_eq!(go("/gitlab?x=1").header("Location"), Some("/loops/gitlab-loop?x=1"));
        assert_eq!(go("/logs?x=1").header("Location"), Some("/runs?view=logs&x=1"));
        assert_eq!(go("/learnings?x=1").header("Location"), Some("/insights?view=memory"));
        assert_eq!(go("/learnings").status, 301);
        assert_eq!(go("/favicon.ico").content_type(), Some("image/x-icon"));
    }
}
