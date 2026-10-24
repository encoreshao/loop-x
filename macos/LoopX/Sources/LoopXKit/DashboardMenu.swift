import Foundation

/// The dashboard pages the Mac app's menus open. Sections mirror the web sidebar (`_NAV_GROUPS`), with
/// the views that otherwise take a click inside a page (Logs, Daemons, Cost, ...) as direct entries.
public enum DashboardMenu {
    public struct Entry: Equatable, Sendable {
        public let title: String
        public let path: String
        public init(_ title: String, _ path: String) { self.title = title; self.path = path }
    }
    public struct Section: Equatable, Sendable {
        public let title: String
        public let entries: [Entry]
    }

    public static let home = Entry("Dashboard", "/")

    public static let sections: [Section] = [
        Section(title: "Loops", entries: [
            Entry("All Loops", "/loops"),
            Entry("GitLab Issues", "/loops/gitlab-loop"),
            Entry("Topic Monitor", "/loops/topic-loop"),
            Entry("Inbox Triage", "/loops/inbox-triage-loop"),
        ]),
        Section(title: "Observe", entries: [
            Entry("Runs", "/runs"),
            Entry("History", "/runs?view=history"),
            Entry("Logs", "/runs?view=logs"),
            Entry("Insights", "/insights"),
            Entry("Cost", "/insights?view=cost"),
            Entry("Budget", "/insights?view=budget"),
            Entry("Memory", "/insights?view=memory"),
            Entry("Harness", "/harness"),
        ]),
        Section(title: "System", entries: [
            Entry("Connectors", "/connectors"),
            Entry("Settings", "/settings"),
            Entry("Daemons", "/settings?view=daemons"),
            Entry("Skills", "/settings?view=skills"),
        ]),
    ]

    /// External links shown in the app's menus; they open in the default browser.
    public static let links: [(title: String, url: URL)] = [
        ("GitHub Repository", URL(string: "https://github.com/encoreshao/loop-x")!),
        ("Releases", URL(string: "https://github.com/encoreshao/loop-x/releases")!),
        ("Report an Issue", URL(string: "https://github.com/encoreshao/loop-x/issues")!),
    ]

    public static func url(for path: String, base: URL) -> URL? {
        URL(string: path, relativeTo: base)?.absoluteURL
    }
}
