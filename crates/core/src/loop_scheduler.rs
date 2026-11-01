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

/// `<LOOP_ENGINEERING_HOME>/outputs/status.json` for `issue-loop`, else
/// `<..>/outputs/status/<loop>.json`. (Python resolves against the checkout
/// root; the other Rust modules use `home_dir()` for `outputs/`, so this does
/// too.)
pub fn status_path_for_loop(loop_name: &str, base_dir: Option<&Path>) -> PathBuf {
    let base = base_dir
        .map_or_else(home_dir, Path::to_path_buf)
        .join("outputs");
    if crate::loops_config::canonical_loop_name(loop_name) == crate::loops_config::ISSUE_LOOP {
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
/// A renamed loop's entry reads under its current id (a key already written
/// under the new id wins), so a rename never makes a loop look never-run.
pub fn read_state(path: &Path) -> SchedulerState {
    let raw = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default();
    let mut out = SchedulerState::new();
    for (name, value) in raw {
        let current = crate::loops_config::canonical_loop_name(&name);
        if current != name && out.contains_key(current) {
            continue;
        }
        out.insert(current.to_string(), value);
    }
    out
}

pub fn write_state(state: &SchedulerState, path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, pyjson::to_string_pretty(state))
}

/// Clears `name`'s recorded attempt so it is due again: a run held back by the
/// usage limit gets its slot back once the pause lifts.
pub fn forget_attempt(name: &str, path: &Path) {
    let mut state = read_state(path);
    if state.remove(name).is_some() {
        let _ = write_state(&state, path);
    }
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
            // Ticks are a minute apart, so allow a minute of slack.
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

/// How late a daily/weekly/monthly slot may be picked up and still count as on time.
const ON_TIME_MINUTES: i64 = 5;

/// How long ago `loop_`'s time-of-day slot passed today. Hourly loops have no slot.
fn slot_lateness(loop_: &Value, now: NaiveDateTime) -> Option<Duration> {
    let schedule = loop_.get("schedule").cloned().unwrap_or(Value::Null);
    if schedule.get("frequency").and_then(Value::as_str) == Some("hourly") {
        return None;
    }
    let slot = now.date().and_hms_opt(
        int_field(&schedule, "hour").clamp(0, 23) as u32,
        int_field(&schedule, "minute").clamp(0, 59) as u32,
        0,
    )?;
    Some(now - slot)
}

/// Whether the slot passed more than [`ON_TIME_MINUTES`] ago: the Mac slept or
/// LoopX was closed at the time.
fn slot_missed(lateness: Option<Duration>) -> bool {
    lateness.is_some_and(|late| late > Duration::minutes(ON_TIME_MINUTES))
}

/// "2h 30m", "2h", "45m".
fn hours_minutes(d: Duration) -> String {
    let (h, m) = (d.num_minutes() / 60, d.num_minutes() % 60);
    match (h, m) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

/// Why a missed slot is too late to run, when the loop's `max_late_minutes`
/// says so: a morning briefing that would only go out in the evening.
fn too_late(loop_: &Value, lateness: Option<Duration>) -> Option<String> {
    let limit = loop_
        .get("schedule")
        .and_then(|s| s.get("max_late_minutes"))
        .and_then(Value::as_i64)
        .map(Duration::minutes)?;
    let late = lateness.filter(|late| *late > limit)?;
    let schedule = &loop_["schedule"];
    Some(format!(
        "Missed its {:02}:{:02} slot by {}, over its {} limit",
        int_field(schedule, "hour"),
        int_field(schedule, "minute"),
        hours_minutes(late),
        hours_minutes(limit)
    ))
}

/// Meeting Prep between its calendar refreshes: due on the tick a meeting in the
/// agenda it last saved comes within the user's "Minutes ahead".
pub fn meeting_prep_due(
    loop_: &Value,
    state: &SchedulerState,
    now: NaiveDateTime,
    repo_root: &Path,
) -> bool {
    use crate::loop_plugins::calendar_prep;
    use chrono::{Local, TimeZone, Utc};
    let name = "meeting-prep-loop";
    if loop_.get("name").and_then(Value::as_str) != Some(name)
        || !loop_.get("enabled").is_none_or(loops_config::py_truthy)
    {
        return false;
    }
    let utc = |t: NaiveDateTime| {
        Local
            .from_local_datetime(&t)
            .earliest()
            .map(|d| d.with_timezone(&Utc))
    };
    let since = state
        .get(name)
        .and_then(|s| s.get("last_attempted_at"))
        .and_then(Value::as_str)
        .and_then(parse_iso)
        .and_then(utc);
    let (Some(since), Some(now)) = (since, utc(now)) else {
        return false;
    };
    calendar_prep::lead_window_opened(
        &calendar_prep::today_path(repo_root),
        loop_.get("settings").unwrap_or(&Value::Null),
        since,
        now,
    )
}

/// The scheduler tick: queue every due loop that is not already queued or
/// running (the worker starts them one by one) and record the attempt, so a loop
/// is queued once per slot. Returns the names queued, in registry order.
pub fn enqueue_due_loops(
    loops: &[Value],
    state_path: &Path,
    queue: &crate::run_queue::RunQueue,
    now: NaiveDateTime,
) -> Vec<String> {
    use crate::run_queue::{Enqueued, Reason};
    let mut state = read_state(state_path);
    let mut queued = Vec::new();
    let repo_root = crate::runner_util::repo_root();
    for loop_ in loops {
        if !is_due(loop_, &state, now) && !meeting_prep_due(loop_, &state, now, &repo_root) {
            continue;
        }
        let name = loop_
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let lateness = slot_lateness(loop_, now);
        let reason = if slot_missed(lateness) {
            Reason::CatchUp
        } else {
            Reason::Scheduled
        };
        let outcome = match too_late(loop_, lateness) {
            Some(note) => queue
                .record_skipped(name, reason, &note, &isoformat(now))
                .map(|()| None),
            None => queue.enqueue(name, reason, &isoformat(now)).map(Some),
        };
        match outcome {
            Ok(None) => {}
            Ok(Some(Enqueued::Queued { .. })) => queued.push(name.to_string()),
            // Already waiting or running: leave it for the next slot.
            Ok(Some(Enqueued::Coalesced { .. })) => continue,
            Err(e) => {
                eprintln!("loop_scheduler: could not queue {name}: {e}");
                continue;
            }
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
    }
    queued
}

#[cfg(test)]
mod tests {
    #[test]
    fn meeting_prep_is_due_between_refreshes_when_a_meeting_enters_its_lead_window() {
        use chrono::{Local, TimeZone};
        let d = tempfile::tempdir().unwrap();
        let last = at(2026, 10, 28, 10, 0);
        let now = at(2026, 10, 28, 10, 3);
        let local = |t: NaiveDateTime| Local.from_local_datetime(&t).earliest().unwrap();
        let prep = json!({"name": "meeting-prep-loop",
            "schedule": {"frequency": "hourly", "interval_minutes": 15},
            "settings": {"lead_minutes": "45"}});
        let mut state = SchedulerState::new();
        state.insert(
            "meeting-prep-loop".into(),
            json!({"last_attempted_at": isoformat(last)}),
        );
        assert!(
            !is_due(&prep, &state, now),
            "the 15-minute refresh is not due"
        );
        assert!(
            !meeting_prep_due(&prep, &state, now, d.path()),
            "no agenda yet"
        );

        let today = crate::loop_plugins::calendar_prep::today_path(d.path());
        std::fs::create_dir_all(today.parent().unwrap()).unwrap();
        let start = local(at(2026, 10, 28, 10, 47)).to_rfc3339();
        std::fs::write(&today, json!({"meetings": [{"start": start}]}).to_string()).unwrap();
        assert!(meeting_prep_due(&prep, &state, now, d.path()));

        let mut off = prep.clone();
        off["enabled"] = json!(false);
        assert!(!meeting_prep_due(&off, &state, now, d.path()));
        let mut other = prep.clone();
        other["name"] = json!("other-loop");
        assert!(!meeting_prep_due(&other, &state, now, d.path()));
    }

    #[test]
    fn the_old_gitlab_loop_state_is_the_issue_loops() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("state.json");
        std::fs::write(
            &p,
            r#"{"gitlab-loop": {"last_attempted_date": "2026-10-30"}, "topic-loop": {"last_attempted_date": "2026-10-29"}}"#,
        )
        .unwrap();
        let st = read_state(&p);
        // Otherwise the renamed loop would look never-run and fire at once.
        assert_eq!(st["issue-loop"]["last_attempted_date"], "2026-10-30");
        assert!(!st.contains_key("gitlab-loop"));
        assert_eq!(st["topic-loop"]["last_attempted_date"], "2026-10-29");
        // When both exist, the new key (written by this version) wins.
        std::fs::write(
            &p,
            r#"{"gitlab-loop": {"last_attempted_date": "2026-10-01"}, "issue-loop": {"last_attempted_date": "2026-10-31"}}"#,
        )
        .unwrap();
        assert_eq!(
            read_state(&p)["issue-loop"]["last_attempted_date"],
            "2026-10-31"
        );
    }

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
    fn forgetting_an_attempt_makes_the_loop_due_again() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("state.json");
        let mut st = SchedulerState::new();
        st.insert("l".into(), json!({"last_attempted_date": "2026-10-22"}));
        st.insert("other".into(), json!({"last_attempted_date": "2026-10-22"}));
        write_state(&st, &p).unwrap();
        let l = daily(9, 0);
        assert!(!is_due(&l, &read_state(&p), at(2026, 10, 22, 12, 0)));
        forget_attempt("l", &p);
        assert!(is_due(&l, &read_state(&p), at(2026, 10, 22, 12, 0)));
        assert!(read_state(&p).contains_key("other"));
        forget_attempt("missing", &d.path().join("none.json"));
        assert!(!d.path().join("none.json").exists());
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
            status_path_for_loop("issue-loop", Some(base)),
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
    fn enqueue_due_loops_queues_due_ones_once_and_records_state() {
        let d = tempfile::tempdir().unwrap();
        let sp = d.path().join("state.json");
        let queue = crate::run_queue::RunQueue::new(d.path().join("q.json"));
        let loops = vec![
            json!({"name": "a", "schedule": {"frequency": "daily", "hour": 9, "minute": 0}}),
            json!({"name": "b", "schedule": {"frequency": "daily", "hour": 10, "minute": 0}}),
            json!({"name": "later", "schedule": {"frequency": "daily", "hour": 23, "minute": 0}}),
            json!({"name": "off", "enabled": false, "schedule": {"frequency": "daily", "hour": 0, "minute": 0}}),
        ];
        let now = at(2026, 10, 22, 10, 0);
        assert_eq!(enqueue_due_loops(&loops, &sp, &queue, now), ["a", "b"]);
        let snap = queue.snapshot(10);
        let reason = |n: &str| {
            snap.queued
                .iter()
                .find(|e| e.loop_name == n)
                .unwrap()
                .reason
                .clone()
        };
        assert_eq!(reason("a"), "catch-up", "its 09:00 slot was an hour ago");
        assert_eq!(reason("b"), "scheduled");
        let st = read_state(&sp);
        assert_eq!(st["a"]["last_attempted_at"], "2026-10-22T10:00:00");
        assert!(!st.contains_key("later") && !st.contains_key("off"));
        // Same day again: nothing is due any more.
        assert!(enqueue_due_loops(&loops, &sp, &queue, now).is_empty());
    }

    #[test]
    fn a_slot_missed_by_more_than_max_late_minutes_is_skipped_not_run() {
        let d = tempfile::tempdir().unwrap();
        let sp = d.path().join("state.json");
        let queue = crate::run_queue::RunQueue::new(d.path().join("q.json"));
        let loops = vec![
            json!({"name": "brief", "schedule": {"frequency": "daily", "hour": 8, "minute": 0, "max_late_minutes": 120}}),
            json!({"name": "near", "schedule": {"frequency": "daily", "hour": 9, "minute": 0, "max_late_minutes": 120}}),
        ];
        let now = at(2026, 10, 22, 10, 30);
        assert_eq!(enqueue_due_loops(&loops, &sp, &queue, now), ["near"]);
        let snap = queue.snapshot(10);
        assert_eq!(snap.queued.len(), 1);
        let skipped = &snap.recent[0];
        assert_eq!(
            (skipped.loop_name.as_str(), skipped.state.as_str()),
            ("brief", "skipped")
        );
        assert_eq!(skipped.reason, "catch-up");
        assert_eq!(
            skipped.note.as_deref(),
            Some("Missed its 08:00 slot by 2h 30m, over its 2h limit")
        );
        // Recorded as attempted, so it waits for tomorrow's slot.
        assert_eq!(
            read_state(&sp)["brief"]["last_attempted_date"],
            "2026-10-22"
        );
        assert!(enqueue_due_loops(&loops, &sp, &queue, at(2026, 10, 22, 10, 31)).is_empty());
        assert_eq!(
            queue.snapshot(10).recent.len(),
            1,
            "skipped once, not every tick"
        );
    }

    #[test]
    fn a_loop_still_queued_is_not_queued_or_marked_attempted_again() {
        let d = tempfile::tempdir().unwrap();
        let sp = d.path().join("state.json");
        let queue = crate::run_queue::RunQueue::new(d.path().join("q.json"));
        let loops =
            vec![json!({"name": "h", "schedule": {"frequency": "hourly", "interval_minutes": 5}})];
        assert_eq!(
            enqueue_due_loops(&loops, &sp, &queue, at(2026, 10, 22, 10, 0)),
            ["h"]
        );
        // Five minutes later it is due again but is still waiting behind a long run.
        assert!(enqueue_due_loops(&loops, &sp, &queue, at(2026, 10, 22, 10, 6)).is_empty());
        assert_eq!(queue.snapshot(10).queued.len(), 1);
        assert_eq!(
            read_state(&sp)["h"]["last_attempted_at"],
            "2026-10-22T10:00:00"
        );
    }
}
