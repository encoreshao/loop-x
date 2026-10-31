import SwiftUI
import LoopXKit

extension IslandTone {
    var color: Color {
        switch self {
        case .neutral: return .gray
        case .active: return .cyan
        case .success: return .green
        case .warning: return .orange
        case .failure: return .red
        }
    }
}

private func capsule(_ title: String, _ symbol: String, fill: Color = Color.white.opacity(0.14),
                     text: Color = .white, _ run: @escaping () -> Void) -> some View {
    Button(action: run) {
        Label(title, systemImage: symbol).font(.system(size: 11, weight: .medium))
            .padding(.horizontal, 10).padding(.vertical, 5)
            .background(Capsule().fill(fill))
    }.buttonStyle(.plain).foregroundStyle(text)
}

/// The pinned island: header, the current screen, the assistant's reply, and the input bar.
struct IslandRoot: View {
    @ObservedObject var island: IslandModel
    @ObservedObject var model: AppModel
    @ObservedObject var ui: NotchUI

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            header
            ScrollView(showsIndicators: false) {
                VStack(alignment: .leading, spacing: 10) {
                    switch island.nav.screen {
                    case .home: HomeView(island: island, model: model)
                    case .detail(let id): DetailView(island: island, taskID: id)
                    }
                    if island.reply != nil { ChatReplyView(island: island) }
                    if let loop = island.confirmRun { confirmCard(loop) }
                }
            }
            if let s = island.status { Text(s).font(.system(size: 10)).foregroundStyle(.white.opacity(0.6)).lineLimit(2) }
            InputBar(island: island, enabled: model.connection == .connected, placeholder: model.connection.text)
        }
        .padding(.horizontal, 18).padding(.bottom, 14).padding(.top, 6)
        .frame(width: ui.expandedSize.width, height: max(ui.expandedSize.height - ui.notchHeight, 0), alignment: .topLeading)
    }

    var header: some View {
        HStack(spacing: 8) {
            if island.nav.canGoBack {
                Button { island.back() } label: {
                    Image(systemName: "chevron.left").font(.system(size: 12, weight: .semibold))
                        .frame(width: 26, height: 26).background(Circle().fill(Color.white.opacity(0.12)))
                }.buttonStyle(.plain).foregroundStyle(.white)
            }
            Text(model.notch.title).font(.system(size: 13, weight: .semibold)).foregroundStyle(.white).lineLimit(1)
            Spacer(minLength: 0)
            Circle().fill(model.connection == .connected ? Color.green : Color.orange).frame(width: 6, height: 6)
                .help(model.connection.text)
            Button { island.unpin() } label: {
                Image(systemName: "xmark").font(.system(size: 11, weight: .semibold))
                    .frame(width: 26, height: 26).background(Circle().fill(Color.white.opacity(0.12)))
            }.buttonStyle(.plain).foregroundStyle(.white)
        }
    }

    func confirmCard(_ loop: String) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(IslandStarts.confirmText(loop: loop)).font(.system(size: 12)).foregroundStyle(.white)
            HStack(spacing: 8) {
                capsule("Allow", "checkmark", fill: Color.green.opacity(0.9), text: .black) { island.confirmRunNow() }
                capsule("Deny", "xmark", fill: Color.red.opacity(0.75)) { island.cancelRun() }
            }
        }
        .padding(10).background(RoundedRectangle(cornerRadius: 12).fill(Color.white.opacity(0.08)))
    }
}

struct HomeView: View {
    @ObservedObject var island: IslandModel
    @ObservedObject var model: AppModel

    var body: some View {
        let tasks = model.notch.tasks
        VStack(alignment: .leading, spacing: 6) {
            if tasks.isEmpty {
                Text("No loops or topics have run yet.").font(.system(size: 11)).foregroundStyle(.white.opacity(0.55))
            }
            ForEach(tasks) { t in
                Button { island.open(t.id) } label: { card(t) }.buttonStyle(.plain)
            }
            let starts = IslandStarts.available(tasks: tasks)
            if !starts.isEmpty {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 6) {
                        ForEach(starts, id: \.loop) { s in capsule(s.title, s.symbol) { island.requestRun(s.loop) } }
                    }
                }.padding(.top, 2)
            }
        }
    }

    func card(_ t: NotchTask) -> some View {
        HStack(spacing: 8) {
            StatusGlyph(style: TaskList.style(t.state), size: 13).frame(width: 18)
            VStack(alignment: .leading, spacing: 1) {
                Text(t.title).font(.system(size: 12, weight: .semibold)).foregroundStyle(.white).lineLimit(1)
                Text(t.detail ?? TaskList.label(t.state)).font(.system(size: 11))
                    .foregroundStyle(.white.opacity(0.55)).lineLimit(1)
            }
            Spacer(minLength: 0)
            Image(systemName: "chevron.right").font(.system(size: 10)).foregroundStyle(.white.opacity(0.35))
        }
        .padding(10).background(RoundedRectangle(cornerRadius: 12).fill(Color.white.opacity(0.08)))
    }
}

struct DetailView: View {
    @ObservedObject var island: IslandModel
    let taskID: String

    var body: some View {
        if let (task, detail) = island.detail(for: taskID) {
            VStack(alignment: .leading, spacing: 8) {
                HStack(spacing: 8) {
                    StatusGlyph(style: TaskList.style(task.state), size: 14).frame(width: 18)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(task.title).font(.system(size: 13, weight: .semibold)).foregroundStyle(.white)
                        Text(task.detail ?? TaskList.label(task.state)).font(.system(size: 11)).foregroundStyle(.white.opacity(0.55))
                    }
                }
                ForEach(detail.rows) { row in
                    HStack(spacing: 8) {
                        Text(row.time.map(Self.time) ?? "").font(.system(size: 11, weight: .semibold).monospacedDigit())
                            .foregroundStyle(.white.opacity(0.7)).frame(width: 42, alignment: .leading)
                        RoundedRectangle(cornerRadius: 1.5).fill(row.tone.color).frame(width: 3, height: 18)
                        Text(row.title).font(.system(size: 12)).foregroundStyle(.white).lineLimit(1)
                        Spacer(minLength: 0)
                    }
                    .padding(.horizontal, 10).padding(.vertical, 6)
                    .background(RoundedRectangle(cornerRadius: 10).fill(Color.white.opacity(0.07)))
                }
                if let r = detail.result { Text(r).font(.system(size: 12, weight: .medium)).foregroundStyle(.white) }
                HStack(spacing: 6) {
                    ForEach(Array(detail.chips.enumerated()), id: \.offset) { _, chip in
                        capsule(Self.title(chip), Self.symbol(chip)) { island.perform(chip) }
                    }
                }
            }
        } else {
            Text("This task is no longer running.").font(.system(size: 11)).foregroundStyle(.white.opacity(0.55))
        }
    }

    static func time(_ d: Date) -> String {
        let f = DateFormatter(); f.dateFormat = "h:mm a"; return f.string(from: d)
    }
    static func title(_ c: IslandChip) -> String {
        switch c {
        case .openMergeRequest: return "Open MR"
        case .openDashboard: return "Dashboard"
        case .runAgain: return "Run again"
        case .dismiss: return "Dismiss"
        }
    }
    static func symbol(_ c: IslandChip) -> String {
        switch c {
        case .openMergeRequest: return "arrow.up.right.square"
        case .openDashboard: return "macwindow"
        case .runAgain: return "arrow.clockwise"
        case .dismiss: return "xmark"
        }
    }
}

struct ChatReplyView: View {
    @ObservedObject var island: IslandModel

    var body: some View {
        if let r = island.reply {
            VStack(alignment: .leading, spacing: 8) {
                if !r.text.isEmpty { Text(r.text).font(.system(size: 12)).foregroundStyle(.white).textSelection(.enabled) }
                if let p = r.pending {
                    VStack(alignment: .leading, spacing: 6) {
                        Text(p.summary.isEmpty ? p.tool : p.summary).font(.system(size: 12, weight: .medium)).foregroundStyle(.white)
                        HStack(spacing: 8) {
                            capsule("Allow", "checkmark", fill: Color.green.opacity(0.9), text: .black) { island.decide(approve: true) }
                            capsule("Deny", "xmark", fill: Color.red.opacity(0.75)) { island.decide(approve: false) }
                        }
                    }
                    .padding(10).background(RoundedRectangle(cornerRadius: 12).fill(Color.white.opacity(0.08)))
                }
                switch r.phase {
                case .thinking:
                    HStack(spacing: 6) { Image(nsImage: NSApp.applicationIconImage).resizable().frame(width: 16, height: 16); Text("Thinking…"); ProgressView().controlSize(.small) }
                        .font(.system(size: 11, weight: .semibold)).foregroundStyle(.white.opacity(0.8))
                        .padding(.horizontal, 10).padding(.vertical, 5).background(Capsule().fill(Color.white.opacity(0.1)))
                case .failed(let m): Text(m).font(.system(size: 11)).foregroundStyle(.red).lineLimit(3)
                case .interrupted: Text("The reply was interrupted. Send again to retry.").font(.system(size: 11)).foregroundStyle(.orange)
                case .done: EmptyView()
                }
            }
        }
    }
}

struct InputBar: View {
    @ObservedObject var island: IslandModel
    var enabled = true
    var placeholder = ""
    @FocusState private var focused: Bool

    var body: some View {
        let blank = !enabled || island.nav.inputText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        HStack(spacing: 8) {
            TextField(enabled ? "Ask LoopX or tell it what to run…" : placeholder, text: $island.nav.inputText)
                .textFieldStyle(.plain).font(.system(size: 12)).foregroundStyle(.white)
                .disabled(!enabled).focused($focused).onSubmit { if !blank && !island.nav.isReplying { island.send() } }
            Button { island.send() } label: {
                Image(systemName: "arrow.up").font(.system(size: 11, weight: .bold))
                    .frame(width: 24, height: 24)
                    .background(Circle().fill(Color.white.opacity(blank || island.nav.isReplying ? 0.12 : 0.9)))
                    .foregroundStyle(blank || island.nav.isReplying ? Color.white.opacity(0.4) : .black)
            }.buttonStyle(.plain).disabled(blank || island.nav.isReplying)
        }
        .padding(.horizontal, 12).padding(.vertical, 8)
        .background(Capsule().fill(Color.white.opacity(0.1)))
        .onAppear { focused = true }
    }
}
