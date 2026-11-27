import SwiftUI

@main
struct GrouperIOSApp: App {
    @StateObject private var model = AppModel()
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate

    var body: some Scene {
        WindowGroup {
            Group {
                if model.signedIn { PhoneRootView() } else { SignInView() }
            }
            .environmentObject(model)
            .tint(Theme.accent)
            .onOpenURL { url in handle(url) }
            // A tapped approval notification. Taken here, not in the tab view, so a tap
            // that launched the app is kept while it signs in.
            .onAppear { takeApprovalRoute() }
            .onReceive(NotificationCenter.default.publisher(for: .grouperApprovalRoute)) { _ in takeApprovalRoute() }
            // The saved copy of the Usage page holds spend figures: signing out removes it.
            .onChange(of: model.signedIn) { _, signedIn in
                if !signedIn {
                    UsageCacheStorage.clear()
                    WidgetSnapshotStore.clear()
                }
            }
        }
    }

    private func takeApprovalRoute() {
        if let route = ApprovalRouteInbox.take() { model.openApprovalRoute(route) }
    }

    private func handle(_ url: URL) {
        guard url.scheme == "grouper" else { return }
        switch url.host {
        case "approvals":
            // No Threads tab: approvals open from the chat they belong to.
            NotificationCenter.default.post(name: .grouperOpenTab, object: PhoneTab.chat)
        case "ask":
            NotificationCenter.default.post(name: .grouperOpenTab, object: PhoneTab.chat)
            if let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
               let text = components.queryItems?.first(where: { $0.name == "text" })?.value, !text.isEmpty {
                Task { _ = await model.send(text) }
            }
        case "usage":
            NotificationCenter.default.post(name: .grouperOpenTab, object: PhoneTab.usage)
        case "stop":
            if let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
               let thread = components.queryItems?.first(where: { $0.name == "thread" })?.value {
                Task { await model.stopThread(thread) }
            }
        default: break
        }
    }
}

/// Tab values. Threads are not a tab: they belong to their chat (Threads button in
/// the chat's toolbar).
enum PhoneTab {
    static let chat = 0
    static let usage = 1
    static let devices = 2
    static let more = 3
}

struct PhoneRootView: View {
    @EnvironmentObject private var model: AppModel
    @State private var selection = PhoneTab.chat

    /// Approvals and waiting threads across chats, shown on the Chat tab now that
    /// there is no Threads tab to carry it.
    private var needsYouCount: Int {
        model.pendingApprovals.count + model.threads.filter { $0.status == "waiting_device" }.count
    }

    var body: some View {
        // Chat, Usage, Devices and More (docs/usage-tab-spec.md 2, which replaced the three tabs of
        // docs/gui-concept-plan.md V10): the native tab bar (Liquid Glass), with the selected tab in
        // the brand teal. Usage has no badge in this round.
        TabView(selection: $selection) {
            Tab("Chat", systemImage: "bubble.left.and.bubble.right", value: PhoneTab.chat) {
                NavigationStack { ChatView() }
            }
            .badge(needsYouCount)
            Tab("Usage", systemImage: "chart.pie", value: PhoneTab.usage) {
                UsageView()
            }
            Tab("Devices", systemImage: "desktopcomputer.and.macbook", value: PhoneTab.devices) {
                DevicesView()
            }
            Tab("More", systemImage: "ellipsis", value: PhoneTab.more) {
                MoreView()
            }
        }
        // VERIFY ON MAC: Confirm tabBarMinimizeBehavior(.onScrollDown) coordinates cleanly with ChatView's .safeAreaBar(edge: .bottom) composer on iOS 26.
        .tabBarMinimizeBehavior(.onScrollDown)
        .tint(Theme.accent)
        // A notification opened a chat's approval dock: show the Chat tab.
        .onChange(of: model.chatRevealRequest) { _, _ in selection = PhoneTab.chat }
        .onReceive(NotificationCenter.default.publisher(for: .grouperOpenTab)) { note in
            guard let tab = note.object as? Int else { return }
            selection = tab
        }
    }
}
