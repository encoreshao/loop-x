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
        #expect(h.contains("#1d1640") && h.contains("class='halo'") && h.contains("class='sheen'"))
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

    /// The mark's master holds about 450px of real detail, so it stays near 220pt (440px at 2x).
    @Test func theMarkStaysWithinItsSourceDetailAndShrinksInAShortWindow() {
        let h = html()
        #expect(h.contains("--mark:min(220px,26vh)"))
        #expect(h.contains("--name:min(180px,22vh)"))
        #expect(h.components(separatedBy: "--mark:").count - 1 == 1, "one size for both modes")
    }

    @Test func markThenNameThenOneTaglineThenTheLoader() {
        let h = html()
        #expect(h.contains("<p class='tag'>Autonomous agents, human in the loop.</p>"))
        for old in ["class='line'", "class='sub'", "busywork", "Nothing ships without you"] {
            #expect(!h.contains(old), "\(old) is gone")
        }
        let order = ["class='stage'", "<h1>", "class='tag'", "class='bar'"].map { h.range(of: $0)!.lowerBound }
        #expect(order == order.sorted())
    }

    @Test func theNameIsAFullWordmarkAndTheTaglineCarriesTheGradient() {
        let h = html()
        let rule = { (sel: String) in h.components(separatedBy: "\n\(sel){")[1].components(separatedBy: "}")[0] }
        #expect(h.contains("h1 .brand-name{display:block;width:var(--name)"))
        #expect(rule(".tag").contains("background-clip:text") && rule(".tag").contains("var(--grad)"))
        for sel in [".stage", "h1", ".bar"] {
            #expect(rule(sel).contains("margin"), "\(sel) sets its own spacing")
        }
    }

    @Test func lightModeShowsItsOwnCrispMarkAndDarkKeepsTheGlowMark() {
        let light = "data:image/png;base64,LL"
        let h = StartScreen.html("Starting…", failed: false, mark: Self.mark, markLight: light)
        #expect(h.contains("<img class='mark on-light' src='\(light)'"))
        #expect(h.contains("<img class='mark on-dark' src='\(Self.mark)'"))
        // The sheen is dark-only, so it is masked to the dark mark.
        #expect(h.contains("--m:url('\(Self.mark)')") && !h.contains("--m:url('\(light)')"))
        let dark = h.components(separatedBy: "@media(prefers-color-scheme:dark){").dropFirst().joined()
        #expect(dark.contains(".mark.on-light{display:none}") && dark.contains(".mark.on-dark{display:block}"))
    }

    @Test func neitherMarkIsBlurredByFilters() {
        let h = html()
        let rule = h.components(separatedBy: "\n.mark{")[1].components(separatedBy: "}")[0]
        #expect(!rule.contains("saturate(") && !rule.contains("drop-shadow("), "the light mark is drawn as rendered")
        let dark = h.components(separatedBy: "@media(prefers-color-scheme:dark){").dropFirst().joined()
        #expect(!dark.contains(".mark{filter:"), "the dark mark is drawn as rendered")
    }

    @Test func declaresUTF8SoTheEllipsisRenders() {
        #expect(html().hasPrefix("<html><head><meta charset='utf-8'>"))
    }

    @Test func keepsTheTitlebarDragStrip() {
        #expect(html().contains("class='\(TitlebarChrome.dragStripClass)'"))
    }
}
