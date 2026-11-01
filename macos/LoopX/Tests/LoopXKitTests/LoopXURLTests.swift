import Foundation
import Testing
@testable import LoopXKit

@Suite struct LoopXURLTests {
    func parse(_ s: String) -> LoopXURL? { LoopXURL.parse(URL(string: s)!) }

    @Test func runNamesAKnownLoop() {
        #expect(parse("loopx://run/issue-loop") == .run(loop: "issue-loop"))
        #expect(parse("loopx://run/topic-loop") == .run(loop: "topic-loop"))
        #expect(parse("loopx://run/inbox-triage-loop") == .run(loop: "inbox-triage-loop"))
    }

    @Test func runRejectsAnythingThatIsNotAStartableLoop() {
        #expect(parse("loopx://run/rm-rf") == nil)
        #expect(parse("loopx://run/") == nil)
        #expect(parse("loopx://run") == nil)
        #expect(parse("loopx://run/issue-loop/extra") == nil)
        #expect(parse("loopx://run/../issue-loop") == nil)
    }

    @Test func openWithoutAPageGoesHome() {
        #expect(parse("loopx://open") == .open(path: "/"))
        #expect(parse("loopx://open/") == .open(path: "/"))
    }

    @Test func openAcceptsEveryDashboardMenuPageIncludingItsView() {
        #expect(parse("loopx://open/runs") == .open(path: "/runs"))
        #expect(parse("loopx://open/runs?view=logs") == .open(path: "/runs?view=logs"))
        #expect(parse("loopx://open/loops/issue-loop") == .open(path: "/loops/issue-loop"))
        for e in DashboardMenu.sections.flatMap(\.entries) {
            #expect(parse("loopx://open" + e.path) == .open(path: e.path), "\(e.path)")
        }
    }

    @Test func openRejectsPagesTheMenuDoesNotOffer() {
        #expect(parse("loopx://open/api/approvals/x") == nil)
        #expect(parse("loopx://open/runs?view=evil") == nil)
        #expect(parse("loopx://open//evil.example.com") == nil)
    }

    @Test func otherSchemesAndActionsAreNotOurs() {
        #expect(parse("https://run/issue-loop") == nil)
        #expect(parse("loopx://delete/issue-loop") == nil)
        #expect(parse("LOOPX://run/issue-loop") == .run(loop: "issue-loop"))
    }
}
