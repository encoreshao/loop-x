import Foundation
import Testing
@testable import LoopXKit

@Suite struct SchedulerPollerTests {
    @Test func pollsImmediatelyThenEveryIntervalUntilStopped() async {
        let polls = Recorder<Int>()
        let gate = AsyncStream<Void>.makeStream()
        let p = SchedulerPoller(interval: 900, poll: { polls.add(1) },
                                sleeper: { secs in
                                    #expect(secs == 900)
                                    for await _ in gate.stream { return }   // one tick per yield
                                })
        p.start(); p.start()                       // a second start is a no-op
        #expect(await waitUntil { polls.count == 1 })
        gate.continuation.yield()
        #expect(await waitUntil { polls.count == 2 })
        p.stop()
        gate.continuation.yield()
        try? await Task.sleep(nanoseconds: 100_000_000)
        #expect(polls.count == 2)
        #expect(!p.isRunning)
    }

    @Test func canBeRestartedAfterStop() async {
        let polls = Recorder<Int>()
        let p = SchedulerPoller(poll: { polls.add(1) }, sleeper: { _ in try? await Task.sleep(nanoseconds: 3_600_000_000_000) })
        p.start(); #expect(await waitUntil { polls.count == 1 }); p.stop()
        p.start(); #expect(await waitUntil { polls.count == 2 }); p.stop()
    }
}
