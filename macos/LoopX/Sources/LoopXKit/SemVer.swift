import Foundation

/// Minimal `X.Y.Z[-pre]` version, enough to decide whether the bundled backend is newer.
public struct SemVer: Comparable, Equatable, Sendable, CustomStringConvertible {
    public var major: Int, minor: Int, patch: Int
    public var prerelease: String?

    public init(_ major: Int, _ minor: Int, _ patch: Int, prerelease: String? = nil) {
        self.major = major; self.minor = minor; self.patch = patch; self.prerelease = prerelease
    }

    public init?(_ string: String) {
        var s = string.trimmingCharacters(in: .whitespaces)
        if s.hasPrefix("v") { s.removeFirst() }
        if let plus = s.firstIndex(of: "+") { s = String(s[..<plus]) }
        var pre: String?
        if let dash = s.firstIndex(of: "-") {
            pre = String(s[s.index(after: dash)...]); s = String(s[..<dash])
        }
        let parts = s.split(separator: ".", omittingEmptySubsequences: false).map { Int($0) }
        guard parts.count == 3, let a = parts[0], let b = parts[1], let c = parts[2] else { return nil }
        self.init(a, b, c, prerelease: pre)
    }

    public var description: String {
        "\(major).\(minor).\(patch)" + (prerelease.map { "-\($0)" } ?? "")
    }

    public static func < (l: SemVer, r: SemVer) -> Bool {
        if (l.major, l.minor, l.patch) != (r.major, r.minor, r.patch) {
            return (l.major, l.minor, l.patch) < (r.major, r.minor, r.patch)
        }
        switch (l.prerelease, r.prerelease) {
        case (nil, _): return false          // release > prerelease
        case (_?, nil): return true
        case let (a?, b?): return a.compare(b, options: .numeric) == .orderedAscending
        }
    }
}
