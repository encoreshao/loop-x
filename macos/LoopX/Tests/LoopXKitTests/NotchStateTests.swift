import Foundation
import Testing
@testable import LoopXKit

@Suite struct NotchStateTests {
    let now = Timestamps.parse("2026-10-22T08:20:30.000Z")!
    func attention(_ kind: Attention.Kind, at: String = "2026-10-22T08:20:00.000Z", reason: String? = nil,
                   mr: String? = nil) -> Attention {
        Attention(kind: kind, at: at, project: "web", issueIid: 7, reason: reason, mrURL: mr)
    }
    func approval(_ id: String = "apr_1") -> Approval {
        Approval(id: id, project: "web", issueIid: 7, title: "Fix #7")
    }

    @Test func idle() {
        let n = NotchState.make(from: Snapshot(), now: now)
        #expect(n.kind == .idle)
        #expect(n.style.accent == .neutral)
        #expect(!n.style.autoExpands)
        #expect(n.actions == [.openDashboard])
    }

    @Test func runningNamesTheLoopAndIssue() {
        let s = Snapshot(appState: .running, loops: ["gitlab-loop": LoopStatus(state: "running", pid: 1, currentIssue: "web #7", currentStep: "analyzing")])
        let n = NotchState.make(from: s, now: now)
        #expect(n.kind == .running)
        #expect(n.title == "gitlab-loop running")
        #expect(n.subtitle == "web #7 · analyzing")
        #expect(n.style.pulses)
        #expect(n.style.accent == .blue)
    }

    @Test func escalateShowsReasonAndPopsOpen() {
        let s = Snapshot(attention: attention(.escalated, reason: "tests keep failing"))
        let n = NotchState.make(from: s, now: now)
        #expect(n.kind == .escalate)
        #expect(n.subtitle == "tests keep failing")
        #expect(n.style.accent == .orange)
        #expect(n.style.autoExpands)
        #expect(n.style.autoCollapseAfter != nil)
        #expect(n.actions.contains(.openDashboard))
        #expect(n.actions.contains(.dismiss(alertKey: s.attention!.key)))
    }

    @Test func mrOpenedOffersTheLinkOnlyForHTTPURLs() {
        let ok = NotchState.make(from: Snapshot(attention: attention(.mrOpened, mr: "https://gl.example.com/mr/42")), now: now)
        #expect(ok.kind == .mrOpened)
        #expect(ok.subtitle == "web #7")
        #expect(ok.style.accent == .green)
        #expect(ok.actions.first == .openMergeRequest(URL(string: "https://gl.example.com/mr/42")!))
        let bad = NotchState.make(from: Snapshot(attention: attention(.mrOpened, mr: "file:///etc/passwd")), now: now)
        #expect(!bad.actions.contains { if case .openMergeRequest = $0 { return true } else { return false } })
        let js = NotchState.make(from: Snapshot(attention: attention(.mrOpened, mr: "javascript:alert(1)")), now: now)
        #expect(!js.actions.contains { if case .openMergeRequest = $0 { return true } else { return false } })
    }

    @Test func failedByAttentionAndByAppState() {
        let a = NotchState.make(from: Snapshot(attention: attention(.failed, reason: "boom")), now: now)
        #expect(a.kind == .failed); #expect(a.subtitle == "boom"); #expect(a.style.accent == .red)
        let b = NotchState.make(from: Snapshot(appState: .failed, loops: ["topic-loop": LoopStatus(state: "failed")]), now: now)
        #expect(b.kind == .failed); #expect(b.subtitle == "topic-loop")
        #expect(!b.style.autoExpands)
        let c = NotchState.make(from: Snapshot(appState: .idle, loops: ["x": LoopStatus(state: "stale")]), now: now)
        #expect(c.kind == .failed)
    }

    @Test func awaitingApprovalHasAllowDenyAndOutranksEverything() {
        let s = Snapshot(appState: .awaitingApproval,
                         loops: ["gitlab-loop": LoopStatus(state: "running")],
                         approvals: ApprovalsBlock(pending: [approval("apr_9")]),
                         attention: attention(.failed))
        let n = NotchState.make(from: s, now: now)
        #expect(n.kind == .awaitingApproval)
        #expect(n.actions == [.approve(id: "apr_9"), .deny(id: "apr_9")])
        #expect(n.subtitle == "web #7")
        #expect(n.style.autoExpands)
        #expect(n.style.autoCollapseAfter == nil)   // must stay until decided
        #expect(n.approvals.count == 1)
    }

    @Test func severalApprovalsSummariseButActOnTheOldest() {
        let s = Snapshot(appState: .awaitingApproval, approvals: ApprovalsBlock(pending: [approval("a"), approval("b")]))
        let n = NotchState.make(from: s, now: now)
        #expect(n.subtitle == "2 approvals waiting")
        #expect(n.actions == [.approve(id: "a"), .deny(id: "a")])
    }

    @Test func pendingListWinsEvenIfAppStateLags() {
        let n = NotchState.make(from: Snapshot(appState: .idle, approvals: ApprovalsBlock(pending: [approval()])), now: now)
        #expect(n.kind == .awaitingApproval)
    }

    @Test func alertsAgeOutAndCanBeDismissed() {
        let a = attention(.mrOpened, mr: "https://x/1")
        let s = Snapshot(attention: a)
        #expect(NotchState.make(from: s, now: now.addingTimeInterval(60)).kind == .mrOpened)
        #expect(NotchState.make(from: s, now: now.addingTimeInterval(600)).kind == .idle)
        #expect(NotchState.make(from: s, now: now, dismissed: [a.key]).kind == .idle)
    }

    @Test func runningShowsAgainWhenAnAlertIsDismissed() {
        let a = attention(.escalated)
        let s = Snapshot(appState: .running, loops: ["gitlab-loop": LoopStatus(state: "running")], attention: a)
        #expect(NotchState.make(from: s, now: now).kind == .escalate)
        #expect(NotchState.make(from: s, now: now, dismissed: [a.key]).kind == .running)
    }

    @Test func everyKindHasADistinctSymbolAccentPair() {
        let states: [NotchState] = [
            .idle,
            NotchState.make(from: Snapshot(appState: .running, loops: ["l": LoopStatus(state: "running")]), now: now),
            NotchState.make(from: Snapshot(attention: attention(.escalated)), now: now),
            NotchState.make(from: Snapshot(attention: attention(.mrOpened)), now: now),
            NotchState.make(from: Snapshot(attention: attention(.failed)), now: now),
            NotchState.make(from: Snapshot(approvals: ApprovalsBlock(pending: [approval()])), now: now),
        ]
        #expect(Set(states.map(\.kind)).count == NotchKind.allCases.count)
        #expect(Set(states.map(\.style.symbol)).count == states.count)
    }
}
