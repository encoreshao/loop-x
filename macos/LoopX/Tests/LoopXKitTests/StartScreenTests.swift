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
        #expect(ungated.contains("animation:slide"))
        #expect(!gated.contains("animation:slide"))
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
        #expect(!h.contains("class='bar'"))
        let starting = html()
        #expect(!starting.contains("loopx-retry"))
        #expect(starting.contains("class='bar'"))
    }

    @Test func showsVersionOnly() {
        let h = html()
        #expect(h.contains("v0.7.0"))
        #expect(!h.contains("Encore Shao") && !h.contains("mailto:") && !h.contains("daily grind"))
    }

    @Test func theMarkIsLargeAndShrinksOnlyInAShortWindow() {
        let h = html()
        #expect(h.contains("--mark:min(220px,32vh)"))
        #expect(h.components(separatedBy: "--mark:").count - 1 == 1, "one size for both modes")
    }

    @Test func bothModesShareOneLayoutWithTheTaglines() {
        let h = html()
        #expect(h.contains("<p class='line'>Your agents. Your Mac. Your call.</p>"))
        #expect(h.contains("<p class='sub'>Handled while you do the real work.</p>"))
        // Dark mode keeps the same column: the status is not pinned elsewhere, no extra dot.
        #expect(!h.contains("position:fixed;bottom:44px") && !h.contains("class='dot'"))
        // Order: mark, name, the two lines, then the bar and status.
        let order = ["class='stage'", "<h1>", "class='line'", "class='sub'", "class='bar'"].map { h.range(of: $0)!.lowerBound }
        #expect(order == order.sorted())
    }

    @Test func theHeadlineLeadsAndEachGroupHasItsOwnSpace() {
        let h = html()
        let rule = { (sel: String) in h.components(separatedBy: "\n\(sel){")[1].components(separatedBy: "}")[0] }
        // The headline is the largest text on the page, well above the 14px subtitle.
        #expect(rule(".line").contains("font-size:var(--title)"))
        #expect(h.contains("--title:clamp(24px,4.6vh,32px)"))
        // Spacing is set per group (mark, name, headline, loader), not one flat gap.
        #expect(rule("body").contains("gap:0"))
        for sel in [".stage", "h1", ".line", ".bar"] {
            #expect(rule(sel).contains("margin"), "\(sel) sets its own spacing")
        }
    }

    @Test func theMarkStandsOutFromTheLightBackground() {
        let h = html()
        let dark = h.components(separatedBy: "@media(prefers-color-scheme:dark){").dropFirst().joined()
        let light = h.replacingOccurrences(of: dark, with: "")
        // A coloured halo behind the mark and a richer mark in light mode too, not only in dark.
        #expect(light.contains(".halo{position:absolute"))
        #expect(light.contains("saturate("))
    }

    @Test func declaresUTF8SoTheEllipsisRenders() {
        #expect(html().hasPrefix("<html><head><meta charset='utf-8'>"))
    }

    @Test func keepsTheTitlebarDragStrip() {
        #expect(html().contains("class='\(TitlebarChrome.dragStripClass)'"))
    }
}
