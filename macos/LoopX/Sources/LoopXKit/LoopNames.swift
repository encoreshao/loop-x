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
        let parts = base.split(whereSeparator: { $0 == "-" || $0 == "_" }).map { word -> String in
            let w = word.lowercased()
            return words[w] ?? w.prefix(1).uppercased() + w.dropFirst()
        }
        return parts.isEmpty ? id : parts.joined(separator: " ")
    }
}
