import Foundation
import ServiceManagement

// MARK: - SMAppService behind a protocol

public enum ServiceStatus: Equatable, Sendable {
    case notRegistered, enabled, requiresApproval, notFound
}

/// What the app needs from `SMAppService`. Tests use a fake; only `SMAppServiceAdapter`
/// ever touches the real thing.
public protocol ServiceRegistering: Sendable {
    var status: ServiceStatus { get }
    func register() throws
    func unregister() throws
}

public struct SMAppServiceAdapter: ServiceRegistering, @unchecked Sendable {
    public enum Kind: Sendable { case mainApp, agent(plistName: String) }
    private let service: SMAppService

    public init(_ kind: Kind) {
        switch kind {
        case .mainApp: service = SMAppService.mainApp
        case .agent(let name): service = SMAppService.agent(plistName: name)
        }
    }

    public var status: ServiceStatus {
        switch service.status {
        case .notRegistered: return .notRegistered
        case .enabled: return .enabled
        case .requiresApproval: return .requiresApproval
        case .notFound: return .notFound
        @unknown default: return .notFound
        }
    }
    public func register() throws { try service.register() }
    public func unregister() throws { try service.unregister() }
}

/// "Launch at login" for the app itself.
public struct LaunchAtLogin: Sendable {
    let service: ServiceRegistering
    public init(service: ServiceRegistering) { self.service = service }
    public var isEnabled: Bool { service.status == .enabled || service.status == .requiresApproval }
    public var needsApproval: Bool { service.status == .requiresApproval }
    public func setEnabled(_ on: Bool) throws {
        switch (on, service.status) {
        case (true, .enabled), (true, .requiresApproval): return
        case (true, _): try service.register()
        case (false, .notRegistered), (false, .notFound): return
        case (false, _): try service.unregister()
        }
    }
}

// MARK: - Discovery

public struct BackendDiscovery: Sendable {
    public enum Outcome: Equatable, Sendable {
        /// A live backend that is the same version or newer than the bundled one.
        case reuse(LockFile, VersionInfo)
        /// No live lock: start the bundled backend.
        case start
        /// A live but older backend: ask the user before replacing it.
        case offerTakeover(running: LockFile, runningVersion: String, bundledVersion: String)
        /// The lock's pid is alive but nothing answers `/api/version` as loopx.
        case unresponsive(LockFile)
    }

    public typealias VersionFetcher = @Sendable (LockFile) async -> VersionInfo?

    public let lockURL: URL
    public let bundledVersion: String
    let pidAlive: @Sendable (Int32) -> Bool
    let fetchVersion: VersionFetcher

    public init(lockURL: URL = LockFile.defaultURL(), bundledVersion: String,
                pidAlive: @escaping @Sendable (Int32) -> Bool = { LockFile.isProcessAlive($0) },
                fetchVersion: @escaping VersionFetcher = BackendDiscovery.fetchVersionOverHTTP) {
        self.lockURL = lockURL; self.bundledVersion = bundledVersion
        self.pidAlive = pidAlive; self.fetchVersion = fetchVersion
    }

    public func discover() async -> Outcome {
        guard let lock = LockFile.read(at: lockURL), !lock.isStale(pidAlive: pidAlive) else { return .start }
        guard let info = await fetchVersion(lock), info.name == "loopx" else { return .unresponsive(lock) }
        if let running = SemVer(info.version), let bundled = SemVer(bundledVersion), running < bundled {
            return .offerTakeover(running: lock, runningVersion: info.version, bundledVersion: bundledVersion)
        }
        return .reuse(lock, info)
    }

    /// The URL the SSE client should connect to, or nil when no live backend exists. Cheap: no HTTP.
    public func liveEventsURL() -> URL? {
        LockFile.readLive(at: lockURL, pidAlive: pidAlive)?.eventsURL
    }

    public static let fetchVersionOverHTTP: VersionFetcher = { lock in
        var req = URLRequest(url: lock.baseURL.appendingPathComponent("api/version"))
        req.timeoutInterval = 2
        guard let (data, resp) = try? await URLSession.shared.data(for: req),
              (resp as? HTTPURLResponse)?.statusCode == 200 else { return nil }
        return try? JSONDecoder().decode(VersionInfo.self, from: data)
    }
}

// MARK: - Starting

public enum BackendError: Error, Equatable, Sendable {
    case binaryNotFound(String)
    case needsApproval                 // user must allow the background item in System Settings
    case launchFailed(String)
    case timedOut
}

public protocol BackendStarter: Sendable {
    func start() async throws
}

/// Registers the bundled LaunchAgent (Contents/Library/LaunchAgents/<plist>) with `SMAppService`,
/// so the backend outlives the app and launchd supervises it.
public struct SMAppServiceStarter: BackendStarter {
    let service: ServiceRegistering
    public init(service: ServiceRegistering) { self.service = service }

    public func start() async throws {
        switch service.status {
        case .requiresApproval:
            throw BackendError.needsApproval
        case .enabled:
            // Registered but not serving (crashed, or lock removed): re-register to relaunch the job.
            try service.unregister()
            try register()
        default:
            try register()
        }
    }

    private func register() throws {
        do { try service.register() } catch { throw BackendError.launchFailed(error.localizedDescription) }
        if service.status == .requiresApproval { throw BackendError.needsApproval }
    }
}

/// Spawns `loopx serve` directly. Used for unbundled runs (`swift run`, development) and tests.
public struct ProcessStarter: BackendStarter {
    public let binary: URL
    public let arguments: [String]
    public let environment: [String: String]?
    public let logURL: URL?

    public init(binary: URL, arguments: [String] = ["serve"], environment: [String: String]? = nil, logURL: URL? = nil) {
        self.binary = binary; self.arguments = arguments; self.environment = environment; self.logURL = logURL
    }

    public func start() async throws {
        guard FileManager.default.isExecutableFile(atPath: binary.path) else {
            throw BackendError.binaryNotFound(binary.path)
        }
        let p = Process()
        p.executableURL = binary
        p.arguments = arguments
        if let environment { p.environment = environment }
        p.standardInput = FileHandle.nullDevice
        if let logURL, let h = Self.openLog(logURL) { p.standardOutput = h; p.standardError = h }
        else { p.standardOutput = FileHandle.nullDevice; p.standardError = FileHandle.nullDevice }
        do { try p.run() } catch { throw BackendError.launchFailed(error.localizedDescription) }
    }

    private static func openLog(_ url: URL) -> FileHandle? {
        try? FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        if !FileManager.default.fileExists(atPath: url.path) { FileManager.default.createFile(atPath: url.path, contents: nil) }
        let h = try? FileHandle(forWritingTo: url)
        _ = try? h?.seekToEnd()
        return h
    }
}

// MARK: - Supervisor

public enum SupervisorResult: Equatable, Sendable {
    case ready(LockFile, startedBundled: Bool)
    /// An older backend is running; call `takeOver()` if the user agrees.
    case takeoverOffered(running: LockFile, runningVersion: String, bundledVersion: String)
    case failed(String)
}

public final class Supervisor: @unchecked Sendable {
    public typealias Terminator = @Sendable (Int32) async -> Bool
    public typealias Sleeper = @Sendable (TimeInterval) async -> Void

    let discovery: BackendDiscovery
    let starter: BackendStarter
    /// Tried once when `starter` fails or its backend never comes up, e.g. launchd refuses to
    /// run an ad-hoc signed bundled agent. The backend it spawns does not outlive the app.
    let fallback: BackendStarter?
    let primaryTimeout: TimeInterval
    let terminator: Terminator
    let sleeper: Sleeper
    let startTimeout: TimeInterval
    let pollInterval: TimeInterval

    public init(discovery: BackendDiscovery, starter: BackendStarter, fallback: BackendStarter? = nil,
                primaryTimeout: TimeInterval = 6,
                terminator: @escaping Terminator = Supervisor.terminateProcess,
                startTimeout: TimeInterval = 15, pollInterval: TimeInterval = 0.2,
                sleeper: @escaping Sleeper = { try? await Task.sleep(nanoseconds: UInt64($0 * 1_000_000_000)) }) {
        self.discovery = discovery; self.starter = starter; self.fallback = fallback
        self.primaryTimeout = primaryTimeout; self.terminator = terminator
        self.startTimeout = startTimeout; self.pollInterval = pollInterval; self.sleeper = sleeper
    }

    /// Reuse a live backend, else start the bundled one. Never kills anything by itself.
    public func ensureBackend() async -> SupervisorResult {
        switch await discovery.discover() {
        case .reuse(let lock, _): return .ready(lock, startedBundled: false)
        case .offerTakeover(let l, let rv, let bv): return .takeoverOffered(running: l, runningVersion: rv, bundledVersion: bv)
        case .unresponsive(let l): return .failed("A process (pid \(l.pid)) holds loopx.lock on port \(l.port) but does not answer as loopx.")
        case .start: return await startAndWait()
        }
    }

    /// Terminates the older backend (SIGTERM, then SIGKILL) and starts the bundled one.
    public func takeOver(running: LockFile) async -> SupervisorResult {
        guard await terminator(running.pid) else { return .failed("Could not stop the running backend (pid \(running.pid)).") }
        return await startAndWait()
    }

    private func startAndWait() async -> SupervisorResult {
        var failure: String
        do {
            try await starter.start()
            if let lock = await waitForBackend(timeout: fallback == nil ? startTimeout : min(startTimeout, primaryTimeout)) {
                return .ready(lock, startedBundled: true)
            }
            failure = "The backend did not come up within \(Int(startTimeout)) seconds."
        } catch let e as BackendError {
            if e == .needsApproval { return .failed(Self.message(e)) }
            failure = Self.message(e)
        } catch { failure = "Could not start the backend: \(error.localizedDescription)" }

        guard let fallback else { return .failed(failure) }
        do { try await fallback.start() }
        catch let e as BackendError { return .failed("\(failure) Fallback: \(Self.message(e))") }
        catch { return .failed("\(failure) Fallback: \(error.localizedDescription)") }
        if let lock = await waitForBackend(timeout: startTimeout) { return .ready(lock, startedBundled: true) }
        return .failed(failure)
    }

    private func waitForBackend(timeout: TimeInterval) async -> LockFile? {
        var waited: TimeInterval = 0
        while waited <= timeout {
            if case .reuse(let lock, _) = await discovery.discover() { return lock }
            await sleeper(pollInterval)
            waited += pollInterval
        }
        return nil
    }

    private static func message(_ e: BackendError) -> String {
        switch e {
        case .needsApproval: return "Allow LoopX in System Settings > General > Login Items, then try again."
        case .binaryNotFound(let p): return "The loopx backend was not found at \(p)."
        case .launchFailed(let m): return "Could not start the backend: \(m)"
        case .timedOut: return "Timed out starting the backend."
        }
    }

    public static let terminateProcess: Terminator = { pid in
        guard pid > 1, LockFile.isProcessAlive(pid) else { return true }
        kill(pid, SIGTERM)
        for _ in 0..<50 {
            if !LockFile.isProcessAlive(pid) { return true }
            try? await Task.sleep(nanoseconds: 100_000_000)
        }
        kill(pid, SIGKILL)
        try? await Task.sleep(nanoseconds: 200_000_000)
        return !LockFile.isProcessAlive(pid)
    }
}
