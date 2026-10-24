import Foundation

/// What a `loopx://` link asks the app to do. Any other app can open one of these (Shortcuts' "Open URL",
/// Spotlight, a script), so parsing is an allowlist: only loops the app itself offers to start, and only
/// dashboard pages its own menus open.
public enum LoopXURL: Equatable, Sendable {
    case run(loop: String)
    case open(path: String)

    public static func parse(_ url: URL) -> LoopXURL? {
        guard url.scheme?.lowercased() == "loopx",
              let c = URLComponents(url: url, resolvingAgainstBaseURL: false) else { return nil }
        switch c.host?.lowercased() {
        case "run":
            let segments = c.path.split(separator: "/", omittingEmptySubsequences: false).dropFirst()
            guard segments.count == 1, let loop = segments.first.map(String.init),
                  IslandStarts.all.contains(where: { $0.loop == loop }) else { return nil }
            return .run(loop: loop)
        case "open":
            let path = c.path.isEmpty ? "/" : c.path
            let candidate = path + (c.percentEncodedQuery.map { "?" + $0 } ?? "")
            let pages = [DashboardMenu.home.path] + DashboardMenu.sections.flatMap(\.entries).map(\.path)
            return pages.contains(candidate) ? .open(path: candidate) : nil
        default:
            return nil
        }
    }
}
