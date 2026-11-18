/// When the start screen's hold (`StartScreen.introSeconds`) begins: once the start screen's own
/// load has finished, so a cold WebContent process spending a few hundred ms before the first paint
/// doesn't eat into the entrance. `fallback()` starts it anyway if that load never reports finishing.
public struct IntroHold {
    /// How long after issuing the load to start the hold regardless.
    public static let fallbackSeconds = 1.5

    private var pending: ObjectIdentifier?
    private var started = false

    public init() {}

    /// The start screen's load was issued; `id` identifies its navigation.
    public mutating func loading(_ id: ObjectIdentifier) {
        pending = id; started = false
    }

    /// A navigation finished. True exactly once: when it is the start screen's and the hold hasn't begun.
    public mutating func finished(_ id: ObjectIdentifier) -> Bool {
        guard !started, pending == id else { return false }
        started = true
        return true
    }

    /// The fallback timer fired. True if the hold should begin now (it hadn't yet).
    public mutating func fallback() -> Bool {
        guard !started, pending != nil else { return false }
        started = true
        return true
    }
}
