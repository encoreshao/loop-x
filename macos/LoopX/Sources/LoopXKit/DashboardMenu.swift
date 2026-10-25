import Foundation

/// The dashboard pages the Mac app's menus open. Sections mirror the web sidebar (`_NAV_GROUPS`), with
/// the views that otherwise take a click inside a page (Logs, Daemons, Cost, ...) as direct entries.
public enum DashboardMenu {
    /// A key equivalent, kept free of AppKit so it can be tested; the app maps it onto `NSMenuItem`.
    public struct Shortcut: Hashable, Sendable {
        public struct Modifiers: OptionSet, Hashable, Sendable {
            public let rawValue: Int
            public init(rawValue: Int) { self.rawValue = rawValue }
            public static let command = Modifiers(rawValue: 1)
            public static let shift = Modifiers(rawValue: 2)
            public static let option = Modifiers(rawValue: 4)
            public static let control = Modifiers(rawValue: 8)
        }
        public let key: String
        public let modifiers: Modifiers
        public init(_ key: String, _ modifiers: Modifiers = .command) { self.key = key; self.modifiers = modifiers }
    }

    public struct Entry: Equatable, Sendable {
        public let title: String
        public let path: String
        public let shortcut: Shortcut
        public init(_ title: String, _ path: String, _ shortcut: Shortcut) {
            self.title = title; self.path = path; self.shortcut = shortcut
        }
    }
    public struct Section: Equatable, Sendable {
        public let title: String
        public let entries: [Entry]
    }

    public static let home = Entry("Dashboard", "/", Shortcut("0"))

    public static let sections: [Section] = [
        Section(title: "Loops", entries: [
            Entry("All Loops", "/loops", Shortcut("1", [.command])),
            Entry("GitLab Issues", "/loops/gitlab-loop", Shortcut("2", [.command])),
            Entry("Topic Monitor", "/loops/topic-loop", Shortcut("3", [.command])),
            Entry("Inbox Triage", "/loops/inbox-triage-loop", Shortcut("4", [.command])),
        ]),
        Section(title: "Observe", entries: [
            Entry("Runs", "/runs", Shortcut("1", [.command, .option])),
            Entry("History", "/runs?view=history", Shortcut("2", [.command, .option])),
            Entry("Logs", "/runs?view=logs", Shortcut("3", [.command, .option])),
            Entry("Insights", "/insights", Shortcut("4", [.command, .option])),
            Entry("Cost", "/insights?view=cost", Shortcut("5", [.command, .option])),
            Entry("Budget", "/insights?view=budget", Shortcut("6", [.command, .option])),
            Entry("Memory", "/insights?view=memory", Shortcut("7", [.command, .option])),
            Entry("Harness", "/harness", Shortcut("8", [.command, .option])),
        ]),
        Section(title: "System", entries: [
            Entry("Connectors", "/connectors", Shortcut("1", [.command, .control])),
            Entry("Settings", "/settings", Shortcut("2", [.command, .control])),
            Entry("Daemons", "/settings?view=daemons", Shortcut("3", [.command, .control])),
            Entry("Skills", "/settings?view=skills", Shortcut("4", [.command, .control])),
        ]),
    ]

    /// External links shown in the app's menus; they open in the default browser.
    public static let links: [(title: String, url: URL, shortcut: Shortcut)] = [
        ("GitHub Repository", URL(string: "https://github.com/encoreshao/loop-x")!, Shortcut("g", [.command, .option])),
        ("Releases", URL(string: "https://github.com/encoreshao/loop-x/releases")!, Shortcut("l", [.command, .option])),
        ("Report an Issue", URL(string: "https://github.com/encoreshao/loop-x/issues")!, Shortcut("i", [.command, .option])),
    ]

    /// Shortcuts for the Developer menu's own actions (the page-navigation ones are ⌘[ ⌘] ⌘R).
    public static let developer: [String: Shortcut] = [
        "Open in Browser": Shortcut("b", [.command, .shift]),
        "Copy Dashboard URL": Shortcut("c", [.command, .shift]),
        "Reveal ~/.loopx in Finder": Shortcut("r", [.command, .shift]),
        "Install Command Line Tool…": Shortcut("i", [.command, .shift]),
    ]

    public static func url(for path: String, base: URL) -> URL? {
        URL(string: path, relativeTo: base)?.absoluteURL
    }
}
