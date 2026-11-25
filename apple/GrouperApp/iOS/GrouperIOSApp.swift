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
            .tint(.teal)
            .onOpenURL { url in handle(url) }
        }
    }

    private func handle(_ url: URL) {
        guard url.scheme == "grouper" else { return }
        switch url.host {
        case "approvals": break // PhoneRootView observes the pending approval badge; opening brings the app forward.
        case "ask":
            if let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
               let text = components.queryItems?.first(where: { $0.name == "text" })?.value, !text.isEmpty {
                Task { _ = await model.send(text) }
            }
        case "stop":
            if let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
               let thread = components.queryItems?.first(where: { $0.name == "thread" })?.value {
                Task { await model.stopThread(thread) }
            }
        default: break
        }
    }
}

struct PhoneRootView: View {
    @EnvironmentObject private var model: AppModel
    @State private var selection = 0
    var body: some View {
        TabView(selection: $selection) {
            NavigationStack { ChatView() }
                .tabItem { Label("Chat", systemImage: "bubble.left.and.bubble.right") }.tag(0)
            ThreadsView().tabItem { Label("Threads", systemImage: "point.3.connected.trianglepath.dotted") }.badge(model.pendingApprovals.count).tag(1)
            DevicesView().tabItem { Label("Devices", systemImage: "desktopcomputer.and.macbook") }.tag(2)
            MoreView().tabItem { Label("More", systemImage: "ellipsis") }.tag(3)
        }
        .onReceive(NotificationCenter.default.publisher(for: .grouperOpenApproval)) { note in
            guard let id = note.object as? String else { return }
            selection = 1
            model.selectedApprovalId = id
        }
    }
}
