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

use crate::linkedin_cdp::{box_script, CdpPage, Chrome, LaunchOptions, DEFAULT_CALL_TIMEOUT};
use crate::linkedin_humanize::Selector;
use crate::loop_plugins::linkedin_autopilot::BrowserError;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
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

/// The URL's host, lowercased, with IPv6 brackets kept (`[::1]`).
fn url_host(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let auth = rest.split(['/', '?', '#']).next()?;
    let auth = auth.rsplit('@').next()?;
    let host = match auth.strip_prefix('[') {
        Some(v6) => format!("[{}]", v6.split(']').next()?),
        None => auth.split(':').next()?.to_string(),
    };
    Some(host.to_ascii_lowercase())
}

/// Only pages served on this machine may be shot: never a staging or
/// production site. A backslash, whitespace or control character is refused
/// outright: browsers and ureq read `\` as `/` and drop tabs and newlines, so
/// the host they would reach is not the one parsed here.
pub fn is_local(url: &str) -> bool {
    if url
        .chars()
        .any(|c| c == '\\' || c.is_whitespace() || c.is_control())
    {
        return false;
    }
    matches!(
        url_host(url).as_deref(),
        Some("localhost" | "127.0.0.1" | "[::1]")
    )
}

/// host:port of an http(s) URL, for the "already served?" probe.
pub fn host_port(url: &str) -> Option<String> {
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
pub fn already_served(url: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
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

/// Stops the `loopx screenshot serve` holder whose state is kept in `dir`
/// (its `pid` file), and removes that state.
pub fn stop_holder(dir: &Path) -> Result<String, String> {
    use crate::run_control::process_alive;
    let pid: Option<i64> = std::fs::read_to_string(dir.join("pid"))
        .ok()
        .and_then(|s| s.trim().parse().ok());
    let Some(pid) = pid else {
        return Ok("no dev server is held".into());
    };
    let _ = std::fs::write(dir.join("stop"), "");
    let deadline = Instant::now() + Duration::from_secs(25);
    while process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
    }
    if process_alive(pid) {
        // SAFETY: plain syscall on a pid the holder recorded for itself.
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
    let _ = std::fs::remove_dir_all(dir);
    Ok("dev server stopped".into())
}

/// The dev server's environment on top of its command's own pairs.
pub fn dev_server_env(serve_env: &[(String, String)]) -> Vec<(String, String)> {
    const NO_TELEMETRY: [&str; 4] = [
        "NEXT_TELEMETRY_DISABLED",
        "NUXT_TELEMETRY_DISABLED",
        "ASTRO_TELEMETRY_DISABLED",
        "GATSBY_TELEMETRY_DISABLED",
    ];
    let mut env = serve_env.to_vec();
    env.extend(crate::dependency_audit::corepack_offline_env());
    env.extend(
        NO_TELEMETRY
            .iter()
            .map(|k| (k.to_string(), "1".to_string())),
    );
    env
}

// ---- cleanup on a signal -------------------------------------------------------------

/// What a signal must clean up: dev-server groups, Chrome processes and
/// profile dirs that are live right now.
struct Live {
    groups: Vec<i32>,
    pids: Vec<i32>,
    dirs: Vec<PathBuf>,
}

static LIVE: Mutex<Live> = Mutex::new(Live {
    groups: Vec::new(),
    pids: Vec::new(),
    dirs: Vec::new(),
});
static SIGNALLED: AtomicI32 = AtomicI32::new(0);

fn live() -> std::sync::MutexGuard<'static, Live> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner())
}

fn register_chrome(pid: u32) {
    live().pids.push(pid as i32);
}

extern "C" fn on_signal(sig: libc::c_int) {
    // Only an atomic store: everything else happens on the watcher thread.
    SIGNALLED.store(sig, Ordering::SeqCst);
}

/// Makes SIGTERM, SIGINT and SIGHUP stop every live dev server and Chrome,
/// remove every live profile dir, and exit with `128 + signal`. Without it a
/// killed `loopx screenshot` (a Bash tool timeout) would orphan its dev
/// server, whose port then blocks the next attempt. Call it once, from the
/// CLI; library callers keep their own signal handling.
pub fn clean_up_on_signals() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            // SAFETY: the handler only stores to an atomic.
            unsafe { libc::signal(sig, handler) };
        }
        std::thread::spawn(|| loop {
            let sig = SIGNALLED.load(Ordering::SeqCst);
            if sig != 0 {
                // Held until exit, so nothing new is registered meanwhile.
                let mut l = live();
                stop_all(&mut l);
                std::process::exit(128 + sig);
            }
            std::thread::sleep(Duration::from_millis(50));
        });
    });
}

/// Stops every live dev server and Chrome, removes every live profile dir and
/// exits with `code`: the last resort of a deadline watchdog.
pub fn stop_everything_and_exit(code: i32) -> ! {
    let mut l = live();
    stop_all(&mut l);
    std::process::exit(code)
}

/// Reaps `pid` (our own child) if it exited; true once it is gone.
fn reaped(pid: i32) -> bool {
    let mut status = 0;
    // SAFETY: plain syscall; ECHILD means someone else already reaped it.
    let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    r == pid || r == -1
}

fn stop_all(l: &mut Live) {
    for g in &l.groups {
        unsafe { libc::kill(-g, libc::SIGTERM) };
    }
    for p in &l.pids {
        unsafe { libc::kill(*p, libc::SIGTERM) };
    }
    let leaders: Vec<i32> = l.groups.iter().chain(&l.pids).copied().collect();
    let until = Instant::now() + Duration::from_secs(3);
    let mut left = leaders.clone();
    while !left.is_empty() && Instant::now() < until {
        left.retain(|p| !reaped(*p));
        std::thread::sleep(Duration::from_millis(50));
    }
    // Group members that ignored SIGTERM, and anything still up.
    for g in &l.groups {
        unsafe { libc::kill(-g, libc::SIGKILL) };
    }
    for p in &left {
        unsafe { libc::kill(*p, libc::SIGKILL) };
    }
    for p in &left {
        let mut status = 0;
        unsafe { libc::waitpid(*p, &mut status, 0) };
    }
    for d in &l.dirs {
        let _ = remove_profile(d);
    }
}

/// A running dev server in its own process group. Dropping it stops the whole
/// group, so a `yarn dev` that forked node workers leaves nothing behind.
pub struct DevServer {
    child: Child,
    tail: OutputTail,
}

/// How many of the dev server's last output lines an error quotes.
const TAIL_LINES: usize = 20;

/// The last [`TAIL_LINES`] lines the dev server wrote to stdout or stderr.
/// Reader threads drain both pipes as the server writes, so a chatty server
/// never blocks on a full pipe.
#[derive(Clone, Default)]
struct OutputTail(Arc<Mutex<VecDeque<String>>>);

impl OutputTail {
    fn drain(&self, pipe: impl Read + Send + 'static) {
        let lines = self.0.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(pipe);
            let mut line = Vec::new();
            // Byte lines, so output that is not UTF-8 can't stop the drain.
            while matches!(reader.read_until(b'\n', &mut line), Ok(n) if n > 0) {
                let text = String::from_utf8_lossy(&line);
                let text: String = text.trim_end().chars().take(500).collect();
                let mut q = lines.lock().unwrap_or_else(|e| e.into_inner());
                if q.len() == TAIL_LINES {
                    q.pop_front();
                }
                q.push_back(text);
                line.clear();
            }
        });
    }

    /// `msg`, followed by the output lines seen so far (if any).
    fn quote(&self, msg: String) -> String {
        // Let the readers catch up with what the server wrote just before.
        std::thread::sleep(Duration::from_millis(100));
        let q = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if q.is_empty() {
            return msg;
        }
        let lines: Vec<&str> = q.iter().map(String::as_str).collect();
        format!("{msg}; its last output:\n{}", lines.join("\n"))
    }
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
        // Spawned and registered under the lock, so a signal can't fall between.
        let mut registry = live();
        let mut child = Command::new(prog)
            .args(args)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .current_dir(cwd)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not start {prog}: {e}"))?;
        registry.groups.push(child.id() as i32);
        drop(registry);
        let tail = OutputTail::default();
        if let Some(out) = child.stdout.take() {
            tail.drain(out);
        }
        if let Some(err) = child.stderr.take() {
            tail.drain(err);
        }
        // From here Drop owns cleanup, including on the error returns below.
        let mut server = DevServer { child, tail };
        let deadline = Instant::now() + timeout;
        let mut agent = ureq::AgentBuilder::new()
            .timeout(timeout.min(Duration::from_secs(5)))
            .build();
        loop {
            match server.child.try_wait() {
                Ok(Some(status)) => {
                    return Err(server.tail.quote(format!(
                        "dev server exited early ({status}) before {url} answered"
                    )))
                }
                Ok(None) => {}
                Err(e) => return Err(format!("could not poll the dev server: {e}")),
            }
            match agent.get(url).call() {
                Ok(_) | Err(ureq::Error::Status(..)) => return Ok(server),
                Err(ureq::Error::Transport(_)) => {}
            }
            let now = Instant::now();
            agent = ureq::AgentBuilder::new()
                .timeout(
                    deadline
                        .saturating_duration_since(now)
                        .min(Duration::from_secs(5)),
                )
                .build();
            if now >= deadline {
                return Err(server.tail.quote(format!(
                    "dev server did not answer {url} within {}s",
                    timeout.as_secs()
                )));
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
        live().groups.retain(|g| *g != pgid);
    }
}

// ---- headless capture ----------------------------------------------------------------

/// One canned backend answer. `url_pattern` is a substring of the request
/// URL, or a glob over the whole URL when it contains `*`.
#[derive(Debug, Clone, Deserialize, Serialize)]
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
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
    let exe = chrome_exe(&opts.chrome, opts.find)?;
    let origin =
        origin_of(&opts.url).ok_or_else(|| format!("{:?} is not an http(s) URL", opts.url))?;
    let browser = Browser {
        exe: &exe,
        origin: &origin,
        mocks: &opts.mocks,
        timeout: opts.timeout,
        profile_parent: opts.profile_parent.clone(),
    };
    browser.run(|shot| shot.run(opts))
}

pub const CHROME_NOT_FOUND: &str =
    "Google Chrome was not found: install it in /Applications or set LOOPX_CHROME";

fn chrome_exe(chrome: &Option<PathBuf>, find: fn() -> Option<PathBuf>) -> Result<PathBuf, String> {
    match chrome {
        Some(p) if p.is_file() => Ok(p.clone()),
        Some(p) => Err(format!("Google Chrome was not found at {}", p.display())),
        None => find().ok_or_else(|| CHROME_NOT_FOUND.to_string()),
    }
}

/// One headless Chrome on a throwaway profile, for one page.
struct Browser<'a> {
    exe: &'a Path,
    origin: &'a str,
    mocks: &'a [MockRule],
    timeout: Duration,
    profile_parent: Option<PathBuf>,
}

impl Browser<'_> {
    fn run<T>(&self, f: impl FnOnce(&mut Shot) -> Result<T, String>) -> Result<T, String> {
        let parent = self
            .profile_parent
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        let profile = parent.join(format!("loopx-shot-{}", uuid::Uuid::new_v4().simple()));
        std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
            .create(&profile)
            .map_err(|e| format!("could not make a Chrome profile dir: {e}"))?;
        live().dirs.push(profile.clone());
        // Chrome is closed inside `shoot`, before its profile goes.
        let result = self.shoot(&profile, f);
        let removed = remove_profile(&profile);
        live().dirs.retain(|d| d != &profile);
        match (result, removed) {
            (Ok(v), Ok(())) => Ok(v),
            (Err(e), Ok(())) | (Ok(_), Err(e)) => Err(e),
            (Err(e), Err(r)) => Err(format!("{e}; {r}")),
        }
    }

    fn shoot<T>(
        &self,
        profile: &Path,
        f: impl FnOnce(&mut Shot) -> Result<T, String>,
    ) -> Result<T, String> {
        let deadline = Instant::now() + self.timeout;
        let launch = LaunchOptions {
            chrome_path: Some(self.exe.to_path_buf()),
            extra_args: QUIET_ARGS.iter().map(|a| a.to_string()).collect(),
            startup_timeout: Some(call_limit(deadline).min(Duration::from_secs(30))),
            on_spawn: Some(register_chrome),
        };
        let launched = Chrome::launch(profile, true, &launch);
        let result = launched.map_err(browser_error).and_then(|chrome| {
            let page = chrome
                .page_within(call_limit(deadline))
                .map_err(browser_error)?;
            let mut shot = Shot {
                chrome,
                page,
                origin: self.origin,
                mocks: self.mocks,
                deadline,
                inflight: HashSet::new(),
                last_network: Instant::now(),
                navigated: false,
                loaded: false,
                seen: Seen::default(),
            };
            let result = f(&mut shot);
            shot.chrome.close();
            result
        });
        // Chrome is closed (or never came up) by now.
        live().pids.clear();
        result
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

/// Runs in every document and dedicated worker before the page's own code.
/// Fetch interception never sees WebSocket handshakes, service workers,
/// shared workers or popups, so those ways out are shut here: a WebSocket to
/// another origin throws, and the other three are refused outright.
const GUARD_JS: &str = r#"(() => {
  const g = globalThis, origin = ORIGIN;
  const refuse = () => { throw new DOMException('blocked by loopx screenshot', 'SecurityError'); };
  const same = (u) => {
    try {
      const x = new URL(String(u), g.location.href);
      const scheme = ({'ws:': 'http:', 'wss:': 'https:'})[x.protocol] || x.protocol;
      return scheme + '//' + x.host === origin;
    } catch (_) { return false; }
  };
  if (g.WebSocket) g.WebSocket = new Proxy(g.WebSocket, {
    construct(t, a, n) { if (!same(a[0])) refuse(); return Reflect.construct(t, a, n); },
  });
  if (g.SharedWorker) g.SharedWorker = new Proxy(g.SharedWorker, { construct: refuse });
  if (g.navigator && g.navigator.serviceWorker) {
    g.navigator.serviceWorker.register = () => Promise.reject(new DOMException('blocked by loopx screenshot', 'SecurityError'));
  }
  if (g.document && typeof g.open === 'function') g.open = () => null;
})();"#;

fn guard_js(origin: &str) -> String {
    GUARD_JS.replace("ORIGIN", &json!(origin).to_string())
}

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
    seen: Seen,
}

/// What the page reported while it loaded, for [`probe`]. Each list keeps at
/// most [`MAX_SEEN`] entries.
#[derive(Default)]
struct Seen {
    exceptions: Vec<String>,
    console_errors: Vec<String>,
    failed_requests: Vec<String>,
    documents: BTreeMap<String, u16>,
    blocked: usize,
    redirected_away: Option<String>,
}

const MAX_SEEN: usize = 5;
const MAX_SEEN_CHARS: usize = 200;

fn push_capped(list: &mut Vec<String>, text: &str) {
    let text = crate::pre_review::clip(text, MAX_SEEN_CHARS);
    if list.len() < MAX_SEEN && !text.is_empty() && !list.contains(&text) {
        list.push(text);
    }
}

/// How long one browser step may wait: what is left of the budget, at least
/// a second, never more than CDP's own 60 s.
fn call_limit(deadline: Instant) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .clamp(Duration::from_secs(1), DEFAULT_CALL_TIMEOUT)
}

fn auto_attach() -> Value {
    json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true})
}

impl Shot<'_> {
    fn send(&self, method: &str, params: Value) -> Result<Value, String> {
        self.page
            .send_timeout(method, params, call_limit(self.deadline))
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
        self.prepare(&opts.url, opts.viewport, &opts.session)?;
        self.navigate(&opts.url, opts.timeout)?;
        let href = self.eval("location.href")?;
        if href
            .as_str()
            .unwrap_or_default()
            .starts_with("chrome-error:")
        {
            return Err(format!("could not load {}", opts.url));
        }
        if let Some(css) = &opts.wait_for {
            if !self.wait_for(css)? {
                return Err(format!("{css:?} never appeared on {}", opts.url));
            }
        }
        self.settle(Duration::from_secs(10))?;
        let png = self.screenshot(opts)?;
        if let Some(dir) = opts.out.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&opts.out, png).map_err(|e| format!("{}: {e}", opts.out.display()))
    }

    /// Viewport, session, the request guard and interception, before any page loads.
    fn prepare(
        &mut self,
        url: &str,
        viewport: (u32, u32),
        session: &Session,
    ) -> Result<(), String> {
        let (width, height) = viewport;
        self.send(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": width, "height": height, "deviceScaleFactor": 1, "mobile": false}),
        )?;
        self.send("Network.enable", json!({}))?;
        for cookie in &session.cookies {
            let mut cookie = cookie.clone();
            let Some(fields) = cookie.as_object_mut() else {
                return Err(format!("a session cookie is not an object: {cookie}"));
            };
            if !fields.contains_key("url") && !fields.contains_key("domain") {
                fields.insert("url".into(), json!(url));
            }
            let r = self.send("Network.setCookie", cookie.clone())?;
            if r["success"] == json!(false) {
                return Err(format!("Chrome refused the session cookie {cookie}"));
            }
        }
        if !session.local_storage.is_empty() {
            // Runs before the page's own scripts, on the page's origin only.
            let source = format!(
                "(() => {{ if (location.origin !== {origin}) return; \
                 try {{ const e = {entries}; for (const k in e) localStorage.setItem(k, e[k]); }} \
                 catch (_) {{}} }})();",
                origin = json!(self.origin),
                entries = json!(session.local_storage),
            );
            self.send(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": source}),
            )?;
        }
        self.send(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": guard_js(self.origin)}),
        )?;
        self.send("Network.setBypassServiceWorker", json!({"bypass": true}))?;
        self.send("Fetch.enable", json!({"patterns": [{"urlPattern": "*"}]}))?;
        // Workers start paused so the guard is in place before their code runs.
        self.send("Target.setAutoAttach", auto_attach())?;
        self.lock_cdp().events.clear();
        Ok(())
    }

    /// Navigates and waits for the load event; `Err` when it never loads.
    fn navigate(&mut self, url: &str, budget: Duration) -> Result<(), String> {
        // `Page.navigate` only answers once the paused document request is
        // let through, so navigate from the page and answer requests meanwhile.
        self.eval(&format!("location.href = {}; true", json!(url)))?;
        while !self.loaded {
            self.pump(Duration::from_millis(100))?;
            if Instant::now() >= self.deadline {
                return Err(format!("{url} did not load within {}s", budget.as_secs()));
            }
        }
        Ok(())
    }

    /// True once `css` matches, false after 15 s.
    fn wait_for(&mut self, css: &str) -> Result<bool, String> {
        let probe = format!("!!document.querySelector({})", json!(css));
        let until = (Instant::now() + Duration::from_secs(15)).min(self.deadline);
        while self.eval(&probe)? != json!(true) {
            if Instant::now() >= until {
                return Ok(false);
            }
            self.pump(Duration::from_millis(250))?;
        }
        Ok(true)
    }

    /// 500 ms with nothing in flight, but never more than `limit` of waiting.
    fn settle(&mut self, limit: Duration) -> Result<(), String> {
        let until = (Instant::now() + limit).min(self.deadline);
        while Instant::now() < until
            && !(self.inflight.is_empty()
                && self.last_network.elapsed() >= Duration::from_millis(500))
        {
            self.pump(Duration::from_millis(100))?;
        }
        Ok(())
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
                let url = p["request"]["url"].as_str().unwrap_or_default();
                if decide(url, self.origin, self.mocks) == Decision::Block {
                    self.seen.blocked += 1;
                }
                self.answer(&self.session(event), p)
            }
            "Target.attachedToTarget" => {
                let child = CdpPage::new(
                    self.chrome.cdp.clone(),
                    p["sessionId"].as_str().unwrap_or_default().to_string(),
                );
                // Failures here only mean the target already went away.
                let t = call_limit(self.deadline);
                if p["targetInfo"]["type"] == json!("worker") {
                    let _ = child.send_timeout(
                        "Runtime.evaluate",
                        json!({"expression": guard_js(self.origin)}),
                        t,
                    );
                    let _ = child.send_timeout("Target.setAutoAttach", auto_attach(), t);
                    let _ = child.send_timeout("Network.enable", json!({}), t);
                } else {
                    let _ = child.send_timeout(
                        "Fetch.enable",
                        json!({"patterns": [{"urlPattern": "*"}]}),
                        t,
                    );
                }
                let _ = child.send_timeout("Runtime.runIfWaitingForDebugger", json!({}), t);
            }
            // Blocked requests are not activity: an EventSource that keeps
            // reconnecting to a blocked origin must not hold the shot back.
            "Network.requestWillBeSent" => {
                if let Some(to) = blocked_redirect(p, self.origin, self.mocks) {
                    self.seen.redirected_away = Some(to);
                }
                let url = p["request"]["url"].as_str().unwrap_or_default();
                if decide(url, self.origin, self.mocks) != Decision::Block {
                    self.last_network = Instant::now();
                    self.inflight.insert(request_id());
                }
            }
            "Network.loadingFinished" | "Network.loadingFailed" => {
                if self.inflight.remove(&request_id()) {
                    self.last_network = Instant::now();
                }
            }
            "Page.frameNavigated" if p["frame"]["parentId"].is_null() => {
                if let Some(url) = p["frame"]["unreachableUrl"].as_str() {
                    return Err(format!("could not load {url}"));
                }
                self.navigated |= p["frame"]["url"] != json!("about:blank");
            }
            "Page.loadEventFired" if self.navigated => self.loaded = true,
            "Runtime.exceptionThrown" => {
                let d = &p["exceptionDetails"];
                let text = d["exception"]["description"]
                    .as_str()
                    .or(d["text"].as_str())
                    .unwrap_or("uncaught exception");
                push_capped(&mut self.seen.exceptions, text);
            }
            "Runtime.consoleAPICalled" if p["type"] == json!("error") => {
                let text: Vec<String> = p["args"]
                    .as_array()
                    .map(|a| a.iter().take(4).map(console_arg).collect())
                    .unwrap_or_default();
                push_capped(&mut self.seen.console_errors, &text.join(" "));
            }
            // Network-sourced entries are the requests this check blocked or
            // that failed; `failed_requests` reports the ones that matter.
            "Log.entryAdded"
                if p["entry"]["level"] == json!("error")
                    && p["entry"]["source"] != json!("network") =>
            {
                let text = p["entry"]["text"].as_str().unwrap_or_default();
                push_capped(&mut self.seen.console_errors, text);
            }
            "Network.responseReceived" => {
                let status = p["response"]["status"].as_u64().unwrap_or(0) as u16;
                let url = p["response"]["url"].as_str().unwrap_or_default();
                if p["type"] == json!("Document") {
                    if let Some(frame) = p["frameId"].as_str() {
                        self.seen.documents.insert(frame.to_string(), status);
                    }
                } else if status >= 400 && origin_of(url).as_deref() == Some(self.origin) {
                    let path = url_path(url);
                    // Chrome asks for it on its own; the page never did.
                    if path != "/favicon.ico" {
                        push_capped(&mut self.seen.failed_requests, &format!("{status} {path}"));
                    }
                }
            }
            "Inspector.detached" | "Inspector.targetCrashed" => {
                return Err("the page crashed".into())
            }
            _ => {}
        }
        Ok(())
    }

    /// The session an event came from, to answer it on.
    fn session(&self, event: &Value) -> CdpPage {
        CdpPage::new(
            self.chrome.cdp.clone(),
            event["sessionId"].as_str().unwrap_or_default().to_string(),
        )
    }

    /// Answers one paused request on the session that paused it. A request
    /// the page already dropped makes Chrome refuse the answer, which is fine
    /// to ignore.
    fn answer(&self, on: &CdpPage, p: &Value) {
        let id = &p["requestId"];
        let url = p["request"]["url"].as_str().unwrap_or_default();
        let t = call_limit(self.deadline);
        let _ = match decide(url, self.origin, self.mocks) {
            Decision::Continue => {
                on.send_timeout("Fetch.continueRequest", json!({"requestId": id}), t)
            }
            Decision::Block => on.send_timeout(
                "Fetch.failRequest",
                json!({"requestId": id, "errorReason": "BlockedByClient"}),
                t,
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
                on.send_timeout(
                    "Fetch.fulfillRequest",
                    json!({"requestId": id, "responseCode": status, "responseHeaders": headers, "body": body}),
                    t,
                )
            }
        };
    }

    fn screenshot(&mut self, opts: &ShotOptions) -> Result<Vec<u8>, String> {
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
                // Page coordinates, so an element below the fold is rendered too.
                "captureBeyondViewport": true,
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

// ---- probe: load a page and report what went wrong ----------------------------------

fn console_arg(a: &Value) -> String {
    match &a["value"] {
        Value::String(s) => s.clone(),
        Value::Null => a["description"].as_str().unwrap_or_default().to_string(),
        v => v.to_string(),
    }
}

/// The target of a top-level document redirect this check refuses to follow
/// (another origin), from `Network.requestWillBeSent` params.
fn blocked_redirect(p: &Value, origin: &str, mocks: &[MockRule]) -> Option<String> {
    let url = p["request"]["url"].as_str()?;
    (p["type"] == json!("Document")
        && p.get("redirectResponse").is_some_and(Value::is_object)
        && decide(url, origin, mocks) == Decision::Block)
        .then(|| url.to_string())
}

/// The path of an http(s) URL, without its query or fragment.
fn url_path(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let path = rest.find('/').map_or("/", |i| &rest[i..]);
    path.split(['?', '#']).next().unwrap_or("/").to_string()
}

pub struct ProbeOptions {
    pub url: String,
    pub wait_for: Option<String>,
    pub dark: bool,
    pub viewport: (u32, u32),
    pub mocks: Vec<MockRule>,
    pub session: Session,
    pub axe: bool,
    /// The whole page, from launch to the axe result.
    pub timeout: Duration,
    pub chrome: Option<PathBuf>,
    pub find: fn() -> Option<PathBuf>,
    pub profile_parent: Option<PathBuf>,
}

impl ProbeOptions {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            wait_for: None,
            dark: false,
            viewport: (1280, 800),
            mocks: Vec::new(),
            session: Session::default(),
            axe: false,
            timeout: Duration::from_secs(30),
            chrome: None,
            find: crate::linkedin_cdp::find_chrome,
            profile_parent: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PageReport {
    pub final_url: String,
    pub status: Option<u16>,
    pub loaded: bool,
    #[serde(default)]
    pub load_error: Option<String>,
    pub wait_for_found: Option<bool>,
    pub blank: bool,
    pub exceptions: Vec<String>,
    pub console_errors: Vec<String>,
    pub failed_requests: Vec<String>,
    /// Requests this check refused to send (other origins).
    #[serde(default)]
    pub blocked_requests: usize,
    /// The origin the page was asked for.
    #[serde(default)]
    pub requested_origin: String,
    pub axe: Option<Result<Vec<crate::a11y::Violation>, String>>,
}

/// Loads `opts.url` in headless Chrome, the same guarded way [`capture`]
/// does, and reports what the page did. `Err` only when Chrome itself could
/// not be used; a page that fails to load is a report with `loaded: false`.
pub fn probe(opts: &ProbeOptions) -> Result<PageReport, String> {
    let exe = chrome_exe(&opts.chrome, opts.find)?;
    let origin =
        origin_of(&opts.url).ok_or_else(|| format!("{:?} is not an http(s) URL", opts.url))?;
    let browser = Browser {
        exe: &exe,
        origin: &origin,
        mocks: &opts.mocks,
        timeout: opts.timeout,
        profile_parent: opts.profile_parent.clone(),
    };
    browser.run(|shot| shot.probe(opts))
}

impl Shot<'_> {
    fn probe(&mut self, opts: &ProbeOptions) -> Result<PageReport, String> {
        self.prepare(&opts.url, opts.viewport, &opts.session)?;
        let scheme = if opts.dark { "dark" } else { "light" };
        self.send(
            "Emulation.setEmulatedMedia",
            json!({"features": [{"name": "prefers-color-scheme", "value": scheme}]}),
        )?;
        self.send("Runtime.enable", json!({}))?;
        self.send("Log.enable", json!({}))?;
        let mut report = PageReport {
            requested_origin: self.origin.to_string(),
            ..PageReport::default()
        };
        if let Err(e) = self.navigate(&opts.url, opts.timeout) {
            report.load_error = Some(crate::pre_review::clip(&e, MAX_SEEN_CHARS));
            self.fill_seen(&mut report);
            self.redirected_away(&mut report);
            return Ok(report);
        }
        report.final_url = self
            .eval("location.href")?
            .as_str()
            .unwrap_or_default()
            .into();
        let main = self.send("Page.getFrameTree", json!({}))?;
        report.status = main["frameTree"]["frame"]["id"]
            .as_str()
            .and_then(|id| self.seen.documents.get(id).copied());
        report.loaded = !report.final_url.starts_with("chrome-error:");
        if !report.loaded {
            report.load_error = Some(format!("could not load {}", opts.url));
            self.fill_seen(&mut report);
            self.redirected_away(&mut report);
            return Ok(report);
        }
        if let Some(css) = &opts.wait_for {
            report.wait_for_found = Some(self.wait_for(css)?);
        }
        self.settle(Duration::from_secs(5))?;
        report.blank = self.eval(
            "(() => { const b = document.body; if (!b) return true; \
             return b.innerText.trim() === '' && !document.querySelector('img,svg,canvas,video'); })()",
        )? == json!(true);
        // Before axe runs, so nothing axe does is counted against the page.
        self.fill_seen(&mut report);
        if opts.axe {
            report.axe = Some(self.run_axe());
        }
        Ok(report)
    }

    /// A page that failed only because it redirected to another origin (an
    /// SSO sign-in) counts as landing there, so it is skipped, not failed.
    fn redirected_away(&self, report: &mut PageReport) {
        if let Some(to) = &self.seen.redirected_away {
            report.final_url = to.clone();
            report.loaded = true;
            report.status = None;
            report.load_error = None;
        }
    }

    fn fill_seen(&self, report: &mut PageReport) {
        report.blocked_requests = self.seen.blocked;
        report.exceptions = self.seen.exceptions.clone();
        report.console_errors = self.seen.console_errors.clone();
        report.failed_requests = self.seen.failed_requests.clone();
    }

    fn run_axe(&self) -> Result<Vec<crate::a11y::Violation>, String> {
        let r = self.send(
            "Runtime.evaluate",
            json!({"expression": crate::a11y::run_expression(), "awaitPromise": true, "returnByValue": true}),
        )?;
        if let Some(ex) = r.get("exceptionDetails") {
            let text = ex["exception"]["description"]
                .as_str()
                .or(ex["text"].as_str())
                .unwrap_or("axe failed");
            return Err(crate::pre_review::clip(
                &format!("axe failed: {text}"),
                MAX_SEEN_CHARS,
            ));
        }
        crate::a11y::parse(&r["result"]["value"])
    }
}

/// One thing wrong with a page. `rule` is one of `page-load`, `wait-for`,
/// `page-error`, `blank-page` (blocking), `console-error`, `failed-request`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Problem {
    pub rule: String,
    pub detail: String,
    pub blocking: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PageVerdict {
    Skipped(String),
    Checked(Vec<Problem>),
}

fn same_path(a: &str, b: &str) -> bool {
    let norm = |p: &str| {
        let p = p.split(['?', '#']).next().unwrap_or("/");
        let p = p.trim_end_matches('/');
        if p.is_empty() {
            "/".to_string()
        } else {
            p.to_string()
        }
    };
    norm(a) == norm(b)
}

/// An error a page throws when one of its requests is refused.
fn caused_by_a_block(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    [
        "failed to fetch",
        "networkerror",
        "err_blocked_by_client",
        "load failed",
        "websocket",
    ]
    .iter()
    .any(|w| e.contains(w))
}

/// What a probe's report means for a page declared at `declared_path`.
pub fn problems(report: &PageReport, declared_path: &str) -> PageVerdict {
    let problem = |rule: &str, detail: &str, blocking: bool| Problem {
        rule: rule.into(),
        detail: crate::pre_review::clip(detail, MAX_SEEN_CHARS),
        blocking,
    };
    if !report.loaded {
        let why = report
            .load_error
            .as_deref()
            .unwrap_or("the page did not load");
        return PageVerdict::Checked(vec![problem("page-load", why, true)]);
    }
    if let Some(status) = report.status.filter(|s| *s >= 400) {
        return PageVerdict::Checked(vec![problem(
            "page-load",
            &format!("status {status}"),
            true,
        )]);
    }
    let landed = url_path(&report.final_url);
    let elsewhere = !report.requested_origin.is_empty()
        && origin_of(&report.final_url).as_deref() != Some(report.requested_origin.as_str());
    if elsewhere || !same_path(&landed, declared_path) {
        return PageVerdict::Skipped(crate::pre_review::clip(
            &format!("redirected to {landed} (sign-in?)"),
            MAX_SEEN_CHARS,
        ));
    }
    let mut out = Vec::new();
    if report.wait_for_found == Some(false) {
        out.push(problem(
            "wait-for",
            "the expected element never appeared",
            true,
        ));
    }
    for e in report.exceptions.iter().take(MAX_SEEN) {
        let ours = e.contains("blocked by loopx screenshot")
            || (report.blocked_requests > 0 && caused_by_a_block(e));
        out.push(problem("page-error", e, !ours));
    }
    if report.blank {
        out.push(problem("blank-page", "the page rendered nothing", true));
    }
    for e in report.console_errors.iter().take(MAX_SEEN) {
        out.push(problem("console-error", e, false));
    }
    for e in report.failed_requests.iter().take(MAX_SEEN) {
        out.push(problem("failed-request", e, false));
    }
    PageVerdict::Checked(out)
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
    fn the_dev_server_env_never_downloads_a_package_manager_or_phones_home() {
        let env = dev_server_env(&[("RAILS_ENV".into(), "test".into())]);
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("RAILS_ENV"), Some("test"));
        for (k, v) in [
            ("COREPACK_ENABLE_NETWORK", "0"),
            ("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0"),
            ("npm_config_manage_package_manager_versions", "false"),
            ("NEXT_TELEMETRY_DISABLED", "1"),
            ("NUXT_TELEMETRY_DISABLED", "1"),
            ("ASTRO_TELEMETRY_DISABLED", "1"),
            ("GATSBY_TELEMETRY_DISABLED", "1"),
        ] {
            assert_eq!(get(k), Some(v), "{k}");
        }
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

    // The fake servers below use `socketserver.TCPServer`, not
    // `http.server.HTTPServer`: the latter's `server_bind` does a reverse DNS
    // lookup (`socket.getfqdn`) before it listens, which can hang for tens of
    // seconds on a CI runner whose resolver never answers.

    #[test]
    fn dev_server_waits_for_the_url_then_kills_its_group() {
        let port = free_port();
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("child.pid");
        let script = format!(
            "import http.server,socketserver,subprocess,sys\n\
             c=subprocess.Popen(['sleep','60'])\n\
             open({:?},'w').write(str(c.pid))\n\
             socketserver.TCPServer(('127.0.0.1',{port}),http.server.SimpleHTTPRequestHandler).serve_forever()\n",
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
    #[test]
    fn dev_server_errors_carry_its_last_output_lines() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("http://127.0.0.1:{}/", free_port());
        let script =
            "i=1; while [ $i -le 30 ]; do echo \"boom-line-$i\" >&2; i=$((i+1)); done; exit 3";
        let argv = vec!["sh".to_string(), "-c".into(), script.into()];
        let err = DevServer::start(&argv, &[], dir.path(), &url, Duration::from_secs(20))
            .err()
            .unwrap();
        assert!(err.contains("exited"), "{err}");
        assert!(err.contains("boom-line-30"), "{err}");
        assert!(err.contains("boom-line-11"), "{err}");
        assert!(!err.contains("boom-line-10\n"), "{err}");

        let script = "echo 'Error: cannot find module next' >&2; sleep 30";
        let argv = vec!["sh".to_string(), "-c".into(), script.into()];
        let err = DevServer::start(&argv, &[], dir.path(), &url, Duration::from_secs(1))
            .err()
            .unwrap();
        assert!(err.contains("did not answer"), "{err}");
        assert!(err.contains("cannot find module next"), "{err}");
    }

    #[test]
    fn a_chatty_dev_server_never_blocks_on_a_full_pipe() {
        // 1 MB of output before it listens: an undrained pipe would stall it.
        let port = free_port();
        let dir = tempfile::tempdir().unwrap();
        let script = format!(
            "import http.server,socketserver,sys\n\
             sys.stderr.write('x'*1000000+'\\n'); sys.stderr.flush()\n\
             sys.stdout.write('y'*1000000+'\\n'); sys.stdout.flush()\n\
             socketserver.TCPServer(('127.0.0.1',{port}),http.server.SimpleHTTPRequestHandler).serve_forever()\n"
        );
        let argv = vec!["python3".to_string(), "-c".to_string(), script];
        let server = DevServer::start(
            &argv,
            &[],
            dir.path(),
            &format!("http://127.0.0.1:{port}/"),
            Duration::from_secs(20),
        );
        assert!(server.is_ok(), "{:?}", server.err());
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
<div id="below" style="width:200px;height:100px;background:linear-gradient(90deg,red,blue)"></div>
<div id="blank" style="width:200px;height:100px;background:#fff"></div>
<script>
fetch('https://www.google-analytics.com/collect').catch(() => {});
fetch('http://127.0.0.1:OTHER/leak').catch(() => {});
try { new WebSocket('ws://127.0.0.1:OTHER/ws'); } catch (_) {}
try { new EventSource('http://127.0.0.1:OTHER/sse'); } catch (_) {}
try { new Worker('worker.js'); } catch (_) {}
try { window.open('http://127.0.0.1:OTHER/popup'); } catch (_) {}
try { navigator.serviceWorker.register('sw.js').catch(() => {}); } catch (_) {}
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
        for (file, path) in [("worker.js", "worker"), ("sw.js", "sw")] {
            let js = format!(
                "fetch('http://127.0.0.1:{other_port}/{path}').catch(() => {{}});\n\
                 try {{ new WebSocket('ws://127.0.0.1:{other_port}/{path}-ws'); }} catch (_) {{}}"
            );
            std::fs::write(site.path().join(file), js).unwrap();
        }
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
        // Below the fold: a gradient must not come out like a blank box.
        let mut below = base("below.png");
        below.selector = Some("#below".into());
        let r4 = capture(&below);
        let mut blank = base("blank.png");
        blank.selector = Some("#blank".into());
        let r5 = capture(&blank);
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
        r4.unwrap();
        r5.unwrap();
        assert_eq!(png_size(&below.out), (200, 100));
        assert_eq!(png_size(&blank.out), (200, 100));
        assert_eq!(std::fs::read_dir(&profiles).unwrap().count(), 0);
        let read = |f: &str| std::fs::read_to_string(logs.path().join(f)).unwrap();
        assert!(
            read("site.log").contains("GET /index.html"),
            "{}",
            read("site.log")
        );
        // Every way out to the unmocked origin, and the below-the-fold shot,
        // are checked together so one failure doesn't hide another.
        let mut problems: Vec<String> = [
            "/leak",
            "/ws",
            "/sse",
            "/worker",
            "/worker-ws",
            "/popup",
            "/sw",
            "/sw-ws",
        ]
        .iter()
        .filter(|path| read("other.log").contains(&format!("GET {path} ")))
        .map(|path| format!("{path} reached the unmocked origin"))
        .collect();
        let size = |p: &Path| std::fs::metadata(p).unwrap().len();
        if size(&below.out) <= 2 * size(&blank.out) {
            problems.push(format!(
                "below the fold came out blank: {} vs {} bytes",
                size(&below.out),
                size(&blank.out)
            ));
        }
        assert!(problems.is_empty(), "{problems:#?}");
    }

    fn loaded(path: &str) -> PageReport {
        PageReport {
            final_url: format!("http://localhost:3000{path}"),
            status: Some(200),
            loaded: true,
            ..PageReport::default()
        }
    }

    fn rules(v: &PageVerdict) -> Vec<(String, bool)> {
        match v {
            PageVerdict::Checked(p) => p.iter().map(|p| (p.rule.clone(), p.blocking)).collect(),
            PageVerdict::Skipped(why) => panic!("skipped: {why}"),
        }
    }

    #[test]
    fn problems_classifies_load_wait_for_error_blank_console_and_requests() {
        assert!(rules(&problems(&loaded("/orders/new"), "/orders/new")).is_empty());
        let failed = PageReport {
            load_error: Some("http://localhost:3000/x did not load within 30s".into()),
            ..PageReport::default()
        };
        assert_eq!(
            rules(&problems(&failed, "/x")),
            vec![("page-load".into(), true)]
        );
        let mut r = loaded("/x");
        r.status = Some(500);
        assert_eq!(rules(&problems(&r, "/x")), vec![("page-load".into(), true)]);
        if let PageVerdict::Checked(p) = problems(&r, "/x") {
            assert_eq!(p[0].detail, "status 500");
        }
        let mut r = loaded("/x");
        r.wait_for_found = Some(false);
        r.exceptions = vec!["TypeError: x is undefined".into()];
        r.blank = true;
        r.console_errors = vec!["boom".into()];
        r.failed_requests = vec!["404 /api/me".into()];
        assert_eq!(
            rules(&problems(&r, "/x")),
            vec![
                ("wait-for".into(), true),
                ("page-error".into(), true),
                ("blank-page".into(), true),
                ("console-error".into(), false),
                ("failed-request".into(), false),
            ]
        );
        r.wait_for_found = Some(true);
        r.exceptions.clear();
        r.blank = false;
        assert!(rules(&problems(&r, "/x")).iter().all(|(_, b)| !b));
    }

    #[test]
    fn a_redirect_to_another_path_is_skipped_not_failed() {
        let r = loaded("/users/sign_in?return_to=%2Forders");
        assert_eq!(
            problems(&r, "/orders/new"),
            PageVerdict::Skipped("redirected to /users/sign_in (sign-in?)".into())
        );
        // A query or trailing-slash change is the same page.
        assert!(rules(&problems(&loaded("/orders?tab=2"), "/orders")).is_empty());
        assert!(rules(&problems(&loaded("/orders/"), "/orders?x=1")).is_empty());
        assert!(rules(&problems(&loaded("/"), "/")).is_empty());
    }

    #[test]
    fn errors_caused_by_the_checks_own_blocking_never_block() {
        let mut r = loaded("/x");
        r.blocked_requests = 2;
        r.exceptions = vec![
            "SecurityError: blocked by loopx screenshot".into(),
            "TypeError: Failed to fetch".into(),
            "ReferenceError: total is not defined".into(),
        ];
        assert_eq!(
            rules(&problems(&r, "/x")),
            vec![
                ("page-error".into(), false),
                ("page-error".into(), false),
                ("page-error".into(), true),
            ]
        );
        // With nothing blocked, a failed fetch is the page's own problem.
        r.blocked_requests = 0;
        r.exceptions = vec!["TypeError: Failed to fetch".into()];
        assert_eq!(
            rules(&problems(&r, "/x")),
            vec![("page-error".into(), true)]
        );
    }

    #[test]
    fn a_document_redirect_to_another_origin_is_recognised() {
        let ev = |url: &str, kind: &str, redirect: bool| {
            let mut p = serde_json::json!({"type": kind, "request": {"url": url}});
            if redirect {
                p["redirectResponse"] = serde_json::json!({"status": 302});
            }
            p
        };
        let o = "http://localhost:3000";
        assert_eq!(
            blocked_redirect(
                &ev("https://sso.example.com/login?next=x", "Document", true),
                o,
                &[]
            ),
            Some("https://sso.example.com/login?next=x".into())
        );
        assert_eq!(
            blocked_redirect(&ev("http://localhost:3000/login", "Document", true), o, &[]),
            None
        );
        assert_eq!(
            blocked_redirect(&ev("https://sso.example.com/", "Document", false), o, &[]),
            None
        );
        assert_eq!(
            blocked_redirect(&ev("https://cdn.example.com/a.js", "Script", true), o, &[]),
            None
        );
        let r = PageReport {
            final_url: "https://sso.example.com/login?next=x".into(),
            loaded: true,
            ..PageReport::default()
        };
        assert_eq!(
            problems(&r, "/orders"),
            PageVerdict::Skipped("redirected to /login (sign-in?)".into())
        );
    }

    #[test]
    fn a_redirect_to_another_origin_with_the_same_path_is_skipped() {
        let r = PageReport {
            final_url: "https://sso.example.com/orders".into(),
            requested_origin: "http://localhost:3000".into(),
            status: Some(200),
            loaded: true,
            ..PageReport::default()
        };
        assert_eq!(
            problems(&r, "/orders"),
            PageVerdict::Skipped("redirected to /orders (sign-in?)".into())
        );
        let same = PageReport {
            final_url: "http://localhost:3000/orders".into(),
            ..r
        };
        assert!(rules(&problems(&same, "/orders")).is_empty());
    }

    #[test]
    fn page_text_is_flattened_and_capped() {
        let mut r = loaded("/x");
        r.exceptions = (0..9)
            .map(|i| format!("/close #{i}\n{}", "e".repeat(900)))
            .collect();
        r.console_errors = (0..9).map(|i| format!("c{i}")).collect();
        let PageVerdict::Checked(p) = problems(&r, "/x") else {
            panic!()
        };
        assert_eq!(
            p.iter().filter(|p| p.rule == "page-error").count(),
            MAX_SEEN
        );
        assert_eq!(
            p.iter().filter(|p| p.rule == "console-error").count(),
            MAX_SEEN
        );
        for x in &p {
            assert!(
                x.detail.chars().count() <= MAX_SEEN_CHARS && !x.detail.contains('\n'),
                "{x:?}"
            );
        }
        let mut seen = Vec::new();
        for i in 0..9 {
            push_capped(&mut seen, &format!("x{i}\n{}", "y".repeat(500)));
        }
        push_capped(&mut seen, "x0");
        assert_eq!(seen.len(), MAX_SEEN);
        assert!(seen
            .iter()
            .all(|s| s.chars().count() <= MAX_SEEN_CHARS && !s.contains('\n')));
        assert_eq!(url_path("http://h:1/a/b?c=d#e"), "/a/b");
        assert_eq!(url_path("http://h:1"), "/");
    }

    #[test]
    fn probe_without_chrome_is_an_error_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = ProbeOptions::new("http://127.0.0.1:9/");
        opts.find = || None;
        opts.profile_parent = Some(dir.path().to_path_buf());
        assert_eq!(probe(&opts).unwrap_err(), CHROME_NOT_FOUND);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    fn probe_on(dir: &Path, page: &str, f: impl FnOnce(&mut ProbeOptions)) -> PageReport {
        let profiles = tempfile::tempdir().unwrap();
        let (mut server, port) = http_server(dir, &profiles.path().join("site.log"));
        let mut opts = ProbeOptions::new(format!("http://127.0.0.1:{port}/{page}"));
        opts.profile_parent = Some(profiles.path().to_path_buf());
        f(&mut opts);
        let r = probe(&opts);
        let _ = server.kill();
        let _ = server.wait();
        let left: Vec<_> = std::fs::read_dir(profiles.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name() != "site.log")
            .collect();
        assert!(left.is_empty(), "{left:?}");
        r.unwrap()
    }

    #[test]
    #[ignore = "launches the Chrome installed on this Mac"]
    fn real_chrome_probe_reports_an_uncaught_error_and_a_contrast_violation() {
        let site = tempfile::tempdir().unwrap();
        std::fs::write(
            site.path().join("index.html"),
            r#"<!doctype html><html lang="en"><head><title>t</title>
<meta http-equiv="Content-Security-Policy" content="script-src 'self'"></head>
<body><main><h1>Orders</h1><p id="faint" style="color:#ccc;background:#fff">faint text</p></main>
<script src="boom.js"></script></body></html>"#,
        )
        .unwrap();
        std::fs::write(
            site.path().join("boom.js"),
            "console.error('console says no'); setTimeout(() => { throw new Error('kaboom'); }, 0);",
        )
        .unwrap();
        let r = probe_on(site.path(), "index.html", |o| {
            o.axe = true;
            o.wait_for = Some("#faint".into());
        });
        assert!(r.loaded && r.status == Some(200), "{r:?}");
        assert_eq!(r.wait_for_found, Some(true));
        assert!(r.exceptions.iter().any(|e| e.contains("kaboom")), "{r:?}");
        assert!(
            r.console_errors
                .iter()
                .any(|e| e.contains("console says no")),
            "{r:?}"
        );
        let axe = r.axe.clone().unwrap().unwrap();
        let contrast = axe
            .iter()
            .find(|v| v.id == "color-contrast")
            .expect("contrast");
        assert_eq!(contrast.impact, crate::a11y::Impact::Serious);
        assert!(
            contrast.targets.contains(&"#faint".to_string()),
            "{contrast:?}"
        );
        assert!(
            matches!(problems(&r, "/index.html"), PageVerdict::Checked(p) if p.iter().any(|p| p.rule == "page-error"))
        );
    }

    #[test]
    #[ignore = "launches the Chrome installed on this Mac"]
    fn real_chrome_probe_blocks_off_origin_requests() {
        let site = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let (mut other, other_port) = http_server(logs.path(), &logs.path().join("other.log"));
        let page = r#"<!doctype html><html><body><p>hi</p><img src="missing.png" alt="x"><script>
fetch('http://127.0.0.1:OTHER/leak').catch(() => {});
try { new WebSocket('ws://127.0.0.1:OTHER/ws'); } catch (_) {}
try { navigator.sendBeacon('http://127.0.0.1:OTHER/beacon', 'x'); } catch (_) {}
</script></body></html>"#;
        std::fs::write(
            site.path().join("index.html"),
            page.replace("OTHER", &other_port.to_string()),
        )
        .unwrap();
        let r = probe_on(site.path(), "index.html", |o| o.axe = true);
        let _ = other.kill();
        let _ = other.wait();
        assert!(r.loaded, "{r:?}");
        assert!(
            r.failed_requests.iter().any(|f| f == "404 /missing.png"),
            "{r:?}"
        );
        assert!(
            !r.failed_requests.iter().any(|f| f.contains("favicon")),
            "{r:?}"
        );
        assert!(
            !r.console_errors.iter().any(|e| e.contains("ERR_BLOCKED")),
            "{r:?}"
        );
        let log = std::fs::read_to_string(logs.path().join("other.log")).unwrap();
        assert!(!log.contains("GET /") && !log.contains("POST /"), "{log}");
    }

    #[test]
    #[ignore = "launches the Chrome installed on this Mac"]
    fn real_chrome_probe_skips_a_page_that_redirects_to_another_origin() {
        let port = free_port();
        let script = format!(
            "import http.server,socketserver\n\
             class H(http.server.BaseHTTPRequestHandler):\n\
             \x20def do_GET(self):\n\
             \x20 self.send_response(302); self.send_header('Location','http://127.0.0.1:9/users/sign_in'); self.end_headers()\n\
             \x20def log_message(self,*a): pass\n\
             socketserver.TCPServer(('127.0.0.1',{port}),H).serve_forever()\n"
        );
        let mut server = Command::new("python3")
            .args(["-c", &script])
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "server did not start");
            std::thread::sleep(Duration::from_millis(100));
        }
        let profiles = tempfile::tempdir().unwrap();
        let mut opts = ProbeOptions::new(format!("http://127.0.0.1:{port}/orders"));
        opts.profile_parent = Some(profiles.path().to_path_buf());
        let r = probe(&opts);
        let _ = server.kill();
        let _ = server.wait();
        let r = r.unwrap();
        assert_eq!(
            problems(&r, "/orders"),
            PageVerdict::Skipped("redirected to /users/sign_in (sign-in?)".into()),
            "{r:?}"
        );
        assert_eq!(std::fs::read_dir(profiles.path()).unwrap().count(), 0);
    }

    #[test]
    #[ignore = "launches the Chrome installed on this Mac"]
    fn real_chrome_probe_honours_dark_mode() {
        let site = tempfile::tempdir().unwrap();
        std::fs::write(
            site.path().join("index.html"),
            r#"<!doctype html><html><body><p id="m">scheme</p><script>
document.getElementById('m').className = matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
</script></body></html>"#,
        )
        .unwrap();
        for dark in [true, false] {
            let r = probe_on(site.path(), "index.html", |o| {
                o.dark = dark;
                o.wait_for = Some(if dark { "#m.dark" } else { "#m.light" }.into());
            });
            assert_eq!(r.wait_for_found, Some(true), "dark={dark}: {r:?}");
            assert!(!r.blank);
        }
        let empty = tempfile::tempdir().unwrap();
        std::fs::write(
            empty.path().join("index.html"),
            "<!doctype html><html><body> </body></html>",
        )
        .unwrap();
        let r = probe_on(empty.path(), "index.html", |_| {});
        assert!(r.blank, "{r:?}");
        let r = probe_on(empty.path(), "nope.html", |_| {});
        assert_eq!(r.status, Some(404), "{r:?}");
    }
}
