import CoreGraphics
import Foundation

/// What the dashboard page asks the app to share (the Creator page's Share buttons): a finished video and/or its
/// caption, handed to the native share sheet (AirDrop, Messages, Mail, ...). The page is only the local backend's,
/// but it renders text written by other parties, so the app never trusts the message: the file must be a small
/// set of media types and sit under `root` once symlinks are resolved, and the text is cut to a sane length.
public struct ShareRequest: Equatable {
    public let fileURL: URL?
    public let text: String?
    /// Where the clicked Share button is, in page coordinates (the web view's own, top-left origin), so the share
    /// sheet opens next to it; nil when the page did not say or sent nonsense.
    public let anchor: CGRect?

    public static let allowedExtensions: Set<String> = ["mp4", "png", "jpg", "jpeg"]
    public static let maxTextLength = 5000

    /// The items for `NSSharingServicePicker`: the file first, then the text.
    public var items: [Any] {
        var out: [Any] = []
        if let fileURL { out.append(fileURL) }
        if let text { out.append(text) }
        return out
    }

    /// `body` is the page's `{path, text}` message; nil when it is malformed, names a file outside `root` or of
    /// another type, or has nothing to share.
    public static func parse(
        _ body: Any, root: URL,
        fileExists: (URL) -> Bool = { FileManager.default.fileExists(atPath: $0.path) }
    ) -> ShareRequest? {
        guard let dict = body as? [String: Any],
              let path = dict["path"] as? String,
              let rawText = dict["text"] as? String else { return nil }
        let trimmed = rawText.trimmingCharacters(in: .whitespacesAndNewlines)
        let text = trimmed.isEmpty ? nil : String(trimmed.prefix(maxTextLength))
        var file: URL?
        if !path.isEmpty {
            let url = URL(fileURLWithPath: path).standardizedFileURL.resolvingSymlinksInPath()
            let base = root.standardizedFileURL.resolvingSymlinksInPath().path
            guard url.path.hasPrefix(base + "/"),
                  allowedExtensions.contains(url.pathExtension.lowercased()),
                  fileExists(url) else { return nil }
            file = url
        }
        guard file != nil || text != nil else { return nil }
        return ShareRequest(fileURL: file, text: text, anchor: anchor(dict))
    }

    /// The button rectangle from `x`, `y`, `width`, `height`: finite numbers, not negative in size, and within a
    /// screen-sized range (a page cannot place the sheet anywhere odd).
    private static func anchor(_ dict: [String: Any]) -> CGRect? {
        func number(_ key: String) -> Double? {
            guard let n = dict[key] as? NSNumber else { return nil }
            let value = n.doubleValue
            return value.isFinite && abs(value) <= 100_000 ? value : nil
        }
        guard let x = number("x"), let y = number("y"),
              let w = number("width"), let h = number("height"), w >= 0, h >= 0 else { return nil }
        return CGRect(x: x, y: y, width: w, height: h)
    }
}
