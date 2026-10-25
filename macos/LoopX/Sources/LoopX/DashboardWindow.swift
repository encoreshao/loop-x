import AppKit
import WebKit
import LoopXKit

/// The dashboard: a plain WKWebView on the backend's own pages. Same server a browser tab would use,
/// so having both open at once is fine.
@MainActor
final class DashboardWindowController: NSObject, WKNavigationDelegate, WKUIDelegate, WKScriptMessageHandler {
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
        cfg.userContentController.addUserScript(WKUserScript(source: TitlebarChrome.script,
                                                             injectionTime: .atDocumentStart, forMainFrameOnly: true))
        cfg.userContentController.add(self, name: TitlebarChrome.messageName)
        let wv = WKWebView(frame: .zero, configuration: cfg)
        wv.navigationDelegate = self
        wv.uiDelegate = self
        wv.allowsBackForwardNavigationGestures = true
        wv.isInspectable = true   // Developer > Web Inspector; the page is the local backend's own
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1180, height: 780),
                         styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
                         backing: .buffered, defer: false)
        w.title = "LoopX"
        w.contentMinSize = NSSize(width: 700, height: 600)
        w.titleVisibility = .hidden
        w.titlebarAppearsTransparent = true
        w.collectionBehavior.insert(.fullScreenPrimary)
        w.contentView = wv
        w.isReleasedWhenClosed = false
        w.setFrameAutosaveName("LoopXDashboard")
        w.center()
        window = w; webView = wv
    }

    /// The page's empty topbar/sidebar header stands in for the title bar: drag moves, double-click zooms.
    func userContentController(_ c: WKUserContentController, didReceive message: WKScriptMessage) {
        guard let w = window, let kind = message.body as? String else { return }
        switch kind {
        case "drag": if let e = NSApp.currentEvent { w.performDrag(with: e) }
        case "zoom": w.performZoom(nil)
        default: break
        }
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

    /// The page shown until a backend answers (see `StartScreen`).
    static func placeholder(_ text: String, failed: Bool, version: String = "") -> String {
        StartScreen.html(text, failed: failed, version: version, logo: LogoSVG.markup)
    }
}
