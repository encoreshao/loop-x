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

/// Process the event-driven queue, then run the sync if it is due (a slot passed since the last run) or was requested.
/// Returns whether it did anything. Writes nothing unless it runs; with the weekly switch
/// off only an explicit "Sync now" request runs.
#[allow(clippy::too_many_arguments)]
pub fn tick(
    store_path: &Path,
    projects: impl FnOnce() -> Vec<TrackedProject>,
    agent: &dyn Agent,
    now: DateTime<Utc>,
    state: &mut TickState,
    root: Option<&Path>,
    approvals_dir: &Path,
    activity: Option<&Path>,
) -> bool {
    let Ok(store) = sync::load_keeping_bad(store_path) else {
        return false;
    };
    // Event-driven refreshes queued by finished runs go first (only while the
    // weekly switch is on; the queue is never written otherwise).
    let mut ran_queue = false;
    let mut projects = Some(projects);
    let mut loaded: Option<Vec<TrackedProject>> = None;
    if store.settings.enabled && store.queued.values().any(|v| !v.is_empty()) {
        let list = loaded.insert(projects.take().map_or_else(Vec::new, |f| f()));
        let res = sync::process_queue_with_progress(
            &store.queued,
            list,
            agent,
            now,
            root,
            approvals_dir,
            activity,
        );
        let _ = sync::with_store(store_path, |s| {
            for (alias, slug) in res.done {
                // Only the entry this pass worked on: one queued again meanwhile
                // (a newer `queued_at`) waits for the next look.
                let seen = store
                    .queued
                    .get(&alias)
                    .and_then(|q| q.iter().find(|x| x.section == slug));
                if let Some(q) = s.queued.get_mut(&alias) {
                    q.retain(|x| x.section != slug || seen != Some(x));
                }
            }
            // A dropped-on-failure entry leaves a note, so it is not silent.
            for (alias, msg) in res.failures {
                s.status.per_project.insert(
                    alias,
                    ProjectSync {
                        at: now,
                        outcome: sync::Outcome::Failed(msg),
                    },
                );
            }
            s.queued.retain(|_, q| !q.is_empty());
        });
        ran_queue = true;
    }
    let started = *state.started.get_or_insert(now);
    let baseline = store.status.last_run.unwrap_or(started);
    let due = store.settings.enabled && sync::next_run(&store.settings, baseline) <= now;
    if !due && !store.sync_requested {
        return ran_queue;
    }
    let mut list = loaded.unwrap_or_else(|| projects.take().map_or_else(Vec::new, |f| f()));
    // Oldest-synced first, so a cap never starves the same projects every week.
    list.sort_by_key(|p| store.status.per_project.get(&p.alias).map(|s| s.at));
    list.truncate(store.settings.max_projects_per_run);
    // Claimed BEFORE the (paid, possibly hours-long) run: if this write fails the
    // run does not start, so a stuck flag cannot re-run it every minute, and a
    // "Sync now" pressed during the run is kept for the next look.
    let claimed = sync::with_store(store_path, |s| {
        s.status.last_run = Some(now);
        s.sync_requested = false;
    });
    if claimed.is_err() {
        return ran_queue;
    }
    let results = sync::sync_all_with_progress(&list, agent, now, root, approvals_dir, activity);
    let _ = sync::with_store(store_path, |s| {
        for (alias, outcome) in results {
            s.status
                .per_project
                .insert(alias, ProjectSync { at: now, outcome });
        }
    });
    true
}

/// The dashboard daemon's profile sync loop. Changing the schedule needs no restart.
/// Each build and refresh it runs is a live job under `activity` (the checkout's
/// `outputs/agent-activity.json`, which the Agents and Terminal pages and the Mac
/// app read).
pub fn start_profile_sync(activity: std::path::PathBuf) {
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
                Some(&activity),
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
        /// Unique per test: the single-flight slot is process-wide.
        alias: String,
    }
    impl Fx {
        fn new() -> Fx {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            Fx {
                dir: tempfile::tempdir().unwrap(),
                work: tempfile::tempdir().unwrap(),
                alias: format!(
                    "tick-app-{}",
                    N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                ),
            }
        }
        fn activity(&self) -> std::path::PathBuf {
            self.dir.path().join("outputs/agent-activity.json")
        }
        fn store(&self) -> std::path::PathBuf {
            self.dir.path().join("profile_sync.json")
        }
        fn projects(&self) -> Vec<TrackedProject> {
            vec![TrackedProject {
                alias: self.alias.clone(),
                instance: "work".into(),
                project_id: "g/app".into(),
                work_dir: self.work.path().to_path_buf(),
            }]
        }
        fn tick_with(&self, agent: &dyn Agent, now: DateTime<Utc>) -> bool {
            tick(
                &self.store(),
                || self.projects(),
                agent,
                now,
                &mut TickState::default(),
                Some(self.dir.path()),
                &self.dir.path().join("approvals"),
                Some(&self.activity()),
            )
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
                Some(&self.activity()),
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
            s.status.per_project[&f.alias].outcome,
            Outcome::Failed(_) | Outcome::Skipped | Outcome::Drafted
        ));
        // Same slot: not again.
        assert!(!f.tick(&a, local(16, 9, 2), &mut st));
        assert!(!f.tick(&a, local(16, 9, 1) + Duration::days(6), &mut st));
        // Next Monday: again.
        assert!(f.tick(&a, local(23, 9, 1), &mut st));
    }

    #[test]
    fn a_sync_run_is_a_live_job_and_leaves_no_record_behind() {
        let f = Fx::new();
        with_a_file(&f);
        sync::with_store(&f.store(), |s| s.sync_requested = true).unwrap();
        let base = f.activity();
        let seen = std::cell::RefCell::new(Vec::new());
        let a = During(
            || {
                seen.borrow_mut()
                    .push(loopx_core::profile_progress::running(&base))
            },
            Cell::new(0),
        );
        assert!(f.tick_with(&a, local(11, 10, 0)));
        let live = seen.borrow();
        let kinds: Vec<(String, String)> = live[0]
            .iter()
            .map(|j| (j.kind.clone(), j.alias.clone()))
            .collect();
        assert!(
            kinds.contains(&("sync".into(), f.alias.clone())),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("build".into(), f.alias.clone())),
            "{kinds:?}"
        );
        assert!(loopx_core::profile_progress::running(&base).is_empty());
        let trace =
            std::fs::read_to_string(f.dir.path().join("outputs/agent-trace.jsonl")).unwrap();
        assert!(
            trace.contains("\"phase\":\"failed\"") && trace.contains("the AI call failed"),
            "{trace}"
        );
        assert!(
            !trace.contains("no AI CLI in tests"),
            "the error text stays out: {trace}"
        );
    }

    #[test]
    fn sync_now_runs_even_off_schedule_and_clears_the_flag() {
        let f = Fx::new();
        let (a, mut st) = (Counting(Cell::new(0)), TickState::default());
        sync::with_store(&f.store(), |s| s.sync_requested = true).unwrap();
        assert!(f.tick(&a, local(11, 10, 0), &mut st));
        let s = sync::load(&f.store()).unwrap();
        assert!(!s.sync_requested);
        assert!(s.status.per_project.contains_key(&f.alias));
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
            Some(&f.activity()),
        );
        assert!(ran);
        let s = sync::load(&f.store()).unwrap();
        // The never-synced project went first; the old one still has its old stamp.
        assert_eq!(s.status.per_project["tick-old"].at, local(1, 0, 0));
        assert_eq!(s.status.per_project[&f.alias].at, local(11, 10, 0));
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

    #[test]
    fn tick_processes_the_event_queue_and_clears_it() {
        let f = Fx::new();
        let (a, mut st) = (Counting(Cell::new(0)), TickState::default());
        // Off the weekly schedule, no sync request: only the queue is pending.
        sync::with_store(&f.store(), |s| {
            let q = |slug: &str| sync::QueuedSection {
                section: slug.into(),
                queued_at: local(11, 9, 0),
                memory: false,
            };
            s.queued
                .insert(f.alias.clone(), vec![q("stack"), q("gotchas")]);
            s.queued.insert("other".into(), vec![q("stack")]);
        })
        .unwrap();
        assert!(f.tick(&a, local(11, 10, 0), &mut st));
        let s = sync::load(&f.store()).unwrap();
        assert!(s.queued.is_empty(), "{:?}", s.queued);
        // Nothing left to do on the next look.
        assert!(!f.tick(&a, local(11, 10, 1), &mut st));
    }

    #[test]
    fn a_disabled_switch_leaves_the_queue_alone() {
        let f = Fx::new();
        let (a, mut st) = (Counting(Cell::new(0)), TickState::default());
        sync::with_store(&f.store(), |s| {
            s.settings.enabled = false;
            s.queued.insert(
                f.alias.clone(),
                vec![sync::QueuedSection {
                    section: "stack".into(),
                    queued_at: local(11, 9, 0),
                    memory: false,
                }],
            );
        })
        .unwrap();
        assert!(!f.tick(&a, local(11, 10, 0), &mut st));
        assert_eq!(sync::load(&f.store()).unwrap().queued.len(), 1);
    }

    #[test]
    fn a_failed_queue_entry_is_recorded_in_the_status() {
        let f = Fx::new();
        let (a, mut st) = (Counting(Cell::new(0)), TickState::default());
        sync::with_store(&f.store(), |s| {
            s.queued.insert(
                f.alias.clone(),
                vec![sync::QueuedSection {
                    section: "stack".into(),
                    queued_at: local(11, 9, 0),
                    memory: false,
                }],
            );
        })
        .unwrap();
        // The checkout is gone: the entry is dropped and the status says why.
        let gone = f.work.path().join("missing");
        let list = vec![TrackedProject {
            work_dir: gone,
            ..f.projects()[0].clone()
        }];
        let ran = tick(
            &f.store(),
            || list,
            &a,
            local(11, 10, 0),
            &mut st,
            Some(f.dir.path()),
            &f.dir.path().join("approvals"),
            Some(&f.activity()),
        );
        assert!(ran);
        let s = sync::load(&f.store()).unwrap();
        assert!(s.queued.is_empty());
        assert!(matches!(
            s.status.per_project[&f.alias].outcome,
            Outcome::Failed(_)
        ));
    }

    /// Runs a hook on every agent call (to act while a run is in progress).
    struct During<F: Fn()>(F, Cell<usize>);
    impl<F: Fn()> Agent for During<F> {
        fn run(&self, _: &str) -> Result<String, String> {
            self.1.set(self.1.get() + 1);
            (self.0)();
            Err("no AI CLI in tests".into())
        }
    }

    fn with_a_file(f: &Fx) {
        std::fs::write(f.work.path().join("README.md"), "hi\n").unwrap();
    }

    #[test]
    fn sync_now_pressed_during_a_run_is_kept_for_the_next_look() {
        let f = Fx::new();
        with_a_file(&f);
        sync::with_store(&f.store(), |s| s.sync_requested = true).unwrap();
        let store = f.store();
        let a = During(
            || {
                sync::with_store(&store, |s| s.sync_requested = true).unwrap();
            },
            Cell::new(0),
        );
        let ran = tick(
            &f.store(),
            || f.projects(),
            &a,
            local(11, 10, 0),
            &mut TickState::default(),
            Some(f.dir.path()),
            &f.dir.path().join("approvals"),
            Some(&f.activity()),
        );
        assert!(ran);
        assert_eq!(a.1.get(), 1, "the build ran");
        let s = sync::load(&f.store()).unwrap();
        assert!(s.sync_requested, "the request made during the run survives");
        assert_eq!(s.status.last_run, Some(local(11, 10, 0)));
    }

    #[test]
    fn no_paid_run_starts_when_the_claim_cannot_be_written() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fx::new();
        with_a_file(&f);
        sync::with_store(&f.store(), |s| s.sync_requested = true).unwrap();
        let ro = std::fs::Permissions::from_mode(0o555);
        std::fs::set_permissions(f.dir.path(), ro).unwrap();
        let a = Counting(Cell::new(0));
        f.tick(&a, local(11, 10, 0), &mut TickState::default());
        std::fs::set_permissions(f.dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(a.0.get(), 0, "no agent call");
        assert!(sync::load(&f.store()).unwrap().sync_requested);
    }

    #[test]
    fn an_entry_queued_again_during_the_queue_pass_is_kept() {
        use loopx_core::project_profile::{self, Profile, Status};
        let f = Fx::new();
        with_a_file(&f);
        project_profile::save(
            &Profile {
                alias: f.alias.clone(),
                generated_at: None,
                head_sha: None,
                status: Status::Approved,
                links: vec![],
                sections: Default::default(),
            },
            Some(f.dir.path()),
        )
        .unwrap();
        let entry = |at| sync::QueuedSection {
            section: "gotchas".into(),
            queued_at: at,
            memory: true,
        };
        let alias = f.alias.clone();
        sync::with_store(&f.store(), |s| {
            s.queued.insert(alias.clone(), vec![entry(local(11, 9, 0))]);
        })
        .unwrap();
        let store = f.store();
        let a = During(
            || {
                sync::with_store(&store, |s| {
                    s.queued
                        .insert(alias.clone(), vec![entry(local(11, 9, 30))]);
                })
                .unwrap();
            },
            Cell::new(0),
        );
        f.tick_with(&a, local(11, 10, 0));
        assert_eq!(a.1.get(), 1);
        let s = sync::load(&f.store()).unwrap();
        assert_eq!(s.queued[&f.alias], vec![entry(local(11, 9, 30))]);
    }
}
