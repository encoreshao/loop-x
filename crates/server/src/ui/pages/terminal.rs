//! Terminal: `/terminal` is the terminal-style view of every loop step (the same
//! lines `loopx watch` prints, from `loopx_core::live_trace`). The page renders
//! the newest steps and polls `/terminal/lines` for newer ones.

use crate::ui::badge::default_badge;
use crate::ui::pyfmt;
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::shell::{render_shell, ShellOpts};
use loopx_core::live_trace::{self, TraceLine};
use serde_json::json;

const FIRST_LOAD: usize = 200;

pub fn register(r: &mut Registry) {
    r.get("/terminal", page);
    r.get("/terminal/lines", lines);
}

fn line_html(l: &TraceLine) -> String {
    format!(
        "<div class='term-line term-{}'><time>{}</time><b>{}</b><i aria-hidden='true'>{}</i><span>{}</span></div>",
        l.level.css(),
        pyfmt::escape(&l.time),
        pyfmt::escape(&l.scope),
        l.level.glyph(),
        pyfmt::escape(&l.text)
    )
}

fn running_html(running: &[String]) -> String {
    running
        .iter()
        .map(|r| format!("<div class='term-line term-run'><i aria-hidden='true'>▶</i><span>{}</span></div>", pyfmt::escape(r)))
        .collect()
}

fn page(ctx: &UiCtx) -> UiResponse {
    let state = ctx.state;
    let rows = live_trace::recent(&state.events_dir, FIRST_LOAD, None);
    let cursor = rows.last().map(|l| l.at.clone()).unwrap_or_default();
    let running = live_trace::running_now(&state.repo_root);
    let body_lines: String = rows.iter().map(line_html).collect();
    let empty = if rows.is_empty() {
        format!("<div class='term-empty'>{}</div>", pyfmt::escape(&ctx.t("No loop steps yet. They appear here as soon as a loop runs.")))
    } else {
        String::new()
    };
    let body = format!(
        "<div class='wrap'><div class='card term-card'>\
<div class='term-head'><div class='term-title'>{title}</div>\
<div class='term-live' id='term-live'><span class='term-dot'></span>{live}</div>\
<button type='button' class='term-btn' id='term-pause'>{pause}</button></div>\
<div class='term-now' id='term-now'>{now}</div>\
<div class='term' id='term' role='log' aria-live='polite' data-after='{cursor}' data-pause='{pause}' data-resume='{resume}' data-idle='{idle}'>{empty}{lines}</div>\
<div class='term-hint'>{hint}</div></div></div>{script}",
        title = pyfmt::escape(&ctx.t("Terminal")),
        live = pyfmt::escape(&ctx.t("Live")),
        pause = pyfmt::escape(&ctx.t("Pause")),
        resume = pyfmt::escape(&ctx.t("Resume")),
        idle = pyfmt::escape(&ctx.t("Nothing is running right now.")),
        now = if running.is_empty() {
            format!("<div class='term-line term-info'><span>{}</span></div>", pyfmt::escape(&ctx.t("Nothing is running right now.")))
        } else {
            running_html(&running)
        },
        cursor = pyfmt::escape(&cursor),
        empty = empty,
        lines = body_lines,
        hint = pyfmt::escape(&ctx.t("The same steps are available in a terminal with: loopx watch --follow")),
        script = SCRIPT,
    );
    UiResponse::html(render_shell(
        ctx,
        "Terminal · LoopX Engineering",
        "terminal",
        &default_badge(ctx),
        &body,
        ShellOpts::default(),
    ))
}

fn lines(ctx: &UiCtx) -> UiResponse {
    let state = ctx.state;
    let after = ctx.query().first("after").map(str::to_string).filter(|a| !a.is_empty());
    let rows = live_trace::recent(&state.events_dir, 500, after.as_deref());
    let running = live_trace::running_now(&state.repo_root);
    UiResponse::json(
        200,
        &json!({
            "after": rows.last().map(|l| l.at.clone()),
            "html": rows.iter().map(line_html).collect::<String>(),
            "running": running,
        }),
    )
}

const SCRIPT: &str = "<script>(function(){\
var t=document.getElementById('term');if(!t)return;\
var now=document.getElementById('term-now'),btn=document.getElementById('term-pause'),live=document.getElementById('term-live');\
var after=t.dataset.after||'',paused=false,stick=true;\
function bottom(){t.scrollTop=t.scrollHeight}bottom();\
t.addEventListener('scroll',function(){stick=t.scrollHeight-t.scrollTop-t.clientHeight<24});\
btn.addEventListener('click',function(){paused=!paused;btn.textContent=paused?t.dataset.resume:t.dataset.pause;live.classList.toggle('paused',paused)});\
function esc(s){return s.replace(/[&<>]/g,function(c){return{'&':'&amp;','<':'&lt;','>':'&gt;'}[c]})}\
function tick(){if(paused||document.hidden)return;\
fetch('/terminal/lines?after='+encodeURIComponent(after),{cache:'no-store'}).then(function(r){return r.json()}).then(function(d){\
if(d.html){var e=t.querySelector('.term-empty');if(e)e.remove();t.insertAdjacentHTML('beforeend',d.html);if(d.after)after=d.after;if(stick)bottom()}\
now.innerHTML=d.running.length?d.running.map(function(x){return\"<div class='term-line term-run'><i>\\u25b6</i><span>\"+esc(x)+'</span></div>'}).join(''):\"<div class='term-line term-info'><span>\"+esc(t.dataset.idle)+'</span></div>'\
}).catch(function(){})}\
setInterval(tick,1500)})();</script>";

#[cfg(test)]
mod tests {
    use crate::ui::routes::UiRequest;
    use crate::ui::state::UiState;
    use loopx_core::events::{emit, EventFields};

    #[test]
    fn page_lists_steps_and_lines_returns_only_newer_ones() {
        let d = tempfile::tempdir().unwrap();
        let st = UiState::explicit(d.path(), d.path(), d.path(), &d.path().join("update"), "tok", None)
            .with_events_dir(&d.path().join("events"));
        emit("run.started", "r1", EventFields::default(), Some(&d.path().join("events"))).unwrap();
        let page = st.registry.dispatch(&st, &UiRequest::get("/terminal")).unwrap();
        assert_eq!(page.status, 200);
        assert!(page.body_text().contains("run started"), "{}", page.body_text());
        let all: serde_json::Value = serde_json::from_str(
            &st.registry.dispatch(&st, &UiRequest::get("/terminal/lines")).unwrap().body_text(),
        )
        .unwrap();
        let cursor = all["after"].as_str().unwrap().to_string();
        let none = st
            .registry
            .dispatch(&st, &UiRequest::get(&format!("/terminal/lines?after={cursor}")))
            .unwrap();
        let none: serde_json::Value = serde_json::from_str(&none.body_text()).unwrap();
        assert_eq!(none["html"], "");
    }

    #[test]
    fn an_empty_ledger_says_so() {
        let d = tempfile::tempdir().unwrap();
        let st = UiState::explicit(d.path(), d.path(), d.path(), &d.path().join("update"), "tok", None)
            .with_events_dir(&d.path().join("events"));
        let page = st.registry.dispatch(&st, &UiRequest::get("/terminal")).unwrap();
        assert!(page.body_text().contains("No loop steps yet"));
    }
}
