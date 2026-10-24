import Testing
@testable import LoopXKit

@Suite struct IslandStartsTests {
    @Test func offersTheThreeBuiltInLoops() {
        #expect(IslandStarts.all.map(\.loop) == ["topic-loop", "gitlab-loop", "inbox-triage-loop"])
        #expect(IslandStarts.all.map(\.title) == ["Run Topic Monitor", "Run GitLab loop", "Triage inbox"])
    }

    @Test func runningLoopsAreNotOffered() {
        let running = [NotchTask(id: "topic:a", name: "a", state: .running, detail: nil)]
        #expect(!IslandStarts.available(tasks: running).map(\.loop).contains("topic-loop"))
        #expect(IslandStarts.available(tasks: []).count == 3)
    }

    @Test func confirmTextNamesTheRealSideEffects() {
        #expect(IslandStarts.confirmText(loop: "gitlab-loop").contains("GitLab"))
        #expect(IslandStarts.confirmText(loop: "unknown") == "Run unknown now?")
    }
}
