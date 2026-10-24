//! Proactive sign-in check for every mailbox and auth-bearing connector.
//! Mirrors `bin/auth_health.py`.
//!
//! The scheduler calls [`run_if_due`] on each poll; it probes at most once per
//! [`CHECK_INTERVAL`]. A probe is a real token refresh (mailboxes) or the
//! connector's own `test()`, so it also keeps rotating refresh tokens
//! (Outlook) and idle ones (Google's 6-month inactivity limit) alive. A
//! sign-in that has died (Google refresh tokens last 7 days while the OAuth
//! consent screen is in "Testing" mode) is reported once to Slack, repeated
//! daily while it stays broken, and a recovery message follows. Never holds or
//! sends a secret.
//!
//! State is `<LOOP_ENGINEERING_HOME>/auth_health.json`:
//! `{"checks": {<probe id>: {ok, message, checked_at, [fail_count,
//! failing_since, last_alert_at]}}, "last_run_at": <naive local isoformat>}`.
//!
//! How probes are found depends on the mailbox token refresh and the connector
//! registry, which live elsewhere: they register a [`ProbeSources`] with
//! [`register_sources`] and [`collect_probes`] turns it into probes. Until one
//! is registered there is nothing to probe and the scheduler's check is a
//! no-op (it does not rewrite the state file).

use crate::config::home_dir;
use crate::loop_scheduler::isoformat;
use crate::pyjson;
use chrono::{Duration, Local, NaiveDateTime};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const CHECK_INTERVAL: Duration = Duration::hours(6);
pub const REMIND_INTERVAL: Duration = Duration::hours(24);
pub const SOFT_FAILURES_BEFORE_ALERT: i64 = 2;
/// Connector types whose credentials can expire or be revoked. Slack,
/// webhook, Telegram and RSS are skipped: their `test()` posts a message or
/// has no auth. "mailbox" accounts are covered by the inbox probes.
pub const AUTH_TYPES: [&str; 6] = [
    "google_calendar",
    "gitlab",
    "github",
    "jira",
    "linear",
    "notion",
];

pub fn default_state_path() -> PathBuf {
    home_dir().join("auth_health.json")
}

/// Why a probe could not even produce an `(ok, message)` answer.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeFailure {
    /// The sign-in is definitely dead (`ReauthRequired` / `AuthExpired`):
    /// alert at once. An empty message becomes "Sign-in expired".
    Hard(String),
    /// Anything else (a transient error or a crash): a soft failure that must
    /// repeat before it alerts. `class` is the exception name, e.g. `OSError`.
    Other { class: String, message: String },
}

impl ProbeFailure {
    /// A sign-in that is definitely dead (`ReauthRequired` / `AuthExpired`).
    pub fn hard(message: impl Into<String>) -> Self {
        ProbeFailure::Hard(message.into())
    }

    /// A transient error or a crash, named like the Python exception class.
    pub fn other(class: &str, message: impl Into<String>) -> Self {
        ProbeFailure::Other {
            class: class.to_string(),
            message: message.into(),
        }
    }
}

/// Delivers an alert text; an error is swallowed.
pub type SendFn<'a> = dyn FnMut(&str) -> Result<(), String> + 'a;
/// Refreshes one inbox's token.
pub type TokenFn = Arc<dyn Fn(&Value) -> Result<(), ProbeFailure> + Send + Sync>;

pub type ProbeResult = Result<(bool, String), ProbeFailure>;

pub struct Probe {
    pub id: String,
    pub label: String,
    pub run: Box<dyn Fn() -> ProbeResult + Send + Sync>,
}

impl Probe {
    pub fn new(
        id: &str,
        label: &str,
        run: impl Fn() -> ProbeResult + Send + Sync + 'static,
    ) -> Self {
        Self {
            id: id.to_string(),
            label: label.to_string(),
            run: Box::new(run),
        }
    }
}

/// Where probes come from. `inboxes` are enabled mailboxes (`{name, provider}`),
/// `accounts` are connector accounts (`{id, type, label, enabled}`).
pub struct ProbeSources {
    pub inboxes: Box<dyn Fn() -> Vec<Value> + Send + Sync>,
    pub accounts: Box<dyn Fn() -> Vec<Value> + Send + Sync>,
    /// A real token refresh for one inbox.
    pub token: TokenFn,
    /// `load_connector(id).test()`.
    pub connector_test: Arc<dyn Fn(&str) -> ProbeResult + Send + Sync>,
}

static REGISTERED: Mutex<Option<Arc<ProbeSources>>> = Mutex::new(None);

/// Make `sources` what [`run_if_due`] probes by default.
pub fn register_sources(sources: ProbeSources) {
    *REGISTERED.lock().unwrap() = Some(Arc::new(sources));
}

pub fn read_state(state_path: Option<&Path>) -> Map<String, Value> {
    let path = state_path.map_or_else(default_state_path, Path::to_path_buf);
    let mut data = match std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    if !data.get("checks").is_some_and(Value::is_object) {
        data.insert("checks".into(), json!({}));
    }
    data
}

fn write_state(state: &Map<String, Value>, path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(
        &tmp,
        pyjson::to_string_pretty(&Value::Object(state.clone())),
    )?;
    std::fs::rename(tmp, path)
}

fn default_send(text: &str) -> Result<(), String> {
    let url = crate::slack::load_webhook_url(&crate::slack::default_config_path(), None)?;
    crate::slack::post_message(&url, text, None).map(|_| ())
}

/// `datetime.fromisoformat` for the naive local times this file stores.
fn parse_iso(s: &str) -> Option<NaiveDateTime> {
    let s = s.replacen(' ', "T", 1);
    NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S%.f").ok()
}

fn elapsed(iso: Option<&Value>, now: NaiveDateTime, default: Duration) -> Duration {
    iso.and_then(Value::as_str)
        .and_then(parse_iso)
        .map_or(default, |t| now - t)
}

/// `(ok, message, hard)`. `hard` = the sign-in is definitely dead, so alert at
/// once; a soft failure (a connector `test()` or a transient error) must repeat.
fn evaluate(probe: &Probe) -> (bool, String, bool) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (probe.run)()));
    match result {
        Ok(Ok((ok, message))) => (ok, message, false),
        Ok(Err(ProbeFailure::Hard(m))) => (
            false,
            if m.is_empty() {
                "Sign-in expired".to_string()
            } else {
                m
            },
            true,
        ),
        Ok(Err(ProbeFailure::Other { class, message })) => {
            (false, format!("{class}: {message}"), false)
        }
        Err(_) => (false, "RuntimeError: probe panicked".to_string(), false),
    }
}

fn truthy(v: Option<&Value>) -> bool {
    v.is_some_and(crate::loop_serialize::truthy)
}

/// Probe each entry, update the state file and alert on transitions. Returns
/// the messages sent. `send` failures are swallowed: a Slack outage must not
/// lose the check.
pub fn run_checks(
    probes: &[Probe],
    state_path: Option<&Path>,
    now: Option<NaiveDateTime>,
    send: Option<&mut SendFn>,
) -> Vec<String> {
    let path = state_path.map_or_else(default_state_path, Path::to_path_buf);
    let now = now.unwrap_or_else(|| Local::now().naive_local());
    let mut default = default_send;
    let send: &mut dyn FnMut(&str) -> Result<(), String> = match send {
        Some(s) => s,
        None => &mut default,
    };
    let mut state = read_state(Some(&path));
    let mut checks = state
        .get("checks")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut sent: Vec<String> = Vec::new();
    let mut notify = |text: String, sent: &mut Vec<String>| -> bool {
        if send(&text).is_err() {
            return false;
        }
        sent.push(text);
        true
    };

    let now_iso = isoformat(now);
    for probe in probes {
        let (ok, message, hard) = evaluate(probe);
        let entry = checks
            .get(&probe.id)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if ok {
            if truthy(entry.get("last_alert_at")) {
                notify(
                    format!("Sign-in recovered: {} is connected again.", probe.label),
                    &mut sent,
                );
            }
            checks.insert(
                probe.id.clone(),
                json!({"ok": true, "message": message, "checked_at": now_iso}),
            );
            continue;
        }
        let was_failing = !entry.get("ok").is_none_or(crate::loop_serialize::truthy);
        let fail_count = if was_failing {
            entry.get("fail_count").and_then(Value::as_i64).unwrap_or(0) + 1
        } else {
            1
        };
        let failing_since = if was_failing {
            entry
                .get("failing_since")
                .filter(|v| truthy(Some(v)))
                .cloned()
        } else {
            None
        }
        .unwrap_or_else(|| json!(now_iso));
        let mut new = Map::new();
        new.insert("ok".into(), json!(false));
        new.insert("message".into(), json!(message));
        new.insert("checked_at".into(), json!(now_iso));
        new.insert("fail_count".into(), json!(fail_count));
        new.insert("failing_since".into(), failing_since);
        if truthy(entry.get("last_alert_at")) {
            new.insert("last_alert_at".into(), entry["last_alert_at"].clone());
        }
        let confirmed = hard || fail_count >= SOFT_FAILURES_BEFORE_ALERT;
        let due = elapsed(entry.get("last_alert_at"), now, REMIND_INTERVAL) >= REMIND_INTERVAL;
        if confirmed
            && due
            && notify(
                format!(
                    "Sign-in needs attention: {} - {message}. Reconnect it from the dashboard (Inbox Triage or Connectors).",
                    probe.label
                ),
                &mut sent,
            )
        {
            new.insert("last_alert_at".into(), json!(now_iso));
        }
        checks.insert(probe.id.clone(), Value::Object(new));
    }

    // Drop entries for accounts that no longer exist.
    let live: std::collections::HashSet<&str> = probes.iter().map(|p| p.id.as_str()).collect();
    checks.retain(|k, _| live.contains(k.as_str()));
    state.insert("checks".into(), Value::Object(checks));
    state.insert("last_run_at".into(), json!(now_iso));
    let _ = write_state(&state, &path);
    sent
}

/// One probe per enabled mailbox and per enabled auth-bearing connector.
pub fn collect_probes(sources: &ProbeSources) -> Vec<Probe> {
    let mut probes = Vec::new();
    for inbox in (sources.inboxes)() {
        let name = inbox
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let provider = inbox.get("provider").and_then(Value::as_str).unwrap_or("");
        let token = sources.token.clone();
        probes.push(Probe::new(
            &format!("mailbox:{name}"),
            &format!("{name} ({provider})"),
            move || {
                token(&inbox)?;
                Ok((true, "Token refreshed".to_string()))
            },
        ));
    }
    for account in (sources.accounts)() {
        let kind = account.get("type").and_then(Value::as_str).unwrap_or("");
        if !AUTH_TYPES.contains(&kind) || account.get("enabled") == Some(&json!(false)) {
            continue;
        }
        let id = account
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let label = account
            .get("label")
            .and_then(Value::as_str)
            .filter(|l| !l.is_empty())
            .unwrap_or(&id)
            .to_string();
        let test = sources.connector_test.clone();
        let probe_id = id.clone();
        probes.push(Probe::new(&format!("connector:{id}"), &label, move || {
            test(&probe_id)
        }));
    }
    probes
}

fn registered_probes() -> Option<Vec<Probe>> {
    let sources = REGISTERED.lock().unwrap().clone()?;
    Some(collect_probes(&sources))
}

/// True if it ran. Never fails: it must not break the scheduler poll. With no
/// `probes` it uses the registered [`ProbeSources`]; with none registered
/// there is nothing to probe and this returns false without touching state.
pub fn run_if_due(
    probes: Option<Vec<Probe>>,
    state_path: Option<&Path>,
    now: Option<NaiveDateTime>,
    send: Option<&mut SendFn>,
) -> bool {
    let path = state_path.map_or_else(default_state_path, Path::to_path_buf);
    let now = now.unwrap_or_else(|| Local::now().naive_local());
    let state = read_state(Some(&path));
    if let Some(last) = state.get("last_run_at") {
        if elapsed(Some(last), now, CHECK_INTERVAL) < CHECK_INTERVAL {
            return false;
        }
    }
    let Some(probes) = probes.or_else(registered_probes) else {
        return false;
    };
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_checks(&probes, Some(&path), Some(now), send);
    }));
    match ran {
        Ok(()) => true,
        Err(_) => {
            eprintln!("auth_health: probe run panicked");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 16)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
    }

    fn probe(id: &str, result: ProbeResult) -> Probe {
        Probe::new(id, id, move || result.clone())
    }

    fn reauth(m: &str) -> ProbeResult {
        Err(ProbeFailure::Hard(m.into()))
    }

    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Self {
            let d = std::env::temp_dir().join(format!("ah-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&d).unwrap();
            Tmp(d)
        }
        fn path(&self) -> PathBuf {
            self.0.join("auth.json")
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn run(t: &Tmp, probes: &[Probe], sent: &mut Vec<String>, at: NaiveDateTime) {
        let mut send = |s: &str| -> Result<(), String> {
            sent.push(s.to_string());
            Ok(())
        };
        run_checks(probes, Some(&t.path()), Some(at), Some(&mut send));
    }

    fn checks(t: &Tmp) -> Map<String, Value> {
        read_state(Some(&t.path()))["checks"]
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn healthy_probe_sends_nothing_and_is_recorded() {
        let (t, mut sent) = (Tmp::new(), vec![]);
        run(
            &t,
            &[probe("gmail", Ok((true, "ok".into())))],
            &mut sent,
            now(),
        );
        assert!(sent.is_empty());
        assert_eq!(checks(&t)["gmail"]["ok"], json!(true));
        assert_eq!(
            checks(&t)["gmail"]["checked_at"],
            json!("2026-10-16T12:00:00")
        );
    }

    #[test]
    fn reauth_required_alerts_immediately_once() {
        let (t, mut sent) = (Tmp::new(), vec![]);
        let probes = [probe(
            "gmail",
            reauth("Google refresh failed (invalid_grant)"),
        )];
        run(&t, &probes, &mut sent, now());
        run(&t, &probes, &mut sent, now() + Duration::hours(6));
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0],
            "Sign-in needs attention: gmail - Google refresh failed (invalid_grant). Reconnect it from the dashboard (Inbox Triage or Connectors)."
        );
    }

    #[test]
    fn failing_alert_repeats_after_a_day() {
        let (t, mut sent) = (Tmp::new(), vec![]);
        let probes = [probe("gmail", reauth("expired"))];
        run(&t, &probes, &mut sent, now());
        run(&t, &probes, &mut sent, now() + Duration::hours(25));
        assert_eq!(sent.len(), 2);
    }

    #[test]
    fn soft_failure_needs_two_consecutive_checks_to_alert() {
        let (t, mut sent) = (Tmp::new(), vec![]);
        let probes = [probe("gitlab", Ok((false, "HTTP 401".into())))];
        run(&t, &probes, &mut sent, now());
        assert!(sent.is_empty());
        assert_eq!(checks(&t)["gitlab"]["fail_count"], json!(1));
        run(&t, &probes, &mut sent, now() + Duration::hours(6));
        assert_eq!(sent.len(), 1);
        let c = &checks(&t)["gitlab"];
        assert_eq!(c["fail_count"], json!(2));
        assert_eq!(c["failing_since"], json!("2026-10-16T12:00:00"));
        assert_eq!(c["last_alert_at"], json!("2026-10-16T18:00:00"));
    }

    #[test]
    fn recovery_sends_one_message_and_clears_failure() {
        let (t, mut sent) = (Tmp::new(), vec![]);
        run(&t, &[probe("gmail", reauth("expired"))], &mut sent, now());
        let ok = [probe("gmail", Ok((true, "ok".into())))];
        run(&t, &ok, &mut sent, now() + Duration::hours(6));
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1], "Sign-in recovered: gmail is connected again.");
        run(&t, &ok, &mut sent, now() + Duration::hours(12));
        assert_eq!(sent.len(), 2);
        assert!(checks(&t)["gmail"].get("last_alert_at").is_none());
    }

    #[test]
    fn run_if_due_is_throttled() {
        let t = Tmp::new();
        let calls = Arc::new(Mutex::new(0));
        let mk = || {
            let calls = calls.clone();
            vec![Probe::new("g", "g", move || {
                *calls.lock().unwrap() += 1;
                Ok((true, "ok".into()))
            })]
        };
        let mut send = |_: &str| -> Result<(), String> { Ok(()) };
        let mut go =
            |at: NaiveDateTime| run_if_due(Some(mk()), Some(&t.path()), Some(at), Some(&mut send));
        assert!(go(now()));
        assert!(!go(now() + Duration::hours(1)));
        assert!(go(now() + Duration::hours(7)));
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    #[test]
    fn run_if_due_without_probes_or_sources_does_nothing() {
        let t = Tmp::new();
        assert!(!run_if_due(None, Some(&t.path()), Some(now()), None));
        assert!(!t.path().exists());
    }

    #[test]
    fn probe_crash_is_a_soft_failure_not_an_exception() {
        let (t, mut sent) = (Tmp::new(), vec![]);
        let boom = Probe::new("x", "x", || -> ProbeResult { panic!("boom") });
        let soft = probe(
            "y",
            Err(ProbeFailure::Other {
                class: "RuntimeError".into(),
                message: "boom".into(),
            }),
        );
        run(&t, &[boom, soft], &mut sent, now());
        assert_eq!(checks(&t)["x"]["ok"], json!(false));
        assert_eq!(checks(&t)["y"]["message"], json!("RuntimeError: boom"));
        assert!(sent.is_empty());
    }

    #[test]
    fn failed_send_does_not_lose_the_check() {
        let t = Tmp::new();
        let mut boom = |_: &str| -> Result<(), String> { Err("no network".into()) };
        let sent = run_checks(
            &[probe("gmail", reauth("expired"))],
            Some(&t.path()),
            Some(now()),
            Some(&mut boom),
        );
        assert!(sent.is_empty());
        let c = &checks(&t)["gmail"];
        assert_eq!(c["ok"], json!(false));
        // The alert did not go out, so there is no last_alert_at and it retries.
        assert!(c.get("last_alert_at").is_none());
    }

    #[test]
    fn empty_hard_failure_message_gets_a_default_and_dead_accounts_are_dropped() {
        let (t, mut sent) = (Tmp::new(), vec![]);
        run(
            &t,
            &[
                probe("old", Ok((true, "ok".into()))),
                probe("a", reauth("")),
            ],
            &mut sent,
            now(),
        );
        assert_eq!(checks(&t)["a"]["message"], json!("Sign-in expired"));
        run(
            &t,
            &[probe("a", Ok((true, "ok".into())))],
            &mut sent,
            now() + Duration::hours(7),
        );
        assert!(!checks(&t).contains_key("old"));
    }

    #[test]
    fn state_file_keys_match_the_python_layout() {
        let (t, mut sent) = (Tmp::new(), vec![]);
        run(&t, &[probe("a", Ok((true, "ok".into())))], &mut sent, now());
        let text = std::fs::read_to_string(t.path()).unwrap();
        assert_eq!(
            text,
            "{\n  \"checks\": {\n    \"a\": {\n      \"ok\": true,\n      \"message\": \"ok\",\n      \"checked_at\": \"2026-10-16T12:00:00\"\n    }\n  },\n  \"last_run_at\": \"2026-10-16T12:00:00\"\n}"
        );
        assert!(!t.0.join(".auth.json.tmp").exists());
    }

    #[test]
    fn collect_probes_covers_mailboxes_and_auth_connectors() {
        let sources = ProbeSources {
            inboxes: Box::new(|| vec![json!({"name": "work", "provider": "gmail"})]),
            accounts: Box::new(|| {
                vec![
                    json!({"id": "cal", "type": "google_calendar", "enabled": true}),
                    json!({"id": "gl", "type": "gitlab", "enabled": true, "label": "Main GitLab"}),
                    json!({"id": "off", "type": "gitlab", "enabled": false}),
                    json!({"id": "hook", "type": "slack", "enabled": true}),
                    json!({"id": "box", "type": "mailbox", "enabled": true}),
                ]
            }),
            token: Arc::new(|_| Ok(())),
            connector_test: Arc::new(|id| Ok((true, format!("tested {id}")))),
        };
        let probes = collect_probes(&sources);
        let ids: Vec<&str> = probes.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["mailbox:work", "connector:cal", "connector:gl"]);
        assert_eq!(probes[0].label, "work (gmail)");
        assert_eq!(probes[1].label, "cal");
        assert_eq!(probes[2].label, "Main GitLab");
        assert_eq!((probes[0].run)(), Ok((true, "Token refreshed".to_string())));
        assert_eq!((probes[2].run)(), Ok((true, "tested gl".to_string())));
    }
}
