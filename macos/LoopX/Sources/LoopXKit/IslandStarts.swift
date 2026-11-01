import Foundation

public enum IslandStarts {
    public struct Start: Equatable, Sendable { public var loop: String; public var title: String; public var symbol: String }

    public static let all: [Start] = [
        Start(loop: "topic-loop", title: "Run Topic Monitor", symbol: "dot.radiowaves.left.and.right"),
        Start(loop: "gitlab-loop", title: "Run GitLab loop", symbol: "arrow.triangle.branch"),
        Start(loop: "inbox-triage-loop", title: "Triage inbox", symbol: "tray"),
    ]

    /// Hides a start whose loop is already running (topics all share `topic-loop`).
    public static func available(tasks: [NotchTask]) -> [Start] {
        let running = Set(tasks.filter { $0.state == .running }.map { $0.id.hasPrefix("topic:") ? "topic-loop" : $0.name })
        return all.filter { !running.contains($0.loop) }
    }

    /// The loop a row's Run button starts: any loop row that is idle or failed. Running and queued
    /// rows are already on their way; topics have no button of their own (Topic Monitor runs them all).
    public static func runnableLoop(for t: NotchTask) -> String? {
        guard t.id.hasPrefix("loop:"), t.state == .idle || t.state == .failed else { return nil }
        return t.name
    }

    public static func confirmText(loop: String) -> String {
        switch loop {
        case "gitlab-loop": return "Run the GitLab loop now? It acts on your real GitLab issues and Slack."
        case "topic-loop": return "Run the Topic Monitor now? It posts briefings to Slack."
        case "inbox-triage-loop": return "Triage your inbox now? It reads your mail and may draft replies."
        default: return "Run \(LoopNames.display(loop)) now?"
        }
    }
}
