import Foundation
import Testing
@testable import LoopXKit

@Suite struct SchedulerPolicyTests {
    func power(battery: Bool = false, pct: Int? = nil, low: Bool = false) -> PowerState {
        PowerState(onBattery: battery, batteryPercent: pct, lowPowerMode: low)
    }

    @Test func pollsOnACPowerEvenWithAnEmptyBattery() {
        #expect(SchedulerPolicy.shouldPoll(power: power(battery: false, pct: 3), enabled: true))
    }

    @Test func pollsOnBatteryWithEnoughCharge() {
        #expect(SchedulerPolicy.shouldPoll(power: power(battery: true, pct: 20), enabled: true))
        #expect(SchedulerPolicy.shouldPoll(power: power(battery: true, pct: 80), enabled: true))
    }

    @Test func skipsOnBatteryBelowTheThreshold() {
        #expect(!SchedulerPolicy.shouldPoll(power: power(battery: true, pct: 19), enabled: true))
        #expect(SchedulerPolicy.lowBatteryPercent == 20)
    }

    @Test func skipsInLowPowerModeWhateverTheSource() {
        #expect(!SchedulerPolicy.shouldPoll(power: power(battery: false, pct: 100, low: true), enabled: true))
    }

    @Test func aMacWithoutABatteryAlwaysPolls() {
        #expect(SchedulerPolicy.shouldPoll(power: power(), enabled: true))
    }

    @Test func anUnreadableChargeOnBatteryPolls() {
        #expect(SchedulerPolicy.shouldPoll(power: power(battery: true, pct: nil), enabled: true))
    }

    @Test func turnedOffNeverSkips() {
        #expect(SchedulerPolicy.shouldPoll(power: power(battery: true, pct: 2, low: true), enabled: false))
    }

    @Test func settingDefaultsOnAndPersists() {
        let name = "power-\(UUID().uuidString)"
        let d = UserDefaults(suiteName: name)!
        #expect(SettingsStore(defaults: d).pauseSchedulerOnLowPower)
        SettingsStore(defaults: d).pauseSchedulerOnLowPower = false
        #expect(!SettingsStore(defaults: d).pauseSchedulerOnLowPower)
        d.removePersistentDomain(forName: name)
    }
}

@Suite struct StatusBadgeTests {
    func state(approvals n: Int) -> NotchState {
        var s = NotchState.idle
        s.approvals = (0..<n).map { Approval(id: "a\($0)") }
        return s
    }

    @Test func noApprovalsNoBadge() { #expect(StatusBadge.text(for: state(approvals: 0)) == nil) }
    @Test func showsTheCount() {
        #expect(StatusBadge.text(for: state(approvals: 1)) == "1")
        #expect(StatusBadge.text(for: state(approvals: 9)) == "9")
    }
    @Test func capsLargeCounts() { #expect(StatusBadge.text(for: state(approvals: 10)) == "9+") }
    @Test func tooltipSaysWhatTheNumberMeans() {
        #expect(StatusBadge.tooltip(count: 1) == "1 thing needs you")
        #expect(StatusBadge.tooltip(count: 3) == "3 things need you")
    }
    @Test func countsEverythingThatNeedsYouNotOnlyApprovals() {
        var s = Snapshot()
        s.needsYou = NeedsYouBlock(count: 3)
        let state = NotchState.make(from: s, now: Date())
        #expect(StatusBadge.count(for: state) == 3)
        #expect(StatusBadge.text(for: state) == "3")
        // A backend that predates the block: pending approvals still count.
        #expect(StatusBadge.text(for: self.state(approvals: 2)) == "2")
    }
}

@Suite struct LastRunsTests {
    let now = Date(timeIntervalSince1970: 1_800_000_000)
    func ago(_ s: TimeInterval) -> String { ISO8601DateFormatter().string(from: now.addingTimeInterval(-s)) }

    @Test func relativeTimeUsesTheLargestUnit() {
        #expect(LastRuns.relative(from: now.addingTimeInterval(-20), to: now) == "just now")
        #expect(LastRuns.relative(from: now.addingTimeInterval(-12 * 60), to: now) == "12m ago")
        #expect(LastRuns.relative(from: now.addingTimeInterval(-3 * 3600 - 59), to: now) == "3h ago")
        #expect(LastRuns.relative(from: now.addingTimeInterval(-2 * 86400), to: now) == "2d ago")
        #expect(LastRuns.relative(from: now.addingTimeInterval(60), to: now) == "just now")   // clock skew
    }

    @Test func failedAndIdleLoopsGetALine() {
        let s = Snapshot(loops: ["issue-loop": LoopStatus(state: "failed", updatedAt: ago(720)),
                                 "inbox": LoopStatus(state: "idle", updatedAt: ago(3 * 3600))])
        #expect(LastRuns.lines(in: s, now: now) == ["issue-loop: failed 12m ago", "inbox: last ran 3h ago"])
    }

    @Test func staleCountsAsFailed() {
        let s = Snapshot(loops: ["a": LoopStatus(state: "stale", updatedAt: ago(60))])
        #expect(LastRuns.lines(in: s, now: now) == ["a: failed 1m ago"])
    }

    @Test func runningLoopsAndLoopsWithoutATimeAreLeftOut() {
        let s = Snapshot(loops: ["a": LoopStatus(state: "running", updatedAt: ago(5)),
                                 "b": LoopStatus(state: "idle"),
                                 "c": LoopStatus(state: "idle", updatedAt: "not a date")])
        #expect(LastRuns.lines(in: s, now: now).isEmpty)
    }

    @Test func mostRecentFirstAndAtMostFour() {
        var loops: [String: LoopStatus] = [:]
        for i in 0..<6 { loops["l\(i)"] = LoopStatus(state: "idle", updatedAt: ago(TimeInterval((6 - i) * 3600))) }
        let lines = LastRuns.lines(in: Snapshot(loops: loops), now: now)
        #expect(lines.count == 4)
        #expect(lines.first == "l5: last ran 1h ago")
    }
}

@Suite struct SystemPowerTests {
    /// The OS binding itself: whatever machine runs this, a reported charge is a real percentage.
    @Test func currentReadsAPlausibleState() {
        let p = PowerState.current()
        if let pct = p.batteryPercent { #expect((0...100).contains(pct)) }
        if !p.onBattery { #expect(p.batteryPercent == nil || p.batteryPercent! >= 0) }
    }
}
