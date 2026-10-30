//! `loopx screenshot`: headless capture of a page, optionally starting its
//! dev server first (see `loopx_core::screenshot`).

use loopx_core::screenshot::{
    capture, check_serve_command, DevServer, MockRule, ServeCommand, Session, ShotOptions,
};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "usage: loopx screenshot --url URL --out FILE.png [--serve \"CMD\"] [--cwd DIR] [--selector CSS] [--full-page] [--viewport WxH] [--mock FILE] [--session FILE] [--wait-for CSS] [--timeout-s N] (a capture can run somewhat past --timeout-s: each browser command has its own 60 s limit)";

#[derive(Debug)]
struct Args {
    url: String,
    out: PathBuf,
    serve: Option<(String, ServeCommand)>,
    cwd: PathBuf,
    selector: Option<String>,
    full_page: bool,
    viewport: (u32, u32),
    mock: Option<PathBuf>,
    session: Option<PathBuf>,
    wait_for: Option<String>,
    timeout_s: u64,
}

fn parse_viewport(raw: &str) -> Result<(u32, u32), String> {
    let bad = || format!("--viewport must look like 1280x800, got {raw:?}");
    let (w, h) = raw.split_once('x').ok_or_else(bad)?;
    let w: u32 = w.parse().map_err(|_| bad())?;
    let h: u32 = h.parse().map_err(|_| bad())?;
    if w == 0 || h == 0 {
        return Err(bad());
    }
    Ok((w, h))
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut a = Args {
        url: String::new(),
        out: PathBuf::new(),
        serve: None,
        cwd: PathBuf::from("."),
        selector: None,
        full_page: false,
        viewport: (1280, 800),
        mock: None,
        session: None,
        wait_for: None,
        timeout_s: 120,
    };
    let (mut url, mut out) = (None, None);
    let mut it = argv.iter();
    while let Some(flag) = it.next() {
        if flag == "--full-page" {
            a.full_page = true;
            continue;
        }
        let known = [
            "--url",
            "--out",
            "--serve",
            "--cwd",
            "--selector",
            "--viewport",
            "--mock",
            "--session",
            "--wait-for",
            "--timeout-s",
        ];
        if !known.contains(&flag.as_str()) {
            return Err(format!("unknown argument {flag:?}"));
        }
        let v = it
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))?
            .clone();
        match flag.as_str() {
            "--url" => url = Some(v),
            "--out" => out = Some(v),
            "--serve" => a.serve = Some((v.clone(), check_serve_command(&v)?)),
            "--cwd" => a.cwd = PathBuf::from(v),
            "--selector" => a.selector = Some(v),
            "--viewport" => a.viewport = parse_viewport(&v)?,
            "--mock" => a.mock = Some(PathBuf::from(v)),
            "--session" => a.session = Some(PathBuf::from(v)),
            "--wait-for" => a.wait_for = Some(v),
            _ => {
                a.timeout_s =
                    v.parse().ok().filter(|n| *n > 0).ok_or_else(|| {
                        format!("--timeout-s must be a positive integer, got {v:?}")
                    })?
            }
        }
    }
    a.url = url.ok_or("--url is required")?;
    if !(a.url.starts_with("http://") || a.url.starts_with("https://")) {
        return Err(format!("--url must be an http(s) URL, got {:?}", a.url));
    }
    let out = out.ok_or("--out is required")?;
    if !out.to_ascii_lowercase().ends_with(".png") {
        return Err(format!("--out must end in .png, got {out:?}"));
    }
    a.out = PathBuf::from(out);
    Ok(a)
}

/// host:port of an http(s) URL, for the "already served?" probe.
fn host_port(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let auth = rest.split(['/', '?', '#']).next()?;
    let auth = auth.rsplit('@').next()?;
    if auth.is_empty() {
        return None;
    }
    if auth
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        return Some(auth.to_string());
    }
    Some(format!(
        "{auth}:{}",
        if scheme == "https" { 443 } else { 80 }
    ))
}

/// True when something already accepts connections at the URL's host:port.
fn already_served(url: &str) -> bool {
    let Some(addr) = host_port(url) else {
        return false;
    };
    let Ok(addrs) = addr.to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|a| TcpStream::connect_timeout(&a, Duration::from_secs(1)).is_ok())
}

fn read_json<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
    what: &str,
) -> Result<T, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {what} file {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("invalid {what} file {}: {e}", path.display()))
}

fn execute(a: &Args) -> Result<(), String> {
    let timeout = Duration::from_secs(a.timeout_s);
    let mut opts = ShotOptions::new(&a.url, &a.out);
    opts.selector = a.selector.clone();
    opts.full_page = a.full_page;
    opts.viewport = a.viewport;
    opts.wait_for = a.wait_for.clone();
    opts.timeout = timeout;
    if let Some(p) = &a.mock {
        opts.mocks = read_json::<Vec<MockRule>>(p, "mock")?;
    }
    if let Some(p) = &a.session {
        opts.session = read_json::<Session>(p, "session")?;
    }
    // Probe before anything spawns: a stale server would be shot instead of
    // this worktree's code.
    let _server = match &a.serve {
        Some((_, (env, argv))) => {
            if already_served(&a.url) {
                return Err(format!(
                    "something is already serving {}; stop it or use another port",
                    a.url
                ));
            }
            Some(DevServer::start(argv, env, &a.cwd, &a.url, timeout)?)
        }
        None => None,
    };
    if let Some(parent) = a.out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    capture(&opts)
}

pub fn run(argv: &[String]) -> ExitCode {
    let args = match parse_args(argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("loopx screenshot: {e}\n{USAGE}");
            return ExitCode::from(1);
        }
    };
    match execute(&args) {
        Ok(()) => {
            println!("{}", args.out.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("loopx screenshot: {}", e.lines().next().unwrap_or("failed"));
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn args_parse_with_defaults() {
        let a = parse_args(&v(&[
            "--url",
            "http://127.0.0.1:3000/x",
            "--out",
            "a/b.png",
        ]))
        .unwrap();
        assert_eq!(a.url, "http://127.0.0.1:3000/x");
        assert_eq!(a.out, PathBuf::from("a/b.png"));
        assert_eq!(a.cwd, PathBuf::from("."));
        assert_eq!(a.viewport, (1280, 800));
        assert_eq!(a.timeout_s, 120);
        assert!(a.serve.is_none() && !a.full_page && a.selector.is_none());
        let a = parse_args(&v(&[
            "--url",
            "http://h/",
            "--out",
            "o.png",
            "--serve",
            "yarn dev -p 3001",
            "--viewport",
            "800x600",
            "--full-page",
            "--selector",
            "#a",
            "--timeout-s",
            "5",
        ]))
        .unwrap();
        assert_eq!(a.viewport, (800, 600));
        assert!(a.full_page);
        assert_eq!(a.timeout_s, 5);
        assert_eq!(a.serve.unwrap().1 .1, v(&["yarn", "dev", "-p", "3001"]));
    }

    #[test]
    fn bad_args_are_refused() {
        let ok = ["--url", "http://h/", "--out", "o.png"];
        assert!(parse_args(&v(&["--out", "o.png"])).is_err());
        assert!(parse_args(&v(&["--url", "http://h/"])).is_err());
        assert!(parse_args(&v(&["--url", "http://h/", "--out", "x.jpg"])).is_err());
        let mut a = ok.to_vec();
        a.extend(["--viewport", "12"]);
        assert!(parse_args(&v(&a)).is_err());
        let mut a = ok.to_vec();
        a.extend(["--serve", "node s.js"]);
        assert!(parse_args(&v(&a)).is_err());
        let mut a = ok.to_vec();
        a.extend(["--bogus", "1"]);
        assert!(parse_args(&v(&a)).is_err());
    }

    #[test]
    fn serve_refuses_when_something_already_answers() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/", l.local_addr().unwrap().port());
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("sub/o.png");
        let a = parse_args(&v(&[
            "--url",
            &url,
            "--out",
            out.to_str().unwrap(),
            "--serve",
            "yarn dev",
            "--cwd",
            dir.path().to_str().unwrap(),
        ]))
        .unwrap();
        let err = execute(&a).unwrap_err();
        assert!(err.contains("already serving"), "{err}");
        // Nothing ran: not even the output directory was made.
        assert!(!out.parent().unwrap().exists());
    }

    #[test]
    fn host_port_defaults_by_scheme() {
        assert_eq!(host_port("http://h/x").as_deref(), Some("h:80"));
        assert_eq!(host_port("https://h").as_deref(), Some("h:443"));
        assert_eq!(host_port("http://h:3000/x?y").as_deref(), Some("h:3000"));
    }
}
