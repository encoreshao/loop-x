import Foundation
import Testing
@testable import LoopXKit

@Suite struct UpdateConfigTests {
    static let key = Data(repeating: 7, count: 32).base64EncodedString()
    static let feed = "https://example.com/appcast.xml"

    @Test func needsFeedAndKey() {
        #expect(UpdateConfig(info: ["SUFeedURL": Self.feed, "SUPublicEDKey": Self.key])?.feedURL.absoluteString == Self.feed)
        #expect(UpdateConfig(info: ["SUFeedURL": Self.feed]) == nil)
        #expect(UpdateConfig(info: ["SUPublicEDKey": Self.key]) == nil)
        #expect(UpdateConfig(info: [:]) == nil)
    }

    @Test func blankOrMalformedValuesDisableUpdates() {
        #expect(UpdateConfig(info: ["SUFeedURL": "  ", "SUPublicEDKey": Self.key]) == nil)
        #expect(UpdateConfig(info: ["SUFeedURL": Self.feed, "SUPublicEDKey": "not-a-key"]) == nil)
        #expect(UpdateConfig(info: ["SUFeedURL": "http://example.com/a.xml", "SUPublicEDKey": Self.key]) == nil)
    }
}
