import SwiftUI
import GrouperAPI
#if canImport(UIKit)
import UIKit
#elseif canImport(AppKit)
import AppKit
#endif

// Model-family accents: the web client's --fam-* tokens (app.css), with the same
// light, dark and Increase Contrast values. Colour stays by family (owner question
// Q4); the common families land on the concept art's set: Gemini/Gemma teal, Qwen
// purple, Llama orange, Radium indigo. The colour is a supplement: the model's
// name is always shown as text beside it, and FamilyMark adds a glyph.

extension ModelFamily {
    var color: Color { Color.grouperDynamic(palette) }

    private var palette: ThemeSwatch {
        switch self {
        case .gemini: return ThemeSwatch(light: 0x0B6B73, dark: 0x4FE3E6, lightHighContrast: 0x06525A, darkHighContrast: 0x7FF0F2)
        case .qwen: return ThemeSwatch(light: 0x5B3FC4, dark: 0xA78BFA, lightHighContrast: 0x47309F, darkHighContrast: 0xC4B0FF)
        case .llama: return ThemeSwatch(light: 0x9A4A06, dark: 0xF9A85A, lightHighContrast: 0x7A3A04, darkHighContrast: 0xFFC48A)
        case .radium: return ThemeSwatch(light: 0x4D4FC9, dark: 0x8891F0, lightHighContrast: 0x3A3CA8, darkHighContrast: 0xB0B6FF)
        case .claude: return ThemeSwatch(light: 0xA12D86, dark: 0xF28AD8, lightHighContrast: 0x82216B, darkHighContrast: 0xF8B0E6)
        case .gpt: return ThemeSwatch(light: 0x1F5FCC, dark: 0x7FB0FF, lightHighContrast: 0x164AA3, darkHighContrast: 0xA9C9FF)
        case .deepseek: return ThemeSwatch(light: 0x0A6F8C, dark: 0x5CC8E8, lightHighContrast: 0x06566D, darkHighContrast: 0x8ADCF2)
        case .mistral: return ThemeSwatch(light: 0xBD2F4A, dark: 0xFF8FA0, lightHighContrast: 0x981F38, darkHighContrast: 0xFFB3BF)
        case .nemotron: return ThemeSwatch(light: 0x436E00, dark: 0x9FD24A, lightHighContrast: 0x395C00, darkHighContrast: 0xBFE57A)
        case .ernie: return ThemeSwatch(light: 0x805B00, dark: 0xE8C04A, lightHighContrast: 0x6A4B00, darkHighContrast: 0xF5D77A)
        case .grok: return ThemeSwatch(light: 0x2B2F36, dark: 0xE6E8EC, lightHighContrast: 0x16181C, darkHighContrast: 0xFFFFFF)
        case .scripted: return ThemeSwatch(light: 0x636327, dark: 0xC9C97A, lightHighContrast: 0x525220, darkHighContrast: 0xDEDE9C)
        case .unknown: return ThemeSwatch(light: 0x5B6573, dark: 0xA3ADBA, lightHighContrast: 0x434B57, darkHighContrast: 0xC2CAD4)
        }
    }
}

/// The family mark: a small box in the family colour with its glyph (the
/// non-colour cue). Unknown gets a generic model icon; test models a dashed box.
struct FamilyMark: View {
    let family: ModelFamily

    var body: some View {
        Group {
            if family == .unknown {
                Image(systemName: "cpu").font(.caption2.weight(.bold))
            } else {
                Text(family.glyph).font(.caption2.monospaced().weight(.bold))
            }
        }
        .foregroundStyle(family.color)
        .padding(.horizontal, 3)
        .frame(minWidth: 22, minHeight: 18)
        .background(Theme.surface, in: RoundedRectangle(cornerRadius: Theme.Radius.xs))
        .overlay {
            RoundedRectangle(cornerRadius: Theme.Radius.xs)
                .strokeBorder(family.color, style: StrokeStyle(lineWidth: 1.5, dash: family == .scripted ? [3, 2] : []))
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(family.accessibilityLabel)
    }
}

/// The model the orchestrator chose for a thread: fixed, with its family colour. No picker.
struct FixedModelChip: View {
    let modelId: String
    let family: ModelFamily
    let providerId: String?

    var body: some View {
        HStack(spacing: 6) {
            FamilyMark(family: family)
            Text(modelId.isEmpty ? "model not recorded" : modelId).font(Theme.Typeface.chip).foregroundStyle(Theme.text).lineLimit(1)
            if !note.isEmpty { Text(note).font(Theme.Typeface.meta).foregroundStyle(Theme.text3).lineLimit(1) }
            Image(systemName: "lock.fill").font(.caption2).foregroundStyle(Theme.text3)
        }
        .padding(.horizontal, 8).padding(.vertical, 4)
        .background(Theme.surfaceSelect, in: RoundedRectangle(cornerRadius: Theme.Radius.sm))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Model chosen by the orchestrator: \(modelId.isEmpty ? "not recorded" : modelId), \(family.accessibilityLabel), fixed")
    }

    private var note: String {
        var parts: [String] = []
        if family != .unknown { parts.append(family.label) }
        if let providerId, !providerId.isEmpty { parts.append("via \(providerId)") }
        parts.append("fixed")
        return parts.joined(separator: " · ")
    }
}

/// A small badge: Free, Paid, Key set, Local and so on. `tint` and `fill` are
/// token pairs (`Theme.ok` on `Theme.okSoft`); the default is quiet text on surface-2.
struct TagBadge: View {
    let text: String
    var tint: Color = Theme.text2
    var fill: Color = Theme.surface2

    var body: some View {
        Text(text)
            .font(.caption2.weight(.semibold))
            .foregroundStyle(tint)
            .padding(.horizontal, 6).padding(.vertical, 2)
            .background(fill, in: RoundedRectangle(cornerRadius: Theme.Radius.xs))
            .lineLimit(1)
    }
}

/// "5 min. ago" for a hub timestamp (seconds since 1970), refreshed once a minute.
/// `Text(date, style: .relative)` ticks every second and has no "ago".
struct RelativeAgeText: View {
    let seconds: UInt64

    var body: some View {
        TimelineView(.everyMinute) { _ in
            Text(Date(timeIntervalSince1970: TimeInterval(seconds))
                .formatted(.relative(presentation: .numeric, unitsStyle: .abbreviated)))
        }
    }
}

/// A thread's elapsed time: ticking once a second while it runs or waits, the
/// total once it is over. The timeline only exists while the thread is live.
struct ElapsedText: View {
    let thread: ThreadInfo

    var body: some View {
        if thread.isActive {
            TimelineView(.periodic(from: .now, by: 1)) { context in
                label(thread.elapsed(now: context.date))
            }
        } else {
            label(thread.elapsed())
        }
    }

    private func label(_ elapsed: ThreadElapsed) -> some View {
        Label(elapsed.text, systemImage: "clock")
            .monospacedDigit()
            .accessibilityLabel(elapsed.live ? "Elapsed \(elapsed.text)" : elapsed.text)
    }
}
