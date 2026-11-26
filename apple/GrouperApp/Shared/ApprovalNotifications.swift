import Foundation
import GrouperAPI
import UserNotifications
#if os(iOS)
import UIKit
#elseif os(macOS)
import AppKit
#endif

/// Local approval notifications on iPhone and Mac (docs/gui-spec.md 5.14).
///
/// - Posted only while the app runs, only after the owner opted in from Settings,
///   and never for the chat already on screen in the foreground.
/// - Title "Grouper needs your review"; body names the tool, device, thread and
///   chat only. Never the input or the reason.
/// - No Approve/Deny/Edit actions: a notification can't show the exact input, so
///   tapping it opens the chat with that request in the approval dock.
///
/// TODO(APNs): background delivery needs APNs provisioning and a paid Apple
/// Developer account. Until then nothing arrives while the app is not running.
enum ApprovalNotifications {
    static let optInKey = "grouper.approvalNotifications"

    enum Permission: Sendable { case notDetermined, denied, allowed }

    static var optedIn: Bool {
        get { UserDefaults.standard.bool(forKey: optInKey) }
        set { UserDefaults.standard.set(newValue, forKey: optInKey) }
    }

    static func permission() async -> Permission {
        let status = await UNUserNotificationCenter.current().notificationSettings().authorizationStatus
        switch status {
        case .notDetermined: return .notDetermined
        case .denied: return .denied
        default: return .allowed
        }
    }

    /// Asks the system. Only ever called from the Settings opt-in button.
    static func requestPermission() async -> Bool {
        (try? await UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge])) ?? false
    }

    static func post(approvalId: String, conversationId: String, threadId: String?, body: String) async {
        guard optedIn, await permission() == .allowed else { return }
        let content = UNMutableNotificationContent()
        content.title = ApprovalNotificationText.title
        content.body = body
        content.sound = .default
        content.threadIdentifier = ApprovalNotificationText.identifier(approvalId: approvalId)
        var info: [String: String] = ["approval_id": approvalId, "conversation_id": conversationId]
        if let threadId { info["thread_id"] = threadId }
        content.userInfo = info
        let request = UNNotificationRequest(identifier: ApprovalNotificationText.identifier(approvalId: approvalId), content: content, trigger: nil)
        try? await UNUserNotificationCenter.current().add(request)
    }

    static func remove(approvalIds: [String]) {
        guard !approvalIds.isEmpty else { return }
        let identifiers = approvalIds.map { ApprovalNotificationText.identifier(approvalId: $0) }
        let center = UNUserNotificationCenter.current()
        center.removeDeliveredNotifications(withIdentifiers: identifiers)
        center.removePendingNotificationRequests(withIdentifiers: identifiers)
    }

    /// Whether the app is frontmost. With the chat on screen, the dock is the notice.
    @MainActor static func appIsActive() -> Bool {
        #if os(iOS)
        return UIApplication.shared.applicationState == .active
        #elseif os(macOS)
        return NSApplication.shared.isActive
        #else
        return false
        #endif
    }

    /// The system's notification settings for this app.
    @MainActor static var systemSettingsURL: URL? {
        #if os(iOS)
        return URL(string: UIApplication.openNotificationSettingsURLString)
        #else
        return URL(string: "x-apple.systempreferences:com.apple.Notifications-Settings.extension")
        #endif
    }
}

/// Where a tapped notification should land.
struct ApprovalRoute: Equatable, Sendable {
    let approvalId: String
    let conversationId: String?
    let threadId: String?
}

/// Holds a tapped notification's route until the app can act on it (a cold launch
/// taps before the chat views exist), then nudges the views.
@MainActor
enum ApprovalRouteInbox {
    private static var pending: ApprovalRoute?

    static func deliver(_ route: ApprovalRoute) {
        pending = route
        NotificationCenter.default.post(name: .grouperApprovalRoute, object: nil)
    }

    static func take() -> ApprovalRoute? {
        defer { pending = nil }
        return pending
    }
}

/// The notification center's delegate on both platforms.
final class ApprovalNotificationDelegate: NSObject, UNUserNotificationCenterDelegate, Sendable {
    static let shared = ApprovalNotificationDelegate()

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        let info = response.notification.request.content.userInfo
        guard let approvalId = info["approval_id"] as? String else { return }
        let route = ApprovalRoute(approvalId: approvalId,
                                  conversationId: info["conversation_id"] as? String,
                                  threadId: info["thread_id"] as? String)
        await MainActor.run { ApprovalRouteInbox.deliver(route) }
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        // AppModel posts nothing for the chat already on screen, so anything that
        // reaches here is for another chat or a background window.
        [.banner, .list, .sound]
    }
}

extension Notification.Name {
    /// A tapped approval notification is waiting in `ApprovalRouteInbox`.
    static let grouperApprovalRoute = Notification.Name("grouper.approvalRoute")
}
