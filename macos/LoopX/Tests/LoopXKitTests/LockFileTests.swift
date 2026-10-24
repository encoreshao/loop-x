import Foundation
import Testing
@testable import LoopXKit

@Suite struct LockFileTests {
    static let json = #"{"pid":4242,"port":8420,"version":"0.4.0","started_at":"2026-10-22T10:00:00.000Z","token":"abc"}"#

    @Test func parsesContractDocument() throws {
        let lock = try #require(LockFile.parse(Data(Self.json.utf8)))
        #expect(lock.pid == 4242)
        #expect(lock.port == 8420)
        #expect(lock.version == "0.4.0")
        #expect(lock.startedAt == "2026-10-22T10:00:00.000Z")
        #expect(lock.token == "abc")
        #expect(lock.baseURL.absoluteString == "http://127.0.0.1:8420")
        #expect(lock.eventsURL.absoluteString == "http://127.0.0.1:8420/api/events")
    }

    @Test func rejectsMalformedAndImplausible() {
        #expect(LockFile.parse(Data("not json".utf8)) == nil)
        #expect(LockFile.parse(Data("{}".utf8)) == nil)
        #expect(LockFile.parse(Data(#"{"pid":0,"port":8420,"version":"1","started_at":"x","token":"t"}"#.utf8)) == nil)
        #expect(LockFile.parse(Data(#"{"pid":5,"port":0,"version":"1","started_at":"x","token":"t"}"#.utf8)) == nil)
        #expect(LockFile.parse(Data(#"{"pid":5,"port":70000,"version":"1","started_at":"x","token":"t"}"#.utf8)) == nil)
    }

    @Test func readsFromDiskAndMissingFileIsNil() throws {
        let dir = tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let url = dir.appendingPathComponent("loopx.lock")
        #expect(LockFile.read(at: url) == nil)
        try Self.json.write(to: url, atomically: true, encoding: .utf8)
        #expect(LockFile.read(at: url)?.port == 8420)
    }

    @Test func staleDetectionUsesPidLiveness() throws {
        let lock = try #require(LockFile.parse(Data(Self.json.utf8)))
        #expect(lock.isStale(pidAlive: { _ in false }))
        #expect(!lock.isStale(pidAlive: { $0 == 4242 }))
        let dir = tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let url = dir.appendingPathComponent("loopx.lock")
        try Self.json.write(to: url, atomically: true, encoding: .utf8)
        #expect(LockFile.readLive(at: url, pidAlive: { _ in false }) == nil)
        #expect(LockFile.readLive(at: url, pidAlive: { _ in true })?.pid == 4242)
    }

    @Test func realPidChecks() {
        #expect(LockFile.isProcessAlive(getpid()))
        #expect(!LockFile.isProcessAlive(0))
        #expect(!LockFile.isProcessAlive(-5))
        // A just-reaped child is gone.
        let p = Process(); p.executableURL = URL(fileURLWithPath: "/usr/bin/true")
        try? p.run(); p.waitUntilExit()
        #expect(!LockFile.isProcessAlive(p.processIdentifier))
    }

    @Test func defaultURLHonoursLoopEngineeringHomeThenHome() {
        #expect(LockFile.defaultURL(environment: ["LOOP_ENGINEERING_HOME": "/tmp/x"]).path == "/tmp/x/loopx.lock")
        #expect(LockFile.defaultURL(environment: ["HOME": "/Users/u"]).path == "/Users/u/.loopx/loopx.lock")
        #expect(LockFile.defaultURL(environment: ["LOOP_ENGINEERING_HOME": "", "HOME": "/Users/u"]).path
                == "/Users/u/.loopx/loopx.lock")
    }
}

@Suite struct SemVerTests {
    @Test func parsesAndOrders() throws {
        #expect(SemVer("0.4.0") == SemVer(0, 4, 0))
        #expect(SemVer("v1.2.3") == SemVer(1, 2, 3))
        #expect(SemVer("1.2.3+build5") == SemVer(1, 2, 3))
        #expect(SemVer("1.2") == nil)
        #expect(SemVer("a.b.c") == nil)
        #expect(try #require(SemVer("0.4.0")) < (try #require(SemVer("0.10.0"))))   // numeric, not lexicographic
        #expect(try #require(SemVer("0.9.9")) < (try #require(SemVer("1.0.0"))))
        #expect(try #require(SemVer("1.0.0-rc.1")) < (try #require(SemVer("1.0.0"))))
        #expect(try #require(SemVer("1.0.0-rc.2")) < (try #require(SemVer("1.0.0-rc.10"))))
        #expect(SemVer("1.2.3")?.description == "1.2.3")
    }
}

@Suite struct BackoffTests {
    @Test func doublesFromOneSecondAndCapsAtThirty() {
        var b = Backoff()
        #expect([b.next(), b.next(), b.next(), b.next(), b.next(), b.next(), b.next(), b.next()]
                == [1, 2, 4, 8, 16, 30, 30, 30])
        b.reset()
        #expect(b.next() == 1)
    }
}
