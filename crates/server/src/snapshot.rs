//! The state document the Mac app and the web UI watch: every loop's run status,
//! the topic monitor's per-topic status, pending approvals and recent outcomes,
//! plus one derived `app_state` for a glanceable indicator.

use chrono::{DateTime, Duration, Utc};
use loopx_core::{approvals, events, status};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// How long an outcome (MR opened, escalation, failure) stays "fresh" in
/// `attention`, i.e. worth surfacing in the notch.
pub const ATTENTION_WINDOW_MINUTES: i64 = 15;
const RECENT_EVENTS: usize = 15;
const INTERESTING: [&str; 9] = [
    "run.started",
    "run.completed",
    "run.failed",
    "issue.queued",
    "issue.escalated",
    "issue.completed",
    "approval.requested",
    "approval.decided",
    "loop.failed",
];

/// Where a root keeps its per-checkout run state.
#[derive(Debug, Clone)]
pub struct Paths {
    pub outputs: PathBuf,
    pub events_dir: PathBuf,
    pub approvals_dir: PathBuf,
    /// The run queue (`~/.loopx/run_queue.json`) behind Agents.
    pub queue_path: PathBuf,
    /// The sign-in checks' state (`~/.loopx/auth_health.json`).
    pub auth_state: PathBuf,
    /// Dismissed "Needs you" items.
    pub dismissed: PathBuf,
    /// Task memory, which holds each issue's review-thread state.
    pub memory_root: PathBuf,
    /// The Creator page's job records (`~/.loopx/creator/jobs`), shown as loops.
    pub creator_jobs_dir: PathBuf,
    /// The MR Review page's release MR jobs (`~/.loopx/release-mr/jobs`), shown as one agent.
    pub release_jobs_dir: PathBuf,
    /// The MR Review page's "Fix now" jobs (`~/.loopx/mr-fix/jobs`), shown as one agent.
    pub mr_fix_jobs_dir: PathBuf,
    /// Where the Creator's skills keep their output (`~`): a clean run that wrote
    /// nothing there is not a success.
    pub user_home: PathBuf,
    /// The loops registry (`~/.loopx/loops.json`): a disabled loop is left out of the snapshot.
    pub loops_config: PathBuf,
}

impl Paths {
    pub fn for_root(root: &Path) -> Self {
        let outputs = root.join("outputs");
        Self {
            events_dir: events::default_events_dir(),
            approvals_dir: outputs.join("approvals"),
            queue_path: loopx_core::run_queue::default_queue_path(),
            auth_state: loopx_core::auth_health::default_state_path(),
            dismissed: loopx_core::needs_you::default_dismissed_path(),
            memory_root: loopx_core::memory_store::default_memory_root(),
            creator_jobs_dir: loopx_core::creator::jobs::jobs_dir(&loopx_core::config::home_dir()),
            release_jobs_dir: loopx_core::release_mr::job::jobs_dir(&loopx_core::config::home_dir()),
            mr_fix_jobs_dir: loopx_core::mr_fix::jobs::jobs_dir(&loopx_core::config::home_dir()),
            user_home: PathBuf::from(std::env::var_os("HOME").unwrap_or_default()),
            loops_config: loopx_core::config::home_dir().join("loops.json"),
            outputs,
        }
    }
}

/// Everything waiting on a person (see `loopx_core::needs_you`).
pub fn needs_you(paths: &Paths, loops: &Map<String, Value>, now: DateTime<Utc>) -> Vec<Value> {
    loopx_core::needs_you::collect(
        &loopx_core::needs_you::Sources {
            approvals_dir: &paths.approvals_dir,
            events_dir: &paths.events_dir,
            auth_state: &paths.auth_state,
            loops,
            dismissed: &paths.dismissed,
            memory_root: &paths.memory_root,
        },
        now,
    )
}

#[cfg(unix)]
fn pid_alive(pid: u64) -> bool {
    u32::try_from(pid).is_ok_and(crate::lock::pid_alive)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u64) -> bool {
    true
}

/// A `running` status whose pid is gone is a run that died without cleaning up.
pub(crate) fn effective_state(doc: &Value) -> String {
    let state = doc["state"].as_str().unwrap_or("unknown").to_string();
    if state == "running" {
        if let Some(pid) = doc.get("pid").and_then(Value::as_u64) {
            if !pid_alive(pid) {
                return "stale".to_string();
            }
        }
    }
    state
}

pub fn loop_statuses(outputs: &Path) -> Map<String, Value> {
    let mut loops = Map::new();
    let legacy = outputs.join("status.json");
    if legacy.exists() {
        loops.insert("issue-loop".into(), status::read_status(&legacy));
    }
    if let Ok(entries) = std::fs::read_dir(outputs.join("status")) {
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        files.sort();
        for p in files {
            if let Some(name) = p.file_stem().map(|n| n.to_string_lossy().into_owned()) {
                loops.insert(name, status::read_status(&p));
            }
        }
    }
    for doc in loops.values_mut() {
        let eff = effective_state(doc);
        if eff != doc["state"].as_str().unwrap_or("") {
            doc["state"] = json!(eff);
        }
    }
    // What the running GitLab agent last did, from its claude hooks.
    if let Some(doc) = loops
        .get_mut("issue-loop")
        .filter(|d| d["state"] == "running")
    {
        // Profile jobs write beside them but are listed as their own entries.
        let all: Vec<Value> =
            loopx_core::agent_activity::read_all(&loopx_core::agent_activity::beside(&legacy))
                .into_iter()
                .filter(|a| !loopx_core::profile_progress::is_profile_record(a))
                .collect();
        if let Some(newest) = all.first() {
            doc["agent_activity"] = newest.clone();
            doc["agent_activities"] = Value::Array(all);
        }
    }
    loops
}

fn compact_event(e: &Value) -> Value {
    json!({
        "type": e["event_type"],
        "at": e["timestamp"],
        "run_id": e["run_id"],
        "project": e["project"],
        "issue_iid": e["issue_iid"],
        "data": e["data"],
    })
}

fn parse_time(e: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(e["timestamp"].as_str()?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// The newest fresh outcome worth showing: an MR opened, an escalation, a failure.
fn attention(recent: &[Value], now: DateTime<Utc>) -> Value {
    let cutoff = now - Duration::minutes(ATTENTION_WINDOW_MINUTES);
    for e in recent.iter().rev() {
        if parse_time(e).is_none_or(|t| t < cutoff) {
            continue;
        }
        let data = &e["data"];
        let kind = match e["event_type"].as_str() {
            Some("issue.escalated") => "escalated",
            Some("issue.completed")
                if data["action"] == "fix" || data["outcome"] == "mr_opened" =>
            {
                "mr_opened"
            }
            Some("run.failed") | Some("loop.failed") => "failed",
            _ => continue,
        };
        return json!({
            "kind": kind,
            "at": e["timestamp"],
            "project": e["project"],
            "issue_iid": e["issue_iid"],
            "reason": data["reason"],
            "mr_url": data["mr_url"],
        });
    }
    Value::Null
}

/// One glanceable state: `awaiting_approval` > `running` > `failed` > `idle`.
fn app_state(loops: &Map<String, Value>, pending: usize) -> &'static str {
    if pending > 0 {
        return "awaiting_approval";
    }
    let any = |s: &str| loops.values().any(|d| d["state"] == s);
    if any("running") {
        "running"
    } else if any("failed") {
        "failed"
    } else {
        "idle"
    }
}

/// Agents's view of the run queue: what runs, what waits (in start
/// order) and the last few finished. A running entry carries the loop's current step.
fn queue_view(queue_path: &Path, loops: &Map<String, Value>) -> Value {
    let snap = loopx_core::run_queue::RunQueue::new(queue_path).snapshot(10);
    let entry = |e: &loopx_core::run_queue::Entry| serde_json::to_value(e).unwrap_or(Value::Null);
    let running: Vec<Value> = snap
        .running
        .iter()
        .map(|e| {
            let mut v = entry(e);
            v["step"] = loops
                .get(&e.loop_name)
                .map_or(Value::Null, |l| l["current_step"].clone());
            v
        })
        .collect();
    json!({
        "running": running,
        "queued": snap.queued.iter().map(entry).collect::<Vec<_>>(),
        "recent": snap.recent.iter().map(entry).collect::<Vec<_>>(),
    })
}

/// Loops the user disabled in the registry. A loop the registry does not list,
/// or no registry at all, hides nothing.
fn disabled_loops(loops_config: &Path) -> std::collections::BTreeSet<String> {
    loopx_core::loops_config::list_loops(loops_config, None)
        .unwrap_or_default()
        .iter()
        .filter(|e| {
            e.get("enabled")
                .is_some_and(|v| !loopx_core::loops_config::py_truthy(v))
        })
        .filter_map(|e| e.get("name").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// The enabled loops as the sidebar lists them (registry order, the sidebar's
/// labels), for the Mac app's Loops menu. Empty without a registry.
fn enabled_loops(loops_config: &Path) -> Vec<Value> {
    loopx_core::loops_config::list_loops(loops_config, None)
        .unwrap_or_default()
        .iter()
        .filter(|e| {
            e.get("enabled")
                .is_none_or(loopx_core::loops_config::py_truthy)
        })
        .filter_map(|e| {
            let name = e
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())?;
            let page = crate::ui::hub::builtin_loop_page(name)
                .unwrap_or_else(|| crate::ui::hub::generic_loop_page(name, Some(e), false));
            Some(json!({"name": name, "label": page.label}))
        })
        .collect()
}

/// A listing of approvals without the page texts a proposal carries: a
/// `profile-section` or `guide-page` approval's `target.old` and `target.new` (a whole
/// section or guide page each) become `old_len`/`new_len` byte counts. Every other
/// kind, and every other field, is unchanged. The views that show the text (the
/// Memory page's Proposals) read the approval from the store.
pub fn without_page_texts(list: Vec<Value>) -> Vec<Value> {
    list.into_iter()
        .map(|mut a| {
            if !loopx_core::proposals::is_kind(&a["kind"]) {
                return a;
            }
            if let Some(t) = a.get_mut("target").and_then(Value::as_object_mut) {
                for (key, len) in [("old", "old_len"), ("new", "new_len")] {
                    if let Some(v) = t.remove(key) {
                        let n = v.as_str().map_or(0, str::len);
                        t.insert(len.to_string(), Value::from(n));
                    }
                }
            }
            a
        })
        .collect()
}

pub fn build_snapshot(paths: &Paths, now: DateTime<Utc>) -> Value {
    let mut loops = loop_statuses(&paths.outputs);
    // Only enabled loops reach the Mac island; a disabled one that is running
    // right now stays so its progress is not hidden.
    let disabled = disabled_loops(&paths.loops_config);
    loops.retain(|name, doc| !disabled.contains(name) || doc["state"] == "running");
    let topics =
        status::read_topic_status(&paths.outputs.join("topic-monitor").join("status.json"));
    approvals::expire_overdue(&paths.approvals_dir, now);
    let pending = without_page_texts(approvals::list(&paths.approvals_dir, Some("pending"), now));

    let since = (now - Duration::days(1)).format("%Y-%m-%d").to_string();
    let all = events::iter_events(Some(&paths.events_dir), Some(&since), None);
    let recent: Vec<Value> = all
        .iter()
        .filter(|e| {
            e["event_type"]
                .as_str()
                .is_some_and(|t| INTERESTING.contains(&t))
        })
        .cloned()
        .collect();
    let tail: Vec<Value> = recent
        .iter()
        .rev()
        .take(RECENT_EVENTS)
        .rev()
        .map(compact_event)
        .collect();
    let queue = queue_view(&paths.queue_path, &loops);
    let waiting = needs_you(paths, &loops, now);
    // The Creator's Find videos and Generate video runs show up beside the loops (the
    // island, the Mac notifications, the menu-bar state); a failure there is reported
    // by its own notification and on the Jobs tab, not in Needs you.
    let mut loops = loops;
    let settled = loopx_core::creator::jobs::settle_outputs(
        loopx_core::creator::jobs::list_jobs(&paths.creator_jobs_dir),
        &paths.user_home,
    );
    for (name, doc) in
        loopx_core::creator::notify::loop_statuses(&paths.creator_jobs_dir, &settled, now)
    {
        loops.insert(name, doc);
    }
    if let Some(entry) = loopx_core::release_mr::job::island_entry(
        &loopx_core::release_mr::job::list(&paths.release_jobs_dir),
        now,
    ) {
        loops.insert("release-mr".to_string(), entry);
    }
    if let Some(entry) = loopx_core::mr_fix::jobs::island_entry(
        &loopx_core::mr_fix::jobs::list(&paths.mr_fix_jobs_dir),
        now,
    ) {
        loops.insert("mr-fix".to_string(), entry);
    }
    // Running project profile jobs (a build, a section refresh, the sync): one
    // entry each, `profile:<alias>` or `profile-sync`, with the step fields. A guide
    // build is left out: its key would collide with the alias's profile build.
    for job in loopx_core::profile_progress::running_at(
        &loopx_core::agent_activity::beside(&paths.outputs.join("status.json")),
        now,
    )
    .into_iter()
    .filter(|j| !j.is_guide())
    {
        loops.insert(job.snapshot_key(), job.island_status());
    }
    let state = app_state(&loops, pending.len());

    json!({
        "version": 1,
        "app_state": state,
        "loops": loops,
        "disabled_loops": disabled,
        "enabled_loops": enabled_loops(&paths.loops_config),
        "topics": topics["topics"],
        "approvals": {"pending": pending},
        "queue": queue,
        "attention": attention(&recent, now),
        "needs_you": {"count": waiting.len(), "items": waiting},
        "recent_events": tail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 22, 12, 0, 0).unwrap()
    }

    fn paths(root: &Path) -> Paths {
        Paths {
            outputs: root.join("outputs"),
            events_dir: root.join("outputs/events"),
            approvals_dir: root.join("outputs/approvals"),
            queue_path: root.join("run_queue.json"),
            auth_state: root.join("auth_health.json"),
            dismissed: root.join("needs_you_dismissed.json"),
            memory_root: root.join("memory"),
            creator_jobs_dir: root.join("creator/jobs"),
            release_jobs_dir: root.join("release-mr/jobs"),
            mr_fix_jobs_dir: root.join("mr-fix/jobs"),
            user_home: root.join("home"),
            loops_config: root.join("loops.json"),
        }
    }

    fn creator_job(p: &Paths, id: &str, kind: &str, alive: bool, log: &str) {
        let pid = if alive {
            i64::from(std::process::id())
        } else {
            let mut c = std::process::Command::new("true").spawn().unwrap();
            c.wait().unwrap();
            i64::from(c.id())
        };
        std::fs::create_dir_all(&p.creator_jobs_dir).unwrap();
        std::fs::write(
            p.creator_jobs_dir.join(format!("{id}.json")),
            json!({"id": id, "kind": kind, "key": "https://www.ted.com/talks/x", "state": "running",
                   "pid": pid, "started_at": "2026-10-22T11:58:00+00:00"})
            .to_string(),
        )
        .unwrap();
        std::fs::write(p.creator_jobs_dir.join(format!("{id}.log")), log).unwrap();
    }

    fn write_status(p: &Paths, name: &str, state: &str) {
        let dir = p.outputs.join("status");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{name}.json")),
            json!({"state": state, "updated_at": "2026-10-22T11:00:00+00:00"}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn a_disabled_loop_is_left_out_of_the_snapshot_even_with_run_history() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        std::fs::write(
            &p.loops_config,
            r#"[{"name": "linkedin-autopilot-loop", "enabled": false},
                {"name": "rss-watch-loop", "enabled": false},
                {"name": "stale-sweeper-loop", "enabled": true}]"#,
        )
        .unwrap();
        for n in [
            "linkedin-autopilot-loop",
            "rss-watch-loop",
            "stale-sweeper-loop",
            "unlisted-loop",
        ] {
            write_status(&p, n, "idle");
        }
        let s = build_snapshot(&p, now());
        let loops = s["loops"].as_object().unwrap();
        assert!(!loops.contains_key("linkedin-autopilot-loop"), "{s}");
        assert!(!loops.contains_key("rss-watch-loop"), "{s}");
        assert!(loops.contains_key("stale-sweeper-loop"), "{s}");
        assert!(
            loops.contains_key("unlisted-loop"),
            "a loop the registry does not list is kept: {s}"
        );
    }

    #[test]
    fn the_snapshot_names_the_disabled_loops_so_the_app_can_hide_their_menus_and_buttons() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        std::fs::write(
            &p.loops_config,
            r#"[{"name": "topic-loop", "enabled": false}, {"name": "issue-loop", "enabled": true},
                {"name": "rss-watch-loop", "enabled": false}]"#,
        )
        .unwrap();
        let s = build_snapshot(&p, now());
        let names: Vec<&str> = s["disabled_loops"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        // Also the template loops the file does not list, as the sidebar treats them.
        assert!(
            names.contains(&"rss-watch-loop") && names.contains(&"topic-loop"),
            "{s}"
        );
        assert!(!names.contains(&"issue-loop"), "{s}");
        assert!(names.windows(2).all(|w| w[0] < w[1]), "sorted: {s}");
        std::fs::remove_file(&p.loops_config).unwrap();
        assert_eq!(build_snapshot(&p, now())["disabled_loops"], json!([]));
    }

    #[test]
    fn the_snapshot_lists_the_enabled_loops_with_the_sidebar_labels_in_registry_order() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        std::fs::write(
            &p.loops_config,
            r#"[{"name": "issue-loop", "enabled": true},
                {"name": "topic-loop", "enabled": false},
                {"name": "mr-review-loop", "label": "MR Review", "enabled": true},
                {"name": "meeting-prep-loop", "label": "Meeting Prep"},
                {"name": "rss-watch-loop", "label": "RSS Watch", "enabled": false}]"#,
        )
        .unwrap();
        let s = build_snapshot(&p, now());
        let listed: Vec<(&str, &str)> = s["enabled_loops"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["name"].as_str().unwrap(), e["label"].as_str().unwrap()))
            .collect();
        assert_eq!(
            &listed[..3],
            [
                ("issue-loop", "Issues"),
                ("mr-review-loop", "MR Review"),
                ("meeting-prep-loop", "Meeting Prep")
            ],
            "{s}"
        );
        assert!(!listed
            .iter()
            .any(|(n, _)| *n == "topic-loop" || *n == "rss-watch-loop"));
        std::fs::remove_file(&p.loops_config).unwrap();
        assert_eq!(build_snapshot(&p, now())["enabled_loops"], json!([]));
    }

    #[test]
    fn a_running_release_job_is_a_running_agent_with_its_step_for_the_island() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        assert!(build_snapshot(&p, now())["loops"]
            .get("release-mr")
            .is_none());
        let j = loopx_core::release_mr::job::start(
            &p.release_jobs_dir,
            "app",
            "g/app",
            "staging",
            "master",
            loopx_core::release_mr::job::Mode::Create,
            &|_: &Path| Ok(i64::from(std::process::id())),
        )
        .unwrap();
        loopx_core::release_mr::job::set_step(&p.release_jobs_dir, &j.id, 3, "4 people to tag")
            .unwrap();
        let s = build_snapshot(&p, now());
        let l = &s["loops"]["release-mr"];
        assert_eq!(l["state"], "running", "{s}");
        assert_eq!(l["current_issue"], "app \u{b7} staging \u{2192} master");
        assert!(
            l["current_step"]
                .as_str()
                .unwrap()
                .starts_with("Writing the summary \u{b7} step 4 of 5"),
            "{s}"
        );
        assert_eq!(s["app_state"], "running");
    }

    #[test]
    fn a_running_fix_now_job_is_a_running_agent_with_its_step_for_the_island() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        assert!(build_snapshot(&p, now())["loops"].get("mr-fix").is_none());
        let j = loopx_core::mr_fix::jobs::start(
            &p.mr_fix_jobs_dir,
            "work:9!1",
            "Fix login",
            &|_: &Path| Ok(i64::from(std::process::id())),
        )
        .unwrap();
        loopx_core::mr_fix::jobs::set_step(&p.mr_fix_jobs_dir, &j.id, 3).unwrap();
        let s = build_snapshot(&p, now());
        let l = &s["loops"]["mr-fix"];
        assert_eq!(l["state"], "running", "{s}");
        assert_eq!(l["current_issue"], "work:9!1 \u{b7} Fix login");
        assert_eq!(
            l["current_step"],
            "Agent fixing the feedback \u{b7} step 4 of 6"
        );
        assert_eq!(s["app_state"], "running");
    }

    #[test]
    fn a_disabled_loop_that_is_running_right_now_stays_visible() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        std::fs::write(
            &p.loops_config,
            r#"[{"name": "rss-watch-loop", "enabled": false}]"#,
        )
        .unwrap();
        write_status(&p, "rss-watch-loop", "running");
        assert!(build_snapshot(&p, now())["loops"]
            .get("rss-watch-loop")
            .is_some());
    }

    #[test]
    fn a_missing_loops_registry_hides_nothing() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        write_status(&p, "rss-watch-loop", "idle");
        assert!(build_snapshot(&p, now())["loops"]
            .get("rss-watch-loop")
            .is_some());
    }

    #[test]
    fn a_running_creator_job_is_a_running_loop_with_its_stage_for_the_island_and_notifications() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        assert!(build_snapshot(&p, now())["loops"]
            .get("creator-generate-video")
            .is_none());
        let step = format!(
            "{}\n",
            json!({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "name": "Bash", "input": {"command": "python scripts/render.py m.json"}}]}})
        );
        creator_job(&p, "20261022T115800-gen000000", "generate", true, &step);
        let s = build_snapshot(&p, now());
        let l = &s["loops"]["creator-generate-video"];
        assert_eq!(l["state"], "running", "{s}");
        assert_eq!(l["current_step"], "Stage 7 of 9 · Render");
        assert_eq!(l["current_issue"], "www.ted.com/talks/x");
        assert_eq!(s["app_state"], "running");
        assert!(s["loops"].get("creator-find-videos").is_none());
    }

    #[test]
    fn a_failed_creator_job_does_not_join_needs_you() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        creator_job(&p, "20261022T115800-scout000", "scout", false, "boom\n");
        let s = build_snapshot(&p, now());
        assert_eq!(s["loops"]["creator-find-videos"]["state"], "failed");
        assert_eq!(s["needs_you"]["count"], 0, "{s}");
    }

    #[test]
    fn the_snapshot_counts_what_needs_you() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        status::write_status(
            &p.outputs.join("status/rss-loop.json"),
            "failed",
            &[("last_exit_code", json!(1))],
        )
        .unwrap();
        event(
            d.path(),
            "issue.escalated",
            "2026-10-22T11:00:00+00:00",
            json!({"reason": "ambiguous"}),
        );
        let s = build_snapshot(&p, now());
        assert_eq!(s["needs_you"]["count"], 2);
        let ids: Vec<&str> = s["needs_you"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect();
        assert!(
            ids.contains(&"escalated:web#7") && ids.contains(&"loop_failed:rss-loop"),
            "{ids:?}"
        );
    }

    fn event(root: &Path, kind: &str, at: &str, data: Value) {
        let dir = root.join("outputs/events");
        std::fs::create_dir_all(&dir).unwrap();
        let day = &at[..10];
        let line = json!({
            "schema_version": 1, "event_id": "evt_x", "timestamp": at, "event_type": kind,
            "run_id": "run_1", "issue_run_id": null, "project": "web", "issue_iid": 7, "data": data
        });
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{day}.jsonl")))
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    #[test]
    fn the_run_queue_is_part_of_the_snapshot() {
        use loopx_core::run_queue::{Reason, RunQueue};
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        assert_eq!(
            build_snapshot(&p, now())["queue"],
            json!({"running": [], "queued": [], "recent": []})
        );
        status::write_status(
            &p.outputs.join("status/topic-loop.json"),
            "running",
            &[("current_step", json!("Fetching feeds"))],
        )
        .unwrap();
        let q = RunQueue::new(&p.queue_path);
        q.enqueue("topic-loop", Reason::Scheduled, "2026-10-22T11:59:00")
            .unwrap();
        q.enqueue("rss-loop", Reason::Manual, "2026-10-22T11:59:30")
            .unwrap();
        q.claim_next("2026-10-22T11:59:40", 1).unwrap();
        let s = build_snapshot(&p, now());
        // The manual run outranked the scheduled one and started first.
        assert_eq!(s["queue"]["running"][0]["loop"], "rss-loop");
        assert_eq!(s["queue"]["queued"][0]["loop"], "topic-loop");
        assert_eq!(s["queue"]["queued"][0]["reason"], "scheduled");
        let steps: Vec<_> = vec![&s["queue"]["running"][0]["step"]];
        assert_eq!(steps[0], &Value::Null, "rss-loop has no status file yet");
    }

    #[test]
    fn a_running_queue_entry_shows_its_loops_current_step() {
        use loopx_core::run_queue::{Reason, RunQueue};
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        status::write_status(
            &p.outputs.join("status/topic-loop.json"),
            "running",
            &[("current_step", json!("Fetching feeds"))],
        )
        .unwrap();
        let q = RunQueue::new(&p.queue_path);
        q.enqueue("topic-loop", Reason::Scheduled, "t").unwrap();
        q.claim_next("t", 1).unwrap();
        let s = build_snapshot(&p, now());
        assert_eq!(s["queue"]["running"][0]["step"], "Fetching feeds");
    }

    #[test]
    fn a_running_gitlab_loop_carries_its_agents_last_activity() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        status::write_status(
            &p.outputs.join("status.json"),
            "running",
            &[("pid", json!(child.id()))],
        )
        .unwrap();
        let activity = json!({"at": "2026-10-31T09:00:00+00:00", "label": "web #7", "event": "PreToolUse", "tool": "Bash", "detail": "cargo test"});
        std::fs::write(p.outputs.join("agent-activity.json"), activity.to_string()).unwrap();
        let s = build_snapshot(&p, now());
        assert_eq!(s["loops"]["issue-loop"]["agent_activity"], activity);
        // Parallel issues: every agent is listed, newest first; the single
        // `agent_activity` stays the newest one for older app builds.
        let other = json!({"at": "2026-10-31T09:05:00+00:00", "label": "api #2", "event": "Start", "tool": null, "detail": null});
        std::fs::create_dir_all(p.outputs.join("agent-activity")).unwrap();
        std::fs::write(
            p.outputs.join("agent-activity/api-2.json"),
            other.to_string(),
        )
        .unwrap();
        let s = build_snapshot(&p, now());
        assert_eq!(s["loops"]["issue-loop"]["agent_activity"], other);
        assert_eq!(
            s["loops"]["issue-loop"]["agent_activities"],
            json!([other, activity])
        );
        status::write_status(&p.outputs.join("status.json"), "idle", &[]).unwrap();
        let s = build_snapshot(&p, now());
        assert!(s["loops"]["issue-loop"].get("agent_activity").is_none());
        let mut child = child;
        let _ = child.kill();
    }

    #[test]
    fn a_running_guide_build_never_takes_the_profile_entry_of_its_alias() {
        use loopx_core::profile_progress::{JobKind, ProfileJob, Progress, Step};
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        let base = loopx_core::agent_activity::beside(&p.outputs.join("status.json"));
        let guide = ProfileJob::start(&base, JobKind::Guide, "web");
        guide.step(2, 5, Step::Page);
        let s = build_snapshot(&p, now());
        assert!(s["loops"].get("profile:web").is_none(), "{s}");
        let job = ProfileJob::start(&base, JobKind::Build, "web");
        job.step(3, 5, Step::Ai);
        let s = build_snapshot(&p, now());
        let l = &s["loops"]["profile:web"];
        assert_eq!(l["kind"], "build", "{s}");
        assert_eq!(l["current_issue"], "Profile \u{b7} web");
        drop((job, guide));
    }

    #[test]
    fn a_running_profile_job_is_a_running_agent_with_its_step_for_the_island() {
        use loopx_core::profile_progress::{JobKind, ProfileJob, Progress, Step};
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        let base = loopx_core::agent_activity::beside(&p.outputs.join("status.json"));
        let sync = ProfileJob::start(&base, JobKind::Sync, "");
        sync.project(2, 3, "web");
        let job = ProfileJob::start(&base, JobKind::Build, "web");
        job.step(3, 5, Step::Ai);
        let s = build_snapshot(&p, now());
        let l = &s["loops"]["profile:web"];
        assert_eq!(l["state"], "running", "{s}");
        assert_eq!(l["current_issue"], "Profile \u{b7} web");
        assert_eq!(l["current_step"], "Asking the AI \u{b7} step 3 of 5");
        assert_eq!(
            (
                l["kind"].as_str(),
                l["alias"].as_str(),
                l["step"].as_u64(),
                l["steps"].as_u64()
            ),
            (Some("build"), Some("web"), Some(3), Some(5))
        );
        assert_eq!(l["step_name"], "Asking the AI");
        let all = &s["loops"]["profile-sync"];
        assert_eq!(
            (
                all["kind"].as_str(),
                all["step"].as_u64(),
                all["steps"].as_u64()
            ),
            (Some("sync"), Some(2), Some(3))
        );
        assert_eq!(all["step_name"], "web");
        assert_eq!(s["app_state"], "running");
        assert_eq!(s["needs_you"]["count"], 0);
        drop((job, sync));
        let s = build_snapshot(&p, now());
        assert!(
            s["loops"].get("profile:web").is_none() && s["loops"].get("profile-sync").is_none(),
            "{s}"
        );
        assert_eq!(s["app_state"], "idle");
    }

    #[test]
    fn a_profile_record_from_a_dead_process_is_not_a_running_agent() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        let mut child = std::process::Command::new("true").spawn().unwrap();
        child.wait().unwrap();
        let dir = p.outputs.join("agent-activity");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("profile-sync.json"),
            json!({"at": "t", "started_at": "t", "label": "Profile sync", "event": "Profile", "tool": "profile",
                   "detail": "web", "kind": "sync", "section": null, "alias": "web", "step": 1, "steps": 2,
                   "step_code": "project", "pid": child.id()})
            .to_string(),
        )
        .unwrap();
        let s = build_snapshot(&p, now());
        assert!(s["loops"].get("profile-sync").is_none(), "{s}");
        assert_eq!(s["app_state"], "idle");
    }

    #[test]
    fn the_gitlab_loops_agent_list_leaves_out_profile_jobs() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        status::write_status(
            &p.outputs.join("status.json"),
            "running",
            &[("pid", json!(std::process::id()))],
        )
        .unwrap();
        let base = loopx_core::agent_activity::beside(&p.outputs.join("status.json"));
        let _job = loopx_core::profile_progress::ProfileJob::start(
            &base,
            loopx_core::profile_progress::JobKind::Build,
            "web",
        );
        let s = build_snapshot(&p, now());
        assert!(
            s["loops"]["issue-loop"].get("agent_activity").is_none(),
            "{s}"
        );
    }

    #[test]
    fn an_empty_root_is_idle() {
        let d = tempfile::tempdir().unwrap();
        let s = build_snapshot(&paths(d.path()), now());
        assert_eq!(s["app_state"], "idle");
        assert_eq!(s["loops"], json!({}));
        assert_eq!(s["attention"], Value::Null);
        assert_eq!(s["recent_events"], json!([]));
    }

    #[test]
    fn statuses_from_every_loop_file_are_collected() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        status::write_status(&p.outputs.join("status.json"), "idle", &[]).unwrap();
        status::write_status(&p.outputs.join("status/topic-loop.json"), "failed", &[]).unwrap();
        status::write_topic_status(
            &p.outputs.join("topic-monitor/status.json"),
            "ai",
            "running",
            &[],
        )
        .unwrap();
        let s = build_snapshot(&p, now());
        assert_eq!(s["loops"]["issue-loop"]["state"], "idle");
        assert_eq!(s["loops"]["topic-loop"]["state"], "failed");
        assert_eq!(s["topics"]["ai"]["state"], "running");
        assert_eq!(s["app_state"], "failed");
    }

    #[test]
    fn running_with_a_live_pid_is_running_and_with_a_dead_pid_is_stale() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        let me = std::process::id();
        status::write_status(
            &p.outputs.join("status.json"),
            "running",
            &[("pid", json!(me))],
        )
        .unwrap();
        assert_eq!(build_snapshot(&p, now())["app_state"], "running");
        status::write_status(
            &p.outputs.join("status.json"),
            "running",
            &[("pid", json!(0x3fff_fffeu32))],
        )
        .unwrap();
        let s = build_snapshot(&p, now());
        assert_eq!(s["loops"]["issue-loop"]["state"], "stale");
        assert_eq!(s["app_state"], "idle");
    }

    #[test]
    fn a_pending_approval_outranks_everything() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        status::write_status(&p.outputs.join("status.json"), "failed", &[]).unwrap();
        approvals::create(
            &p.approvals_dir,
            &approvals::NewApproval {
                kind: "open_mr".into(),
                project: "web".into(),
                title: "t".into(),
                timeout_seconds: 3600,
                ..Default::default()
            },
            now(),
        )
        .unwrap();
        let s = build_snapshot(&p, now());
        assert_eq!(s["app_state"], "awaiting_approval");
        assert_eq!(s["approvals"]["pending"].as_array().unwrap().len(), 1);
    }

    /// A pending approval of `kind` whose target holds page texts.
    fn texts_approval(p: &Paths, kind: &str) -> String {
        approvals::create(
            &p.approvals_dir,
            &approvals::NewApproval {
                kind: kind.into(),
                project: "web".into(),
                title: "t".into(),
                timeout_seconds: 3600,
                target: Some(
                    json!({"alias": "web", "slug": "feature-orders", "section": "stack",
                    "old": "OLD-TEXT", "new": "NEW TEXT \u{e9}", "old_hash": "h"}),
                ),
                ..Default::default()
            },
            now(),
        )
        .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn approval_listings_carry_no_page_texts() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        let guide = texts_approval(&p, "guide-page");
        let profile = texts_approval(&p, "profile-section");
        let other = texts_approval(&p, "open_mr");
        let s = build_snapshot(&p, now());
        let pending = s["approvals"]["pending"].as_array().unwrap();
        let by_id = |id: &str| pending.iter().find(|a| a["id"] == id).unwrap().clone();
        for id in [&guide, &profile] {
            let t = &by_id(id)["target"];
            assert!(t.get("old").is_none() && t.get("new").is_none(), "{t}");
            assert_eq!(t["old_len"], 8, "{t}");
            assert_eq!(t["new_len"], "NEW TEXT \u{e9}".len(), "{t}");
            assert_eq!(
                t["slug"], "feature-orders",
                "the rest of the target is kept"
            );
        }
        // Other kinds are untouched.
        assert_eq!(by_id(&other)["target"]["old"], "OLD-TEXT");
        // Only the other kind's copy of the text is in the snapshot.
        assert_eq!(s.to_string().matches("NEW TEXT").count(), 1, "{s}");
        // The store keeps the full text for the detail views.
        assert_eq!(
            approvals::get(&p.approvals_dir, &guide).unwrap()["target"]["new"],
            "NEW TEXT \u{e9}"
        );
    }

    #[test]
    fn expired_approvals_do_not_count_as_pending() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        approvals::create(
            &p.approvals_dir,
            &approvals::NewApproval {
                timeout_seconds: 60,
                ..Default::default()
            },
            now() - Duration::hours(1),
        )
        .unwrap();
        let s = build_snapshot(&p, now());
        assert_eq!(s["approvals"]["pending"], json!([]));
        assert_eq!(s["app_state"], "idle");
    }

    #[test]
    fn fresh_outcomes_surface_as_attention_and_old_ones_do_not() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        event(
            d.path(),
            "issue.completed",
            "2026-10-22T11:50:00.000Z",
            json!({"action": "fix", "outcome": "mr_opened", "mr_url": "http://mr/1"}),
        );
        let s = build_snapshot(&p, now());
        assert_eq!(s["attention"]["kind"], "mr_opened");
        assert_eq!(s["attention"]["mr_url"], "http://mr/1");
        assert_eq!(s["attention"]["project"], "web");

        event(
            d.path(),
            "issue.escalated",
            "2026-10-22T11:55:00.000Z",
            json!({"reason": "verification_failed"}),
        );
        let s = build_snapshot(&p, now());
        assert_eq!(
            s["attention"]["kind"], "escalated",
            "the newest outcome wins"
        );
        assert_eq!(s["attention"]["reason"], "verification_failed");

        let later = now() + Duration::minutes(30);
        assert_eq!(build_snapshot(&p, later)["attention"], Value::Null);
    }

    #[test]
    fn recent_events_keep_only_interesting_types_and_are_bounded() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        event(
            d.path(),
            "iteration.started",
            "2026-10-22T11:00:00.000Z",
            json!({}),
        );
        for i in 0..20 {
            event(
                d.path(),
                "issue.queued",
                &format!("2026-10-22T11:{i:02}:00.000Z"),
                json!({"n": i}),
            );
        }
        let s = build_snapshot(&p, now());
        let recent = s["recent_events"].as_array().unwrap();
        assert_eq!(recent.len(), RECENT_EVENTS);
        assert!(recent.iter().all(|e| e["type"] == "issue.queued"));
        assert_eq!(recent.last().unwrap()["data"]["n"], 19, "newest last");
    }
}
