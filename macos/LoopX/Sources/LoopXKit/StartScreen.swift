import Foundation

/// The page shown until a backend answers. Light and dark share one layout: the LoopX mark large
/// over a soft halo, the name, the headline "Hello, future." in the brand gradient and the mission line with its keywords picked out, then a thin progress bar and the status line; only the version sits in the footer.
/// Light is a pale backdrop with a blue-pink halo and a richer mark so it stands out; dark is the
/// glow stage, a violet halo breathing and a light sheen passing over the mark only. A failure swaps
/// the bar for the reason and a Try again button.
/// Under Reduce Motion the entrance, halo and sheen are off; the bar keeps moving, since its motion
/// is the sign that startup is still going.
public enum StartScreen {
    /// How long the app holds this page once it has loaded, even with a backend already up, so the
    /// entrance (mark, name and the two lines, .5s each, staggered by .1s) finishes before the dashboard loads.
    public static let introSeconds = 0.9

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
            <i class='sheen' style="-webkit-mask-image:url('\(escape(mark))');mask-image:url('\(escape(mark))')"></i></div>
            """
        let footer = version.isEmpty ? "" : "<footer>v\(escape(version))</footer>"
        return """
        <html><head><meta charset='utf-8'><meta name='color-scheme' content='light dark'><style>
        .\(TitlebarChrome.dragStripClass){position:fixed;top:0;left:0;right:0;height:28px;z-index:10}
        :root{--mark:min(220px,32vh);--title:clamp(30px,6vh,44px);--gap-l:clamp(18px,5vh,40px);--gap-m:clamp(14px,3.4vh,28px);--gap-s:clamp(12px,2.4vh,18px);--fg:#1d1d1f;--sub:#6e6e73;--accent:#0a84ff;--track:rgba(0,0,0,.08);--err:#c0392b;
        --grad:linear-gradient(90deg,#0a84ff,#7c4dff 50%,#e0457b);
        --halo:radial-gradient(circle,rgba(10,132,255,.20),rgba(224,69,123,.14) 45%,transparent 70%)}
        @media(prefers-color-scheme:dark){:root{--fg:#f5f5f7;--sub:rgba(245,245,247,.6);--accent:#a78bfa;--track:rgba(255,255,255,.1);--err:#ff6b5e;
        --grad:linear-gradient(90deg,#64d2ff,#b49cff 50%,#ff7eb3);
        --halo:radial-gradient(circle,rgba(150,90,255,.42),rgba(255,70,140,.16) 45%,transparent 70%)}}
        html{background:radial-gradient(ellipse at 50% 38%,#ffffff 0%,#f5f5f7 55%,#e9e9ee 100%) fixed,#f5f5f7}
        @media(prefers-color-scheme:dark){html{background:radial-gradient(ellipse at 50% 38%,#1b1530 0%,#0b0b10 70%) fixed,#0b0b10}}
        html,body{height:100%;margin:0;overflow:hidden}
        body{font:13px -apple-system,system-ui;color:var(--sub);display:flex;flex-direction:column;align-items:center;justify-content:center;gap:0;text-align:center;padding:0 24px 24px;box-sizing:border-box}
        .stage{position:relative;width:var(--mark);height:var(--mark);margin:0 0 var(--gap-l)}
        .mark{position:relative;width:100%;height:100%;object-fit:contain;display:block;filter:saturate(1.6) contrast(1.15) drop-shadow(0 6px 18px rgba(80,60,200,.18))}
        .halo{position:absolute;inset:-28%;border-radius:50%;background:var(--halo);filter:blur(14px);opacity:.8}
        .sheen{display:none}
        h1{margin:0 0 var(--gap-m);font-size:13px;font-weight:600;text-transform:uppercase;letter-spacing:.3em;color:var(--sub);line-height:1}
        h1 .brand-name{display:block;height:30px;width:auto;margin:0 auto}h1 .brand-name.on-dark{display:none}
        .line{margin:0;padding-bottom:.06em;font-size:var(--title);font-weight:700;letter-spacing:-.03em;line-height:1.3;color:var(--fg);background:var(--grad);-webkit-background-clip:text;background-clip:text;-webkit-text-fill-color:transparent}
        .sub{margin:var(--gap-s) auto 0;max-width:440px;font-size:15px;line-height:1.6;color:var(--sub);text-wrap:balance}
        .sub b{font-weight:600;color:var(--fg)}
        .bar{position:relative;width:140px;height:3px;border-radius:2px;background:var(--track);overflow:hidden;margin:var(--gap-l) 0 12px}
        .bar::after{content:'';position:absolute;top:0;left:-40%;width:40%;height:100%;border-radius:2px;background:var(--accent);animation:slide 1.4s ease-in-out infinite}
        .status{font-size:12.5px}
        .fail{display:flex;flex-direction:column;align-items:center;gap:12px;margin-top:var(--gap-l)}
        .err{color:var(--err);max-width:420px;line-height:1.45;overflow-wrap:anywhere}
        .retry{display:inline-flex;align-items:center;height:30px;padding:0 16px;border-radius:15px;background:var(--accent);color:#fff;font-weight:600;text-decoration:none;transition:filter .2s ease-out}
        .retry:hover{filter:brightness(1.08)}
        footer{position:fixed;bottom:16px;left:0;right:0;font-size:11.5px;color:var(--sub);opacity:.7}
        @media(prefers-color-scheme:dark){
        .mark{filter:none}
        .sheen{display:block;position:absolute;inset:0;mix-blend-mode:overlay;background:linear-gradient(105deg,transparent 35%,rgba(255,255,255,.55) 50%,transparent 65%) no-repeat;background-size:250% 100%;background-position:150% 0;
        -webkit-mask-size:contain;mask-size:contain;-webkit-mask-repeat:no-repeat;mask-repeat:no-repeat;-webkit-mask-position:center;mask-position:center}
        h1 .brand-name.on-light{display:none}h1 .brand-name.on-dark{display:block}}
        @media(prefers-reduced-motion:no-preference){
        .stage{animation:enter .5s cubic-bezier(.22,1,.36,1) both}
        h1{animation:enter .5s cubic-bezier(.22,1,.36,1) .1s both}
        .line{animation:enter .5s cubic-bezier(.22,1,.36,1) .2s both}
        .sub{animation:enter .5s cubic-bezier(.22,1,.36,1) .3s both}
        .halo{animation:breathe 3.6s ease-in-out infinite}
        .sheen{animation:sheen 3.2s ease-in-out .6s infinite}}
        @keyframes enter{from{opacity:0;transform:scale(.96)}}
        @keyframes slide{to{left:100%}}
        @keyframes breathe{50%{opacity:1;transform:scale(1.08)}}
        @keyframes sheen{0%{background-position:150% 0}55%,100%{background-position:-50% 0}}
        </style></head><body><div class='\(TitlebarChrome.dragStripClass)'></div>
        \(markHTML)<h1>\(name.isEmpty ? "LoopX" : name)</h1>
        <p class='line'>Hello, future.</p>
        <p class='sub'>We're building a world where <b>agents handle the busywork</b> and people do <b>what matters most</b>. It starts on your Mac.</p>
        \(status)
        \(footer)
        </body></html>
        """
    }
}
