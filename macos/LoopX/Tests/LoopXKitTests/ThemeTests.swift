import Foundation
import Testing
@testable import LoopXKit

@Suite struct ThemeTests {
    @Test func accentDefaultsToIndigoLikeTheDashboard() {
        #expect(Accent(stored: nil) == .indigo)
        #expect(Accent(stored: "") == .indigo)
        #expect(Accent(stored: "nonsense") == .indigo)
        for a in Accent.allCases { #expect(Accent(stored: a.rawValue) == a) }
    }

    @Test func accentTokensMatchTheDashboardCSS() {
        #expect(Accent.indigo.navSurface == 0xF4F0FF)
        #expect(Accent.indigo.navActiveSurface == 0xE0D6FF)
        #expect(Accent.indigo.navActiveOnSurface == 0x4B3FA8)
        #expect(Accent.green.navSurface == 0xECF4EE)
        #expect(Accent.red.navActiveOnSurface == 0xB3261E)
        #expect(Accent.default.navSurface == nil)   // "Default" is the neutral surface, not a tint
    }

    @Test func colorModeFollowsTheSystemUnlessLightOrDarkIsStored() {
        #expect(ColorMode(stored: nil) == .auto)
        #expect(ColorMode(stored: "auto") == .auto)
        #expect(ColorMode(stored: "light") == .light)
        #expect(ColorMode(stored: "dark") == .dark)
        #expect(ColorMode(stored: "purple") == .auto)
    }

    @Test func settingsStoreRemembersTheLastSeenTheme() {
        let s = SettingsStore(defaults: UserDefaults(suiteName: "theme-\(UUID().uuidString)")!)
        #expect(s.accent == .indigo && s.colorMode == .auto)
        s.accent = .green; s.colorMode = .dark
        #expect(s.accent == .green && s.colorMode == .dark)
    }
}
