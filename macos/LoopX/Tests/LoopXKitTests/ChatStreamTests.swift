import Testing
@testable import LoopXKit

@Suite struct ChatStreamTests {
    func msg(_ event: String, _ data: String) -> SSEMessage { SSEMessage(event: event, data: data, id: nil) }

    @Test func parsesChunkDoneAndError() {
        #expect(ChatEvent(msg("chunk", #""Hel""#)) == .chunk("Hel"))
        #expect(ChatEvent(msg("done", #""All set""#)) == .done("All set"))
        #expect(ChatEvent(msg("error", #""boom""#)) == .error("boom"))
    }

    @Test func parsesAPendingToolCardAndIgnoresOtherTools() {
        let p = #"{"kind":"pending","id":"t1","pending_id":"p9","tool":"gitlab.comment","summary":"Comment on #7","args":{}}"#
        #expect(ChatEvent(msg("tool", p)) == .pending(PendingCard(id: "p9", tool: "gitlab.comment", summary: "Comment on #7")))
        #expect(ChatEvent(msg("tool", #"{"kind":"call","name":"x"}"#)) == .ignored)
        #expect(ChatEvent(msg("changed", "true")) == .ignored)
        #expect(ChatEvent(msg("chunk", "not json")) == .ignored)
    }

    @Test func replyAccumulatesTextThenFinishes() {
        var r = ChatReply()
        #expect(r.phase == .thinking)
        r.apply(.chunk("Hel")); r.apply(.chunk("lo"))
        #expect(r.text == "Hello")
        r.apply(.done("Hello world"))
        #expect(r.text == "Hello world"); #expect(r.phase == .done)
    }

    @Test func doneWithEmptyTextKeepsStreamedText() {
        var r = ChatReply(); r.apply(.chunk("kept")); r.apply(.done(""))
        #expect(r.text == "kept")
    }

    @Test func streamDropKeepsPartialText() {
        var r = ChatReply(); r.apply(.chunk("part"))
        r.streamEnded()
        #expect(r.text == "part"); #expect(r.phase == .interrupted)
        var finished = ChatReply(); finished.apply(.done("x")); finished.streamEnded()
        #expect(finished.phase == .done)     // a normal end after done is not an interruption
    }

    @Test func errorKeepsPartialTextAndRecordsTheMessage() {
        var r = ChatReply(); r.apply(.chunk("a")); r.apply(.error("quota"))
        #expect(r.text == "a"); #expect(r.phase == .failed("quota"))
    }

    @Test func pendingCardIsHeldUntilCleared() {
        var r = ChatReply()
        r.apply(.pending(PendingCard(id: "p", tool: "t", summary: "s")))
        #expect(r.pending?.id == "p")
        r.clearPending(); #expect(r.pending == nil)
    }
}

@Suite struct ChatStreamDecoderTests {
    /// What the server really sends: blank-line-terminated frames and `: keepalive` comments.
    static let wire = "event: chunk\ndata: \"Hel\"\n\n: keepalive\n\nevent: chunk\ndata: \"lo\"\n\n"
        + "event: tool\ndata: {\"kind\":\"pending\",\"pending_id\":\"p1\",\"tool\":\"t\",\"summary\":\"s\"}\n\n"
        + "event: done\ndata: \"Hello\"\n\n"

    @Test func decodesARealStreamByteByByte() {
        var d = ChatStreamDecoder()
        var events: [ChatEvent] = []
        for b in Array(Self.wire.utf8) { events += d.feed(b) }
        #expect(events == [.chunk("Hel"), .chunk("lo"),
                           .pending(PendingCard(id: "p1", tool: "t", summary: "s")), .done("Hello")])
    }

    @Test func aReplyBuiltFromTheWireEndsDoneWithItsText() {
        var d = ChatStreamDecoder(), r = ChatReply()
        for b in Array(Self.wire.utf8) { d.feed(b).forEach { r.apply($0) } }
        r.streamEnded()
        #expect(r.text == "Hello"); #expect(r.phase == .done); #expect(r.pending?.id == "p1")
    }

    @Test func multiByteCharactersSurviveBeingSplit() {
        var d = ChatStreamDecoder(), events: [ChatEvent] = []
        for b in Array("event: chunk\ndata: \"日本語\"\n\n".utf8) { events += d.feed(b) }
        #expect(events == [.chunk("日本語")])
    }
}
