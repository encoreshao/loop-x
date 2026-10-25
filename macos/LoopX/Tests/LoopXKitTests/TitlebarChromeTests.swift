import Testing
@testable import LoopXKit

@Suite struct TitlebarChromeTests {
    @Test func marksTheDocumentAsHostedByTheApp() {
        #expect(TitlebarChrome.script.contains("classList.add('loopx-app')"))
    }

    @Test func clearsTheTrafficLightsInTheSidebarHeader() {
        #expect(TitlebarChrome.css.contains(".loopx-app .sidebar-top"))
        #expect(TitlebarChrome.css.contains("padding-top"))
    }

    @Test func emptyTopbarAndSidebarHeaderDragTheWindow() {
        let s = TitlebarChrome.script
        #expect(s.contains(".topbar") && s.contains(".sidebar-top"))
        #expect(s.contains("webkit.messageHandlers.\(TitlebarChrome.messageName)"))
        #expect(s.contains("dblclick"))
    }

    @Test func controlsInTheBarsKeepTheirClicks() {
        for tag in ["a", "button", "input", "select", "textarea", "[role=button]"] {
            #expect(TitlebarChrome.interactiveSelector.contains(tag))
        }
        #expect(TitlebarChrome.script.contains(TitlebarChrome.interactiveSelector))
    }

    @Test func theStartScreenHasADragStripAtTheTop() {
        let h = StartScreen.html("Starting…", failed: false, logo: "")
        #expect(h.contains("class='\(TitlebarChrome.dragStripClass)'"))
        #expect(TitlebarChrome.script.contains(".\(TitlebarChrome.dragStripClass)"))
    }
}
