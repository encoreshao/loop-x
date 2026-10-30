//! `loopx screenshot`: start the project's dev server, wait for it, and clean
//! it up. Only a short allowlist of dev-server commands may be started, since
//! the command comes from an agent handoff.
//!
//! [`capture`] then drives the Chrome already installed on this Mac, headless
//! on a throwaway profile, over `linkedin_cdp`. Every request the page makes
//! is paused: a matching [`MockRule`] answers it with canned JSON, a
//! same-origin one goes through, and anything else fails without being sent.
//! A fake signed-in [`Session`] (cookies and localStorage) is set up before
//! the page loads. The profile dir is removed on every exit path.

use crate::linkedin_cdp::{box_script, CdpPage, Chrome, LaunchOptions};
use crate::linkedin_humanize::Selector;
use crate::loop_plugins::linkedin_autopilot::BrowserError;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Dev-server commands the capture may start: the command must equal one of
/// these or continue with a space-separated argument (`-p 3001`).
const SERVE_COMMANDS: &[&str] = &[
    "yarn dev",
    "pnpm dev",
    "npm run dev",
    "next dev",
    "RAILS_ENV=test bin/rails server",
];

/// Environment pairs and argv of an allowed dev-server command.
pub type ServeCommand = (Vec<(String, String)>, Vec<String>);

/// Splits an allowed dev-server command into its environment pairs and argv,
/// or says why it is refused. It runs without a shell, so operators are
/// refused, and only `RAILS_ENV=test` is accepted as an environment prefix.
pub fn check_serve_command(cmd: &str) -> Result<ServeCommand, String> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return Err("empty command".into());
    }
    crate::verify_commands::refuse_shell_operators(cmd)?;
    let allowed = SERVE_COMMANDS.iter().any(|p| match cmd.strip_prefix(p) {
        Some(tail) => tail.is_empty() || tail.starts_with(' '),
        None => false,
    });
    if !allowed {
        return Err(format!("{cmd:?} is not an allowed dev-server command"));
    }
    let mut env = Vec::new();
    let mut argv = Vec::new();
    for word in cmd.split_whitespace() {
        match word.split_once('=') {
            Some((k, v)) if argv.is_empty() => env.push((k.to_string(), v.to_string())),
            _ => argv.push(word.to_string()),
        }
    }
    Ok((env, argv))
}

/// A running dev server in its own process group. Dropping it stops the whole
/// group, so a `yarn dev` that forked node workers leaves nothing behind.
pub struct DevServer {
    child: Child,
}

impl DevServer {
    /// Spawns `argv` in `cwd` and polls `url` every 250 ms until any HTTP
    /// status answers. The group is already killed when this returns `Err`.
    pub fn start(
        argv: &[String],
        env: &[(String, String)],
        cwd: &Path,
        url: &str,
        timeout: Duration,
    ) -> Result<DevServer, String> {
        let (prog, args) = argv.split_first().ok_or("empty command")?;
        let child = Command::new(prog)
            .args(args)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .current_dir(cwd)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not start {prog}: {e}"))?;
        // From here Drop owns cleanup, including on the error returns below.
        let mut server = DevServer { child };
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(5))
            .build();
        let deadline = Instant::now() + timeout;
        loop {
            match server.child.try_wait() {
                Ok(Some(status)) => {
                    return Err(format!(
                        "dev server exited early ({status}) before {url} answered"
                    ))
                }
                Ok(None) => {}
                Err(e) => return Err(format!("could not poll the dev server: {e}")),
            }
            match agent.get(url).call() {
                Ok(_) | Err(ureq::Error::Status(..)) => return Ok(server),
                Err(ureq::Error::Transport(_)) => {}
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "dev server did not answer {url} within {}s",
                    timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

impl Drop for DevServer {
    fn drop(&mut self) {
        let pgid = self.child.id() as i32;
        // SAFETY: signalling the group this struct created; no memory involved.
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Leftover group members (workers that ignored SIGTERM) die here even
        // when the leader already exited.
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

// ---- headless capture ----------------------------------------------------------------

/// One canned backend answer. `url_pattern` is a substring of the request
/// URL, or a glob over the whole URL when it contains `*`.
#[derive(Debug, Clone, Deserialize)]
pub struct MockRule {
    pub url_pattern: String,
    #[serde(default = "default_status")]
    pub status: u16,
    pub json: Value,
}

fn default_status() -> u16 {
    200
}

/// A fake signed-in session. Each cookie is a CDP `Network.setCookie` params
/// object; one with neither `url` nor `domain` is set for the page URL.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Session {
    pub cookies: Vec<Value>,
    #[serde(alias = "localStorage")]
    pub local_storage: BTreeMap<String, String>,
}

/// What to do with one paused request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Continue,
    /// Answer with the mock at this index.
    Fulfill(usize),
    Block,
}

/// Decides a request: a matching mock always wins, then same-origin, `data:`
/// and `blob:` requests go through, and everything else is blocked.
pub fn decide(request_url: &str, page_origin: &str, mocks: &[MockRule]) -> Decision {
    let hit = |m: &MockRule| {
        let p = m.url_pattern.as_str();
        !p.is_empty()
            && if p.contains('*') {
                glob_match(p, request_url)
            } else {
                request_url.contains(p)
            }
    };
    if let Some(i) = mocks.iter().position(hit) {
        return Decision::Fulfill(i);
    }
    if request_url.starts_with("data:") || request_url.starts_with("blob:") {
        return Decision::Continue;
    }
    match origin_of(request_url) {
        Some(o) if o == page_origin => Decision::Continue,
        _ => Decision::Block,
    }
}

/// `*`-only glob over the whole of `text`.
fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            // Let the last `*` swallow one more character and retry.
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

/// `scheme://host[:port]` of an http(s) URL, lowercased and without a
/// default port, the way `location.origin` spells it.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let mut host = rest[..end].to_ascii_lowercase();
    if let Some(at) = host.rfind('@') {
        host = host[at + 1..].to_string();
    }
    let default = if scheme == "http" { ":80" } else { ":443" };
    if let Some(h) = host.strip_suffix(default) {
        host = h.to_string();
    }
    (!host.is_empty()).then(|| format!("{scheme}://{host}"))
}

/// What `capture` shoots: a `selector` gives that element, `full_page` the
/// whole document, neither the viewport.
pub struct ShotOptions {
    pub url: String,
    pub out: PathBuf,
    pub selector: Option<String>,
    pub full_page: bool,
    pub viewport: (u32, u32),
    pub wait_for: Option<String>,
    pub mocks: Vec<MockRule>,
    pub session: Session,
    /// The whole capture, from launch to PNG.
    pub timeout: Duration,
    /// An explicit Chrome executable; otherwise `find` looks for one.
    pub chrome: Option<PathBuf>,
    pub find: fn() -> Option<PathBuf>,
    /// Where the throwaway profile dir is made (the system temp dir if unset).
    pub profile_parent: Option<PathBuf>,
}

impl ShotOptions {
    /// A 1280x800 viewport shot of `url`, no mocks, no session, 60 s budget.
    pub fn new(url: impl Into<String>, out: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            out: out.into(),
            selector: None,
            full_page: false,
            viewport: (1280, 800),
            wait_for: None,
            mocks: Vec::new(),
            session: Session::default(),
            timeout: Duration::from_secs(60),
            chrome: None,
            find: crate::linkedin_cdp::find_chrome,
            profile_parent: None,
        }
    }
}

/// Loads `opts.url` in headless Chrome and writes the PNG to `opts.out`.
/// Chrome is only ever the one on this machine; nothing fetches a browser.
pub fn capture(opts: &ShotOptions) -> Result<(), String> {
    let exe = match &opts.chrome {
        Some(p) if p.is_file() => p.clone(),
        Some(p) => return Err(format!("Google Chrome was not found at {}", p.display())),
        None => (opts.find)().ok_or(
            "Google Chrome was not found: install it in /Applications or set LOOPX_CHROME",
        )?,
    };
    let origin =
        origin_of(&opts.url).ok_or_else(|| format!("{:?} is not an http(s) URL", opts.url))?;
    let parent = opts
        .profile_parent
        .clone()
        .unwrap_or_else(std::env::temp_dir);
    let profile = parent.join(format!("loopx-shot-{}", uuid::Uuid::new_v4().simple()));
    std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
        .create(&profile)
        .map_err(|e| format!("could not make a Chrome profile dir: {e}"))?;
    // Chrome is closed inside `shoot`, before its profile goes.
    let result = shoot(&exe, &profile, &origin, opts);
    let removed = remove_profile(&profile);
    match (result, removed) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
        (Err(e), Err(r)) => Err(format!("{e}; {r}")),
    }
}

/// Removes the profile, retrying while Chrome's helpers finish writing to it.
fn remove_profile(path: &Path) -> Result<(), String> {
    let mut last = None;
    for attempt in 0..10 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(200));
        }
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(format!(
        "could not remove the Chrome profile {}: {}",
        path.display(),
        last.map(|e| e.to_string()).unwrap_or_default()
    ))
}

/// Flags that keep a throwaway headless Chrome from phoning home or touching
/// the login Keychain; the page's own requests are handled by `decide`.
const QUIET_ARGS: &[&str] = &[
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-default-apps",
    "--disable-domain-reliability",
    "--disable-extensions",
    "--disable-sync",
    "--disable-client-side-phishing-detection",
    "--disable-breakpad",
    "--metrics-recording-only",
    "--no-pings",
    "--mute-audio",
    "--hide-scrollbars",
    "--use-mock-keychain",
    "--password-store=basic",
];

fn browser_error(e: BrowserError) -> String {
    match e {
        BrowserError::Unavailable(m) => m,
        BrowserError::PageTimeout => "Chrome did not answer in time".into(),
        BrowserError::Other { detail, .. } => detail,
        other => format!("{other:?}"),
    }
}

/// One capture on a launched Chrome; dropping it closes the browser.
struct Shot<'a> {
    chrome: Chrome,
    page: CdpPage,
    origin: &'a str,
    mocks: &'a [MockRule],
    deadline: Instant,
    inflight: HashSet<String>,
    last_network: Instant,
    navigated: bool,
    loaded: bool,
}

fn shoot(exe: &Path, profile: &Path, origin: &str, opts: &ShotOptions) -> Result<(), String> {
    let launch = LaunchOptions {
        chrome_path: Some(exe.to_path_buf()),
        extra_args: QUIET_ARGS.iter().map(|a| a.to_string()).collect(),
    };
    let deadline = Instant::now() + opts.timeout;
    let chrome = Chrome::launch(profile, true, &launch).map_err(browser_error)?;
    let page = chrome.page().map_err(browser_error)?;
    let mut shot = Shot {
        chrome,
        page,
        origin,
        mocks: &opts.mocks,
        deadline,
        inflight: HashSet::new(),
        last_network: Instant::now(),
        navigated: false,
        loaded: false,
    };
    let result = shot.run(opts);
    shot.chrome.close();
    result
}

impl Shot<'_> {
    fn send(&self, method: &str, params: Value) -> Result<Value, String> {
        self.page
            .send(method, params)
            .map_err(|e| format!("{method}: {}", browser_error(e)))
    }

    fn eval(&self, expression: &str) -> Result<Value, String> {
        let r = self.send(
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true}),
        )?;
        if let Some(ex) = r.get("exceptionDetails") {
            return Err(format!("page script failed: {}", ex["text"]));
        }
        Ok(r["result"]["value"].clone())
    }

    fn run(&mut self, opts: &ShotOptions) -> Result<(), String> {
        let (width, height) = opts.viewport;
        self.send(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": width, "height": height, "deviceScaleFactor": 1, "mobile": false}),
        )?;
        self.send("Network.enable", json!({}))?;
        for cookie in &opts.session.cookies {
            let mut cookie = cookie.clone();
            let Some(fields) = cookie.as_object_mut() else {
                return Err(format!("a session cookie is not an object: {cookie}"));
            };
            if !fields.contains_key("url") && !fields.contains_key("domain") {
                fields.insert("url".into(), json!(opts.url));
            }
            let r = self.send("Network.setCookie", cookie.clone())?;
            if r["success"] == json!(false) {
                return Err(format!("Chrome refused the session cookie {cookie}"));
            }
        }
        if !opts.session.local_storage.is_empty() {
            // Runs before the page's own scripts, on the page's origin only.
            let source = format!(
                "(() => {{ if (location.origin !== {origin}) return; \
                 try {{ const e = {entries}; for (const k in e) localStorage.setItem(k, e[k]); }} \
                 catch (_) {{}} }})();",
                origin = json!(self.origin),
                entries = json!(opts.session.local_storage),
            );
            self.send(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": source}),
            )?;
        }
        self.send("Fetch.enable", json!({"patterns": [{"urlPattern": "*"}]}))?;
        self.lock_cdp().events.clear();
        // `Page.navigate` only answers once the paused document request is
        // let through, so navigate from the page and answer requests meanwhile.
        self.eval(&format!("location.href = {}; true", json!(opts.url)))?;
        while !self.loaded {
            self.pump(Duration::from_millis(100))?;
            if Instant::now() >= self.deadline {
                return Err(format!(
                    "{} did not load within {}s",
                    opts.url,
                    opts.timeout.as_secs()
                ));
            }
        }
        let href = self.eval("location.href")?;
        if href
            .as_str()
            .unwrap_or_default()
            .starts_with("chrome-error:")
        {
            return Err(format!("could not load {}", opts.url));
        }
        if let Some(css) = &opts.wait_for {
            let probe = format!("!!document.querySelector({})", json!(css));
            let until = (Instant::now() + Duration::from_secs(15)).min(self.deadline);
            while self.eval(&probe)? != json!(true) {
                if Instant::now() >= until {
                    return Err(format!("{css:?} never appeared on {}", opts.url));
                }
                self.pump(Duration::from_millis(250))?;
            }
        }
        // 500 ms with nothing in flight, but never more than 10 s of waiting.
        let until = (Instant::now() + Duration::from_secs(10)).min(self.deadline);
        while Instant::now() < until
            && !(self.inflight.is_empty()
                && self.last_network.elapsed() >= Duration::from_millis(500))
        {
            self.pump(Duration::from_millis(100))?;
        }
        let png = self.screenshot(opts)?;
        if let Some(dir) = opts.out.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&opts.out, png).map_err(|e| format!("{}: {e}", opts.out.display()))
    }

    fn lock_cdp(&self) -> std::sync::MutexGuard<'_, crate::linkedin_cdp::Cdp> {
        self.chrome.cdp.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Handles every event that arrives within `wait`.
    fn pump(&mut self, wait: Duration) -> Result<(), String> {
        let until = Instant::now() + wait;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let event = self
                .lock_cdp()
                .next_event(left)
                .map_err(|e| format!("Chrome went away: {e}"))?;
            match event {
                Some(event) => self.handle(&event)?,
                None => return Ok(()),
            }
            if Instant::now() >= until {
                return Ok(());
            }
        }
    }

    fn handle(&mut self, event: &Value) -> Result<(), String> {
        let p = &event["params"];
        let request_id = || p["requestId"].as_str().unwrap_or_default().to_string();
        match event["method"].as_str().unwrap_or_default() {
            "Fetch.requestPaused" => {
                self.last_network = Instant::now();
                self.answer(p);
            }
            "Network.requestWillBeSent" => {
                self.last_network = Instant::now();
                self.inflight.insert(request_id());
            }
            "Network.loadingFinished" | "Network.loadingFailed" => {
                self.last_network = Instant::now();
                self.inflight.remove(&request_id());
            }
            "Page.frameNavigated" if p["frame"]["parentId"].is_null() => {
                if let Some(url) = p["frame"]["unreachableUrl"].as_str() {
                    return Err(format!("could not load {url}"));
                }
                self.navigated |= p["frame"]["url"] != json!("about:blank");
            }
            "Page.loadEventFired" if self.navigated => self.loaded = true,
            "Inspector.detached" | "Inspector.targetCrashed" => {
                return Err("the page crashed".into())
            }
            _ => {}
        }
        Ok(())
    }

    /// Answers one paused request. A request the page already dropped makes
    /// Chrome refuse the answer, which is fine to ignore.
    fn answer(&self, p: &Value) {
        let id = &p["requestId"];
        let url = p["request"]["url"].as_str().unwrap_or_default();
        let _ = match decide(url, self.origin, self.mocks) {
            Decision::Continue => self.send("Fetch.continueRequest", json!({"requestId": id})),
            Decision::Block => self.send(
                "Fetch.failRequest",
                json!({"requestId": id, "errorReason": "BlockedByClient"}),
            ),
            Decision::Fulfill(i) => {
                let mock = &self.mocks[i];
                // The page may call its API on another origin, so the canned
                // answer carries CORS headers, and a preflight gets a bare 204.
                let asked = |name: &str| {
                    p["request"]["headers"]
                        .as_object()
                        .and_then(|h| h.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)))
                        .and_then(|(_, v)| v.as_str())
                        .unwrap_or("*")
                        .to_string()
                };
                let mut headers = vec![
                    json!({"name": "Access-Control-Allow-Origin", "value": self.origin}),
                    json!({"name": "Access-Control-Allow-Credentials", "value": "true"}),
                    json!({"name": "Access-Control-Allow-Methods", "value": asked("Access-Control-Request-Method")}),
                    json!({"name": "Access-Control-Allow-Headers", "value": asked("Access-Control-Request-Headers")}),
                    json!({"name": "Cache-Control", "value": "no-store"}),
                ];
                let (status, body) = if p["request"]["method"] == json!("OPTIONS") {
                    (204, String::new())
                } else {
                    headers.push(json!({"name": "Content-Type", "value": "application/json"}));
                    (mock.status, B64.encode(mock.json.to_string()))
                };
                self.send(
                    "Fetch.fulfillRequest",
                    json!({"requestId": id, "responseCode": status, "responseHeaders": headers, "body": body}),
                )
            }
        };
    }

    fn screenshot(&mut self, opts: &ShotOptions) -> Result<Vec<u8>, String> {
        let (_, vh) = opts.viewport;
        let params = if let Some(css) = &opts.selector {
            let script = format!(
                "(() => {{ const b = ({})(); return b && {{x: b.x + scrollX, y: b.y + scrollY, \
                 width: b.width, height: b.height}}; }})()",
                box_script(&Selector::Css(css.clone()))
            );
            let b = self.eval(&script)?;
            if !b.is_object() {
                return Err(format!("no visible element matches {css:?}"));
            }
            let n = |k: &str| b[k].as_f64().unwrap_or(0.0);
            json!({
                "format": "png",
                "captureBeyondViewport": n("height") > vh as f64,
                "clip": {"x": n("x"), "y": n("y"), "width": n("width").ceil(),
                         "height": n("height").ceil(), "scale": 1},
            })
        } else if opts.full_page {
            let size = self.eval(
                "({w: document.documentElement.scrollWidth, h: document.documentElement.scrollHeight})",
            )?;
            json!({
                "format": "png",
                "captureBeyondViewport": true,
                "clip": {"x": 0, "y": 0, "width": size["w"], "height": size["h"], "scale": 1},
            })
        } else {
            json!({"format": "png"})
        };
        let r = self.send("Page.captureScreenshot", params)?;
        B64.decode(r["data"].as_str().unwrap_or_default())
            .map_err(|e| format!("Chrome sent a bad screenshot: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Every pid listed in `file` (written by the child) is dead within 5 s.
    fn assert_gone(file: &Path, expected: usize) {
        let pids: Vec<i32> = std::fs::read_to_string(file)
            .unwrap()
            .lines()
            .map(|l| l.trim().parse().unwrap())
            .collect();
        assert_eq!(pids.len(), expected, "{pids:?}");
        let deadline = Instant::now() + Duration::from_secs(5);
        while pids.iter().any(|p| alive(*p)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        for p in pids {
            assert!(!alive(p), "pid {p} still running");
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn serve_allowlist_accepts_only_known_dev_servers() {
        for c in [
            "yarn dev",
            "npm run dev -- -p 3001",
            "next dev -p 3001",
            "RAILS_ENV=test bin/rails server -p 3100",
        ] {
            assert!(check_serve_command(c).is_ok(), "{c}");
        }
        for c in [
            "rails server",
            "yarn devx",
            "npm run dev; rm -rf /",
            "node server.js",
            "RAILS_ENV=development bin/rails server",
            "",
        ] {
            assert!(check_serve_command(c).is_err(), "{c}");
        }
        let (env, argv) = check_serve_command("RAILS_ENV=test bin/rails server -p 3100").unwrap();
        assert_eq!(env, vec![("RAILS_ENV".to_string(), "test".to_string())]);
        assert_eq!(argv, ["bin/rails", "server", "-p", "3100"]);
    }

    #[test]
    fn dev_server_waits_for_the_url_then_kills_its_group() {
        let port = free_port();
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("child.pid");
        let script = format!(
            "import http.server,subprocess,sys\n\
             c=subprocess.Popen(['sleep','60'])\n\
             open({:?},'w').write(str(c.pid))\n\
             http.server.HTTPServer(('127.0.0.1',{port}),http.server.SimpleHTTPRequestHandler).serve_forever()\n",
            pidfile.to_str().unwrap()
        );
        let argv = vec!["python3".to_string(), "-c".to_string(), script];
        let server = DevServer::start(
            &argv,
            &[],
            dir.path(),
            &format!("http://127.0.0.1:{port}/"),
            Duration::from_secs(20),
        )
        .unwrap();
        let parent = server.child.id() as i32;
        let grandchild: i32 = std::fs::read_to_string(&pidfile).unwrap().parse().unwrap();
        assert!(alive(parent) && alive(grandchild));
        drop(server);
        let deadline = Instant::now() + Duration::from_secs(5);
        while (alive(parent) || alive(grandchild)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(parent), "server still running");
        assert!(!alive(grandchild), "its child still running");
    }

    #[test]
    fn dev_server_that_exits_or_never_answers_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("http://127.0.0.1:{}/", free_port());
        let started = Instant::now();
        // The leader exits but leaves a backgrounded child behind.
        let pids = dir.path().join("exit.pid");
        let script = format!("sleep 30 & echo $! > {}; exit 3", pids.display());
        let argv = vec!["sh".to_string(), "-c".into(), script];
        let err = DevServer::start(&argv, &[], dir.path(), &url, Duration::from_secs(20))
            .err()
            .unwrap();
        assert!(err.contains("exited"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_gone(&pids, 1);

        // Never answers: the leader and its child are both gone once Err returns.
        let pids = dir.path().join("hang.pid");
        let script = format!(
            "echo $$ > {p}; sleep 30 & echo $! >> {p}; wait",
            p = pids.display()
        );
        let argv = vec!["sh".to_string(), "-c".into(), script];
        let err = DevServer::start(&argv, &[], dir.path(), &url, Duration::from_secs(1))
            .err()
            .unwrap();
        assert!(err.contains("did not answer"), "{err}");
        assert_gone(&pids, 2);
    }
    fn mock(pattern: &str) -> MockRule {
        MockRule {
            url_pattern: pattern.into(),
            status: 200,
            json: serde_json::json!({}),
        }
    }

    #[test]
    fn mock_wins_then_same_origin_continues_else_blocked() {
        let page = "http://localhost:3001";
        let mocks = [mock("/api/me"), mock("*/api/companies/*")];
        assert_eq!(
            decide("http://localhost:3001/_next/x.js", page, &mocks),
            Decision::Continue
        );
        assert_eq!(
            decide("http://localhost:3000/api/me", page, &mocks),
            Decision::Fulfill(0)
        );
        assert_eq!(
            decide("http://localhost:3000/api/companies", page, &mocks),
            Decision::Block
        );
        assert_eq!(
            decide("https://www.google-analytics.com/x", page, &mocks),
            Decision::Block
        );
        assert_eq!(
            decide("http://localhost:3000/api/companies/9", page, &mocks),
            Decision::Fulfill(1)
        );
        // A mock wins even on the page's own origin.
        assert_eq!(
            decide("http://localhost:3001/api/me", page, &mocks),
            Decision::Fulfill(0)
        );
        assert_eq!(
            decide("data:image/png;base64,AA", page, &[]),
            Decision::Continue
        );
        assert_eq!(
            decide("blob:http://localhost:3001/1-2", page, &[]),
            Decision::Continue
        );
        // Another port or host spelling is another origin.
        assert_eq!(
            decide("http://127.0.0.1:3001/x.js", page, &[]),
            Decision::Block
        );
        assert_eq!(
            decide("http://localhost:3001.evil.test/x", page, &[]),
            Decision::Block
        );
        // A glob is anchored to the whole URL.
        let anchored = [mock("http://localhost:3000/api/*")];
        assert_eq!(
            decide(
                "http://evil.test/?http://localhost:3000/api/x",
                page,
                &anchored
            ),
            Decision::Block
        );
    }

    #[test]
    fn mock_and_session_files_parse() {
        let mocks: Vec<MockRule> = serde_json::from_str(
            r#"[{"url_pattern": "/api/me", "json": {"name": "Test User"}},
                {"url_pattern": "*/api/companies/*", "status": 404, "json": null}]"#,
        )
        .unwrap();
        assert_eq!(mocks[0].status, 200);
        assert_eq!(mocks[0].json["name"], "Test User");
        assert_eq!(mocks[1].status, 404);
        let session: Session = serde_json::from_str(
            r#"{"cookies": [{"name": "sid", "value": "abc", "domain": "localhost"}],
                "local_storage": {"token": "fake"}}"#,
        )
        .unwrap();
        assert_eq!(session.cookies[0]["name"], "sid");
        assert_eq!(session.local_storage["token"], "fake");
        let empty: Session = serde_json::from_str("{}").unwrap();
        assert!(empty.cookies.is_empty() && empty.local_storage.is_empty());
    }

    #[test]
    fn chrome_comes_from_the_mac_never_a_download() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = ShotOptions::new("http://127.0.0.1:9/", dir.path().join("x.png"));
        opts.find = || None;
        opts.profile_parent = Some(dir.path().to_path_buf());
        let err = capture(&opts).unwrap_err();
        assert!(err.contains("Google Chrome was not found"), "{err}");
        // An explicit path that is not there is refused, not replaced.
        opts.chrome = Some(dir.path().join("no-such-chrome"));
        let err = capture(&opts).unwrap_err();
        assert!(err.contains("Google Chrome was not found"), "{err}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        // Nothing in the module fetches a browser.
        let source = include_str!("screenshot.rs").to_lowercase();
        let module = source.split("#[cfg(test)]").next().unwrap();
        for word in [
            concat!("playwright ", "install"),
            concat!("chrome-for-", "testing"),
            concat!("googlechrome", "labs"),
            concat!("down", "load"),
        ] {
            assert!(!module.contains(word), "{word}");
        }
    }

    #[test]
    fn profile_dir_is_removed_even_on_error() {
        // A stand-in Chrome that fills its profile, then dies before answering.
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake-chrome");
        std::fs::write(
            &fake,
            "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in --user-data-dir=*) d=\"${a#--user-data-dir=}\";; esac; done\n\
             mkdir -p \"$d/Default\" && echo x > \"$d/Default/Cookies\"\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let parent = dir.path().join("profiles");
        std::fs::create_dir(&parent).unwrap();
        let mut opts = ShotOptions::new("http://127.0.0.1:9/", dir.path().join("x.png"));
        opts.chrome = Some(fake);
        opts.find = || None;
        opts.profile_parent = Some(parent.clone());
        assert!(capture(&opts).is_err());
        assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 0);
        assert!(!dir.path().join("x.png").exists());
    }

    /// Width and height from a PNG's IHDR chunk.
    fn png_size(path: &Path) -> (u32, u32) {
        let b = std::fs::read(path).unwrap();
        assert_eq!(&b[1..4], b"PNG");
        let n = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        (n(16), n(20))
    }

    /// `python3 -m http.server` on a free port in `dir`, its request log in `log`.
    fn http_server(dir: &Path, log: &Path) -> (std::process::Child, u16) {
        let port = free_port();
        let server = Command::new("python3")
            .args(["-m", "http.server", &port.to_string()])
            .args(["--bind", "127.0.0.1"])
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(log).unwrap())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "http.server did not start");
            std::thread::sleep(Duration::from_millis(100));
        }
        (server, port)
    }

    #[test]
    #[ignore = "launches the Chrome installed on this Mac"]
    fn real_chrome_captures_element_and_full_page() {
        let site = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let (mut backend, other_port) = http_server(logs.path(), &logs.path().join("other.log"));
        // `#who` only reads `ready` when the mock, the cookie and the
        // localStorage token all reached the page. The unmocked calls to other
        // origins must never be sent.
        let page = r#"<!doctype html><html><body style="margin:0;font:16px sans-serif">
<h1>Account</h1><span id="who">loading</span><div style="height:1500px"></div>
<script>
fetch('https://www.google-analytics.com/collect').catch(() => {});
fetch('http://127.0.0.1:OTHER/leak').catch(() => {});
fetch('http://127.0.0.1:9/api/me', {headers: {Authorization: 'Bearer ' + localStorage.getItem('token')}})
  .then((r) => r.json())
  .then((j) => {
    const who = document.getElementById('who');
    who.textContent = j.name;
    if (j.name === 'Test User' && localStorage.getItem('token') === 'fake' && document.cookie.includes('sid=abc')) who.className = 'ready';
  })
  .catch((e) => { document.getElementById('who').textContent = 'error ' + e; });
</script></body></html>"#;
        std::fs::write(
            site.path().join("index.html"),
            page.replace("OTHER", &other_port.to_string()),
        )
        .unwrap();
        let (mut server, port) = http_server(site.path(), &logs.path().join("site.log"));
        let out = tempfile::tempdir().unwrap();
        let profiles = out.path().join("profiles");
        std::fs::create_dir(&profiles).unwrap();
        let base = |file: &str| {
            let mut o = ShotOptions::new(
                format!("http://127.0.0.1:{port}/index.html"),
                out.path().join(file),
            );
            o.mocks = vec![MockRule {
                url_pattern: "/api/me".into(),
                status: 200,
                json: serde_json::json!({"name": "Test User"}),
            }];
            o.session
                .local_storage
                .insert("token".into(), "fake".into());
            o.session
                .cookies
                .push(serde_json::json!({"name": "sid", "value": "abc"}));
            o.wait_for = Some("#who.ready".into());
            o.profile_parent = Some(profiles.clone());
            o
        };

        let mut element = base("who.png");
        element.selector = Some("#who".into());
        let r = capture(&element);
        let mut full = base("full.png");
        full.full_page = true;
        let r2 = capture(&full);
        // Nothing answers there: navigation fails and the profile still goes.
        let unreachable = ShotOptions {
            url: format!("http://127.0.0.1:{}/", free_port()),
            profile_parent: Some(profiles.clone()),
            ..ShotOptions::new("", out.path().join("none.png"))
        };
        let r3 = capture(&unreachable);
        for child in [&mut server, &mut backend] {
            let _ = child.kill();
            let _ = child.wait();
        }

        r.unwrap();
        let (w, h) = png_size(&element.out);
        assert!(w > 0 && w < 1280 && h > 0 && h < 800, "{w}x{h}");
        r2.unwrap();
        let (w, h) = png_size(&full.out);
        assert!(w == 1280 && h >= 1500, "{w}x{h}");
        assert!(r3.is_err() && !unreachable.out.exists(), "{r3:?}");
        assert_eq!(std::fs::read_dir(&profiles).unwrap().count(), 0);
        let read = |f: &str| std::fs::read_to_string(logs.path().join(f)).unwrap();
        assert!(
            read("site.log").contains("GET /index.html"),
            "{}",
            read("site.log")
        );
        assert!(
            !read("other.log").contains("/leak"),
            "{}",
            read("other.log")
        );
    }
}
