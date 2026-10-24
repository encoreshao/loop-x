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
    var onChange: () -> Void = {}

    init(store: SettingsStore, login: LaunchAtLogin) {
        self.store = store; self.login = login
        displayMode = store.displayMode; hotkey = store.hotkey; dashboardWindow = store.dashboardWindow; runScheduler = store.runScheduler
        launchAtLogin = login.isEnabled; loginNeedsApproval = login.needsApproval
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
        Form {
            Section("General") {
                Toggle("Launch LoopX at login", isOn: Binding(get: { settings.launchAtLogin }, set: settings.setLaunchAtLogin))
                if settings.loginNeedsApproval {
                    Text("Approve LoopX in System Settings > General > Login Items.").font(.caption).foregroundStyle(.orange)
                }
                if let e = settings.error { Text(e).font(.caption).foregroundStyle(.red) }
                Toggle("Run scheduled loops while LoopX is open", isOn: $settings.runScheduler)
                Text("Checks every 15 minutes and starts any loop that is due. Due loops act on your real GitLab and Slack.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Display") {
                Picker("Dashboard window opens", selection: $settings.dashboardWindow) {
                    Text("Fit the screen").tag(DashboardWindowMode.maximized)
                    Text("Full screen").tag(DashboardWindowMode.fullScreen)
                    Text("Standard window").tag(DashboardWindowMode.standard)
                }
                Picker("Show status as", selection: $settings.displayMode) {
                    Text("Notch").tag(DisplayMode.notch)
                    Text("Floating HUD").tag(DisplayMode.hud)
                    Text("Menu bar only").tag(DisplayMode.off)
                }
                Text("Notch pins to the camera housing and falls back to a draggable HUD on screens without one.")
                    .font(.caption).foregroundStyle(.secondary)
                Picker("Hide / show hotkey", selection: $settings.hotkey) {
                    ForEach(HotkeyPreset.presets) { Text($0.label).tag($0) }
                    Text("None").tag(HotkeyPreset.none)
                }
            }
            Section("Accounts") {
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
            }
            Section("Backend") {
                LabeledContent("Status", value: app.connection.text)
                if let l = app.lock { LabeledContent("Address", value: "127.0.0.1:\(l.port)  (v\(l.version), pid \(l.pid))") }
                if let offer = app.takeoverOffer {
                    Text("A backend v\(offer.runningVersion) is running; this app bundles v\(offer.bundledVersion).").font(.caption)
                    HStack {
                        Button("Replace it") { app.acceptTakeover() }
                        Button("Keep it") { app.declineTakeover() }
                    }
                }
            }
        }
        .formStyle(.grouped)
        .frame(width: 440, height: 640)
        .onAppear {
            settings.refreshAccounts(lock: app.lock)
        }
    }
}
