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

/// Apple-style pill buttons: filled accent for the main action, quiet translucent fill for the rest.
struct MDButtonStyle: ButtonStyle {
    var filled = false
    @Environment(\.isEnabled) private var enabled
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 13, weight: .medium))
            .padding(.horizontal, 16).padding(.vertical, 6)
            .foregroundStyle(filled ? MD.onPrimary : MD.onSurface)
            .background(filled ? AnyShapeStyle(MD.primary) : AnyShapeStyle(Color.primary.opacity(configuration.isPressed ? 0.16 : 0.08)), in: Capsule())
            .overlay(Capsule().stroke(Color.primary.opacity(filled ? 0 : 0.06), lineWidth: 0.5))
            .opacity(enabled ? (filled && configuration.isPressed ? 0.85 : 1) : 0.45)
            .animation(.timingCurve(0.32, 0.72, 0, 1, duration: 0.2), value: configuration.isPressed)
    }
}

/// A label on the left and an iOS/macOS-style switch on the right, like System Settings.
struct MDSwitchRow: View {
    @Environment(\.accent) private var accent
    let title: String
    @Binding var isOn: Bool
    init(_ title: String, isOn: Binding<Bool>) { self.title = title; self._isOn = isOn }
    var body: some View {
        HStack(spacing: 12) {
            Text(title).font(.system(size: 13)).foregroundStyle(MD.cardText(accent))
            Spacer(minLength: 12)
            Toggle(title, isOn: $isOn).labelsHidden().toggleStyle(.switch)
        }
    }
}

/// A label on the left and a native pop-up menu on the right.
struct MDMenuRow<V: Hashable, Options: View>: View {
    @Environment(\.accent) private var accent
    let title: String
    @Binding var selection: V
    let options: Options
    init(_ title: String, selection: Binding<V>, @ViewBuilder options: () -> Options) {
        self.title = title; self._selection = selection; self.options = options()
    }
    var body: some View {
        HStack(spacing: 12) {
            Text(title).font(.system(size: 13)).foregroundStyle(MD.cardText(accent))
            Spacer(minLength: 12)
            Picker(title, selection: $selection) { options }
                .labelsHidden().pickerStyle(.menu).fixedSize()
        }
    }
}

/// Translucent behind-window material, like a Mac app's sidebar.
struct MDVisualEffect: NSViewRepresentable {
    var material: NSVisualEffectView.Material = .sidebar
    func makeNSView(context: Context) -> NSVisualEffectView {
        let v = NSVisualEffectView()
        v.material = material; v.blendingMode = .behindWindow; v.state = .active
        return v
    }
    func updateNSView(_ v: NSVisualEffectView, context: Context) { v.material = material }
}
