import Foundation
import Testing
@testable import LoopXKit

final class FakeService: ServiceRegistering, @unchecked Sendable {
    private let l = NSLock()
    private var _status: ServiceStatus
    private(set) var calls: [String] = []
    var registerError: Error?
    var statusAfterRegister: ServiceStatus = .enabled
    init(_ s: ServiceStatus = .notRegistered) { _status = s }
    var status: ServiceStatus { l.lock(); defer { l.unlock() }; return _status }
    func register() throws {
        l.lock(); calls.append("register"); l.unlock()
        if let registerError { throw registerError }
        l.lock(); _status = statusAfterRegister; l.unlock()
    }
    func unregister() throws { l.lock(); calls.append("unregister"); _status = .notRegistered; l.unlock() }
}

struct SomeError: Error {}

@Suite struct LaunchAtLoginTests {
    @Test func registersAndUnregistersOnlyWhenNeeded() throws {
        let s = FakeService(.notRegistered)
        let l = LaunchAtLogin(service: s)
        #expect(!l.isEnabled)
        try l.setEnabled(false); #expect(s.calls.isEmpty)
        try l.setEnabled(true); #expect(s.calls == ["register"]); #expect(l.isEnabled)
        try l.setEnabled(true); #expect(s.calls == ["register"])
        try l.setEnabled(false); #expect(s.calls == ["register", "unregister"])
    }
    @Test func requiresApprovalCountsAsOnAndIsFlagged() throws {
        let s = FakeService(.requiresApproval)
        let l = LaunchAtLogin(service: s)
        #expect(l.isEnabled); #expect(l.needsApproval)
        try l.setEnabled(true); #expect(s.calls.isEmpty)
    }
    @Test func errorsPropagate() {
        let s = FakeService(.notRegistered); s.registerError = SomeError()
        #expect(throws: SomeError.self) { try LaunchAtLogin(service: s).setEnabled(true) }
    }
}

@Suite struct SMAppServiceStarterTests {
    @Test func freshRegistersOnce() async throws {
        let s = FakeService(.notRegistered)
        try await SMAppServiceStarter(service: s).start()
        #expect(s.calls == ["register"])
    }
    @Test func alreadyEnabledButNotServingReRegisters() async throws {
        let s = FakeService(.enabled)
        try await SMAppServiceStarter(service: s).start()
        #expect(s.calls == ["unregister", "register"])
    }
    @Test func needsApprovalIsReportedNotRetried() async {
        let s = FakeService(.requiresApproval)
        await #expect(throws: BackendError.needsApproval) { try await SMAppServiceStarter(service: s).start() }
        #expect(s.calls.isEmpty)
        let s2 = FakeService(.notRegistered); s2.statusAfterRegister = .requiresApproval
        await #expect(throws: BackendError.needsApproval) { try await SMAppServiceStarter(service: s2).start() }
    }
    @Test func registerFailureBecomesLaunchFailed() async {
        let s = FakeService(.notRegistered); s.registerError = SomeError()
        do { try await SMAppServiceStarter(service: s).start(); Issue.record("expected throw") }
        catch { guard case .launchFailed = error as? BackendError else { Issue.record("\(error)"); return } }
    }
}

@Suite struct ProcessStarterTests {
    @Test func missingBinaryIsReported() async {
        let p = ProcessStarter(binary: URL(fileURLWithPath: "/nonexistent/loopx"))
        await #expect(throws: BackendError.binaryNotFound("/nonexistent/loopx")) { try await p.start() }
    }
    @Test func startsTheBinaryWithArgumentsAndEnvironment() async throws {
        let dir = tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let script = dir.appendingPathComponent("fake-loopx")
        try "#!/bin/sh\necho \"$@ $FOO\" > \"$OUT\"\n".write(to: script, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: script.path)
        let out = dir.appendingPathComponent("out.txt")
        try await ProcessStarter(binary: script, arguments: ["serve", "--port", "0"],
                                 environment: ["FOO": "bar", "OUT": out.path], logURL: dir.appendingPathComponent("log/x.log")).start()
        #expect(await waitUntil { (try? String(contentsOf: out, encoding: .utf8)) == "serve --port 0 bar\n" })
    }
}

@Suite struct DiscoveryTests {
    func lockFile(_ dir: URL, version: String = "0.4.0", pid: Int32 = 4242) throws -> URL {
        let url = dir.appendingPathComponent("loopx.lock")
        try #"{"pid":\#(pid),"port":8420,"version":"\#(version)","started_at":"t","token":"tok"}"#.write(to: url, atomically: true, encoding: .utf8)
        return url
    }
    func discovery(_ url: URL, bundled: String, alive: Bool = true, info: VersionInfo?) -> BackendDiscovery {
        BackendDiscovery(lockURL: url, bundledVersion: bundled, pidAlive: { _ in alive }, fetchVersion: { _ in info })
    }

    @Test func noLockMeansStart() async {
        let d = tempDir(); defer { try? FileManager.default.removeItem(at: d) }
        let r = await discovery(d.appendingPathComponent("loopx.lock"), bundled: "0.4.0", info: nil).discover()
        #expect(r == .start)
    }
    @Test func staleLockMeansStart() async throws {
        let d = tempDir(); defer { try? FileManager.default.removeItem(at: d) }
        let r = await discovery(try lockFile(d), bundled: "0.4.0", alive: false, info: VersionInfo(version: "0.4.0")).discover()
        #expect(r == .start)
    }
    @Test func corruptLockMeansStart() async throws {
        let d = tempDir(); defer { try? FileManager.default.removeItem(at: d) }
        let url = d.appendingPathComponent("loopx.lock"); try "{{".write(to: url, atomically: true, encoding: .utf8)
        #expect(await discovery(url, bundled: "0.4.0", info: nil).discover() == .start)
    }
    @Test func liveSameOrNewerIsReused() async throws {
        let d = tempDir(); defer { try? FileManager.default.removeItem(at: d) }
        let url = try lockFile(d)
        for running in ["0.4.0", "0.5.0"] {
            let info = VersionInfo(version: running)
            let r = await discovery(url, bundled: "0.4.0", info: info).discover()
            guard case .reuse(let lock, let got) = r else { Issue.record("expected reuse, got \(r)"); continue }
            #expect(lock.port == 8420); #expect(got == info)
        }
    }
    @Test func olderRunningBackendOffersTakeover() async throws {
        let d = tempDir(); defer { try? FileManager.default.removeItem(at: d) }
        let url = try lockFile(d, version: "0.3.0")
        let r = await discovery(url, bundled: "0.4.0", info: VersionInfo(version: "0.3.0")).discover()
        guard case .offerTakeover(let lock, let rv, let bv) = r else { Issue.record("got \(r)"); return }
        #expect(lock.pid == 4242); #expect(rv == "0.3.0"); #expect(bv == "0.4.0")
    }
    @Test func comparesNumericallyNotAsStrings() async throws {
        let d = tempDir(); defer { try? FileManager.default.removeItem(at: d) }
        let url = try lockFile(d)
        let r = await discovery(url, bundled: "0.9.0", info: VersionInfo(version: "0.10.0")).discover()
        guard case .reuse = r else { Issue.record("0.10.0 is newer than 0.9.0, got \(r)"); return }
    }
    @Test func unparseableVersionsAreReusedNotKilled() async throws {
        let d = tempDir(); defer { try? FileManager.default.removeItem(at: d) }
        let r = await discovery(try lockFile(d), bundled: "0.4.0", info: VersionInfo(version: "dev")).discover()
        guard case .reuse = r else { Issue.record("got \(r)"); return }
    }
    @Test func liveLockThatDoesNotAnswerIsUnresponsive() async throws {
        let d = tempDir(); defer { try? FileManager.default.removeItem(at: d) }
        let r = await discovery(try lockFile(d), bundled: "0.4.0", info: nil).discover()
        guard case .unresponsive(let l) = r else { Issue.record("got \(r)"); return }
        #expect(l.pid == 4242)
        let other = await discovery(try lockFile(d), bundled: "0.4.0", info: VersionInfo(name: "nginx", version: "1.0.0")).discover()
        guard case .unresponsive = other else { Issue.record("got \(other)"); return }
    }
    @Test func liveEventsURLOnlyForLiveLocks() throws {
        let d = tempDir(); defer { try? FileManager.default.removeItem(at: d) }
        let url = try lockFile(d)
        #expect(BackendDiscovery(lockURL: url, bundledVersion: "1.0.0", pidAlive: { _ in true }).liveEventsURL()?.absoluteString
                == "http://127.0.0.1:8420/api/events")
        #expect(BackendDiscovery(lockURL: url, bundledVersion: "1.0.0", pidAlive: { _ in false }).liveEventsURL() == nil)
    }
}

/// A starter that "starts" by writing a lock file after a few polls, like a real backend booting.
final class FakeStarter: BackendStarter, @unchecked Sendable {
    let lockURL: URL; let version: String
    var error: Error?
    private let l = NSLock(); private var _starts = 0
    var writeLock = true
    init(lockURL: URL, version: String) { self.lockURL = lockURL; self.version = version }
    var starts: Int { l.lock(); defer { l.unlock() }; return _starts }
    func start() async throws {
        l.withLock { _starts += 1 }
        if let error { throw error }
        if writeLock {
            try #"{"pid":999,"port":9000,"version":"\#(version)","started_at":"t","token":"n"}"#
                .write(to: lockURL, atomically: true, encoding: .utf8)
        }
    }
}

@Suite struct SupervisorTests {
    func make(_ dir: URL, bundled: String = "0.4.0", running: VersionInfo?, alive: @escaping @Sendable (Int32) -> Bool = { _ in true },
              starter: BackendStarter, fallback: BackendStarter? = nil,
              terminator: @escaping Supervisor.Terminator = { _ in true },
              timeout: TimeInterval = 1) -> Supervisor {
        let url = dir.appendingPathComponent("loopx.lock")
        let d = BackendDiscovery(lockURL: url, bundledVersion: bundled, pidAlive: alive, fetchVersion: { _ in running })
        return Supervisor(discovery: d, starter: starter, fallback: fallback, primaryTimeout: 0.3,
                          terminator: terminator, startTimeout: timeout,
                          pollInterval: 0.1, sleeper: { _ in })
    }

    @Test func reusesALiveBackendWithoutStarting() async throws {
        let dir = tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let url = dir.appendingPathComponent("loopx.lock")
        try #"{"pid":7,"port":8420,"version":"0.4.0","started_at":"t","token":"k"}"#.write(to: url, atomically: true, encoding: .utf8)
        let starter = FakeStarter(lockURL: url, version: "0.4.0")
        let r = await make(dir, running: VersionInfo(version: "0.4.0"), starter: starter).ensureBackend()
        guard case .ready(let lock, let started) = r else { Issue.record("\(r)"); return }
        #expect(lock.pid == 7); #expect(!started); #expect(starter.starts == 0)
    }

    @Test func startsTheBundledBackendWhenNoneRunsAndWaitsForTheLock() async throws {
        let dir = tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let starter = FakeStarter(lockURL: dir.appendingPathComponent("loopx.lock"), version: "0.4.0")
        let r = await make(dir, running: VersionInfo(version: "0.4.0"), starter: starter).ensureBackend()
        guard case .ready(let lock, let started) = r else { Issue.record("\(r)"); return }
        #expect(lock.port == 9000); #expect(started); #expect(starter.starts == 1)
    }

    @Test func timesOutWhenTheBackendNeverComesUp() async throws {
        let dir = tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let starter = FakeStarter(lockURL: dir.appendingPathComponent("loopx.lock"), version: "0.4.0")
        starter.writeLock = false
        let r = await make(dir, running: nil, starter: starter, timeout: 0.5).ensureBackend()
        guard case .failed(let m) = r else { Issue.record("\(r)"); return }
        #expect(m.contains("did not come up"))
    }

    @Test func fallsBackToSpawningWhenTheAgentNeverServes() async throws {
        let dir = tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let lock = dir.appendingPathComponent("loopx.lock")
        let agent = FakeStarter(lockURL: lock, version: "0.4.0"); agent.writeLock = false
        let spawn = FakeStarter(lockURL: lock, version: "0.4.0")
        let r = await make(dir, running: VersionInfo(version: "0.4.0"), starter: agent, fallback: spawn, timeout: 1).ensureBackend()
        guard case .ready = r else { Issue.record("\(r)"); return }
        #expect(agent.starts == 1); #expect(spawn.starts == 1)
    }

    @Test func fallsBackWhenRegisteringTheAgentFails() async throws {
        let dir = tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let lock = dir.appendingPathComponent("loopx.lock")
        let agent = FakeStarter(lockURL: lock, version: "0.4.0"); agent.error = BackendError.launchFailed("denied")
        let spawn = FakeStarter(lockURL: lock, version: "0.4.0")
        let r = await make(dir, running: VersionInfo(version: "0.4.0"), starter: agent, fallback: spawn).ensureBackend()
        guard case .ready = r else { Issue.record("\(r)"); return }
        #expect(spawn.starts == 1)
    }

    @Test func doesNotFallBackWhenTheUserMustApproveTheLoginItem() async throws {
        let dir = tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let lock = dir.appendingPathComponent("loopx.lock")
        let agent = FakeStarter(lockURL: lock, version: "0.4.0"); agent.error = BackendError.needsApproval
        let spawn = FakeStarter(lockURL: lock, version: "0.4.0")
        let r = await make(dir, running: nil, starter: agent, fallback: spawn).ensureBackend()
        guard case .failed(let m) = r else { Issue.record("\(r)"); return }
        #expect(m.contains("Login Items")); #expect(spawn.starts == 0)
    }

    @Test func reportsBothFailuresWhenTheFallbackAlsoFails() async throws {
        let dir = tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let lock = dir.appendingPathComponent("loopx.lock")
        let agent = FakeStarter(lockURL: lock, version: "0.4.0"); agent.writeLock = false
        let spawn = FakeStarter(lockURL: lock, version: "0.4.0"); spawn.error = BackendError.binaryNotFound("/x")
        let r = await make(dir, running: nil, starter: agent, fallback: spawn).ensureBackend()
        guard case .failed(let m) = r else { Issue.record("\(r)"); return }
        #expect(m.contains("did not come up") && m.contains("Fallback"))
    }

    @Test func offersTakeoverForOlderBackendAndNeverKillsUninvited() async throws {
        let dir = tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let url = dir.appendingPathComponent("loopx.lock")
        try #"{"pid":7,"port":8420,"version":"0.3.0","started_at":"t","token":"k"}"#.write(to: url, atomically: true, encoding: .utf8)
        let killed = Recorder<Int32>()
        let starter = FakeStarter(lockURL: url, version: "0.4.0")
        let sup = make(dir, running: VersionInfo(version: "0.3.0"), starter: starter, terminator: { killed.add($0); return true })
        let r = await sup.ensureBackend()
        #expect(r == .takeoverOffered(running: LockFile(pid: 7, port: 8420, version: "0.3.0", startedAt: "t", token: "k"),
                                      runningVersion: "0.3.0", bundledVersion: "0.4.0"))
        #expect(killed.count == 0); #expect(starter.starts == 0)
    }

    @Test func takeOverTerminatesThenStartsTheBundledOne() async throws {
        let dir = tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let url = dir.appendingPathComponent("loopx.lock")
        let old = LockFile(pid: 7, port: 8420, version: "0.3.0", startedAt: "t", token: "k")
        let order = Recorder<String>()
        let starter = FakeStarter(lockURL: url, version: "0.4.0")
        let sup = make(dir, running: VersionInfo(version: "0.4.0"), starter: starter, terminator: { order.add("kill \($0)"); return true })
        let r = await sup.takeOver(running: old)
        guard case .ready(_, let started) = r else { Issue.record("\(r)"); return }
        #expect(started); #expect(order.all == ["kill 7"]); #expect(starter.starts == 1)
    }

    @Test func takeOverFailsCleanlyIfTheOldOneWillNotDie() async throws {
        let dir = tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let starter = FakeStarter(lockURL: dir.appendingPathComponent("loopx.lock"), version: "0.4.0")
        let sup = make(dir, running: nil, starter: starter, terminator: { _ in false })
        let r = await sup.takeOver(running: LockFile(pid: 7, port: 1, version: "0.3.0", token: "k"))
        guard case .failed = r else { Issue.record("\(r)"); return }
        #expect(starter.starts == 0)
    }

    @Test func unresponsiveHolderIsAFailureNotASilentStart() async throws {
        let dir = tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let url = dir.appendingPathComponent("loopx.lock")
        try #"{"pid":7,"port":8420,"version":"0.4.0","started_at":"t","token":"k"}"#.write(to: url, atomically: true, encoding: .utf8)
        let starter = FakeStarter(lockURL: url, version: "0.4.0")
        let r = await make(dir, running: nil, starter: starter).ensureBackend()
        guard case .failed(let m) = r else { Issue.record("\(r)"); return }
        #expect(m.contains("pid 7")); #expect(starter.starts == 0)
    }

    @Test func starterErrorsBecomeReadableMessages() async throws {
        let dir = tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let starter = FakeStarter(lockURL: dir.appendingPathComponent("loopx.lock"), version: "0.4.0")
        starter.error = BackendError.needsApproval
        let r = await make(dir, running: nil, starter: starter).ensureBackend()
        guard case .failed(let m) = r else { Issue.record("\(r)"); return }
        #expect(m.contains("Login Items"))
        starter.error = BackendError.binaryNotFound("/x/loopx")
        let r2 = await make(dir, running: nil, starter: starter).ensureBackend()
        guard case .failed(let m2) = r2 else { Issue.record("\(r2)"); return }
        #expect(m2.contains("/x/loopx"))
    }

    @Test func realTerminatorStopsAChildAndIsIdempotentForDeadPids() async throws {
        let p = Process(); p.executableURL = URL(fileURLWithPath: "/bin/sleep"); p.arguments = ["30"]
        try p.run()
        #expect(await Supervisor.terminateProcess(p.processIdentifier))
        p.waitUntilExit()
        #expect(await Supervisor.terminateProcess(p.processIdentifier))
        #expect(await Supervisor.terminateProcess(0))
        #expect(await Supervisor.terminateProcess(1) || true)   // never signals pid 1 (guard)
    }
}
