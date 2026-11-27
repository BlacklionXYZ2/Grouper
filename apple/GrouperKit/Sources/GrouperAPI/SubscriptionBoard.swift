import Foundation

// The Usage tab's Subscriptions section (docs/usage-tab-spec.md section 5 and the owner
// decisions at its top): turns the hub's probe snapshots, the devices and the harness
// providers into cards with meters, and slim notes for the states without numbers. Pure
// Foundation, so it is tested on every platform; the SwiftUI side only draws the result.

/// The four subscription providers a device can read, as the probes name them.
public enum SubscriptionProvider: String, CaseIterable, Sendable {
    case claude, codex, grok, antigravity

    public init?(probeName: String) { self.init(rawValue: probeName.lowercased()) }

    /// From a harness name (`claude-code`, `codex`, `grok`, `antigravity`).
    public init?(harness: String) {
        let name = harness.lowercased()
        if name.hasPrefix("claude") { self = .claude }
        else if name.hasPrefix("codex") { self = .codex }
        else if name.hasPrefix("grok") { self = .grok }
        else if name.hasPrefix("antigravity") { self = .antigravity }
        else { return nil }
    }

    public var displayName: String {
        switch self {
        case .claude: return "Claude"
        case .codex: return "ChatGPT"
        case .grok: return "Grok"
        case .antigravity: return "Antigravity"
        }
    }

    /// The app to open on the device ("Open Codex there to renew it").
    public var appName: String {
        switch self {
        case .claude: return "Claude Code"
        case .codex: return "Codex"
        case .grok: return "Grok"
        case .antigravity: return "Antigravity"
        }
    }

    /// The family whose mark and colour the card wears (Cl, GP, Gk, Ge).
    public var family: ModelFamily {
        switch self {
        case .claude: return .claude
        case .codex: return .gpt
        case .grok: return .grok
        case .antigravity: return .gemini
        }
    }
}

// MARK: - Meters

/// One bar: a window of a plan, an amount of extra spend, or a credit balance.
public struct SubscriptionMeter: Identifiable, Equatable, Sendable {
    public enum Kind: Equatable, Sendable { case window, spend, balance, other }

    public enum State: Equatable, Sendable {
        case normal
        /// 80% and over: the word "Nearly used" and a warning icon, not just a tone.
        case nearlyUsed
        case usedUp
        /// No active window: 0% used and no reset time (spec 5.4).
        case notInUse
        /// The reset time has passed since the reading, so the bar is stale.
        case resetPassed
    }

    public let id: String
    public let kind: Kind
    /// The full label: "5-hour", "Weekly · Fable", "Extra usage (paid)".
    public let label: String
    /// The label without its scope, for a meter under its group's heading.
    public let spanLabel: String
    public let usedPct: Double?
    /// "$4.10 of $20.00" for spend, the amount for credits.
    public let amountText: String?
    /// When the window resets; nil when unknown (a reset before the year 2000 is the provider's "none").
    public let resetsAt: UInt64?
    public let spanSeconds: Int64
    public let scope: String?
    public let scopeLabel: String?

    init(snapshot s: UsageSnapshot) {
        let parts = s.window.split(separator: ":", maxSplits: 1, omittingEmptySubsequences: false).map(String.init)
        let span = parts.first ?? ""
        let scope = parts.count > 1 && !parts[1].isEmpty ? parts[1] : nil
        let parsed = SubscriptionMeter.parseSpan(span)
        let scopeLabel = SubscriptionMeter.nonEmpty(s.label)
        let k: Kind
        switch s.kind {
        case "window": k = .window
        case "spend": k = .spend
        case "balance": k = .balance
        default: k = .other
        }
        let short: String
        if k == .spend && s.window == "extra" { short = "Extra usage (paid)" }
        else if k == .balance && s.window == "credits" { short = "Credits" }
        else if let parsed { short = parsed.label }
        else if k == .spend { short = "Spend, \(s.window)" }
        else if k == .balance { short = "Balance" }
        else { short = SubscriptionMeter.capitalized(s.window.isEmpty ? s.kind : s.window) }
        var full = short
        if parsed != nil, let scope {
            full += " \u{00B7} " + (scopeLabel ?? SubscriptionMeter.scopeTitle(scope))
        }
        kind = k
        id = "\(s.window)|\(s.kind)|\(s.currency ?? "")"
        label = full
        spanLabel = short
        usedPct = s.usedPct
        resetsAt = s.resetsAt.flatMap { $0 >= 946_684_800 ? $0 : nil }
        spanSeconds = parsed?.seconds ?? Int64.max
        self.scope = scope
        self.scopeLabel = scopeLabel
        switch k {
        case .spend:
            if let used = s.usedAmount {
                if let limit = s.limitAmount { amountText = "\(UsageFormat.amount(used, currency: s.currency)) of \(UsageFormat.amount(limit, currency: s.currency))" }
                else { amountText = UsageFormat.amount(used, currency: s.currency) }
            } else { amountText = nil }
        case .balance:
            // A balance's used amount is what is left (the web page reads it the same way).
            if let used = s.usedAmount {
                if let limit = s.limitAmount, limit > 0 {
                    amountText = "\(UsageFormat.amount(max(0, limit - used), currency: s.currency)) left of \(UsageFormat.amount(limit, currency: s.currency))"
                } else { amountText = UsageFormat.amount(used, currency: s.currency) }
            } else { amountText = nil }
        default:
            amountText = nil
        }
    }

    /// "5h" is 5-hour, "7d" Weekly, "30d" Monthly, anything else "N-minute/hour/day".
    static func parseSpan(_ span: String) -> (label: String, seconds: Int64)? {
        guard let unit = span.last, "mhd".contains(unit), let n = Int64(span.dropLast()), n > 0 else { return nil }
        let unitSeconds: Int64 = unit == "m" ? 60 : unit == "h" ? 3600 : 86400
        let seconds = n * unitSeconds
        if unit == "d" && n == 7 { return ("Weekly", seconds) }
        if unit == "d" && n == 30 { return ("Monthly", seconds) }
        let word = unit == "m" ? "minute" : unit == "h" ? "hour" : "day"
        return ("\(n)-\(word)", seconds)
    }

    static func scopeTitle(_ scope: String) -> String {
        capitalized(scope.replacingOccurrences(of: "_", with: " "))
    }

    static func capitalized(_ text: String) -> String {
        String(text.prefix(1)).uppercased() + String(text.dropFirst())
    }

    static func nonEmpty(_ text: String?) -> String? {
        guard let trimmed = text?.trimmingCharacters(in: .whitespacesAndNewlines), !trimmed.isEmpty else { return nil }
        return trimmed
    }

    /// Windows, then spend, then balance.
    var rank: Int {
        switch kind { case .window, .other: return 0; case .spend: return 1; case .balance: return 2 }
    }

    /// Shortest span first, plain windows before scoped ones, then spend and balance (spec 5.2).
    static func inOrder(_ a: SubscriptionMeter, _ b: SubscriptionMeter) -> Bool {
        if a.rank != b.rank { return a.rank < b.rank }
        if a.spanSeconds != b.spanSeconds { return a.spanSeconds < b.spanSeconds }
        if (a.scope == nil) != (b.scope == nil) { return a.scope == nil }
        return a.label < b.label
    }

    public func state(now: Date) -> State {
        let nowSeconds = UInt64(max(0, now.timeIntervalSince1970))
        let pct = usedPct ?? 0
        if kind == .window || kind == .other {
            if resetsAt == nil && pct <= 0 { return .notInUse }
            if let resetsAt, resetsAt <= nowSeconds { return .resetPassed }
        }
        guard usedPct != nil else { return .normal }
        if pct >= 100 { return .usedUp }
        if pct >= 80 { return .nearlyUsed }
        return .normal
    }

    /// The word that goes with a warning icon, never the tone alone.
    public func warningWord(now: Date) -> String? {
        switch state(now: now) {
        case .nearlyUsed: return "Nearly used"
        case .usedUp: return "Used up"
        default: return nil
        }
    }

    /// The figure on the label's line: "62% used", the amount, or "Not in use".
    public func valueText(now: Date) -> String {
        switch state(now: now) {
        case .notInUse: return "Not in use"
        case .resetPassed: return "Reset"
        default: break
        }
        if let amountText { return amountText }
        if let usedPct { return "\(UsageFormat.usedPercent(usedPct)) used" }
        return ""
    }

    /// The line under the bar: when it resets, or what to wait for.
    public func detailText(now: Date, locale: Locale = .current, timeZone: TimeZone = .current) -> String? {
        switch state(now: now) {
        case .notInUse: return "Not in use. Starts when you next use it."
        case .resetPassed:
            guard let resetsAt else { return nil }
            return "Reset at \(UsageFormat.clock(resetsAt, locale: locale, timeZone: timeZone)). Waiting for a new reading."
        default:
            guard let resetsAt else { return nil }
            return UsageFormat.resetText(resetsAt: resetsAt, now: now, locale: locale, timeZone: timeZone)
        }
    }

    /// One VoiceOver sentence: "Weekly, 62 percent used, resets in 2 hours 14 minutes".
    public func spokenText(now: Date, locale: Locale = .current, timeZone: TimeZone = .current) -> String {
        var parts = [label]
        switch state(now: now) {
        case .notInUse:
            parts.append("not in use, starts when you next use it")
        case .resetPassed:
            if let resetsAt { parts.append("reset at \(UsageFormat.clock(resetsAt, locale: locale, timeZone: timeZone)), waiting for a new reading") }
        default:
            if let amountText { parts.append(amountText) }
            if let usedPct { parts.append("\(UsageFormat.usedPercent(usedPct).dropLast()) percent used") }
            if let word = warningWord(now: now) { parts.append(word.lowercased()) }
            if let resetsAt { parts.append(UsageFormat.resetSpeech(resetsAt: resetsAt, now: now, locale: locale, timeZone: timeZone)) }
        }
        return parts.joined(separator: ", ")
    }
}

/// Meters under one heading (Antigravity's "Gemini models"), or the card's plain meters (no title).
public struct SubscriptionGroup: Identifiable, Equatable, Sendable {
    public let id: String
    public let title: String?
    public let meters: [SubscriptionMeter]
}

// MARK: - Cards and notes

/// One subscription account: a provider and its plan, merged across the devices that
/// report the same account (spec decision 4).
public struct SubscriptionCard: Identifiable, Equatable, Sendable {
    public let id: String
    public let provider: String
    public let kind: SubscriptionProvider?
    /// "Claude Max", "ChatGPT Plus", "SuperGrok", "Antigravity".
    public let planName: String
    /// Every device that reports this account, newest reading first.
    public let deviceNames: [String]
    public let deviceIds: [String]
    /// When the newest reading was taken (Unix seconds).
    public let fetchedAt: UInt64
    public let groups: [SubscriptionGroup]
    /// Plain-words problems on a device behind this card: sign-in expired, reading off.
    public let notes: [String]
    /// The newest reading's device has a problem, so the values are shown as stale.
    public let degraded: Bool
    /// The highest used percent on the card, for ordering.
    public let maxUsedPct: Double

    public var family: ModelFamily { kind?.family ?? .unknown }

    public func isStale(now: Date) -> Bool {
        degraded || now.timeIntervalSince1970 - TimeInterval(fetchedAt) > SubscriptionBoard.staleAfter
    }

    /// "Signed in on Gaming PC and MacBook".
    public var signedInLine: String { "Signed in on " + SubscriptionBoard.names(deviceNames) }

    public var hasMeters: Bool { groups.contains { !$0.meters.isEmpty } }
}

/// A slim card: a state with no numbers (spec 5.4).
public struct SubscriptionNote: Identifiable, Equatable, Sendable {
    public enum Kind: Equatable, Sendable { case notSignedIn, expired, unavailable, readingOff }
    public let id: String
    public let kind: Kind
    public let text: String
    /// How to fix it, when there is a way.
    public let hint: String?
}

public struct SubscriptionBoardResult: Equatable, Sendable {
    public let cards: [SubscriptionCard]
    public let notes: [SubscriptionNote]
    public var isEmpty: Bool { cards.isEmpty && notes.isEmpty }
}

public enum SubscriptionBoard {
    /// A reading older than this is stale (spec 5.4).
    public static let staleAfter: TimeInterval = 3600

    /// "A", "A and B", "A, B and C".
    public static func names(_ list: [String]) -> String {
        switch list.count {
        case 0: return "a device"
        case 1: return list[0]
        default: return list.dropLast().joined(separator: ", ") + " and " + list[list.count - 1]
        }
    }

    private struct Reading {
        let provider: String
        let deviceId: String
        var windows: [UsageSnapshot]
        var statuses: [UsageSnapshot]
        var fetchedAt: UInt64 { windows.map(\.fetchedAt).max() ?? 0 }
        var account: String { windows.first?.account ?? "" }
        var accountKey: String? { windows.compactMap { SubscriptionMeter.nonEmpty($0.accountKey) }.first }
    }

    /// Builds the cards and slim notes from the hub's snapshots (device probes only), the device
    /// list (names, and whether each device reads usage) and the harness providers (what is signed
    /// in where). Cards merge by provider and account key: one card naming every device when the
    /// keys match, separate cards when they differ, one per device when a probe sent no key.
    public static func build(snapshots: [UsageSnapshot], devices: [DeviceInfo], providers: [ProviderInfo] = [],
                             now: Date = Date()) -> SubscriptionBoardResult {
        func deviceName(_ id: String) -> String {
            if let device = devices.first(where: { $0.id == id }) { return device.name }
            return id.isEmpty ? "a device" : id
        }

        // 1. One reading per provider and device: the newest probe's rows, plus any status rows.
        var windows: [String: [UsageSnapshot]] = [:]
        var statuses: [String: [UsageSnapshot]] = [:]
        var order: [String] = []
        for s in snapshots where s.source == "probe" {
            let provider = s.provider.lowercased()
            let key = provider + "\u{1F}" + (s.deviceId ?? "")
            if windows[key] == nil && statuses[key] == nil { order.append(key) }
            if s.kind == "status" { statuses[key, default: []].append(s) }
            else { windows[key, default: []].append(s) }
        }
        var readings: [Reading] = []
        for key in order {
            let pieces = key.components(separatedBy: "\u{1F}")
            let rows = windows[key] ?? []
            let newest = rows.map(\.fetchedAt).max()
            readings.append(Reading(provider: pieces[0], deviceId: pieces.count > 1 ? pieces[1] : "",
                                    windows: rows.filter { $0.fetchedAt == newest }, statuses: statuses[key] ?? []))
        }

        // 2. Merge readings of the same account into one card.
        var mergeOrder: [String] = []
        var merged: [String: [Reading]] = [:]
        for reading in readings where !reading.windows.isEmpty {
            let id = reading.accountKey.map { "\(reading.provider)|\($0)" } ?? "\(reading.provider)|device:\(reading.deviceId)"
            if merged[id] == nil { mergeOrder.append(id) }
            merged[id, default: []].append(reading)
        }

        func readingOff(deviceId: String, provider: String) -> Bool {
            guard let device = devices.first(where: { $0.id == deviceId }), let on = device.usageProbes else { return false }
            if !on { return true }
            let advertised = device.usageProbeProviders.map { $0.lowercased() }
            return !advertised.isEmpty && !advertised.contains(provider)
        }

        var cards: [SubscriptionCard] = []
        for id in mergeOrder {
            let members = (merged[id] ?? []).sorted { $0.fetchedAt > $1.fetchedAt }
            guard let newest = members.first else { continue }
            let kind = SubscriptionProvider(probeName: newest.provider)
            var cardNotes: [String] = []
            var degraded = false
            for member in members {
                let name = deviceName(member.deviceId)
                for status in member.statuses {
                    let state = statusState(status.window)
                    if member.deviceId == newest.deviceId { degraded = true }
                    switch state {
                    case .expired: cardNotes.append("Sign-in expired on \(name). Open \(kind?.appName ?? "the app") there to renew it.")
                    case .notSignedIn: cardNotes.append("Signed out on \(name).")
                    case .unavailable: cardNotes.append("\(name): \(sentence(statusMessage(status.window)))")
                    }
                }
                if readingOff(deviceId: member.deviceId, provider: member.provider) {
                    cardNotes.append("Reading is off on \(name).")
                    if member.deviceId == newest.deviceId { degraded = true }
                }
            }
            let meters = newest.windows.map { SubscriptionMeter(snapshot: $0) }
            let ids = members.map(\.deviceId)
            var seenDevice = Set<String>()
            let uniqueIds = ids.filter { seenDevice.insert($0).inserted }
            cards.append(SubscriptionCard(
                id: id, provider: newest.provider, kind: kind,
                planName: planName(provider: newest.provider, account: newest.account),
                deviceNames: uniqueIds.map(deviceName), deviceIds: uniqueIds,
                fetchedAt: newest.fetchedAt, groups: groups(of: meters), notes: cardNotes, degraded: degraded,
                maxUsedPct: meters.filter { $0.kind == .window }.compactMap(\.usedPct).max() ?? 0))
        }
        cards.sort { a, b in
            a.maxUsedPct != b.maxUsedPct ? a.maxUsedPct > b.maxUsedPct : a.planName < b.planName
        }

        // 3. Slim notes: a status with no numbers behind it, and devices that have a harness
        // signed in but are not reading (spec 5.4, as changed by the owner decisions).
        var notes: [SubscriptionNote] = []
        var seenReading = Set<String>()
        for reading in readings {
            seenReading.insert("\(reading.provider)|\(reading.deviceId)")
            guard reading.windows.isEmpty, let status = reading.statuses.first else { continue }
            let kind = SubscriptionProvider(probeName: reading.provider)
            let who = "\(kind?.displayName ?? reading.provider.capitalized) on \(deviceName(reading.deviceId))"
            let id = "status|\(reading.provider)|\(reading.deviceId)"
            switch statusState(status.window) {
            case .notSignedIn:
                notes.append(SubscriptionNote(id: id, kind: .notSignedIn, text: "\(who): not signed in.", hint: nil))
            case .expired:
                notes.append(SubscriptionNote(id: id, kind: .expired,
                                              text: "\(who): sign-in expired. Open \(kind?.appName ?? "the app") there to renew it.", hint: nil))
            default:
                notes.append(SubscriptionNote(id: id, kind: .unavailable, text: "\(who): \(sentence(statusMessage(status.window)))", hint: nil))
            }
        }
        for provider in providers where provider.kind == "harness" {
            guard case .object(let options)? = provider.options, options["signed_in"] == JSONValue.bool(true) else { continue }
            var harness = provider.id.components(separatedBy: "@").first ?? provider.id
            if case .string(let name)? = options["harness"] { harness = name }
            guard let kind = SubscriptionProvider(harness: harness) else { continue }
            var via = provider.viaDevice ?? ""
            if case .string(let name)? = options["via_device"] { via = name }
            if via.isEmpty, let at = provider.id.firstIndex(of: "@") { via = String(provider.id[provider.id.index(after: at)...]) }
            guard let device = devices.first(where: { $0.id == via || $0.name.caseInsensitiveCompare(via) == .orderedSame }) else { continue }
            guard readingOff(deviceId: device.id, provider: kind.rawValue),
                  !seenReading.contains("\(kind.rawValue)|\(device.id)") else { continue }
            let id = "off|\(kind.rawValue)|\(device.id)"
            guard !notes.contains(where: { $0.id == id }) else { continue }
            notes.append(SubscriptionNote(id: id, kind: .readingOff,
                                          text: "\(kind.displayName) on \(device.name). Reading is off on that device.",
                                          hint: "Open Grouper on \(device.name) \u{203A} Settings \u{203A} This device to turn it on."))
        }
        notes.sort { $0.text < $1.text }
        return SubscriptionBoardResult(cards: cards, notes: notes)
    }

    // MARK: - Pieces

    /// Meters grouped under a heading when two or more rows share a labelled scope (Antigravity's
    /// groups); everything else is one plain list, so a scope with a label but a single row
    /// (Grok's product) stays inline as "Weekly · Grok Build".
    static func groups(of meters: [SubscriptionMeter]) -> [SubscriptionGroup] {
        var counts: [String: Int] = [:]
        for meter in meters where meter.kind == .window {
            if let scope = meter.scope, meter.scopeLabel != nil { counts[scope, default: 0] += 1 }
        }
        let grouped = Set(counts.filter { $0.value >= 2 }.keys)
        let plain = meters.filter { m in !(m.kind == .window && m.scope.map { grouped.contains($0) } == true) }.sorted(by: SubscriptionMeter.inOrder)
        var result: [SubscriptionGroup] = []
        if !plain.isEmpty { result.append(SubscriptionGroup(id: "plain", title: nil, meters: plain)) }
        for scope in grouped.sorted() {
            let inGroup = meters.filter { $0.kind == .window && $0.scope == scope }.sorted(by: SubscriptionMeter.inOrder)
            let title = inGroup.compactMap(\.scopeLabel).first ?? SubscriptionMeter.scopeTitle(scope)
            // Under its heading a meter shows only its span ("5-hour").
            let trimmed = inGroup.map { m in
                SubscriptionMeter.relabelled(m, as: m.spanLabel)
            }
            result.append(SubscriptionGroup(id: "scope:\(scope)", title: title, meters: trimmed))
        }
        return result
    }

    enum StatusState { case notSignedIn, expired, unavailable }

    /// A device's non-ok probe result is stored as a `status` row whose `window` is the message.
    static func statusState(_ text: String) -> StatusState {
        let lower = text.lowercased()
        if lower.contains("not signed in") || lower.contains("not_signed_in") { return .notSignedIn }
        if lower.contains("expired") { return .expired }
        return .unavailable
    }

    static func statusMessage(_ text: String) -> String {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty || trimmed.lowercased() == "unavailable" { return "usage is unavailable right now" }
        return trimmed
    }

    static func sentence(_ text: String) -> String {
        text.hasSuffix(".") ? text : text + "."
    }

    /// "Claude Max", "ChatGPT Plus", "SuperGrok", "Antigravity" from the provider and the plan label.
    static func planName(provider: String, account: String) -> String {
        let kind = SubscriptionProvider(probeName: provider)
        var brand = kind?.displayName ?? SubscriptionMeter.capitalized(provider)
        let lower = account.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        var words = lower.split(whereSeparator: { $0 == "-" || $0 == "_" || $0 == " " }).map(String.init)
        let own: [String]
        switch kind {
        case .claude?: own = ["claude"]
        case .codex?: own = ["chatgpt", "codex"]
        case .grok?:
            own = ["grok"]
            if words.first == "supergrok" { brand = "SuperGrok"; words.removeFirst() }
        case .antigravity?: own = ["antigravity"]
        case nil: own = [provider.lowercased()]
        }
        if let first = words.first, own.contains(first) { words.removeFirst() }
        let rest = words.map(SubscriptionMeter.capitalized).joined(separator: " ")
        return rest.isEmpty ? brand : "\(brand) \(rest)"
    }
}

extension SubscriptionMeter {
    /// The same meter under a group heading, with its label shortened to the span.
    static func relabelled(_ meter: SubscriptionMeter, as label: String) -> SubscriptionMeter {
        SubscriptionMeter(copying: meter, label: label)
    }

    init(copying m: SubscriptionMeter, label: String) {
        id = m.id; kind = m.kind; self.label = label; spanLabel = m.spanLabel; usedPct = m.usedPct
        amountText = m.amountText; resetsAt = m.resetsAt; spanSeconds = m.spanSeconds
        scope = m.scope; scopeLabel = m.scopeLabel
    }
}
