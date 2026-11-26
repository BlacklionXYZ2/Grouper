import AppKit
import SwiftUI

@main
struct GrouperMacApp: App {
    @StateObject private var model = AppModel()
    @StateObject private var node = MacNodeController()

    var body: some Scene {
        WindowGroup("Grouper", id: "main") {
            Group { if model.signedIn { MacRootView() } else { SignInView() } }
                .environmentObject(model).environmentObject(node).tint(.teal)
                .onChange(of: model.signedIn, initial: true) { _, signedIn in
                    if signedIn, !model.deviceToken.isEmpty { node.start(hubURL: model.hubURL, token: model.deviceToken) }
                    else if !signedIn { node.stop() }
                }
        }
        .defaultSize(width: 1180, height: 760)

        MenuBarExtra { MenuBarView().environmentObject(model).environmentObject(node) } label: {
            Label("Grouper", systemImage: menuIcon)
        }
        .menuBarExtraStyle(.window)

        Settings { SettingsView().environmentObject(model).environmentObject(node).frame(minWidth: 520, minHeight: 420) }
    }

    private var menuIcon: String { model.pendingApprovals.isEmpty ? (node.running ? "circle.hexagongrid.fill" : "pause.circle") : "exclamationmark.circle.fill" }
}

/// No Threads destination: threads belong to their chat and open in the chat's
/// inspector (Threads button in the chat toolbar, or a thread card). Each chat row
/// is tagged, so the sidebar highlights the open chat.
private enum MacSection: Hashable {
    case chat(String)
    case newChat
    case devices, settings

    var isChat: Bool {
        switch self {
        case .chat, .newChat: return true
        case .devices, .settings: return false
        }
    }
}

struct MacRootView: View {
    @EnvironmentObject private var model: AppModel
    @State private var section: MacSection?

    var body: some View {
        NavigationSplitView {
            List(selection: $section) {
                Section("Chats") {
                    ForEach(model.conversations) { conversation in
                        Label(conversation.title, systemImage: conversation.mode == "standard" ? "bubble.left" : "folder")
                            .tag(MacSection.chat(conversation.id))
                    }
                    Label("New project", systemImage: "plus").tag(MacSection.newChat)
                }
                Section {
                    Label("Devices", systemImage: "desktopcomputer").tag(MacSection.devices)
                    Label("Settings", systemImage: "gear").tag(MacSection.settings)
                }
            }
            .navigationSplitViewColumnWidth(min: 210, ideal: 260)
        } detail: {
            switch section ?? .newChat {
            case .chat, .newChat: NavigationStack { ChatView() }
            case .devices: DevicesView()
            case .settings: NavigationStack { SettingsView() }
            }
        }
        .onChange(of: section) { _, selected in open(selected) }
        // Keep the highlight on the open chat when it changes elsewhere (the chat
        // menu, or the first send of a new chat).
        .onChange(of: model.selectedConversationId, initial: true) { _, id in
            guard section?.isChat ?? true else { return }
            section = id.map { MacSection.chat($0) } ?? .newChat
        }
    }

    private func open(_ selected: MacSection?) {
        switch selected {
        case .chat(let id)?:
            if model.selectedConversationId != id { Task { await model.selectConversation(id) } }
        case .newChat?:
            if model.selectedConversationId != nil { model.beginNewChat(mode: "project") }
        default:
            break
        }
    }
}

struct MenuBarView: View {
    @EnvironmentObject private var model: AppModel
    @EnvironmentObject private var node: MacNodeController
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack { Circle().fill(model.online ? Color.green : Color.red).frame(width: 8, height: 8); Text(model.online ? "Hub connected" : "Hub offline"); Spacer(); Text(node.statusText).foregroundStyle(.secondary) }
            if !model.pendingApprovals.isEmpty {
                Divider()
                ForEach(model.pendingApprovals.prefix(3)) { approval in
                    VStack(alignment: .leading, spacing: 6) {
                        Text("\(approval.tool) · \(approval.reason)").lineLimit(2)
                        GlassEffectContainer(spacing: 8) {
                            HStack {
                                Button("Approve") { Task { await model.resolveApproval(approval.id, approve: true) } }
                                Button("Deny") { Task { await model.resolveApproval(approval.id, approve: false) } }
                            }.buttonStyle(.glass)
                        }
                    }
                }
            }
            let active = model.threads.filter { !$0.isTerminal }.prefix(5)
            if !active.isEmpty {
                Divider()
                ForEach(Array(active)) { thread in HStack { Text(thread.title).lineLimit(1); Spacer(); StatusLabel(status: thread.status) } }
            }
            Divider()
            HStack {
                Button("Open Chat") {
                    openWindow(id: "main")
                    NSApplication.shared.activate()
                }
                SettingsLink { Text("Settings") }
                Spacer()
                Button(node.running ? "Pause this Mac" : "Resume this Mac") {
                    if node.running { node.stop() } else { node.start(hubURL: model.hubURL, token: model.deviceToken) }
                }.disabled(model.deviceToken.isEmpty)
            }
        }.padding().frame(width: 390)
    }
}
