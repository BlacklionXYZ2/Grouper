import SwiftUI
import GrouperAPI

/// The chat's model picker: a covering panel (a full-height sheet on iPhone, a
/// large sheet on Mac), because there are many models. "Default model" comes
/// first; then API sections (the models with this chat's role, then "Other models",
/// each grouped under its provider), then "Harnesses (subscriptions)" grouped by
/// device. A model reached through two providers is two entries, one under each.
struct ModelPickerSheet: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    /// "orchestrator" for project chats, "standard" for standard chats.
    let role: String
    /// The chosen registry id; nil means the default.
    let current: String?
    let onPick: (String?) -> Void
    @State private var query = ""
    /// The owner's expand/collapse choice per provider, one "+id" or "-id" per line.
    @AppStorage("grouper.mp.groups") private var groupPrefs = ""
    /// Providers toggled while this sheet is open (their choice beats the defaults).
    @State private var touched: Set<String> = []
    /// Providers whose "Show all N" row was used.
    @State private var showAll: Set<String> = []
    private static let collapseAbove = 12
    private static let showFirst = 50
    #if os(macOS)
    @FocusState private var searchFocused: Bool
    #endif

    var body: some View {
        NavigationStack {
            let filtering = !Self.words(query).isEmpty
            let sections = Self.sections(models: model.models, providers: model.providers, devices: model.devices,
                                          role: role, words: Self.words(query), familyOf: { ModelFamily.of($0) })
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
                        let collapsible = isCollapsible(section: section, group: group)
                        let expanded = !collapsible || filtering || isExpanded(group)
                        let capped = expanded && !filtering && collapsible && group.models.count > Self.showFirst && !showAll.contains(group.providerId)
                        Section {
                            if expanded {
                                ForEach(capped ? Array(group.models.prefix(Self.showFirst)) : group.models) { entry in modelRow(entry) }
                                if capped {
                                    Button("Show all \(group.models.count)") { showAll.insert(group.providerId) }
                                        .buttonStyle(.plain)
                                        .font(.subheadline.weight(.semibold))
                                        .foregroundStyle(Theme.text2)
                                        .grouperMinimumControlSize()
                                }
                            }
                        } header: {
                            groupHeader(section: section, group: group, collapsible: collapsible, expanded: expanded, filtering: filtering)
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

    /// Only the "other models" provider groups collapse; the role-default section and the harness devices stay open.
    private func isCollapsible(section: PickerSection, group: PickerGroup) -> Bool {
        !group.isHarnessGroup && section.id != "primary"
    }

    private func storedPrefs() -> [String: Bool] {
        var result: [String: Bool] = [:]
        for line in groupPrefs.split(separator: "\n") {
            guard let sign = line.first, sign == "+" || sign == "-" else { continue }
            result[String(line.dropFirst())] = sign == "+"
        }
        return result
    }

    /// Touched this opening, then "holds the chosen model", then the saved choice, then the size default.
    private func isExpanded(_ group: PickerGroup) -> Bool {
        let stored = storedPrefs()[group.providerId]
        if touched.contains(group.providerId) { return stored ?? true }
        if let current, group.models.contains(where: { $0.id == current }) { return true }
        if let stored { return stored }
        return group.models.count <= Self.collapseAbove
    }

    private func toggle(_ group: PickerGroup) {
        let next = !isExpanded(group)
        var prefs = storedPrefs()
        prefs[group.providerId] = next
        groupPrefs = prefs.sorted { $0.key < $1.key }.map { ($0.value ? "+" : "-") + $0.key }.joined(separator: "\n")
        touched.insert(group.providerId)
    }

    private func groupHeader(section: PickerSection, group: PickerGroup, collapsible: Bool, expanded: Bool, filtering: Bool) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            if group.isFirst {
                Text(section.title).font(.headline).foregroundStyle(.primary)
            }
            if group.isHarnessGroup {
                let devName = group.deviceId.map { model.deviceDisplayName($0) } ?? "Unknown device"
                let devOnline = group.deviceId.flatMap { id in model.devices.first(where: { $0.id == id })?.online }
                HStack(spacing: 6) {
                    Text(devName).font(.subheadline.weight(.semibold))
                    TagBadge(text: "Harness", tint: Theme.accent, fill: Theme.accentSoft)
                    if let devOnline {
                        TagBadge(
                            text: devOnline ? "Online" : "Offline",
                            tint: devOnline ? Theme.ok : Theme.text2,
                            fill: devOnline ? Theme.okSoft : Theme.surface2
                        )
                    }
                }
            } else if collapsible && !filtering {
                Button { toggle(group) } label: {
                    HStack(spacing: 6) {
                        Image(systemName: expanded ? "chevron.down" : "chevron.right")
                            .font(.caption.weight(.semibold)).foregroundStyle(Theme.text3).frame(width: 12)
                            .accessibilityHidden(true)
                        providerHeaderLabel(group)
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .grouperMinimumControlSize()
                .accessibilityValue(expanded ? "Expanded" : "Collapsed")
                .accessibilityHint("Shows or hides this provider's models")
            } else {
                // Plain label: the role-default section, or any group while searching (every match is shown).
                HStack(spacing: 6) { providerHeaderLabel(group) }
            }
        }
        .textCase(nil)
        .modifier(HeaderAccessibility(combine: !(collapsible && !filtering)))
    }

    /// Provider name (uppercase mono, with the model count), then its badges.
    @ViewBuilder private func providerHeaderLabel(_ group: PickerGroup) -> some View {
        let name = group.providerId.isEmpty ? "Unknown provider" : group.providerId
        Text("\(name.uppercased()) · \(group.models.count)")
            .font(.caption.monospaced().weight(.semibold))
            .foregroundStyle(Theme.text2)
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

    private struct HeaderAccessibility: ViewModifier {
        let combine: Bool
        func body(content: Content) -> some View {
            if combine {
                content.accessibilityElement(children: .combine).accessibilityAddTraits(.isHeader)
            } else {
                content.accessibilityAddTraits(.isHeader)
            }
        }
    }

    @ViewBuilder private func modelRow(_ entry: ModelEntry) -> some View {
        let family = ModelFamily.of(entry)
        let selected = entry.id == current
        let provider = model.providers.first { $0.id == entry.providerId }
        if Self.isHarnessModel(entry, provider: provider) {
            harnessRow(entry, provider: provider, family: family, selected: selected)
        } else {
            apiModelRow(entry, family: family, selected: selected)
        }
    }

    private func apiModelRow(_ entry: ModelEntry, family: ModelFamily, selected: Bool) -> some View {
        Button { pick(entry.id) } label: {
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
                RoundedRectangle(cornerRadius: 2).fill(family.solidColor).frame(width: 4)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }

    private func harnessRow(_ entry: ModelEntry, provider: ProviderInfo?, family: ModelFamily, selected: Bool) -> some View {
        let cliName = Self.harnessCliName(entry, provider: provider)
        let devId = Self.harnessDeviceId(entry, provider: provider)
        let devName = devId.isEmpty ? "unknown device" : model.deviceDisplayName(devId)
        let devOnline = devId.isEmpty ? nil : model.devices.first(where: { $0.id == devId })?.online
        let isDisabled = role == "orchestrator" && !entry.canOrchestrate
        let place = devOnline.map { "\(cliName) on \(devName) · \($0 ? "online" : "offline")" } ?? "\(cliName) on \(devName)"
        return Button { pick(entry.id) } label: {
            HStack(alignment: .top, spacing: 10) {
                FamilyMark(family: family).padding(.top, 2)
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 6) {
                        Text("\(cliName) · \(entry.model)")
                            .font(.body.weight(.semibold))
                            .foregroundStyle(isDisabled ? Theme.text3 : .primary)
                            .lineLimit(1)
                        TagBadge(text: "Harness", tint: Theme.accent, fill: Theme.accentSoft)
                    }
                    Text(place).font(.caption).foregroundStyle(Theme.text2).lineLimit(1)
                    Text(Self.harnessMeta(entry, role: role)).font(.caption).foregroundStyle(Theme.text2)
                }
                Spacer(minLength: 8)
                if selected { Image(systemName: "checkmark").foregroundStyle(Theme.accent).accessibilityHidden(true) }
            }
            .padding(.leading, 6)
            .background(alignment: .leading) {
                RoundedRectangle(cornerRadius: 2).fill(family.solidColor).frame(width: 4)
            }
            .opacity(isDisabled ? 0.58 : 1)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(isDisabled)
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
        let deviceId: String?
        let isHarnessGroup: Bool
        let models: [ModelEntry]
        let isFirst: Bool
    }

    struct PickerSection: Identifiable {
        let id: String
        let title: String
        let groups: [PickerGroup]
    }

    private static let harnessNames: [String: String] = [
        "claude-code": "Claude Code",
        "codex": "Codex",
        "antigravity": "Antigravity",
        "grok": "Grok",
        "hermes": "Hermes",
        "pi": "pi",
        "opencode": "OpenCode",
        "cursor": "Cursor",
    ]

    static func isHarnessModel(_ entry: ModelEntry, provider: ProviderInfo?) -> Bool {
        entry.isHarness || provider?.kind == "harness"
    }

    static func harnessDeviceId(_ entry: ModelEntry, provider: ProviderInfo?) -> String {
        if let deviceId = entry.deviceId, !deviceId.isEmpty { return deviceId }
        if let via = provider?.viaDevice, !via.isEmpty { return via }
        if let at = entry.providerId.firstIndex(of: "@") {
            return String(entry.providerId[entry.providerId.index(after: at)...])
        }
        return ""
    }

    static func harnessCliName(_ entry: ModelEntry, provider: ProviderInfo?) -> String {
        if case .object(let opts)? = provider?.options {
            if case .string(let displayName) = opts["display_name"] {
                let trimmed = displayName.trimmingCharacters(in: .whitespacesAndNewlines)
                if !trimmed.isEmpty { return trimmed }
            }
            if case .string(let slug) = opts["harness"] {
                let trimmed = slug.trimmingCharacters(in: .whitespacesAndNewlines)
                if !trimmed.isEmpty { return harnessNames[trimmed] ?? trimmed }
            }
        }
        if let at = entry.providerId.firstIndex(of: "@") {
            let slug = String(entry.providerId[..<at])
            if !slug.isEmpty { return harnessNames[slug] ?? slug }
        }
        return "Harness"
    }

    static func harnessMeta(_ entry: ModelEntry, role: String) -> String {
        var parts: [String] = ["Subscription"]
        if role == "orchestrator" {
            parts.append(entry.canOrchestrate ? "Orchestrates via MCP" : "Standard chats and threads only")
        } else if entry.roles.contains(role) {
            parts.append("\(role) default")
        }
        if entry.contextWindow > 0 {
            parts.append("\(entry.contextWindow.formatted(.number.notation(.compactName))) context")
        }
        return parts.joined(separator: " · ")
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

    static func sections(models: [ModelEntry], providers: [ProviderInfo], devices: [DeviceInfo] = [], role: String,
                         words: [String], familyOf: (ModelEntry) -> ModelFamily) -> [PickerSection] {
        let byId = Dictionary(providers.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        let devicesById = Dictionary(devices.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        let shown = models.filter { entry in
            guard !words.isEmpty else { return true }
            let provider = byId[entry.providerId]
            let isHarness = isHarnessModel(entry, provider: provider)
            let family = familyOf(entry)
            let familyWords: String = family == .unknown ? "" : "\(family.label) \(family.maker)"
            let kind: String = provider?.kind ?? ""
            let devId = isHarness ? harnessDeviceId(entry, provider: provider) : (provider?.viaDevice ?? "")
            let dev = devId.isEmpty ? nil : devicesById[devId]
            let devName = dev?.name ?? devId
            let parts: [String] = [
                entry.id,
                entry.model,
                entry.displayName ?? "",
                entry.providerId,
                kind,
                isHarness ? "\(harnessCliName(entry, provider: provider)) harness subscription" : "",
                (isHarness && entry.canOrchestrate) ? "orchestrates mcp" : "",
                dev.map { $0.online ? "online" : "offline" } ?? "",
                devName,
                entry.roles.joined(separator: " "),
                familyWords,
                isHarness ? "" : (entry.free ? "free" : "paid"),
                (!isHarness && entry.local) ? "local" : "",
            ]
            let hay = parts.joined(separator: " ").lowercased()
            return words.allSatisfy { hay.contains($0) }
        }
        let apiShown = shown.filter { !isHarnessModel($0, provider: byId[$0.providerId]) }
        let harnessShown = shown.filter { isHarnessModel($0, provider: byId[$0.providerId]) }

        let primary = apiShown.filter { $0.roles.contains(role) }
        let other = apiShown.filter { !$0.roles.contains(role) }
        var result: [PickerSection] = []
        if !primary.isEmpty {
            result.append(section(id: "primary", title: role == "standard" ? "Standard models" : "Orchestrator models", models: primary, byId: byId))
        }
        if !other.isEmpty {
            result.append(section(id: "other", title: primary.isEmpty ? "All models" : "Other models", models: other, byId: byId))
        }
        if !harnessShown.isEmpty {
            let grouped = Dictionary(grouping: harnessShown) { harnessDeviceId($0, provider: byId[$0.providerId]) }
            let sortedDeviceIds = grouped.keys.sorted { lhs, rhs in
                let lName = lhs.isEmpty ? "Unknown device" : (devicesById[lhs]?.name ?? lhs)
                let rName = rhs.isEmpty ? "Unknown device" : (devicesById[rhs]?.name ?? rhs)
                return (lName, lhs) < (rName, rhs)
            }
            let groups = sortedDeviceIds.enumerated().map { index, devId in
                let sortedModels = (grouped[devId] ?? []).sorted { a, b in
                    let ca = harnessCliName(a, provider: byId[a.providerId])
                    let cb = harnessCliName(b, provider: byId[b.providerId])
                    return (ca, a.model, a.id) < (cb, b.model, b.id)
                }
                return PickerGroup(
                    id: "harnesses:\(devId)",
                    providerId: devId,
                    provider: nil,
                    deviceId: devId.isEmpty ? nil : devId,
                    isHarnessGroup: true,
                    models: sortedModels,
                    isFirst: index == 0
                )
            }
            result.append(PickerSection(id: "harnesses", title: "Harnesses (subscriptions)", groups: groups))
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
                deviceId: nil,
                isHarnessGroup: false,
                models: (grouped[providerId] ?? []).sorted { $0.id < $1.id },
                isFirst: index == 0
            )
        }
        return PickerSection(id: id, title: title, groups: groups)
    }
}
