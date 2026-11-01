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
        #expect(Accent.dark.navSurface == 0x1C1C1F)
        #expect(Accent.dark.navOnSurface == 0xE4E4E8)
        #expect(Accent.dark.navActiveSurface == 0x2E2E33)
        #expect(Accent.dark.navActiveOnSurface == 0xFFFFFF)
        #expect(Accent.default.navSurface == nil)   // "Default" is the neutral surface, not a tint
    }

    @Test func paletteMatchesTheDashboardTokens() {
        #expect(MacPalette.primaryLight == 0x007AFF)
        #expect(MacPalette.primaryDark == 0x0A84FF)
        #expect(MacPalette.surfaceLight == 0xF5F5F7)
        #expect(MacPalette.surfaceDark == 0x242426)
        #expect(MacPalette.errorLight == 0x8C2A24)
        #expect(MacPalette.errorDark == 0xFF8A84)
        #expect(MacPalette.cardRadius == 14)
        #expect(MacPalette.controlRadius == 8)
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
