import Foundation
import Testing
@testable import LoopXKit

final class FakeTransport: HTTPTransport, @unchecked Sendable {
    private let l = NSLock()
    private var _requests: [URLRequest] = []
    let respond: @Sendable (URLRequest) throws -> (Int, String)
    init(_ respond: @escaping @Sendable (URLRequest) throws -> (Int, String)) { self.respond = respond }
    var requests: [URLRequest] { l.lock(); defer { l.unlock() }; return _requests }
    func send(_ request: URLRequest) async throws -> (Data, HTTPURLResponse) {
        l.withLock { _requests.append(request) }
        let (code, body) = try respond(request)
        return (Data(body.utf8), HTTPURLResponse(url: request.url!, statusCode: code, httpVersion: nil, headerFields: nil)!)
    }
}

@Suite struct ApprovalsAPITests {
    static let approved = #"{"id":"apr_1","kind":"open_mr","status":"approved","decided_by":"mac-app","decided_at":"t"}"#
    func api(_ t: FakeTransport) -> ApprovalsAPI {
        ApprovalsAPI(baseURL: URL(string: "http://127.0.0.1:8420")!, token: "tok", transport: t)
    }

    @Test func postsDecisionWithTokenHeaderAndBody() async throws {
        let t = FakeTransport { _ in (200, Self.approved) }
        let a = try await api(t).decide(id: "apr_1", .approve)
        #expect(a.status == "approved")
        let r = try #require(t.requests.first)
        #expect(r.httpMethod == "POST")
        #expect(r.url?.absoluteString == "http://127.0.0.1:8420/api/approvals/apr_1")
        #expect(r.value(forHTTPHeaderField: "X-LoopX-Token") == "tok")
        #expect(r.value(forHTTPHeaderField: "Content-Type") == "application/json")
        let raw = try #require(r.httpBody)
        let body = try #require(try JSONSerialization.jsonObject(with: raw) as? [String: String])
        #expect(body == ["decision": "approve", "by": "mac-app"])
    }

    @Test func denySendsDeny() async throws {
        let t = FakeTransport { _ in (200, Self.approved) }
        _ = try await api(t).decide(id: "apr_1", .deny)
        let raw = try #require(t.requests.first?.httpBody)
        let body = try JSONSerialization.jsonObject(with: raw) as? [String: String]
        #expect(body?["decision"] == "deny")
    }

    @Test(arguments: [
        (400, #"{"error":"bad decision"}"#, ApprovalError.badRequest("bad decision")),
        (403, "", ApprovalError.forbidden),
        (404, "", ApprovalError.notFound),
        (409, #"{"status":"approved"}"#, ApprovalError.alreadyDecided("approved")),
        (409, #"{"status":"timeout"}"#, ApprovalError.alreadyDecided("timeout")),
        (500, "", ApprovalError.server(500)),
        (200, "garbage", ApprovalError.invalidResponse),
    ])
    func mapsStatusCodes(code: Int, body: String, expected: ApprovalError) async {
        let t = FakeTransport { _ in (code, body) }
        do { _ = try await api(t).decide(id: "apr_1", .approve); Issue.record("expected \(expected)") }
        catch { #expect(error as? ApprovalError == expected) }
    }

    @Test func transportFailureBecomesUnreachable() async {
        let t = FakeTransport { _ in throw URLError(.cannotConnectToHost) }
        do { _ = try await api(t).decide(id: "apr_1", .approve); Issue.record("expected failure") }
        catch {
            guard case .unreachable = error as? ApprovalError else { Issue.record("wrong error \(error)"); return }
        }
    }

    @Test func idCannotInjectPathSegments() async throws {
        let t = FakeTransport { _ in (200, Self.approved) }
        _ = try await api(t).decide(id: "../../etc/x?y=1", .approve)
        let path = try #require(t.requests.first?.url?.absoluteString)
        #expect(path == "http://127.0.0.1:8420/api/approvals/..%2F..%2Fetc%2Fx%3Fy%3D1")
        do { _ = try await api(t).decide(id: "", .approve); Issue.record("empty id accepted") }
        catch { #expect(error as? ApprovalError == .badRequest("invalid approval id")) }
    }

    @Test func listsPendingAndRecent() async throws {
        let t = FakeTransport { _ in (200, #"{"pending":[{"id":"a","kind":"open_mr","status":"pending"}],"recent":[],"escalations":[]}"#) }
        let l = try await api(t).list()
        #expect(l.pending.map(\.id) == ["a"])
        #expect(t.requests.first?.httpMethod == nil || t.requests.first?.httpMethod == "GET")
        #expect(t.requests.first?.value(forHTTPHeaderField: "X-LoopX-Token") == "tok")
    }

    @Test func messagesAreHumanReadable() {
        #expect(ApprovalError.alreadyDecided("denied").message == "Already decided (denied).")
        #expect(ApprovalError.notFound.message.contains("no longer exists"))
        #expect(!ApprovalError.forbidden.message.isEmpty)
    }

    @Test func buildsFromALockFile() {
        let lock = LockFile(pid: 1, port: 9999, version: "0.4.0", token: "zz")
        let a = ApprovalsAPI(lock: lock)
        #expect(a.baseURL.absoluteString == "http://127.0.0.1:9999")
        #expect(a.token == "zz")
    }
}
