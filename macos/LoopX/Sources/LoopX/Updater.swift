import Foundation
import LoopXKit
import Sparkle

/// Sparkle, started only when Info.plist carries a feed URL and an EdDSA public key
/// (see `UpdateConfig`; the build script fills them from `SPARKLE_FEED_URL` and
/// `SPARKLE_PUBLIC_ED_KEY`). Without them there is no updater and the menu item is hidden.
@MainActor
final class Updater {
    private let controller: SPUStandardUpdaterController?

    var isEnabled: Bool { controller != nil }

    init(bundle: Bundle = .main) {
        if let info = bundle.infoDictionary, UpdateConfig(info: info) != nil {
            controller = SPUStandardUpdaterController(startingUpdater: true, updaterDelegate: nil, userDriverDelegate: nil)
        } else {
            controller = nil
        }
    }

    func checkForUpdates() { controller?.checkForUpdates(nil) }
}
