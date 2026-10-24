import SwiftUI
import AppKit

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

    static let primary = pair(0x0B57D0, 0x9CC0FC)
    static let onPrimary = pair(0xFFFFFF, 0x032763)
    static let primaryContainer = pair(0xD3E3FD, 0x043B95)
    static let surface = pair(0xFBF8FA, 0x232529)
    static let surfaceLow = pair(0xF5F1F4, 0x2A2D32)
    static let surfaceContainer = pair(0xEFEBEE, 0x2F3237)
    static let surfaceHighest = pair(0xE3DEE2, 0x454A52)
    static let onSurface = pair(0x1C1B1E, 0xE3E5E8)
    static let onSurfaceVariant = pair(0x47464A, 0xC7CBD1)
    static let outlineVariant = pair(0xC7C5CA, 0x454B54)
    static let success = pair(0x2A6F43, 0xB5E3C6)
    static let warning = pair(0x774C22, 0xE8CCB0)
    static let error = pair(0x772822, 0xE8B4B0)

    static let cardRadius: CGFloat = 12
}

/// A dashboard `.card`: tinted surface, 12pt radius, a 500-weight title.
struct MDCard<Content: View>: View {
    let title: String?
    let content: Content
    init(_ title: String? = nil, @ViewBuilder content: () -> Content) { self.title = title; self.content = content() }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if let title { Text(title).font(.system(size: 15, weight: .medium)).foregroundStyle(MD.onSurface) }
            content
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 20).padding(.vertical, 16)
        .background(MD.surfaceContainer, in: RoundedRectangle(cornerRadius: MD.cardRadius))
    }
}

/// A label/value line inside a card.
struct MDRow: View {
    let label: String
    let value: String
    var body: some View {
        HStack(alignment: .firstTextBaseline) {
            Text(label).foregroundStyle(MD.onSurfaceVariant)
            Spacer(minLength: 16)
            Text(value).foregroundStyle(MD.onSurface).multilineTextAlignment(.trailing).textSelection(.enabled)
        }.font(.system(size: 13))
    }
}

struct MDNote: View {
    let text: String
    init(_ text: String) { self.text = text }
    var body: some View {
        Text(text).font(.system(size: 12)).foregroundStyle(MD.onSurfaceVariant).fixedSize(horizontal: false, vertical: true)
    }
}

/// Pill buttons like the dashboard's: filled for the main action, tonal for the rest.
struct MDButtonStyle: ButtonStyle {
    var filled = false
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 13, weight: .medium))
            .padding(.horizontal, 16).padding(.vertical, 7)
            .foregroundStyle(filled ? MD.onPrimary : MD.primary)
            .background(filled ? MD.primary : MD.primaryContainer.opacity(0.55), in: Capsule())
            .opacity(configuration.isPressed ? 0.8 : 1)
    }
}
