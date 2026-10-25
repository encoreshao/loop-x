import Foundation

/// What the Mac app injects into the dashboard so its own bars double as the window's title bar: the
/// window uses a transparent, full-size title bar, so the page runs under the traffic lights, keeps
/// the brand clear of them, and lets an empty stretch of the topbar or sidebar header move the window.
/// Injected by the app rather than shipped in the page's CSS, so a browser tab looks as before.
public enum TitlebarChrome {
    public static let messageName = "loopxWindow"
    /// A strip the start screen puts along its top edge, since it has no topbar.
    public static let dragStripClass = "loopx-drag"
    public static let interactiveSelector = "a,button,input,select,textarea,label,summary,[role=button],[contenteditable]"

    public static let css = """
    .loopx-app .sidebar-top { padding-top: 28px; }
    .loopx-app .topbar, .loopx-app .sidebar-top { -webkit-user-select: none; user-select: none; }
    """

    public static var script: String {
        """
        (function () {
          var root = document.documentElement;
          root.classList.add('loopx-app');
          var style = document.createElement('style');
          style.textContent = \(jsString(css));
          root.appendChild(style);
          function dragZone(e) {
            if (e.button !== 0 || !e.target.closest) return false;
            if (e.target.closest('\(interactiveSelector)')) return false;
            return !!e.target.closest('.topbar, .sidebar-top, .\(dragStripClass)');
          }
          function post(kind) { window.webkit.messageHandlers.\(messageName).postMessage(kind); }
          document.addEventListener('mousedown', function (e) {
            if (dragZone(e)) post(e.detail > 1 ? 'zoom' : 'drag');
          }, true);
          document.addEventListener('dblclick', function (e) { if (dragZone(e)) e.preventDefault(); }, true);
        })();
        """
    }

    private static func jsString(_ s: String) -> String {
        let data = try? JSONSerialization.data(withJSONObject: [s])
        let arr = data.flatMap { String(data: $0, encoding: .utf8) } ?? "[\"\"]"
        return String(arr.dropFirst().dropLast())
    }
}
