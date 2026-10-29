import Foundation

/// The brand images for the start screen, read from the bundle's Resources (copied there by
/// build_loopx_app.sh from assets/brand/) and inlined as data URLs, since the page is loaded from a
/// string. Empty when the resources are missing (a bare `swift run`): the start screen then shows no
/// logo and the plain-text name.
enum LogoSVG {
    /// The app icon inside an SVG, so the start screen's `.logo svg` sizing and float apply; the
    /// `frag x2` group gives it the assembly fly-in.
    static let markup: String = {
        guard let icon = dataURL("brand-mark") else { return "" }
        return "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 1024 1024'><g class='frag x2'>"
            + "<image href='\(icon)' x='0' y='0' width='1024' height='1024'/></g></svg>"
    }()

    /// The "LoopX" name images: ink lettering for the light appearance, white for the dark one.
    static let name: String = {
        guard let light = dataURL("brand-name-light"), let dark = dataURL("brand-name") else { return "" }
        return "<img class='brand-name on-light' src='\(light)' alt='LoopX'>"
            + "<img class='brand-name on-dark' src='\(dark)' alt='LoopX'>"
    }()

    private static func dataURL(_ resource: String) -> String? {
        guard let url = Bundle.main.url(forResource: resource, withExtension: "png"),
              let data = try? Data(contentsOf: url) else { return nil }
        return "data:image/png;base64," + data.base64EncodedString()
    }
}
