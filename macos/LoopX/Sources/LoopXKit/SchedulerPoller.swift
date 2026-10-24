import Foundation

/// Runs `loopx scheduler` (one poll that starts every loop that is due) now and then every
/// `interval` while it is started. The scheduler decides what is due and skips a loop that is
/// already running, so a poll that overlaps another is harmless.
public final class SchedulerPoller: @unchecked Sendable {
    public typealias Sleeper = @Sendable (TimeInterval) async -> Void

    public let interval: TimeInterval
    let poll: @Sendable () async -> Void
    let sleeper: Sleeper
    private let lock = NSLock()
    private var task: Task<Void, Never>?

    public init(interval: TimeInterval = 900, poll: @escaping @Sendable () async -> Void,
                sleeper: @escaping Sleeper = { try? await Task.sleep(nanoseconds: UInt64($0 * 1_000_000_000)) }) {
        self.interval = interval; self.poll = poll; self.sleeper = sleeper
    }

    public var isRunning: Bool { lock.withLock { task != nil } }

    public func start() {
        lock.withLock {
            guard task == nil else { return }
            task = Task { [poll, sleeper, interval] in
                while !Task.isCancelled {
                    await poll()
                    await sleeper(interval)
                }
            }
        }
    }

    public func stop() {
        lock.withLock { task?.cancel(); task = nil }
    }

    deinit { task?.cancel() }
}
