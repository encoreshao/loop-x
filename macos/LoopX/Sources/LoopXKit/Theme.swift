import Foundation

/// The dashboard's sidebar/card wash (`data-accent` in `_STYLE.txt`). Hex values are the CSS `--md-nav-*` tokens;
/// `default` has none because it uses the plain neutral surface.
public enum Accent: String, CaseIterable, Sendable {
    case `default`, indigo, blue, green, red, gray

    /// What the dashboard falls back to when nothing was chosen (`accent || 'indigo'` in the shell script).
    public init(stored: String?) { self = stored.flatMap(Accent.init(rawValue:)) ?? .indigo }

    public var navSurface: UInt32? { tokens?.0 }
    public var navOnSurface: UInt32? { tokens?.1 }
    public var navActiveSurface: UInt32? { tokens?.2 }
    public var navActiveOnSurface: UInt32? { tokens?.3 }

    private var tokens: (UInt32, UInt32, UInt32, UInt32)? {
        switch self {
        case .default: return nil
        case .indigo: return (0xF4F0FF, 0x3A3550, 0xE0D6FF, 0x4B3FA8)
        case .blue: return (0xE9F3FC, 0x2C3E4A, 0xCFE7FB, 0x0B57D0)
        case .green: return (0xECF4EE, 0x2A3B2D, 0xD2E8D6, 0x2E7D3C)
        case .red: return (0xFCF1EF, 0x4A2F2B, 0xF7D9D3, 0xB3261E)
        case .gray: return (0xECECEF, 0x3A3A3D, 0xD6D6DB, 0x3A3A3D)
        }
    }
}

/// The dashboard's color mode: `light` / `dark` are stored; anything else follows the system.
public enum ColorMode: String, CaseIterable, Sendable {
    case auto, light, dark
    public init(stored: String?) { self = stored.flatMap(ColorMode.init(rawValue:)) ?? .auto }
}

/// The macOS palette shared with the dashboard (`--md-*` in `_STYLE.txt`): light/dark hex pairs and the corner radii.
/// Kept in LoopXKit so tests can pin it to the CSS; the app's `MD` tokens read from here.
public enum MacPalette {
    public static let primaryLight: UInt32 = 0x007AFF, primaryDark: UInt32 = 0x0A84FF
    public static let onPrimaryLight: UInt32 = 0xFFFFFF, onPrimaryDark: UInt32 = 0xFFFFFF
    public static let primaryContainerLight: UInt32 = 0xD6E8FF, primaryContainerDark: UInt32 = 0x1B3A5E
    public static let surfaceLight: UInt32 = 0xF5F5F7, surfaceDark: UInt32 = 0x242426
    public static let surfaceLowLight: UInt32 = 0xF2F2F7, surfaceLowDark: UInt32 = 0x2C2C2E
    public static let surfaceContainerLight: UInt32 = 0xEBEBF0, surfaceContainerDark: UInt32 = 0x323234
    public static let surfaceHighestLight: UInt32 = 0xD8D8DE, surfaceHighestDark: UInt32 = 0x48484A
    public static let onSurfaceLight: UInt32 = 0x1D1D1F, onSurfaceDark: UInt32 = 0xF5F5F7
    public static let onSurfaceVariantLight: UInt32 = 0x6E6E73, onSurfaceVariantDark: UInt32 = 0xAEAEB2
    public static let outlineVariantLight: UInt32 = 0xD1D1D6, outlineVariantDark: UInt32 = 0x3A3A3C
    public static let successLight: UInt32 = 0x1F6B35, successDark: UInt32 = 0x8CE6A1
    public static let warningLight: UInt32 = 0x7A4A0A, warningDark: UInt32 = 0xFFB454
    public static let errorLight: UInt32 = 0x8C2A24, errorDark: UInt32 = 0xFF8A84

    public static let cardRadius: Double = 14
    public static let controlRadius: Double = 8
}
