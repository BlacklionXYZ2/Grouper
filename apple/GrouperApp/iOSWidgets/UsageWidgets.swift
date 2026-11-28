import AppIntents
import GrouperAPI
import SwiftUI
import WidgetKit

// The Usage tab as widgets: "API costs" (a donut by key, Day / Week / Month) and "Subscription usage"
// (the plan meters), on the Home Screen and the Lock Screen.
//
// Data: the widgets read the snapshot the app writes into the App Group
// (`WidgetSnapshotStore`, `GrouperAPI.WidgetSnapshot`). They hold no token and make no network call,
// so they are as fresh as the last time the Usage page loaded in the app; every figure says how old
// it is, and a stale one says so in words and an icon. The timeline looks at the file again every
// 30 minutes, redraws every 10 so the "resets in" times keep counting down, and the app also
// reloads the widgets whenever it saves a new snapshot.
//
// VERIFY ON MAC: written without a SwiftUI or WidgetKit compiler (the Apple workflow is the first build).

// MARK: - Configuration

/// Day / Week / Month, as the Usage tab's meter (rolling, in local time).
enum WidgetPeriodChoice: String, AppEnum {
    case day, week, month

    static let typeDisplayRepresentation: TypeDisplayRepresentation = "Period"
    static let caseDisplayRepresentations: [WidgetPeriodChoice: DisplayRepresentation] = [
        .day: "Day",
        .week: "Week",
        .month: "Month",
    ]

    var period: UsagePeriod {
        switch self {
        case .day: return .day
        case .week: return .week
        case .month: return .month
        }
    }
}

struct ApiCostsIntent: WidgetConfigurationIntent {
    static let title: LocalizedStringResource = "API costs"
    static let description = IntentDescription("Estimated spend through your API keys, by key.")

    @Parameter(title: "Period", default: .day)
    var period: WidgetPeriodChoice

    init() {}
}

/// A subscription account the widget can be pinned to.
struct SubscriptionAccountEntity: AppEntity {
    static let typeDisplayRepresentation: TypeDisplayRepresentation = "Account"
    static let defaultQuery = SubscriptionAccountQuery()

    var id: String
    var title: String
    var subtitle: String

    var displayRepresentation: DisplayRepresentation {
        DisplayRepresentation(title: "\(title)", subtitle: "\(subtitle)")
    }
}

struct SubscriptionAccountQuery: EntityQuery {
    func entities(for identifiers: [String]) async throws -> [SubscriptionAccountEntity] {
        accounts().filter { identifiers.contains($0.id) }
    }

    func suggestedEntities() async throws -> [SubscriptionAccountEntity] {
        accounts()
    }

    private func accounts() -> [SubscriptionAccountEntity] {
        (WidgetSnapshotStore.load()?.accounts ?? []).map {
            SubscriptionAccountEntity(id: $0.id, title: $0.planName, subtitle: $0.subtitle)
        }
    }
}

struct SubscriptionUsageIntent: WidgetConfigurationIntent {
    static let title: LocalizedStringResource = "Subscription usage"
    static let description = IntentDescription("How much of each plan's windows you have used. Leave the account empty for the one closest to a limit.")

    /// Nil means "the one closest to a limit".
    @Parameter(title: "Account")
    var account: SubscriptionAccountEntity?

    init() {}
}

// MARK: - Entries and providers

struct CostsEntry: TimelineEntry {
    let date: Date
    let snapshot: WidgetSnapshot?
    let period: UsagePeriod
}

struct SubscriptionsEntry: TimelineEntry {
    let date: Date
    let snapshot: WidgetSnapshot?
    /// The account the owner picked; nil is "closest to a limit".
    let accountId: String?
}

/// Both providers read the same file and ask for the same dates.
private func timelineDates(for snapshot: WidgetSnapshot?, now: Date) -> [Date] {
    snapshot?.timelineDates(now: now) ?? [now]
}

private var reloadPolicy: TimelineReloadPolicy {
    .after(Date().addingTimeInterval(WidgetSnapshot.reloadInterval))
}

struct CostsProvider: AppIntentTimelineProvider {
    func placeholder(in context: Context) -> CostsEntry {
        CostsEntry(date: Date(), snapshot: WidgetSample.snapshot(now: Date()), period: .day)
    }

    func snapshot(for configuration: ApiCostsIntent, in context: Context) async -> CostsEntry {
        let now = Date()
        let saved = WidgetSnapshotStore.load()
        // The widget gallery shows sample figures when the app has not written anything yet.
        let snapshot = saved ?? (context.isPreview ? WidgetSample.snapshot(now: now) : nil)
        return CostsEntry(date: now, snapshot: snapshot, period: configuration.period.period)
    }

    func timeline(for configuration: ApiCostsIntent, in context: Context) async -> Timeline<CostsEntry> {
        let now = Date()
        let snapshot = WidgetSnapshotStore.load()
        let entries = timelineDates(for: snapshot, now: now).map {
            CostsEntry(date: $0, snapshot: snapshot, period: configuration.period.period)
        }
        return Timeline(entries: entries, policy: reloadPolicy)
    }
}

struct SubscriptionsProvider: AppIntentTimelineProvider {
    func placeholder(in context: Context) -> SubscriptionsEntry {
        SubscriptionsEntry(date: Date(), snapshot: WidgetSample.snapshot(now: Date()), accountId: nil)
    }

    func snapshot(for configuration: SubscriptionUsageIntent, in context: Context) async -> SubscriptionsEntry {
        let now = Date()
        let saved = WidgetSnapshotStore.load()
        let snapshot = saved ?? (context.isPreview ? WidgetSample.snapshot(now: now) : nil)
        return SubscriptionsEntry(date: now, snapshot: snapshot, accountId: configuration.account?.id)
    }

    func timeline(for configuration: SubscriptionUsageIntent, in context: Context) async -> Timeline<SubscriptionsEntry> {
        let now = Date()
        let snapshot = WidgetSnapshotStore.load()
        let entries = timelineDates(for: snapshot, now: now).map {
            SubscriptionsEntry(date: $0, snapshot: snapshot, accountId: configuration.account?.id)
        }
        return Timeline(entries: entries, policy: reloadPolicy)
    }
}

// MARK: - Widgets

struct ApiCostsWidget: Widget {
    static let kind = "uk.clockmillbarn.grouper.widget.api-costs"

    var body: some WidgetConfiguration {
        AppIntentConfiguration(kind: Self.kind, intent: ApiCostsIntent.self, provider: CostsProvider()) { entry in
            CostsWidgetView(entry: entry)
        }
        .configurationDisplayName("API costs")
        .description("Estimated spend through your API keys, as a donut by key. Day, Week or Month.")
        .supportedFamilies([.systemSmall, .systemMedium, .systemLarge, .accessoryRectangular])
    }
}

struct SubscriptionUsageWidget: Widget {
    static let kind = "uk.clockmillbarn.grouper.widget.subscriptions"

    var body: some WidgetConfiguration {
        AppIntentConfiguration(kind: Self.kind, intent: SubscriptionUsageIntent.self, provider: SubscriptionsProvider()) { entry in
            SubscriptionsWidgetView(entry: entry)
        }
        .configurationDisplayName("Subscription usage")
        .description("How much of your plan windows you have used, and when they reset.")
        .supportedFamilies([.systemSmall, .systemMedium, .systemLarge, .accessoryCircular])
    }
}
