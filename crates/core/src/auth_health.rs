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
    /// The AI CLI loops run with; `None` leaves it unchecked.
    pub ai_cli: Option<AiCliSource>,
}

/// Which AI CLI is selected (read at each check, so a switch is picked up) and
/// how to check it (`ai_cli_health::check`).
pub struct AiCliSource {
    pub selected: Box<dyn Fn() -> String + Send + Sync>,
    pub check: Arc<dyn Fn(&str) -> ProbeResult + Send + Sync>,
}

impl ProbeSources {
    /// Also probe the CLI selected in `ai_cli.json`, found on the login shell's
    /// `PATH` the way a run finds it.
    pub fn with_selected_ai_cli(mut self) -> Self {
        self.ai_cli = Some(AiCliSource {
            selected: Box::new(crate::ai_cli_config::selected_cli),
            check: Arc::new(|cli| {
                let current = std::env::var("PATH").unwrap_or_default();
                let path = crate::ai_cli_health::login_shell_path("/bin/zsh", &current);
                crate::ai_cli_health::check(cli, &path)
            }),
        });
        self
    }
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
    if crate::slack_report::send(&alert_report(text), None) {
        Ok(())
    } else {
        Err("Slack report not sent".into())
    }
}

/// `Sign-in needs attention: <account> - <why>` as a one-message report.
pub fn alert_report(text: &str) -> crate::slack_report::Report {
    use crate::slack_report::State;
    let (title, rest) = text.split_once(": ").unwrap_or(("Sign-in check", text));
    let state = if title == "Sign-in recovered" {
        State::Done
    } else {
        State::Warning
    };
    let status = if state == State::Done {
        "Connected again"
    } else {
        "Needs attention"
    };
    crate::slack_report::alert(title, state, status, rest)
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
                json!({"ok": true, "message": message, "checked_at": now_iso, "label": probe.label}),
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
        // What "Needs you" lists (`needs_you`).
        new.insert("label".into(), json!(probe.label));
        new.insert("confirmed".into(), json!(confirmed));
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

/// The person just signed `probe_id` back in (a Google sign-in finished, or a
/// Test passed): record it as healthy now instead of waiting for the next
/// 6-hourly probe, so "Needs you" drops it at once. No recovery alert follows,
/// since the person did it. Something never checked is left alone.
pub fn mark_reconnected(state_path: Option<&Path>, probe_id: &str, now: Option<NaiveDateTime>) {
    let path = state_path.map_or_else(default_state_path, Path::to_path_buf);
    let mut state = read_state(Some(&path));
    let Some(checks) = state.get_mut("checks").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(entry) = checks.get(probe_id) else {
        return;
    };
    if entry.get("ok").is_none_or(crate::loop_serialize::truthy) {
        return;
    }
    let label = entry
        .get("label")
        .cloned()
        .unwrap_or_else(|| json!(probe_id));
    let now = now.unwrap_or_else(|| Local::now().naive_local());
    checks.insert(
        probe_id.to_string(),
        json!({"ok": true, "message": "Reconnected", "checked_at": isoformat(now), "label": label}),
    );
    let _ = write_state(&state, &path);
}

/// A connector `test()` as a probe result: a rejected or revoked token is a
/// dead sign-in (alert at once), any other failure may be transient.
pub fn connector_result((ok, message): (bool, String)) -> ProbeResult {
    if !ok && crate::connectors::is_dead_sign_in(&message) {
        return Err(ProbeFailure::hard(message));
    }
    Ok((ok, message))
}

/// Something other than the 6-hourly check (a Test, a loop run) found
/// `probe_id`'s sign-in dead: list it in "Needs you" now. The check itself
/// still sends the Slack alert, so the person hears about it once.
pub fn mark_dead(
    state_path: Option<&Path>,
    probe_id: &str,
    label: &str,
    message: &str,
    now: Option<NaiveDateTime>,
) {
    let path = state_path.map_or_else(default_state_path, Path::to_path_buf);
    let mut state = read_state(Some(&path));
    let now_iso = isoformat(now.unwrap_or_else(|| Local::now().naive_local()));
    let Some(checks) = state.get_mut("checks").and_then(Value::as_object_mut) else {
        return;
    };
    let old = checks
        .get(probe_id)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let was_failing = !old.get("ok").is_none_or(crate::loop_serialize::truthy);
    let mut new = Map::new();
    new.insert("ok".into(), json!(false));
    new.insert("message".into(), json!(message));
    new.insert("checked_at".into(), json!(now_iso));
    let (count, since) = if was_failing {
        (
            old.get("fail_count").and_then(Value::as_i64).unwrap_or(0) + 1,
            old.get("failing_since")
                .filter(|v| truthy(Some(v)))
                .cloned()
                .unwrap_or_else(|| json!(now_iso)),
        )
    } else {
        (1, json!(now_iso))
    };
    new.insert("fail_count".into(), json!(count));
    new.insert("failing_since".into(), since);
    if was_failing && truthy(old.get("last_alert_at")) {
        new.insert("last_alert_at".into(), old["last_alert_at"].clone());
    }
    new.insert("label".into(), json!(label));
    new.insert("confirmed".into(), json!(true));
    checks.insert(probe_id.to_string(), Value::Object(new));
    let _ = write_state(&state, &path);
}

/// A sign-in check's verdict as other pages show it.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckView {
    pub ok: bool,
    pub message: String,
    /// When it was checked (naive local, as stored).
    pub at: String,
}

/// `check` (one entry of `checks`) when it is newer than `than`, the time the
/// page's own record was made (a Test, a run; RFC 3339 or naive local; `None`
/// when there is none). A failure counts only once confirmed: a single soft
/// miss is not news. `None` means the page's own record stands.
pub fn newer_check(check: Option<&Value>, than: Option<&str>) -> Option<CheckView> {
    let check = check?;
    let ok = check.get("ok").is_some_and(crate::loop_serialize::truthy);
    if !ok
        && !check
            .get("confirmed")
            .is_some_and(crate::loop_serialize::truthy)
    {
        return None;
    }
    let at = check.get("checked_at").and_then(Value::as_str)?;
    if let Some(than) = than.and_then(crate::needs_you::parse_time) {
        if crate::needs_you::parse_time(at)? <= than {
            return None;
        }
    }
    Some(CheckView {
        ok,
        message: check
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        at: at.to_string(),
    })
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
    if let Some(source) = &sources.ai_cli {
        let cli = (source.selected)();
        let check = source.check.clone();
        let label = format!("AI CLI ({cli})");
        probes.push(Probe::new("ai_cli", &label, move || check(&cli)));
    }
    probes
}

/// A mailbox refresh error as a probe failure: a missing or rejected refresh
/// token (or a 401) is a dead sign-in; anything else may be transient.
pub fn mail_failure(e: crate::mail_http::MailError) -> ProbeFailure {
    use crate::mail_http::MailError;
    match e {
        MailError::Reauth(m) => ProbeFailure::hard(m),
        MailError::AuthExpired(m) => ProbeFailure::hard(m.to_string()),
        MailError::Http(m) => ProbeFailure::other("MailHTTPError", m.to_string()),
        MailError::Keychain(m) => ProbeFailure::other("KeychainError", m),
        MailError::Other(m) => ProbeFailure::other("MailError", m),
    }
}

/// The real sources: enabled inboxes and connector accounts from `paths`
/// (connectors.json, ~/.gitlab, ~/.slack, inboxes.json), refreshed and tested
/// with the Keychain and the network.
pub fn config_sources(paths: crate::connectors_config::Paths) -> ProbeSources {
    use crate::connectors_config as cc;
    let inbox_path = paths.inbox();
    let account_paths = paths.clone();
    let connectors = Arc::new(cc::ConfigConnectors::real().with_paths(paths));
    ProbeSources {
        inboxes: Box::new(move || {
            crate::inbox_config::enabled_inboxes(&inbox_path).unwrap_or_default()
        }),
        accounts: Box::new(move || {
            cc::list_accounts(&account_paths)
                .map(|a| a.iter().map(crate::connectors::Account::to_value).collect())
                .unwrap_or_default()
        }),
        token: Arc::new(|inbox| {
            crate::mail_auth::get_access_token_default(inbox)
                .map(|_| ())
                .map_err(mail_failure)
        }),
        connector_test: Arc::new(move |id| {
            use crate::connectors::ConnectorSource;
            match connectors.load(id) {
                Ok(c) => connector_result(c.test()),
                Err(e) => Err(ProbeFailure::other("LoadFailure", format!("{e:?}"))),
            }
        }),
        ai_cli: None,
    }
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

    #[test]
    fn alerts_become_reports() {
        let r = alert_report("Sign-in needs attention: Gmail (Work) - expired. Reconnect it.");
        assert_eq!(
            crate::slack_report::plain_text(&r),
            "*Sign-in needs attention* :warning: Needs attention\nGmail (Work) - expired. Reconnect it.\n\n_LoopX_"
        );
        let r = alert_report("Sign-in recovered: Gmail (Work) is connected again.");
        assert_eq!(r.status.unwrap().state, crate::slack_report::State::Done);
    }

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
    fn checks_keep_their_label_and_whether_the_failure_is_confirmed() {
        let (t, mut sent) = (Tmp::new(), vec![]);
        let soft = Probe::new("cal", "Work calendar", || {
            Err(ProbeFailure::other("OSError", "timeout"))
        });
        run(&t, &[soft], &mut sent, now());
        assert_eq!(checks(&t)["cal"]["label"], "Work calendar");
        assert_eq!(checks(&t)["cal"]["confirmed"], false);
        run(&t, &[probe("gmail", reauth("expired"))], &mut sent, now());
        assert_eq!(checks(&t)["gmail"]["confirmed"], true);
        run(
            &t,
            &[probe("gmail", Ok((true, "ok".into())))],
            &mut sent,
            now(),
        );
        assert_eq!(checks(&t)["gmail"]["label"], "gmail");
    }

    #[test]
    fn a_reconnected_sign_in_is_ok_at_once_without_a_second_recovery_alert() {
        let (t, mut sent) = (Tmp::new(), vec![]);
        run(
            &t,
            &[probe("gmail", reauth("invalid_grant"))],
            &mut sent,
            now(),
        );
        assert_eq!(checks(&t)["gmail"]["ok"], false);
        mark_reconnected(Some(&t.path()), "gmail", Some(now() + Duration::minutes(5)));
        let c = &checks(&t)["gmail"];
        assert_eq!(c["ok"], true);
        assert_eq!(c["label"], "gmail");
        assert!(c.get("last_alert_at").is_none(), "{c}");
        // The next real probe passes quietly: the person already knows.
        run(
            &t,
            &[probe("gmail", Ok((true, "ok".into())))],
            &mut sent,
            now() + Duration::hours(6),
        );
        assert_eq!(sent.len(), 1, "{sent:?}");
    }

    #[test]
    fn reconnecting_something_never_checked_writes_nothing() {
        let t = Tmp::new();
        mark_reconnected(Some(&t.path()), "connector:new", Some(now()));
        assert!(!t.path().exists());
        run(
            &t,
            &[probe("gmail", Ok((true, "ok".into())))],
            &mut vec![],
            now(),
        );
        mark_reconnected(Some(&t.path()), "connector:new", Some(now()));
        assert!(!checks(&t).contains_key("connector:new"));
    }

    #[test]
    fn a_connector_whose_token_is_dead_is_a_hard_failure() {
        use crate::connectors::google_auth::REAUTH_MESSAGE;
        assert_eq!(
            connector_result((false, REAUTH_MESSAGE.into())),
            Err(ProbeFailure::Hard(REAUTH_MESSAGE.into()))
        );
        let rejected = crate::connectors::describe_http_error(
            &crate::connectors::ConnectorError::Http(crate::connectors::http::HttpError {
                status: Some(401),
                body: String::new(),
                url: String::new(),
                auth_expired: true,
            }),
        );
        assert!(matches!(
            connector_result((false, rejected)),
            Err(ProbeFailure::Hard(_))
        ));
        assert_eq!(
            connector_result((false, "Network error: timed out".into())),
            Ok((false, "Network error: timed out".into()))
        );
        assert_eq!(
            connector_result((true, "Connected".into())),
            Ok((true, "Connected".into()))
        );
    }

    #[test]
    fn a_dead_sign_in_seen_outside_the_check_is_listed_at_once() {
        let t = Tmp::new();
        mark_dead(
            Some(&t.path()),
            "connector:cal",
            "Work calendar",
            "expired",
            Some(now()),
        );
        let c = &checks(&t)["connector:cal"];
        assert_eq!(
            (c["ok"].clone(), c["confirmed"].clone()),
            (json!(false), json!(true))
        );
        assert_eq!(c["label"], "Work calendar");
        mark_dead(
            Some(&t.path()),
            "connector:cal",
            "Work calendar",
            "expired",
            Some(now() + Duration::hours(1)),
        );
        let c = &checks(&t)["connector:cal"];
        assert_eq!(c["failing_since"], json!(isoformat(now())), "{c}");
        assert_eq!(c["fail_count"], 2);
        // Only the 6-hourly check alerts Slack: it still sends the one alert.
        let mut sent = vec![];
        run(
            &t,
            &[Probe::new("connector:cal", "Work calendar", || {
                reauth("expired")
            })],
            &mut sent,
            now() + Duration::hours(2),
        );
        assert_eq!(sent.len(), 1, "{sent:?}");
    }

    #[test]
    fn a_check_counts_on_other_pages_only_when_it_is_newer_and_settled() {
        let dead = json!({"ok": false, "confirmed": true, "message": "expired", "checked_at": "2026-10-31T18:50:03.617752"});
        let one_miss = json!({"ok": false, "confirmed": false, "message": "timeout", "checked_at": "2026-10-31T18:50:03"});
        let fine = json!({"ok": true, "message": "Connected", "checked_at": "2026-10-31T18:50:03"});
        // A Test from a week ago loses to tonight's check, in any time format.
        let v = newer_check(Some(&dead), Some("2026-10-25T04:14:45Z")).unwrap();
        assert_eq!((v.ok, v.message.as_str()), (false, "expired"));
        assert!(newer_check(Some(&fine), None).unwrap().ok);
        // A run after the check wins; a single soft miss is not news.
        let later = (Local::now() + Duration::days(3650)).to_rfc3339();
        assert!(newer_check(Some(&dead), Some(&later)).is_none());
        assert!(newer_check(Some(&one_miss), None).is_none());
        assert!(newer_check(None, None).is_none());
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
    fn state_file_keys_keep_their_layout() {
        let (t, mut sent) = (Tmp::new(), vec![]);
        run(&t, &[probe("a", Ok((true, "ok".into())))], &mut sent, now());
        let text = std::fs::read_to_string(t.path()).unwrap();
        assert_eq!(
            text,
            "{\n  \"checks\": {\n    \"a\": {\n      \"ok\": true,\n      \"message\": \"ok\",\n      \"checked_at\": \"2026-10-16T12:00:00\",\n      \"label\": \"a\"\n    }\n  },\n  \"last_run_at\": \"2026-10-16T12:00:00\"\n}"
        );
        assert!(!t.0.join(".auth.json.tmp").exists());
    }

    #[test]
    fn config_sources_find_the_real_inboxes_and_auth_connectors() {
        let d = tempfile::tempdir().unwrap();
        let gitlab = d.path().join("gitlab.json");
        std::fs::write(
            &gitlab,
            r#"{"instances": {"work": {"url": "https://gitlab.test", "token": "t"}}}"#,
        )
        .unwrap();
        let inboxes = d.path().join("inboxes.json");
        std::fs::write(
            &inboxes,
            json!({"inboxes": [
                {"name": "me", "label": "Me", "provider": "gmail", "account": "me@example.com",
                 "enabled": true, "urgent_brief": "", "vip_senders": [], "exclude_senders": [],
                 "categories": null, "slack_bundle": null},
                {"name": "old", "label": "Old", "provider": "outlook", "account": "old@example.com",
                 "enabled": false, "urgent_brief": "", "vip_senders": [], "exclude_senders": [],
                 "categories": null, "slack_bundle": null}
            ]})
            .to_string(),
        )
        .unwrap();
        let paths = crate::connectors_config::Paths {
            config_path: Some(d.path().join("connectors.json")),
            gitlab_config_path: Some(gitlab),
            slack_config_path: Some(d.path().join("slack.json")),
            inbox_config_path: Some(inboxes),
        };
        let ids: Vec<String> = collect_probes(&config_sources(paths))
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, ["mailbox:me", "connector:work"]);
    }

    #[test]
    fn a_rejected_mail_refresh_token_is_a_hard_failure() {
        use crate::mail_http::MailError;
        assert_eq!(
            mail_failure(MailError::Reauth("sign in again".into())),
            ProbeFailure::hard("sign in again")
        );
        assert!(matches!(
            mail_failure(MailError::Other("x".into())),
            ProbeFailure::Other { .. }
        ));
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
            ai_cli: None,
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

    #[test]
    fn the_selected_ai_cli_is_probed_too() {
        let sources = ProbeSources {
            inboxes: Box::new(Vec::new),
            accounts: Box::new(Vec::new),
            token: Arc::new(|_| Ok(())),
            connector_test: Arc::new(|_| Ok((true, String::new()))),
            ai_cli: Some(AiCliSource {
                selected: Box::new(|| "codex".to_string()),
                check: Arc::new(|cli| Ok((true, format!("{cli} ok")))),
            }),
        };
        let probes = collect_probes(&sources);
        assert_eq!(probes.len(), 1);
        assert_eq!(
            (probes[0].id.as_str(), probes[0].label.as_str()),
            ("ai_cli", "AI CLI (codex)")
        );
        assert_eq!((probes[0].run)(), Ok((true, "codex ok".to_string())));
    }
}
