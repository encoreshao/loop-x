import Foundation
import Testing
@testable import LoopXKit

@Suite struct SSEParserTests {
    @Test func parsesEventAndDataAndIgnoresComments() {
        var p = SSEParser()
        let out = p.feed(": ping\n\nevent: snapshot\ndata: {\"a\":1}\n\n")
        #expect(out == [SSEMessage(event: "snapshot", data: "{\"a\":1}", id: nil)])
    }

    @Test func handlesSplitChunksAndCRLF() {
        var p = SSEParser()
        #expect(p.feed("event: snap").isEmpty)
        #expect(p.feed("shot\r\ndata: x\r").isEmpty)
        #expect(p.feed("\n\r\n") == [SSEMessage(event: "snapshot", data: "x", id: nil)])
    }

    @Test func joinsMultipleDataLinesAndDefaultsEventName() {
        var p = SSEParser()
        let out = p.feed("data: a\ndata: b\n\n")
        #expect(out == [SSEMessage(event: "message", data: "a\nb", id: nil)])
    }

    @Test func blankLineWithoutDataEmitsNothingAndEventNameDoesNotLeak() {
        var p = SSEParser()
        #expect(p.feed("event: x\n\n").isEmpty)
        #expect(p.feed("data: y\n\n") == [SSEMessage(event: "message", data: "y", id: nil)])
    }

    @Test func parsesTwoMessagesInOneChunkAndKeepsID() {
        var p = SSEParser()
        let out = p.feed("id: 7\nevent: snapshot\ndata: 1\n\nevent: snapshot\ndata: 2\n\n")
        #expect(out.map(\.data) == ["1", "2"])
        #expect(out[0].id == "7")
    }

    @Test func fieldWithoutColonAndNoSpaceAfterColon() {
        var p = SSEParser()
        let out = p.feed("data:nospace\ndata\n\n")
        #expect(out == [SSEMessage(event: "message", data: "nospace\n", id: nil)])
    }
}

@Suite(.serialized) struct SSEClientTests {
    func collect(_ client: SSEClient) -> (Recorder<SSEClientEvent>, Task<Void, Never>) {
        let rec = Recorder<SSEClientEvent>()
        let t = Task { for await e in client.events() { rec.add(e) } }
        return (rec, t)
    }
    func snapshots(_ r: Recorder<SSEClientEvent>) -> [Snapshot] {
        r.all.compactMap { if case .snapshot(let s) = $0 { return s } else { return nil } }
    }

    @Test func deliversSnapshotsAndIgnoresPingsAndOtherEvents() async throws {
        let server = try TestSSEServer { _, send, _ in
            send("event: snapshot\ndata: \(snapshotJSON())\n\n")
            send(": ping\n\n")
            send("event: other\ndata: {}\n\n")
            send("event: snapshot\ndata: \(snapshotJSON(app: "running"))\n\n")
        }
        let url = server.url
        let client = SSEClient(endpoint: { url }, backoff: Backoff(initial: 0.05, maximum: 0.2), wakeHook: nil)
        let (rec, task) = collect(client); defer { task.cancel() }
        #expect(await waitUntil { snapshots(rec).count == 2 })
        #expect(snapshots(rec).map(\.appState) == [.idle, .running])
        #expect(rec.all.first == .connected)
    }

    @Test func reconnectsWithBackoffAfterTheServerDropsTheStream() async throws {
        let server = try TestSSEServer { i, send, close in
            send("event: snapshot\ndata: \(snapshotJSON(app: i == 0 ? "idle" : "running"))\n\n")
            if i == 0 { DispatchQueue.global().asyncAfter(deadline: .now() + 0.1) { close() } }
        }
        let url = server.url
        let client = SSEClient(endpoint: { url }, backoff: Backoff(initial: 0.05, maximum: 0.2), wakeHook: nil)
        let (rec, task) = collect(client); defer { task.cancel() }
        #expect(await waitUntil { snapshots(rec).count == 2 })
        #expect(snapshots(rec).map(\.appState) == [.idle, .running])
        #expect(rec.all.contains { if case .disconnected = $0 { return true } else { return false } })
        #expect(server.connectionCount >= 2)
    }

    @Test func backoffDoublesWhileTheBackendIsMissingAndResetsOnceItReturns() async throws {
        let delays = Recorder<TimeInterval>()
        let up = Recorder<Bool>()
        let server = try TestSSEServer { _, send, _ in send("event: snapshot\ndata: \(snapshotJSON())\n\n") }
        let url = server.url
        let client = SSEClient(
            endpoint: { up.count > 0 ? url : nil },
            backoff: Backoff(initial: 1, maximum: 4), wakeHook: nil,
            sleeper: { d in
                // The watchdog also sleeps; record only the (large) backoff waits.
                if d >= 1 { delays.add(d) }
                try await Task.sleep(nanoseconds: 5_000_000)
            })
        let (rec, task) = collect(client); defer { task.cancel() }
        #expect(await waitUntil { delays.count >= 4 })
        #expect(Array(delays.all.prefix(4)) == [1, 2, 4, 4])
        up.add(true)
        #expect(await waitUntil { snapshots(rec).count >= 1 })
    }

    @Test func silenceTimeoutKillsAQuietConnectionAndReconnects() async throws {
        let server = try TestSSEServer { i, send, _ in
            send("event: snapshot\ndata: \(snapshotJSON(app: i == 0 ? "idle" : "failed"))\n\n")
            // never sends again on any connection: a half-open link
        }
        let url = server.url
        let client = SSEClient(endpoint: { url }, backoff: Backoff(initial: 0.05, maximum: 0.1),
                               silenceTimeout: 0.4, wakeHook: nil)
        let (rec, task) = collect(client); defer { task.cancel() }
        #expect(await waitUntil { snapshots(rec).count >= 2 })
        #expect(rec.all.contains { if case .disconnected(let r) = $0 { return r.contains("no data") } else { return false } })
    }

    @Test func pingCommentsKeepAQuietConnectionAlive() async throws {
        let server = try TestSSEServer { _, send, _ in
            send("event: snapshot\ndata: \(snapshotJSON())\n\n")
            for k in 1...12 {
                DispatchQueue.global().asyncAfter(deadline: .now() + 0.1 * Double(k)) { send(": ping\n\n") }
            }
        }
        let url = server.url
        let client = SSEClient(endpoint: { url }, backoff: Backoff(initial: 0.05, maximum: 0.1),
                               silenceTimeout: 0.5, wakeHook: nil)
        let (rec, task) = collect(client); defer { task.cancel() }
        _ = await waitUntil(timeout: 1.3) { false }
        #expect(server.connectionCount == 1)
        #expect(!rec.all.contains { if case .disconnected = $0 { return true } else { return false } })
    }

    @Test func wakeNotificationReconnectsImmediatelyEvenDuringALongBackoff() async throws {
        let wake = Recorder<@Sendable () -> Void>()
        let server = try TestSSEServer { _, send, _ in send("event: snapshot\ndata: \(snapshotJSON())\n\n") }
        let url = server.url
        let client = SSEClient(endpoint: { url }, backoff: Backoff(initial: 30, maximum: 30),
                               wakeHook: { fire in wake.add(fire); return { } })
        let (rec, task) = collect(client); defer { task.cancel() }
        #expect(await waitUntil { snapshots(rec).count == 1 && wake.count == 1 })
        wake.all[0]()   // laptop woke up: the old socket is probably dead
        #expect(await waitUntil { snapshots(rec).count == 2 })
        #expect(server.connectionCount == 2)
        // And it wakes a client that is sitting in a 30s backoff after losing the server.
        server.stop()
        #expect(await waitUntil { rec.all.contains { if case .disconnected = $0 { return true } else { return false } } })
    }

    @Test func nonOKStatusIsADisconnect() async throws {
        // Port with nothing listening: connection refused.
        let url = URL(string: "http://127.0.0.1:1/api/events")!
        let client = SSEClient(endpoint: { url }, backoff: Backoff(initial: 0.05, maximum: 0.1), wakeHook: nil)
        let (rec, task) = collect(client); defer { task.cancel() }
        #expect(await waitUntil { rec.all.contains { if case .disconnected = $0 { return true } else { return false } } })
        #expect(snapshots(rec).isEmpty)
    }

    @Test func cancellingTheConsumerStopsReconnecting() async throws {
        let server = try TestSSEServer { _, send, close in send("event: snapshot\ndata: \(snapshotJSON())\n\n"); close() }
        let url = server.url
        let client = SSEClient(endpoint: { url }, backoff: Backoff(initial: 0.05, maximum: 0.05), wakeHook: nil)
        let (rec, task) = collect(client)
        #expect(await waitUntil { rec.count >= 2 })
        task.cancel()
        _ = await waitUntil(timeout: 0.3) { false }
        let n = server.connectionCount
        _ = await waitUntil(timeout: 0.5) { false }
        #expect(server.connectionCount == n)
    }
}
