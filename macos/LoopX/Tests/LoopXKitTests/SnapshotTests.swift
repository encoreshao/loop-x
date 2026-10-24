import Foundation
import Testing
@testable import LoopXKit

@Suite struct SnapshotTests {
    static let full = #"""
    {"version":1,"app_state":"awaiting_approval",
     "loops":{"gitlab-loop":{"state":"running","updated_at":"2026-10-22T08:15:03.412907+00:00","pid":5150,"current_issue":"web #7","current_step":"analyzing"},
              "topic-loop":{"state":"idle","updated_at":"x","last_exit_code":0}},
     "topics":{"ai":{"state":"idle","updated_at":"x"}},
     "approvals":{"pending":[{"id":"apr_0123456789ab","kind":"open_mr","run_id":"run_1","project":"web","issue_iid":7,
        "title":"Fix #7: x","branch":"loop/issue-7","target_branch":"main","summary":"s","status":"pending",
        "created_at":"2026-10-22T10:00:00.000Z","expires_at":"2026-10-22T10:30:00.000Z","decided_at":null,"decided_by":null}]},
     "attention":{"kind":"mr_opened","at":"2026-10-22T08:20:00.000Z","project":"web","issue_iid":7,"reason":null,
        "mr_url":"https://gitlab.example.com/g/web/-/merge_requests/42"},
     "recent_events":[{"type":"issue.completed","at":"2026-10-22T08:20:00.000Z","run_id":"r","project":"web","issue_iid":7,"data":{"n":1,"ok":true,"l":[1,"a",null]}}]}
    """#

    @Test func decodesTheContractExample() throws {
        let s = try Snapshot.decode(Data(Self.full.utf8))
        #expect(s.appState == .awaitingApproval)
        #expect(s.loops["gitlab-loop"]?.pid == 5150)
        #expect(s.loops["gitlab-loop"]?.currentIssue == "web #7")
        #expect(s.loops["topic-loop"]?.lastExitCode == 0)
        #expect(s.topics["ai"]?.state == "idle")
        let a = try #require(s.approvals.pending.first)
        #expect(a.id == "apr_0123456789ab")
        #expect(a.issueIid == 7)
        #expect(a.targetBranch == "main")
        #expect(a.isPending)
        #expect(a.decidedBy == nil)
        #expect(a.expiry != nil)
        #expect(s.attention?.kind == .mrOpened)
        #expect(s.attention?.mrURL == "https://gitlab.example.com/g/web/-/merge_requests/42")
        #expect(s.recentEvents.first?.type == "issue.completed")
        #expect(s.recentEvents.first?.data == .object(["n": .number(1), "ok": .bool(true), "l": .array([.number(1), .string("a"), .null])]))
        #expect(s.runningLoop?.name == "gitlab-loop")
    }

    @Test func decodesTheIdleDocumentTheBackendActuallySends() throws {
        let s = try Snapshot.decode(Data(snapshotJSON().utf8))
        #expect(s == Snapshot())
        #expect(s.attention == nil)
    }

    @Test func toleratesUnknownStatesAndMissingFields() throws {
        let s = try Snapshot.decode(Data(#"{"app_state":"something_new","future":true}"#.utf8))
        #expect(s.appState == .idle)
        #expect(s.loops.isEmpty)
        let s2 = try Snapshot.decode(Data(#"{"attention":{"kind":"brand_new","at":"x"}}"#.utf8))
        #expect(s2.attention == nil)
    }

    @Test func staleLoopCountsAsFailure() {
        #expect(LoopStatus(state: "stale").isFailure)
        #expect(LoopStatus(state: "failed").isFailure)
        #expect(!LoopStatus(state: "idle").isFailure)
    }

    @Test func versionInfoDecodes() throws {
        let v = try JSONDecoder().decode(VersionInfo.self, from: Data(
            #"{"name":"loopx","version":"0.4.0","pid":4242,"started_at":"t","root":"/r","legacy_dashboard":true}"#.utf8))
        #expect(v == VersionInfo(name: "loopx", version: "0.4.0", pid: 4242, startedAt: "t", root: "/r", legacyDashboard: true))
    }

    @Test func timestampsParseWithAndWithoutFraction() {
        #expect(Timestamps.parse("2026-10-22T10:00:00.000Z") != nil)
        #expect(Timestamps.parse("2026-10-22T10:00:00Z") != nil)
        #expect(Timestamps.parse("2026-10-22T08:15:03.412907+00:00") != nil)
        #expect(Timestamps.parse("garbage") == nil)
    }
}
