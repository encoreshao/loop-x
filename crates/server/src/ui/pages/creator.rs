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

const TABS: [&str; 3] = ["discover", "videos", "publish"];
/// A GET without `Range` larger than this is refused (players always send ranges).
const MAX_FULL_BYTES: u64 = 64 * 1024 * 1024;
/// The most one range response carries; an open or larger range is cut to this.
const MAX_RANGE_BYTES: u64 = 8 * 1024 * 1024;
const JOBS_SHOWN: usize = 10;
/// What a scout job searches for when the topic field is left empty.
const OPEN_SCOUT_QUERY: &str =
    "topic-free discovery: a few of the most worthwhile recent public talks and interviews";
const PLATFORMS: [(&str, &str); 2] = [("wechat", "WeChat"), ("douyin", "Douyin")];

pub fn register(r: &mut Registry) {
    r.get("/creator", page);
    r.get("/creator/media/:slug/:file", media);
    r.get("/creator/jobs/:id/log", job_log);
    r.post("/creator/generate", generate);
    r.post("/creator/scout", start_scout);
    r.post("/creator/stop", stop);
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
    TABS.iter()
        .find(|t| **t == requested)
        .copied()
        .unwrap_or("discover")
}

fn back(tab: &str) -> String {
    format!("/creator?tab={}", tab_of(tab))
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
    let to = back("videos");
    if !known_slug(ctx, slug) {
        return UiResponse::flash_redirect(false, &ctx.t("No such video"), &to);
    }
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
    let to = back("publish");
    if !known_slug(ctx, slug) {
        return UiResponse::flash_redirect(false, &ctx.t("No such video"), &to);
    }
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

/// The end of a job's log as plain text (never rendered as HTML); 404 for an unsafe
/// or unknown job id.
fn job_log(ctx: &UiCtx) -> UiResponse {
    match jobs::log_tail(&jobs_dir(ctx), ctx.param("id")) {
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

fn card(icon: &str, title: &str, body: &str) -> String {
    format!(
        "<div class='card'><div class='section-header'>{}<h2>{}</h2></div>{body}</div>",
        icons::icon(icon),
        h(title)
    )
}

fn subtitle(text: &str) -> String {
    format!("<p class='section-subtitle'>{}</p>", h(text))
}

fn tabs_html(ctx: &UiCtx, active: &str) -> String {
    let labels = [
        ("discover", ctx.t("Discover")),
        ("videos", ctx.t("Videos")),
        ("publish", ctx.t("Publish")),
    ];
    let links: String = labels
        .iter()
        .map(|(key, label)| {
            let on = *key == active;
            format!(
                "<a class='{}' href='/creator?tab={key}'{}>{}</a>",
                if on { "hub-tab active" } else { "hub-tab" },
                if on { " aria-current='page'" } else { "" },
                h(label)
            )
        })
        .collect();
    format!(
        "<nav class='hub-tabs' aria-label='{}'>{links}</nav>",
        h(&ctx.t("Page sections"))
    )
}

fn state_pill(ctx: &UiCtx, state: &str) -> String {
    let (class, label) = match state {
        "running" => ("pill-grey", ctx.t("Running")),
        "done" => ("pill-green", ctx.t("Done")),
        "failed" => ("pill-red", ctx.t("Failed")),
        "stopped" => ("pill-grey", ctx.t("Stopped")),
        other => ("pill-grey", other.to_string()),
    };
    format!("<span class='pill {class}'>{}</span>", h(&label))
}

fn stop_form(ctx: &UiCtx, job: &JobRecord, tab: &str) -> String {
    format!(
        "<form method='post' action='/creator/stop' class='daemon-action-form'>{}\
<input type='hidden' name='id' value=\"{}\"><input type='hidden' name='tab' value='{tab}'>\
<button type='submit' class='btn btn-neutral'>{} {}</button></form>",
        ctx.csrf_input(),
        h(&job.id),
        icons::icon("stop"),
        h(&ctx.t("Stop"))
    )
}

fn job_row(ctx: &UiCtx, job: &JobRecord, tab: &str) -> String {
    let kind = match job.kind {
        JobKind::Scout => ctx.t("Find videos"),
        JobKind::Generate => ctx.t("Generate"),
    };
    let stop = if job.state == "running" {
        stop_form(ctx, job, tab)
    } else {
        String::new()
    };
    let action = format!(
        "<a class='btn btn-neutral' href='/creator/jobs/{}/log' target='_blank' rel='noopener noreferrer'>{} {}</a>{stop}",
        h(&job.id),
        icons::icon("description"),
        h(&ctx.t("View log"))
    );
    format!(
        "<div class='project-block account-row'><div class='account-main'><strong>{}</strong> {}\
<div class='section-subtitle'>{}</div></div><div class='account-actions'>{action}</div></div>",
        h(&kind),
        state_pill(ctx, &job.state),
        h(&joined(&[job.key.clone(), when(&job.started_at)]))
    )
}

fn errors_card(ctx: &UiCtx, errors: &[String]) -> String {
    if errors.is_empty() {
        return String::new();
    }
    let rows: String = errors.iter().map(|e| flash_html(Some(e), false)).collect();
    card("warning", &ctx.t("Some files could not be read"), &rows)
}

fn generate_form(ctx: &UiCtx, url: &str) -> String {
    format!(
        "<form method='post' action='/creator/generate'>{}\
<input type='hidden' name='url' value=\"{}\">\
<label class='section-subtitle'><input type='checkbox' name='rights' value='1' required> {}</label> \
<button type='submit' class='btn btn-primary'>{} {}</button></form>",
        ctx.csrf_input(),
        h(url),
        h(&ctx.t("I own this video or have permission to reuse it")),
        icons::icon("video"),
        h(&ctx.t("Generate"))
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
            "<a href=\"{}\" target='_blank' rel='noopener noreferrer'><strong>{}</strong></a>",
            h(u),
            h(&title)
        ),
        None => format!("<strong>{}</strong>", h(&title)),
    };
    let quality = joined(&[
        s.grade.clone().unwrap_or_default(),
        s.score.map(|v| format!("{v:.1}")).unwrap_or_default(),
    ]);
    let quality = if quality.is_empty() {
        String::new()
    } else {
        format!(" <span class='pill pill-grey'>{}</span>", h(&quality))
    };
    let ready = if s.handoff_ready {
        format!(
            " <span class='pill pill-green'>{}</span>",
            h(&ctx.t("Ready to generate"))
        )
    } else {
        String::new()
    };
    let meta = joined(&[
        s.channel.clone(),
        s.platform.clone(),
        s.published.clone().unwrap_or_default(),
        s.duration_s.map(clock).unwrap_or_default(),
    ]);
    let segments: String = s
        .segments
        .iter()
        .map(|seg| {
            let span = match (&seg.start, &seg.end) {
                (Some(a), Some(b)) => format!("{a}–{b} "),
                (Some(a), None) => format!("{a} "),
                _ => String::new(),
            };
            format!("<li>{}{}</li>", h(&span), h(&seg.idea))
        })
        .collect();
    let segments = if segments.is_empty() {
        String::new()
    } else {
        format!(
            "<div class='section-subtitle'>{}</div><ul>{segments}</ul>",
            h(&ctx.t("Suggested segments"))
        )
    };
    // Only a source whose address passes the job's own URL check can be generated.
    let usable = valid_source_url(&s.url).ok();
    let running = usable
        .as_deref()
        .and_then(|u| jobs::running(jobs, JobKind::Generate, u));
    let action = match (running, usable) {
        (Some(job), _) => format!(
            "<span class='pill pill-grey'>{}</span>{}",
            h(&ctx.t("Rendering")),
            stop_form(ctx, job, "discover")
        ),
        (None, Some(url)) => generate_form(ctx, &url),
        (None, None) => String::new(),
    };
    format!(
        "<div class='project-block account-row'><div class='account-main'>\
<span class='section-subtitle'>#{}</span> {title_html}{quality}{ready}\
<div class='section-subtitle'>{}</div><p>{}</p>{segments}</div>\
<div class='account-actions'>{action}</div></div>",
        s.rank,
        h(&meta),
        h(&s.rationale)
    )
}

fn run_card(ctx: &UiCtx, run: &ScoutRun, jobs: &[JobRecord]) -> String {
    let title = match run.query.as_deref() {
        Some(q) => ctx.tf("Search: {query}", &[("query", q)]),
        None => ctx.t("Topic-free discovery"),
    };
    let rows: String = if run.sources.is_empty() {
        subtitle(&ctx.t("This run found no videos."))
    } else {
        run.sources
            .iter()
            .map(|s| source_row(ctx, s, jobs))
            .collect()
    };
    let meta = joined(&[when(&run.generated_at), run.mode.clone()]);
    card("search", &title, &format!("{}{rows}", subtitle(&meta)))
}

fn discover_tab(ctx: &UiCtx, jobs: &[JobRecord]) -> String {
    let root = scout::scout_root(&ctx.state.user_home);
    let (runs, errors) = scout::list_runs(&root);
    let find = card(
        "search",
        &ctx.t("Find videos"),
        &format!(
            "{}<form method='post' action='/creator/scout' class='daemon-action-form'>{}\
<input type='text' name='query' placeholder=\"{}\" aria-label=\"{}\"> \
<button type='submit' class='btn btn-primary'>{} {}</button></form>",
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
    let jobs_card = if jobs.is_empty() {
        String::new()
    } else {
        let rows: String = jobs
            .iter()
            .take(JOBS_SHOWN)
            .map(|j| job_row(ctx, j, "discover"))
            .collect();
        card("schedule", &ctx.t("Jobs"), &rows)
    };
    let runs_html: String = if runs.is_empty() {
        card(
            "video",
            &ctx.t("Scout runs"),
            &subtitle(&ctx.tf(
                "No video-scout catalogs yet. Find videos above, or run the video-scout skill in Claude Code; catalogs in {path} show up here.",
                &[("path", &root.display().to_string())],
            )),
        )
    } else {
        runs.iter().map(|r| run_card(ctx, r, jobs)).collect()
    };
    format!("{find}{jobs_card}{}{runs_html}", errors_card(ctx, &errors))
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
    card(
        "video",
        &ctx.t("Videos"),
        &subtitle(&ctx.tf(
            "No videos yet. Generate one from a source under Discover; finished videos in {path} show up here.",
            &[("path", &root.display().to_string())],
        )),
    )
}

fn video_card(ctx: &UiCtx, p: &VideoProject, st: &CreatorState, jobs: &[JobRecord]) -> String {
    let source_key = p
        .source_url
        .as_deref()
        .and_then(|u| valid_source_url(u).ok());
    let rendering = source_key
        .as_deref()
        .and_then(|u| jobs::running(jobs, JobKind::Generate, u));
    let approved = st.approved.contains(&p.slug);
    let (class, status) = if rendering.is_some() {
        ("pill-grey", ctx.t("Rendering"))
    } else if approved {
        ("pill-green", ctx.t("Approved"))
    } else if p.mp4.is_some() {
        ("pill-grey", ctx.t("Ready for review"))
    } else {
        ("pill-red", ctx.t("No video file"))
    };
    let player = match &p.mp4 {
        Some(mp4) => {
            let poster = p
                .cover
                .as_deref()
                .map(|c| format!(" poster=\"{}\"", media_url(&p.slug, c)))
                .unwrap_or_default();
            format!(
                "<video controls preload='metadata' src=\"{}\"{poster} \
style='display:block;width:100%;max-height:540px;background:#000;border-radius:8px;'></video>",
                media_url(&p.slug, mp4)
            )
        }
        None => String::new(),
    };
    let source = match p.source_url.as_deref().and_then(https_link) {
        Some(u) => format!(
            "<a class='btn btn-neutral' href=\"{}\" target='_blank' rel='noopener noreferrer'>{} {}</a>",
            h(u),
            icons::icon("open_in_new"),
            h(&ctx.t("Source"))
        ),
        None => String::new(),
    };
    let toggle = if approved || (rendering.is_none() && p.mp4.is_some()) {
        let (on, label, icon) = if approved {
            ("0", ctx.t("Remove approval"), "close")
        } else {
            ("1", ctx.t("Approve"), "check_circle")
        };
        format!(
            "<form method='post' action='/creator/approve' class='daemon-action-form'>{}\
<input type='hidden' name='slug' value=\"{}\"><input type='hidden' name='on' value='{on}'>\
<button type='submit' class='btn btn-primary'>{} {}</button></form>",
            ctx.csrf_input(),
            h(&p.slug),
            icons::icon(icon),
            h(&label)
        )
    } else {
        String::new()
    };
    let stop = rendering
        .map(|j| stop_form(ctx, j, "videos"))
        .unwrap_or_default();
    format!(
        "<div class='card'><div class='section-header'>{}<h2>{}</h2></div>\
<div class='pill-row'><span class='pill {class}'>{}</span></div>{}{player}\
<div class='account-actions'>{toggle}{source}{stop}</div></div>",
        icons::icon("video"),
        h(&p.title),
        h(&status),
        subtitle(&when(p.created_at.as_deref().unwrap_or("")))
    )
}

fn videos_tab(ctx: &UiCtx, jobs: &[JobRecord]) -> String {
    let root = videos_root(ctx);
    let (projects, errors) = videos::list_projects(&root);
    let st = cstate::load(&state_path(ctx));
    let known: Vec<String> = projects
        .iter()
        .filter_map(|p| p.source_url.as_deref())
        .map(|u| valid_source_url(u).unwrap_or_else(|_| u.to_string()))
        .collect();
    let pending: String = jobs
        .iter()
        .filter(|j| j.kind == JobKind::Generate && !known.contains(&j.key))
        .filter(|j| j.state == "running" || j.state == "failed")
        .take(JOBS_SHOWN)
        .map(|j| job_row(ctx, j, "videos"))
        .collect();
    let pending = if pending.is_empty() {
        String::new()
    } else {
        card("schedule", &ctx.t("Generating"), &pending)
    };
    let list = if projects.is_empty() {
        no_videos(ctx, &root)
    } else {
        let cards: String = projects
            .iter()
            .map(|p| video_card(ctx, p, &st, jobs))
            .collect();
        format!("<div class='grid'>{cards}</div>")
    };
    format!("{pending}{}{list}", errors_card(ctx, &errors))
}

fn copy_button(ctx: &UiCtx, value: &str, label: &str) -> String {
    format!(
        "<button type='button' class='btn btn-neutral' data-copy=\"{}\" data-copied-label=\"{}\">{} <span data-copy-label>{}</span></button>",
        h(value),
        h(&ctx.t("Copied")),
        icons::icon("content_copy"),
        h(label)
    )
}

/// A labelled read-only field with its Copy button; "Not available" and no button
/// when there is nothing to copy.
fn copy_field(ctx: &UiCtx, label: &str, value: &str, multiline: bool) -> String {
    if value.trim().is_empty() {
        return format!(
            "<div class='section-subtitle'>{}</div>{}",
            h(label),
            subtitle(&ctx.t("Not available"))
        );
    }
    let field = if multiline {
        format!(
            "<textarea readonly rows='4' aria-label=\"{}\" style='flex:1;'>{}</textarea>",
            h(label),
            h(value)
        )
    } else {
        format!(
            "<input type='text' readonly value=\"{}\" aria-label=\"{}\" style='flex:1;'>",
            h(value),
            h(label)
        )
    };
    format!(
        "<div class='section-subtitle'>{}</div>\
<div style='display:flex;gap:0.5rem;align-items:flex-start;margin-bottom:0.5rem;'>{field}{}</div>",
        h(label),
        copy_button(ctx, value, &ctx.t("Copy"))
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

fn platform_block(
    ctx: &UiCtx,
    slug: &str,
    (key, label): (&str, &str),
    copy: Option<&PublishCopy>,
    is_published: bool,
) -> String {
    let pill = if is_published {
        format!(
            " <span class='pill pill-green'>{}</span>",
            h(&ctx.t("Published"))
        )
    } else {
        String::new()
    };
    let body = match copy {
        None => subtitle(&ctx.t("Not available")),
        Some(c) => {
            let tags = hashtags(c);
            let all = [c.title.as_str(), c.caption.as_str(), tags.as_str()]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join("\n\n");
            let (on, toggle) = if is_published {
                ("0", ctx.t("Mark as not published"))
            } else {
                ("1", ctx.t("Mark as published"))
            };
            let copy_all = if all.trim().is_empty() {
                String::new()
            } else {
                copy_button(ctx, &all, &ctx.t("Copy all"))
            };
            format!(
                "{}{}{}<div class='account-actions'>{copy_all}\
<form method='post' action='/creator/published' class='daemon-action-form'>{}\
<input type='hidden' name='slug' value=\"{}\"><input type='hidden' name='platform' value='{key}'>\
<input type='hidden' name='on' value='{on}'><button type='submit' class='btn btn-primary'>{} {}</button></form></div>",
                copy_field(ctx, &ctx.t("Title"), &c.title, false),
                copy_field(ctx, &ctx.t("Caption"), &c.caption, true),
                copy_field(ctx, &ctx.t("Hashtags"), &tags, false),
                ctx.csrf_input(),
                h(slug),
                icons::icon("send"),
                h(&toggle)
            )
        }
    };
    format!(
        "<div class='project-block'><h3>{}{pill}</h3>{body}</div>",
        h(label)
    )
}

fn publish_tab(ctx: &UiCtx) -> String {
    let root = videos_root(ctx);
    let (projects, errors) = videos::list_projects(&root);
    let st = cstate::load(&state_path(ctx));
    let list: String = if projects.is_empty() {
        no_videos(ctx, &root)
    } else {
        projects
            .iter()
            .map(|p| {
                let done = st.published.get(&p.slug);
                let blocks: String = PLATFORMS
                    .iter()
                    .map(|(key, label)| {
                        let copy = match *key {
                            "wechat" => p.wechat.as_ref(),
                            _ => p.douyin.as_ref(),
                        };
                        let is_published = done.is_some_and(|d| d.iter().any(|x| x == key));
                        platform_block(ctx, &p.slug, (key, label), copy, is_published)
                    })
                    .collect();
                card(
                    "send",
                    &p.title,
                    &format!("<div class='grid'>{blocks}</div>"),
                )
            })
            .collect()
    };
    format!("{}{list}", errors_card(ctx, &errors))
}

fn page(ctx: &UiCtx) -> UiResponse {
    let tab = tab_of(ctx.query().first("tab").unwrap_or(""));
    let (flash, ok) = ctx.flash();
    let jobs = jobs::list_jobs(&jobs_dir(ctx));
    let body = match tab {
        "videos" => videos_tab(ctx, &jobs),
        "publish" => publish_tab(ctx),
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
        &format!("{head}{body}"),
        ShellOpts::default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{range_response, when, MAX_FULL_BYTES, MAX_RANGE_BYTES};
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
        let publish = fx.get("/creator?tab=publish").body_text();
        assert!(publish.contains("No videos yet"), "{publish}");
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
    fn videos_tab_shows_player_cover_and_status() {
        let fx = Fx::new();
        write_project(&fx, "T", true);
        let body = fx.get("/creator?tab=videos").body_text();
        assert!(
            body.contains("<video controls preload='metadata'"),
            "{body}"
        );
        assert!(
            body.contains("src=\"/creator/media/demo/demo.mp4\""),
            "{body}"
        );
        assert!(
            body.contains("poster=\"/creator/media/demo/cover.png\""),
            "{body}"
        );
        assert!(
            body.contains("<span class='pill pill-grey'>Ready for review</span>"),
            "{body}"
        );
        assert!(body.contains("action='/creator/approve'"));
        fx.running_job(SOURCE);
        let body = fx.get("/creator?tab=videos").body_text();
        assert!(
            body.contains("<span class='pill pill-grey'>Rendering</span>"),
            "{body}"
        );
        assert!(!body.contains("action='/creator/approve'"), "{body}");
    }

    #[test]
    fn publish_tab_escapes_hostile_copy() {
        let fx = Fx::new();
        write_project(&fx, "\"><script>alert(1)</script>", true);
        let body = fx.get("/creator?tab=publish").body_text();
        assert!(!body.contains("<script>alert(1)"), "{body}");
        assert!(!body.contains("\"><script>"), "{body}");
        assert!(
            body.contains("data-copy=\"&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;\""),
            "{body}"
        );
        assert!(body.contains("it&#x27;s a caption"), "{body}");
        assert!(body.contains("#grow #learn"), "{body}");
        assert!(body.contains("Copy all"));
        assert!(body.contains("action='/creator/published'"));
    }

    #[test]
    fn publish_missing_platform_shows_not_available() {
        let fx = Fx::new();
        write_project(&fx, "T", false);
        let body = fx.get("/creator?tab=publish").body_text();
        assert!(
            body.contains("Douyin") && body.contains("Not available"),
            "{body}"
        );
        assert!(body.contains("WeChat"));
    }

    #[test]
    fn approve_and_published_toggles_persist() {
        let fx = Fx::new();
        write_project(&fx, "T", true);
        let (loc, ok) = flash_of(&fx.post("/creator/approve", "csrf_token=tok&slug=demo&on=1"));
        assert!(loc.starts_with("/creator?tab=videos&flash="), "{loc}");
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
        assert!(loc.starts_with("/creator?tab=publish&flash="), "{loc}");
        assert_eq!(ok, "1");
        assert_eq!(
            cstate::load(&fx.state_path()).published.get("demo"),
            Some(&vec!["wechat".to_string()])
        );
        assert!(fx
            .get("/creator?tab=publish")
            .body_text()
            .contains("Published"));

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

    #[test]
    fn a_failed_job_shows_failed_and_links_its_log() {
        let fx = Fx::new();
        let id = "20261105T000000-failed00";
        finished_job(&fx, id, "boom\nloopx-exit=3\n");
        for tab in ["discover", "videos"] {
            let body = fx.get(&format!("/creator?tab={tab}")).body_text();
            assert!(
                body.contains("<span class='pill pill-red'>Failed</span>"),
                "{tab}: {body}"
            );
            assert!(!body.contains(">Done</span>"), "{tab}: {body}");
            assert!(
                body.contains(&format!("href='/creator/jobs/{id}/log'")),
                "{tab}: {body}"
            );
            assert!(body.contains("View log"), "{tab}: {body}");
        }
        let ok = "20261105T000001-success0";
        finished_job(&fx, ok, "loopx-exit=0\n");
        let body = fx.get("/creator").body_text();
        assert!(
            body.contains("<span class='pill pill-green'>Done</span>"),
            "{body}"
        );
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
    fn empty_publish_fields_say_not_available_without_a_copy_button() {
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
        let body = fx.get("/creator?tab=publish").body_text();
        // Only Douyin's title and its Copy all have anything to copy.
        assert_eq!(body.matches("data-copy=").count(), 2, "{body}");
        assert!(body.contains("data-copy=\"Only a title\""), "{body}");
        assert!(!body.contains("data-copy=\"\""), "{body}");
        assert!(body.matches("Not available").count() >= 5, "{body}");
    }

    #[test]
    fn stop_redirects_back_to_its_tab() {
        let fx = Fx::new();
        let (loc, ok) = flash_of(&fx.post("/creator/stop", "csrf_token=tok&id=nope&tab=videos"));
        assert!(loc.starts_with("/creator?tab=videos&flash="), "{loc}");
        assert_eq!(ok, "0");
    }
}
