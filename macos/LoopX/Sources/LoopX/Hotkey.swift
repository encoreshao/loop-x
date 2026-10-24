import AppKit
import Carbon.HIToolbox
import LoopXKit

/// Global hotkey via Carbon `RegisterEventHotKey`: needs no Accessibility / Input Monitoring permission.
@MainActor
final class HotkeyManager {
    private var ref: EventHotKeyRef?
    private var handler: EventHandlerRef?
    var onPress: () -> Void = {}
    private(set) var registered: HotkeyPreset?
    private var box: Unmanaged<HotkeyManager>?

    func register(_ preset: HotkeyPreset) {
        unregister()
        guard preset.isEnabled else { return }
        var spec = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
        let me = Unmanaged.passUnretained(self); box = me
        InstallEventHandler(GetApplicationEventTarget(), { _, _, ctx in
            guard let ctx else { return noErr }
            let m = Unmanaged<HotkeyManager>.fromOpaque(ctx).takeUnretainedValue()
            DispatchQueue.main.async { MainActor.assumeIsolated { m.onPress() } }
            return noErr
        }, 1, &spec, me.toOpaque(), &handler)
        let id = EventHotKeyID(signature: OSType(0x4C4F4F50), id: 1)   // 'LOOP'
        let status = RegisterEventHotKey(preset.keyCode, preset.modifiers, id, GetApplicationEventTarget(), 0, &ref)
        registered = status == noErr ? preset : nil
    }

    func unregister() {
        if let ref { UnregisterEventHotKey(ref) }
        if let handler { RemoveEventHandler(handler) }
        ref = nil; handler = nil; registered = nil
    }
}
