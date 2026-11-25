import AppKit
import Foundation
import LoopXKit

@MainActor
final class IslandModel: ObservableObject {
    @Published var nav = IslandNav() { didSet { onNavChange(oldValue, nav) } }
    @Published private(set) var reply: ChatReply?
    @Published var status: String?
    @Published var confirmRun: String?
    enum Voice { case idle, recording, transcribing }
    @Published private(set) var voice: Voice = .idle
    @Published private(set) var voiceLevels: [Float] = []
    @Published private(set) var voiceStarted = Date()
    private var meterTask: Task<Void, Never>?
    private var transcribeTask: Task<Void, Never>?
    private let dictation = Dictation()
    private var session: String?
    private var streamTask: Task<Void, Never>?
    private var streamID = UUID()
    let app: AppModel
    var onNavChange: (IslandNav, IslandNav) -> Void = { _, _ in }

    init(app: AppModel) {
        self.app = app
        dictation.onFinish = { [weak self] url in self?.transcribe(url) }
    }

    private var api: IslandAPI? { app.lock.map { IslandAPI(lock: $0) } }

    func open(_ id: String) { nav.open(id) }
    func back() { nav.back() }
    func pin() { nav.pin() }
    func unpin() { cancelDictation(); streamTask?.cancel(); streamID = UUID(); reply = nil; status = nil; confirmRun = nil; session = nil; nav.unpin() }

    func detail(for taskID: String) -> (NotchTask, IslandDetail)? {
        guard let task = app.notch.tasks.first(where: { $0.id == taskID }) else { return nil }
        return (task, IslandDetail.make(task: task, events: app.snapshot.recentEvents, attention: app.snapshot.attention))
    }

    func send() {
        guard !nav.isReplying else { return }   // one reply at a time: a second send would start a second assistant run
        guard let api else { status = IslandAPIError.unreachable("no backend").message; return }
        let text = nav.inputText
        status = nil
        nav.isReplying = true
        Task {
            do {
                let start = try await api.startChat(text: text, session: session)
                session = start.session
                nav.inputText = ""
                reply = ChatReply()
                stream(api.streamURL(replyKey: start.replyKey))
            } catch let e as IslandAPIError { status = e.message; nav.isReplying = false }
            catch { status = error.localizedDescription; nav.isReplying = false }
        }
    }

    /// Mic button: start recording, or stop and put the transcript into the input field.
    func toggleDictation() {
        switch voice {
        case .recording: dictation.stop()
        case .transcribing: return
        case .idle:
            status = nil
            Task {
                guard await Dictation.requestAccess() else {
                    status = "Microphone access is off for LoopX. Allow it in System Settings > Privacy & Security > Microphone."
                    return
                }
                do {
                    try dictation.start()
                    voiceLevels = []
                    voiceStarted = Date()
                    voice = .recording
                    meter()
                } catch { status = "Couldn't start recording: \(error.localizedDescription)" }
            }
        }
    }

    func cancelDictation() {
        meterTask?.cancel()
        transcribeTask?.cancel()
        dictation.cancel()
        voice = .idle
    }

    private func meter() {
        meterTask?.cancel()
        meterTask = Task { [weak self] in
            while let self, !Task.isCancelled, self.voice == .recording {
                self.voiceLevels.append(self.dictation.level())
                if self.voiceLevels.count > 120 { self.voiceLevels.removeFirst(self.voiceLevels.count - 120) }
                try? await Task.sleep(for: .milliseconds(50))
            }
        }
    }

    private func transcribe(_ file: URL?) {
        meterTask?.cancel()
        guard let file else { voice = .idle; status = "The recording stopped unexpectedly."; return }
        defer { try? FileManager.default.removeItem(at: file) }
        guard let api, let audio = try? Data(contentsOf: file) else { voice = .idle; return }
        voice = .transcribing
        transcribeTask = Task {
            do {
                let text = try await api.transcribe(audio: audio, contentType: Dictation.contentType)
                guard !Task.isCancelled else { return }
                let current = nav.inputText
                nav.inputText = current.isEmpty || current.hasSuffix(" ") ? current + text : current + " " + text
            } catch let e as IslandAPIError { if !Task.isCancelled { status = e.message } }
            catch { if !Task.isCancelled { status = error.localizedDescription } }
            if !Task.isCancelled { voice = .idle }
        }
    }

    private func stream(_ url: URL) {
        streamTask?.cancel()
        let id = UUID()
        streamID = id
        streamTask = Task {
            var decoder = ChatStreamDecoder()
            do {
                let (bytes, _) = try await URLSession(configuration: .ephemeral).bytes(from: url)
                for try await byte in bytes {
                    for event in decoder.feed(byte) { reply?.apply(event) }
                }
            } catch {}
            // A cancelled or superseded stream must not touch the next reply's state.
            guard streamID == id, !Task.isCancelled else { return }
            reply?.streamEnded()
            nav.isReplying = false
        }
    }

    func decide(approve: Bool) {
        guard let api, let card = reply?.pending, let session else { return }
        Task {
            do { try await api.decideChat(id: card.id, session: session, approve: approve); reply?.clearPending(); status = approve ? "Approved." : "Rejected." }
            catch let e as IslandAPIError {
                if case .expired = e { reply?.clearPending() }   // a network error leaves the card to retry
                status = e.message
            }
            catch { status = error.localizedDescription }
        }
    }

    func requestRun(_ loop: String) { confirmRun = loop }
    func cancelRun() { confirmRun = nil }
    func confirmRunNow() {
        guard let loop = confirmRun, let api else { return }
        confirmRun = nil
        Task {
            do { try await api.runLoop(loop); status = "Started \(loop)." }
            catch let e as IslandAPIError { status = e.message }
            catch { status = error.localizedDescription }
        }
    }

    func perform(_ chip: IslandChip) {
        switch chip {
        case .openMergeRequest(let url): NSWorkspace.shared.open(url)
        case .openDashboard: app.openDashboard()
        case .runAgain(let loop): requestRun(loop)
        case .dismiss: unpin()
        }
    }
}
