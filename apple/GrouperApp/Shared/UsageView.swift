import Charts
import GrouperAPI
import SwiftUI

// The Usage tab (docs/usage-tab-spec.md): two summaries on one scrolling page. Section 1 is the
// API costs: a Day / Week / Month meter, the total, a donut by key and the key list as its legend.
// Section 2 is the Subscriptions: one card per signed-in plan with a meter per window. The two
// tracks are never added together. The data logic (ranges, donut segments, card merging, labels,
// states) is in GrouperAPI and tested there; this file only draws it.
//
// VERIFY ON MAC: written without a SwiftUI compiler (the Apple workflow is the first build).

struct UsageView: View {
    @EnvironmentObject private var model: AppModel
    @StateObject private var store = UsageStore()
    /// The Day / Week / Month choice, remembered per device.
    @AppStorage("grouper.usage.period") private var periodRaw = UsagePeriod.day.rawValue
    @State private var colorTarget: KeyColorTarget?

    private var period: UsagePeriod { UsagePeriod(rawValue: periodRaw) ?? .day }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    sections
                    UsageMoreList()
                }
                .padding(16)
                .frame(maxWidth: 1400)
                .frame(maxWidth: .infinity)
            }
            .background(Theme.bg)
            .navigationTitle("Usage")
            .refreshable { await refreshAll() }
            #if os(macOS)
            .toolbar {
                ToolbarItem(placement: .primaryAction) {
                    Button { Task { await refreshAll() } } label: { Label("Refresh", systemImage: "arrow.clockwise") }
                        .disabled(!model.online || store.readingNow)
                        .help("Read plan usage and provider balances now")
                }
            }
            .onExitCommand { store.clearFocus() }
            #endif
        }
        .task(id: periodRaw) {
            store.attach(hub: model.hubURL)
            await store.refreshSummary(client: model.client, period: period)
        }
        .task {
            store.attach(hub: model.hubURL)
            await model.refreshDevices()
            await model.loadProviders()
            store.setBoardContext(devices: model.devices, providers: model.providers)
            await store.refreshSnapshots(client: model.client, readIfOld: true)
            // Reset countdowns tick by themselves; the numbers are re-read once a minute while the page is open.
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(60))
                if Task.isCancelled { break }
                store.setBoardContext(devices: model.devices, providers: model.providers)
                await store.refreshSnapshots(client: model.client, readIfOld: false)
                await store.refreshSummary(client: model.client, period: period)
            }
        }
        .sheet(item: $colorTarget) { target in
            KeyColorSheet(target: target, usedBy: colorsInUse(excluding: target.provider)) { name in
                let failure = await store.setColor(name, provider: target.provider, client: model.client, period: period)
                if failure == nil { await model.loadProviders() }
                return failure
            }
        }
    }

    private func refreshAll() async {
        await model.refreshDevices()
        guard model.online else { return }
        store.setBoardContext(devices: model.devices, providers: model.providers)
        await store.manualRefresh(client: model.client, period: period)
        await model.loadProviders()
    }

    /// Which other keys use each colour, so the picker can name them.
    private func colorsInUse(excluding provider: String) -> [String: [String]] {
        var result: [String: [String]] = [:]
        for key in store.summaries[periodRaw]?.summary.keys ?? [] where key.provider != provider && !key.removed {
            if let color = key.color { result[color, default: []].append(key.provider) }
        }
        return result
    }

    // MARK: Layout

    /// Phone: stacked. Wide Mac window: two columns of equal width (spec 3).
    @ViewBuilder private var sections: some View {
        #if os(macOS)
        ViewThatFits(in: .horizontal) {
            HStack(alignment: .top, spacing: 16) {
                apiCostsCard.frame(maxWidth: .infinity, alignment: .top)
                subscriptionsCard.frame(maxWidth: .infinity, alignment: .top)
            }
            .frame(minWidth: 1050)
            VStack(alignment: .leading, spacing: 16) {
                apiCostsCard
                subscriptionsCard
            }
        }
        #else
        VStack(alignment: .leading, spacing: 16) {
            apiCostsCard
            subscriptionsCard
        }
        #endif
    }

    // MARK: Section 1: API costs

    private var apiCostsCard: some View {
        let timed = store.summaries[periodRaw]
        let fetched = timed.map { Date(timeIntervalSince1970: $0.fetchedAt) }
        return VStack(alignment: .leading, spacing: 14) {
            UsageSectionHeader(title: "API costs", subtitle: "Estimated from calls made through Grouper.")
            Picker("Period", selection: $periodRaw) {
                ForEach(UsagePeriod.allCases) { option in Text(option.title).tag(option.rawValue) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .accessibilityLabel("Period")
            UsageSectionStatus(state: store.summaryState, hasData: timed != nil, fetchedAt: fetched, offline: !model.online) {
                Task { await store.refreshSummary(client: model.client, period: period) }
            }
            if let timed {
                ApiCostsBody(summary: timed.summary, period: period, store: store,
                             snapshots: store.snapshots ?? [], limits: store.limits, canEdit: model.online) { key in
                    colorTarget = KeyColorTarget(provider: key.provider, current: key.color)
                }
            } else if !model.online && !store.summaryState.isFailed {
                Text("You're offline and there is no saved copy yet.").font(.callout).foregroundStyle(Theme.text2)
            } else if !store.summaryState.isFailed {
                UsageSkeleton(rows: 3, ring: true)
            }
        }
        .grouperCard(padding: 16)
    }

    // MARK: Section 2: Subscriptions

    private var subscriptionsCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            UsageSectionHeader(title: "Subscriptions", subtitle: "Read from the apps signed in on your devices. Unofficial; may lag.")
            if store.readingNow {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Reading plan usage…").font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
                }
                .accessibilityElement(children: .combine)
            }
            UsageSectionStatus(state: store.snapshotsState, hasData: store.snapshots != nil, fetchedAt: store.snapshotsAt, offline: !model.online) {
                Task { await store.refreshSnapshots(client: model.client, readIfOld: false) }
            }
            // Reset countdowns tick once a minute.
            TimelineView(.everyMinute) { context in
                subscriptionsBody(now: context.date)
            }
        }
        .grouperCard(padding: 16)
    }

    @ViewBuilder private func subscriptionsBody(now: Date) -> some View {
        if let snapshots = store.snapshots {
            let board = SubscriptionBoard.build(snapshots: snapshots, devices: model.devices, providers: model.providers, now: now)
            if board.isEmpty {
                Text("No subscriptions found. Sign in to Claude Code, Codex, Grok or Antigravity on one of your devices; Grouper reads their usage there unless reading is turned off on that device.")
                    .font(.callout).foregroundStyle(Theme.text2)
            } else {
                VStack(alignment: .leading, spacing: 12) {
                    ForEach(board.cards) { card in
                        SubscriptionCardView(card: card, now: now, throughGrouper: throughGrouperLine(card))
                    }
                    ForEach(board.notes) { note in SubscriptionNoteView(note: note) }
                }
            }
        } else if !store.snapshotsState.isFailed && model.online {
            UsageSkeleton(rows: 3, ring: false)
        }
    }

    /// "Through Grouper today: 212K tokens in, 18K out": context only, never a cost (spec 5.5).
    private func throughGrouperLine(_ card: SubscriptionCard) -> String? {
        guard let kind = card.kind, let summary = store.summaries[periodRaw]?.summary,
              let tokens = summary.harnessTokens(for: kind, deviceIds: Set(card.deviceIds)) else { return nil }
        let when = period == .day ? "today" : "in \(period.phrase)"
        return "Through Grouper \(when): \(UsageFormat.tokens(tokens.tokensIn)) tokens in, \(UsageFormat.tokens(tokens.tokensOut)) out"
    }
}

// MARK: - Shared small pieces

private struct UsageSectionHeader: View {
    let title: String
    let subtitle: String

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(title).font(Theme.Typeface.title).foregroundStyle(Theme.text)
                .accessibilityAddTraits(.isHeader)
            Text(subtitle).font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
        }
    }
}

/// A section's own error banner with "Try again", and the "Showing the copy from 12 min ago" line
/// when what is shown is a saved copy (offline, or the last load failed). The other section stays.
private struct UsageSectionStatus: View {
    let state: UsageStore.LoadState
    let hasData: Bool
    let fetchedAt: Date?
    let offline: Bool
    let retry: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if case .failed(let message) = state {
                HStack(alignment: .top, spacing: 8) {
                    Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(Theme.danger).accessibilityHidden(true)
                    Text("Couldn't load this section. \(message)").font(.callout).foregroundStyle(Theme.danger)
                    Spacer(minLength: 4)
                    Button("Try again", action: retry).font(.callout.weight(.semibold))
                        .grouperMinimumControlSize()
                }
                .grouperCard(padding: 10, fill: Theme.dangerSoft, stroke: Theme.dangerSoft)
            }
            if hasData, let fetchedAt, offline || state.isFailed {
                HStack(spacing: 0) {
                    Text("Showing the copy from ")
                    RelativeAgeText(seconds: UInt64(max(0, fetchedAt.timeIntervalSince1970)))
                }
                .font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
                .accessibilityElement(children: .combine)
            }
        }
    }
}

/// A skeleton ring and rows (or just rows) while a section loads.
private struct UsageSkeleton: View {
    let rows: Int
    let ring: Bool

    var body: some View {
        VStack(spacing: 12) {
            if ring {
                Circle().strokeBorder(Theme.surface3, lineWidth: 22)
                    .frame(width: 180, height: 180)
                    .frame(maxWidth: .infinity)
            }
            ForEach(0..<rows, id: \.self) { _ in
                RoundedRectangle(cornerRadius: Theme.Radius.sm).fill(Theme.surface3).frame(height: 44)
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Loading")
    }
}

private struct KeySwatch: View {
    let color: Color
    /// A key with no cost gets an outline ring, not a filled disc (spec 4.2).
    let filled: Bool

    var body: some View {
        Group {
            if filled { Circle().fill(color) } else { Circle().strokeBorder(color, lineWidth: 2) }
        }
        .frame(width: 12, height: 12)
        .accessibilityHidden(true)
    }
}

/// A 4pt share bar in the key's colour.
private struct ShareBar: View {
    let fraction: Double
    let color: Color

    var body: some View {
        GeometryReader { geo in
            ZStack(alignment: .leading) {
                Capsule().fill(Theme.surface3)
                if fraction > 0 {
                    Capsule().fill(color).frame(width: max(4, geo.size.width * min(1, fraction)))
                }
            }
        }
        .frame(height: 4)
        .accessibilityHidden(true)
    }
}

// MARK: - API costs body

private struct ApiCostsBody: View {
    let summary: UsageSummary
    let period: UsagePeriod
    @ObservedObject var store: UsageStore
    let snapshots: [UsageSnapshot]
    let limits: LimitsReport?
    let canEdit: Bool
    let onChangeColor: (UsageKey) -> Void

    private var donut: DonutModel { UsageDonut.build(keys: summary.keys) }

    var body: some View {
        let model = donut
        VStack(alignment: .leading, spacing: 14) {
            totalBlock
            if summary.unpricedCalls > 0 {
                Label("\(summary.unpricedCalls) \(summary.unpricedCalls == 1 ? "call" : "calls") used a model with no price set and \(summary.unpricedCalls == 1 ? "is" : "are") not in this total.",
                      systemImage: "exclamationmark.triangle.fill")
                    .font(.footnote).foregroundStyle(Theme.warn)
            }
            budgetLine
            VStack(spacing: 8) {
                UsageDonutChart(model: model, totals: summary.total, period: period, focusedKey: store.focusedKey) { id in
                    store.toggleFocus(id)
                }
                if model.basis == .empty {
                    Text("No calls through Grouper in this period.").font(.callout).foregroundStyle(Theme.text2)
                } else if model.basis == .tokens {
                    Text("No paid usage. Showing share of tokens.").font(.callout).foregroundStyle(Theme.text2)
                }
            }
            .frame(maxWidth: .infinity)
            keyList(model)
        }
    }

    private var totalBlock: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text(UsageFormat.money(summary.total.cost))
                    .font(.title.weight(.semibold)).monospacedDigit().foregroundStyle(Theme.text)
                TagBadge(text: "estimated")
            }
            Text(period.caption).font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
            Text("\(UsageFormat.tokens(summary.total.tokensIn)) tokens in \u{00B7} \(UsageFormat.tokens(summary.total.tokensOut)) out \u{00B7} \(UsageFormat.calls(summary.total.calls))")
                .font(Theme.Typeface.meta).monospacedDigit().foregroundStyle(Theme.text3)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(UsageFormat.money(summary.total.cost)) estimated, \(period.phrase). \(UsageFormat.tokenSpeech(summary.total))")
    }

    /// "Daily budget: $1.20 of $5.00 · resets 00:00 UTC (01:00 your time)". The budget is a UTC day,
    /// so it is its own line, never the Day total (spec 4.1).
    @ViewBuilder private var budgetLine: some View {
        if let report = limits {
            if let budget = report.limits.dailyBudgetUsd {
                Text("Daily budget: \(UsageFormat.money(report.spentTodayUsd)) of \(UsageFormat.money(budget)) \u{00B7} resets \(resetClock(report.dayResetsAt))")
                    .font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
            } else {
                Text("No daily budget set").font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
            }
        }
    }

    private func resetClock(_ seconds: UInt64) -> String {
        let utc = TimeZone(identifier: "UTC") ?? .gmt
        let utcText = UsageFormat.clock(seconds, locale: Locale(identifier: "en_GB"), timeZone: utc)
        let localText = UsageFormat.clock(seconds)
        return utcText == localText ? "\(utcText) UTC" : "\(utcText) UTC (\(localText) your time)"
    }

    private func keyList(_ model: DonutModel) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Registered keys").font(Theme.Typeface.sectionLabel).foregroundStyle(Theme.text)
                .accessibilityAddTraits(.isHeader)
            if summary.keys.isEmpty {
                Text("No API keys are registered yet.").font(.callout).foregroundStyle(Theme.text2)
            }
            ForEach(summary.keys) { key in
                UsageKeyRow(key: key, share: share(of: key, in: model), store: store,
                            reports: UsageProviderReports.lines(forProvider: key.provider, in: snapshots),
                            lowBalance: UsageProviderReports.isLowBalance(forProvider: key.provider, in: snapshots),
                            focused: isFocused(key, in: model), canEdit: canEdit) {
                    onChangeColor(key)
                }
            }
        }
    }

    private func share(of key: UsageKey, in model: DonutModel) -> Double? {
        guard model.total > 0 else { return nil }
        switch model.basis {
        case .cost: return key.totals.cost > 0 ? key.totals.cost / model.total : nil
        case .tokens: return key.totals.tokensTotal > 0 ? Double(key.totals.tokensTotal) / model.total : nil
        case .empty: return nil
        }
    }

    private func isFocused(_ key: UsageKey, in model: DonutModel) -> Bool {
        guard let id = store.focusedKey else { return false }
        return id == key.provider || (id == "other" && model.segment(forProvider: key.provider)?.isOther == true)
    }
}

// MARK: - Donut

/// The ring (spec 4.2): Swift Charts `SectorMark`, one segment per key in the key's colour, clockwise
/// from 12 o'clock, largest first. A tap on a segment focuses it (it grows 3pt, the others fade to
/// 35%); a tap on the centre or the same segment again clears it. The key list is the legend and the
/// operable part; the chart is one image with a spoken summary.
private struct UsageDonutChart: View {
    let model: DonutModel
    let totals: UsageTotals
    let period: UsagePeriod
    let focusedKey: String?
    let onToggle: (String) -> Void

    #if os(macOS)
    private let size: CGFloat = 200
    #else
    private let size: CGFloat = 180
    #endif
    private let thickness: CGFloat = 22
    private var innerRatio: Double { Double((size / 2 - thickness) / (size / 2)) }

    private var focusedSegment: DonutSegment? {
        guard let id = focusedKey else { return nil }
        return model.segment(forProvider: id) ?? model.segments.first { $0.id == id }
    }

    private func color(_ segment: DonutSegment) -> Color {
        segment.colorName.map { Theme.keyColor($0) } ?? Theme.keyUnknown
    }

    private func figure(_ value: Double) -> String {
        model.basis == .tokens ? UsageFormat.tokens(UInt64(max(0, value.rounded()))) : UsageFormat.money(value)
    }

    var body: some View {
        ZStack {
            if model.basis == .empty {
                Circle().strokeBorder(Theme.border, lineWidth: thickness)
            } else {
                Chart(model.segments) { segment in
                    SectorMark(
                        angle: .value(model.basis == .tokens ? "Tokens" : "Cost", segment.weight),
                        innerRadius: .ratio(innerRatio),
                        outerRadius: isFocused(segment) ? .inset(0) : .inset(3),
                        angularInset: model.segments.count > 1 ? 1 : 0
                    )
                    .foregroundStyle(color(segment))
                    .opacity(dimmed(segment) ? 0.35 : 1)
                    .accessibilityLabel(segment.title)
                    .accessibilityValue("\(figure(segment.value)), \(UsageFormat.percent(segment.share))")
                }
                .accessibilityLabel(UsageDonut.accessibilitySummary(model, period: period, totals: totals))
            }
            centre
                .frame(maxWidth: size * 0.6)
                .allowsHitTesting(false)
                .accessibilityHidden(true)
        }
        .frame(width: size, height: size)
        .contentShape(Circle())
        .onTapGesture { location in tap(at: location) }
    }

    private func isFocused(_ segment: DonutSegment) -> Bool {
        guard let focused = focusedSegment else { return false }
        return focused.id == segment.id
    }

    private func dimmed(_ segment: DonutSegment) -> Bool {
        focusedSegment != nil && !isFocused(segment)
    }

    /// Nothing focused: the total and "All keys". A key focused: its name, cost and share.
    @ViewBuilder private var centre: some View {
        VStack(spacing: 2) {
            if let segment = focusedSegment {
                Text(segment.title).font(.footnote.weight(.semibold)).foregroundStyle(Theme.text).lineLimit(1)
                Text(figure(segment.value)).font(.title3.weight(.semibold)).monospacedDigit().foregroundStyle(Theme.text)
                Text(UsageFormat.percent(segment.share)).font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
            } else {
                Text(model.basis == .tokens ? figure(model.total) : UsageFormat.money(totals.cost))
                    .font(.title3.weight(.semibold)).monospacedDigit().foregroundStyle(Theme.text)
                Text(model.basis == .tokens ? "Tokens" : "All keys").font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
            }
        }
        .multilineTextAlignment(.center)
    }

    /// The segment under a tap, from its angle clockwise from 12 o'clock. The centre clears focus.
    private func tap(at point: CGPoint) {
        guard model.basis != .empty, !model.segments.isEmpty else { return }
        let dx = Double(point.x - size / 2)
        let dy = Double(point.y - size / 2)
        let radius = (dx * dx + dy * dy).squareRoot()
        let outer = Double(size / 2)
        let inner = outer - Double(thickness)
        if radius < inner {
            if let id = focusedKey { onToggle(id) }
            return
        }
        guard radius <= outer + 4 else { return }
        var angle = atan2(dx, -dy)
        if angle < 0 { angle += 2 * Double.pi }
        let totalWeight = model.segments.reduce(0) { $0 + $1.weight }
        let target = angle / (2 * Double.pi) * totalWeight
        var running = 0.0
        for segment in model.segments {
            running += segment.weight
            if target <= running {
                onToggle(segment.isOther ? "other" : segment.providers.first ?? segment.id)
                return
            }
        }
        if let last = model.segments.last { onToggle(last.isOther ? "other" : last.providers.first ?? last.id) }
    }
}

// MARK: - Key rows

/// One key: swatch, name, badges, cost and share on the row; the models, totals, provider reports
/// and actions in its disclosure (spec 4.3). One VoiceOver element for the collapsed row.
private struct UsageKeyRow: View {
    let key: UsageKey
    let share: Double?
    @ObservedObject var store: UsageStore
    let reports: [ProviderReportLine]
    let lowBalance: Bool
    let focused: Bool
    let canEdit: Bool
    let onChangeColor: () -> Void

    private var color: Color { key.removed ? Theme.keyUnknown : Theme.keyColor(key.color) }

    var body: some View {
        DisclosureGroup(isExpanded: Binding(
            get: { store.expandedKeys.contains(key.provider) },
            set: { store.setExpanded(key.provider, $0) }
        )) {
            UsageKeyDetails(key: key, color: color, reports: reports, showAll: store.showAllModels.contains(key.provider),
                            toggleShowAll: {
                                if store.showAllModels.contains(key.provider) { store.showAllModels.remove(key.provider) }
                                else { store.showAllModels.insert(key.provider) }
                            },
                            canEdit: canEdit, onChangeColor: onChangeColor)
                .padding(.top, 8)
        } label: {
            label
        }
        .padding(.horizontal, 8).padding(.vertical, 6)
        .background(focused ? Theme.surfaceSelect : Color.clear, in: RoundedRectangle(cornerRadius: Theme.Radius.sm))
    }

    private var label: some View {
        ViewThatFits(in: .horizontal) {
            HStack(alignment: .top, spacing: 10) {
                KeySwatch(color: color, filled: key.totals.cost > 0).padding(.top, 4)
                nameBlock
                Spacer(minLength: 8)
                costBlock(alignment: .trailing)
            }
            // At large text sizes the cost wraps under the name instead of truncating.
            HStack(alignment: .top, spacing: 10) {
                KeySwatch(color: color, filled: key.totals.cost > 0).padding(.top, 4)
                VStack(alignment: .leading, spacing: 4) {
                    nameBlock
                    costBlock(alignment: .leading)
                }
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(spoken)
    }

    private var nameBlock: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Text(key.displayName).font(Theme.Typeface.row).foregroundStyle(Theme.text).lineLimit(2)
                if key.local { TagBadge(text: "Local", tint: Theme.ok, fill: Theme.okSoft) }
                if lowBalance { TagBadge(text: "Low balance", tint: Theme.warn, fill: Theme.warnSoft) }
            }
            Text(UsageFormat.tokenLine(key.totals)).font(Theme.Typeface.meta).monospacedDigit().foregroundStyle(Theme.text3)
        }
    }

    private func costBlock(alignment: HorizontalAlignment) -> some View {
        VStack(alignment: alignment, spacing: 2) {
            Text(UsageFormat.money(key.totals.cost)).font(Theme.Typeface.row).monospacedDigit().foregroundStyle(Theme.text)
            if let share { Text(UsageFormat.percent(share)).font(Theme.Typeface.meta).monospacedDigit().foregroundStyle(Theme.text3) }
        }
    }

    private var spoken: String {
        var parts = [key.displayName]
        if key.local { parts.append("local") }
        if lowBalance { parts.append("low balance") }
        parts.append(UsageFormat.money(key.totals.cost))
        if let share { parts.append("\(UsageFormat.percent(share)) of the total") }
        parts.append(UsageFormat.tokenSpeech(key.totals))
        return parts.joined(separator: ", ")
    }
}

/// The expanded part of a key row.
private struct UsageKeyDetails: View {
    let key: UsageKey
    let color: Color
    let reports: [ProviderReportLine]
    let showAll: Bool
    let toggleShowAll: () -> Void
    let canEdit: Bool
    let onChangeColor: () -> Void

    private static let modelLimit = 6

    private var models: [UsageKeyModel] {
        key.models.sorted { a, b in
            if a.totals.cost != b.totals.cost { return a.totals.cost > b.totals.cost }
            if a.totals.tokensTotal != b.totals.tokensTotal { return a.totals.tokensTotal > b.totals.tokensTotal }
            return a.model < b.model
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            modelsList
            keyTotals
            if !reports.isEmpty { providerReports }
            actions
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    @ViewBuilder private var modelsList: some View {
        if key.models.isEmpty {
            Text("No models used through this key in this period.").font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
        } else {
            let all = models
            let shown = showAll ? all : Array(all.prefix(Self.modelLimit))
            VStack(alignment: .leading, spacing: 10) {
                ForEach(shown) { model in modelRow(model) }
                if all.count > Self.modelLimit {
                    Button(showAll ? "Show fewer" : "Show all \(all.count)", action: toggleShowAll)
                        .font(.footnote.weight(.semibold))
                        .grouperMinimumControlSize()
                }
            }
        }
    }

    private func modelRow(_ model: UsageKeyModel) -> some View {
        let tokensTotal = model.totals.tokensTotal
        let fraction: Double = key.totals.cost > 0
            ? model.totals.cost / key.totals.cost
            : (key.totals.tokensTotal > 0 ? Double(tokensTotal) / Double(key.totals.tokensTotal) : 0)
        return VStack(alignment: .leading, spacing: 4) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                FamilyMark(family: ModelFamily.of(model: model.model))
                Text(model.name).font(Theme.Typeface.row).foregroundStyle(Theme.text).lineLimit(2)
                if model.free { TagBadge(text: "Free", tint: Theme.ok, fill: Theme.okSoft) }
                if model.local { TagBadge(text: "Local", tint: Theme.ok, fill: Theme.okSoft) }
                if !model.priced && !model.free && !model.local { TagBadge(text: "No price set", tint: Theme.warn, fill: Theme.warnSoft) }
                Spacer(minLength: 6)
                VStack(alignment: .trailing, spacing: 1) {
                    Text(UsageFormat.money(model.totals.cost)).font(Theme.Typeface.row).monospacedDigit().foregroundStyle(Theme.text)
                    if tokensTotal > 0 {
                        Text("\(UsageFormat.tokens(model.totals.tokensIn)) / \(UsageFormat.tokens(model.totals.tokensOut))")
                            .font(Theme.Typeface.meta).monospacedDigit().foregroundStyle(Theme.text3)
                    }
                }
            }
            ShareBar(fraction: fraction, color: color)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(modelSpoken(model, fraction: fraction))
    }

    private func modelSpoken(_ model: UsageKeyModel, fraction: Double) -> String {
        var parts = [model.name]
        if model.free { parts.append("free") }
        if model.local { parts.append("local") }
        if !model.priced && !model.free && !model.local { parts.append("no price set") }
        parts.append(UsageFormat.money(model.totals.cost))
        if model.totals.tokensTotal > 0 { parts.append(UsageFormat.tokenSpeech(model.totals)) }
        if fraction > 0 { parts.append("\(UsageFormat.percent(fraction)) of this key") }
        return parts.joined(separator: ", ")
    }

    /// "Input 412K (of which 301K read from cache, 12K written to cache) · Output 38K"; the cache
    /// parts only when they are not zero.
    private var keyTotals: some View {
        let t = key.totals
        var cache: [String] = []
        if t.cacheReadTokens > 0 { cache.append("\(UsageFormat.tokens(t.cacheReadTokens)) read from cache") }
        if t.cacheWriteTokens > 0 { cache.append("\(UsageFormat.tokens(t.cacheWriteTokens)) written to cache") }
        var line = "Input \(UsageFormat.tokens(t.tokensIn))"
        if !cache.isEmpty { line += " (of which \(cache.joined(separator: ", ")))" }
        line += " \u{00B7} Output \(UsageFormat.tokens(t.tokensOut))"
        return Text(line).font(Theme.Typeface.meta).monospacedDigit().foregroundStyle(Theme.text2)
            .accessibilityLabel("Input \(UsageFormat.exactTokens(t.tokensIn)) tokens, output \(UsageFormat.exactTokens(t.tokensOut)) tokens")
    }

    /// What the provider's own balance or spend API says. It can include use outside Grouper, so it
    /// is labelled and never added to the total or drawn in the donut.
    private var providerReports: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Provider reports").font(Theme.Typeface.sectionLabel).foregroundStyle(Theme.text2)
            ForEach(reports) { line in
                VStack(alignment: .leading, spacing: 2) {
                    Text(line.text).font(.footnote).foregroundStyle(Theme.text)
                    TimelineView(.everyMinute) { context in
                        HStack(spacing: 6) {
                            HStack(spacing: 4) {
                                Text("Fetched")
                                RelativeAgeText(seconds: line.fetchedAt)
                            }
                            if line.isStale(now: context.date) { TagBadge(text: "Stale", tint: Theme.warn, fill: Theme.warnSoft) }
                        }
                        .font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
                    }
                }
                .accessibilityElement(children: .combine)
            }
            Text("Can include use outside Grouper. Not part of the total above.")
                .font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
        }
    }

    private var actions: some View {
        VStack(alignment: .leading, spacing: 6) {
            Button(action: onChangeColor) { Label("Change colour", systemImage: "paintpalette") }
                .disabled(!canEdit)
                .grouperMinimumControlSize()
            Text("See breakdown: coming to this app").font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
        }
    }
}

// MARK: - Colour picker

struct KeyColorTarget: Identifiable, Equatable {
    let provider: String
    let current: String?
    var id: String { provider }
}

/// "Change colour": ten swatches, each named, with another key's name under a colour it uses.
/// Picking one saves at once.
private struct KeyColorSheet: View {
    @Environment(\.dismiss) private var dismiss
    let target: KeyColorTarget
    let usedBy: [String: [String]]
    /// Saves the choice; returns an error message, or nil when it worked.
    let pick: (String?) async -> String?
    @State private var saving = false
    @State private var failure: String?

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    LazyVGrid(columns: [GridItem(.adaptive(minimum: 92), spacing: 12)], spacing: 16) {
                        ForEach(KeyColorName.allCases) { option in swatch(option) }
                    }
                    Button { choose(nil) } label: { Label("Let Grouper choose", systemImage: "wand.and.stars") }
                        .disabled(saving)
                        .grouperMinimumControlSize()
                    if let failure {
                        Text(failure).font(.callout).foregroundStyle(Theme.danger)
                    }
                }
                .padding(16)
            }
            .background(Theme.bg)
            .navigationTitle("Colour for \(target.provider)")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
            }
        }
        .presentationDetents([.medium, .large])
        #if os(macOS)
        .frame(minWidth: 420, minHeight: 380)
        #endif
    }

    private func swatch(_ option: KeyColorName) -> some View {
        let selected = target.current == option.rawValue
        let others = usedBy[option.rawValue] ?? []
        var spoken = option.title
        if selected { spoken += ", selected" }
        if !others.isEmpty { spoken += ", used by " + others.joined(separator: ", ") }
        return Button { choose(option.rawValue) } label: {
            VStack(spacing: 4) {
                ZStack {
                    Circle().fill(Theme.keyColor(option.rawValue)).frame(width: 36, height: 36)
                    if selected {
                        Image(systemName: "checkmark").font(.footnote.weight(.bold)).foregroundStyle(Theme.bg)
                    }
                }
                Text(option.title).font(Theme.Typeface.chip).foregroundStyle(Theme.text)
                if !others.isEmpty {
                    Text(others.joined(separator: ", ")).font(.caption2).foregroundStyle(Theme.text3).lineLimit(2)
                        .multilineTextAlignment(.center)
                }
            }
            .frame(maxWidth: .infinity, minHeight: 44)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(saving)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(spoken)
        .accessibilityAddTraits(.isButton)
    }

    private func choose(_ name: String?) {
        saving = true
        failure = nil
        Task {
            let problem = await pick(name)
            saving = false
            if let problem { failure = problem } else { dismiss() }
        }
    }
}

// MARK: - Subscription cards

/// One subscription account: family mark, plan, devices, the age of the reading, a meter per
/// window (Antigravity under its group headings), and the optional "Through Grouper" line.
private struct SubscriptionCardView: View {
    let card: SubscriptionCard
    let now: Date
    let throughGrouper: String?

    var body: some View {
        let stale = card.isStale(now: now)
        VStack(alignment: .leading, spacing: 12) {
            header(stale: stale)
            ForEach(card.notes, id: \.self) { note in
                Label(note, systemImage: "exclamationmark.triangle.fill").font(.footnote).foregroundStyle(Theme.warn)
            }
            ForEach(card.groups) { group in
                VStack(alignment: .leading, spacing: 10) {
                    if let title = group.title {
                        Text(title).font(Theme.Typeface.sectionLabel).foregroundStyle(Theme.text2)
                            .accessibilityAddTraits(.isHeader)
                    }
                    ForEach(group.meters) { meter in
                        SubscriptionMeterRow(meter: meter, family: card.family, stale: stale, now: now)
                    }
                }
            }
            if let throughGrouper {
                Text(throughGrouper).font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
            }
        }
        .grouperCard(padding: 12, fill: Theme.surface2)
    }

    private func header(stale: Bool) -> some View {
        HStack(alignment: .top, spacing: 8) {
            FamilyMark(family: card.family)
            VStack(alignment: .leading, spacing: 2) {
                Text(card.planName).font(Theme.Typeface.row.weight(.semibold)).foregroundStyle(Theme.text)
                Text(card.signedInLine).font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
            }
            Spacer(minLength: 8)
            if stale {
                TagBadge(text: "Stale \u{00B7} read \(readTime)", tint: Theme.warn, fill: Theme.warnSoft)
            } else {
                RelativeAgeText(seconds: card.fetchedAt).font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
            }
        }
        .accessibilityElement(children: .combine)
    }

    /// "14:05" today, "Thu 3 Dec, 14:05" otherwise.
    private var readTime: String {
        let read = Date(timeIntervalSince1970: TimeInterval(card.fetchedAt))
        return Calendar.current.isDate(read, inSameDayAs: now) ? UsageFormat.clock(card.fetchedAt) : UsageFormat.dateTime(card.fetchedAt)
    }
}

/// One window: label, a 6pt bar, "62% used", and when it resets. The state is always a word and an
/// icon, never colour alone. One VoiceOver element: "Weekly, 62 percent used, resets in 2 hours 14 minutes".
private struct SubscriptionMeterRow: View {
    let meter: SubscriptionMeter
    let family: ModelFamily
    let stale: Bool
    let now: Date

    var body: some View {
        let state = meter.state(now: now)
        VStack(alignment: .leading, spacing: 5) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text(meter.label).font(Theme.Typeface.row).foregroundStyle(Theme.text)
                Spacer(minLength: 8)
                Text(meter.valueText(now: now)).font(Theme.Typeface.row).monospacedDigit().foregroundStyle(Theme.text2)
            }
            if meter.usedPct != nil || state == .notInUse || state == .resetPassed {
                bar(state: state)
            }
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                if let word = meter.warningWord(now: now) {
                    Label(word, systemImage: state == .usedUp ? "xmark.octagon.fill" : "exclamationmark.triangle.fill")
                        .font(Theme.Typeface.chip).foregroundStyle(state == .usedUp ? Theme.danger : Theme.warn)
                }
                if let detail = meter.detailText(now: now) {
                    Text(detail).font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
                }
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(meter.spokenText(now: now))
    }

    private func bar(state: SubscriptionMeter.State) -> some View {
        let fill: Color = state == .usedUp ? Theme.danger : (state == .nearlyUsed ? Theme.warn : family.color)
        let fraction = state == .notInUse || state == .resetPassed ? 0 : min(1, max(0, (meter.usedPct ?? 0) / 100))
        return GeometryReader { geo in
            ZStack(alignment: .leading) {
                Capsule().fill(Theme.surface3)
                if state == .resetPassed {
                    Capsule().strokeBorder(Theme.borderControl, style: StrokeStyle(lineWidth: 1, dash: [3, 2]))
                } else if fraction > 0 {
                    Capsule().fill(fill).frame(width: max(6, geo.size.width * fraction))
                }
            }
        }
        .frame(height: 6)
        .opacity(stale ? 0.6 : 1)
        .accessibilityHidden(true)
    }
}

/// A slim card: a state with no numbers (not signed in, sign-in expired, unavailable, reading off).
private struct SubscriptionNoteView: View {
    let note: SubscriptionNote

    private var icon: String {
        switch note.kind {
        case .notSignedIn: return "person.crop.circle.badge.xmark"
        case .expired: return "exclamationmark.triangle.fill"
        case .unavailable: return "exclamationmark.circle"
        case .readingOff: return "pause.circle"
        }
    }

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: icon).foregroundStyle(Theme.text2).accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(note.text).font(.footnote).foregroundStyle(Theme.text)
                if let hint = note.hint {
                    Text(hint).font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
                }
            }
        }
        .grouperCard(padding: 10, fill: Theme.surface2)
        .accessibilityElement(children: .combine)
    }
}

// MARK: - More

/// Breakdown and Limits and budgets are not built in this app yet (package A2): the rows are here
/// and say so, rather than opening the web page. "Where these numbers come from" is a closed
/// disclosure at the foot (spec 6).
private struct UsageMoreList: View {
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("More").font(Theme.Typeface.title).foregroundStyle(Theme.text)
                .accessibilityAddTraits(.isHeader)
                .padding(.bottom, 6)
            row("Breakdown", systemImage: "list.bullet.rectangle")
            Divider().overlay(Theme.border)
            row("Limits and budgets", systemImage: "slider.horizontal.3")
            Divider().overlay(Theme.border)
            DisclosureGroup {
                VStack(alignment: .leading, spacing: 8) {
                    Text("API costs are Grouper's own estimate: the tokens each provider reported for calls made through Grouper, times the price in the model registry at the time of the call. Calls made outside Grouper are not counted. What a provider's own balance or spend API says is shown separately inside each key, and is never added to the total.")
                    Text("Subscriptions are read from the Claude, ChatGPT, Grok and Antigravity apps signed in on your devices, using each app's own sign-in. The sign-in never leaves the device; only percentages and reset times reach your hub. These readings are unofficial and may lag or change.")
                }
                .font(.footnote).foregroundStyle(Theme.text2)
                .padding(.top, 6)
            } label: {
                Text("Where these numbers come from").font(Theme.Typeface.row).foregroundStyle(Theme.text)
                    .frame(minHeight: 44, alignment: .leading)
            }
        }
        .grouperCard(padding: 16)
    }

    private func row(_ title: String, systemImage: String) -> some View {
        HStack(spacing: 10) {
            Label(title, systemImage: systemImage).font(Theme.Typeface.row).foregroundStyle(Theme.text2)
            Spacer(minLength: 8)
            Text("Coming to this app").font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
        }
        .frame(minHeight: 44)
        .accessibilityElement(children: .combine)
    }
}
