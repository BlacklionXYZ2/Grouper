import ActivityKit
import AppIntents
import GrouperAPI
import SwiftUI
import UIKit
import UserNotifications

struct AskGrouperIntent: AppIntent {
    static let title: LocalizedStringResource = "Ask Grouper"
    static let description = IntentDescription("Open the pinned project chat with an optional message.")
    static let openAppWhenRun = true

    @Parameter(title: "Message") var text: String?

    func perform() async throws -> some IntentResult & OpensIntent {
        var components = URLComponents()
        components.scheme = "grouper"
        components.host = "ask"
        if let text, !text.isEmpty { components.queryItems = [URLQueryItem(name: "text", value: text)] }
        return .result(opensIntent: OpenURLIntent(components.url!))
    }
}

struct OpenApprovalsIntent: AppIntent {
    static let title: LocalizedStringResource = "Open Grouper Approvals"
    static let openAppWhenRun = true
    func perform() async throws -> some IntentResult & OpensIntent {
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
        guard !Activity<ThreadActivityAttributes>.activities.contains(where: { $0.attributes.threadId == threadId }) else { return }
        let attributes = ThreadActivityAttributes(threadId: threadId, title: title, device: device)
        let state = ThreadActivityAttributes.ContentState(status: status, phase: phase, streamedLine: nil)
        _ = try Activity.request(attributes: attributes, content: ActivityContent(state: state, staleDate: nil))
    }

    static func update(threadId: String, status: String, phase: String, streamedLine: String?) async {
        let line = streamedLine.map { String($0.prefix(80)) }
        let content = ActivityContent(state: ThreadActivityAttributes.ContentState(status: status, phase: phase, streamedLine: line), staleDate: nil)
        for activity in Activity<ThreadActivityAttributes>.activities where activity.attributes.threadId == threadId { await activity.update(content) }
    }

    static func end(threadId: String, status: String) async {
        let content = ActivityContent(state: ThreadActivityAttributes.ContentState(status: status, phase: status.capitalized, streamedLine: nil), staleDate: nil)
        for activity in Activity<ThreadActivityAttributes>.activities where activity.attributes.threadId == threadId {
            await activity.end(content, dismissalPolicy: .after(.now + 60))
        }
    }
}

enum ApprovalNotificationCoordinator {
    static func post(_ request: ApprovalRequest, threadTitle: String, deviceName: String) async {
        let center = UNUserNotificationCenter.current()
        let allowed = (try? await center.requestAuthorization(options: [.alert, .sound])) ?? false
        guard allowed else { return }
        let content = UNMutableNotificationContent()
        content.title = "Run \(request.tool)?"
        content.subtitle = "\(threadTitle) · \(deviceName)"
        content.body = request.reason
        content.categoryIdentifier = "grouper.approval"
        content.interruptionLevel = .timeSensitive
        content.userInfo = ["approval_id": request.approvalId]
        try? await center.add(UNNotificationRequest(identifier: "approval-\(request.approvalId)", content: content, trigger: nil))
    }

    static func remove(approvalId: String) {
        UNUserNotificationCenter.current().removeDeliveredNotifications(withIdentifiers: ["approval-\(approvalId)"])
        UNUserNotificationCenter.current().removePendingNotificationRequests(withIdentifiers: ["approval-\(approvalId)"])
    }
}

final class AppDelegate: NSObject, UIApplicationDelegate, UNUserNotificationCenterDelegate {
    func application(_ application: UIApplication, didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil) -> Bool {
        // Approving runs a tool on one of your devices: never from a locked phone.
        let approve = UNNotificationAction(identifier: "GROUPER_APPROVE", title: "Approve", options: .authenticationRequired)
        let deny = UNNotificationAction(identifier: "GROUPER_DENY", title: "Deny", options: .destructive)
        let edit = UNNotificationAction(identifier: "GROUPER_EDIT", title: "Edit", options: .foreground)
        let category = UNNotificationCategory(identifier: "grouper.approval", actions: [approve, deny, edit], intentIdentifiers: [])
        let center = UNUserNotificationCenter.current()
        center.setNotificationCategories([category])
        center.delegate = self
        return true
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        guard let approvalId = response.notification.request.content.userInfo["approval_id"] as? String else { return }
        if response.actionIdentifier == "GROUPER_EDIT" {
            await MainActor.run { NotificationCenter.default.post(name: .grouperOpenApproval, object: approvalId) }
            return
        }
        guard ["GROUPER_APPROVE", "GROUPER_DENY"].contains(response.actionIdentifier),
              let hub = UserDefaults.standard.string(forKey: "grouper.hubURL"),
              let token = KeychainStore.read(.admin) else { return }
        do {
            let client = try HubClient(baseURL: hub, token: token)
            _ = try await client.resolveApproval(id: approvalId, approve: response.actionIdentifier == "GROUPER_APPROVE")
        } catch {
            // A 404 means another device answered first; the notification can disappear quietly.
        }
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        [.banner, .list, .sound]
    }
}

extension Notification.Name {
    static let grouperOpenApproval = Notification.Name("grouper.openApproval")
}
