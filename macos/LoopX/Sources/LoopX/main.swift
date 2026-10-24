import AppKit

MainActor.assumeIsolated {
    let app = NSApplication.shared
    let delegate = AppDelegate()
    app.delegate = delegate
    if ProcessInfo.processInfo.environment["LOOPX_APP_SELFTEST"] == "1" {
        app.setActivationPolicy(.accessory)
        let ok = delegate.selfTest()
        exit(ok ? 0 : 1)
    }
    app.run()
}
