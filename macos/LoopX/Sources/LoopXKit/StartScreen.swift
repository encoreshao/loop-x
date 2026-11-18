import Foundation

/// The page shown until a backend answers. One focal point, the LoopX mark, and out of the way fast.
/// Light ("Quiet"): the mark and name on a plain system background, a thin blue progress bar, the
/// status line. Dark ("Glow stage"): the mark larger on a dark stage, a soft halo breathing behind
/// it and a light sheen passing over the mark only, a small pulsing dot with the status near the
/// bottom. A failure swaps the bar and dot for the reason and a Try again button. Only the version
/// sits in the footer.
/// Under Reduce Motion the entrance, halo and sheen are off; the bar and the dot keep moving, since
/// their motion is the sign that startup is still going.
public enum StartScreen {
    /// How long the app holds this page on launch, even with a backend already up, so the entrance
    /// (mark .5s, name .5s after .1s) finishes before the dashboard loads.
    public static let introSeconds = 0.7

    /// `mark` is the brand mark image's URL (a data URL); empty shows no mark. `name` is the brand
    /// name's markup (the name images); empty falls back to the plain text "LoopX".
    public static func html(_ text: String, failed: Bool, version: String = "", mark: String = "", name: String = "") -> String {
        func escape(_ t: String) -> String {
            t.replacingOccurrences(of: "&", with: "&amp;").replacingOccurrences(of: "<", with: "&lt;")
                .replacingOccurrences(of: "'", with: "&#39;")
        }
        let status = failed
            ? "<div class='fail'><div class='err'>\(escape(text))</div><a class='retry' href='loopx-retry://now'>Try again</a></div>"
            : "<div class='bar'></div><div class='status'><span class='dot'></span><span class='msg'>\(escape(text))</span></div>"
        let markHTML = mark.isEmpty ? "" : """
            <div class='stage'><i class='halo'></i><img class='mark' src='\(escape(mark))' alt=''>\
            <i class='sheen' style="-webkit-mask-image:url('\(escape(mark))');mask-image:url('\(escape(mark))')"></i></div>
            """
        let footer = version.isEmpty ? "" : "<footer>v\(escape(version))</footer>"
        return """
        <html><head><meta name='color-scheme' content='light dark'><style>
        .\(TitlebarChrome.dragStripClass){position:fixed;top:0;left:0;right:0;height:28px;z-index:10}
        :root{--bg:#f5f5f7;--fg:#1d1d1f;--sub:#6e6e73;--accent:#0a84ff;--track:rgba(0,0,0,.08);--err:#c0392b;--mark:104px}
        @media(prefers-color-scheme:dark){:root{--bg:#0b0b10;--fg:#f5f5f7;--sub:rgba(245,245,247,.55);--accent:#a78bfa;--track:rgba(255,255,255,.1);--err:#ff6b5e;--mark:180px}}
        html{background:var(--bg)}
        @media(prefers-color-scheme:dark){html{background:radial-gradient(ellipse at 50% 42%,#1b1530 0%,#0b0b10 70%) fixed,#0b0b10}}
        html,body{height:100%;margin:0;overflow:hidden}
        body{font:13px -apple-system,system-ui;color:var(--sub);display:flex;flex-direction:column;align-items:center;justify-content:center;gap:14px;text-align:center;padding:0 24px;box-sizing:border-box}
        .stage{position:relative;width:var(--mark);height:var(--mark);display:flex;align-items:center;justify-content:center}
        .mark{position:relative;width:100%;height:100%;object-fit:contain;display:block}
        .halo,.sheen{display:none}
        h1{margin:0;font-size:28px;font-weight:700;color:var(--fg);line-height:1}
        h1 .brand-name{display:block;height:30px;width:auto;margin:0 auto}h1 .brand-name.on-dark{display:none}
        .bar{position:relative;width:120px;height:3px;border-radius:2px;background:var(--track);overflow:hidden;margin-top:8px}
        .bar::after{content:'';position:absolute;top:0;left:-40%;width:40%;height:100%;border-radius:2px;background:var(--accent);animation:slide 1.4s ease-in-out infinite}
        .status{display:flex;align-items:center;gap:8px;font-size:12.5px}
        .dot{display:none;width:6px;height:6px;border-radius:50%;background:var(--accent);animation:pulse 1.2s ease-in-out infinite}
        .fail{display:flex;flex-direction:column;align-items:center;gap:12px;margin-top:8px}
        .err{color:var(--err);max-width:420px;line-height:1.45;overflow-wrap:anywhere}
        .retry{display:inline-flex;align-items:center;height:30px;padding:0 16px;border-radius:15px;background:var(--accent);color:#fff;font-weight:600;text-decoration:none;transition:filter .2s ease-out}
        .retry:hover{filter:brightness(1.08)}
        footer{position:fixed;bottom:16px;left:0;right:0;font-size:11.5px;color:var(--sub);opacity:.7}
        @media(prefers-color-scheme:dark){
        .halo{display:block;position:absolute;inset:-30%;border-radius:50%;background:radial-gradient(circle,rgba(150,90,255,.42),rgba(255,70,140,.16) 45%,transparent 70%);filter:blur(12px);opacity:.75}
        .sheen{display:block;position:absolute;inset:0;-webkit-mask-size:contain;mask-size:contain;-webkit-mask-repeat:no-repeat;mask-repeat:no-repeat;-webkit-mask-position:center;mask-position:center;background:linear-gradient(105deg,transparent 35%,rgba(255,255,255,.55) 50%,transparent 65%) no-repeat;background-size:250% 100%;background-position:150% 0;mix-blend-mode:overlay}
        h1 .brand-name.on-light{display:none}h1 .brand-name.on-dark{display:block}
        .bar{display:none}.dot{display:block}
        .status{position:fixed;bottom:44px;left:0;right:0;justify-content:center}
        .fail{margin-top:12px}}
        @media(prefers-reduced-motion:no-preference){
        .stage{animation:enter .5s cubic-bezier(.22,1,.36,1) both}
        h1{animation:enter .5s cubic-bezier(.22,1,.36,1) .1s both}
        .halo{animation:breathe 3.6s ease-in-out infinite}
        .sheen{animation:sheen 3.2s ease-in-out .6s infinite}}
        @keyframes enter{from{opacity:0;transform:scale(.96)}}
        @keyframes slide{to{left:100%}}
        @keyframes pulse{50%{opacity:.35;transform:scale(.8)}}
        @keyframes breathe{50%{opacity:1;transform:scale(1.08)}}
        @keyframes sheen{0%{background-position:150% 0}55%,100%{background-position:-50% 0}}
        </style></head><body><div class='\(TitlebarChrome.dragStripClass)'></div>
        \(markHTML)<h1>\(name.isEmpty ? "LoopX" : name)</h1>
        \(status)
        \(footer)
        </body></html>
        """
    }
}
