//! Maps each loop to one of the office view's five states.

use crate::ui::pages::loops;
use crate::ui::pyfmt;
use crate::ui::routes::UiCtx;
use loopx_core::approvals;
use loopx_core::loops_config::py_truthy;
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

/// Approvals carry no loop name and only the GitLab issue loop creates them, so
/// the whole pending total belongs to it and to no other loop.
pub fn pending_for(loop_name: &str, pending_total: usize) -> usize {
    if loop_name == "gitlab-loop" {
        pending_total
    } else {
        0
    }
}

fn reason(ctx: &UiCtx, state: OfficeState) -> String {
    match state {
        OfficeState::Disabled => ctx.t("Disabled"),
        OfficeState::Waiting => ctx.t("Waiting for your approval"),
        OfficeState::Failed => ctx.t("The last run failed"),
        OfficeState::Working => ctx.t("Working on a run right now"),
        OfficeState::Idle => ctx.t("Idle until the next run"),
    }
}

fn run_now_url(name: &str) -> String {
    match name {
        "gitlab-loop" => "/run-now".to_string(),
        "topic-loop" => "/topic-monitor/run-now".to_string(),
        "inbox-triage-loop" => "/inbox/run-now".to_string(),
        _ => format!("/loops/{}/run-now", pyfmt::quote(name, "")),
    }
}

/// The `/office/state` document: one entry per visible registered loop.
pub fn build_office_state(ctx: &UiCtx) -> Value {
    let state = ctx.state;
    let now = state.now();
    let pending_total = approvals::list(
        &state.repo_root.join("outputs").join("approvals"),
        Some("pending"),
        now,
    )
    .len();
    let mut entries = Vec::new();
    for l in loops::list_loops(state).unwrap_or_default() {
        if !l.is_object() || !loops::is_visible(state, &l) {
            continue;
        }
        let name = l.get("name").map_or_else(|| "?".to_string(), py_str);
        let enabled = l.get("enabled").is_none_or(py_truthy);
        let status = loopx_core::status::read_status(&state.status_path_for_loop(&name));
        let run_state = crate::snapshot::effective_state(&status);
        let pending = pending_for(&name, pending_total);
        let os = office_state(enabled, &run_state, pending);
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
            "reason": reason(ctx, os),
            "last_run": status.get("updated_at").and_then(Value::as_str),
            "next_run": loops::schedule_summary(ctx, &l),
            "history": history,
            "run_now_url": run_now_url(&name),
            "toggle_url": format!("/daemons/loops/{}/{toggle}", pyfmt::quote(&name, "")),
            "pending_approvals": pending,
        }));
    }
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
        assert_eq!(pending_for("gitlab-loop", 2), 2);
        assert_eq!(pending_for("rss-watch", 2), 0);
    }
}
