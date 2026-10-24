import Foundation
import Testing
@testable import LoopXKit

/// Drives a real `loopx serve` from target/debug/loopx in a scratch root. Build it first:
///   export PATH=$HOME/.cargo/bin:$PATH; cargo build        (or point LOOPX_BINARY at a binary)
/// If the binary is missing these tests are reported as skipped, not failed.
enum RealLoopx {
    static let repoRoot: URL = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent().deletingLastPathComponent()   // file -> LoopXKitTests -> Tests
        .deletingLastPathComponent().deletingLastPathComponent()   // Tests -> LoopX -> macos
        .deletingLastPathComponent()                               // macos -> repo root
    static let binary: URL = {
        if let p = ProcessInfo.processInfo.environment["LOOPX_BINARY"], !p.isEmpty { return URL(fileURLWithPath: p) }
        return repoRoot.appendingPathComponent("target/debug/loopx")
    }()
    static let available = FileManager.default.isExecutableFile(atPath: binary.path)
    static let skipNote = "loopx binary not found at \(binary.path); run `cargo build` or set LOOPX_BINARY to enable the integration tests"
}

final class ScratchBackend: @unchecked Sendable {
    let root = tempDir()
    private var process: Process?
    var lockURL: URL { root.appendingPathComponent("loopx.lock") }

    var environment: [String: String] {
        var e = ProcessInfo.processInfo.environment
        for k in ["LOOP_ENGINEERING_HOME", "LOOPX_ROOT", "HOME"] { e[k] = root.path }
        e["LOOP_EVENTS_DIR"] = root.appendingPathComponent("events").path
        e["LOOP_UPDATE_DIR"] = root.appendingPathComponent("update").path
        return e
    }

    func run(_ args: [String]) throws -> String {
        let p = Process(); p.executableURL = RealLoopx.binary; p.arguments = args; p.environment = environment
        let pipe = Pipe(); p.standardOutput = pipe; p.standardError = pipe
        try p.run(); p.waitUntilExit()
        return String(decoding: pipe.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
    }

    /// Starts `loopx serve --port 0 --no-legacy`; returns once a live lock (different from `notPid`) exists.
    @discardableResult
    func start(notPid: Int32? = nil) async throws -> LockFile {
        let p = Process(); p.executableURL = RealLoopx.binary
        p.arguments = ["serve", "--port", "0", "--no-legacy"]; p.environment = environment
        p.standardOutput = FileHandle.nullDevice; p.standardError = FileHandle.nullDevice
        try p.run(); process = p
        let url = lockURL
        let pid = p.processIdentifier
        let ok = await waitUntil(timeout: 10) {
            if let l = LockFile.readLive(at: url), l.pid != notPid, l.pid == pid { return true }
            return false
        }
        #expect(ok, "loopx serve did not publish a live lock")
        return try #require(LockFile.readLive(at: url))
    }

    /// SIGKILL: no clean shutdown, so the lock stays behind and goes stale.
    func kill9() {
        guard let p = process else { return }
        kill(p.processIdentifier, SIGKILL); p.waitUntilExit(); process = nil
    }
    func stop() { if let p = process { p.terminate(); p.waitUntilExit() }; process = nil }
    deinit { stop(); try? FileManager.default.removeItem(at: root) }
}

@Suite(.serialized, .enabled(if: RealLoopx.available, Comment(rawValue: RealLoopx.skipNote)))
struct IntegrationTests {
    @Test func discoversTheLockAndComparesVersions() async throws {
        let backend = ScratchBackend(); defer { backend.stop() }
        let lock = try await backend.start()
        #expect(lock.token.count == 64)
        #expect(!lock.isStale())

        let ok = BackendDiscovery(lockURL: backend.lockURL, bundledVersion: lock.version)
        guard case .reuse(let l, let info) = await ok.discover() else { Issue.record("expected reuse"); return }
        #expect(l.port == lock.port); #expect(info.name == "loopx"); #expect(info.version == lock.version)

        let newer = BackendDiscovery(lockURL: backend.lockURL, bundledVersion: "99.0.0")
        guard case .offerTakeover(_, let rv, let bv) = await newer.discover() else { Issue.record("expected takeover offer"); return }
        #expect(rv == lock.version); #expect(bv == "99.0.0")

        // The supervisor reuses it rather than starting a second one (which would refuse anyway).
        let sup = Supervisor(discovery: ok, starter: ProcessStarter(binary: RealLoopx.binary, environment: backend.environment))
        guard case .ready(let rl, let started) = await sup.ensureBackend() else { Issue.record("expected ready"); return }
        #expect(rl.pid == lock.pid); #expect(!started)
    }

    @Test func supervisorStartsTheBundledBackendWhenNoneIsRunning() async throws {
        let backend = ScratchBackend(); defer { backend.stop(); killLeftover(backend) }
        let discovery = BackendDiscovery(lockURL: backend.lockURL, bundledVersion: "0.0.1")
        let starter = ProcessStarter(binary: RealLoopx.binary, arguments: ["serve", "--port", "0", "--no-legacy"], environment: backend.environment)
        let result = await Supervisor(discovery: discovery, starter: starter, startTimeout: 10).ensureBackend()
        guard case .ready(let lock, let started) = result else { Issue.record("\(result)"); return }
        #expect(started); #expect(lock.isStale() == false)
    }

    private func killLeftover(_ b: ScratchBackend) {
        if let l = LockFile.read(at: b.lockURL) { kill(l.pid, SIGTERM) }
    }

    @Test func streamsSnapshotsReconnectsAndApproves() async throws {
        let backend = ScratchBackend(); defer { backend.stop() }
        let first = try await backend.start()
        let discovery = BackendDiscovery(lockURL: backend.lockURL, bundledVersion: first.version)

        // Endpoint is re-resolved on every attempt, so a restart on a new port is followed.
        let client = SSEClient(endpoint: { discovery.liveEventsURL() },
                               backoff: Backoff(initial: 0.1, maximum: 0.5), silenceTimeout: 20, wakeHook: nil)
        let snaps = Recorder<Snapshot>(), events = Recorder<SSEClientEvent>()
        let task = Task { for await e in client.events() { events.add(e); if case .snapshot(let s) = e { snaps.add(s) } } }
        defer { task.cancel() }

        // 1. Initial snapshot arrives immediately and is idle.
        #expect(await waitUntil { snaps.count >= 1 })
        #expect(snaps.all[0].appState == .idle)
        #expect(snaps.all[0].version == 1)

        // 2. `loopx status write running --pid <us>` shows up as a running loop.
        _ = try backend.run(["status", "write", "running", "--pid", "\(getpid())"])
        #expect(await waitUntil { snaps.all.last?.appState == .running })
        let running = try #require(snaps.all.last)
        #expect(running.runningLoop != nil)
        #expect(NotchState.make(from: running, now: Date()).kind == .running)

        // 3. A pending approval flips the notch to Allow / Deny.
        let dir = backend.root.appendingPathComponent("outputs/approvals")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let fmt = ISO8601DateFormatter(); fmt.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        let now = Date()
        let doc = """
        {"id":"apr_0123456789ab","kind":"open_mr","run_id":"run_1","project":"web","issue_iid":7,
         "title":"Fix #7: x","branch":"loop/issue-7","target_branch":"main","summary":"s","status":"pending",
         "created_at":"\(fmt.string(from: now))","expires_at":"\(fmt.string(from: now.addingTimeInterval(600)))",
         "decided_at":null,"decided_by":null}
        """
        try doc.write(to: dir.appendingPathComponent("apr_0123456789ab.json"), atomically: true, encoding: .utf8)
        #expect(await waitUntil { snaps.all.last?.appState == .awaitingApproval })
        let awaiting = NotchState.make(from: try #require(snaps.all.last), now: Date())
        #expect(awaiting.kind == .awaitingApproval)
        #expect(awaiting.actions == [.approve(id: "apr_0123456789ab"), .deny(id: "apr_0123456789ab")])

        // 4. Wrong token is refused; the right one approves; a second decision conflicts.
        let bad = ApprovalsAPI(lock: first.with(token: "nope"))
        await #expect(throws: ApprovalError.forbidden) { try await bad.decide(id: "apr_0123456789ab", .approve) }
        let api = ApprovalsAPI(lock: first)
        let listed = try await api.list()
        #expect(listed.pending.map(\.id) == ["apr_0123456789ab"])
        let decided = try await api.decide(id: "apr_0123456789ab", .approve)
        #expect(decided.status == "approved"); #expect(decided.decidedBy == "mac-app")
        await #expect(throws: ApprovalError.alreadyDecided("approved")) { try await api.decide(id: "apr_0123456789ab", .deny) }
        await #expect(throws: ApprovalError.notFound) { try await api.decide(id: "apr_doesnotexist", .approve) }
        #expect(await waitUntil { snaps.all.last?.approvals.pending.isEmpty == true && snaps.all.last?.appState == .running })
        #expect(try await api.list().pending.isEmpty)

        // 5. Kill the backend hard: the stream drops, the lock goes stale, and nothing is served.
        let before = snaps.count
        backend.kill9()
        #expect(await waitUntil { events.all.contains { if case .disconnected = $0 { return true } else { return false } } })
        #expect(LockFile.readLive(at: backend.lockURL) == nil)
        #expect(discovery.liveEventsURL() == nil)
        guard case .start = await discovery.discover() else { Issue.record("a stale lock must mean start"); return }

        // 6. Start it again (new pid, probably new port): the client finds it and is caught up by
        //    the first snapshot alone (status.json persisted on disk, so it is still running).
        let second = try await backend.start(notPid: first.pid)
        #expect(second.pid != first.pid)
        #expect(await waitUntil(timeout: 10) { snaps.count > before })
        #expect(await waitUntil { snaps.all.last?.appState == .running })
        let last = try #require(snaps.all.last)
        #expect(last.approvals.pending.isEmpty)
        // And the new token works over HTTP too.
        #expect(try await ApprovalsAPI(lock: second).list().pending.isEmpty)
    }
}

extension LockFile {
    func with(token: String) -> LockFile { var c = self; c.token = token; return c }
}
