import Testing
@testable import LoopXKit

@Suite struct IntroHoldTests {
    final class Nav {}

    @Test func theHoldStartsWhenTheStartScreenItselfFinishesLoading() {
        var hold = IntroHold()
        let intro = Nav(), other = Nav()
        hold.loading(ObjectIdentifier(intro))
        // Another navigation finishing (an earlier page) does not start the clock.
        let r1 = hold.finished(ObjectIdentifier(other))
        #expect(!r1)
        let r2 = hold.finished(ObjectIdentifier(intro))
        #expect(r2)
        // Only once: a second didFinish for the same load is ignored.
        let r3 = hold.finished(ObjectIdentifier(intro))
        #expect(!r3)
    }

    @Test func nothingStartsBeforeTheStartScreenIsLoading() {
        var hold = IntroHold()
        let r4 = hold.finished(ObjectIdentifier(Nav()))
        #expect(!r4)
    }

    @Test func aFallbackStartsTheClockIfTheLoadNeverReportsFinishing() {
        var hold = IntroHold()
        let intro = Nav()
        hold.loading(ObjectIdentifier(intro))
        let r5 = hold.fallback()
        #expect(r5)
        let r6 = hold.finished(ObjectIdentifier(intro))
        #expect(!r6, "already started by the fallback")
        let r7 = hold.fallback()
        #expect(!r7)
        #expect(IntroHold.fallbackSeconds >= 1 && IntroHold.fallbackSeconds <= 2)
    }
}
