//! The background tick of the weekly project-profile sync (see
//! `loopx_core::profile_sync`): runs `sync_all` once per scheduled slot, or when
//! "Sync now" set the store's `sync_requested` flag.

use chrono::{DateTime, Utc};
use loopx_core::approvals;
use loopx_core::profile_build::{Agent, SealedAgent};
use loopx_core::profile_sync::{self as sync, ProjectSync, TrackedProject};
use std::path::Path;

/// How often the thread looks at the schedule.
const LOOK_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Default)]
pub struct TickState {
    /// When this process first looked, the baseline for a store with no `last_run`:
    /// a fresh install waits for its first slot instead of syncing at startup.
    started: Option<DateTime<Utc>>,
}

/// Run the sync if it is due (a slot passed since the last run) or was requested.
/// Returns whether it ran. Writes nothing unless it runs; with the weekly switch
/// off only an explicit "Sync now" request runs.
pub fn tick(
    store_path: &Path,
    projects: impl FnOnce() -> Vec<TrackedProject>,
    agent: &dyn Agent,
    now: DateTime<Utc>,
    state: &mut TickState,
    root: Option<&Path>,
    approvals_dir: &Path,
) -> bool {
    let Ok(store) = sync::load_keeping_bad(store_path) else {
        return false;
    };
    let started = *state.started.get_or_insert(now);
    let baseline = store.status.last_run.unwrap_or(started);
    let due = store.settings.enabled && sync::next_run(&store.settings, baseline) <= now;
    if !due && !store.sync_requested {
        return false;
    }
    let mut list = projects();
    // Oldest-synced first, so a cap never starves the same projects every week.
    list.sort_by_key(|p| store.status.per_project.get(&p.alias).map(|s| s.at));
    list.truncate(store.settings.max_projects_per_run);
    let results = sync::sync_all(&list, agent, now, root, approvals_dir);
    let _ = sync::with_store(store_path, |s| {
        s.status.last_run = Some(now);
        s.sync_requested = false;
        for (alias, outcome) in results {
            s.status
                .per_project
                .insert(alias, ProjectSync { at: now, outcome });
        }
    });
    true
}

/// The dashboard daemon's profile sync loop. Changing the schedule needs no restart.
pub fn start_profile_sync() {
    std::thread::spawn(move || {
        let mut state = TickState::default();
        loop {
            tick(
                &sync::store_path(),
                sync::tracked_projects,
                &SealedAgent,
                Utc::now(),
                &mut state,
                None,
                &approvals::default_dir(),
            );
            std::thread::sleep(LOOK_EVERY);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Local, TimeZone};
    use loopx_core::profile_sync::{Outcome, SyncSettings};
    use std::cell::Cell;

    struct Counting(Cell<usize>);
    impl Agent for Counting {
        fn run(&self, _: &str) -> Result<String, String> {
            self.0.set(self.0.get() + 1);
            Err("no AI CLI in tests".into())
        }
    }

    fn local(d: u32, h: u32, m: u32) -> DateTime<Utc> {
        Local
            .with_ymd_and_hms(2026, 11, d, h, m, 0)
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    struct Fx {
        dir: tempfile::TempDir,
        work: tempfile::TempDir,
    }
    impl Fx {
        fn new() -> Fx {
            Fx {
                dir: tempfile::tempdir().unwrap(),
                work: tempfile::tempdir().unwrap(),
            }
        }
        fn store(&self) -> std::path::PathBuf {
            self.dir.path().join("profile_sync.json")
        }
        fn projects(&self) -> Vec<TrackedProject> {
            vec![TrackedProject {
                alias: "tick-app".into(),
                instance: "work".into(),
                project_id: "g/app".into(),
                work_dir: self.work.path().to_path_buf(),
            }]
        }
        fn tick(&self, agent: &Counting, now: DateTime<Utc>, st: &mut TickState) -> bool {
            tick(
                &self.store(),
                || self.projects(),
                agent,
                now,
                st,
                Some(self.dir.path()),
                &self.dir.path().join("approvals"),
            )
        }
    }

    #[test]
    fn disabled_sync_does_nothing_and_writes_nothing() {
        let f = Fx::new();
        sync::with_store(&f.store(), |s| {
            s.settings = SyncSettings {
                enabled: false,
                ..SyncSettings::default()
            }
        })
        .unwrap();
        let before = std::fs::read_to_string(f.store()).unwrap();
        let (a, mut st) = (Counting(Cell::new(0)), TickState::default());
        assert!(!f.tick(&a, local(16, 10, 0), &mut st));
        assert!(!f.tick(&a, local(30, 10, 0), &mut st));
        assert_eq!(std::fs::read_to_string(f.store()).unwrap(), before);
        // And with no file at all, none is created.
        let g = Fx::new();
        assert!(!g.tick(&a, local(16, 10, 0), &mut TickState::default()));
        assert!(!g.store().exists());
        assert_eq!(a.0.get(), 0);
    }

    #[test]
    fn tick_runs_once_per_week_slot_and_records_status() {
        let f = Fx::new();
        let (a, mut st) = (Counting(Cell::new(0)), TickState::default());
        // Wednesday: first look, baseline only, no file written.
        assert!(!f.tick(&a, local(11, 10, 0), &mut st));
        assert!(!f.store().exists());
        // Monday 09:01: due.
        assert!(f.tick(&a, local(16, 9, 1), &mut st));
        let s = sync::load(&f.store()).unwrap();
        assert_eq!(s.status.last_run, Some(local(16, 9, 1)));
        assert!(matches!(
            s.status.per_project["tick-app"].outcome,
            Outcome::Failed(_) | Outcome::Skipped | Outcome::Drafted
        ));
        // Same slot: not again.
        assert!(!f.tick(&a, local(16, 9, 2), &mut st));
        assert!(!f.tick(&a, local(16, 9, 1) + Duration::days(6), &mut st));
        // Next Monday: again.
        assert!(f.tick(&a, local(23, 9, 1), &mut st));
    }

    #[test]
    fn sync_now_runs_even_off_schedule_and_clears_the_flag() {
        let f = Fx::new();
        let (a, mut st) = (Counting(Cell::new(0)), TickState::default());
        sync::with_store(&f.store(), |s| s.sync_requested = true).unwrap();
        assert!(f.tick(&a, local(11, 10, 0), &mut st));
        let s = sync::load(&f.store()).unwrap();
        assert!(!s.sync_requested);
        assert!(s.status.per_project.contains_key("tick-app"));
        assert!(!f.tick(&a, local(11, 10, 1), &mut st));
    }

    #[test]
    fn the_cap_syncs_the_longest_waiting_projects_first() {
        let f = Fx::new();
        let (a, mut st) = (Counting(Cell::new(0)), TickState::default());
        sync::with_store(&f.store(), |s| {
            s.settings.max_projects_per_run = 1;
            s.sync_requested = true;
            s.status.per_project.insert(
                "tick-old".into(),
                ProjectSync {
                    at: local(1, 0, 0),
                    outcome: Outcome::Skipped,
                },
            );
        })
        .unwrap();
        let mut list = f.projects();
        list.insert(
            0,
            TrackedProject {
                alias: "tick-old".into(),
                ..list[0].clone()
            },
        );
        let ran = tick(
            &f.store(),
            || list,
            &a,
            local(11, 10, 0),
            &mut st,
            Some(f.dir.path()),
            &f.dir.path().join("approvals"),
        );
        assert!(ran);
        let s = sync::load(&f.store()).unwrap();
        // The never-synced project went first; the old one still has its old stamp.
        assert_eq!(s.status.per_project["tick-old"].at, local(1, 0, 0));
        assert_eq!(s.status.per_project["tick-app"].at, local(11, 10, 0));
    }

    #[test]
    fn a_corrupt_store_is_copied_to_bad_and_the_tick_does_not_run() {
        let f = Fx::new();
        std::fs::write(f.store(), "{nope").unwrap();
        let (a, mut st) = (Counting(Cell::new(0)), TickState::default());
        assert!(!f.tick(&a, local(16, 9, 1), &mut st));
        assert_eq!(
            std::fs::read_to_string(f.dir.path().join("profile_sync.json.bad")).unwrap(),
            "{nope"
        );
        assert_eq!(std::fs::read_to_string(f.store()).unwrap(), "{nope");
    }
}
