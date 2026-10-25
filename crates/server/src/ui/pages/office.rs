//! `GET /office/state`: the JSON the office view polls, one entry per loop.

use crate::ui::assets;
use crate::ui::office;
use crate::ui::pyfmt::escape as h;
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::shell::{render_shell, ShellOpts};
use serde_json::{json, Value};

pub fn register(r: &mut Registry) {
    r.get("/office", office_page);
    r.get("/office/state", state_json);
    r.get("/office/assets/*rest", asset);
}

/// A vendored file under `assets/office/`, by exact embedded name only.
fn asset(ctx: &UiCtx) -> UiResponse {
    let rest = ctx.param("rest");
    if rest.contains("..") || rest.contains('\\') || rest.contains('\0') || rest.starts_with('/') {
        return UiResponse::not_found(ctx.lang);
    }
    let Some(body) = assets::get(&format!("office/{rest}")) else {
        return UiResponse::not_found(ctx.lang);
    };
    let content_type = if rest.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else {
        "text/plain; charset=utf-8"
    };
    UiResponse {
        status: 200,
        headers: vec![
            ("Content-Type".to_string(), content_type.to_string()),
            ("Cache-Control".to_string(), "public, max-age=31536000, immutable".to_string()),
        ],
        body: body.to_vec(),
    }
}

fn cookie_value(ctx: &UiCtx, name: &str) -> Option<String> {
    ctx.req.header("cookie")?.split(';').find_map(|part| {
        let (n, v) = part.trim().split_once('=')?;
        (n == name).then(|| v.to_string())
    })
}

// ---- landing: Office or Dashboard ----------------------------------------------------------

const MODE_COOKIE: &str = "loop_mode";

/// Where `GET /` should go.
pub enum Landing {
    Dashboard,
    Office,
    Choose,
}

fn mode_of(v: &str) -> Option<Landing> {
    match v {
        "office" => Some(Landing::Office),
        "dashboard" => Some(Landing::Dashboard),
        "choose" => Some(Landing::Choose),
        _ => None,
    }
}

/// `?mode=` wins over the remembered `loop_mode` cookie; with neither, the chooser. Only a
/// bare `/` is a landing: `/?session=...` and the like (flash redirects) stay on the Dashboard.
pub fn landing(ctx: &UiCtx) -> Landing {
    if let Some(l) = ctx.query().first("mode").and_then(mode_of) {
        return l;
    }
    if !ctx.req.query.is_empty() {
        return Landing::Dashboard;
    }
    match cookie_value(ctx, MODE_COOKIE).as_deref().and_then(mode_of) {
        Some(Landing::Choose) | None => Landing::Choose,
        Some(l) => l,
    }
}

fn mode_cookie(mode: &str) -> (String, String) {
    ("Set-Cookie".to_string(), format!("{MODE_COOKIE}={mode}; Path=/; Max-Age=31536000; SameSite=Lax"))
}

/// The `Set-Cookie` header remembering an explicit `?mode=dashboard` choice.
pub fn dashboard_cookie_header(ctx: &UiCtx) -> Option<(String, String)> {
    (ctx.query().first("mode") == Some("dashboard")).then(|| mode_cookie("dashboard"))
}

/// Marks `<html>` with a class the stylesheet keys the page's chrome off.
fn with_html_class(html: String, class: &str) -> String {
    html.replacen("<html ", &format!("<html class=\"{class}\" "), 1)
}

const CHOOSER_CSS: &str = "<style>\
html.chooser-mode .sidebar,html.chooser-mode .topbar,html.chooser-mode .ai-panel,html.chooser-mode .app-bg,html.chooser-mode .update-banner{display:none!important}\
html.chooser-mode .content-area,html.chooser-mode .main-scroll,html.chooser-mode .wrap{margin:0;padding:0;max-width:none;border-radius:0;overflow:visible}\
.chooser{position:fixed;top:0;left:0;right:0;bottom:0;z-index:100;display:flex;overflow:auto;box-sizing:border-box;padding:2rem 1.25rem;color:var(--md-on-surface);\
background:radial-gradient(900px 520px at 50% 18%,color-mix(in srgb,var(--md-primary) 14%,transparent),transparent 70%),var(--md-surface-dim)}\
.chooser-inner{margin:auto;width:min(760px,100%);display:flex;flex-direction:column;align-items:center;gap:2.25rem;text-align:center}\
.chooser-lang{position:fixed;top:1rem;right:1.25rem;z-index:101}\
.chooser-lang button,.chooser-lang .icon{color:var(--md-on-surface-variant)}\
.chooser-mark{display:grid;place-items:center;width:64px;height:64px;margin:0 auto;border-radius:18px;background:linear-gradient(145deg,var(--md-primary),color-mix(in srgb,var(--md-primary) 55%,#7d5cff));color:var(--md-on-primary);box-shadow:0 10px 30px color-mix(in srgb,var(--md-primary) 38%,transparent)}\
.chooser-mark .icon{width:32px;height:32px}\
.chooser h1{margin:1.1rem 0 0;font-size:clamp(1.75rem,4vw,2.4rem);font-weight:650;letter-spacing:-.02em;line-height:1.15}\
.chooser-sub{margin:.55rem 0 0;font-size:1.05rem;color:var(--md-on-surface-variant)}\
.chooser-grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:1.25rem;width:100%}\
.chooser-card{display:flex;flex-direction:column;align-items:flex-start;gap:.5rem;box-sizing:border-box;padding:1.75rem;text-align:left;color:inherit;text-decoration:none;border-radius:28px;\
border:1px solid color-mix(in srgb,var(--md-outline-variant) 70%,transparent);\
background:color-mix(in srgb,var(--md-surface) 82%,transparent);-webkit-backdrop-filter:saturate(180%) blur(24px);backdrop-filter:saturate(180%) blur(24px);\
box-shadow:0 1px 2px rgba(0,0,0,.06),0 12px 32px rgba(0,0,0,.10)}\
.chooser-card:hover{border-color:color-mix(in srgb,var(--md-primary) 55%,transparent);box-shadow:0 2px 4px rgba(0,0,0,.08),0 18px 44px rgba(0,0,0,.16);text-decoration:none}\
.chooser-card:focus-visible{outline:3px solid color-mix(in srgb,var(--md-primary) 70%,transparent);outline-offset:3px;text-decoration:none}\
.chooser-card:active{transform:scale(.99)}\
.chooser-tile{display:grid;place-items:center;width:52px;height:52px;margin-bottom:.6rem;border-radius:15px;background:var(--md-primary-container);color:var(--md-on-primary-container)}\
.chooser-tile .icon{width:28px;height:28px}\
.chooser-card strong{font-size:1.3rem;font-weight:600;letter-spacing:-.01em}\
.chooser-card span.chooser-text{font-size:.98rem;line-height:1.45;color:var(--md-on-surface-variant)}\
.chooser-open{display:inline-flex;align-items:center;gap:.3rem;margin-top:auto;padding-top:1rem;font-weight:500;color:var(--md-primary)}\
.chooser-open .icon{width:18px;height:18px}\
.chooser-hint{margin:0;font-size:.9rem;color:var(--md-on-surface-variant)}\
@media(prefers-reduced-motion:no-preference){.chooser-card{transition:transform .18s ease,box-shadow .18s ease,border-color .18s ease}.chooser-card:hover{transform:translateY(-3px)}}\
@media(max-width:640px){.chooser-grid{grid-template-columns:1fr}.chooser-card{padding:1.4rem}}\
</style>";

/// The landing page: a centred pair of cards, one per mode.
pub fn chooser_response(ctx: &UiCtx) -> UiResponse {
    let open = ctx.t("Open");
    let card = |href: &str, icon: &str, title: String, text: String| {
        format!(
            "<a class='chooser-card' href='{href}'><span class='chooser-tile'>{}</span><strong>{}</strong>\
<span class='chooser-text'>{}</span><span class='chooser-open'>{}{}</span></a>",
            crate::ui::icons::icon(icon),
            h(&title),
            h(&text),
            h(&open),
            crate::ui::icons::icon("arrow_forward")
        )
    };
    let body = format!(
        "{CHOOSER_CSS}<div class='chooser'><div class='chooser-lang'>{}</div><div class='chooser-inner'>\
<div><div class='chooser-mark'>{}</div><h1>{}</h1><p class='chooser-sub'>{}</p></div>\
<div class='chooser-grid'>{}{}</div><p class='chooser-hint'>{}</p></div></div>",
        crate::ui::shell::lang_switch_html(ctx.lang),
        crate::ui::icons::icon("auto_awesome"),
        h(&ctx.t("Choose how you want to work")),
        h(&ctx.t("Pick a view to start with.")),
        card(
            "/office",
            "apartment",
            ctx.t("Office"),
            ctx.t("Your loops as people in a 3D office, with a chat beside it to talk to them.")
        ),
        card(
            "/?mode=dashboard",
            "space_dashboard",
            ctx.t("Dashboard"),
            ctx.t("The classic view: pages for loops, runs, insights and settings.")
        ),
        h(&ctx.t("You can switch at any time.")),
    );
    let html = render_shell(ctx, "Loop X · Loop X Engineering", "overview", "", &body, ShellOpts::default());
    UiResponse::html(with_html_class(html, "chooser-mode"))
}

const OFFICE_MODE_CSS: &str = "<style>\
html.office-mode .sidebar,html.office-mode .topbar,html.office-mode .app-bg,html.office-mode .update-banner{display:none!important}\
html.office-mode .content-area,html.office-mode .main-scroll,html.office-mode .wrap{margin:0;padding:0;max-width:none;border-radius:0;overflow:hidden}\
html.office-mode .ai-panel{top:0;right:0;bottom:0;border-radius:0;box-shadow:-1px 0 0 var(--md-outline-variant,#d0d0d0)}\
html.office-mode #office{position:fixed;top:0;left:0;bottom:0;right:0;height:auto;min-height:0;border-radius:0}\
html.office-mode.ai-panel-open #office{right:var(--ai-panel-width)}\
.office-chat-toggle{position:absolute;top:12px;right:12px;display:none;font:inherit;padding:.4rem .9rem;border:0;border-radius:999px;background:rgba(20,22,28,.78);color:#e6e8ee;cursor:pointer}\
html.office-mode:not(.ai-panel-open) .office-chat-toggle{display:block}\
html.office-mode:not(.ai-panel-open) .office-panel{top:56px}\
#office-list:not([hidden]){position:fixed;top:0;left:0;bottom:0;right:0;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:1rem;padding:1.5rem;text-align:center}\
html.office-mode.ai-panel-open #office-list:not([hidden]){right:var(--ai-panel-width)}\
.office-fallback-link{padding:.6rem 1.2rem;border-radius:999px;background:var(--md-primary);color:var(--md-on-primary);text-decoration:none}\
@media(max-width:760px){html.office-mode.ai-panel-open #office,html.office-mode.ai-panel-open #office-list:not([hidden]){right:0}}\
</style>";

/// Opens the chat on first visit; a width the person chose or a closed chat stays as it is.
const OFFICE_CHAT_OPEN_JS: &str = "<script>try{if(localStorage.getItem('loop-ai-panel-open')===null)document.documentElement.classList.add('ai-panel-open')}catch(e){document.documentElement.classList.add('ai-panel-open')}window.__loopNoReloadOnChange=true;</script>";

/// `GET /office`: the 3D office filling the window, with the assistant docked on the right
/// and no menu: switching modes lives in the chat header.
fn office_page(ctx: &UiCtx) -> UiResponse {
    let toggle = format!(
        "<button type='button' class='office-chat-toggle' data-office-chat-toggle aria-label=\"{0}\">{0}</button>",
        h(&ctx.t("Open chat"))
    );
    let view = office_view_html(ctx).replacen("</section>", &format!("{toggle}</section>"), 1);
    // What shows when the scene cannot (no WebGL, a failed load): the way to the Dashboard.
    let fallback = format!(
        "<div id='office-list' hidden><a class='office-fallback-link' href='/?mode=dashboard' onclick=\"try{{localStorage.removeItem('loop-ai-panel-open')}}catch(e){{}}\">{}</a></div>",
        h(&ctx.t("Switch to Dashboard"))
    );
    let body = format!("{OFFICE_MODE_CSS}{OFFICE_CHAT_OPEN_JS}{view}{fallback}");
    let mut html = render_shell(ctx, "Office · Loop X Engineering", "office", "", &body, ShellOpts::default());
    let switch = format!(
        "<a class='ai-panel-icon-btn' href='/?mode=dashboard' onclick=\"try{{localStorage.removeItem('loop-ai-panel-open')}}catch(e){{}}\" aria-label=\"{0}\" title=\"{0}\">{1}</a>",
        h(&ctx.t("Switch to Dashboard")),
        crate::ui::icons::icon("space_dashboard")
    );
    html = html.replacen(
        "<div class='ai-panel-header-actions'>",
        &format!("<div class='ai-panel-header-actions'>{switch}"),
        1,
    );
    let mut resp = UiResponse::html(with_html_class(html, "office-mode"));
    resp.headers.push(mode_cookie("office"));
    resp
}

/// JSON that is safe inside a `<script>` element.
fn script_json(v: &Value) -> String {
    serde_json::to_string(v)
        .unwrap_or_else(|_| "{}".to_string())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn client_strings(ctx: &UiCtx) -> Value {
    json!({
        "state_working": ctx.t("Working"),
        "state_waiting": ctx.t("Waiting for you"),
        "state_idle": ctx.t("Idle"),
        "state_failed": ctx.t("Failed"),
        "state_disabled": ctx.t("Disabled"),
        "disabled_standby": ctx.t("Disabled · standby"),
        "run_now": ctx.t("Run now"),
        "enable": ctx.t("Enable"),
        "disable": ctx.t("Disable"),
        "open_loop": ctx.t("Open loop page"),
        "pending_approvals": ctx.t("Pending approvals"),
        "stale": ctx.t("Showing last known state"),
        "time_of_day": ctx.t("Time of day"),
        "auto": ctx.t("Auto"),
        "day": ctx.t("Day"),
        "dusk": ctx.t("Dusk"),
        "night": ctx.t("Night"),
        "no_webgl": ctx.t("The office view needs WebGL, which this browser has turned off. Use the Dashboard instead."),
        "and_more": ctx.t("and {n} more"),
        "canvas_label": ctx.t("3D office: each loop is a person at a desk"),
        "last_run": ctx.t("Last run"),
        "next_run": ctx.t("Next run"),
        "run_history": ctx.t("Run history"),
        "why": ctx.t("Why"),
        "action_failed": ctx.t("Action failed"),
        "close": ctx.t("Close"),
        "reload": ctx.t("Reload the page and try again"),
        "context_lost": ctx.t("The 3D view stopped (graphics were reset). Reload the page."),
        "load_failed": ctx.t("The office view could not load. Use the Dashboard instead."),
        "chat_about": ctx.t("About the loop {name}: "),
    })
}

/// Runs when the entry module (or something it imports) fails to load: shows the List
/// with the translated note, as `office.js` itself does when WebGL is missing.
const LOAD_FAILED_JS: &str = "var r=document.getElementById('office'),l=document.getElementById('office-list'),p=document.createElement('p');p.className='office-note';p.setAttribute('role','status');try{p.textContent=JSON.parse(document.getElementById('office-i18n').textContent).load_failed||''}catch(e){}if(r)r.hidden=true;if(l){l.prepend(p);l.hidden=false}";

/// The Office view: the import map, the entry module, the panel template and the
/// translated strings the client script reads.
pub fn office_view_html(ctx: &UiCtx) -> String {
    // office.js passes its own ?v= on to the modules it imports.
    let version = assets::version_of("office/");
    let token = ctx.state.csrf_token.clone();
    format!(
        "<style>\
#office{{position:relative;height:calc(100vh - 220px);min-height:480px;border-radius:12px;overflow:hidden;background:#20232a}}\
#office canvas{{display:block;width:100%;height:100%}}\
.office-tod{{position:absolute;right:12px;bottom:12px;display:flex;gap:2px;padding:2px;border-radius:999px;background:rgba(20,22,28,.72)}}\
.office-tod button{{font:inherit;font-size:.8rem;padding:.2rem .7rem;border:0;border-radius:999px;background:none;color:#e6e8ee;cursor:pointer}}\
.office-tod button[aria-pressed='true']{{background:rgba(255,255,255,.18);color:#fff}}\
.office-note{{margin:0 0 1rem}}\
.office-panel{{position:absolute;top:12px;right:12px;max-height:calc(100% - 68px);width:min(300px,calc(100% - 24px));box-sizing:border-box;overflow:auto;padding:1rem;border-radius:12px;background:rgba(20,22,28,.94);color:#e6e8ee;font-size:.9rem;box-shadow:0 8px 28px rgba(0,0,0,.4)}}\
.office-panel-close{{position:absolute;top:.5rem;right:.5rem;font:inherit;font-size:.8rem;padding:.2rem .6rem;border:0;border-radius:999px;background:rgba(255,255,255,.12);color:inherit;cursor:pointer}}\
.office-panel-name{{margin:0 3.5rem .4rem 0;font-size:1.1rem;word-break:break-word}}\
.office-panel-state{{display:inline-block;margin:0 0 .8rem;padding:.15rem .7rem;border-radius:999px;background:rgba(255,255,255,.12);font-size:.8rem}}\
.office-panel-state::before{{content:'';display:inline-block;width:.55rem;height:.55rem;margin-right:.4rem;border-radius:50%;background:#7d8590}}\
.office-panel-state[data-state='working']::before{{background:#3ddc84}}\
.office-panel-state[data-state='waiting']::before{{background:#ffb020}}\
.office-panel-state[data-state='failed']::before{{background:#ff5252}}\
.office-panel-state[data-state='disabled']::before{{background:#6b7280}}\
.office-panel dl{{margin:0 0 .8rem}}.office-panel dt{{margin-top:.5rem;font-size:.75rem;color:#9aa1ad}}.office-panel dd{{margin:0;word-break:break-word}}\
.office-panel h3{{margin:.8rem 0 .3rem;font-size:.8rem;color:#9aa1ad;font-weight:600}}\
.office-panel ul{{margin:0;padding-left:1.1rem}}.office-panel a{{color:#8ab4f8}}\
.office-panel-approvals{{margin:0 0 .8rem}}\
.office-panel-actions{{display:flex;flex-wrap:wrap;gap:.5rem;margin-top:1rem;align-items:center}}\
.office-panel-actions button{{font:inherit;padding:.4rem .9rem;border:0;border-radius:999px;background:var(--primary,#1a73e8);color:#fff;cursor:pointer}}\
.office-panel-actions button[data-act='toggle']{{background:rgba(255,255,255,.14);color:inherit}}\
.office-panel-actions button[aria-disabled='true']{{opacity:.45;cursor:not-allowed}}\
.office-panel button:focus-visible,.office-panel a:focus-visible,#office:focus-visible{{outline:2px solid #8ab4f8;outline-offset:2px}}\
.office-panel-error{{margin:.8rem 0 0;color:#ff8a80}}\
.office-panel[aria-busy='true']{{cursor:progress}}\
.office-more{{position:absolute;left:12px;bottom:12px;margin:0;padding:.2rem .7rem;border-radius:999px;background:rgba(20,22,28,.72);color:#e6e8ee;font-size:.8rem}}\
.office-stale{{position:absolute;left:50%;top:12px;transform:translateX(-50%);margin:0;padding:.25rem .8rem;border-radius:999px;background:rgba(120,70,0,.88);color:#fff;font-size:.8rem}}\
.office-stale[hidden]{{display:none}}\
</style>\
<noscript><style>#office{{display:none}}#office-list[hidden]{{display:block}}</style></noscript>\
<section id='office' aria-label=\"{label}\">\
<script type='importmap'>{{\"imports\":{{\"three\":\"/office/assets/three.module.min.js?v={version}\",\"three/addons/controls/OrbitControls.js\":\"/office/assets/OrbitControls.js?v={version}\"}}}}</script>\
<template id='office-panel'><aside class='office-panel' role='dialog' aria-label=\"{panel_label}\">\
<button type='button' class='office-panel-close' aria-label=\"{close}\">{close}</button>\
<h2 class='office-panel-name'></h2><p class='office-panel-state'></p>\
<dl><dt>{why}</dt><dd class='office-panel-why'></dd>\
<dt>{last_run}</dt><dd class='office-panel-last'></dd>\
<dt>{next_run}</dt><dd class='office-panel-next'></dd></dl>\
<p class='office-panel-approvals' hidden><a data-act='approvals'></a></p>\
<div class='office-panel-history-block'><h3>{history}</h3><ul class='office-panel-history'></ul></div>\
<div class='office-panel-actions'><button type='button' data-act='run'>{run_now}</button>\
<button type='button' data-act='toggle'></button>\
<a data-act='open' class='office-panel-link'>{open_loop}</a></div>\
<p class='office-panel-error' role='alert' hidden>{action_failed}</p></aside></template>\
<script type='application/json' id='office-i18n'>{i18n}</script>\
<input type='hidden' id='office-csrf' value=\"{token}\">\
<script type='module' src='/office/assets/office.js?v={version}' onerror=\"{load_failed_js}\"></script>\
</section>",
        label = h(&ctx.t("3D office: each loop is a person at a desk")),
        panel_label = h(&ctx.t("Loop details")),
        close = h(&ctx.t("Close")),
        why = h(&ctx.t("Why")),
        last_run = h(&ctx.t("Last run")),
        next_run = h(&ctx.t("Next run")),
        history = h(&ctx.t("Run history")),
        run_now = h(&ctx.t("Run now")),
        open_loop = h(&ctx.t("Open loop page")),
        action_failed = h(&ctx.t("Action failed")),
        load_failed_js = h(LOAD_FAILED_JS),
        i18n = script_json(&client_strings(ctx)),
        token = h(&token),
        version = version,
    )
}

fn state_json(ctx: &UiCtx) -> UiResponse {
    UiResponse::json(200, &office::build_office_state(ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::routes::UiRequest;
    use crate::ui::state::UiState;
    use loopx_core::approvals::{self, NewApproval};
    use serde_json::{json, Value};

    struct Fx {
        dir: tempfile::TempDir,
        state: UiState,
    }

    fn fx(loops: Value) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("loops.json"), loops.to_string()).unwrap();
        let state = UiState::explicit(dir.path(), &home, dir.path(), dir.path(), "tok", None);
        Fx { dir, state }
    }

    impl Fx {
        fn status(&self, name: &str, doc: &str) {
            let p = self.state.status_path_for_loop(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, doc).unwrap();
        }

        fn approval(&self) {
            let new = NewApproval {
                kind: "merge_request".into(),
                run_id: "r".into(),
                project: "p".into(),
                issue_iid: Some(1),
                title: "t".into(),
                branch: "b".into(),
                target_branch: "main".into(),
                summary: "s".into(),
                timeout_seconds: 3600,
            };
            approvals::create(&self.dir.path().join("outputs").join("approvals"), &new, chrono::Utc::now()).unwrap();
        }

        fn get(&self, lang: Option<&str>) -> Value {
            let mut req = UiRequest::new("GET".parse().unwrap(), "/office/state");
            if let Some(l) = lang {
                req = req.with_header("accept-language", l);
            }
            let resp = Registry::with_pages().dispatch(&self.state, &req).expect("route");
            assert_eq!(resp.status, 200);
            serde_json::from_str(&resp.body_text()).unwrap()
        }
    }

    fn four() -> Value {
        json!([
            {"name": "gitlab-loop", "enabled": true},
            {"name": "off-loop", "enabled": false},
            {"name": "busy-loop", "enabled": true},
            {"name": "never-loop", "enabled": true},
        ])
    }

    fn by<'a>(doc: &'a Value, name: &str) -> &'a Value {
        doc["loops"].as_array().unwrap().iter().find(|l| l["name"] == name).unwrap()
    }

    fn running_now() -> String {
        format!(r#"{{"state":"running","pid":{},"updated_at":"2026-01-01T00:00:00Z"}}"#, std::process::id())
    }

    #[test]
    fn state_json_has_one_entry_per_registered_loop() {
        let f = fx(four());
        f.status("off-loop", r#"{"state":"running","pid":999999999}"#);
        f.status("busy-loop", &running_now());
        f.status("gitlab-loop", r#"{"state":"failed","updated_at":"2026-01-02T00:00:00Z"}"#);
        let doc = f.get(None);
        let names: Vec<&str> = doc["loops"].as_array().unwrap().iter().map(|l| l["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), 4, "{names:?}");
        assert_eq!(by(&doc, "busy-loop")["state"], "working");
        assert_eq!(by(&doc, "gitlab-loop")["state"], "failed");
        assert_eq!(by(&doc, "gitlab-loop")["last_run"], "2026-01-02T00:00:00Z");
        assert_eq!(by(&doc, "never-loop")["state"], "idle");
        assert!(by(&doc, "never-loop")["last_run"].is_null());
        assert_eq!(by(&doc, "busy-loop")["run_now_url"], "/loops/busy-loop/run-now");
        assert_eq!(by(&doc, "gitlab-loop")["run_now_url"], "/run-now");
        assert_eq!(by(&doc, "busy-loop")["toggle_url"], "/daemons/loops/busy-loop/disable");
        assert_eq!(by(&doc, "off-loop")["toggle_url"], "/daemons/loops/off-loop/enable");
        assert!(doc["generated_at"].is_string());
    }

    #[test]
    fn disabled_loop_with_running_status_is_disabled() {
        let f = fx(four());
        f.status("off-loop", &running_now());
        let doc = f.get(None);
        assert_eq!(by(&doc, "off-loop")["state"], "disabled");
        assert_eq!(by(&doc, "off-loop")["enabled"], false);
    }

    #[test]
    fn pending_approval_makes_gitlab_loop_waiting() {
        let f = fx(four());
        f.approval();
        let doc = f.get(None);
        assert_eq!(by(&doc, "gitlab-loop")["state"], "waiting");
        assert_eq!(by(&doc, "gitlab-loop")["pending_approvals"], 1);
        assert_eq!(by(&doc, "never-loop")["state"], "idle");
        assert_eq!(doc["pending_approvals"], 1);
    }

    #[test]
    fn pending_approval_without_gitlab_loop_changes_nobody_and_is_counted() {
        let f = fx(json!([{"name": "a-loop", "enabled": true}]));
        f.approval();
        let doc = f.get(None);
        assert_eq!(by(&doc, "a-loop")["state"], "idle");
        assert_eq!(by(&doc, "a-loop")["pending_approvals"], 0);
        assert_eq!(doc["pending_approvals"], 1);
    }

    #[test]
    fn corrupt_or_missing_status_is_idle() {
        let f = fx(four());
        f.status("busy-loop", "{not json");
        let doc = f.get(None);
        assert_eq!(by(&doc, "busy-loop")["state"], "idle");
        assert_eq!(by(&doc, "never-loop")["state"], "idle");
    }

    #[test]
    fn names_with_slashes_quotes_and_non_ascii_are_encoded_in_urls() {
        let f = fx(json!([{"name": "a/b'c é", "enabled": true}]));
        let doc = f.get(None);
        let l = by(&doc, "a/b'c é");
        assert_eq!(l["run_now_url"], "/loops/a%2Fb%27c%20%C3%A9/run-now");
        assert_eq!(l["toggle_url"], "/daemons/loops/a%2Fb%27c%20%C3%A9/disable");
    }

    #[test]
    fn available_loop_with_no_status_file_is_listed_as_disabled() {
        let f = fx(json!([{"name": "idle-one", "enabled": false}]));
        let doc = f.get(None);
        assert_eq!(doc["loops"].as_array().unwrap().len(), 1);
        assert_eq!(by(&doc, "idle-one")["state"], "disabled");
        assert!(by(&doc, "idle-one")["last_run"].is_null());
    }

    #[test]
    fn urls_for_hostile_names_are_percent_encoded() {
        let name = "a/b\"c<d é";
        let f = fx(json!([{"name": name, "enabled": true}]));
        let doc = f.get(None);
        let l = by(&doc, name);
        for key in ["run_now_url", "toggle_url"] {
            let u = l[key].as_str().unwrap();
            let tail = u.rsplit_once('/').unwrap().0;
            let seg = tail.rsplit_once('/').unwrap().1;
            assert!(!seg.contains(['"', '<', '/', ' ']) && seg.is_ascii(), "{u}");
            assert!(seg.contains("%2F") && seg.contains("%22") && seg.contains("%3C"), "{u}");
        }
        let toggle = l["toggle_url"].as_str().unwrap();
        let routed = Registry::with_pages().pattern_for(&"POST".parse().unwrap(), toggle);
        assert_eq!(routed, Some("/daemons/loops/:name/disable"));
    }

    #[test]
    fn empty_registry_gives_empty_list() {
        let f = fx(json!([]));
        let doc = f.get(None);
        assert_eq!(doc["loops"], json!([]));
        assert_eq!(doc["pending_approvals"], 0);
    }

    #[test]
    fn reason_is_translated() {
        let f = fx(four());
        assert_eq!(by(&f.get(None), "never-loop")["reason"], "Idle until the next run");
        assert_ne!(by(&f.get(Some("fr")), "never-loop")["reason"], "Idle until the next run");
    }

    fn get_asset(path: &str) -> UiResponse {
        let f = fx(json!([]));
        let req = UiRequest::get(path);
        Registry::with_pages().dispatch(&f.state, &req).expect("route")
    }

    #[test]
    fn serves_vendored_three_with_js_type_and_cache_header() {
        let resp = get_asset("/office/assets/three.module.min.js?v=1");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type(), Some("text/javascript; charset=utf-8"));
        assert_eq!(resp.header("cache-control"), Some("public, max-age=31536000, immutable"));
        assert!(resp.body.len() > 100_000);
        let orbit = get_asset("/office/assets/OrbitControls.js");
        assert_eq!(orbit.status, 200);
        assert_eq!(orbit.content_type(), Some("text/javascript; charset=utf-8"));
        let licence = get_asset("/office/assets/LICENSE-three.txt");
        assert_eq!(licence.status, 200);
        assert_eq!(licence.content_type(), Some("text/plain; charset=utf-8"));
    }

    #[test]
    fn unknown_asset_is_404() {
        assert_eq!(get_asset("/office/assets/nope.js").status, 404);
        assert_eq!(get_asset("/office/assets/three.module.js").status, 404);
    }

    #[test]
    fn path_traversal_is_404() {
        for p in [
            "/office/assets/..%2f..%2fCargo.toml",
            "/office/assets/%2e%2e/x",
            "/office/assets/a/../three.module.min.js",
            "/office/assets/..%5cthree.module.min.js",
            "/office/assets/%2fthree.module.min.js",
            "/office/assets/three.module.min.js%00",
        ] {
            assert_eq!(get_asset(p).status, 404, "{p}");
        }
    }

    #[test]
    fn loops_page_has_no_office_switch() {
        let f = fx(four());
        let body = page(&f, "/loops", None, None).body_text();
        assert!(!body.contains("view-toggle") && !body.contains("view=office") && !body.contains("id='office'"));
        assert!(body.contains("class='card loop-section'"));
    }

    #[test]
    fn office_script_version_covers_every_office_module() {
        let f = fx(four());
        let body = page(&f, "/office", None, None).body_text();
        let v = assets::version_of("office/");
        assert!(body.contains(&format!("/office/assets/office.js?v={v}")), "{body}");
        let entry_only = loopx_core::sha256::hex(&loopx_core::sha256::sha256(assets::get("office/office.js").unwrap()))[..8].to_string();
        assert_ne!(v, entry_only, "world.js etc. must bust the cache too");
        assert!(body.contains(&format!("/office/assets/three.module.min.js?v={v}")), "{body}");
        assert!(body.contains(&format!("/office/assets/OrbitControls.js?v={v}")), "{body}");
        assert!(body.contains("onerror=") && body.contains("office-list"), "module load failure falls back to the Dashboard link");
        assert!(body.contains("<div id='office-list' hidden>") && body.contains("href='/?mode=dashboard'"));
    }

    #[test]
    fn office_i18n_json_is_translated_per_language() {
        let f = fx(four());
        let body = page(&f, "/office", None, Some("fr")).body_text();
        let start = body.find("id='office-i18n'>").unwrap() + "id='office-i18n'>".len();
        let end = start + body[start..].find("</script>").unwrap();
        let raw = body[start..end].replace("\\u003c", "<").replace("\\u003e", ">").replace("\\u0026", "&");
        let doc: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(doc["run_now"], "Exécuter maintenant");
        assert_eq!(doc["and_more"], "et {n} de plus");
        let en = page(&f, "/office", None, None).body_text();
        assert!(en.contains("\"run_now\":\"Run now\""));
    }

    fn page(f: &Fx, path: &str, cookie: Option<&str>, lang: Option<&str>) -> UiResponse {
        let mut req = UiRequest::new("GET".parse().unwrap(), path);
        if let Some(c) = cookie {
            req = req.with_header("cookie", c);
        }
        if let Some(l) = lang {
            req = req.with_header("accept-language", l);
        }
        Registry::with_pages().dispatch(&f.state, &req).expect("route")
    }

    fn sets_mode_cookie(resp: &UiResponse, mode: &str) -> bool {
        resp.headers.iter().any(|(k, v)| {
            k == "Set-Cookie" && v.starts_with(&format!("loop_mode={mode};")) && v.contains("Max-Age=31536000") && v.contains("SameSite=Lax")
        })
    }

    #[test]
    fn landing_without_a_remembered_mode_is_the_chooser() {
        let f = fx(four());
        let resp = page(&f, "/", None, None);
        assert_eq!(resp.status, 200);
        let body = resp.body_text();
        assert!(body.contains("<html class=\"chooser-mode\""), "{body}");
        assert!(body.contains("href='/office'"));
        assert!(body.contains("href='/?mode=dashboard'"));
        assert!(!body.contains("id='office'"));
    }

    #[test]
    fn landing_follows_the_remembered_mode() {
        let f = fx(four());
        let to_office = page(&f, "/", Some("loop_mode=office"), None);
        assert_eq!(to_office.status, 302);
        assert_eq!(to_office.header("location"), Some("/office"));
        let dash = page(&f, "/", Some("loop_mode=dashboard"), None);
        assert_eq!(dash.status, 200);
        assert!(!dash.body_text().contains("chooser-mode"));
        assert!(dash.body_text().contains("class=\"sidebar\""));
    }

    #[test]
    fn mode_query_picks_remembers_or_reopens_the_chooser() {
        let f = fx(four());
        let dash = page(&f, "/?mode=dashboard", Some("loop_mode=office"), None);
        assert_eq!(dash.status, 200);
        assert!(sets_mode_cookie(&dash, "dashboard"));
        assert!(!dash.body_text().contains("chooser-mode"));
        let choose = page(&f, "/?mode=choose", Some("loop_mode=office"), None);
        assert_eq!(choose.status, 200);
        assert!(choose.body_text().contains("chooser-mode"));
    }

    #[test]
    fn office_page_is_the_scene_and_the_chat_with_nothing_else() {
        let f = fx(four());
        let resp = page(&f, "/office", None, None);
        assert_eq!(resp.status, 200);
        let body = resp.body_text();
        assert!(body.contains("<html class=\"office-mode\""), "{body}");
        assert!(body.contains("<section id='office'"));
        assert!(body.contains("<template id='office-panel'>"));
        assert!(body.contains("id='ai-panel'"), "the chat sits beside the scene");
        assert!(body.contains("href='/?mode=dashboard'"), "the switch lives in the chat header");
        assert!(body.contains("data-office-chat-toggle"));
        assert!(body.contains("\"chat_about\":\"About the loop {name}: \""));
        assert!(sets_mode_cookie(&resp, "office"));
    }

    #[test]
    fn office_page_chat_text_is_translated() {
        let f = fx(four());
        let body = page(&f, "/office", None, Some("fr")).body_text();
        assert!(!body.contains("\"chat_about\":\"About the loop"), "{body}");
        let choose = page(&f, "/?mode=choose", None, Some("fr")).body_text();
        assert!(!choose.contains(">Choose how you want to work<"));
    }
}
