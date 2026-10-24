import Foundation

/// The menu bar item's number: how many approvals are waiting for the user.
public enum StatusBadge {
    public static func text(for state: NotchState) -> String? {
        let n = state.approvals.count
        return n == 0 ? nil : (n > 9 ? "9+" : String(n))
    }

    public static func tooltip(count: Int) -> String { "\(count) approval\(count == 1 ? "" : "s") waiting" }
}

/// One line per loop that is not running, saying how it last ended, for the menu bar menu.
public enum LastRuns {
    public static let maxLines = 4

    public static func lines(in s: Snapshot, now: Date) -> [String] {
        let rows: [(Date, String)] = s.loops.compactMap { name, l in
            guard l.state != "running", let at = l.updatedAt.flatMap(Timestamps.parse) else { return nil }
            let how = l.isFailure ? "failed" : "last ran"
            return (at, "\(name): \(how) \(relative(from: at, to: now))")
        }
        return rows.sorted { ($1.0, $0.1) < ($0.0, $1.1) }.prefix(maxLines).map(\.1)
    }

    public static func relative(from date: Date, to now: Date) -> String {
        let s = now.timeIntervalSince(date)
        switch s {
        case ..<60: return "just now"
        case ..<3600: return "\(Int(s / 60))m ago"
        case ..<86400: return "\(Int(s / 3600))h ago"
        default: return "\(Int(s / 86400))d ago"
        }
    }
}
