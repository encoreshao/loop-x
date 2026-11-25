import AVFoundation
import Foundation

/// Records the microphone to a temporary AAC file for the island's voice input. `onFinish`
/// gets the file once recording stops (by `stop()` or the time limit), `nil` when recording
/// failed, and is never called after `cancel()`.
@MainActor
final class Dictation: NSObject, AVAudioRecorderDelegate {
    static let maxSeconds: TimeInterval = 120
    static let contentType = "audio/mp4"

    private var recorder: AVAudioRecorder?
    private var cancelled = false
    var onFinish: (URL?) -> Void = { _ in }

    /// Asks for microphone access the first time; `false` when it is denied.
    static func requestAccess() async -> Bool {
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized: return true
        case .notDetermined: return await AVCaptureDevice.requestAccess(for: .audio)
        default: return false
        }
    }

    func start() throws {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("loopx-dictation-\(UUID().uuidString).m4a")
        let settings: [String: Any] = [
            AVFormatIDKey: kAudioFormatMPEG4AAC,
            AVSampleRateKey: 16_000,
            AVNumberOfChannelsKey: 1,
            AVEncoderAudioQualityKey: AVAudioQuality.high.rawValue,
        ]
        let r = try AVAudioRecorder(url: url, settings: settings)
        r.delegate = self
        cancelled = false
        guard r.record(forDuration: Self.maxSeconds) else { throw CocoaError(.fileWriteUnknown) }
        recorder = r
    }

    func stop() { recorder?.stop() }

    func cancel() {
        cancelled = true
        recorder?.stop()
    }

    nonisolated func audioRecorderDidFinishRecording(_ recorder: AVAudioRecorder, successfully flag: Bool) {
        let url = recorder.url
        Task { @MainActor in
            self.recorder = nil
            if self.cancelled || !flag {
                try? FileManager.default.removeItem(at: url)
                if !self.cancelled { self.onFinish(nil) }
                return
            }
            self.onFinish(url)
        }
    }
}
