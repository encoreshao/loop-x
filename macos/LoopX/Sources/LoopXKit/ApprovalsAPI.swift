import Foundation

public protocol HTTPTransport: Sendable {
    func send(_ request: URLRequest) async throws -> (Data, HTTPURLResponse)
}

public struct URLSessionTransport: HTTPTransport {
    let session: URLSession
    public init(session: URLSession = .shared) { self.session = session }
    public func send(_ request: URLRequest) async throws -> (Data, HTTPURLResponse) {
        let (data, resp) = try await session.data(for: request)
        guard let http = resp as? HTTPURLResponse else { throw URLError(.badServerResponse) }
        return (data, http)
    }
}

public enum ApprovalError: Error, Equatable, Sendable {
    case badRequest(String)        // 400
    case forbidden                 // 403: wrong or missing token, foreign Host
    case notFound                  // 404
    case alreadyDecided(String)    // 409: approved / denied / timeout
    case server(Int)
    case unreachable(String)       // transport failure: backend down
    case invalidResponse

    public var message: String {
        switch self {
        case .badRequest(let m): return m.isEmpty ? "The backend rejected the request." : m
        case .forbidden: return "The backend refused the token. Reconnecting may help."
        case .notFound: return "That approval no longer exists."
        case .alreadyDecided(let s): return s.isEmpty ? "Already decided." : "Already decided (\(s))."
        case .server(let c): return "Backend error (HTTP \(c))."
        case .unreachable(let m): return "Cannot reach the backend: \(m)"
        case .invalidResponse: return "Unexpected response from the backend."
        }
    }
}

public enum ApprovalDecision: String, Sendable { case approve, deny }

public struct ApprovalList: Codable, Equatable, Sendable {
    public var pending: [Approval]
    public var recent: [Approval]
}

public struct ApprovalsAPI: Sendable {
    public let baseURL: URL
    public let token: String
    public let decidedBy: String
    let transport: HTTPTransport

    public init(baseURL: URL, token: String, decidedBy: String = "mac-app",
                transport: HTTPTransport = URLSessionTransport()) {
        self.baseURL = baseURL; self.token = token; self.decidedBy = decidedBy; self.transport = transport
    }

    public init(lock: LockFile, decidedBy: String = "mac-app", transport: HTTPTransport = URLSessionTransport()) {
        self.init(baseURL: lock.baseURL, token: lock.token, decidedBy: decidedBy, transport: transport)
    }

    public func list() async throws -> ApprovalList {
        var req = URLRequest(url: baseURL.appendingPathComponent("api/approvals"))
        req.setValue(token, forHTTPHeaderField: "X-LoopX-Token")
        let data = try await perform(req)
        guard let list = try? JSONDecoder().decode(ApprovalList.self, from: data) else { throw ApprovalError.invalidResponse }
        return list
    }

    /// POST /api/approvals/{id}; returns the updated approval.
    @discardableResult
    public func decide(id: String, _ decision: ApprovalDecision) async throws -> Approval {
        // The id is a path component: never let it add path segments.
        guard let enc = id.addingPercentEncoding(withAllowedCharacters: .alphanumerics.union(CharacterSet(charactersIn: "_-.~"))),
              !enc.isEmpty else { throw ApprovalError.badRequest("invalid approval id") }
        guard var comps = URLComponents(url: baseURL, resolvingAgainstBaseURL: false) else { throw ApprovalError.invalidResponse }
        comps.percentEncodedPath = "/api/approvals/\(enc)"
        guard let url = comps.url else { throw ApprovalError.badRequest("invalid approval id") }
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        req.setValue(token, forHTTPHeaderField: "X-LoopX-Token")
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try JSONSerialization.data(withJSONObject: ["decision": decision.rawValue, "by": decidedBy])
        let data = try await perform(req)
        guard let a = try? JSONDecoder().decode(Approval.self, from: data) else { throw ApprovalError.invalidResponse }
        return a
    }

    private func perform(_ req: URLRequest) async throws -> Data {
        let data: Data, http: HTTPURLResponse
        do { (data, http) = try await transport.send(req) }
        catch { throw ApprovalError.unreachable((error as NSError).localizedDescription) }
        switch http.statusCode {
        case 200..<300: return data
        case 400: throw ApprovalError.badRequest(Self.errorText(data))
        case 403: throw ApprovalError.forbidden
        case 404: throw ApprovalError.notFound
        case 409: throw ApprovalError.alreadyDecided(Self.status(data))
        default: throw ApprovalError.server(http.statusCode)
        }
    }

    private static func object(_ data: Data) -> [String: Any]? {
        (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
    }
    private static func errorText(_ data: Data) -> String {
        (object(data)?["error"] as? String) ?? (object(data)?["message"] as? String)
            ?? String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
    }
    /// The decided status of a 409: a `status` field if present, else the tail of the server's
    /// "approval apr_x is already approved" message.
    private static func status(_ data: Data) -> String {
        if let s = object(data)?["status"] as? String { return s }
        let text = errorText(data)
        if let r = text.range(of: "is already ") { return String(text[r.upperBound...]).trimmingCharacters(in: .whitespaces) }
        return ""
    }
}
