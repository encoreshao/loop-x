//! Scheduled runs and the user's own AI subscription: LoopX runs on the plan the
//! user pays for, so an unattended loop must not use up the quota they need for
//! their own work. Two signals pause scheduled runs until the limit resets:
//!
//! - Opt-in, Claude only: before a scheduled run, read the subscription's 5-hour
//!   and weekly usage (the endpoint Claude Code's own `/usage` reads, with the
//!   CLI's OAuth token) and pause when either is above the user's threshold.
//! - Always: a run whose output says the CLI hit its usage limit.
//!
//! A pause is one file, `quota_pause.json` in the loopx home. The scheduler queues
//! nothing while it is active for the selected CLI, and a scheduled run that
//! starts anyway skips without starting the agent. "Run now" is never paused.

use crate::config::home_dir;
use crate::connectors::http::{HttpClient, HttpRequest};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// Thresholds offered in Settings → AI CLI; absent means off.
pub const PAUSE_PERCENT_CHOICES: [i64; 3] = [80, 90, 95];
/// How long to pause after a limit message that gives no reset time.
pub const DEFAULT_PAUSE_MINUTES: i64 = 60;
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// One usage window of the subscription.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// "5-hour" or "weekly".
    pub label: &'static str,
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

/// Reads the subscription's usage windows (a seam: tests pass canned ones).
pub type UsageProbe = fn() -> Result<Vec<Window>, String>;

/// A pause as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Pause {
    pub cli: String,
    pub until: DateTime<Utc>,
    pub reason: String,
}

pub fn pause_path() -> PathBuf {
    home_dir().join("quota_pause.json")
}

/// The pause for `cli` that is still in force at `now`.
pub fn active_pause(path: &Path, cli: &str, now: DateTime<Utc>) -> Option<Pause> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let pause = Pause {
        cli: v.get("cli")?.as_str()?.to_string(),
        until: v.get("until")?.as_str()?.parse::<DateTime<Utc>>().ok()?,
        reason: v
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    };
    (pause.cli == cli && now < pause.until).then_some(pause)
}

pub fn write_pause(path: &Path, pause: &Pause) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let doc = json!({
        "cli": pause.cli,
        "until": pause.until.to_rfc3339(),
        "reason": pause.reason,
    });
    std::fs::write(path, crate::pyjson::to_string_pretty(&doc))
}

/// `ai_cli.json`'s `pause_above_percent`, when it is one of the choices.
pub fn pause_threshold(ai_cli_config: &serde_json::Map<String, Value>) -> Option<f64> {
    ai_cli_config
        .get("pause_above_percent")
        .and_then(Value::as_i64)
        .filter(|p| PAUSE_PERCENT_CHOICES.contains(p))
        .map(|p| p as f64)
}

/// Epoch seconds or milliseconds, numeric strings, or RFC 3339.
fn parse_reset(v: &Value) -> Option<DateTime<Utc>> {
    let epoch = |n: f64| {
        let ms = if n > 1e10 { n } else { n * 1000.0 };
        DateTime::<Utc>::from_timestamp_millis(ms as i64)
    };
    match v {
        Value::Number(n) => epoch(n.as_f64()?),
        Value::String(s) => match s.trim().parse::<f64>() {
            Ok(n) => epoch(n),
            Err(_) => s.parse::<DateTime<Utc>>().ok(),
        },
        _ => None,
    }
}

/// The 5-hour and weekly windows of an `oauth/usage` response.
pub fn parse_claude_usage(v: &Value) -> Vec<Window> {
    [("five_hour", "5-hour"), ("seven_day", "weekly")]
        .into_iter()
        .filter_map(|(key, label)| {
            let w = v.get(key)?;
            let used = w
                .get("utilization")
                .or_else(|| w.get("used_percentage"))
                .and_then(Value::as_f64)?;
            Some(Window {
                label,
                used_percent: used.clamp(0.0, 100.0),
                resets_at: w.get("resets_at").and_then(parse_reset),
            })
        })
        .collect()
}

/// The fullest window at or above `threshold` percent.
pub fn over_threshold(windows: &[Window], threshold: f64) -> Option<&Window> {
    windows
        .iter()
        .filter(|w| w.used_percent >= threshold)
        .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent))
}

/// The access token in Claude Code's stored credentials (`claudeAiOauth`).
pub fn oauth_token(credentials_json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(credentials_json).ok()?;
    v.get("claudeAiOauth")?
        .get("accessToken")?
        .as_str()
        .filter(|t| !t.is_empty())
        .map(String::from)
}

/// Claude Code's credentials: its Keychain item on macOS, else
/// `~/.claude/.credentials.json`. The `security` call is bounded so a Keychain
/// prompt can never hold a scheduled run.
fn claude_credentials() -> Option<String> {
    let keychain = crate::proc::run(
        std::process::Command::new("security").args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-w",
        ]),
        None,
        Some(std::time::Duration::from_secs(5)),
    )
    .ok()
    .filter(crate::proc::ProcOutput::success)
    .map(|o| o.stdout.trim().to_string())
    .filter(|s| !s.is_empty());
    keychain.or_else(|| {
        let home = std::env::var_os("HOME")?;
        std::fs::read_to_string(Path::new(&home).join(".claude/.credentials.json")).ok()
    })
}

/// Reads the subscription's usage windows from `url` with `token`.
pub fn fetch_claude_usage(
    client: &dyn HttpClient,
    url: &str,
    token: &str,
) -> Result<Vec<Window>, String> {
    let req = HttpRequest::new("GET", url)
        .token(Some(token))
        .header("anthropic-beta", "oauth-2025-04-20")
        .timeout_secs(10)
        .attempts(1);
    let body = client.request_json(&req).map_err(|e| e.to_string())?;
    let windows = parse_claude_usage(&body);
    if windows.is_empty() {
        return Err("the usage response had no 5-hour or weekly window".into());
    }
    Ok(windows)
}

/// The live probe: Claude Code's own token against [`USAGE_URL`].
pub fn live_claude_usage() -> Result<Vec<Window>, String> {
    let token = claude_credentials()
        .as_deref()
        .and_then(oauth_token)
        .ok_or("no Claude Code sign-in found")?;
    fetch_claude_usage(
        &crate::connectors::http::UreqClient::default(),
        USAGE_URL,
        &token,
    )
}

/// The pause a full window calls for: until it resets, or an hour when the
/// response gave no future reset time.
pub fn pause_for(cli: &str, window: &Window, threshold: f64, now: DateTime<Utc>) -> Pause {
    Pause {
        cli: cli.to_string(),
        until: window
            .resets_at
            .filter(|t| *t > now)
            .unwrap_or(now + Duration::minutes(DEFAULT_PAUSE_MINUTES)),
        reason: format!(
            "{} usage at {:.0}% (pausing at {threshold:.0}%)",
            window.label, window.used_percent
        ),
    }
}

/// Whether a run's output says the AI CLI refused for a usage or rate limit.
pub fn limit_hit(output: &str) -> bool {
    let text = output.to_lowercase();
    [
        "usage limit reached",
        "usage limit has been reached",
        "you've hit your usage limit",
        "you've hit your limit",
        "you have hit your limit",
        "rate limit reached",
        "rate_limit_error",
        "resource_exhausted",
        "quota exceeded",
        "exceeded your current quota",
    ]
    .iter()
    .any(|p| text.contains(p))
}

/// Whether a run's output says the AI CLI is not signed in, or its sign-in or
/// API key was rejected.
pub fn sign_in_lapsed(output: &str) -> bool {
    let text = output.to_lowercase();
    [
        "invalid api key",
        "please run /login",
        "oauth token has expired",
        "not logged in",
        "401 unauthorized",
        "authentication_error",
        "please login",
        "please log in",
        "authentication required",
    ]
    .iter()
    .any(|p| text.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 31, h, m, 0).unwrap()
    }

    #[test]
    fn parses_both_windows_and_every_reset_shape() {
        let v = json!({
            "five_hour": {"utilization": 92.5, "resets_at": "2026-10-31T14:00:00Z"},
            "seven_day": {"used_percentage": 40, "resets_at": 1793462400},
            "extra_usage": {"is_enabled": false}
        });
        let w = parse_claude_usage(&v);
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].label, w[0].used_percent), ("5-hour", 92.5));
        assert_eq!(w[0].resets_at, Some(t(14, 0)));
        assert_eq!((w[1].label, w[1].used_percent), ("weekly", 40.0));
        assert_eq!(
            w[1].resets_at,
            DateTime::<Utc>::from_timestamp(1793462400, 0)
        );
        let ms = parse_claude_usage(
            &json!({"five_hour": {"utilization": 120, "resets_at": 1793462400000_i64}}),
        );
        assert_eq!(ms[0].used_percent, 100.0, "clamped");
        assert_eq!(ms[0].resets_at, w[1].resets_at, "milliseconds too");
        assert!(parse_claude_usage(&json!({"five_hour": {}})).is_empty());
    }

    #[test]
    fn the_fullest_window_over_the_threshold_wins() {
        let w = |label, used| Window {
            label,
            used_percent: used,
            resets_at: None,
        };
        let windows = [w("5-hour", 91.0), w("weekly", 96.0)];
        assert_eq!(over_threshold(&windows, 90.0).unwrap().label, "weekly");
        assert_eq!(over_threshold(&windows, 95.0).unwrap().label, "weekly");
        assert!(over_threshold(&windows, 97.0).is_none());
    }

    #[test]
    fn a_pause_lasts_until_the_reset_or_an_hour() {
        let now = t(12, 0);
        let full = Window {
            label: "5-hour",
            used_percent: 93.0,
            resets_at: Some(t(14, 30)),
        };
        let p = pause_for("claude", &full, 90.0, now);
        assert_eq!(p.until, t(14, 30));
        assert_eq!(p.reason, "5-hour usage at 93% (pausing at 90%)");
        let stale = Window {
            resets_at: Some(t(11, 0)),
            ..full.clone()
        };
        assert_eq!(pause_for("claude", &stale, 90.0, now).until, t(13, 0));
    }

    #[test]
    fn a_pause_is_active_only_for_its_cli_and_until_it_ends() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("sub/quota_pause.json");
        assert!(active_pause(&path, "claude", t(12, 0)).is_none());
        let pause = Pause {
            cli: "claude".into(),
            until: t(14, 0),
            reason: "5-hour usage at 93%".into(),
        };
        write_pause(&path, &pause).unwrap();
        assert_eq!(active_pause(&path, "claude", t(12, 0)), Some(pause));
        assert!(active_pause(&path, "codex", t(12, 0)).is_none());
        assert!(active_pause(&path, "claude", t(14, 0)).is_none());
        std::fs::write(&path, "{broken").unwrap();
        assert!(active_pause(&path, "claude", t(12, 0)).is_none());
    }

    #[test]
    fn the_threshold_is_off_unless_it_is_one_of_the_choices() {
        let cfg = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(
            pause_threshold(&cfg(json!({"pause_above_percent": 90}))),
            Some(90.0)
        );
        assert_eq!(
            pause_threshold(&cfg(json!({"pause_above_percent": 42}))),
            None
        );
        assert_eq!(pause_threshold(&cfg(json!({"cli": "claude"}))), None);
    }

    #[test]
    fn reads_the_token_from_claude_codes_credentials() {
        let creds = r#"{"claudeAiOauth": {"accessToken": "sk-ant-oat-x", "refreshToken": "r"}}"#;
        assert_eq!(oauth_token(creds).as_deref(), Some("sk-ant-oat-x"));
        assert!(oauth_token(r#"{"claudeAiOauth": {"accessToken": ""}}"#).is_none());
        assert!(oauth_token("not json").is_none());
    }

    #[test]
    fn fetches_usage_with_the_oauth_headers() {
        let http = crate::connectors::testutil::FakeHttp::new(|_| {
            Ok(json!({"five_hour": {"utilization": 12, "resets_at": "2026-10-31T14:00:00Z"}}))
        });
        let w = fetch_claude_usage(http.as_ref(), "https://usage.test/x", "tok").unwrap();
        assert_eq!(w[0].used_percent, 12.0);
        let calls = http.calls.lock().unwrap();
        assert_eq!(
            (calls[0].method.as_str(), calls[0].url.as_str()),
            ("GET", "https://usage.test/x")
        );
        assert_eq!(calls[0].token.as_deref(), Some("tok"));
        assert!(calls[0]
            .headers
            .contains(&("anthropic-beta".into(), "oauth-2025-04-20".into())));
        drop(calls);
        let empty = crate::connectors::testutil::FakeHttp::new(|_| Ok(json!({})));
        assert!(fetch_claude_usage(empty.as_ref(), "https://usage.test/x", "tok").is_err());
    }

    #[test]
    fn spots_a_lapsed_sign_in_from_each_cli() {
        for out in [
            "Invalid API key · Please run /login",
            "OAuth token has expired. Please obtain a new token or refresh your existing token.",
            "Error: Not logged in. Run `codex login`.",
            "error: 401 Unauthorized: token_expired",
            "Please login with `kimi login`",
            "Authentication required. Run `cursor-agent login`.",
        ] {
            assert!(sign_in_lapsed(out), "{out}");
        }
        assert!(!sign_in_lapsed("test_login_page PASSED"));
        assert!(!sign_in_lapsed("All checks passed"));
    }

    #[test]
    fn spots_usage_limit_messages_from_each_cli() {
        for out in [
            "Claude AI usage limit reached|1793462400",
            "You've hit your usage limit. Upgrade or try again at 3pm.",
            "ERROR: You've hit your limit · resets 5pm",
            "{\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}",
            "[API Error: RESOURCE_EXHAUSTED]",
            "Error: You exceeded your current quota",
        ] {
            assert!(limit_hit(out), "{out}");
        }
        assert!(!limit_hit("test_rate_limiter.py::test_resets PASSED"));
        assert!(!limit_hit("All checks passed"));
    }
}
