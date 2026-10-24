//! Decides which registered loop is due and runs it through `run-loop-now.sh`.
//! Mirrors `bin/loop_scheduler.py` (`is_due`, `run_due_loops`, the state file).
//! The Python `main()` extras (ledger backfill, auth-health check) belong to
//! the modules that own them; this module is the scheduling core.
//!
//! All wall-clock logic takes a naive LOCAL time, matching launchd's own
//! `StartCalendarInterval` semantics the old per-loop plists used.

use crate::config::home_dir;
use crate::loops_config;
use crate::pyjson;
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub type SchedulerState = Map<String, Value>;

pub fn default_state_path() -> PathBuf {
    home_dir().join("loop_scheduler_state.json")
}

/// Where the scheduler's single-poller lock lives.
pub fn default_lock_path() -> PathBuf {
    home_dir().join("scheduler.lock")
}

/// Held for the whole poll (the poll waits for the loops it starts), so the app's
/// built-in scheduler and a launchd `loopx scheduler` can never both start the
/// same due loop. The lock is released when the guard drops or the process dies.
#[derive(Debug)]
pub struct PollLock {
    _file: std::fs::File,
}

/// `Some(guard)` when this process now owns the poll, `None` when another
/// scheduler holds it. A lock file that cannot be opened counts as held: skipping
/// one poll is safe, starting a loop twice is not.
pub fn try_lock_poll(path: &Path) -> Option<PollLock> {
    use std::os::unix::io::AsRawFd;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    // SAFETY: plain flock(2) on a descriptor we own.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (rc == 0).then_some(PollLock { _file: file })
}

/// `<LOOP_ENGINEERING_HOME>/outputs/status.json` for `gitlab-loop`, else
/// `<..>/outputs/status/<loop>.json`. (Python resolves against the checkout
/// root; the other Rust modules use `home_dir()` for `outputs/`, so this does
/// too.)
pub fn status_path_for_loop(loop_name: &str, base_dir: Option<&Path>) -> PathBuf {
    let base = base_dir
        .map_or_else(home_dir, Path::to_path_buf)
        .join("outputs");
    if loop_name == "gitlab-loop" {
        base.join("status.json")
    } else {
        base.join("status").join(format!("{loop_name}.json"))
    }
}

/// The `state` of a status file: `never_run` when absent, `unknown` when
/// corrupt (the `dashboard_server.read_status` contract).
pub fn read_status_state(path: &Path) -> String {
    if !path.exists() {
        return "never_run".into();
    }
    match std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        Some(Value::Object(m)) => m
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => "unknown".into(),
    }
}

/// An empty map when the file is missing, unreadable or not a JSON object.
pub fn read_state(path: &Path) -> SchedulerState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

pub fn write_state(state: &SchedulerState, path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, pyjson::to_string_pretty(state))
}

/// First-install seeding: mark every loop as attempted `today`, so the first poll
/// does not see "never attempted" and fire an unattended run against live
/// GitLab/Slack. Does nothing when the state file already exists.
pub fn seed_state(loops: &[Value], state_path: &Path, today: NaiveDate) {
    if state_path.exists() {
        return;
    }
    let date = today.format("%Y-%m-%d").to_string();
    let state: SchedulerState = loops
        .iter()
        .filter_map(|l| l.get("name").and_then(Value::as_str))
        .map(|name| (name.to_string(), json!({"last_attempted_date": date})))
        .collect();
    let _ = write_state(&state, state_path);
}

/// Python `datetime.isoformat()` for a naive time.
pub fn isoformat(t: NaiveDateTime) -> String {
    let micros = t.nanosecond() / 1000;
    if micros == 0 {
        t.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        t.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
    }
}

fn parse_iso(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok()
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(ny, nm, 1)
        .and_then(|d| d.pred_opt())
        .map_or(28, |d| d.day())
}

fn int_field(schedule: &Value, key: &str) -> i64 {
    schedule.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// Is this registry entry due at `now`? `state` maps loop name to
/// `{"last_attempted_date", "last_attempted_at"}`. A schedule without a
/// `frequency` is a pre-rename entry: `daily` when `weekdays == "all"`, else
/// `weekly`. Disabled loops are never due.
pub fn is_due(loop_: &Value, state: &SchedulerState, now: NaiveDateTime) -> bool {
    if !loop_.get("enabled").is_none_or(loops_config::py_truthy) {
        return false;
    }
    let schedule = loop_.get("schedule").cloned().unwrap_or(Value::Null);
    let name = loop_
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let frequency = match schedule.get("frequency").and_then(Value::as_str) {
        Some(f) => f.to_string(),
        None if schedule.get("weekdays").and_then(Value::as_str) == Some("all") => "daily".into(),
        None => "weekly".into(),
    };
    let last = state.get(name);

    if frequency == "hourly" {
        let Some(last_at) = last
            .and_then(|s| s.get("last_attempted_at"))
            .and_then(Value::as_str)
            .and_then(parse_iso)
        else {
            return true;
        };
        let elapsed = now - last_at;
        let minutes = schedule
            .get("interval_minutes")
            .filter(|v| loops_config::py_truthy(v));
        if let Some(minutes) = minutes.and_then(Value::as_i64) {
            // A minute interval is as long as the scheduler's own 15-minute
            // poll, so allow a minute of slack.
            return elapsed >= Duration::minutes(minutes) - Duration::minutes(1);
        }
        return elapsed >= Duration::hours(int_field(&schedule, "interval_hours"));
    }

    if frequency == "weekly" {
        let weekdays = schedule.get("weekdays").unwrap_or(&Value::Null);
        if weekdays.as_str() != Some("all") {
            let today = i64::from(now.weekday().number_from_monday());
            let listed = weekdays
                .as_array()
                .is_some_and(|a| a.iter().any(|d| d.as_i64() == Some(today)));
            if !listed {
                return false;
            }
        }
    } else if frequency == "monthly" {
        let target =
            int_field(&schedule, "day").min(i64::from(days_in_month(now.year(), now.month())));
        if i64::from(now.day()) != target {
            return false;
        }
    }

    let at = (i64::from(now.hour()), i64::from(now.minute()));
    if at < (int_field(&schedule, "hour"), int_field(&schedule, "minute")) {
        return false;
    }
    let today = now.date().format("%Y-%m-%d").to_string();
    last.and_then(|s| s.get("last_attempted_date"))
        .and_then(Value::as_str)
        != Some(today.as_str())
}

/// Run every due, not-already-running loop in registry order, one at a time.
/// `runner` receives `["bash", <run-loop-now.sh>, <loop name>]` and blocks until
/// the run ends; its failure never stops the rest. `last_attempted_*` is
/// recorded right after each attempt regardless of outcome, so a failed run is
/// not retried until its next slot. Returns the names attempted.
pub fn run_due_loops(
    loops: &[Value],
    state_path: &Path,
    run_loop_now_path: &Path,
    now: NaiveDateTime,
    is_running: &dyn Fn(&str) -> bool,
    runner: &mut dyn FnMut(&[String]) -> std::io::Result<()>,
) -> Vec<String> {
    let mut state = read_state(state_path);
    let mut attempted = Vec::new();
    for loop_ in loops {
        if !is_due(loop_, &state, now) {
            continue;
        }
        let name = loop_
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if is_running(name) {
            continue;
        }
        let argv = vec![
            "bash".to_string(),
            run_loop_now_path.display().to_string(),
            name.to_string(),
        ];
        if let Err(e) = runner(&argv) {
            eprintln!("loop_scheduler: {name} failed to run: {e}");
        }
        let entry = state.entry(name.to_string()).or_insert_with(|| json!({}));
        if let Some(obj) = entry.as_object_mut() {
            obj.insert(
                "last_attempted_date".into(),
                json!(now.date().format("%Y-%m-%d").to_string()),
            );
            obj.insert("last_attempted_at".into(), json!(isoformat(now)));
        }
        if let Err(e) = write_state(&state, state_path) {
            eprintln!("loop_scheduler: failed to record state for {name}: {e}");
        }
        attempted.push(name.to_string());
    }
    attempted
}

/// The real runner: a child in its own session/process group (so a dashboard
/// Stop button can `killpg` it without hitting the scheduler), awaited.
#[cfg(unix)]
pub fn spawn_and_wait(argv: &[String]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command"))?;
    std::process::Command::new(program)
        .args(args)
        .process_group(0)
        .status()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeding_marks_every_loop_attempted_today_and_never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let state_path = d.path().join("state.json");
        let loops = vec![json!({"name": "a"}), json!({"name": "b"})];
        let today = at(2026, 10, 24, 10, 0).date();

        seed_state(&loops, &state_path, today);
        let st = read_state(&state_path);
        assert_eq!(st["a"]["last_attempted_date"], "2026-10-24");
        assert_eq!(st["b"]["last_attempted_date"], "2026-10-24");

        // A second call leaves the existing file alone.
        seed_state(&[json!({"name": "c"})], &state_path, today);
        assert!(!read_state(&state_path).contains_key("c"));
    }

    #[test]
    fn a_second_poller_cannot_take_the_lock_until_the_first_lets_go() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("sub").join("scheduler.lock");
        let first = try_lock_poll(&path).expect("first poller gets the lock");
        assert!(try_lock_poll(&path).is_none());
        drop(first);
        assert!(try_lock_poll(&path).is_some());
    }

    #[test]
    fn an_unopenable_lock_file_counts_as_held() {
        let d = tempfile::tempdir().unwrap();
        // The lock path is a directory: it cannot be opened for writing.
        assert!(try_lock_poll(d.path()).is_none());
    }

    use super::*;
    use chrono::NaiveDate;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    fn daily(hour: i64, minute: i64) -> Value {
        json!({"name": "l", "schedule": {"frequency": "daily", "hour": hour, "minute": minute}})
    }

    fn no_state() -> SchedulerState {
        Map::new()
    }

    fn state_of(date: &str, at: &str) -> SchedulerState {
        let mut m = Map::new();
        m.insert(
            "l".into(),
            json!({"last_attempted_date": date, "last_attempted_at": at}),
        );
        m
    }

    #[test]
    fn disabled_is_never_due() {
        let mut l = daily(0, 0);
        l["enabled"] = json!(false);
        assert!(!is_due(&l, &no_state(), at(2026, 10, 22, 12, 0)));
    }

    #[test]
    fn daily_waits_for_its_time_and_runs_once_per_day() {
        let l = daily(10, 30);
        assert!(!is_due(&l, &no_state(), at(2026, 10, 22, 10, 29)));
        assert!(is_due(&l, &no_state(), at(2026, 10, 22, 10, 30)));
        assert!(is_due(&l, &no_state(), at(2026, 10, 22, 23, 59)));
        let done = state_of("2026-10-22", "2026-10-22T10:30:00");
        assert!(!is_due(&l, &done, at(2026, 10, 22, 11, 0)));
        assert!(is_due(&l, &done, at(2026, 10, 23, 10, 30)));
    }

    #[test]
    fn weekly_respects_isoweekdays_and_all() {
        // 2026-10-22 is a Thursday (4).
        let mk = |wd: Value| json!({"name": "l", "schedule": {"frequency": "weekly", "weekdays": wd, "hour": 0, "minute": 0}});
        let now = at(2026, 10, 22, 9, 0);
        assert!(is_due(&mk(json!([4])), &no_state(), now));
        assert!(!is_due(&mk(json!([1, 2, 3, 5, 6, 7])), &no_state(), now));
        assert!(is_due(&mk(json!("all")), &no_state(), now));
    }

    #[test]
    fn monthly_clamps_to_the_last_day() {
        let l = json!({"name": "l", "schedule": {"frequency": "monthly", "day": 31, "hour": 0, "minute": 0}});
        assert!(is_due(&l, &no_state(), at(2026, 2, 28, 1, 0)));
        assert!(!is_due(&l, &no_state(), at(2026, 2, 27, 1, 0)));
        assert!(is_due(&l, &no_state(), at(2026, 12, 31, 1, 0)));
        assert!(!is_due(&l, &no_state(), at(2026, 12, 30, 1, 0)));
        let l15 = json!({"name": "l", "schedule": {"frequency": "monthly", "day": 15, "hour": 8, "minute": 0}});
        assert!(!is_due(&l15, &no_state(), at(2026, 10, 15, 7, 59)));
        assert!(is_due(&l15, &no_state(), at(2026, 10, 15, 8, 0)));
    }

    #[test]
    fn schedule_without_frequency_is_inferred() {
        let all = json!({"name": "l", "schedule": {"weekdays": "all", "hour": 9, "minute": 0}});
        assert!(is_due(&all, &no_state(), at(2026, 10, 22, 9, 0)));
        let some = json!({"name": "l", "schedule": {"weekdays": [1], "hour": 9, "minute": 0}});
        assert!(!is_due(&some, &no_state(), at(2026, 10, 22, 9, 0))); // Thursday
        assert!(is_due(&some, &no_state(), at(2026, 10, 19, 9, 0))); // Monday
    }

    #[test]
    fn hourly_uses_last_attempted_at() {
        let l = json!({"name": "l", "schedule": {"frequency": "hourly", "interval_hours": 2}});
        assert!(is_due(&l, &no_state(), at(2026, 10, 22, 12, 0)));
        let s = state_of("2026-10-22", "2026-10-22T10:00:00");
        assert!(!is_due(&l, &s, at(2026, 10, 22, 11, 59)));
        assert!(is_due(&l, &s, at(2026, 10, 22, 12, 0)));
    }

    #[test]
    fn hourly_minutes_allow_a_minute_of_slack() {
        let l = json!({"name": "l", "schedule": {"frequency": "hourly", "interval_minutes": 30}});
        let s = state_of("2026-10-22", "2026-10-22T10:00:00.250000");
        assert!(!is_due(&l, &s, at(2026, 10, 22, 10, 29)));
        assert!(is_due(&l, &s, at(2026, 10, 22, 10, 30)));
        let whole = state_of("2026-10-22", "2026-10-22T10:00:00");
        assert!(!is_due(&l, &whole, at(2026, 10, 22, 10, 28)));
        assert!(is_due(&l, &whole, at(2026, 10, 22, 10, 29)));
    }

    #[test]
    fn isoformat_matches_python() {
        assert_eq!(isoformat(at(2026, 10, 22, 9, 5)), "2026-10-22T09:05:00");
        let frac = at(2026, 10, 22, 9, 5) + Duration::microseconds(42);
        assert_eq!(isoformat(frac), "2026-10-22T09:05:00.000042");
    }

    #[test]
    fn state_file_round_trips_and_tolerates_corruption() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub").join("state.json");
        assert!(read_state(&p).is_empty());
        write_state(&state_of("2026-10-22", "x"), &p).unwrap();
        assert_eq!(read_state(&p)["l"]["last_attempted_date"], "2026-10-22");
        std::fs::write(&p, "{broken").unwrap();
        assert!(read_state(&p).is_empty());
        std::fs::write(&p, "[1]").unwrap();
        assert!(read_state(&p).is_empty());
    }

    #[test]
    fn status_paths_and_state() {
        let base = Path::new("/x");
        assert_eq!(
            status_path_for_loop("gitlab-loop", Some(base)),
            Path::new("/x/outputs/status.json")
        );
        assert_eq!(
            status_path_for_loop("topic-loop", Some(base)),
            Path::new("/x/outputs/status/topic-loop.json")
        );
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.json");
        assert_eq!(read_status_state(&p), "never_run");
        std::fs::write(&p, r#"{"state": "running"}"#).unwrap();
        assert_eq!(read_status_state(&p), "running");
        std::fs::write(&p, "nope").unwrap();
        assert_eq!(read_status_state(&p), "unknown");
    }

    #[test]
    fn run_due_loops_runs_due_records_state_and_skips_running() {
        let d = tempfile::tempdir().unwrap();
        let sp = d.path().join("state.json");
        let loops = vec![
            json!({"name": "a", "schedule": {"frequency": "daily", "hour": 9, "minute": 0}}),
            json!({"name": "busy", "schedule": {"frequency": "daily", "hour": 9, "minute": 0}}),
            json!({"name": "later", "schedule": {"frequency": "daily", "hour": 23, "minute": 0}}),
            json!({"name": "off", "enabled": false, "schedule": {"frequency": "daily", "hour": 0, "minute": 0}}),
            json!({"name": "boom", "schedule": {"frequency": "daily", "hour": 9, "minute": 0}}),
        ];
        let now = at(2026, 10, 22, 10, 0);
        let mut calls: Vec<Vec<String>> = Vec::new();
        let mut runner = |argv: &[String]| {
            calls.push(argv.to_vec());
            if argv[2] == "boom" {
                Err(std::io::Error::other("nope"))
            } else {
                Ok(())
            }
        };
        let attempted = run_due_loops(
            &loops,
            &sp,
            Path::new("/r/run-loop-now.sh"),
            now,
            &|n| n == "busy",
            &mut runner,
        );
        assert_eq!(attempted, ["a", "boom"]);
        assert_eq!(calls[0], ["bash", "/r/run-loop-now.sh", "a"]);
        let st = read_state(&sp);
        assert_eq!(st["a"]["last_attempted_date"], "2026-10-22");
        assert_eq!(st["a"]["last_attempted_at"], "2026-10-22T10:00:00");
        assert!(st.contains_key("boom"), "a failed run is still recorded");
        assert!(!st.contains_key("busy") && !st.contains_key("later"));
        // Same day again: nothing is due any more.
        let mut none = |_: &[String]| Ok(());
        assert!(
            run_due_loops(&loops, &sp, Path::new("x"), now, &|_| false, &mut none)
                .iter()
                .all(|n| n == "busy")
        );
    }

    #[cfg(unix)]
    #[test]
    fn spawn_and_wait_runs_the_command() {
        let d = tempfile::tempdir().unwrap();
        let marker = d.path().join("m");
        let script = d.path().join("run.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho \"$1\" > {}\n", marker.display()),
        )
        .unwrap();
        spawn_and_wait(&["bash".into(), script.display().to_string(), "name-1".into()]).unwrap();
        assert_eq!(std::fs::read_to_string(marker).unwrap().trim(), "name-1");
        assert!(spawn_and_wait(&["/nonexistent/prog".into()]).is_err());
    }
}
