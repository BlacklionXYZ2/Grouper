import Foundation
import XCTest
@testable import GrouperAPI

/// The widgets' shared snapshot (docs: the App Group file the app writes and the extension reads):
/// the fixture round trip, stale detection, the "closest to a limit" pick and the timeline dates.
/// Fake values only.
final class WidgetSnapshotTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_795_600_000)
    private var nowSeconds: UInt64 { UInt64(now.timeIntervalSince1970) }

    private func snap(_ provider: String, _ account: String, _ window: String, pct: Double, device: String = "pc",
                      resetIn: Int64? = 3600, age: UInt64 = 120, label: String? = nil, key: String? = nil) -> UsageSnapshot {
        UsageSnapshot(account: account, provider: provider, kind: "window", window: window, usedPct: pct,
                      resetsAt: resetIn.map { UInt64(Int64(nowSeconds) + $0) }, source: "probe", deviceId: device,
                      fetchedAt: nowSeconds - age, label: label, accountKey: key)
    }

    private let pc = DeviceInfo(id: "pc", name: "Gaming PC", platform: "windows", online: true, usageProbes: true)

    private func accounts(_ rows: [UsageSnapshot]) -> [WidgetAccount] {
        SubscriptionBoard.build(snapshots: rows, devices: [pc], now: now).cards.map(WidgetAccount.init)
    }

    private func key(_ id: String, cost: Double, color: String? = "teal") -> UsageKey {
        UsageKey(provider: id, color: color,
                 totals: UsageTotals(calls: 2, inputTokens: 1000, outputTokens: 200, cacheReadTokens: 500, cost: cost),
                 models: [UsageKeyModel(model: "m", totals: UsageTotals(calls: 1, cost: cost))])
    }

    private func timed(_ period: UsagePeriod, at: Date, keys: [UsageKey]) -> UsageCacheFile.TimedSummary {
        UsageCacheFile.TimedSummary(
            period: period.rawValue, fetchedAt: at.timeIntervalSince1970,
            summary: UsageSummary(total: UsageTotals(calls: 4, cost: keys.reduce(0) { $0 + $1.totals.cost }), keys: keys,
                                  subscriptions: [UsageSubscriptionTotals(provider: "claude-code@pc", harness: "claude")]))
    }

    private var utc: Calendar {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: "UTC")!
        return calendar
    }

    // MARK: - Round trip

    func testSnapshotRoundTripKeepsCostsAndAccounts() throws {
        let rows = [
            snap("claude", "claude-max", "5h", pct: 62, key: "aaaa"),
            snap("claude", "claude-max", "7d", pct: 30, key: "aaaa"),
            snap("claude", "claude-max", "7d:fable", pct: 85, label: "Fable", key: "aaaa"),
            snap("antigravity", "antigravity", "5h:gemini", pct: 40, label: "Gemini models", key: "bbbb"),
            snap("antigravity", "antigravity", "7d:gemini", pct: 99.6, label: "Gemini models", key: "bbbb"),
        ]
        let original = WidgetSnapshot(
            writtenAt: 1_795_600_001,
            costs: WidgetSnapshot.compactCosts([timed(.day, at: now, keys: [key("openrouter", cost: 1.83), key("deepseek", cost: 0.4, color: "cyan")])]),
            accounts: accounts(rows), accountsAt: now.timeIntervalSince1970)
        XCTAssertEqual(original.accounts.count, 2)

        let data = try XCTUnwrap(original.encoded())
        let decoded = try XCTUnwrap(WidgetSnapshot.decode(data))
        XCTAssertEqual(decoded, original)

        // The card rebuilt from the file speaks as the app's card did.
        let claude = try XCTUnwrap(decoded.accounts.first { $0.provider == "claude" })
        XCTAssertEqual(claude.card.planName, "Claude Max")
        XCTAssertEqual(claude.card.kind, .claude)
        let labels = claude.card.allMeters.map(\.label)
        XCTAssertEqual(labels, ["5-hour", "Weekly", "Weekly \u{00B7} Fable"])
        let fable = try XCTUnwrap(claude.card.allMeters.last)
        XCTAssertEqual(fable.warningWord(now: now), "Nearly used")
        XCTAssertEqual(fable.detailText(now: now), "resets in 1h")
        let antigravity = try XCTUnwrap(decoded.accounts.first { $0.provider == "antigravity" })
        XCTAssertEqual(antigravity.card.groups.compactMap(\.title), ["Gemini models"])
    }

    func testCompactCostsDropModelsAndHarnessRows() throws {
        let compact = WidgetSnapshot.compactCosts([timed(.week, at: now, keys: [key("openrouter", cost: 1)])])
        let summary = try XCTUnwrap(compact.first).summary
        XCTAssertEqual(summary.keys.first?.models, [])
        XCTAssertEqual(summary.subscriptions, [])
        XCTAssertEqual(summary.keys.first?.totals.cost ?? 0, 1, accuracy: 1e-9)
        XCTAssertEqual(summary.keys.first?.totals.tokensIn, 1500)
    }

    func testDecodeRejectsGarbageMissingAndNewerFiles() throws {
        XCTAssertNil(WidgetSnapshot.decode(nil))
        XCTAssertNil(WidgetSnapshot.decode(Data("not json".utf8)))
        XCTAssertNil(WidgetSnapshot.decode(Data(#"{"version":99,"costs":[],"accounts":[]}"#.utf8)))
        // An older or sparse file is still read, with empty lists.
        let sparse = try XCTUnwrap(WidgetSnapshot.decode(Data(#"{"version":1}"#.utf8)))
        XCTAssertEqual(sparse.costs, [])
        XCTAssertEqual(sparse.accounts, [])
        XCTAssertNil(sparse.accountsAt)
    }

    func testMeterWithoutASpanSurvivesTheFile() throws {
        let extra = UsageSnapshot(account: "claude-max", provider: "claude", kind: "spend", window: "extra", usedPct: 20,
                                  usedAmount: 4.1, limitAmount: 20, currency: "USD", source: "probe", deviceId: "pc",
                                  fetchedAt: nowSeconds - 60)
        let window = snap("claude", "claude-max", "5h", pct: 10, age: 60)
        let original = accounts([window, extra])
        let file = WidgetSnapshot(accounts: original)
        let back = try XCTUnwrap(WidgetSnapshot.decode(file.encoded()))
        let meter = try XCTUnwrap(back.accounts.first?.card.allMeters.first { $0.kind == .spend })
        XCTAssertEqual(meter.label, "Extra usage (paid)")
        XCTAssertEqual(meter.amountText, "$4.10 of $20.00")
        XCTAssertEqual(meter.spanSeconds, Int64.max)
    }

    // MARK: - Costs and staleness

    func testCostsForAPeriodCarryTheDonutAndRankedKeys() throws {
        let file = WidgetSnapshot(costs: WidgetSnapshot.compactCosts([
            timed(.day, at: now, keys: [key("small", cost: 0.5, color: "pink"), key("big", cost: 3, color: "teal"), key("free", cost: 0)]),
        ]))
        let costs = try XCTUnwrap(file.costs(for: .day, now: now, calendar: utc))
        XCTAssertEqual(costs.donut.basis, .cost)
        XCTAssertEqual(costs.donut.segments.map(\.id), ["big", "small"])
        XCTAssertEqual(costs.rankedKeys.map(\.provider), ["big", "small"])
        XCTAssertEqual(costs.summary.total.cost, 3.5, accuracy: 1e-9)
        XCTAssertNil(file.costs(for: .week, now: now, calendar: utc), "a period the app never loaded is nil, so the widget asks to open Grouper")
    }

    func testFreshnessWordsAndStaleAfterAnHour() {
        func make(_ age: TimeInterval) -> WidgetFreshness { .make(fetchedAt: now.addingTimeInterval(-age), now: now) }
        XCTAssertEqual(make(10).updatedText, "Updated just now")
        XCTAssertEqual(make(12 * 60 + 5).updatedText, "Updated 12 min ago")
        XCTAssertFalse(make(12 * 60).isStale)
        XCTAssertNil(make(12 * 60).note)
        XCTAssertFalse(make(3600).isStale)
        XCTAssertTrue(make(3601).isStale)
        XCTAssertEqual(make(3 * 3600 + 100).updatedText, "Updated 3h ago")
        XCTAssertEqual(make(2 * 86_400 + 5).updatedText, "Updated 2d ago")
        XCTAssertEqual(make(3601).note, "Open Grouper to refresh.")
        // A clock that ran backwards never prints a negative age.
        XCTAssertEqual(make(-500).updatedText, "Updated just now")
    }

    func testTodayCopiedBeforeMidnightIsStaleEvenWhenRecent() throws {
        let fetched = try XCTUnwrap(utc.date(from: DateComponents(year: 2026, month: 11, day: 27, hour: 23, minute: 50)))
        let later = try XCTUnwrap(utc.date(from: DateComponents(year: 2026, month: 11, day: 28, hour: 0, minute: 10)))
        let file = WidgetSnapshot(costs: [timed(.day, at: fetched, keys: [key("a", cost: 1)])])
        let costs = try XCTUnwrap(file.costs(for: .day, now: later, calendar: utc))
        XCTAssertTrue(costs.freshness.isStale, "20 minutes old but from yesterday")
        XCTAssertEqual(costs.freshness.note, "From an earlier day. Open Grouper to refresh.")
        XCTAssertEqual(costs.freshness.updatedText, "Updated 20 min ago")
        // The same copy within the same day is fresh.
        let sameDay = try XCTUnwrap(utc.date(from: DateComponents(year: 2026, month: 11, day: 27, hour: 23, minute: 59)))
        XCTAssertFalse(try XCTUnwrap(file.costs(for: .day, now: sameDay, calendar: utc)).freshness.isStale)
    }

    func testStaleReadingIsAlwaysStaleAndSaysWhenItWasRead() {
        let stale = WidgetFreshness.staleReading(fetchedAt: now.addingTimeInterval(-7200 - 30), now: now)
        XCTAssertTrue(stale.isStale)
        XCTAssertEqual(stale.updatedText, "Read 2h ago")
        XCTAssertEqual(stale.note, "Open Grouper to refresh.")
    }

    func testAccountsFreshnessUsesWhenTheAppLoadedThem() throws {
        XCTAssertNil(WidgetSnapshot().accountsFreshness(now: now))
        let loaded = WidgetSnapshot(accounts: [], accountsAt: now.timeIntervalSince1970 - 5400)
        let freshness = try XCTUnwrap(loaded.accountsFreshness(now: now))
        XCTAssertTrue(freshness.isStale)
        XCTAssertEqual(freshness.updatedText, "Updated 1h ago")
    }

    // MARK: - Closest to a limit

    func testClosestToLimitPicksTheHighestLiveWindow() throws {
        let file = WidgetSnapshot(accounts: accounts([
            snap("claude", "claude-max", "5h", pct: 30, key: "aaaa"),
            snap("codex", "chatgpt-plus", "5h", pct: 71, key: "cccc"),
            snap("codex", "chatgpt-plus", "7d", pct: 92, key: "cccc"),
            snap("grok", "supergrok", "7d", pct: 50, key: "dddd"),
        ]))
        XCTAssertEqual(file.closestToLimit(now: now)?.provider, "codex")
        XCTAssertEqual(file.accountsByUse(now: now).map(\.provider), ["codex", "grok", "claude"])
        // A chosen account wins, and one that no longer exists falls back to the closest.
        let grok = try XCTUnwrap(file.accounts.first { $0.provider == "grok" })
        XCTAssertEqual(file.resolvedAccount(chosenId: grok.id, now: now)?.provider, "grok")
        XCTAssertEqual(file.resolvedAccount(chosenId: "gone|x", now: now)?.provider, "codex")
        XCTAssertEqual(file.resolvedAccount(chosenId: nil, now: now)?.provider, "codex")
    }

    func testAWindowPastItsResetDoesNotCountAsClosest() {
        let file = WidgetSnapshot(accounts: accounts([
            snap("claude", "claude-max", "5h", pct: 100, resetIn: -300, key: "aaaa"),
            snap("codex", "chatgpt-plus", "5h", pct: 40, key: "cccc"),
        ]))
        XCTAssertEqual(file.closestToLimit(now: now)?.provider, "codex")
        XCTAssertEqual(file.mostUsedWindow(now: now)?.card.provider, "codex")
    }

    func testNoLiveWindowFallsBackToTheFirstAccountAndNoAccountIsNil() {
        let idle = WidgetSnapshot(accounts: accounts([snap("claude", "claude-max", "5h", pct: 0, resetIn: nil, key: "aaaa")]))
        XCTAssertEqual(idle.closestToLimit(now: now)?.provider, "claude")
        XCTAssertNil(idle.mostUsedWindow(now: now))
        XCTAssertNil(WidgetSnapshot().closestToLimit(now: now))
        XCTAssertNil(WidgetSnapshot().mostUsedWindow(now: now))
    }

    func testMostUsedWindowNamesTheMeterAndItsCard() throws {
        let file = WidgetSnapshot(accounts: accounts([
            snap("claude", "claude-max", "5h", pct: 62, key: "aaaa"),
            snap("claude", "claude-max", "7d:fable", pct: 88, label: "Fable", key: "aaaa"),
            snap("codex", "chatgpt-plus", "7d", pct: 70, key: "cccc"),
        ]))
        let pick = try XCTUnwrap(file.mostUsedWindow(now: now))
        XCTAssertEqual(pick.card.planName, "Claude Max")
        XCTAssertEqual(pick.meter.label, "Weekly \u{00B7} Fable")
        XCTAssertEqual(pick.meter.usedPct, 88)
    }

    // MARK: - Rows

    func testRowsFoldGroupHeadingsIntoLabels() throws {
        let account = try XCTUnwrap(accounts([
            snap("antigravity", "antigravity", "5h:gemini", pct: 10, label: "Gemini models", key: "bbbb"),
            snap("antigravity", "antigravity", "7d:gemini", pct: 20, label: "Gemini models", key: "bbbb"),
            snap("antigravity", "antigravity", "5h:claude", pct: 30, label: "Claude models", key: "bbbb"),
            snap("antigravity", "antigravity", "7d:claude", pct: 99, label: "Claude models", key: "bbbb"),
        ]).first)
        let rows = account.card.rows(limit: 8)
        XCTAssertEqual(rows.map(\.label), ["Claude models \u{00B7} 5-hour", "Claude models \u{00B7} Weekly",
                                           "Gemini models \u{00B7} 5-hour", "Gemini models \u{00B7} Weekly"])
    }

    func testRowsOverTheLimitKeepTheMostUsedInOrder() throws {
        let account = try XCTUnwrap(accounts([
            snap("claude", "claude-max", "5h", pct: 10, key: "aaaa"),
            snap("claude", "claude-max", "7d", pct: 90, key: "aaaa"),
            snap("claude", "claude-max", "7d:fable", pct: 50, label: "Fable", key: "aaaa"),
            snap("claude", "claude-max", "7d:haiku", pct: 20, label: "Haiku", key: "aaaa"),
        ]).first)
        let rows = account.card.rows(limit: 2)
        XCTAssertEqual(rows.map(\.label), ["Weekly", "Weekly \u{00B7} Fable"])
        XCTAssertEqual(account.card.rows(limit: 0), [])
        XCTAssertEqual(account.card.rows(limit: 10).count, 4)
    }

    // MARK: - Timeline

    func testTimelineDatesAreNowTenMinutesAndResets() throws {
        let file = WidgetSnapshot(accounts: accounts([
            snap("claude", "claude-max", "5h", pct: 10, resetIn: 25 * 60, key: "aaaa"),
            snap("codex", "chatgpt-plus", "5h", pct: 10, resetIn: 3 * 3600, key: "cccc"),
        ]))
        let dates = file.timelineDates(now: now)
        XCTAssertEqual(dates.first, now)
        XCTAssertEqual(dates.count, 4)
        XCTAssertTrue(dates.contains(now.addingTimeInterval(600)))
        XCTAssertTrue(dates.contains(now.addingTimeInterval(25 * 60 + 30)), "just after the 25-minute reset")
        XCTAssertFalse(dates.contains { $0 > now.addingTimeInterval(WidgetSnapshot.reloadInterval) })
        XCTAssertEqual(dates, dates.sorted())
        XCTAssertEqual(WidgetSnapshot().timelineDates(now: now).count, 3)
    }
}
