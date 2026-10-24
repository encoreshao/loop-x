import Foundation
import CoreGraphics

public enum DisplayMode: String, Codable, CaseIterable, Sendable {
    case notch, hud, off
}

/// Everything geometry needs to know about one screen, in AppKit coordinates (origin bottom-left).
/// Built from `NSScreen` by the app; plain values here so tests need no display.
public struct ScreenInfo: Equatable, Sendable {
    public var id: Int
    public var frame: CGRect
    public var visibleFrame: CGRect
    public var safeAreaTop: CGFloat
    /// The menu-bar strip left / right of the notch (`NSScreen.auxiliaryTopLeftArea` / `...RightArea`).
    public var auxiliaryTopLeft: CGRect?
    public var auxiliaryTopRight: CGRect?

    public init(id: Int = 0, frame: CGRect, visibleFrame: CGRect? = nil, safeAreaTop: CGFloat = 0,
                auxiliaryTopLeft: CGRect? = nil, auxiliaryTopRight: CGRect? = nil) {
        self.id = id; self.frame = frame; self.visibleFrame = visibleFrame ?? frame
        self.safeAreaTop = safeAreaTop
        self.auxiliaryTopLeft = auxiliaryTopLeft; self.auxiliaryTopRight = auxiliaryTopRight
    }

    public var hasNotch: Bool {
        safeAreaTop > 0 && auxiliaryTopLeft != nil && auxiliaryTopRight != nil
    }

    /// The physical notch rectangle (the gap between the two auxiliary areas), or nil.
    public var notchRect: CGRect? {
        guard hasNotch, let l = auxiliaryTopLeft, let r = auxiliaryTopRight else { return nil }
        let width = r.minX - l.maxX
        guard width > 0 else { return nil }
        return CGRect(x: l.maxX, y: frame.maxY - safeAreaTop, width: width, height: safeAreaTop)
    }
}

public enum Placement: Equatable, Sendable {
    case notch(ScreenInfo)
    case hud(ScreenInfo)
    case none
}

public struct NotchLayout: Equatable, Sendable {
    /// Width added to each side of the notch while collapsed (room for the status glyph).
    public var wing: CGFloat = 36
    public var expandedWidth: CGFloat = 400
    public var expandedContentHeight: CGFloat = 150
    public var hudCollapsed = CGSize(width: 230, height: 34)
    public var hudExpanded = CGSize(width: 380, height: 170)
    public var hudTopMargin: CGFloat = 8
    public var snapThreshold: CGFloat = 28
    public var edgeMargin: CGFloat = 8
    public init() {}
}

public enum NotchGeometry {
    /// Which screen shows alerts: the one under the pointer, else the first (main) screen.
    public static func alertScreen(screens: [ScreenInfo], pointer: CGPoint) -> ScreenInfo? {
        screens.first { $0.frame.contains(pointer) } ?? screens.first
    }

    /// Notch mode pins to a real notch and falls back to a HUD where a screen has none; HUD mode
    /// is always a HUD; off shows nothing.
    public static func placement(mode: DisplayMode, screen: ScreenInfo?) -> Placement {
        guard let screen else { return .none }
        switch mode {
        case .off: return .none
        case .hud: return .hud(screen)
        case .notch: return screen.hasNotch ? .notch(screen) : .hud(screen)
        }
    }

    /// Panel frame hugging the notch: flush with the screen's top edge, centred on the notch.
    /// Collapsed = notch plus a wing on each side at menu-bar height; expanded drops down.
    public static func notchFrame(screen: ScreenInfo, expanded: Bool, layout: NotchLayout = NotchLayout()) -> CGRect {
        guard let notch = screen.notchRect else {
            return hudFrame(screen: screen, expanded: expanded, layout: layout)
        }
        let collapsedWidth = notch.width + 2 * layout.wing
        let width = expanded ? max(layout.expandedWidth, collapsedWidth) : collapsedWidth
        let height = expanded ? notch.height + layout.expandedContentHeight : notch.height
        // Keep the panel inside the screen even on an unusually narrow one.
        let x = clamp(notch.midX - width / 2, lower: screen.frame.minX, upper: screen.frame.maxX - width)
        return CGRect(x: x, y: screen.frame.maxY - height, width: width, height: height)
    }

    /// Default HUD spot (no notch): centred under the menu bar.
    public static func hudFrame(screen: ScreenInfo, expanded: Bool, layout: NotchLayout = NotchLayout(),
                                origin: CGPoint? = nil) -> CGRect {
        let size = expanded ? layout.hudExpanded : layout.hudCollapsed
        let area = screen.visibleFrame
        let anchor: CGPoint
        if let origin {
            anchor = origin
        } else {
            anchor = CGPoint(x: area.midX - size.width / 2, y: area.maxY - size.height - layout.hudTopMargin)
        }
        return keepInside(CGRect(origin: anchor, size: size), area: area)
    }

    /// Moves a dragged HUD rect so edges within `snapThreshold` of the screen's visible edges
    /// stick to them (with `edgeMargin`), never leaving the screen.
    public static func snap(_ rect: CGRect, to screen: ScreenInfo, layout: NotchLayout = NotchLayout()) -> CGRect {
        let area = screen.visibleFrame
        var r = keepInside(rect, area: area)
        let m = layout.edgeMargin, t = layout.snapThreshold
        if r.minX - area.minX < t { r.origin.x = area.minX + m }
        else if area.maxX - r.maxX < t { r.origin.x = area.maxX - r.width - m }
        if r.minY - area.minY < t { r.origin.y = area.minY + m }
        else if area.maxY - r.maxY < t { r.origin.y = area.maxY - r.height - m }
        return keepInside(r, area: area)
    }

    /// A HUD position is remembered as the top-centre of the panel relative to the screen, so it
    /// survives the collapsed/expanded size change and a changed resolution.
    public static func hudAnchor(of rect: CGRect, on screen: ScreenInfo) -> CGPoint {
        let a = screen.visibleFrame
        return CGPoint(x: (rect.midX - a.minX) / max(a.width, 1), y: (a.maxY - rect.maxY) / max(a.height, 1))
    }

    /// Frame for a remembered anchor at the given size, grown downward from its top edge.
    public static func hudFrame(screen: ScreenInfo, expanded: Bool, anchor: CGPoint, layout: NotchLayout = NotchLayout()) -> CGRect {
        let size = expanded ? layout.hudExpanded : layout.hudCollapsed
        let a = screen.visibleFrame
        let top = a.maxY - anchor.y * a.height
        let origin = CGPoint(x: a.minX + anchor.x * a.width - size.width / 2, y: top - size.height)
        return keepInside(CGRect(origin: origin, size: size), area: a)
    }

    static func keepInside(_ r: CGRect, area: CGRect) -> CGRect {
        var o = r
        o.origin.x = clamp(r.minX, lower: area.minX, upper: area.maxX - r.width)
        o.origin.y = clamp(r.minY, lower: area.minY, upper: area.maxY - r.height)
        return o
    }

    private static func clamp(_ v: CGFloat, lower: CGFloat, upper: CGFloat) -> CGFloat {
        upper < lower ? lower : min(max(v, lower), upper)
    }
}
