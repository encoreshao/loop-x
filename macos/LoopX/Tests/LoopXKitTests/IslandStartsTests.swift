import Testing
@testable import LoopXKit

@Suite struct IslandStartsTests {
    @Test func offersTheThreeBuiltInLoops() {
        #expect(IslandStarts.all.map(\.loop) == ["topic-loop", "issue-loop", "inbox-triage-loop"])
        #expect(IslandStarts.all.map(\.title) == ["Run Topic Monitor", "Run issue loop", "Triage inbox"])
    }

    @Test func runningLoopsAreNotOffered() {
        let running = [NotchTask(id: "topic:a", name: "a", state: .running, detail: nil)]
        #expect(!IslandStarts.available(tasks: running).map(\.loop).contains("topic-loop"))
        #expect(IslandStarts.available(tasks: []).count == 3)
    }

    @Test func confirmTextNamesTheRealSideEffects() {
        #expect(IslandStarts.confirmText(loop: "issue-loop").contains("GitLab"))
        #expect(IslandStarts.confirmText(loop: "issue-loop").contains("GitHub"))
        #expect(IslandStarts.confirmText(loop: "daily-digest-loop") == "Run Daily Digest now?")
    }

    @Test func everyIdleOrFailedLoopRowStartsWithOneClick() {
        let row = { (id: String, name: String, state: NotchTask.State) in
            IslandStarts.runnableLoop(for: NotchTask(id: id, name: name, state: state, detail: nil))
        }
        #expect(row("loop:daily-digest-loop", "daily-digest-loop", .idle) == "daily-digest-loop")
        #expect(row("loop:issue-loop", "issue-loop", .failed) == "issue-loop")
        #expect(row("loop:issue-loop", "issue-loop", .running) == nil, "already running")
        #expect(row("queue:q1", "issue-loop", .queued) == nil, "already waiting its turn")
        #expect(row("topic:ai-agents", "ai-agents", .idle) == nil, "topics run together, from Topic Monitor")
    }

    @Test func creatorJobsHaveNoRunButton() {
        // A search needs a topic and a generation a source: neither can be run from the island.
        for (id, name) in [("loop:creator-find-videos", "creator-find-videos"), ("loop:creator-generate-video", "creator-generate-video")] {
            for state in [NotchTask.State.idle, .failed] {
                #expect(IslandStarts.runnableLoop(for: NotchTask(id: id, name: name, state: state, detail: nil)) == nil)
            }
        }
        let failed = NotchTask(id: "loop:creator-generate-video", name: "creator-generate-video", state: .failed, detail: nil)
        #expect(IslandDetail.make(task: failed, events: [], attention: nil).chips == [.openDashboard])
    }

    @Test func aDisabledLoopHasNoStartButton() {
        let starts = IslandStarts.available(tasks: [], disabled: ["topic-loop", "rss-watch-loop"])
        #expect(starts.map(\.loop) == ["issue-loop", "inbox-triage-loop"])
    }
}
