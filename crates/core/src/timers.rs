//! One-off timers: "remind me at 9 tomorrow", or an agent's "look at this issue
//! again in 24 hours". A timer either sends its note to you through the outbox,
//! or queues a run of a loop (one issue's run, for the issue loop) and leaves the
//! note for that run. Each timer is one file under `outputs/timers/`; the
//! scheduler's minute tick ([`tick`]) fires the due ones.

use crate::file_lock::FileLock;
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Longest note a timer keeps (characters).
pub const MAX_NOTE: usize = 500;
/// Timers waiting at once.
pub const MAX_PENDING: usize = 200;
/// How far ahead a timer may be set.
pub const MAX_AHEAD_DAYS: i64 = 90;
/// Notes kept for a woken run at most; older ones drop off.
const MAX_NOTES_PER_RUN: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Send the note to you.
    Remind {
        #[serde(default)]
        urgent: bool,
    },
    /// Queue `loopx run <loop_name> <args…>`, with the note for that run.
    Run {
        loop_name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timer {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub due_at: DateTime<Utc>,
    pub note: String,
    pub action: Action,
    /// `you`, `chat`, or the loop whose agent set it.
    pub set_by: String,
}

pub fn default_dir() -> PathBuf {
    crate::runner_util::repo_root()
        .join("outputs")
        .join("timers")
}

/// When `spec` falls, from `now`: `30m`, `2h`, `1d`, `1h30m` from now; `09:00`
/// (the next 09:00); `2026-12-01 09:00` or `2026-12-01` (09:00) on the wall
/// clock; or an RFC 3339 time.
pub fn parse_when(spec: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let spec = spec.trim();
    let bad = || {
        format!("can't read the time \"{spec}\": use 30m, 2h, 1d, 09:00, 2026-12-01 09:00 or an RFC 3339 time")
    };
    if let Some(d) = parse_duration(spec) {
        return Ok(now + d);
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(spec) {
        return Ok(t.with_timezone(&Utc));
    }
    let local_now = now.with_timezone(&Local).naive_local();
    let naive = if let Ok(t) = NaiveTime::parse_from_str(spec, "%H:%M") {
        let today = local_now.date().and_time(t);
        if today > local_now {
            today
        } else {
            today + Duration::days(1)
        }
    } else if let Ok(dt) = NaiveDateTime::parse_from_str(spec, "%Y-%m-%d %H:%M") {
        dt
    } else if let Ok(d) = NaiveDate::parse_from_str(spec, "%Y-%m-%d") {
        d.and_time(NaiveTime::from_hms_opt(9, 0, 0).expect("valid"))
    } else {
        return Err(bad());
    };
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
        .ok_or_else(bad)
}

fn parse_duration(spec: &str) -> Option<Duration> {
    let mut total = Duration::zero();
    let mut digits = String::new();
    let mut any = false;
    for c in spec.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let n: i64 = digits.parse().ok()?;
        digits.clear();
        total += match c {
            'm' => Duration::minutes(n),
            'h' => Duration::hours(n),
            'd' => Duration::days(n),
            'w' => Duration::weeks(n),
            _ => return None,
        };
        any = true;
    }
    (any && digits.is_empty()).then_some(total)
}

fn lock(dir: &Path) -> std::io::Result<FileLock> {
    FileLock::exclusive(&dir.join(".lock"))
}

fn path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// Every waiting timer, soonest first.
pub fn list(dir: &Path) -> Vec<Timer> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Timer> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| serde_json::from_slice(&std::fs::read(p).ok()?).ok())
        .collect();
    out.sort_by(|a, b| (a.due_at, &a.id).cmp(&(b.due_at, &b.id)));
    out
}

/// Save a new timer due at `due_at`. Refuses an empty note, a time in the past
/// or too far ahead, and a full list.
pub fn add(
    dir: &Path,
    due_at: DateTime<Utc>,
    note: &str,
    action: Action,
    set_by: &str,
    now: DateTime<Utc>,
) -> Result<Timer, String> {
    let note: String = note.trim().chars().take(MAX_NOTE).collect();
    if note.is_empty() {
        return Err("a timer needs a note saying what it is for".into());
    }
    if due_at <= now {
        return Err("that time has already passed".into());
    }
    if due_at > now + Duration::days(MAX_AHEAD_DAYS) {
        return Err(format!(
            "a timer can be at most {MAX_AHEAD_DAYS} days ahead"
        ));
    }
    if let Action::Run { loop_name, .. } = &action {
        if loop_name.trim().is_empty() {
            return Err("name the loop to run".into());
        }
    }
    let _lock = lock(dir).map_err(|e| e.to_string())?;
    if list(dir).len() >= MAX_PENDING {
        return Err(format!(
            "{MAX_PENDING} timers are already waiting; cancel some first"
        ));
    }
    let timer = Timer {
        id: format!(
            "t{}-{}",
            now.format("%Y%m%d%H%M%S"),
            &crate::events::new_uuid_hex()[..6]
        ),
        created_at: now,
        due_at,
        note,
        action,
        set_by: set_by.to_string(),
    };
    write(dir, &timer).map_err(|e| e.to_string())?;
    Ok(timer)
}

fn write(dir: &Path, t: &Timer) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(t).map_err(std::io::Error::other)?;
    crate::settings_store::atomic_write(&path(dir, &t.id), &bytes, Some(0o600))
}

/// Remove a waiting timer; whether it was there.
pub fn cancel(dir: &Path, id: &str) -> bool {
    if id.is_empty() || id.contains(['/', '.']) {
        return false;
    }
    let Ok(_lock) = lock(dir) else {
        return false;
    };
    std::fs::remove_file(path(dir, id)).is_ok()
}

/// Where the notes for a woken run of `loop_name args` wait.
fn notes_path(dir: &Path, loop_name: &str, args: &[String]) -> PathBuf {
    let key: String = std::iter::once(loop_name)
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join("_")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join("notes").join(format!("{key}.json"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub set_at: DateTime<Utc>,
    pub set_by: String,
    pub note: String,
}

fn leave_note(dir: &Path, t: &Timer, loop_name: &str, args: &[String]) -> std::io::Result<()> {
    let p = notes_path(dir, loop_name, args);
    let mut notes: Vec<Note> = std::fs::read(&p)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    notes.push(Note {
        set_at: t.created_at,
        set_by: t.set_by.clone(),
        note: t.note.clone(),
    });
    let skip = notes.len().saturating_sub(MAX_NOTES_PER_RUN);
    let notes = &notes[skip..];
    let bytes = serde_json::to_vec_pretty(notes).map_err(std::io::Error::other)?;
    crate::settings_store::atomic_write(&p, &bytes, Some(0o600))
}

/// The notes timers left for the next run of `loop_name args`.
pub fn notes(dir: &Path, loop_name: &str, args: &[String]) -> Vec<Note> {
    std::fs::read(notes_path(dir, loop_name, args))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Drop those notes once the run they were for has finished.
pub fn clear_notes(dir: &Path, loop_name: &str, args: &[String]) {
    if let Ok(_lock) = lock(dir) {
        let _ = std::fs::remove_file(notes_path(dir, loop_name, args));
    }
}

/// Queues `loopx run <loop> <args…>`.
pub type Enqueue<'a> = dyn Fn(&str, &[String]) -> Result<(), String> + 'a;

/// Fire every timer due at `now`: `remind` sends a reminder (and says whether it
/// was taken), `queue` queues a run. A timer whose action failed stays for the
/// next tick, unless it is a day overdue. Returns a line per fired timer.
pub fn tick(
    dir: &Path,
    now: DateTime<Utc>,
    remind: &dyn Fn(&Timer) -> bool,
    queue: &Enqueue<'_>,
    log: &dyn Fn(&str),
) -> Vec<String> {
    let Ok(_lock) = lock(dir) else {
        return Vec::new();
    };
    let mut fired = Vec::new();
    for t in list(dir).into_iter().filter(|t| t.due_at <= now) {
        let done = match &t.action {
            Action::Remind { .. } => remind(&t),
            Action::Run { loop_name, args } => {
                let noted = leave_note(dir, &t, loop_name, args);
                match noted
                    .map_err(|e| e.to_string())
                    .and_then(|()| queue(loop_name, args))
                {
                    Ok(()) => true,
                    Err(e) => {
                        log(&format!("timer {}: could not queue {loop_name}: {e}", t.id));
                        false
                    }
                }
            }
        };
        if done || now - t.due_at > Duration::days(1) {
            let _ = std::fs::remove_file(path(dir, &t.id));
            fired.push(match &t.action {
                Action::Remind { .. } => format!("reminder {}", t.id),
                Action::Run { loop_name, args } if args.is_empty() => loop_name.clone(),
                Action::Run { loop_name, args } => format!("{loop_name} {}", args.join(" ")),
            });
        }
    }
    fired
}

/// What a reminder says.
pub fn reminder_text(t: &Timer) -> String {
    format!("*Reminder:* {}", t.note)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-27T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn remind() -> Action {
        Action::Remind { urgent: false }
    }

    #[test]
    fn times_read_as_durations_clock_times_dates_or_rfc3339() {
        let n = now();
        assert_eq!(parse_when("30m", n).unwrap(), n + Duration::minutes(30));
        assert_eq!(parse_when("1h30m", n).unwrap(), n + Duration::minutes(90));
        assert_eq!(parse_when("2d", n).unwrap(), n + Duration::days(2));
        assert_eq!(
            parse_when("2026-11-28T09:00:00Z", n).unwrap(),
            n + Duration::hours(23)
        );
        let at9 = parse_when("09:00", n).unwrap();
        assert!(at9 > n && at9 <= n + Duration::days(1));
        assert_eq!(
            at9.with_timezone(&Local).format("%H:%M").to_string(),
            "09:00"
        );
        let day = parse_when("2026-12-01", n).unwrap().with_timezone(&Local);
        assert_eq!(day.format("%Y-%m-%d %H:%M").to_string(), "2026-12-01 09:00");
        for bad in ["", "soon", "5x", "h", "25:00"] {
            assert!(parse_when(bad, n).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_timer_needs_a_note_and_a_time_in_the_next_ninety_days() {
        let d = tempfile::tempdir().unwrap();
        let n = now();
        assert!(add(d.path(), n + Duration::hours(1), "  ", remind(), "you", n).is_err());
        assert!(add(d.path(), n - Duration::hours(1), "x", remind(), "you", n).is_err());
        assert!(add(d.path(), n + Duration::days(91), "x", remind(), "you", n).is_err());
        let long = "x".repeat(MAX_NOTE + 50);
        let t = add(d.path(), n + Duration::hours(1), &long, remind(), "you", n).unwrap();
        assert_eq!(t.note.chars().count(), MAX_NOTE);
        assert_eq!(list(d.path()), vec![t.clone()]);
        assert!(cancel(d.path(), &t.id));
        assert!(!cancel(d.path(), &t.id));
        assert!(!cancel(d.path(), "../x"));
        assert!(list(d.path()).is_empty());
    }

    #[test]
    fn a_due_reminder_is_sent_once_and_a_later_one_waits() {
        let d = tempfile::tempdir().unwrap();
        let n = now();
        add(
            d.path(),
            n + Duration::minutes(5),
            "check the deploy",
            remind(),
            "you",
            n,
        )
        .unwrap();
        add(
            d.path(),
            n + Duration::hours(5),
            "later",
            remind(),
            "you",
            n,
        )
        .unwrap();
        let sent = RefCell::new(Vec::new());
        let remind = |t: &Timer| {
            sent.borrow_mut().push(reminder_text(t));
            true
        };
        let no_queue = |_: &str, _: &[String]| -> Result<(), String> { panic!("no runs") };
        assert!(tick(d.path(), n, &remind, &no_queue, &|_| {}).is_empty());
        let fired = tick(
            d.path(),
            n + Duration::minutes(6),
            &remind,
            &no_queue,
            &|_| {},
        );
        assert_eq!(fired.len(), 1);
        tick(
            d.path(),
            n + Duration::minutes(7),
            &remind,
            &no_queue,
            &|_| {},
        );
        assert_eq!(*sent.borrow(), ["*Reminder:* check the deploy"]);
        assert_eq!(list(d.path()).len(), 1);
    }

    #[test]
    fn a_due_run_queues_the_loop_and_leaves_its_note_for_that_run_only() {
        let d = tempfile::tempdir().unwrap();
        let n = now();
        let args = vec!["web".to_string(), "7".to_string()];
        let run = Action::Run {
            loop_name: "issue-loop".into(),
            args: args.clone(),
        };
        add(
            d.path(),
            n + Duration::hours(1),
            "has the reporter answered?",
            run,
            "issue-loop",
            n,
        )
        .unwrap();
        let queued = RefCell::new(Vec::new());
        let queue = |l: &str, a: &[String]| {
            queued.borrow_mut().push(format!("{l} {}", a.join(" ")));
            Ok(())
        };
        let fired = tick(d.path(), n + Duration::hours(2), &|_| true, &queue, &|_| {});
        assert_eq!(fired, ["issue-loop web 7"]);
        assert_eq!(*queued.borrow(), ["issue-loop web 7"]);
        assert!(notes(d.path(), "issue-loop", &["web".into(), "8".into()]).is_empty());
        let left = notes(d.path(), "issue-loop", &args);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].note, "has the reporter answered?");
        assert_eq!(left[0].set_by, "issue-loop");
        clear_notes(d.path(), "issue-loop", &args);
        assert!(notes(d.path(), "issue-loop", &args).is_empty());
    }

    #[test]
    fn a_timer_whose_action_fails_is_tried_again_until_a_day_overdue() {
        let d = tempfile::tempdir().unwrap();
        let n = now();
        let run = Action::Run {
            loop_name: "rss-watch".into(),
            args: vec![],
        };
        add(d.path(), n + Duration::minutes(1), "x", run, "you", n).unwrap();
        let down = |_: &str, _: &[String]| Err("queue locked".to_string());
        let logged = RefCell::new(Vec::new());
        let log = |m: &str| logged.borrow_mut().push(m.to_string());
        assert!(tick(d.path(), n + Duration::minutes(2), &|_| true, &down, &log).is_empty());
        assert_eq!(list(d.path()).len(), 1);
        assert!(logged.borrow()[0].contains("queue locked"));
        assert_eq!(
            tick(d.path(), n + Duration::days(2), &|_| true, &down, &log),
            ["rss-watch"]
        );
        assert!(list(d.path()).is_empty());
    }
}
