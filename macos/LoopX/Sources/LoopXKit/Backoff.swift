import Foundation

/// 1s, 2s, 4s ... capped at 30s; `reset()` after a healthy connection.
public struct Backoff: Equatable, Sendable {
    public let initial: TimeInterval
    public let maximum: TimeInterval
    public private(set) var current: TimeInterval

    public init(initial: TimeInterval = 1, maximum: TimeInterval = 30) {
        self.initial = initial; self.maximum = maximum; self.current = initial
    }

    /// Delay to wait now; the following call returns double (capped).
    public mutating func next() -> TimeInterval {
        let d = current
        current = min(current * 2, maximum)
        return d
    }

    public mutating func reset() { current = initial }
}
