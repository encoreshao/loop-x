import Foundation
import LoopXKit

/// The app-driven scheduler: while "Run scheduled loops" is on, `loopx scheduler` runs on launch and
/// every 15 minutes.
@MainActor
final class SchedulerRunner {
    private let binary: URL?
    private let environment: [String: String]
    private lazy var poller = SchedulerPoller(poll: { [binary, environment] in
        guard let binary else { return }
        await Self.runOnce(binary, environment)
    })

    init(binary: URL?, environment: [String: String] = ProcessInfo.processInfo.environment) {
        self.binary = binary; self.environment = environment
    }

    func setEnabled(_ on: Bool) { on ? poller.start() : poller.stop() }

    nonisolated static func runOnce(_ binary: URL, _ environment: [String: String]) async {
        await withCheckedContinuation { (cont: CheckedContinuation<Void, Never>) in
            let p = Process()
            p.executableURL = binary
            p.arguments = ["scheduler"]
            var env = environment
            env["PATH"] = "/opt/homebrew/bin:/usr/local/bin:\(NSHomeDirectory())/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
            p.environment = env
            p.standardInput = FileHandle.nullDevice
            p.standardOutput = FileHandle.nullDevice; p.standardError = FileHandle.nullDevice
            p.terminationHandler = { _ in cont.resume() }
            do { try p.run() } catch { cont.resume() }
        }
    }
}
