//! Self-update state and UI (the update check's
//! side, and `bin/web/update_pages.py`): the topbar chip, the banner above every
//! page, the `/update` body, and the `git fetch` check behind `POST /update/check`.
//!
//! State lives in `<update_dir>/status.json` (what is new on origin) and
//! `progress.json` (the running upgrade), both written by the Python tools in
//! the same JSON shapes; the upgrade runner itself is still
//! `bin/update_runner.py`, which [`start_detached`] launches.

use super::i18n::{self, Lang};
use super::{consts, pyfmt};
use chrono::{DateTime, SecondsFormat, Utc};
use loopx_core::loops_config::py_truthy;
use loopx_core::{pydumps, pyjson};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::process::Command;

const STEP_ORDER: [&str; 6] = [
    "fetch",
    "pull",
    "build",
    "install",
    "restart_dashboard",
    "relaunch",
];
const IN_FLIGHT: [&str; 2] = ["starting", "running"];
const TERMINAL: [&str; 3] = ["done", "failed", "refused"];
/// The loop rewrites PROGRESS.md on every run; it must not block an upgrade.
const IGNORED_DIRTY_FILES: [&str; 1] = ["PROGRESS.md"];

fn empty() -> Value {
    json!({})
}

/// `update_check.read_status()`: `{}` when missing or unreadable.
pub fn read_status(dir: &Path) -> Value {
    match std::fs::read_to_string(dir.join("status.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        Some(v @ Value::Object(_)) => v,
        _ => empty(),
    }
}

fn pid_alive(pid: &Value) -> bool {
    let pid = match pid {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    };
    match pid {
        // Python's os.kill(pid, 0) raising PermissionError counts as "not alive".
        Some(p) if p > 0 => unsafe { libc::kill(p as libc::pid_t, 0) == 0 },
        _ => false,
    }
}

/// `update_runner.read_progress()`: a run whose process is gone reads as failed.
pub fn read_progress(dir: &Path) -> Value {
    let Some(mut progress) = std::fs::read_to_string(dir.join("progress.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(Value::is_object)
    else {
        return empty();
    };
    let state = progress.get("state").and_then(Value::as_str).unwrap_or("");
    if IN_FLIGHT.contains(&state) && !pid_alive(progress.get("pid").unwrap_or(&Value::Null)) {
        progress["state"] = json!("failed");
        progress["error"] = json!("Upgrade was interrupted");
    }
    progress
}

/// Both documents, as the shell reads them.
pub fn read_both(dir: &Path) -> (Value, Value) {
    (read_status(dir), read_progress(dir))
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).map(pydumps::py_str).unwrap_or_default()
}

fn truthy(v: &Value, key: &str) -> bool {
    v.get(key).is_some_and(py_truthy)
}

fn flag(v: &Value, key: &str) -> bool {
    v.get(key).is_none_or(py_truthy)
}

fn esc_t(lang: Lang, s: &str) -> String {
    pyfmt::escape(&i18n::t(lang, s, &[]))
}

fn step_label(lang: Lang, name: &str) -> String {
    match name {
        "fetch" => i18n::t(lang, "Fetch changes", &[]),
        "pull" => i18n::t(lang, "Pull latest code", &[]),
        "build" => i18n::t(lang, "Build app and DMG", &[]),
        "install" => i18n::t(lang, "Install app", &[]),
        "restart_dashboard" => i18n::t(lang, "Restart dashboard", &[]),
        "relaunch" => i18n::t(lang, "Relaunch Loop X", &[]),
        other => other.to_string(),
    }
}

fn summary(lang: Lang, status: &Value) -> String {
    if truthy(status, "new_release") && truthy(status, "latest_tag") {
        return i18n::t(
            lang,
            "New release {tag} is available",
            &[("tag", &pyfmt::escape(&text(status, "latest_tag")))],
        );
    }
    let behind = match status.get("behind") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        Some(Value::Bool(b)) => i64::from(*b),
        _ => 0,
    };
    i18n::t(
        lang,
        "{count} new change(s) on {branch}",
        &[
            ("count", &behind.to_string()),
            ("branch", &pyfmt::escape(&text(status, "branch"))),
        ],
    )
}

/// `render_chip`: the topbar pill, only while origin is ahead.
pub fn render_chip(lang: Lang, status: &Value) -> String {
    if !truthy(status, "behind") {
        return String::new();
    }
    format!(
        "<a class='pill' href='/update'><span class='material-symbols-outlined' aria-hidden='true'>system_update</span>{}</a>",
        esc_t(lang, "Update available")
    )
}

fn blocker(lang: Lang, status: &Value) -> Option<String> {
    if !flag(status, "clean") {
        return Some(i18n::t(
            lang,
            "You have local changes to tracked files; commit or stash them first.",
            &[],
        ));
    }
    if !flag(status, "can_fast_forward") {
        return Some(i18n::t(
            lang,
            "This checkout has diverged from origin; resolve it with git first.",
            &[],
        ));
    }
    None
}

/// `render_banner`: the card above any page when origin has something new (or
/// an upgrade is in flight).
pub fn render_banner(lang: Lang, status: &Value, progress: &Value, csrf_input: &str) -> String {
    let state = progress.get("state").and_then(Value::as_str).unwrap_or("");
    if IN_FLIGHT.contains(&state) {
        return format!(
            "<div class='card'><p>{} <a href='/update'>{}</a></p></div>",
            esc_t(lang, "An upgrade is in progress."),
            esc_t(lang, "View progress")
        );
    }
    if !truthy(status, "behind") {
        return String::new();
    }
    let commits: String = status
        .get("commits")
        .and_then(Value::as_array)
        .map(|c| c.iter().take(5).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|c| {
            format!(
                "<li><code>{}</code> {}</li>",
                pyfmt::escape(&text(c, "sha")),
                pyfmt::escape(&text(c, "subject"))
            )
        })
        .collect();
    let action = match blocker(lang, status) {
        Some(b) => format!("<p>{}</p>", pyfmt::escape(&b)),
        None => String::new(),
    };
    let check = format!(
        "<form method='post' action='/update/check'>{csrf_input}<button type='submit' class='btn btn-neutral'>{}</button></form>",
        esc_t(lang, "Check now")
    );
    format!(
        "<div class='card'><h2>{}</h2><p>{}</p><ul>{commits}</ul>{action}{check}</div>",
        esc_t(lang, "Update available"),
        summary(lang, status)
    )
}

fn steps_html(lang: Lang, progress: &Value) -> String {
    let steps: Vec<Value> = match progress
        .get("steps")
        .filter(|s| py_truthy(s))
        .and_then(Value::as_array)
    {
        Some(s) => s.clone(),
        None => STEP_ORDER
            .iter()
            .map(|n| json!({"name": n, "status": "pending", "log": ""}))
            .collect(),
    };
    let icon_for = |status: &str| -> String {
        consts::value("update_pages", "_STATUS_ICON")
            .and_then(|m| m.get(status))
            .and_then(Value::as_str)
            .unwrap_or("circle")
            .to_string()
    };
    let items: String = steps
        .iter()
        .map(|step| {
            let status = step
                .get("status")
                .map_or_else(|| "pending".to_string(), pydumps::py_str);
            let name = text(step, "name");
            let log = step
                .get("log")
                .filter(|l| py_truthy(l))
                .map(pydumps::py_str)
                .unwrap_or_default();
            format!(
                "<li data-step='{}' class='step-{}'><span class='material-symbols-outlined' aria-hidden='true'>{}</span> {}<pre>{}</pre></li>",
                pyfmt::escape(&name),
                pyfmt::escape(&status),
                icon_for(&status),
                pyfmt::escape(&step_label(lang, &name)),
                pyfmt::escape(&log)
            )
        })
        .collect();
    format!("<ol id='update-steps'>{items}</ol>")
}

fn poll_script() -> String {
    let terminal = pydumps::dumps(&TERMINAL.to_vec());
    format!(
        "<script>(function(){{var terminal={terminal};function render(d){{var steps=(d.progress&&d.progress.steps)||[];steps.forEach(function(s){{var li=document.querySelector(\"[data-step='\"+s.name+\"']\");if(!li)return;li.className='step-'+s.status;var pre=li.querySelector('pre');if(pre)pre.textContent=s.log||'';}});if(d.progress&&terminal.indexOf(d.progress.state)>=0){{location.reload();return false;}}return true;}}function poll(){{fetch('/update/progress',{{cache:'no-store'}}).then(function(r){{return r.json();}}).then(function(d){{if(render(d))setTimeout(poll,1000);}}).catch(function(){{setTimeout(poll,1500);}});}}poll();}})();</script>"
    )
}

/// `render_update_body`.
pub fn render_update_body(lang: Lang, status: &Value, progress: &Value) -> String {
    let state = progress.get("state").and_then(Value::as_str).unwrap_or("");
    let mut out = format!("<div class='card'><h1>{}</h1>", esc_t(lang, "Update"));
    if state == "done" && !truthy(status, "behind") {
        let to = progress.get("to").map_or_else(String::new, pydumps::py_str);
        out.push_str(&format!(
            "<p>{}</p>",
            pyfmt::escape(&i18n::t(lang, "Updated to {version}", &[("version", &to)]))
        ));
    } else if state == "failed" || state == "refused" {
        let error = progress
            .get("error")
            .filter(|e| py_truthy(e))
            .map(pydumps::py_str)
            .unwrap_or_default();
        out.push_str(&format!(
            "<p class='flash flash-danger'>{}</p>",
            pyfmt::escape(&error)
        ));
    } else if IN_FLIGHT.contains(&state) {
        out.push_str(&format!(
            "<p>{}</p>",
            esc_t(
                lang,
                "Upgrading Loop X - this window will relaunch when it is done."
            )
        ));
    } else if truthy(status, "behind") {
        out.push_str(&format!("<p>{}</p>", summary(lang, status)));
        if let Some(b) = blocker(lang, status) {
            out.push_str(&format!("<p>{}</p>", pyfmt::escape(&b)));
        }
    } else {
        out.push_str(&format!("<p>{}</p>", esc_t(lang, "Loop X is up to date.")));
    }
    out.push_str("</div>");
    if truthy(progress, "steps") || IN_FLIGHT.contains(&state) || TERMINAL.contains(&state) {
        out.push_str(&format!(
            "<div class='card'>{}</div>",
            steps_html(lang, progress)
        ));
    }
    if IN_FLIGHT.contains(&state) {
        out.push_str(&poll_script());
    }
    out
}

// ---- the write side: `POST /update/check` and `POST /update/start` ----

fn git(repo: &Path, args: &[&str], non_interactive: bool) -> Option<(bool, String, String)> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(args);
    if non_interactive {
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        let ssh = std::env::var("GIT_SSH_COMMAND").unwrap_or_else(|_| "ssh".to_string());
        cmd.env("GIT_SSH_COMMAND", format!("{ssh} -o BatchMode=yes"));
    }
    let out = cmd.output().ok()?;
    Some((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    match git(repo, args, false) {
        Some((true, stdout, _)) => stdout.trim().to_string(),
        _ => String::new(),
    }
}

fn is_clean(repo: &Path) -> bool {
    let stdout = git(
        repo,
        &["status", "--porcelain", "--untracked-files=no"],
        false,
    )
    .map(|(_, o, _)| o)
    .unwrap_or_default();
    pyfmt::splitlines(&stdout).iter().all(|line| {
        let name = line.chars().skip(3).collect::<String>();
        IGNORED_DIRTY_FILES.contains(&pyfmt::strip(&name))
    })
}

fn write_json(dir: &Path, name: &str, data: &Value) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, pyjson::to_string_pretty(data))?;
    std::fs::rename(tmp, dir.join(name))
}

fn now_secs(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// `update_check.check(repo, state_dir, fetch)`: compare HEAD with
/// `origin/<branch>` and record the answer in `status.json`. Never fails: a
/// problem is recorded in `error`.
pub fn check(repo: &Path, dir: &Path, fetch: bool, now: DateTime<Utc>) -> Value {
    let previous = read_status(dir);
    let prev = |k: &str, d: Value| previous.get(k).cloned().unwrap_or(d);
    let mut status = Map::new();
    status.insert("checked_at".into(), json!(now_secs(now)));
    status.insert("branch".into(), json!(""));
    status.insert("behind".into(), prev("behind", json!(0)));
    status.insert("ahead".into(), prev("ahead", json!(0)));
    status.insert("commits".into(), prev("commits", json!([])));
    status.insert("current".into(), json!(""));
    status.insert("latest_tag".into(), prev("latest_tag", json!("")));
    status.insert("new_release".into(), prev("new_release", json!(false)));
    status.insert("origin_sha".into(), prev("origin_sha", json!("")));
    status.insert("clean".into(), json!(true));
    status.insert("can_fast_forward".into(), json!(true));
    status.insert("error".into(), Value::Null);
    let mut status = Value::Object(status);
    let save = |status: &mut Value| {
        if let Err(e) = write_json(dir, "status.json", status) {
            if status["error"].is_null() {
                status["error"] = json!(e.to_string());
            }
        }
    };
    let branch = git_out(repo, &["rev-parse", "--abbrev-ref", "HEAD"]);
    status["branch"] = json!(branch);
    status["current"] = json!(git_out(repo, &["describe", "--tags", "--always"]));
    status["clean"] = json!(is_clean(repo));
    if fetch {
        match git(repo, &["fetch", "--tags", "--quiet", "origin"], true) {
            Some((true, _, _)) => {}
            other => {
                let stderr = other
                    .map(|(_, _, e)| e.trim().to_string())
                    .unwrap_or_default();
                status["error"] = json!(if stderr.is_empty() {
                    "git fetch failed".to_string()
                } else {
                    stderr
                });
                save(&mut status);
                return status;
            }
        }
    }
    let remote = format!("origin/{branch}");
    let has_remote = !branch.is_empty()
        && branch != "HEAD"
        && git(repo, &["rev-parse", "--verify", "--quiet", &remote], false)
            .is_some_and(|(ok, _, _)| ok);
    if !has_remote {
        status["behind"] = json!(0);
        status["ahead"] = json!(0);
        status["commits"] = json!([]);
        status["error"] = json!(format!("no {remote} to compare against"));
        save(&mut status);
        return status;
    }
    let count = |range: String| -> i64 {
        git_out(repo, &["rev-list", "--count", &range])
            .parse()
            .unwrap_or(0)
    };
    status["behind"] = json!(count(format!("HEAD..{remote}")));
    status["ahead"] = json!(count(format!("{remote}..HEAD")));
    status["origin_sha"] = json!(git_out(repo, &["rev-parse", &remote]));
    let log = git_out(
        repo,
        &[
            "log",
            "--format=%h%x09%s",
            "-20",
            &format!("HEAD..{remote}"),
        ],
    );
    status["commits"] = Value::Array(
        pyfmt::splitlines(&log)
            .into_iter()
            .map(|line| {
                let (sha, _, subject) = match line.split_once('\t') {
                    Some((a, b)) => (a, "\t", b),
                    None => (line, "", ""),
                };
                json!({"sha": sha, "subject": subject})
            })
            .collect(),
    );
    status["can_fast_forward"] = json!(git(
        repo,
        &["merge-base", "--is-ancestor", "HEAD", &remote],
        false
    )
    .is_some_and(|(ok, _, _)| ok));
    let latest = git_out(repo, &["describe", "--tags", "--abbrev=0", &remote]);
    let here = git_out(repo, &["describe", "--tags", "--abbrev=0", "HEAD"]);
    status["new_release"] = json!(!latest.is_empty() && latest != here);
    status["latest_tag"] = json!(latest);
    save(&mut status);
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(behind: i64) -> Value {
        json!({"behind": behind, "branch": "main", "commits": [{"sha": "abc123", "subject": "Fix <it>"}]})
    }

    #[test]
    fn chip_and_banner_only_when_behind() {
        assert_eq!(render_chip(Lang::En, &json!({})), "");
        assert!(render_chip(Lang::En, &status(2)).contains("Update available"));
        assert_eq!(render_banner(Lang::En, &json!({}), &json!({}), ""), "");
        let banner = render_banner(Lang::En, &status(2), &json!({}), "<csrf>");
        assert!(banner.contains("2 new change(s) on main"));
        assert!(banner.contains("<code>abc123</code> Fix &lt;it&gt;"));
        assert!(banner.contains("action='/update/check'><csrf>"));
        let dirty = render_banner(
            Lang::En,
            &json!({"behind": 1, "clean": false}),
            &json!({}),
            "",
        );
        assert!(dirty.contains("local changes") && !dirty.contains("/update/start"));
    }

    #[test]
    fn an_upgrade_in_flight_replaces_the_banner() {
        let b = render_banner(Lang::En, &status(2), &json!({"state": "running"}), "");
        assert!(b.contains("An upgrade is in progress."));
    }

    #[test]
    fn a_dead_runner_reads_as_failed() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("progress.json"),
            r#"{"state": "running", "pid": 999999999}"#,
        )
        .unwrap();
        let p = read_progress(d.path());
        assert_eq!(p["state"], "failed");
        assert_eq!(p["error"], "Upgrade was interrupted");
        assert_eq!(read_progress(&d.path().join("nope")), json!({}));
    }

    #[test]
    fn body_states() {
        let up = render_update_body(Lang::En, &json!({}), &json!({}));
        assert!(up.contains("Loop X is up to date.") && !up.contains("update-steps"));
        let failed = render_update_body(
            Lang::En,
            &json!({}),
            &json!({"state": "failed", "error": "boom"}),
        );
        assert!(failed.contains("flash flash-danger'>boom") && failed.contains("update-steps"));
        let running = render_update_body(Lang::En, &json!({}), &json!({"state": "running"}));
        assert!(running
            .contains("<script>(function(){var terminal=[\"done\", \"failed\", \"refused\"];"));
    }

    #[test]
    fn check_in_a_repo_without_a_remote_records_the_reason() {
        let repo = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
        ] {
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .output()
                .unwrap();
        }
        let s = check(repo.path(), dir.path(), false, Utc::now());
        assert_eq!(s["behind"], 0);
        assert!(s["error"].as_str().unwrap().starts_with("no origin/"));
        assert_eq!(read_status(dir.path())["error"], s["error"]);
    }
}
