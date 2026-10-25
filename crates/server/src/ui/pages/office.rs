//! `GET /office/state`: the JSON the office view polls, one entry per loop.

use crate::ui::assets;
use crate::ui::office;
use crate::ui::pyfmt::escape as h;
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use serde_json::{json, Value};

pub fn register(r: &mut Registry) {
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

const VIEW_COOKIE: &str = "loop_view";

fn cookie_value(ctx: &UiCtx, name: &str) -> Option<String> {
    ctx.req.header("cookie")?.split(';').find_map(|part| {
        let (n, v) = part.trim().split_once('=')?;
        (n == name).then(|| v.to_string())
    })
}

/// The view named by `?view=`, if it is one of the two we know.
fn query_view(ctx: &UiCtx) -> Option<&'static str> {
    match ctx.query().first("view") {
        Some("office") => Some("office"),
        Some("list") => Some("list"),
        _ => None,
    }
}

/// Office when `?view=office`, else when the `loop_view=office` cookie is set;
/// `?view=list` always wins over the cookie.
pub fn wants_office(ctx: &UiCtx) -> bool {
    match query_view(ctx) {
        Some(v) => v == "office",
        None => cookie_value(ctx, VIEW_COOKIE).as_deref() == Some("office"),
    }
}

/// The `Set-Cookie` header remembering an explicit `?view=` choice for a year.
pub fn view_cookie_header(ctx: &UiCtx) -> Option<(String, String)> {
    query_view(ctx).map(|v| {
        (
            "Set-Cookie".to_string(),
            format!("{VIEW_COOKIE}={v}; Path=/; Max-Age=31536000; SameSite=Lax"),
        )
    })
}

/// The List | Office switch in the Loops header.
pub fn view_toggle_html(ctx: &UiCtx, office: bool) -> String {
    let link = |view: &str, label: String, active: bool| {
        format!(
            "<a href='/loops?view={view}'{}>{}</a>",
            if active { " aria-current='page'" } else { "" },
            h(&label)
        )
    };
    format!(
        "<style>\
.view-toggle{{display:inline-flex;gap:2px;padding:2px;border:1px solid var(--outline,#d0d0d0);border-radius:999px}}\
.view-toggle a{{padding:.3rem .9rem;border-radius:999px;text-decoration:none;font-size:.9rem;color:inherit}}\
.view-toggle a[aria-current='page']{{background:var(--primary,#1a73e8);color:#fff}}\
</style><nav class='view-toggle' aria-label=\"{}\">{}{}</nav>",
        h(&ctx.t("Loops view")),
        link("list", ctx.t("List"), !office),
        link("office", ctx.t("Office"), office)
    )
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
        "needs_you": ctx.t("Needs you"),
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
        "no_webgl": ctx.t("The office view needs WebGL, which this browser has turned off. Use the List view instead."),
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
        "load_failed": ctx.t("The office view could not load. Use the List view instead."),
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

    fn loops_page(f: &Fx, path: &str, cookie: Option<&str>, lang: Option<&str>) -> UiResponse {
        let mut req = UiRequest::new("GET".parse().unwrap(), path);
        if let Some(c) = cookie {
            req = req.with_header("cookie", c);
        }
        if let Some(l) = lang {
            req = req.with_header("accept-language", l);
        }
        let resp = Registry::with_pages().dispatch(&f.state, &req).expect("route");
        assert_eq!(resp.status, 200);
        resp
    }

    #[test]
    fn loops_page_defaults_to_list_and_has_the_toggle() {
        let f = fx(four());
        let body = loops_page(&f, "/loops", None, None).body_text();
        assert!(body.contains("href='/loops?view=list'"), "{body}");
        assert!(body.contains("href='/loops?view=office'"));
        assert!(body.contains("aria-current='page'"));
        assert!(body.contains("class='card loop-section'"));
    }

    #[test]
    fn view_office_query_renders_the_office_container_and_import_map() {
        let f = fx(four());
        let resp = loops_page(&f, "/loops?view=office", None, None);
        let body = resp.body_text();
        assert!(body.contains("<section id='office'"), "{body}");
        assert!(body.contains("<script type='importmap'>"));
        assert!(body.contains("/office/assets/three.module.min.js"));
        assert!(body.contains("/office/assets/office.js?v="));
        assert!(body.contains("<template id='office-panel'>"));
        assert!(body.contains("id='office-i18n'"));
        assert!(body.contains("id='office-csrf'"));
        let list = body.find("<div id='office-list' hidden>").expect("hidden List wrapper");
        assert!(body[list..].contains("class='card loop-section'"), "the List is the text alternative");
        assert!(body.contains("<noscript>"));
        assert!(resp.headers.iter().any(|(k, v)| k == "Set-Cookie" && v.starts_with("loop_view=office;") && v.contains("Max-Age=31536000") && v.contains("SameSite=Lax")));
    }

    #[test]
    fn office_script_version_covers_every_office_module() {
        let f = fx(four());
        let body = loops_page(&f, "/loops?view=office", None, None).body_text();
        let v = assets::version_of("office/");
        assert!(body.contains(&format!("/office/assets/office.js?v={v}")), "{body}");
        let entry_only = loopx_core::sha256::hex(&loopx_core::sha256::sha256(assets::get("office/office.js").unwrap()))[..8].to_string();
        assert_ne!(v, entry_only, "world.js etc. must bust the cache too");
        assert!(body.contains(&format!("/office/assets/three.module.min.js?v={v}")), "{body}");
        assert!(body.contains(&format!("/office/assets/OrbitControls.js?v={v}")), "{body}");
        assert!(body.contains("onerror=") && body.contains("office-list"), "module load failure falls back to the List");
    }

    #[test]
    fn cookie_loop_view_office_selects_office() {
        let f = fx(four());
        let body = loops_page(&f, "/loops", Some("loop_view=office"), None).body_text();
        assert!(body.contains("<section id='office'"));
    }

    #[test]
    fn view_list_query_beats_the_cookie() {
        let f = fx(four());
        let resp = loops_page(&f, "/loops?view=list", Some("loop_view=office"), None);
        assert!(!resp.body_text().contains("<section id='office'"));
        assert!(resp.headers.iter().any(|(k, v)| k == "Set-Cookie" && v.starts_with("loop_view=list;")));
    }

    #[test]
    fn office_container_absent_in_list_view() {
        let f = fx(four());
        let body = loops_page(&f, "/loops", None, None).body_text();
        assert!(!body.contains("id='office'") && !body.contains("importmap") && !body.contains("office-i18n"));
    }

    #[test]
    fn office_i18n_json_is_translated_per_language() {
        let f = fx(four());
        let body = loops_page(&f, "/loops?view=office", None, Some("fr")).body_text();
        let start = body.find("id='office-i18n'>").unwrap() + "id='office-i18n'>".len();
        let end = start + body[start..].find("</script>").unwrap();
        let raw = body[start..end].replace("\\u003c", "<").replace("\\u003e", ">").replace("\\u0026", "&");
        let doc: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(doc["run_now"], "Exécuter maintenant");
        assert_eq!(doc["and_more"], "et {n} de plus");
        let en = loops_page(&f, "/loops?view=office", None, None).body_text();
        assert!(en.contains("\"run_now\":\"Run now\""));
    }
}
