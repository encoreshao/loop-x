import Foundation
import Testing
@testable import LoopXKit

@Suite struct DashboardMenuTests {
    private var all: [DashboardMenu.Entry] { DashboardMenu.sections.flatMap(\.entries) }

    @Test func sectionsFollowTheSidebarGroups() {
        #expect(DashboardMenu.sections.map(\.title) == ["Loops", "Observe", "System"])
    }

    @Test func everySidebarPageHasAnEntry() {
        let paths = Set(all.map(\.path))
        for p in ["/loops", "/runs", "/insights", "/harness", "/connectors", "/settings"] { #expect(paths.contains(p), "missing \(p)") }
        #expect(DashboardMenu.home.path == "/")
    }

    @Test func pathsAreLocalAndTitlesAreUnique() {
        #expect(all.allSatisfy { $0.path.hasPrefix("/") && !$0.path.contains("://") })
        #expect(Set(all.map(\.title)).count == all.count)
    }

    @Test func viewsDeepLinkIntoTheirPage() {
        let byTitle = Dictionary(uniqueKeysWithValues: all.map { ($0.title, $0.path) })
        #expect(byTitle["Logs"] == "/runs?view=logs")
        #expect(byTitle["Daemons"] == "/settings?view=daemons")
        #expect(byTitle["Cost"] == "/insights?view=cost")
        #expect(byTitle["GitLab Issues"] == "/loops/gitlab-loop")
    }

    @Test func repositoryLinksPointAtTheProject() {
        #expect(DashboardMenu.links.first?.url.absoluteString == "https://github.com/encoreshao/loop-x")
        #expect(DashboardMenu.links.allSatisfy { $0.url.scheme == "https" && $0.url.host == "github.com" })
    }

    @Test func urlJoinsTheBackendBase() {
        let base = URL(string: "http://127.0.0.1:8420")!
        #expect(DashboardMenu.url(for: "/runs?view=logs", base: base)?.absoluteString == "http://127.0.0.1:8420/runs?view=logs")
        #expect(DashboardMenu.url(for: "/", base: base)?.absoluteString == "http://127.0.0.1:8420/")
    }
}
