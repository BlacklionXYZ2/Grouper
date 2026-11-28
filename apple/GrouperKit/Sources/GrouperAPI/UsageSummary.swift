import Foundation

// The Usage tab's API-costs data (docs/usage-tab-spec.md sections 4 and 7.2): the
// `GET /v1/usage/summary` payload, the Day / Week / Month ranges, the donut's
// segments, the key colour names and the number formats. Foundation only, so the
// logic is tested on every platform; the SwiftUI side only draws it.

// MARK: - Payload

/// Totals of calls and tokens and their estimated cost. Every field defaults to
/// zero: a model row carries only `calls` and `cost`.
public struct UsageTotals: Codable, Equatable, Sendable {
    public let calls: UInt64
    public let inputTokens: UInt64
    public let outputTokens: UInt64
    public let cacheReadTokens: UInt64
    public let cacheWriteTokens: UInt64
    public let cost: Double

    public init(calls: UInt64 = 0, inputTokens: UInt64 = 0, outputTokens: UInt64 = 0,
                cacheReadTokens: UInt64 = 0, cacheWriteTokens: UInt64 = 0, cost: Double = 0) {
        self.calls = calls; self.inputTokens = inputTokens; self.outputTokens = outputTokens
        self.cacheReadTokens = cacheReadTokens; self.cacheWriteTokens = cacheWriteTokens; self.cost = cost
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        calls = try c.decodeIfPresent(UInt64.self, forKey: .calls) ?? 0
        inputTokens = try c.decodeIfPresent(UInt64.self, forKey: .inputTokens) ?? 0
        outputTokens = try c.decodeIfPresent(UInt64.self, forKey: .outputTokens) ?? 0
        cacheReadTokens = try c.decodeIfPresent(UInt64.self, forKey: .cacheReadTokens) ?? 0
        cacheWriteTokens = try c.decodeIfPresent(UInt64.self, forKey: .cacheWriteTokens) ?? 0
        cost = try c.decodeIfPresent(Double.self, forKey: .cost) ?? 0
    }

    /// Tokens in = input + cache-read + cache-write (spec 4.3).
    public var tokensIn: UInt64 { inputTokens &+ cacheReadTokens &+ cacheWriteTokens }
    /// Tokens out = output tokens.
    public var tokensOut: UInt64 { outputTokens }
    /// Every token, for the token-share donut.
    public var tokensTotal: UInt64 { tokensIn &+ outputTokens }
}

/// One model used through a key in the period.
public struct UsageKeyModel: Codable, Equatable, Identifiable, Sendable {
    public let model: String
    public let name: String
    public let free: Bool
    public let local: Bool
    public let priced: Bool
    public let totals: UsageTotals
    public var id: String { model }

    public init(model: String, name: String? = nil, free: Bool = false, local: Bool = false, priced: Bool = true,
                totals: UsageTotals = UsageTotals()) {
        self.model = model; self.name = name ?? model; self.free = free; self.local = local
        self.priced = priced; self.totals = totals
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        model = try c.decode(String.self, forKey: .model)
        name = try c.decodeIfPresent(String.self, forKey: .name) ?? model
        free = try c.decodeIfPresent(Bool.self, forKey: .free) ?? false
        local = try c.decodeIfPresent(Bool.self, forKey: .local) ?? false
        priced = try c.decodeIfPresent(Bool.self, forKey: .priced) ?? true
        totals = try c.decodeIfPresent(UsageTotals.self, forKey: .totals) ?? UsageTotals()
    }
}

/// One API provider (a "key") with its usage in the period.
public struct UsageKey: Codable, Equatable, Identifiable, Sendable {
    public let provider: String
    public let kind: String
    public let color: String?
    public let local: Bool
    public let removed: Bool
    public let totals: UsageTotals
    public let models: [UsageKeyModel]
    public var id: String { provider }

    public init(provider: String, kind: String = "openai", color: String? = nil, local: Bool = false,
                removed: Bool = false, totals: UsageTotals = UsageTotals(), models: [UsageKeyModel] = []) {
        self.provider = provider; self.kind = kind; self.color = color; self.local = local
        self.removed = removed; self.totals = totals; self.models = models
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        provider = try c.decode(String.self, forKey: .provider)
        kind = try c.decodeIfPresent(String.self, forKey: .kind) ?? ""
        color = try c.decodeIfPresent(String.self, forKey: .color)
        local = try c.decodeIfPresent(Bool.self, forKey: .local) ?? false
        removed = try c.decodeIfPresent(Bool.self, forKey: .removed) ?? false
        totals = try c.decodeIfPresent(UsageTotals.self, forKey: .totals) ?? UsageTotals()
        models = try c.decodeIfPresent([UsageKeyModel].self, forKey: .models) ?? []
    }

    /// The provider's id, as the rest of the app names it; a removed key says so.
    public var displayName: String { removed ? "Removed key (\(provider))" : provider }
}

/// Calls made through a harness (a subscription), metered at cost 0 (spec 5.5).
public struct UsageSubscriptionTotals: Codable, Equatable, Sendable {
    public let provider: String
    public let harness: String
    public let deviceId: String?
    public let totals: UsageTotals

    public init(provider: String, harness: String, deviceId: String? = nil, totals: UsageTotals = UsageTotals()) {
        self.provider = provider; self.harness = harness; self.deviceId = deviceId; self.totals = totals
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        provider = try c.decode(String.self, forKey: .provider)
        harness = try c.decodeIfPresent(String.self, forKey: .harness) ?? ""
        deviceId = try c.decodeIfPresent(String.self, forKey: .deviceId)
        totals = try c.decodeIfPresent(UsageTotals.self, forKey: .totals) ?? UsageTotals()
    }
}

/// `GET /v1/usage/summary` (spec 7.2): the donut and the key list in one call.
public struct UsageSummary: Codable, Equatable, Sendable {
    public let since: Int64?
    public let until: Int64?
    public let total: UsageTotals
    public let unpricedCalls: UInt64
    public let keys: [UsageKey]
    public let subscriptions: [UsageSubscriptionTotals]

    public init(since: Int64? = nil, until: Int64? = nil, total: UsageTotals = UsageTotals(), unpricedCalls: UInt64 = 0,
                keys: [UsageKey] = [], subscriptions: [UsageSubscriptionTotals] = []) {
        self.since = since; self.until = until; self.total = total; self.unpricedCalls = unpricedCalls
        self.keys = keys; self.subscriptions = subscriptions
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        since = try c.decodeIfPresent(Int64.self, forKey: .since)
        until = try c.decodeIfPresent(Int64.self, forKey: .until)
        total = try c.decodeIfPresent(UsageTotals.self, forKey: .total) ?? UsageTotals()
        unpricedCalls = try c.decodeIfPresent(UInt64.self, forKey: .unpricedCalls) ?? 0
        keys = try c.decodeIfPresent([UsageKey].self, forKey: .keys) ?? []
        subscriptions = try c.decodeIfPresent([UsageSubscriptionTotals].self, forKey: .subscriptions) ?? []
    }

    /// Tokens used through harnesses on these devices for one provider (`claude`, `codex`, …),
    /// summed for the Subscriptions card's "Through Grouper" line. Nil when there are none.
    public func harnessTokens(for provider: SubscriptionProvider, deviceIds: Set<String>) -> (tokensIn: UInt64, tokensOut: UInt64)? {
        var tokensIn: UInt64 = 0
        var tokensOut: UInt64 = 0
        for row in subscriptions where SubscriptionProvider(harness: row.harness) == provider {
            if let device = row.deviceId, !deviceIds.isEmpty, !deviceIds.contains(device) { continue }
            tokensIn &+= row.totals.tokensIn
            tokensOut &+= row.totals.tokensOut
        }
        return tokensIn == 0 && tokensOut == 0 ? nil : (tokensIn, tokensOut)
    }
}

// MARK: - Periods

/// The Day / Week / Month choice (spec 4.1): rolling ranges in the device's local time.
public enum UsagePeriod: String, CaseIterable, Identifiable, Sendable {
    case day, week, month

    public var id: String { rawValue }
    public var title: String {
        switch self { case .day: return "Day"; case .week: return "Week"; case .month: return "Month" }
    }
    /// The caption under the total.
    public var caption: String {
        switch self { case .day: return "Today"; case .week: return "Last 7 days"; case .month: return "Last 30 days" }
    }
    /// For a sentence: "API costs, last 7 days, …".
    public var phrase: String {
        switch self { case .day: return "today"; case .week: return "the last 7 days"; case .month: return "the last 30 days" }
    }
    /// Local midnights back from today: 0, 6 or 29 (7 or 30 days including today).
    public var daysBack: Int {
        switch self { case .day: return 0; case .week: return 6; case .month: return 29 }
    }

    /// Local midnight `daysBack` days ago up to now, as Unix seconds. The hub needs no
    /// time-zone setting: the client sends the instants. `Calendar` steps by calendar
    /// days, so a 23- or 25-hour day around a clock change still starts at midnight.
    public func range(now: Date = Date(), calendar: Calendar = .current) -> UsageRange {
        let today = calendar.startOfDay(for: now)
        let start = calendar.date(byAdding: .day, value: -daysBack, to: today) ?? today
        return UsageRange(since: Int64(start.timeIntervalSince1970), until: Int64(now.timeIntervalSince1970))
    }
}

/// `[since, until)` in Unix seconds.
public struct UsageRange: Equatable, Sendable {
    public let since: Int64
    public let until: Int64
    public init(since: Int64, until: Int64) { self.since = since; self.until = until }
}

// MARK: - Key colours (spec 4.4)

/// The ten named key colours. The hub stores the name; each client maps it to its own
/// theme value, so contrast modes keep working. Grey is reserved for "Other" and removed keys.
public enum KeyColorName: String, CaseIterable, Identifiable, Sendable {
    case teal, purple, orange, pink, blue, green, amber, rose, indigo, cyan

    public var id: String { rawValue }
    public var title: String { String(rawValue.prefix(1)).uppercased() + String(rawValue.dropFirst()) }
}

// MARK: - Donut

/// One segment of the donut: a key, or the merged "Other".
public struct DonutSegment: Equatable, Identifiable, Sendable {
    public let id: String
    public let title: String
    /// A `KeyColorName` raw value; nil for "Other", removed keys and keys with no colour.
    public let colorName: String?
    public let isOther: Bool
    /// The real figure: cost in dollars, or tokens in token mode.
    public let value: Double
    /// Share of the ring's total, 0...1.
    public let share: Double
    /// The angle weight to draw: `share`, but never under 3 degrees so a small key stays visible.
    public let weight: Double
    /// Provider ids this segment stands for (several for "Other").
    public let providers: [String]
}

public struct DonutModel: Equatable, Sendable {
    public enum Basis: Equatable, Sendable { case cost, tokens, empty }
    public let basis: Basis
    public let segments: [DonutSegment]
    /// Sum of the real figures (dollars or tokens).
    public let total: Double

    public func segment(forProvider id: String) -> DonutSegment? {
        segments.first { $0.providers.contains(id) }
    }
}

public enum UsageDonut {
    /// More paid keys than this are folded into "Other" (the smallest ones).
    public static let maxSegments = 8
    /// A paid key always gets at least this much arc.
    public static let minimumDegrees = 3.0

    /// Builds the ring (spec 4.2): by cost; by tokens when nothing cost money but tokens were
    /// used; empty when there were no calls.
    public static func build(keys: [UsageKey]) -> DonutModel {
        let paid = keys.filter { $0.totals.cost > 0 }
        if !paid.isEmpty {
            return model(basis: .cost, entries: paid.map { ($0, $0.totals.cost) })
        }
        let withTokens = keys.filter { $0.totals.tokensTotal > 0 }
        if !withTokens.isEmpty {
            return model(basis: .tokens, entries: withTokens.map { ($0, Double($0.totals.tokensTotal)) })
        }
        return DonutModel(basis: .empty, segments: [], total: 0)
    }

    private static func model(basis: DonutModel.Basis, entries: [(UsageKey, Double)]) -> DonutModel {
        let sorted = entries.sorted { a, b in
            a.1 != b.1 ? a.1 > b.1 : a.0.provider < b.0.provider
        }
        let total = sorted.reduce(0) { $0 + $1.1 }
        guard total > 0 else { return DonutModel(basis: .empty, segments: [], total: 0) }
        let minShare = minimumDegrees / 360
        func segment(_ id: String, _ title: String, _ color: String?, other: Bool, _ value: Double, _ providers: [String]) -> DonutSegment {
            let share = value / total
            return DonutSegment(id: id, title: title, colorName: color, isOther: other, value: value, share: share,
                                weight: max(share, minShare), providers: providers)
        }
        var segments: [DonutSegment] = []
        for (key, value) in sorted.prefix(maxSegments) {
            segments.append(segment(key.provider, key.displayName, key.removed ? nil : key.color, other: false, value, [key.provider]))
        }
        let rest = Array(sorted.dropFirst(maxSegments))
        if !rest.isEmpty {
            let value = rest.reduce(0) { $0 + $1.1 }
            segments.append(segment("other", "Other", nil, other: true, value, rest.map { $0.0.provider }))
        }
        return DonutModel(basis: basis, segments: segments, total: total)
    }

    /// The chart's one spoken summary (spec 8): "API costs, the last 7 days, $4.82 estimated.
    /// OpenRouter $1.83, 38%. …".
    public static func accessibilitySummary(_ model: DonutModel, period: UsagePeriod, totals: UsageTotals) -> String {
        switch model.basis {
        case .empty:
            return "API costs, \(period.phrase), \(UsageFormat.money(totals.cost)) estimated. No calls through Grouper in this period."
        case .cost:
            let parts = model.segments.map { "\($0.title) \(UsageFormat.money($0.value)), \(UsageFormat.percent($0.share))." }
            return "API costs, \(period.phrase), \(UsageFormat.money(totals.cost)) estimated. " + parts.joined(separator: " ")
        case .tokens:
            let parts = model.segments.map { "\($0.title) \(UsageFormat.exactTokens(UInt64($0.value))) tokens, \(UsageFormat.percent($0.share))." }
            return "API usage, \(period.phrase). No paid usage; share of tokens. " + parts.joined(separator: " ")
        }
    }
}

// MARK: - Provider-reported figures (spec 4.3, "Provider reports")

/// One line of what a provider's own balance or spend API says about a key.
public struct ProviderReportLine: Equatable, Identifiable, Sendable {
    public let id: String
    /// "Credits: $12.40 left of $20.00".
    public let text: String
    public let fetchedAt: UInt64
    /// Credits under 10% of the limit.
    public let low: Bool

    /// Older than an hour (spec: the "Stale" label).
    public func isStale(now: Date) -> Bool {
        Date(timeIntervalSince1970: TimeInterval(fetchedAt)).distance(to: now) > 3600
    }
}

public enum UsageProviderReports {
    /// The hub's own readings (balance, spend, free-request allowance) for one key: snapshots
    /// whose provider is the key's id and whose source is not a device probe.
    public static func lines(forProvider provider: String, in snapshots: [UsageSnapshot]) -> [ProviderReportLine] {
        snapshots
            .filter { $0.provider == provider && $0.source != "probe" && $0.kind != "status" }
            .sorted { ($0.kind, $0.window) < ($1.kind, $1.window) }
            .compactMap(line)
    }

    private static func line(_ s: UsageSnapshot) -> ProviderReportLine? {
        let id = "\(s.provider)|\(s.kind)|\(s.window)|\(s.currency ?? "")"
        switch s.kind {
        case "balance":
            guard let used = s.usedAmount else { return nil }
            if let limit = s.limitAmount, limit > 0 {
                let left = max(0, limit - used)
                return ProviderReportLine(id: id, text: "Credits: \(UsageFormat.amount(left, currency: s.currency)) left of \(UsageFormat.amount(limit, currency: s.currency))",
                                          fetchedAt: s.fetchedAt, low: left / limit < 0.10)
            }
            return ProviderReportLine(id: id, text: "Balance: \(UsageFormat.amount(used, currency: s.currency))", fetchedAt: s.fetchedAt, low: false)
        case "spend":
            guard let used = s.usedAmount else { return nil }
            let words = ["monthly": "this month", "weekly": "this week", "daily": "today", "total": "all time"][s.window] ?? s.window
            if let limit = s.limitAmount {
                return ProviderReportLine(id: id, text: "Key spend, \(words): \(UsageFormat.amount(used, currency: s.currency)) of \(UsageFormat.amount(limit, currency: s.currency)) limit",
                                          fetchedAt: s.fetchedAt, low: false)
            }
            return ProviderReportLine(id: id, text: "Key spend, \(words): \(UsageFormat.amount(used, currency: s.currency))", fetchedAt: s.fetchedAt, low: false)
        default:
            // OpenRouter's free-request allowance and other counted allowances.
            guard let limit = s.limitAmount, let used = s.usedAmount else {
                guard let pct = s.usedPct else { return nil }
                return ProviderReportLine(id: id, text: "\(s.kind.capitalized): \(UsageFormat.usedPercent(pct)) used", fetchedAt: s.fetchedAt, low: false)
            }
            let left = max(0, limit - used)
            return ProviderReportLine(id: id, text: "Free requests: \(UsageFormat.count(left)) left of \(UsageFormat.count(limit))",
                                      fetchedAt: s.fetchedAt, low: false)
        }
    }

    /// True when any report says credits are under 10% of the limit ("Low balance").
    public static func isLowBalance(forProvider provider: String, in snapshots: [UsageSnapshot]) -> Bool {
        lines(forProvider: provider, in: snapshots).contains { $0.low }
    }
}

// MARK: - Formats

public enum UsageFormat {
    /// "$4.82". A non-zero amount under half a cent reads "<$0.01".
    public static func money(_ value: Double) -> String {
        if value > 0, value < 0.005 { return "<$0.01" }
        return "$" + String(format: "%.2f", value)
    }

    /// An amount in the provider's currency; USD (or none) uses "$".
    public static func amount(_ value: Double, currency: String?) -> String {
        let code = (currency ?? "USD").uppercased()
        switch code {
        case "USD": return "$" + String(format: "%.2f", value)
        case "EUR": return "\u{20AC}" + String(format: "%.2f", value)
        case "GBP": return "\u{00A3}" + String(format: "%.2f", value)
        default: return code + " " + String(format: "%.2f", value)
        }
    }

    /// 812, 38K, 412K, 1.84M: abbreviated, with the exact figure in `exactTokens`.
    public static func tokens(_ n: UInt64) -> String {
        if n < 1_000 { return String(n) }
        if n < 999_500 { return scaled(Double(n) / 1_000, "K") }
        if n < 999_500_000 { return scaled(Double(n) / 1_000_000, "M") }
        return scaled(Double(n) / 1_000_000_000, "B")
    }

    private static func scaled(_ x: Double, _ suffix: String) -> String {
        let text = x >= 10 ? String(format: "%.0f", x) : String(format: "%.2f", x)
        guard text.contains(".") else { return text + suffix }
        var trimmed = text
        while trimmed.hasSuffix("0") { trimmed.removeLast() }
        if trimmed.hasSuffix(".") { trimmed.removeLast() }
        return trimmed + suffix
    }

    /// 1,234,567 (for the accessible label and the tooltip).
    public static func exactTokens(_ n: UInt64) -> String {
        let digits = Array(String(n))
        var out = ""
        for (index, digit) in digits.enumerated() {
            if index > 0, (digits.count - index) % 3 == 0 { out.append(",") }
            out.append(digit)
        }
        return out
    }

    /// "31 calls", "1 call".
    public static func calls(_ n: UInt64) -> String { "\(n) \(n == 1 ? "call" : "calls")" }

    /// A plain count with thousands separators (free-request allowances).
    public static func count(_ value: Double) -> String { exactTokens(UInt64(max(0, value.rounded()))) }

    /// "38%". Under half a percent reads "<1%".
    public static func percent(_ share: Double) -> String {
        if share > 0, share < 0.005 { return "<1%" }
        return "\(Int((share * 100).rounded()))%"
    }

    /// The used share of a meter: "62%", "99.6%". Under 100 it never rounds up to "100%".
    public static func usedPercent(_ pct: Double) -> String {
        let clamped = min(max(pct, 0), 100)
        var tenths = (clamped * 10).rounded() / 10
        if clamped < 100 { tenths = min(tenths, 99.9) }
        if tenths == tenths.rounded() { return "\(Int(tenths))%" }
        return String(format: "%.1f%%", tenths)
    }

    /// "412K in · 38K out · 31 calls".
    public static func tokenLine(_ totals: UsageTotals) -> String {
        "\(tokens(totals.tokensIn)) in \u{00B7} \(tokens(totals.tokensOut)) out \u{00B7} \(calls(totals.calls))"
    }

    /// The same, in words for VoiceOver, with exact figures.
    public static func tokenSpeech(_ totals: UsageTotals) -> String {
        "\(exactTokens(totals.tokensIn)) tokens in, \(exactTokens(totals.tokensOut)) out, \(calls(totals.calls))"
    }

    /// "resets in 38 min", "resets in 2h 14m", or "resets Thu 3 Dec, 09:00" in local time.
    public static func resetText(resetsAt: UInt64, now: Date, locale: Locale = .current, timeZone: TimeZone = .current) -> String {
        let secs = Int64(resetsAt) - Int64(now.timeIntervalSince1970)
        let minutes = max(1, (secs + 59) / 60)
        if minutes < 60 { return "resets in \(minutes) min" }
        if minutes < 24 * 60 {
            let hours = minutes / 60
            let rest = minutes % 60
            return rest == 0 ? "resets in \(hours)h" : "resets in \(hours)h \(rest)m"
        }
        return "resets " + dateTime(resetsAt, locale: locale, timeZone: timeZone)
    }

    /// The same, for VoiceOver: "resets in 2 hours 14 minutes".
    public static func resetSpeech(resetsAt: UInt64, now: Date, locale: Locale = .current, timeZone: TimeZone = .current) -> String {
        let secs = Int64(resetsAt) - Int64(now.timeIntervalSince1970)
        let minutes = max(1, (secs + 59) / 60)
        func unit(_ n: Int64, _ word: String) -> String { "\(n) \(word)\(n == 1 ? "" : "s")" }
        if minutes < 60 { return "resets in \(unit(minutes, "minute"))" }
        if minutes < 24 * 60 {
            let hours = minutes / 60
            let rest = minutes % 60
            return rest == 0 ? "resets in \(unit(hours, "hour"))" : "resets in \(unit(hours, "hour")) \(unit(rest, "minute"))"
        }
        return "resets " + dateTime(resetsAt, locale: locale, timeZone: timeZone)
    }

    /// "Thu 3 Dec, 09:00" in the given zone (the locale decides 12 or 24 hour clocks).
    public static func dateTime(_ seconds: UInt64, locale: Locale = .current, timeZone: TimeZone = .current) -> String {
        let formatter = DateFormatter()
        formatter.locale = locale
        formatter.timeZone = timeZone
        formatter.setLocalizedDateFormatFromTemplate("EEEdMMMjmm")
        return formatter.string(from: Date(timeIntervalSince1970: TimeInterval(seconds)))
    }

    /// "09:00" in the given zone.
    public static func clock(_ seconds: UInt64, locale: Locale = .current, timeZone: TimeZone = .current) -> String {
        let formatter = DateFormatter()
        formatter.locale = locale
        formatter.timeZone = timeZone
        formatter.setLocalizedDateFormatFromTemplate("jmm")
        return formatter.string(from: Date(timeIntervalSince1970: TimeInterval(seconds)))
    }
}

// MARK: - Offline copy

/// The last good copy of the Usage page, so it can be shown offline (spec 8, "Offline").
public struct UsageCacheFile: Codable, Equatable, Sendable {
    public struct TimedSummary: Codable, Equatable, Sendable {
        public let period: String
        public let fetchedAt: Double
        public let summary: UsageSummary
        public init(period: String, fetchedAt: Double, summary: UsageSummary) {
            self.period = period; self.fetchedAt = fetchedAt; self.summary = summary
        }
    }

    /// The hub the copy came from; a copy for another hub is ignored.
    public var hub: String
    public var summaries: [TimedSummary]
    public var snapshots: [UsageSnapshot]?
    public var snapshotsAt: Double?
    public var limits: LimitsReport?
    public var limitsAt: Double?

    public init(hub: String, summaries: [TimedSummary] = [], snapshots: [UsageSnapshot]? = nil, snapshotsAt: Double? = nil,
                limits: LimitsReport? = nil, limitsAt: Double? = nil) {
        self.hub = hub; self.summaries = summaries; self.snapshots = snapshots; self.snapshotsAt = snapshotsAt
        self.limits = limits; self.limitsAt = limitsAt
    }

    public func encoded() -> Data? { try? JSONEncoder.grouper.encode(self) }

    /// The saved copy for `hub`, or nil when there is none, it is unreadable, or it is another hub's.
    public static func decode(_ data: Data?, hub: String) -> UsageCacheFile? {
        guard let data, let file = try? JSONDecoder.grouper.decode(UsageCacheFile.self, from: data), file.hub == hub else { return nil }
        return file
    }
}
