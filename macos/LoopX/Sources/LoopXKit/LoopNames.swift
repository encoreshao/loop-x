import Foundation

/// How a loop id reads on the island and in notifications: `gitlab-loop` is "GitLab",
/// `inbox-triage-loop` is "Inbox Triage". The built-ins use the dashboard's names; any other
/// id drops `-loop` and is title-cased, keeping brand spellings. The id itself stays the key
/// for actions (Run again, queue).
public enum LoopNames {
    private static let named: [String: String] = [
        "gitlab-loop": "GitLab",
        "topic-loop": "Topic Monitor",
        "inbox-triage-loop": "Inbox Triage",
        "stale-sweeper-loop": "Stale Work Sweeper",
    ]

    private static let words: [String: String] = [
        "gitlab": "GitLab", "github": "GitHub", "linkedin": "LinkedIn",
        "mr": "MR", "rss": "RSS", "ai": "AI", "ci": "CI", "pr": "PR",
    ]

    public static func display(_ id: String) -> String {
        if let name = named[id] { return name }
        let base = id.hasSuffix("-loop") ? String(id.dropLast("-loop".count)) : id
        return titleCase(base) ?? id
    }

    /// A topic's name as given, unless it is a slug (`ai-agents`, `rust_news`): then "AI Agents".
    /// A name typed with spaces or capitals is the user's own spelling and is kept.
    public static func topic(_ name: String) -> String {
        let slug = name == name.lowercased() && !name.contains(" ") && name.contains(where: { $0 == "-" || $0 == "_" })
        return slug ? (titleCase(name) ?? name) : name
    }

    private static func titleCase(_ s: String) -> String? {
        let parts = s.split(whereSeparator: { $0 == "-" || $0 == "_" }).map { word -> String in
            let w = word.lowercased()
            return words[w] ?? w.prefix(1).uppercased() + w.dropFirst()
        }
        return parts.isEmpty ? nil : parts.joined(separator: " ")
    }

    private static let symbols: [String: String] = [
        "gitlab-loop": "arrow.triangle.branch",
        "topic-loop": "dot.radiowaves.left.and.right",
        "inbox-triage-loop": "tray.full",
        "daily-digest-loop": "newspaper",
        "mr-review-loop": "checklist",
        "rss-watch-loop": "antenna.radiowaves.left.and.right",
        "linkedin-autopilot-loop": "person.2.wave.2",
        "meeting-prep-loop": "calendar.badge.clock",
        "stale-sweeper-loop": "wind",
    ]

    /// The SF Symbol a loop's tile shows on the island; any other loop gets a generic one.
    public static func symbol(_ id: String) -> String { symbols[id] ?? "repeat" }
}
