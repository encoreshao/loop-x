import Foundation
import IOKit.pwr_mgt

/// The OS power assertion, behind a seam so tests never touch IOKit.
public protocol PowerAssertion: Sendable {
    /// Returns an id to hand back to `end`, or nil when the system refused.
    func begin(reason: String) -> UInt32?
    func end(_ id: UInt32)
}

/// Stops idle *system* sleep only: the display may still sleep, and closing the lid still sleeps the Mac.
public struct IOKitPowerAssertion: PowerAssertion {
    public init() {}

    public func begin(reason: String) -> UInt32? {
        var id = IOPMAssertionID(0)
        let r = IOPMAssertionCreateWithName(kIOPMAssertionTypePreventUserIdleSystemSleep as CFString,
                                            IOPMAssertionLevel(kIOPMAssertionLevelOn), reason as CFString, &id)
        return r == kIOReturnSuccess ? id : nil
    }

    public func end(_ id: UInt32) { IOPMAssertionRelease(IOPMAssertionID(id)) }
}

/// Keeps the Mac awake while a loop or topic is running, so a long agent run is not cut off by idle sleep.
/// Feed it every snapshot; it holds at most one assertion and drops it as soon as nothing is running.
public final class KeepAwake {
    private let power: PowerAssertion
    private var held: UInt32?

    public init(power: PowerAssertion = IOKitPowerAssertion()) { self.power = power }

    public var isHolding: Bool { held != nil }

    public func update(snapshot: Snapshot?, enabled: Bool) {
        let running = enabled ? snapshot.flatMap { TaskNotifications.tasks(in: $0).first { $0.state == .running } } : nil
        guard let running else { release(); return }
        if held == nil { held = power.begin(reason: "LoopX: \(running.name) is running") }
    }

    public func release() {
        guard let id = held else { return }
        held = nil
        power.end(id)
    }

    deinit { release() }
}
