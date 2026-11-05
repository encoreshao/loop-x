//! `/creator`: video-scout runs (Discover), finished inspire-video shorts (Videos)
//! and their WeChat / Douyin publish copy (Publish), plus the media route that
//! serves a project's video and cover with byte ranges.

//!
//! Everything read from the skills' folders (titles, rationales, publish copy) is
//! untrusted: it is escaped for text and attribute contexts, sources are linked
//! only when they are `https://`, and no skill HTML is ever rendered. Jobs start
//! only from the POST routes here (CSRF checked by the registry).

use super::loops_common::flash_html;
use crate::ui::badge::default_badge;
use crate::ui::icons;
use crate::ui::pyfmt::{self, escape as h};
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::shell::{render_shell, ShellOpts};
use loopx_core::creator::jobs::{self, JobKind, JobRecord};
use loopx_core::creator::scout::{self, ScoutRun, Source};
use loopx_core::creator::spawn;
use loopx_core::creator::state::{self as cstate, CreatorState};
use loopx_core::creator::valid_source_url;
use loopx_core::creator::videos::{self, PublishCopy, VideoProject};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const TABS: [&str; 3] = ["discover", "videos", "jobs"];
/// A GET without `Range` larger than this is refused (players always send ranges).
const MAX_FULL_BYTES: u64 = 64 * 1024 * 1024;
/// The most one range response carries; an open or larger range is cut to this.
const MAX_RANGE_BYTES: u64 = 8 * 1024 * 1024;
/// Rows on the Jobs tab (newest first); older records stay on disk until deleted.
const JOBS_SHOWN: usize = 200;
/// What a scout job searches for when the topic field is left empty.
const OPEN_SCOUT_QUERY: &str =
    "topic-free discovery: a few of the most worthwhile recent public talks and interviews";
const PLATFORMS: [(&str, &str); 2] = [("wechat", "WeChat"), ("douyin", "Douyin")];

pub fn register(r: &mut Registry) {
    r.get("/creator", page);
    r.get("/creator/media/:slug/:file", media);
    r.get("/creator/jobs/:id/log", job_log);
    r.get("/creator/jobs/:id/status", job_status);
    r.post("/creator/jobs/delete", delete_job);
    r.post("/creator/jobs/clear", clear_jobs);
    r.post("/creator/generate", generate);
    r.post("/creator/scout", start_scout);
    r.post("/creator/stop", stop);
    r.post("/creator/folder", open_folder);
    r.post("/creator/approve", approve);
    r.post("/creator/published", published);
}

fn jobs_dir(ctx: &UiCtx) -> PathBuf {
    jobs::jobs_dir(&ctx.state.home_dir)
}

fn state_path(ctx: &UiCtx) -> PathBuf {
    cstate::state_path(&ctx.state.home_dir)
}

fn videos_root(ctx: &UiCtx) -> PathBuf {
    videos::videos_root(&ctx.state.user_home)
}

fn tab_of(requested: &str) -> &'static str {
    // The publish copy moved into each video's dialog on Videos.
    if requested == "publish" {
        return "videos";
    }
    TABS.iter()
        .find(|t| **t == requested)
        .copied()
        .unwrap_or("discover")
}

fn back(tab: &str) -> String {
    format!("/creator?tab={}", tab_of(tab))
}

/// Videos, with `slug`'s dialog reopened by the page script.
fn back_to_video(slug: &str) -> String {
    format!("/creator?tab=videos&open={}", pyfmt::quote(slug, ""))
}

fn https_link(url: &str) -> Option<&str> {
    url.starts_with("https://").then_some(url)
}

// ---- actions ---------------------------------------------------------------------------

fn generate(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let url = form.first("url").unwrap_or("").trim().to_string();
    let to = back("discover");
    let fail = |m: String| UiResponse::flash_redirect(false, &m, &to);
    if form.first("rights") != Some("1") {
        return fail(
            ctx.t(
                "Nothing started: tick \"I own this video or have permission to reuse it\" first",
            ),
        );
    }
    let url = match valid_source_url(&url) {
        Ok(u) => u,
        // The common case (a playlist or timestamp parameter) gets a translated hint.
        Err(_) if url.contains('&') => {
            return fail(ctx.t(
                "Nothing started: remove the parameters after the video id from the link (for example &list= or &t=)",
            ))
        }
        Err(e) => return fail(ctx.tf("Nothing started: {detail}", &[("detail", &e)])),
    };
    let dir = jobs_dir(ctx);
    if jobs::running(&jobs::list_jobs(&dir), JobKind::Generate, &url).is_some() {
        return fail(ctx.t("A job for this video is already running"));
    }
    if !ctx.state.spawn_processes {
        return fail(ctx.t("This server does not start agent jobs"));
    }
    match spawn::start_generate(&dir, &ctx.state.user_home, &url, true) {
        Ok(_) => UiResponse::flash_redirect(
            true,
            &ctx.t("Started generating the video. It appears under Videos when it is ready."),
            &to,
        ),
        Err(e) => fail(ctx.tf("Could not start the job: {detail}", &[("detail", &e)])),
    }
}

fn start_scout(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let typed = form.first("query").unwrap_or("").trim().to_string();
    let query = if typed.is_empty() {
        OPEN_SCOUT_QUERY.to_string()
    } else {
        typed
    };
    let to = back("discover");
    if !ctx.state.spawn_processes {
        return UiResponse::flash_redirect(
            false,
            &ctx.t("This server does not start agent jobs"),
            &to,
        );
    }
    match spawn::start_scout(&jobs_dir(ctx), &ctx.state.user_home, &query) {
        Ok(_) => UiResponse::flash_redirect(
            true,
            &ctx.t("Started looking for videos. The results appear here when the run finishes."),
            &to,
        ),
        Err(e) => UiResponse::flash_redirect(
            false,
            &ctx.tf("Could not start the job: {detail}", &[("detail", &e)]),
            &to,
        ),
    }
}

fn stop(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let to = back(form.first("tab").unwrap_or(""));
    match jobs::stop(&jobs_dir(ctx), form.first("id").unwrap_or("")) {
        Ok(()) => UiResponse::flash_redirect(true, &ctx.t("Job stopped"), &to),
        Err(e) => UiResponse::flash_redirect(
            false,
            &ctx.tf("Could not stop the job: {detail}", &[("detail", &e)]),
            &to,
        ),
    }
}

fn delete_job(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let to = back("jobs");
    match jobs::delete(&jobs_dir(ctx), form.first("id").unwrap_or("")) {
        Ok(()) => UiResponse::flash_redirect(true, &ctx.t("Job deleted"), &to),
        Err(e) => UiResponse::flash_redirect(
            false,
            &ctx.tf("Could not delete the job: {detail}", &[("detail", &e)]),
            &to,
        ),
    }
}

fn clear_jobs(ctx: &UiCtx) -> UiResponse {
    let n = jobs::delete_finished(&jobs_dir(ctx));
    UiResponse::flash_redirect(
        true,
        &ctx.tf("Deleted {count} finished jobs", &[("count", &n.to_string())]),
        &back("jobs"),
    )
}

/// The arguments `open` takes to show `path` in Finder: a file is revealed, a
/// folder is opened.
fn finder_args(path: &Path) -> Vec<String> {
    if path.is_dir() {
        vec![path.display().to_string()]
    } else {
        vec!["-R".to_string(), path.display().to_string()]
    }
}

#[cfg(target_os = "macos")]
fn open_in_finder(path: &Path) -> Result<(), String> {
    let status = std::process::Command::new("/usr/bin/open")
        .args(finder_args(path))
        .status()
        .map_err(|e| e.to_string())?;
    status.success().then_some(()).ok_or_else(|| format!("open exited with {status}"))
}

#[cfg(not(target_os = "macos"))]
fn open_in_finder(_path: &Path) -> Result<(), String> {
    Err("this needs a Mac".to_string())
}

/// Show a video's file in Finder. The path comes from the listed project, never from
/// the request.
fn open_folder(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let slug = form.first("slug").unwrap_or("");
    let to = back_to_video(slug);
    let projects = videos::list_projects(&videos_root(ctx)).0;
    let Some(p) = projects.iter().find(|p| p.slug == slug) else {
        return UiResponse::flash_redirect(false, &ctx.t("No such video"), &back("videos"));
    };
    if !ctx.state.spawn_processes {
        return UiResponse::flash_redirect(false, &ctx.t("This server cannot open folders"), &to);
    }
    let Some(target) = p.mp4.as_ref().or(p.cover.as_ref()) else {
        return UiResponse::flash_redirect(false, &ctx.t("Nothing to open yet"), &to);
    };
    match open_in_finder(target) {
        Ok(()) => UiResponse::flash_redirect(true, &ctx.t("Opened in Finder"), &to),
        Err(e) => UiResponse::flash_redirect(
            false,
            &ctx.tf("Could not open the folder: {detail}", &[("detail", &e)]),
            &to,
        ),
    }
}

/// The slug from the form when it names a listed project.
fn known_slug(ctx: &UiCtx, slug: &str) -> bool {
    videos::list_projects(&videos_root(ctx))
        .0
        .iter()
        .any(|p| p.slug == slug)
}

fn approve(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let slug = form.first("slug").unwrap_or("");
    let on = form.first("on") == Some("1");
    if !known_slug(ctx, slug) {
        return UiResponse::flash_redirect(false, &ctx.t("No such video"), &back("videos"));
    }
    let to = back_to_video(slug);
    match cstate::set_approved(&state_path(ctx), slug, on) {
        Ok(()) => {
            let msg = if on {
                ctx.t("Video approved")
            } else {
                ctx.t("Approval removed")
            };
            UiResponse::flash_redirect(true, &msg, &to)
        }
        Err(e) => UiResponse::flash_redirect(false, &e, &to),
    }
}

fn published(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let slug = form.first("slug").unwrap_or("");
    let platform = form.first("platform").unwrap_or("");
    let on = form.first("on") == Some("1");
    if !known_slug(ctx, slug) {
        return UiResponse::flash_redirect(false, &ctx.t("No such video"), &back("videos"));
    }
    let to = back_to_video(slug);
    if !PLATFORMS.iter().any(|(k, _)| *k == platform) {
        return UiResponse::flash_redirect(false, &ctx.t("Unknown platform"), &to);
    }
    match cstate::set_published(&state_path(ctx), slug, platform, on) {
        Ok(()) => {
            let msg = if on {
                ctx.t("Marked as published")
            } else {
                ctx.t("Marked as not published")
            };
            UiResponse::flash_redirect(true, &msg, &to)
        }
        Err(e) => UiResponse::flash_redirect(false, &e, &to),
    }
}

// ---- job log ---------------------------------------------------------------------------

/// A job's log, rendered step by step, as plain text (never rendered as HTML); 404 for an unsafe
/// or unknown job id.
fn job_log(ctx: &UiCtx) -> UiResponse {
    match jobs::log_view(&jobs_dir(ctx), ctx.param("id")) {
        Some(text) => UiResponse::bytes(
            200,
            "text/plain; charset=utf-8",
            &[
                ("X-Content-Type-Options", "nosniff"),
                ("Cache-Control", "no-store"),
            ],
            text.into_bytes(),
        ),
        None => UiResponse::not_found(ctx.lang),
    }
}

/// `{"state": ..., "line": ...}` for one job: what the live status rows poll.
fn job_status(ctx: &UiCtx) -> UiResponse {
    let dir = jobs_dir(ctx);
    let id = ctx.param("id");
    match (jobs::get(&dir, id), jobs::status_line(&dir, id)) {
        (Some(job), Some(line)) => UiResponse::bytes(
            200,
            "application/json",
            &[
                ("X-Content-Type-Options", "nosniff"),
                ("Cache-Control", "no-store"),
            ],
            serde_json::json!({
                "state": job.state,
                "line": line,
                "progress": progress_parts(ctx, &job).map(|(percent, text)| {
                    let (stage, total) = jobs::progress(&dir, id)
                        .map_or((0, loopx_core::creator::progress::STAGES.len()), |p| (p.stage, p.total));
                    serde_json::json!({"stage": stage, "total": total, "percent": percent, "text": text})
                }),
            })
            .to_string()
            .into_bytes(),
        ),
        _ => UiResponse::not_found(ctx.lang),
    }
}

// ---- media -----------------------------------------------------------------------------

fn media_type(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "mp4" => Some("video/mp4"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        _ => None,
    }
}

/// `Range: bytes=a-b`, `bytes=a-` or `bytes=-n` against a file of `len` bytes, as
/// an inclusive `(start, end)`. Anything else (several ranges, other units,
/// garbage, out of bounds) is `None`, answered with 416.
fn parse_range(header: &str, len: u64) -> Option<(u64, u64)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || len == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 {
            return None;
        }
        return Some((len - n.min(len), len - 1));
    }
    let start: u64 = a.parse().ok()?;
    let end = if b.is_empty() {
        len - 1
    } else {
        let e: u64 = b.parse().ok()?;
        if e < start {
            return None;
        }
        e.min(len - 1)
    };
    (start < len).then_some((start, end))
}

fn media(ctx: &UiCtx) -> UiResponse {
    let not_found = || UiResponse::not_found(ctx.lang);
    let Some(path) = videos::project_file(&videos_root(ctx), ctx.param("slug"), ctx.param("file"))
    else {
        return not_found();
    };
    let Some(content_type) = media_type(&path) else {
        return not_found();
    };
    let Ok(mut file) = std::fs::File::open(&path) else {
        return not_found();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let base = [
        ("Accept-Ranges", "bytes"),
        ("X-Content-Type-Options", "nosniff"),
        ("Cache-Control", "no-store"),
    ];
    let Some(range) = ctx.req.header("range") else {
        if len > MAX_FULL_BYTES {
            return UiResponse::bytes(
                413,
                "text/plain; charset=utf-8",
                &base,
                ctx.t("This file is too large to send whole; request a byte range")
                    .into_bytes(),
            );
        }
        let mut body = Vec::new();
        if file.read_to_end(&mut body).is_err() {
            return not_found();
        }
        return UiResponse::bytes(200, content_type, &base, body);
    };
    let Some((start, end)) = parse_range(range, len) else {
        let unsatisfied = format!("bytes */{len}");
        let mut headers = base.to_vec();
        headers.push(("Content-Range", &unsatisfied));
        return UiResponse::bytes(416, "text/plain; charset=utf-8", &headers, Vec::new());
    };
    let end = end.min(start + MAX_RANGE_BYTES - 1);
    let mut body = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err()
        || (&mut file)
            .take(end - start + 1)
            .read_to_end(&mut body)
            .is_err()
    {
        return not_found();
    }
    range_response(content_type, &base, start, len, body)
}

/// The 206 for `body` read at `start` of a `len`-byte file, or 416 when the read came
/// back empty (the file shrank after its length was taken).
fn range_response(
    content_type: &str,
    base: &[(&str, &str)],
    start: u64,
    len: u64,
    body: Vec<u8>,
) -> UiResponse {
    let mut headers = base.to_vec();
    if body.is_empty() {
        let unsatisfied = format!("bytes */{len}");
        headers.push(("Content-Range", &unsatisfied));
        return UiResponse::bytes(416, "text/plain; charset=utf-8", &headers, Vec::new());
    }
    let content_range = format!("bytes {start}-{}/{len}", start + body.len() as u64 - 1);
    headers.push(("Content-Range", &content_range));
    UiResponse::bytes(206, content_type, &headers, body)
}

// ---- rendering -------------------------------------------------------------------------

fn when(stamp: &str) -> String {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(stamp) {
        return t
            .with_timezone(&chrono::Utc)
            .format("%Y-%m-%d %H:%M UTC")
            .to_string();
    }
    match stamp.get(..16) {
        Some(head) if stamp.as_bytes().get(10) == Some(&b'T') => head.replace('T', " "),
        _ => stamp.to_string(),
    }
}

fn clock(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

fn joined(parts: &[String]) -> String {
    parts
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The page's own styles and its small dialog script (static files, no data).
const CREATOR_CSS: &str = include_str!("../../../assets/creator/creator.css");
const CREATOR_JS: &str = include_str!("../../../assets/creator/creator.js");
/// A caption longer than this is clamped with a Show more control.
const CLAMP_CHARS: usize = 160;
/// Segments shown before the rest fold into a disclosure.
const SEGMENTS_SHOWN: usize = 2;
const PLAY_GLYPH: &str =
    "<svg viewBox='0 0 24 24' aria-hidden='true' focusable='false'><path d='M8 5.5v13l11-6.5z'/></svg>";

fn subtitle(text: &str) -> String {
    format!("<p class='cr-muted'>{}</p>", h(text))
}

/// A titled group of the page: a heading row and a rounded surface below it.
fn section(title: &str, aside: &str, body: &str) -> String {
    format!(
        "<section class='cr-section'><div class='cr-section-head'><h2>{}</h2>{aside}</div>{body}</section>",
        h(title)
    )
}

fn tabs_html(ctx: &UiCtx, active: &str) -> String {
    let labels = [
        ("discover", ctx.t("Discover")),
        ("videos", ctx.t("Videos")),
        ("jobs", ctx.t("Jobs")),
    ];
    let links: String = labels
        .iter()
        .map(|(key, label)| {
            let on = *key == active;
            format!(
                "<a class='cr-segment{}' href='/creator?tab={key}'{}>{}</a>",
                if on { " is-active" } else { "" },
                if on { " aria-current='page'" } else { "" },
                h(label)
            )
        })
        .collect();
    format!(
        "<nav class='cr-segmented' aria-label='{}'>{links}</nav>",
        h(&ctx.t("Page sections"))
    )
}

fn pill(kind: &str, label: &str) -> String {
    format!("<span class='cr-pill cr-pill-{kind}'>{}</span>", h(label))
}

fn state_pill(ctx: &UiCtx, state: &str) -> String {
    match state {
        "running" => pill("accent", &ctx.t("Running")),
        "done" => pill("ok", &ctx.t("Done")),
        "failed" => pill("error", &ctx.t("Failed")),
        "stopped" => pill("neutral", &ctx.t("Stopped")),
        other => pill("neutral", other),
    }
}

fn stop_form(ctx: &UiCtx, job: &JobRecord, tab: &str) -> String {
    format!(
        "<form method='post' action='/creator/stop' class='cr-inline-form'>{}\
<input type='hidden' name='id' value=\"{}\"><input type='hidden' name='tab' value='{tab}'>\
<button type='submit' class='cr-btn cr-btn-plain'>{} {}</button></form>",
        ctx.csrf_input(),
        h(&job.id),
        icons::icon("stop"),
        h(&ctx.t("Stop"))
    )
}

fn log_link(ctx: &UiCtx, job: &JobRecord) -> String {
    format!(
        "<a class='cr-btn cr-btn-plain' href='/creator/jobs/{}/log' data-cr-log target='_blank' rel='noopener noreferrer'>{} {}</a>",
        h(&job.id),
        icons::icon("description"),
        h(&ctx.t("View log"))
    )
}

/// The one right-hand sheet that shows a job's log in the page; creator.js fills it
/// from the link's own href, so the link keeps working as a new-tab fallback.
fn log_sheet(ctx: &UiCtx) -> String {
    format!(
        "<dialog class='cr-sheet' id='cr-sheet' aria-labelledby='cr-sheet-title' \
data-failed-label=\"{}\" data-loading-label=\"{}\" data-empty-label=\"{}\">\
<div class='cr-sheet-inner'><header class='cr-sheet-head'><h2 id='cr-sheet-title'>{}</h2>\
<div class='cr-sheet-tools'>\
<button type='button' class='cr-icon-btn' data-cr-refresh aria-label=\"{}\">{}</button>\
<button type='button' class='cr-icon-btn' data-cr-close aria-label=\"{}\">{}</button></div></header>\
<pre class='cr-sheet-log' tabindex='0' aria-live='polite'></pre></div></dialog>",
        h(&ctx.t("Could not load the log.")),
        h(&ctx.t("Loading…")),
        h(&ctx.t("Waiting for the first step…")),
        h(&ctx.t("Job log")),
        h(&ctx.t("Refresh")),
        icons::icon("autorenew"),
        h(&ctx.t("Close")),
        icons::icon("close"),
    )
}

fn kind_label(ctx: &UiCtx, job: &JobRecord) -> String {
    match job.kind {
        JobKind::Scout => ctx.t("Find videos"),
        JobKind::Generate => ctx.t("Generate"),
    }
}

/// The translated name of stage `n` (1-based) of a video; literal strings so the
/// catalogs can be checked against the sources.
fn stage_label(ctx: &UiCtx, n: usize) -> String {
    match n {
        1 => ctx.t("Fetch the source"),
        2 => ctx.t("Probe and extract audio"),
        3 => ctx.t("Transcribe"),
        4 => ctx.t("Choose a segment and write the copy"),
        5 => ctx.t("Cut the clip"),
        6 => ctx.t("Build the render manifest"),
        7 => ctx.t("Render"),
        8 => ctx.t("Quality check"),
        _ => ctx.t("Write the publishing copy"),
    }
}

/// `(percent, text)` of a Generate job's progress: "Stage 3 of 9 · Transcribe", or
/// "Getting started" before the first recognised step.
fn progress_parts(ctx: &UiCtx, job: &JobRecord) -> Option<(u8, String)> {
    if job.kind != JobKind::Generate {
        return None;
    }
    Some(match jobs::progress(&jobs_dir(ctx), &job.id) {
        Some(p) => (
            p.percent,
            ctx.tf(
                "Stage {stage} of {total} · {label}",
                &[
                    ("stage", &p.stage.to_string()),
                    ("total", &p.total.to_string()),
                    ("label", &stage_label(ctx, p.stage)),
                ],
            ),
        ),
        None => (0, ctx.t("Getting started")),
    })
}

fn progress_html(ctx: &UiCtx, job: &JobRecord) -> String {
    let Some((percent, text)) = progress_parts(ctx, job).filter(|_| job.state == "running") else {
        return String::new();
    };
    format!(
        "<div class='cr-progress' data-cr-progress=\"{}\"><div class='cr-progress-head'>\
<span class='cr-progress-text'>{}</span><span class='cr-progress-pct'>{percent}%</span></div>\
<div class='cr-progress-bar' role='progressbar' aria-valuemin='0' aria-valuemax='100' aria-valuenow='{percent}' aria-label=\"{}\">\
<span style='width:{percent}%'></span></div></div>",
        h(&job.id),
        h(&text),
        h(&text)
    )
}

/// The current step of a running job. `creator.js` asks `/creator/jobs/<id>/status`
/// for it every couple of seconds and reloads the page when the job ends.
fn live_status(ctx: &UiCtx, job: &JobRecord) -> String {
    if job.state != "running" {
        return String::new();
    }
    let line = jobs::status_line(&jobs_dir(ctx), &job.id).unwrap_or_default();
    let empty = ctx.t("Starting…");
    format!(
        "<div class='cr-status' data-cr-status=\"{}\" data-empty-label=\"{}\" aria-live='polite'>{}</div>",
        h(&job.id),
        h(&empty),
        h(if line.is_empty() { &empty } else { &line })
    )
}

fn delete_form(ctx: &UiCtx, job: &JobRecord) -> String {
    format!(
        "<form method='post' action='/creator/jobs/delete' class='cr-inline-form'>{}\
<input type='hidden' name='id' value=\"{}\">\
<button type='submit' class='cr-btn cr-btn-plain' data-cr-confirm data-confirm-label=\"{}\">{} {}</button></form>",
        ctx.csrf_input(),
        h(&job.id),
        h(&ctx.t("Click again to confirm")),
        icons::icon("delete"),
        h(&ctx.t("Delete"))
    )
}

/// One job: its kind and state, what it ran on, its live step while it runs, and
/// View log with Stop (running) or Delete (finished).
fn job_row(ctx: &UiCtx, job: &JobRecord, tab: &str) -> String {
    let action = if job.state == "running" {
        stop_form(ctx, job, tab)
    } else {
        delete_form(ctx, job)
    };
    format!(
        "<li class='cr-row'><div class='cr-row-main'><div class='cr-row-title'><strong>{}</strong> {}</div>\
<div class='cr-meta'>{}</div>{}{}</div><div class='cr-row-actions'>{}{action}</div></li>",
        h(&kind_label(ctx, job)),
        state_pill(ctx, &job.state),
        h(&joined(&[job.key.clone(), when(&job.started_at)])),
        progress_html(ctx, job),
        live_status(ctx, job),
        log_link(ctx, job)
    )
}

/// Running jobs only (a row each, with its live step and Stop); nothing when none.
fn running_list(ctx: &UiCtx, title: &str, jobs: &[&JobRecord], tab: &str) -> String {
    if jobs.is_empty() {
        return String::new();
    }
    let rows: String = jobs.iter().map(|j| job_row(ctx, j, tab)).collect();
    section(title, "", &format!("<ul class='cr-list'>{rows}</ul>"))
}

/// The Jobs tab: every Find videos and Generate run, newest first.
fn jobs_tab(ctx: &UiCtx, jobs: &[JobRecord]) -> String {
    if jobs.is_empty() {
        return section(
            &ctx.t("Job history"),
            "",
            &format!(
                "<div class='cr-surface cr-empty'>{}</div>",
                subtitle(&ctx.t(
                    "No jobs yet. Find videos or generate a video, and every run shows up here."
                ))
            ),
        );
    }
    let rows: String = jobs
        .iter()
        .take(JOBS_SHOWN)
        .map(|j| job_row(ctx, j, "jobs"))
        .collect();
    let finished = jobs.iter().any(|j| j.state != "running");
    let clear = if finished {
        format!(
            "<form method='post' action='/creator/jobs/clear' class='cr-inline-form'>{}\
<button type='submit' class='cr-btn cr-btn-plain cr-btn-small' data-cr-confirm data-confirm-label=\"{}\">{} {}</button></form>",
            ctx.csrf_input(),
            h(&ctx.t("Click again to confirm")),
            icons::icon("delete"),
            h(&ctx.t("Clear finished"))
        )
    } else {
        String::new()
    };
    format!(
        "{}{}",
        subtitle(&ctx.t(
            "Every Find videos and Generate run, newest first. Deleting a record also removes its log."
        )),
        section(
            &ctx.t("Job history"),
            &clear,
            &format!("<ul class='cr-list'>{rows}</ul>")
        )
    )
}

fn errors_card(ctx: &UiCtx, errors: &[String]) -> String {
    if errors.is_empty() {
        return String::new();
    }
    let rows: String = errors.iter().map(|e| flash_html(Some(e), false)).collect();
    section(&ctx.t("Some files could not be read"), "", &rows)
}

/// The rights checkbox and Generate button. The button looks disabled until the
/// box is ticked (CSS `:has`), and `required` keeps the browser from posting
/// without it; the server checks `rights=1` again.
fn generate_form(ctx: &UiCtx, url: &str) -> String {
    format!(
        "<form method='post' action='/creator/generate' class='cr-item-actions'>{}\
<input type='hidden' name='url' value=\"{}\">\
<label class='cr-check'><input type='checkbox' name='rights' value='1' required>\
<span class='cr-check-box' aria-hidden='true'></span><span class='cr-check-label'>{}</span></label>\
<button type='submit' class='cr-btn cr-btn-primary cr-generate'>{} {}</button></form>",
        ctx.csrf_input(),
        h(url),
        h(&ctx.t("I own this video or have permission to reuse it")),
        icons::icon("video"),
        h(&ctx.t("Generate"))
    )
}

fn segment_li(seg: &scout::Segment) -> String {
    let span = match (&seg.start, &seg.end) {
        (Some(a), Some(b)) => format!("{a}–{b}"),
        (Some(a), None) => a.clone(),
        _ => String::new(),
    };
    let chip = if span.is_empty() {
        String::new()
    } else {
        format!("<span class='cr-chip'>{}</span>", h(&span))
    };
    format!("<li>{chip}<span>{}</span></li>", h(&seg.idea))
}

fn segments_html(ctx: &UiCtx, s: &Source) -> String {
    if s.segments.is_empty() {
        return String::new();
    }
    let first: String = s
        .segments
        .iter()
        .take(SEGMENTS_SHOWN)
        .map(segment_li)
        .collect();
    let rest: Vec<_> = s.segments.iter().skip(SEGMENTS_SHOWN).collect();
    let more = if rest.is_empty() {
        String::new()
    } else {
        let items: String = rest.iter().map(|seg| segment_li(seg)).collect();
        format!(
            "<details class='cr-more'><summary>{}</summary><ul class='cr-segments'>{items}</ul></details>",
            h(&ctx.tf(
                "More segments ({count})",
                &[("count", &rest.len().to_string())]
            ))
        )
    };
    format!(
        "<div class='cr-segments-block'><div class='cr-label'>{}</div><ul class='cr-segments'>{first}</ul>{more}</div>",
        h(&ctx.t("Suggested segments"))
    )
}

fn source_row(ctx: &UiCtx, s: &Source, jobs: &[JobRecord]) -> String {
    let title = if s.title.is_empty() {
        ctx.t("Untitled")
    } else {
        s.title.clone()
    };
    let title_html = match https_link(&s.url) {
        Some(u) => format!(
            "<a href=\"{}\" target='_blank' rel='noopener noreferrer'>{}</a>",
            h(u),
            h(&title)
        ),
        None => h(&title),
    };
    let quality = joined(&[
        s.grade.clone().unwrap_or_default(),
        s.score.map(|v| format!("{v:.1}")).unwrap_or_default(),
    ]);
    let mut badges = String::new();
    if !quality.is_empty() {
        badges.push_str(&format!("<span class='cr-badge'>{}</span>", h(&quality)));
    }
    if s.handoff_ready {
        badges.push_str(&pill("ok", &ctx.t("Ready to generate")));
    }
    let meta = joined(&[
        s.channel.clone(),
        s.platform.clone(),
        s.published.clone().unwrap_or_default(),
        s.duration_s.map(clock).unwrap_or_default(),
    ]);
    let rationale = if s.rationale.is_empty() {
        String::new()
    } else {
        format!("<p class='cr-rationale'>{}</p>", h(&s.rationale))
    };
    // Only a source whose address passes the job's own URL check can be generated.
    let usable = valid_source_url(&s.url).ok();
    let running = usable
        .as_deref()
        .and_then(|u| jobs::running(jobs, JobKind::Generate, u));
    let action = match (running, usable) {
        (Some(job), _) => format!(
            "<div class='cr-item-actions'>{}{}</div>",
            pill("accent", &ctx.t("Rendering")),
            stop_form(ctx, job, "discover")
        ),
        (None, Some(url)) => generate_form(ctx, &url),
        (None, None) => String::new(),
    };
    format!(
        "<li class='cr-item'><div class='cr-item-head'><span class='cr-rank'>{}</span>\
<div class='cr-item-main'><h3 class='cr-title'>{title_html}</h3><div class='cr-meta'>{}</div>\
<div class='cr-badges'>{badges}</div></div></div>{rationale}{}{action}</li>",
        s.rank,
        h(&meta),
        segments_html(ctx, s)
    )
}

fn run_section(ctx: &UiCtx, run: &ScoutRun, jobs: &[JobRecord]) -> String {
    let title = match run.query.as_deref() {
        Some(q) => ctx.tf("Search: {query}", &[("query", q)]),
        None => ctx.t("Topic-free discovery"),
    };
    let meta = format!(
        "<span class='cr-meta'>{}</span>",
        h(&joined(&[when(&run.generated_at), run.mode.clone()]))
    );
    let body = if run.sources.is_empty() {
        format!(
            "<div class='cr-surface cr-empty'>{}</div>",
            subtitle(&ctx.t("This run found no videos."))
        )
    } else {
        let items: String = run
            .sources
            .iter()
            .map(|s| source_row(ctx, s, jobs))
            .collect();
        format!("<ol class='cr-list cr-results'>{items}</ol>")
    };
    section(&title, &meta, &body)
}

fn discover_tab(ctx: &UiCtx, jobs: &[JobRecord]) -> String {
    let root = scout::scout_root(&ctx.state.user_home);
    let (runs, errors) = scout::list_runs(&root);
    let find = section(
        &ctx.t("Find videos"),
        "",
        &format!(
            "<div class='cr-surface'>{}<form method='post' action='/creator/scout' class='cr-search'>{}\
<input type='text' name='query' class='cr-input' placeholder=\"{}\" aria-label=\"{}\">\
<button type='submit' class='cr-btn cr-btn-primary'>{} {}</button></form></div>",
            subtitle(&ctx.t(
                "Runs the video-scout skill in the background. Leave the topic empty to let it pick worthwhile videos."
            )),
            ctx.csrf_input(),
            h(&ctx.t("Topic, for example leadership")),
            h(&ctx.t("Topic")),
            icons::icon("search"),
            h(&ctx.t("Find videos"))
        ),
    );
    let running: Vec<&JobRecord> = jobs
        .iter()
        .filter(|j| j.kind == JobKind::Scout && j.state == "running")
        .collect();
    let jobs_html = running_list(ctx, &ctx.t("Running jobs"), &running, "discover");
    let runs_html: String = if runs.is_empty() {
        section(
            &ctx.t("Scout runs"),
            "",
            &format!(
                "<div class='cr-surface cr-empty'>{}</div>",
                subtitle(&ctx.tf(
                    "No video-scout catalogs yet. Find videos above, or run the video-scout skill in Claude Code; catalogs in {path} show up here.",
                    &[("path", &root.display().to_string())],
                ))
            ),
        )
    } else {
        runs.iter().map(|r| run_section(ctx, r, jobs)).collect()
    };
    format!("{find}{jobs_html}{}{runs_html}", errors_card(ctx, &errors))
}

fn media_url(slug: &str, path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    h(&format!(
        "/creator/media/{}/{}",
        pyfmt::quote(slug, ""),
        pyfmt::quote(&name, "")
    ))
}

fn no_videos(ctx: &UiCtx, root: &Path) -> String {
    section(
        &ctx.t("Videos"),
        "",
        &format!(
            "<div class='cr-surface cr-empty'>{}</div>",
            subtitle(&ctx.tf(
                "No videos yet. Generate one from a source under Discover; finished videos in {path} show up here.",
                &[("path", &root.display().to_string())],
            ))
        ),
    )
}

/// Everything one video's poster and dialog need.
struct VideoView<'a> {
    p: &'a VideoProject,
    dom_id: String,
    approved: bool,
    rendering: Option<&'a JobRecord>,
    /// The newest generate job for this video's source, for its log.
    last_job: Option<&'a JobRecord>,
    status: String,
    status_kind: &'static str,
}

fn video_view<'a>(
    ctx: &UiCtx,
    i: usize,
    p: &'a VideoProject,
    st: &CreatorState,
    jobs: &'a [JobRecord],
) -> VideoView<'a> {
    let key = p
        .source_url
        .as_deref()
        .and_then(|u| valid_source_url(u).ok());
    let rendering = key
        .as_deref()
        .and_then(|u| jobs::running(jobs, JobKind::Generate, u));
    let last_job = key.as_deref().and_then(|u| {
        jobs.iter()
            .find(|j| j.kind == JobKind::Generate && j.key == u)
    });
    let approved = st.approved.contains(&p.slug);
    let (status_kind, status) = if rendering.is_some() {
        ("accent", ctx.t("Rendering"))
    } else if approved {
        ("ok", ctx.t("Approved"))
    } else if p.mp4.is_some() {
        ("neutral", ctx.t("Ready for review"))
    } else {
        ("warn", ctx.t("No video file"))
    };
    VideoView {
        p,
        dom_id: format!("cr-video-{i}"),
        approved,
        rendering,
        last_job,
        status,
        status_kind,
    }
}

fn poster_card(ctx: &UiCtx, v: &VideoView<'_>) -> String {
    let image = match &v.p.cover {
        Some(c) => format!(
            "<img src=\"{}\" alt='' loading='lazy' decoding='async'>",
            media_url(&v.p.slug, c)
        ),
        None => format!(
            "<span class='cr-poster-blank'>{}</span>",
            icons::icon("video")
        ),
    };
    let play = if v.p.mp4.is_some() {
        format!("<span class='cr-play'>{PLAY_GLYPH}</span>")
    } else {
        String::new()
    };
    let tools = {
        let share = video_share(ctx, v, true);
        let folder = folder_form(ctx, v, true);
        if share.is_empty() && folder.is_empty() {
            String::new()
        } else {
            format!("<div class='cr-poster-tools'>{share}{folder}</div>")
        }
    };
    format!(
        "<li><button type='button' class='cr-poster' data-cr-open='{id}' aria-haspopup='dialog' aria-label=\"{}\">\
<span class='cr-poster-media'>{image}{play}<span class='cr-poster-status'>{}</span></span>\
<span class='cr-poster-title'>{}</span></button>{tools}</li>",
        h(&ctx.tf("Open {title}", &[("title", &v.p.title)])),
        pill(v.status_kind, &v.status),
        h(&v.p.title),
        id = v.dom_id
    )
}

fn approve_form(ctx: &UiCtx, v: &VideoView<'_>) -> String {
    if !(v.approved || (v.rendering.is_none() && v.p.mp4.is_some())) {
        return String::new();
    }
    let (on, label, icon, class) = if v.approved {
        ("0", ctx.t("Remove approval"), "close", "cr-btn-plain")
    } else {
        ("1", ctx.t("Approve"), "check_circle", "cr-btn-primary")
    };
    format!(
        "<form method='post' action='/creator/approve' class='cr-inline-form'>{}\
<input type='hidden' name='slug' value=\"{}\"><input type='hidden' name='on' value='{on}'>\
<button type='submit' class='cr-btn {class}'>{} {}</button></form>",
        ctx.csrf_input(),
        h(&v.p.slug),
        icons::icon(icon),
        h(&label)
    )
}

/// A Share button. Hidden until `creator.js` finds a way to share (the Mac app's
/// share sheet, or the browser's): `path` is the video file for the native sheet,
/// `url` the same file over HTTP for the browser, `text` the caption or title that
/// goes with it. `kind` is `video` or `caption`.
fn share_button(
    ctx: &UiCtx,
    kind: &str,
    text: &str,
    file: Option<(&Path, String)>,
    icon_only: bool,
) -> String {
    let file_attrs = file
        .map(|(path, url)| {
            format!(
                " data-share-path=\"{}\" data-share-url=\"{}\"",
                h(&path.display().to_string()),
                h(&url)
            )
        })
        .unwrap_or_default();
    let label = ctx.t("Share");
    if icon_only {
        format!(
            "<button type='button' class='cr-icon-btn' data-cr-share data-share-kind='{kind}' data-share-text=\"{}\"{file_attrs} aria-label=\"{}\" title=\"{}\" hidden>{}</button>",
            h(text),
            h(&label),
            h(&label),
            icons::icon("send")
        )
    } else {
        format!(
            "<button type='button' class='cr-btn cr-btn-plain cr-btn-small' data-cr-share data-share-kind='{kind}' data-share-text=\"{}\"{file_attrs} hidden>{} {}</button>",
            h(text),
            icons::icon("send"),
            h(&label)
        )
    }
}

/// Open the video's folder in Finder (a POST, so the page cannot be made to do it
/// by a link).
fn folder_form(ctx: &UiCtx, v: &VideoView<'_>, icon_only: bool) -> String {
    if v.p.mp4.is_none() && v.p.cover.is_none() {
        return String::new();
    }
    let label = ctx.t("Open folder");
    let button = if icon_only {
        format!(
            "<button type='submit' class='cr-icon-btn' aria-label=\"{}\" title=\"{}\">{}</button>",
            h(&label),
            h(&label),
            icons::icon("folder")
        )
    } else {
        format!(
            "<button type='submit' class='cr-btn cr-btn-plain cr-btn-small'>{} {}</button>",
            icons::icon("folder"),
            h(&label)
        )
    };
    format!(
        "<form method='post' action='/creator/folder' class='cr-inline-form'>{}\
<input type='hidden' name='slug' value=\"{}\">{button}</form>",
        ctx.csrf_input(),
        h(&v.p.slug)
    )
}

/// The video's Share button (a video file is needed to have anything to share).
fn video_share(ctx: &UiCtx, v: &VideoView<'_>, icon_only: bool) -> String {
    let Some(mp4) = v.p.mp4.as_deref() else {
        return String::new();
    };
    let url = media_url(&v.p.slug, mp4);
    share_button(ctx, "video", &v.p.title, Some((mp4, url)), icon_only)
}

fn copy_button(ctx: &UiCtx, value: &str, label: &str) -> String {
    format!(
        "<button type='button' class='cr-btn cr-btn-plain cr-btn-small' data-copy=\"{}\" data-copied-label=\"{}\">{} <span data-copy-label>{}</span></button>",
        h(value),
        h(&ctx.t("Copied")),
        icons::icon("content_copy"),
        h(label)
    )
}

/// One copyable field; nothing at all when it is blank. A long multi-line value is
/// clamped with a Show more control.
fn copy_field(ctx: &UiCtx, id: &str, label: &str, value: &str, clamp: bool) -> String {
    if value.trim().is_empty() {
        return String::new();
    }
    let long = clamp && value.chars().count() > CLAMP_CHARS;
    let expand = if long {
        format!(
            "<button type='button' class='cr-link' data-cr-expand aria-controls='{id}' aria-expanded='false' \
data-more=\"{more}\" data-less=\"{}\">{more}</button>",
            h(&ctx.t("Show less")),
            more = h(&ctx.t("Show more"))
        )
    } else {
        String::new()
    };
    format!(
        "<div class='cr-field'><div class='cr-field-head'><span class='cr-label'>{}</span>{}</div>\
<p class='cr-field-text{}' id='{id}'>{}</p>{expand}</div>",
        h(label),
        copy_button(ctx, value, &ctx.t("Copy")),
        if long { " cr-clamp" } else { "" },
        h(value)
    )
}

fn hashtags(c: &PublishCopy) -> String {
    c.hashtags
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| {
            if t.starts_with('#') {
                t.to_string()
            } else {
                format!("#{t}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn platform_group(
    ctx: &UiCtx,
    v: &VideoView<'_>,
    (key, label): (&str, &str),
    c: &PublishCopy,
    is_published: bool,
    open: bool,
) -> String {
    let base = format!("{}-{key}", v.dom_id);
    let tags = hashtags(c);
    let all = [c.title.as_str(), c.caption.as_str(), tags.as_str()]
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let fields = if all.is_empty() {
        subtitle(&ctx.t("This platform has no text for this video yet."))
    } else {
        format!(
            "{}{}{}",
            copy_field(
                ctx,
                &format!("{base}-title"),
                &ctx.t("Title"),
                &c.title,
                false
            ),
            copy_field(
                ctx,
                &format!("{base}-caption"),
                &ctx.t("Caption"),
                &c.caption,
                true
            ),
            copy_field(
                ctx,
                &format!("{base}-tags"),
                &ctx.t("Hashtags"),
                &tags,
                false
            ),
        )
    };
    let copy_all = if all.is_empty() {
        String::new()
    } else {
        format!(
            "{}{}",
            copy_button(ctx, &all, &ctx.t("Copy all")),
            share_button(ctx, "caption", &all, None, false)
        )
    };
    let (on, toggle) = if is_published {
        ("0", ctx.t("Mark as not published"))
    } else {
        ("1", ctx.t("Mark as published"))
    };
    let badge = if is_published {
        pill("ok", &ctx.t("Published"))
    } else {
        String::new()
    };
    format!(
        "<details class='cr-group'{}><summary><span class='cr-group-title'>{}</span>{badge}\
<span class='cr-chevron' aria-hidden='true'></span></summary><div class='cr-group-body'>{fields}\
<div class='cr-group-actions'>{copy_all}<form method='post' action='/creator/published' class='cr-inline-form'>{}\
<input type='hidden' name='slug' value=\"{}\"><input type='hidden' name='platform' value='{key}'>\
<input type='hidden' name='on' value='{on}'><button type='submit' class='cr-btn cr-btn-plain cr-btn-small'>{} {}</button></form></div></div></details>",
        if open { " open" } else { "" },
        h(label),
        ctx.csrf_input(),
        h(&v.p.slug),
        icons::icon("send"),
        h(&toggle)
    )
}

fn publish_html(ctx: &UiCtx, v: &VideoView<'_>, st: &CreatorState) -> String {
    let done = st.published.get(&v.p.slug);
    let groups: Vec<String> = PLATFORMS
        .iter()
        .filter_map(|(key, label)| {
            let copy = match *key {
                "wechat" => v.p.wechat.as_ref(),
                _ => v.p.douyin.as_ref(),
            }?;
            let is_published = done.is_some_and(|d| d.iter().any(|x| x == key));
            Some((*key, *label, copy, is_published))
        })
        .enumerate()
        .map(|(i, (key, label, copy, is_published))| {
            platform_group(ctx, v, (key, label), copy, is_published, i == 0)
        })
        .collect();
    let body = if groups.is_empty() {
        format!(
            "<p class='cr-muted cr-publish-empty'>{}</p>",
            h(&ctx.t("No publish text yet. When inspire-video writes a WeChat or Douyin title, caption and hashtags for this video, they show up here."))
        )
    } else {
        groups.concat()
    };
    format!(
        "<div class='cr-publish'><div class='cr-label'>{}</div>{body}</div>",
        h(&ctx.t("Publish"))
    )
}

fn video_dialog(ctx: &UiCtx, v: &VideoView<'_>, st: &CreatorState) -> String {
    let player = match &v.p.mp4 {
        Some(mp4) => {
            let poster =
                v.p.cover
                    .as_deref()
                    .map(|c| format!(" poster=\"{}\"", media_url(&v.p.slug, c)))
                    .unwrap_or_default();
            // `data-src`: the video loads only when the dialog opens.
            format!(
                "<video controls playsinline preload='none' data-src=\"{}\"{poster}></video>",
                media_url(&v.p.slug, mp4)
            )
        }
        None => format!(
            "<div class='cr-player-empty'>{}</div>",
            h(&ctx.t("No video file"))
        ),
    };
    let source = match v.p.source_url.as_deref().and_then(https_link) {
        Some(u) => format!(
            "<a class='cr-btn cr-btn-plain' href=\"{}\" target='_blank' rel='noopener noreferrer'>{} {}</a>",
            h(u),
            icons::icon("open_in_new"),
            h(&ctx.t("Source"))
        ),
        None => String::new(),
    };
    let log = v.last_job.map(|j| log_link(ctx, j)).unwrap_or_default();
    let share = video_share(ctx, v, false);
    let folder = folder_form(ctx, v, false);
    let stop = v
        .rendering
        .map(|j| stop_form(ctx, j, "videos"))
        .unwrap_or_default();
    let created = v.p.created_at.as_deref().map(when).unwrap_or_default();
    format!(
        "<dialog class='cr-dialog' id='{id}' data-slug=\"{}\" aria-labelledby='{id}-title'>\
<div class='cr-dialog-inner'><header class='cr-dialog-head'><div class='cr-dialog-titles'>\
<h2 id='{id}-title'>{}</h2><div class='cr-badges'>{}<span class='cr-meta'>{}</span></div></div>\
<button type='button' class='cr-icon-btn' data-cr-close aria-label=\"{}\">{}</button></header>\
<div class='cr-dialog-body'><div class='cr-player'>{player}</div><div class='cr-details'>\
<div class='cr-actions'>{}{share}{folder}{source}{log}{stop}</div>{}</div></div></div></dialog>",
        h(&v.p.slug),
        h(&v.p.title),
        pill(v.status_kind, &v.status),
        h(&created),
        h(&ctx.t("Close")),
        icons::icon("close"),
        approve_form(ctx, v),
        publish_html(ctx, v, st),
        id = v.dom_id
    )
}

fn videos_tab(ctx: &UiCtx, jobs: &[JobRecord]) -> String {
    let root = videos_root(ctx);
    let (projects, errors) = videos::list_projects(&root);
    let st = cstate::load(&state_path(ctx));
    let pending: Vec<&JobRecord> = jobs
        .iter()
        .filter(|j| j.kind == JobKind::Generate && j.state == "running")
        .collect();
    let pending = running_list(ctx, &ctx.t("Generating"), &pending, "videos");
    let list = if projects.is_empty() {
        no_videos(ctx, &root)
    } else {
        let views: Vec<VideoView<'_>> = projects
            .iter()
            .enumerate()
            .map(|(i, p)| video_view(ctx, i, p, &st, jobs))
            .collect();
        let posters: String = views.iter().map(|v| poster_card(ctx, v)).collect();
        let dialogs: String = views.iter().map(|v| video_dialog(ctx, v, &st)).collect();
        format!(
            "{}{dialogs}",
            section(
                &ctx.t("Videos"),
                "",
                &format!("<ul class='cr-grid'>{posters}</ul>")
            )
        )
    };
    format!("{pending}{}{list}", errors_card(ctx, &errors))
}

fn page(ctx: &UiCtx) -> UiResponse {
    let tab = tab_of(ctx.query().first("tab").unwrap_or(""));
    let (flash, ok) = ctx.flash();
    let jobs = jobs::list_jobs(&jobs_dir(ctx));
    let body = match tab {
        "videos" => videos_tab(ctx, &jobs),
        "jobs" => jobs_tab(ctx, &jobs),
        _ => discover_tab(ctx, &jobs),
    };
    let head = format!(
        "<div class='page-title'><h1>{}</h1><p class='subtitle'>{}</p></div>{}{}",
        h(&ctx.t("Creator")),
        h(&ctx.t(
            "Find source videos, generate short videos from them, review them and copy their publish info."
        )),
        flash_html(flash.as_deref(), ok),
        tabs_html(ctx, tab)
    );
    UiResponse::html(render_shell(
        ctx,
        "Creator · LoopX Engineering",
        "creator",
        &default_badge(ctx),
        &format!(
            "<style>{CREATOR_CSS}</style><div class='cr'>{head}{body}</div>{}<script>{CREATOR_JS}</script>",
            log_sheet(ctx)
        ),
        ShellOpts::default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{finder_args, range_response, when, MAX_FULL_BYTES, MAX_RANGE_BYTES};
    use crate::ui::routes::{UiRequest, UiResponse};
    use crate::ui::state::UiState;
    use axum::http::Method;
    use loopx_core::creator::state as cstate;
    use serde_json::json;
    use std::path::PathBuf;

    struct Fx {
        dir: tempfile::TempDir,
        st: UiState,
    }

    impl Fx {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("loopx-home");
            let user = dir.path().join("user");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(&user).unwrap();
            let st = UiState::explicit(
                dir.path(),
                &home,
                &user,
                &dir.path().join("update"),
                "tok",
                None,
            );
            Fx { dir, st }
        }
        fn user(&self) -> PathBuf {
            self.dir.path().join("user")
        }
        fn scout(&self) -> PathBuf {
            let p = self.user().join(".video-scout");
            std::fs::create_dir_all(&p).unwrap();
            p
        }
        fn project(&self) -> PathBuf {
            let p = self.user().join(".inspire-video/output/final/demo");
            std::fs::create_dir_all(&p).unwrap();
            p
        }
        fn jobs(&self) -> PathBuf {
            self.st.home_dir.join("creator").join("jobs")
        }
        fn job_files(&self) -> usize {
            std::fs::read_dir(self.jobs())
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
                        .count()
                })
                .unwrap_or(0)
        }
        fn get(&self, path: &str) -> UiResponse {
            self.st
                .registry
                .dispatch(&self.st, &UiRequest::get(path))
                .unwrap()
        }
        fn get_range(&self, path: &str, range: &str) -> UiResponse {
            self.st
                .registry
                .dispatch(&self.st, &UiRequest::get(path).with_header("range", range))
                .unwrap()
        }
        fn post(&self, path: &str, body: &str) -> UiResponse {
            self.st
                .registry
                .dispatch(
                    &self.st,
                    &UiRequest::new(Method::POST, path).with_body(body.to_string()),
                )
                .unwrap()
        }
        fn state_path(&self) -> PathBuf {
            self.st.home_dir.join("creator.json")
        }
        /// A job record: `alive` ones are held by this test process, others by a
        /// process that has exited (their log decides done or failed).
        fn job(&self, id: &str, kind: &str, key: &str, alive: bool, log: &str) {
            let pid = if alive {
                i64::from(std::process::id())
            } else {
                let mut c = std::process::Command::new("true").spawn().unwrap();
                c.wait().unwrap();
                i64::from(c.id())
            };
            std::fs::create_dir_all(self.jobs()).unwrap();
            std::fs::write(
                self.jobs().join(format!("{id}.json")),
                json!({"id": id, "kind": kind, "key": key, "state": "running",
                       "pid": pid, "started_at": "2026-11-05T00:00:00Z"})
                .to_string(),
            )
            .unwrap();
            std::fs::write(self.jobs().join(format!("{id}.log")), log).unwrap();
        }
        /// A `running` generate record held by this (alive) test process.
        fn running_job(&self, key: &str) {
            std::fs::create_dir_all(self.jobs()).unwrap();
            std::fs::write(
                self.jobs().join("20261104T000000-aaaaaaaa.json"),
                json!({"id": "20261104T000000-aaaaaaaa", "kind": "generate", "key": key,
                       "state": "running", "pid": std::process::id(),
                       "started_at": chrono::Utc::now().to_rfc3339()})
                .to_string(),
            )
            .unwrap();
        }
    }

    const SOURCE: &str = "https://example.com/talks/a";

    fn catalog() -> String {
        json!({"schema_version": "1.1", "generated_at": "2026-11-04T08:00:00Z",
            "discovery_mode": "topic_based", "query": "leadership",
            "videos": [
              {"rank": 1, "title": "A <b>talk</b>", "url": SOURCE, "platform": "TED",
               "channel_or_publisher": "TED", "duration_seconds": 600,
               "quality": {"total_score": 81.2, "grade": "Excellent", "rationale": "Solid & clear."},
               "recommended_segments": [{"start_seconds": 60, "end_seconds": 90, "description": "The thesis"}],
               "handoff_ready": true},
              {"rank": 2, "title": "Sneaky", "url": "javascript:alert(1)", "platform": "x",
               "quality": {"total_score": 50.0, "grade": "Fair", "rationale": "meh"}}
            ]})
        .to_string()
    }

    fn write_project(fx: &Fx, wechat_title: &str, with_douyin: bool) {
        let p = fx.project();
        let bytes: Vec<u8> = (0..100u8).collect();
        std::fs::write(p.join("demo.mp4"), bytes).unwrap();
        std::fs::write(p.join("cover.png"), b"\x89PNG fake").unwrap();
        let mut publish = json!({"wechat": {"title": wechat_title, "caption": "it's a caption",
                                             "hashtags": ["#grow", "learn"]}});
        if with_douyin {
            publish["douyin"] = json!({"title": "Douyin title", "caption": "c", "hashtags": []});
        }
        std::fs::write(
            p.join("project.json"),
            json!({"title": "Demo short", "source_url": SOURCE, "video": "demo.mp4",
                   "cover": "cover.png", "created_at": "2026-11-04T09:00:00Z", "publish": publish})
            .to_string(),
        )
        .unwrap();
    }

    fn flash_of(r: &UiResponse) -> (String, String) {
        assert_eq!(r.status, 303, "{}", r.body_text());
        let loc = r.header("Location").unwrap().to_string();
        let ok = if loc.ends_with("&ok=1") { "1" } else { "0" };
        (loc.clone(), ok.to_string())
    }

    #[test]
    fn creator_page_renders_empty_state_without_skill_folders() {
        let fx = Fx::new();
        let r = fx.get("/creator");
        assert_eq!(r.status, 200);
        let body = r.body_text();
        assert!(body.contains("No video-scout catalogs yet"), "{body}");
        assert!(body.contains("action='/creator/scout'"), "{body}");
        assert!(body.contains("href='/creator?tab=videos'"), "{body}");
        let videos = fx.get("/creator?tab=videos").body_text();
        assert!(videos.contains("No videos yet"), "{videos}");
        // The old Publish tab is gone; its links land on Videos.
        assert!(!body.contains("tab=publish"), "{body}");
        assert!(!body.contains(">Publish</a>"), "{body}");
        let publish = fx.get("/creator?tab=publish").body_text();
        assert!(publish.contains("No videos yet"), "{publish}");
        assert!(
            publish.contains("href='/creator?tab=videos' aria-current='page'"),
            "{publish}"
        );
    }

    #[test]
    fn discover_lists_sources_and_error_rows() {
        let fx = Fx::new();
        let s = fx.scout();
        std::fs::write(s.join("catalog-2026-11-04-a.json"), catalog()).unwrap();
        std::fs::write(s.join("catalog-bad.json"), "{ not json").unwrap();
        let body = fx.get("/creator?tab=discover").body_text();
        assert!(
            body.contains("A &lt;b&gt;talk&lt;/b&gt;") && !body.contains("A <b>talk"),
            "{body}"
        );
        assert!(body.contains(&format!("href=\"{SOURCE}\"")), "{body}");
        assert!(!body.contains("href=\"javascript:"), "{body}");
        assert!(body.contains("Sneaky"));
        assert!(
            body.contains("Excellent") && body.contains("81.2"),
            "{body}"
        );
        assert!(body.contains("Solid &amp; clear."));
        assert!(body.contains("The thesis"));
        assert!(body.contains("I own this video or have permission to reuse it"));
        assert!(body.contains("name='rights' value='1' required"), "{body}");
        assert!(body.contains("action='/creator/generate'"));
        // Apple-style item: rank, muted meta, badge, small pill, segments, footer form.
        assert!(body.contains("<span class='cr-rank'>1</span>"), "{body}");
        assert!(
            body.contains("<span class='cr-badge'>Excellent · 81.2</span>"),
            "{body}"
        );
        assert!(
            body.contains("<span class='cr-pill cr-pill-ok'>Ready to generate</span>"),
            "{body}"
        );
        assert!(body.contains("<span class='cr-chip'>"), "{body}");
        assert!(!body.contains("class='card"), "{body}");
        assert!(
            body.contains("flash flash-danger") && body.contains("catalog-bad.json"),
            "{body}"
        );
    }

    #[test]
    fn generate_requires_rights_checkbox() {
        let fx = Fx::new();
        let r = fx.post(
            "/creator/generate",
            "csrf_token=tok&url=https%3A%2F%2Fexample.com%2Ftalks%2Fa",
        );
        let (loc, ok) = flash_of(&r);
        assert!(loc.starts_with("/creator?tab=discover&flash="), "{loc}");
        assert_eq!(ok, "0");
        assert_eq!(fx.job_files(), 0);
    }

    #[test]
    fn generate_rejects_second_start_for_same_url() {
        let fx = Fx::new();
        std::fs::write(fx.scout().join("catalog-x.json"), catalog()).unwrap();
        fx.running_job(SOURCE);
        let r = fx.post(
            "/creator/generate",
            "csrf_token=tok&rights=1&url=https%3A%2F%2Fexample.com%2Ftalks%2Fa",
        );
        let (loc, ok) = flash_of(&r);
        assert_eq!(ok, "0");
        assert!(loc.contains("already%20running"), "{loc}");
        assert_eq!(fx.job_files(), 1);
        // The source row shows the running job and a Stop button instead of Generate.
        let body = fx.get("/creator").body_text();
        assert!(body.contains("action='/creator/stop'"), "{body}");
        assert!(
            body.contains("value=\"20261104T000000-aaaaaaaa\""),
            "{body}"
        );
    }

    #[test]
    fn generate_requires_csrf() {
        let fx = Fx::new();
        for path in [
            "/creator/generate",
            "/creator/scout",
            "/creator/stop",
            "/creator/approve",
            "/creator/published",
        ] {
            let r = fx.post(
                path,
                "url=https%3A%2F%2Fexample.com&rights=1&slug=demo&on=1",
            );
            assert_eq!(r.status, 403, "{path}");
        }
        assert_eq!(fx.job_files(), 0);
    }

    #[test]
    fn job_starts_are_refused_when_this_server_does_not_spawn() {
        let fx = Fx::new();
        let (_, ok) = flash_of(&fx.post("/creator/scout", "csrf_token=tok&query=teams"));
        assert_eq!(ok, "0");
        assert_eq!(fx.job_files(), 0);
    }

    #[test]
    fn media_serves_mp4_and_206_range() {
        let fx = Fx::new();
        write_project(&fx, "T", true);
        let all: Vec<u8> = (0..100u8).collect();
        let r = fx.get("/creator/media/demo/demo.mp4");
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type(), Some("video/mp4"));
        assert_eq!(r.header("Accept-Ranges"), Some("bytes"));
        assert_eq!(r.header("X-Content-Type-Options"), Some("nosniff"));
        assert_eq!(r.body, all);

        let r = fx.get_range("/creator/media/demo/demo.mp4", "bytes=10-19");
        assert_eq!(r.status, 206);
        assert_eq!(r.header("Content-Range"), Some("bytes 10-19/100"));
        assert_eq!(r.header("Accept-Ranges"), Some("bytes"));
        assert_eq!(r.body, all[10..20]);

        let r = fx.get_range("/creator/media/demo/demo.mp4", "bytes=90-");
        assert_eq!(r.status, 206);
        assert_eq!(r.header("Content-Range"), Some("bytes 90-99/100"));
        assert_eq!(r.body, all[90..]);

        let r = fx.get_range("/creator/media/demo/demo.mp4", "bytes=-5");
        assert_eq!(r.header("Content-Range"), Some("bytes 95-99/100"));
        assert_eq!(r.body, all[95..]);

        let r = fx.get_range("/creator/media/demo/demo.mp4", "bytes=50-500");
        assert_eq!(r.header("Content-Range"), Some("bytes 50-99/100"));

        for bad in ["bytes=200-300", "bytes=20-10", "items=0-1", "bytes=x-y"] {
            let r = fx.get_range("/creator/media/demo/demo.mp4", bad);
            assert_eq!(r.status, 416, "{bad}");
            assert_eq!(r.header("Content-Range"), Some("bytes */100"), "{bad}");
            assert!(r.body.is_empty() || r.body.len() < 100);
        }

        let r = fx.get("/creator/media/demo/cover.png");
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type(), Some("image/png"));
    }

    #[test]
    fn empty_range_read_is_416_not_an_underflow() {
        let r = range_response("video/mp4", &[], 10, 100, Vec::new());
        assert_eq!(r.status, 416);
        assert_eq!(r.header("Content-Range"), Some("bytes */100"));
        let r = range_response("video/mp4", &[], 10, 100, vec![1, 2, 3]);
        assert_eq!(r.status, 206);
        assert_eq!(r.header("Content-Range"), Some("bytes 10-12/100"));
    }

    #[test]
    fn media_caps_one_range_at_8_mib_and_refuses_a_huge_full_get() {
        let fx = Fx::new();
        write_project(&fx, "T", true);
        let big = fx.project().join("big.mp4");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_FULL_BYTES + 1)
            .unwrap();
        let r = fx.get("/creator/media/demo/big.mp4");
        assert_eq!(r.status, 413);
        let r = fx.get_range("/creator/media/demo/big.mp4", "bytes=0-");
        assert_eq!(r.status, 206);
        assert_eq!(r.body.len() as u64, MAX_RANGE_BYTES);
        let want = format!("bytes 0-{}/{}", MAX_RANGE_BYTES - 1, MAX_FULL_BYTES + 1);
        assert_eq!(r.header("Content-Range"), Some(want.as_str()));
    }

    #[test]
    fn when_converts_offset_timestamps_to_utc() {
        assert_eq!(when("2026-11-04T09:00:00Z"), "2026-11-04 09:00 UTC");
        assert_eq!(when("2026-11-04T09:00:00+08:00"), "2026-11-04 01:00 UTC");
        assert_eq!(when("2026-11-04T09:00:00"), "2026-11-04 09:00");
        assert_eq!(when("yesterday"), "yesterday");
    }

    #[test]
    fn no_approve_button_without_a_video_file() {
        let fx = Fx::new();
        write_project(&fx, "T", true);
        std::fs::remove_file(fx.project().join("demo.mp4")).unwrap();
        let body = fx.get("/creator?tab=videos").body_text();
        assert!(body.contains("No video file"), "{body}");
        assert!(!body.contains("action='/creator/approve'"), "{body}");
    }

    #[test]
    fn media_404_on_traversal_and_symlink_escape() {
        let fx = Fx::new();
        write_project(&fx, "T", true);
        let secret = fx.dir.path().join("secret.mp4");
        std::fs::write(&secret, b"secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, fx.project().join("escape.mp4")).unwrap();
        for path in [
            "/creator/media/demo/escape.mp4",
            "/creator/media/demo/nope.mp4",
            "/creator/media/demo/project.json",
            "/creator/media/demo/..%2F..%2F..%2Fsecret.mp4",
            "/creator/media/..%2F..%2F..%2F..%2F/secret.mp4",
            "/creator/media/../secret.mp4",
            "/creator/media/%2E%2E/secret.mp4",
        ] {
            let r = fx.get(path);
            assert_eq!(r.status, 404, "{path}");
            assert!(!r.body_text().contains("secret"), "{path}");
        }
    }

    #[test]
    fn videos_tab_shows_poster_cards_and_a_dialog_per_video() {
        let fx = Fx::new();
        write_project(&fx, "T", true);
        let body = fx.get("/creator?tab=videos").body_text();
        // A poster card that opens its dialog; no inline player on the page.
        assert!(
            body.contains("<button type='button' class='cr-poster' data-cr-open='cr-video-0' aria-haspopup='dialog'"),
            "{body}"
        );
        assert!(
            body.contains("<img src=\"/creator/media/demo/cover.png\""),
            "{body}"
        );
        assert!(
            body.contains("<span class='cr-poster-title'>Demo short</span>"),
            "{body}"
        );
        assert!(
            body.contains("<dialog class='cr-dialog' id='cr-video-0' data-slug=\"demo\" aria-labelledby='cr-video-0-title'>"),
            "{body}"
        );
        // The video URL waits in data-src until the dialog opens.
        assert!(
            body.contains("<video controls playsinline preload='none' data-src=\"/creator/media/demo/demo.mp4\" poster=\"/creator/media/demo/cover.png\"></video>"),
            "{body}"
        );
        assert!(
            !body.contains(" src=\"/creator/media/demo/demo.mp4\""),
            "{body}"
        );
        assert_eq!(body.matches("<video").count(), 1, "{body}");
        assert!(body.contains("data-cr-close"), "{body}");
        assert!(
            body.contains("<span class='cr-pill cr-pill-neutral'>Ready for review</span>"),
            "{body}"
        );
        assert!(body.contains("action='/creator/approve'"));
        fx.running_job(SOURCE);
        let body = fx.get("/creator?tab=videos").body_text();
        assert!(
            body.contains("<span class='cr-pill cr-pill-accent'>Rendering</span>"),
            "{body}"
        );
        assert!(!body.contains("action='/creator/approve'"), "{body}");
        // The dialog carries the job's log link and Stop while it renders.
        assert!(
            body.contains("href='/creator/jobs/20261104T000000-aaaaaaaa/log'"),
            "{body}"
        );
        assert!(body.contains("action='/creator/stop'"), "{body}");
    }

    #[test]
    fn the_dialog_holds_the_publish_groups_with_escaped_copy() {
        let fx = Fx::new();
        write_project(&fx, "\"><script>alert(1)</script>", true);
        let body = fx.get("/creator?tab=videos").body_text();
        let dialog = &body[body.find("<dialog class='cr-dialog'").unwrap()..];
        let dialog = &dialog[..dialog.find("</dialog>").unwrap()];
        assert!(!body.contains("<script>alert(1)"), "{body}");
        assert!(!body.contains("\"><script>"), "{body}");
        assert!(
            dialog.contains("data-copy=\"&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;\""),
            "{dialog}"
        );
        assert!(dialog.contains("it&#x27;s a caption"), "{dialog}");
        assert!(dialog.contains("#grow #learn"), "{dialog}");
        assert!(dialog.contains("Copy all"), "{dialog}");
        assert!(dialog.contains("data-copied-label=\"Copied\""), "{dialog}");
        assert!(dialog.contains("action='/creator/published'"), "{dialog}");
        assert!(dialog.contains("name='csrf_token'"), "{dialog}");
        // Two disclosure groups, the first one open.
        assert!(
            dialog.contains("<details class='cr-group' open><summary><span class='cr-group-title'>WeChat</span>"),
            "{dialog}"
        );
        assert!(
            dialog.contains(
                "<details class='cr-group'><summary><span class='cr-group-title'>Douyin</span>"
            ),
            "{dialog}"
        );
        assert!(!dialog.contains("Not available"), "{dialog}");
    }

    #[test]
    fn a_long_caption_gets_a_show_more_control() {
        let fx = Fx::new();
        let p = fx.project();
        std::fs::write(p.join("demo.mp4"), b"x").unwrap();
        let long = "word ".repeat(60);
        std::fs::write(
            p.join("project.json"),
            json!({"title": "Demo", "video": "demo.mp4", "publish": {
                "wechat": {"title": "t", "caption": long, "hashtags": []},
                "douyin": {"title": "t", "caption": "short", "hashtags": []}}})
            .to_string(),
        )
        .unwrap();
        let body = fx.get("/creator?tab=videos").body_text();
        assert!(
            body.contains("<p class='cr-field-text cr-clamp' id='cr-video-0-wechat-caption'>"),
            "{body}"
        );
        assert!(
            body.contains("aria-controls='cr-video-0-wechat-caption' aria-expanded='false'"),
            "{body}"
        );
        assert!(body.contains("Show more"), "{body}");
        assert_eq!(
            body.matches("data-cr-expand aria-controls").count(),
            1,
            "{body}"
        );
    }

    #[test]
    fn a_video_without_publish_text_shows_one_calm_sentence() {
        let fx = Fx::new();
        let p = fx.project();
        std::fs::write(p.join("demo.mp4"), b"x").unwrap();
        std::fs::write(
            p.join("project.json"),
            json!({"title": "Demo", "video": "demo.mp4"}).to_string(),
        )
        .unwrap();
        let body = fx.get("/creator?tab=videos").body_text();
        assert_eq!(body.matches("No publish text yet.").count(), 1, "{body}");
        assert!(!body.contains("Not available"), "{body}");
        assert!(!body.contains("<details class='cr-group'"), "{body}");
        assert!(!body.contains("action='/creator/published'"), "{body}");
        // One platform missing: only the other one is shown.
        write_project(&fx, "T", false);
        let body = fx.get("/creator?tab=videos").body_text();
        assert!(
            body.contains("WeChat") && !body.contains("Douyin"),
            "{body}"
        );
        assert!(!body.contains("No publish text yet."), "{body}");
    }

    #[test]
    fn approve_and_published_toggles_persist() {
        let fx = Fx::new();
        write_project(&fx, "T", true);
        let (loc, ok) = flash_of(&fx.post("/creator/approve", "csrf_token=tok&slug=demo&on=1"));
        // Back on Videos with the video's dialog reopened.
        assert!(
            loc.starts_with("/creator?tab=videos&open=demo&flash="),
            "{loc}"
        );
        assert_eq!(ok, "1");
        assert!(cstate::load(&fx.state_path()).approved.contains("demo"));
        assert!(fx
            .get("/creator?tab=videos")
            .body_text()
            .contains("Approved"));
        flash_of(&fx.post("/creator/approve", "csrf_token=tok&slug=demo&on=0"));
        assert!(!cstate::load(&fx.state_path()).approved.contains("demo"));

        let (loc, ok) = flash_of(&fx.post(
            "/creator/published",
            "csrf_token=tok&slug=demo&platform=wechat&on=1",
        ));
        assert!(
            loc.starts_with("/creator?tab=videos&open=demo&flash="),
            "{loc}"
        );
        assert_eq!(ok, "1");
        assert_eq!(
            cstate::load(&fx.state_path()).published.get("demo"),
            Some(&vec!["wechat".to_string()])
        );
        assert!(fx
            .get("/creator?tab=videos")
            .body_text()
            .contains("<span class='cr-pill cr-pill-ok'>Published</span>"));

        // Unknown projects and platforms change nothing.
        let (_, ok) = flash_of(&fx.post("/creator/approve", "csrf_token=tok&slug=ghost&on=1"));
        assert_eq!(ok, "0");
        let (_, ok) = flash_of(&fx.post(
            "/creator/published",
            "csrf_token=tok&slug=demo&platform=tiktok&on=1",
        ));
        assert_eq!(ok, "0");
        let s = cstate::load(&fx.state_path());
        assert!(!s.approved.contains("ghost"));
        assert_eq!(s.published.get("demo"), Some(&vec!["wechat".to_string()]));
    }

    #[test]
    fn generate_refuses_an_unsafe_url_before_anything_else() {
        let fx = Fx::new();
        for url in [
            "http%3A%2F%2Fexample.com%2Fv",
            "https%3A%2F%2Fexample.com%2Fv%3Bid",
            "https%3A%2F%2Fexample.com%2F%24(id)",
            "https%3A%2F%2Fexample.com%2Fwatch%3Fv%3Da%26list%3Db",
        ] {
            let r = fx.post(
                "/creator/generate",
                &format!("csrf_token=tok&rights=1&url={url}"),
            );
            let (loc, ok) = flash_of(&r);
            assert_eq!(ok, "0", "{url}");
            assert!(loc.contains("Nothing%20started"), "{url}: {loc}");
            assert!(!loc.contains("does%20not%20start"), "{url}: {loc}");
        }
        assert_eq!(fx.job_files(), 0);
    }

    #[test]
    fn a_source_with_an_unsafe_url_has_no_generate_form() {
        let fx = Fx::new();
        let body = json!({"generated_at": "2026-11-04T08:00:00Z", "videos": [
            {"rank": 1, "title": "Shady", "url": "https://example.com/v?a=1;rm"},
            {"rank": 2, "title": "Plain http", "url": "http://example.com/v"}]});
        std::fs::write(fx.scout().join("catalog-x.json"), body.to_string()).unwrap();
        let page = fx.get("/creator").body_text();
        assert!(
            page.contains("Shady") && page.contains("Plain http"),
            "{page}"
        );
        assert!(!page.contains("action='/creator/generate'"), "{page}");
    }

    /// A job record whose process has exited, with `log` as its log.
    fn finished_job(fx: &Fx, id: &str, log: &str) {
        let mut c = std::process::Command::new("true").spawn().unwrap();
        c.wait().unwrap();
        std::fs::create_dir_all(fx.jobs()).unwrap();
        std::fs::write(
            fx.jobs().join(format!("{id}.json")),
            json!({"id": id, "kind": "generate", "key": "https://example.com/never",
                   "state": "running", "pid": c.id(), "started_at": "2026-11-05T00:00:00Z"})
            .to_string(),
        )
        .unwrap();
        std::fs::write(fx.jobs().join(format!("{id}.log")), log).unwrap();
    }

    fn step_line(cmd: &str) -> String {
        format!(
            "{}\n",
            json!({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "name": "Bash", "input": {"command": cmd}}]}})
        )
    }

    #[test]
    fn the_jobs_tab_lists_every_job_with_stop_for_running_and_delete_for_finished() {
        let fx = Fx::new();
        fx.job("20261105T000001-scout000", "scout", "leadership", false, "loopx-exit=0\n");
        fx.job("20261105T000002-genfail0", "generate", "https://example.com/talks/a", false, "boom\n");
        fx.job(
            "20261105T000003-genlive0",
            "generate",
            "https://example.com/talks/b",
            true,
            &step_line("python scripts/transcribe.py"),
        );
        let body = fx.get("/creator?tab=jobs").body_text();
        assert!(body.contains("href='/creator?tab=jobs' aria-current='page'"), "{body}");
        for id in ["20261105T000001-scout000", "20261105T000002-genfail0", "20261105T000003-genlive0"] {
            assert!(body.contains(&format!("/creator/jobs/{id}/log")), "{id}");
        }
        assert!(body.contains("Done</span>") && body.contains("Failed</span>"), "{body}");
        // Finished jobs offer Delete (a confirm-on-second-click button), the running one Stop.
        assert_eq!(body.matches("action='/creator/jobs/delete'").count(), 2, "{body}");
        assert!(body.contains("data-cr-confirm"), "{body}");
        assert_eq!(body.matches("action='/creator/stop'").count(), 1, "{body}");
        assert!(body.contains("action='/creator/jobs/clear'"), "{body}");
        // The running job shows its current step and is watched for live updates.
        assert!(body.contains("Step 1 · Bash: python scripts/transcribe.py"), "{body}");
        assert!(body.contains("data-cr-status=\"20261105T000003-genlive0\""), "{body}");
        // Newest first.
        assert!(body.find("genlive0").unwrap() < body.find("scout000").unwrap());
    }

    #[test]
    fn a_running_generate_job_shows_its_stage_and_a_progress_bar() {
        let fx = Fx::new();
        let log = [
            step_line("python scripts/fetch.py u"),
            step_line("python scripts/transcribe.py a.wav"),
        ]
        .concat();
        fx.job("20261105T000003-genlive0", "generate", "https://example.com/talks/b", true, &log);
        fx.job("20261105T000005-scoutrun", "scout", "x", true, &step_line("WebSearch x"));
        for tab in ["videos", "jobs"] {
            let body = fx.get(&format!("/creator?tab={tab}")).body_text();
            assert!(body.contains("role='progressbar'"), "{tab}: {body}");
            assert!(body.contains("aria-valuenow='27'"), "{tab}: {body}");
            assert!(body.contains("style='width:27%'"), "{tab}: {body}");
            assert!(body.contains("Stage 3 of 9 · Transcribe"), "{tab}: {body}");
            assert!(body.contains("data-cr-progress=\"20261105T000003-genlive0\""), "{tab}: {body}");
            // The step line is still there under the bar.
            assert!(body.contains("Step 2 · Bash: python scripts/transcribe.py a.wav"), "{tab}: {body}");
        }
        // A Find videos job has no stages, so no bar.
        let body = fx.get("/creator?tab=discover").body_text();
        assert!(!body.contains("role='progressbar'"), "{body}");
    }

    #[test]
    fn a_generate_job_that_has_not_reached_a_stage_starts_the_bar_at_zero() {
        let fx = Fx::new();
        fx.job("20261105T000003-genlive0", "generate", "k", true, "");
        let body = fx.get("/creator?tab=videos").body_text();
        assert!(body.contains("aria-valuenow='0'"), "{body}");
        assert!(body.contains("Getting started"), "{body}");
    }

    #[test]
    fn the_status_endpoint_carries_progress_for_generate_jobs_only() {
        let fx = Fx::new();
        let id = "20261105T000003-genlive0";
        fx.job(id, "generate", "k", true, &step_line("python scripts/render.py m.json"));
        let v: serde_json::Value =
            serde_json::from_str(&fx.get(&format!("/creator/jobs/{id}/status")).body_text()).unwrap();
        assert_eq!(v["progress"]["stage"], 7);
        assert_eq!(v["progress"]["total"], 9);
        assert_eq!(v["progress"]["percent"], 72);
        assert_eq!(v["progress"]["text"], "Stage 7 of 9 · Render");
        fx.job("20261105T000005-scoutrun", "scout", "x", true, "");
        let v: serde_json::Value = serde_json::from_str(
            &fx.get("/creator/jobs/20261105T000005-scoutrun/status").body_text(),
        )
        .unwrap();
        assert!(v["progress"].is_null(), "{v}");
    }

    #[test]
    fn the_video_dialog_and_list_offer_share_and_open_folder() {
        let fx = Fx::new();
        write_project(&fx, "T", true);
        let body = fx.get("/creator?tab=videos").body_text();
        let dialog = &body[body.find("<dialog class='cr-dialog'").unwrap()..];
        let dialog = &dialog[..dialog.find("</dialog>").unwrap()];
        // The video: a Share button (shown by the script only where sharing works)
        // carrying the file's path and the title, and an Open folder form.
        assert!(dialog.contains("data-cr-share"), "{dialog}");
        assert!(dialog.contains("data-share-path=\""), "{dialog}");
        assert!(dialog.contains("demo.mp4\""), "{dialog}");
        assert!(dialog.contains("data-share-text=\"Demo short\""), "{dialog}");
        assert!(dialog.contains("data-share-url=\"/creator/media/demo/demo.mp4\""), "{dialog}");
        assert!(dialog.contains("action='/creator/folder'"), "{dialog}");
        assert!(dialog.contains("Open folder"), "{dialog}");
        // Each platform's caption can be shared as text (no file path).
        let groups = dialog.matches("data-share-kind='caption'").count();
        assert_eq!(groups, 2, "WeChat and Douyin: {dialog}");
        // The list cards carry the same two actions as icon buttons.
        let list = &body[..body.find("<dialog class='cr-dialog'").unwrap()];
        assert!(list.contains("cr-poster-tools"), "{list}");
        assert!(list.contains("data-cr-share"), "{list}");
        assert!(list.contains("action='/creator/folder'"), "{list}");
    }

    #[test]
    fn share_text_is_escaped_in_its_attribute() {
        let fx = Fx::new();
        write_project(&fx, "\"><script>alert(1)</script>", true);
        let body = fx.get("/creator?tab=videos").body_text();
        assert!(!body.contains("data-share-text=\"\"><script>"), "{body}");
        assert!(body.contains("&quot;&gt;&lt;script&gt;"), "{body}");
    }

    #[test]
    fn open_folder_needs_a_known_video_and_a_server_that_may_open_things() {
        let fx = Fx::new();
        write_project(&fx, "T", true);
        let (loc, ok) = flash_of(&fx.post("/creator/folder", "csrf_token=tok&slug=ghost"));
        assert_eq!(ok, "0", "{loc}");
        assert!(loc.contains("No%20such%20video"), "{loc}");
        // The test server does not spawn processes, so a real video is refused too
        // (and nothing is opened).
        let (loc, ok) = flash_of(&fx.post("/creator/folder", "csrf_token=tok&slug=demo"));
        assert_eq!(ok, "0", "{loc}");
        assert!(loc.contains("cannot%20open%20folders"), "{loc}");
        assert_eq!(fx.post("/creator/folder", "slug=demo").status, 403);
    }

    #[test]
    fn finder_reveals_a_file_and_opens_a_folder() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("a.mp4");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(
            finder_args(&file),
            vec!["-R".to_string(), file.display().to_string()]
        );
        assert_eq!(finder_args(d.path()), vec![d.path().display().to_string()]);
    }

    #[test]
    fn the_jobs_tab_has_an_empty_state() {
        let fx = Fx::new();
        let body = fx.get("/creator?tab=jobs").body_text();
        assert!(body.contains("No jobs yet"), "{body}");
        assert!(!body.contains("/creator/jobs/clear"), "{body}");
    }

    #[test]
    fn discover_shows_only_running_find_jobs_with_their_current_step() {
        let fx = Fx::new();
        fx.job("20261105T000001-scoutrun", "scout", "leadership", true, &step_line("WebSearch leadership talks"));
        fx.job("20261105T000002-scoutold", "scout", "old topic", false, "loopx-exit=0\n");
        fx.job("20261105T000003-genlive0", "generate", "https://example.com/talks/b", true, "");
        let body = fx.get("/creator?tab=discover").body_text();
        assert!(body.contains("Running jobs"), "{body}");
        assert!(body.contains("data-cr-status=\"20261105T000001-scoutrun\""), "{body}");
        assert!(body.contains("Step 1 · Bash: WebSearch leadership talks"), "{body}");
        // Finished runs and generate jobs are not listed here.
        assert!(!body.contains("20261105T000002-scoutold"), "{body}");
        assert!(!body.contains("data-cr-status=\"20261105T000003-genlive0\""), "{body}");
    }

    #[test]
    fn discover_shows_no_jobs_section_without_a_running_find_job() {
        let fx = Fx::new();
        fx.job("20261105T000002-scoutold", "scout", "old", false, "loopx-exit=0\n");
        let body = fx.get("/creator?tab=discover").body_text();
        assert!(!body.contains("Running jobs"), "{body}");
    }

    #[test]
    fn videos_shows_running_generate_jobs_with_a_status_message() {
        let fx = Fx::new();
        fx.job("20261105T000003-genlive0", "generate", "https://example.com/talks/b", true, "");
        fx.job("20261105T000004-genfail0", "generate", "https://example.com/talks/c", false, "boom\n");
        fx.job("20261105T000005-scoutrun", "scout", "x", true, "");
        let body = fx.get("/creator?tab=videos").body_text();
        assert!(body.contains("data-cr-status=\"20261105T000003-genlive0\""), "{body}");
        // Nothing yet: a starting message, not a blank row.
        assert!(body.contains("Starting…"), "{body}");
        assert!(!body.contains("20261105T000004-genfail0"), "failures live on the Jobs tab: {body}");
        assert!(!body.contains("20261105T000005-scoutrun"), "{body}");
    }

    #[test]
    fn the_status_endpoint_reports_state_and_latest_step() {
        let fx = Fx::new();
        let id = "20261105T000003-genlive0";
        fx.job(id, "generate", "https://example.com/talks/b", true, &step_line("ls"));
        let r = fx.get(&format!("/creator/jobs/{id}/status"));
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type(), Some("application/json"));
        let v: serde_json::Value = serde_json::from_str(&r.body_text()).unwrap();
        assert_eq!(v["state"], "running");
        assert_eq!(v["line"], "Step 1 · Bash: ls");
        fx.job("20261105T000004-genfail0", "generate", "k", false, "loopx-exit=1\n");
        let v: serde_json::Value =
            serde_json::from_str(&fx.get("/creator/jobs/20261105T000004-genfail0/status").body_text()).unwrap();
        assert_eq!(v["state"], "failed");
        assert_eq!(fx.get("/creator/jobs/..%2Fx/status").status, 404);
        assert_eq!(fx.get("/creator/jobs/20261105T999999-nothere0/status").status, 404);
    }

    #[test]
    fn deleting_removes_a_finished_job_and_refuses_a_running_one() {
        let fx = Fx::new();
        fx.job("20261105T000001-scout000", "scout", "q", false, "loopx-exit=0\n");
        fx.job("20261105T000003-genlive0", "generate", "k", true, "");
        let (loc, ok) = flash_of(&fx.post("/creator/jobs/delete", "csrf_token=tok&id=20261105T000001-scout000"));
        assert_eq!(ok, "1", "{loc}");
        assert!(loc.starts_with("/creator?tab=jobs"), "{loc}");
        assert!(!fx.jobs().join("20261105T000001-scout000.json").exists());
        assert!(!fx.jobs().join("20261105T000001-scout000.log").exists());
        let (_, ok) = flash_of(&fx.post("/creator/jobs/delete", "csrf_token=tok&id=20261105T000003-genlive0"));
        assert_eq!(ok, "0");
        assert!(fx.jobs().join("20261105T000003-genlive0.json").exists());
        let (_, ok) = flash_of(&fx.post("/creator/jobs/delete", "csrf_token=tok&id=nope"));
        assert_eq!(ok, "0");
    }

    #[test]
    fn clear_removes_every_finished_job_and_keeps_running_ones() {
        let fx = Fx::new();
        fx.job("20261105T000001-scout000", "scout", "q", false, "loopx-exit=0\n");
        fx.job("20261105T000002-genfail0", "generate", "k", false, "boom\n");
        fx.job("20261105T000003-genlive0", "generate", "k2", true, "");
        let (loc, ok) = flash_of(&fx.post("/creator/jobs/clear", "csrf_token=tok"));
        assert_eq!(ok, "1");
        assert!(loc.contains("Deleted%202%20finished%20jobs"), "{loc}");
        assert_eq!(fx.job_files(), 1);
    }

    #[test]
    fn the_new_post_routes_require_csrf() {
        let fx = Fx::new();
        for path in ["/creator/jobs/delete", "/creator/jobs/clear"] {
            assert_eq!(fx.post(path, "id=20261105T000001-scout000").status, 403, "{path}");
        }
    }

    #[test]
    fn a_failed_job_shows_failed_and_links_its_log_on_the_jobs_tab() {
        let fx = Fx::new();
        let id = "20261105T000000-failed00";
        finished_job(&fx, id, "boom\nloopx-exit=3\n");
        let body = fx.get("/creator?tab=jobs").body_text();
        assert!(
            body.contains("<span class='cr-pill cr-pill-error'>Failed</span>"),
            "{body}"
        );
        assert!(!body.contains(">Done</span>"), "{body}");
        assert!(body.contains(&format!("href='/creator/jobs/{id}/log'")), "{body}");
        assert!(body.contains("View log"), "{body}");
        // Discover and Videos list running jobs only: a failure is not repeated there.
        for tab in ["discover", "videos"] {
            let other = fx.get(&format!("/creator?tab={tab}")).body_text();
            assert!(!other.contains(id), "{tab}: {other}");
        }
        let ok = "20261105T000001-success0";
        finished_job(&fx, ok, "loopx-exit=0\n");
        let body = fx.get("/creator?tab=jobs").body_text();
        assert!(
            body.contains("<span class='cr-pill cr-pill-ok'>Done</span>"),
            "{body}"
        );
    }

    #[test]
    fn view_log_opens_an_in_page_sheet_and_keeps_the_link_as_a_fallback() {
        let fx = Fx::new();
        let id = "20261105T000000-sheet000";
        finished_job(&fx, id, "sh: claude: command not found\nloopx-exit=127\n");
        let body = fx.get("/creator?tab=jobs").body_text();
        // The link still works without the script, and the script hooks it.
        assert!(
            body.contains(&format!(
                "<a class='cr-btn cr-btn-plain' href='/creator/jobs/{id}/log' data-cr-log"
            )),
            "{body}"
        );
        // One right-hand sheet per page, labelled in the request's language.
        assert_eq!(body.matches("<dialog class='cr-sheet'").count(), 1, "{body}");
        assert!(body.contains("aria-labelledby='cr-sheet-title'"), "{body}");
        assert!(body.contains("data-failed-label=\"Could not load the log.\""), "{body}");
        assert!(body.contains("data-empty-label=\"Waiting for the first step…\""), "{body}");
        assert!(body.contains("<pre class='cr-sheet-log'"), "{body}");
    }

    #[test]
    fn job_log_is_served_as_inert_plain_text() {
        let fx = Fx::new();
        let id = "20261105T000000-logview0";
        finished_job(&fx, id, "<script>alert(1)</script>\nloopx-exit=1\n");
        let r = fx.get(&format!("/creator/jobs/{id}/log"));
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type(), Some("text/plain; charset=utf-8"));
        assert_eq!(r.header("X-Content-Type-Options"), Some("nosniff"));
        assert_eq!(r.header("Cache-Control"), Some("no-store"));
        assert!(r.body_text().contains("loopx-exit=1"), "{}", r.body_text());
        for path in [
            "/creator/jobs/nope/log",
            "/creator/jobs/..%2F..%2Fcreator/log",
            "/creator/jobs/%2E%2E/log",
            "/creator/jobs/../log",
        ] {
            let r = fx.get(path);
            assert_eq!(r.status, 404, "{path}");
            assert!(!r.body_text().contains("<script>alert"), "{path}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn job_log_route_does_not_follow_a_symlinked_log() {
        let fx = Fx::new();
        let id = "20261105T000000-symlink0";
        finished_job(&fx, id, "");
        let log = fx.jobs().join(format!("{id}.log"));
        std::fs::remove_file(&log).unwrap();
        let secret = fx.dir.path().join("secret.txt");
        std::fs::write(&secret, "TOP SECRET").unwrap();
        std::os::unix::fs::symlink(&secret, &log).unwrap();
        let r = fx.get(&format!("/creator/jobs/{id}/log"));
        assert_eq!(r.status, 404);
        assert!(!r.body_text().contains("TOP SECRET"));
    }

    #[test]
    fn an_ampersand_link_is_refused_with_a_translated_hint() {
        let fx = Fx::new();
        let body =
            "csrf_token=tok&rights=1&url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3Da%26list%3Db";
        let (loc, ok) = flash_of(&fx.post("/creator/generate", body));
        assert_eq!(ok, "0");
        assert!(loc.contains("video%20id"), "{loc}");
        assert!(!loc.contains("percent"), "{loc}");
        let r = fx.st.registry.dispatch(
            &fx.st,
            &UiRequest::new(Method::POST, "/creator/generate")
                .with_body(body.to_string())
                .with_header("cookie", "loop_lang=fr"),
        );
        let (loc, _) = flash_of(&r.unwrap());
        assert!(!loc.contains("video%20id"), "{loc}");
        assert!(loc.contains("%26list%3D"), "{loc}");
        assert_eq!(fx.job_files(), 0);
    }

    #[test]
    fn empty_publish_fields_have_no_copy_button() {
        let fx = Fx::new();
        let p = fx.project();
        std::fs::write(p.join("demo.mp4"), b"x").unwrap();
        std::fs::write(
            p.join("project.json"),
            json!({"title": "Demo", "video": "demo.mp4", "publish": {
                "wechat": {"title": "", "caption": "", "hashtags": []},
                "douyin": {"title": "Only a title", "caption": "", "hashtags": ["", " "]}}})
            .to_string(),
        )
        .unwrap();
        let body = fx.get("/creator?tab=videos").body_text();
        // Only Douyin's title and its Copy all have anything to copy.
        assert_eq!(body.matches("data-copy=").count(), 2, "{body}");
        assert!(body.contains("data-copy=\"Only a title\""), "{body}");
        assert!(!body.contains("data-copy=\"\""), "{body}");
        assert_eq!(
            body.matches("This platform has no text for this video yet.")
                .count(),
            1,
            "{body}"
        );
        assert!(!body.contains("Not available"), "{body}");
    }

    #[test]
    fn the_generate_form_posts_rights_and_csrf_from_the_item_footer() {
        let fx = Fx::new();
        std::fs::write(fx.scout().join("catalog-x.json"), catalog()).unwrap();
        let body = fx.get("/creator").body_text();
        let form = &body[body
            .find("<form method='post' action='/creator/generate'")
            .unwrap()..];
        let form = &form[..form.find("</form>").unwrap()];
        assert!(form.contains("class='cr-item-actions'"), "{form}");
        assert!(form.contains("name='csrf_token' value=\"tok\""), "{form}");
        assert!(
            form.contains(&format!(
                "<input type='hidden' name='url' value=\"{SOURCE}\">"
            )),
            "{form}"
        );
        assert!(
            form.contains(
                "<label class='cr-check'><input type='checkbox' name='rights' value='1' required>"
            ),
            "{form}"
        );
        assert!(
            form.contains("<button type='submit' class='cr-btn cr-btn-primary cr-generate'>"),
            "{form}"
        );
        // More than two segments fold into a disclosure.
        let three = json!({"generated_at": "2026-11-04T08:00:00Z", "videos": [
            {"rank": 1, "title": "Many", "url": SOURCE, "recommended_segments": [
                {"start_seconds": 1, "end_seconds": 2, "description": "one"},
                {"start_seconds": 3, "end_seconds": 4, "description": "two"},
                {"start_seconds": 5, "end_seconds": 6, "description": "three"}]}]});
        std::fs::write(fx.scout().join("catalog-y.json"), three.to_string()).unwrap();
        let body = fx.get("/creator").body_text();
        assert!(
            body.contains("<details class='cr-more'><summary>More segments (1)</summary>"),
            "{body}"
        );
    }

    #[test]
    fn stop_redirects_back_to_its_tab() {
        let fx = Fx::new();
        let (loc, ok) = flash_of(&fx.post("/creator/stop", "csrf_token=tok&id=nope&tab=videos"));
        assert!(loc.starts_with("/creator?tab=videos&flash="), "{loc}");
        assert_eq!(ok, "0");
    }
}
