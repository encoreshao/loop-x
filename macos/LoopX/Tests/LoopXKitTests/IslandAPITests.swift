import Foundation
import Testing
@testable import LoopXKit

@Suite struct IslandAPITests {
    func api(_ t: FakeTransport) -> IslandAPI {
        IslandAPI(baseURL: URL(string: "http://127.0.0.1:8420")!, token: "tok", transport: t)
    }
    func body(_ r: URLRequest) throws -> [String: String] {
        let raw = try #require(r.httpBody)
        let obj = try JSONSerialization.jsonObject(with: raw) as? [String: String]
        return try #require(obj)
    }

    @Test func startChatPostsTextAndSessionWithToken() async throws {
        let t = FakeTransport { _ in (200, #"{"reply_key":"k1","session":"s1"}"#) }
        let s = try await api(t).startChat(text: " hi ", session: "s0")
        #expect(s == ChatStart(replyKey: "k1", session: "s1"))
        let r = try #require(t.requests.first)
        #expect(r.url?.path == "/api/chat"); #expect(r.httpMethod == "POST")
        #expect(r.value(forHTTPHeaderField: "X-LoopX-Token") == "tok")
        #expect(try body(r) == ["text": "hi", "session": "s0"])
    }

    @Test func blankTextNeverHitsTheNetwork() async {
        let t = FakeTransport { _ in (200, "{}") }
        await #expect(throws: IslandAPIError.blank) { try await api(t).startChat(text: "  ", session: nil) }
        #expect(t.requests.isEmpty)
    }

    @Test func chatApproveMapsExpired() async {
        let t = FakeTransport { _ in (200, #"{"ok":false,"error":"This action expired or was already handled."}"#) }
        await #expect(throws: IslandAPIError.expired("This action expired or was already handled.")) {
            try await api(t).decideChat(id: "p1", session: "s1", approve: true)
        }
        #expect(t.requests.first?.url?.path == "/api/chat/approve")
    }

    @Test func rejectUsesTheRejectPath() async throws {
        let t = FakeTransport { _ in (200, #"{"ok":true}"#) }
        try await api(t).decideChat(id: "p1", session: "s1", approve: false)
        #expect(t.requests.first?.url?.path == "/api/chat/reject")
    }

    @Test func runLoopMapsStatuses() async throws {
        let ok = FakeTransport { _ in (202, #"{"started":true}"#) }
        try await api(ok).runLoop("topic-loop")
        #expect(ok.requests.first?.url?.path == "/api/loops/topic-loop/run")
        await #expect(throws: IslandAPIError.alreadyRunning) { try await api(FakeTransport { _ in (409, "{}") }).runLoop("topic-loop") }
        await #expect(throws: IslandAPIError.unknownLoop) { try await api(FakeTransport { _ in (404, "{}") }).runLoop("x") }
        await #expect(throws: IslandAPIError.forbidden) { try await api(FakeTransport { _ in (403, "{}") }).runLoop("x") }
    }

    @Test func loopNamesCannotAddPathSegments() async {
        let t = FakeTransport { _ in (202, "{}") }
        await #expect(throws: IslandAPIError.unknownLoop) { try await api(t).runLoop("../approvals") }
        #expect(t.requests.isEmpty)
    }

    @Test func streamURLCarriesTheReplyKey() {
        let u = api(FakeTransport { _ in (200, "") }).streamURL(replyKey: "a b")
        #expect(u.absoluteString == "http://127.0.0.1:8420/activity/chat-stream?reply_key=a%20b")
    }
}
