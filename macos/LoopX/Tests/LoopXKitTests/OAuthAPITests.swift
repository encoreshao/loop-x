import Foundation
import Testing
@testable import LoopXKit

@Suite struct OAuthAPITests {
    func api(_ t: FakeTransport) -> OAuthAPI {
        OAuthAPI(baseURL: URL(string: "http://127.0.0.1:8420")!, token: "tok", transport: t)
    }

    @Test func startPostsSchemeAndTokenAndParsesTheUrl() async throws {
        let t = FakeTransport { _ in (200, #"{"authorization_url":"https://accounts.google.com/o/oauth2/v2/auth?x=1","state":"S1","redirect_uri":"r"}"#) }
        let s = try await api(t).start(.google, account: "cal")
        #expect(s.state == "S1")
        #expect(s.authorizationURL.host == "accounts.google.com")
        let r = try #require(t.requests.first)
        #expect(r.httpMethod == "POST")
        #expect(r.url?.path == "/api/oauth/google/start")
        #expect(r.value(forHTTPHeaderField: "X-LoopX-Token") == "tok")
        let raw = try #require(r.httpBody)
        let body = try #require(try JSONSerialization.jsonObject(with: raw) as? [String: String])
        #expect(body == ["app_scheme": "loopx-auth", "account": "cal"])
    }

    @Test func startRejectsANonHttpAuthorizationUrl() async {
        let t = FakeTransport { _ in (200, #"{"authorization_url":"file:///etc/passwd","state":"S"}"#) }
        await #expect(throws: OAuthError.api(.invalidResponse)) { try await api(t).start(.linkedin) }
    }

    @Test func statusReadsFlowAndConnection() async throws {
        let t = FakeTransport { _ in (200, #"{"connected":true,"expired":false,"flow":{"state":"done","message":"ok"}}"#) }
        let s = try await api(t).status(.google, account: "a", state: "S1")
        #expect(s == OAuthStatus(connected: true, expired: false, flowState: "done", message: "ok"))
        #expect(t.requests.first?.url?.query?.contains("state=S1") == true)
    }

    @Test func errorsMap() async {
        let bad = FakeTransport { _ in (400, #"{"error":"no client id"}"#) }
        await #expect(throws: OAuthError.failed("no client id")) { try await api(bad).start(.google) }
        let forbidden = FakeTransport { _ in (403, "{}") }
        await #expect(throws: OAuthError.api(.forbidden)) { try await api(forbidden).logout(.google) }
    }

    @Test func callbackParsesOnlyOurScheme() {
        let ok = URL(string: "loopx-auth://oauth?provider=google&status=ok&account=cal")!
        #expect(OAuthCallback.parse(ok, scheme: "loopx-auth") == OAuthCallback(ok: true, message: ""))
        let err = URL(string: "loopx-auth://oauth?status=error&message=denied")!
        #expect(OAuthCallback.parse(err, scheme: "loopx-auth") == OAuthCallback(ok: false, message: "denied"))
        #expect(OAuthCallback.parse(URL(string: "https://evil/?status=ok")!, scheme: "loopx-auth") == nil)
    }
}
