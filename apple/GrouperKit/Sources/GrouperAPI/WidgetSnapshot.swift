import Foundation

// The Home Screen and Lock Screen widgets' data. The widget extension has no sign-in of its own:
// the app writes this compact snapshot into the App Group container whenever the Usage page loads
// or refreshes, and the widget only reads it. So this is the whole contract between the two, plus
// the small rules the widgets need (which account is "closest to a limit", how old the figures
// are, when to look again). Foundation only, so it is tested on every platform.
//
// What goes in: the API-costs summary per period with the models stripped (the donut and a key
// list need only the keys' totals), and the subscription cards as plain values. What stays out:
// tokens, keys, the hub address and anything else that is not on the Usage page.

// MARK: - Subscription mirror

/// A `SubscriptionMeter` as plain Codable values (its own initialiser is not public and the type
/// is not Codable).
public struct WidgetMeter: Codable, Equatable, Sendable {
    public var id: String
    /// `window`, `spend`, `balance` or `other`.
    public var kind: String
    public var label: String
    public var spanLabel: String
    public var usedPct: Double?
    public var amountText: String?
    public var resetsAt: UInt64?
    /// Nil when the window has no span ("Extra usage").
    public var spanSeconds: Int64?
    public var scope: String?
    public var scopeLabel: String?

    public init(id: String, kind: String = "window", label: String, spanLabel: String? = nil, usedPct: Double? = nil,
                amountText: String? = nil, resetsAt: UInt64? = nil, spanSeconds: Int64? = nil,
                scope: String? = nil, scopeLabel: String? = nil) {
        self.id = id; self.kind = kind; self.label = label; self.spanLabel = spanLabel ?? label
        self.usedPct = usedPct; self.amountText = amountText; self.resetsAt = resetsAt
        self.spanSeconds = spanSeconds; self.scope = scope; self.scopeLabel = scopeLabel
    }

    public init(_ meter: SubscriptionMeter) {
        let kindName: String
        switch meter.kind {
        case .window: kindName = "window"
        case .spend: kindName = "spend"
        case .balance: kindName = "balance"
        case .other: kindName = "other"
        }
        self.init(id: meter.id, kind: kindName, label: meter.label, spanLabel: meter.spanLabel, usedPct: meter.usedPct,
                  amountText: meter.amountText, resetsAt: meter.resetsAt,
                  spanSeconds: meter.spanSeconds == Int64.max ? nil : meter.spanSeconds,
                  scope: meter.scope, scopeLabel: meter.scopeLabel)
    }

    /// The real meter, so the widget uses the same state, warning words and reset text as the app.
    public var meter: SubscriptionMeter { SubscriptionMeter(widget: self) }
}

extension SubscriptionMeter {
    init(widget w: WidgetMeter) {
        let k: Kind
        switch w.kind {
        case "window": k = .window
        case "spend": k = .spend
        case "balance": k = .balance
        default: k = .other
        }
        id = w.id; kind = k; label = w.label; spanLabel = w.spanLabel; usedPct = w.usedPct
        amountText = w.amountText; resetsAt = w.resetsAt; spanSeconds = w.spanSeconds ?? Int64.max
        scope = w.scope; scopeLabel = w.scopeLabel
    }
}

public struct WidgetGroup: Codable, Equatable, Sendable {
    public var id: String
    /// Antigravity's "Gemini models"; nil for a card's plain meters.
    public var title: String?
    public var meters: [WidgetMeter]

    public init(id: String, title: String? = nil, meters: [WidgetMeter] = []) {
        self.id = id; self.title = title; self.meters = meters
    }
}

/// One bar for a small or medium widget: the meter and its label, with the group heading folded
/// in ("Gemini models · 5-hour") because those widgets have no room for headings.
public struct WidgetRow: Equatable, Identifiable, Sendable {
    public let label: String
    public let meter: SubscriptionMeter
    public var id: String { meter.id + "|" + label }
}

/// One subscription account (a `SubscriptionCard`) as plain values.
public struct WidgetAccount: Codable, Equatable, Identifiable, Sendable {
    public var id: String
    /// The probe's provider name: `claude`, `codex`, `grok`, `antigravity`.
    public var provider: String
    public var planName: String
    public var deviceNames: [String]
    public var fetchedAt: UInt64
    public var groups: [WidgetGroup]
    public var notes: [String]
    public var degraded: Bool

    public init(id: String, provider: String, planName: String, deviceNames: [String] = [], fetchedAt: UInt64,
                groups: [WidgetGroup] = [], notes: [String] = [], degraded: Bool = false) {
        self.id = id; self.provider = provider; self.planName = planName; self.deviceNames = deviceNames
        self.fetchedAt = fetchedAt; self.groups = groups; self.notes = notes; self.degraded = degraded
    }

    public init(_ card: SubscriptionCard) {
        self.init(id: card.id, provider: card.provider, planName: card.planName, deviceNames: card.deviceNames,
                  fetchedAt: card.fetchedAt,
                  groups: card.groups.map { WidgetGroup(id: $0.id, title: $0.title, meters: $0.meters.map(WidgetMeter.init)) },
                  notes: card.notes, degraded: card.degraded)
    }

    /// The card, so the widget can ask it for states and text.
    public var card: SubscriptionCard {
        let groups = self.groups.map { SubscriptionGroup(id: $0.id, title: $0.title, meters: $0.meters.map { $0.meter }) }
        let top = groups.flatMap(\.meters).filter { $0.kind == .window }.compactMap(\.usedPct).max() ?? 0
        return SubscriptionCard(id: id, provider: provider, kind: SubscriptionProvider(probeName: provider), planName: planName,
                                deviceNames: deviceNames, deviceIds: [], fetchedAt: fetchedAt, groups: groups, notes: notes,
                                degraded: degraded, maxUsedPct: top)
    }

    /// For the account picker: "Claude Max" with "Signed in on Gaming PC" under it.
    public var subtitle: String { "Signed in on " + SubscriptionBoard.names(deviceNames) }
}

// MARK: - Card helpers

extension SubscriptionCard {
    /// Every meter on the card, in the card's order.
    public var allMeters: [SubscriptionMeter] { groups.flatMap(\.meters) }

    /// The highest used percent among windows that are live now: not "not in use", and not past their
    /// reset (an old 100% must not outrank a window that has since reset). Nil when none is live.
    public func currentMaxUsedPct(now: Date) -> Double? {
        allMeters.filter { $0.kind == .window && Self.isLive($0, now: now) }.compactMap(\.usedPct).max()
    }

    static func isLive(_ meter: SubscriptionMeter, now: Date) -> Bool {
        switch meter.state(now: now) {
        case .notInUse, .resetPassed: return false
        case .normal, .nearlyUsed, .usedUp: return meter.usedPct != nil
        }
    }

    /// The bars for a small or medium widget, with group headings folded into the labels. When
    /// there are more than `limit`, the most used keep their place and the rest are dropped
    /// (window order is kept).
    public func rows(limit: Int) -> [WidgetRow] {
        var rows: [WidgetRow] = []
        for group in groups {
            for meter in group.meters {
                let label = group.title.map { "\($0) \u{00B7} \(meter.spanLabel)" } ?? meter.label
                rows.append(WidgetRow(label: label, meter: meter))
            }
        }
        guard limit > 0, rows.count > limit else { return limit > 0 ? rows : [] }
        // Keep the `limit` most used (ties keep the earlier row), then restore the card's order.
        let ranked = rows.enumerated().sorted { a, b in
            let pa = a.element.meter.usedPct ?? -1, pb = b.element.meter.usedPct ?? -1
            return pa != pb ? pa > pb : a.offset < b.offset
        }
        let keep = Set(ranked.prefix(limit).map(\.offset))
        return rows.enumerated().filter { keep.contains($0.offset) }.map(\.element)
    }
}

/// The most used window across all accounts, for the Lock Screen gauge.
public struct WidgetWindow: Equatable, Sendable {
    public let card: SubscriptionCard
    public let meter: SubscriptionMeter
}

// MARK: - Freshness

/// How old a figure is, in words, and whether it should read as stale.
public struct WidgetFreshness: Equatable, Sendable {
    public let isStale: Bool
    /// "Updated 12 min ago".
    public let updatedText: String
    /// Why it is stale and what to do, when it is.
    public let note: String?

    public init(isStale: Bool, updatedText: String, note: String? = nil) {
        self.isStale = isStale; self.updatedText = updatedText; self.note = note
    }

    /// A card whose reading is old, or whose device has a problem, however recently the app loaded it:
    /// "Read 2h ago", always stale.
    public static func staleReading(fetchedAt: Date, now: Date) -> WidgetFreshness {
        WidgetFreshness(isStale: true, updatedText: "Read " + agoText(max(0, now.timeIntervalSince(fetchedAt))),
                        note: "Open Grouper to refresh.")
    }

    public static func make(fetchedAt: Date, now: Date, rolledOver: Bool = false) -> WidgetFreshness {
        let age = max(0, now.timeIntervalSince(fetchedAt))
        let stale = rolledOver || age > WidgetSnapshot.staleAfter
        let note: String?
        if rolledOver { note = "From an earlier day. Open Grouper to refresh." }
        else if stale { note = "Open Grouper to refresh." }
        else { note = nil }
        return WidgetFreshness(isStale: stale, updatedText: "Updated " + agoText(age), note: note)
    }

    /// "just now", "12 min ago", "3h ago", "2d ago".
    public static func agoText(_ age: TimeInterval) -> String {
        let minutes = Int(max(0, age) / 60)
        if minutes < 1 { return "just now" }
        if minutes < 60 { return "\(minutes) min ago" }
        let hours = minutes / 60
        if hours < 24 { return "\(hours)h ago" }
        return "\(hours / 24)d ago"
    }
}

/// The costs for one period, ready to draw.
public struct WidgetCosts: Equatable, Sendable {
    public let period: UsagePeriod
    public let summary: UsageSummary
    public let fetchedAt: Date
    public let donut: DonutModel
    public let freshness: WidgetFreshness

    /// Keys with a figure, largest first, for the list under the donut (the donut's own order).
    public var rankedKeys: [UsageKey] {
        let tokens = donut.basis == .tokens
        let figure: (UsageKey) -> Double = { tokens ? Double($0.totals.tokensTotal) : $0.totals.cost }
        return summary.keys.filter { figure($0) > 0 }
            .sorted { a, b in figure(a) != figure(b) ? figure(a) > figure(b) : a.provider < b.provider }
    }
}

// MARK: - The snapshot

public struct WidgetSnapshot: Codable, Equatable, Sendable {
    public static let currentVersion = 1
    /// Figures older than an hour read as stale (the Subscriptions page's own rule).
    public static let staleAfter: TimeInterval = 3600
    /// How often the widget asks WidgetKit to look at the file again.
    public static let reloadInterval: TimeInterval = 30 * 60

    public var version: Int
    /// When the file was last written.
    public var writtenAt: Double
    /// Per period (`UsagePeriod.rawValue`), models stripped.
    public var costs: [UsageCacheFile.TimedSummary]
    public var accounts: [WidgetAccount]
    /// When the app last loaded the subscription readings; nil until it has.
    public var accountsAt: Double?

    public init(writtenAt: Double = Date().timeIntervalSince1970, costs: [UsageCacheFile.TimedSummary] = [],
                accounts: [WidgetAccount] = [], accountsAt: Double? = nil) {
        self.version = WidgetSnapshot.currentVersion
        self.writtenAt = writtenAt; self.costs = costs; self.accounts = accounts; self.accountsAt = accountsAt
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        version = try c.decodeIfPresent(Int.self, forKey: .version) ?? WidgetSnapshot.currentVersion
        writtenAt = try c.decodeIfPresent(Double.self, forKey: .writtenAt) ?? 0
        costs = try c.decodeIfPresent([UsageCacheFile.TimedSummary].self, forKey: .costs) ?? []
        accounts = try c.decodeIfPresent([WidgetAccount].self, forKey: .accounts) ?? []
        accountsAt = try c.decodeIfPresent(Double.self, forKey: .accountsAt)
    }

    // MARK: Encoding

    public func encoded() -> Data? { try? JSONEncoder.grouper.encode(self) }

    /// The saved snapshot, or nil when there is none, it is unreadable, or it was written by a newer
    /// app than this widget understands (the widget then asks to open Grouper).
    public static func decode(_ data: Data?) -> WidgetSnapshot? {
        guard let data, let snapshot = try? JSONDecoder.grouper.decode(WidgetSnapshot.self, from: data),
              snapshot.version <= currentVersion else { return nil }
        return snapshot
    }

    // MARK: Building from the app's data

    /// The costs for the widget: the periods the app has loaded, models and harness rows dropped.
    public static func compactCosts(_ summaries: [UsageCacheFile.TimedSummary]) -> [UsageCacheFile.TimedSummary] {
        summaries.sorted { $0.period < $1.period }.map { timed in
            let s = timed.summary
            let keys = s.keys.map {
                UsageKey(provider: $0.provider, kind: $0.kind, color: $0.color, local: $0.local, removed: $0.removed,
                         totals: $0.totals, models: [])
            }
            return UsageCacheFile.TimedSummary(
                period: timed.period, fetchedAt: timed.fetchedAt,
                summary: UsageSummary(since: s.since, until: s.until, total: s.total, unpricedCalls: s.unpricedCalls,
                                      keys: keys, subscriptions: []))
        }
    }

    // MARK: Costs

    /// The costs for a period, or nil when the app has not loaded that period yet.
    public func costs(for period: UsagePeriod, now: Date, calendar: Calendar = .current) -> WidgetCosts? {
        guard let timed = costs.first(where: { $0.period == period.rawValue }) else { return nil }
        let fetched = Date(timeIntervalSince1970: timed.fetchedAt)
        // A "today" copied yesterday is not today's figure: the rolling range has moved on.
        let rolledOver = period.range(now: fetched, calendar: calendar).since != period.range(now: now, calendar: calendar).since
        return WidgetCosts(period: period, summary: timed.summary, fetchedAt: fetched,
                           donut: UsageDonut.build(keys: timed.summary.keys),
                           freshness: .make(fetchedAt: fetched, now: now, rolledOver: rolledOver && period == .day))
    }

    // MARK: Subscriptions

    public func account(id: String?) -> WidgetAccount? {
        guard let id else { return nil }
        return accounts.first { $0.id == id }
    }

    /// The account with the highest live window (a tie goes to the first listed, which the board
    /// already orders), or the first account when nothing is live. Nil when there are no accounts.
    public func closestToLimit(now: Date) -> WidgetAccount? {
        var best: (account: WidgetAccount, pct: Double)?
        for account in accounts {
            guard let pct = account.card.currentMaxUsedPct(now: now) else { continue }
            if best == nil || pct > best!.pct { best = (account, pct) }
        }
        return best?.account ?? accounts.first
    }

    /// The account a widget shows: the chosen one if it still exists, otherwise the closest to a limit.
    public func resolvedAccount(chosenId: String?, now: Date) -> WidgetAccount? {
        account(id: chosenId) ?? closestToLimit(now: now)
    }

    /// Accounts in the order a large widget lists them: the most used first, ties keep the board's order.
    public func accountsByUse(now: Date) -> [WidgetAccount] {
        accounts.enumerated().sorted { a, b in
            let pa = a.element.card.currentMaxUsedPct(now: now) ?? -1
            let pb = b.element.card.currentMaxUsedPct(now: now) ?? -1
            return pa != pb ? pa > pb : a.offset < b.offset
        }.map(\.element)
    }

    /// The most used live window of any account, for the Lock Screen gauge. Nil when none is live.
    public func mostUsedWindow(now: Date) -> WidgetWindow? {
        var best: (WidgetWindow, Double)?
        for account in accounts {
            let card = account.card
            for meter in card.allMeters where meter.kind == .window && SubscriptionCard.isLive(meter, now: now) {
                let pct = meter.usedPct ?? 0
                if best == nil || pct > best!.1 { best = (WidgetWindow(card: card, meter: meter), pct) }
            }
        }
        return best?.0
    }

    /// How old the subscription readings are; nil until the app has loaded them.
    public func accountsFreshness(now: Date) -> WidgetFreshness? {
        guard let accountsAt else { return nil }
        return .make(fetchedAt: Date(timeIntervalSince1970: accountsAt), now: now)
    }

    // MARK: Timeline

    /// When the widget should draw again over the next reload interval: now, every ten minutes, and
    /// just after any live window resets (so "resets in 2 min" does not sit there after it has).
    public func timelineDates(now: Date) -> [Date] {
        let horizon = now.addingTimeInterval(Self.reloadInterval)
        var dates: [Date] = [now, now.addingTimeInterval(600), now.addingTimeInterval(1200)]
        for account in accounts {
            for meter in account.card.allMeters {
                guard let reset = meter.resetsAt else { continue }
                let at = Date(timeIntervalSince1970: TimeInterval(reset) + 30)
                if at > now, at < horizon { dates.append(at) }
            }
        }
        var unique: [Date] = []
        for date in dates.sorted() where unique.last.map({ date.timeIntervalSince($0) >= 60 }) ?? true {
            unique.append(date)
        }
        return Array(unique.prefix(6))
    }
}
