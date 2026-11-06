//! `loopx mr-fix run <account:project!iid>` (the runner "Fix now" launches) and
//! `loopx mr-fix announce <plan-file>` (what the fix agent runs before it edits).

use loopx_core::config::{default_config_path, home_dir, Config};
use loopx_core::connectors_config::ConfigConnectors;
use loopx_core::creator::spawn::Fence;
use loopx_core::loop_plugins::mr_review::list_my_mrs;
use loopx_core::mr_fix::{
    agent::{self, ClaudeAgent, PLAN_ENV},
    eligibility,
    jobs::{self, FixJob},
    real::{GitWorkspace, GitlabMr},
    run::{self as runner, MrApi, RunResult},
    valid_mr_ref,
};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str =
    "Usage: loopx mr-fix run <account:project!iid>\n       loopx mr-fix announce <plan-file>";

pub fn run(args: &[String]) -> ExitCode {
    let result = match args {
        [sub, mr_ref] if sub == "run" => run_fix(mr_ref),
        [sub, draft] if sub == "announce" => announce(Path::new(draft)),
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

/// How long `announce` waits for the runner to post the start note.
const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(120);

/// Validate the agent's plan and hand it to the runner (`$LOOPX_MR_FIX_PLAN`).
fn announce(draft: &Path) -> Result<ExitCode, String> {
    let plan = std::env::var_os(PLAN_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| {
            format!("{PLAN_ENV} is not set: this command only works inside a Fix now run")
        })?;
    let n = agent::announce_request(draft, &plan, ANNOUNCE_TIMEOUT, Duration::from_millis(250))?;
    println!("Start note posted with {n} planned change(s). You may start editing.");
    Ok(ExitCode::SUCCESS)
}

/// `account`, `project` and `iid` of a ref already checked by `valid_mr_ref`.
fn split_ref(mr_ref: &str) -> (&str, &str, &str) {
    let (account, rest) = mr_ref.split_once(':').unwrap_or_default();
    let (project, iid) = rest.split_once('!').unwrap_or_default();
    (account, project, iid)
}

/// The running record this process was launched for: newest running job for `mr_ref`.
fn own_job(dir: &Path, mr_ref: &str) -> Option<FixJob> {
    let mut all: Vec<FixJob> = jobs::list(dir)
        .into_iter()
        .filter(|j| j.state == "running" && j.mr_ref == mr_ref)
        .collect();
    all.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.id.cmp(&b.id)));
    all.pop()
}

/// A running record whose pid is another live runner is not ours to adopt.
/// The dashboard records the wrapper shell's pid, whose child is this runner,
/// so the record is ours when its pid is this process or its parent.
fn check_owner(job: Option<&FixJob>, me: u32, parent: u32) -> Result<(), String> {
    match job.and_then(|j| j.pid) {
        Some(pid) if pid != i64::from(me) && pid != i64::from(parent) => Err(format!(
            "a Fix now job for this merge request is already running (pid {pid}); \
             wait for it or stop it first"
        )),
        _ => Ok(()),
    }
}

/// Only GitLab accounts can be fixed in place.
fn require_gitlab(kind: &str) -> Result<(), String> {
    if kind == "gitlab" {
        Ok(())
    } else {
        Err(eligibility::Ineligible::NotGitlab.reason().to_string())
    }
}

/// A private (0700) fresh scratch folder under `base`, never inside the loopx home.
fn make_scratch(base: &Path, loopx_home: &Path) -> Result<PathBuf, String> {
    let scratch = base.join(format!("loopx-mr-fix-{}", uuid::Uuid::new_v4()));
    if scratch.starts_with(loopx_home) {
        return Err(format!(
            "{} must not be inside the loopx home",
            scratch.display()
        ));
    }
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(&scratch)
        .map_err(|e| format!("cannot create {}: {e}", scratch.display()))?;
    Ok(scratch)
}

/// Record how the run ended on the job's record (when there is one) and map it
/// to the exit code. An `Err` is recorded as `failed` with its reason.
fn finish_outcome(
    dir: &Path,
    job: Option<&FixJob>,
    outcome: Result<RunResult, String>,
) -> Result<ExitCode, String> {
    let (state, summary, code) = match &outcome {
        Ok(r) => {
            println!("mr-fix: {}: {}", r.state, r.summary);
            (
                r.state.as_str(),
                r.summary.as_str(),
                if r.state == "failed" { 1 } else { 0 },
            )
        }
        Err(e) => ("failed", e.as_str(), 1),
    };
    match job {
        Some(j) => {
            if let Err(e) = jobs::finish(dir, &j.id, state, summary) {
                eprintln!("mr-fix: could not record the result: {e}");
            }
        }
        None => eprintln!("mr-fix: no job record was updated"),
    }
    match outcome {
        Err(e) => Err(e),
        Ok(_) => Ok(ExitCode::from(code)),
    }
}

fn run_fix(mr_ref: &str) -> Result<ExitCode, String> {
    if !valid_mr_ref(mr_ref) {
        return Err(format!(
            "{mr_ref:?} is not a valid merge request reference (account:project!iid)"
        ));
    }
    let home = home_dir();
    let dir = jobs::jobs_dir(&home);
    let job = own_job(&dir, mr_ref);
    // Refused before anything is recorded: the other runner owns that record.
    #[cfg(unix)]
    let parent = std::os::unix::process::parent_id();
    #[cfg(not(unix))]
    let parent = 0;
    check_owner(job.as_ref(), std::process::id(), parent)?;
    if job.is_none() {
        eprintln!("mr-fix: no running job record for {mr_ref}; none will be updated");
    }
    let outcome = fix(mr_ref, &home, job.as_ref());
    finish_outcome(&dir, job.as_ref(), outcome)
}

/// Everything after the record lookup; any `Err` becomes the job's failure reason.
fn fix(mr_ref: &str, home: &Path, job: Option<&FixJob>) -> Result<RunResult, String> {
    let (account, project, iid) = split_ref(mr_ref);
    let paths = loopx_core::connectors_config::Paths::default();
    let kind = loopx_core::connectors_config::get_account(account, &paths)
        .map_err(|e| e.0)?
        .ok_or_else(|| format!("unknown connector account {account}"))?
        .kind;
    require_gitlab(&kind)?;
    let connectors = ConfigConnectors::real();
    let client = connectors.gitlab_client(account)?;
    let iid_num: i64 = iid
        .parse()
        .map_err(|_| format!("bad merge request number {iid}"))?;
    let config = Config::load(&default_config_path()).map_err(|e| e.to_string())?;
    let listed = list_my_mrs(&connectors)
        .into_iter()
        .filter(|a| a.account_id == account)
        .flat_map(|a| a.mrs)
        .find(|m| m.mr_ref() == mr_ref);
    let gitlab = GitlabMr {
        client,
        project_id: project.to_string(),
        iid: iid_num,
    };
    let (target, title) = match listed {
        Some(mr) => (
            eligibility::resolve(&mr, &kind, &config).map_err(|e| e.reason().to_string())?,
            mr.title,
        ),
        None => {
            // Not among the user's open MRs: only a merged or closed one may
            // run, so the runner can remove its worktree.
            let info = gitlab.mr()?;
            if !matches!(info.state.as_str(), "merged" | "closed") {
                return Err(format!(
                    "{mr_ref} is not among your open merge requests, so it cannot be fixed"
                ));
            }
            // The reference path too, for a project stored as `group/project`.
            let target = eligibility::project_for(project, &info.project_path, &config)
                .map_err(|e| e.reason().to_string())?;
            (target, info.title)
        }
    };

    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot find the loopx binary: {e}"))?
        .display()
        .to_string();
    // Outside the loopx home, whose reads the agent is denied.
    let scratch = make_scratch(&std::env::temp_dir(), home)?;
    let worktree = PathBuf::from(loopx_core::worktree::mr_worktree_path(
        &target.local_path,
        iid,
        &target.worktree_root,
    ));
    let cli = loopx_core::ai_cli_config::selected_cli();
    let user_home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let (argv_scratch, argv_exe, argv_home) = (scratch.clone(), exe.clone(), home.to_path_buf());
    let argv = Box::new(move |prompt: &str| {
        agent::fix_argv(
            &cli,
            prompt,
            &Fence {
                cwd: &worktree,
                user_home: &user_home,
                loopx_home: &argv_home,
                exe: &argv_exe,
            },
            &argv_scratch,
        )
    });
    let agent = ClaudeAgent::new(argv, scratch.clone(), &exe);
    let ws = GitWorkspace::new(&target.local_path, &target.worktree_root, iid);
    let record = job.cloned().unwrap_or_else(|| FixJob {
        id: String::new(),
        mr_ref: mr_ref.to_string(),
        title,
        state: "running".into(),
        pid: None,
        started_at: String::new(),
        summary: String::new(),
        log_path: PathBuf::new(),
    });
    let result = runner::run(&gitlab, &ws, &agent, &record, &mut std::io::stdout());
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use loopx_core::mr_fix::run::RunResult;

    fn start(dir: &Path, mr_ref: &str) -> FixJob {
        jobs::start(dir, mr_ref, "T", &|_| Ok(std::process::id() as i64)).unwrap()
    }

    #[test]
    fn a_hand_run_refuses_a_record_owned_by_another_live_runner() {
        let mut j = FixJob {
            id: "j".into(),
            mr_ref: "a:1!1".into(),
            title: String::new(),
            state: "running".into(),
            pid: Some(4242),
            started_at: String::new(),
            summary: String::new(),
            log_path: PathBuf::new(),
        };
        // Launched by the dashboard: the record's pid is this process or its
        // wrapper shell (the parent).
        assert!(check_owner(Some(&j), 4242, 1).is_ok());
        assert!(check_owner(Some(&j), 5000, 4242).is_ok());
        let e = check_owner(Some(&j), 5000, 1).unwrap_err();
        assert!(e.contains("already running") && e.contains("4242"), "{e}");
        j.pid = None;
        assert!(check_owner(Some(&j), 5000, 1).is_ok());
        assert!(check_owner(None, 5000, 1).is_ok());
    }

    #[test]
    fn split_ref_returns_account_project_and_iid() {
        assert_eq!(split_ref("work:12!3"), ("work", "12", "3"));
        assert_eq!(split_ref("gh:owner/name!4"), ("gh", "owner/name", "4"));
    }

    #[test]
    fn own_job_is_the_newest_running_record_for_the_ref() {
        let d = tempfile::tempdir().unwrap();
        assert!(own_job(d.path(), "a:1!1").is_none());
        let a = start(d.path(), "a:1!1");
        start(d.path(), "a:2!2");
        assert_eq!(own_job(d.path(), "a:1!1").unwrap().id, a.id);
        jobs::finish(d.path(), &a.id, "done", "x").unwrap();
        assert!(own_job(d.path(), "a:1!1").is_none());
    }

    #[test]
    fn an_error_before_the_run_finishes_the_record_as_failed_with_the_reason() {
        let d = tempfile::tempdir().unwrap();
        let job = start(d.path(), "a:1!1");
        let r = finish_outcome(d.path(), Some(&job), Err("no token".into()));
        assert_eq!(r.unwrap_err(), "no token");
        let got = jobs::get(d.path(), &job.id).unwrap();
        assert_eq!(
            (got.state.as_str(), got.summary.as_str()),
            ("failed", "no token")
        );
    }

    #[test]
    fn a_run_result_is_recorded_and_sets_the_exit_code() {
        let d = tempfile::tempdir().unwrap();
        for (state, code) in [("done", 0u8), ("nothing", 0), ("failed", 1)] {
            let job = start(
                d.path(),
                &format!("a:1!{code}{state}").replace(char::is_alphabetic, "9"),
            );
            let res = RunResult {
                state: state.into(),
                summary: "s".into(),
            };
            let exit = finish_outcome(d.path(), Some(&job), Ok(res)).unwrap();
            assert_eq!(format!("{exit:?}"), format!("{:?}", ExitCode::from(code)));
            assert_eq!(jobs::get(d.path(), &job.id).unwrap().state, state);
        }
        // Without a record nothing is written and an error still propagates.
        assert!(finish_outcome(d.path(), None, Err("x".into())).is_err());
    }

    #[test]
    fn a_non_gitlab_account_is_refused_with_the_ineligible_reason() {
        assert!(require_gitlab("gitlab").is_ok());
        let e = require_gitlab("github").unwrap_err();
        assert!(e.contains("only works on GitLab"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn the_scratch_dir_is_private_and_never_inside_the_loopx_home() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let s = make_scratch(d.path(), Path::new("/elsewhere")).unwrap();
        assert_eq!(
            std::fs::metadata(&s).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(make_scratch(d.path(), d.path()).is_err());
    }
}
