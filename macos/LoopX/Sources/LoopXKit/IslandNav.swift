import Foundation

/// Which island screen is showing, and whether the user has pinned it open.
public struct IslandNav: Equatable, Sendable {
    public enum Screen: Equatable, Sendable { case home, detail(taskID: String) }

    public private(set) var stack: [Screen] = [.home]
    public private(set) var isPinned = false
    public var inputText = ""
    public var isReplying = false

    public init() {}

    public var screen: Screen { stack.last ?? .home }
    public var canGoBack: Bool { stack.count > 1 }

    public mutating func open(_ taskID: String) { stack.append(.detail(taskID: taskID)) }
    public mutating func back() { if canGoBack { stack.removeLast() } }
    public mutating func pin() { isPinned = true }
    public mutating func unpin() {
        isPinned = false; stack = [.home]; inputText = ""; isReplying = false
    }

    /// A draft or a streaming reply must survive an Esc-less pointer drift and a snapshot refresh.
    public var blocksFold: Bool {
        isPinned && (!inputText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || isReplying)
    }

    public func shouldFoldOnPointerLeave() -> Bool { !isPinned }
}
