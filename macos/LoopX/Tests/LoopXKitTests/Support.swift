import Foundation
import Network
@testable import LoopXKit

/// Polls until `condition` holds or `timeout` elapses.
func waitUntil(timeout: TimeInterval = 5, interval: TimeInterval = 0.02,
               _ condition: @Sendable () async -> Bool) async -> Bool {
    let deadline = Date().addingTimeInterval(timeout)
    while Date() < deadline {
        if await condition() { return true }
        try? await Task.sleep(nanoseconds: UInt64(interval * 1_000_000_000))
    }
    return await condition()
}

/// Thread-safe list that tests append to from stream consumers.
final class Recorder<T: Sendable>: @unchecked Sendable {
    private let l = NSLock(); private var items: [T] = []
    func add(_ t: T) { l.lock(); items.append(t); l.unlock() }
    var all: [T] { l.lock(); defer { l.unlock() }; return items }
    var count: Int { all.count }
}

func tempDir() -> URL {
    let d = FileManager.default.temporaryDirectory.appendingPathComponent("loopxkit-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

func snapshotJSON(app: String = "idle", extra: String = "") -> String {
    #"{"version":1,"app_state":"\#(app)","loops":{},"topics":{},"approvals":{"pending":[]},"attention":null,"recent_events":[]\#(extra)}"#
}

/// A scripted HTTP/SSE server on 127.0.0.1 (Network.framework), so client tests need no real backend.
final class TestSSEServer: @unchecked Sendable {
    typealias Handler = @Sendable (_ index: Int, _ send: @escaping @Sendable (String) -> Void,
                                   _ close: @escaping @Sendable () -> Void) -> Void
    private let listener: NWListener
    private let queue = DispatchQueue(label: "TestSSEServer")
    private let l = NSLock()
    private var _connections = 0
    private var live: [NWConnection] = []
    let handler: Handler
    private(set) var port: UInt16 = 0

    init(handler: @escaping Handler) throws {
        self.handler = handler
        listener = try NWListener(using: .tcp, on: .any)
        let ready = DispatchSemaphore(value: 0)
        listener.stateUpdateHandler = { if case .ready = $0 { ready.signal() } }
        listener.newConnectionHandler = { [unowned self] c in accept(c) }
        listener.start(queue: queue)
        _ = ready.wait(timeout: .now() + 5)
        port = listener.port?.rawValue ?? 0
    }

    var url: URL { URL(string: "http://127.0.0.1:\(port)/api/events")! }
    var connectionCount: Int { l.lock(); defer { l.unlock() }; return _connections }

    private func accept(_ c: NWConnection) {
        l.lock(); let index = _connections; _connections += 1; live.append(c); l.unlock()
        c.start(queue: queue)
        c.receive(minimumIncompleteLength: 1, maximumLength: 65536) { [handler] _, _, _, _ in
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
            c.send(content: Data(head.utf8), completion: .contentProcessed { _ in })
            handler(index, { s in c.send(content: Data(s.utf8), completion: .contentProcessed { _ in }) },
                    { c.cancel() })
        }
    }

    func stop() {
        listener.cancel()
        l.lock(); let cs = live; live = []; l.unlock()
        cs.forEach { $0.cancel() }
    }
    deinit { stop() }
}
