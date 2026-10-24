import AppKit
import AuthenticationServices
import LoopXKit

/// Drives one provider sign-in through `ASWebAuthenticationSession`: the backend
/// starts the flow, the session shows the provider's page, and the backend's
/// callback page hands back to `loopx-auth://` once the code is exchanged. The
/// URL only means "finished"; the outcome is confirmed with `status`.
@MainActor
final class OAuthSession: NSObject, ASWebAuthenticationPresentationContextProviding {
    private var session: ASWebAuthenticationSession?

    func signIn(_ provider: OAuthProvider, api: OAuthAPI) async throws {
        let start = try await api.start(provider)
        let callback = try await run(start.authorizationURL, scheme: api.appScheme)
        if callback.ok == false, !callback.message.isEmpty { throw OAuthError.failed(callback.message) }
        let status = try await api.status(provider, state: start.state)
        guard status.flowState == "done" || status.connected else {
            throw OAuthError.failed(status.message)
        }
    }

    private func run(_ url: URL, scheme: String) async throws -> OAuthCallback {
        try await withCheckedThrowingContinuation { cont in
            let s = ASWebAuthenticationSession(url: url, callbackURLScheme: scheme) { callbackURL, error in
                if let error {
                    let cancelled = (error as? ASWebAuthenticationSessionError)?.code == .canceledLogin
                    cont.resume(throwing: cancelled ? OAuthError.cancelled : OAuthError.failed(error.localizedDescription))
                } else if let callbackURL, let cb = OAuthCallback.parse(callbackURL, scheme: scheme) {
                    cont.resume(returning: cb)
                } else {
                    cont.resume(throwing: OAuthError.failed(""))
                }
            }
            s.presentationContextProvider = self
            s.prefersEphemeralWebBrowserSession = false
            session = s
            if !s.start() { cont.resume(throwing: OAuthError.failed("Could not open the sign-in window.")) }
        }
    }

    func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        NSApp.keyWindow ?? NSApp.windows.first ?? ASPresentationAnchor()
    }
}
