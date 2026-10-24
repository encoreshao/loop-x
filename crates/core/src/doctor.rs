//! `loopx doctor`: first-launch checks for a fresh machine.
//!
//! Detects the AI CLIs (`claude`, `codex`) on `PATH`, the `gitlab-config` skill,
//! the GitLab/Slack credential files, the three loop config files, and whether a
//! `loopx serve` backend is alive. Pure data in, pure data out: the caller picks
//! the environment ([`Env`]) so tests can point it at scratch dirs and a fake
//! `PATH`. Token values are never read into the report: a credential file is only
//! checked for existing and parsing.

use crate::ai_cli_config;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// How long `<cli> --version` may take before we give up on the version string.
const VERSION_TIMEOUT: Duration = Duration::from_secs(3);

/// Where to look. Built from the process environment, or by hand in tests.
#[derive(Debug, Clone)]
pub struct Env {
    /// The user's home (`$HOME`): `~/.gitlab`, `~/.slack`, `~/.encore-skills`.
    pub home: PathBuf,
    /// `$LOOP_ENGINEERING_HOME`, else `<home>/.loopx`.
    pub loop_home: PathBuf,
    /// A `PATH`-style, colon-separated list of directories.
    pub path: String,
}

impl Env {
    pub fn from_process() -> Self {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let loop_home = std::env::var_os("LOOP_ENGINEERING_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                crate::config::migrate_legacy_home(&home);
                crate::config::default_home_under(&home)
            });
        let path = std::env::var("PATH").unwrap_or_default();
        Self {
            home,
            loop_home,
            path,
        }
    }
}

/// First executable called `name` in `path`.
pub fn find_on_path(name: &str, path: &str) -> Option<PathBuf> {
    path.split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(name))
        .find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// The first non-empty line of `<exe> --version`, or `None` on failure/timeout.
fn cli_version(exe: &Path) -> Option<String> {
    let out = crate::proc::run(
        Command::new(exe).arg("--version"),
        None,
        Some(VERSION_TIMEOUT),
    )
    .ok()?;
    if !out.success() {
        return None;
    }
    out.stdout
        .lines()
        .chain(out.stderr.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

fn cli_report(name: &str, env: &Env) -> Value {
    match find_on_path(name, &env.path) {
        Some(p) => json!({
            "found": true,
            "path": p.display().to_string(),
            "version": cli_version(&p),
        }),
        None => json!({ "found": false, "path": null, "version": null }),
    }
}

/// `{path, exists, valid}`: `valid` is whether the file parses as JSON.
fn json_file_report(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).ok();
    let valid = text
        .as_deref()
        .is_some_and(|t| serde_json::from_str::<Value>(t).is_ok());
    json!({
        "path": path.display().to_string(),
        "exists": path.is_file(),
        "valid": valid,
    })
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}

/// `loopx.lock` state. The token in the lock file is deliberately not reported.
fn backend_report(lock_path: &Path) -> Value {
    let lock: Option<Value> = std::fs::read_to_string(lock_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    let pid = lock
        .as_ref()
        .and_then(|v| v.get("pid")?.as_u64())
        .and_then(|p| u32::try_from(p).ok());
    let running = pid.is_some_and(pid_alive);
    json!({
        "lock_path": lock_path.display().to_string(),
        "running": running,
        "stale_lock": lock.is_some() && !running,
        "pid": if running { json!(pid) } else { Value::Null },
        "port": if running { lock.as_ref().and_then(|v| v.get("port").cloned()).unwrap_or(Value::Null) } else { Value::Null },
        "version": if running { lock.as_ref().and_then(|v| v.get("version").cloned()).unwrap_or(Value::Null) } else { Value::Null },
    })
}

fn flag(v: &Value, pointer: &str) -> bool {
    v.pointer(pointer).and_then(Value::as_bool).unwrap_or(false)
}

/// Run every check. `ready` is true when the loops could actually run: the
/// selected AI CLI is installed. Everything else only produces `next_steps`.
pub fn check(env: &Env) -> Value {
    let claude = cli_report("claude", env);
    let codex = cli_report("codex", env);
    let selected = ai_cli_config::get_selected_cli(&env.loop_home.join("ai_cli.json"));
    let selected_found = flag(if selected == "codex" { &codex } else { &claude }, "/found");

    let skill_dir = env
        .home
        .join(".encore-skills")
        .join("skills")
        .join("gitlab-config");
    let skill = json!({
        "path": skill_dir.display().to_string(),
        "installed": skill_dir.is_dir(),
    });

    let gitlab = json_file_report(&env.home.join(".gitlab").join("config.json"));
    let slack = json_file_report(&env.home.join(".slack").join("config.json"));
    let projects = json_file_report(&env.loop_home.join("projects.json"));
    let loops = json_file_report(&env.loop_home.join("loops.json"));
    let topics = json_file_report(&env.loop_home.join("topics.json"));
    let backend = backend_report(&env.loop_home.join("loopx.lock"));

    let mut steps: Vec<String> = Vec::new();
    if !flag(&claude, "/found") && !flag(&codex, "/found") {
        steps.push(
            "Install an AI CLI: Claude Code (https://claude.com/claude-code) or the Codex CLI, then make sure it is on your PATH.".into(),
        );
    } else if !selected_found {
        let other = if selected == "codex" {
            "claude"
        } else {
            "codex"
        };
        steps.push(format!(
            "The selected AI CLI ({selected}) is not installed; install it, or switch with `loopx ai-cli set {other}`."
        ));
    }
    if !flag(&gitlab, "/exists") {
        steps.push(
            "Add your GitLab instance and token to ~/.gitlab/config.json (`loopx gitlab` reads it).".into(),
        );
    } else if !flag(&gitlab, "/valid") {
        steps.push("~/.gitlab/config.json is not valid JSON; fix it.".into());
    }
    if !flag(&slack, "/exists") {
        steps.push(
            "Optional: add a Slack webhook to ~/.slack/config.json for notifications.".into(),
        );
    } else if !flag(&slack, "/valid") {
        steps.push("~/.slack/config.json is not valid JSON; fix it.".into());
    }
    for (name, f) in [
        ("projects.json", &projects),
        ("loops.json", &loops),
        ("topics.json", &topics),
    ] {
        let path = f["path"].as_str().unwrap_or(name);
        if !flag(f, "/exists") {
            steps.push(format!(
                "Create {path}: re-run the installer, or copy config/{name}.template there."
            ));
        } else if !flag(f, "/valid") {
            steps.push(format!("{path} is not valid JSON; fix it."));
        }
    }
    if !flag(&skill, "/installed") {
        steps.push(
            "Optional: the gitlab-config skill is not installed (~/.encore-skills); `loopx gitlab` does not need it, but the Claude skills that call it do.".into(),
        );
    }
    if !flag(&backend, "/running") {
        steps.push("Start the dashboard backend: `loopx serve` (or open the Mac app).".into());
    }

    json!({
        "version": crate::VERSION,
        "home": env.home.display().to_string(),
        "loop_home": env.loop_home.display().to_string(),
        "ready": selected_found,
        "ai_clis": { "claude": claude, "codex": codex },
        "selected_cli": selected,
        "selected_cli_found": selected_found,
        "gitlab_skill": skill,
        "gitlab_config": gitlab,
        "slack_config": slack,
        "config": { "projects": projects, "loops": loops, "topics": topics },
        "backend": backend,
        "next_steps": steps,
    })
}

fn mark(ok: bool) -> &'static str {
    if ok {
        "ok     "
    } else {
        "MISSING"
    }
}

fn file_line(label: &str, f: &Value) -> String {
    let state = if !flag(f, "/exists") {
        "MISSING"
    } else if !flag(f, "/valid") {
        "INVALID"
    } else {
        "ok     "
    };
    format!(
        "  [{state}] {label}: {}\n",
        f["path"].as_str().unwrap_or_default()
    )
}

/// The report as text for a terminal.
pub fn render(report: &Value) -> String {
    let mut s = format!(
        "loopx doctor (loopx {})\n\nAI CLIs\n",
        report["version"].as_str().unwrap_or_default()
    );
    let selected = report["selected_cli"].as_str().unwrap_or("claude");
    for name in ["claude", "codex"] {
        let c = &report["ai_clis"][name];
        let found = flag(c, "/found");
        let detail = if found {
            format!(
                "{} ({})",
                c["path"].as_str().unwrap_or_default(),
                c["version"].as_str().unwrap_or("version unknown")
            )
        } else {
            "not found on PATH".to_string()
        };
        let sel = if name == selected { " [selected]" } else { "" };
        s.push_str(&format!("  [{}] {name}: {detail}{sel}\n", mark(found)));
    }
    s.push_str("\nCredentials\n");
    s.push_str(&file_line("GitLab", &report["gitlab_config"]));
    s.push_str(&file_line("Slack ", &report["slack_config"]));
    s.push_str(&format!(
        "  [{}] gitlab-config skill: {}\n",
        mark(flag(report, "/gitlab_skill/installed")),
        report["gitlab_skill"]["path"].as_str().unwrap_or_default()
    ));
    s.push_str("\nLoop config\n");
    for (label, key) in [
        ("projects.json", "projects"),
        ("loops.json   ", "loops"),
        ("topics.json  ", "topics"),
    ] {
        s.push_str(&file_line(label, &report["config"][key]));
    }
    s.push_str("\nBackend\n");
    if flag(report, "/backend/running") {
        s.push_str(&format!(
            "  [ok     ] loopx serve running: pid {}, port {}, version {}\n",
            report["backend"]["pid"], report["backend"]["port"], report["backend"]["version"]
        ));
    } else if flag(report, "/backend/stale_lock") {
        s.push_str("  [MISSING] not running (stale loopx.lock, it will be replaced on start)\n");
    } else {
        s.push_str("  [MISSING] loopx serve is not running\n");
    }
    s.push('\n');
    let steps = report["next_steps"].as_array().cloned().unwrap_or_default();
    if flag(report, "/ready") && steps.is_empty() {
        s.push_str("All set.\n");
    } else {
        if !flag(report, "/ready") {
            s.push_str("Not ready: the selected AI CLI is missing.\n");
        }
        if !steps.is_empty() {
            s.push_str("Next steps:\n");
            for (i, st) in steps.iter().enumerate() {
                s.push_str(&format!(
                    "  {}. {}\n",
                    i + 1,
                    st.as_str().unwrap_or_default()
                ));
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, name: &str, body: &str) {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    struct Fx {
        _t: tempfile::TempDir,
        env: Env,
        bin: PathBuf,
    }

    fn fx() -> Fx {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let bin = t.path().join("bin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let env = Env {
            loop_home: home.join(".loopx"),
            home,
            path: bin.display().to_string(),
        };
        Fx { _t: t, env, bin }
    }

    #[test]
    fn empty_machine_is_not_ready_and_says_what_to_do() {
        let f = fx();
        let r = check(&f.env);
        assert_eq!(r["ready"], false);
        assert_eq!(r["ai_clis"]["claude"]["found"], false);
        assert_eq!(r["gitlab_config"]["exists"], false);
        assert_eq!(r["backend"]["running"], false);
        let steps = r["next_steps"].to_string();
        assert!(steps.contains("Install an AI CLI"));
        assert!(steps.contains("projects.json"));
        assert!(render(&r).contains("Not ready"));
    }

    #[test]
    fn finds_clis_and_their_versions_on_the_given_path() {
        let f = fx();
        script(&f.bin, "claude", "echo '2.1.0 (Claude Code)'");
        script(&f.bin, "codex", "echo 'codex-cli 0.9.1'");
        let r = check(&f.env);
        assert_eq!(r["ai_clis"]["claude"]["version"], "2.1.0 (Claude Code)");
        assert_eq!(r["ai_clis"]["codex"]["version"], "codex-cli 0.9.1");
        assert_eq!(r["selected_cli"], "claude");
        assert_eq!(r["ready"], true);
    }

    #[test]
    fn a_hanging_version_check_times_out_but_the_cli_still_counts_as_found() {
        let f = fx();
        script(&f.bin, "claude", "exec sleep 30");
        let start = std::time::Instant::now();
        let r = check(&f.env);
        assert!(start.elapsed() < Duration::from_secs(15));
        assert_eq!(r["ai_clis"]["claude"]["found"], true);
        assert!(r["ai_clis"]["claude"]["version"].is_null());
    }

    #[test]
    fn selected_cli_follows_ai_cli_json() {
        let f = fx();
        script(&f.bin, "claude", "echo 1");
        std::fs::create_dir_all(&f.env.loop_home).unwrap();
        std::fs::write(f.env.loop_home.join("ai_cli.json"), r#"{"cli":"codex"}"#).unwrap();
        let r = check(&f.env);
        assert_eq!(r["selected_cli"], "codex");
        assert_eq!(r["ready"], false);
        assert!(r["next_steps"]
            .to_string()
            .contains("loopx ai-cli set claude"));
    }

    #[test]
    fn credential_files_are_checked_but_never_echoed() {
        let f = fx();
        std::fs::create_dir_all(f.env.home.join(".gitlab")).unwrap();
        std::fs::write(
            f.env.home.join(".gitlab/config.json"),
            r#"{"token":"glpat-SECRET"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(f.env.home.join(".slack")).unwrap();
        std::fs::write(f.env.home.join(".slack/config.json"), "{not json").unwrap();
        let r = check(&f.env);
        assert_eq!(r["gitlab_config"]["valid"], true);
        assert_eq!(r["slack_config"]["exists"], true);
        assert_eq!(r["slack_config"]["valid"], false);
        assert!(!r.to_string().contains("glpat-SECRET"));
        assert!(!render(&r).contains("glpat-SECRET"));
    }

    #[test]
    fn detects_the_skill_dir_and_config_files() {
        let f = fx();
        std::fs::create_dir_all(f.env.home.join(".encore-skills/skills/gitlab-config")).unwrap();
        std::fs::create_dir_all(&f.env.loop_home).unwrap();
        for n in ["projects.json", "loops.json", "topics.json"] {
            std::fs::write(f.env.loop_home.join(n), "{}").unwrap();
        }
        let r = check(&f.env);
        assert_eq!(r["gitlab_skill"]["installed"], true);
        assert_eq!(r["config"]["loops"]["valid"], true);
        assert!(!r["next_steps"].to_string().contains("gitlab-config skill"));
    }

    #[test]
    fn a_live_lock_means_running_and_a_dead_one_is_stale() {
        let f = fx();
        std::fs::create_dir_all(&f.env.loop_home).unwrap();
        let lock = f.env.loop_home.join("loopx.lock");
        let body = |pid: u32| {
            format!(
                r#"{{"pid":{pid},"port":48421,"version":"0.4.0","started_at":"x","token":"tok-SECRET"}}"#
            )
        };
        std::fs::write(&lock, body(std::process::id())).unwrap();
        let r = check(&f.env);
        assert_eq!(r["backend"]["running"], true);
        assert_eq!(r["backend"]["port"], 48421);
        assert!(!r.to_string().contains("tok-SECRET"));

        // A pid that cannot exist (above any real pid_max).
        std::fs::write(&lock, body(4_000_000_000)).unwrap();
        let r = check(&f.env);
        assert_eq!(r["backend"]["running"], false);
        assert_eq!(r["backend"]["stale_lock"], true);
    }

    #[test]
    fn fully_configured_machine_says_all_set() {
        let f = fx();
        script(&f.bin, "claude", "echo 1.0");
        std::fs::create_dir_all(f.env.home.join(".encore-skills/skills/gitlab-config")).unwrap();
        std::fs::create_dir_all(f.env.home.join(".gitlab")).unwrap();
        std::fs::write(f.env.home.join(".gitlab/config.json"), "{}").unwrap();
        std::fs::create_dir_all(f.env.home.join(".slack")).unwrap();
        std::fs::write(f.env.home.join(".slack/config.json"), "{}").unwrap();
        std::fs::create_dir_all(&f.env.loop_home).unwrap();
        for n in ["projects.json", "loops.json", "topics.json"] {
            std::fs::write(f.env.loop_home.join(n), "{}").unwrap();
        }
        std::fs::write(
            f.env.loop_home.join("loopx.lock"),
            format!(
                r#"{{"pid":{},"port":1,"version":"v","started_at":"x","token":"t"}}"#,
                std::process::id()
            ),
        )
        .unwrap();
        let r = check(&f.env);
        assert_eq!(r["next_steps"].as_array().unwrap().len(), 0);
        assert!(render(&r).contains("All set."));
    }
}
