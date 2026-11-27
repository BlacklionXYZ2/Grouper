import Foundation
import SwiftUI
import GrouperAPI

/// Where the last good copy of the Usage page is kept on this device (docs/usage-tab-spec.md 8,
/// "Offline"). One entry, for one hub; signing out removes it.
enum UsageCacheStorage {
    static let key = "grouper.usage.cache.v1"

    static func load(hub: String) -> UsageCacheFile? {
        UsageCacheFile.decode(UserDefaults.standard.data(forKey: key), hub: hub)
    }

    static func save(_ file: UsageCacheFile) {
        if let data = file.encoded() { UserDefaults.standard.set(data, forKey: key) }
    }

    static func clear() {
        UserDefaults.standard.removeObject(forKey: key)
    }
}

/// What the Usage page shows and how it loads it: the API-costs summary per period, the plan
/// snapshots, the daily budget line, and which key is focused. Each section loads and fails on
/// its own. The last good copy of both is kept on the device, so the page still shows something
/// offline.
@MainActor
final class UsageStore: ObservableObject {
    enum LoadState: Equatable {
        case idle
        case loading
        case failed(String)

        var isFailed: Bool {
            if case .failed = self { return true }
            return false
        }
    }

    /// The newest summary for each period (`UsagePeriod.rawValue`), with when it was fetched.
    @Published private(set) var summaries: [String: UsageCacheFile.TimedSummary] = [:]
    @Published private(set) var summaryState: LoadState = .idle
    /// The device probe and balance snapshots; nil until the first load (or a saved copy) arrives.
    @Published private(set) var snapshots: [UsageSnapshot]?
    @Published private(set) var snapshotsAt: Date?
    @Published private(set) var snapshotsState: LoadState = .idle
    /// For the daily budget line; nil when the hub does not say.
    @Published private(set) var limits: LimitsReport?
    /// A manual or automatic "read the plans now" is in flight.
    @Published private(set) var readingNow = false
    /// The segment or key that is focused: a provider id, or "other".
    @Published private(set) var focusedKey: String?
    @Published private(set) var expandedKeys: Set<String> = []
    @Published var showAllModels: Set<String> = []

    private var hub = ""
    /// What the Subscriptions board needs to name devices and spot a reading that is off. The page
    /// sets it before it loads snapshots, so the widgets' copy of the board matches the page's.
    private var boardDevices: [DeviceInfo] = []
    private var boardProviders: [ProviderInfo] = []
    private var summaryRequest = 0
    private var snapshotsRequest = 0
    private var lastReadNow: Date?

    /// A device reading older than this triggers a fresh read when the tab opens (spec 7.4).
    private static let readIfOlderThan: TimeInterval = 120
    /// The widgets show Day, Week and Month whichever the page is on; the other two are reloaded
    /// when they are older than this.
    private static let otherPeriodMaxAge: TimeInterval = 300

    /// Loads the saved copy for this hub, once per hub.
    func attach(hub: String) {
        guard hub != self.hub else { return }
        self.hub = hub
        guard let file = UsageCacheStorage.load(hub: hub) else { return }
        summaries = Dictionary(file.summaries.map { ($0.period, $0) }, uniquingKeysWith: { first, _ in first })
        snapshots = file.snapshots
        snapshotsAt = file.snapshotsAt.map { Date(timeIntervalSince1970: $0) }
        limits = file.limits
    }

    private func persist() {
        guard !hub.isEmpty else { return }
        UsageCacheStorage.save(UsageCacheFile(
            hub: hub, summaries: Array(summaries.values), snapshots: snapshots,
            snapshotsAt: snapshotsAt?.timeIntervalSince1970, limits: limits))
        publishToWidgets()
    }

    /// The page's devices and providers, for the board the widgets show.
    func setBoardContext(devices: [DeviceInfo], providers: [ProviderInfo]) {
        boardDevices = devices
        boardProviders = providers
    }

    /// Writes the compact copy the Home Screen and Lock Screen widgets read (App Group file) and asks
    /// WidgetKit to redraw them, only when something changed. iPhone only: the Mac has no widgets here.
    private func publishToWidgets() {
        #if os(iOS)
        let existing = WidgetSnapshotStore.load()
        var next = existing ?? WidgetSnapshot()
        next.costs = WidgetSnapshot.compactCosts(Array(summaries.values))
        if let snapshots {
            let board = SubscriptionBoard.build(snapshots: snapshots, devices: boardDevices, providers: boardProviders)
            next.accounts = board.cards.map(WidgetAccount.init)
            next.accountsAt = snapshotsAt?.timeIntervalSince1970
        }
        if let existing {
            var same = existing
            same.writtenAt = next.writtenAt
            if same == next { return }
        }
        next.writtenAt = Date().timeIntervalSince1970
        if WidgetSnapshotStore.save(next) { WidgetSnapshotStore.reloadTimelines() }
        #endif
    }

    // MARK: - Loading

    func refreshSummary(client: HubClient?, period: UsagePeriod) async {
        guard let client else { return }
        summaryRequest += 1
        let token = summaryRequest
        summaryState = .loading
        let now = Date()
        let range = period.range(now: now)
        do {
            let fetched = try await client.usageSummary(since: range.since, until: range.until)
            guard token == summaryRequest else { return }
            summaries[period.rawValue] = UsageCacheFile.TimedSummary(
                period: period.rawValue, fetchedAt: now.timeIntervalSince1970, summary: fetched)
            summaryState = .idle
            // The budget line is a nicety: its failure never fails the summary.
            if let report = try? await client.limits() { limits = report }
            persist()
            #if os(iOS)
            await refreshOtherPeriods(client: client, current: period)
            #endif
        } catch {
            guard token == summaryRequest else { return }
            summaryState = Task.isCancelled ? .idle : .failed(error.localizedDescription)
        }
    }

    /// Keeps the two periods the page is not on loaded, for the widgets. Best effort: a failure
    /// leaves the older copy, and each period is skipped while its copy is young.
    private func refreshOtherPeriods(client: HubClient, current: UsagePeriod) async {
        for other in UsagePeriod.allCases where other != current {
            if Task.isCancelled { return }
            if let have = summaries[other.rawValue],
               Date().timeIntervalSince1970 - have.fetchedAt < Self.otherPeriodMaxAge { continue }
            let now = Date()
            let range = other.range(now: now)
            guard let fetched = try? await client.usageSummary(since: range.since, until: range.until) else { continue }
            summaries[other.rawValue] = UsageCacheFile.TimedSummary(
                period: other.rawValue, fetchedAt: now.timeIntervalSince1970, summary: fetched)
        }
        persist()
    }

    /// Loads the plan snapshots. With `readIfOld`, a reading older than two minutes (or none) also
    /// asks the hub to read the devices now, then loads again.
    func refreshSnapshots(client: HubClient?, readIfOld: Bool) async {
        guard let client else { return }
        snapshotsRequest += 1
        let token = snapshotsRequest
        snapshotsState = .loading
        do {
            let fetched = try await client.usageSnapshots()
            guard token == snapshotsRequest else { return }
            snapshots = fetched
            snapshotsAt = Date()
            snapshotsState = .idle
            persist()
            if readIfOld, shouldReadNow(fetched) { await readDevicesNow(client: client) }
        } catch {
            guard token == snapshotsRequest else { return }
            snapshotsState = Task.isCancelled ? .idle : .failed(error.localizedDescription)
        }
    }

    private func shouldReadNow(_ fetched: [UsageSnapshot]) -> Bool {
        if let last = lastReadNow, Date().timeIntervalSince(last) < Self.readIfOlderThan { return false }
        guard let newest = fetched.filter({ $0.source == "probe" }).map(\.fetchedAt).max() else { return true }
        return Date().timeIntervalSince1970 - TimeInterval(newest) > Self.readIfOlderThan
    }

    /// `POST /v1/usage/refresh` (the hub reads provider balances and asks each device to read its
    /// plans), then loads the snapshots again. Failure keeps what is shown.
    private func readDevicesNow(client: HubClient) async {
        lastReadNow = Date()
        readingNow = true
        defer { readingNow = false }
        _ = try? await client.refreshUsage()
        if let fetched = try? await client.usageSnapshots() {
            snapshots = fetched
            snapshotsAt = Date()
            persist()
        }
    }

    /// Pull to refresh and the Mac's Refresh button: read now, then load both sections.
    func manualRefresh(client: HubClient?, period: UsagePeriod) async {
        guard let client else { return }
        readingNow = true
        _ = try? await client.refreshUsage()
        lastReadNow = Date()
        readingNow = false
        async let summary: Void = refreshSummary(client: client, period: period)
        async let plans: Void = refreshSnapshots(client: client, readIfOld: false)
        await summary
        await plans
    }

    /// Saves a key's colour on the hub (nil lets the hub choose again) and reloads the summary.
    /// Returns an error message, or nil when it worked.
    func setColor(_ name: String?, provider: String, client: HubClient?, period: UsagePeriod) async -> String? {
        guard let client else { return "Not connected to the hub." }
        do {
            _ = try await client.setProviderColor(id: provider, color: name)
        } catch {
            return error.localizedDescription
        }
        await refreshSummary(client: client, period: period)
        return nil
    }

    // MARK: - Focus

    /// Tapping a segment or a key row: focus it and open its row; again to clear. "Other" has no row.
    func toggleFocus(_ id: String) {
        if focusedKey == id {
            focusedKey = nil
            expandedKeys.remove(id)
        } else {
            focusedKey = id
            if id != "other" { expandedKeys.insert(id) }
        }
    }

    /// A key row opened or closed by its own disclosure arrow. Opening focuses it.
    func setExpanded(_ id: String, _ expanded: Bool) {
        if expanded {
            expandedKeys.insert(id)
            focusedKey = id
        } else {
            expandedKeys.remove(id)
            if focusedKey == id { focusedKey = nil }
        }
    }

    func clearFocus() { focusedKey = nil }
}
