import SwiftUI
import AppKit
import LoopXKit

@MainActor
final class SettingsModel: ObservableObject {
    @Published var displayMode: DisplayMode { didSet { store.displayMode = displayMode; onChange() } }
    @Published var dashboardWindow: DashboardWindowMode { didSet { store.dashboardWindow = dashboardWindow } }
    @Published var hotkey: HotkeyPreset { didSet { store.hotkey = hotkey; onChange() } }
    @Published var runScheduler: Bool { didSet { store.runScheduler = runScheduler; onChange() } }
    @Published var notifyTasks: Bool { didSet { store.notifyTasks = notifyTasks } }
    @Published var launchAtLogin: Bool
    @Published var loginNeedsApproval: Bool
    @Published var error: String?
    @Published var accountStatus: [OAuthProvider: String] = [:]
    @Published var signingIn: OAuthProvider?
    private let oauth = OAuthSession()
    private let store: SettingsStore
    private let login: LaunchAtLogin
    @Published var cliStatus: CLIInstaller.Status = .notInstalled
    @Published var tabIndex = 0
    @Published var accent: Accent
    @Published var colorMode: ColorMode
    var onThemeChange: () -> Void = {}
    var cli: CLIInstaller?
    var onChange: () -> Void = {}
    var onOpenPage: (String) -> Void = { _ in }
    var onCheckForUpdates: (() -> Void)?
    let appVersion = (Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String) ?? "dev"
    var bundledLoopxVersion = "–"
    var loopxBinaryPath: String? { cli?.binary }
    var dataDirectory: URL {
        ProcessInfo.processInfo.environment["LOOP_ENGINEERING_HOME"].map { URL(fileURLWithPath: $0) }
            ?? FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".loopx")
    }
    func reveal(_ url: URL) { NSWorkspace.shared.activateFileViewerSelecting([url]) }
    func open(_ url: URL) { NSWorkspace.shared.open(url) }

    init(store: SettingsStore, login: LaunchAtLogin) {
        self.store = store; self.login = login
        displayMode = store.displayMode; hotkey = store.hotkey; dashboardWindow = store.dashboardWindow; runScheduler = store.runScheduler; notifyTasks = store.notifyTasks
        launchAtLogin = login.isEnabled; loginNeedsApproval = login.needsApproval
        accent = store.accent; colorMode = store.colorMode
    }

    /// Follow the dashboard's own Appearance choice (accent and light/dark/auto).
    func applyTheme(accent raw: String?, colorMode rawMode: String?) {
        accent = Accent(stored: raw); colorMode = ColorMode(stored: rawMode)
        store.accent = accent; store.colorMode = colorMode
        onThemeChange()
    }

    func refreshCLI() { cliStatus = cli?.status() ?? .notInstalled }

    /// Links the bundled `loopx` into /usr/local/bin; macOS asks for the administrator password.
    func installCLI() {
        guard let cli else { return }
        cliStatus = cli.install(run: { Self.runAsAdministrator($0) })
        if cliStatus != .installed { error = "Could not install the loopx command." } else { error = nil }
    }

    static func runAsAdministrator(_ command: String) -> Bool {
        let escaped = command.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"")
        var err: NSDictionary?
        NSAppleScript(source: "do shell script \"\(escaped)\" with administrator privileges")?.executeAndReturnError(&err)
        return err == nil
    }

    func signIn(_ provider: OAuthProvider, lock: LockFile?) {
        guard let lock else { accountStatus[provider] = "Backend is not running."; return }
        signingIn = provider
        Task {
            defer { signingIn = nil }
            do {
                try await oauth.signIn(provider, api: OAuthAPI(lock: lock))
                accountStatus[provider] = "Connected"
            } catch let e as OAuthError {
                accountStatus[provider] = e == .cancelled ? nil : e.message
            } catch { accountStatus[provider] = error.localizedDescription }
        }
    }

    func refreshAccounts(lock: LockFile?) {
        guard let lock else { return }
        Task {
            for p in OAuthProvider.allCases {
                if let s = try? await OAuthAPI(lock: lock).status(p) {
                    accountStatus[p] = s.connected ? (s.expired ? "Expired" : "Connected") : nil
                }
            }
        }
    }

    func setLaunchAtLogin(_ on: Bool) {
        do { try login.setEnabled(on); error = nil }
        catch { self.error = error.localizedDescription }
        launchAtLogin = login.isEnabled; loginNeedsApproval = login.needsApproval
    }
}

struct SettingsView: View {
    @ObservedObject var settings: SettingsModel
    @ObservedObject var app: AppModel
    private var tab: Tab { Tab.allCases[min(settings.tabIndex, Tab.allCases.count - 1)] }

    enum Tab: String, CaseIterable, Identifiable {
        case general = "General", display = "Display", accounts = "Accounts"
        case commandLine = "Command Line", backend = "Backend", about = "About"
        var id: String { rawValue }
        var icon: String {
            switch self {
            case .general: return "gearshape"
            case .display: return "rectangle.topthird.inset.filled"
            case .accounts: return "person.crop.circle"
            case .commandLine: return "terminal"
            case .backend: return "server.rack"
            case .about: return "info.circle"
            }
        }
    }

    var body: some View {
        HStack(spacing: 0) {
            sidebar
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    Text(tab.rawValue).font(.system(size: 22, weight: .medium)).foregroundStyle(MD.onSurface)
                    page
                }
                .padding(24)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .background(MD.surface)
        }
        .environment(\.accent, settings.accent)
        .frame(width: 760, height: 520)
        .tint(MD.primary)
        .onAppear {
            settings.refreshAccounts(lock: app.lock)
            settings.refreshCLI()
        }
    }

    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Settings").font(.system(size: 12, weight: .medium)).foregroundStyle(MD.navOn(settings.accent))
                .padding(.horizontal, 12).padding(.bottom, 6)
            ForEach(Tab.allCases) { t in
                Button { settings.tabIndex = Tab.allCases.firstIndex(of: t) ?? 0 } label: {
                    HStack(spacing: 10) {
                        Image(systemName: t.icon).frame(width: 18)
                        Text(t.rawValue)
                        Spacer(minLength: 0)
                    }
                    .font(.system(size: 13, weight: tab == t ? .medium : .regular))
                    .foregroundStyle(tab == t ? MD.navActiveOn(settings.accent) : MD.navOn(settings.accent))
                    .padding(.horizontal, 12).padding(.vertical, 8)
                    .background(tab == t ? MD.navActive(settings.accent) : .clear, in: Capsule())
                    .contentShape(Capsule())
                }.buttonStyle(.plain).focusEffectDisabled()
            }
            Spacer()
        }
        .padding(.top, 20).padding(.horizontal, 12)
        .frame(width: 190)
        .frame(maxHeight: .infinity)
        .background(MD.sidebar(settings.accent))
    }

    @ViewBuilder private var page: some View {
        switch tab {
        case .general: general
        case .display: display
        case .accounts: accounts
        case .commandLine: commandLine
        case .backend: backend
        case .about: about
        }
    }

    private var general: some View {
        VStack(alignment: .leading, spacing: 16) {
            MDCard("Startup") {
                Toggle("Launch LoopX at login", isOn: Binding(get: { settings.launchAtLogin }, set: settings.setLaunchAtLogin))
                if settings.loginNeedsApproval {
                    Text("Approve LoopX in System Settings > General > Login Items.").font(.system(size: 12)).foregroundStyle(MD.warning)
                }
                if let e = settings.error { Text(e).font(.system(size: 12)).foregroundStyle(MD.error) }
                MDNote("The dashboard window opens when you start LoopX yourself, but stays closed when it starts at login.")
            }
            MDCard("Scheduled loops") {
                Toggle("Run scheduled loops while LoopX is open", isOn: $settings.runScheduler)
                MDNote("Checks every 15 minutes and starts any loop that is due. Due loops act on your real GitLab and Slack, so this is off until you turn it on. Each loop's own schedule is under Loops in the dashboard.")
            }
            MDCard("Notifications") {
                Toggle("Notify when a task starts, completes or fails", isOn: $settings.notifyTasks)
                MDNote("Posts to the macOS Notification Center, including when an approval is waiting for you.")
            }
            MDCard("Dashboard window") {
                Picker("Opens", selection: $settings.dashboardWindow) {
                    Text("Fit the screen").tag(DashboardWindowMode.maximized)
                    Text("Full screen").tag(DashboardWindowMode.fullScreen)
                    Text("Standard window").tag(DashboardWindowMode.standard)
                }
                MDNote("Applies the first time the window opens after LoopX starts; after that your own sizing is kept.")
            }
        }
    }

    private var display: some View {
        VStack(alignment: .leading, spacing: 16) {
            MDCard("Status display") {
                Picker("Show status as", selection: $settings.displayMode) {
                    Text("Notch").tag(DisplayMode.notch)
                    Text("Floating HUD").tag(DisplayMode.hud)
                    Text("Menu bar only").tag(DisplayMode.off)
                }
                MDNote("Notch pins to the camera housing and falls back to a draggable HUD on screens without one. The menu bar icon is always there.")
            }
            MDCard("Hotkey") {
                Picker("Hide / show", selection: $settings.hotkey) {
                    ForEach(HotkeyPreset.presets) { Text($0.label).tag($0) }
                    Text("None").tag(HotkeyPreset.none)
                }
                MDNote("A global shortcut that hides or shows the notch or HUD from any app.")
            }
        }
    }

    private var accounts: some View {
        VStack(alignment: .leading, spacing: 16) {
            MDCard("Sign-ins") {
                ForEach(OAuthProvider.allCases, id: \.self) { p in
                    HStack {
                        Text(p.title).font(.system(size: 13)).foregroundStyle(MD.onSurface)
                        Spacer()
                        if let t = settings.accountStatus[p] {
                            Text(t).font(.system(size: 12)).foregroundStyle(t == "Connected" ? MD.success : MD.onSurfaceVariant)
                        }
                        Button(settings.accountStatus[p] == "Connected" ? "Reconnect" : "Connect") {
                            settings.signIn(p, lock: app.lock)
                        }.buttonStyle(MDButtonStyle()).disabled(settings.signingIn != nil)
                    }
                }
                MDNote("Tokens are kept in the macOS Keychain, never in plain files. GitLab, Slack and other connectors are managed in the dashboard under System → Connectors.")
            }
            Button("Open Connectors in Dashboard") { settings.onOpenPage("/connectors") }.buttonStyle(MDButtonStyle())
        }
    }

    private var commandLine: some View {
        MDCard("loopx command") {
            MDRow(label: "Status", value: settings.cliStatus.summary)
            MDRow(label: "Link", value: CLIInstaller.defaultLinkPath)
            if let b = settings.loopxBinaryPath { MDRow(label: "Points to", value: b) }
            switch settings.cliStatus {
            case .notInstalled, .outdated:
                Button(settings.cliStatus == .outdated ? "Update Command Line Tool" : "Install Command Line Tool") { settings.installCLI() }
                    .buttonStyle(MDButtonStyle(filled: true))
            case .installed, .conflict: EmptyView()
            }
            if let e = settings.error { Text(e).font(.system(size: 12)).foregroundStyle(MD.error) }
            MDNote("Lets you run loopx from Terminal, for example loopx doctor. Installing links the copy inside LoopX.app and asks for your administrator password once. A loopx from Homebrew or the script installer is never replaced.")
        }
    }

    private var backend: some View {
        VStack(alignment: .leading, spacing: 16) {
            MDCard("Backend") {
                MDRow(label: "Status", value: app.connection.text)
                if let l = app.lock {
                    MDRow(label: "Address", value: "127.0.0.1:\(l.port)")
                    MDRow(label: "Version", value: "v\(l.version)")
                    MDRow(label: "Process", value: "pid \(l.pid)")
                }
                MDRow(label: "Bundled in this app", value: "v\(settings.bundledLoopxVersion)")
                if let offer = app.takeoverOffer {
                    MDNote("A backend v\(offer.runningVersion) is running; this app bundles v\(offer.bundledVersion).")
                    HStack {
                        Button("Replace it") { app.acceptTakeover() }.buttonStyle(MDButtonStyle(filled: true))
                        Button("Keep it") { app.declineTakeover() }.buttonStyle(MDButtonStyle())
                    }
                }
            }
            MDCard("Files") {
                MDRow(label: "Data folder", value: settings.dataDirectory.path)
                HStack {
                    Button("Reveal in Finder") { settings.reveal(settings.dataDirectory) }.buttonStyle(MDButtonStyle())
                    Button("Open Logs") { settings.onOpenPage("/runs?view=logs") }.buttonStyle(MDButtonStyle())
                }
                MDNote("Your config, run history and logs live in the data folder. The backend keeps running when you quit LoopX; it is its own background agent.")
            }
        }
    }

    private var about: some View {
        VStack(alignment: .leading, spacing: 16) {
            MDCard("LoopX") {
                MDRow(label: "App version", value: settings.appVersion)
                MDRow(label: "loopx version", value: settings.bundledLoopxVersion)
                MDRow(label: "Author", value: "Encore Shao")
                if let check = settings.onCheckForUpdates {
                    Button("Check for Updates…", action: check).buttonStyle(MDButtonStyle(filled: true))
                } else {
                    MDNote("Updates: download the newest LoopX DMG from GitHub Releases, or run brew upgrade loopx.")
                }
            }
            MDCard("Links") {
                HStack {
                    ForEach(DashboardMenu.links, id: \.title) { l in
                        Button(l.title) { settings.open(l.url) }.buttonStyle(MDButtonStyle())
                    }
                }
                Button("encore.shao@gmail.com") { settings.open(URL(string: "mailto:encore.shao@gmail.com")!) }.buttonStyle(MDButtonStyle())
                MDNote("Created by Encore Shao. MIT licensed.")
            }
        }
    }
}
