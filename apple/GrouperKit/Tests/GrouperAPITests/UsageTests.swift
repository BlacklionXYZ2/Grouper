import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import XCTest
@testable import GrouperAPI

/// The Usage tab's data layer (docs/usage-tab-spec.md): payload decoding on fixture JSON with fake
/// values, the Day / Week / Month ranges, the donut, the number formats, the Subscriptions cards and
/// the two new API calls.
final class UsageTests: XCTestCase {
    private func decode<T: Decodable>(_ type: T.Type, _ raw: String) throws -> T {
        try JSONDecoder.grouper.decode(T.self, from: Data(raw.utf8))
    }

    // MARK: - Payloads

    func testDecodesUsageSummaryPayload() throws {
        let raw = #"""
        {"since":1795564800,"until":1795651200,
         "total":{"calls":96,"input_tokens":1840000,"output_tokens":212000,"cache_read_tokens":0,"cache_write_tokens":0,"cost":4.82},
         "unpriced_calls":3,
         "keys":[
          {"provider":"openrouter","kind":"openai","color":"teal","local":false,"removed":false,
           "totals":{"calls":31,"input_tokens":412000,"output_tokens":38000,"cache_read_tokens":301000,"cache_write_tokens":12000,"cost":1.83},
           "models":[{"model":"anthropic/claude-fable-5","name":"Claude Fable 5","free":false,"local":false,"priced":true,
                      "totals":{"calls":12,"cost":1.61}}]},
          {"provider":"ollama","kind":"openai","color":null,"local":true,"removed":false,
           "totals":{"calls":0,"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"cost":0},"models":[]},
          {"provider":"old-key","kind":"","color":null,"local":false,"removed":true,
           "totals":{"calls":2,"input_tokens":10,"output_tokens":5,"cache_read_tokens":0,"cache_write_tokens":0,"cost":0.01},"models":[]}
         ],
         "subscriptions":[{"provider":"claude-code@gaming-pc","harness":"claude","device_id":"dev1",
                           "totals":{"calls":9,"input_tokens":212000,"output_tokens":18000,"cost":0}}]}
        """#
        let summary = try decode(UsageSummary.self, raw)
        XCTAssertEqual(summary.since, 1_795_564_800)
        XCTAssertEqual(summary.until, 1_795_651_200)
        XCTAssertEqual(summary.total.calls, 96)
        XCTAssertEqual(summary.total.cost, 4.82, accuracy: 1e-9)
        XCTAssertEqual(summary.unpricedCalls, 3)
        XCTAssertEqual(summary.keys.count, 3)

        let key = summary.keys[0]
        XCTAssertEqual(key.provider, "openrouter")
        XCTAssertEqual(key.color, "teal")
        XCTAssertFalse(key.removed)
        XCTAssertEqual(key.totals.tokensIn, 412_000 + 301_000 + 12_000)
        XCTAssertEqual(key.totals.tokensOut, 38_000)
        let model = try XCTUnwrap(key.models.first)
        XCTAssertEqual(model.name, "Claude Fable 5")
        XCTAssertTrue(model.priced)
        XCTAssertEqual(model.totals.calls, 12)
        // A model row carries only calls and cost; the rest is zero.
        XCTAssertEqual(model.totals.tokensIn, 0)

        XCTAssertNil(summary.keys[1].color)
        XCTAssertTrue(summary.keys[1].local)
        XCTAssertTrue(summary.keys[2].removed)
        XCTAssertEqual(summary.keys[2].displayName, "Removed key (old-key)")

        let harness = try XCTUnwrap(summary.subscriptions.first)
        XCTAssertEqual(harness.harness, "claude")
        XCTAssertEqual(harness.deviceId, "dev1")
        XCTAssertEqual(harness.totals.tokensIn, 212_000)
    }

    func testSummaryToleratesMissingFields() throws {
        let summary = try decode(UsageSummary.self, #"{"keys":[{"provider":"p","models":[{"model":"m"}]}]}"#)
        XCTAssertNil(summary.since)
        XCTAssertEqual(summary.total, UsageTotals())
        XCTAssertEqual(summary.unpricedCalls, 0)
        XCTAssertEqual(summary.subscriptions, [])
        XCTAssertEqual(summary.keys[0].kind, "")
        XCTAssertEqual(summary.keys[0].models[0].name, "m")
        XCTAssertTrue(summary.keys[0].models[0].priced)
        XCTAssertFalse(summary.keys[0].models[0].free)
    }

    func testProviderColorAndDeviceProbeFlagsDecode() throws {
        let provider = try decode(ProviderInfo.self,
                                  #"{"id":"openrouter","kind":"openai","base_url":null,"has_key":true,"via_device":null,"options":null,"color":"purple"}"#)
        XCTAssertEqual(provider.color, "purple")
        // An older hub sends no colour.
        let old = try decode(ProviderInfo.self, #"{"id":"p","kind":"openai","has_key":false}"#)
        XCTAssertNil(old.color)

        let on = try decode(DeviceInfo.self,
                            #"{"id":"pc","name":"Gaming PC","platform":"windows","online":true,"capabilities":[],"last_seen":1,"usage_probes":true,"usage_probe_providers":["claude","grok"]}"#)
        XCTAssertEqual(on.usageProbes, true)
        XCTAssertEqual(on.usageProbeProviders, ["claude", "grok"])
        let off = try decode(DeviceInfo.self, #"{"id":"mac","name":"MacBook","platform":"macos","online":false,"usage_probes":false,"usage_probe_providers":[]}"#)
        XCTAssertEqual(off.usageProbes, false)
        // An older hub says nothing, which is not the same as off.
        let unknown = try decode(DeviceInfo.self, #"{"id":"x","name":"X","platform":"linux","online":true}"#)
        XCTAssertNil(unknown.usageProbes)
        XCTAssertEqual(unknown.usageProbeProviders, [])
    }

    func testSnapshotLabelAndAccountKeyDecode() throws {
        let rows = try decode([UsageSnapshot].self, #"""
        [{"account":"antigravity","provider":"antigravity","kind":"window","window":"5h:gemini","used_pct":1.0,"used_amount":null,
          "limit_amount":null,"currency":null,"resets_at":1795600000,"source":"probe","device_id":"pc","fetched_at":1795590000,
          "label":"Gemini models","account_key":"0123456789abcdef0123456789abcdef"},
         {"account":"claude-max","provider":"claude","kind":"window","window":"5h","used_pct":12.0,"resets_at":null,
          "source":"probe","device_id":"pc","fetched_at":1795590000}]
        """#)
        XCTAssertEqual(rows[0].label, "Gemini models")
        XCTAssertEqual(rows[0].accountKey, "0123456789abcdef0123456789abcdef")
        XCTAssertNil(rows[1].label)
        XCTAssertNil(rows[1].accountKey)
    }

    func testCacheRoundTripsAndIgnoresAnotherHub() throws {
        let summary = UsageSummary(since: 10, until: 20, total: UsageTotals(calls: 1, cost: 0.5),
                                   keys: [UsageKey(provider: "p", color: "teal", totals: UsageTotals(calls: 1, cost: 0.5))])
        let file = UsageCacheFile(hub: "https://hub.example",
                                  summaries: [.init(period: "day", fetchedAt: 100, summary: summary)],
                                  snapshots: [UsageSnapshot(account: "a", provider: "claude", kind: "window", window: "5h", usedPct: 3, fetchedAt: 9)],
                                  snapshotsAt: 101)
        let data = try XCTUnwrap(file.encoded())
        XCTAssertEqual(UsageCacheFile.decode(data, hub: "https://hub.example"), file)
        XCTAssertNil(UsageCacheFile.decode(data, hub: "https://other.example"))
        XCTAssertNil(UsageCacheFile.decode(Data("not json".utf8), hub: "https://hub.example"))
        XCTAssertNil(UsageCacheFile.decode(nil, hub: "https://hub.example"))
    }

    // MARK: - Periods

    private func calendar(_ identifier: String) throws -> Calendar {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = try XCTUnwrap(TimeZone(identifier: identifier))
        return calendar
    }

    private func date(_ calendar: Calendar, _ y: Int, _ m: Int, _ d: Int, _ h: Int, _ min: Int = 0) throws -> Date {
        try XCTUnwrap(calendar.date(from: DateComponents(year: y, month: m, day: d, hour: h, minute: min)))
    }

    func testPeriodRangesAreLocalMidnights() throws {
        let tz = try calendar("Etc/GMT-1") // UTC+1 all year
        let now = try date(tz, 2026, 11, 27, 0, 30) // 00:30 local, 23:30 UTC the day before
        let midnightToday = try date(tz, 2026, 11, 27, 0)

        let day = UsagePeriod.day.range(now: now, calendar: tz)
        XCTAssertEqual(day.since, Int64(midnightToday.timeIntervalSince1970))
        XCTAssertEqual(day.until, Int64(now.timeIntervalSince1970))

        let week = UsagePeriod.week.range(now: now, calendar: tz)
        XCTAssertEqual(week.since, Int64(try date(tz, 2026, 11, 21, 0).timeIntervalSince1970)) // 7 days including today
        let month = UsagePeriod.month.range(now: now, calendar: tz)
        XCTAssertEqual(month.since, Int64(try date(tz, 2026, 10, 29, 0).timeIntervalSince1970)) // 30 days including today
        XCTAssertEqual(UsagePeriod.allCases.map(\.caption), ["Today", "Last 7 days", "Last 30 days"])
    }

    func testPeriodRangeStepsByCalendarDaysAcrossAClockChange() throws {
        // New York leaves daylight time on 1 Nov 2026 (a 25-hour day).
        let ny = try calendar("America/New_York")
        let now = try date(ny, 2026, 11, 3, 15)
        let week = UsagePeriod.week.range(now: now, calendar: ny)
        XCTAssertEqual(week.since, Int64(try date(ny, 2026, 10, 28, 0).timeIntervalSince1970))
        XCTAssertTrue(week.since < week.until)
    }

    // MARK: - Donut

    private func key(_ id: String, cost: Double, tokens: UInt64 = 0, color: String? = "teal", removed: Bool = false) -> UsageKey {
        UsageKey(provider: id, color: color, removed: removed,
                 totals: UsageTotals(calls: cost > 0 || tokens > 0 ? 1 : 0, inputTokens: tokens, cost: cost))
    }

    func testDonutIsSizedByCostLargestFirstAndSkipsFreeKeys() {
        let model = UsageDonut.build(keys: [
            key("small", cost: 1.0, color: "pink"), key("free", cost: 0, tokens: 5000), key("big", cost: 3.0, color: "teal"),
        ])
        XCTAssertEqual(model.basis, .cost)
        XCTAssertEqual(model.segments.map(\.id), ["big", "small"])
        XCTAssertEqual(model.total, 4.0, accuracy: 1e-9)
        XCTAssertEqual(model.segments[0].share, 0.75, accuracy: 1e-9)
        XCTAssertEqual(model.segments[0].colorName, "teal")
        XCTAssertNil(model.segment(forProvider: "free"))
    }

    func testDonutGivesATinyKeyAtLeastThreeDegrees() throws {
        let model = UsageDonut.build(keys: [key("big", cost: 1000), key("tiny", cost: 0.001)])
        let tiny = try XCTUnwrap(model.segment(forProvider: "tiny"))
        XCTAssertLessThan(tiny.share, 3.0 / 360)
        XCTAssertEqual(tiny.weight, 3.0 / 360, accuracy: 1e-12)
        let big = try XCTUnwrap(model.segment(forProvider: "big"))
        XCTAssertEqual(big.weight, big.share, accuracy: 1e-12)
    }

    func testDonutFoldsTheSmallestIntoOtherBeyondEightKeys() throws {
        let keys = (1...11).map { key("k\($0)", cost: Double(12 - $0)) } // k1 is the biggest
        let model = UsageDonut.build(keys: keys)
        XCTAssertEqual(model.segments.count, 9)
        let other = try XCTUnwrap(model.segments.last)
        XCTAssertTrue(other.isOther)
        XCTAssertNil(other.colorName)
        XCTAssertEqual(other.providers, ["k9", "k10", "k11"])
        XCTAssertEqual(other.value, 3 + 2 + 1, accuracy: 1e-9)
        XCTAssertEqual(model.segments.reduce(0) { $0 + $1.share }, 1, accuracy: 1e-9)
        XCTAssertNotNil(model.segment(forProvider: "k10")) // a folded key still finds its segment
    }

    func testDonutFallsBackToTokensWhenNothingCostMoney() {
        let model = UsageDonut.build(keys: [key("local", cost: 0, tokens: 300), key("free", cost: 0, tokens: 100), key("idle", cost: 0)])
        XCTAssertEqual(model.basis, .tokens)
        XCTAssertEqual(model.segments.map(\.id), ["local", "free"])
        XCTAssertEqual(model.total, 400, accuracy: 1e-9)
        XCTAssertTrue(UsageDonut.accessibilitySummary(model, period: .day, totals: UsageTotals()).contains("share of tokens"))
    }

    func testDonutIsEmptyWithNoCalls() {
        let model = UsageDonut.build(keys: [key("idle", cost: 0)])
        XCTAssertEqual(model.basis, .empty)
        XCTAssertEqual(model.segments, [])
        let summary = UsageDonut.accessibilitySummary(model, period: .week, totals: UsageTotals())
        XCTAssertEqual(summary, "API costs, the last 7 days, $0.00 estimated. No calls through Grouper in this period.")
    }

    func testDonutDrawsARemovedKeyInGrey() throws {
        let model = UsageDonut.build(keys: [key("gone", cost: 2, color: "teal", removed: true)])
        let segment = try XCTUnwrap(model.segments.first)
        XCTAssertNil(segment.colorName)
        XCTAssertEqual(segment.title, "Removed key (gone)")
    }

    func testChartSummaryNamesEachKeyWithCostAndShare() {
        let model = UsageDonut.build(keys: [key("openrouter", cost: 1.83), key("anthropic", cost: 3.0)])
        let totals = UsageTotals(calls: 5, cost: 4.82)
        XCTAssertEqual(UsageDonut.accessibilitySummary(model, period: .week, totals: totals),
                       "API costs, the last 7 days, $4.82 estimated. anthropic $3.00, 62%. openrouter $1.83, 38%.")
    }

    // MARK: - Formats

    func testNumberFormats() {
        XCTAssertEqual(UsageFormat.money(4.8249), "$4.82")
        XCTAssertEqual(UsageFormat.money(0), "$0.00")
        XCTAssertEqual(UsageFormat.money(0.001), "<$0.01")
        XCTAssertEqual(UsageFormat.tokens(0), "0")
        XCTAssertEqual(UsageFormat.tokens(812), "812")
        XCTAssertEqual(UsageFormat.tokens(9_800), "9.8K")
        XCTAssertEqual(UsageFormat.tokens(38_000), "38K")
        XCTAssertEqual(UsageFormat.tokens(412_000), "412K")
        XCTAssertEqual(UsageFormat.tokens(999_700), "1M")
        XCTAssertEqual(UsageFormat.tokens(1_840_000), "1.84M")
        XCTAssertEqual(UsageFormat.tokens(12_300_000), "12M")
        XCTAssertEqual(UsageFormat.exactTokens(1_840_000), "1,840,000")
        XCTAssertEqual(UsageFormat.exactTokens(12), "12")
        XCTAssertEqual(UsageFormat.percent(0.38), "38%")
        XCTAssertEqual(UsageFormat.percent(0.001), "<1%")
        XCTAssertEqual(UsageFormat.usedPercent(62), "62%")
        XCTAssertEqual(UsageFormat.usedPercent(99.6), "99.6%")
        XCTAssertEqual(UsageFormat.usedPercent(99.97), "99.9%")
        XCTAssertEqual(UsageFormat.usedPercent(100), "100%")
        XCTAssertEqual(UsageFormat.calls(1), "1 call")
        let totals = UsageTotals(calls: 31, inputTokens: 100_000, outputTokens: 38_000, cacheReadTokens: 312_000)
        XCTAssertEqual(UsageFormat.tokenLine(totals), "412K in \u{00B7} 38K out \u{00B7} 31 calls")
    }

    func testResetText() throws {
        let now = Date(timeIntervalSince1970: 1_795_600_000)
        let at: (TimeInterval) -> UInt64 = { UInt64(now.timeIntervalSince1970 + $0) }
        XCTAssertEqual(UsageFormat.resetText(resetsAt: at(38 * 60), now: now), "resets in 38 min")
        XCTAssertEqual(UsageFormat.resetText(resetsAt: at(2 * 3600 + 14 * 60), now: now), "resets in 2h 14m")
        XCTAssertEqual(UsageFormat.resetText(resetsAt: at(3 * 3600), now: now), "resets in 3h")
        XCTAssertEqual(UsageFormat.resetSpeech(resetsAt: at(2 * 3600 + 14 * 60), now: now), "resets in 2 hours 14 minutes")
        XCTAssertEqual(UsageFormat.resetSpeech(resetsAt: at(60), now: now), "resets in 1 minute")
        // Past a day it is a local date and time.
        let far = UsageFormat.resetText(resetsAt: at(3 * 86_400), now: now, locale: Locale(identifier: "en_GB"), timeZone: TimeZone(identifier: "UTC")!)
        XCTAssertTrue(far.hasPrefix("resets "), far)
        XCTAssertTrue(far.contains("Dec") || far.contains("Nov"), far)
    }

    // MARK: - Subscription cards

    private let now = Date(timeIntervalSince1970: 1_795_600_000)
    private var nowSeconds: UInt64 { UInt64(now.timeIntervalSince1970) }

    private func snap(_ provider: String, _ account: String, _ window: String, pct: Double, device: String,
                      resetIn: Int64? = 3600, age: UInt64 = 120, label: String? = nil, key: String? = nil,
                      kind: String = "window") -> UsageSnapshot {
        UsageSnapshot(account: account, provider: provider, kind: kind, window: window, usedPct: pct,
                      resetsAt: resetIn.map { UInt64(Int64(nowSeconds) + $0) }, source: "probe", deviceId: device,
                      fetchedAt: nowSeconds - age, label: label, accountKey: key)
    }

    private func status(_ provider: String, _ text: String, device: String, age: UInt64 = 60) -> UsageSnapshot {
        UsageSnapshot(account: provider, provider: provider, kind: "status", window: text, source: "probe",
                      deviceId: device, fetchedAt: nowSeconds - age)
    }

    private let pc = DeviceInfo(id: "pc", name: "Gaming PC", platform: "windows", online: true, usageProbes: true)
    private let mac = DeviceInfo(id: "mac", name: "MacBook", platform: "macos", online: true, usageProbes: true)

    func testSameAccountKeyOnTwoDevicesIsOneCardNamingBoth() throws {
        let board = SubscriptionBoard.build(snapshots: [
            snap("claude", "claude-max", "5h", pct: 10, device: "pc", age: 600, key: "aaaa"),
            snap("claude", "claude-max", "7d", pct: 40, device: "pc", age: 600, key: "aaaa"),
            snap("claude", "claude-max", "5h", pct: 12, device: "mac", age: 60, key: "aaaa"),
            snap("claude", "claude-max", "7d", pct: 41, device: "mac", age: 60, key: "aaaa"),
        ], devices: [pc, mac], now: now)
        XCTAssertEqual(board.cards.count, 1)
        let card = board.cards[0]
        XCTAssertEqual(card.planName, "Claude Max")
        XCTAssertEqual(card.deviceNames, ["MacBook", "Gaming PC"]) // newest reading first
        XCTAssertEqual(card.signedInLine, "Signed in on MacBook and Gaming PC")
        XCTAssertEqual(card.fetchedAt, nowSeconds - 60)
        let meters = card.groups.flatMap(\.meters)
        XCTAssertEqual(meters.map(\.label), ["5-hour", "Weekly"])
        XCTAssertEqual(meters.map(\.usedPct), [12, 41]) // the newest reading's values
    }

    func testDifferentAccountKeysAreSeparateCardsAndMissingKeysSplitPerDevice() {
        let different = SubscriptionBoard.build(snapshots: [
            snap("claude", "claude-max", "5h", pct: 10, device: "pc", key: "aaaa"),
            snap("claude", "claude-pro", "5h", pct: 90, device: "mac", key: "bbbb"),
        ], devices: [pc, mac], now: now)
        XCTAssertEqual(different.cards.map(\.planName), ["Claude Pro", "Claude Max"]) // closest to a limit first
        XCTAssertEqual(different.cards.map(\.deviceNames), [["MacBook"], ["Gaming PC"]])

        let missing = SubscriptionBoard.build(snapshots: [
            snap("codex", "chatgpt-plus", "5h", pct: 5, device: "pc"),
            snap("codex", "chatgpt-plus", "5h", pct: 5, device: "mac"),
        ], devices: [pc, mac], now: now)
        XCTAssertEqual(missing.cards.count, 2)
        XCTAssertEqual(Set(missing.cards.map(\.planName)), ["ChatGPT Plus"])
    }

    func testAntigravityGroupsAndNotInUse() throws {
        // Antigravity's groups, as the probe stores them: remaining converted to used, 1970 reset dropped.
        let rows = [
            snap("antigravity", "antigravity", "5h:gemini", pct: 1, device: "pc", label: "Gemini models"),
            snap("antigravity", "antigravity", "7d:gemini", pct: 99.6, device: "pc", resetIn: 86_400 * 2, label: "Gemini models"),
            snap("antigravity", "antigravity", "5h:third_party", pct: 0, device: "pc", resetIn: nil, label: "Claude and GPT models"),
            snap("antigravity", "antigravity", "7d:third_party", pct: 50, device: "pc", label: "Claude and GPT models"),
        ]
        let card = try XCTUnwrap(SubscriptionBoard.build(snapshots: rows, devices: [pc], now: now).cards.first)
        XCTAssertEqual(card.planName, "Antigravity")
        XCTAssertEqual(card.groups.map(\.title), ["Gemini models", "Claude and GPT models"])
        XCTAssertEqual(card.groups[0].meters.map(\.label), ["5-hour", "Weekly"])
        XCTAssertEqual(card.groups[0].meters[1].state(now: now), .nearlyUsed)
        XCTAssertEqual(card.groups[0].meters[1].warningWord(now: now), "Nearly used")
        XCTAssertEqual(card.groups[0].meters[1].valueText(now: now), "99.6% used")
        let thirdParty = card.groups[1].meters
        XCTAssertEqual(thirdParty[0].state(now: now), .notInUse)
        XCTAssertEqual(thirdParty[0].detailText(now: now), "Not in use. Starts when you next use it.")
        XCTAssertEqual(thirdParty[1].state(now: now), .normal)
    }

    func testClaudeScopesAreInlineAndSpendFollowsWindows() throws {
        let rows = [
            snap("claude", "claude-max", "7d:fable", pct: 20, device: "pc"),
            snap("claude", "claude-max", "7d", pct: 62, device: "pc"),
            snap("claude", "claude-max", "5h", pct: 30, device: "pc"),
            UsageSnapshot(account: "claude-max", provider: "claude", kind: "spend", window: "extra", usedPct: 20.5,
                          usedAmount: 4.10, limitAmount: 20, currency: "USD", source: "probe", deviceId: "pc", fetchedAt: nowSeconds - 120),
            snap("claude", "claude-max", "3h", pct: 1, device: "pc"),
        ]
        let card = try XCTUnwrap(SubscriptionBoard.build(snapshots: rows, devices: [pc], now: now).cards.first)
        XCTAssertEqual(card.groups.count, 1)
        XCTAssertNil(card.groups[0].title)
        XCTAssertEqual(card.groups[0].meters.map(\.label), ["3-hour", "5-hour", "Weekly", "Weekly \u{00B7} Fable", "Extra usage (paid)"])
        XCTAssertEqual(card.groups[0].meters.last?.amountText, "$4.10 of $20.00")
        XCTAssertEqual(card.maxUsedPct, 62)
    }

    func testGrokProductStaysInlineWithItsLabel() throws {
        let rows = [
            snap("grok", "supergrok", "7d", pct: 30, device: "pc"),
            snap("grok", "supergrok", "7d:grok_build", pct: 10, device: "pc", label: "Grok Build"),
        ]
        let card = try XCTUnwrap(SubscriptionBoard.build(snapshots: rows, devices: [pc], now: now).cards.first)
        XCTAssertEqual(card.planName, "SuperGrok")
        XCTAssertEqual(card.groups.count, 1)
        XCTAssertEqual(card.groups[0].meters.map(\.label), ["Weekly", "Weekly \u{00B7} Grok Build"])
    }

    func testMeterStatesAndSpeech() throws {
        let card = try XCTUnwrap(SubscriptionBoard.build(snapshots: [
            snap("claude", "claude-max", "5h", pct: 100, device: "pc"),
            snap("claude", "claude-max", "7d", pct: 62, device: "pc", resetIn: 2 * 3600 + 14 * 60),
            snap("codex", "chatgpt-plus", "5h", pct: 55, device: "pc", resetIn: -300),
        ], devices: [pc], now: now).cards.first { $0.provider == "claude" })
        let five = card.groups[0].meters[0]
        let weekly = card.groups[0].meters[1]
        XCTAssertEqual(five.state(now: now), .usedUp)
        XCTAssertEqual(five.warningWord(now: now), "Used up")
        XCTAssertEqual(weekly.spokenText(now: now), "Weekly, 62 percent used, resets in 2 hours 14 minutes")

        let past = try XCTUnwrap(SubscriptionBoard.build(snapshots: [snap("codex", "chatgpt-plus", "5h", pct: 55, device: "pc", resetIn: -300)],
                                                         devices: [pc], now: now).cards.first).groups[0].meters[0]
        XCTAssertEqual(past.state(now: now), .resetPassed)
        XCTAssertTrue(past.detailText(now: now, locale: Locale(identifier: "en_GB"), timeZone: TimeZone(identifier: "UTC")!)?.hasSuffix("Waiting for a new reading.") == true)
    }

    func testStaleReadingKeepsItsValues() throws {
        let board = SubscriptionBoard.build(snapshots: [
            snap("claude", "claude-max", "5h", pct: 30, device: "pc", age: 2 * 3600),
        ], devices: [pc], now: now)
        let card = try XCTUnwrap(board.cards.first)
        XCTAssertTrue(card.isStale(now: now))
        XCTAssertEqual(card.groups[0].meters[0].usedPct, 30)
        let fresh = SubscriptionBoard.build(snapshots: [snap("claude", "claude-max", "5h", pct: 30, device: "pc", age: 100)], devices: [pc], now: now)
        XCTAssertFalse(try XCTUnwrap(fresh.cards.first).isStale(now: now))
    }

    func testOnlyTheNewestProbeRowsOfADeviceAreShown() throws {
        let board = SubscriptionBoard.build(snapshots: [
            snap("codex", "chatgpt-plus", "5h", pct: 5, device: "pc", age: 100),
            snap("codex", "chatgpt-plus", "30d", pct: 77, device: "pc", age: 9000), // a window the probe no longer reports
        ], devices: [pc], now: now)
        XCTAssertEqual(board.cards[0].groups.flatMap(\.meters).map(\.label), ["5-hour"])
    }

    func testExpiredSignInKeepsTheCardAndExplainsIt() throws {
        let board = SubscriptionBoard.build(snapshots: [
            snap("grok", "supergrok", "7d", pct: 30, device: "pc", age: 7200),
            status("grok", "expired", device: "pc"),
        ], devices: [pc], now: now)
        let card = try XCTUnwrap(board.cards.first)
        XCTAssertTrue(card.degraded)
        XCTAssertTrue(card.isStale(now: now))
        XCTAssertEqual(card.notes, ["Sign-in expired on Gaming PC. Open Grok there to renew it."])
        XCTAssertEqual(board.notes, [])
    }

    func testStatusWithoutNumbersBecomesASlimNote() {
        let board = SubscriptionBoard.build(snapshots: [
            status("claude", "not signed in", device: "mac"),
            status("antigravity", "signed in with an API key, so it has no plan windows", device: "pc"),
            status("codex", "expired", device: "pc"),
        ], devices: [pc, mac], now: now)
        XCTAssertEqual(board.cards, [])
        XCTAssertEqual(board.notes.map(\.text), [
            "Antigravity on Gaming PC: signed in with an API key, so it has no plan windows.",
            "ChatGPT on Gaming PC: sign-in expired. Open Codex there to renew it.",
            "Claude on MacBook: not signed in.",
        ])
        XCTAssertEqual(board.notes.map(\.kind), [.unavailable, .expired, .notSignedIn])
    }

    func testReadingOffIsSaidOnTheDeviceThatHoldsTheSignIn() throws {
        let harness = try decode(ProviderInfo.self, #"""
        {"id":"codex@pc","kind":"harness","has_key":false,"via_device":"pc",
         "options":{"harness":"codex","via_device":"pc","signed_in":true,"sign_in":"signed_in"}}
        """#)
        let signedOut = try decode(ProviderInfo.self, #"""
        {"id":"claude-code@pc","kind":"harness","has_key":false,"via_device":"pc",
         "options":{"harness":"claude-code","via_device":"pc","signed_in":false}}
        """#)
        let off = DeviceInfo(id: "pc", name: "Gaming PC", platform: "windows", online: true, usageProbes: false)
        let board = SubscriptionBoard.build(snapshots: [], devices: [off], providers: [harness, signedOut], now: now)
        XCTAssertEqual(board.notes.count, 1)
        XCTAssertEqual(board.notes[0].kind, .readingOff)
        XCTAssertEqual(board.notes[0].text, "ChatGPT on Gaming PC. Reading is off on that device.")
        XCTAssertEqual(board.notes[0].hint, "Open Grouper on Gaming PC \u{203A} Settings \u{203A} This device to turn it on.")

        // Reading on, but this provider switched off on that device.
        let partly = DeviceInfo(id: "pc", name: "Gaming PC", platform: "windows", online: true, usageProbes: true, usageProbeProviders: ["claude"])
        XCTAssertEqual(SubscriptionBoard.build(snapshots: [], devices: [partly], providers: [harness], now: now).notes.count, 1)
        // On for all (empty list), or an older hub that says nothing: no note.
        let all = DeviceInfo(id: "pc", name: "Gaming PC", platform: "windows", online: true, usageProbes: true)
        XCTAssertEqual(SubscriptionBoard.build(snapshots: [], devices: [all], providers: [harness], now: now).notes, [])
        let unknown = DeviceInfo(id: "pc", name: "Gaming PC", platform: "windows", online: true)
        XCTAssertEqual(SubscriptionBoard.build(snapshots: [], devices: [unknown], providers: [harness], now: now).notes, [])
    }

    func testReadingTurnedOffMarksAnExistingCardStale() throws {
        let off = DeviceInfo(id: "pc", name: "Gaming PC", platform: "windows", online: true, usageProbes: false)
        let board = SubscriptionBoard.build(snapshots: [snap("claude", "claude-max", "5h", pct: 30, device: "pc", age: 60)], devices: [off], now: now)
        let card = try XCTUnwrap(board.cards.first)
        XCTAssertTrue(card.degraded)
        XCTAssertEqual(card.notes, ["Reading is off on Gaming PC."])
    }

    func testHubReadingsFromTheBalanceCollectorAreNotSubscriptionCards() {
        let balance = UsageSnapshot(account: "openrouter", provider: "openrouter", kind: "balance", window: "credits",
                                usedAmount: 12.4, limitAmount: 20, currency: "USD", source: "api", fetchedAt: nowSeconds - 300)
        XCTAssertTrue(SubscriptionBoard.build(snapshots: [balance], devices: [pc], now: now).isEmpty)
    }

    func testHarnessTokensFollowTheCardsDevices() throws {
        let summary = try decode(UsageSummary.self, #"""
        {"subscriptions":[
          {"provider":"claude-code@pc","harness":"claude","device_id":"pc","totals":{"calls":3,"input_tokens":100,"output_tokens":10}},
          {"provider":"claude-code@mac","harness":"claude-code","device_id":"mac","totals":{"calls":1,"input_tokens":50,"output_tokens":5}},
          {"provider":"codex@pc","harness":"codex","device_id":"pc","totals":{"calls":1,"input_tokens":7,"output_tokens":1}}]}
        """#)
        let both = try XCTUnwrap(summary.harnessTokens(for: .claude, deviceIds: ["pc", "mac"]))
        XCTAssertEqual(both.tokensIn, 150)
        XCTAssertEqual(both.tokensOut, 15)
        XCTAssertEqual(try XCTUnwrap(summary.harnessTokens(for: .claude, deviceIds: ["mac"])).tokensIn, 50)
        XCTAssertNil(summary.harnessTokens(for: .grok, deviceIds: ["pc"]))
    }

    func testProviderReports() {
        let rows = [
            UsageSnapshot(account: "openrouter", provider: "openrouter", kind: "balance", window: "credits", usedAmount: 12.4,
                          limitAmount: 20, currency: "USD", source: "api", fetchedAt: 1000),
            UsageSnapshot(account: "x", provider: "other", kind: "balance", window: "credits", usedAmount: 1,
                          limitAmount: 100, currency: "USD", source: "api", fetchedAt: 1000),
            UsageSnapshot(account: "openrouter", provider: "openrouter", kind: "window", window: "5h", usedPct: 3, source: "probe", fetchedAt: 1000),
        ]
        let lines = UsageProviderReports.lines(forProvider: "openrouter", in: rows)
        XCTAssertEqual(lines.map(\.text), ["Credits: $7.60 left of $20.00"]) // $12.40 used of $20.00 leaves $7.60 (as the web page reads a balance row)
        XCTAssertFalse(UsageProviderReports.isLowBalance(forProvider: "openrouter", in: rows))
        // "other" has $1 used of $100: $99 left. A key with $1.50 left of $20 is low.
        let low = UsageSnapshot(account: "o", provider: "o", kind: "balance", window: "credits", usedAmount: 18.5, limitAmount: 20,
                                currency: nil, source: "api", fetchedAt: 1)
        XCTAssertTrue(UsageProviderReports.isLowBalance(forProvider: "o", in: [low]))
        XCTAssertTrue(lines[0].isStale(now: Date(timeIntervalSince1970: 1000 + 3601)))
        XCTAssertFalse(lines[0].isStale(now: Date(timeIntervalSince1970: 1000 + 60)))
    }

    // MARK: - API calls

    func testSummaryAndColorRequests() async throws {
        let session = UsageStub.session { request in
            switch (request.httpMethod, request.url?.path) {
            case ("GET"?, "/v1/usage/summary"?):
                return (200, #"{"total":{"calls":1,"cost":0.5},"unpriced_calls":0,"keys":[],"subscriptions":[]}"#)
            case ("PUT"?, "/v1/providers/openrouter/color"?):
                return (200, #"{"id":"openrouter","kind":"openai","has_key":true,"color":"purple"}"#)
            case ("PUT"?, "/v1/providers/nope/color"?):
                return (404, #"{"error":"no such provider"}"#)
            default:
                return (405, "")
            }
        }
        let client = try HubClient(baseURL: "http://hub.test", token: "admin.token", session: session)
        let summary = try await client.usageSummary(since: 1_795_564_800, until: 1_795_600_000)
        XCTAssertEqual(summary.total.cost, 0.5, accuracy: 1e-9)
        let get = try XCTUnwrap(UsageStub.requests.first { $0.method == "GET" })
        XCTAssertEqual(get.authorization, "Bearer admin.token")
        XCTAssertTrue(get.query.contains("since=1795564800"), get.query)
        XCTAssertTrue(get.query.contains("until=1795600000"), get.query)

        let updated = try await client.setProviderColor(id: "openrouter", color: "purple")
        XCTAssertEqual(updated.color, "purple")
        let put = try XCTUnwrap(UsageStub.requests.last { $0.method == "PUT" })
        XCTAssertEqual(try XCTUnwrap(JSONSerialization.jsonObject(with: put.body) as? [String: String]), ["color": "purple"])

        // nil asks the hub to choose again, and must be sent as null rather than left out.
        _ = try await client.setProviderColor(id: "openrouter", color: nil)
        let reset = try XCTUnwrap(UsageStub.requests.last { $0.method == "PUT" })
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: reset.body) as? [String: Any])
        XCTAssertEqual(object.count, 1)
        XCTAssertTrue(object["color"] is NSNull)

        do {
            _ = try await client.setProviderColor(id: "nope", color: "teal")
            XCTFail("expected 404")
        } catch {
            XCTAssertEqual(error as? HubClientError, .notFound("no such provider"))
        }
    }
}

/// Answers requests from a closure, and records them (method, query, Authorization, body).
private final class UsageStub: URLProtocol, @unchecked Sendable {
    struct Seen { let method: String; let query: String; let authorization: String?; let body: Data }

    nonisolated(unsafe) static var handler: ((URLRequest) -> (Int, String))?
    nonisolated(unsafe) static var requests: [Seen] = []

    static func session(_ handler: @escaping (URLRequest) -> (Int, String)) -> URLSession {
        self.handler = handler
        requests = []
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [UsageStub.self]
        return URLSession(configuration: configuration)
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        let body = request.httpBody ?? request.httpBodyStream.map(Self.read) ?? Data()
        Self.requests.append(Seen(method: request.httpMethod ?? "GET", query: request.url?.query ?? "",
                                  authorization: request.value(forHTTPHeaderField: "Authorization"), body: body))
        let (status, text) = Self.handler?(request) ?? (500, "")
        let response = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: "HTTP/1.1",
                                       headerFields: ["Content-Type": "application/json"])!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(text.utf8))
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {}

    private static func read(_ stream: InputStream) -> Data {
        stream.open()
        defer { stream.close() }
        var data = Data()
        var buffer = [UInt8](repeating: 0, count: 4096)
        while stream.hasBytesAvailable {
            let count = stream.read(&buffer, maxLength: buffer.count)
            if count <= 0 { break }
            data.append(buffer, count: count)
        }
        return data
    }
}
