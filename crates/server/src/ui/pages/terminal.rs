//! Terminal: `/terminal` is the terminal-style view of every loop step (the same
//! lines `loopx watch` prints, from `loopx_core::live_trace`). The page renders
//! the newest steps and polls `/terminal/lines` for newer ones.

use crate::ui::badge::default_badge;
use crate::ui::pyfmt;
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::shell::{render_shell, ShellOpts};
use loopx_core::live_trace::{self, Filter, TraceLine};
use serde_json::json;

const FIRST_LOAD: usize = 200;

pub fn register(r: &mut Registry) {
    r.get("/terminal", page);
    r.get("/terminal/lines", lines);
}

fn line_html(l: &TraceLine) -> String {
    format!(
        "<div class='term-line term-{}' data-src='{}' data-scope='{}' data-lvl='{}'><time>{}</time><b>{}</b><i aria-hidden='true'>{}</i><span>{}</span></div>",
        l.level.css(),
        l.source.css(),
        pyfmt::escape(&l.scope),
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

/// Everything running now: each running loop or job (what the topbar island shows,
/// project profile jobs included, translated), then what each GitLab agent is doing.
fn running_lines(ctx: &UiCtx) -> Vec<String> {
    let mut out = super::queue::running_summaries(ctx);
    out.extend(live_trace::agents_running_now(&ctx.state.status_path_for_loop("issue-loop")));
    out
}

fn page(ctx: &UiCtx) -> UiResponse {
    let state = ctx.state;
    let rows = live_trace::recent(&state.events_dir, &state.status_path_for_loop("issue-loop"), FIRST_LOAD, None, &Filter::default());
    let cursor = rows.last().map(|l| l.at.clone()).unwrap_or_default();
    let running = running_lines(ctx);
    let body_lines: String = rows.iter().map(line_html).collect();
    let empty = if rows.is_empty() {
        format!("<div class='term-empty'>{}</div>", pyfmt::escape(&ctx.t("No loop steps yet. They appear here as soon as a loop runs.")))
    } else {
        String::new()
    };
    let body = format!(
        "<div class='card term-card'>\
<div class='term-head'><div class='term-title'>{title}</div>\
<div class='term-live' id='term-live'><span class='term-dot'></span>{live}</div>\
<button type='button' class='term-btn' id='term-pause'>{pause}</button></div>\
<div class='term-filters' id='term-filters'>\
<input type='search' class='term-input' id='term-q' placeholder='{ph}' aria-label='{ph}'>\
<select class='term-select' id='term-scope' aria-label='{scope_l}'><option value=''>{all_scopes}</option></select>\
<div class='term-seg' role='group' aria-label='{src_l}'>\
<button type='button' class='on' data-src=''>{all}</button><button type='button' data-src='step'>{steps}</button><button type='button' data-src='agent'>{agents}</button><button type='button' data-src='profile'>{profiles}</button></div>\
<button type='button' class='term-btn' id='term-attn' aria-pressed='false'>{attn}</button></div>\
<div class='term-now' id='term-now'>{now}</div>\
<div class='term' id='term' role='log' aria-live='polite' data-after='{cursor}' data-pause='{pause}' data-resume='{resume}' data-idle='{idle}'>{empty}{lines}</div>\
<div class='term-hint'>{hint}</div></div>{script}",
        ph = pyfmt::escape(&ctx.t("Filter steps")),
        scope_l = pyfmt::escape(&ctx.t("Loop or issue")),
        all_scopes = pyfmt::escape(&ctx.t("All loops and issues")),
        src_l = pyfmt::escape(&ctx.t("Show")),
        all = pyfmt::escape(&ctx.t("All")),
        steps = pyfmt::escape(&ctx.t("Steps")),
        agents = pyfmt::escape(&ctx.t("Agent tools")),
        profiles = pyfmt::escape(&ctx.t("Profile builds")),
        attn = pyfmt::escape(&ctx.t("Needs attention")),
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
    let rows = live_trace::recent(&state.events_dir, &state.status_path_for_loop("issue-loop"), 500, after.as_deref(), &Filter::default());
    let running = running_lines(ctx);
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
var after=t.dataset.after||'',paused=false,stick=true,fq='',fs='',fsrc='',fat=false;\
var q=document.getElementById('term-q'),sel=document.getElementById('term-scope'),attn=document.getElementById('term-attn'),seen={};\
function apply(){var n=fq.toLowerCase();t.querySelectorAll('.term-line').forEach(function(l){var ok=(!fs||l.dataset.scope===fs)&&(!fsrc||l.dataset.src===fsrc)&&(!fat||l.dataset.lvl==='warn'||l.dataset.lvl==='err')&&(!n||l.textContent.toLowerCase().indexOf(n)>-1);l.hidden=!ok});if(stick)bottom()}\
function scopes(){t.querySelectorAll('.term-line[data-scope]').forEach(function(l){var s=l.dataset.scope;if(s&&!seen[s]){seen[s]=1;var o=document.createElement('option');o.value=s;o.textContent=s;sel.appendChild(o)}})}\
q.addEventListener('input',function(){fq=q.value;apply()});sel.addEventListener('change',function(){fs=sel.value;apply()});\
attn.addEventListener('click',function(){fat=!fat;attn.setAttribute('aria-pressed',fat);attn.classList.toggle('on',fat);apply()});\
document.querySelectorAll('.term-seg button').forEach(function(b){b.addEventListener('click',function(){fsrc=b.dataset.src;document.querySelectorAll('.term-seg button').forEach(function(x){x.classList.toggle('on',x===b)});apply()})});\
function bottom(){t.scrollTop=t.scrollHeight}\
t.addEventListener('scroll',function(){stick=t.scrollHeight-t.scrollTop-t.clientHeight<24});\
btn.addEventListener('click',function(){paused=!paused;btn.textContent=paused?t.dataset.resume:t.dataset.pause;live.classList.toggle('paused',paused)});\
function esc(s){return s.replace(/[&<>]/g,function(c){return{'&':'&amp;','<':'&lt;','>':'&gt;'}[c]})}\
function tick(){if(paused||document.hidden)return;\
fetch('/terminal/lines?after='+encodeURIComponent(after),{cache:'no-store'}).then(function(r){return r.json()}).then(function(d){\
if(d.html){var e=t.querySelector('.term-empty');if(e)e.remove();t.insertAdjacentHTML('beforeend',d.html);if(d.after)after=d.after;scopes();apply()}\
now.innerHTML=d.running.length?d.running.map(function(x){return\"<div class='term-line term-run'><i>\\u25b6</i><span>\"+esc(x)+'</span></div>'}).join(''):\"<div class='term-line term-info'><span>\"+esc(t.dataset.idle)+'</span></div>'\
}).catch(function(){})}\
scopes();bottom();setInterval(tick,1500)})();</script>";

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
    fn a_running_loop_shows_in_the_running_strip_even_without_agent_activity() {
        let d = tempfile::tempdir().unwrap();
        let st = UiState::explicit(d.path(), d.path(), d.path(), &d.path().join("update"), "tok", None)
            .with_events_dir(&d.path().join("events"));
        std::fs::create_dir_all(d.path().join("outputs")).unwrap();
        std::fs::write(
            d.path().join("outputs/status.json"),
            r#"{"state":"running","current_issue":"web #7","current_step":"verifying"}"#,
        )
        .unwrap();
        let page = st.registry.dispatch(&st, &UiRequest::get("/terminal")).unwrap().body_text();
        let now = page.split("id='term-now'>").nth(1).unwrap().split("</div></div>").next().unwrap();
        assert!(now.contains("web #7") && !now.contains("Nothing is running"), "{now}");
        let lines: serde_json::Value =
            serde_json::from_str(&st.registry.dispatch(&st, &UiRequest::get("/terminal/lines")).unwrap().body_text()).unwrap();
        assert_eq!(lines["running"].as_array().unwrap().len(), 1, "{lines}");
    }

    #[test]
    fn a_running_profile_job_shows_once_translated_in_the_strip_and_its_steps_stream_in() {
        use loopx_core::profile_progress::{JobKind, ProfileJob, Progress, Step};
        let d = tempfile::tempdir().unwrap();
        let st = UiState::explicit(d.path(), d.path(), d.path(), &d.path().join("update"), "tok", None)
            .with_events_dir(&d.path().join("events"));
        let base = loopx_core::agent_activity::beside(&d.path().join("outputs/status.json"));
        let job = ProfileJob::start(&base, JobKind::Build, "web");
        job.step(3, 5, Step::Ai);
        let get = |path: &str, lang: &str| {
            st.registry
                .dispatch(&st, &UiRequest::get(path).with_header("cookie", &format!("loop_lang={lang}")))
                .unwrap()
                .body_text()
        };
        let page = get("/terminal", "en");
        let now = page.split("id='term-now'>").nth(1).unwrap().split("<div class='term' ").next().unwrap();
        assert_eq!(now.matches("Profile \u{b7} web").count(), 1, "{now}");
        assert!(now.contains("Profile \u{b7} web \u{2014} Asking the AI (3/5)"), "{now}");
        assert!(page.contains("data-src='profile'>Profile builds</button>"), "a filter for them");
        assert!(page.contains("data-src='profile' data-scope='Profile \u{b7} web'"), "{page}");
        assert!(page.contains("step 3/5: Asking the AI"), "{page}");
        let fr: serde_json::Value = serde_json::from_str(&get("/terminal/lines", "fr")).unwrap();
        assert_eq!(fr["running"], serde_json::json!(["Profil \u{b7} web \u{2014} Interrogation de l'IA (3/5)"]));
        job.fail("boom glpat-abcdefghij0123456789");
        drop(job);
        let after: serde_json::Value = serde_json::from_str(&get("/terminal/lines", "en")).unwrap();
        assert_eq!(after["running"], serde_json::json!([]));
        let html = after["html"].as_str().unwrap();
        assert!(html.contains("failed at step 3/5 (Asking the AI): details hidden"), "{html}");
        assert!(!html.contains("glpat"), "{html}");
    }

    #[test]
    fn a_build_whose_cli_could_not_start_says_so_in_the_trace() {
        use loopx_core::profile_progress::{JobKind, ProfileJob, Progress, Step};
        let d = tempfile::tempdir().unwrap();
        let st = UiState::explicit(d.path(), d.path(), d.path(), &d.path().join("update"), "tok", None)
            .with_events_dir(&d.path().join("events"));
        let base = loopx_core::agent_activity::beside(&d.path().join("outputs/status.json"));
        let job = ProfileJob::start(&base, JobKind::Build, "web");
        job.step(4, 5, Step::Ai);
        job.fail("FileNotFoundError");
        drop(job);
        let lines: serde_json::Value =
            serde_json::from_str(&st.registry.dispatch(&st, &UiRequest::get("/terminal/lines")).unwrap().body_text()).unwrap();
        let html = lines["html"].as_str().unwrap();
        assert!(
            html.contains("failed at step 4/5 (Asking the AI): the AI command could not be started \u{2014} check Settings \u{2192} AI CLI"),
            "{html}"
        );
        assert!(!html.contains("FileNotFoundError"), "{html}");
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
