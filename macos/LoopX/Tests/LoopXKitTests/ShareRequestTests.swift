import Foundation
import Testing
@testable import LoopXKit

@Suite struct ShareRequestTests {
    private func root() throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("share-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir.appendingPathComponent("final/demo"), withIntermediateDirectories: true)
        return dir
    }

    private func file(_ root: URL, _ name: String) throws -> URL {
        let url = root.appendingPathComponent("final/demo/\(name)")
        try Data([1, 2, 3]).write(to: url)
        return url
    }

    @Test func acceptsAVideoUnderTheRootWithItsText() throws {
        let root = try root()
        let mp4 = try file(root, "demo.mp4")
        let req = ShareRequest.parse(["path": mp4.path, "text": "Demo short"], root: root)
        #expect(req?.fileURL?.lastPathComponent == "demo.mp4")
        #expect(req?.text == "Demo short")
        #expect(req?.items.count == 2)
    }

    @Test func acceptsCaptionTextAloneWithoutAFile() throws {
        let req = ShareRequest.parse(["path": "", "text": "Title\n\nCaption #tag"], root: try root())
        #expect(req?.fileURL == nil)
        #expect(req?.text == "Title\n\nCaption #tag")
    }

    @Test func refusesFilesOutsideTheRoot() throws {
        let root = try root()
        let other = FileManager.default.temporaryDirectory.appendingPathComponent("elsewhere-\(UUID().uuidString).mp4")
        try Data([1]).write(to: other)
        #expect(ShareRequest.parse(["path": other.path, "text": "x"], root: root) == nil)
        #expect(ShareRequest.parse(["path": "/etc/hosts", "text": "x"], root: root) == nil)
        // `..` cannot climb out.
        let sneaky = root.path + "/final/../../" + other.lastPathComponent
        #expect(ShareRequest.parse(["path": sneaky, "text": "x"], root: root) == nil)
    }

    @Test func refusesASymlinkThatLeavesTheRoot() throws {
        let root = try root()
        let target = FileManager.default.temporaryDirectory.appendingPathComponent("secret-\(UUID().uuidString).mp4")
        try Data([1]).write(to: target)
        let link = root.appendingPathComponent("final/demo/link.mp4")
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: target)
        #expect(ShareRequest.parse(["path": link.path, "text": "x"], root: root) == nil)
    }

    @Test func refusesOtherFileTypesAndMissingFiles() throws {
        let root = try root()
        let script = try file(root, "run.sh")
        #expect(ShareRequest.parse(["path": script.path, "text": "x"], root: root) == nil)
        let gone = root.appendingPathComponent("final/demo/gone.mp4")
        #expect(ShareRequest.parse(["path": gone.path, "text": "x"], root: root) == nil)
    }

    @Test func refusesAMessageWithNothingToShareOrTheWrongShape() throws {
        let root = try root()
        #expect(ShareRequest.parse(["path": "", "text": "   "], root: root) == nil)
        #expect(ShareRequest.parse("not a dictionary", root: root) == nil)
        #expect(ShareRequest.parse(["path": 5, "text": "x"], root: root) == nil)
    }

    @Test func longTextIsCut() throws {
        let long = String(repeating: "a", count: ShareRequest.maxTextLength + 500)
        let req = ShareRequest.parse(["path": "", "text": long], root: try root())
        #expect(req?.text?.count == ShareRequest.maxTextLength)
    }

    @Test func carriesTheClickedButtonsRectangleAsTheAnchor() throws {
        let req = ShareRequest.parse(
            ["path": "", "text": "x", "x": 120.5, "y": 48, "width": 96, "height": 28], root: try root())
        #expect(req?.anchor == CGRect(x: 120.5, y: 48, width: 96, height: 28))
    }

    @Test func noAnchorWhenThePageSendsNoneOrNonsense() throws {
        let root = try root()
        #expect(ShareRequest.parse(["path": "", "text": "x"], root: root)?.anchor == nil)
        #expect(ShareRequest.parse(["path": "", "text": "x", "x": "left", "y": 1, "width": 1, "height": 1], root: root)?.anchor == nil)
        #expect(ShareRequest.parse(["path": "", "text": "x", "x": 1, "y": 1, "width": -5, "height": 1], root: root)?.anchor == nil)
        #expect(ShareRequest.parse(["path": "", "text": "x", "x": 1e12, "y": 1, "width": 1, "height": 1], root: root)?.anchor == nil)
        #expect(ShareRequest.parse(["path": "", "text": "x", "x": Double.nan, "y": 1, "width": 1, "height": 1], root: root)?.anchor == nil)
    }
}
