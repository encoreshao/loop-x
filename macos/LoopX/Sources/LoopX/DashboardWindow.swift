import AppKit
import WebKit
import LoopXKit

/// The dashboard: a plain WKWebView on the backend's own pages. Same server a browser tab would use,
/// so having both open at once is fine.
@MainActor
final class DashboardWindowController: NSObject, WKNavigationDelegate, WKUIDelegate {
    private(set) var window: NSWindow?
    private(set) var webView: WKWebView?
    private var loadedBase: URL?
    var baseURLProvider: () -> URL? = { nil }
    var modeProvider: () -> DashboardWindowMode = { .maximized }
    private var sizedThisSession = false
    /// Why there is no backend yet ("Starting…" or the failure), shown in place of the page.
    var statusProvider: () -> (text: String, failed: Bool) = { ("Starting the backend…", false) }
    /// Asked to bring a backend up when the window opens without one.
    var onNeedsBackend: () -> Void = {}
    /// Shown in the placeholder's footer.
    var appVersion = ""

    func show() {
        ensureWindow()
        guard let base = baseURLProvider() else {
            showPlaceholder()
            loadedBase = nil
            onNeedsBackend()
            present(); return
        }
        if loadedBase?.port != base.port {
            webView?.load(URLRequest(url: base)); loadedBase = base
        }
        present()
    }

    /// Opens one dashboard page (`/runs?view=logs`) in the window, starting the backend first if needed.
    func open(path: String) {
        guard let base = baseURLProvider(), let url = DashboardMenu.url(for: path, base: base) else { show(); return }
        ensureWindow()
        webView?.load(URLRequest(url: url)); loadedBase = base
        present()
    }

    var currentURL: URL? { webView?.url }
    /// Called with the accent and color mode the dashboard page has stored (nil when none is chosen).
    var onTheme: (_ accent: String?, _ colorMode: String?) -> Void = { _, _ in }

    /// The dashboard keeps its theme in the page's localStorage; read it so native windows can follow.
    func readTheme() {
        guard let wv = webView, wv.url?.host == "127.0.0.1" || wv.url?.host == "localhost" else { return }
        let js = "JSON.stringify([localStorage.getItem('loop-dashboard-accent'), localStorage.getItem('loop-dashboard-color-mode')])"
        wv.evaluateJavaScript(js) { [weak self] result, _ in
            guard let s = result as? String, let data = s.data(using: .utf8),
                  let a = try? JSONSerialization.jsonObject(with: data) as? [Any?], a.count == 2 else { return }
            self?.onTheme(a[0] as? String, a[1] as? String)
        }
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) { readTheme() }
    func reload() { webView?.reload() }
    func goBack() { webView?.goBack() }
    func goForward() { webView?.goForward() }

    private func showPlaceholder() {
        let s = statusProvider()
        webView?.loadHTMLString(Self.placeholder(s.text, failed: s.failed, version: appVersion), baseURL: nil)
    }

    /// The backend's state changed while the placeholder is up (starting -> failed): redraw it.
    func refreshPlaceholder() {
        guard window != nil, loadedBase == nil, baseURLProvider() == nil else { return }
        showPlaceholder()
    }

    private func present() {
        NSApp.activate(ignoringOtherApps: true)
        window?.makeKeyAndOrderFront(nil)
        applyModeOnce()
    }

    /// The first time the window is shown in a session it fills the screen's visible area (or goes
    /// native full screen, or stays the remembered standard window, per Settings); after that the user's own sizing is left alone.
    private func applyModeOnce() {
        guard !sizedThisSession, let w = window else { return }
        sizedThisSession = true
        switch modeProvider() {
        case .fullScreen:
            if !w.styleMask.contains(.fullScreen) { w.toggleFullScreen(nil) }
        case .maximized:
            if let f = (w.screen ?? NSScreen.main)?.visibleFrame { w.setFrame(f, display: true, animate: false) }
        case .standard:
            break
        }
    }

    func ensureWindow() {
        guard window == nil else { return }
        let cfg = WKWebViewConfiguration()
        cfg.websiteDataStore = .default()
        let wv = WKWebView(frame: .zero, configuration: cfg)
        wv.navigationDelegate = self
        wv.uiDelegate = self
        wv.allowsBackForwardNavigationGestures = true
        wv.isInspectable = true   // Developer > Web Inspector; the page is the local backend's own
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1180, height: 780),
                         styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
        w.title = "LoopX"
        w.collectionBehavior.insert(.fullScreenPrimary)
        w.contentView = wv
        w.isReleasedWhenClosed = false
        w.setFrameAutosaveName("LoopXDashboard")
        w.center()
        window = w; webView = wv
    }

    /// Reload after the backend moved to another port.
    func backendChanged() {
        guard window != nil, let base = baseURLProvider(), loadedBase?.port != base.port else { return }
        webView?.load(URLRequest(url: base)); loadedBase = base
    }

    // Only the local backend loads in-window; every other link goes to the default browser.
    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction,
                 decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
        let url = action.request.url
        if url?.scheme == "loopx-retry" {
            decisionHandler(.cancel)
            onNeedsBackend(); showPlaceholder()
            return
        }
        if url?.scheme == "mailto", let url { NSWorkspace.shared.open(url); decisionHandler(.cancel); return }
        let local = url?.host == "127.0.0.1" || url?.host == "localhost" || url?.scheme == "about"
        if local { decisionHandler(.allow); return }
        if let url, ["http", "https"].contains(url.scheme?.lowercased()) { NSWorkspace.shared.open(url) }
        decisionHandler(.cancel)
    }

    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                             for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        if let url = action.request.url, ["http", "https"].contains(url.scheme?.lowercased()) { NSWorkspace.shared.open(url) }
        return nil
    }

    /// The page shown until a backend answers: the logo, name, what LoopX does, one status line (a
    /// spinner while starting, the reason and a Retry link when it failed) and the author. Follows the
    /// system appearance. Only the logo's glow is decorative motion, so it is gated on reduced motion.
    static func placeholder(_ text: String, failed: Bool, version: String = "") -> String {
        func escape(_ t: String) -> String {
            t.replacingOccurrences(of: "&", with: "&amp;").replacingOccurrences(of: "<", with: "&lt;")
        }
        let status = failed
            ? "<div class='msg err'>\(escape(text))</div><a class='retry' href='loopx-retry://now'>Try again</a>"
            : "<div class='row'><span class='spin'></span><span class='msg'>\(escape(text))</span></div>"
        let chips = ["Fix your GitLab issues", "Triage your inbox", "Brief you on topics", "Prep your meetings", "Start days informed"]
            .map { "<span class='chip'>\($0)</span>" }.joined()
        let versionLine = version.isEmpty ? "" : "<span>v\(escape(version))</span><span class='dot'>&middot;</span>"
        return """
        <html><head><meta name='color-scheme' content='light dark'><style>
        :root{--fg:#1d1d1f;--sub:#6e6e73;--err:#c0392b;--bg:#fff;--accent:#4f6df5;--chip:#f2f3f7;--glow:rgba(79,109,245,.22)}
        @media(prefers-color-scheme:dark){:root{--fg:#f5f5f7;--sub:#98989d;--err:#ff6b5e;--bg:#1e1e1e;--accent:#7aa2ff;--chip:#2b2c31;--glow:rgba(122,162,255,.28)}}
        html,body{height:100%;margin:0;background:var(--bg)}
        body{font:14px -apple-system,system-ui;color:var(--sub);display:flex;flex-direction:column;align-items:center;justify-content:center;gap:16px;text-align:center}
        .logo{position:relative;width:240px;height:240px}
        .logo::before{content:'';position:absolute;inset:-18%;border-radius:50%;background:radial-gradient(circle,var(--glow),transparent 68%)}
        .logo svg{position:relative;width:240px;height:240px;display:block}
        @media(prefers-reduced-motion:no-preference){.logo::before{animation:glow 3.2s ease-in-out infinite}}
        @keyframes glow{50%{transform:scale(1.12);opacity:.65}}
        h1{margin:0;font-size:44px;font-weight:700;color:var(--fg);letter-spacing:.3px}
        .tag{margin:-6px 0 0;font-size:16px;color:var(--sub);max-width:460px;line-height:1.45}
        .chips{display:flex;flex-wrap:wrap;gap:8px;justify-content:center;max-width:520px}
        .chip{padding:5px 12px;border-radius:999px;background:var(--chip);color:var(--fg);font-size:12.5px;font-weight:550}
        .row{display:flex;align-items:center;gap:10px;margin-top:6px}
        .spin{width:14px;height:14px;border:2px solid var(--sub);border-top-color:var(--accent);border-radius:50%;animation:s 0.9s linear infinite}
        @keyframes s{to{transform:rotate(360deg)}}
        .err{color:var(--err);max-width:420px;line-height:1.4}
        .retry{color:var(--accent);text-decoration:none;font-weight:600;padding:6px 14px;border:1px solid var(--accent);border-radius:8px}
        footer{position:fixed;bottom:22px;left:0;right:0;font-size:12.5px;line-height:1.7}
        footer .dot{margin:0 8px}
        footer a{color:var(--accent);text-decoration:none}
        </style></head><body>
        <div class='logo'>\(LogoSVG.markup)</div><h1>LoopX</h1>
        <p class='tag'>Get your time back from the daily grind of issues, email and status checks.</p>
        <div class='chips'>\(chips)</div>\(status)
        <footer>\(versionLine)<span>Created by <strong>Encore Shao</strong></span><span class='dot'>&middot;</span><a href='mailto:encore.shao@gmail.com'>encore.shao@gmail.com</a></footer>
        </body></html>
        """
    }
}
