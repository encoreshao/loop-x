import Foundation
import LoopXKit

/// The app-driven scheduler: while "Run scheduled loops" is on, `loopx scheduler` runs on launch and
/// every 15 minutes.
@MainActor
final class SchedulerRunner {
    private let binary: URL?
    private let environment: [String: String]
    private lazy var poller = SchedulerPoller(poll: { [binary, environment, shouldPoll] in
        guard let binary, shouldPoll() else { return }
        await Self.runOnce(binary, environment)
    })
    private let shouldPoll: @Sendable () -> Bool

    /// `shouldPoll` is asked before every poll, so a battery or Low Power Mode change takes effect on the next one.
    init(binary: URL?, environment: [String: String] = ProcessInfo.processInfo.environment,
         shouldPoll: @escaping @Sendable () -> Bool = { true }) {
        self.binary = binary; self.environment = environment; self.shouldPoll = shouldPoll
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
