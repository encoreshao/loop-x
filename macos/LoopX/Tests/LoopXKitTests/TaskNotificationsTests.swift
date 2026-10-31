import Foundation
import Testing
@testable import LoopXKit

@Suite struct TaskNotificationsTests {
    func snap(loops: [String: LoopStatus] = [:], topics: [String: TopicStatus] = [:],
              approvals: [Approval] = []) -> Snapshot {
        Snapshot(loops: loops, topics: topics, approvals: ApprovalsBlock(pending: approvals))
    }

    @Test func tasksListEveryLoopAndTopicRunningFirstWithTheirMessage() {
        let s = snap(loops: ["gitlab-loop": LoopStatus(state: "running", currentIssue: "web #7", currentStep: "analyzing"),
                             "inbox": LoopStatus(state: "idle")],
                     topics: ["ai-news": TopicStatus(state: "running", currentStep: "fetching sources"),
                              "broken": TopicStatus(state: "failed")])
        let t = TaskNotifications.tasks(in: s)
        #expect(t.map(\.name) == ["ai-news", "gitlab-loop", "broken", "inbox"])
        #expect(t.first { $0.name == "gitlab-loop" }?.detail == "web #7 · analyzing")
        #expect(t.first { $0.name == "ai-news" }?.detail == "fetching sources")
        #expect(t.first { $0.name == "broken" }?.state == .failed)
    }

    @Test func staleCountsAsFailed() {
        #expect(TaskNotifications.tasks(in: snap(loops: ["a": LoopStatus(state: "stale")])).first?.state == .failed)
    }

    @Test func firstSnapshotNeverNotifies() {
        let s = snap(loops: ["a": LoopStatus(state: "running")])
        #expect(TaskNotifications.changes(from: nil, to: s).isEmpty)
    }

    @Test func startedCompletedAndFailedAreReportedOnTransition() {
        let idle = snap(loops: ["a": LoopStatus(state: "idle")], topics: ["t": TopicStatus(state: "idle")])
        let running = snap(loops: ["a": LoopStatus(state: "running", currentIssue: "web #7")],
                           topics: ["t": TopicStatus(state: "running")])
        let started = TaskNotifications.changes(from: idle, to: running)
        #expect(started.map(\.kind) == [.started, .started])
        #expect(started.first { $0.title == "A started" }?.body == "web #7")

        let done = snap(loops: ["a": LoopStatus(state: "idle")], topics: ["t": TopicStatus(state: "failed")])
        let ended = TaskNotifications.changes(from: running, to: done)
        #expect(Set(ended.map(\.title)) == ["A completed", "t failed"])
    }

    @Test func aTaskNewlyAppearingAsRunningIsStarted() {
        let n = TaskNotifications.changes(from: snap(), to: snap(loops: ["a": LoopStatus(state: "running")]))
        #expect(n.map(\.title) == ["A started"])
    }

    @Test func unchangedStateIsSilent() {
        let s = snap(loops: ["a": LoopStatus(state: "running", currentStep: "x")])
        let s2 = snap(loops: ["a": LoopStatus(state: "running", currentStep: "y")])
        #expect(TaskNotifications.changes(from: s, to: s2).isEmpty)
    }

    @Test func aNewApprovalNotifiesOnce() {
        let a = Approval(id: "apr_1", project: "web", issueIid: 7, title: "Fix #7")
        let n = TaskNotifications.changes(from: snap(), to: snap(approvals: [a]))
        #expect(n.map(\.title) == ["Approval needed"])
        #expect(n.first?.body == "web #7")
        #expect(TaskNotifications.changes(from: snap(approvals: [a]), to: snap(approvals: [a])).isEmpty)
    }
}
