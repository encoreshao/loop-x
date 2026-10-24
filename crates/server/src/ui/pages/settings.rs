//! The Settings hub (`/settings`): views General (Appearance, Notifications, AI CLI,
//! Instructions tabs), Daemons (launchd agents and the loops registry), Skills and
//! Chat tools, plus the POST routes behind them. `/settings/fragment` and the
//! GitLab/bundle/tracked-project POSTs belong to the GitLab loop page.

#[path = "settings/daemons.rs"]
mod daemons;
#[path = "settings/general.rs"]
mod general;
#[path = "settings/posts.rs"]
mod posts;
#[path = "settings/skills.rs"]
mod skills;

use crate::ui::badge::default_badge;
use crate::ui::hub;
use crate::ui::routes::{Registry, UiCtx, UiResponse};

pub fn register(r: &mut Registry) {
    r.get("/settings", page);
    posts::register(r);
}

fn page(ctx: &UiCtx) -> UiResponse {
    let q = ctx.query();
    let (flash, ok) = ctx.flash();
    let view = q.first("view");
    let hub = hub::hub_by_key("settings").expect("settings hub");
    let current = hub::resolve_view(&hub, view).key.clone();
    let body = match current.as_str() {
        "daemons" => daemons::body(ctx, flash.as_deref(), ok),
        "skills" => skills::skills_body(ctx, flash.as_deref(), ok),
        "chat" => skills::chat_body(ctx, flash.as_deref(), ok),
        _ => general::body(ctx, flash.as_deref(), ok, q.first("tab").unwrap_or("")),
    };
    UiResponse::html(hub::render_hub_page(ctx, &hub, view, &body, &default_badge(ctx)))
}
