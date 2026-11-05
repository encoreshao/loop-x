//! `loopx screenshot serve start|stop`: keep a project's dev server up across several
//! agent commands, so the Claude in Chrome tools can open its page and
//! `loopx screenshot --window` can save it. A detached holder process owns the
//! server (the allowed dev-server commands only, from `check_serve_command`) and
//! stops it on `serve stop`, or by itself after [`MAX_HOLD`].

use crate::screenshot_cmd::{already_served, parse_args};
use loopx_core::run_control::process_alive;
use loopx_core::screenshot::{clean_up_on_signals, DevServer};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// How long a held server may live without `serve stop`.
const MAX_HOLD: Duration = Duration::from_secs(15 * 60);

const USAGE: &str = "usage: loopx screenshot serve start --serve \"CMD\" --cwd DIR --url URL [--timeout-s N]\n       loopx screenshot serve stop";

/// The folder holding the holder's `pid`, `ready`, `error`, `stop` and `log` files.
fn state_dir() -> PathBuf {
    loopx_core::runner_util::repo_root()
        .join("outputs")
        .join("screens")
        .join(".server")
}

fn read_pid(dir: &Path) -> Option<i64> {
    std::fs::read_to_string(dir.join("pid"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The `--serve/--cwd/--url/--timeout-s` flags of `argv`, validated by the same
/// parser `loopx screenshot --url ...` uses (a throwaway `--out` satisfies it).
fn flags(argv: &[String]) -> Result<(Vec<String>, u64), String> {
    let mut full: Vec<String> = argv.to_vec();
    full.extend(["--out".into(), "unused.png".into()]);
    let parsed = parse_args(&full)?;
    if parsed.serve.is_none() {
        return Err("--serve is required".into());
    }
    Ok((argv.to_vec(), parsed.timeout_s))
}

pub fn run(argv: &[String]) -> ExitCode {
    let result = match argv.first().map(String::as_str) {
        Some("start") => start(&argv[1..]),
        Some("stop") => stop(),
        Some("run") => hold(&argv[1..]),
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(msg) => {
            if !msg.is_empty() {
                println!("{msg}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("loopx screenshot serve: {e}");
            ExitCode::from(1)
        }
    }
}

fn start(argv: &[String]) -> Result<String, String> {
    let (flags, timeout_s) = flags(argv)?;
    let dir = state_dir();
    if read_pid(&dir).is_some_and(process_alive) {
        return Err("a dev server is already held; run `loopx screenshot serve stop` first".into());
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let log = std::fs::File::create(dir.join("log")).map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["screenshot", "serve", "run"]).args(&flags);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own session: the agent's Bash timeout must not take the server with it.
        cmd.process_group(0);
    }
    let holder = cmd
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .spawn()
        .map_err(|e| format!("could not start the holder: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(timeout_s + 10);
    loop {
        if dir.join("ready").exists() {
            return Ok(format!(
                "dev server is up (held for at most {} min); open the page, then run `loopx screenshot --window --out FILE.png`, then `loopx screenshot serve stop`",
                MAX_HOLD.as_secs() / 60
            ));
        }
        if let Ok(e) = std::fs::read_to_string(dir.join("error")) {
            return Err(e.trim().to_string());
        }
        if !process_alive(i64::from(holder.id())) && !dir.join("ready").exists() {
            let tail = std::fs::read_to_string(dir.join("log")).unwrap_or_default();
            return Err(format!("the holder exited early: {}", tail.trim()));
        }
        if Instant::now() > deadline {
            return Err("timed out waiting for the dev server".into());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The detached holder: start the server, announce it, wait to be stopped.
fn hold(argv: &[String]) -> Result<String, String> {
    clean_up_on_signals();
    let dir = state_dir();
    let mut full: Vec<String> = argv.to_vec();
    full.extend(["--out".into(), "unused.png".into()]);
    let a = parse_args(&full)?;
    let fail = |e: String| {
        let _ = std::fs::write(dir.join("error"), &e);
        e
    };
    let Some((_, (env, cmd))) = a.serve.as_ref() else {
        return Err(fail("--serve is required".into()));
    };
    if already_served(&a.url) {
        return Err(fail(format!(
            "something is already serving {}; stop it or use another port",
            a.url
        )));
    }
    let server = DevServer::start(cmd, env, &a.cwd, &a.url, Duration::from_secs(a.timeout_s))
        .map_err(fail)?;
    std::fs::write(dir.join("pid"), std::process::id().to_string()).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("ready"), "").map_err(|e| e.to_string())?;
    let until = Instant::now() + MAX_HOLD;
    while !dir.join("stop").exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(500));
    }
    drop(server);
    let _ = std::fs::remove_file(dir.join("pid"));
    Ok(String::new())
}

fn stop() -> Result<String, String> {
    let dir = state_dir();
    let Some(pid) = read_pid(&dir) else {
        return Ok("no dev server is held".into());
    };
    let _ = std::fs::write(dir.join("stop"), "");
    let deadline = Instant::now() + Duration::from_secs(25);
    while process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
    }
    if process_alive(pid) {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok("dev server stopped".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(a: &[&str]) -> Vec<String> {
        a.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn start_flags_are_validated_like_a_screenshot() {
        let ok = s(&[
            "--serve",
            "yarn dev",
            "--cwd",
            ".",
            "--url",
            "http://localhost:3000/",
            "--timeout-s",
            "30",
        ]);
        assert_eq!(flags(&ok).unwrap().1, 30);
        // Only the allowed dev-server commands, only local pages, and a server is required.
        for bad in [
            s(&["--serve", "rm -rf /", "--url", "http://localhost:3000/"]),
            s(&["--serve", "yarn dev", "--url", "https://example.com/"]),
            s(&["--url", "http://localhost:3000/"]),
            s(&["--serve", "yarn dev"]),
        ] {
            assert!(flags(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn stop_with_nothing_held_is_not_an_error() {
        let root = tempfile::tempdir().unwrap();
        std::env::set_var("LOOPX_ROOT", root.path());
        assert_eq!(stop().unwrap(), "no dev server is held");
    }
}
