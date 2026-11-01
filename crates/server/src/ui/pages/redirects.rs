//! The legacy dashboard paths that 301 to their hub or loop page
//! (`_LEGACY_REDIRECTS` and `/learnings`), plus `/favicon.ico` and the `/brand/*.png` images.

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
    r.get("/brand/*rest", brand_image);
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

/// The logo and name images (`assets/brand/*.png`, rendered by `bin/scripts/render_brand_assets.sh`).
/// Unversioned URLs, since they sit in static constants: a short cache picks up a new brand after an update.
fn brand_image(ctx: &UiCtx) -> UiResponse {
    let rest = ctx.param("rest");
    let plain = !rest.is_empty() && rest.ends_with(".png") && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    let Some(body) = plain.then(|| crate::ui::assets::get(&format!("brand/{rest}"))).flatten() else {
        return UiResponse::not_found(ctx.lang);
    };
    UiResponse {
        status: 200,
        headers: vec![
            ("Content-Type".into(), "image/png".into()),
            ("Cache-Control".into(), "public, max-age=3600".into()),
        ],
        body: body.to_vec(),
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
        assert_eq!(go("/gitlab").header("Location"), Some("/loops/issue-loop"));
        assert_eq!(go("/gitlab?x=1").header("Location"), Some("/loops/issue-loop?x=1"));
        // The issue loop's page under its old id.
        assert_eq!(
            go("/loops/gitlab-loop?view=tasks").header("Location"),
            Some("/loops/issue-loop?view=tasks")
        );
        assert_eq!(go("/logs?x=1").header("Location"), Some("/runs?view=logs&x=1"));
        assert_eq!(go("/learnings?x=1").header("Location"), Some("/insights?view=memory"));
        assert_eq!(go("/learnings").status, 301);
        assert_eq!(go("/favicon.ico").content_type(), Some("image/x-icon"));
    }

    #[test]
    fn brand_images_are_served_as_png_and_nothing_else_is() {
        let st = UiState::for_tests();
        let go = |p: &str| st.registry.dispatch(&st, &UiRequest::get(p)).unwrap();
        for name in ["mark.png", "name.png", "name-light.png"] {
            let resp = go(&format!("/brand/{name}"));
            assert_eq!(resp.status, 200, "{name}");
            assert_eq!(resp.content_type(), Some("image/png"), "{name}");
            assert!(resp.body.starts_with(b"\x89PNG"), "{name}");
        }
        assert_eq!(go(&crate::ui::assets::brand_url("mark.png")).status, 200);
        assert_eq!(go("/brand/nope.png").status, 404);
        assert_eq!(go("/brand/../constants.json").status, 404);
        assert_eq!(go("/brand/../favicon.ico").status, 404);
    }
}
