import GrouperAPI
import SwiftUI
import WidgetKit

/// The API-costs widget: small is the total and a small donut, medium the donut and the top keys,
/// large the donut with up to eight keys and their tokens, and the Lock Screen rectangle today's cost.
struct CostsWidgetView: View {
    @Environment(\.widgetFamily) private var family
    let entry: CostsEntry

    var body: some View {
        content
            .widgetURL(usageWidgetURL)
            .containerBackground(for: .widget) { UsageWidgetBackground(family: family) }
    }

    @ViewBuilder private var content: some View {
        if let costs = entry.snapshot?.costs(for: entry.period, now: entry.date) {
            switch family {
            case .accessoryRectangular: LockScreenCosts(costs: costs)
            case .systemMedium: MediumCosts(costs: costs)
            case .systemLarge: LargeCosts(costs: costs)
            default: SmallCosts(costs: costs)
            }
        } else if family == .accessoryRectangular {
            LockScreenEmpty(title: "API cost", detail: "Open Grouper to load usage")
        } else {
            OpenGrouperState(detail: entry.snapshot == nil ? "Open Grouper to load usage"
                                                          : "Open Grouper to load \(entry.period.title.lowercased()) costs")
        }
    }
}

// MARK: - Figures

private extension WidgetCosts {
    /// The headline: dollars, or tokens when nothing cost money but tokens were used.
    var headline: String {
        donut.basis == .tokens ? UsageFormat.tokens(UInt64(max(0, donut.total.rounded()))) : UsageFormat.money(summary.total.cost)
    }

    /// What the headline counts, under it.
    var headlineCaption: String {
        switch donut.basis {
        case .tokens: return "tokens, " + period.caption.lowercased()
        case .cost, .empty: return period.caption
        }
    }

    var headlineSpeech: String {
        UsageDonut.accessibilitySummary(donut, period: period, totals: summary.total)
    }
}

private struct CostsHeader: View {
    let costs: WidgetCosts

    var body: some View {
        HStack(spacing: 4) {
            Image(systemName: "chart.pie.fill").font(.caption2).foregroundStyle(Theme.accent).accessibilityHidden(true)
            Text("API costs").font(.caption.weight(.semibold)).foregroundStyle(Theme.text2).lineLimit(1)
        }
        .accessibilityElement(children: .combine)
    }
}

private struct CostsHeadline: View {
    let costs: WidgetCosts
    var font: Font = .title2

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(costs.headline).font(font.weight(.semibold)).monospacedDigit().foregroundStyle(Theme.text)
                .lineLimit(1).minimumScaleFactor(0.6)
            Text(costs.headlineCaption).font(.caption2).foregroundStyle(Theme.text3).lineLimit(1)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(costs.headlineSpeech)
    }
}

// MARK: - Small

private struct SmallCosts: View {
    let costs: WidgetCosts

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            CostsHeader(costs: costs)
            Spacer(minLength: 0)
            HStack(alignment: .center, spacing: 8) {
                CostsHeadline(costs: costs, font: .title3)
                Spacer(minLength: 0)
                CostDonut(costs: costs, innerRatio: 0.6, emptyRing: 6)
                    .frame(width: 54, height: 54)
            }
            Spacer(minLength: 0)
            FreshnessLine(freshness: costs.freshness)
        }
    }
}

// MARK: - Medium

private struct MediumCosts: View {
    let costs: WidgetCosts

    var body: some View {
        let rows = CostRow.rows(costs, limit: 4)
        HStack(alignment: .center, spacing: 14) {
            ZStack {
                CostDonut(costs: costs)
                Text(costs.headline).font(.footnote.weight(.semibold)).monospacedDigit().foregroundStyle(Theme.text)
                    .lineLimit(1).minimumScaleFactor(0.5).frame(maxWidth: 62)
                    .accessibilityHidden(true)
            }
            .frame(width: 96, height: 96)
            VStack(alignment: .leading, spacing: 4) {
                HStack {
                    CostsHeader(costs: costs)
                    Spacer(minLength: 4)
                    Text(costs.period.caption).font(.caption2).foregroundStyle(Theme.text3).lineLimit(1)
                }
                if rows.isEmpty {
                    Text("No calls through Grouper in this period.").font(.caption).foregroundStyle(Theme.text2)
                } else {
                    ForEach(rows) { row in KeyLine(row: row, basis: costs.donut.basis, showTokens: false) }
                }
                Spacer(minLength: 0)
                FreshnessLine(freshness: costs.freshness)
            }
        }
    }
}

// MARK: - Large

private struct LargeCosts: View {
    let costs: WidgetCosts

    var body: some View {
        let rows = CostRow.rows(costs, limit: 8)
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 14) {
                CostDonut(costs: costs).frame(width: 104, height: 104)
                VStack(alignment: .leading, spacing: 6) {
                    CostsHeader(costs: costs)
                    CostsHeadline(costs: costs, font: .title)
                    Text(UsageFormat.tokenLine(costs.summary.total)).font(.caption2).foregroundStyle(Theme.text3)
                        .lineLimit(1).minimumScaleFactor(0.7)
                }
                Spacer(minLength: 0)
            }
            if rows.isEmpty {
                Text("No calls through Grouper in this period.").font(.footnote).foregroundStyle(Theme.text2)
            } else {
                VStack(spacing: 5) {
                    ForEach(rows) { row in KeyLine(row: row, basis: costs.donut.basis, showTokens: true) }
                }
            }
            Spacer(minLength: 0)
            FreshnessLine(freshness: costs.freshness, showNote: costs.freshness.isStale)
        }
    }
}

/// A swatch, the key's name, optionally its tokens in and out, and its cost. The swatch is a
/// supplement: the name is always there as text.
private struct KeyLine: View {
    let row: CostRow
    let basis: DonutModel.Basis
    let showTokens: Bool

    var body: some View {
        HStack(spacing: 6) {
            RoundedRectangle(cornerRadius: 2).fill(row.color).frame(width: 9, height: 9).accessibilityHidden(true)
            Text(row.title).font(.caption.weight(.medium)).foregroundStyle(Theme.text).lineLimit(1)
            Spacer(minLength: 4)
            if showTokens {
                Text("\(UsageFormat.tokens(row.tokensIn)) in \u{00B7} \(UsageFormat.tokens(row.tokensOut)) out")
                    .font(.caption2).foregroundStyle(Theme.text3).lineLimit(1).minimumScaleFactor(0.7)
            }
            Text(row.figureText(basis: basis)).font(.caption.weight(.semibold)).monospacedDigit()
                .foregroundStyle(Theme.text).lineLimit(1)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(speech)
    }

    private var speech: String {
        var text = "\(row.title), \(row.figureText(basis: basis))"
        if basis == .tokens { text += " tokens" }
        if showTokens {
            text += ", \(UsageFormat.exactTokens(row.tokensIn)) tokens in, \(UsageFormat.exactTokens(row.tokensOut)) out"
        }
        return text
    }
}

// MARK: - Lock Screen

private struct LockScreenCosts: View {
    let costs: WidgetCosts

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text("API cost \u{00B7} \(costs.period.caption)").font(.caption2.weight(.semibold)).lineLimit(1)
            Text(costs.headline).font(.title3.weight(.semibold)).monospacedDigit().lineLimit(1).minimumScaleFactor(0.6)
            if costs.freshness.isStale {
                Label("Stale", systemImage: "exclamationmark.triangle.fill").font(.caption2)
            } else {
                Text(UsageFormat.calls(costs.summary.total.calls)).font(.caption2).foregroundStyle(.secondary)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(costs.headlineSpeech + (costs.freshness.isStale ? " Stale. " + costs.freshness.updatedText + "." : ""))
    }
}

struct LockScreenEmpty: View {
    let title: String
    let detail: String

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(title).font(.caption2.weight(.semibold))
            Text(detail).font(.caption)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
    }
}
