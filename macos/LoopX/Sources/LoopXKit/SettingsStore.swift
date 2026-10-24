import Foundation

/// A global hide/show hotkey choice. Fixed presets keep the settings UI trivial; `keyCode` is a
/// Carbon virtual key code and `modifiers` use Carbon masks (cmd 256, shift 512, option 2048, control 4096).
public struct HotkeyPreset: Codable, Equatable, Hashable, Sendable, Identifiable {
    public var id: String
    public var label: String
    public var keyCode: UInt32
    public var modifiers: UInt32

    public static let cmd: UInt32 = 256, shift: UInt32 = 512, option: UInt32 = 2048, control: UInt32 = 4096

    public static let presets: [HotkeyPreset] = [
        HotkeyPreset(id: "ctrl-opt-cmd-l", label: "⌃⌥⌘L", keyCode: 37, modifiers: control | option | cmd),
        HotkeyPreset(id: "ctrl-opt-cmd-n", label: "⌃⌥⌘N", keyCode: 45, modifiers: control | option | cmd),
        HotkeyPreset(id: "ctrl-opt-l", label: "⌃⌥L", keyCode: 37, modifiers: control | option),
        HotkeyPreset(id: "shift-opt-cmd-l", label: "⇧⌥⌘L", keyCode: 37, modifiers: shift | option | cmd),
    ]
    public static let none = HotkeyPreset(id: "none", label: "None", keyCode: 0, modifiers: 0)
    public var isEnabled: Bool { self != .none && modifiers != 0 }
    public static func preset(id: String) -> HotkeyPreset {
        presets.first { $0.id == id } ?? (id == none.id ? none : presets[0])
    }
}

/// How the dashboard window opens the first time it is shown in a session.
public enum DashboardWindowMode: String, CaseIterable, Sendable {
    case maximized, fullScreen, standard
}

/// UserDefaults-backed preferences. The defaults object is injected so tests use a private suite.
public final class SettingsStore: @unchecked Sendable {
    private let defaults: UserDefaults
    public init(defaults: UserDefaults = .standard) { self.defaults = defaults }

    public var displayMode: DisplayMode {
        get { defaults.string(forKey: "displayMode").flatMap(DisplayMode.init(rawValue:)) ?? .notch }
        set { defaults.set(newValue.rawValue, forKey: "displayMode") }
    }

    public var dashboardWindow: DashboardWindowMode {
        get { defaults.string(forKey: "dashboardWindow").flatMap(DashboardWindowMode.init(rawValue:)) ?? .maximized }
        set { defaults.set(newValue.rawValue, forKey: "dashboardWindow") }
    }

    public var hotkey: HotkeyPreset {
        get { HotkeyPreset.preset(id: defaults.string(forKey: "hotkey") ?? HotkeyPreset.presets[0].id) }
        set { defaults.set(newValue.id, forKey: "hotkey") }
    }

    /// Whether the app runs `loopx scheduler` every 15 minutes while it is open. Off by default:
    /// a due loop acts on the real GitLab and Slack, so it is something the user opts into.
    public var runScheduler: Bool {
        get { defaults.bool(forKey: "runScheduler") }
        set { defaults.set(newValue, forKey: "runScheduler") }
    }

    /// HUD top-centre anchor as fractions of the screen's visible frame (see `NotchGeometry.hudAnchor`).
    public var hudAnchor: CGPoint? {
        get {
            guard let a = defaults.array(forKey: "hudAnchor") as? [Double], a.count == 2 else { return nil }
            return CGPoint(x: a[0], y: a[1])
        }
        set {
            if let p = newValue { defaults.set([Double(p.x), Double(p.y)], forKey: "hudAnchor") }
            else { defaults.removeObject(forKey: "hudAnchor") }
        }
    }
}
