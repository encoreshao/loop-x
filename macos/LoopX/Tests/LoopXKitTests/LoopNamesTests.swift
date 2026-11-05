import Foundation
import Testing
@testable import LoopXKit

@Suite struct LoopNamesTests {
    @Test func loopIdsReadAsTheirNames() {
        #expect(LoopNames.display("issue-loop") == "Issues")
        #expect(LoopNames.display("gitlab-loop") == "Issues", "the issue loop's old id, in older events")
        #expect(LoopNames.display("inbox-triage-loop") == "Inbox Triage")
        #expect(LoopNames.display("topic-loop") == "Topic Monitor")
        #expect(LoopNames.display("daily-digest-loop") == "Daily Digest")
        #expect(LoopNames.display("mr-review-loop") == "MR Review")
        #expect(LoopNames.display("rss-watch-loop") == "RSS Watch")
        #expect(LoopNames.display("linkedin-autopilot-loop") == "LinkedIn")
        #expect(LoopNames.display("meeting-prep-loop") == "Meeting Prep")
        #expect(LoopNames.display("stale-sweeper-loop") == "Stale Work Sweeper")
        #expect(LoopNames.display("my_custom-thing") == "My Custom Thing")
    }

    @Test func theIslandShowsNamesButKeepsIdsForActions() {
        let s = Snapshot(appState: .running, loops: ["issue-loop": LoopStatus(state: "running", currentIssue: "web #7")],
                         topics: ["ai-agents": TopicStatus(state: "idle")])
        let tasks = TaskNotifications.tasks(in: s)
        #expect(tasks.map(\.title) == ["Issues", "AI Agents"], "a slug-like topic name reads title-cased")
        #expect(tasks.map(\.name) == ["issue-loop", "ai-agents"])
        #expect(NotchState.make(from: s, now: Date()).title == "Issues running")
        let failed = Snapshot(appState: .failed, loops: ["inbox-triage-loop": LoopStatus(state: "failed")])
        #expect(NotchState.make(from: failed, now: Date()).subtitle == "Inbox Triage")
    }

    @Test func topicsTitleCaseOnlyWhenTheyLookLikeSlugs() {
        let title = { (name: String) in NotchTask(id: "topic:\(name)", name: name, state: .idle, detail: nil).title }
        #expect(title("ai-agents") == "AI Agents")
        #expect(title("rust_news") == "Rust News")
        #expect(title("My Startup Radar") == "My Startup Radar", "a name typed with spaces is kept as given")
        #expect(title("t") == "t")
        #expect(title("topic-loop-ideas") == "Topic Loop Ideas", "a topic never loses a word to the -loop rule")
    }

    @Test func everyLoopHasASymbol() {
        #expect(LoopNames.symbol("issue-loop") == "arrow.triangle.branch")
        #expect(LoopNames.symbol("inbox-triage-loop") == "tray.full")
        #expect(LoopNames.symbol("some-new-loop") == "repeat")
    }

    @Test func notificationsUseTheName() {
        let before = Snapshot(loops: ["mr-review-loop": LoopStatus(state: "idle")])
        let after = Snapshot(loops: ["mr-review-loop": LoopStatus(state: "running")])
        #expect(TaskNotifications.changes(from: before, to: after).map(\.title) == ["MR Review started"])
    }

    @Test func creatorJobsReadAsTheirNamesAndNotifyLikeLoops() {
        #expect(LoopNames.display("creator-find-videos") == "Find Videos")
        #expect(LoopNames.display("creator-generate-video") == "Generate Video")
        #expect(LoopNames.symbol("creator-find-videos") == "magnifyingglass")
        #expect(LoopNames.symbol("creator-generate-video") == "film")
        func snap(_ state: String, step: String? = nil) -> Snapshot {
            Snapshot(appState: state == "running" ? .running : .idle,
                     loops: ["creator-generate-video": LoopStatus(state: state, currentIssue: "www.ted.com/talks/x", currentStep: step)])
        }
        let started = TaskNotifications.changes(from: snap("idle"), to: snap("running", step: "Stage 3 of 9 · Transcribe"))
        #expect(started.map(\.title) == ["Generate Video started"])
        #expect(started.first?.body == "www.ted.com/talks/x · Stage 3 of 9 · Transcribe")
        let done = TaskNotifications.changes(from: snap("running"), to: snap("idle"))
        #expect(done.map(\.title) == ["Generate Video completed"])
        let failed = TaskNotifications.changes(from: snap("running"), to: snap("failed", step: "No video produced"))
        #expect(failed.map(\.title) == ["Generate Video failed"])
        #expect(failed.first?.body == "www.ted.com/talks/x · No video produced")
    }
}
