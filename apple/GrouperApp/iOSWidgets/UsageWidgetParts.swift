import Charts
import GrouperAPI
import SwiftUI
import WidgetKit

// Small pieces the usage widgets share: the donut, a meter bar, the "open Grouper" empty state and
// the freshness line. Colours come from Theme (the same tokens as the app); state is always a word
// or an icon as well as a colour. The widgets read only the snapshot the app wrote
// (`WidgetSnapshotStore`); they never talk to the hub.

/// Where a tap on any usage widget goes (the app's `grouper://usage` link opens the Usage tab).
let usageWidgetURL = URL(string: "grouper://usage")

// MARK: - Donut

/// The API-costs donut: `SectorMark` segments in the key colours, largest first from 12 o'clock.
/// One spoken summary for the whole chart, as in the app.
struct CostDonut: View {
    let costs: WidgetCosts
    /// Inner radius as a share of the outer one.
    var innerRatio: Double = 0.62
    /// Stroke width of the empty ring, in points.
    var emptyRing: CGFloat = 8

    var body: some View {
        Group {
            if costs.donut.basis == .empty {
                Circle().stroke(Theme.border, lineWidth: emptyRing).padding(emptyRing / 2)
            } else {
                Chart(costs.donut.segments) { segment in
                    SectorMark(
                        angle: .value("Share", segment.weight),
                        innerRadius: .ratio(innerRatio),
                        angularInset: costs.donut.segments.count > 1 ? 1 : 0
                    )
                    .foregroundStyle(WidgetColors.color(for: segment))
                }
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(UsageDonut.accessibilitySummary(costs.donut, period: costs.period, totals: costs.summary.total))
    }
}

enum WidgetColors {
    static func color(for segment: DonutSegment) -> Color {
        segment.colorName.map { Theme.keyColor($0) } ?? Theme.keyUnknown
    }
}

// MARK: - Cost rows

/// One line of the key list under or beside the donut: a key, or the keys folded into "Other".
struct CostRow: Identifiable {
    let id: String
    let title: String
    let color: Color
    /// Dollars, or tokens when the donut is by token share.
    let figure: Double
    let tokensIn: UInt64
    let tokensOut: UInt64

    /// The donut's segments as rows. With more than `limit`, the smallest are folded into one grey
    /// "Other" row so the list still adds up to the total.
    static func rows(_ costs: WidgetCosts, limit: Int) -> [CostRow] {
        let keys = Dictionary(costs.summary.keys.map { ($0.provider, $0) }, uniquingKeysWith: { first, _ in first })
        func row(_ segment: DonutSegment) -> CostRow {
            let members = segment.providers.compactMap { keys[$0] }
            return CostRow(id: segment.id, title: segment.title, color: WidgetColors.color(for: segment), figure: segment.value,
                           tokensIn: members.reduce(0) { $0 &+ $1.totals.tokensIn },
                           tokensOut: members.reduce(0) { $0 &+ $1.totals.tokensOut })
        }
        let all = costs.donut.segments.map(row)
        guard limit > 0, all.count > limit else { return limit > 0 ? all : [] }
        let kept = Array(all.prefix(limit - 1))
        let rest = all.dropFirst(limit - 1)
        let folded = CostRow(id: "folded", title: "Other (\(rest.count))", color: Theme.keyUnknown,
                             figure: rest.reduce(0) { $0 + $1.figure },
                             tokensIn: rest.reduce(0) { $0 &+ $1.tokensIn },
                             tokensOut: rest.reduce(0) { $0 &+ $1.tokensOut })
        return kept + [folded]
    }

    func figureText(basis: DonutModel.Basis) -> String {
        basis == .tokens ? UsageFormat.tokens(UInt64(max(0, figure.rounded()))) : UsageFormat.money(figure)
    }
}

// MARK: - Meter bar

/// A 5pt bar: used share in the family colour, amber at 80% and red at 100% (with the icon and word
/// beside it elsewhere), a dashed outline when the window has reset since the reading.
struct MeterBar: View {
    let meter: SubscriptionMeter
    let family: ModelFamily
    let now: Date
    var stale = false

    var body: some View {
        let state = meter.state(now: now)
        let fraction = state == .notInUse || state == .resetPassed ? 0 : min(1, max(0, (meter.usedPct ?? 0) / 100))
        let fill: Color = state == .usedUp ? Theme.danger : (state == .nearlyUsed ? Theme.warn : family.color)
        GeometryReader { geo in
            ZStack(alignment: .leading) {
                Capsule().fill(Theme.surface3)
                if state == .resetPassed {
                    Capsule().strokeBorder(Theme.borderControl, style: StrokeStyle(lineWidth: 1, dash: [3, 2]))
                } else if fraction > 0 {
                    Capsule().fill(fill).frame(width: max(5, geo.size.width * fraction))
                }
            }
        }
        .frame(height: 5)
        .opacity(stale ? 0.6 : 1)
        .accessibilityHidden(true)
    }
}

/// How a meter's figure reads in a narrow cell: "62%", "$4.10 of $20.00", "Reset" or a dash.
enum MeterText {
    static func value(_ meter: SubscriptionMeter, now: Date) -> String {
        switch meter.state(now: now) {
        case .notInUse: return "\u{2013}"
        case .resetPassed: return "Reset"
        default: break
        }
        if meter.kind != .window, let amount = meter.amountText { return amount }
        if let pct = meter.usedPct { return UsageFormat.usedPercent(pct) }
        return ""
    }

    /// The line under a bar: "resets in 38 min", "Not in use", "Reset, waiting".
    static func detail(_ meter: SubscriptionMeter, now: Date) -> String? {
        switch meter.state(now: now) {
        case .notInUse: return "Not in use"
        case .resetPassed: return "Reset, waiting for a reading"
        default:
            guard let resetsAt = meter.resetsAt else { return nil }
            return UsageFormat.resetText(resetsAt: resetsAt, now: now)
        }
    }

    /// "in 38 min" / "Thu 3 Dec, 09:00": the reset time without its "resets" lead, for the line
    /// that already starts with a warning word.
    static func shortReset(_ meter: SubscriptionMeter, now: Date) -> String? {
        guard let text = detail(meter, now: now), text.hasPrefix("resets ") else { return nil }
        return String(text.dropFirst("resets ".count))
    }

    static func icon(_ meter: SubscriptionMeter, now: Date) -> String? {
        switch meter.state(now: now) {
        case .usedUp: return "xmark.octagon.fill"
        case .nearlyUsed: return "exclamationmark.triangle.fill"
        default: return nil
        }
    }

    static func tint(_ meter: SubscriptionMeter, now: Date) -> Color {
        meter.state(now: now) == .usedUp ? Theme.danger : Theme.warn
    }
}

/// One window in a cell: label and figure (with the warning icon), the bar, then the warning word
/// and the reset. One VoiceOver element with the full sentence.
struct MeterCell: View {
    let label: String
    let meter: SubscriptionMeter
    let family: ModelFamily
    let now: Date
    var stale = false
    /// Small widgets leave the line under the bar out.
    var showDetail = true

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(alignment: .firstTextBaseline, spacing: 4) {
                Text(label).font(.caption2.weight(.medium)).foregroundStyle(Theme.text).lineLimit(1)
                Spacer(minLength: 2)
                if let icon = MeterText.icon(meter, now: now) {
                    Image(systemName: icon).font(.system(size: 9)).foregroundStyle(MeterText.tint(meter, now: now))
                }
                Text(MeterText.value(meter, now: now)).font(.caption2.weight(.semibold)).monospacedDigit()
                    .foregroundStyle(Theme.text2).lineLimit(1).minimumScaleFactor(0.7)
            }
            MeterBar(meter: meter, family: family, now: now, stale: stale)
            if showDetail { detail }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(meter.spokenText(now: now))
    }

    @ViewBuilder private var detail: some View {
        if let word = meter.warningWord(now: now) {
            HStack(spacing: 3) {
                Text(word).font(.system(size: 10, weight: .semibold)).foregroundStyle(MeterText.tint(meter, now: now))
                if let reset = MeterText.shortReset(meter, now: now) {
                    Text("\u{00B7} " + reset).font(.system(size: 10)).foregroundStyle(Theme.text3)
                }
            }
            .lineLimit(1).minimumScaleFactor(0.7)
        } else if let text = MeterText.detail(meter, now: now) {
            Text(text).font(.system(size: 10)).foregroundStyle(Theme.text3).lineLimit(1).minimumScaleFactor(0.7)
        }
    }
}

// MARK: - Empty and freshness

/// No snapshot yet (or none readable): the widget cannot load anything itself, so it says so.
struct OpenGrouperState: View {
    var detail = "Open Grouper to load usage"

    var body: some View {
        VStack(spacing: 6) {
            Image(systemName: "chart.pie").font(.title2).foregroundStyle(Theme.accent)
            Text(detail).font(.footnote.weight(.medium)).foregroundStyle(Theme.text2).multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .accessibilityElement(children: .combine)
    }
}

/// "Updated 12 min ago", or "Stale · Updated 3h ago" with an icon in the warning colour.
struct FreshnessLine: View {
    let freshness: WidgetFreshness
    /// Replaces the clock with the note ("Open Grouper to refresh.") when there is room.
    var showNote = false

    var body: some View {
        if freshness.isStale {
            Label(showNote ? (freshness.note ?? freshness.updatedText) : "Stale \u{00B7} " + freshness.updatedText,
                  systemImage: "exclamationmark.triangle.fill")
                .font(.system(size: 10, weight: .medium)).foregroundStyle(Theme.warn).lineLimit(1).minimumScaleFactor(0.7)
        } else {
            Text(freshness.updatedText).font(.system(size: 10)).foregroundStyle(Theme.text3).lineLimit(1)
        }
    }
}

// MARK: - Backgrounds

/// The widget's own surface: the app's canvas on the Home Screen, clear on the Lock Screen (the
/// system draws its own material there).
struct UsageWidgetBackground: View {
    let family: WidgetFamily

    var body: some View {
        switch family {
        case .accessoryCircular, .accessoryRectangular, .accessoryInline: Color.clear
        default: Theme.bg
        }
    }
}
