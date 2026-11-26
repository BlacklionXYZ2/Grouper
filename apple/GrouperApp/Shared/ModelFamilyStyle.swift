import SwiftUI
import GrouperAPI
#if canImport(UIKit)
import UIKit
#elseif canImport(AppKit)
import AppKit
#endif

// Model-family accents: the web client's --fam-* tokens (app.css), with the same
// light, dark and Increase Contrast values. The colour is a supplement: the model's
// name is always shown as text beside it, and FamilyMark adds a glyph.

struct FamilyPalette: Sendable {
    let light: UInt32
    let dark: UInt32
    let lightHighContrast: UInt32
    let darkHighContrast: UInt32
}

extension ModelFamily {
    var color: Color { Color.grouperDynamic(palette) }

    private var palette: FamilyPalette {
        switch self {
        case .claude: return FamilyPalette(light: 0x6A42C2, dark: 0xB9A2FF, lightHighContrast: 0x55309F, darkHighContrast: 0xD2C2FF)
        case .gpt: return FamilyPalette(light: 0x0A7058, dark: 0x4FD1A8, lightHighContrast: 0x065C46, darkHighContrast: 0x7FE6C4)
        case .gemini: return FamilyPalette(light: 0x1F5FCC, dark: 0x7FB0FF, lightHighContrast: 0x164AA3, darkHighContrast: 0xA9C9FF)
        case .llama: return FamilyPalette(light: 0xA8430A, dark: 0xFF9D5C, lightHighContrast: 0x8A3606, darkHighContrast: 0xFFBB8A)
        case .qwen: return FamilyPalette(light: 0x805B00, dark: 0xE8C04A, lightHighContrast: 0x6A4B00, darkHighContrast: 0xF5D77A)
        case .deepseek: return FamilyPalette(light: 0x0A6F8C, dark: 0x5CC8E8, lightHighContrast: 0x06566D, darkHighContrast: 0x8ADCF2)
        case .mistral: return FamilyPalette(light: 0xBD2F4A, dark: 0xFF8FA0, lightHighContrast: 0x981F38, darkHighContrast: 0xFFB3BF)
        case .nemotron: return FamilyPalette(light: 0x436E00, dark: 0x9FD24A, lightHighContrast: 0x395C00, darkHighContrast: 0xBFE57A)
        case .ernie: return FamilyPalette(light: 0xA12D86, dark: 0xF28AD8, lightHighContrast: 0x82216B, darkHighContrast: 0xF8B0E6)
        case .grok: return FamilyPalette(light: 0x2B2F36, dark: 0xE6E8EC, lightHighContrast: 0x16181C, darkHighContrast: 0xFFFFFF)
        case .radium: return FamilyPalette(light: 0x4D4FC9, dark: 0xA7A9FF, lightHighContrast: 0x3A3CA8, darkHighContrast: 0xC4C6FF)
        case .scripted: return FamilyPalette(light: 0x6B6B2A, dark: 0xC9C97A, lightHighContrast: 0x525220, darkHighContrast: 0xDEDE9C)
        case .unknown: return FamilyPalette(light: 0x5B6573, dark: 0xA3ADBA, lightHighContrast: 0x434B57, darkHighContrast: 0xC2CAD4)
        }
    }
}

extension Color {
    /// A colour that follows light/dark appearance and Increase Contrast.
    static func grouperDynamic(_ palette: FamilyPalette) -> Color {
        #if os(iOS)
        return Color(uiColor: UIColor { traits in
            let dark = traits.userInterfaceStyle == .dark
            let high = traits.accessibilityContrast == .high
            let hex = dark ? (high ? palette.darkHighContrast : palette.dark) : (high ? palette.lightHighContrast : palette.light)
            return UIColor(grouperHex: hex)
        })
        #else
        return Color(nsColor: NSColor(name: nil) { appearance in
            let match = appearance.bestMatch(from: [.aqua, .darkAqua, .accessibilityHighContrastAqua, .accessibilityHighContrastDarkAqua])
            let hex: UInt32
            if match == .darkAqua { hex = palette.dark }
            else if match == .accessibilityHighContrastAqua { hex = palette.lightHighContrast }
            else if match == .accessibilityHighContrastDarkAqua { hex = palette.darkHighContrast }
            else { hex = palette.light }
            return NSColor(grouperHex: hex)
        })
        #endif
    }
}

#if os(iOS)
private extension UIColor {
    convenience init(grouperHex hex: UInt32) {
        self.init(red: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255, blue: CGFloat(hex & 0xFF) / 255, alpha: 1)
    }
}
#else
private extension NSColor {
    convenience init(grouperHex hex: UInt32) {
        self.init(srgbRed: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255, blue: CGFloat(hex & 0xFF) / 255, alpha: 1)
    }
}
#endif

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
        .background(.background, in: RoundedRectangle(cornerRadius: 5))
        .overlay {
            RoundedRectangle(cornerRadius: 5)
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
            Text(modelId.isEmpty ? "model not recorded" : modelId).font(.callout.monospaced()).lineLimit(1)
            if !note.isEmpty { Text(note).font(.caption).foregroundStyle(.secondary).lineLimit(1) }
            Image(systemName: "lock.fill").font(.caption2).foregroundStyle(.secondary)
        }
        .padding(.horizontal, 10).padding(.vertical, 5)
        .background(Color.grouperSecondaryBackground, in: Capsule())
        .overlay { Capsule().strokeBorder(family.color.opacity(0.6), lineWidth: 1) }
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

/// A small capsule label: Free, Paid, Key set, Local and so on.
struct TagBadge: View {
    let text: String
    var tint: Color = .secondary

    var body: some View {
        Text(text)
            .font(.caption2.weight(.semibold))
            .foregroundStyle(tint)
            .padding(.horizontal, 6).padding(.vertical, 2)
            .background(tint.opacity(0.14), in: Capsule())
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
