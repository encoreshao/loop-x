import CoreGraphics
import Foundation
import Testing
@testable import LoopXKit

/// Numbers modelled on a 14" MacBook Pro: 1512x982 points, 32pt notch strip, ~185pt wide notch.
let notchScreen = ScreenInfo(
    id: 1,
    frame: CGRect(x: 0, y: 0, width: 1512, height: 982),
    visibleFrame: CGRect(x: 0, y: 0, width: 1512, height: 950),
    safeAreaTop: 32,
    auxiliaryTopLeft: CGRect(x: 0, y: 950, width: 663, height: 32),
    auxiliaryTopRight: CGRect(x: 849, y: 950, width: 663, height: 32))

/// A plain external display to the right of the built-in one (frame origin is global).
let plainScreen = ScreenInfo(
    id: 2,
    frame: CGRect(x: 1512, y: 100, width: 1920, height: 1080),
    visibleFrame: CGRect(x: 1512, y: 100, width: 1920, height: 1055))

@Suite struct NotchGeometryTests {
    let layout = NotchLayout()

    @Test func detectsNotchAndItsRect() throws {
        #expect(notchScreen.hasNotch)
        #expect(!plainScreen.hasNotch)
        let n = try #require(notchScreen.notchRect)
        #expect(n == CGRect(x: 663, y: 950, width: 186, height: 32))
        #expect(plainScreen.notchRect == nil)
        // A safe-area inset alone (no aux areas) is not a notch.
        #expect(!ScreenInfo(frame: .init(x: 0, y: 0, width: 100, height: 100), safeAreaTop: 24).hasNotch)
    }

    @Test func collapsedFrameHugsTheNotchAtTheVeryTop() {
        let f = NotchGeometry.notchFrame(screen: notchScreen, expanded: false, layout: layout)
        #expect(f.maxY == notchScreen.frame.maxY)      // flush with the top edge
        #expect(f.height == 32)                         // menu-bar strip height
        #expect(f.midX == 756)                          // centred on the notch (663 + 93)
        #expect(f.width == 186 + 2 * layout.wing)
    }

    @Test func expandedFrameGrowsDownwardAndStaysCentred() {
        let c = NotchGeometry.notchFrame(screen: notchScreen, expanded: false, layout: layout)
        let e = NotchGeometry.notchFrame(screen: notchScreen, expanded: true, layout: layout)
        #expect(e.maxY == c.maxY)
        #expect(e.midX == c.midX)
        #expect(e.width == layout.expandedWidth)
        #expect(e.height == 32 + layout.expandedContentHeight)
        #expect(e.minY < c.minY)
    }

    @Test func offsetScreensUseGlobalCoordinates() {
        // Notched built-in screen placed at x=-1512 (left of the main one).
        var s = notchScreen
        s.frame.origin.x = -1512
        s.visibleFrame.origin.x = -1512
        s.auxiliaryTopLeft!.origin.x -= 1512
        s.auxiliaryTopRight!.origin.x -= 1512
        let f = NotchGeometry.notchFrame(screen: s, expanded: true, layout: layout)
        #expect(f.midX == CGFloat(-756))
        #expect(s.frame.contains(CGPoint(x: f.midX, y: f.midY)))
    }

    @Test func panelNeverLeavesAnUnusuallyNarrowScreen() {
        var s = notchScreen
        s.frame = CGRect(x: 0, y: 0, width: 300, height: 400); s.visibleFrame = s.frame
        s.auxiliaryTopLeft = CGRect(x: 0, y: 368, width: 60, height: 32)
        s.auxiliaryTopRight = CGRect(x: 240, y: 368, width: 60, height: 32)
        let f = NotchGeometry.notchFrame(screen: s, expanded: true, layout: layout)
        #expect(f.minX >= s.frame.minX)
    }

    @Test func noNotchFallsBackToHUDCentredUnderTheMenuBar() {
        let f = NotchGeometry.notchFrame(screen: plainScreen, expanded: false, layout: layout)
        #expect(f.size == layout.hudCollapsed)
        #expect(f.midX == plainScreen.visibleFrame.midX)
        #expect(f.maxY == plainScreen.visibleFrame.maxY - layout.hudTopMargin)
    }

    @Test func placementByMode() {
        #expect(NotchGeometry.placement(mode: .notch, screen: notchScreen) == .notch(notchScreen))
        #expect(NotchGeometry.placement(mode: .notch, screen: plainScreen) == .hud(plainScreen))
        #expect(NotchGeometry.placement(mode: .hud, screen: notchScreen) == .hud(notchScreen))
        #expect(NotchGeometry.placement(mode: .off, screen: notchScreen) == .none)
        #expect(NotchGeometry.placement(mode: .notch, screen: nil) == .none)
    }

    @Test func alertsAppearOnlyOnTheScreenUnderThePointer() {
        let screens = [notchScreen, plainScreen]
        #expect(NotchGeometry.alertScreen(screens: screens, pointer: CGPoint(x: 100, y: 500))?.id == 1)
        #expect(NotchGeometry.alertScreen(screens: screens, pointer: CGPoint(x: 2000, y: 500))?.id == 2)
        // Pointer in a gap / off every screen: fall back to the first (main) screen.
        #expect(NotchGeometry.alertScreen(screens: screens, pointer: CGPoint(x: -5000, y: -5000))?.id == 1)
        #expect(NotchGeometry.alertScreen(screens: [], pointer: .zero) == nil)
        // Shared edge belongs to exactly one screen (CGRect.contains is half-open).
        #expect(NotchGeometry.alertScreen(screens: screens, pointer: CGPoint(x: 1512, y: 500))?.id == 2)
    }

    @Test func hudSnapsToEdgesWithinTheThresholdOnly() {
        let a = plainScreen.visibleFrame
        let size = layout.hudCollapsed
        // Close to the left and bottom edges: snaps to both with the margin.
        var r = NotchGeometry.snap(CGRect(origin: CGPoint(x: a.minX + 10, y: a.minY + 12), size: size), to: plainScreen, layout: layout)
        #expect(r.minX == a.minX + layout.edgeMargin)
        #expect(r.minY == a.minY + layout.edgeMargin)
        // Close to the right and top.
        r = NotchGeometry.snap(CGRect(x: a.maxX - size.width - 5, y: a.maxY - size.height - 20, width: size.width, height: size.height), to: plainScreen, layout: layout)
        #expect(r.maxX == a.maxX - layout.edgeMargin)
        #expect(r.maxY == a.maxY - layout.edgeMargin)
        // Mid-screen: untouched.
        let mid = CGRect(x: a.midX - 100, y: a.midY, width: size.width, height: size.height)
        #expect(NotchGeometry.snap(mid, to: plainScreen, layout: layout) == mid)
    }

    @Test func draggingOffScreenIsPulledBack() {
        let a = plainScreen.visibleFrame
        let size = layout.hudCollapsed
        let r = NotchGeometry.snap(CGRect(x: a.minX - 500, y: a.maxY + 300, width: size.width, height: size.height), to: plainScreen, layout: layout)
        #expect(a.contains(r))
    }

    @Test func hudAnchorRoundTripsAcrossSizesAndResolutions() {
        let collapsed = CGRect(x: 2000, y: 600, width: layout.hudCollapsed.width, height: layout.hudCollapsed.height)
        let anchor = NotchGeometry.hudAnchor(of: collapsed, on: plainScreen)
        let again = NotchGeometry.hudFrame(screen: plainScreen, expanded: false, anchor: anchor, layout: layout)
        #expect(abs(again.minX - collapsed.minX) < 0.001)
        #expect(abs(again.maxY - collapsed.maxY) < 0.001)
        let big = NotchGeometry.hudFrame(screen: plainScreen, expanded: true, anchor: anchor, layout: layout)
        #expect(big.size == layout.hudExpanded)
        #expect(abs(big.maxY - collapsed.maxY) < 0.001)     // grows downward from the same top
        #expect(abs(big.midX - collapsed.midX) < 0.001)
        // Same anchor on a different resolution lands at the same relative spot, inside the screen.
        let other = ScreenInfo(id: 3, frame: CGRect(x: 0, y: 0, width: 1280, height: 800))
        #expect(other.visibleFrame.contains(NotchGeometry.hudFrame(screen: other, expanded: true, anchor: anchor, layout: layout)))
    }

    @Test func hudDefaultIsInsideTheScreenEvenWhenExpanded() {
        for s in [notchScreen, plainScreen] {
            for expanded in [false, true] {
                #expect(s.visibleFrame.contains(NotchGeometry.hudFrame(screen: s, expanded: expanded, layout: layout)))
            }
        }
    }
}

@Suite struct SettingsStoreTests {
    func store() -> (SettingsStore, UserDefaults, String) {
        let name = "loopx-test-\(UUID().uuidString)"
        let d = UserDefaults(suiteName: name)!
        return (SettingsStore(defaults: d), d, name)
    }

    @Test func defaultsAndPersistence() {
        let (s, d, name) = store(); defer { d.removePersistentDomain(forName: name) }
        #expect(s.displayMode == .notch)
        #expect(s.dashboardWindow == .maximized)
        #expect(s.hotkey == HotkeyPreset.presets[0])
        #expect(s.hudAnchor == nil)
        s.dashboardWindow = .fullScreen
        #expect(SettingsStore(defaults: d).dashboardWindow == .fullScreen)
        d.set("tiny", forKey: "dashboardWindow")
        #expect(s.dashboardWindow == .maximized)
        s.displayMode = .hud
        s.hotkey = HotkeyPreset.presets[2]
        s.hudAnchor = CGPoint(x: 0.25, y: 0.5)
        let again = SettingsStore(defaults: d)
        #expect(again.displayMode == .hud)
        #expect(again.hotkey.id == "ctrl-opt-l")
        #expect(again.hudAnchor == CGPoint(x: 0.25, y: 0.5))
        again.hudAnchor = nil
        #expect(again.hudAnchor == nil)
    }

    @Test func garbageValuesFallBackToDefaults() {
        let (s, d, name) = store(); defer { d.removePersistentDomain(forName: name) }
        d.set("sideways", forKey: "displayMode"); d.set("zzz", forKey: "hotkey"); d.set([1.0], forKey: "hudAnchor")
        #expect(s.displayMode == .notch)
        #expect(s.hotkey == HotkeyPreset.presets[0])
        #expect(s.hudAnchor == nil)
    }

    @Test func hotkeyNoneDisablesAndPresetsAreUnique() {
        s_none()
        #expect(Set(HotkeyPreset.presets.map(\.id)).count == HotkeyPreset.presets.count)
        #expect(HotkeyPreset.presets.allSatisfy { $0.isEnabled })
    }
    func s_none() {
        let (s, d, name) = store(); defer { d.removePersistentDomain(forName: name) }
        s.hotkey = .none
        #expect(!s.hotkey.isEnabled)
    }
}
