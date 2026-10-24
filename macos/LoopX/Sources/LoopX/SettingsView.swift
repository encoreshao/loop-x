import SwiftUI
import AppKit
import LoopXKit

@MainActor
final class SettingsModel: ObservableObject {
    @Published var displayMode: DisplayMode { didSet { store.displayMode = displayMode; onChange() } }
    @Published var dashboardWindow: DashboardWindowMode { didSet { store.dashboardWindow = dashboardWindow } }
    @Published var hotkey: HotkeyPreset { didSet { store.hotkey = hotkey; onChange() } }
    @Published var runScheduler: Bool { didSet { store.runScheduler = runScheduler; onChange() } }
    @Published var launchAtLogin: Bool
    @Published var loginNeedsApproval: Bool
    @Published var error: String?
    @Published var accountStatus: [OAuthProvider: String] = [:]
    @Published var signingIn: OAuthProvider?
    private let oauth = OAuthSession()
    private let store: SettingsStore
    private let login: LaunchAtLogin
    @Published var cliStatus: CLIInstaller.Status = .notInstalled
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
        displayMode = store.displayMode; hotkey = store.hotkey; dashboardWindow = store.dashboardWindow; runScheduler = store.runScheduler
        launchAtLogin = login.isEnabled; loginNeedsApproval = login.needsApproval
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

    var body: some View {
        TabView {
            general.tabItem { Label("General", systemImage: "gearshape") }
            display.tabItem { Label("Display", systemImage: "rectangle.topthird.inset.filled") }
            accounts.tabItem { Label("Accounts", systemImage: "person.crop.circle") }
            commandLine.tabItem { Label("Command Line", systemImage: "terminal") }
            backend.tabItem { Label("Backend", systemImage: "server.rack") }
            about.tabItem { Label("About", systemImage: "info.circle") }
        }
        .frame(width: 560, height: 460)
        .onAppear {
            settings.refreshAccounts(lock: app.lock)
            settings.refreshCLI()
        }
    }

    private func note(_ text: String) -> some View { Text(text).font(.caption).foregroundStyle(.secondary) }

    private var general: some View {
        Form {
            Section("Startup") {
                Toggle("Launch LoopX at login", isOn: Binding(get: { settings.launchAtLogin }, set: settings.setLaunchAtLogin))
                if settings.loginNeedsApproval {
                    Text("Approve LoopX in System Settings > General > Login Items.").font(.caption).foregroundStyle(.orange)
                }
                if let e = settings.error { Text(e).font(.caption).foregroundStyle(.red) }
                note("The dashboard window opens when you start LoopX yourself, but stays closed when it starts at login.")
            }
            Section("Scheduled loops") {
                Toggle("Run scheduled loops while LoopX is open", isOn: $settings.runScheduler)
                note("Checks every 15 minutes and starts any loop that is due. Due loops act on your real GitLab and Slack, so this is off until you turn it on. Each loop's own schedule is under Loops in the dashboard.")
            }
            Section("Dashboard window") {
                Picker("Opens", selection: $settings.dashboardWindow) {
                    Text("Fit the screen").tag(DashboardWindowMode.maximized)
                    Text("Full screen").tag(DashboardWindowMode.fullScreen)
                    Text("Standard window").tag(DashboardWindowMode.standard)
                }
                note("Applies the first time the window opens after LoopX starts; after that your own sizing is kept.")
            }
        }.formStyle(.grouped)
    }

    private var display: some View {
        Form {
            Section("Status display") {
                Picker("Show status as", selection: $settings.displayMode) {
                    Text("Notch").tag(DisplayMode.notch)
                    Text("Floating HUD").tag(DisplayMode.hud)
                    Text("Menu bar only").tag(DisplayMode.off)
                }
                note("Notch pins to the camera housing and falls back to a draggable HUD on screens without one. The menu bar icon is always there.")
            }
            Section("Hotkey") {
                Picker("Hide / show", selection: $settings.hotkey) {
                    ForEach(HotkeyPreset.presets) { Text($0.label).tag($0) }
                    Text("None").tag(HotkeyPreset.none)
                }
                note("A global shortcut that hides or shows the notch or HUD from any app.")
            }
        }.formStyle(.grouped)
    }

    private var accounts: some View {
        Form {
            Section("Sign-ins") {
                ForEach(OAuthProvider.allCases, id: \.self) { p in
                    LabeledContent(p.title) {
                        HStack {
                            if let t = settings.accountStatus[p] { Text(t).font(.caption).foregroundStyle(.secondary) }
                            Button(settings.accountStatus[p] == "Connected" ? "Reconnect" : "Connect") {
                                settings.signIn(p, lock: app.lock)
                            }.disabled(settings.signingIn != nil)
                        }
                    }
                }
                note("Tokens are kept in the macOS Keychain, never in plain files. GitLab, Slack and other connectors are managed in the dashboard under System → Connectors.")
            }
            Section {
                Button("Open Connectors in Dashboard") { settings.onOpenPage("/connectors") }
            }
        }.formStyle(.grouped)
    }

    private var commandLine: some View {
        Form {
            Section("loopx command") {
                LabeledContent("Status", value: settings.cliStatus.summary)
                LabeledContent("Link", value: CLIInstaller.defaultLinkPath)
                if let b = settings.loopxBinaryPath { LabeledContent("Points to", value: b) }
                switch settings.cliStatus {
                case .notInstalled, .outdated:
                    Button(settings.cliStatus == .outdated ? "Update Command Line Tool" : "Install Command Line Tool") { settings.installCLI() }
                case .installed, .conflict: EmptyView()
                }
                if let e = settings.error { Text(e).font(.caption).foregroundStyle(.red) }
                note("Lets you run loopx from Terminal, for example loopx doctor. Installing links the copy inside LoopX.app and asks for your administrator password once. A loopx from Homebrew or the script installer is never replaced.")
            }
        }.formStyle(.grouped)
    }

    private var backend: some View {
        Form {
            Section("Backend") {
                LabeledContent("Status", value: app.connection.text)
                if let l = app.lock {
                    LabeledContent("Address", value: "127.0.0.1:\(l.port)")
                    LabeledContent("Version", value: "v\(l.version)")
                    LabeledContent("Process", value: "pid \(l.pid)")
                }
                LabeledContent("Bundled in this app", value: "v\(settings.bundledLoopxVersion)")
                if let offer = app.takeoverOffer {
                    Text("A backend v\(offer.runningVersion) is running; this app bundles v\(offer.bundledVersion).").font(.caption)
                    HStack {
                        Button("Replace it") { app.acceptTakeover() }
                        Button("Keep it") { app.declineTakeover() }
                    }
                }
            }
            Section("Files") {
                LabeledContent("Data folder", value: settings.dataDirectory.path)
                HStack {
                    Button("Reveal in Finder") { settings.reveal(settings.dataDirectory) }
                    Button("Open Logs") { settings.onOpenPage("/runs?view=logs") }
                }
                note("Your config, run history and logs live in the data folder. The backend keeps running when you quit LoopX; it is its own background agent.")
            }
        }.formStyle(.grouped)
    }

    private var about: some View {
        Form {
            Section("LoopX") {
                LabeledContent("App version", value: settings.appVersion)
                LabeledContent("loopx version", value: settings.bundledLoopxVersion)
                if let check = settings.onCheckForUpdates { Button("Check for Updates…", action: check) }
                else { note("Updates: download the newest LoopX DMG from GitHub Releases, or run brew upgrade loopx.") }
            }
            Section("Links") {
                ForEach(DashboardMenu.links, id: \.title) { l in
                    Link(l.title, destination: l.url)
                }
                note("MIT licensed.")
            }
        }.formStyle(.grouped)
    }
}
