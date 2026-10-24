import Foundation
import Testing
@testable import LoopXKit

@Suite struct CLIInstallerTests {
    private func scratch() throws -> URL {
        let d = FileManager.default.temporaryDirectory.appendingPathComponent("cli-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
        return d
    }

    @Test func notInstalledWhenNothingIsAtTheLinkPath() throws {
        let d = try scratch()
        let i = CLIInstaller(binary: "/Applications/LoopX.app/Contents/Helpers/loopx", linkPath: d.appendingPathComponent("loopx").path)
        #expect(i.status() == .notInstalled)
    }

    @Test func installedWhenTheLinkPointsAtThisBinary() throws {
        let d = try scratch()
        let link = d.appendingPathComponent("loopx").path
        try FileManager.default.createSymbolicLink(atPath: link, withDestinationPath: "/Applications/LoopX.app/Contents/Helpers/loopx")
        let i = CLIInstaller(binary: "/Applications/LoopX.app/Contents/Helpers/loopx", linkPath: link)
        #expect(i.status() == .installed)
    }

    @Test func staleLinkIntoAnotherAppCopyNeedsRelinking() throws {
        let d = try scratch()
        let link = d.appendingPathComponent("loopx").path
        try FileManager.default.createSymbolicLink(atPath: link, withDestinationPath: "/Volumes/LoopX/LoopX.app/Contents/Helpers/loopx")
        let i = CLIInstaller(binary: "/Applications/LoopX.app/Contents/Helpers/loopx", linkPath: link)
        #expect(i.status() == .outdated)
    }

    @Test func aForeignLoopxIsNeverOverwritten() throws {
        let d = try scratch()
        let link = d.appendingPathComponent("loopx").path
        try FileManager.default.createSymbolicLink(atPath: link, withDestinationPath: "/opt/homebrew/bin/loopx")
        let i = CLIInstaller(binary: "/Applications/LoopX.app/Contents/Helpers/loopx", linkPath: link)
        #expect(i.status() == .conflict("/opt/homebrew/bin/loopx"))
        #expect(i.install(run: { _ in true }) == .conflict("/opt/homebrew/bin/loopx"))
    }

    @Test func installRunsOneQuotedLinkCommandAndReportsTheNewStatus() throws {
        let d = try scratch()
        let link = d.appendingPathComponent("my bin/loopx").path
        let binary = "/Applications/Loop X.app/Contents/Helpers/loopx"
        let i = CLIInstaller(binary: binary, linkPath: link)
        var ran: [String] = []
        let result = i.install(run: { cmd in
            ran.append(cmd)
            try? FileManager.default.createDirectory(atPath: (link as NSString).deletingLastPathComponent, withIntermediateDirectories: true)
            try? FileManager.default.createSymbolicLink(atPath: link, withDestinationPath: binary)
            return true
        })
        #expect(result == .installed)
        #expect(ran.count == 1)
        #expect(ran[0].contains("'\(binary)'") && ran[0].contains("ln -sfn"))
    }

    @Test func failedInstallLeavesTheStatusUnchanged() throws {
        let d = try scratch()
        let i = CLIInstaller(binary: "/Applications/LoopX.app/Contents/Helpers/loopx", linkPath: d.appendingPathComponent("loopx").path)
        #expect(i.install(run: { _ in false }) == .notInstalled)
    }

    @Test func promptIsOfferedOnlyOnce() {
        let s = SettingsStore(defaults: UserDefaults(suiteName: "cli-\(UUID().uuidString)")!)
        #expect(s.cliPromptShown == false)
        s.cliPromptShown = true
        #expect(s.cliPromptShown == true)
    }
}
