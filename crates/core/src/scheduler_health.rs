//! `loopx scheduler status`: is the scheduler actually ticking, and what will it
//! do next?
//!
//! A tick that queues nothing otherwise leaves no trace (the app throws its output
//! away), so every completed `loopx scheduler` poll records a heartbeat
//! (`scheduler_heartbeat.json` in the loopx home). The report reads that, the run
//! queue, the scheduler state and the loops registry; it never changes any of them.

use crate::loop_scheduler::{self, isoformat};
use crate::loops_config;
use crate::run_queue::{Entry, RunQueue};
use chrono::{Duration, NaiveDateTime, Timelike};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The app ticks every minute; a few missed ticks still count as running.
pub const STALE_AFTER_SECS: i64 = 180;
/// How far ahead `next_due` looks for a loop's next slot.
const NEXT_DUE_HORIZON_DAYS: i64 = 35;

pub fn heartbeat_path(loop_home: &Path) -> PathBuf {
    loop_home.join("scheduler_heartbeat.json")
}

/// Written at the end of every completed poll.
pub fn record_tick(path: &Path, now: NaiveDateTime, queued: &[String]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = json!({
        "last_tick_at": isoformat(now),
        "pid": std::process::id(),
        "version": crate::VERSION,
        "queued": queued,
    });
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&body).unwrap_or_default(),
    )?;
    std::fs::rename(&tmp, path)
}

fn parse_iso(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok()
}

/// The heartbeat alone (`state`: `ok`, `stale` or `never`), for `loopx doctor`.
pub fn tick_status(loop_home: &Path, now: NaiveDateTime) -> Value {
    tick_report(&heartbeat_path(loop_home), now)
}

fn tick_report(path: &Path, now: NaiveDateTime) -> Value {
    let beat: Option<Value> = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    let at = beat
        .as_ref()
        .and_then(|b| b.get("last_tick_at")?.as_str())
        .and_then(parse_iso);
    let Some(at) = at else {
        return json!({ "state": "never", "last_tick_at": null, "age_seconds": null });
    };
    let age = (now - at).num_seconds().max(0);
    let beat = beat.unwrap_or_default();
    json!({
        "state": if age <= STALE_AFTER_SECS { "ok" } else { "stale" },
        "last_tick_at": isoformat(at),
        "age_seconds": age,
        "version": beat.get("version").cloned().unwrap_or(Value::Null),
        "queued": beat.get("queued").cloned().unwrap_or(json!([])),
    })
}

fn hhmm(schedule: &Value) -> String {
    let n = |k: &str| schedule.get(k).and_then(Value::as_i64).unwrap_or(0);
    format!("{:02}:{:02}", n("hour"), n("minute"))
}

/// The schedule in a few words, e.g. `weekly Mon,Fri at 09:30`.
pub fn describe_schedule(schedule: &Value) -> String {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let weekdays = schedule.get("weekdays").unwrap_or(&Value::Null);
    let frequency = match schedule.get("frequency").and_then(Value::as_str) {
        Some(f) => f,
        None if weekdays.as_str() == Some("all") => "daily",
        None => "weekly",
    };
    match frequency {
        "hourly" => match schedule.get("interval_minutes").and_then(Value::as_i64) {
            Some(m) if m > 0 => format!("every {m} min"),
            _ => {
                let h = schedule
                    .get("interval_hours")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                format!("every {h} h")
            }
        },
        "monthly" => {
            let day = schedule.get("day").and_then(Value::as_i64).unwrap_or(0);
            format!("monthly on day {day} at {}", hhmm(schedule))
        }
        "weekly" if weekdays.as_str() != Some("all") => {
            let days: Vec<&str> = weekdays
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_i64)
                        .filter_map(|d| DAYS.get(usize::try_from(d - 1).ok()?).copied())
                        .collect()
                })
                .unwrap_or_default();
            if days == DAYS[..5] {
                format!("weekdays at {}", hhmm(schedule))
            } else {
                format!("weekly {} at {}", days.join(","), hhmm(schedule))
            }
        }
        _ => format!("daily at {}", hhmm(schedule)),
    }
}

/// The first whole minute after `now` at which the loop is due, with the state as
/// it is now. `None` when disabled or nothing falls inside the horizon.
pub fn next_due(
    loop_: &Value,
    state: &loop_scheduler::SchedulerState,
    now: NaiveDateTime,
) -> Option<NaiveDateTime> {
    let start = now.with_second(0)?.with_nanosecond(0)? + Duration::minutes(1);
    (0..NEXT_DUE_HORIZON_DAYS * 24 * 60)
        .map(|m| start + Duration::minutes(m))
        .find(|t| loop_scheduler::is_due(loop_, state, *t))
}

fn last_run(entries: &[Entry], name: &str) -> Value {
    entries
        .iter()
        .find(|e| e.loop_name == name)
        .map_or(Value::Null, |e| {
            json!({
                "state": e.state,
                "reason": e.reason,
                "finished_at": e.finished_at,
                "exit_code": e.exit_code,
            })
        })
}

/// The whole picture. `alive` checks a queue entry's pid (injected for tests).
pub fn report(loop_home: &Path, now: NaiveDateTime, alive: &dyn Fn(i64) -> bool) -> Value {
    report_with_template(loop_home, None, now, alive)
}

fn report_with_template(
    loop_home: &Path,
    template: Option<&Path>,
    now: NaiveDateTime,
    alive: &dyn Fn(i64) -> bool,
) -> Value {
    let tick = tick_report(&heartbeat_path(loop_home), now);
    let queue = RunQueue::new(loop_home.join("run_queue.json")).snapshot(usize::MAX);
    let state = loop_scheduler::read_state(&loop_home.join("loop_scheduler_state.json"));

    let running: Vec<Value> = queue
        .running
        .iter()
        .map(|e| {
            json!({
                "loop": e.loop_name,
                "pid": e.pid,
                "started_at": e.started_at,
                "alive": e.pid.is_some_and(alive),
            })
        })
        .collect();
    let dead: Vec<String> = running
        .iter()
        .filter(|r| r["alive"] == false)
        .filter_map(|r| r["loop"].as_str().map(str::to_string))
        .collect();
    let queued: Vec<Value> = queue
        .queued
        .iter()
        .map(|e| json!({ "loop": e.loop_name, "reason": e.reason, "enqueued_at": e.enqueued_at }))
        .collect();

    let mut problems: Vec<String> = Vec::new();
    let loops = match loops_config::list_loops(&loop_home.join("loops.json"), template) {
        Ok(l) => l,
        Err(e) => {
            problems.push(format!("Could not read the loops registry: {e}"));
            Vec::new()
        }
    };
    let loops: Vec<Value> = loops
        .iter()
        .map(|l| {
            let name = l.get("name").and_then(Value::as_str).unwrap_or_default();
            let enabled = l.get("enabled").is_none_or(loops_config::py_truthy);
            let schedule = l.get("schedule").cloned().unwrap_or(Value::Null);
            let due_now = loop_scheduler::is_due(l, &state, now);
            let next = if enabled && !due_now {
                next_due(l, &state, now).map(isoformat)
            } else {
                None
            };
            json!({
                "name": name,
                "enabled": enabled,
                "schedule": describe_schedule(&schedule),
                "last_attempted_at": state.get(name).and_then(|s| s.get("last_attempted_at")).cloned().unwrap_or(Value::Null),
                "due_now": enabled && due_now,
                "next_due": next,
                "last_run": last_run(&queue.recent, name),
            })
        })
        .collect();

    match tick["state"].as_str() {
        Some("never") => problems.push(
            "The scheduler has never ticked: open the LoopX app and turn on \"Run scheduled loops while LoopX is open\".".into(),
        ),
        Some("stale") => problems.push(format!(
            "The scheduler last ticked {} min ago ({}): check that the LoopX app is open with \"Run scheduled loops\" on (it also skips ticks on low battery when that option is set).",
            tick["age_seconds"].as_i64().unwrap_or(0) / 60,
            tick["last_tick_at"].as_str().unwrap_or_default()
        )),
        _ => {}
    }
    for name in &dead {
        problems.push(format!(
            "{name} is marked running but its process is gone; the next queue worker will recover it."
        ));
    }
    if tick["state"] == "ok"
        && loops.iter().any(|l| l["due_now"] == true)
        && queued.is_empty()
        && running.is_empty()
    {
        problems.push("A loop is due but nothing is queued; the next tick should queue it.".into());
    }

    json!({
        "healthy": tick["state"] == "ok" && dead.is_empty(),
        "now": isoformat(now),
        "tick": tick,
        "queue": { "running": running, "queued": queued },
        "loops": loops,
        "problems": problems,
    })
}

fn short(ts: &Value) -> String {
    ts.as_str()
        .map(|s| s.get(..16).unwrap_or(s).replace('T', " "))
        .unwrap_or_else(|| "-".into())
}

/// The report as text for a terminal.
pub fn render(r: &Value) -> String {
    let mut s = String::from("Scheduler\n");
    let tick = &r["tick"];
    let line = match tick["state"].as_str() {
        Some("ok") => format!(
            "  [ok     ] last tick {}s ago ({})\n",
            tick["age_seconds"],
            short(&tick["last_tick_at"])
        ),
        Some("stale") => format!(
            "  [STALE  ] last tick {} min ago ({})\n",
            tick["age_seconds"].as_i64().unwrap_or(0) / 60,
            short(&tick["last_tick_at"])
        ),
        _ => "  [MISSING] no tick recorded yet\n".into(),
    };
    s.push_str(&line);

    s.push_str("\nQueue\n");
    let running = r["queue"]["running"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let queued = r["queue"]["queued"].as_array().cloned().unwrap_or_default();
    if running.is_empty() && queued.is_empty() {
        s.push_str("  idle\n");
    }
    for e in &running {
        let mark = if e["alive"] == true {
            "running"
        } else {
            "DEAD   "
        };
        s.push_str(&format!(
            "  [{mark}] {} (pid {}, since {})\n",
            e["loop"].as_str().unwrap_or_default(),
            e["pid"],
            short(&e["started_at"])
        ));
    }
    for e in &queued {
        s.push_str(&format!(
            "  [queued ] {} ({}, {})\n",
            e["loop"].as_str().unwrap_or_default(),
            e["reason"].as_str().unwrap_or_default(),
            short(&e["enqueued_at"])
        ));
    }

    s.push_str("\nLoops\n");
    let loops = r["loops"].as_array().cloned().unwrap_or_default();
    let width = loops
        .iter()
        .filter_map(|l| l["name"].as_str().map(str::len))
        .max()
        .unwrap_or(0);
    let sched_width = loops
        .iter()
        .filter_map(|l| l["schedule"].as_str().map(str::len))
        .max()
        .unwrap_or(0);
    for l in &loops {
        let name = l["name"].as_str().unwrap_or_default();
        let next = if l["enabled"] != true {
            "disabled".to_string()
        } else if l["due_now"] == true {
            "due now".to_string()
        } else {
            format!("next {}", short(&l["next_due"]))
        };
        let last = match l["last_run"].as_object() {
            Some(run) => format!(
                "last run {} {}",
                run.get("state").and_then(Value::as_str).unwrap_or("?"),
                short(run.get("finished_at").unwrap_or(&Value::Null))
            ),
            None => "no run in the queue history".into(),
        };
        s.push_str(&format!(
            "  {name:<width$}  {:<sched_width$}  {next:<22}  {last}\n",
            l["schedule"].as_str().unwrap_or_default()
        ));
    }

    s.push('\n');
    let problems = r["problems"].as_array().cloned().unwrap_or_default();
    if r["healthy"] == true {
        s.push_str("Scheduler is running.\n");
    } else {
        s.push_str("Scheduler is NOT running correctly.\n");
    }
    for p in &problems {
        s.push_str(&format!("  - {}\n", p.as_str().unwrap_or_default()));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_queue::Reason;
    use chrono::NaiveDate;

    fn at(h: u32, m: u32) -> NaiveDateTime {
        // A Thursday.
        NaiveDate::from_ymd_opt(2026, 10, 29)
            .unwrap()
            .and_hms_opt(h, m, 0)
            .unwrap()
    }

    fn report(home: &Path, now: NaiveDateTime, alive: &dyn Fn(i64) -> bool) -> Value {
        report_with_template(home, Some(&home.join("no-template.json")), now, alive)
    }

    fn home_with_loops(loops: Value) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("loops.json"), loops.to_string()).unwrap();
        t
    }

    fn daily(name: &str, hour: i64) -> Value {
        json!({"name": name, "enabled": true, "schedule": {"frequency": "daily", "hour": hour, "minute": 0}})
    }

    #[test]
    fn no_heartbeat_means_never_ticked_and_unhealthy() {
        let t = home_with_loops(json!([daily("a-loop", 9)]));
        let r = report(t.path(), at(12, 0), &|_| true);
        assert_eq!(r["tick"]["state"], "never");
        assert_eq!(r["healthy"], false);
        assert!(r["problems"].to_string().contains("never ticked"));
        assert!(render(&r).contains("NOT running"));
    }

    #[test]
    fn a_recent_tick_is_healthy_and_an_old_one_is_stale() {
        let t = home_with_loops(json!([]));
        record_tick(&heartbeat_path(t.path()), at(12, 0), &["a-loop".into()]).unwrap();
        let r = report(t.path(), at(12, 2), &|_| true);
        assert_eq!(r["tick"]["state"], "ok");
        assert_eq!(r["tick"]["age_seconds"], 120);
        assert_eq!(r["tick"]["queued"], json!(["a-loop"]));
        assert_eq!(r["healthy"], true);
        assert!(render(&r).contains("Scheduler is running."));

        let r = report(t.path(), at(12, 10), &|_| true);
        assert_eq!(r["tick"]["state"], "stale");
        assert_eq!(r["healthy"], false);
        assert!(r["problems"].to_string().contains("10 min ago"));
    }

    #[test]
    fn a_running_entry_whose_process_is_gone_is_flagged() {
        let t = home_with_loops(json!([daily("a-loop", 9)]));
        record_tick(&heartbeat_path(t.path()), at(12, 0), &[]).unwrap();
        let q = RunQueue::new(t.path().join("run_queue.json"));
        q.enqueue("a-loop", Reason::Manual, "2026-10-29T11:00:00")
            .unwrap();
        let e = q.claim_next("2026-10-29T11:00:01", 1).unwrap().unwrap();
        q.set_pid(&e.id, 4242).unwrap();

        let r = report(t.path(), at(12, 0), &|pid| pid != 4242);
        assert_eq!(r["queue"]["running"][0]["alive"], false);
        assert_eq!(r["healthy"], false);
        assert!(r["problems"]
            .to_string()
            .contains("a-loop is marked running"));
        assert!(render(&r).contains("DEAD"));
    }

    #[test]
    fn each_loop_shows_its_schedule_next_slot_and_last_run() {
        let t = home_with_loops(json!([
            daily("daily-loop", 9),
            {"name": "hourly-loop", "enabled": true, "schedule": {"frequency": "hourly", "interval_minutes": 30}},
            {"name": "off-loop", "enabled": false, "schedule": {"frequency": "daily", "hour": 9, "minute": 0}},
        ]));
        let mut state = loop_scheduler::SchedulerState::new();
        state.insert(
            "daily-loop".into(),
            json!({"last_attempted_date": "2026-10-29", "last_attempted_at": "2026-10-29T09:00:00"}),
        );
        state.insert(
            "hourly-loop".into(),
            json!({"last_attempted_date": "2026-10-29", "last_attempted_at": "2026-10-29T11:50:00"}),
        );
        loop_scheduler::write_state(&state, &t.path().join("loop_scheduler_state.json")).unwrap();
        let q = RunQueue::new(t.path().join("run_queue.json"));
        q.enqueue("daily-loop", Reason::Scheduled, "2026-10-29T09:00:00")
            .unwrap();
        let e = q.claim_next("2026-10-29T09:00:01", 1).unwrap().unwrap();
        q.finish(&e.id, "done", Some(0), None, "2026-10-29T09:05:00")
            .unwrap();
        record_tick(&heartbeat_path(t.path()), at(12, 0), &[]).unwrap();

        let r = report(t.path(), at(12, 0), &|_| true);
        let l = |n: &str| {
            r["loops"]
                .as_array()
                .unwrap()
                .iter()
                .find(|l| l["name"] == n)
                .unwrap()
                .clone()
        };
        assert_eq!(l("daily-loop")["schedule"], "daily at 09:00");
        assert_eq!(l("daily-loop")["next_due"], "2026-10-30T09:00:00");
        assert_eq!(l("daily-loop")["last_run"]["state"], "done");
        assert_eq!(l("daily-loop")["last_run"]["exit_code"], 0);
        assert_eq!(l("hourly-loop")["schedule"], "every 30 min");
        assert_eq!(l("hourly-loop")["next_due"], "2026-10-29T12:19:00");
        assert_eq!(l("off-loop")["next_due"], Value::Null);
        assert_eq!(l("off-loop")["due_now"], false);
        let text = render(&r);
        assert!(text.contains("disabled") && text.contains("next 2026-10-30 09:00"));
    }

    #[test]
    fn describes_weekly_and_monthly_schedules() {
        assert_eq!(
            describe_schedule(
                &json!({"frequency": "weekly", "weekdays": [1, 5], "hour": 9, "minute": 30})
            ),
            "weekly Mon,Fri at 09:30"
        );
        assert_eq!(
            describe_schedule(
                &json!({"frequency": "weekly", "weekdays": [1, 2, 3, 4, 5], "hour": 10, "minute": 0})
            ),
            "weekdays at 10:00"
        );
        assert_eq!(
            describe_schedule(&json!({"frequency": "monthly", "day": 1, "hour": 8, "minute": 0})),
            "monthly on day 1 at 08:00"
        );
        assert_eq!(
            describe_schedule(&json!({"frequency": "hourly", "interval_hours": 2})),
            "every 2 h"
        );
    }

    #[test]
    fn a_due_loop_with_an_empty_queue_is_called_out() {
        let t = home_with_loops(json!([daily("a-loop", 9)]));
        record_tick(&heartbeat_path(t.path()), at(12, 0), &[]).unwrap();
        let r = report(t.path(), at(12, 0), &|_| true);
        assert_eq!(r["loops"][0]["due_now"], true);
        assert!(r["problems"].to_string().contains("A loop is due"));
        assert!(render(&r).contains("due now"));
    }
}
