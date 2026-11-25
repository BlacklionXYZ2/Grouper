import SwiftUI
import GrouperAPI

extension Color {
    static var grouperSecondaryBackground: Color {
        #if os(iOS)
        Color(uiColor: .secondarySystemBackground)
        #else
        Color(nsColor: .controlBackgroundColor)
        #endif
    }
}

struct GlassSurface<Content: View>: View {
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency
    @ViewBuilder let content: Content

    init(@ViewBuilder content: () -> Content) { self.content = content() }

    var body: some View {
        if reduceTransparency {
            content.background(Color.grouperSecondaryBackground, in: .rect(cornerRadius: 18))
        } else {
            content.glassEffect(.regular, in: .rect(cornerRadius: 18))
        }
    }
}

struct ErrorBanner: View {
    let message: String
    var body: some View {
        Label(message, systemImage: "exclamationmark.triangle.fill")
            .font(.callout).foregroundStyle(.red).padding()
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(.red.opacity(0.08), in: .rect(cornerRadius: 12))
            .accessibilityElement(children: .combine)
    }
}

struct StatusLabel: View {
    let status: String
    var body: some View {
        Label(title, systemImage: icon).foregroundStyle(color).font(.caption.weight(.semibold))
    }
    private var title: String {
        switch status { case "waiting_device": "Waiting"; default: status.capitalized }
    }
    private var icon: String {
        switch status {
        case "running": "circle.fill"
        case "waiting_device": "clock.fill"
        case "failed": "xmark.circle.fill"
        case "done": "checkmark.circle.fill"
        case "stopped": "stop.circle.fill"
        default: "circle"
        }
    }
    private var color: Color {
        switch status { case "running": .teal; case "waiting_device": .orange; case "failed": .red; default: .secondary }
    }
}

struct ThreadChip: View {
    let card: ThreadCard
    let deviceName: String
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(card.title.isEmpty ? "Thread" : card.title).font(.headline).lineLimit(2)
            HStack { StatusLabel(status: card.status); Text(deviceName).font(.caption).foregroundStyle(.secondary) }
            if let preview = card.liveDraft.isEmpty ? card.latestText : card.liveDraft, !preview.isEmpty {
                Text(preview).font(.caption).foregroundStyle(.secondary).lineLimit(1)
            } else if let tool = card.openTool {
                Label(tool, systemImage: "wrench.and.screwdriver").font(.caption).foregroundStyle(.secondary)
            }
            if card.pendingApprovals > 0 {
                Label("Needs you", systemImage: "exclamationmark.circle.fill").font(.caption.weight(.semibold)).foregroundStyle(.orange)
            }
        }
        .padding(12).frame(minWidth: 170, maxWidth: 240, alignment: .leading)
        .background(Color.grouperSecondaryBackground, in: .rect(cornerRadius: 14))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(card.title), \(card.status), on \(deviceName)")
        .accessibilityValue(card.liveDraft.isEmpty ? card.latestText ?? "" : card.liveDraft)
    }
}

extension View {
    @ViewBuilder func grouperMinimumControlSize() -> some View {
        self.frame(minWidth: 44, minHeight: 44)
    }
}
