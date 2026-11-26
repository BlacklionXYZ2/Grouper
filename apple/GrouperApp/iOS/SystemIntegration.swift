import ActivityKit
import AppIntents
import GrouperAPI
import SwiftUI
import UIKit
import UserNotifications

struct AskGrouperIntent: AppIntent {
    static let title: LocalizedStringResource = "Ask Grouper"
    static let description = IntentDescription("Open the pinned project chat with an optional message.")
    // VERIFY ON MAC: Confirm IntentModes.foreground compiles in Xcode 26 AppIntents SDK (replacing deprecated openAppWhenRun = true).
    static let supportedModes: IntentModes = .foreground

    @Parameter(title: "Message") var text: String?

    func perform() async throws -> some IntentResult & OpensIntent {
        var components = URLComponents()
        components.scheme = "grouper"
        components.host = "ask"
        if let text, !text.isEmpty { components.queryItems = [URLQueryItem(name: "text", value: text)] }
        let url = components.url ?? URL(string: "grouper://ask")!
        // VERIFY ON MAC: Confirm OpenURLIntent accepts custom URL scheme "grouper://" on iOS 26.
        return .result(opensIntent: OpenURLIntent(url))
    }
}

struct OpenApprovalsIntent: AppIntent {
    static let title: LocalizedStringResource = "Open Grouper Approvals"
    static let supportedModes: IntentModes = .foreground
    func perform() async throws -> some IntentResult & OpensIntent {
        // VERIFY ON MAC: Confirm OpenURLIntent accepts custom URL scheme "grouper://" on iOS 26.
        .result(opensIntent: OpenURLIntent(URL(string: "grouper://approvals")!))
    }
}

struct GrouperShortcuts: AppShortcutsProvider {
    static var appShortcuts: [AppShortcut] {
        AppShortcut(intent: AskGrouperIntent(), phrases: ["Ask \(.applicationName)"], shortTitle: "Ask Grouper", systemImageName: "bubble.left")
        AppShortcut(intent: OpenApprovalsIntent(), phrases: ["Open \(.applicationName) approvals"], shortTitle: "Approvals", systemImageName: "exclamationmark.shield")
    }
}

@MainActor
enum LiveActivityCoordinator {
    static func start(threadId: String, title: String, device: String, status: String, phase: String) async throws {
        guard ActivityAuthorizationInfo().areActivitiesEnabled else { return }
        guard !Activity<ThreadActivityAttributes>.activities.contains(where: { $0.attributes.threadId == threadId }) else { return }
        let attributes = ThreadActivityAttributes(threadId: threadId, title: title, device: device)
        let state = ThreadActivityAttributes.ContentState(status: status, phase: phase, streamedLine: nil)
        _ = try Activity<ThreadActivityAttributes>.request(
            attributes: attributes,
            content: ActivityContent(state: state, staleDate: nil),
            pushType: nil
        )
    }

    static func update(threadId: String, status: String, phase: String, streamedLine: String?) async {
        let line = streamedLine.map { String($0.prefix(80)) }
        let content = ActivityContent(state: ThreadActivityAttributes.ContentState(status: status, phase: phase, streamedLine: line), staleDate: nil)
        for activity in Activity<ThreadActivityAttributes>.activities where activity.attributes.threadId == threadId { await activity.update(content) }
    }

    static func end(threadId: String, status: String) async {
        let content = ActivityContent(state: ThreadActivityAttributes.ContentState(status: status, phase: status.capitalized, streamedLine: nil), staleDate: nil)
        for activity in Activity<ThreadActivityAttributes>.activities where activity.attributes.threadId == threadId {
            await activity.end(content, dismissalPolicy: .after(Date.now.addingTimeInterval(60)))
        }
    }
}

/// Approval notifications live in `Shared/ApprovalNotifications.swift`, shared with
/// the Mac. They have no actions: a notification can't show the exact input, so it
/// only opens the chat with the request in its approval dock.
final class AppDelegate: NSObject, UIApplicationDelegate {
    func application(_ application: UIApplication, didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil) -> Bool {
        // Set before launch finishes, so a tap that launched the app is delivered.
        UNUserNotificationCenter.current().delegate = ApprovalNotificationDelegate.shared
        return true
    }
}

extension Notification.Name {
    static let grouperOpenTab = Notification.Name("grouper.openTab")
}
