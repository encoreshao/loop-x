import Foundation
import Testing
@testable import LoopXKit

@Suite struct LoopNamesTests {
    @Test func loopIdsReadAsTheirNames() {
        #expect(LoopNames.display("gitlab-loop") == "GitLab")
        #expect(LoopNames.display("inbox-triage-loop") == "Inbox Triage")
        #expect(LoopNames.display("topic-loop") == "Topic Monitor")
        #expect(LoopNames.display("daily-digest-loop") == "Daily Digest")
        #expect(LoopNames.display("mr-review-loop") == "MR Review")
        #expect(LoopNames.display("rss-watch-loop") == "RSS Watch")
        #expect(LoopNames.display("linkedin-autopilot-loop") == "LinkedIn Autopilot")
        #expect(LoopNames.display("meeting-prep-loop") == "Meeting Prep")
        #expect(LoopNames.display("stale-sweeper-loop") == "Stale Work Sweeper")
        #expect(LoopNames.display("my_custom-thing") == "My Custom Thing")
    }

    @Test func theIslandShowsNamesButKeepsIdsForActions() {
        let s = Snapshot(appState: .running, loops: ["gitlab-loop": LoopStatus(state: "running", currentIssue: "web #7")],
                         topics: ["ai-agents": TopicStatus(state: "idle")])
        let tasks = TaskNotifications.tasks(in: s)
        #expect(tasks.map(\.title) == ["GitLab", "ai-agents"], "a topic keeps the name you gave it")
        #expect(tasks.map(\.name) == ["gitlab-loop", "ai-agents"])
        #expect(NotchState.make(from: s, now: Date()).title == "GitLab running")
        let failed = Snapshot(appState: .failed, loops: ["inbox-triage-loop": LoopStatus(state: "failed")])
        #expect(NotchState.make(from: failed, now: Date()).subtitle == "Inbox Triage")
    }

    @Test func notificationsUseTheName() {
        let before = Snapshot(loops: ["mr-review-loop": LoopStatus(state: "idle")])
        let after = Snapshot(loops: ["mr-review-loop": LoopStatus(state: "running")])
        #expect(TaskNotifications.changes(from: before, to: after).map(\.title) == ["MR Review started"])
    }
}
