import Foundation

public enum IslandTone: Equatable, Sendable { case neutral, active, success, warning, failure }

public struct IslandRow: Equatable, Identifiable, Sendable {
    public var id: String
    public var time: Date?
    public var title: String
    public var tone: IslandTone
}

public enum IslandChip: Equatable, Sendable {
    case openMergeRequest(URL)
    case openDashboard
    case runAgain(loop: String)
    case dismiss
}

public struct IslandDetail: Equatable, Sendable {
    public var rows: [IslandRow]
    public var result: String?
    public var chips: [IslandChip]
    public static let maxRows = 6

    public static func make(task: NotchTask, events: [RecentEvent], attention: Attention?) -> IslandDetail {
        let issue = task.detail.flatMap { $0.components(separatedBy: " · ").first }
        let scoped = events.filter { e in
            guard let issue else { return true }
            return NotchState.describe(project: e.project, iid: e.issueIid, fallback: nil) == issue
        }
        let rows = scoped.reversed().prefix(maxRows).enumerated().map { i, e in
            IslandRow(id: "\(e.type)#\(e.at ?? "")#\(i)", time: e.at.flatMap(Timestamps.parse),
                      title: title(e.type), tone: tone(e.type))
        }

        var result: String?
        var chips: [IslandChip] = []
        if let a = attention {
            let who = NotchState.describe(project: a.project, iid: a.issueIid, fallback: nil)
            switch a.kind {
            case .mrOpened:
                result = "Merge request opened" + (who.isEmpty ? "" : " · \(who)")
                if let url = a.mrURL.flatMap(URL.init(string:)), ["http", "https"].contains(url.scheme?.lowercased()) {
                    chips.append(.openMergeRequest(url))
                }
            case .escalated: result = a.reason ?? "Needs your attention"
            case .failed: result = a.reason ?? "Run failed"
            }
        }
        if task.state != .running { chips.append(.runAgain(loop: loopName(for: task))) }
        chips.append(.openDashboard)
        return IslandDetail(rows: Array(rows), result: result, chips: chips)
    }

    /// The runnable loop behind a task: topics all run under the topic monitor.
    static func loopName(for task: NotchTask) -> String {
        task.id.hasPrefix("topic:") ? "topic-loop" : task.name
    }

    static func title(_ type: String) -> String {
        let words = type.replacingOccurrences(of: ".", with: " ").replacingOccurrences(of: "_", with: " ")
        return words.prefix(1).uppercased() + words.dropFirst()
    }

    static func tone(_ type: String) -> IslandTone {
        switch type {
        case "issue.completed", "run.completed": return .success
        case "run.failed", "loop.failed": return .failure
        case "issue.escalated": return .warning
        case "run.started": return .active
        default: return .neutral
        }
    }
}
