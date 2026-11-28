import SwiftUI
#if canImport(UIKit)
import UIKit
#elseif canImport(AppKit)
import AppKit
#endif

// The Grouper palette, radii and type scale (docs/gui-concept-plan.md section 2),
// mirroring the web client's tokens in crates/hub/web/app.css so the web and the
// apps share one look. Where the plan's hex values and app.css differ, app.css wins
// (W1 adjusted some for contrast). Every colour has four values: light, dark, and
// each under Increase Contrast, the same blocks app.css has.
//
// This file stands alone (SwiftUI and UIKit/AppKit only), so the widget extension
// compiles it too. Model-family colours are in ModelFamilyStyle.swift.

/// One colour token: light, dark, and both under Increase Contrast (0xRRGGBB).
struct ThemeSwatch: Sendable {
    let light: UInt32
    let dark: UInt32
    let lightHighContrast: UInt32
    let darkHighContrast: UInt32

    init(light: UInt32, dark: UInt32, lightHighContrast: UInt32, darkHighContrast: UInt32) {
        self.light = light
        self.dark = dark
        self.lightHighContrast = lightHighContrast
        self.darkHighContrast = darkHighContrast
    }

    /// The value for a scheme, for places that can't use a dynamic colour (widgets).
    func hex(dark isDark: Bool, highContrast: Bool) -> UInt32 {
        isDark ? (highContrast ? darkHighContrast : dark) : (highContrast ? lightHighContrast : light)
    }
}

enum Theme {
    // MARK: Surfaces (navy in dark mode: blue above red on every one)

    /// Main canvas behind the transcript.
    static let bg = swatch(0xF4F6F9, 0x10141D, 0xF2F4F7, 0x0A0D14)
    /// Thread panel, inspector.
    static let bgTint = swatch(0xEEF2F6, 0x13171F, 0xEBEFF4, 0x0C1018)
    /// Solid sidebar, title bar and tab bar (where the glass is made solid).
    static let glassSolid = swatch(0xE9EEF4, 0x141821, 0xE6EBF1, 0x0E121A)
    /// Cards, user bubble, composer, approval dock.
    static let surface = swatch(0xFFFFFF, 0x171C25, 0xFFFFFF, 0x121722)
    /// Chips, quiet buttons, code blocks.
    static let surface2 = swatch(0xEEF2F6, 0x1D222C, 0xE8ECF1, 0x1A202C)
    /// Hover, pressed.
    static let surface3 = swatch(0xE3E8EF, 0x20242D, 0xD9DFE7, 0x232A37)
    /// Selected row, model chips.
    static let surfaceSelect = swatch(0xE0E8F4, 0x1F2D41, 0xD4E0EF, 0x1C2C44)

    // MARK: Text, four tiers (each at least 4.5:1 on every surface above, 7:1 under Increase Contrast)

    static let text = swatch(0x0F1722, 0xF3F6F9, 0x05090F, 0xFFFFFF)
    /// Row labels, secondary copy. Light is one step darker than before (design-system 2.2).
    static let text2 = swatch(0x39434F, 0xC5CFDC, 0x222B38, 0xE1E7EF)
    /// Captions, sub-lines, activity lines. Light is one step darker than before.
    static let text3 = swatch(0x4C5663, 0xA4ADBB, 0x2E3846, 0xCDD5E0)
    /// Timestamps, placeholders, units, uppercase labels. Under Increase Contrast it equals `text3`.
    static let text4 = swatch(0x5A6471, 0x8C96A5, 0x2E3846, 0xCDD5E0)

    // MARK: Hairlines (design-system 2.3: one ink per theme at fixed alphas, written out as solids)

    /// Dividers inside a card, table rules (dark 6%, light 8%).
    static let line1 = swatch(0xECECED, 0x222831, 0x778190, 0x6B7584)
    /// Card and panel edges (dark 8%, light 14%). Today's `border` stays as the public name.
    static let line2 = swatch(0xDDDFE0, 0x262C36, 0x778190, 0x6B7584)
    /// Composer, dock, floating edges (dark 14%; light keeps `borderStrong`).
    static let line3 = swatch(0xC3CBD6, 0x323842, 0x4F5967, 0xA3ADBB)

    // MARK: Borders

    /// Card and divider hairline.
    static let border = swatch(0xDDE3EA, 0x272D36, 0x778190, 0x6B7584)
    /// Composer, dock, Deny outline.
    static let borderStrong = swatch(0xC3CBD6, 0x3D434D, 0x4F5967, 0xA3ADBB)
    /// Inputs that need a 3:1 boundary.
    static let borderControl = swatch(0x78828F, 0x656F7E, 0x4F5967, 0xA3ADBB)

    // MARK: Accent (teal is the one action colour)

    /// Text, icons, links, the selected tab.
    static let accent = Color.grouperDynamic(accentSwatch)
    static let accentSwatch = ThemeSwatch(light: 0x0B7372, dark: 0x22D3CE, lightHighContrast: 0x044E4D, darkHighContrast: 0x5CE6E1)
    /// New chat, selected tab tile.
    static let accentSoft = swatch(0xDDF3F2, 0x152A38, 0xD3EEED, 0x10303A)
    /// Approve, send, primary buttons. Always with `accentInk` text, never white (2.2:1).
    static let accentFill = swatch(0x12C4BF, 0x12C4BF, 0x12C4BF, 0x2AD4CF)
    /// The fill's 3:1 boundary on light surfaces.
    static let accentFillEdge = swatch(0x0B8A86, 0x12C4BF, 0x044E4D, 0x5CE6E1)
    /// Text on `accentFill`: 7.3:1.
    static let accentInk = swatch(0x06262A, 0x06262A, 0x031A1C, 0x031A1C)

    // MARK: Brand purple (V2, owner 26 Nov: coloured accents, not a blend of blues)

    /// The app icon's purple as a second accent for highlights: Needs you, the
    /// selected chat, the plan's step in progress. Text-safe: 6.1:1 on white,
    /// 6.7:1 on the dark canvas. Teal stays the one action colour.
    static let brand = Color.grouperDynamic(brandSwatch)
    static let brandSwatch = ThemeSwatch(light: 0x6D3FE0, dark: 0xA78BFA, lightHighContrast: 0x5530C4, darkHighContrast: 0xC4B0FF)
    /// Tinted background for a brand highlight (brand text on it: 5.1 / 5.8).
    static let brandSoft = swatch(0xEEE8FD, 0x241D3D, 0xE6DEFC, 0x2A2050)
    /// The icon's purple itself, for dots, bars and marks (graphics, 3:1 and up).
    static let brandFill = swatch(0x8B5CF6, 0x8B5CF6, 0x6D3FE0, 0xA78BFA)
    /// The icon's teal → blue → purple, for brand surfaces only (sign-in, empty
    /// states, the mark), never behind body text.
    static var brandGradient: LinearGradient {
        LinearGradient(colors: [Color(grouperHex: 0x12C4BF), Color(grouperHex: 0x3B82F6), Color(grouperHex: 0x8B5CF6)],
                       startPoint: .topLeading, endPoint: .bottomTrailing)
    }

    // MARK: Status (always with a word and an icon, never colour alone)

    /// Good state: running chip, Free, Local, done tick, online.
    static let ok = swatch(0x0B6B4F, 0x2EE6B0, 0x08503B, 0x6EF0C8)
    static let okSoft = swatch(0xD9F3E8, 0x102E27, 0xD3F0E3, 0x0C2E25)
    /// Waiting for a device, needs you.
    static let warn = swatch(0x8A5A00, 0xFBBF24, 0x6B4500, 0xFFD060)
    static let warnSoft = swatch(0xFBEFD0, 0x3D3016, 0xFAEBC6, 0x3A2C0C)
    /// Failed, destructive.
    static let danger = swatch(0xB42318, 0xF87171, 0x8F170E, 0xFF9C92)
    static let dangerSoft = swatch(0xFBE4E1, 0x3F1D22, 0xFAE2DF, 0x3E1A1E)
    /// Neutral notices: offline copy, stale reading. The blue hue under a second name; a tinted
    /// banner with an icon, never an outline (so it can't be mistaken for `focus`).
    static let info = swatch(0x1F5FCC, 0x7FB0FF, 0x164AA3, 0xA9C9FF)
    static let infoSoft = swatch(0xE7F0FC, 0x223048, 0xE7F0FC, 0x223048)
    /// Solid destructive buttons, with white text.
    static let dangerFill = swatch(0xB42318, 0xC9362B, 0x8F170E, 0xA8241A)
    /// Focus rings: blue, so focus never reads as the accent.
    static let focus = swatch(0x1A73E8, 0x7CB4FF, 0x0B57D0, 0x9AC7FF)

    // MARK: People

    /// The owner's initial disc.
    static let userAvatar = swatch(0x3F6FE0, 0x5986F4, 0x3F6FE0, 0x5986F4)
    /// The initial on the disc: white on the light disc (4.6:1), navy on the dark one (5.3:1).
    static let userAvatarInk = swatch(0xFFFFFF, 0x0F1722, 0xFFFFFF, 0x05090F)

    // MARK: Key colours (docs/usage-tab-spec.md 4.4)

    /// The ten colours an API key can wear on the Usage tab (swatch, donut segment, share bars),
    /// by the name the hub stores. Key colours are only ever fills, so they are step 3 (`solid`)
    /// of the identity-hue ramp (`--key-teal: var(--hue-teal-solid)` in app.css).
    static func keyColor(_ name: String?) -> Color {
        Color.grouperDynamic(keySwatch(name))
    }

    /// Grey: "Other", removed keys, and a key whose colour the hub has not sent.
    static let keyUnknown = Color.grouperDynamic(neutralSwatch)

    static func keySwatch(_ name: String?) -> ThemeSwatch {
        IdentityHue(name: name)?.solidSwatch ?? neutralSwatch
    }

    /// Outside the ramp: unknown models, "Other", removed keys.
    static let neutralSwatch = ThemeSwatch(light: 0x5B6573, dark: 0xA3ADBA, lightHighContrast: 0x434B57, darkHighContrast: 0xC2CAD4)

    /// How strongly a family colour tints a status pill over `surface` (app.css --fam-pill).
    static func familyPillOpacity(_ scheme: ColorScheme) -> Double { scheme == .dark ? 0.16 : 0.12 }

    // MARK: Radii

    enum Radius {
        /// Window, phone sheet, dialogs.
        static let lg: CGFloat = 16
        /// Cards, bubbles, composer, dock.
        static let card: CGFloat = 12
        /// Buttons, rows, model chips, status pills.
        static let sm: CGFloat = 8
        /// Badges.
        static let xs: CGFloat = 6
        /// The composer only: the roundest object on the screen (`--r-composer`).
        static let composer: CGFloat = 24
        /// Corner-radius value for full rounding (`--r-full`); for shapes prefer `Capsule` or `Circle`.
        static let full: CGFloat = 999
        /// Tab bar container (the system tab bar draws its own on iOS).
        static let tabBar: CGFloat = 18
    }

    // MARK: Sizes

    enum Size {
        /// Send button and avatars: full circles.
        static let avatar: CGFloat = 32
        static let familyDot: CGFloat = 10
        static let sidebarWidth: CGFloat = 240
        static let inspectorMin: CGFloat = 384
        static let inspectorIdeal: CGFloat = 400
        static let inspectorMax: CGFloat = 420
        /// The transcript, composer and dock line up at this width.
        static let transcriptMax: CGFloat = 720
        /// Approve / Deny height.
        static let dockButton: CGFloat = 44
    }

    // MARK: Type

    /// The web scale (px / weight) mapped onto text styles, so Dynamic Type still
    /// scales everything. On the phone body is one step up (16 on the web) to match iOS.
    enum Typeface {
        /// Brand wordmark: 18 / 600.
        static var wordmark: Font { .title3.weight(.semibold) }
        /// View and panel title: 15 / 600.
        static var title: Font { .headline }
        /// Body and bubbles: 15 / 400, line height 1.5.
        static var body: Font { .body }
        /// Sidebar and list rows: 13.5 / 500.
        static var row: Font { .subheadline.weight(.medium) }
        /// Section labels ("Chats", "Needs you"): 12.5 / 600.
        static var sectionLabel: Font { .footnote.weight(.semibold) }
        /// Chips and pills: 12.5 / 500.
        static var chip: Font { .footnote.weight(.medium) }
        /// Timestamps and meta: 12 / 400, in `text3`.
        static var meta: Font { .caption }
        /// Approval dock title: 17 / 600.
        static var dockTitle: Font { .title3.weight(.semibold) }
        /// Approval dock buttons: 15 / 600.
        static var dockButton: Font { .body.weight(.semibold) }
        /// Monospace (tool input, model ids where exact).
        static var mono: Font { .system(.callout, design: .monospaced) }
    }

    private static func swatch(_ light: UInt32, _ dark: UInt32, _ lightHC: UInt32, _ darkHC: UInt32) -> Color {
        Color.grouperDynamic(ThemeSwatch(light: light, dark: dark, lightHighContrast: lightHC, darkHighContrast: darkHC))
    }
}

/// The ten identity-hue ramps (design-system 2.6), by hue and not by model family, so the
/// mapping from family (later: provider) to hue lives in one table, `ModelFamily.identityHue`.
/// Four steps each: 1 `bg` (tinted pill and chip), 2 `border`, 3 `solid` (dots, marks, donut
/// segments, meter and share bars), 4 `text` (today's `--fam-*` value). Increase Contrast keeps
/// today's high-contrast values for `text` and uses them for `solid` too; `bg` and `border`
/// have no contrast variant.
enum IdentityHue: String, CaseIterable, Sendable {
    case teal, purple, orange, pink, blue, green, amber, rose, indigo, cyan

    /// From the colour name the hub stores for an API key (case-insensitive).
    init?(name: String?) {
        guard let name, let hue = IdentityHue(rawValue: name.lowercased()) else { return nil }
        self = hue
    }

    var bgSwatch: ThemeSwatch {
        ThemeSwatch(light: row.lightBg, dark: row.darkBg, lightHighContrast: row.lightBg, darkHighContrast: row.darkBg)
    }
    var borderSwatch: ThemeSwatch {
        ThemeSwatch(light: row.lightBorder, dark: row.darkBorder, lightHighContrast: row.lightBorder, darkHighContrast: row.darkBorder)
    }
    var solidSwatch: ThemeSwatch {
        ThemeSwatch(light: row.lightSolid, dark: row.darkSolid, lightHighContrast: row.lightHC, darkHighContrast: row.darkHC)
    }
    var textSwatch: ThemeSwatch {
        ThemeSwatch(light: row.lightText, dark: row.darkText, lightHighContrast: row.lightHC, darkHighContrast: row.darkHC)
    }

    /// Step 1: tinted pill and chip background.
    var bg: Color { Color.grouperDynamic(bgSwatch) }
    /// Step 2: pill outline.
    var border: Color { Color.grouperDynamic(borderSwatch) }
    /// Step 3: dots, family marks, donut segments, meter and share bars.
    var solid: Color { Color.grouperDynamic(solidSwatch) }
    /// Step 4: model name in a chip, pill text, glyph on a tinted mark.
    var text: Color { Color.grouperDynamic(textSwatch) }

    private struct Row {
        let darkBg, darkBorder, darkSolid, darkText: UInt32
        let lightBg, lightBorder, lightSolid, lightText: UInt32
        let lightHC, darkHC: UInt32
    }

    private var row: Row {
        switch self {
        case .teal: return Row(darkBg: 0x183940, darkBorder: 0x1A686B, darkSolid: 0x1DD0CB, darkText: 0x4FE3E6,
                               lightBg: 0xE2F3F2, lightBorder: 0x93D0CE, lightSolid: 0x0E9793, lightText: 0x0B6B73,
                               lightHC: 0x06525A, darkHC: 0x7FF0F2)
        case .purple: return Row(darkBg: 0x2B2646, darkBorder: 0x4D367C, darkSolid: 0x9759F5, darkText: 0xA78BFA,
                                 lightBg: 0xEFEAFC, lightBorder: 0xC4AFF5, lightSolid: 0x7C4DE8, lightText: 0x5B3FC4,
                                 lightHC: 0x47309F, darkHC: 0xC4B0FF)
        case .orange: return Row(darkBg: 0x3B322E, darkBorder: 0x76553E, darkSolid: 0xF8A360, darkText: 0xF9A85A,
                                 lightBg: 0xF9EEE5, lightBorder: 0xE7BE9E, lightSolid: 0xCA6F28, lightText: 0x9A4A06,
                                 lightHC: 0x7A3A04, darkHC: 0xFFC48A)
        case .pink: return Row(darkBg: 0x38293F, darkBorder: 0x6E3D69, darkSolid: 0xE66BC8, darkText: 0xF28AD8,
                               lightBg: 0xF9E9F5, lightBorder: 0xE7ADD8, lightSolid: 0xC948A8, lightText: 0xA12D86,
                               lightHC: 0x82216B, darkHC: 0xF8B0E6)
        case .blue: return Row(darkBg: 0x223048, darkBorder: 0x345181, darkSolid: 0x5B9BFF, darkText: 0x7FB0FF,
                               lightBg: 0xE7F0FC, lightBorder: 0xA7C5F4, lightSolid: 0x3B7FE6, lightText: 0x1F5FCC,
                               lightHC: 0x164AA3, darkHC: 0xA9C9FF)
        case .green: return Row(darkBg: 0x283726, darkBorder: 0x456329, darkSolid: 0x84C52E, darkText: 0x9FD24A,
                                lightBg: 0xEBF3E2, lightBorder: 0xB6D091, lightSolid: 0x5C970A, lightText: 0x436E00,
                                lightHC: 0x395C00, darkHC: 0xBFE57A)
        case .amber: return Row(darkBg: 0x373424, darkBorder: 0x6B5A23, darkSolid: 0xE0B020, darkText: 0xE8C04A,
                                lightBg: 0xF5F0E2, lightBorder: 0xDBC591, lightSolid: 0xAF7F0A, lightText: 0x805B00,
                                lightHC: 0x6A4B00, darkHC: 0xF5D77A)
        case .rose: return Row(darkBg: 0x3A2833, darkBorder: 0x743C4A, darkSolid: 0xF4677E, darkText: 0xFF8FA0,
                               lightBg: 0xFBE9EC, lightBorder: 0xEFAEB9, lightSolid: 0xDC4A64, lightText: 0xBD2F4A,
                               lightHC: 0x981F38, darkHC: 0xFFB3BF)
        case .indigo: return Row(darkBg: 0x252B45, darkBorder: 0x3C4379, darkSolid: 0x7078EE, darkText: 0x8891F0,
                                 lightBg: 0xECECFB, lightBorder: 0xB7B8EF, lightSolid: 0x5F62DC, lightText: 0x4D4FC9,
                                 lightHC: 0x3A3CA8, darkHC: 0xB0B6FF)
        case .cyan: return Row(darkBg: 0x1C3543, darkBorder: 0x255D73, darkSolid: 0x38B6DE, darkText: 0x5CC8E8,
                               lightBg: 0xE3F2F6, lightBorder: 0x94CDDD, lightSolid: 0x1290B4, lightText: 0x0A6F8C,
                               lightHC: 0x06566D, darkHC: 0x8ADCF2)
        }
    }
}

extension Color {
    /// A colour that follows light/dark appearance and Increase Contrast.
    static func grouperDynamic(_ swatch: ThemeSwatch) -> Color {
        #if os(iOS)
        return Color(uiColor: UIColor { traits in
            let dark = traits.userInterfaceStyle == .dark
            let high = traits.accessibilityContrast == .high
            return UIColor(grouperHex: swatch.hex(dark: dark, highContrast: high))
        })
        #else
        return Color(nsColor: NSColor(name: nil) { appearance in
            let match = appearance.bestMatch(from: [.aqua, .darkAqua, .accessibilityHighContrastAqua, .accessibilityHighContrastDarkAqua])
            let hex: UInt32
            if match == .darkAqua { hex = swatch.dark }
            else if match == .accessibilityHighContrastAqua { hex = swatch.lightHighContrast }
            else if match == .accessibilityHighContrastDarkAqua { hex = swatch.darkHighContrast }
            else { hex = swatch.light }
            return NSColor(grouperHex: hex)
        })
        #endif
    }

    /// A fixed sRGB colour, for places that resolve the scheme themselves (widgets).
    init(grouperHex hex: UInt32) {
        self.init(.sRGB,
                  red: Double((hex >> 16) & 0xFF) / 255,
                  green: Double((hex >> 8) & 0xFF) / 255,
                  blue: Double(hex & 0xFF) / 255,
                  opacity: 1)
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
