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
        case "approvals":
            NotificationCenter.default.post(name: .grouperOpenTab, object: 1)
        case "ask":
            NotificationCenter.default.post(name: .grouperOpenTab, object: 0)
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

    private var needsYouCount: Int {
        model.pendingApprovals.count + model.threads.filter { $0.status == "waiting_device" }.count
    }

    var body: some View {
        TabView(selection: $selection) {
            Tab("Chat", systemImage: "bubble.left.and.bubble.right", value: 0) {
                NavigationStack { ChatView() }
            }
            Tab("Threads", systemImage: "point.3.connected.trianglepath.dotted", value: 1) {
                ThreadsView()
            }
            .badge(needsYouCount)
            Tab("Devices", systemImage: "desktopcomputer.and.macbook", value: 2) {
                DevicesView()
            }
            Tab("More", systemImage: "ellipsis", value: 3) {
                MoreView()
            }
        }
        // VERIFY ON MAC: Confirm tabBarMinimizeBehavior(.onScrollDown) coordinates cleanly with ChatView's .safeAreaBar(edge: .bottom) composer on iOS 26.
        .tabBarMinimizeBehavior(.onScrollDown)
        .onReceive(NotificationCenter.default.publisher(for: .grouperOpenApproval)) { note in
            guard let id = note.object as? String else { return }
            selection = 1
            model.selectedApprovalId = id
        }
        .onReceive(NotificationCenter.default.publisher(for: .grouperOpenTab)) { note in
            guard let tab = note.object as? Int else { return }
            selection = tab
        }
    }
}
