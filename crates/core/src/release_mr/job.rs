//! Release jobs: `<dir>/<id>.json` plus `<id>.log`, like the Fix now jobs.

use std::path::{Path, PathBuf};

pub type Spawn<'a> = dyn Fn(&Path) -> Result<i64, String> + 'a;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Create,
    Preview,
}

/// running | ready | created | failed | expired | discarded | stopped
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReleaseJob {
    pub id: String,
    pub alias: String,
    pub project: String,
    pub source: String,
    pub target: String,
    pub mode: Mode,
    pub state: String,
    #[serde(default)]
    pub pid: Option<i64>,
    pub started_at: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub source_head: String,
    #[serde(default)]
    pub target_head: String,
    #[serde(default)]
    pub mr_url: String,
    /// The step in progress (0-based); `STEP_COUNT` once every step is done.
    #[serde(default)]
    pub step: usize,
    /// A short human line about the latest step, e.g. "42 commits".
    #[serde(default)]
    pub detail: String,
    #[serde(skip)]
    pub log_path: PathBuf,
}

pub const STEP_COUNT: usize = 5;

/// The steps a release job goes through, in order; the last one depends on the mode.
pub fn step_labels(mode: Mode) -> [&'static str; STEP_COUNT] {
    [
        "Checking the branches",
        "Reading the changes",
        "Matching contributors",
        "Writing the summary",
        match mode {
            Mode::Create => "Creating the merge request",
            Mode::Preview => "Preparing the preview",
        },
    ]
}

/// Records progress: the step now in progress and a short detail about it.
pub fn set_step(dir: &Path, id: &str, step: usize, detail: &str) -> Result<ReleaseJob, String> {
    update(dir, id, |j| {
        j.step = step;
        j.detail = detail.to_string();
    })
}

/// A `ready` preview nobody confirmed expires after this many hours.
pub const READY_TTL_HOURS: i64 = 24;

/// `<loopx_home>/release-mr/jobs`.
pub fn jobs_dir(loopx_home: &Path) -> PathBuf {
    loopx_home.join("release-mr").join("jobs")
}

/// A job id is a filename component: ASCII alphanumerics, `-` and `_` only.
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn record_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn write_record(dir: &Path, j: &ReleaseJob) -> Result<(), String> {
    let v = serde_json::to_value(j).map_err(|e| e.to_string())?;
    crate::settings_store::atomic_write_json(&v, &record_path(dir, &j.id))
        .map_err(|e| e.to_string())
}

fn read_record(dir: &Path, id: &str) -> Option<ReleaseJob> {
    let mut j: ReleaseJob =
        serde_json::from_str(&std::fs::read_to_string(record_path(dir, id)).ok()?).ok()?;
    j.id = id.to_string();
    j.log_path = dir.join(format!("{id}.log"));
    Some(j)
}

/// Apply `change` to the record under its cross-process lock and write it back.
pub fn update(
    dir: &Path,
    id: &str,
    change: impl FnOnce(&mut ReleaseJob),
) -> Result<ReleaseJob, String> {
    if !is_safe_id(id) {
        return Err("invalid job id".to_string());
    }
    crate::file_lock::with_lock(&record_path(dir, id), || {
        let mut cur = read_record(dir, id).ok_or_else(|| "no such job".to_string())?;
        change(&mut cur);
        write_record(dir, &cur)?;
        Ok(cur)
    })
    .map_err(|e| e.to_string())?
}

/// Settle jobs that can no longer progress: a `running` job whose process is gone
/// failed unexpectedly (a placeholder with no pid is abandoned after two minutes), and a
/// `ready` preview older than [`READY_TTL_HOURS`] expired.
pub fn expire_stale(dir: &Path, now: chrono::DateTime<chrono::Utc>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".json").filter(|i| is_safe_id(i)) else {
            continue;
        };
        let Some(j) = read_record(dir, id) else {
            continue;
        };
        let age = chrono::DateTime::parse_from_rfc3339(&j.started_at)
            .map(|t| now - t.to_utc())
            .ok();
        let settle = match j.state.as_str() {
            "running" => {
                let dead = match j.pid {
                    Some(p) => !crate::run_control::process_alive(p),
                    None => age.is_none_or(|a| a.num_seconds() > 120),
                };
                dead.then_some(("failed", "the run ended unexpectedly"))
            }
            "ready" if age.is_some_and(|a| a.num_hours() >= READY_TTL_HOURS) => {
                Some(("expired", "the preview expired"))
            }
            _ => None,
        };
        if let Some((state, why)) = settle {
            let _ = update(dir, id, |c| {
                if c.state == j.state {
                    c.state = state.to_string();
                    c.summary = why.to_string();
                }
            });
        }
    }
}

/// All jobs, newest first, settled against their processes and age.
pub fn list(dir: &Path) -> Vec<ReleaseJob> {
    expire_stale(dir, chrono::Utc::now());
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<ReleaseJob> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            read_record(dir, name.strip_suffix(".json").filter(|i| is_safe_id(i))?)
        })
        .collect();
    out.sort_by(|a, b| b.started_at.cmp(&a.started_at).then(b.id.cmp(&a.id)));
    out
}

pub fn get(dir: &Path, id: &str) -> Option<ReleaseJob> {
    if !is_safe_id(id) {
        return None;
    }
    expire_stale(dir, chrono::Utc::now());
    read_record(dir, id)
}

static START_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Record and launch a job; fails if one is already running for this project and
/// branch pair. The `running` record is written before spawning.
pub fn start(
    dir: &Path,
    alias: &str,
    project: &str,
    source: &str,
    target: &str,
    mode: Mode,
    spawn: &Spawn<'_>,
) -> Result<ReleaseJob, String> {
    let _guard = START_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if list(dir).iter().any(|j| {
        j.state == "running" && j.project == project && j.source == source && j.target == target
    }) {
        return Err("a release MR for these branches is already running".to_string());
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let id = format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let rec = ReleaseJob {
        log_path: dir.join(format!("{id}.log")),
        id,
        alias: alias.into(),
        project: project.into(),
        source: source.into(),
        target: target.into(),
        mode,
        state: "running".into(),
        pid: None,
        started_at: chrono::Utc::now().to_rfc3339(),
        summary: String::new(),
        title: String::new(),
        description: String::new(),
        source_head: String::new(),
        target_head: String::new(),
        mr_url: String::new(),
        step: 0,
        detail: String::new(),
    };
    write_record(dir, &rec)?;
    match spawn(&rec.log_path) {
        Ok(pid) => update(dir, &rec.id, |c| c.pid = Some(pid)),
        Err(e) => {
            let _ = update(dir, &rec.id, |c| {
                c.state = "failed".into();
                c.summary = e.clone();
            });
            Err(e)
        }
    }
}

/// Stops a `running` job: its whole process group is killed (the runner and the agent it
/// started) and the record becomes `stopped`. `false` when the job is not running.
pub fn stop(dir: &Path, id: &str) -> bool {
    let Some(j) = get(dir, id).filter(|j| j.state == "running") else {
        return false;
    };
    if let Some(pid) = j.pid {
        crate::run_control::kill_process_group(
            pid,
            std::time::Duration::from_secs(3),
            std::time::Duration::from_millis(100),
        );
    }
    update(dir, id, |c| {
        if c.state == "running" {
            c.state = "stopped".into();
            c.summary = "Stopped".into();
        }
    })
    .is_ok()
}

/// The one entry the island and the Agents page show for release jobs, as a loop status
/// document: a running job with its step, a failure from the last 24 hours, or a preview
/// waiting for review. `None` when there is nothing to show. `jobs` is newest first.
pub fn island_entry(
    jobs: &[ReleaseJob],
    now: chrono::DateTime<chrono::Utc>,
) -> Option<serde_json::Value> {
    let label = |j: &ReleaseJob| format!("{} \u{b7} {} \u{2192} {}", j.alias, j.source, j.target);
    if let Some(j) = jobs.iter().find(|j| j.state == "running") {
        let name = step_labels(j.mode)[j.step.min(STEP_COUNT - 1)];
        let mut step = format!(
            "{name} \u{b7} step {} of {STEP_COUNT}",
            j.step.min(STEP_COUNT - 1) + 1
        );
        if !j.detail.is_empty() {
            step.push_str(&format!(" \u{b7} {}", j.detail));
        }
        return Some(
            serde_json::json!({"state": "running", "updated_at": j.started_at, "current_issue": label(j), "current_step": step}),
        );
    }
    let latest = jobs.first()?;
    let age = chrono::DateTime::parse_from_rfc3339(&latest.started_at)
        .map(|t| now - t.to_utc())
        .ok()?;
    match latest.state.as_str() {
        "failed" if age.num_hours() < READY_TTL_HOURS => Some(
            serde_json::json!({"state": "failed", "updated_at": latest.started_at, "current_issue": label(latest), "current_step": latest.summary}),
        ),
        "ready" => Some(
            serde_json::json!({"state": "idle", "updated_at": latest.started_at, "current_issue": label(latest), "current_step": "Preview ready to review"}),
        ),
        _ => None,
    }
}

/// Launch `<exe> release-mr run <job_id>` detached (own process group), output to `log`.
pub fn spawn_runner(log: &Path, exe: &str, job_id: &str) -> Result<i64, String> {
    let argv: Vec<String> = [exe, "release-mr", "run", job_id]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let cwd = log.parent().unwrap_or_else(|| Path::new("."));
    crate::creator::spawn::spawn_agent(&argv, cwd, log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn me() -> i64 {
        std::process::id() as i64
    }
    fn fake(pid: i64) -> impl Fn(&Path) -> Result<i64, String> {
        move |_| Ok(pid)
    }

    #[test]
    fn a_new_job_starts_at_the_first_step_and_labels_follow_the_mode() {
        let d = tempfile::tempdir().unwrap();
        let j = start(
            d.path(),
            "app",
            "g/app",
            "staging",
            "master",
            Mode::Preview,
            &fake(me()),
        )
        .unwrap();
        assert_eq!((j.step, j.detail.as_str()), (0, ""));
        assert_eq!(step_labels(Mode::Create)[4], "Creating the merge request");
        assert_eq!(step_labels(Mode::Preview)[4], "Preparing the preview");
        assert_eq!(
            step_labels(Mode::Create)[..4],
            step_labels(Mode::Preview)[..4]
        );
        set_step(d.path(), &j.id, 2, "5 contributors").unwrap();
        let again = get(d.path(), &j.id).unwrap();
        assert_eq!((again.step, again.detail.as_str()), (2, "5 contributors"));
    }

    #[test]
    fn the_island_entry_shows_a_running_job_with_its_step_a_recent_failure_and_a_waiting_preview() {
        let d = tempfile::tempdir().unwrap();
        let now = Utc::now();
        assert_eq!(island_entry(&[], now), None);
        let j = start(
            d.path(),
            "app",
            "g/app",
            "staging",
            "master",
            Mode::Preview,
            &fake(me()),
        )
        .unwrap();
        set_step(d.path(), &j.id, 1, "42 commits").unwrap();
        let running = island_entry(&list(d.path()), now).unwrap();
        assert_eq!(running["state"], "running");
        assert_eq!(
            running["current_issue"],
            "app \u{b7} staging \u{2192} master"
        );
        assert_eq!(
            running["current_step"],
            "Reading the changes \u{b7} step 2 of 5 \u{b7} 42 commits"
        );
        update(d.path(), &j.id, |j| {
            j.state = "ready".into();
            j.step = STEP_COUNT;
        })
        .unwrap();
        let waiting = island_entry(&list(d.path()), now).unwrap();
        assert_eq!(
            (waiting["state"].as_str(), waiting["current_step"].as_str()),
            (Some("idle"), Some("Preview ready to review"))
        );
        update(d.path(), &j.id, |j| {
            j.state = "failed".into();
            j.summary = "Nothing to release".into();
        })
        .unwrap();
        let failed = island_entry(&list(d.path()), now).unwrap();
        assert_eq!(
            (failed["state"].as_str(), failed["current_step"].as_str()),
            (Some("failed"), Some("Nothing to release"))
        );
        assert_eq!(
            island_entry(&list(d.path()), now + Duration::hours(25)),
            None,
            "an old failure is forgotten"
        );
        update(d.path(), &j.id, |j| j.state = "created".into()).unwrap();
        assert_eq!(
            island_entry(&list(d.path()), now),
            None,
            "a finished job needs nothing from the island"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stop_kills_the_running_process_group_and_marks_the_job_stopped() {
        use std::os::unix::process::CommandExt;
        let d = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = i64::from(child.id());
        let j = start(
            d.path(),
            "app",
            "g/app",
            "staging",
            "master",
            Mode::Create,
            &fake(pid),
        )
        .unwrap();
        assert!(stop(d.path(), &j.id));
        let _ = child.wait();
        let after = get(d.path(), &j.id).unwrap();
        assert_eq!(
            (after.state.as_str(), after.summary.as_str()),
            ("stopped", "Stopped")
        );
        assert!(
            !stop(d.path(), &j.id),
            "a settled job cannot be stopped again"
        );
        assert!(!stop(d.path(), "nope"));
    }

    #[test]
    fn jobs_dir_is_under_the_loopx_home() {
        assert_eq!(
            jobs_dir(Path::new("/h")),
            Path::new("/h").join("release-mr").join("jobs")
        );
    }

    #[test]
    fn start_refuses_a_second_running_job_for_the_same_pair_but_not_another_pair() {
        let d = tempfile::tempdir().unwrap();
        let a = start(
            d.path(),
            "app",
            "g/app",
            "staging",
            "master",
            Mode::Create,
            &fake(me()),
        )
        .unwrap();
        assert_eq!((a.state.as_str(), a.pid), ("running", Some(me())));
        let again = start(
            d.path(),
            "app",
            "g/app",
            "staging",
            "master",
            Mode::Preview,
            &fake(me()),
        );
        assert!(again.unwrap_err().contains("already running"));
        start(
            d.path(),
            "app",
            "g/app",
            "develop",
            "master",
            Mode::Create,
            &fake(me()),
        )
        .unwrap();
    }

    #[test]
    fn a_failed_spawn_marks_the_record_failed() {
        let d = tempfile::tempdir().unwrap();
        let e = start(
            d.path(),
            "app",
            "g/app",
            "staging",
            "master",
            Mode::Create,
            &|_: &Path| Err("no exe".to_string()),
        )
        .unwrap_err();
        assert_eq!(e, "no exe");
        assert_eq!(list(d.path())[0].state, "failed");
    }

    #[test]
    fn stale_ready_job_expires_after_24_hours_and_a_dead_pid_becomes_failed() {
        let d = tempfile::tempdir().unwrap();
        let ready = start(
            d.path(),
            "app",
            "g/app",
            "staging",
            "master",
            Mode::Preview,
            &fake(me()),
        )
        .unwrap();
        update(d.path(), &ready.id, |j| {
            j.state = "ready".into();
            j.started_at = (Utc::now() - Duration::hours(25)).to_rfc3339();
        })
        .unwrap();
        let fresh = start(
            d.path(),
            "app",
            "g/app",
            "a",
            "b",
            Mode::Preview,
            &fake(me()),
        )
        .unwrap();
        update(d.path(), &fresh.id, |j| j.state = "ready".into()).unwrap();
        let dead = start(
            d.path(),
            "app",
            "g/app",
            "c",
            "d",
            Mode::Create,
            &fake(i64::from(i32::MAX)),
        )
        .unwrap();
        expire_stale(d.path(), Utc::now());
        assert_eq!(get(d.path(), &ready.id).unwrap().state, "expired");
        assert_eq!(get(d.path(), &fresh.id).unwrap().state, "ready");
        let dead = get(d.path(), &dead.id).unwrap();
        assert_eq!(dead.state, "failed");
        assert!(dead.summary.contains("ended unexpectedly"));
    }

    #[test]
    fn unsafe_ids_are_unknown_and_the_log_sits_beside_the_record() {
        let d = tempfile::tempdir().unwrap();
        assert!(get(d.path(), "../x").is_none());
        let j = start(
            d.path(),
            "app",
            "g/app",
            "staging",
            "master",
            Mode::Create,
            &fake(me()),
        )
        .unwrap();
        assert_eq!(j.log_path, d.path().join(format!("{}.log", j.id)));
    }
}
