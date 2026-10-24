import AppKit
import SwiftUI
import Combine
import LoopXKit

/// Borderless panel that floats over every Space and over full-screen apps, never takes focus.
final class NotchPanel: NSPanel {
    init() {
        super.init(contentRect: .zero, styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: true)
        isFloatingPanel = true
        // Above the menu bar so it covers the notch strip; `.mainMenu + 3` is the usual choice.
        level = NSWindow.Level(rawValue: NSWindow.Level.mainMenu.rawValue + 3)
        collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .stationary, .ignoresCycle]
        isOpaque = false
        backgroundColor = .clear
        hasShadow = false
        hidesOnDeactivate = false
        isReleasedWhenClosed = false
        animationBehavior = .none
        titleVisibility = .hidden
        isMovableByWindowBackground = false
    }
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }
}

/// Hosting view that reports the pointer entering/leaving even while the app is inactive.
final class HoverHostingView<Content: View>: NSHostingView<Content> {
    var onHover: ((Bool) -> Void)?
    private var area: NSTrackingArea?
    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let area { removeTrackingArea(area) }
        let a = NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .activeAlways, .inVisibleRect], owner: self, userInfo: nil)
        addTrackingArea(a); area = a
    }
    override func mouseEntered(with event: NSEvent) { onHover?(true) }
    override func mouseExited(with event: NSEvent) { onHover?(false) }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
}

/// Owns the panel: where it sits (notch / HUD / hidden), hover expansion, alert pop-ups.
@MainActor
final class NotchController: NSObject {
    let model: AppModel
    let ui = NotchUI()
    let panel = NotchPanel()
    private var host: HoverHostingView<NotchRootView>!
    private var cancellables = Set<AnyCancellable>()
    private var collapseWork: DispatchWorkItem?
    private var shrinkWork: DispatchWorkItem?
    private var moveWork: DispatchWorkItem?
    private var userHidden = false
    private var currentScreenID: Int?
    private var lastAlertKey: String?
    private var lastKind: NotchKind = .idle
    private let layout = NotchLayout()
    /// Test hook: pretend these are the screens.
    var screensProvider: () -> [ScreenInfo] = { ScreenInfo.current() }
    var pointerProvider: () -> CGPoint = { NSEvent.mouseLocation }

    init(model: AppModel) {
        self.model = model
        super.init()
        host = HoverHostingView(rootView: NotchRootView(model: model, ui: ui))
        host.onHover = { [weak self] inside in self?.hover(inside) }
        panel.contentView = host
        host.sizingOptions = []   // the controller sets the frame; the view must not resize the panel
        model.onNotchChange = { [weak self] old, new in self?.notchChanged(old, new) }
        NotificationCenter.default.addObserver(self, selector: #selector(screensChanged),
                                               name: NSApplication.didChangeScreenParametersNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(panelMoved),
                                               name: NSWindow.didMoveNotification, object: panel)
    }

    // MARK: visibility

    var isHidden: Bool { userHidden }
    func setUserHidden(_ hidden: Bool) { userHidden = hidden; refresh() }
    func toggleHidden() { setUserHidden(!userHidden) }

    /// Re-evaluates placement for the current mode / screens / alert state.
    func refresh(animated: Bool = false) {
        let screens = screensProvider()
        let mode = model.settings.displayMode
        let alerting = model.notch.style.autoExpands
        let screen: ScreenInfo? = alerting
            ? NotchGeometry.alertScreen(screens: screens, pointer: pointerProvider())
            : (screens.first(where: \.hasNotch) ?? screens.first)
        let placement = NotchGeometry.placement(mode: mode, screen: screen)
        guard !userHidden, placement != .none, let screen else { panel.orderOut(nil); return }

        let isHUD: Bool
        if case .hud = placement { isHUD = true } else { isHUD = false }
        currentScreenID = screen.id
        ui.isHUD = isHUD
        panel.isMovableByWindowBackground = isHUD
        ui.notchHeight = isHUD ? 0 : (screen.notchRect?.height ?? 0)
        ui.collapsedSize = frame(for: screen, expanded: false, hud: isHUD).size
        ui.expandedSize = frame(for: screen, expanded: true, hud: isHUD).size
        applyFrame(screen: screen, hud: isHUD, expanded: ui.isExpanded)
        panel.orderFrontRegardless()
    }

    private func frame(for screen: ScreenInfo, expanded: Bool, hud: Bool) -> CGRect {
        if !hud { return NotchGeometry.notchFrame(screen: screen, expanded: expanded, layout: layout) }
        if let a = model.settings.hudAnchor {
            return NotchGeometry.hudFrame(screen: screen, expanded: expanded, anchor: a, layout: layout)
        }
        return NotchGeometry.hudFrame(screen: screen, expanded: expanded, layout: layout)
    }

    private func applyFrame(screen: ScreenInfo, hud: Bool, expanded: Bool) {
        // The panel is always as big as its biggest state while expanding, and shrinks only after
        // the collapse animation, so SwiftUI never draws outside the window.
        let f = frame(for: screen, expanded: expanded, hud: hud)
        suppressMove = true
        panel.setFrame(f, display: true)
        suppressMove = false
    }

    // MARK: hover / alerts

    private func hover(_ inside: Bool) {
        collapseWork?.cancel()
        if inside { setExpanded(true) }
        else {
            // An approval must not fold away just because the pointer drifted off.
            guard model.notch.kind != .awaitingApproval else { return }
            let w = DispatchWorkItem { [weak self] in self?.setExpanded(false) }
            collapseWork = w
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.25, execute: w)
        }
    }

    func setExpanded(_ expanded: Bool) {
        guard expanded != ui.isExpanded else { return }
        shrinkWork?.cancel()
        guard let screen = screensProvider().first(where: { $0.id == currentScreenID }) else { ui.isExpanded = expanded; return }
        if expanded {
            applyFrame(screen: screen, hud: ui.isHUD, expanded: true)
            ui.isExpanded = true
        } else {
            ui.isExpanded = false
            let w = DispatchWorkItem { [weak self] in
                guard let self, !self.ui.isExpanded else { return }
                self.applyFrame(screen: screen, hud: self.ui.isHUD, expanded: false)
            }
            shrinkWork = w
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.45, execute: w)
        }
    }

    private func notchChanged(_ old: NotchState, _ new: NotchState) {
        let alertChanged = new.alertKey != lastAlertKey || new.kind != lastKind
        lastAlertKey = new.alertKey; lastKind = new.kind
        refresh()
        guard new.style.autoExpands, alertChanged, !userHidden else {
            if !new.style.autoExpands, old.style.autoExpands { collapseWork?.cancel(); setExpanded(false) }
            return
        }
        setExpanded(true)
        collapseWork?.cancel()
        if let after = new.style.autoCollapseAfter {
            let w = DispatchWorkItem { [weak self] in self?.setExpanded(false) }
            collapseWork = w
            DispatchQueue.main.asyncAfter(deadline: .now() + after, execute: w)
        }
    }

    @objc private func screensChanged() { refresh() }

    // MARK: HUD dragging

    private var suppressMove = false
    @objc private func panelMoved() {
        guard ui.isHUD, !suppressMove else { return }
        moveWork?.cancel()
        let w = DispatchWorkItem { [weak self] in self?.settleHUD() }
        moveWork = w
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.2, execute: w)
    }

    private func settleHUD() {
        let screens = screensProvider()
        let center = CGPoint(x: panel.frame.midX, y: panel.frame.midY)
        guard let screen = screens.first(where: { $0.frame.contains(center) }) ?? screens.first else { return }
        let snapped = NotchGeometry.snap(panel.frame, to: screen, layout: layout)
        suppressMove = true
        panel.setFrame(snapped, display: true, animate: true)
        suppressMove = false
        model.settings.hudAnchor = NotchGeometry.hudAnchor(of: snapped, on: screen)
        currentScreenID = screen.id
        ui.collapsedSize = frame(for: screen, expanded: false, hud: true).size
        ui.expandedSize = frame(for: screen, expanded: true, hud: true).size
    }
}
