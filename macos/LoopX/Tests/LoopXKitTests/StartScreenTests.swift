import Testing
@testable import LoopXKit

@Suite struct StartScreenTests {
    static let mark = "data:image/png;base64,AA"

    func html(_ text: String = "Starting LoopX…", failed: Bool = false, mark: String = StartScreenTests.mark) -> String {
        StartScreen.html(text, failed: failed, version: "0.7.0", mark: mark)
    }

    /// The `@media(prefers-reduced-motion:no-preference){…}` block, and the page without it.
    func motionSplit(_ h: String) -> (gated: String, ungated: String) {
        let open = "@media(prefers-reduced-motion:no-preference){"
        let gated = h.components(separatedBy: open)[1].components(separatedBy: "}}")[0]
        return (gated, h.replacingOccurrences(of: open + gated + "}}", with: ""))
    }

    @Test func oneFocalPointNoOldDecoration() {
        let h = html()
        for old in ["class='water'", "ripple", "class='wave", "bub", "orbit", "assemble"] {
            #expect(!h.contains(old), "\(old) is gone")
        }
        #expect(h.contains("<img class='mark' src='\(Self.mark)'"))
    }

    @Test func lightIsQuietAndDarkIsTheGlowStage() {
        let h = html()
        #expect(h.contains("#f5f5f7") && h.contains("#0a84ff"))
        #expect(h.contains("@media(prefers-color-scheme:dark)"))
        #expect(h.contains("#1b1530") && h.contains("class='halo'") && h.contains("class='sheen'"))
        // The sheen only ever lights the mark itself: it is masked to the mark's own shape.
        #expect(h.contains("url('\(Self.mark)')"))
    }

    @Test func reduceMotionGatesOnlyDecoration() {
        let (gated, ungated) = motionSplit(html())
        #expect(gated.contains("animation:breathe") && gated.contains("animation:sheen") && gated.contains("animation:enter"))
        // The bar and the dot mean "still starting": never gated.
        #expect(ungated.contains("animation:slide") && ungated.contains("animation:pulse"))
        #expect(!gated.contains("animation:slide") && !gated.contains("animation:pulse"))
    }

    @Test func introHoldCoversTheEntrance() {
        // Each "animation:enter <duration>s <easing> [<delay>s] both": the hold covers the latest end.
        let rules = html().components(separatedBy: "animation:enter ").dropFirst()
        let ends = rules.map { rule -> Double in
            rule.components(separatedBy: " both")[0].split(separator: " ")
                .filter { $0.hasSuffix("s") }.compactMap { Double($0.dropLast()) }.reduce(0, +)
        }
        #expect(!ends.isEmpty)
        #expect(StartScreen.introSeconds >= ends.max()!)
        #expect(StartScreen.introSeconds < ends.max()! + 0.3, "no dead time after the entrance")
    }

    @Test func noMarkMeansNoImageAndNoMask() {
        let h = html(mark: "")
        #expect(!h.contains("<img class='mark'"))
        #expect(!h.contains("url('')"))
        #expect(!h.contains("class='sheen'"))
    }

    @Test func theNameIsTheBrandImageWhenGivenAndTextOtherwise() {
        let name = "<img class='brand-name' src='data:image/png;base64,AA' alt='LoopX'>"
        let withImage = StartScreen.html("Starting…", failed: false, mark: "", name: name)
        #expect(withImage.contains("<h1>\(name)</h1>"))
        #expect(!withImage.contains("<h1>LoopX</h1>"))
        #expect(html().contains("<h1>LoopX</h1>"))
    }

    @Test func statusTextIsEscapedAndFailureOffersRetry() {
        let h = html("<b>boom</b> & more", failed: true)
        #expect(h.contains("&lt;b>boom&lt;/b> &amp; more"))
        #expect(h.contains("class='retry' href='loopx-retry://now'"))
        #expect(!h.contains("class='bar'") && !h.contains("class='dot'"))
        let starting = html()
        #expect(!starting.contains("loopx-retry"))
        #expect(starting.contains("class='bar'") && starting.contains("class='dot'"))
    }

    @Test func showsVersionOnly() {
        let h = html()
        #expect(h.contains("v0.7.0"))
        #expect(!h.contains("Encore Shao") && !h.contains("mailto:") && !h.contains("daily grind"))
    }

    @Test func keepsTheTitlebarDragStrip() {
        #expect(html().contains("class='\(TitlebarChrome.dragStripClass)'"))
    }
}
