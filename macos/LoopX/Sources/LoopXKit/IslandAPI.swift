import Foundation

public struct ChatStart: Equatable, Sendable {
    public var replyKey: String
    public var session: String
}

public enum IslandAPIError: Error, Equatable, Sendable {
    case blank, alreadyRunning, unknownLoop, forbidden
    case expired(String)
    case server(Int)
    case unreachable(String)
    case invalid

    public var message: String {
        switch self {
        case .blank: return "Type a message first."
        case .alreadyRunning: return "Already running."
        case .unknownLoop: return "That loop isn't available."
        case .forbidden: return "The backend refused the token. Reconnecting may help."
        case .expired(let m): return m.isEmpty ? "This action expired or was already handled." : m
        case .server(let c): return "Backend error (HTTP \(c))."
        case .unreachable(let m): return "Cannot reach the backend: \(m)"
        case .invalid: return "Unexpected response from the backend."
        }
    }
}

/// The island's token-authenticated calls: start a chat reply, decide its approval card, start a loop.
public struct IslandAPI: Sendable {
    public let baseURL: URL
    public let token: String
    let transport: HTTPTransport

    public init(baseURL: URL, token: String, transport: HTTPTransport = URLSessionTransport()) {
        self.baseURL = baseURL; self.token = token; self.transport = transport
    }
    public init(lock: LockFile, transport: HTTPTransport = URLSessionTransport()) {
        self.init(baseURL: lock.baseURL, token: lock.token, transport: transport)
    }

    public func startChat(text: String, session: String?) async throws -> ChatStart {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { throw IslandAPIError.blank }
        var payload = ["text": trimmed]
        if let session { payload["session"] = session }
        let obj = try await post("/api/chat", payload)
        guard let key = obj["reply_key"] as? String, let sid = obj["session"] as? String else { throw IslandAPIError.invalid }
        return ChatStart(replyKey: key, session: sid)
    }

    public func decideChat(id: String, session: String, approve: Bool) async throws {
        let obj = try await post(approve ? "/api/chat/approve" : "/api/chat/reject", ["id": id, "session": session])
        if obj["ok"] as? Bool != true { throw IslandAPIError.expired((obj["error"] as? String) ?? "") }
    }

    public func runLoop(_ name: String) async throws {
        guard !name.isEmpty, name.allSatisfy({ $0.isLetter || $0.isNumber || $0 == "-" || $0 == "_" }) else {
            throw IslandAPIError.unknownLoop
        }
        _ = try await post("/api/loops/\(name)/run", [:])
    }

    public func streamURL(replyKey: String) -> URL {
        var c = URLComponents(url: baseURL, resolvingAgainstBaseURL: false)!
        c.path = "/activity/chat-stream"
        c.percentEncodedQuery = "reply_key=" + (replyKey.addingPercentEncoding(withAllowedCharacters: .alphanumerics.union(CharacterSet(charactersIn: "-_.~"))) ?? "")
        return c.url!
    }

    private func post(_ path: String, _ payload: [String: String]) async throws -> [String: Any] {
        var req = URLRequest(url: baseURL.appendingPathComponent(path))
        req.httpMethod = "POST"
        req.setValue(token, forHTTPHeaderField: "X-LoopX-Token")
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try JSONSerialization.data(withJSONObject: payload)
        let data: Data, http: HTTPURLResponse
        do { (data, http) = try await transport.send(req) }
        catch { throw IslandAPIError.unreachable((error as NSError).localizedDescription) }
        switch http.statusCode {
        case 200..<300: return ((try? JSONSerialization.jsonObject(with: data)) as? [String: Any]) ?? [:]
        case 400: throw IslandAPIError.blank
        case 403: throw IslandAPIError.forbidden
        case 404: throw IslandAPIError.unknownLoop
        case 409: throw IslandAPIError.alreadyRunning
        default: throw IslandAPIError.server(http.statusCode)
        }
    }
}
