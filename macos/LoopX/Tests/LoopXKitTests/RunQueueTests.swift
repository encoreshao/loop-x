import Testing
import Foundation
@testable import LoopXKit

@Suite struct RunQueueTests {
    let now = Timestamps.parse("2026-10-27T10:00:30.000Z")!

    static let json = """
    {"version":1,"app_state":"running","loops":{"gitlab-loop":{"state":"running","current_step":"analyzing"},
     "topic-loop":{"state":"idle"}},
     "queue":{"running":[{"id":"r1","loop":"gitlab-loop","reason":"scheduled","state":"running","enqueued_at":"2026-10-27T10:00:00","step":"analyzing"}],
              "queued":[{"id":"q1","loop":"topic-loop","reason":"manual","state":"queued","enqueued_at":"2026-10-27T10:00:05"},
                        {"id":"q2","loop":"inbox-triage-loop","reason":"catch-up","state":"queued","enqueued_at":"2026-10-27T10:00:06"}],
              "recent":[{"id":"d1","loop":"old-loop","reason":"scheduled","state":"done","enqueued_at":"2026-10-27T09:00:00"}]}}
    """

    @Test func snapshotDecodesTheQueueAndToleratesItsAbsence() throws {
        let s = try Snapshot.decode(Data(Self.json.utf8))
        #expect(s.queue.running.map(\.loop) == ["gitlab-loop"])
        #expect(s.queue.queued.map(\.id) == ["q1", "q2"])
        #expect(s.queue.queued[0].reason == "manual")
        #expect(s.queue.recent.first?.state == "done")
        let none = try Snapshot.decode(Data(#"{"version":1,"app_state":"idle"}"#.utf8))
        #expect(none.queue == QueueBlock())
    }

    @Test func waitingRunsAreTasksInLineAndReplaceTheirIdleRow() throws {
        let s = try Snapshot.decode(Data(Self.json.utf8))
        let tasks = TaskNotifications.tasks(in: s)
        #expect(tasks.map(\.name) == ["gitlab-loop", "topic-loop", "inbox-triage-loop"])
        #expect(tasks.map(\.state) == [.running, .queued, .queued])
        #expect(tasks[1].detail == "#1 in line · manual")
        #expect(tasks[2].detail == "#2 in line · catch-up")
    }

    @Test func theRunningNotchSaysHowManyAreWaiting() throws {
        let s = try Snapshot.decode(Data(Self.json.utf8))
        let n = NotchState.make(from: s, now: now)
        #expect(n.kind == .running)
        #expect(n.title == "GitLab running")
        #expect(n.subtitle == "analyzing · 2 queued")
    }

    @Test func aQueueWithNothingRunningYetStillShowsAsRunning() {
        let s = Snapshot(queue: QueueBlock(queued: [QueueEntry(id: "q", loop: "a", reason: "scheduled", state: "queued", enqueuedAt: "t")]))
        let n = NotchState.make(from: s, now: now)
        #expect(n.kind == .running)
        #expect(n.title == "1 queued")
        #expect(n.subtitle == "A")
    }

    @Test func aRunLeavingTheQueueToRunStartsANotification() {
        let queued = Snapshot(queue: QueueBlock(queued: [QueueEntry(id: "q", loop: "a", reason: "scheduled", state: "queued", enqueuedAt: "t")]))
        let running = Snapshot(appState: .running, loops: ["a": LoopStatus(state: "running")])
        #expect(TaskNotifications.changes(from: nil, to: queued).isEmpty)
        #expect(TaskNotifications.changes(from: queued, to: running).map(\.title) == ["A started"])
    }
}
