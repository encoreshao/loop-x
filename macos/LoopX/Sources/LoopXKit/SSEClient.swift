import Foundation
import AppKit

public enum SSEClientEvent: Equatable, Sendable {
    case connected
    case snapshot(Snapshot)
    case disconnected(String)
}

/// Registers a callback for "the machine woke up" and returns a closure that unregisters it.
public typealias WakeHook = @Sendable (@escaping @Sendable () -> Void) -> (@Sendable () -> Void)

public enum SystemWake {
    /// Real hook: `NSWorkspace.didWakeNotification`. Tests inject their own.
    public static let hook: WakeHook = { fire in
        let center = NSWorkspace.shared.notificationCenter
        let token = center.addObserver(forName: NSWorkspace.didWakeNotification, object: nil, queue: nil) { _ in fire() }
        return { center.removeObserver(token) }
    }
}

/// Server-sent-events client for `GET /api/events`.
///
/// * Reconnects forever with `Backoff` (1s doubling to 30s); a received message resets it.
/// * No bytes (messages *or* `: ping` comments) for `silenceTimeout` means the link is dead.
/// * `endpoint` is asked again before every attempt, so a backend that restarted on a new port
///   is found again.
/// * A wake notification drops the current connection and reconnects immediately.
public final class SSEClient: @unchecked Sendable {
    public typealias Endpoint = @Sendable () async -> URL?
    public typealias Sleeper = @Sendable (TimeInterval) async throws -> Void

    private let endpoint: Endpoint
    private let session: URLSession
    private let silenceTimeout: TimeInterval
    private let wakeHook: WakeHook?
    private let sleeper: Sleeper
    private let initialBackoff: Backoff

    private let lock = NSLock()
    private var current: Task<Void, Never>?
    private var wakePending = false

    public init(endpoint: @escaping Endpoint,
                session: URLSession = SSEClient.makeSession(),
                backoff: Backoff = Backoff(),
                silenceTimeout: TimeInterval = 45,
                wakeHook: WakeHook? = SystemWake.hook,
                sleeper: @escaping Sleeper = { try await Task.sleep(nanoseconds: UInt64($0 * 1_000_000_000)) }) {
        self.endpoint = endpoint; self.session = session; self.initialBackoff = backoff
        self.silenceTimeout = silenceTimeout; self.wakeHook = wakeHook; self.sleeper = sleeper
    }

    public static func makeSession() -> URLSession {
        let c = URLSessionConfiguration.ephemeral
        c.timeoutIntervalForRequest = 3600   // our own watchdog decides when it is dead
        c.timeoutIntervalForResource = .infinity
        c.waitsForConnectivity = false
        return URLSession(configuration: c)
    }

    /// Starts connecting. Cancel the consuming task (or stop iterating) to shut it down.
    public func events() -> AsyncStream<SSEClientEvent> {
        AsyncStream { continuation in
            let runner = Task { [self] in await run(continuation) }
            let unhook = wakeHook?({ [weak self] in self?.reconnectNow() })
            continuation.onTermination = { _ in runner.cancel(); unhook?() }
        }
    }

    /// Drops the live connection (or backoff sleep) and connects again without waiting.
    public func reconnectNow() {
        lock.lock(); wakePending = true; let t = current; lock.unlock()
        t?.cancel()
    }

    private func consumeWake() -> Bool {
        lock.lock(); defer { lock.unlock() }
        let w = wakePending; wakePending = false; return w
    }

    private func setCurrent(_ t: Task<Void, Never>?) {
        lock.lock(); current = t; lock.unlock()
    }

    private func run(_ out: AsyncStream<SSEClientEvent>.Continuation) async {
        var backoff = initialBackoff
        while !Task.isCancelled {
            var gotMessage = false
            var reason = "no backend"
            if let url = await endpoint() {
                let box = ResultBox()
                let attempt = Task { [self] in
                    box.set(await connectOnce(url, out, gotMessage: { box.markMessage() }))
                }
                setCurrent(attempt)
                await withTaskCancellationHandler { await attempt.value } onCancel: { attempt.cancel() }
                setCurrent(nil)
                reason = box.reason; gotMessage = box.gotMessage
            }
            if Task.isCancelled { break }
            out.yield(.disconnected(reason))
            if gotMessage { backoff.reset() }
            if consumeWake() { backoff.reset(); continue }
            let delay = backoff.next()
            let sleeper = self.sleeper
            let nap = Task<Void, Never> { try? await sleeper(delay) }
            setCurrent(nap)
            await withTaskCancellationHandler { await nap.value } onCancel: { nap.cancel() }
            setCurrent(nil)
            _ = consumeWake()   // a wake during the nap just ended it early
        }
        out.finish()
    }

    private final class ResultBox: @unchecked Sendable {
        private let l = NSLock()
        private var _reason = "cancelled", _got = false
        func set(_ r: String) { l.lock(); _reason = r; l.unlock() }
        func markMessage() { l.lock(); _got = true; l.unlock() }
        var reason: String { l.lock(); defer { l.unlock() }; return _reason }
        var gotMessage: Bool { l.lock(); defer { l.unlock() }; return _got }
    }

    private final class Activity: @unchecked Sendable {
        private let l = NSLock(); private var t = Date()
        func touch() { l.lock(); t = Date(); l.unlock() }
        var idle: TimeInterval { l.lock(); defer { l.unlock() }; return Date().timeIntervalSince(t) }
    }

    private func connectOnce(_ url: URL, _ out: AsyncStream<SSEClientEvent>.Continuation,
                             gotMessage: @escaping @Sendable () -> Void) async -> String {
        var req = URLRequest(url: url)
        req.setValue("text/event-stream", forHTTPHeaderField: "Accept")
        req.setValue("no-cache", forHTTPHeaderField: "Cache-Control")
        req.timeoutInterval = 3600
        let activity = Activity()
        let timeout = silenceTimeout
        let session = self.session
        let sleeper = self.sleeper
        return await withTaskGroup(of: String.self) { group in
            group.addTask {
                do {
                    let (bytes, resp) = try await session.bytes(for: req)
                    guard let http = resp as? HTTPURLResponse, http.statusCode == 200 else {
                        return "HTTP \((resp as? HTTPURLResponse)?.statusCode ?? 0)"
                    }
                    activity.touch()
                    out.yield(.connected)
                    var parser = SSEParser()
                    var chunk: [UInt8] = []
                    for try await byte in bytes {
                        activity.touch()
                        chunk.append(byte)
                        guard byte == 0x0A else { continue }
                        for m in parser.feed(chunk) where m.event == "snapshot" {
                            if let snap = try? Snapshot.decode(Data(m.data.utf8)) {
                                gotMessage()
                                out.yield(.snapshot(snap))
                            }
                        }
                        chunk.removeAll(keepingCapacity: true)
                    }
                    return "stream ended"
                } catch is CancellationError {
                    return "cancelled"
                } catch {
                    return Task.isCancelled ? "cancelled" : "\((error as NSError).localizedDescription)"
                }
            }
            group.addTask {
                // Watchdog: polls the shared activity clock.
                let step = max(timeout / 5, 0.01)
                while !Task.isCancelled {
                    do { try await sleeper(step) } catch { return "cancelled" }
                    if activity.idle > timeout { return "no data for \(timeout)s" }
                }
                return "cancelled"
            }
            let first = await group.next() ?? "cancelled"
            group.cancelAll()
            return first
        }
    }
}
