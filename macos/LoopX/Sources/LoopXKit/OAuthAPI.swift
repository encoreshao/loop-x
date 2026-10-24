import Foundation

public enum OAuthProvider: String, CaseIterable, Sendable {
    case google, linkedin
    public var title: String { self == .google ? "Google" : "LinkedIn" }
}

public enum OAuthError: Error, Equatable, Sendable {
    case cancelled
    case failed(String)
    case api(ApprovalError)

    public var message: String {
        switch self {
        case .cancelled: return "Sign-in was cancelled."
        case .failed(let m): return m.isEmpty ? "Sign-in failed." : m
        case .api(let e): return e.message
        }
    }
}

public struct OAuthStart: Equatable, Sendable {
    public let authorizationURL: URL
    public let state: String
}

public struct OAuthStatus: Equatable, Sendable {
    public let connected: Bool
    public let expired: Bool
    public let flowState: String   // pending | done | failed | unknown
    public let message: String
}

/// The result of the in-session hand-off: the success/failure page navigates to
/// `<scheme>://oauth?provider=..&status=ok|error&account=..&message=..`.
public struct OAuthCallback: Equatable, Sendable {
    public let ok: Bool
    public let message: String

    /// Only the scheme and `status` are read; the app confirms the real outcome
    /// with `status` before trusting it.
    public static func parse(_ url: URL, scheme: String) -> OAuthCallback? {
        guard url.scheme?.lowercased() == scheme.lowercased(),
              let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems
        else { return nil }
        let q = Dictionary(items.compactMap { i in i.value.map { (i.name, $0) } }, uniquingKeysWith: { a, _ in a })
        return OAuthCallback(ok: q["status"] == "ok", message: q["message"] ?? "")
    }
}

/// Client for `POST /api/oauth/{provider}/start`, `GET .../status`, `POST .../logout`.
public struct OAuthAPI: Sendable {
    public let baseURL: URL
    public let token: String
    public let appScheme: String
    let transport: HTTPTransport

    public init(baseURL: URL, token: String, appScheme: String = "loopx-auth",
                transport: HTTPTransport = URLSessionTransport()) {
        self.baseURL = baseURL; self.token = token; self.appScheme = appScheme; self.transport = transport
    }

    public init(lock: LockFile, appScheme: String = "loopx-auth", transport: HTTPTransport = URLSessionTransport()) {
        self.init(baseURL: lock.baseURL, token: lock.token, appScheme: appScheme, transport: transport)
    }

    public func start(_ provider: OAuthProvider, account: String? = nil) async throws -> OAuthStart {
        var body: [String: String] = ["app_scheme": appScheme]
        if let account { body["account"] = account }
        let obj = try await call("POST", "start", provider, body: body)
        guard let s = obj["authorization_url"] as? String, let url = URL(string: s),
              url.scheme == "https" || url.scheme == "http", let state = obj["state"] as? String
        else { throw OAuthError.api(.invalidResponse) }
        return OAuthStart(authorizationURL: url, state: state)
    }

    public func status(_ provider: OAuthProvider, account: String? = nil, state: String? = nil) async throws -> OAuthStatus {
        var query: [URLQueryItem] = []
        if let account { query.append(.init(name: "account", value: account)) }
        if let state { query.append(.init(name: "state", value: state)) }
        let obj = try await call("GET", "status", provider, query: query)
        let flow = obj["flow"] as? [String: Any]
        return OAuthStatus(connected: obj["connected"] as? Bool ?? false, expired: obj["expired"] as? Bool ?? false,
                           flowState: flow?["state"] as? String ?? "unknown", message: flow?["message"] as? String ?? "")
    }

    public func logout(_ provider: OAuthProvider, account: String? = nil) async throws {
        var body: [String: String] = [:]
        if let account { body["account"] = account }
        _ = try await call("POST", "logout", provider, body: body)
    }

    private func call(_ method: String, _ action: String, _ provider: OAuthProvider,
                      body: [String: String]? = nil, query: [URLQueryItem] = []) async throws -> [String: Any] {
        guard var comps = URLComponents(url: baseURL, resolvingAgainstBaseURL: false) else { throw OAuthError.api(.invalidResponse) }
        comps.path = "/api/oauth/\(provider.rawValue)/\(action)"
        comps.queryItems = query.isEmpty ? nil : query
        guard let url = comps.url else { throw OAuthError.api(.invalidResponse) }
        var req = URLRequest(url: url)
        req.httpMethod = method
        req.setValue(token, forHTTPHeaderField: "X-LoopX-Token")
        if let body {
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = try JSONSerialization.data(withJSONObject: body)
        }
        let data: Data, http: HTTPURLResponse
        do { (data, http) = try await transport.send(req) }
        catch { throw OAuthError.api(.unreachable((error as NSError).localizedDescription)) }
        let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
        switch http.statusCode {
        case 200..<300: return obj
        case 400: throw OAuthError.failed(obj["error"] as? String ?? "")
        case 403: throw OAuthError.api(.forbidden)
        default: throw OAuthError.api(.server(http.statusCode))
        }
    }
}
