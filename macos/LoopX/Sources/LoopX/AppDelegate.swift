import AppKit
import SwiftUI
import LoopXKit

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    let model: AppModel
    let notch: NotchController
    let status: StatusItemController
    let dashboard = DashboardWindowController()
    let hotkey = HotkeyManager()
    let updater = Updater()
    let scheduler: SchedulerRunner
    let settingsModel: SettingsModel
    private var settingsWindow: NSWindow?

    override init() {
        let settings = SettingsStore()
        model = AppModel(settings: settings)
        notch = NotchController(model: model)
        status = StatusItemController(model: model)
        settingsModel = SettingsModel(store: settings, login: LaunchAtLogin(service: SMAppServiceAdapter(.mainApp)))
        scheduler = SchedulerRunner(binary: model.backendConfig.loopxBinary)
        super.init()
        wire()
    }

    private func wire() {
        dashboard.baseURLProvider = { [weak self] in self?.model.lock?.baseURL }
        dashboard.modeProvider = { [weak self] in self?.settingsModel.dashboardWindow ?? .maximized }
        dashboard.statusProvider = { [weak self] in
            switch self?.model.connection {
            case .failed(let m)?: return (m, true)
            default: return ("Starting the backend…", false)
            }
        }
        dashboard.onNeedsBackend = { [weak self] in
            guard let self, self.model.lock == nil else { return }
            Task { await self.model.ensureBackend() }
        }
        model.openDashboard = { [weak self] in self?.dashboard.show() }
        let previous = model.onNotchChange
        model.onNotchChange = { [weak self] old, new in
            previous(old, new)
            self?.status.update(new)
            self?.dashboard.backendChanged()
        }
        status.onOpenDashboard = { [weak self] in self?.dashboard.show() }
        status.onOpenSettings = { [weak self] in self?.showSettings() }
        if updater.isEnabled { status.onCheckForUpdates = { [weak self] in self?.updater.checkForUpdates() } }
        status.onToggleHidden = { [weak self] in self?.notch.toggleHidden() }
        status.isHidden = { [weak self] in self?.notch.isHidden ?? false }
        status.onModeChange = { [weak self] m in self?.settingsModel.displayMode = m }
        settingsModel.onChange = { [weak self] in
            guard let self else { return }
            self.notch.refresh()
            self.hotkey.register(self.settingsModel.hotkey)
            self.scheduler.setEnabled(self.settingsModel.runScheduler)
        }
        hotkey.onPress = { [weak self] in self?.notch.toggleHidden() }
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
        NSApp.mainMenu = makeMainMenu()
        notch.refresh()
        hotkey.register(settingsModel.hotkey)
        scheduler.setEnabled(settingsModel.runScheduler)
        model.start()
        // The takeover prompt is modal-ish: ask once, as soon as discovery finds an older backend.
        Task { @MainActor in
            for await _ in model.$takeoverOffer.values {
                guard let offer = model.takeoverOffer else { continue }
                askTakeover(offer)
            }
        }
        Task { @MainActor in
            for await _ in model.$lock.values { dashboard.backendChanged() }
        }
        Task { @MainActor in
            for await _ in model.$connection.values { dashboard.refreshPlaceholder() }
        }
    }

    /// An accessory app has no menu bar of its own, but the key equivalents (⌘Q, ⌘W, ⌘C/⌘V, ⌘,)
    /// are dispatched through the main menu, so it must exist even while the bar is not shown.
    private func makeMainMenu() -> NSMenu {
        func item(_ title: String, _ action: Selector?, _ key: String, target: AnyObject? = nil,
                  _ flags: NSEvent.ModifierFlags = .command) -> NSMenuItem {
            let i = NSMenuItem(title: title, action: action, keyEquivalent: key)
            i.keyEquivalentModifierMask = flags; i.target = target
            return i
        }
        func submenu(_ title: String, _ items: [NSMenuItem]) -> NSMenuItem {
            let m = NSMenu(title: title); items.forEach(m.addItem)
            let host = NSMenuItem(title: title, action: nil, keyEquivalent: ""); host.submenu = m
            return host
        }
        let main = NSMenu()
        main.addItem(submenu("LoopX", [
            item("Settings…", #selector(openSettingsFromMenu), ",", target: self),
            .separator(),
            item("Hide LoopX", #selector(NSApplication.hide(_:)), "h"),
            item("Hide Others", #selector(NSApplication.hideOtherApplications(_:)), "h", [.command, .option]),
            .separator(),
            item("Quit LoopX", #selector(NSApplication.terminate(_:)), "q"),
        ]))
        main.addItem(submenu("Edit", [
            item("Undo", Selector(("undo:")), "z"),
            item("Redo", Selector(("redo:")), "z", [.command, .shift]),
            .separator(),
            item("Cut", #selector(NSText.cut(_:)), "x"),
            item("Copy", #selector(NSText.copy(_:)), "c"),
            item("Paste", #selector(NSText.paste(_:)), "v"),
            item("Select All", #selector(NSText.selectAll(_:)), "a"),
        ]))
        main.addItem(submenu("Window", [
            item("Minimize", #selector(NSWindow.performMiniaturize(_:)), "m"),
            item("Close", #selector(NSWindow.performClose(_:)), "w"),
        ]))
        return main
    }

    @objc private func openSettingsFromMenu() { showSettings() }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }

    func applicationWillTerminate(_ notification: Notification) {
        hotkey.unregister(); model.stop()   // the backend keeps running: it is its own LaunchAgent
    }

    private func askTakeover(_ offer: TakeoverOffer) {
        NSApp.activate(ignoringOtherApps: true)
        let a = NSAlert()
        a.messageText = "A newer loopx backend is bundled"
        a.informativeText = "loopx \(offer.runningVersion) is already running, but this app includes \(offer.bundledVersion). Replace the running backend? Loops in progress will be interrupted."
        a.addButton(withTitle: "Replace")
        a.addButton(withTitle: "Keep Running Version")
        if a.runModal() == .alertFirstButtonReturn { model.acceptTakeover() } else { model.declineTakeover() }
    }

    func showSettings() {
        if settingsWindow == nil {
            let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 440, height: 430),
                             styleMask: [.titled, .closable], backing: .buffered, defer: false)
            w.title = "LoopX Settings"
            w.contentView = NSHostingView(rootView: SettingsView(settings: settingsModel, app: model))
            w.isReleasedWhenClosed = false
            w.center()
            settingsWindow = w
        }
        NSApp.activate(ignoringOtherApps: true)
        settingsWindow?.makeKeyAndOrderFront(nil)
    }

    /// `LOOPX_APP_SELFTEST=1`: build the whole UI without a backend, lay it out, report, exit.
    func selfTest() -> Bool {
        var lines: [String] = []
        let screen = ScreenInfo(id: 1, frame: CGRect(x: 0, y: 0, width: 1512, height: 982), safeAreaTop: 32,
                                auxiliaryTopLeft: CGRect(x: 0, y: 950, width: 663, height: 32),
                                auxiliaryTopRight: CGRect(x: 849, y: 950, width: 663, height: 32))
        notch.screensProvider = { [screen] in [screen] }
        notch.refresh()
        lines.append("panel=\(NSStringFromRect(notch.panel.frame))")
        notch.setExpanded(true)
        notch.host_layoutForSelfTest()
        lines.append("expanded=\(NSStringFromRect(notch.panel.frame))")
        status.update(NotchState.idle)
        let menuOK = status.item.menu != nil
        dashboard.ensureWindow()
        let webOK = dashboard.webView != nil
        let settingsHost = NSHostingView(rootView: SettingsView(settings: settingsModel, app: model))
        settingsHost.layoutSubtreeIfNeeded()
        let panelLevel = notch.panel.level.rawValue
        let all = notch.panel.collectionBehavior.contains(.canJoinAllSpaces) && notch.panel.collectionBehavior.contains(.fullScreenAuxiliary)
        lines.append("backend=\(model.backendConfig.starter is SMAppServiceStarter ? "bundled-agent" : "process") bundledVersion=\(model.backendConfig.discovery.bundledVersion)")
        lines.append("level=\(panelLevel) spaces=\(all) menu=\(menuOK) web=\(webOK) settings=\(settingsHost.fittingSize)")
        print("LoopX selftest ok: " + lines.joined(separator: " "))
        return menuOK && webOK && all
    }
}

extension NotchController {
    func host_layoutForSelfTest() { panel.contentView?.layoutSubtreeIfNeeded() }
}
