import Foundation

/// Puts the bundled `loopx` on the user's `PATH` by symlinking it into `/usr/local/bin`, so nobody has to
/// run `ln -s` by hand. Never replaces a `loopx` that did not come from a LoopX.app (Homebrew, the script installer).
public struct CLIInstaller: Equatable, Sendable {
    public enum Status: Equatable, Sendable {
        case notInstalled
        case installed
        /// Linked to another LoopX.app copy (moved or replaced app); safe to relink.
        case outdated
        /// Something else owns the path; carries what it points at (or the path itself for a regular file).
        case conflict(String)

        public var summary: String {
            switch self {
            case .notInstalled: return "Not installed"
            case .installed: return "Installed"
            case .outdated: return "Linked to another copy of LoopX"
            case .conflict(let p): return "Another loopx is in the way: \(p)"
            }
        }
    }

    public static let defaultLinkPath = "/usr/local/bin/loopx"

    public let binary: String
    public let linkPath: String

    public init(binary: String, linkPath: String = CLIInstaller.defaultLinkPath) {
        self.binary = binary; self.linkPath = linkPath
    }

    public func status() -> Status {
        let fm = FileManager.default
        if let dest = try? fm.destinationOfSymbolicLink(atPath: linkPath) {
            if dest == binary { return .installed }
            return dest.hasSuffix("LoopX.app/Contents/Helpers/loopx") ? .outdated : .conflict(dest)
        }
        return fm.fileExists(atPath: linkPath) ? .conflict(linkPath) : .notInstalled
    }

    /// The shell command that creates the link, with every path single-quoted.
    public var shellCommand: String {
        let dir = (linkPath as NSString).deletingLastPathComponent
        return "mkdir -p \(Self.quote(dir)) && ln -sfn \(Self.quote(binary)) \(Self.quote(linkPath))"
    }

    /// Runs `shellCommand` through `run` (the app passes an administrator-privileges runner) and returns the new status.
    public func install(run: (String) -> Bool) -> Status {
        let before = status()
        if case .conflict = before { return before }
        if before == .installed { return before }
        _ = run(shellCommand)
        return status()
    }

    static func quote(_ s: String) -> String { "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'" }
}
