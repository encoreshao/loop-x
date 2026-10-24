import Foundation

/// `loopx.lock`: how a client finds the running backend (see docs/contract/mac-app-api.md).
public struct LockFile: Codable, Equatable, Sendable {
    public var pid: Int32
    public var port: Int
    public var version: String
    public var startedAt: String
    public var token: String

    public init(pid: Int32, port: Int, version: String, startedAt: String = "", token: String) {
        self.pid = pid; self.port = port; self.version = version
        self.startedAt = startedAt; self.token = token
    }

    enum CodingKeys: String, CodingKey {
        case pid, port, version, token
        case startedAt = "started_at"
    }

    public var baseURL: URL { URL(string: "http://127.0.0.1:\(port)")! }
    public var eventsURL: URL { baseURL.appendingPathComponent("api/events") }

    /// The lock is stale when its owner process is gone.
    public func isStale(pidAlive: (Int32) -> Bool = LockFile.isProcessAlive) -> Bool {
        !pidAlive(pid)
    }

    public static func parse(_ data: Data) -> LockFile? {
        guard let lock = try? JSONDecoder().decode(LockFile.self, from: data),
              lock.pid > 0, (1...65535).contains(lock.port) else { return nil }
        return lock
    }

    /// Reads and parses; nil when the file is missing or malformed.
    public static func read(at url: URL) -> LockFile? {
        guard let data = try? Data(contentsOf: url) else { return nil }
        return parse(data)
    }

    /// Reads the lock and returns it only when its pid is alive.
    public static func readLive(at url: URL, pidAlive: (Int32) -> Bool = LockFile.isProcessAlive) -> LockFile? {
        guard let lock = read(at: url), !lock.isStale(pidAlive: pidAlive) else { return nil }
        return lock
    }

    public static func isProcessAlive(_ pid: Int32) -> Bool {
        guard pid > 0 else { return false }
        if kill(pid, 0) == 0 { return true }
        return errno == EPERM  // exists, owned by someone else
    }

    /// `$LOOP_ENGINEERING_HOME/loopx.lock`, defaulting to `~/.loopx/loopx.lock`.
    public static func defaultURL(environment: [String: String] = ProcessInfo.processInfo.environment) -> URL {
        let base: URL
        if let home = environment["LOOP_ENGINEERING_HOME"], !home.isEmpty {
            base = URL(fileURLWithPath: home, isDirectory: true)
        } else {
            let h = environment["HOME"].flatMap { $0.isEmpty ? nil : $0 } ?? NSHomeDirectory()
            base = URL(fileURLWithPath: h, isDirectory: true).appendingPathComponent(".loopx", isDirectory: true)
        }
        return base.appendingPathComponent("loopx.lock")
    }
}
