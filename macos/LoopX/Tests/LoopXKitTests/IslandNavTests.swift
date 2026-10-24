import Testing
@testable import LoopXKit

@Suite struct IslandNavTests {
    @Test func startsOnHomeWithoutBack() {
        let n = IslandNav()
        #expect(n.screen == .home); #expect(!n.canGoBack)
    }

    @Test func openPushesDetailAndBackPops() {
        var n = IslandNav()
        n.open("loop:a")
        #expect(n.screen == .detail(taskID: "loop:a")); #expect(n.canGoBack)
        n.back(); #expect(n.screen == .home)
        n.back(); #expect(n.screen == .home)   // never pops the root
    }

    @Test func pointerLeaveFoldsOnlyWhenNotPinned() {
        var n = IslandNav()
        #expect(n.shouldFoldOnPointerLeave())
        n.pin()
        #expect(!n.shouldFoldOnPointerLeave())
    }

    @Test func unpinResetsToHomeAndClearsDraft() {
        var n = IslandNav()
        n.pin(); n.open("loop:a"); n.inputText = "hi"
        n.unpin()
        #expect(n.screen == .home); #expect(n.inputText.isEmpty); #expect(!n.isPinned)
    }

    @Test func foldBlockedWhileTyping() {
        var n = IslandNav()
        n.pin(); n.inputText = "  "
        #expect(!n.blocksFold)
        n.inputText = "run it"
        #expect(n.blocksFold)
        n.inputText = ""; n.isReplying = true
        #expect(n.blocksFold)
    }
}
