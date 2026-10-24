import Foundation
import LoopXKit

/// How this process finds and starts `loopx`: from inside LoopX.app (SMAppService agent) or, when
/// run unbundled (`swift run`), by spawning a binary found via LOOPX_BINARY / the repo build / PATH.
struct BackendConfig {
    static let agentPlistName = "com.hermes.loopx.backend.plist"

    var discovery: BackendDiscovery
    var starter: BackendStarter
    var fallback: BackendStarter?
    /// The `loopx` binary the scheduler poll runs: the bundled helper, or the one found for dev runs.
    var loopxBinary: URL?

    func makeSupervisor() -> Supervisor { Supervisor(discovery: discovery, starter: starter, fallback: fallback) }

    static func detect(environment: [String: String] = ProcessInfo.processInfo.environment,
                       bundle: Bundle = .main) -> BackendConfig {
        let lockURL = LockFile.defaultURL(environment: environment)
        let agent = bundle.bundleURL.appendingPathComponent("Contents/Library/LaunchAgents/\(agentPlistName)")
        let helper = bundle.bundleURL.appendingPathComponent("Contents/Helpers/loopx")
        if FileManager.default.fileExists(atPath: agent.path), FileManager.default.isExecutableFile(atPath: helper.path) {
            let version = (bundle.object(forInfoDictionaryKey: "LoopXBundledVersion") as? String)
                ?? binaryVersion(helper) ?? "0.0.0"
            let log = lockURL.deletingLastPathComponent().appendingPathComponent("outputs/loopx-serve.log")
            var env = environment
            env["PATH"] = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
            return BackendConfig(discovery: BackendDiscovery(lockURL: lockURL, bundledVersion: version),
                                 starter: SMAppServiceStarter(service: SMAppServiceAdapter(.agent(plistName: agentPlistName))),
                                 fallback: ProcessStarter(binary: helper, arguments: ["serve"], environment: env, logURL: log),
                                 loopxBinary: helper)
        }
        let binary = findBinary(environment: environment)
        let version = binary.flatMap(binaryVersion) ?? "0.0.0"
        let log = lockURL.deletingLastPathComponent().appendingPathComponent("outputs/loopx-serve.log")
        return BackendConfig(
            discovery: BackendDiscovery(lockURL: lockURL, bundledVersion: version),
            starter: ProcessStarter(binary: binary ?? URL(fileURLWithPath: "/nonexistent/loopx"),
                                    arguments: serveArguments(environment), environment: environment, logURL: log),
            loopxBinary: binary)
    }

    /// `LOOPX_SERVE_ARGS` overrides the arguments of the spawned backend (unbundled runs only), e.g.
    /// `"--port 0 --no-legacy"` for a sandbox without the Python dashboard.
    static func serveArguments(_ environment: [String: String]) -> [String] {
        let extra = (environment["LOOPX_SERVE_ARGS"] ?? "").split(whereSeparator: \.isWhitespace).map(String.init)
        return ["serve"] + extra
    }

    static func findBinary(environment: [String: String]) -> URL? {
        var candidates: [String] = []
        if let p = environment["LOOPX_BINARY"], !p.isEmpty { candidates.append(p) }
        let cwd = FileManager.default.currentDirectoryPath
        candidates += ["\(cwd)/target/debug/loopx", "\(cwd)/target/release/loopx", "\(cwd)/../../target/debug/loopx"]
        candidates += (environment["PATH"] ?? "").split(separator: ":").map { "\($0)/loopx" }
        return candidates.first { FileManager.default.isExecutableFile(atPath: $0) }
            .map { URL(fileURLWithPath: $0).standardizedFileURL }
    }

    /// `loopx --version` prints `loopx X.Y.Z`.
    static func binaryVersion(_ binary: URL) -> String? {
        let p = Process(); p.executableURL = binary; p.arguments = ["--version"]
        let pipe = Pipe(); p.standardOutput = pipe; p.standardError = FileHandle.nullDevice
        guard (try? p.run()) != nil else { return nil }
        p.waitUntilExit()
        let out = String(decoding: pipe.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
        return out.split(whereSeparator: \.isWhitespace).compactMap { SemVer(String($0)) }.first?.description
    }
}
