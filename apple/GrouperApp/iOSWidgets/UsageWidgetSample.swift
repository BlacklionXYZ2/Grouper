import Foundation
import GrouperAPI

/// Made-up figures for the widget gallery and the placeholder, so the pickers show what each size
/// looks like before the app has written anything. Never shown as real data on a Home Screen: the
/// timeline reads only the app's file, and an empty one says "Open Grouper to load usage".
enum WidgetSample {
    static func snapshot(now: Date) -> WidgetSnapshot {
        let at = now.timeIntervalSince1970
        let nowSeconds = UInt64(at)

        func key(_ provider: String, _ color: String, cost: Double, tokensIn: UInt64, tokensOut: UInt64, calls: UInt64) -> UsageKey {
            UsageKey(provider: provider, color: color,
                     totals: UsageTotals(calls: calls, inputTokens: tokensIn, outputTokens: tokensOut, cost: cost))
        }
        let keys = [
            key("openrouter", "teal", cost: 1.83, tokensIn: 412_000, tokensOut: 38_000, calls: 31),
            key("deepseek", "cyan", cost: 1.12, tokensIn: 880_000, tokensOut: 61_000, calls: 24),
            key("together", "purple", cost: 0.74, tokensIn: 190_000, tokensOut: 22_000, calls: 12),
            key("mistral", "rose", cost: 0.31, tokensIn: 64_000, tokensOut: 9_000, calls: 7),
        ]
        func summary(_ factor: Double) -> UsageSummary {
            let scaled = keys.map {
                UsageKey(provider: $0.provider, color: $0.color,
                         totals: UsageTotals(calls: $0.totals.calls, inputTokens: UInt64(Double($0.totals.inputTokens) * factor),
                                             outputTokens: UInt64(Double($0.totals.outputTokens) * factor),
                                             cost: $0.totals.cost * factor))
            }
            let total = scaled.reduce(UsageTotals()) {
                UsageTotals(calls: $0.calls + $1.totals.calls, inputTokens: $0.inputTokens + $1.totals.inputTokens,
                            outputTokens: $0.outputTokens + $1.totals.outputTokens, cost: $0.cost + $1.totals.cost)
            }
            return UsageSummary(total: total, keys: scaled)
        }
        let periods: [(UsagePeriod, Double)] = [(.day, 1), (.week, 5.5), (.month, 21)]
        let costs = periods.map { period, factor in
            UsageCacheFile.TimedSummary(period: period.rawValue, fetchedAt: at, summary: summary(factor))
        }

        func window(_ id: String, _ label: String, pct: Double, resetsIn: UInt64, seconds: Int64, scope: String? = nil,
                    scopeLabel: String? = nil, span: String? = nil) -> WidgetMeter {
            WidgetMeter(id: id, label: label, spanLabel: span, usedPct: pct, resetsAt: nowSeconds + resetsIn,
                        spanSeconds: seconds, scope: scope, scopeLabel: scopeLabel)
        }
        let claude = WidgetAccount(id: "claude|sample", provider: "claude", planName: "Claude Max", deviceNames: ["Gaming PC"],
                                   fetchedAt: nowSeconds - 120, groups: [WidgetGroup(id: "plain", meters: [
                                       window("5h", "5-hour", pct: 62, resetsIn: 2 * 3600 + 14 * 60, seconds: 18_000),
                                       window("7d", "Weekly", pct: 31, resetsIn: 3 * 86_400, seconds: 604_800),
                                       window("7d:fable", "Weekly \u{00B7} Fable", pct: 86, resetsIn: 3 * 86_400, seconds: 604_800,
                                              scope: "fable", scopeLabel: "Fable", span: "Weekly"),
                                   ])])
        let codex = WidgetAccount(id: "codex|sample", provider: "codex", planName: "ChatGPT Plus", deviceNames: ["MacBook"],
                                  fetchedAt: nowSeconds - 120, groups: [WidgetGroup(id: "plain", meters: [
                                      window("5h", "5-hour", pct: 18, resetsIn: 3 * 3600, seconds: 18_000),
                                      window("7d", "Weekly", pct: 44, resetsIn: 4 * 86_400, seconds: 604_800),
                                  ])])
        let antigravity = WidgetAccount(id: "antigravity|sample", provider: "antigravity", planName: "Antigravity",
                                        deviceNames: ["Gaming PC"], fetchedAt: nowSeconds - 120, groups: [
                                            WidgetGroup(id: "scope:gemini", title: "Gemini models", meters: [
                                                window("5h:gemini", "5-hour", pct: 40, resetsIn: 3600, seconds: 18_000, scope: "gemini", scopeLabel: "Gemini models"),
                                                window("7d:gemini", "Weekly", pct: 100, resetsIn: 2 * 86_400, seconds: 604_800, scope: "gemini", scopeLabel: "Gemini models"),
                                            ]),
                                        ])
        return WidgetSnapshot(writtenAt: at, costs: costs, accounts: [antigravity, claude, codex], accountsAt: at)
    }
}
