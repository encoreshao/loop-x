import AppKit
import LoopXKit

@MainActor
final class StatusItemController: NSObject {
    let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
    private let model: AppModel
    private let menu = NSMenu()
    var onOpenDashboard: () -> Void = {}
    var onOpenSettings: () -> Void = {}
    var onOpenPage: (String) -> Void = { _ in }
    var onCheckForUpdates: (() -> Void)?
    var onRunLoop: (String) -> Void = { _ in }
    var onToggleHidden: () -> Void = {}
    var onModeChange: (DisplayMode) -> Void = { _ in }
    var isHidden: () -> Bool = { false }

    init(model: AppModel) {
        self.model = model
        super.init()
        item.menu = menu
        menu.delegate = self
        update(model.notch)
    }

    func update(_ state: NotchState) {
        guard let button = item.button else { return }
        let img = NSImage(systemSymbolName: state.style.symbol, accessibilityDescription: "LoopX: \(state.title)")
        // Template images follow the menu bar's appearance; attention states are tinted so they stand out.
        switch state.kind {
        case .idle, .running:
            img?.isTemplate = true; button.contentTintColor = nil
        default:
            let cfg = NSImage.SymbolConfiguration(paletteColors: [Self.color(state.style.accent)])
            button.image = img?.withSymbolConfiguration(cfg); img?.isTemplate = false
            button.toolTip = state.title; return
        }
        button.image = img
        button.toolTip = "LoopX – \(state.title)"
    }

    static func color(_ a: NotchAccent) -> NSColor {
        switch a {
        case .neutral: return .secondaryLabelColor
        case .blue: return .systemCyan
        case .orange: return .systemOrange
        case .green: return .systemGreen
        case .red: return .systemRed
        case .purple: return .systemPurple
        }
    }

    @objc private func openDashboard() { onOpenDashboard() }
    @objc private func openSettings() { onOpenSettings() }
    @objc private func openLink(_ s: NSMenuItem) { if let u = s.representedObject as? URL { NSWorkspace.shared.open(u) } }
    @objc private func openPage(_ s: NSMenuItem) { if let p = s.representedObject as? String { onOpenPage(p) } }
    @objc private func checkForUpdates() { onCheckForUpdates?() }
    @objc private func toggleHidden() { onToggleHidden() }
    @objc private func setMode(_ s: NSMenuItem) { if let m = DisplayMode(rawValue: s.representedObject as? String ?? "") { onModeChange(m) } }
    @objc private func quit() { NSApp.terminate(nil) }
}

extension StatusItemController: NSMenuDelegate {
    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        let st = model.notch
        let head = NSMenuItem(title: st.subtitle.map { "\(st.title) – \($0)" } ?? st.title, action: nil, keyEquivalent: "")
        head.isEnabled = false; menu.addItem(head)
        let conn = NSMenuItem(title: model.connection.text, action: nil, keyEquivalent: ""); conn.isEnabled = false
        menu.addItem(conn)
        menu.addItem(.separator())
        for a in st.approvals.prefix(3) {
            let t = a.title ?? a.id
            let allow = NSMenuItem(title: "Allow: \(t)", action: #selector(approve(_:)), keyEquivalent: "")
            allow.representedObject = a.id; allow.target = self; menu.addItem(allow)
            let deny = NSMenuItem(title: "Deny: \(t)", action: #selector(deny(_:)), keyEquivalent: "")
            deny.representedObject = a.id; deny.target = self; menu.addItem(deny)
        }
        if !st.approvals.isEmpty { menu.addItem(.separator()) }
        add("Open Dashboard", #selector(openDashboard), "d")
        let starts = IslandStarts.available(tasks: st.tasks)
        if !starts.isEmpty {
            let run = NSMenu()
            for s in starts {
                let i = NSMenuItem(title: s.title, action: #selector(runLoop(_:)), keyEquivalent: "")
                i.representedObject = s.loop; i.target = self
                i.image = NSImage(systemSymbolName: s.symbol, accessibilityDescription: nil)
                run.addItem(i)
            }
            let host = NSMenuItem(title: "Run", action: nil, keyEquivalent: ""); host.submenu = run
            menu.addItem(host)
        }
        let hide = add(isHidden() ? "Show Notch" : "Hide Notch", #selector(toggleHidden), "")
        hide.isEnabled = model.settings.displayMode != .off
        let modes = NSMenu()
        for (m, t) in [(DisplayMode.notch, "Notch"), (.hud, "Floating HUD"), (.off, "Menu Bar Only")] {
            let i = NSMenuItem(title: t, action: #selector(setMode(_:)), keyEquivalent: "")
            i.representedObject = m.rawValue; i.target = self
            i.state = model.settings.displayMode == m ? .on : .off
            modes.addItem(i)
        }
        let modeItem = NSMenuItem(title: "Display Mode", action: nil, keyEquivalent: ""); modeItem.submenu = modes
        menu.addItem(modeItem)
        for section in DashboardMenu.sections {
            let sub = NSMenu()
            for e in section.entries {
                let i = NSMenuItem(title: e.title, action: #selector(openPage(_:)), keyEquivalent: "")
                i.representedObject = e.path; i.target = self; sub.addItem(i)
            }
            let host = NSMenuItem(title: section.title, action: nil, keyEquivalent: ""); host.submenu = sub
            menu.addItem(host)
        }
        let links = NSMenu()
        for l in DashboardMenu.links {
            let i = NSMenuItem(title: l.title, action: #selector(openLink(_:)), keyEquivalent: "")
            i.representedObject = l.url; i.target = self; links.addItem(i)
        }
        let linksHost = NSMenuItem(title: "GitHub", action: nil, keyEquivalent: ""); linksHost.submenu = links
        menu.addItem(linksHost)
        menu.addItem(.separator())
        add("Settings…", #selector(openSettings), ",")
        if onCheckForUpdates != nil { add("Check for Updates…", #selector(checkForUpdates), "") }
        add("Quit LoopX", #selector(quit), "q")
    }

    @discardableResult
    private func add(_ title: String, _ sel: Selector, _ key: String) -> NSMenuItem {
        let i = NSMenuItem(title: title, action: sel, keyEquivalent: key); i.target = self; menu.addItem(i); return i
    }
    @objc private func runLoop(_ s: NSMenuItem) { if let l = s.representedObject as? String { onRunLoop(l) } }
    @objc private func approve(_ s: NSMenuItem) { if let id = s.representedObject as? String { model.perform(.approve(id: id)) } }
    @objc private func deny(_ s: NSMenuItem) { if let id = s.representedObject as? String { model.perform(.deny(id: id)) } }
}
