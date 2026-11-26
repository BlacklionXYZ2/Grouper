import SwiftUI
import GrouperAPI

/// The chat's model picker: a covering panel (a full-height sheet on iPhone, a
/// large sheet on Mac), because there are many models. "Default model" comes
/// first; then the models with this chat's role, then "Other models", each grouped
/// under its provider (id, kind, key set / no key from `GET /v1/providers`). A model
/// reached through two providers is two entries, one under each.
struct ModelPickerSheet: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    /// "orchestrator" for project chats, "standard" for standard chats.
    let role: String
    /// The chosen registry id; nil means the default.
    let current: String?
    let onPick: (String?) -> Void
    @State private var query = ""
    #if os(macOS)
    @FocusState private var searchFocused: Bool
    #endif

    var body: some View {
        NavigationStack {
            let sections = Self.sections(models: model.models, providers: model.providers, role: role,
                                          words: Self.words(query), familyOf: { ModelFamily.of($0) })
            List {
                #if os(macOS)
                Section { searchField }
                #endif
                Section {
                    Text(currentLine).font(.caption).foregroundStyle(Theme.text2)
                    defaultRow
                }
                if model.providersFailed && model.providers.isEmpty {
                    Section {
                        Label("Provider details (kind, key) could not be loaded. Models are still grouped by provider id.", systemImage: "exclamationmark.triangle")
                            .font(.caption).foregroundStyle(Theme.text2)
                    }
                }
                ForEach(sections) { section in
                    ForEach(section.groups) { group in
                        Section {
                            ForEach(group.models) { entry in modelRow(entry) }
                        } header: {
                            groupHeader(section: section, group: group)
                        }
                    }
                }
                if sections.isEmpty && !query.trimmingCharacters(in: .whitespaces).isEmpty {
                    Section {
                        Text("No models match “\(query.trimmingCharacters(in: .whitespaces))”. Default model is still available.")
                            .foregroundStyle(Theme.text2)
                    }
                }
            }
            #if os(iOS)
            // Not activated on open: active search mode hides the title and Cancel, and
            // its own Cancel would only leave search instead of closing the picker.
            .searchable(text: $query, placement: .navigationBarDrawer(displayMode: .always), prompt: "Search by name, provider, family or role")
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .navigationTitle(role == "standard" ? "Standard model" : "Orchestrator model")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
            }
            .task { await model.loadProviders() }
            #if os(macOS)
            .onAppear { searchFocused = true }
            #endif
        }
        #if os(iOS)
        .presentationDetents([.large])
        #else
        .frame(minWidth: 600, idealWidth: 680, minHeight: 560, idealHeight: 760)
        #endif
    }

    #if os(macOS)
    // VERIFY ON MAC: `.searchable` inside a sheet's NavigationStack may not show a field on macOS, so the Mac uses a plain focused field.
    private var searchField: some View {
        HStack {
            Image(systemName: "magnifyingglass").foregroundStyle(Theme.text2)
            TextField("Search by name, provider, family or role", text: $query)
                .textFieldStyle(.plain)
                .focused($searchFocused)
        }
    }
    #endif

    private var currentLine: String {
        if let current { return "Current: \(current)" }
        if let fallback = roleDefault { return "Current: Default model (uses \(fallback.id))" }
        return "Current: Default model"
    }

    private var roleDefault: ModelEntry? { model.models.first { $0.roles.contains(role) } }

    private var defaultRow: some View {
        Button { pick(nil) } label: {
            HStack(spacing: 10) {
                Image(systemName: "sparkles").frame(minWidth: 22)
                VStack(alignment: .leading, spacing: 2) {
                    Text("Default model").font(.body.weight(.semibold)).foregroundStyle(.primary)
                    Text(roleDefault.map { "The \(role) default, now \($0.id)" } ?? "The hub's \(role) default")
                        .font(.caption).foregroundStyle(Theme.text2)
                }
                Spacer(minLength: 8)
                if current == nil { Image(systemName: "checkmark").foregroundStyle(Theme.accent).accessibilityHidden(true) }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .grouperMinimumControlSize()
        .accessibilityAddTraits(current == nil ? .isSelected : [])
    }

    private func groupHeader(section: PickerSection, group: PickerGroup) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            if group.isFirst {
                Text(section.title).font(.headline).foregroundStyle(.primary)
            }
            HStack(spacing: 6) {
                Text(group.providerId.isEmpty ? "Unknown provider" : group.providerId).font(.subheadline.weight(.semibold))
                if let provider = group.provider {
                    TagBadge(text: provider.kind)
                    TagBadge(text: provider.hasKey ? "Key set" : "No key", tint: provider.hasKey ? Theme.ok : Theme.text2, fill: provider.hasKey ? Theme.okSoft : Theme.surface2)
                    if let via = provider.viaDevice, !via.isEmpty {
                        TagBadge(text: "via \(model.deviceDisplayName(via))")
                    }
                } else if model.providersLoaded {
                    TagBadge(text: "Unknown provider", tint: Theme.warn, fill: Theme.warnSoft)
                }
            }
        }
        .textCase(nil)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isHeader)
    }

    private func modelRow(_ entry: ModelEntry) -> some View {
        let family = ModelFamily.of(entry)
        let selected = entry.id == current
        return Button { pick(entry.id) } label: {
            HStack(alignment: .top, spacing: 10) {
                FamilyMark(family: family).padding(.top, 2)
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 6) {
                        Text(entry.id).font(.body.weight(.semibold)).foregroundStyle(.primary).lineLimit(1)
                        priceBadge(entry)
                        if entry.local { TagBadge(text: "Local", tint: Theme.ok, fill: Theme.okSoft) }
                    }
                    Text(entry.model).font(.caption.monospaced()).foregroundStyle(Theme.text2).lineLimit(1)
                    Text(Self.meta(entry, family: family)).font(.caption).foregroundStyle(Theme.text2)
                }
                Spacer(minLength: 8)
                if selected { Image(systemName: "checkmark").foregroundStyle(Theme.accent).accessibilityHidden(true) }
            }
            .padding(.leading, 6)
            .background(alignment: .leading) {
                RoundedRectangle(cornerRadius: 2).fill(family.color).frame(width: 4)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }

    @ViewBuilder private func priceBadge(_ entry: ModelEntry) -> some View {
        if entry.free {
            TagBadge(text: "Free", tint: Theme.ok, fill: Theme.okSoft)
        } else if entry.inputPrice > 0 || entry.outputPrice > 0 {
            TagBadge(text: "Paid")
        } else {
            TagBadge(text: "No price set")
        }
    }

    private func pick(_ id: String?) {
        onPick(id)
        dismiss()
    }

    // MARK: - Grouping and search (the web picker's rules)

    struct PickerGroup: Identifiable {
        let id: String
        let providerId: String
        let provider: ProviderInfo?
        let models: [ModelEntry]
        let isFirst: Bool
    }

    struct PickerSection: Identifiable {
        let id: String
        let title: String
        let groups: [PickerGroup]
    }

    /// Family, roles, price per 1M tokens and context, for the row's secondary line.
    static func meta(_ entry: ModelEntry, family: ModelFamily) -> String {
        var parts: [String] = []
        if family != .unknown { parts.append("\(family.label) family") }
        parts.append(entry.roles.isEmpty ? "No roles" : "Roles: \(entry.roles.joined(separator: ", "))")
        if !entry.free && (entry.inputPrice > 0 || entry.outputPrice > 0) {
            parts.append("$\(price(entry.inputPrice)) in · $\(price(entry.outputPrice)) out per 1M")
        }
        if entry.contextWindow > 0 {
            parts.append("\(entry.contextWindow.formatted(.number.notation(.compactName))) context")
        }
        return parts.joined(separator: " · ")
    }

    static func price(_ value: Double) -> String {
        String(format: value > 0 && value < 0.1 ? "%.3f" : "%.2f", value)
    }

    static func words(_ query: String) -> [String] {
        query.lowercased().split(whereSeparator: \.isWhitespace).map(String.init)
    }

    static func sections(models: [ModelEntry], providers: [ProviderInfo], role: String, words: [String],
                         familyOf: (ModelEntry) -> ModelFamily) -> [PickerSection] {
        let byId = Dictionary(providers.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        let shown = models.filter { entry in
            guard !words.isEmpty else { return true }
            let family = familyOf(entry)
            let familyWords: String = family == .unknown ? "" : "\(family.label) \(family.maker)"
            let kind: String = byId[entry.providerId]?.kind ?? ""
            let parts: [String] = [
                entry.id, entry.model, entry.providerId, kind, entry.roles.joined(separator: " "), familyWords,
                entry.free ? "free" : "paid", entry.local ? "local" : "",
            ]
            let hay = parts.joined(separator: " ").lowercased()
            return words.allSatisfy { hay.contains($0) }
        }
        let primary = shown.filter { $0.roles.contains(role) }
        let other = shown.filter { !$0.roles.contains(role) }
        var result: [PickerSection] = []
        if !primary.isEmpty {
            result.append(section(id: "primary", title: role == "standard" ? "Standard models" : "Orchestrator models", models: primary, byId: byId))
        }
        if !other.isEmpty {
            result.append(section(id: "other", title: primary.isEmpty ? "All models" : "Other models", models: other, byId: byId))
        }
        return result
    }

    private static func section(id: String, title: String, models: [ModelEntry], byId: [String: ProviderInfo]) -> PickerSection {
        let grouped = Dictionary(grouping: models, by: \.providerId)
        let groups = grouped.keys.sorted().enumerated().map { index, providerId in
            PickerGroup(
                id: "\(id):\(providerId)",
                providerId: providerId,
                provider: byId[providerId],
                models: (grouped[providerId] ?? []).sorted { $0.id < $1.id },
                isFirst: index == 0
            )
        }
        return PickerSection(id: id, title: title, groups: groups)
    }
}
