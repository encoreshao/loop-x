import Foundation

/// The page shown until a backend answers: the logo over a water scene (soft ripples spreading from it,
/// drifting waves, a slow colour wash), the name, what LoopX does, one status line (a spinner while starting,
/// the reason and a Retry link when it failed) and the author. Follows the system appearance.
/// Under Reduce Motion the ripples and waves still run, but small, slow and faint; the larger ripples, faster
/// waves, wash drift, entrance, float and glow are gated on it. The orbiting ring and the spinner are never
/// gated, since their motion is the sign that startup is still going.
public enum StartScreen {
    public static func html(_ text: String, failed: Bool, version: String = "", logo: String) -> String {
        func escape(_ t: String) -> String {
            t.replacingOccurrences(of: "&", with: "&amp;").replacingOccurrences(of: "<", with: "&lt;")
        }
        let status = failed
            ? "<div class='msg err'>\(escape(text))</div><a class='retry' href='loopx-retry://now'>Try again</a>"
            : "<div class='row'><span class='spin'></span><span class='msg'>\(escape(text))</span></div>"
        let versionLine = version.isEmpty ? "" : "<span>v\(escape(version))</span><span class='dot'>&middot;</span>"
        let wave = "<path d='M0,60 C240,110 480,10 720,60 C960,110 1200,10 1440,60 C1680,110 1920,10 2160,60 C2400,110 2640,10 2880,60 L2880,120 L0,120Z'/>"
        return """
        <html><head><meta name='color-scheme' content='light dark'><style>
        :root{--fg:#1d1d1f;--sub:#6e6e73;--err:#c0392b;--bg:#fff;--accent:#4f6df5;--pink:#e0457b;--glow:rgba(79,109,245,.22);--wash1:rgba(79,109,245,.10);--wash2:rgba(224,69,123,.08)}
        @media(prefers-color-scheme:dark){:root{--fg:#f5f5f7;--sub:#98989d;--err:#ff6b5e;--bg:#1e1e1e;--accent:#7aa2ff;--pink:#ff6fa5;--glow:rgba(122,162,255,.28);--wash1:rgba(122,162,255,.14);--wash2:rgba(255,111,165,.10)}}
        html{background:var(--bg)}
        html,body{height:100%;margin:0;overflow:hidden}
        body{background:transparent}
        body{font:14px -apple-system,system-ui;color:var(--sub);display:flex;flex-direction:column;align-items:center;justify-content:center;gap:16px;text-align:center}
        .water{position:fixed;inset:0;z-index:-1;pointer-events:none;overflow:hidden;background:linear-gradient(180deg,transparent 40%,var(--wash1))}
        .water::before,.water::after{content:'';position:absolute;width:60vmax;height:60vmax;border-radius:50%;filter:blur(60px)}
        .water::before{left:-20vmax;top:-25vmax;background:var(--wash1)}
        .water::after{right:-22vmax;bottom:-30vmax;background:var(--wash2)}
        .wave{position:absolute;left:0;bottom:0;width:200%;height:110px;fill:var(--accent);opacity:.10}
        .w2{height:84px;bottom:-6px;fill:var(--pink);opacity:.08}
        .logo{position:relative;width:240px;height:240px;margin:36px 0}
        .ripple{position:absolute;left:50%;top:50%;width:240px;height:240px;margin:-120px 0 0 -120px;border-radius:50%;border:1.5px solid var(--accent);opacity:0;animation:ripple 9s ease-out infinite}
        .r2{animation-delay:2.25s}.r3{animation-delay:4.5s}.r4{animation-delay:6.75s}
        .w1{animation:wave 40s linear infinite}
        .w2{animation:wave 60s linear infinite reverse}
        .track,.ring{position:absolute;inset:-32px;border-radius:50%}
        .track{border:1.5px dashed var(--glow)}
        .ring{animation:orbit 5s linear infinite;background:conic-gradient(from 0deg,transparent 0 50%,var(--accent) 82%,var(--pink));-webkit-mask:radial-gradient(farthest-side,transparent calc(100% - 3px),#000 calc(100% - 2px));filter:drop-shadow(0 0 6px var(--accent))}
        .logo::before{content:'';position:absolute;inset:-18%;border-radius:50%;background:radial-gradient(circle,var(--glow),transparent 68%)}
        .logo svg{position:relative;width:240px;height:240px;display:block}
        @media(prefers-reduced-motion:no-preference){
        .ripple{animation:rippleBig 6s ease-out infinite}
        .r2{animation-delay:1.5s}.r3{animation-delay:3s}.r4{animation-delay:4.5s}
        .w1{animation:wave 14s linear infinite}
        .w2{animation:wave 22s linear infinite reverse}
        .water::before{animation:drift 18s ease-in-out infinite alternate}
        .water::after{animation:drift 24s ease-in-out infinite alternate-reverse}
        .logo::before{animation:glow 3.2s ease-in-out infinite}
        .logo svg{animation:pop .9s cubic-bezier(.2,.9,.3,1.2) both,float 5s ease-in-out .9s infinite}
        h1{animation:rise .7s ease-out .25s both}
        .tag{animation:rise .7s ease-out .45s both}
        .row,.err,.retry{animation:rise .7s ease-out .65s both}
        footer{animation:rise .9s ease-out .9s both}}
        @keyframes orbit{to{transform:rotate(360deg)}}
        @keyframes ripple{from{transform:scale(.95);opacity:.32}to{transform:scale(2.4);opacity:0}}
        @keyframes rippleBig{from{transform:scale(.9);opacity:.45}to{transform:scale(4.6);opacity:0}}
        @keyframes wave{to{transform:translateX(-50%)}}
        @keyframes drift{to{transform:translate(8vmax,6vmax) scale(1.15)}}
        @keyframes glow{50%{transform:scale(1.12);opacity:.65}}
        @keyframes pop{from{opacity:0;transform:scale(.6) rotate(-14deg)}}
        @keyframes float{50%{transform:translateY(-10px)}}
        @keyframes rise{from{opacity:0;transform:translateY(12px)}}
        h1{margin:0;font-size:44px;font-weight:700;color:var(--fg);letter-spacing:.3px}
        .tag{margin:-6px 0 0;font-size:16px;color:var(--sub);max-width:460px;line-height:1.45}
        .row{display:flex;align-items:center;gap:10px;margin-top:6px}
        .spin{width:14px;height:14px;border:2px solid var(--sub);border-top-color:var(--accent);border-radius:50%;animation:s 0.9s linear infinite}
        @keyframes s{to{transform:rotate(360deg)}}
        .err{color:var(--err);max-width:420px;line-height:1.4}
        .retry{color:var(--accent);text-decoration:none;font-weight:600;padding:6px 14px;border:1.5px solid transparent;border-radius:8px;background:linear-gradient(var(--bg),var(--bg)) padding-box,linear-gradient(90deg,var(--accent),var(--pink)) border-box;transition:box-shadow .2s}
        .retry:hover{box-shadow:0 0 14px var(--glow)}
        footer{position:fixed;bottom:22px;left:0;right:0;font-size:12.5px;line-height:1.7}
        footer::before{content:'';display:block;width:140px;height:1px;margin:0 auto 10px;background:linear-gradient(90deg,transparent,var(--glow),transparent)}
        footer .dot{margin:0 8px}
        footer a{color:var(--accent);text-decoration:none}
        </style></head><body>
        <div class='water'><svg class='wave w1' viewBox='0 0 2880 120' preserveAspectRatio='none'>\(wave)</svg><svg class='wave w2' viewBox='0 0 2880 120' preserveAspectRatio='none'>\(wave)</svg></div>
        <div class='logo'><i class='ripple r1'></i><i class='ripple r2'></i><i class='ripple r3'></i><i class='ripple r4'></i><i class='track'></i><i class='ring'></i>\(logo)</div><h1>LoopX</h1>
        <p class='tag'>Get your time back from the daily grind.</p>
        \(status)
        <footer>\(versionLine)<span>Created by <strong>Encore Shao</strong></span><span class='dot'>&middot;</span><a href='mailto:encore.shao@gmail.com'>encore.shao@gmail.com</a></footer>
        </body></html>
        """
    }
}
