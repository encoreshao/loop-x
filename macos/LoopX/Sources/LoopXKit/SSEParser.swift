import Foundation

public struct SSEMessage: Equatable, Sendable {
    public var event: String
    public var data: String
    public var id: String?
}

/// Incremental parser for `text/event-stream`. Feed bytes; get complete messages.
/// Comment lines (`: ping`) produce no message but still count as activity to the caller.
public struct SSEParser: Sendable {
    private var buffer: [UInt8] = []
    private var event = ""
    private var dataLines: [String] = []
    private var lastID: String?

    public init() {}

    /// Feeds one chunk; returns messages completed by it.
    public mutating func feed(_ bytes: [UInt8]) -> [SSEMessage] {
        var out: [SSEMessage] = []
        for b in bytes {
            if b == 0x0A {
                var line = buffer; buffer.removeAll(keepingCapacity: true)
                if line.last == 0x0D { line.removeLast() }
                if let m = feedLine(String(decoding: line, as: UTF8.self)) { out.append(m) }
            } else {
                buffer.append(b)
            }
        }
        return out
    }

    public mutating func feed(_ text: String) -> [SSEMessage] { feed(Array(text.utf8)) }

    /// Processes one line (without its terminator). A blank line dispatches the pending message.
    public mutating func feedLine(_ line: String) -> SSEMessage? {
        if line.isEmpty {
            defer { event = ""; dataLines.removeAll() }
            guard !dataLines.isEmpty else { return nil }
            return SSEMessage(event: event.isEmpty ? "message" : event,
                              data: dataLines.joined(separator: "\n"), id: lastID)
        }
        if line.hasPrefix(":") { return nil }
        let field: String, value: String
        if let colon = line.firstIndex(of: ":") {
            field = String(line[..<colon])
            var v = line[line.index(after: colon)...]
            if v.first == " " { v = v.dropFirst() }
            value = String(v)
        } else {
            field = line; value = ""
        }
        switch field {
        case "event": event = value
        case "data": dataLines.append(value)
        case "id": lastID = value
        default: break  // retry:, unknown fields
        }
        return nil
    }
}
