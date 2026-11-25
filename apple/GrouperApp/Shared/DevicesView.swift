import SwiftUI
import GrouperAPI

struct DevicesView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        NavigationStack {
            List(model.devices) { device in
                NavigationLink(value: device.id) {
                    VStack(alignment: .leading, spacing: 5) {
                        HStack { Circle().fill(device.online ? .green : .secondary).frame(width: 8, height: 8); Text(device.name).font(.headline) }
                        Text(device.online ? "Online · \(device.platform)" : offlineText(device)).font(.caption).foregroundStyle(.secondary)
                        Text(device.id == "hub" ? "Hub threads have file tools only." : "\(device.capabilities.count) tools")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
            .overlay { if model.devices.isEmpty { ContentUnavailableView("No devices", systemImage: "desktopcomputer", description: Text("Run grouper connect on a computer to make it available.")) } }
            .navigationTitle("Devices")
            .navigationDestination(for: String.self) { id in
                if let device = model.devices.first(where: { $0.id == id }) { DeviceDetailView(device: device) }
            }
            .refreshable { await model.refreshDevices() }
            .task {
                while !Task.isCancelled {
                    await model.refreshDevices()
                    try? await Task.sleep(for: .seconds(15))
                }
            }
        }
    }

    private func offlineText(_ device: DeviceInfo) -> String {
        guard let last = device.lastSeen else { return "Offline · never seen" }
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .abbreviated
        return "Offline · seen \(formatter.localizedString(for: Date(timeIntervalSince1970: TimeInterval(last)), relativeTo: Date()))"
    }
}

struct DeviceDetailView: View {
    @EnvironmentObject private var model: AppModel
    let device: DeviceInfo
    @State private var bypassCandidate: ToolInfo?

    var body: some View {
        List {
            Section {
                Text("Workspace file tools and known agent tools start on Auto; everything else starts on Manual. Only you can change these permissions.")
                    .font(.callout).foregroundStyle(.secondary)
                if device.id == "hub" { Text("Hub threads have file tools only.").font(.callout) }
            }
            Section("Available") { toolRows(tools.filter(\.available)) }
            if tools.contains(where: { !$0.available }) {
                Section("Unavailable") { toolRows(tools.filter { !$0.available }) }
            }
        }
        .navigationTitle(device.name)
        .task(id: device.id) { await model.loadTools(deviceId: device.id) }
        .confirmationDialog("Always run \(bypassCandidate?.name ?? "this tool")?", isPresented: Binding(
            get: { bypassCandidate != nil }, set: { if !$0 { bypassCandidate = nil } }
        ), titleVisibility: .visible) {
            Button("Set to Bypass", role: .destructive) {
                guard let tool = bypassCandidate else { return }
                bypassCandidate = nil
                Task { await model.setPermission(deviceId: device.id, tool: tool, permission: .bypass) }
            }
            Button("Cancel", role: .cancel) { bypassCandidate = nil }
        } message: {
            Text("Every call runs without asking, including deletes and commands outside the workspace. Only you can put it back.")
        }
    }

    private var tools: [ToolInfo] { model.toolsByDevice[device.id] ?? [] }

    @ViewBuilder private func toolRows(_ tools: [ToolInfo]) -> some View {
        if tools.isEmpty { Text("No tools").foregroundStyle(.secondary) }
        ForEach(tools) { tool in
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    Text(tool.name).font(.headline)
                    if !tool.available { Text("Unavailable").font(.caption).foregroundStyle(.secondary) }
                    if tool.permission == .bypass { Label("Bypass", systemImage: "exclamationmark.triangle.fill").font(.caption).foregroundStyle(.red) }
                }
                if !tool.description.isEmpty { Text(.init(tool.description)).font(.callout).foregroundStyle(.secondary) }
                Picker("Permission for \(tool.name)", selection: Binding(
                    get: { tool.permission },
                    set: { permission in
                        if permission == .bypass { bypassCandidate = tool }
                        else { Task { await model.setPermission(deviceId: device.id, tool: tool, permission: permission) } }
                    }
                )) {
                    Text("Manual").tag(Permission.manual)
                    Text("Auto").tag(Permission.auto)
                    Text("Bypass").tag(Permission.bypass)
                }
                .pickerStyle(.segmented).disabled(!tool.available || !model.online)
                .accessibilityHint(policy(tool: tool, permission: tool.permission))
                Text(policy(tool: tool, permission: tool.permission)).font(.caption)
                    .foregroundStyle(tool.permission == .bypass ? .red : isUnconfined(tool) ? .orange : .secondary)
            }.padding(.vertical, 6)
        }
    }

    private func policy(tool: ToolInfo, permission: Permission) -> String {
        switch permission {
        case .manual: return "Every call waits for you."
        case .bypass: return "Every call runs, including deletes and commands outside the workspace."
        case .auto:
            let server = tool.name.split(separator: "__").first.map(String.init) ?? ""
            if ["hermes", "grok", "opencode"].contains(server) {
                return "Auto does not confine this agent. It has no safety mode, so Auto runs it unconfined."
            }
            if ["claude-code", "codex", "cursor", "antigravity", "pi"].contains(server) {
                return "Runs in the thread workspace unless a risk rule asks; leaving it, requesting bypass, or a destructive instruction still asks."
            }
            return "Runs unless a risk rule asks."
        }
    }

    private func isUnconfined(_ tool: ToolInfo) -> Bool {
        let server = tool.name.split(separator: "__").first.map(String.init) ?? ""
        return tool.permission == .auto && ["hermes", "grok", "opencode"].contains(server)
    }
}
