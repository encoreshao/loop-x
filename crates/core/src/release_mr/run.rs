//! The runner behind a release job, and confirming a previewed one.

use super::api::{NewMr, ReleaseApi};
use super::authors::resolve;
use super::collect::collect;
use super::job::{self, Mode};
use super::render::render;
use super::summarize::summarize;
use super::ReleaseError;
use crate::agents::sealed::SealedHooks;
use std::path::Path;

/// Runs the job `job_id` to its end: `created`, `ready` (preview mode) or `failed`.
/// An `Err` is also recorded on the job as its failure reason.
pub fn run(
    api: &dyn ReleaseApi,
    hooks: &SealedHooks<'_>,
    dir: &Path,
    job_id: &str,
) -> Result<(), String> {
    let job = job::get(dir, job_id).ok_or_else(|| "no such job".to_string())?;
    match run_inner(api, hooks, dir, &job) {
        Ok(()) => Ok(()),
        Err(e) => {
            let reason = e.to_string();
            let _ = job::update(dir, job_id, |j| {
                j.state = "failed".into();
                j.summary = reason.clone();
            });
            Err(reason)
        }
    }
}

fn run_inner(
    api: &dyn ReleaseApi,
    hooks: &SealedHooks<'_>,
    dir: &Path,
    job: &job::ReleaseJob,
) -> Result<(), ReleaseError> {
    let (project, source, target) = (
        job.project.as_str(),
        job.source.as_str(),
        job.target.as_str(),
    );
    let step = |n: usize, detail: &str| {
        let _ = job::set_step(dir, &job.id, n, detail);
    };
    step(0, &format!("{source} \u{2192} {target}"));
    if let Some(url) = api.open_mr(project, source, target)? {
        return Err(ReleaseError::OpenMrExists(url));
    }
    let source_head = api.branch_head(project, source)?;
    let target_head = api.branch_head(project, target)?;
    step(1, "");
    let changes = collect(api, project, target, source)?;
    step(2, &format!("{} found", plural(changes.len(), "change")));
    let resolved = resolve(api, project, &changes);
    step(
        3,
        &format!("{} to tag", plural(resolved.tags.len(), "person")).replace("persons", "people"),
    );
    let mut log = |m: &str| {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&job.log_path)
        {
            let _ = writeln!(f, "{m}");
        }
    };
    let summary = summarize(&changes, hooks, &mut log)?;
    step(
        4,
        &format!("{} written", plural(summary.features.len(), "feature")),
    );
    let (title, description) = render(&summary, &resolved, (source, target));
    let (state, mr_url) = match job.mode {
        Mode::Create => {
            let url = api.create_mr(
                project,
                &NewMr {
                    source: source.into(),
                    target: target.into(),
                    title: title.clone(),
                    description: description.clone(),
                },
            )?;
            ("created", url)
        }
        Mode::Preview => ("ready", String::new()),
    };
    let done = format!("{} written", plural(summary.features.len(), "feature"));
    job::update(dir, &job.id, |j| {
        j.state = state.into();
        j.title = title;
        j.description = description;
        j.source_head = source_head;
        j.target_head = target_head;
        j.mr_url = mr_url;
        j.step = job::STEP_COUNT;
        j.detail = done;
        j.summary = if state == "created" {
            "Merge request created".into()
        } else {
            "Ready to review".into()
        };
    })?;
    Ok(())
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

/// Creates the MR of a `ready` preview with the (possibly edited) text; returns its URL.
/// Refuses if either branch moved since the preview, or an MR for the pair is now open.
pub fn confirm(
    api: &dyn ReleaseApi,
    dir: &Path,
    job_id: &str,
    title: &str,
    description: &str,
) -> Result<String, ReleaseError> {
    let job = job::get(dir, job_id).ok_or_else(|| ReleaseError::Other("no such job".into()))?;
    if job.state != "ready" {
        return Err(ReleaseError::Other(
            "this preview is no longer available".into(),
        ));
    }
    let title = title.trim();
    if title.is_empty() {
        return Err(ReleaseError::Other("the title cannot be empty".into()));
    }
    let title: String = title.chars().take(255).collect();
    if api.branch_head(&job.project, &job.source)? != job.source_head
        || api.branch_head(&job.project, &job.target)? != job.target_head
    {
        return Err(ReleaseError::BranchesMoved);
    }
    if let Some(url) = api.open_mr(&job.project, &job.source, &job.target)? {
        return Err(ReleaseError::OpenMrExists(url));
    }
    let url = api.create_mr(
        &job.project,
        &NewMr {
            source: job.source.clone(),
            target: job.target.clone(),
            title: title.clone(),
            description: description.to_string(),
        },
    )?;
    job::update(dir, job_id, |j| {
        j.state = "created".into();
        j.title = title;
        j.description = description.to_string();
        j.mr_url = url.clone();
        j.summary = "Merge request created".into();
    })?;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::ProcOutput;
    use crate::release_mr::api::{CommitInfo, FakeApi, Member, MrInfo};
    use crate::release_mr::job::{self, Mode};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const REPLY: &str = r#"{"title_brief":"order export","features":[{"name":"Order export","summary":"Download orders as CSV.","authors":["Ann"]}]}"#;

    fn api() -> FakeApi {
        let mut api = FakeApi {
            commits: vec![CommitInfo {
                sha: "c1".into(),
                title: "Add export".into(),
                author_name: "Ann".into(),
                author_email: "ann@x.io".into(),
            }],
            members: vec![Member {
                username: "ann".into(),
                name: "Ann".into(),
                email: Some("ann@x.io".into()),
            }],
            create_url: "https://g/app/-/merge_requests/10".into(),
            ..FakeApi::default()
        };
        api.commit_mrs.insert(
            "c1".into(),
            MrInfo {
                title: "Order export".into(),
                author_username: "ann".into(),
            },
        );
        api
    }

    /// Runs `f` with hooks whose fake claude replies `reply` and counts its calls.
    fn with_agent<T>(reply: &str, f: impl FnOnce(&SealedHooks<'_>, &AtomicUsize) -> T) -> T {
        let calls = AtomicUsize::new(0);
        let out = serde_json::json!({"result": reply, "response": reply}).to_string();
        let runner = |_: &[String], _: &str, _: f64, _: &std::path::Path| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(ProcOutput {
                code: Some(0),
                stdout: out.clone(),
                stderr: String::new(),
                timed_out: false,
            })
        };
        let cli = || "claude".to_string();
        let hooks = SealedHooks {
            runner: Some(&runner),
            cli_fn: Some(&cli),
            command_fn: None,
        };
        f(&hooks, &calls)
    }

    fn new_job(dir: &Path, mode: Mode) -> String {
        job::start(
            dir,
            "app",
            "g/app",
            "staging",
            "master",
            mode,
            &|_: &Path| Ok(std::process::id() as i64),
        )
        .unwrap()
        .id
    }

    #[test]
    fn run_in_create_mode_creates_the_mr_and_records_the_url() {
        let d = tempfile::tempdir().unwrap();
        let (api, id) = (api(), new_job(d.path(), Mode::Create));
        with_agent(REPLY, |h, _| run(&api, h, d.path(), &id)).unwrap();
        let j = job::get(d.path(), &id).unwrap();
        assert_eq!(
            (j.state.as_str(), j.mr_url.as_str()),
            ("created", "https://g/app/-/merge_requests/10")
        );
        let made = api.created.borrow();
        assert_eq!(made.len(), 1);
        assert_eq!(
            (made[0].source.as_str(), made[0].target.as_str()),
            ("staging", "master")
        );
        assert_eq!(made[0].title, "Release: order export");
        assert!(
            made[0].description.contains("_By @ann_") && !made[0].description.contains("cc @ann"),
            "{}",
            made[0].description
        );
    }

    #[test]
    fn run_in_preview_mode_parks_ready_with_title_description_and_head_shas() {
        let d = tempfile::tempdir().unwrap();
        let (api, id) = (api(), new_job(d.path(), Mode::Preview));
        with_agent(REPLY, |h, _| run(&api, h, d.path(), &id)).unwrap();
        let j = job::get(d.path(), &id).unwrap();
        assert_eq!(j.state, "ready");
        assert_eq!(j.title, "Release: order export");
        assert!(j.description.contains("Order export"));
        assert_eq!(
            (j.source_head.as_str(), j.target_head.as_str()),
            ("head-of-staging", "head-of-master")
        );
        assert!(api.created.borrow().is_empty(), "a preview creates nothing");
    }

    #[test]
    fn run_refuses_when_an_open_mr_exists_and_creates_nothing() {
        let d = tempfile::tempdir().unwrap();
        let mut api = api();
        api.open_mr = Some("https://g/app/-/merge_requests/3".into());
        let id = new_job(d.path(), Mode::Create);
        let (r, calls) = with_agent(REPLY, |h, c| {
            (run(&api, h, d.path(), &id), c.load(Ordering::SeqCst))
        });
        assert!(r.is_err());
        let j = job::get(d.path(), &id).unwrap();
        assert_eq!(j.state, "failed");
        assert!(
            j.summary.contains("https://g/app/-/merge_requests/3"),
            "{}",
            j.summary
        );
        assert!(api.created.borrow().is_empty());
        assert_eq!(
            calls, 0,
            "no agent call when the pair already has an open MR"
        );
    }

    #[test]
    fn run_with_no_commits_never_calls_the_agent() {
        let d = tempfile::tempdir().unwrap();
        let mut api = api();
        api.commits.clear();
        let id = new_job(d.path(), Mode::Create);
        let (_, calls) = with_agent(REPLY, |h, c| {
            (run(&api, h, d.path(), &id), c.load(Ordering::SeqCst))
        });
        assert_eq!(calls, 0);
        let j = job::get(d.path(), &id).unwrap();
        assert_eq!(j.state, "failed");
        assert!(j.summary.contains("Nothing to release"), "{}", j.summary);
        assert_eq!(j.step, 1, "it failed while reading the changes");
    }

    #[test]
    fn a_failed_run_records_the_reason_and_creates_nothing() {
        let d = tempfile::tempdir().unwrap();
        let (api, id) = (api(), new_job(d.path(), Mode::Create));
        assert!(with_agent("not json", |h, _| run(&api, h, d.path(), &id)).is_err());
        assert_eq!(job::get(d.path(), &id).unwrap().state, "failed");
        assert!(api.created.borrow().is_empty());
    }

    fn ready_job(d: &Path) -> (FakeApi, String) {
        let (api, id) = (api(), new_job(d, Mode::Preview));
        with_agent(REPLY, |h, _| run(&api, h, d, &id)).unwrap();
        (api, id)
    }

    #[test]
    fn confirm_creates_with_the_edited_text_and_records_created() {
        let d = tempfile::tempdir().unwrap();
        let (api, id) = ready_job(d.path());
        let url = confirm(&api, d.path(), &id, "Release: edited", "my own words").unwrap();
        assert_eq!(url, "https://g/app/-/merge_requests/10");
        assert_eq!(api.created.borrow()[0].title, "Release: edited");
        assert_eq!(api.created.borrow()[0].description, "my own words");
        assert_eq!(job::get(d.path(), &id).unwrap().state, "created");
    }

    #[test]
    fn confirm_refuses_when_a_branch_head_moved() {
        let d = tempfile::tempdir().unwrap();
        let (mut api, id) = ready_job(d.path());
        api.heads.insert("staging".into(), "someone-pushed".into());
        assert_eq!(
            confirm(&api, d.path(), &id, "Release: x", "d"),
            Err(ReleaseError::BranchesMoved)
        );
        assert!(api.created.borrow().is_empty());
        assert_eq!(
            job::get(d.path(), &id).unwrap().state,
            "ready",
            "stays ready so the user can discard or regenerate"
        );
    }

    #[test]
    fn confirm_rechecks_for_an_open_mr_and_for_a_ready_job() {
        let d = tempfile::tempdir().unwrap();
        let (mut api, id) = ready_job(d.path());
        api.open_mr = Some("https://g/app/-/merge_requests/4".into());
        assert_eq!(
            confirm(&api, d.path(), &id, "Release: x", "d"),
            Err(ReleaseError::OpenMrExists(
                "https://g/app/-/merge_requests/4".into()
            ))
        );
        api.open_mr = None;
        confirm(&api, d.path(), &id, "Release: x", "d").unwrap();
        assert!(
            confirm(&api, d.path(), &id, "Release: x", "d").is_err(),
            "a created job cannot be confirmed again"
        );
        assert!(confirm(&api, d.path(), "../x", "Release: x", "d").is_err());
        assert!(
            confirm(&api, d.path(), &id, "  ", "d").is_err(),
            "an empty title is refused"
        );
    }
}
