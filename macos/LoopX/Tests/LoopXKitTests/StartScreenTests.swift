import Testing
@testable import LoopXKit

@Suite struct StartScreenTests {
    func html(_ text: String = "Starting the backend…", failed: Bool = false) -> String {
        StartScreen.html(text, failed: failed, version: "0.7.0", logo: "<svg id='logo'></svg>")
    }

    @Test func hasTheRippleAndWaveLayersBehindTheLogo() {
        let h = html()
        #expect(h.contains("class='water'"))
        #expect(h.components(separatedBy: "class='ripple").count - 1 == 4)
        #expect(h.contains("class='wave"))
        #expect(h.contains("<svg id='logo'>"))
    }

    @Test func theWaterKeepsMovingCalmlyUnderReducedMotion() {
        let h = html()
        let open = "@media(prefers-reduced-motion:no-preference){"
        let gated = h.components(separatedBy: open)[1].components(separatedBy: "}}")[0]   // the block ends at its first "}}"
        let ungated = h.replacingOccurrences(of: open + gated + "}}", with: "")
        // Reduce Motion is on for some users, and the ripples are what they asked for: a slow, small
        // version always runs; the larger, faster one (and the entrance/float) only without the setting.
        #expect(ungated.contains("animation:ripple ") && ungated.contains("animation:wave "))
        #expect(gated.contains("rippleBig") && gated.contains("assemble") && gated.contains("float"))
        // The orbit ring and spinner are never gated: that motion means "still starting".
        #expect(!gated.contains("animation:orbit") && !gated.contains("animation:s "))
        #expect(ungated.contains("animation:orbit") && ungated.contains("animation:s "))
    }

    @Test func theLogoAssemblesFromItsFragmentsOnlyWithoutReducedMotion() {
        let h = html()
        #expect(h.contains("@keyframes assemble"))
        // The four arcs and the two X strokes each fly in on their own, gated like the other entrances.
        let open = "@media(prefers-reduced-motion:no-preference){"
        let gated = h.components(separatedBy: open)[1].components(separatedBy: "}}")[0]
        for part in ["a1", "a2", "a3", "a4", "x1", "x2"] {
            #expect(h.contains(".logo .\(part){"), "\(part) has a scattered start")
            #expect(gated.contains(".logo .\(part){animation:assemble"), "\(part) animates only when motion is allowed")
        }
    }

    @Test func theIntroHoldLastsUntilTheLastFragmentHasLanded() {
        // Each "animation:assemble <duration>s <easing> [<delay>s] both": the hold must cover the latest end.
        let rules = html().components(separatedBy: "animation:assemble ").dropFirst()
        let ends = rules.map { rule -> Double in
            let secs = rule.components(separatedBy: " both")[0].split(separator: " ")
                .filter { $0.hasSuffix("s") }.compactMap { Double($0.dropLast()) }
            return secs.reduce(0, +)
        }
        #expect(ends.count == 6)
        #expect(StartScreen.introSeconds >= ends.max()!)
        #expect(StartScreen.introSeconds < ends.max()! + 0.5, "no dead time after the logo is whole")
    }

    @Test func theBodyDoesNotPaintOverTheWaterLayer() {
        let h = html()
        #expect(h.contains("html{background:var(--bg)"))
        #expect(h.contains("body{background:transparent"))
    }

    @Test func statusTextIsEscapedAndFailureOffersRetry() {
        let h = html("<b>boom</b> & more", failed: true)
        #expect(h.contains("&lt;b>boom&lt;/b> &amp; more"))
        #expect(h.contains("loopx-retry://now"))
        #expect(!html().contains("loopx-retry"))
    }

    @Test func showsVersionAndAuthor() {
        #expect(html().contains("v0.7.0"))
        #expect(html().contains("Encore Shao"))
    }
}
