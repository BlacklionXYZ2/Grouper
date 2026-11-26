import SwiftUI
import GrouperAPI

/// The window's palette toggle, published by `MacRootView` for the menu command.
struct CommandPaletteKey: FocusedValueKey {
    typealias Value = Binding<Bool>
}

extension FocusedValues {
    var commandPalette: Binding<Bool>? {
        get { self[CommandPaletteKey.self] }
        set { self[CommandPaletteKey.self] = newValue }
    }
}

/// Edit > Search… (⌘K) opens the command palette over the focused main window.
struct PaletteCommands: Commands {
    @FocusedValue(\.commandPalette) private var palette

    var body: some Commands {
        CommandGroup(after: .textEditing) {
            Button("Search…") {
                if let palette { palette.wrappedValue.toggle() }
            }
            .keyboardShortcut("k", modifiers: .command)
            .disabled(palette == nil)
        }
    }
}

/// The Mac command palette (docs/gui-spec.md 5.15): a search field over the chats,
/// threads, models and devices the app already holds, plus pages and safe actions.
/// It jumps and starts new chats; it never sends a message, answers an approval or
/// changes a permission. No server search.
///
/// Keys: Up/Down move, Return opens the selected row, Escape closes. With an empty
/// query Return does nothing until a row has been chosen with the arrows.
struct CommandPaletteView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    /// Kept by the window, so closing and reopening keeps what was typed.
    @Binding var query: String
    let actions: [PaletteAction]
    let pages: [PalettePage]
    /// Opens the item; false when it no longer exists.
    let onActivate: (PaletteItem) -> Bool

    @State private var selection: String?
    @State private var movedByKeyboard = false
    @State private var unavailable = false
    @FocusState private var queryFocused: Bool

    var body: some View {
        let items: [PaletteItem] = CommandPalette.items(
            conversations: model.conversations, threads: model.threads, models: model.models,
            devices: model.devices, actions: actions, pages: pages
        )
        let sections: [PaletteSection] = CommandPalette.sections(items, query: query, recents: model.paletteRecents)
        let rows: [PaletteItem] = CommandPalette.flatten(sections)
        let hasQuery = !CommandPalette.tokens(query).isEmpty
        let selected: String? = (hasQuery || movedByKeyboard) ? CommandPalette.resolveSelection(selection, in: rows) : nil
        VStack(spacing: 0) {
            searchField(rows: rows, selected: selected)
            Divider()
            results(sections, rows: rows, selected: selected)
            Divider()
            footer(count: rows.count)
        }
        .frame(width: 640, height: 460)
        .onAppear { queryFocused = true }
        .onChange(of: query) { _, _ in
            unavailable = false
            movedByKeyboard = false
        }
    }

    private func searchField(rows: [PaletteItem], selected: String?) -> some View {
        HStack(spacing: 10) {
            Image(systemName: "magnifyingglass").foregroundStyle(Theme.text2)
            TextField("Search chats, threads, models, devices, pages, actions", text: $query)
                .textFieldStyle(.plain)
                .font(.title3)
                .focused($queryFocused)
                .onSubmit { activate(rows.first { $0.id == selected }) }
                .onKeyPress(.downArrow) {
                    move(selected == nil ? 0 : 1, from: selected, rows: rows)
                    return .handled
                }
                .onKeyPress(.upArrow) {
                    move(-1, from: selected, rows: rows)
                    return .handled
                }
                .accessibilityLabel("Search chats, threads, models, devices, pages and actions")
                .accessibilityValue(rows.isEmpty ? "No matches" : "\(rows.count) results")
            Text("⌘K").font(.caption.monospaced()).foregroundStyle(Theme.text2)
        }
        .padding(14)
    }

    private func results(_ sections: [PaletteSection], rows: [PaletteItem], selected: String?) -> some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 2) {
                    if unavailable {
                        Label("That item is no longer available.", systemImage: "exclamationmark.triangle")
                            .font(.callout).foregroundStyle(Theme.warn).padding(.horizontal, 8).padding(.vertical, 6)
                    }
                    if sections.isEmpty {
                        HStack {
                            Text("No matches").foregroundStyle(Theme.text2)
                            Button("Clear search") { query = "" }.buttonStyle(.borderless)
                        }
                        .padding(12)
                    }
                    ForEach(sections) { section in
                        Text(section.title)
                            .font(.caption.weight(.semibold)).foregroundStyle(Theme.text2)
                            .padding(.horizontal, 8).padding(.top, 10).padding(.bottom, 2)
                            .accessibilityAddTraits(.isHeader)
                        ForEach(section.items) { item in
                            row(item, isSelected: item.id == selected).id(item.id)
                        }
                        if section.total > section.items.count {
                            Text("\(section.total - section.items.count) more. Keep typing to narrow the list.")
                                .font(.caption).foregroundStyle(Theme.text2).padding(.horizontal, 8)
                        }
                    }
                }
                .padding(8)
            }
            .onChange(of: selected) { _, id in
                if let id { proxy.scrollTo(id) }
            }
        }
        .background(Theme.surface)
    }

    private func row(_ item: PaletteItem, isSelected: Bool) -> some View {
        Button { activate(item) } label: {
            HStack(spacing: 10) {
                Image(systemName: icon(item)).frame(width: 20).foregroundStyle(Theme.text2)
                VStack(alignment: .leading, spacing: 2) {
                    Text(item.title).lineLimit(1)
                    if !item.subtitle.isEmpty {
                        Text(item.subtitle).font(.caption).foregroundStyle(Theme.text2).lineLimit(1)
                    }
                }
                Spacer(minLength: 8)
                Text(kindLabel(item)).font(.caption).foregroundStyle(Theme.text2)
            }
            .padding(.horizontal, 8).padding(.vertical, 6)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(isSelected ? Theme.surfaceSelect : Color.clear, in: .rect(cornerRadius: Theme.Radius.sm))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel("\(kindLabel(item)): \(item.title). \(item.subtitle)")
        .accessibilityAddTraits(isSelected ? .isSelected : [])
    }

    private func footer(count: Int) -> some View {
        HStack {
            Text("↑↓ to move · Return to open · Esc to close").font(.caption).foregroundStyle(Theme.text2)
            Spacer()
            Button("Close") { dismiss() }.keyboardShortcut(.cancelAction)
        }
        .padding(.horizontal, 14).padding(.vertical, 8)
    }

    private func move(_ step: Int, from selected: String?, rows: [PaletteItem]) {
        movedByKeyboard = true
        selection = selected == nil ? rows.first?.id : CommandPalette.move(selected, by: step, in: rows)
    }

    private func activate(_ item: PaletteItem?) {
        guard let item else { return }
        if onActivate(item) {
            model.rememberRecent(item.id)
            query = ""
            dismiss()
        } else {
            unavailable = true
            AccessibilityNotification.Announcement("That item is no longer available").post()
        }
    }

    private func icon(_ item: PaletteItem) -> String {
        switch item.target {
        case .action: return "plus.bubble"
        case .chat: return "bubble.left.and.bubble.right"
        case .thread: return "point.3.connected.trianglepath.dotted"
        case .model: return "cpu"
        case .device: return "desktopcomputer"
        case .page(let page):
            switch page {
            case .needsYou: return "exclamationmark.circle"
            case .devices: return "desktopcomputer"
            case .models: return "cpu"
            case .usage: return "chart.bar"
            case .memory: return "brain"
            case .skills: return "sparkles"
            case .settings: return "gear"
            }
        }
    }

    private func kindLabel(_ item: PaletteItem) -> String {
        switch item.kind {
        case .action: return "Action"
        case .chat: return "Chat"
        case .thread: return "Thread"
        case .model: return "Model"
        case .device: return "Device"
        case .page: return "Page"
        }
    }
}

/// Models, read-only, grouped by provider: where a palette model result lands. Model
/// changes stay in the chat's model picker.
struct ModelsListView: View {
    @EnvironmentObject private var model: AppModel
    let highlighted: String?

    private struct ProviderGroup: Identifiable {
        let id: String
        let models: [ModelEntry]
    }

    var body: some View {
        let groups: [ProviderGroup] = Dictionary(grouping: model.models, by: { $0.providerId })
            .map { ProviderGroup(id: $0.key, models: $0.value.sorted { $0.id < $1.id }) }
            .sorted { $0.id < $1.id }
        ScrollViewReader { proxy in
            List {
                ForEach(groups) { group in
                    Section(header: Text(providerTitle(group.id))) {
                        ForEach(group.models) { entry in
                            row(entry).id(entry.id)
                        }
                    }
                }
            }
            .overlay {
                if model.models.isEmpty {
                    ContentUnavailableView("No models", systemImage: "cpu", description: Text("The hub's registry is empty or still loading."))
                }
            }
            .onChange(of: highlighted, initial: true) { _, id in
                guard let id else { return }
                Task {
                    try? await Task.sleep(for: .milliseconds(150))
                    proxy.scrollTo(id, anchor: .center)
                }
            }
        }
        .navigationTitle("Models")
        .task { await model.loadProviders() }
    }

    private func providerTitle(_ id: String) -> String {
        guard let provider = model.providers.first(where: { $0.id == id }) else { return id }
        return "\(id) · \(provider.kind) · \(provider.hasKey ? "key set" : "no key")"
    }

    private func row(_ entry: ModelEntry) -> some View {
        HStack(spacing: 8) {
            FamilyMark(family: ModelFamily.of(entry))
            VStack(alignment: .leading, spacing: 2) {
                Text(entry.id).font(.body.monospaced())
                Text(entry.roles.isEmpty ? entry.model : "\(entry.model) · \(entry.roles.joined(separator: ", "))")
                    .font(.caption).foregroundStyle(Theme.text2)
            }
            Spacer()
        }
        .padding(.vertical, 2)
        .listRowBackground(entry.id == highlighted ? Theme.surfaceSelect : nil)
    }
}
