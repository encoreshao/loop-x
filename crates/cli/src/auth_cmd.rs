//! `loopx auth <google|linkedin> start|status|refresh|logout`: OAuth sign-ins from the
//! command line (the same flows `POST /api/oauth/{provider}/start` drives for the app).

use loopx_core::oauth::{self, OauthContext, Provider, StartRequest};
use loopx_server::lock;
use serde_json::{json, Value};
use std::process::ExitCode;

const DEFAULT_PORT: u16 = 8420;

const USAGE: &str = "Usage: loopx auth <google|linkedin> <command> [options]

Commands:
  start    [--account A] [--kind connector|inbox] [--scope S] [--login-hint H]
           [--app-scheme SCHEME] [--port N] [--open]
           Print {authorization_url, state, redirect_uri}; open the URL in a browser. The
           redirect lands on the running `loopx serve` (port: --port, else the live
           loopx.lock, else 8420).
  status   [--account A]        Connection state as JSON (never the tokens)
  refresh  [--account A] [--if-expired]
  logout   [--account A]        Forget the stored tokens";

pub fn run(args: &[String]) -> ExitCode {
    match dispatch(args) {
        Ok(text) => {
            if !text.is_empty() {
                println!("{text}");
            }
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
    }
}

fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn dispatch(args: &[String]) -> Result<String, String> {
    let provider = args
        .first()
        .and_then(|p| Provider::parse(p))
        .ok_or_else(|| USAGE.to_string())?;
    let cmd = args.get(1).map(String::as_str).unwrap_or("");
    let rest = &args[args.len().min(2)..];
    let ctx = OauthContext::from_env().map_err(|e| e.to_string())?;
    let account = opt(rest, "--account").unwrap_or(oauth::DEFAULT_ACCOUNT);
    let now = oauth::now_secs();
    match cmd {
        "start" => {
            let port = match opt(rest, "--port") {
                Some(p) => p
                    .parse::<u16>()
                    .map_err(|_| "--port needs a number".to_string())?,
                None => lock::read(&lock::default_path())
                    .filter(|l| lock::pid_alive(l.pid))
                    .map(|l| l.port)
                    .unwrap_or_else(|| {
                        eprintln!(
                            "note: no running `loopx serve` found; the sign-in can only finish \
                             once one listens on port {DEFAULT_PORT} (or pass --port)"
                        );
                        DEFAULT_PORT
                    }),
            };
            let req = StartRequest {
                account: account.to_string(),
                kind: opt(rest, "--kind").unwrap_or("").to_string(),
                scope: opt(rest, "--scope").map(str::to_string),
                login_hint: opt(rest, "--login-hint").map(str::to_string),
                app_scheme: opt(rest, "--app-scheme").map(str::to_string),
            };
            let s = oauth::start(&ctx, provider, &req, &format!("127.0.0.1:{port}"), now)
                .map_err(|e| e.to_string())?;
            if flag(rest, "--open") {
                let _ = std::process::Command::new("open")
                    .arg(&s.authorization_url)
                    .status();
            }
            Ok(json!({
                "authorization_url": s.authorization_url,
                "state": s.state,
                "redirect_uri": s.redirect_uri,
            })
            .to_string())
        }
        "status" => {
            let one = |a: &str| -> Result<Value, String> {
                let st = oauth::status(&ctx, provider, a, now).map_err(|e| e.to_string())?;
                Ok(json!({
                    "account": st.account, "connected": st.connected,
                    "has_refresh_token": st.has_refresh_token,
                    "expires_at": st.expires_at, "expired": st.expired,
                }))
            };
            let doc = match opt(rest, "--account") {
                Some(a) => one(a)?,
                None => {
                    let list = oauth::accounts(&ctx, provider).map_err(|e| e.to_string())?;
                    let items = list.iter().map(|a| one(a)).collect::<Result<Vec<_>, _>>()?;
                    json!({"provider": provider.name(), "accounts": items})
                }
            };
            Ok(doc.to_string())
        }
        "refresh" => {
            let st = oauth::status(&ctx, provider, account, now).map_err(|e| e.to_string())?;
            if !st.connected {
                return Err(format!(
                    "{} account {account:?} is not connected",
                    provider.name()
                ));
            }
            let refreshed = if flag(rest, "--if-expired") && !st.expired {
                false
            } else {
                oauth::refresh(&ctx, provider, account, now).map_err(|e| e.to_string())?;
                true
            };
            let st = oauth::status(&ctx, provider, account, now).map_err(|e| e.to_string())?;
            Ok(
                json!({"account": account, "refreshed": refreshed, "expires_at": st.expires_at})
                    .to_string(),
            )
        }
        "logout" => {
            oauth::logout(&ctx, provider, account).map_err(|e| e.to_string())?;
            Ok(json!({"account": account, "connected": false}).to_string())
        }
        _ => Err(USAGE.to_string()),
    }
}
