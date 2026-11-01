//! The Activity view's top half (`/?view=activity`): the "Latest progress" card
//! (what a running loop is doing, plus the newest steps from the event ledger)
//! and one compact status card per loop. `chat::activity_body` places both above
//! the run reviews; no routes of its own.

use crate::ui::badge::{self, py_title};
use crate::ui::hub;
use crate::ui::nav::{loop_is_visible, registered_loops};
use crate::ui::pages::chat::{configured_topics, topic_progress_text};
use crate::ui::pages::loops_common::{relative_time, run_now_action_html, stop_action_html};
use crate::ui::routes::{Registry, UiCtx};
use crate::ui::{icons, pyfmt};
use loopx_core::loops_config::py_truthy;
use loopx_core::{events, pydumps, status as core_status};
use serde_json::Value;

/// How many ledger steps the progress card lists.
const FEED_LEN: usize = 6;

const BUILTIN_LOOPS: [&str; 3] = ["issue-loop", "topic-loop", "inbox-triage-loop"];

pub fn register(_r: &mut Registry) {}

/// Where one loop stands, as the Activity view shows it.
struct LoopStatus {
    name: String,
    label: String,
    icon: String,
    /// `running`, `idle`, `failed` or `never_run` (the pill colour).
    state: &'static str,
    /// The pill text: what it is doing when running, else its state.
    headline: String,
    updated_at: Option<String>,
    /// The card's button (Run now), when the loop has one and is not running.
    run_action: String,
    /// The Stop button, when the loop is running and can be stopped from here.
    stop_action: String,
}

fn text(v: &Value) -> Option<String> {
    Some(v).filter(|v| py_truthy(v)).map(pydumps::py_str)
}

/// `(state, headline)` from a loop's status file, for loops without their own rules.
fn plain_state(ctx: &UiCtx, status: &Value, exists: bool) -> (&'static str, String) {
    if !exists {
        return ("never_run", ctx.t("Never run"));
    }
    match status.get("state").and_then(Value::as_str).unwrap_or("unknown") {
        "running" if status.get("current_step").is_some_and(py_truthy) => {
            ("running", badge::progress_text(ctx.lang, status))
        }
        "running" => ("running", ctx.t("Running")),
        "failed" => ("failed", ctx.t("Failed")),
        "idle" if status.get("last_exit_code").and_then(Value::as_i64).is_some_and(|c| c != 0) => {
            ("failed", ctx.t("Last run failed"))
        }
        "idle" => ("idle", ctx.t("Idle")),
        other => ("never_run", ctx.t(&py_title(&other.replace('_', " ")))),
    }
}

fn gitlab_status(ctx: &UiCtx, csrf: &str) -> LoopStatus {
    let path = ctx.state.status_path();
    let status = core_status::read_status(&path);
    let (state, headline) = plain_state(ctx, &status, path.exists());
    let headline = if state == "running" {
        badge::progress_text(ctx.lang, &status)
    } else {
        headline
    };
    let has_projects = std::fs::read_to_string(ctx.state.home_dir.join("projects.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|c| c.get("projects").cloned())
        .is_some_and(|p| py_truthy(&p));
    let (run_action, stop_action) = if state == "running" {
        (
            String::new(),
            stop_action_html(
                ctx,
                "/gitlab/stop",
                &ctx.t("Stop the running issue loop? The in-progress issue's work will be abandoned."),
                csrf,
            ),
        )
    } else if has_projects {
        (run_now_action_html(ctx, "/run-now", csrf, None), String::new())
    } else {
        (
            run_now_action_html(
                ctx,
                "/run-now",
                csrf,
                Some(&ctx.t(
                    "No projects configured yet - <a href='/loops/issue-loop?view=projects'>add one on the GitLab page</a>.",
                )),
            ),
            String::new(),
        )
    };
    LoopStatus {
        updated_at: status.get("updated_at").and_then(text),
        run_action,
        stop_action,
        ..identity(ctx, "issue-loop", None, state, headline)
    }
}

/// The topic loop's scheduler status plus each topic's own entry (a topic runs
/// even when the scheduler file says idle).
fn topic_status(ctx: &UiCtx, csrf: &str) -> LoopStatus {
    let path = ctx.state.status_path_for_loop("topic-loop");
    let status = core_status::read_status(&path);
    let per_topic = core_status::read_topic_status(
        &ctx.state.repo_root.join("outputs").join("topic-monitor").join("status.json"),
    );
    let empty = serde_json::Map::new();
    let per_topic = per_topic.get("topics").and_then(Value::as_object).unwrap_or(&empty);
    let topics = configured_topics(ctx.state);
    let topic_running = per_topic
        .values()
        .any(|e| e.get("state").and_then(Value::as_str) == Some("running"));
    let (state, headline) = if topic_running {
        ("running", topic_progress_text(ctx.lang, per_topic, &topics))
    } else if topics.is_empty() {
        ("never_run", ctx.t("Not configured"))
    } else {
        plain_state(ctx, &status, path.exists() || !per_topic.is_empty())
    };
    let updated_at = per_topic
        .values()
        .filter_map(|e| e.get("updated_at").and_then(text))
        .chain(status.get("updated_at").and_then(text))
        .max_by_key(|t| super::loops_common::aware(t));
    let (run_action, stop_action) = if state == "running" {
        (
            String::new(),
            stop_action_html(
                ctx,
                "/topic-monitor/stop",
                &ctx.t("Stop the running topic loop? The in-progress topic's work will be abandoned."),
                csrf,
            ),
        )
    } else if topics.is_empty() {
        (
            run_now_action_html(
                ctx,
                "/topic-monitor/run-now",
                csrf,
                Some(&ctx.t(
                    "No topics configured yet - <a href='/loops/topic-loop?view=topics'>add one on the Topic Settings page</a>.",
                )),
            ),
            String::new(),
        )
    } else {
        (run_now_action_html(ctx, "/topic-monitor/run-now", csrf, None), String::new())
    };
    LoopStatus {
        updated_at,
        run_action,
        stop_action,
        ..identity(ctx, "topic-loop", None, state, headline)
    }
}

fn other_status(ctx: &UiCtx, name: &str, entry: Option<&Value>) -> LoopStatus {
    let path = ctx.state.status_path_for_loop(name);
    let status = core_status::read_status(&path);
    let (state, headline) = plain_state(ctx, &status, path.exists());
    LoopStatus {
        updated_at: status.get("updated_at").and_then(text),
        ..identity(ctx, name, entry, state, headline)
    }
}

fn identity(
    ctx: &UiCtx,
    name: &str,
    entry: Option<&Value>,
    state: &'static str,
    headline: String,
) -> LoopStatus {
    let page = hub::builtin_loop_page(name)
        .unwrap_or_else(|| hub::generic_loop_page(name, entry, false));
    LoopStatus {
        name: name.to_string(),
        label: crate::ui::i18n::t(ctx.lang, &page.label, &[]),
        icon: page.icon,
        state,
        headline,
        updated_at: None,
        run_action: String::new(),
        stop_action: String::new(),
    }
}

/// Every loop the sidebar would list, the built-in three first (even with no
/// registry file), each with its current status.
fn loop_statuses(ctx: &UiCtx) -> Vec<LoopStatus> {
    let registered = registered_loops(ctx.state);
    let entry = |name: &str| {
        registered
            .iter()
            .find(|e| e.get("name").and_then(Value::as_str) == Some(name))
    };
    let csrf = ctx.csrf_input();
    let mut out = Vec::new();
    for name in BUILTIN_LOOPS {
        if entry(name).is_some_and(|e| !loop_is_visible(ctx.state, e)) {
            continue;
        }
        out.push(match name {
            "issue-loop" => gitlab_status(ctx, &csrf),
            "topic-loop" => topic_status(ctx, &csrf),
            _ => other_status(ctx, name, entry(name)),
        });
    }
    for e in &registered {
        let Some(name) = e.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.is_empty() || BUILTIN_LOOPS.contains(&name) || !loop_is_visible(ctx.state, e) {
            continue;
        }
        out.push(other_status(ctx, name, Some(e)));
    }
    out
}

/// The sentence a ledger event reads as, or `None` for bookkeeping events
/// (iterations, policy checks, memory) that say nothing to a person.
fn event_label(ctx: &UiCtx, ev: &Value) -> Option<String> {
    let data = ev.get("data").cloned().unwrap_or(Value::Null);
    let field = |k: &str| data.get(k).and_then(text);
    Some(match ev.get("event_type").and_then(Value::as_str)? {
        "run.started" => ctx.t("Run started"),
        "run.completed" => ctx.t("Run finished"),
        "issue.queued" => ctx.t("Queued"),
        "issue.started" => ctx.t("Started work"),
        "issue.classified" => match field("type") {
            Some(kind) => ctx.tf("Classified as {kind}", &[("kind", &kind)]),
            None => ctx.t("Classified"),
        },
        "verification.started" => ctx.t("Verifying the change"),
        "verification.passed" => ctx.t("Verification passed"),
        "verification.failed" => ctx.t("Verification failed"),
        "issue.completed" => match field("action").as_deref() {
            Some("fix") => ctx.t("Fix done, merge request opened"),
            Some("answer") => ctx.t("Answered on the issue"),
            _ => ctx.t("Completed"),
        },
        "issue.escalated" => match field("reason") {
            Some(reason) => ctx.tf("Escalated: {reason}", &[("reason", &reason.replace('_', " "))]),
            None => ctx.t("Escalated"),
        },
        "loop.failed" => ctx.t("Loop failed"),
        _ => return None,
    })
}

/// The newest meaningful ledger events of today and yesterday (UTC), newest first.
fn recent_events(ctx: &UiCtx) -> Vec<(Value, String)> {
    let today = ctx.state.now().date_naive();
    let since = (today - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
    let until = today.format("%Y-%m-%d").to_string();
    let mut out: Vec<(Value, String)> =
        events::iter_events(Some(&ctx.state.events_dir), Some(&since), Some(&until))
            .into_iter()
            .filter_map(|ev| event_label(ctx, &ev).map(|label| (ev, label)))
            .collect();
    out.sort_by(|a, b| {
        let ts = |e: &Value| e.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();
        ts(&b.0).cmp(&ts(&a.0))
    });
    out.truncate(FEED_LEN);
    out
}

fn feed_html(ctx: &UiCtx, events: &[(Value, String)]) -> String {
    if events.is_empty() {
        return format!(
            "<p class='activity-feed-empty'>{}</p>",
            pyfmt::escape(&ctx.t("No loop activity recorded yet today."))
        );
    }
    let now = ctx.state.now();
    let items: String = events
        .iter()
        .map(|(ev, label)| {
            let ts = ev.get("timestamp").and_then(Value::as_str).unwrap_or("");
            let subject = match (ev.get("project").and_then(text), ev.get("issue_iid").and_then(text)) {
                (Some(p), Some(iid)) => format!("{p} #{iid}"),
                (Some(p), None) => p,
                _ => String::new(),
            };
            let mr = ev
                .get("data")
                .and_then(|d| d.get("mr_url"))
                .and_then(Value::as_str)
                .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
                .map(|u| {
                    format!(
                        " <a href='{}' target='_blank' rel='noopener'>{}</a>",
                        pyfmt::escape(u),
                        pyfmt::escape(&ctx.t("View MR"))
                    )
                })
                .unwrap_or_default();
            format!(
                "<li><time datetime='{ts_attr}' title='{ts_attr}'>{when}</time>\
                 <span class='activity-feed-subject'>{subject}</span>\
                 <span class='activity-feed-label'>{label}{mr}</span></li>",
                ts_attr = pyfmt::escape(ts),
                when = pyfmt::escape(&relative_time(now, ts)),
                subject = pyfmt::escape(&subject),
                label = pyfmt::escape(label),
            )
        })
        .collect();
    format!("<ol class='activity-feed'>{items}</ol>")
}

/// The "Latest progress" card: the first running loop's progress line and its
/// Stop button, or "All loops idle"; then the newest ledger steps; then today's
/// log tail while the GitLab loop runs.
fn now_card_html(ctx: &UiCtx, loops: &[LoopStatus], log_tail: Option<&str>) -> String {
    let events = recent_events(ctx);
    let running = loops.iter().find(|l| l.state == "running");
    let (hero, meta, stop) = match running {
        Some(l) => {
            let (class, icon) = badge::status_badge("running");
            let mut meta = pyfmt::escape(&l.label);
            if let Some(updated) = &l.updated_at {
                meta.push_str(" \u{b7} ");
                meta.push_str(&pyfmt::escape(&ctx.tf(
                    "updated {when}",
                    &[("when", &relative_time(ctx.state.now(), updated))],
                )));
            }
            (
                format!("<span class='pill pill-lg {class}'>{icon}{}</span>", pyfmt::escape(&l.headline)),
                meta,
                l.stop_action.clone(),
            )
        }
        None => {
            let (class, icon) = badge::status_badge("idle");
            let meta = events
                .first()
                .and_then(|(ev, _)| ev.get("timestamp").and_then(Value::as_str))
                .map(|ts| {
                    pyfmt::escape(&ctx.tf(
                        "Last activity {when}",
                        &[("when", &relative_time(ctx.state.now(), ts))],
                    ))
                })
                .unwrap_or_default();
            (
                format!(
                    "<span class='pill pill-lg {class}'>{icon}{}</span>",
                    pyfmt::escape(&ctx.t("All loops idle"))
                ),
                meta,
                String::new(),
            )
        }
    };
    let meta = if meta.is_empty() {
        String::new()
    } else {
        format!("<p class='activity-now-meta'>{meta}</p>")
    };
    let log = match log_tail.filter(|_| running.is_some_and(|l| l.name == "issue-loop")) {
        Some(tail) => format!(
            "<details class='activity-log'><summary>{}</summary><pre class='log'>{}</pre></details>",
            pyfmt::escape(&ctx.t("Today's log (tail)")),
            pyfmt::escape(tail)
        ),
        None => String::new(),
    };
    format!(
        "<section class='card activity-now'>\
         <div class='section-header'>{}<h2>{}</h2></div>\
         <div class='status-hero'>{hero}</div>{meta}{}{log}{stop}</section>",
        icons::icon("monitoring"),
        pyfmt::escape(&ctx.t("Latest progress")),
        feed_html(ctx, &events),
    )
}

fn loop_card_html(ctx: &UiCtx, l: &LoopStatus, focused: bool) -> String {
    let (class, icon) = badge::status_badge(l.state);
    let href = format!("/loops/{}", pyfmt::quote(&l.name, "/"));
    let last = l
        .updated_at
        .as_deref()
        .map(|u| {
            format!(
                "<p class='activity-loop-meta' title='{}'>{}</p>",
                pyfmt::escape(u),
                pyfmt::escape(&ctx.tf(
                    "Last update {when}",
                    &[("when", &relative_time(ctx.state.now(), u))],
                ))
            )
        })
        .unwrap_or_default();
    // The progress card already carries the focused loop's Stop button.
    let action = if focused || l.stop_action.is_empty() { &l.run_action } else { &l.stop_action };
    format!(
        "<section class='card activity-loop' data-loop='{name}'>\
         <div class='section-header'>{}<h2><a href='{href}'>{}</a></h2></div>\
         <div class='status-hero'><span class='pill {class}'>{icon}{}</span></div>{last}{action}</section>",
        l.icon,
        pyfmt::escape(&l.label),
        pyfmt::escape(&l.headline),
        name = pyfmt::escape(&l.name),
    )
}

/// `(progress card, loop cards)` for the Activity view.
pub fn activity_top_html(ctx: &UiCtx, log_tail: Option<&str>) -> (String, String) {
    let loops = loop_statuses(ctx);
    let focused = loops.iter().position(|l| l.state == "running");
    let cards: String = loops
        .iter()
        .enumerate()
        .map(|(i, l)| loop_card_html(ctx, l, Some(i) == focused))
        .collect();
    (
        now_card_html(ctx, &loops, log_tail),
        format!("<div class='activity-loops'>{cards}</div>"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::routes::UiRequest;
    use crate::ui::state::UiState;
    use chrono::{TimeZone, Utc};

    fn state(dir: &std::path::Path) -> UiState {
        let now = Utc.with_ymd_and_hms(2026, 10, 28, 14, 0, 0).unwrap();
        UiState::explicit(dir, dir, dir, dir, "tok", Some(now))
    }

    fn write(path: std::path::PathBuf, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn event(ts: &str, kind: &str, data: &str) -> String {
        format!(
            r#"{{"timestamp": "{ts}", "event_type": "{kind}", "run_id": "run_1", "project": "camp", "issue_iid": 438, "data": {data}}}"#
        )
    }

    fn render(state: &UiState) -> (String, String) {
        let req = UiRequest::get("/?view=activity");
        activity_top_html(&UiCtx::new(state, &req), Some("line one\nline two"))
    }

    #[test]
    fn a_running_gitlab_loop_heads_the_progress_card_with_its_newest_steps_first() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path());
        write(
            state.status_path(),
            r#"{"state": "running", "updated_at": "2026-10-28T13:58:00+00:00", "current_issue": "camp #438", "current_step": "verifying"}"#,
        );
        write(
            state.events_dir.join("2026-10-28.jsonl"),
            &[
                event("2026-10-28T13:40:00.000Z", "issue.started", "{}"),
                event("2026-10-28T13:41:00.000Z", "policy.allowed", "{}"),
                event("2026-10-28T13:42:00.000Z", "issue.classified", r#"{"type": "feature"}"#),
                event("2026-10-28T13:43:00.000Z", "verification.started", "{}"),
            ]
            .join("\n"),
        );
        let (now, cards) = render(&state);
        assert!(now.contains("Processing camp #438 — Running verification"), "{now}");
        assert!(now.contains("action='/gitlab/stop'"), "{now}");
        assert!(now.contains("line two"), "the log tail shows while GitLab runs: {now}");
        let verifying = now.find("Verifying the change").expect("verification step");
        let classified = now.find("Classified as feature").expect("classified step");
        let started = now.find("Started work").expect("started step");
        assert!(verifying < classified && classified < started, "newest first: {now}");
        assert!(!now.contains("policy"), "bookkeeping events are skipped: {now}");
        assert!(now.contains("camp #438"), "{now}");
        // The Stop button lives in the progress card, not twice.
        assert!(!cards.contains("/gitlab/stop"), "{cards}");
    }

    #[test]
    fn with_nothing_running_the_card_says_all_idle_and_when_it_last_moved() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path());
        write(state.status_path(), r#"{"state": "idle", "updated_at": "2026-10-28T12:00:00+00:00"}"#);
        write(
            state.events_dir.join("2026-10-28.jsonl"),
            &event(
                "2026-10-28T12:00:00.000Z",
                "issue.completed",
                r#"{"action": "fix", "mr_url": "https://gitlab.example.com/a/b/-/merge_requests/9"}"#,
            ),
        );
        let (now, _) = render(&state);
        assert!(now.contains("All loops idle"), "{now}");
        assert!(now.contains("Last activity 2h ago"), "{now}");
        assert!(now.contains("Fix done, merge request opened"), "{now}");
        assert!(now.contains("href='https://gitlab.example.com/a/b/-/merge_requests/9'"), "{now}");
        assert!(!now.contains("/stop"), "{now}");
        assert!(!now.contains("line two"), "no log tail when idle: {now}");
    }

    #[test]
    fn an_empty_ledger_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (now, _) = render(&state(dir.path()));
        assert!(now.contains("No loop activity recorded yet today."), "{now}");
    }

    #[test]
    fn every_visible_loop_gets_a_status_card_in_sidebar_order() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path());
        write(
            state.loops_config_path(),
            r#"[{"name": "issue-loop", "enabled": true}, {"name": "topic-loop", "enabled": true},
                {"name": "inbox-triage-loop", "enabled": true},
                {"name": "daily-digest-loop", "label": "Daily Digest", "enabled": true},
                {"name": "rss-watch-loop", "label": "RSS Watch", "enabled": false}]"#,
        );
        write(
            state.status_path_for_loop("daily-digest-loop"),
            r#"{"state": "idle", "updated_at": "2026-10-27T14:00:00+00:00", "last_exit_code": 1}"#,
        );
        write(
            state.status_path_for_loop("inbox-triage-loop"),
            r#"{"state": "idle", "updated_at": "2026-10-28T13:00:00+00:00", "last_exit_code": 0}"#,
        );
        let (_, cards) = render(&state);
        let pos = |name: &str| cards.find(&format!("data-loop='{name}'")).unwrap_or_else(|| panic!("{name}: {cards}"));
        assert!(pos("issue-loop") < pos("topic-loop"));
        assert!(pos("topic-loop") < pos("inbox-triage-loop"));
        assert!(pos("inbox-triage-loop") < pos("daily-digest-loop"));
        assert!(!cards.contains("rss-watch-loop"), "a disabled loop that never ran is hidden");
        let digest = &cards[pos("daily-digest-loop")..];
        assert!(digest.contains("Daily Digest") && digest.contains("Last run failed"), "{digest}");
        assert!(digest.contains("pill-red"), "{digest}");
        let inbox = &cards[pos("inbox-triage-loop")..pos("daily-digest-loop")];
        assert!(inbox.contains("Idle") && inbox.contains("Last update 1h ago"), "{inbox}");
        // No projects and no topics yet: both buttons say how to set them up.
        assert!(cards.contains("No projects configured yet"), "{cards}");
        assert!(cards.contains("No topics configured yet"), "{cards}");
        assert!(cards.contains("Never run"), "{cards}");
    }
}
