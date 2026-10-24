import AppKit
import Foundation
import LoopXKit

enum Connection: Equatable {
    case starting
    case connected
    case reconnecting(String)
    case failed(String)

    var text: String {
        switch self {
        case .starting: return "Starting backend…"
        case .connected: return "Connected"
        case .reconnecting(let r): return "Reconnecting (\(r))"
        case .failed(let m): return m
        }
    }
}

struct TakeoverOffer: Equatable {
    var running: LockFile
    var runningVersion: String
    var bundledVersion: String
}

/// All live state the UI renders. Pure decisions live in LoopXKit; this only wires them to the world.
@MainActor
final class AppModel: ObservableObject {
    @Published private(set) var snapshot = Snapshot()
    @Published private(set) var notch = NotchState.idle
    @Published private(set) var connection: Connection = .starting
    @Published private(set) var lock: LockFile?
    @Published var takeoverOffer: TakeoverOffer?
    @Published var actionError: String?
    @Published private(set) var busyApprovals: Set<String> = []

    let settings: SettingsStore
    let backendConfig: BackendConfig
    private var dismissed = Set<String>()
    private var sse: SSEClient?
    private var streamTask: Task<Void, Never>?
    private var recoveryTask: Task<Void, Never>?
    private var ticker: Timer?

    var openDashboard: () -> Void = {}
    var onNotchChange: (NotchState, NotchState) -> Void = { _, _ in }

    init(settings: SettingsStore = SettingsStore(), backendConfig: BackendConfig = .detect()) {
        self.settings = settings
        self.backendConfig = backendConfig
    }

    var dashboardURL: URL? { lock?.baseURL.appendingPathComponent("") }

    // MARK: lifecycle

    func start() {
        guard streamTask == nil else { return }
        ticker = Timer.scheduledTimer(withTimeInterval: 5, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.recompute() }   // lets old alerts age out
        }
        streamTask = Task { [weak self] in
            guard let self else { return }
            await self.ensureBackend()
            await self.consumeEvents()
        }
    }

    func stop() {
        streamTask?.cancel(); streamTask = nil
        recoveryTask?.cancel(); ticker?.invalidate(); ticker = nil
    }

    func ensureBackend() async {
        let supervisor = backendConfig.makeSupervisor()
        apply(await supervisor.ensureBackend())
    }

    func acceptTakeover() {
        guard let offer = takeoverOffer else { return }
        takeoverOffer = nil
        connection = .starting
        Task {
            let result = await backendConfig.makeSupervisor().takeOver(running: offer.running)
            apply(result)
        }
    }

    func declineTakeover() {
        // Keep using the older backend: it speaks the same contract.
        guard let offer = takeoverOffer else { return }
        takeoverOffer = nil
        lock = offer.running
    }

    private func apply(_ result: SupervisorResult) {
        switch result {
        case .ready(let l, _): lock = l; connection = .starting
        case .takeoverOffered(let running, let rv, let bv):
            takeoverOffer = TakeoverOffer(running: running, runningVersion: rv, bundledVersion: bv)
            lock = running      // keep working against it until the user decides
        case .failed(let m): connection = .failed(m)
        }
    }

    private func consumeEvents() async {
        let discovery = backendConfig.discovery
        let client = SSEClient(endpoint: { discovery.liveEventsURL() })
        sse = client
        for await event in client.events() {
            if Task.isCancelled { break }
            switch event {
            case .connected:
                connection = .connected
                lock = LockFile.readLive(at: discovery.lockURL) ?? lock
            case .snapshot(let s): update(s)
            case .disconnected(let reason):
                connection = .reconnecting(reason)
                scheduleRecovery()
            }
        }
    }

    /// The SSE client keeps retrying on its own; if no live backend exists for a while, start one.
    private func scheduleRecovery() {
        guard recoveryTask == nil else { return }
        recoveryTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: 4_000_000_000)
            guard let self, !Task.isCancelled else { return }
            if self.backendConfig.discovery.liveEventsURL() == nil, self.takeoverOffer == nil {
                await self.ensureBackend()
            }
            self.recoveryTask = nil
        }
    }

    // MARK: state

    private func update(_ s: Snapshot) {
        snapshot = s
        recompute()
    }

    func recompute(now: Date = Date()) {
        let old = notch
        let new = NotchState.make(from: snapshot, now: now, dismissed: dismissed)
        guard new != old else { return }
        notch = new
        onNotchChange(old, new)
    }

    // MARK: actions

    func perform(_ action: NotchAction) {
        switch action {
        case .approve(let id): decide(id, .approve)
        case .deny(let id): decide(id, .deny)
        case .openMergeRequest(let url): NSWorkspace.shared.open(url)
        case .openDashboard: openDashboard()
        case .dismiss(let key): dismissed.insert(key); recompute()
        }
    }

    func decide(_ id: String, _ decision: ApprovalDecision) {
        guard !busyApprovals.contains(id) else { return }
        guard let live = LockFile.readLive(at: backendConfig.discovery.lockURL) ?? lock else {
            actionError = ApprovalError.unreachable("no backend").message; return
        }
        busyApprovals.insert(id); actionError = nil
        let api = ApprovalsAPI(lock: live)
        Task {
            do { _ = try await api.decide(id: id, decision) }
            catch let e as ApprovalError {
                // 409/404 mean someone else decided it first: the next snapshot clears the card.
                actionError = e.message
            } catch { actionError = error.localizedDescription }
            busyApprovals.remove(id)
        }
    }
}
