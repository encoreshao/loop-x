import Foundation

/// One line of the island's task list: a loop or topic and what it is doing right now.
public struct NotchTask: Equatable, Sendable, Identifiable {
    public enum State: String, Equatable, Sendable { case running, queued, failed, idle }
    public var id: String
    public var name: String
    public var state: State
    /// The message that says what the task is doing (issue, step), nil when it has none.
    public var detail: String?
}

/// A Mac notification worth posting for a task's change of state.
public struct TaskNotification: Equatable, Sendable {
    public enum Kind: String, Equatable, Sendable { case started, completed, failed }
    public var id: String
    public var kind: Kind
    public var title: String
    public var body: String?
}

public enum TaskNotifications {
    /// Every loop and topic that has anything to say, running first, then failed, then idle.
    public static func tasks(in s: Snapshot) -> [NotchTask] {
        var out: [NotchTask] = []
        let waiting = Set(s.queue.queued.map(\.loop))
        for (name, l) in s.loops {
            // A loop waiting its turn shows once, as queued, not also as idle.
            if waiting.contains(name), state(l.state) == .idle { continue }
            let detail = [l.currentIssue, l.currentStep].compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · ")
            out.append(NotchTask(id: "loop:\(name)", name: name, state: state(l.state),
                                 detail: detail.isEmpty ? nil : detail))
        }
        for (name, t) in s.topics {
            out.append(NotchTask(id: "topic:\(name)", name: name, state: state(t.state),
                                 detail: t.currentStep.flatMap { $0.isEmpty ? nil : $0 }))
        }
        let line = s.queue.queued.enumerated().map { i, e in
            NotchTask(id: "queue:\(e.id)", name: e.loop, state: .queued, detail: "#\(i + 1) in line · \(e.reason)")
        }
        func rank(_ t: NotchTask) -> Int {
            switch t.state { case .running: return 0; case .queued: return 1; case .failed: return 2; case .idle: return 3 }
        }
        // Queued rows keep their place in line; the rest sort by name.
        let rest = out.sorted { (rank($0), $0.name) < (rank($1), $1.name) }
        let running = rest.filter { $0.state == .running }
        return running + line + rest.filter { $0.state != .running }
    }

    /// Notifications for what changed between two snapshots. Nothing for the first snapshot after
    /// launch (`old == nil`): a task already running when the app opens is not news.
    public static func changes(from old: Snapshot?, to new: Snapshot) -> [TaskNotification] {
        guard let old else { return [] }
        let before = Dictionary(uniqueKeysWithValues: tasks(in: old).map { ($0.id, $0) })
        var out: [TaskNotification] = []
        for t in tasks(in: new) {
            let prev = before[t.id]?.state ?? .idle
            guard prev != t.state else { continue }
            switch (prev, t.state) {
            case (_, .running):
                out.append(TaskNotification(id: "\(t.id)#started", kind: .started, title: "\(t.name) started", body: t.detail))
            case (.running, .idle):
                out.append(TaskNotification(id: "\(t.id)#completed", kind: .completed, title: "\(t.name) completed", body: nil))
            case (_, .failed):
                out.append(TaskNotification(id: "\(t.id)#failed", kind: .failed, title: "\(t.name) failed", body: t.detail))
            default: break
            }
        }
        let known = Set(old.approvals.pending.map(\.id))
        for a in new.approvals.pending where a.isPending && !known.contains(a.id) {
            out.append(TaskNotification(id: "approval:\(a.id)", kind: .started, title: "Approval needed",
                                        body: NotchState.describe(project: a.project, iid: a.issueIid, fallback: a.title)))
        }
        return out
    }

    private static func state(_ raw: String) -> NotchTask.State {
        switch raw {
        case "running": return .running
        case "failed", "stale": return .failed
        default: return .idle
        }
    }
}
