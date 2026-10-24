import Foundation

public struct PendingCard: Equatable, Identifiable, Sendable {
    public var id: String
    public var tool: String
    public var summary: String
}

/// One server-sent event of `GET /activity/chat-stream`.
public enum ChatEvent: Equatable, Sendable {
    case chunk(String), pending(PendingCard), done(String), error(String), ignored

    public init(_ m: SSEMessage) {
        guard let data = m.data.data(using: .utf8),
              let json = try? JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed]) else {
            self = .ignored; return
        }
        switch (m.event, json) {
        case ("chunk", let s as String): self = .chunk(s)
        case ("done", let s as String): self = .done(s)
        case ("error", let s as String): self = .error(s)
        case ("tool", let o as [String: Any]):
            guard o["kind"] as? String == "pending", let id = o["pending_id"] as? String, !id.isEmpty else { self = .ignored; return }
            self = .pending(PendingCard(id: id, tool: o["tool"] as? String ?? "", summary: o["summary"] as? String ?? ""))
        default: self = .ignored
        }
    }
}

/// What the island shows for the assistant's current reply.
public struct ChatReply: Equatable, Sendable {
    public enum Phase: Equatable, Sendable { case thinking, done, failed(String), interrupted }
    public private(set) var text = ""
    public private(set) var pending: PendingCard?
    public private(set) var phase: Phase = .thinking

    public init() {}

    public mutating func apply(_ e: ChatEvent) {
        switch e {
        case .chunk(let s): text += s
        case .pending(let p): pending = p
        case .done(let s): if !s.isEmpty { text = s }; phase = .done
        case .error(let m): phase = .failed(m)
        case .ignored: break
        }
    }

    /// The connection closed: fine after `done`/`error`, an interruption otherwise.
    public mutating func streamEnded() { if phase == .thinking { phase = .interrupted } }
    public mutating func clearPending() { pending = nil }
}

/// Turns the raw bytes of `GET /activity/chat-stream` into events. It must see every line, blank ones
/// included (a blank line ends a frame), so feed bytes, never `AsyncBytes.lines`, which drops them.
public struct ChatStreamDecoder: Sendable {
    private var parser = SSEParser()
    public init() {}
    public mutating func feed(_ byte: UInt8) -> [ChatEvent] {
        parser.feed([byte]).map(ChatEvent.init).filter { $0 != .ignored }
    }
}
