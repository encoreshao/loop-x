import Foundation

/// The page shown until a backend answers. Light and dark share one stacked layout: the LoopX mark
/// large over a soft halo, the name under it, one gradient tagline, then a thin progress bar and the
/// status line; only the version sits in the footer.
/// Light is a pale backdrop with a richer mark so its glow does not wash out; dark is the glow
/// stage, a violet halo breathing and a light sheen passing over the mark only. A failure swaps
/// the bar for the reason and a Try again button.
/// Under Reduce Motion the entrance, halo and sheen are off; the bar keeps moving, since its motion
/// is the sign that startup is still going.
public enum StartScreen {
    /// How long the app holds this page once it has loaded, even with a backend already up, so the
    /// entrance (mark, name and tagline, .5s each, staggered by .1s) finishes before the dashboard loads.
    public static let introSeconds = 0.8

    public static let tagline = "Autonomous agents, human in the loop."

    /// `mark` is the brand mark image's URL (a data URL); empty shows no mark. `name` is the brand
    /// name's markup (the name images); empty falls back to the plain text "LoopX".
    public static func html(_ text: String, failed: Bool, version: String = "", mark: String = "", name: String = "") -> String {
        func escape(_ t: String) -> String {
            t.replacingOccurrences(of: "&", with: "&amp;").replacingOccurrences(of: "<", with: "&lt;")
                .replacingOccurrences(of: "'", with: "&#39;")
        }
        let status = failed
            ? "<div class='fail'><div class='err'>\(escape(text))</div><a class='retry' href='loopx-retry://now'>Try again</a></div>"
            : "<div class='bar'></div><div class='status'>\(escape(text))</div>"
        let markHTML = mark.isEmpty ? "" : """
            <div class='stage'><i class='halo'></i><img class='mark' src='\(escape(mark))' alt=''>\
            <i class='sheen' style="--m:url('\(escape(mark))')"></i></div>
            """
        let footer = version.isEmpty ? "" : "<footer>v\(escape(version))</footer>"
        return """
        <html><head><meta charset='utf-8'><meta name='color-scheme' content='light dark'><style>
        .\(TitlebarChrome.dragStripClass){position:fixed;top:0;left:0;right:0;height:28px;z-index:10}
        :root{--mark:min(320px,36vh);--name:min(260px,30vh);--gap-l:clamp(18px,4vh,36px);--fg:#1d1d1f;--sub:#6e6e73;--accent:#0a84ff;--track:rgba(0,0,0,.08);--err:#c0392b;
        --grad:linear-gradient(90deg,#e0457b,#c0569a 45%,#f0954f);
        --halo:radial-gradient(circle,rgba(124,77,255,.22),rgba(224,69,123,.12) 45%,transparent 70%)}
        @media(prefers-color-scheme:dark){:root{--fg:#f5f5f7;--sub:rgba(245,245,247,.55);--accent:#a78bfa;--track:rgba(255,255,255,.1);--err:#ff6b5e;
        --grad:linear-gradient(90deg,#ff6fa3,#d77ad0 45%,#ffb36b);
        --halo:radial-gradient(circle,rgba(150,90,255,.45),rgba(255,70,140,.16) 45%,transparent 70%)}}
        html{background:radial-gradient(ellipse at 50% 40%,#ffffff 0%,#f5f5f7 55%,#e9e9ee 100%) fixed,#f5f5f7}
        @media(prefers-color-scheme:dark){html{background:radial-gradient(ellipse at 50% 40%,#1d1640 0%,#0d0b1e 60%,#08070f 100%) fixed,#08070f}}
        html,body{height:100%;margin:0;overflow:hidden}
        body{font:13px -apple-system,system-ui;color:var(--sub);display:flex;flex-direction:column;align-items:center;justify-content:center;gap:0;text-align:center;padding:0 24px 24px;box-sizing:border-box}
        .stage{position:relative;width:var(--mark);aspect-ratio:1119/930;margin:0 0 var(--gap-l)}
        .mark{position:relative;width:100%;height:100%;object-fit:contain;display:block;filter:saturate(1.8) contrast(1.25) brightness(.92) drop-shadow(0 8px 24px rgba(124,77,255,.2))}
        .halo{position:absolute;inset:-18%;border-radius:50%;background:var(--halo);filter:blur(18px);opacity:.7}
        .sheen{display:none}
        h1{margin:0 0 12px;font-size:40px;font-weight:700;letter-spacing:-.02em;color:var(--fg);line-height:1}
        h1 .brand-name{display:block;width:var(--name);height:auto;margin:0 auto}h1 .brand-name.on-dark{display:none}
        .tag{margin:0;font-size:22px;font-weight:500;letter-spacing:.01em;background:var(--grad);-webkit-background-clip:text;background-clip:text;-webkit-text-fill-color:transparent}
        .bar{position:relative;width:180px;height:3px;border-radius:2px;background:var(--track);overflow:hidden;margin:clamp(32px,7vh,64px) 0 12px}
        .bar::after{content:'';position:absolute;top:0;left:-40%;width:40%;height:100%;border-radius:2px;background:var(--accent);animation:slide 1.4s ease-in-out infinite}
        .status{font-size:12.5px}
        .fail{display:flex;flex-direction:column;align-items:center;gap:12px;margin-top:clamp(32px,7vh,64px)}
        .err{color:var(--err);max-width:420px;line-height:1.45;overflow-wrap:anywhere}
        .retry{display:inline-flex;align-items:center;height:30px;padding:0 16px;border-radius:15px;background:var(--accent);color:#fff;font-weight:600;text-decoration:none;transition:filter .2s ease-out}
        .retry:hover{filter:brightness(1.08)}
        footer{position:fixed;bottom:16px;left:0;right:0;font-size:11.5px;color:var(--sub);opacity:.7}
        @media(prefers-color-scheme:dark){
        .mark{filter:drop-shadow(0 8px 24px rgba(124,77,255,.25))}
        .halo{inset:-30%;opacity:1}
        .sheen{display:block;position:absolute;inset:0;mix-blend-mode:overlay;background:linear-gradient(105deg,transparent 35%,rgba(255,255,255,.55) 50%,transparent 65%) no-repeat;background-size:250% 100%;background-position:150% 0;
        -webkit-mask-image:var(--m);mask-image:var(--m);-webkit-mask-size:contain;mask-size:contain;-webkit-mask-repeat:no-repeat;mask-repeat:no-repeat;-webkit-mask-position:center;mask-position:center}
        h1 .brand-name.on-light{display:none}h1 .brand-name.on-dark{display:block}}
        @media(prefers-reduced-motion:no-preference){
        .stage{animation:enter .5s cubic-bezier(.22,1,.36,1) both}
        h1{animation:enter .5s cubic-bezier(.22,1,.36,1) .1s both}
        .tag{animation:enter .5s cubic-bezier(.22,1,.36,1) .2s both}
        .halo{animation:breathe 3.6s ease-in-out infinite}
        .sheen{animation:sheen 3.2s ease-in-out .6s infinite}}
        @keyframes enter{from{opacity:0;transform:scale(.96)}}
        @keyframes slide{to{left:100%}}
        @keyframes breathe{50%{opacity:1;transform:scale(1.08)}}
        @keyframes sheen{0%{background-position:150% 0}55%,100%{background-position:-50% 0}}
        </style></head><body><div class='\(TitlebarChrome.dragStripClass)'></div>
        \(markHTML)<h1>\(name.isEmpty ? "LoopX" : name)</h1>
        <p class='tag'>\(tagline)</p>
        \(status)
        \(footer)
        </body></html>
        """
    }
}
