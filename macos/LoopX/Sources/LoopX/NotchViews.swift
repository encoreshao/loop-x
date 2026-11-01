import SwiftUI
import LoopXKit

extension NotchAccent {
    var color: Color {
        switch self {
        case .neutral: return .gray
        case .blue: return .cyan
        case .orange: return .orange
        case .green: return .green
        case .red: return .red
        case .purple: return Color(red: 0.72, green: 0.52, blue: 1.0)
        }
    }
}

/// Layout facts and hover state the controller owns and the SwiftUI view reads.
@MainActor
final class NotchUI: ObservableObject {
    @Published var isExpanded = false
    @Published var collapsedSize = CGSize(width: 230, height: 34)
    @Published var expandedSize = CGSize(width: 400, height: 180)
    /// Height of the physical notch: content starts below it. 0 for the HUD.
    @Published var notchHeight: CGFloat = 0
    @Published var isHUD = false
    @Published var visible = true
    /// The user clicked the island open: it stays until closed and takes keyboard input.
    @Published var isPinned = false
}

/// Black pill that is flat at the top (it fuses with the screen edge/notch) and rounded below.
struct NotchShape: Shape {
    var bottomRadius: CGFloat
    var topRadius: CGFloat = 0
    var animatableData: AnimatablePair<CGFloat, CGFloat> {
        get { AnimatablePair(bottomRadius, topRadius) }
        set { bottomRadius = newValue.first; topRadius = newValue.second }
    }
    func path(in r: CGRect) -> Path {
        var p = Path()
        let b = min(bottomRadius, r.height / 2, r.width / 2), t = min(topRadius, r.height / 2, r.width / 2)
        p.move(to: CGPoint(x: r.minX + t, y: r.minY))
        p.addLine(to: CGPoint(x: r.maxX - t, y: r.minY))
        p.addArc(center: CGPoint(x: r.maxX - t, y: r.minY + t), radius: t, startAngle: .degrees(-90), endAngle: .degrees(0), clockwise: false)
        p.addLine(to: CGPoint(x: r.maxX, y: r.maxY - b))
        p.addArc(center: CGPoint(x: r.maxX - b, y: r.maxY - b), radius: b, startAngle: .degrees(0), endAngle: .degrees(90), clockwise: false)
        p.addLine(to: CGPoint(x: r.minX + b, y: r.maxY))
        p.addArc(center: CGPoint(x: r.minX + b, y: r.maxY - b), radius: b, startAngle: .degrees(90), endAngle: .degrees(180), clockwise: false)
        p.addLine(to: CGPoint(x: r.minX, y: r.minY + t))
        p.addArc(center: CGPoint(x: r.minX + t, y: r.minY + t), radius: t, startAngle: .degrees(180), endAngle: .degrees(270), clockwise: false)
        p.closeSubpath()
        return p
    }
}

struct NotchRootView: View {
    @ObservedObject var model: AppModel
    @ObservedObject var island: IslandModel
    @ObservedObject var ui: NotchUI
    static let spring = Animation.spring(response: 0.38, dampingFraction: 0.76)

    var body: some View {
        let size = ui.isExpanded ? ui.expandedSize : ui.collapsedSize
        ZStack(alignment: .top) {
            NotchShape(bottomRadius: ui.isExpanded ? 24 : (ui.isHUD ? 17 : 10), topRadius: ui.isHUD ? (ui.isExpanded ? 24 : 17) : 0)
                .fill(Color.black.opacity(ui.isHUD ? 0.92 : 1))
                .overlay(
                    NotchShape(bottomRadius: ui.isExpanded ? 24 : (ui.isHUD ? 17 : 10), topRadius: ui.isHUD ? (ui.isExpanded ? 24 : 17) : 0)
                        .stroke(model.notch.style.accent.color.opacity(ui.isExpanded ? 0.35 : 0.0), lineWidth: 1))
                .shadow(color: .black.opacity(ui.isExpanded ? 0.45 : 0), radius: 14, y: 6)
            if ui.isExpanded && ui.isPinned && model.notch.kind != .awaitingApproval {
                IslandRoot(island: island, model: model, ui: ui)
                    .padding(.top, ui.notchHeight)
                    .transition(.opacity.combined(with: .scale(scale: 0.92, anchor: .top)))
            } else if ui.isExpanded {
                ExpandedContent(model: model, island: island, ui: ui)
                    .padding(.top, ui.notchHeight)
                    .transition(.opacity.combined(with: .scale(scale: 0.92, anchor: .top)))
            } else {
                CollapsedContent(model: model, ui: ui)
                    .transition(.opacity)
            }
        }
        .frame(width: size.width, height: size.height, alignment: .top)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .animation(Self.spring, value: ui.isExpanded)
        .animation(Self.spring, value: model.notch.kind)
        .opacity(ui.visible ? 1 : 0)
        .preferredColorScheme(.dark)
    }
}

struct StatusGlyph: View {
    let style: NotchStyle
    var size: CGFloat = 14
    var body: some View {
        // Pulse is derived from the clock rather than @State, so it needs no stored view state.
        TimelineView(.animation(minimumInterval: 1.0 / 20, paused: !style.pulses)) { ctx in
            let phase = style.pulses ? (sin(ctx.date.timeIntervalSinceReferenceDate * 2 * .pi / 1.8) + 1) / 2 : 0
            Image(systemName: style.symbol)
                .font(.system(size: size, weight: .semibold))
                .foregroundStyle(style.accent.color)
                .opacity(1 - 0.55 * phase)
        }
        .accessibilityHidden(true)
    }
}

struct CollapsedContent: View {
    @ObservedObject var model: AppModel
    @ObservedObject var ui: NotchUI
    var body: some View {
        let n = model.notch
        if ui.isHUD {
            HStack(spacing: 8) {
                StatusGlyph(style: n.style)
                Text(n.title).font(.system(size: 12, weight: .medium)).foregroundStyle(.white).lineLimit(1)
                if n.kind == .running, let s = n.subtitle, !s.isEmpty {
                    Text(s).font(.system(size: 11)).foregroundStyle(.white.opacity(0.55)).lineLimit(1).truncationMode(.tail)
                }
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 14)
            .frame(height: ui.collapsedSize.height)
        } else {
            // Wings either side of the physical notch: glyph left, counter right.
            HStack {
                StatusGlyph(style: n.style).padding(.leading, 12)
                Spacer()
                let running = n.tasks.filter { $0.state == .running }.count
                if n.approvals.isEmpty, running > 1 {
                    Text("\(running)").font(.system(size: 11, weight: .bold, design: .rounded))
                        .foregroundStyle(n.style.accent.color).padding(.trailing, 12)
                }
                if n.approvals.count > 0 {
                    Text("\(n.approvals.count)").font(.system(size: 11, weight: .bold, design: .rounded))
                        .foregroundStyle(n.style.accent.color).padding(.trailing, 12)
                }
            }
            .frame(height: ui.collapsedSize.height)
        }
    }
}

struct ExpandedContent: View {
    @ObservedObject var model: AppModel
    @ObservedObject var island: IslandModel
    @ObservedObject var ui: NotchUI
    var body: some View {
        let n = model.notch
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                StatusGlyph(style: n.style, size: 16)
                VStack(alignment: .leading, spacing: 1) {
                    Text(n.title).font(.system(size: 13, weight: .semibold)).foregroundStyle(.white)
                    if let s = n.subtitle, !s.isEmpty {
                        Text(s).font(.system(size: 11)).foregroundStyle(.white.opacity(0.65)).lineLimit(2)
                    }
                }
                Spacer(minLength: 0)
                connectionDot
            }
            if n.kind == .awaitingApproval, let a = n.approvals.first {
                ApprovalCard(model: model, approval: a)
            } else {
                if case .reconnecting = model.connection { Text(model.connection.text).font(.system(size: 10)).foregroundStyle(.orange) }
                if case .failed(let m) = model.connection { Text(m).font(.system(size: 10)).foregroundStyle(.red).lineLimit(3) }
                LoopGrid(model: model, island: island, tasks: n.tasks)
                Spacer(minLength: 0)
                HStack(spacing: 8) {
                    let hidden = max(0, n.tasks.count - LoopGrid.maxTiles)
                    pill(hidden > 0 ? "Tasks +\(hidden)" : "Tasks", "list.bullet") { island.pin() }
                    ForEach(Array(n.actions.enumerated()), id: \.offset) { _, action in
                        actionButton(action)
                    }
                    Spacer(minLength: 0)
                }
            }
            if let e = model.actionError {
                Text(e).font(.system(size: 10)).foregroundStyle(.red).lineLimit(2)
            }
        }
        .padding(.horizontal, 18).padding(.bottom, 14).padding(.top, 6)
        .frame(width: ui.expandedSize.width, height: max(ui.expandedSize.height - ui.notchHeight, 0), alignment: .topLeading)
    }

    var connectionDot: some View {
        Circle().fill(model.connection == .connected ? Color.green : Color.orange).frame(width: 6, height: 6)
            .help(model.connection.text)
    }

    @ViewBuilder func actionButton(_ a: NotchAction) -> some View {
        switch a {
        case .openDashboard: pill("Dashboard", "macwindow") { model.perform(a) }
        case .openMergeRequest: pill("Open MR", "arrow.up.right.square") { model.perform(a) }
        case .dismiss: pill("Dismiss", "xmark") { model.perform(a) }
        case .approve, .deny, .requestChanges: EmptyView()
        }
    }

    func pill(_ title: String, _ symbol: String, _ run: @escaping () -> Void) -> some View {
        Button(action: run) {
            Label(title, systemImage: symbol).font(.system(size: 11, weight: .medium))
                .padding(.horizontal, 9).padding(.vertical, 4)
                .background(Capsule().fill(Color.white.opacity(0.14)))
        }.buttonStyle(.plain).foregroundStyle(.white)
    }
}

/// Every loop and topic as a two-column grid of tiles, running first: an icon badge tinted by state,
/// the name, what it is doing, and a one-click Run button on any loop that is idle or failed.
/// Clicking the tile itself opens its detail. Extras beyond `maxTiles` are counted on the Tasks pill.
struct LoopGrid: View {
    @ObservedObject var model: AppModel
    @ObservedObject var island: IslandModel
    let tasks: [NotchTask]
    static let maxTiles = 10
    private let columns = [GridItem(.flexible(), spacing: 6), GridItem(.flexible(), spacing: 6)]

    var body: some View {
        if tasks.isEmpty {
            Text("No loops or topics yet.").font(.system(size: 11)).foregroundStyle(.white.opacity(0.5))
        } else {
            LazyVGrid(columns: columns, alignment: .leading, spacing: 6) {
                ForEach(tasks.prefix(Self.maxTiles)) { t in
                    LoopTile(task: t, starting: IslandStarts.runnableLoop(for: t).map { model.startingLoops.contains($0) } ?? false,
                             run: IslandStarts.runnableLoop(for: t).map { loop in { model.runLoop(loop) } },
                             open: { island.pin(); island.open(t.id) })
                }
            }
        }
    }
}

struct LoopTile: View {
    let task: NotchTask
    let starting: Bool
    let run: (() -> Void)?
    let open: () -> Void
    @StateObject private var hover = HoverFlag()

    private var tint: Color {
        switch task.state {
        case .running: return .cyan
        case .queued: return .orange
        case .failed: return .red
        case .idle: return .white
        }
    }

    private var symbol: String {
        task.id.hasPrefix("topic:") ? "text.magnifyingglass" : LoopNames.symbol(task.name)
    }

    private var status: String {
        if starting { return "Starting…" }
        if let d = task.detail, !d.isEmpty { return d }
        return TaskList.label(task.state).capitalized
    }

    var body: some View {
        let live = task.state != .idle || starting
        HStack(spacing: 8) {
            ZStack {
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .fill(tint.opacity(task.state == .idle ? 0.08 : 0.2))
                Image(systemName: symbol).font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(tint.opacity(task.state == .idle ? 0.75 : 1))
            }
            .frame(width: 26, height: 26)
            .overlay(alignment: .topTrailing) {
                if task.state == .running { PulseDot(color: tint).offset(x: 2, y: -2) }
            }
            VStack(alignment: .leading, spacing: 1) {
                Text(task.title).font(.system(size: 11.5, weight: .semibold)).foregroundStyle(.white).lineLimit(1)
                Text(status).font(.system(size: 9.5, weight: live ? .medium : .regular))
                    .foregroundStyle(live ? tint.opacity(task.state == .idle ? 0.6 : 0.9) : .white.opacity(0.42))
                    .lineLimit(1).truncationMode(.tail)
            }
            Spacer(minLength: 0)
            trailing
        }
        .padding(.leading, 6).padding(.trailing, 5).padding(.vertical, 5)
        .background(
            RoundedRectangle(cornerRadius: 11, style: .continuous)
                .fill(Color.white.opacity(hover.on ? 0.11 : 0.06))
                .overlay(RoundedRectangle(cornerRadius: 11, style: .continuous)
                    .strokeBorder(tint.opacity(task.state == .idle ? 0 : 0.45), lineWidth: 1))
        )
        .contentShape(RoundedRectangle(cornerRadius: 11))
        .onTapGesture(perform: open)
        .onHover { hover.on = $0 }
        .help(task.detail ?? "\(task.title) · \(TaskList.label(task.state))")
        .animation(.easeOut(duration: 0.15), value: hover.on)
    }

    @ViewBuilder private var trailing: some View {
        if starting {
            ProgressView().controlSize(.mini).frame(width: 24, height: 24)
        } else if let run {
            RunButton(retry: task.state == .failed, action: run)
        } else if task.state == .running {
            Image(systemName: "waveform").font(.system(size: 10, weight: .semibold)).foregroundStyle(tint)
                .symbolEffect(.variableColor.iterative, options: .repeating)
                .frame(width: 24, height: 24)
        }
    }
}

/// The one-click start: a play disc that lights up green under the pointer.
struct RunButton: View {
    let retry: Bool
    let action: () -> Void
    @StateObject private var hover = HoverFlag()
    var body: some View {
        Button(action: action) {
            Image(systemName: retry ? "arrow.clockwise" : "play.fill")
                .font(.system(size: 9, weight: .bold))
                .foregroundStyle(hover.on ? Color.black : Color.white.opacity(0.85))
                .frame(width: 24, height: 24)
                .background(Circle().fill(hover.on ? Color.green : Color.white.opacity(0.14)))
                .scaleEffect(hover.on ? 1.08 : 1)
        }
        .buttonStyle(.plain)
        .onHover { hover.on = $0 }
        .help(retry ? "Run again now" : "Run now")
        .animation(.spring(response: 0.25, dampingFraction: 0.7), value: hover.on)
    }
}

/// Hover state for a tile or button (the Command Line Tools have no SwiftUI macros, so no @State).
@MainActor final class HoverFlag: ObservableObject { @Published var on = false }

/// A small breathing dot on a running tile's badge.
struct PulseDot: View {
    let color: Color
    var body: some View {
        TimelineView(.animation(minimumInterval: 1.0 / 20)) { ctx in
            let phase = (sin(ctx.date.timeIntervalSinceReferenceDate * 2 * .pi / 1.6) + 1) / 2
            Circle().fill(color).frame(width: 6, height: 6)
                .overlay(Circle().stroke(color.opacity(0.5 * (1 - phase)), lineWidth: 3 * phase + 1))
        }
        .accessibilityHidden(true)
    }
}

/// State labels and glyph styles shared by the island's tiles and the pinned task views.
enum TaskList {
    static func label(_ s: NotchTask.State) -> String {
        switch s { case .running: return "running"; case .queued: return "queued"; case .failed: return "failed"; case .idle: return "idle" }
    }

    static func style(_ s: NotchTask.State) -> NotchStyle {
        switch s {
        case .running: return NotchStyle(accent: .blue, symbol: "arrow.triangle.2.circlepath", pulses: true, autoExpands: false, autoCollapseAfter: nil)
        case .queued: return NotchStyle(accent: .orange, symbol: "clock.fill", pulses: false, autoExpands: false, autoCollapseAfter: nil)
        case .failed: return NotchStyle(accent: .red, symbol: "xmark.octagon.fill", pulses: false, autoExpands: false, autoCollapseAfter: nil)
        case .idle: return NotchStyle(accent: .neutral, symbol: "circle.dotted", pulses: false, autoExpands: false, autoCollapseAfter: nil)
        }
    }
}

struct ApprovalCard: View {
    @ObservedObject var model: AppModel
    let approval: Approval
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            if let t = approval.title { Text(t).font(.system(size: 12, weight: .medium)).foregroundStyle(.white).lineLimit(1) }
            if let b = approval.branch {
                Text("\(b) → \(approval.targetBranch ?? "main")").font(.system(size: 10, design: .monospaced))
                    .foregroundStyle(.white.opacity(0.6)).lineLimit(1)
            }
            if let s = approval.summary, !s.isEmpty {
                Text(s).font(.system(size: 11)).foregroundStyle(.white.opacity(0.75)).lineLimit(2)
            }
            HStack(spacing: 10) {
                let busy = model.busyApprovals.contains(approval.id)
                Button { model.perform(.approve(id: approval.id)) } label: {
                    Label("Allow", systemImage: "checkmark").font(.system(size: 12, weight: .semibold))
                        .padding(.horizontal, 14).padding(.vertical, 5)
                        .background(Capsule().fill(Color.green.opacity(busy ? 0.4 : 0.9)))
                }.buttonStyle(.plain).foregroundStyle(.black).disabled(busy)
                    .keyboardShortcut(.return, modifiers: [])
                Button { model.perform(.deny(id: approval.id)) } label: {
                    Label("Deny", systemImage: "xmark").font(.system(size: 12, weight: .semibold))
                        .padding(.horizontal, 14).padding(.vertical, 5)
                        .background(Capsule().fill(Color.red.opacity(busy ? 0.3 : 0.75)))
                }.buttonStyle(.plain).foregroundStyle(.white).disabled(busy)
                    .keyboardShortcut(.escape, modifiers: [])
                Button { model.perform(.requestChanges(id: approval.id)) } label: {
                    Label("Changes…", systemImage: "pencil").font(.system(size: 12, weight: .semibold))
                        .padding(.horizontal, 14).padding(.vertical, 5)
                        .background(Capsule().fill(Color.white.opacity(busy ? 0.1 : 0.2)))
                }.buttonStyle(.plain).foregroundStyle(.white).disabled(busy)
                    .help("Send the fix back to the agent with what to change")
                if busy { ProgressView().controlSize(.small) }
                Spacer(minLength: 0)
                if let exp = approval.expiry {
                    TimelineView(.periodic(from: .now, by: 1)) { ctx in
                        let left = max(0, Int(exp.timeIntervalSince(ctx.date)))
                        Text(String(format: "%d:%02d", left / 60, left % 60))
                            .font(.system(size: 10, design: .monospaced)).foregroundStyle(.white.opacity(0.5))
                    }
                }
            }
        }
    }
}
