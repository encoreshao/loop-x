import SwiftUI
import AppKit
import LoopXKit

/// The dashboard's Material 3 tokens (`--md-*` in `crates/server/assets/blobs/dashboard_server/_STYLE.txt`),
/// as colors that follow the system light/dark appearance, so native windows look like the web pages.
enum MD {
    private static func hex(_ v: UInt32) -> NSColor {
        NSColor(srgbRed: CGFloat((v >> 16) & 0xFF) / 255, green: CGFloat((v >> 8) & 0xFF) / 255, blue: CGFloat(v & 0xFF) / 255, alpha: 1)
    }
    private static func pair(_ light: UInt32, _ dark: UInt32) -> Color {
        Color(nsColor: NSColor(name: nil) { a in
            a.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? hex(dark) : hex(light)
        })
    }

    static let primary = pair(MacPalette.primaryLight, MacPalette.primaryDark)
    static let onPrimary = pair(MacPalette.onPrimaryLight, MacPalette.onPrimaryDark)
    static let primaryContainer = pair(MacPalette.primaryContainerLight, MacPalette.primaryContainerDark)
    static let surface = pair(MacPalette.surfaceLight, MacPalette.surfaceDark)
    static let surfaceLow = pair(MacPalette.surfaceLowLight, MacPalette.surfaceLowDark)
    static let surfaceContainer = pair(MacPalette.surfaceContainerLight, MacPalette.surfaceContainerDark)
    static let surfaceHighest = pair(MacPalette.surfaceHighestLight, MacPalette.surfaceHighestDark)
    static let onSurface = pair(MacPalette.onSurfaceLight, MacPalette.onSurfaceDark)
    static let onSurfaceVariant = pair(MacPalette.onSurfaceVariantLight, MacPalette.onSurfaceVariantDark)
    static let outlineVariant = pair(MacPalette.outlineVariantLight, MacPalette.outlineVariantDark)
    static let success = pair(MacPalette.successLight, MacPalette.successDark)
    static let warning = pair(MacPalette.warningLight, MacPalette.warningDark)
    static let error = pair(MacPalette.errorLight, MacPalette.errorDark)

    static let cardRadius = CGFloat(MacPalette.cardRadius)
    static let controlRadius = CGFloat(MacPalette.controlRadius)

    /// The accent wash the dashboard puts on its sidebar and cards (`--md-nav-*`); `default` keeps the neutral surface.
    private static func wash(_ v: UInt32?) -> Color? { v.map { Color(nsColor: hex($0)) } }
    static func sidebar(_ a: Accent) -> Color { wash(a.navSurface) ?? surfaceLow }
    static func navOn(_ a: Accent) -> Color { wash(a.navOnSurface) ?? onSurfaceVariant }
    static func navActive(_ a: Accent) -> Color { wash(a.navActiveSurface) ?? surfaceHighest }
    static func navActiveOn(_ a: Accent) -> Color { wash(a.navActiveOnSurface) ?? primary }
    static func card(_ a: Accent) -> Color { wash(a.navSurface) ?? surfaceContainer }
    /// Text on a card: the washes are light even in dark mode, so tinted cards use the dark nav text color.
    static func cardText(_ a: Accent) -> Color { wash(a.navOnSurface) ?? onSurface }
    static func cardTextVariant(_ a: Accent) -> Color { wash(a.navOnSurface)?.opacity(0.8) ?? onSurfaceVariant }
}

/// A dashboard `.card`: tinted surface, 14pt continuous corners with a hairline border, a semibold title.
private struct AccentKey: EnvironmentKey { static let defaultValue = Accent.default }
extension EnvironmentValues {
    var accent: Accent { get { self[AccentKey.self] } set { self[AccentKey.self] = newValue } }
}

struct MDCard<Content: View>: View {
    @Environment(\.accent) private var accent
    let title: String?
    let content: Content
    init(_ title: String? = nil, @ViewBuilder content: () -> Content) { self.title = title; self.content = content() }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if let title { Text(title).font(.system(size: 15, weight: .semibold)).foregroundStyle(MD.cardText(accent)) }
            content
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 20).padding(.vertical, 16)
        .background(MD.card(accent), in: RoundedRectangle(cornerRadius: MD.cardRadius, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: MD.cardRadius, style: .continuous).stroke(MD.outlineVariant.opacity(0.5), lineWidth: 0.5))
        .modifier(TintedCardScheme(tinted: accent != .default))
    }
}

/// A tinted card is a light wash even in dark mode, so its system controls (toggle and picker labels,
/// checkboxes) render with the light scheme instead of white text on the wash.
private struct TintedCardScheme: ViewModifier {
    let tinted: Bool
    func body(content: Content) -> some View {
        if tinted { content.environment(\.colorScheme, .light) } else { content }
    }
}

/// A label/value line inside a card.
struct MDRow: View {
    @Environment(\.accent) private var accent
    let label: String
    let value: String
    var body: some View {
        HStack(alignment: .firstTextBaseline) {
            Text(label).foregroundStyle(MD.cardTextVariant(accent))
            Spacer(minLength: 16)
            Text(value).foregroundStyle(MD.cardText(accent)).multilineTextAlignment(.trailing).textSelection(.enabled)
        }.font(.system(size: 13))
    }
}

struct MDNote: View {
    @Environment(\.accent) private var accent
    let text: String
    init(_ text: String) { self.text = text }
    var body: some View {
        Text(text).font(.system(size: 12)).foregroundStyle(MD.cardTextVariant(accent)).fixedSize(horizontal: false, vertical: true)
    }
}

/// Rounded-rect buttons like the dashboard's: filled for the main action, tonal for the rest.
struct MDButtonStyle: ButtonStyle {
    var filled = false
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 13, weight: .medium))
            .padding(.horizontal, 16).padding(.vertical, 7)
            .foregroundStyle(filled ? MD.onPrimary : MD.onSurface)
            .background(filled ? MD.primary : MD.surfaceHighest,
                        in: RoundedRectangle(cornerRadius: MD.controlRadius, style: .continuous))
            .scaleEffect(configuration.isPressed ? 0.97 : 1)
    }
}
