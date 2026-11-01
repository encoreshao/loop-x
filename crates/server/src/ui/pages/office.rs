//! `GET /office/state`: the JSON the office view polls, one entry per loop.

use crate::ui::assets;
use crate::ui::components::{pill_bar, pill_icon_button, pill_icon_link, ICON_DASHBOARD, ICON_OFFICE};
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
    let body = match std::str::from_utf8(body) {
        Ok(text) if rest.ends_with(".js") => assets::versioned_brand_urls(text).into_bytes(),
        _ => body.to_vec(),
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
        body,
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
.chooser-mark{display:grid;place-items:center;margin:0 auto}\
.chooser-mark .brand-mark{width:96px;height:96px}\
.chooser-brand{display:flex;justify-content:center}.chooser-brand .brand-name-img{height:76px;width:auto}\
.chooser h1{margin:.6rem 0 0;font-size:clamp(2rem,5vw,2.9rem);font-weight:750;letter-spacing:-.02em;line-height:1.1}\
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
@keyframes chooser-assemble{from{opacity:0;transform:scale(.6) rotate(-12deg)}60%{opacity:1}to{opacity:1;transform:none}}\
@media(prefers-reduced-motion:no-preference){.chooser-mark .brand-mark{animation:chooser-assemble 1.1s cubic-bezier(.22,1,.36,1) both}}\
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
    let brand_name = format!(
        "<img class='brand-name-img on-light' src='{}' alt='LoopX'><img class='brand-name-img on-dark' src='{}' alt='LoopX'>",
        assets::brand_url("name-light.png"),
        assets::brand_url("name.png")
    );
    let body = format!(
        "{CHOOSER_CSS}<div class='chooser'><div class='chooser-lang'>{}</div><div class='chooser-inner'>\
<div><div class='chooser-mark'>{}</div><h1 class='chooser-brand'>{}</h1><p class='chooser-sub'>{}</p></div>\
<div class='chooser-grid'>{}{}</div><p class='chooser-hint'>{}</p></div></div>",
        crate::ui::shell::lang_switch_html(ctx.lang),
        crate::ui::consts::ds("_BRAND_MARK_ICON"),
        brand_name,
        h(&ctx.t("Choose your view")),
        card(
            "/office",
            ICON_OFFICE,
            ctx.t("Office"),
            ctx.t("A 3D office with a chat")
        ),
        card(
            "/?mode=dashboard",
            ICON_DASHBOARD,
            ctx.t("Dashboard"),
            ctx.t("Loops, runs and settings")
        ),
        h(&ctx.t("You can switch any time.")),
    );
    let html = render_shell(ctx, "LoopX · LoopX Engineering", "overview", "", &body, ShellOpts::default());
    UiResponse::html(with_html_class(html, "chooser-mode"))
}

const OFFICE_MODE_CSS: &str = "<style>\
html.office-mode .sidebar,html.office-mode .topbar,html.office-mode .app-bg,html.office-mode .update-banner{display:none!important}\
html.office-mode .content-area,html.office-mode .main-scroll,html.office-mode .wrap{margin:0;padding:0;max-width:none;border-radius:0;overflow:hidden}\
html.office-mode .ai-panel{top:0;right:0;bottom:0;border-radius:0;box-shadow:-1px 0 0 var(--md-outline-variant,#d0d0d0)}\
html.office-mode #office{position:fixed;top:0;left:0;bottom:0;right:0;height:auto;min-height:0;border-radius:0}\
html.office-mode.ai-panel-open #office{right:var(--ai-panel-width)}\
.pill-btn.office-chat-toggle{display:none}\
html.office-mode:not(.ai-panel-open) .pill-btn.office-chat-toggle{display:inline-flex}\
html.office-mode .office-panel{top:56px;max-height:calc(100% - 116px)}\
#office-list:not([hidden]){position:fixed;top:0;left:0;bottom:0;right:0;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:1rem;padding:1.5rem;text-align:center}\
html.office-mode.ai-panel-open #office-list:not([hidden]){right:var(--ai-panel-width)}\
.office-fallback-link{padding:.6rem 1.2rem;border-radius:999px;background:var(--md-primary);color:var(--md-on-primary);text-decoration:none}\
@media(max-width:760px){html.office-mode.ai-panel-open #office,html.office-mode.ai-panel-open #office-list:not([hidden]){right:0}}\
</style>";

/// Opens the chat on first visit; a width the person chose or a closed chat stays as it is.
const OFFICE_CHAT_OPEN_JS: &str = "<script>try{if(localStorage.getItem('loop-ai-panel-open')===null)document.documentElement.classList.add('ai-panel-open')}catch(e){document.documentElement.classList.add('ai-panel-open')}window.__loopNoReloadOnChange=true;</script>";

/// `GET /office`: the 3D office filling the window, with the assistant docked on the right
/// and no menu: the Dashboard and Open chat pills sit top right of the office view.
fn office_page(ctx: &UiCtx) -> UiResponse {
    let controls = pill_bar(&format!(
        "{}{}",
        pill_icon_link(
            "/?mode=dashboard",
            &ctx.t("Switch to Dashboard"),
            &crate::ui::icons::icon(ICON_DASHBOARD),
            "onclick=\"try{localStorage.removeItem('loop-ai-panel-open')}catch(e){}\""
        ),
        pill_icon_button(
            &ctx.t("Open chat"),
            &crate::ui::icons::icon("chat"),
            "office-chat-toggle",
            &format!("title=\"{} (⌘O)\" aria-keyshortcuts='Meta+O' data-office-chat-toggle", h(&ctx.t("Open chat")))
        )
    ));
    let view = office_view_html(ctx).replacen("</section>", &format!("{controls}</section>"), 1);
    // What shows when the scene cannot (no WebGL, a failed load): the way to the Dashboard.
    let fallback = format!(
        "<div id='office-list' hidden><a class='office-fallback-link' href='/?mode=dashboard' onclick=\"try{{localStorage.removeItem('loop-ai-panel-open')}}catch(e){{}}\">{}</a></div>",
        h(&ctx.t("Switch to Dashboard"))
    );
    let body = format!("{OFFICE_MODE_CSS}{OFFICE_CHAT_OPEN_JS}{view}{fallback}");
    let html = render_shell(ctx, "Office · LoopX Engineering", "office", "", &body, ShellOpts::default());
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
    let mut all = base_strings(ctx);
    for more in [assign_strings(ctx), toast_strings(ctx)] {
        all.as_object_mut().unwrap().extend(more.as_object().unwrap().clone());
    }
    all
}

/// The bottom-left notifications: a run starting, its step, its outcome.
fn toast_strings(ctx: &UiCtx) -> Value {
    json!({
        "toast_region": ctx.t("Loop notifications"),
        "toast_started": ctx.t("{loop} started a run"),
        "toast_step": ctx.t("{loop}: {step}"),
        "toast_done": ctx.t("{loop} finished its run"),
        "toast_failed": ctx.t("{loop} failed"),
        "toast_waiting": ctx.t("{loop} is waiting for your approval"),
        "toast_dismiss": ctx.t("Dismiss"),
    })
}

/// The colleague card's send-out and conversation controls, and the lines of an assigned chat.
fn assign_strings(ctx: &UiCtx) -> Value {
    json!({
        "send_out": ctx.t("Send out"),
        "call_back": ctx.t("Call back to desk"),
        "talk_about": ctx.t("Talk with a colleague"),
        "any_topic": ctx.t("Any topic…"),
        "chat_with": ctx.t("Chat with {name}"),
        "unavailable": ctx.t("{name} is busy or away right now"),
        "human_1": ctx.t("weekend plans"),
        "human_2": ctx.t("favourite food"),
        "human_3": ctx.t("a good movie"),
        "human_4": ctx.t("travel"),
        "human_5": ctx.t("music"),
        "human_6": ctx.t("sports"),
        "human_7": ctx.t("pets"),
        "human_8": ctx.t("the weather"),
        "talk_line_1": ctx.t("Hey, got any thoughts on {topic}?"),
        "talk_line_2": ctx.t("Hmm, {topic}... I would love to hear more."),
        "talk_line_3": ctx.t("Ha, that reminds me of something from last week."),
        "talk_line_4": ctx.t("We should chat about {topic} over coffee sometime.")
    })
}

fn base_strings(ctx: &UiCtx) -> Value {
    json!({
        "state_working": ctx.t("Working"),
        "state_waiting": ctx.t("Waiting for you"),
        "state_idle": ctx.t("Idle"),
        "state_failed": ctx.t("Failed"),
        "state_disabled": ctx.t("Disabled"),
        "run_now": ctx.t("Run now"),
        "enable": ctx.t("Enable"),
        "disable": ctx.t("Disable"),
        "stale": ctx.t("Showing last known state"),
        "time_of_day": ctx.t("Time of day"),
        "animate": ctx.t("Animate the office"),
        "animate_hint": ctx.t("Your system's Reduce Motion setting is on, so the office stays still. Turn this on to animate it anyway."),
        "auto": ctx.t("Auto"),
        "day": ctx.t("Day"),
        "dawn": ctx.t("Dawn"),
        "dusk": ctx.t("Dusk"),
        "night": ctx.t("Night"),
        "no_webgl": ctx.t("The office view needs WebGL, which this browser has turned off. Use the Dashboard instead."),
        "canvas_label": ctx.t("3D office: three colleagues sit around a round desk"),
        "loops": ctx.t("Loops"),
        "no_loops": ctx.t("No loops yet"),
        "answering": ctx.t("Answering your question"),
        "coffee_shop": ctx.t("Coffee shop"),
        "bookstore": ctx.t("Bookstore"),
        "at_coffee": ctx.t("At the coffee shop"),
        "at_books": ctx.t("At the bookstore"),
        "at_roam": ctx.t("Strolling round the office"),
        "topic_1": ctx.t("Should the daily review run earlier?"),
        "topic_2": ctx.t("Which loop is costing us the most time?"),
        "topic_3": ctx.t("Can we trust the inbox triage on its own yet?"),
        "topic_4": ctx.t("What should we automate next?"),
        "topic_5": ctx.t("Are any approvals piling up?"),
        "topic_failed": ctx.t("Why did {loop} fail last time?"),
        "topic_waiting": ctx.t("{loop} needs a decision. Who takes it?"),
        "topic_ok": ctx.t("{loop} ran cleanly. Can it run more often?"),
        "topic_disabled": ctx.t("Is {loop} still worth keeping switched off?"),
        "action_failed": ctx.t("Action failed"),
        "close": ctx.t("Close"),
        "reload": ctx.t("Reload the page and try again"),
        "context_lost": ctx.t("The 3D view stopped (graphics were reset). Reload the page."),
        "load_failed": ctx.t("The office view could not load. Use the Dashboard instead."),
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
.office-tod.office-motion{{right:auto;left:12px}}\
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
.office-panel-note{{margin:0 0 .8rem;color:#9aa1ad;font-size:.85rem}}\
.office-panel .office-panel-loops{{list-style:none;padding:0}}.office-panel-loop{{display:flex;flex-wrap:wrap;align-items:center;gap:.3rem .6rem;padding:.55rem 0;border-top:1px solid rgba(255,255,255,.1)}}.office-panel-loop .office-panel-state{{margin:0;font-size:.75rem}}.office-panel-loop .office-panel-actions{{flex-basis:100%;margin-top:.2rem}}\
.office-panel-empty{{margin:.3rem 0 0;color:#9aa1ad}}\
.office-panel h3{{margin:.8rem 0 .3rem;font-size:.8rem;color:#9aa1ad;font-weight:600}}\
.office-panel ul{{margin:0;padding-left:1.1rem}}.office-panel a{{color:#8ab4f8}}\
.office-panel-actions{{display:flex;flex-wrap:wrap;gap:.5rem;margin-top:1rem;align-items:center}}\
.office-panel-assign h3{{margin:.9rem 0 .3rem}}.office-panel-row{{display:flex;flex-wrap:wrap;gap:.5rem;margin:.3rem 0}}.office-panel-assign-msg{{margin:.3rem 0 0;color:#e8b04a;font-size:.8rem}}.office-panel-assign button{{font:inherit;font-size:.8rem;padding:.3rem .8rem;border:0;border-radius:999px;background:var(--primary,#1a73e8);color:#fff;cursor:pointer}}.office-panel-assign button[aria-disabled='true']{{opacity:.45;cursor:not-allowed}}\
.office-panel-actions button{{font:inherit;font-size:.8rem;padding:.3rem .8rem;border:0;border-radius:999px;background:var(--primary,#1a73e8);color:#fff;cursor:pointer}}\
.office-panel-actions button[data-act='toggle']{{background:rgba(255,255,255,.14);color:inherit}}\
.office-panel-actions button[aria-disabled='true']{{opacity:.45;cursor:not-allowed}}\
.office-panel button:focus-visible,.office-panel a:focus-visible,#office:focus-visible{{outline:2px solid #8ab4f8;outline-offset:2px}}\
/* The topic field and its suggestions, drawn like a macOS combo box and menu. */\
.office-combo{{position:relative}}\
.office-panel-topic{{width:100%;box-sizing:border-box;font:13px/1.3 -apple-system,BlinkMacSystemFont,'SF Pro Text',system-ui,sans-serif;padding:.38rem 2rem .38rem .6rem;border-radius:6px;border:.5px solid rgba(255,255,255,.18);background:rgba(255,255,255,.07);color:inherit;box-shadow:inset 0 .5px 0 rgba(255,255,255,.06);outline:none}}\
.office-panel-topic::placeholder{{color:rgba(235,235,245,.38)}}\
.office-panel-topic:focus{{box-shadow:0 0 0 3px rgba(10,132,255,.5);border-color:rgba(10,132,255,.9)}}\
.office-panel-assign .office-combo-toggle{{position:absolute;top:50%;right:4px;transform:translateY(-50%);display:flex;align-items:center;justify-content:center;width:20px;height:20px;padding:0;border:0;border-radius:5px;background:linear-gradient(#3b99fc,#0a84ff);color:#fff;box-shadow:0 .5px 1px rgba(0,0,0,.4),inset 0 .5px 0 rgba(255,255,255,.25);cursor:default}}\
.office-combo-toggle svg{{width:8px;height:12px}}\
.office-panel .office-menu{{position:fixed;z-index:40;box-sizing:border-box;max-height:240px;overflow:auto;margin:0;padding:5px;list-style:none;border-radius:10px;background:rgba(36,36,40,.82);-webkit-backdrop-filter:blur(30px) saturate(190%);backdrop-filter:blur(30px) saturate(190%);border:.5px solid rgba(255,255,255,.16);box-shadow:0 0 0 .5px rgba(0,0,0,.7),0 12px 36px rgba(0,0,0,.5);font:13px/1.25 -apple-system,BlinkMacSystemFont,'SF Pro Text',system-ui,sans-serif;color:rgba(255,255,255,.88);cursor:default;user-select:none;-webkit-user-select:none}}\
.office-panel .office-menu[hidden]{{display:none}}\
.office-menu-item{{position:relative;padding:3px 10px 3px 22px;border-radius:5px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}}\
.office-menu-item[data-checked='true']::before{{content:'\u{2713}';position:absolute;left:7px;font-weight:600}}\
.office-menu-item[aria-selected='true']{{background:#0a84ff;color:#fff}}\
@media(prefers-reduced-transparency:reduce){{.office-panel .office-menu{{background:#2a2a2e;-webkit-backdrop-filter:none;backdrop-filter:none}}}}\
.office-panel-error{{margin:.8rem 0 0;color:#ff8a80}}\
.office-panel[aria-busy='true']{{cursor:progress}}\
.office-stale{{position:absolute;left:50%;top:12px;transform:translateX(-50%);margin:0;padding:.25rem .8rem;border-radius:999px;background:rgba(120,70,0,.88);color:#fff;font-size:.8rem}}\
.office-stale[hidden]{{display:none}}\
/* Loop notifications, bottom left: newest at the bottom, at most ten, in the theme's colours. */\
.office-toasts{{position:absolute;left:12px;bottom:12px;z-index:30;display:flex;flex-direction:column;justify-content:flex-end;gap:8px;width:min(340px,calc(100% - 24px));max-height:calc(100% - 80px);margin:0;padding:0;list-style:none;overflow:hidden;pointer-events:none}}\
#office:has(.office-motion) .office-toasts{{bottom:52px}}\
.office-toast{{position:relative;display:flex;align-items:flex-start;gap:.6rem;box-sizing:border-box;padding:.6rem .6rem .6rem .75rem;border-radius:14px;border:1px solid color-mix(in srgb,var(--md-outline-variant,#888) 70%,transparent);border-left:3px solid var(--toast-tint);\
background:color-mix(in srgb,var(--md-surface-container-high,#2a2d34) 90%,transparent);color:var(--md-on-surface,#e6e8ee);-webkit-backdrop-filter:saturate(160%) blur(18px);backdrop-filter:saturate(160%) blur(18px);\
box-shadow:0 1px 2px rgba(0,0,0,.12),0 8px 24px rgba(0,0,0,.18);font-size:.85rem;line-height:1.35;pointer-events:auto;cursor:pointer;--toast-tint:var(--md-nav-accent,var(--md-primary,#8ab4f8));animation:office-toast-in .32s ease-out both}}\
.office-toast[data-kind='done']{{--toast-tint:var(--md-success,#3ddc84)}}\
.office-toast[data-kind='failed']{{--toast-tint:var(--md-error,#ff5252)}}\
.office-toast[data-kind='waiting']{{--toast-tint:var(--md-warning,#ffb020)}}\
.office-toast-dot{{flex:none;width:.55rem;height:.55rem;margin-top:.4rem;border-radius:50%;background:var(--toast-tint)}}\
.office-toast-body{{flex:1;min-width:0;overflow-wrap:anywhere}}\
.office-toast-title{{display:block;font-weight:600}}\
.office-toast-time{{display:block;margin-top:.1rem;font-size:.75rem;color:var(--md-on-surface-variant,#9aa1ad)}}\
.office-toast-close{{flex:none;display:grid;place-items:center;width:22px;height:22px;padding:0;border:0;border-radius:50%;background:none;color:var(--md-on-surface-variant,#9aa1ad);font:inherit;font-size:1rem;line-height:1;cursor:pointer}}\
.office-toast-close:hover{{background:color-mix(in srgb,var(--md-on-surface,#fff) 10%,transparent)}}\
.office-toast:focus-visible,.office-toast-close:focus-visible{{outline:2px solid var(--toast-tint);outline-offset:2px}}\
.office-toast.leaving{{animation:office-toast-out .24s ease-in both}}\
@keyframes office-toast-in{{from{{opacity:0}}to{{opacity:1}}}}\
@keyframes office-toast-out{{to{{opacity:0}}}}\
@media(prefers-reduced-motion:no-preference){{@keyframes office-toast-in{{from{{opacity:0;transform:translateY(8px)}}to{{opacity:1;transform:none}}}}}}\
@media(prefers-reduced-transparency:reduce){{.office-toast{{background:var(--md-surface-container-high,#2a2d34);-webkit-backdrop-filter:none;backdrop-filter:none}}}}\
</style>\
<noscript><style>#office{{display:none}}#office-list[hidden]{{display:block}}</style></noscript>\
<section id='office' aria-label=\"{label}\">\
<script type='importmap'>{{\"imports\":{{\"three\":\"/office/assets/three.module.min.js?v={version}\",\"three/addons/controls/OrbitControls.js\":\"/office/assets/OrbitControls.js?v={version}\"}}}}</script>\
<template id='office-panel'><aside class='office-panel' role='dialog' aria-label=\"{panel_label}\">\
<button type='button' class='office-panel-close' aria-label=\"{close}\">{close}</button>\
<h2 class='office-panel-name'></h2><p class='office-panel-state'></p>\
<p class='office-panel-note' hidden></p>\
<h3>{loops}</h3><ul class='office-panel-loops'></ul><p class='office-panel-empty' hidden>{no_loops}</p>\
<p class='office-panel-error' role='alert' hidden>{action_failed}</p></aside></template>\
<script type='application/json' id='office-i18n'>{i18n}</script>\
<input type='hidden' id='office-csrf' value=\"{token}\">\
<script type='module' src='/office/assets/office.js?v={version}' onerror=\"{load_failed_js}\"></script>\
</section>",
        label = h(&ctx.t("3D office: three colleagues sit around a round desk")),
        panel_label = h(&ctx.t("Colleague details")),
        close = h(&ctx.t("Close")),
        loops = h(&ctx.t("Loops")),
        no_loops = h(&ctx.t("No loops yet")),
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
            {"name": "issue-loop", "enabled": true},
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
        f.status("issue-loop", r#"{"state":"failed","updated_at":"2026-01-02T00:00:00Z"}"#);
        let doc = f.get(None);
        let names: Vec<&str> = doc["loops"].as_array().unwrap().iter().map(|l| l["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), 4, "{names:?}");
        assert_eq!(by(&doc, "busy-loop")["state"], "working");
        assert_eq!(by(&doc, "issue-loop")["state"], "failed");
        assert_eq!(by(&doc, "issue-loop")["last_run"], "2026-01-02T00:00:00Z");
        assert_eq!(by(&doc, "never-loop")["state"], "idle");
        assert!(by(&doc, "never-loop")["last_run"].is_null());
        assert_eq!(by(&doc, "busy-loop")["run_now_url"], "/loops/busy-loop/run-now");
        assert_eq!(by(&doc, "issue-loop")["run_now_url"], "/run-now");
        assert_eq!(by(&doc, "busy-loop")["toggle_url"], "/daemons/loops/busy-loop/disable");
        assert_eq!(by(&doc, "off-loop")["toggle_url"], "/daemons/loops/off-loop/enable");
        assert!(doc["generated_at"].is_string());
    }

    #[test]
    fn state_json_carries_the_running_step_for_the_notifications() {
        let f = fx(four());
        f.status("busy-loop", &format!(r#"{{"state":"running","pid":{},"current_step":"analyzing"}}"#, std::process::id()));
        f.status("issue-loop", r#"{"state":"failed","current_step":"left over"}"#);
        let doc = f.get(None);
        assert_eq!(by(&doc, "busy-loop")["step"], "analyzing");
        assert!(by(&doc, "issue-loop")["step"].is_null(), "only a working loop has a step");
        assert!(by(&doc, "never-loop")["step"].is_null());
    }

    #[test]
    fn office_page_has_the_bottom_left_notifications_in_the_theme_colours() {
        let f = fx(four());
        let body = page(&f, "/office", None, None).body_text();
        let at = body.find(".office-toasts{").expect("toast stack styled");
        let rule = &body[at..at + body[at..].find('}').unwrap()];
        assert!(rule.contains("left:12px") && rule.contains("bottom:12px"), "{rule}");
        // Theme tokens, never fixed colours, so the accent and light/dark mode carry over.
        let toast = &body[body.find(".office-toast{").expect("toast styled")..];
        let toast = &toast[..toast.find('}').unwrap()];
        assert!(toast.contains("var(--md-surface-container-high") && toast.contains("var(--md-on-surface"), "{toast}");
        assert!(toast.contains("--toast-tint:var(--md-nav-accent"), "a run's card wears the chosen accent: {toast}");
        assert!(body.contains("@keyframes office-toast-in"));
        let i18n_at = body.find("id='office-i18n'>").unwrap();
        for key in ["toast_started", "toast_step", "toast_done", "toast_failed", "toast_waiting", "toast_dismiss", "toast_region"] {
            assert!(body[i18n_at..].contains(&format!("\"{key}\":")), "{key}");
        }
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
        assert_eq!(by(&doc, "issue-loop")["state"], "waiting");
        assert_eq!(by(&doc, "issue-loop")["pending_approvals"], 1);
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
    fn office_art_loads_the_versioned_brand_images() {
        let f = fx(four());
        let art = page(&f, "/office/assets/art.js", None, None).body_text();
        assert!(art.contains(&assets::brand_url("mark.png")) && art.contains(&assets::brand_url("name-light.png")), "{art}");
        let v = assets::version_of("office/");
        assert_ne!(v, assets::hash_of(&["office/"]), "a new brand image must bust the cached office modules too");
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
        assert_eq!(doc["answering"], "Répond à votre question");
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
        // The LoopX logo and name, and the same mode icons the topbar and office use.
        assert!(body.contains("<img class='brand-mark'") && body.contains(&format!("src='{}'", assets::brand_url("name.png"))));
        assert!(body.contains("#i-office") && body.contains("#i-space_dashboard") && !body.contains("#i-apartment"));
        assert!(!body.contains("id='office'"));
    }

    #[test]
    fn chooser_logo_zooms_in_unless_motion_is_reduced() {
        let f = fx(four());
        let body = page(&f, "/?mode=choose", None, None).body_text();
        assert!(body.contains("@keyframes chooser-assemble"), "{body}");
        let at = body.find("@media(prefers-reduced-motion:no-preference){.chooser-mark").expect("gated animation");
        assert!(body[at..].contains("animation:chooser-assemble"));
        assert!(!body[..at].contains("animation:chooser-assemble"), "the animation must sit inside the media query");
    }

    #[test]
    fn brand_mark_and_name_are_the_brand_images() {
        for key in ["_BRAND_MARK_ICON", "_MESSAGE_BRAND_ICON"] {
            let mark = crate::ui::consts::ds(key);
            assert!(mark.starts_with("<img ") && mark.contains(&format!("src='{}'", assets::brand_url("mark.png"))), "{key}: {mark}");
        }
        let f = fx(four());
        let body = page(&f, "/?mode=choose", None, None).body_text();
        // One name image per background: white lettering for dark, ink lettering for light.
        assert!(
            body.contains(&format!("src='{}'", assets::brand_url("name.png")))
                && body.contains(&format!("src='{}'", assets::brand_url("name-light.png"))),
            "{body}"
        );
        let unversioned = body.match_indices("/brand/").any(|(i, _)| !body[i..].split(['\'', '"']).next().unwrap().contains("?v="));
        assert!(!unversioned, "every brand image is versioned: {body}");
        assert!(body.contains("alt='LoopX'"));
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
        assert!(sets_mode_cookie(&resp, "office"));
    }

    #[test]
    fn office_open_chat_is_the_dashboards_chat_icon_with_its_shortcut() {
        let f = fx(four());
        let body = page(&f, "/office", None, None).body_text();
        let start = body.find("class='pill-btn pill-btn-icon office-chat-toggle'").expect("icon pill");
        let pill = &body[start..start + body[start..].find("</button>").unwrap()];
        assert!(pill.contains(&crate::ui::icons::icon("chat")), "{pill}");
        assert!(pill.contains("aria-label=\"Open chat\"") && pill.contains("aria-keyshortcuts='Meta+O'"), "{pill}");
        assert!(pill.contains("title=\"Open chat (⌘O)\""), "{pill}");
    }

    #[test]
    fn office_card_always_sits_below_the_corner_pills() {
        // The Dashboard pill stays top right while the chat is open, so the card must
        // clear it in both states or it covers the card's close button.
        assert!(OFFICE_MODE_CSS.contains("html.office-mode .office-panel{top:56px"), "{OFFICE_MODE_CSS}");
        assert!(!OFFICE_MODE_CSS.contains(":not(.ai-panel-open) .office-panel"));
    }

    #[test]
    fn office_topic_suggestions_are_a_styled_menu_not_a_native_datalist() {
        let f = fx(four());
        let body = page(&f, "/office", None, None).body_text();
        assert!(body.contains(".office-panel .office-menu{"), "the Apple-style menu is styled");
        assert!(body.contains(".office-menu-item[aria-selected='true']"));
        let panel_js = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/office/panel.js")).unwrap();
        assert!(!panel_js.contains("datalist"), "the native datalist renders unstyled in WKWebView");
        assert!(panel_js.contains("role\", \"combobox\"") && panel_js.contains("\"listbox\""));
    }

    #[test]
    fn office_page_chat_text_is_translated() {
        let f = fx(four());
        let body = page(&f, "/office", None, Some("fr")).body_text();
        assert!(!body.contains("\"answering\":\"Answering your question\""), "{body}");
        let choose = page(&f, "/?mode=choose", None, Some("fr")).body_text();
        assert!(!choose.contains(">Choose how you want to work<"));
    }
}
