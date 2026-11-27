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

    // MARK: Text (each at least 4.5:1 on every surface above, 7:1 under Increase Contrast)

    static let text = swatch(0x0F1722, 0xF3F6F9, 0x05090F, 0xFFFFFF)
    static let text2 = swatch(0x46505E, 0xC5CFDC, 0x222B38, 0xE1E7EF)
    /// Timestamps, placeholders, meta.
    static let text3 = swatch(0x5A6471, 0xA4ADBB, 0x2E3846, 0xCDD5E0)

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
    /// by the name the hub stores. They reuse the family hues (`--key-teal: var(--fam-gemini)` and
    /// so on in app.css), so light, dark and Increase Contrast values are already defined.
    /// Names: teal (gemini), purple (qwen), orange (llama), pink (claude), blue (gpt), green
    /// (nemotron), amber (ernie), rose (mistral), indigo (radium), cyan (deepseek).
    static func keyColor(_ name: String?) -> Color {
        Color.grouperDynamic(keySwatch(name))
    }

    /// Grey: "Other", removed keys, and a key whose colour the hub has not sent.
    static let keyUnknown = Color.grouperDynamic(keySwatch(nil))

    static func keySwatch(_ name: String?) -> ThemeSwatch {
        switch name?.lowercased() {
        case "teal": return ThemeSwatch(light: 0x0B6B73, dark: 0x4FE3E6, lightHighContrast: 0x06525A, darkHighContrast: 0x7FF0F2)
        case "purple": return ThemeSwatch(light: 0x5B3FC4, dark: 0xA78BFA, lightHighContrast: 0x47309F, darkHighContrast: 0xC4B0FF)
        case "orange": return ThemeSwatch(light: 0x9A4A06, dark: 0xF9A85A, lightHighContrast: 0x7A3A04, darkHighContrast: 0xFFC48A)
        case "pink": return ThemeSwatch(light: 0xA12D86, dark: 0xF28AD8, lightHighContrast: 0x82216B, darkHighContrast: 0xF8B0E6)
        case "blue": return ThemeSwatch(light: 0x1F5FCC, dark: 0x7FB0FF, lightHighContrast: 0x164AA3, darkHighContrast: 0xA9C9FF)
        case "green": return ThemeSwatch(light: 0x436E00, dark: 0x9FD24A, lightHighContrast: 0x395C00, darkHighContrast: 0xBFE57A)
        case "amber": return ThemeSwatch(light: 0x805B00, dark: 0xE8C04A, lightHighContrast: 0x6A4B00, darkHighContrast: 0xF5D77A)
        case "rose": return ThemeSwatch(light: 0xBD2F4A, dark: 0xFF8FA0, lightHighContrast: 0x981F38, darkHighContrast: 0xFFB3BF)
        case "indigo": return ThemeSwatch(light: 0x4D4FC9, dark: 0x8891F0, lightHighContrast: 0x3A3CA8, darkHighContrast: 0xB0B6FF)
        case "cyan": return ThemeSwatch(light: 0x0A6F8C, dark: 0x5CC8E8, lightHighContrast: 0x06566D, darkHighContrast: 0x8ADCF2)
        default: return ThemeSwatch(light: 0x5B6573, dark: 0xA3ADBA, lightHighContrast: 0x434B57, darkHighContrast: 0xC2CAD4)
        }
    }

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
