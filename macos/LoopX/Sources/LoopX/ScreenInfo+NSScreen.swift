import AppKit
import LoopXKit

extension ScreenInfo {
    init(_ s: NSScreen) {
        let id = (s.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.intValue ?? 0
        self.init(id: id, frame: s.frame, visibleFrame: s.visibleFrame, safeAreaTop: s.safeAreaInsets.top,
                  auxiliaryTopLeft: s.auxiliaryTopLeftArea, auxiliaryTopRight: s.auxiliaryTopRightArea)
    }

    static func current() -> [ScreenInfo] { NSScreen.screens.map(ScreenInfo.init) }
}

extension NSScreen {
    var loopxID: Int { (deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.intValue ?? 0 }
}
