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
            if ui.isExpanded {
                ExpandedContent(model: model, ui: ui)
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
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 14)
            .frame(height: ui.collapsedSize.height)
        } else {
            // Wings either side of the physical notch: glyph left, counter right.
            HStack {
                StatusGlyph(style: n.style).padding(.leading, 12)
                Spacer()
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
                Spacer(minLength: 0)
                HStack(spacing: 8) {
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
        case .approve, .deny: EmptyView()
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
