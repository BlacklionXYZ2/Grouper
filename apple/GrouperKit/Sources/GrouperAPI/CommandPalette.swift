import Foundation

/// Destinations the desktop command palette can open (docs/gui-spec.md 5.15). A
/// client lists only the ones it has.
public enum PalettePage: String, CaseIterable, Sendable {
    case needsYou, devices, models, usage, memory, skills, settings

    public var title: String {
        switch self {
        case .needsYou: return "Needs you"
        case .devices: return "Devices"
        case .models: return "Models"
        case .usage: return "Usage"
        case .memory: return "Memory"
        case .skills: return "Skills"
        case .settings: return "Settings"
        }
    }
}

/// Safe palette actions. None sends a message, answers an approval or changes a permission.
public enum PaletteAction: String, CaseIterable, Sendable {
    case newChat, newProjectChat, newStandardChat, switchTheme

    public var title: String {
        switch self {
        case .newChat: return "New chat"
        case .newProjectChat: return "New project chat"
        case .newStandardChat: return "New standard chat"
        case .switchTheme: return "Switch theme"
        }
    }

    public var subtitle: String {
        switch self {
        case .newChat: return "Start a chat; nothing is sent until you send it"
        case .newProjectChat: return "Start a chat that can hand work to your devices"
        case .newStandardChat: return "Start a chat with one model"
        case .switchTheme: return "Choose System, Light or Dark"
        }
    }
}

public enum PaletteTarget: Hashable, Sendable {
    case action(PaletteAction)
    case chat(id: String)
    case thread(id: String, conversationId: String)
    case model(id: String)
    case device(id: String)
    case page(PalettePage)
}

/// Result groups, in the order the palette shows them.
public enum PaletteKind: Int, CaseIterable, Comparable, Sendable {
    case action, chat, thread, model, device, page

    public var title: String {
        switch self {
        case .action: return "Actions"
        case .chat: return "Chats"
        case .thread: return "Threads"
        case .model: return "Models"
        case .device: return "Devices"
        case .page: return "Pages"
        }
    }

    public static func < (lhs: PaletteKind, rhs: PaletteKind) -> Bool { lhs.rawValue < rhs.rawValue }
}

public struct PaletteItem: Equatable, Identifiable, Sendable {
    public let target: PaletteTarget
    public let title: String
    /// The disambiguator shown under the title: chat kind, a thread's chat, device,
    /// model and status, a model's provider, a device's presence.
    public let subtitle: String
    /// Extra words a query may match besides the title (never transcript text or input).
    public let metadata: [String]

    public init(target: PaletteTarget, title: String, subtitle: String, metadata: [String] = []) {
        self.target = target
        self.title = title
        self.subtitle = subtitle
        self.metadata = metadata
    }

    public var kind: PaletteKind {
        switch target {
        case .action: return .action
        case .chat: return .chat
        case .thread: return .thread
        case .model: return .model
        case .device: return .device
        case .page: return .page
        }
    }

    /// Stable across store updates, so the selection survives new results.
    public var id: String {
        switch target {
        case .action(let action): return "action:\(action.rawValue)"
        case .chat(let id): return "chat:\(id)"
        case .thread(let id, _): return "thread:\(id)"
        case .model(let id): return "model:\(id)"
        case .device(let id): return "device:\(id)"
        case .page(let page): return "page:\(page.rawValue)"
        }
    }
}

public struct PaletteSection: Equatable, Identifiable, Sendable {
    public let title: String
    public let items: [PaletteItem]
    /// Matches before the per-group cap.
    public let total: Int
    public var id: String { title }
}

/// Local search for the command palette: chat, thread, model and device names the
/// client already holds, plus pages and actions. No server search.
public enum CommandPalette {
    public static let groupLimit = 8
    public static let recentLimit = 5

    public static func items(
        conversations: [ConversationInfo],
        threads: [ThreadInfo],
        models: [ModelEntry],
        devices: [DeviceInfo],
        actions: [PaletteAction],
        pages: [PalettePage]
    ) -> [PaletteItem] {
        let chatTitles = Dictionary(conversations.map { ($0.id, $0.title) }, uniquingKeysWith: { first, _ in first })
        let deviceNames = Dictionary(devices.map { ($0.id, $0.name) }, uniquingKeysWith: { first, _ in first })
        var items: [PaletteItem] = actions.map { PaletteItem(target: .action($0), title: $0.title, subtitle: $0.subtitle) }
        items += conversations.map { chat in
            let kind = chat.mode == "standard" ? "Standard chat" : "Project chat"
            return PaletteItem(target: .chat(id: chat.id), title: displayTitle(chat.title, fallback: "Untitled chat"), subtitle: kind,
                               metadata: [chat.mode == "standard" ? "standard" : "project"])
        }
        items += threads.map { thread in
            let chat = displayTitle(chatTitles[thread.conversationId] ?? "", fallback: "a chat")
            let device = deviceNames[thread.deviceId] ?? thread.deviceId
            let parts: [String?] = ["In \(chat)", device, thread.model.isEmpty ? nil : thread.model, statusLabel(thread.status)]
            return PaletteItem(target: .thread(id: thread.id, conversationId: thread.conversationId),
                               title: displayTitle(thread.title, fallback: "Thread"),
                               subtitle: parts.compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · "),
                               metadata: [chat, device, thread.model])
        }
        items += models.map { entry in
            PaletteItem(target: .model(id: entry.id), title: entry.id, subtitle: "\(entry.model) · \(entry.providerId)",
                        metadata: [entry.model, entry.providerId])
        }
        items += devices.map { device in
            PaletteItem(target: .device(id: device.id), title: displayTitle(device.name, fallback: device.id),
                        subtitle: "\(device.online ? "Online" : "Offline") · \(device.platform)",
                        metadata: [device.platform])
        }
        items += pages.map { PaletteItem(target: .page($0), title: $0.title, subtitle: "Open \($0.title)") }
        return items
    }

    /// Lowercased, accent-free, punctuation turned into spaces, single-spaced.
    public static func normalize(_ text: String) -> String {
        let folded = text.folding(options: [.caseInsensitive, .diacriticInsensitive], locale: nil).lowercased()
        var out = ""
        var pendingSpace = false
        for character in folded {
            if character.isLetter || character.isNumber {
                if pendingSpace && !out.isEmpty { out.append(" ") }
                pendingSpace = false
                out.append(character)
            } else {
                pendingSpace = true
            }
        }
        return out
    }

    public static func tokens(_ query: String) -> [String] {
        normalize(query).split(separator: " ").map(String.init)
    }

    /// Lower is better; nil means no match. Title exact or prefix, then every token
    /// starting a title word, then every token inside the title, then matches that
    /// need the metadata. Tokens may come in any order.
    public static func rank(_ item: PaletteItem, tokens: [String]) -> Int? {
        guard !tokens.isEmpty else { return nil }
        let title = normalize(item.title)
        let query = tokens.joined(separator: " ")
        if title == query { return 0 }
        if title.hasPrefix(query) { return 1 }
        let words = title.split(separator: " ")
        if tokens.allSatisfy({ token in words.contains { $0.hasPrefix(token) } }) { return 2 }
        if tokens.allSatisfy({ title.contains($0) }) { return 3 }
        let extra = ([title] + item.metadata.map(normalize)).joined(separator: " ")
        if tokens.allSatisfy({ extra.contains($0) }) { return 4 }
        return nil
    }

    /// The palette's result groups. An empty (or punctuation-only) query shows up to
    /// five recent items the owner opened, then actions and pages. A query shows
    /// Actions, Chats, Threads, Models, Devices, Pages, each capped at `limit`,
    /// ranked by match strength, then recency, then title.
    public static func sections(_ items: [PaletteItem], query: String, recents: [String], limit: Int = groupLimit) -> [PaletteSection] {
        let tokens = Self.tokens(query)
        let byId = Dictionary(items.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        if tokens.isEmpty {
            let recent: [PaletteItem] = Array(recents.compactMap { byId[$0] }.prefix(recentLimit))
            let recentIds = Set(recent.map(\.id))
            var sections: [PaletteSection] = []
            if !recent.isEmpty { sections.append(PaletteSection(title: "Recent", items: recent, total: recent.count)) }
            for kind in [PaletteKind.action, .page] {
                let rows = items.filter { $0.kind == kind && !recentIds.contains($0.id) }
                if !rows.isEmpty { sections.append(PaletteSection(title: kind.title, items: rows, total: rows.count)) }
            }
            return sections
        }
        let recency = Dictionary(recents.enumerated().map { ($1, $0) }, uniquingKeysWith: { first, _ in first })
        let scored: [(item: PaletteItem, rank: Int)] = items.compactMap { item in
            Self.rank(item, tokens: tokens).map { (item: item, rank: $0) }
        }
        return PaletteKind.allCases.compactMap { (kind: PaletteKind) -> PaletteSection? in
            let rows = scored.filter { $0.item.kind == kind }.sorted { lhs, rhs in
                if lhs.rank != rhs.rank { return lhs.rank < rhs.rank }
                let l = recency[lhs.item.id] ?? Int.max, r = recency[rhs.item.id] ?? Int.max
                if l != r { return l < r }
                let lt = normalize(lhs.item.title), rt = normalize(rhs.item.title)
                if lt != rt { return lt < rt }
                return lhs.item.id < rhs.item.id
            }
            guard !rows.isEmpty else { return nil }
            return PaletteSection(title: kind.title, items: rows.prefix(limit).map(\.item), total: rows.count)
        }
    }

    public static func flatten(_ sections: [PaletteSection]) -> [PaletteItem] { sections.flatMap(\.items) }

    /// Keeps the selection by id while results change; falls back to the first row.
    public static func resolveSelection(_ selection: String?, in items: [PaletteItem]) -> String? {
        if let selection, items.contains(where: { $0.id == selection }) { return selection }
        return items.first?.id
    }

    /// Up/Down: one row, stopping at the ends. Home/End: pass `Int.min` / `Int.max`.
    public static func move(_ selection: String?, by step: Int, in items: [PaletteItem]) -> String? {
        guard !items.isEmpty else { return nil }
        if step == Int.min { return items.first?.id }
        if step == Int.max { return items.last?.id }
        guard let current = resolveSelection(selection, in: items),
              let index = items.firstIndex(where: { $0.id == current }) else { return items.first?.id }
        return items[min(max(index + step, 0), items.count - 1)].id
    }

    /// Most recent first, no repeats, at most `recentLimit`.
    public static func remember(_ id: String, in recents: [String]) -> [String] {
        Array(([id] + recents.filter { $0 != id }).prefix(recentLimit))
    }

    static func displayTitle(_ title: String, fallback: String) -> String {
        let trimmed = title.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? fallback : trimmed
    }

    static func statusLabel(_ status: String) -> String {
        switch status {
        case "running": return "Running"
        case "waiting_device": return "Waiting for device"
        case "done": return "Done"
        case "failed": return "Failed"
        case "stopped": return "Stopped"
        default: return status.capitalized
        }
    }
}
