//! Maps each loop to one of the office view's five states.

use crate::ui::pages::{loops, needs_you};
use crate::ui::pyfmt;
use crate::ui::routes::UiCtx;
use loopx_core::approvals;
use loopx_core::loops_config::py_truthy;
use loopx_core::office_stage::{self, Stage};
use loopx_core::pydumps::py_str;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficeState {
    Disabled,
    Waiting,
    Failed,
    Working,
    Idle,
}

impl OfficeState {
    pub fn as_str(self) -> &'static str {
        match self {
            OfficeState::Disabled => "disabled",
            OfficeState::Waiting => "waiting",
            OfficeState::Failed => "failed",
            OfficeState::Working => "working",
            OfficeState::Idle => "idle",
        }
    }
}

/// Precedence: disabled, waiting, failed, working, idle. `pending_approvals` is
/// what the caller attributes to this loop (see [`pending_for`]).
pub fn office_state(enabled: bool, run_state: &str, pending_approvals: usize) -> OfficeState {
    if !enabled {
        return OfficeState::Disabled;
    }
    if pending_approvals > 0 {
        return OfficeState::Waiting;
    }
    match run_state {
        "failed" | "stale" => OfficeState::Failed,
        "running" => OfficeState::Working,
        _ => OfficeState::Idle,
    }
}

/// Which loop a Needs you item waits on: the issue loop's approvals,
/// escalations, review threads and specs, a failed loop itself, Pipeline
/// Auto-fix's escalations; anything else (a sign-in, the Slack listener) is nobody's.
pub fn owner_of(item: &Value) -> Option<String> {
    match item.get("kind").and_then(Value::as_str)? {
        "approval" if item.get("approval_kind").is_some() => None,
        "approval" | "escalated" | "mr_thread" | "spec" => Some("issue-loop".to_string()),
        "loop_failed" => item.get("loop").and_then(Value::as_str).map(str::to_string),
        "mr_autofix" => Some(loopx_core::mr_autofix::LOOP_NAME.to_string()),
        _ => None,
    }
}

fn stage_label(ctx: &UiCtx, stage: Stage) -> String {
    match stage {
        Stage::Classifying => ctx.t("Classifying"),
        Stage::AwaitingSpec => ctx.t("Awaiting spec approval"),
        Stage::Coding => ctx.t("Coding"),
        Stage::Verifying => ctx.t("Verifying (tests)"),
        Stage::Reviewing => ctx.t("Reviewing"),
        Stage::SecurityScan => ctx.t("Security scan"),
        Stage::AwaitingYou => ctx.t("Awaiting you"),
        Stage::Blocked => ctx.t("Blocked"),
    }
}

/// The Office panel's card for one Needs you item, linking to its row.
fn needs_you_card(ctx: &UiCtx, item: &Value) -> Value {
    let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
    json!({
        "id": id,
        "kind": item.get("kind"),
        "title": needs_you::headline(ctx, item),
        "since": item.get("since"),
        "href": format!("/needs-you#{}", needs_you::anchor_id(id)),
    })
}

const MAX_NEEDS_YOU_CARDS: usize = 3;

/// Approvals carry no loop name and only the GitLab issue loop creates them, so
/// the whole pending total belongs to it and to no other loop.
pub fn pending_for(loop_name: &str, pending_total: usize) -> usize {
    if loop_name == "issue-loop" {
        pending_total
    } else {
        0
    }
}

fn reason(ctx: &UiCtx, state: OfficeState) -> String {
    reason_with(ctx, state, true)
}

fn reason_with(ctx: &UiCtx, state: OfficeState, approval: bool) -> String {
    match state {
        OfficeState::Disabled => ctx.t("Disabled"),
        OfficeState::Waiting if !approval => ctx.t("Waiting for you"),
        OfficeState::Waiting => ctx.t("Waiting for your approval"),
        OfficeState::Failed => ctx.t("The last run failed"),
        OfficeState::Working => ctx.t("Working on a run right now"),
        OfficeState::Idle => ctx.t("Idle until the next run"),
    }
}

fn run_now_url(name: &str) -> String {
    match name {
        "issue-loop" => "/run-now".to_string(),
        "topic-loop" => "/topic-monitor/run-now".to_string(),
        "inbox-triage-loop" => "/inbox/run-now".to_string(),
        _ => format!("/loops/{}/run-now", pyfmt::quote(name, "")),
    }
}

/// The Creator's Find videos and Generate video jobs as office entries (`kind: "creator"`).
/// They have no schedule, Run now or toggle; the office hands them to its video colleague.
fn creator_entries(ctx: &UiCtx) -> Vec<Value> {
    let state = ctx.state;
    let dir = loopx_core::creator::jobs::jobs_dir(&state.home_dir);
    let settled = loopx_core::creator::jobs::settle_outputs(
        loopx_core::creator::jobs::list_jobs(&dir),
        &state.user_home,
    );
    loopx_core::creator::notify::loop_statuses(&dir, &settled, chrono::Utc::now())
        .into_iter()
        .map(|(name, doc)| {
            let run_state = doc.get("state").and_then(Value::as_str).unwrap_or("idle");
            let os = office_state(true, run_state, 0);
            let title = if name == loopx_core::creator::notify::FIND_LOOP {
                ctx.t("Find videos")
            } else {
                ctx.t("Generate video")
            };
            json!({
                "name": name,
                "title": title,
                "enabled": true,
                "state": os.as_str(),
                "reason": reason(ctx, os),
                "last_run": doc.get("updated_at").and_then(Value::as_str),
                "step": (os == OfficeState::Working)
                    .then(|| doc.get("current_step").and_then(Value::as_str).filter(|s| !s.is_empty()))
                    .flatten(),
                "next_run": Value::Null,
                "history": [],
                "run_now_url": Value::Null,
                "toggle_url": Value::Null,
                "pending_approvals": 0,
                "stage": Value::Null,
                "stage_label": Value::Null,
                "stage_scope": Value::Null,
                "needs_you": [],
                "kind": "creator",
                "href": "/creator",
            })
        })
        .collect()
}

/// The `/office/state` document: one entry per registered loop.
pub fn build_office_state(ctx: &UiCtx) -> Value {
    let state = ctx.state;
    let now = state.now();
    let pending_total = approvals::list(
        &state.repo_root.join("outputs").join("approvals"),
        Some("pending"),
        now,
    )
    .len();
    let waiting_items = needs_you::items(state);
    let today = now.format("%Y-%m-%d").to_string();
    let yesterday = (now - chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let issue_events =
        loopx_core::events::iter_events(Some(&state.events_dir), Some(&yesterday), Some(&today));
    let issue_active = office_stage::issue_stages(&issue_events);
    let issue_run_start = office_stage::latest_run_start(&issue_events);
    let mut entries = Vec::new();
    for l in loops::list_loops(state).unwrap_or_default() {
        if !l.is_object() {
            continue;
        }
        let name = l.get("name").map_or_else(|| "?".to_string(), py_str);
        let enabled = l.get("enabled").is_none_or(py_truthy);
        let status = loopx_core::status::read_status(&state.status_path_for_loop(&name));
        let run_state = crate::snapshot::effective_state(&status);
        let pending = pending_for(&name, pending_total);
        let mine: Vec<&Value> = waiting_items
            .iter()
            .filter(|i| owner_of(i).as_deref() == Some(name.as_str()))
            .collect();
        let kind_is = |i: &&&Value, k: &str| i.get("kind").and_then(Value::as_str) == Some(k);
        let failed_item = mine.iter().any(|i| kind_is(&i, "loop_failed"));
        let specs = mine.iter().filter(|i| kind_is(i, "spec")).count();
        let decisions = mine
            .iter()
            .filter(|i| !kind_is(i, "loop_failed") && !kind_is(i, "spec"))
            .count();
        let os = office_state(enabled, &run_state, pending.max(decisions + specs));
        let current =
            office_stage::since_run_start(&issue_active, issue_run_start.as_deref().unwrap_or(""));
        let (active, awaiting_spec): (&[_], usize) = if name == "issue-loop" {
            (&current, specs)
        } else {
            (&[], 0)
        };
        let stage = if enabled {
            office_stage::loop_stage(
                run_state == "running",
                active,
                awaiting_spec,
                decisions,
                failed_item,
            )
        } else {
            None
        };
        let label = match crate::ui::hub::builtin_loop_page(&name) {
            Some(b) => b.label,
            None if l.get("label").is_some_and(py_truthy) => py_str(&l["label"]),
            None => name.clone(),
        };
        let history: Vec<Value> = loops::history_files(state, &name)
            .into_iter()
            .take(5)
            .map(|f| json!({"name": f.trim_end_matches(".md"), "file": f}))
            .collect();
        let toggle = if enabled { "disable" } else { "enable" };
        entries.push(json!({
            "name": name,
            "title": ctx.t(&label),
            "enabled": enabled,
            "state": os.as_str(),
            "reason": reason_with(ctx, os, pending > 0),
            "last_run": status.get("updated_at").and_then(Value::as_str),
            "stage": stage.as_ref().map(|(s, _)| s.as_str()),
            "stage_label": stage.as_ref().map(|(s, _)| stage_label(ctx, *s)),
            "stage_scope": stage.as_ref().map(|(_, scope)| scope.clone()),
            "needs_you": mine.iter().take(MAX_NEEDS_YOU_CARDS).map(|i| needs_you_card(ctx, i)).collect::<Vec<_>>(),
            // What a working loop is doing now: the office's notifications report its changes.
            "step": (os == OfficeState::Working)
                .then(|| status.get("current_step").and_then(Value::as_str).filter(|s| !s.is_empty()))
                .flatten(),
            "next_run": loops::schedule_summary(ctx, &l),
            "history": history,
            "run_now_url": run_now_url(&name),
            "toggle_url": format!("/daemons/loops/{}/{toggle}", pyfmt::quote(&name, "")),
            "pending_approvals": pending,
            "kind": "loop",
        }));
    }
    entries.extend(creator_entries(ctx));
    json!({
        "loops": entries,
        "pending_approvals": pending_total,
        "generated_at": now.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_and_guide_approvals_light_no_loop() {
        use serde_json::json;
        let fix = json!({"kind": "approval", "project": "web", "issue_iid": "7"});
        assert_eq!(owner_of(&fix).as_deref(), Some("issue-loop"));
        for k in ["profile-section", "guide-page"] {
            let other = json!({"kind": "approval", "approval_kind": k});
            assert_eq!(owner_of(&other), None, "{k}");
        }
    }

    #[test]
    fn each_row_of_the_mapping() {
        assert_eq!(office_state(false, "running", 0), OfficeState::Disabled);
        assert_eq!(office_state(false, "failed", 3), OfficeState::Disabled);
        assert_eq!(office_state(true, "running", 0), OfficeState::Working);
        assert_eq!(office_state(true, "failed", 0), OfficeState::Failed);
        assert_eq!(office_state(true, "stale", 0), OfficeState::Failed);
        for s in ["idle", "stopped", "never_run", "unknown", "weird"] {
            assert_eq!(office_state(true, s, 0), OfficeState::Idle, "{s}");
        }
        assert_eq!(office_state(true, "idle", 1), OfficeState::Waiting);
        assert_eq!(OfficeState::Waiting.as_str(), "waiting");
    }

    #[test]
    fn precedence() {
        assert_eq!(office_state(false, "running", 2), OfficeState::Disabled);
        assert_eq!(office_state(true, "running", 2), OfficeState::Waiting);
        assert_eq!(office_state(true, "failed", 2), OfficeState::Waiting);
    }

    #[test]
    fn pending_only_counts_for_gitlab_loop() {
        assert_eq!(pending_for("issue-loop", 2), 2);
        assert_eq!(pending_for("rss-watch", 2), 0);
    }
}
