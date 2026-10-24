import Foundation

/// Sparkle settings read from the app's Info.plist. Updates are on only when both
/// the feed and the EdDSA public key are set, so an unsigned dev or fork build
/// never starts an updater it cannot verify.
public struct UpdateConfig: Equatable, Sendable {
    public let feedURL: URL
    public let publicKey: String

    public init?(info: [String: Any]) {
        func text(_ k: String) -> String? {
            let v = (info[k] as? String)?.trimmingCharacters(in: .whitespacesAndNewlines)
            return v?.isEmpty == false ? v : nil
        }
        guard let feed = text("SUFeedURL"), let url = URL(string: feed), url.scheme == "https",
              let key = text("SUPublicEDKey"),
              Data(base64Encoded: key)?.count == 32   // an Ed25519 public key
        else { return nil }
        feedURL = url; publicKey = key
    }
}
