//! Running project profile jobs (builds, section refreshes, the weekly sync) as
//! the Agents pill and page, the Terminal page and the Memory page show them:
//! read from their live records (`loopx_core::profile_progress`) and translated
//! here. Only the step code, counts and alias reach a page, never a record's
//! free text.

use super::routes::UiCtx;
use loopx_core::profile_progress::{self, JobView, Step};

/// Every running profile job, newest first, from the activity files beside the
/// issue loop's status file (where the jobs write them).
pub fn running(ctx: &UiCtx) -> Vec<JobView> {
    profile_progress::running_at(&base(ctx), ctx.state.now())
}

/// The shared activity file the jobs write beside.
pub fn base(ctx: &UiCtx) -> std::path::PathBuf {
    loopx_core::agent_activity::beside(&ctx.state.status_path_for_loop("issue-loop"))
}

/// The translated name of what the job is doing now.
pub fn step_name(ctx: &UiCtx, job: &JobView) -> String {
    match job.step_kind() {
        Step::Start => ctx.t("Starting"),
        Step::Scan => ctx.t("Scanning repo"),
        Step::Issues => ctx.t("Reading issues"),
        Step::Ai => ctx.t("Asking the AI"),
        Step::Check => ctx.t("Checking the draft"),
        Step::Save => ctx.t("Saving"),
        Step::Proposal => ctx.t("Raising the proposal"),
        Step::Project => ctx.t("Syncing a project"),
    }
}

fn started(job: &JobView) -> bool {
    job.step_kind() != Step::Start && job.steps > 0
}

/// The job's name: "Profile · web", or "Profile sync".
pub fn title(ctx: &UiCtx, job: &JobView) -> String {
    if job.is_sync() {
        ctx.t("Profile sync")
    } else {
        ctx.tf("Profile · {alias}", &[("alias", &job.alias)])
    }
}

/// One line for the header pill and the Terminal's running strip:
/// "Profile · web — Asking the AI (3/5)", "Profile sync — api (2/3)".
pub fn progress_text(ctx: &UiCtx, job: &JobView) -> String {
    let (n, total) = (job.step.to_string(), job.steps.to_string());
    match (job.is_sync(), started(job)) {
        (true, true) => ctx.tf(
            "Profile sync — {alias} ({n}/{total})",
            &[("alias", &job.alias), ("n", &n), ("total", &total)],
        ),
        (true, false) => ctx.tf("Profile sync — {step}", &[("step", &step_name(ctx, job))]),
        (false, true) => ctx.tf(
            "Profile · {alias} — {step} ({n}/{total})",
            &[
                ("alias", &job.alias),
                ("step", &step_name(ctx, job)),
                ("n", &n),
                ("total", &total),
            ],
        ),
        (false, false) => ctx.tf(
            "Profile · {alias} — {step}",
            &[("alias", &job.alias), ("step", &step_name(ctx, job))],
        ),
    }
}

/// What the Agents row says: "Asking the AI · step 3 of 5", "api · project 2 of 3".
pub fn step_detail(ctx: &UiCtx, job: &JobView) -> String {
    let (n, total) = (job.step.to_string(), job.steps.to_string());
    match (job.is_sync(), started(job)) {
        (_, false) => step_name(ctx, job),
        (true, true) => ctx.tf(
            "{alias} · project {n} of {total}",
            &[("alias", &job.alias), ("n", &n), ("total", &total)],
        ),
        (false, true) => ctx.tf(
            "{step} · step {n} of {total}",
            &[("step", &step_name(ctx, job)), ("n", &n), ("total", &total)],
        ),
    }
}

/// The kind as a short pill: "Profile build", "Section refresh", "Profile sync"
/// (the weekly run and "Sync now" alike).
pub fn kind_label(ctx: &UiCtx, job: &JobView) -> String {
    match job.kind.as_str() {
        "refresh" => ctx.t("Section refresh"),
        "sync" => ctx.t("Profile sync"),
        _ => ctx.t("Profile build"),
    }
}

/// Where a job links: the project's Memory page, or the Memory overview for a sync.
pub fn href(job: &JobView) -> String {
    if job.is_sync() || job.alias.is_empty() {
        "/insights?view=memory".to_string()
    } else {
        format!(
            "/insights?view=memory&project={}",
            super::pyfmt::quote(&job.alias, "")
        )
    }
}
