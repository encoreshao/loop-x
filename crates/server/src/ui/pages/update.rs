//! `/update` and its fragments: the self-update progress page, the JSON the page
//! polls, and the POST route (`bin/web/update_pages.py`).

use crate::ui::badge::default_badge;
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::shell::{render_shell, ShellOpts};
use crate::ui::update;
use serde_json::json;

pub fn register(r: &mut Registry) {
    r.get("/update", page);
    r.get("/update/progress", progress);
    r.post("/update/check", check);
}

fn page(ctx: &UiCtx) -> UiResponse {
    let (status, progress) = update::read_both(&ctx.state.update_dir);
    let body = update::render_update_body(ctx.lang, &status, &progress);
    UiResponse::html(render_shell(
        ctx,
        "Update · Loop X Engineering",
        "update",
        &default_badge(ctx),
        &body,
        ShellOpts::default(),
    ))
}

fn progress(ctx: &UiCtx) -> UiResponse {
    let (status, progress) = update::read_both(&ctx.state.update_dir);
    UiResponse::json(200, &json!({"progress": progress, "status": status}))
}

fn check(ctx: &UiCtx) -> UiResponse {
    update::check(
        &ctx.state.repo_root,
        &ctx.state.update_dir,
        ctx.state.update_fetch,
        ctx.state.now(),
    );
    UiResponse::redirect(303, "/update")
}

#[cfg(test)]
mod tests {
    use crate::ui::routes::UiRequest;
    use crate::ui::state::UiState;
    use axum::http::Method;

    fn state(dir: &std::path::Path) -> UiState {
        UiState::explicit(dir, dir, dir, &dir.join("update"), "tok", None)
    }

    #[test]
    fn page_renders_and_progress_is_json() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let resp = st.registry.dispatch(&st, &UiRequest::get("/update")).unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body_text().contains("Loop X is up to date."));
        let p = st.registry.dispatch(&st, &UiRequest::get("/update/progress")).unwrap();
        assert_eq!(p.body_text(), r#"{"progress": {}, "status": {}}"#);
    }

    #[test]
    fn posts_need_the_csrf_token_and_redirect_back() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let bad = st.registry.dispatch(&st, &UiRequest::new(Method::POST, "/update/check")).unwrap();
        assert_eq!(bad.status, 403);
        let ok = st
            .registry
            .dispatch(&st, &UiRequest::new(Method::POST, "/update/check").with_body("csrf_token=tok"))
            .unwrap();
        assert_eq!((ok.status, ok.header("Location")), (303, Some("/update")));
        assert!(d.path().join("update").join("status.json").exists());
    }
}
