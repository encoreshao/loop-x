import Foundation

/// What the dashboard page asks the app to share (the Creator page's Share buttons): a finished video and/or its
/// caption, handed to the native share sheet (AirDrop, Messages, Mail, ...). The page is only the local backend's,
/// but it renders text written by other parties, so the app never trusts the message: the file must be a small
/// set of media types and sit under `root` once symlinks are resolved, and the text is cut to a sane length.
public struct ShareRequest: Equatable {
    public let fileURL: URL?
    public let text: String?

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
        return ShareRequest(fileURL: file, text: text)
    }
}
