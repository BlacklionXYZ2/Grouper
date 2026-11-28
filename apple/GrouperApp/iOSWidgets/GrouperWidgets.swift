import ActivityKit
import AppIntents
import SwiftUI
import WidgetKit

@main
struct GrouperWidgets: WidgetBundle {
    var body: some Widget {
        ThreadLiveActivity()
        // The Usage tab: API costs and subscription usage (UsageWidgets.swift).
        ApiCostsWidget()
        SubscriptionUsageWidget()
    }
}

struct ThreadLiveActivity: Widget {
    var body: some WidgetConfiguration {
        ActivityConfiguration(for: ThreadActivityAttributes.self) { context in
            VStack(alignment: .leading, spacing: 6) {
                HStack {
                    Text(context.attributes.title).font(.headline)
                    Spacer()
                    BrandText(text: statusTitle(context.state.status), swatch: Theme.accentSwatch)
                }
                Text(context.state.phase).font(.subheadline)
                if let line = context.state.streamedLine { Text(line).font(.caption).foregroundStyle(.secondary).lineLimit(2) }
            }
            .padding()
            .activityBackgroundTint(Color(grouperHex: 0x10141D).opacity(0.08))
            .activitySystemActionForegroundColor(Color(grouperHex: 0x12C4BF))
        } dynamicIsland: { context in
            DynamicIsland {
                DynamicIslandExpandedRegion(.leading) { BrandMark() }
                DynamicIslandExpandedRegion(.trailing) { Text(context.attributes.device).font(.caption) }
                DynamicIslandExpandedRegion(.center) { Text(context.attributes.title).font(.headline).lineLimit(1) }
                DynamicIslandExpandedRegion(.bottom) {
                    HStack { Text(context.state.phase).lineLimit(1); Spacer(); Button(intent: StopThreadIntent(threadId: context.attributes.threadId)) { Label("Stop", systemImage: "stop.fill") } }
                }
            } compactLeading: {
                BrandMark()
            } compactTrailing: {
                Text(String(context.attributes.device.prefix(3)))
            } minimal: {
                BrandMark()
            }
        }
    }

    private func statusTitle(_ status: String) -> String {
        status == "waiting_device" ? "Waiting" : status.capitalized
    }
}

/// Text in a Theme colour resolved for the current scheme. Widgets get fixed colours,
/// not the app's dynamic ones, so the scheme is read here.
private struct BrandText: View {
    @Environment(\.colorScheme) private var scheme
    @Environment(\.colorSchemeContrast) private var contrast
    let text: String
    let swatch: ThemeSwatch

    var body: some View {
        Text(text).foregroundStyle(Color(grouperHex: swatch.hex(dark: scheme == .dark, highContrast: contrast == .increased)))
    }
}

/// Grouper's mark in the Dynamic Island (always dark): the icon's purple and teal.
private struct BrandMark: View {
    var body: some View {
        Image(systemName: "fish.fill")
            .foregroundStyle(LinearGradient(colors: [Color(grouperHex: 0x22D3CE), Color(grouperHex: 0xA78BFA)],
                                            startPoint: .topLeading, endPoint: .bottomTrailing))
            .accessibilityLabel("Grouper")
    }
}