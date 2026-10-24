import Foundation

/// Arbitrary JSON, for the free-form `data` of events.
public enum JSONValue: Codable, Equatable, Sendable {
    case null, bool(Bool), number(Double), string(String), array([JSONValue]), object([String: JSONValue])

    public init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let b = try? c.decode(Bool.self) { self = .bool(b) }
        else if let n = try? c.decode(Double.self) { self = .number(n) }
        else if let s = try? c.decode(String.self) { self = .string(s) }
        else if let a = try? c.decode([JSONValue].self) { self = .array(a) }
        else { self = .object(try c.decode([String: JSONValue].self)) }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let v): try c.encode(v)
        case .number(let v): try c.encode(v)
        case .string(let v): try c.encode(v)
        case .array(let v): try c.encode(v)
        case .object(let v): try c.encode(v)
        }
    }
}

public enum AppState: String, Codable, Equatable, Sendable {
    case idle, running, failed
    case awaitingApproval = "awaiting_approval"
}

public struct LoopStatus: Codable, Equatable, Sendable {
    public var state: String
    public var updatedAt: String?
    public var pid: Int?
    public var currentIssue: String?
    public var currentStep: String?
    public var lastExitCode: Int?

    enum CodingKeys: String, CodingKey {
        case state, pid
        case updatedAt = "updated_at"
        case currentIssue = "current_issue"
        case currentStep = "current_step"
        case lastExitCode = "last_exit_code"
    }

    public init(state: String, updatedAt: String? = nil, pid: Int? = nil, currentIssue: String? = nil,
                currentStep: String? = nil, lastExitCode: Int? = nil) {
        self.state = state; self.updatedAt = updatedAt; self.pid = pid
        self.currentIssue = currentIssue; self.currentStep = currentStep; self.lastExitCode = lastExitCode
    }

    /// `stale` is the backend's name for a `running` status whose pid died; treat it like `failed`.
    public var isFailure: Bool { state == "failed" || state == "stale" }
}

public struct TopicStatus: Codable, Equatable, Sendable {
    public var state: String
    public var updatedAt: String?
    enum CodingKeys: String, CodingKey { case state; case updatedAt = "updated_at" }
    public init(state: String, updatedAt: String? = nil) { self.state = state; self.updatedAt = updatedAt }
}

public struct Approval: Codable, Equatable, Sendable, Identifiable {
    public var id: String
    public var kind: String
    public var runId: String?
    public var project: String?
    public var issueIid: Int?
    public var title: String?
    public var branch: String?
    public var targetBranch: String?
    public var summary: String?
    public var status: String
    public var createdAt: String?
    public var expiresAt: String?
    public var decidedAt: String?
    public var decidedBy: String?

    enum CodingKeys: String, CodingKey {
        case id, kind, project, title, branch, summary, status
        case runId = "run_id", issueIid = "issue_iid", targetBranch = "target_branch"
        case createdAt = "created_at", expiresAt = "expires_at"
        case decidedAt = "decided_at", decidedBy = "decided_by"
    }

    public init(id: String, kind: String = "open_mr", runId: String? = nil, project: String? = nil,
                issueIid: Int? = nil, title: String? = nil, branch: String? = nil, targetBranch: String? = nil,
                summary: String? = nil, status: String = "pending", createdAt: String? = nil,
                expiresAt: String? = nil, decidedAt: String? = nil, decidedBy: String? = nil) {
        self.id = id; self.kind = kind; self.runId = runId; self.project = project; self.issueIid = issueIid
        self.title = title; self.branch = branch; self.targetBranch = targetBranch; self.summary = summary
        self.status = status; self.createdAt = createdAt; self.expiresAt = expiresAt
        self.decidedAt = decidedAt; self.decidedBy = decidedBy
    }

    public var isPending: Bool { status == "pending" }
    public var expiry: Date? { expiresAt.flatMap(Timestamps.parse) }
}

public struct Attention: Codable, Equatable, Sendable {
    public enum Kind: String, Codable, Sendable { case mrOpened = "mr_opened", escalated, failed }
    public var kind: Kind
    public var at: String
    public var project: String?
    public var issueIid: Int?
    public var reason: String?
    public var mrURL: String?

    enum CodingKeys: String, CodingKey {
        case kind, at, project, reason
        case issueIid = "issue_iid", mrURL = "mr_url"
    }

    public init(kind: Kind, at: String, project: String? = nil, issueIid: Int? = nil,
                reason: String? = nil, mrURL: String? = nil) {
        self.kind = kind; self.at = at; self.project = project; self.issueIid = issueIid
        self.reason = reason; self.mrURL = mrURL
    }

    public var date: Date? { Timestamps.parse(at) }
    /// Identity used to remember that the user dismissed this alert.
    public var key: String { "\(kind.rawValue)@\(at)" }
}

public struct RecentEvent: Codable, Equatable, Sendable {
    public var type: String
    public var at: String?
    public var runId: String?
    public var project: String?
    public var issueIid: Int?
    public var data: JSONValue?
    enum CodingKeys: String, CodingKey {
        case type, at, project, data
        case runId = "run_id", issueIid = "issue_iid"
    }
}

public struct ApprovalsBlock: Codable, Equatable, Sendable {
    public var pending: [Approval]
    public init(pending: [Approval] = []) { self.pending = pending }
}

/// The whole-state document pushed on every `event: snapshot`.
public struct Snapshot: Codable, Equatable, Sendable {
    public var version: Int
    public var appState: AppState
    public var loops: [String: LoopStatus]
    public var topics: [String: TopicStatus]
    public var approvals: ApprovalsBlock
    public var attention: Attention?
    public var recentEvents: [RecentEvent]

    enum CodingKeys: String, CodingKey {
        case version, loops, topics, approvals, attention
        case appState = "app_state", recentEvents = "recent_events"
    }

    public init(version: Int = 1, appState: AppState = .idle, loops: [String: LoopStatus] = [:],
                topics: [String: TopicStatus] = [:], approvals: ApprovalsBlock = .init(),
                attention: Attention? = nil, recentEvents: [RecentEvent] = []) {
        self.version = version; self.appState = appState; self.loops = loops; self.topics = topics
        self.approvals = approvals; self.attention = attention; self.recentEvents = recentEvents
    }

    // Tolerant decoding: a field the backend adds or omits never breaks the whole stream.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        version = try c.decodeIfPresent(Int.self, forKey: .version) ?? 1
        appState = (try? c.decode(AppState.self, forKey: .appState)) ?? .idle
        loops = try c.decodeIfPresent([String: LoopStatus].self, forKey: .loops) ?? [:]
        topics = try c.decodeIfPresent([String: TopicStatus].self, forKey: .topics) ?? [:]
        approvals = try c.decodeIfPresent(ApprovalsBlock.self, forKey: .approvals) ?? .init()
        attention = try? c.decodeIfPresent(Attention.self, forKey: .attention)
        recentEvents = try c.decodeIfPresent([RecentEvent].self, forKey: .recentEvents) ?? []
    }

    public static func decode(_ data: Data) throws -> Snapshot { try JSONDecoder().decode(Snapshot.self, from: data) }

    /// The loop currently doing something worth naming, for the collapsed notch's subtitle.
    public var runningLoop: (name: String, status: LoopStatus)? {
        loops.filter { $0.value.state == "running" }.sorted { $0.key < $1.key }
            .first.map { ($0.key, $0.value) }
    }
}

public enum Timestamps {
    public static func parse(_ s: String) -> Date? {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let d = f.date(from: s) { return d }
        f.formatOptions = [.withInternetDateTime]
        return f.date(from: s)
    }
}

/// `GET /api/version`
public struct VersionInfo: Codable, Equatable, Sendable {
    public var name: String
    public var version: String
    public var pid: Int32?
    public var startedAt: String?
    public var root: String?
    public var legacyDashboard: Bool?
    enum CodingKeys: String, CodingKey {
        case name, version, pid, root
        case startedAt = "started_at", legacyDashboard = "legacy_dashboard"
    }
    public init(name: String = "loopx", version: String, pid: Int32? = nil, startedAt: String? = nil,
                root: String? = nil, legacyDashboard: Bool? = nil) {
        self.name = name; self.version = version; self.pid = pid; self.startedAt = startedAt
        self.root = root; self.legacyDashboard = legacyDashboard
    }
}
