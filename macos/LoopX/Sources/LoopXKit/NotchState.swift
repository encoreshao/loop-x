import Foundation

public enum NotchKind: String, Equatable, Sendable, CaseIterable {
    case idle, running, escalate, mrOpened, failed, awaitingApproval
}

public enum NotchAccent: String, Equatable, Sendable { case neutral, blue, orange, green, red, purple }

public struct NotchStyle: Equatable, Sendable {
    public var accent: NotchAccent
    /// SF Symbol name, shared by the notch and the menu bar item.
    public var symbol: String
    public var pulses: Bool
    /// Alert states pop the notch open on their own; the rest wait for a hover.
    public var autoExpands: Bool
    /// Seconds before an auto-expanded alert folds back; nil = stays until acted on.
    public var autoCollapseAfter: TimeInterval?
}

public enum NotchAction: Equatable, Sendable {
    case approve(id: String)
    case deny(id: String)
    case openMergeRequest(URL)
    case openDashboard
    case dismiss(alertKey: String)
}

public struct NotchState: Equatable, Sendable {
    public var kind: NotchKind
    public var title: String
    public var subtitle: String?
    public var style: NotchStyle
    public var actions: [NotchAction]
    public var approvals: [Approval]
    /// Identity of the alert shown (for dismiss), nil for non-alert states.
    public var alertKey: String?

    public static let alertWindow: TimeInterval = 120

    public static let idle = NotchState.make(from: Snapshot(), now: Date())

    /// Maps a snapshot to what the notch shows. Priority: pending approval, fresh attention,
    /// failure, running, idle. `attention` lingers for 15 minutes on the backend; the notch only
    /// pops for `alertWindow` seconds and never again once dismissed.
    public static func make(from s: Snapshot, now: Date, dismissed: Set<String> = [],
                            alertWindow: TimeInterval = NotchState.alertWindow) -> NotchState {
        let pending = s.approvals.pending.filter(\.isPending)
        if s.appState == .awaitingApproval || !pending.isEmpty {
            let first = pending.first
            let sub: String?
            if pending.count > 1 { sub = "\(pending.count) approvals waiting" }
            else { sub = first.map { describe(project: $0.project, iid: $0.issueIid, fallback: $0.title) } }
            var actions: [NotchAction] = []
            if let first { actions = [.approve(id: first.id), .deny(id: first.id)] }
            return NotchState(kind: .awaitingApproval, title: "Approve merge request?", subtitle: sub,
                              style: NotchStyle(accent: .purple, symbol: "hand.raised.fill", pulses: true,
                                                autoExpands: true, autoCollapseAfter: nil),
                              actions: actions, approvals: pending, alertKey: nil)
        }

        if let a = s.attention, !dismissed.contains(a.key), isFresh(a, now: now, window: alertWindow) {
            let who = describe(project: a.project, iid: a.issueIid, fallback: nil)
            switch a.kind {
            case .escalated:
                return alert(.escalate, "Needs your attention", a.reason ?? who, .orange,
                             "exclamationmark.triangle.fill", key: a.key, extra: [.openDashboard])
            case .mrOpened:
                var extra: [NotchAction] = []
                if let url = a.mrURL.flatMap(URL.init(string:)), ["http", "https"].contains(url.scheme?.lowercased()) {
                    extra.append(.openMergeRequest(url))
                }
                extra.append(.openDashboard)
                return alert(.mrOpened, "Merge request opened", who, .green, "checkmark.circle.fill",
                             key: a.key, extra: extra)
            case .failed:
                return alert(.failed, "Run failed", a.reason ?? who, .red, "xmark.octagon.fill",
                             key: a.key, extra: [.openDashboard])
            }
        }

        if s.appState == .failed || s.loops.values.contains(where: \.isFailure) {
            let name = s.loops.filter { $0.value.isFailure }.keys.sorted().first
            return NotchState(kind: .failed, title: "Loop failed", subtitle: name,
                              style: NotchStyle(accent: .red, symbol: "xmark.octagon.fill", pulses: false,
                                                autoExpands: false, autoCollapseAfter: nil),
                              actions: [.openDashboard], approvals: [], alertKey: nil)
        }

        if s.appState == .running || s.runningLoop != nil {
            let run = s.runningLoop
            let sub = [run?.status.currentIssue, run?.status.currentStep].compactMap { $0 }.joined(separator: " · ")
            return NotchState(kind: .running, title: run.map { "\($0.name) running" } ?? "Running",
                              subtitle: sub.isEmpty ? nil : sub,
                              style: NotchStyle(accent: .blue, symbol: "arrow.triangle.2.circlepath", pulses: true,
                                                autoExpands: false, autoCollapseAfter: nil),
                              actions: [.openDashboard], approvals: [], alertKey: nil)
        }

        return NotchState(kind: .idle, title: "Idle", subtitle: nil,
                          style: NotchStyle(accent: .neutral, symbol: "circle.dotted", pulses: false,
                                            autoExpands: false, autoCollapseAfter: nil),
                          actions: [.openDashboard], approvals: [], alertKey: nil)
    }

    private static func alert(_ kind: NotchKind, _ title: String, _ sub: String?, _ accent: NotchAccent,
                              _ symbol: String, key: String, extra: [NotchAction]) -> NotchState {
        NotchState(kind: kind, title: title, subtitle: sub,
                   style: NotchStyle(accent: accent, symbol: symbol, pulses: false, autoExpands: true,
                                     autoCollapseAfter: 8),
                   actions: extra + [.dismiss(alertKey: key)], approvals: [], alertKey: key)
    }

    private static func isFresh(_ a: Attention, now: Date, window: TimeInterval) -> Bool {
        guard let d = a.date else { return true }  // unparseable timestamp: show rather than hide
        return now.timeIntervalSince(d) <= window
    }

    static func describe(project: String?, iid: Int?, fallback: String?) -> String {
        switch (project, iid) {
        case let (p?, i?): return "\(p) #\(i)"
        case let (p?, nil): return p
        case let (nil, i?): return "#\(i)"
        default: return fallback ?? ""
        }
    }
}
