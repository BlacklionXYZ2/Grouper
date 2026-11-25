import SwiftUI

@main
struct GrouperMacApp: App {
    @StateObject private var model = AppModel()
    @StateObject private var node = MacNodeController()

    var body: some Scene {
        WindowGroup("Grouper", id: "main") {
            Group { if model.signedIn { MacRootView() } else { SignInView() } }
                .environmentObject(model).environmentObject(node).tint(.teal)
                .onChange(of: model.signedIn) { _, signedIn in
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

private enum MacSection: Hashable { case chat, threads, devices, settings }

struct MacRootView: View {
    @EnvironmentObject private var model: AppModel
    @State private var section: MacSection = .chat

    var body: some View {
        NavigationSplitView {
            List(selection: $section) {
                Section("Chats") {
                    ForEach(model.conversations) { conversation in
                        Button {
                            section = .chat
                            Task { await model.selectConversation(conversation.id) }
                        } label: {
                            Label(conversation.title, systemImage: conversation.mode == "standard" ? "bubble.left" : "folder")
                        }.buttonStyle(.plain)
                    }
                    Button { section = .chat; model.beginNewChat(mode: "project") } label: { Label("New project", systemImage: "plus") }
                }
                Section {
                    Label("Threads", systemImage: "point.3.connected.trianglepath.dotted").tag(MacSection.threads)
                    Label("Devices", systemImage: "desktopcomputer").tag(MacSection.devices)
                    Label("Settings", systemImage: "gear").tag(MacSection.settings)
                }
            }
            .navigationSplitViewColumnWidth(min: 210, ideal: 260)
        } content: {
            switch section {
            case .chat: ChatView()
            case .threads: ThreadsView()
            case .devices: DevicesView()
            case .settings: NavigationStack { SettingsView() }
            }
        } detail: {
            if let thread = model.selectedThread { ThreadDetailView(thread: thread) }
            else { ContentUnavailableView("Select a thread", systemImage: "sidebar.right") }
        }
    }
}

struct MenuBarView: View {
    @EnvironmentObject private var model: AppModel
    @EnvironmentObject private var node: MacNodeController
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack { Circle().fill(model.online ? .green : .red).frame(width: 8, height: 8); Text(model.online ? "Hub connected" : "Hub offline"); Spacer(); Text(node.statusText).foregroundStyle(.secondary) }
            if !model.pendingApprovals.isEmpty {
                Divider()
                ForEach(model.pendingApprovals.prefix(3)) { approval in
                    VStack(alignment: .leading, spacing: 6) {
                        Text("\(approval.tool) · \(approval.reason)").lineLimit(2)
                        HStack {
                            Button("Approve") { Task { await model.resolveApproval(approval.id, approve: true) } }
                            Button("Deny") { Task { await model.resolveApproval(approval.id, approve: false) } }
                        }.buttonStyle(.glass)
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
                Button("Open Chat") { openWindow(id: "main") }
                Spacer()
                Button(node.running ? "Pause this Mac" : "Resume this Mac") {
                    if node.running { node.stop() } else { node.start(hubURL: model.hubURL, token: model.deviceToken) }
                }.disabled(model.deviceToken.isEmpty)
            }
        }.padding().frame(width: 390)
    }
}
