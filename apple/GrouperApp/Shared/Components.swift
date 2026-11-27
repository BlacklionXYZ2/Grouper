import SwiftUI
import GrouperAPI

// Shared pieces in the concept-art look (docs/gui-concept-plan.md), built on Theme
// tokens: cards, status pills, model tags, avatars, the threads card and its rows.

/// Navigation chrome may be glass (the composer card); content never is. With
/// Reduce Transparency the glass becomes the solid surface.
struct GlassSurface<Content: View>: View {
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency
    var cornerRadius: CGFloat = Theme.Radius.card
    let content: Content

    init(cornerRadius: CGFloat = Theme.Radius.card, @ViewBuilder content: () -> Content) {
        self.cornerRadius = cornerRadius
        self.content = content()
    }

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: cornerRadius)
        Group {
            if reduceTransparency {
                content.background(Theme.surface, in: shape)
            } else {
                content.glassEffect(.regular, in: shape)
            }
        }
        .overlay { shape.strokeBorder(Theme.borderStrong, lineWidth: 1) }
    }
}

extension View {
    /// An opaque card: surface, hairline, 12pt corners. No shadow (depth by tone).
    func grouperCard(padding: CGFloat = 12, fill: Color = Theme.surface, stroke: Color = Theme.border) -> some View {
        self.padding(padding)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(fill, in: RoundedRectangle(cornerRadius: Theme.Radius.card))
            .overlay { RoundedRectangle(cornerRadius: Theme.Radius.card).strokeBorder(stroke, lineWidth: 1) }
    }

    /// Lists and forms on the navy (or cool light) canvas instead of system grey.
    func grouperListBackground() -> some View {
        self.scrollContentBackground(.hidden).background(Theme.bg)
    }

    func grouperMinimumControlSize() -> some View {
        self.frame(minWidth: 44, minHeight: 44)
    }
}

struct ErrorBanner: View {
    let message: String
    var body: some View {
        Label(message, systemImage: "exclamationmark.triangle.fill")
            .font(.callout).foregroundStyle(Theme.danger)
            .grouperCard(fill: Theme.dangerSoft, stroke: Theme.dangerSoft)
            .accessibilityElement(children: .combine)
    }
}

// MARK: - Status

/// A thread's status as a rounded-rect pill: word plus icon, never colour alone.
/// A Running pill is tinted by the model family (Q4); waiting, failed and stopped
/// keep fixed status colours, so an orange family never reads as a warning.
struct StatusPill: View {
    @Environment(\.colorScheme) private var scheme
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    let status: String
    var family: ModelFamily? = nil
    var needsApproval = false

    var body: some View {
        let style = StatusStyle(status: status, needsApproval: needsApproval)
        let running = status == "running" && !needsApproval
        let ink: Color = running ? (family?.color ?? Theme.accent) : style.ink
        HStack(spacing: 5) {
            if running {
                Image(systemName: "circle.fill")
                    .font(.system(size: 7))
                    .symbolEffect(.pulse, isActive: !reduceMotion)
            } else {
                Image(systemName: style.icon).font(.caption2.weight(.bold))
            }
            Text(style.word)
        }
        .font(Theme.Typeface.chip)
        .foregroundStyle(ink)
        .lineLimit(1)
        .padding(.leading, 7).padding(.trailing, 8).padding(.vertical, 2)
        .frame(minHeight: 22)
        .background { fill(running: running, ink: ink, style: style) }
        .fixedSize()
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(style.word)
    }

    @ViewBuilder private func fill(running: Bool, ink: Color, style: StatusStyle) -> some View {
        let shape = RoundedRectangle(cornerRadius: Theme.Radius.sm)
        if running && family != nil {
            ZStack {
                shape.fill(Theme.surface)
                shape.fill(ink.opacity(Theme.familyPillOpacity(scheme)))
            }
        } else {
            shape.fill(running ? Theme.accentSoft : style.fill)
        }
    }
}

/// Word, icon and tone of a status (the web client's `threadStatus`).
struct StatusStyle {
    let word: String
    let icon: String
    let ink: Color
    let fill: Color

    init(status: String, needsApproval: Bool = false) {
        let style = Self.lookup(status: status, needsApproval: needsApproval)
        word = style.word
        icon = style.icon
        ink = style.ink
        fill = style.fill
    }

    private static func lookup(status: String, needsApproval: Bool) -> (word: String, icon: String, ink: Color, fill: Color) {
        if needsApproval { return ("Needs approval", "exclamationmark.shield.fill", Theme.warn, Theme.warnSoft) }
        switch status {
        case "running": return ("Running", "circle.fill", Theme.accent, Theme.accentSoft)
        case "waiting_device": return ("Waiting", "wifi.slash", Theme.warn, Theme.warnSoft)
        case "done": return ("Done", "checkmark", Theme.text2, Theme.surface2)
        case "failed": return ("Failed", "xmark", Theme.danger, Theme.dangerSoft)
        case "stopped": return ("Stopped", "stop.fill", Theme.text2, Theme.surface2)
        default: return (status.isEmpty ? "Unknown" : status.capitalized, "circle", Theme.text2, Theme.surface2)
        }
    }
}

/// The model's family colour as a small dot. Decorative: the model is named in text beside it.
struct FamilyDot: View {
    let family: ModelFamily
    var body: some View {
        Circle().fill(family.solidColor)
            .frame(width: Theme.Size.familyDot, height: Theme.Size.familyDot)
            .accessibilityHidden(true)
    }
}

/// A model's name as a chip: the text is the fact, the colour sits beside it (V6).
struct ModelTag: View {
    let name: String
    var body: some View {
        Text(name.isEmpty ? "model not recorded" : name)
            .font(Theme.Typeface.chip)
            .foregroundStyle(Theme.text2)
            .lineLimit(1)
            .truncationMode(.middle)
            .padding(.horizontal, 8).padding(.vertical, 2)
            .frame(minHeight: 24)
            .background(Theme.surfaceSelect, in: RoundedRectangle(cornerRadius: Theme.Radius.sm))
    }
}

// MARK: - Avatars

/// The owner's initial on a blue disc (a person symbol when no name is known).
struct UserAvatar: View {
    let initial: String?
    var body: some View {
        ZStack {
            Circle().fill(Theme.userAvatar)
            if let initial, !initial.isEmpty {
                Text(initial).font(.subheadline.weight(.semibold))
            } else {
                Image(systemName: "person.fill").font(.footnote)
            }
        }
        .foregroundStyle(Theme.userAvatarInk)
        .frame(width: Theme.Size.avatar, height: Theme.Size.avatar)
        .accessibilityHidden(true)
    }
}

/// Grouper's avatar on the icon's teal→blue→purple gradient. The fish symbol is a
/// stand-in until the owner's vector mark (Q1) is in the asset catalogue.
struct GrouperAvatar: View {
    var size: CGFloat = Theme.Size.avatar
    var body: some View {
        ZStack {
            Circle().fill(Theme.brandGradient)
            Image(systemName: "fish.fill")
                .font(.system(size: size * 0.42, weight: .semibold))
                .foregroundStyle(Theme.accentInk)
        }
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }
}

// MARK: - Buttons

/// Approve / Deny and other full-width actions. Primary is the teal fill with dark
/// ink (never white on teal); outline is the Deny style. A disabled primary is
/// visibly off: quiet surface, muted text, plain hairline.
struct GrouperActionButtonStyle: ButtonStyle {
    enum Kind { case primary, outline }
    let kind: Kind
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        let shape = RoundedRectangle(cornerRadius: Theme.Radius.sm)
        return configuration.label
            .font(Theme.Typeface.dockButton)
            .foregroundStyle(foreground)
            .frame(maxWidth: .infinity, minHeight: Theme.Size.dockButton)
            .padding(.horizontal, 12)
            .background(shape.fill(fill(pressed: configuration.isPressed)))
            .overlay { shape.strokeBorder(stroke, lineWidth: 1) }
            .contentShape(shape)
            .opacity(configuration.isPressed ? 0.85 : 1)
    }

    private var foreground: Color {
        guard isEnabled else { return Theme.text3 }
        return kind == .primary ? Theme.accentInk : Theme.text
    }

    private func fill(pressed: Bool) -> Color {
        guard isEnabled else { return Theme.surface2 }
        switch kind {
        case .primary: return Theme.accentFill
        case .outline: return pressed ? Theme.surface3 : Theme.surface
        }
    }

    private var stroke: Color {
        guard isEnabled else { return Theme.border }
        return kind == .primary ? Theme.accentFillEdge : Theme.borderStrong
    }
}

/// The round teal send button (32pt circle inside a 44pt target).
struct SendButtonStyle: ButtonStyle {
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.subheadline.weight(.bold))
            .foregroundStyle(isEnabled ? Theme.accentInk : Theme.text3)
            .frame(width: Theme.Size.avatar, height: Theme.Size.avatar)
            .background(Circle().fill(isEnabled ? Theme.accentFill : Theme.surface3))
            .opacity(configuration.isPressed ? 0.8 : 1)
            .frame(minWidth: 44, minHeight: 44)
            .contentShape(Rectangle())
    }
}

/// A row inside a card: pressed state on surface-3, rounded like the row token.
struct CardRowButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(
                RoundedRectangle(cornerRadius: Theme.Radius.sm)
                    .fill(configuration.isPressed ? Theme.surface3 : Color.clear)
            )
            .contentShape(Rectangle())
    }
}

// MARK: - Threads

/// One thread as a row: family dot, title, model tag, status pill, chevron. Falls
/// back to two lines (model tag under the title) when one line doesn't fit.
struct ThreadLine<Detail: View>: View {
    let title: String
    let modelId: String
    let family: ModelFamily
    let status: String
    var needsApproval = false
    var showsChevron = true
    let detail: Detail

    init(title: String, modelId: String, family: ModelFamily, status: String, needsApproval: Bool = false,
         showsChevron: Bool = true, @ViewBuilder detail: () -> Detail) {
        self.title = title
        self.modelId = modelId
        self.family = family
        self.status = status
        self.needsApproval = needsApproval
        self.showsChevron = showsChevron
        self.detail = detail()
    }

    var body: some View {
        ViewThatFits(in: .horizontal) {
            HStack(spacing: 12) {
                FamilyDot(family: family)
                titleBlock(withTag: false)
                Spacer(minLength: 8)
                ModelTag(name: modelId)
                trailing
            }
            HStack(spacing: 10) {
                FamilyDot(family: family)
                titleBlock(withTag: true)
                Spacer(minLength: 6)
                trailing
            }
        }
        .foregroundStyle(Theme.text)
    }

    private func titleBlock(withTag: Bool) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(title.isEmpty ? "Thread" : title)
                .font(Theme.Typeface.row)
                .lineLimit(withTag ? 2 : 1)
            if withTag { ModelTag(name: modelId) }
            detail
        }
    }

    private var trailing: some View {
        HStack(spacing: 8) {
            StatusPill(status: status, family: family, needsApproval: needsApproval)
            if showsChevron {
                Image(systemName: "chevron.right").font(.caption.weight(.semibold)).foregroundStyle(Theme.text3)
                    .accessibilityHidden(true)
            }
        }
    }
}

extension ThreadLine where Detail == EmptyView {
    init(title: String, modelId: String, family: ModelFamily, status: String, needsApproval: Bool = false, showsChevron: Bool = true) {
        self.init(title: title, modelId: modelId, family: family, status: status, needsApproval: needsApproval,
                  showsChevron: showsChevron) { EmptyView() }
    }
}

/// One delegation burst in the transcript (V5): "N threads running", a row per
/// thread, then "View details" for the threads panel. Device and live output live
/// in the panel; each row's spoken label still names title, status and device.
struct ThreadsCard: View {
    @EnvironmentObject private var model: AppModel
    let cards: [ThreadCard]
    let onOpen: (String) -> Void
    let onDetails: () -> Void

    #if os(iOS)
    private static let rowHeight: CGFloat = 44
    #else
    private static let rowHeight: CGFloat = 34
    #endif

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 8) {
                Image(systemName: "point.3.connected.trianglepath.dotted").foregroundStyle(Theme.accent)
                    .accessibilityHidden(true)
                Text(Self.summary(cards)).font(Theme.Typeface.row).foregroundStyle(Theme.text)
            }
            .padding(.horizontal, 8).padding(.top, 6).padding(.bottom, 4)
            .accessibilityElement(children: .combine)
            .accessibilityAddTraits(.isHeader)
            ForEach(cards) { card in row(card) }
            Rectangle().fill(Theme.border).frame(height: 1).padding(.top, 2)
            Button(action: onDetails) {
                HStack(spacing: 6) {
                    Text("View details")
                    Spacer()
                    Image(systemName: "chevron.right").accessibilityHidden(true)
                }
                .font(Theme.Typeface.chip)
                .foregroundStyle(Theme.text3)
                .padding(.horizontal, 8).padding(.vertical, 6)
                .frame(minHeight: Self.rowHeight)
            }
            .buttonStyle(CardRowButtonStyle())
            .accessibilityHint("Opens the threads panel for this chat")
        }
        .grouperCard(padding: 6)
    }

    private func row(_ card: ThreadCard) -> some View {
        let family = model.family(ofModel: card.model)
        let needs = card.pendingApprovals > 0
        let title = card.title.isEmpty ? "Thread" : card.title
        let word = StatusStyle(status: card.status, needsApproval: needs).word
        return Button { onOpen(card.threadId) } label: {
            ThreadLine(title: title, modelId: card.model, family: family, status: card.status, needsApproval: needs)
                .padding(.horizontal, 8).padding(.vertical, 5)
                .frame(minHeight: Self.rowHeight)
        }
        .buttonStyle(CardRowButtonStyle())
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(title), \(word), on \(model.deviceDisplayName(card.deviceId)), \(card.model.isEmpty ? "model not recorded" : card.model)")
        .accessibilityAddTraits(.isButton)
        .accessibilityHint("Opens the thread")
    }

    /// "3 threads running", or "2 running · 1 done" when they differ.
    static func summary(_ cards: [ThreadCard]) -> String {
        let n = cards.count
        func count(_ status: String) -> Int { cards.filter { $0.pendingApprovals == 0 && $0.status == status }.count }
        let needs = cards.filter { $0.pendingApprovals > 0 }.count
        let parts: [(count: Int, one: String, many: String)] = [
            (needs, "needs you", "need you"),
            (count("running"), "running", "running"),
            (count("waiting_device"), "waiting", "waiting"),
            (count("done"), "done", "done"),
            (count("failed"), "failed", "failed"),
            (count("stopped"), "stopped", "stopped"),
        ].filter { $0.count > 0 }
        let noun = n == 1 ? "thread" : "threads"
        guard let first = parts.first else { return "\(n) \(noun)" }
        if parts.count == 1 { return "\(n) \(noun) \(n == 1 ? first.one : first.many)" }
        return parts.map { "\($0.count) \($0.count == 1 ? $0.one : $0.many)" }.joined(separator: " · ")
    }
}

extension View {
    /// The keyboard for a chat's composer (owner, 26 Nov): a tap on the transcript dismisses it,
    /// but scrolling never does. The composer is multi-line, so Return adds a line instead of
    /// closing the keyboard, and without this nothing else would. iOS only: on the Mac, clicking
    /// the transcript (to select text) keeps the composer's focus as before.
    func dismissesKeyboardOnTap(_ focus: FocusState<Bool>.Binding) -> some View {
        #if os(iOS)
        return self
            .scrollDismissesKeyboard(.never)
            .simultaneousGesture(TapGesture().onEnded { focus.wrappedValue = false })
        #else
        return self
        #endif
    }
}
