import ActivityKit
import SwiftUI
import WidgetKit

@main
struct GrouperWidgets: WidgetBundle {
    var body: some Widget { ThreadLiveActivity() }
}

struct ThreadLiveActivity: Widget {
    var body: some WidgetConfiguration {
        ActivityConfiguration(for: ThreadActivityAttributes.self) { context in
            VStack(alignment: .leading, spacing: 6) {
                HStack { Text(context.attributes.title).font(.headline); Spacer(); Text(context.state.status.capitalized).foregroundStyle(.teal) }
                Text(context.state.phase).font(.subheadline)
                if let line = context.state.streamedLine { Text(line).font(.caption).foregroundStyle(.secondary).lineLimit(2) }
            }
            .padding().activityBackgroundTint(.black.opacity(0.08)).activitySystemActionForegroundColor(.teal)
        } dynamicIsland: { context in
            DynamicIsland {
                DynamicIslandExpandedRegion(.leading) { Image(systemName: "circle.hexagongrid.fill").foregroundStyle(.teal) }
                DynamicIslandExpandedRegion(.trailing) { Text(context.attributes.device).font(.caption) }
                DynamicIslandExpandedRegion(.center) { Text(context.attributes.title).font(.headline).lineLimit(1) }
                DynamicIslandExpandedRegion(.bottom) {
                    HStack { Text(context.state.phase).lineLimit(1); Spacer(); Button(intent: StopThreadIntent(threadId: context.attributes.threadId)) { Label("Stop", systemImage: "stop.fill") } }
                }
            } compactLeading: {
                Image(systemName: "circle.hexagongrid.fill").foregroundStyle(.teal)
            } compactTrailing: {
                Text(context.attributes.device.prefix(3))
            } minimal: {
                Image(systemName: "circle.hexagongrid.fill").foregroundStyle(.teal)
            }
        }
    }
}
