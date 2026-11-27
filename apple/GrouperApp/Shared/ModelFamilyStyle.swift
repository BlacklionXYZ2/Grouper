import SwiftUI
import GrouperAPI
#if canImport(UIKit)
import UIKit
#elseif canImport(AppKit)
import AppKit
#endif

// Model-family accents: the web client's --fam-* aliases (app.css) of the identity-hue
// ramps in Theme.swift. Colour stays by family (owner question Q4) through the single
// table `identityHue` below; the common families land on the concept art's set:
// Gemini/Gemma teal, Qwen purple, Llama orange, Radium indigo. The colour is a supplement: the model's
// name is always shown as text beside it, and FamilyMark adds a glyph.

extension ModelFamily {
    /// THE family -> hue table. Identity colours will be re-mapped from model families to
    /// providers (Anthropic, OpenAI, OpenRouter, DeepSeek...): change the mapping here and
    /// nowhere else. nil means a neutral that sits outside the ramp.
    var identityHue: IdentityHue? {
        switch self {
        case .gemini: return .teal
        case .qwen: return .purple
        case .llama: return .orange
        case .radium: return .indigo
        case .claude: return .pink
        case .gpt: return .blue
        case .deepseek: return .cyan
        case .mistral: return .rose
        case .nemotron: return .green
        case .ernie: return .amber
        case .grok, .scripted, .unknown: return nil
        }
    }

    /// Text, glyph and outline colour: step 4 of the hue ramp.
    var color: Color { identityHue?.text ?? Color.grouperDynamic(neutralPalette) }
    /// Dots and bars: step 3.
    var solidColor: Color { identityHue?.solid ?? Color.grouperDynamic(neutralPalette) }
    /// Chip and mark background: step 1 (the plain surface for the neutrals).
    var chipBackground: Color { identityHue?.bg ?? Theme.surface }

    /// Grok (near-white / near-black), test models and unknown stay single neutral values.
    private var neutralPalette: ThemeSwatch {
        switch self {
        case .grok: return ThemeSwatch(light: 0x2B2F36, dark: 0xE6E8EC, lightHighContrast: 0x16181C, darkHighContrast: 0xFFFFFF)
        case .scripted: return ThemeSwatch(light: 0x636327, dark: 0xC9C97A, lightHighContrast: 0x525220, darkHighContrast: 0xDEDE9C)
        default: return Theme.neutralSwatch
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
        .background(family.chipBackground, in: RoundedRectangle(cornerRadius: Theme.Radius.xs))
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
