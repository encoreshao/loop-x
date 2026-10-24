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
