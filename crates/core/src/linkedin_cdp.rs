//! A minimal Chrome DevTools Protocol client for the LinkedIn browser.
//!
//! Choice of implementation: `chromiumoxide` and `headless_chrome` were
//! evaluated and rejected. chromiumoxide is async-only and drags a large
//! futures/tokio-tungstenite/serde-codegen tree into a crate (`loopx-core`)
//! that is otherwise synchronous; headless_chrome is sync but its tab/element
//! model is built around its own launcher (pipe/arg defaults that fight a
//! persistent, user-visible profile) and pins older dependencies. The loop
//! needs a dozen CDP methods, so this talks to Chrome directly over a plain
//! `ws://127.0.0.1` WebSocket (`tungstenite`, no TLS, no async runtime):
//! launch Chrome with `--remote-debugging-port=0`, read the port from the
//! profile's `DevToolsActivePort` file, attach to a page target with a flat
//! session and `Runtime.evaluate` / `Input.dispatch*` / `Page.navigate`.
//! Chrome only ever listens on the loopback interface.

use crate::linkedin_humanize::{BBox, PageDriver, Selector};
use crate::loop_plugins::linkedin_autopilot::BrowserError;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);
const VIEWPORT: (u32, u32) = (1440, 900);

#[derive(Debug, Clone, PartialEq)]
pub enum CdpError {
    Timeout,
    Closed,
    Protocol(String),
    Io(String),
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CdpError::Timeout => write!(f, "timed out"),
            CdpError::Closed => write!(f, "connection closed"),
            CdpError::Protocol(m) => write!(f, "{m}"),
            CdpError::Io(m) => write!(f, "{m}"),
        }
    }
}

impl From<CdpError> for BrowserError {
    fn from(e: CdpError) -> Self {
        match e {
            CdpError::Timeout => BrowserError::PageTimeout,
            other => BrowserError::Other {
                class: "CdpError".into(),
                detail: other.to_string(),
            },
        }
    }
}

/// One WebSocket to the browser endpoint; sessions are multiplexed by id.
pub struct Cdp {
    ws: WebSocket<TcpStream>,
    next_id: u64,
    pub events: VecDeque<Value>,
}

impl Cdp {
    pub fn connect(port: u16, path: &str) -> Result<Self, CdpError> {
        use tungstenite::client::IntoClientRequest;
        let io = |e: std::io::Error| CdpError::Io(e.to_string());
        let stream = TcpStream::connect(("127.0.0.1", port)).map_err(io)?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(io)?;
        let request = format!("ws://127.0.0.1:{port}{path}")
            .into_client_request()
            .map_err(|e| CdpError::Protocol(e.to_string()))?;
        let (ws, _) = tungstenite::client(request, stream)
            .map_err(|e| CdpError::Protocol(format!("handshake: {e}")))?;
        ws.get_ref()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(io)?;
        Ok(Self {
            ws,
            next_id: 0,
            events: VecDeque::new(),
        })
    }

    /// Send one command and wait for its response, queueing events meanwhile.
    pub fn call(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CdpError> {
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.ws
            .send(Message::Text(msg.to_string()))
            .map_err(|e| match e {
                tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
                    CdpError::Closed
                }
                other => CdpError::Io(other.to_string()),
            })?;
        let deadline = Instant::now() + timeout;
        loop {
            match self.ws.read() {
                Ok(Message::Text(t)) => {
                    let Ok(v) = serde_json::from_str::<Value>(&t) else {
                        continue;
                    };
                    if v.get("id").and_then(Value::as_u64) == Some(id) {
                        return match v.get("error") {
                            Some(e) => Err(CdpError::Protocol(
                                e.get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("protocol error")
                                    .to_string(),
                            )),
                            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                        };
                    }
                    if v.get("method").is_some() {
                        if self.events.len() >= 1000 {
                            self.events.pop_front();
                        }
                        self.events.push_back(v);
                    }
                }
                Ok(Message::Close(_)) => return Err(CdpError::Closed),
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if Instant::now() >= deadline {
                        return Err(CdpError::Timeout);
                    }
                }
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    return Err(CdpError::Closed)
                }
                Err(e) => return Err(CdpError::Io(e.to_string())),
            }
            if Instant::now() >= deadline {
                return Err(CdpError::Timeout);
            }
        }
    }
}

pub type SharedCdp = Arc<Mutex<Cdp>>;

// ---- launching Chrome ---------------------------------------------------------------

/// The Chrome/Chromium executable: `LOOPX_CHROME` if set, else Google Chrome,
/// then Chromium (macOS app bundles, then common Linux names on `PATH`).
pub fn find_chrome() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LOOPX_CHROME").filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let apps = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ];
    if let Some(p) = apps.iter().map(PathBuf::from).find(|p| p.is_file()) {
        return Some(p);
    }
    let path = std::env::var_os("PATH")?;
    for name in ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"] {
        if let Some(p) = std::env::split_paths(&path)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
        {
            return Some(p);
        }
    }
    None
}

#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    pub chrome_path: Option<PathBuf>,
    pub extra_args: Vec<String>,
}

/// A running Chrome on a persistent profile, with its browser-level CDP socket.
pub struct Chrome {
    child: Option<Child>,
    pub cdp: SharedCdp,
}

impl Chrome {
    pub fn launch(profile: &Path, headless: bool, opts: &LaunchOptions) -> Result<Self, BrowserError> {
        let unavailable = |m: &str| BrowserError::Unavailable(m.to_string());
        let exe = opts
            .chrome_path
            .clone()
            .or_else(find_chrome)
            .ok_or_else(|| unavailable("Google Chrome was not found"))?;
        let port_file = profile.join("DevToolsActivePort");
        let _ = std::fs::remove_file(&port_file); // a stale file from a crashed run
        let mut cmd = Command::new(exe);
        cmd.arg(format!("--user-data-dir={}", profile.display()))
            .arg("--remote-debugging-port=0")
            .arg(format!("--window-size={},{}", VIEWPORT.0, VIEWPORT.1))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-blink-features=AutomationControlled",
                "--disable-session-crashed-bubble",
                "--hide-crash-restore-bubble",
            ])
            .args(&opts.extra_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if headless {
            cmd.arg("--headless=new");
        }
        cmd.arg("about:blank");
        let mut child = cmd
            .spawn()
            .map_err(|_| unavailable("Chrome could not be started"))?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let (port, path) = loop {
            if let Some((p, path)) = read_port_file(&port_file) {
                break (p, path);
            }
            // Chrome exits at once when the profile is already open in another Chrome.
            if matches!(child.try_wait(), Ok(Some(_))) || Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(unavailable("Chrome could not be started"));
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        match Cdp::connect(port, &path) {
            Ok(cdp) => Ok(Self {
                child: Some(child),
                cdp: Arc::new(Mutex::new(cdp)),
            }),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(unavailable("Chrome could not be started"))
            }
        }
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value, CdpError> {
        let mut c = self.cdp.lock().unwrap_or_else(|e| e.into_inner());
        c.call(None, method, params, DEFAULT_CALL_TIMEOUT)
    }

    /// Attach to the first page (or a new one) with a flat session.
    pub fn page(&self) -> Result<CdpPage, BrowserError> {
        let targets = self.call("Target.getTargets", json!({}))?;
        let existing = targets["targetInfos"]
            .as_array()
            .and_then(|a| a.iter().find(|t| t["type"] == "page"))
            .and_then(|t| t["targetId"].as_str().map(str::to_string));
        let target_id = match existing {
            Some(id) => id,
            None => self.call("Target.createTarget", json!({"url": "about:blank"}))?["targetId"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        };
        let attached = self.call(
            "Target.attachToTarget",
            json!({"targetId": target_id, "flatten": true}),
        )?;
        let session = attached["sessionId"]
            .as_str()
            .ok_or_else(|| BrowserError::Other {
                class: "CdpError".into(),
                detail: "no session".into(),
            })?
            .to_string();
        let page = CdpPage::new(self.cdp.clone(), session);
        page.send("Page.enable", json!({}))?;
        Ok(page)
    }

    /// Close gracefully so cookies reach the profile, then make sure the process is gone.
    pub fn close(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        {
            let mut c = self.cdp.lock().unwrap_or_else(|e| e.into_inner());
            let _ = c.call(None, "Browser.close", json!({}), Duration::from_secs(5));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for Chrome {
    fn drop(&mut self) {
        self.close();
    }
}

fn read_port_file(path: &Path) -> Option<(u16, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let port = lines.next()?.trim().parse::<u16>().ok()?;
    let path = lines.next()?.trim().to_string();
    path.starts_with('/').then_some((port, path))
}

// ---- a page -------------------------------------------------------------------------

pub struct CdpPage {
    cdp: SharedCdp,
    session: String,
    pos: (f64, f64),
}

fn js_error(detail: &str) -> BrowserError {
    BrowserError::Other {
        class: "JsError".into(),
        detail: detail.to_string(),
    }
}

/// The JS that finds the first visible match and returns its viewport box.
pub fn box_script(selector: &Selector) -> String {
    let find = match selector {
        Selector::Css(css) => format!(
            "[...document.querySelectorAll({})].find(vis)",
            serde_json::to_string(css).unwrap_or_default()
        ),
        Selector::Text { tags, text } => format!(
            "[...document.querySelectorAll({})].find((e) => vis(e) && (e.innerText || '').toLowerCase().includes({}))",
            serde_json::to_string(&tags.join(",")).unwrap_or_default(),
            serde_json::to_string(&text.to_lowercase()).unwrap_or_default()
        ),
    };
    format!(
        "() => {{ const vis = (e) => {{ const r = e.getBoundingClientRect(); const s = getComputedStyle(e); \
         return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none'; }}; \
         const el = {find}; if (!el) return null; \
         el.scrollIntoView({{block: 'center', inline: 'center'}}); \
         const r = el.getBoundingClientRect(); return {{x: r.x, y: r.y, width: r.width, height: r.height}}; }}"
    )
}

impl CdpPage {
    pub fn new(cdp: SharedCdp, session: String) -> Self {
        Self {
            cdp,
            session,
            pos: (400.0, 300.0),
        }
    }

    pub fn send(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
        self.send_timeout(method, params, DEFAULT_CALL_TIMEOUT)
    }

    fn send_timeout(&self, method: &str, params: Value, t: Duration) -> Result<Value, BrowserError> {
        let mut c = self.cdp.lock().unwrap_or_else(|e| e.into_inner());
        Ok(c.call(Some(&self.session), method, params, t)?)
    }

    fn eval(&self, expression: &str) -> Result<Value, BrowserError> {
        let r = self.send(
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
        )?;
        if let Some(ex) = r.get("exceptionDetails") {
            let text = ex["exception"]["description"]
                .as_str()
                .or(ex["text"].as_str())
                .unwrap_or("script error");
            return Err(js_error(text));
        }
        Ok(r["result"]["value"].clone())
    }

    fn key(&self, params: Value) -> Result<(), BrowserError> {
        self.send("Input.dispatchKeyEvent", params).map(|_| ())
    }
}

impl PageDriver for CdpPage {
    fn goto(&mut self, url: &str, timeout_s: f64) -> Result<(), BrowserError> {
        let timeout = Duration::from_secs_f64(timeout_s.max(1.0));
        let started = Instant::now();
        let nav = self.send_timeout("Page.navigate", json!({"url": url}), timeout)?;
        if let Some(err) = nav.get("errorText").and_then(Value::as_str) {
            return Err(BrowserError::Other {
                class: "NavigationError".into(),
                detail: err.to_string(),
            });
        }
        // DOMContentLoaded: the new document exists once navigate returns; wait for it to leave "loading".
        loop {
            if matches!(self.eval("document.readyState").ok().as_ref().and_then(Value::as_str),
                Some("interactive" | "complete"))
            {
                return Ok(());
            }
            if started.elapsed() >= timeout {
                return Err(BrowserError::PageTimeout);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn url(&mut self) -> Result<String, BrowserError> {
        Ok(self.eval("location.href")?.as_str().unwrap_or_default().to_string())
    }

    fn content(&mut self) -> Result<String, BrowserError> {
        Ok(self
            .eval("document.documentElement ? document.documentElement.outerHTML : ''")?
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    fn evaluate(&mut self, js: &str) -> Result<Value, BrowserError> {
        self.eval(&format!("({js})()"))
    }

    fn visible_box(&mut self, selector: &Selector, timeout_s: f64) -> Result<Option<BBox>, BrowserError> {
        let script = box_script(selector);
        let started = Instant::now();
        loop {
            let v = self.evaluate(&script)?;
            if v.is_object() {
                let n = |k: &str| v[k].as_f64().unwrap_or(0.0);
                return Ok(Some(BBox {
                    x: n("x"),
                    y: n("y"),
                    width: n("width"),
                    height: n("height"),
                }));
            }
            if started.elapsed().as_secs_f64() >= timeout_s {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn mouse_move(&mut self, x: f64, y: f64) -> Result<(), BrowserError> {
        self.pos = (x, y);
        self.send("Input.dispatchMouseEvent", json!({"type": "mouseMoved", "x": x, "y": y}))
            .map(|_| ())
    }

    fn mouse_wheel(&mut self, dy: f64) -> Result<(), BrowserError> {
        self.send(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseWheel", "x": self.pos.0, "y": self.pos.1, "deltaX": 0, "deltaY": dy}),
        )
        .map(|_| ())
    }

    fn mouse_click(&mut self, x: f64, y: f64) -> Result<(), BrowserError> {
        for kind in ["mousePressed", "mouseReleased"] {
            self.send(
                "Input.dispatchMouseEvent",
                json!({"type": kind, "x": x, "y": y, "button": "left", "clickCount": 1}),
            )?;
        }
        Ok(())
    }

    fn type_char(&mut self, ch: char) -> Result<(), BrowserError> {
        let s = ch.to_string();
        self.key(json!({"type": "keyDown", "key": s, "text": s}))?;
        self.key(json!({"type": "keyUp", "key": s}))
    }

    fn press_enter(&mut self) -> Result<(), BrowserError> {
        let enter = json!({"key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13});
        let mut down = enter.clone();
        down["type"] = json!("keyDown");
        down["text"] = json!("\r");
        let mut up = enter;
        up["type"] = json!("keyUp");
        self.key(down)?;
        self.key(up)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_file_is_parsed() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("DevToolsActivePort");
        std::fs::write(&p, "9222\n/devtools/browser/abc-123\n").unwrap();
        assert_eq!(read_port_file(&p), Some((9222, "/devtools/browser/abc-123".into())));
        std::fs::write(&p, "9222\n").unwrap();
        assert_eq!(read_port_file(&p), None);
        std::fs::write(&p, "x\n/y\n").unwrap();
        assert_eq!(read_port_file(&p), None);
    }

    #[test]
    fn box_script_quotes_its_inputs() {
        let s = box_script(&Selector::Css("a[href=\"x\"]".into()));
        assert!(s.contains(r#"querySelectorAll("a[href=\"x\"]")"#));
        let t = box_script(&Selector::Text {
            tags: vec!["button".into(), "a".into()],
            text: "People".into(),
        });
        assert!(t.contains(r#"querySelectorAll("button,a")"#) && t.contains("\"people\""));
    }
}
