import GrouperAPI
import SwiftUI
import WidgetKit

/// The subscription-usage widget. Small is one account's windows as bars, medium two accounts (or one
/// account's windows when the owner picked it), large every account as a compact card with its
/// group headings, and the Lock Screen circle a gauge of the most used window. Warnings (80% and
/// over) are a word and an icon as well as the amber or red bar.
struct SubscriptionsWidgetView: View {
    @Environment(\.widgetFamily) private var family
    let entry: SubscriptionsEntry

    var body: some View {
        content
            .widgetURL(usageWidgetURL)
            .containerBackground(for: .widget) { UsageWidgetBackground(family: family) }
    }

    @ViewBuilder private var content: some View {
        if let snapshot = entry.snapshot, !snapshot.accounts.isEmpty {
            let now = entry.date
            let freshness = snapshot.accountsFreshness(now: now)
            switch family {
            case .accessoryCircular:
                LockScreenGauge(snapshot: snapshot, accountId: entry.accountId, now: now)
            case .systemLarge:
                LargeSubscriptions(accounts: snapshot.accountsByUse(now: now), now: now, freshness: freshness)
            case .systemMedium:
                MediumSubscriptions(snapshot: snapshot, chosenId: entry.accountId, now: now, freshness: freshness)
            default:
                SmallSubscriptions(snapshot: snapshot, chosenId: entry.accountId, now: now, freshness: freshness)
            }
        } else if family == .accessoryCircular {
            LockScreenGaugeEmpty()
        } else if let snapshot = entry.snapshot, snapshot.accountsAt != nil {
            // Loaded, and nothing is signed in anywhere.
            OpenGrouperState(detail: "No subscriptions found. Open Grouper to see why.")
        } else {
            OpenGrouperState(detail: "Open Grouper to load usage")
        }
    }
}

// MARK: - Shared bits

/// The card's mark and plan name.
private struct AccountHeader: View {
    let account: WidgetAccount
    let card: SubscriptionCard

    var body: some View {
        HStack(spacing: 5) {
            FamilyMark(family: card.family)
            Text(account.planName).font(.caption.weight(.semibold)).foregroundStyle(Theme.text).lineLimit(1)
        }
        .accessibilityElement(children: .combine)
    }
}

/// The line at the foot: stale first (a figure that may be wrong), then the worst warning, then how old
/// the reading is.
private struct SubscriptionsFooter: View {
    let cards: [SubscriptionCard]
    let freshness: WidgetFreshness?
    let now: Date
    /// Show the warning word even when the figures are fresh (small widgets have no room per row).
    var showWarning = false

    var body: some View {
        if let stale = staleness {
            FreshnessLine(freshness: stale)
        } else if showWarning, let meter = worstWarning {
            Label(meter.warningWord(now: now) ?? "", systemImage: MeterText.icon(meter, now: now) ?? "exclamationmark.triangle.fill")
                .font(.system(size: 10, weight: .semibold)).foregroundStyle(MeterText.tint(meter, now: now)).lineLimit(1)
        } else if let freshness {
            FreshnessLine(freshness: freshness)
        }
    }

    /// The app's own load, or a card whose reading is old or whose device has a problem.
    private var staleness: WidgetFreshness? {
        if let freshness, freshness.isStale { return freshness }
        if let card = cards.first(where: { $0.isStale(now: now) }) {
            return WidgetFreshness.staleReading(fetchedAt: Date(timeIntervalSince1970: TimeInterval(card.fetchedAt)), now: now)
        }
        return nil
    }

    private var worstWarning: SubscriptionMeter? {
        cards.flatMap(\.allMeters).filter { $0.warningWord(now: now) != nil }
            .max { ($0.usedPct ?? 0) < ($1.usedPct ?? 0) }
    }
}

// MARK: - Small

private struct SmallSubscriptions: View {
    let snapshot: WidgetSnapshot
    let chosenId: String?
    let now: Date
    let freshness: WidgetFreshness?

    var body: some View {
        if let account = snapshot.resolvedAccount(chosenId: chosenId, now: now) {
            let card = account.card
            let stale = card.isStale(now: now)
            let rows = card.rows(limit: 3)
            VStack(alignment: .leading, spacing: 7) {
                AccountHeader(account: account, card: card)
                if rows.isEmpty {
                    Text("No windows to show.").font(.caption).foregroundStyle(Theme.text2)
                }
                ForEach(rows) { row in
                    MeterCell(label: row.label, meter: row.meter, family: card.family, now: now, stale: stale, showDetail: false)
                }
                Spacer(minLength: 0)
                SubscriptionsFooter(cards: [card], freshness: freshness, now: now, showWarning: true)
            }
        }
    }
}

// MARK: - Medium

private struct MediumSubscriptions: View {
    let snapshot: WidgetSnapshot
    let chosenId: String?
    let now: Date
    let freshness: WidgetFreshness?

    var body: some View {
        if let chosen = snapshot.account(id: chosenId) {
            single(chosen)
        } else {
            let accounts = Array(snapshot.accountsByUse(now: now).prefix(2))
            if accounts.count == 1, let only = accounts.first {
                single(only)
            } else {
                VStack(alignment: .leading, spacing: 6) {
                    HStack(alignment: .top, spacing: 14) {
                        ForEach(accounts) { account in column(account) }
                    }
                    Spacer(minLength: 0)
                    SubscriptionsFooter(cards: accounts.map(\.card), freshness: freshness, now: now)
                }
            }
        }
    }

    /// One account, every window (up to four), in two columns.
    private func single(_ account: WidgetAccount) -> some View {
        let card = account.card
        let stale = card.isStale(now: now)
        let rows = card.rows(limit: 4)
        return VStack(alignment: .leading, spacing: 6) {
            AccountHeader(account: account, card: card)
            LazyVGrid(columns: [GridItem(.flexible(), spacing: 14), GridItem(.flexible())], alignment: .leading, spacing: 8) {
                ForEach(rows) { row in
                    MeterCell(label: row.label, meter: row.meter, family: card.family, now: now, stale: stale)
                }
            }
            Spacer(minLength: 0)
            SubscriptionsFooter(cards: [card], freshness: freshness, now: now)
        }
    }

    /// An account in a column: its header and its two most used windows.
    private func column(_ account: WidgetAccount) -> some View {
        let card = account.card
        let stale = card.isStale(now: now)
        return VStack(alignment: .leading, spacing: 7) {
            AccountHeader(account: account, card: card)
            ForEach(card.rows(limit: 2)) { row in
                MeterCell(label: row.label, meter: row.meter, family: card.family, now: now, stale: stale)
            }
        }
        .frame(maxWidth: .infinity, alignment: .topLeading)
    }
}

// MARK: - Large

private struct LargeSubscriptions: View {
    let accounts: [WidgetAccount]
    let now: Date
    let freshness: WidgetFreshness?

    /// What fits: cards in order of use until the height budget is spent, the rest counted.
    private var layout: (shown: [WidgetAccount], hidden: Int) {
        let budget: CGFloat = 255
        var used: CGFloat = 0
        var shown: [WidgetAccount] = []
        for account in accounts {
            let height = Self.height(of: account.card)
            if !shown.isEmpty, used + height > budget { break }
            used += height + 8
            shown.append(account)
        }
        return (shown, accounts.count - shown.count)
    }

    /// A card's height: its header and padding, then per group an optional heading and two windows
    /// per row.
    static func height(of card: SubscriptionCard) -> CGFloat {
        var height: CGFloat = 34
        if !card.notes.isEmpty { height += 14 }
        for group in card.groups where !group.meters.isEmpty {
            if group.title != nil { height += 14 }
            height += CGFloat((group.meters.count + 1) / 2) * 36
        }
        return height
    }

    var body: some View {
        let layout = self.layout
        VStack(alignment: .leading, spacing: 8) {
            ForEach(layout.shown) { account in AccountCard(account: account, now: now) }
            if layout.hidden > 0 {
                Text("+\(layout.hidden) more \(layout.hidden == 1 ? "account" : "accounts") in Grouper")
                    .font(.caption2).foregroundStyle(Theme.text3)
            }
            Spacer(minLength: 0)
            SubscriptionsFooter(cards: layout.shown.map(\.card), freshness: freshness, now: now)
        }
    }
}

/// One account as a compact card: header, any problem notes, then each group (Antigravity's get a
/// heading) with its windows two to a row.
private struct AccountCard: View {
    let account: WidgetAccount
    let now: Date

    var body: some View {
        let card = account.card
        let stale = card.isStale(now: now)
        VStack(alignment: .leading, spacing: 6) {
            AccountHeader(account: account, card: card)
            ForEach(card.notes, id: \.self) { note in
                Label(note, systemImage: "exclamationmark.triangle.fill")
                    .font(.system(size: 10)).foregroundStyle(Theme.warn).lineLimit(1).minimumScaleFactor(0.7)
            }
            ForEach(card.groups, id: \.id) { group in
                if !group.meters.isEmpty {
                    VStack(alignment: .leading, spacing: 4) {
                        if let title = group.title {
                            Text(title).font(.system(size: 10, weight: .semibold)).foregroundStyle(Theme.text2)
                                .accessibilityAddTraits(.isHeader)
                        }
                        LazyVGrid(columns: [GridItem(.flexible(), spacing: 12), GridItem(.flexible())], alignment: .leading, spacing: 6) {
                            ForEach(group.meters) { meter in
                                MeterCell(label: meter.label, meter: meter, family: card.family, now: now, stale: stale)
                            }
                        }
                    }
                }
            }
        }
        .padding(8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.surface2, in: RoundedRectangle(cornerRadius: Theme.Radius.sm))
    }
}

// MARK: - Lock Screen

/// A circular gauge of the most used live window, with the percentage in the middle and a warning
/// icon above it from 80% (and a different one at 100%).
private struct LockScreenGauge: View {
    let snapshot: WidgetSnapshot
    let accountId: String?
    let now: Date

    var body: some View {
        if let pick = window {
            let pct = min(100, max(0, pick.meter.usedPct ?? 0))
            Gauge(value: pct / 100) {
                Text("Plan")
            } currentValueLabel: {
                VStack(spacing: 0) {
                    if let icon = MeterText.icon(pick.meter, now: now) {
                        Image(systemName: icon).font(.system(size: 9))
                    }
                    Text("\(Int(pct.rounded()))").font(.system(size: 16, weight: .semibold)).monospacedDigit()
                }
            }
            .gaugeStyle(.accessoryCircularCapacity)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("\(pick.card.planName), \(pick.meter.spokenText(now: now))")
        } else {
            LockScreenGaugeEmpty()
        }
    }

    /// The most used live window: of the chosen account when one is picked, otherwise of all.
    private var window: WidgetWindow? {
        if let chosen = snapshot.account(id: accountId) {
            let single = WidgetSnapshot(accounts: [chosen])
            return single.mostUsedWindow(now: now)
        }
        return snapshot.mostUsedWindow(now: now)
    }
}

struct LockScreenGaugeEmpty: View {
    var body: some View {
        ZStack {
            AccessoryWidgetBackground()
            Image(systemName: "chart.pie").font(.title3)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Grouper usage. Open Grouper to load usage.")
    }
}
