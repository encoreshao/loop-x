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

    private func showPlaceholder() {
        let s = statusProvider()
        webView?.loadHTMLString(Self.placeholder(s.text, failed: s.failed), baseURL: nil)
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

    /// The page shown until a backend answers: logo, name and one status line (a spinner while
    /// starting, the reason and a Retry link when it failed). Follows the system appearance.
    static func placeholder(_ text: String, failed: Bool) -> String {
        let safe = text.replacingOccurrences(of: "&", with: "&amp;").replacingOccurrences(of: "<", with: "&lt;")
        let status = failed
            ? "<div class='msg err'>\(safe)</div><a class='retry' href='loopx-retry://now'>Try again</a>"
            : "<div class='row'><span class='spin'></span><span class='msg'>\(safe)</span></div>"
        return """
        <html><head><meta name='color-scheme' content='light dark'><style>
        :root{--fg:#1d1d1f;--sub:#6e6e73;--err:#c0392b;--bg:#fff;--accent:#4f6df5}
        @media(prefers-color-scheme:dark){:root{--fg:#f5f5f7;--sub:#98989d;--err:#ff6b5e;--bg:#1e1e1e;--accent:#7aa2ff}}
        html,body{height:100%;margin:0;background:var(--bg)}
        body{font:14px -apple-system,system-ui;color:var(--sub);display:flex;flex-direction:column;align-items:center;justify-content:center;gap:14px}
        .logo svg{width:96px;height:96px;display:block}
        h1{margin:0;font-size:26px;font-weight:650;color:var(--fg);letter-spacing:.2px}
        .row{display:flex;align-items:center;gap:10px}
        .spin{width:14px;height:14px;border:2px solid var(--sub);border-top-color:var(--accent);border-radius:50%;animation:s 0.9s linear infinite}
        @keyframes s{to{transform:rotate(360deg)}}
        .err{color:var(--err);max-width:420px;text-align:center;line-height:1.4}
        .retry{color:var(--accent);text-decoration:none;font-weight:600;padding:6px 14px;border:1px solid var(--accent);border-radius:8px}
        </style></head><body>
        <div class='logo'>\(LogoSVG.markup)</div><h1>LoopX</h1>\(status)
        </body></html>
        """
    }
}
