import Foundation
import IOKit.ps

public struct PowerState: Equatable, Sendable {
    public var onBattery: Bool
    /// Charge 0...100, nil on a Mac without a battery or when it cannot be read.
    public var batteryPercent: Int?
    public var lowPowerMode: Bool

    public init(onBattery: Bool, batteryPercent: Int?, lowPowerMode: Bool) {
        self.onBattery = onBattery; self.batteryPercent = batteryPercent; self.lowPowerMode = lowPowerMode
    }

    /// The Mac's current power state, read from IOKit and `ProcessInfo`.
    public static func current() -> PowerState {
        let low = ProcessInfo.processInfo.isLowPowerModeEnabled
        guard let info = IOPSCopyPowerSourcesInfo()?.takeRetainedValue(),
              let list = IOPSCopyPowerSourcesList(info)?.takeRetainedValue() as? [CFTypeRef] else {
            return PowerState(onBattery: false, batteryPercent: nil, lowPowerMode: low)
        }
        let onBattery = (IOPSGetProvidingPowerSourceType(info)?.takeUnretainedValue() as String?) == kIOPSBatteryPowerValue
        var percent: Int?
        for source in list {
            guard let d = IOPSGetPowerSourceDescription(info, source)?.takeUnretainedValue() as? [String: Any],
                  d[kIOPSTypeKey] as? String == kIOPSInternalBatteryType,
                  let cur = d[kIOPSCurrentCapacityKey] as? Int, let max = d[kIOPSMaxCapacityKey] as? Int, max > 0
            else { continue }
            percent = cur * 100 / max
        }
        return PowerState(onBattery: onBattery, batteryPercent: percent, lowPowerMode: low)
    }
}

/// Whether the app-driven scheduler should poll now. A due loop starts a long, costly agent run, which is
/// a poor use of a nearly flat battery or a Mac the user asked to save power. Skipped polls are not lost:
/// the next one, 15 minutes later, starts whatever is still due.
public enum SchedulerPolicy {
    public static let lowBatteryPercent = 20

    public static func shouldPoll(power: PowerState, enabled: Bool) -> Bool {
        guard enabled else { return true }
        if power.lowPowerMode { return false }
        if power.onBattery, let pct = power.batteryPercent, pct < lowBatteryPercent { return false }
        return true
    }
}
