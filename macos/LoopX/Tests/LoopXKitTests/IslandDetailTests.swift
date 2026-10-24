import Foundation
import Testing
@testable import LoopXKit

@Suite struct IslandDetailTests {
    func ev(_ type: String, at: String, project: String? = "web", iid: Int? = 7, data: JSONValue? = nil) -> RecentEvent {
        RecentEvent(type: type, at: at, runId: "r1", project: project, issueIid: iid, data: data)
    }
    let task = NotchTask(id: "loop:gitlab-loop", name: "gitlab-loop", state: .running, detail: "web #7 · analyzing")

    @Test func rowsAreTheTasksIssueEventsNewestFirstWithTones() {
        let events = [
            ev("issue.queued", at: "2026-10-24T08:00:00Z"),
            ev("issue.completed", at: "2026-10-24T08:10:00Z"),
            ev("issue.queued", at: "2026-10-24T08:05:00Z", project: "other", iid: 9),
        ]
        let d = IslandDetail.make(task: task, events: events, attention: nil)
        #expect(d.rows.map(\.title) == ["Issue completed", "Issue queued"])
        #expect(d.rows.map(\.tone) == [.success, .neutral])
    }

    @Test func withoutAnIssueTheLatestEventsOfAnyProjectAreShown() {
        let t = NotchTask(id: "loop:x", name: "x", state: .idle, detail: nil)
        let d = IslandDetail.make(task: t, events: [ev("run.started", at: "2026-10-24T08:00:00Z")], attention: nil)
        #expect(d.rows.map(\.title) == ["Run started"])
    }

    @Test func rowsAreCappedAtSix() {
        let many = (0..<10).map { ev("issue.queued", at: "2026-10-24T08:0\($0):00Z") }
        #expect(IslandDetail.make(task: task, events: many, attention: nil).rows.count == 6)
    }

    @Test func mrOpenedResultOffersTheLinkOnlyForHTTP() {
        let ok = Attention(kind: .mrOpened, at: "2026-10-24T08:00:00Z", project: "web", issueIid: 7, mrURL: "https://gl.example.com/mr/1")
        let d = IslandDetail.make(task: task, events: [], attention: ok)
        #expect(d.result == "Merge request opened · web #7")
        #expect(d.chips.contains(.openMergeRequest(URL(string: "https://gl.example.com/mr/1")!)))
        let bad = Attention(kind: .mrOpened, at: "2026-10-24T08:00:00Z", project: "web", issueIid: 7, mrURL: "file:///etc/passwd")
        #expect(!IslandDetail.make(task: task, events: [], attention: bad).chips.contains { if case .openMergeRequest = $0 { true } else { false } })
    }

    @Test func chipsAlwaysEndWithDashboardAndRunAgainOnlyWhenNotRunning() {
        let running = IslandDetail.make(task: task, events: [], attention: nil)
        #expect(running.chips == [.openDashboard])
        let idle = NotchTask(id: "loop:gitlab-loop", name: "gitlab-loop", state: .idle, detail: nil)
        #expect(IslandDetail.make(task: idle, events: [], attention: nil).chips == [.runAgain(loop: "gitlab-loop"), .openDashboard])
    }

    @Test func topicsRunAgainViaTheTopicLoop() {
        let t = NotchTask(id: "topic:ai-news", name: "ai-news", state: .idle, detail: nil)
        #expect(IslandDetail.make(task: t, events: [], attention: nil).chips == [.runAgain(loop: "topic-loop"), .openDashboard])
    }
}
