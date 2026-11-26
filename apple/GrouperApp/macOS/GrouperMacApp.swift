import AppKit
import GrouperAPI
import SwiftUI
import UserNotifications

@main
struct GrouperMacApp: App {
    @StateObject private var model = AppModel()
    @StateObject private var node = MacNodeController()

    init() {
        // Local approval notifications (Shared/ApprovalNotifications.swift). Set
        // before launch finishes, so a tap that launched the app is delivered.
        UNUserNotificationCenter.current().delegate = ApprovalNotificationDelegate.shared
    }

    var body: some Scene {
        WindowGroup("Grouper", id: "main") {
            Group { if model.signedIn { MacRootView() } else { SignInView() } }
                .environmentObject(model).environmentObject(node).tint(.teal)
                // The node runs on this Mac's own device token, whether or not the window is
                // signed in: signing out keeps it serving; "Sign out and remove this Mac" stops it.
                .onChange(of: model.nodeTarget, initial: true) { _, target in node.apply(target) }
                // A revoked or unknown token: the model checks with the hub and registers again.
                .onChange(of: node.tokenRejected) { _, rejected in
                    if rejected { Task { await model.deviceTokenRejected() } }
                }
                // A tapped approval notification opens its chat's approval dock.
                .onAppear { takeApprovalRoute() }
                .onReceive(NotificationCenter.default.publisher(for: .grouperApprovalRoute)) { _ in takeApprovalRoute() }
        }
        .defaultSize(width: 1180, height: 760)
        .commands { PaletteCommands() }

        MenuBarExtra {
            MenuBarView().environmentObject(model).environmentObject(node)
                .onChange(of: model.nodeTarget, initial: true) { _, target in node.apply(target) }
        } label: {
            Label("Grouper", systemImage: menuIcon)
        }
        .menuBarExtraStyle(.window)

        Settings { SettingsView().environmentObject(model).environmentObject(node).frame(minWidth: 520, minHeight: 420) }
    }

    private var menuIcon: String { model.pendingApprovals.isEmpty ? (node.running ? "circle.hexagongrid.fill" : "pause.circle") : "exclamationmark.circle.fill" }

    private func takeApprovalRoute() {
        if let route = ApprovalRouteInbox.take() { model.openApprovalRoute(route) }
    }
}

/// No Threads destination: threads belong to their chat and open in the chat's
/// inspector (Threads button in the chat toolbar, or a thread card). Each chat row
/// is tagged, so the sidebar highlights the open chat.
private enum MacSection: Hashable {
    case chat(String)
    case newChat
    case devices, models, settings

    var isChat: Bool {
        switch self {
        case .chat, .newChat: return true
        case .devices, .models, .settings: return false
        }
    }
}

struct MacRootView: View {
    @EnvironmentObject private var model: AppModel
    @State private var section: MacSection?
    @State private var showPalette = false
    @State private var paletteQuery = ""
    @State private var highlightedModel: String?

    /// Destinations the Mac has. Needs you, Usage, Memory and Skills are not built here yet.
    private static let palettePages: [PalettePage] = [.devices, .models, .settings]
    /// No appearance setting exists yet, so no Switch theme action.
    private static let paletteActions: [PaletteAction] = [.newChat, .newProjectChat, .newStandardChat]

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
                    Label("Models", systemImage: "cpu").tag(MacSection.models)
                    Label("Settings", systemImage: "gear").tag(MacSection.settings)
                }
            }
            .navigationSplitViewColumnWidth(min: 210, ideal: 260)
            .toolbar {
                ToolbarItem {
                    Button { showPalette = true } label: { Label("Search", systemImage: "magnifyingglass") }
                        .help("Search chats, threads, models, devices, pages and actions (⌘K)")
                }
            }
        } detail: {
            switch section ?? .newChat {
            case .chat, .newChat: NavigationStack { ChatView() }
            case .devices: DevicesView()
            case .models: NavigationStack { ModelsListView(highlighted: highlightedModel) }
            case .settings: NavigationStack { SettingsView() }
            }
        }
        .focusedSceneValue(\.commandPalette, $showPalette)
        .sheet(isPresented: $showPalette) {
            CommandPaletteView(query: $paletteQuery, actions: Self.paletteActions, pages: Self.palettePages) { item in
                activate(item)
            }
        }
        .onChange(of: section) { _, selected in open(selected) }
        // Keep the highlight on the open chat when it changes elsewhere (the chat
        // menu, or the first send of a new chat).
        .onChange(of: model.selectedConversationId, initial: true) { _, id in
            guard section?.isChat ?? true else { return }
            section = id.map { MacSection.chat($0) } ?? .newChat
        }
        // A notification opened a chat's approval dock: show that chat.
        .onChange(of: model.chatRevealRequest) { _, _ in
            section = model.selectedConversationId.map { MacSection.chat($0) } ?? .newChat
        }
    }

    private func open(_ selected: MacSection?) {
        switch selected {
        case .chat(let id)?:
            if model.selectedConversationId != id { Task { await model.selectConversation(id) } }
        case .newChat?:
            if model.selectedConversationId != nil { model.beginNewChat(mode: "project") }
        case .models?:
            break
        default:
            highlightedModel = nil
        }
    }

    /// Opens a palette row through the app's own navigation. False when the item
    /// has gone, so the palette can say so instead of navigating somewhere else.
    private func activate(_ item: PaletteItem) -> Bool {
        switch item.target {
        case .action(let action):
            switch action {
            case .newChat, .newProjectChat:
                model.beginNewChat(mode: "project")
                section = .newChat
            case .newStandardChat:
                model.beginNewChat(mode: "standard")
                section = .newChat
            case .switchTheme:
                return false
            }
        case .chat(let id):
            guard model.conversations.contains(where: { $0.id == id }) else { return false }
            section = .chat(id)
        case .thread(let id, let conversationId):
            guard model.threads.contains(where: { $0.id == id }) else { return false }
            section = .chat(conversationId)
            if model.selectedConversationId == conversationId {
                model.selectedThreadId = id // the chat's inspector opens it
            } else {
                Task {
                    await model.selectConversation(conversationId)
                    model.selectedThreadId = id
                }
            }
        case .model(let id):
            guard model.models.contains(where: { $0.id == id }) else { return false }
            highlightedModel = id
            section = .models
        case .device(let id):
            guard model.devices.contains(where: { $0.id == id }) else { return false }
            model.selectedDeviceId = id
            section = .devices
        case .page(let page):
            switch page {
            case .devices: section = .devices
            case .models:
                highlightedModel = nil
                section = .models
            case .settings: section = .settings
            case .needsYou, .usage, .memory, .skills: return false
            }
        }
        return true
    }
}

struct MenuBarView: View {
    @EnvironmentObject private var model: AppModel
    @EnvironmentObject private var node: MacNodeController
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack { Circle().fill(model.online ? Color.green : Color.red).frame(width: 8, height: 8); Text(model.online ? "Hub connected" : "Hub offline"); Spacer(); Text(node.statusText).foregroundStyle(.secondary) }
            if let message = model.deviceMessage {
                Text(message).font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            } else if model.nodeTarget == nil, !model.registeringDevice {
                Text("This Mac isn't registered with the hub. Connect in the Grouper window to register it.")
                    .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            if !model.pendingApprovals.isEmpty {
                Divider()
                // No Approve/Deny here: the exact input must be reviewed in the dock first.
                ForEach(model.pendingApprovals.prefix(3)) { approval in
                    HStack(alignment: .firstTextBaseline) {
                        VStack(alignment: .leading, spacing: 2) {
                            Text("\(approval.tool) on \(model.deviceDisplayName(approval.deviceId))").lineLimit(1)
                            Text("\(model.threadTitle(approval.threadId)) · \(model.chatTitle(approval.conversationId))")
                                .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                        }
                        Spacer()
                        Button("Review") { review(approval) }
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
                    if node.running { node.stop() } else { node.resume() }
                }.disabled(model.nodeTarget == nil)
            }
        }.padding().frame(width: 390)
    }

    private func review(_ approval: PendingApproval) {
        model.openApprovalRoute(ApprovalRoute(approvalId: approval.id, conversationId: approval.conversationId, threadId: approval.threadId))
        openWindow(id: "main")
        NSApplication.shared.activate()
    }
}
