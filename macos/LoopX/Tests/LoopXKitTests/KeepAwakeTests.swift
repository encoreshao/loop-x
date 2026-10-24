import Foundation
import Testing
@testable import LoopXKit

/// Records what the controller asks the OS for, so the tests assert on holds, not on IOKit.
final class FakePower: PowerAssertion, @unchecked Sendable {
    var begun: [String] = []
    var ended: [UInt32] = []
    var refuse = false
    private var next: UInt32 = 1
    func begin(reason: String) -> UInt32? {
        guard !refuse else { return nil }
        begun.append(reason); defer { next += 1 }
        return next
    }
    func end(_ id: UInt32) { ended.append(id) }
}

@Suite struct KeepAwakeTests {
    func snap(loops: [String: String] = [:], topics: [String: String] = [:]) -> Snapshot {
        Snapshot(loops: loops.mapValues { LoopStatus(state: $0) }, topics: topics.mapValues { TopicStatus(state: $0) })
    }

    @Test func holdsOnceWhileALoopRunsNoMatterHowManySnapshotsArrive() {
        let power = FakePower(); let k = KeepAwake(power: power)
        for _ in 0..<3 { k.update(snapshot: snap(loops: ["gitlab-loop": "running"]), enabled: true) }
        #expect(power.begun.count == 1)
        #expect(k.isHolding)
    }

    @Test func releasesWhenEverythingIsIdle() {
        let power = FakePower(); let k = KeepAwake(power: power)
        k.update(snapshot: snap(loops: ["a": "running"]), enabled: true)
        k.update(snapshot: snap(loops: ["a": "idle"]), enabled: true)
        #expect(power.ended == [1])
        #expect(!k.isHolding)
    }

    @Test func aRunningTopicAlsoHoldsButAFailedOrStaleTaskDoesNot() {
        let power = FakePower(); let k = KeepAwake(power: power)
        k.update(snapshot: snap(loops: ["a": "failed", "b": "stale"]), enabled: true)
        #expect(!k.isHolding)
        k.update(snapshot: snap(topics: ["news": "running"]), enabled: true)
        #expect(k.isHolding)
    }

    @Test func turningTheSettingOffReleasesARunningHold() {
        let power = FakePower(); let k = KeepAwake(power: power)
        k.update(snapshot: snap(loops: ["a": "running"]), enabled: true)
        k.update(snapshot: snap(loops: ["a": "running"]), enabled: false)
        #expect(power.ended == [1])
        #expect(!k.isHolding)
    }

    @Test func noSnapshotYetHoldsNothing() {
        let power = FakePower(); let k = KeepAwake(power: power)
        k.update(snapshot: nil, enabled: true)
        #expect(power.begun.isEmpty)
    }

    @Test func aRefusedAssertionIsRetriedOnTheNextSnapshot() {
        let power = FakePower(); power.refuse = true
        let k = KeepAwake(power: power)
        k.update(snapshot: snap(loops: ["a": "running"]), enabled: true)
        #expect(!k.isHolding)
        power.refuse = false
        k.update(snapshot: snap(loops: ["a": "running"]), enabled: true)
        #expect(k.isHolding)
    }

    @Test func releaseDropsTheHoldAtQuit() {
        let power = FakePower(); let k = KeepAwake(power: power)
        k.update(snapshot: snap(loops: ["a": "running"]), enabled: true)
        k.release()
        #expect(power.ended == [1])
        k.release()   // idempotent
        #expect(power.ended == [1])
    }

    @Test func theReasonNamesTheRunningTask() {
        let power = FakePower(); let k = KeepAwake(power: power)
        k.update(snapshot: snap(loops: ["gitlab-loop": "running"]), enabled: true)
        #expect(power.begun.first?.contains("gitlab-loop") == true)
    }

    @Test func keepAwakeSettingDefaultsOnAndPersists() {
        let name = "awake-\(UUID().uuidString)"
        let d = UserDefaults(suiteName: name)!
        #expect(SettingsStore(defaults: d).keepAwakeWhileRunning)
        SettingsStore(defaults: d).keepAwakeWhileRunning = false
        #expect(!SettingsStore(defaults: d).keepAwakeWhileRunning)
        d.removePersistentDomain(forName: name)
    }
}
