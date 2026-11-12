//! Helpers shared by the GitLab and topic runners: the unified log, best-effort
//! events and Slack alerts, and the agent CLI's JSON envelope.

use crate::config::home_dir;
use crate::events::{self, EventFields};
use crate::{loop_notify, slack};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How much of a failed invocation's output goes into an event's `data`.
pub const STDERR_EXCERPT_CHARS: usize = 800;

/// The checkout / install root (`LOOPX_ROOT`, else the loop-engineering home).
pub fn repo_root() -> PathBuf {
    match std::env::var_os("LOOPX_ROOT").filter(|v| !v.is_empty()) {
        Some(root) => PathBuf::from(root),
        None => home_dir(),
    }
}

pub fn unified_log_path(repo_root: &Path) -> PathBuf {
    repo_root.join("logs").join("loop-engineering.log")
}

/// Append `text` (newline-terminated) to the unified log; failures are ignored.
pub fn append_unified_log(text: &str, unified_log_path: &Path) {
    if let Some(dir) = unified_log_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(unified_log_path)
    {
        let _ = f.write_all(text.as_bytes());
        if !text.ends_with('\n') {
            let _ = f.write_all(b"\n");
        }
    }
}

/// The failure text for stdout a CLI exited 0 with that holds no reply.
pub fn unreadable_output(cli: &str, stdout: &str) -> String {
    format!("unreadable output from {cli}\n{stdout}")
}

/// The failure text for a CLI binary that could not be spawned; the hint tells
/// the user where to change the CLI.
pub fn could_not_start(cli: &str, program: &str, err: &dyn std::fmt::Display) -> String {
    format!("could not start {program} ({cli} CLI): {err}. Switch CLI in Settings -> AI CLI.")
}

/// The binary to spawn for `cli`: whichever candidate is on `PATH`, else the
/// first candidate, else the CLI id.
pub fn resolved_program(cli: &str) -> String {
    let path = std::env::var("PATH").unwrap_or_default();
    crate::ai_cli_config::resolve_binary(cli, &|n| crate::doctor::find_on_path(n, &path).is_some())
        .or_else(|| {
            crate::ai_cli_config::binary_candidates(cli)
                .first()
                .map(|n| n.to_string())
        })
        .unwrap_or_else(|| cli.to_string())
}

/// How long [`cli_launch_path`] reuses the login shell's answer before asking
/// again, so a CLI installed while the dashboard runs is found within minutes.
pub const LAUNCH_PATH_TTL: Duration = Duration::from_secs(10 * 60);

/// The shell asked for the user's `PATH`. Unit tests never start the real login
/// shell (it reads the real home): theirs does not exist, so they fall back to
/// the current `PATH`.
#[cfg(not(test))]
const LOGIN_SHELL: &str = "/bin/zsh";
#[cfg(test)]
const LOGIN_SHELL: &str = "/nonexistent/loopx-test-login-shell";

/// A computed value reused for a time-to-live. Callers waiting while it is
/// computed get the fresh value instead of computing it again.
pub struct TtlCache {
    ttl: Duration,
    slot: Mutex<Option<(Instant, String)>>,
}

impl TtlCache {
    pub const fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            slot: Mutex::new(None),
        }
    }

    /// The cached value when it is younger than the TTL at `now`, else
    /// `compute()`'s, which is kept from `now` on.
    pub fn get(&self, now: Instant, compute: impl FnOnce() -> String) -> String {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, value)) = slot.as_ref() {
            if now.saturating_duration_since(*at) < self.ttl {
                return value.clone();
            }
        }
        let value = compute();
        *slot = Some((now, value.clone()));
        value
    }

    #[cfg(test)]
    pub(crate) fn seed(&self, now: Instant, value: &str) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some((now, value.to_string()));
    }
}

static LAUNCH_PATH: TtlCache = TtlCache::new(LAUNCH_PATH_TTL);

/// The `PATH` an AI CLI is started with from any process: the user's login-shell
/// `PATH` in front of ours. A dashboard started by the Mac app or launchd has a
/// bare `PATH` (no `~/.local/bin`, nvm or Homebrew) where `claude` is not found.
/// The login shell is asked at most once per [`LAUNCH_PATH_TTL`] (and never waits
/// longer than its own command timeout); without an answer this is our `PATH`.
pub fn cli_launch_path() -> String {
    LAUNCH_PATH.get(Instant::now(), || {
        crate::ai_cli_health::login_shell_path(
            LOGIN_SHELL,
            &std::env::var("PATH").unwrap_or_default(),
        )
    })
}

/// Make [`cli_launch_path`] answer `path` (tests: no login shell runs).
#[cfg(test)]
pub(crate) fn seed_cli_launch_path(path: &str) {
    LAUNCH_PATH.seed(Instant::now(), path);
}

/// The last `max_chars` characters of `s`.
pub fn tail(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else {
        s.chars().skip(count - max_chars).collect()
    }
}

/// The first `max_chars` characters of `s`.
pub fn head(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Emit an event, never failing the run: observability must not take it down.
/// `run_id` falls back to `$LOOP_RUN_ID`; with neither, nothing is emitted.
pub fn emit_best_effort(
    event_type: &str,
    run_id: Option<&str>,
    fields: EventFields,
    events_dir: Option<&Path>,
) -> Option<Value> {
    let env_run = std::env::var("LOOP_RUN_ID").ok().filter(|v| !v.is_empty());
    let run_id = run_id.map(str::to_owned).or(env_run)?;
    match events::emit(event_type, &run_id, fields, events_dir) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("loopx: emitting {event_type} failed: {e}");
            None
        }
    }
}

/// A Slack alert nobody is watching for: a bound Block Kit template is used when
/// `notification_key` has one, and if Slack rejects it the plain text is retried
/// once so the alert itself still has a chance. Returns whether it was sent.
pub fn notify_slack_best_effort(
    message: &str,
    notification_key: Option<&str>,
    bundle: Option<&str>,
) -> bool {
    notify_slack_with(
        message,
        notification_key,
        bundle,
        &slack::default_config_path(),
        &|url, text, blocks| slack::post_message(url, text, blocks).map(|_| ()),
    )
}

type Poster<'a> = dyn Fn(&str, &str, Option<&Value>) -> Result<(), String> + 'a;

/// [`notify_slack_best_effort`] with the config path and the HTTP post injected.
pub fn notify_slack_with(
    message: &str,
    notification_key: Option<&str>,
    bundle: Option<&str>,
    config_path: &Path,
    post: &Poster<'_>,
) -> bool {
    let blocks = loop_notify::resolve_blocks(notification_key, message, config_path);
    let url = match slack::load_webhook_url(config_path, bundle) {
        Ok(url) => url,
        Err(e) => {
            eprintln!("loopx: Slack notification failed: {e}");
            return false;
        }
    };
    match post(&url, message, blocks.as_ref()) {
        Ok(()) => true,
        Err(e) if blocks.is_some() => match post(&url, message, None) {
            Ok(()) => true,
            Err(retry) => {
                eprintln!(
                    "loopx: Slack notification failed even without blocks: {retry} (first: {e})"
                );
                false
            }
        },
        Err(e) => {
            eprintln!("loopx: Slack notification failed: {e}");
            false
        }
    }
}

/// The Claude CLI's JSON result object, or `None` when absent/unparseable.
pub fn parse_envelope(stdout: &str) -> Option<Value> {
    if stdout.is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(stdout) {
        Ok(v) if v.is_object() => Some(v),
        _ => None,
    }
}

/// Spend of a failed call: the envelope's figure; the cap we passed only when
/// the CLI stopped on that cap without reporting one; else `None` (unknown).
pub fn failed_call_cost(
    envelope: Option<&Value>,
    budget_stop: bool,
    cap: Option<f64>,
) -> Option<f64> {
    let cost = envelope
        .and_then(|e| e.get("total_cost_usd"))
        .and_then(Value::as_f64);
    if cost.is_none() && budget_stop {
        cap
    } else {
        cost
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    #[test]
    fn the_launch_path_is_computed_once_within_its_ttl_and_again_after_it() {
        let cache = TtlCache::new(Duration::from_secs(600));
        let calls = RefCell::new(0);
        let compute = || {
            *calls.borrow_mut() += 1;
            format!("/login/{}:/usr/bin", calls.borrow())
        };
        let t0 = Instant::now();
        assert_eq!(cache.get(t0, compute), "/login/1:/usr/bin");
        assert_eq!(
            cache.get(t0 + Duration::from_secs(599), compute),
            "/login/1:/usr/bin"
        );
        assert_eq!(*calls.borrow(), 1, "reused within the TTL");
        assert_eq!(
            cache.get(t0 + Duration::from_secs(600), compute),
            "/login/2:/usr/bin"
        );
        assert_eq!(*calls.borrow(), 2, "asked again once the TTL is over");
        // A caller whose clock reading predates the stored value (it waited for
        // the lock) reuses it.
        assert_eq!(
            cache.get(t0 + Duration::from_secs(1), compute),
            "/login/2:/usr/bin"
        );
        assert_eq!(*calls.borrow(), 2);
    }

    #[test]
    fn the_launch_path_puts_the_login_shell_path_first() {
        let d = tempfile::tempdir().unwrap();
        let shell = crate::agents::testutil::script(
            d.path(),
            "fake-zsh",
            "printf '\\n__LOOPX_PATH__/only/in/login:/usr/bin\\n'",
        );
        let cache = TtlCache::new(LAUNCH_PATH_TTL);
        let got = cache.get(Instant::now(), || {
            crate::ai_cli_health::login_shell_path(&shell.display().to_string(), "/usr/bin:/bin")
        });
        assert_eq!(got, "/only/in/login:/usr/bin:/bin");
    }

    #[test]
    fn tail_and_head_count_characters() {
        assert_eq!(tail("héllo wörld", 5), "wörld");
        assert_eq!(head("héllo", 2), "hé");
        assert_eq!(tail("ab", 5), "ab");
    }

    #[test]
    fn unified_log_appends_with_newline() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("logs/x.log");
        append_unified_log("one", &p);
        append_unified_log("two\n", &p);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one\ntwo\n");
    }

    #[test]
    fn envelope_must_be_a_json_object() {
        assert!(parse_envelope("").is_none());
        assert!(parse_envelope("not json").is_none());
        assert!(parse_envelope("[1]").is_none());
        assert_eq!(parse_envelope(r#"{"a":1}"#).unwrap()["a"], 1);
    }

    #[test]
    fn failed_call_cost_prefers_the_envelope_then_the_cap() {
        let env = json!({"total_cost_usd": 0.4});
        assert_eq!(failed_call_cost(Some(&env), true, Some(1.0)), Some(0.4));
        assert_eq!(
            failed_call_cost(Some(&json!({})), true, Some(1.0)),
            Some(1.0)
        );
        assert_eq!(failed_call_cost(Some(&json!({})), false, Some(1.0)), None);
        assert_eq!(failed_call_cost(None, false, None), None);
    }

    fn slack_config(dir: &Path, with_template: bool) -> PathBuf {
        let p = dir.join("config.json");
        let templates = if with_template {
            json!({"t": {"notification_key": "k", "blocks": [{"text": "{{message}}"}]}})
        } else {
            json!({})
        };
        std::fs::write(
            &p,
            json!({"webhook_url": "http://hook", "block_templates": templates}).to_string(),
        )
        .unwrap();
        p
    }

    #[test]
    fn slack_retries_without_blocks_when_the_template_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        let cfg = slack_config(d.path(), true);
        let calls = RefCell::new(Vec::new());
        let post = |_: &str, text: &str, blocks: Option<&Value>| {
            calls.borrow_mut().push((text.to_string(), blocks.cloned()));
            if blocks.is_some() {
                Err("invalid_blocks".to_string())
            } else {
                Ok(())
            }
        };
        assert!(notify_slack_with("hi", Some("k"), None, &cfg, &post));
        let calls = calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, Some(json!([{"text": "hi"}])));
        assert_eq!(calls[1].1, None);
    }

    #[test]
    fn slack_gives_up_after_the_plain_retry_fails() {
        let d = tempfile::tempdir().unwrap();
        let cfg = slack_config(d.path(), true);
        let post = |_: &str, _: &str, _: Option<&Value>| Err("down".to_string());
        assert!(!notify_slack_with("hi", Some("k"), None, &cfg, &post));
    }

    #[test]
    fn slack_without_a_template_posts_plain_once() {
        let d = tempfile::tempdir().unwrap();
        let cfg = slack_config(d.path(), false);
        let n = RefCell::new(0);
        let post = |_: &str, _: &str, b: Option<&Value>| {
            *n.borrow_mut() += 1;
            assert!(b.is_none());
            Ok(())
        };
        assert!(notify_slack_with("hi", Some("k"), None, &cfg, &post));
        assert_eq!(*n.borrow(), 1);
    }

    #[test]
    fn slack_missing_config_reports_failure_without_panicking() {
        let d = tempfile::tempdir().unwrap();
        let post = |_: &str, _: &str, _: Option<&Value>| Ok(());
        assert!(!notify_slack_with(
            "hi",
            None,
            None,
            &d.path().join("nope.json"),
            &post
        ));
    }
}
