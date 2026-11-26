import Foundation
import SwiftUI
import GrouperAPI

private final class TaskBox {
    var task: Task<Void, Never>?

    func replace(with newTask: Task<Void, Never>?) {
        task?.cancel()
        task = newTask
    }

    func cancel() {
        task?.cancel()
        task = nil
    }

    deinit {
        task?.cancel()
    }
}

/// A one-line status in a chat, e.g. a tapped notification whose request is gone.
struct ChatNotice: Equatable {
    let conversationId: String?
    let text: String
}

/// What the Mac's local node connects with: the hub and this Mac's own device token
/// (never the admin token). `nil` stops the node.
struct NodeTarget: Equatable, Sendable {
    let hubURL: String
    let token: String
}

/// The name and platform of a device, for Settings > Account (never its id or token).
struct AppDeviceSummary: Equatable {
    let name: String
    /// As the hub reports it: `ios`, `macos`, ….
    let platform: String

    /// The platform as people say it.
    var platformName: String {
        switch platform.lowercased() {
        case "ios": return "iOS"
        case "macos": return "macOS"
        case "windows": return "Windows"
        case "linux": return "Linux"
        case "web": return "Web"
        case "cli": return "Command line"
        default: return platform
        }
    }
}

/// How a device token fared against the hub.
private enum DeviceTokenCheck {
    case usable
    /// 401: revoked, or never valid on this hub.
    case rejected
    /// The token can read providers, so it is an admin token.
    case admin
    /// Offline or an unexpected answer: keep the token and let the node retry.
    case unknown
}

/// How an answer from the approval dock went.
enum ApprovalAnswer: Equatable {
    case done
    /// Someone else answered first (the hub returned 404).
    case settledElsewhere
    case failed(String)
}

@MainActor
final class AppModel: ObservableObject {
    @Published var hubURL = ""
    @Published var adminToken = ""
    /// This Mac's device token, which only the local node uses. Set from the Keychain, an
    /// automatic registration (`POST /v1/devices`) or a verified paste; never the admin token.
    @Published private(set) var deviceToken = ""
    /// The hub's id for this Mac's device, when known.
    @Published private(set) var deviceId: String?
    /// The node's hub and token; the Mac app starts, restarts or stops the node when it changes.
    @Published private(set) var nodeTarget: NodeTarget?
    @Published private(set) var registeringDevice = false
    /// A plain-language problem with this Mac's registration, shown in Settings and the menu bar.
    @Published private(set) var deviceMessage: String?
    /// Settings > This Mac > "Use an existing device token".
    @Published var showManualDeviceToken = false
    @Published var manualDeviceToken = ""
    @Published private(set) var client: HubClient?
    @Published private(set) var signedIn = false
    @Published private(set) var online = false {
        didSet { if !online { approvalsReconciled = false } }
    }
    @Published var errorMessage: String?
    /// `GET /v1/auth/status` for the hub: claimed, and whether Google sign-in is on. Nil while unknown.
    @Published private(set) var authStatus: AuthStatus?
    /// The hub address (trimmed) `authStatus` was last checked for, so the sign-in screen can
    /// tell "still checking" from "this hub has no Google sign-in".
    @Published private(set) var authStatusCheckedFor: String?
    /// The last check couldn't reach a hub at that address (or it isn't a valid address).
    @Published private(set) var hubUnreachable = false
    /// A Google sign-in or link is running (Google's page may be open).
    @Published private(set) var googleBusy = false
    /// Settings > Account: the Google account that owns the hub, once `googleLinkLoaded`.
    @Published private(set) var linkedGoogle: LinkedGoogleAccount?
    @Published private(set) var googleLinkLoaded = false
    @Published private(set) var accountMessage: String?

    @Published private(set) var conversations: [ConversationInfo] = []
    @Published var selectedConversationId: String?
    @Published private(set) var folders: [String: TranscriptFolder] = [:]
    @Published private(set) var threads: [ThreadInfo] = []
    @Published private(set) var models: [ModelEntry] = []
    /// `GET /v1/providers` (admin), for the model picker's provider groups and key status.
    @Published private(set) var providers: [ProviderInfo] = []
    @Published private(set) var providersLoaded = false
    @Published private(set) var providersFailed = false
    @Published private(set) var devices: [DeviceInfo] = []
    @Published private(set) var toolsByDevice: [String: [ToolInfo]] = [:]
    @Published private(set) var streamState: EventStreamState = .disconnected
    @Published var selectedThreadId: String?
    @Published var selectedApprovalId: String?
    @Published var selectedDeviceId: String?
    @Published var threadMessageDrafts: [String: String] = [:]
    @Published var newChatMode = "project"
    /// The model picked for a chat that has not been sent yet. Cleared whenever the
    /// chat changes, so a choice never carries into another chat.
    @Published var newChatModel: String?

    /// Pending approvals across every chat: `GET /v1/approvals` plus approval events.
    @Published private(set) var approvalBook = ApprovalBook()
    /// False after going offline (or a failed answer) until the pending list has been
    /// fetched again; the dock keeps its actions off until then.
    @Published private(set) var approvalsReconciled = false
    /// The request that just left the open chat's dock, shown briefly with why.
    @Published private(set) var settledApproval: SettledApproval?
    /// Set when a notification opened the dock: the dock moves VoiceOver focus to its heading.
    @Published var dockFocusRequest: String?
    @Published var chatNotice: ChatNotice?
    /// Bumped when something outside the chat (a notification) needs the chat shown.
    @Published private(set) var chatRevealRequest = 0
    /// Ids of palette rows the owner opened, most recent first (command palette).
    @Published private(set) var paletteRecents: [String] = UserDefaults.standard.stringArray(forKey: "grouper.paletteRecents") ?? []
    /// Chat views on screen. With one up and the app frontmost, that chat's
    /// approvals show in its dock instead of as notifications.
    var chatViewsOnScreen = 0

    private static let recentsKey = "grouper.paletteRecents"
    /// The hub the saved device token belongs to, and the name it was registered under.
    private static let deviceHubKey = "grouper.deviceHubURL"
    private static let deviceNameKey = "grouper.deviceName"
    /// The name and platform of this app install's own device, from the last Google sign-in
    /// (Settings > Account shows them when the devices list doesn't have the device).
    private static let appDeviceNameKey = "grouper.appDeviceName"
    private static let appDevicePlatformKey = "grouper.appDevicePlatform"
    private var deviceHubURL = ""
    private var handlingRejectedToken = false
    private let streamTask = TaskBox()
    private let accountStreamTask = TaskBox()
    private let settleTask = TaskBox()
    private var approvalRefreshQueued = false
    private var pendingRoute: ApprovalRoute?
    private var initialLoadDone = false

    init() {
        hubURL = UserDefaults.standard.string(forKey: "grouper.hubURL") ?? ""
        adminToken = KeychainStore.read(.admin) ?? ""
        #if os(macOS)
        deviceToken = KeychainStore.read(.device) ?? ""
        deviceId = KeychainStore.read(.deviceId) ?? DeviceRegistration.deviceId(ofToken: deviceToken)
        // Tokens saved before the hub was recorded belong to the saved hub.
        deviceHubURL = UserDefaults.standard.string(forKey: Self.deviceHubKey) ?? hubURL
        // The node serves with its own token even before (or without) an admin sign-in.
        updateNodeTarget()
        #endif
        if !hubURL.isEmpty, !adminToken.isEmpty {
            Task { [weak self] in await self?.connect(save: false) }
        }
    }

    var selectedConversation: ConversationInfo? {
        conversations.first { $0.id == selectedConversationId }
    }

    var selectedFolder: TranscriptFolder? {
        guard let id = selectedConversationId else { return nil }
        return folders[id]
    }

    var conversationThreads: [ThreadInfo] {
        guard let id = selectedConversationId else { return [] }
        return threads.filter { $0.conversationId == id }
    }

    var transcript: [TranscriptRow] {
        selectedFolder?.transcript(threads: conversationThreads) ?? []
    }

    /// Every pending approval, oldest first (ties by id). The hub's list once it has
    /// loaded; before that (or on a hub without `GET /v1/approvals`) the requests
    /// folded from loaded chats, merged with any that arrived as events.
    var pendingApprovals: [PendingApproval] {
        if approvalBook.loaded { return approvalBook.ordered }
        var byId: [String: PendingApproval] = approvalBook.pending
        for folder in folders.values {
            let folded = folder.pendingApprovals(threads: threads.filter { $0.conversationId == folder.conversationId })
            for approval in folded where byId[approval.id] == nil && approvalBook.outcome(of: approval.id) == nil {
                byId[approval.id] = approval
            }
        }
        return ApprovalQueue.ordered(Array(byId.values))
    }

    var selectedThread: ThreadInfo? { threads.first { $0.id == selectedThreadId } }

    /// Approve, Deny, Change it and Stop need a connection that has checked the
    /// pending list since it was last offline.
    var canAnswerApprovals: Bool { client != nil && online && approvalsReconciled }

    /// Threads belong to their chat: there is no global thread list.
    func threads(in conversationId: String) -> [ThreadInfo] {
        threads.filter { $0.conversationId == conversationId }
    }

    func approvals(in conversationId: String) -> [PendingApproval] {
        pendingApprovals.filter { $0.conversationId == conversationId }
    }

    /// Pending approvals plus threads waiting for a device, in one chat.
    func needsYouCount(in conversationId: String) -> Int {
        let pending = self.approvals(in: conversationId)
        let withApproval = Set(pending.compactMap(\.threadId))
        let waiting = self.threads(in: conversationId).filter { $0.status == "waiting_device" && !withApproval.contains($0.id) }
        return pending.count + waiting.count
    }

    /// The thread's streamed text that no stored event has replaced yet.
    func threadDraft(_ threadId: String) -> String {
        guard let conversationId = threads.first(where: { $0.id == threadId })?.conversationId else { return "" }
        return folders[conversationId]?.threadDrafts[threadId] ?? ""
    }

    func modelEntry(_ id: String) -> ModelEntry? { models.first { $0.id == id } }

    /// The family of a registry id as the hub reports it on a thread or chat.
    func family(ofModel id: String) -> ModelFamily { ModelFamily.of(registryId: id, in: models) }

    func deviceDisplayName(_ id: String) -> String { deviceName(id) }

    func threadTitle(_ id: String?) -> String {
        guard let id, let title = threads.first(where: { $0.id == id })?.title, !title.isEmpty else { return "Thread" }
        return title
    }

    func chatTitle(_ id: String) -> String {
        guard let title = conversations.first(where: { $0.id == id })?.title, !title.isEmpty else { return "Chat" }
        return title
    }

    func connect(save: Bool = true) async {
        errorMessage = nil
        let trimmedHub = hubURL.trimmingCharacters(in: .whitespacesAndNewlines)
        let trimmedAdmin = adminToken.trimmingCharacters(in: .whitespacesAndNewlines)
        do {
            let api = try HubClient(baseURL: trimmedHub, token: trimmedAdmin)
            try await api.health()
            _ = try await api.providers() // proves this is the admin token, not merely a valid device token
            hubURL = trimmedHub
            adminToken = trimmedAdmin
            client = api
            signedIn = true
            online = true
            if save {
                UserDefaults.standard.set(trimmedHub, forKey: "grouper.hubURL")
                try KeychainStore.save(trimmedAdmin, as: .admin)
            }
            #if os(macOS)
            // The Mac registers itself; the owner never pastes a device token.
            await ensureThisMacRegistered(api: api, hubURL: trimmedHub)
            #endif
            await loadInitial()
        } catch {
            signedIn = false
            online = false
            errorMessage = error.localizedDescription
        }
    }

    // MARK: - Sign in with Google (docs/accounts-design.md)

    /// Whether this app has a Google client ID and the hub at `hubURL` allows Google sign-in.
    var googleSignInAvailable: Bool { GoogleSignIn.oauth != nil && authStatus?.google == true }

    /// Sign-in screen: `GET /v1/auth/status` for the address as typed. A hub that answers without
    /// the route (one from before Google sign-in) leaves it nil, which means token sign-in. A
    /// hub that can't be reached (or an invalid address) also leaves it nil and sets
    /// `hubUnreachable`, so the screen says so instead of falling back to the token field.
    func checkAuthStatus(for typed: String) async {
        let hub = typed.trimmingCharacters(in: .whitespacesAndNewlines)
        var status: AuthStatus?
        var unreachable = false
        if !hub.isEmpty {
            do {
                status = try await HubClient(baseURL: hub).authStatus()
            } catch {
                switch error as? HubClientError {
                case .transport?, .invalidBaseURL?:
                    unreachable = true
                case .server(let code, _)? where code >= 500:
                    unreachable = true // e.g. the tunnel is up but the hub behind it isn't
                default:
                    break // the hub answered, but not with an auth status: no Google sign-in there
                }
            }
        }
        // Typed on since: a newer check owns the result.
        guard hub == hubURL.trimmingCharacters(in: .whitespacesAndNewlines) else { return }
        authStatus = status
        hubUnreachable = unreachable
        authStatusCheckedFor = hub
    }

    /// Signs in with Google: the hub's nonce, Google's page, then `POST /v1/auth/google` for an
    /// admin token for this install. The token is saved like a pasted one and the app connects
    /// exactly as a token sign-in does (on the Mac that also registers this Mac's node).
    /// `setupCode` is sent only while the hub is unclaimed. The install's `install_id` goes
    /// with it, so signing in again from this install reuses its device on the hub.
    func signInWithGoogle(setupCode: String) async {
        guard let oauth = GoogleSignIn.oauth, !googleBusy else { return }
        googleBusy = true
        defer { googleBusy = false }
        errorMessage = nil
        let trimmedHub = hubURL.trimmingCharacters(in: .whitespacesAndNewlines)
        do {
            let api = try HubClient(baseURL: trimmedHub)
            let nonce = try await api.googleNonce()
            let idToken = try await GoogleSignIn.idToken(oauth: oauth, nonce: nonce.nonce)
            let code = authStatus?.claimed == false ? (SetupCode.normalized(setupCode) ?? setupCode) : nil
            let result = try await api.signInWithGoogle(idToken: idToken, setupCode: code, client: .current,
                                                        deviceName: GoogleSignIn.deviceName, installId: AppInstall.id)
            // Kept now, so a connection hiccup below doesn't lose the new token.
            UserDefaults.standard.set(trimmedHub, forKey: "grouper.hubURL")
            try KeychainStore.save(result.token, as: .admin)
            UserDefaults.standard.set(result.device.name, forKey: Self.appDeviceNameKey)
            UserDefaults.standard.set(result.device.platform, forKey: Self.appDevicePlatformKey)
            hubURL = trimmedHub
            adminToken = result.token
        } catch {
            errorMessage = GoogleSignIn.message(for: error)
            switch error as? HubClientError {
            case .conflict?, .notFound?:
                // Claimed a moment ago, or Google sign-in was turned off: show what the hub allows now.
                await checkAuthStatus(for: trimmedHub)
            default:
                break
            }
            return
        }
        await connect()
    }

    /// Settings > Account: this app install's own device on the hub (the one its admin token
    /// belongs to), from the devices list, else as the last Google sign-in named it.
    var thisAppDevice: AppDeviceSummary? {
        if let id = DeviceRegistration.deviceId(ofToken: adminToken), let device = devices.first(where: { $0.id == id }) {
            return AppDeviceSummary(name: device.name, platform: device.platform)
        }
        guard let name = UserDefaults.standard.string(forKey: Self.appDeviceNameKey) else { return nil }
        let platform = UserDefaults.standard.string(forKey: Self.appDevicePlatformKey) ?? GoogleSignInClient.current.rawValue
        return AppDeviceSummary(name: name, platform: platform)
    }

    /// Settings > Account: the Google account linked to this hub, and whether the hub allows Google.
    func refreshGoogleLink() async {
        guard let client else { return }
        authStatus = try? await client.authStatus()
        do {
            linkedGoogle = try await client.linkedGoogleAccount()
            accountMessage = nil
        } catch HubClientError.notFound(_) {
            linkedGoogle = nil // a hub from before Google sign-in
        } catch {
            accountMessage = "Couldn't check the linked Google account: \(error.localizedDescription)"
        }
        googleLinkLoaded = true
    }

    /// Settings > Account > Link your Google account: the same Google flow, then `POST /v1/auth/google/link`.
    func linkGoogle() async {
        guard let client, let oauth = GoogleSignIn.oauth, !googleBusy else { return }
        googleBusy = true
        defer { googleBusy = false }
        accountMessage = nil
        do {
            let nonce = try await client.googleNonce()
            let idToken = try await GoogleSignIn.idToken(oauth: oauth, nonce: nonce.nonce)
            linkedGoogle = try await client.linkGoogle(idToken: idToken)
            googleLinkLoaded = true
        } catch {
            accountMessage = GoogleSignIn.message(for: error)
        }
    }

    /// Settings > Account > Unlink. Tokens and devices keep working.
    func unlinkGoogle() async {
        guard let client, !googleBusy else { return }
        googleBusy = true
        defer { googleBusy = false }
        accountMessage = nil
        do {
            try await client.unlinkGoogle()
            linkedGoogle = nil
        } catch {
            accountMessage = "Couldn't unlink the Google account: \(error.localizedDescription)"
        }
    }

    // MARK: - This Mac as a device

    #if os(macOS)
    /// The name this Mac is registered under on the hub.
    var thisMacName: String? {
        guard let deviceId, !deviceToken.isEmpty else { return nil }
        return devices.first { $0.id == deviceId }?.name ?? UserDefaults.standard.string(forKey: Self.deviceNameKey)
    }

    /// On connect: keep a device token the hub accepts, or register this Mac and save a new one.
    private func ensureThisMacRegistered(api: HubClient, hubURL: String) async {
        guard !registeringDevice else { return }
        var revoke: String?
        let token = deviceToken
        if !token.isEmpty, DeviceRegistration.sameHub(deviceHubURL, hubURL) {
            switch await check(deviceToken: token, hubURL: hubURL) {
            case .usable, .unknown:
                deviceMessage = nil
                updateNodeTarget()
                return
            case .rejected:
                revoke = deviceId // best effort: it may already be revoked or unknown
            case .admin:
                // Never hand an admin token to the node. It's the owner's, so don't revoke it either.
                break
            }
            // Replaced while we checked (the node's 401 report got there first).
            guard token == deviceToken else { return }
        }
        await registerThisMac(api: api, hubURL: hubURL, revoking: revoke)
    }

    /// Settings > This Mac > Register again: a new device for this Mac, then the old one revoked.
    func registerThisMacAgain() async {
        guard let client, !registeringDevice else { return }
        let old = DeviceRegistration.sameHub(deviceHubURL, hubURL) ? deviceId : nil
        if await registerThisMac(api: client, hubURL: hubURL, revoking: old) { await refreshDevices() }
    }

    /// The node said the hub refused its token. If the hub confirms (401), register again.
    func deviceTokenRejected() async {
        guard !handlingRejectedToken, !registeringDevice, !deviceToken.isEmpty else { return }
        handlingRejectedToken = true
        defer { handlingRejectedToken = false }
        let token = deviceToken
        guard await check(deviceToken: token, hubURL: deviceHubURL) == .rejected, token == deviceToken else { return }
        guard let client, DeviceRegistration.sameHub(deviceHubURL, hubURL) else {
            deviceMessage = "The hub no longer accepts this Mac's device token. Sign in to register this Mac again."
            return
        }
        if await registerThisMac(api: client, hubURL: hubURL, revoking: deviceId) { await refreshDevices() }
    }

    /// "Use an existing device token": verify it is a plain device token on this hub, then use it.
    func saveManualDeviceToken() async -> Bool {
        let token = manualDeviceToken.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !token.isEmpty else { return false }
        switch await check(deviceToken: token, hubURL: hubURL) {
        case .usable:
            break
        case .admin:
            deviceMessage = "That is an admin token. The Mac node needs a plain device token (grouper-hub device add)."
            return false
        case .rejected:
            deviceMessage = "The hub doesn't accept that device token."
            return false
        case .unknown:
            deviceMessage = "Couldn't check that token with the hub. Try again when the hub is reachable."
            return false
        }
        do {
            try storeDevice(token: token, id: DeviceRegistration.deviceId(ofToken: token), name: nil, hubURL: hubURL)
        } catch {
            deviceMessage = error.localizedDescription
            return false
        }
        manualDeviceToken = ""
        deviceMessage = nil
        await refreshDevices()
        return true
    }

    /// Revokes this Mac's device on the hub, forgets its token and signs out. Stays signed in
    /// (with a message) if the hub can't revoke it.
    func signOutAndRemoveThisMac() async {
        guard !registeringDevice else { return }
        if let client, let deviceId, DeviceRegistration.sameHub(deviceHubURL, hubURL) {
            do {
                try await client.revokeDevice(id: deviceId)
            } catch HubClientError.notFound(_) {
                // Already gone from the hub.
            } catch {
                deviceMessage = "Couldn't remove this Mac from the hub: \(error.localizedDescription)"
                return
            }
        }
        forgetThisMac()
        signOut()
    }

    /// Registers this Mac as a plain device and saves its token, then revokes `old` (best
    /// effort). On failure the manual token field opens with a plain explanation.
    ///
    /// The registration carries this install's node id (`InstallID.node`), so a hub that
    /// supports install ids hands back the same node device with a fresh token (revoking the
    /// old one) instead of adding a second device for this Mac; `old` is then that same device
    /// and is not revoked. The node id differs from the app's own sign-in id, so the node can
    /// never be handed the app's admin device.
    @discardableResult
    private func registerThisMac(api: HubClient, hubURL: String, revoking old: String?) async -> Bool {
        // Launch can race the node's own 401 report; only one registration runs.
        guard !registeringDevice else { return false }
        registeringDevice = true
        defer { registeringDevice = false }
        let name = DeviceRegistration.name(MacNodeController.computerName())
        let registered: RegisteredDevice
        do {
            registered = try await api.registerDevice(name: name, platform: "macos",
                                                      installId: AppInstall.id.map(InstallID.node))
        } catch {
            if DeviceRegistration.hubLacksRegistration(error) {
                deviceMessage = "This hub can't register devices yet; paste a device token instead."
            } else {
                deviceMessage = "Couldn't register this Mac with the hub: \(error.localizedDescription)"
            }
            showManualDeviceToken = true
            return false
        }
        do {
            try storeDevice(token: registered.token, id: registered.device.id, name: registered.device.name, hubURL: hubURL)
        } catch {
            // Unusable without the Keychain: take the new device back off the hub.
            _ = try? await api.revokeDevice(id: registered.device.id)
            deviceMessage = "Couldn't save this Mac's device token: \(error.localizedDescription)"
            return false
        }
        deviceMessage = nil
        showManualDeviceToken = false
        if let old, old != registered.device.id {
            do {
                try await api.revokeDevice(id: old)
            } catch HubClientError.notFound(_) {
                // Already revoked or unknown on this hub.
            } catch {
                deviceMessage = "This Mac has a new device token, but the old device \(old) couldn't be revoked: \(error.localizedDescription)"
            }
        }
        return true
    }

    private func check(deviceToken token: String, hubURL: String) async -> DeviceTokenCheck {
        guard let deviceAPI = try? HubClient(baseURL: hubURL, token: token) else { return .unknown }
        do {
            _ = try await deviceAPI.providers()
            return .admin
        } catch HubClientError.forbidden(_) {
            return .usable // a device token can read, but cannot list providers
        } catch HubClientError.unauthorized(_) {
            return .rejected
        } catch {
            return .unknown
        }
    }

    private func storeDevice(token: String, id: String?, name: String?, hubURL: String) throws {
        try KeychainStore.save(token, as: .device)
        if let id { try KeychainStore.save(id, as: .deviceId) } else { KeychainStore.delete(.deviceId) }
        UserDefaults.standard.set(hubURL, forKey: Self.deviceHubKey)
        if let name { UserDefaults.standard.set(name, forKey: Self.deviceNameKey) }
        else { UserDefaults.standard.removeObject(forKey: Self.deviceNameKey) }
        deviceToken = token
        deviceId = id
        deviceHubURL = hubURL
        updateNodeTarget()
    }

    private func forgetThisMac() {
        KeychainStore.delete(.device)
        KeychainStore.delete(.deviceId)
        UserDefaults.standard.removeObject(forKey: Self.deviceHubKey)
        UserDefaults.standard.removeObject(forKey: Self.deviceNameKey)
        deviceToken = ""
        deviceId = nil
        deviceHubURL = ""
        deviceMessage = nil
        updateNodeTarget()
    }

    private func updateNodeTarget() {
        let target = deviceToken.isEmpty || deviceHubURL.isEmpty ? nil : NodeTarget(hubURL: deviceHubURL, token: deviceToken)
        if target != nodeTarget { nodeTarget = target }
    }
    #endif

    func signOut() {
        streamTask.cancel()
        accountStreamTask.cancel()
        settleTask.cancel()
        ApprovalNotifications.remove(approvalIds: Array(approvalBook.pending.keys))
        approvalBook.reset()
        settledApproval = nil
        chatNotice = nil
        dockFocusRequest = nil
        pendingRoute = nil
        initialLoadDone = false
        paletteRecents = []
        UserDefaults.standard.removeObject(forKey: Self.recentsKey)
        client = nil
        signedIn = false
        online = false
        adminToken = ""
        errorMessage = nil
        linkedGoogle = nil
        googleLinkLoaded = false
        accountMessage = nil
        conversations.removeAll()
        threads.removeAll()
        folders.removeAll()
        providers.removeAll()
        providersLoaded = false
        providersFailed = false
        selectedConversationId = nil
        selectedThreadId = nil
        selectedApprovalId = nil
        streamState = .disconnected
        KeychainStore.delete(.admin)
        UserDefaults.standard.removeObject(forKey: Self.appDeviceNameKey)
        UserDefaults.standard.removeObject(forKey: Self.appDevicePlatformKey)
        // This Mac's device token stays (see signOutAndRemoveThisMac), so its node keeps serving.
        // The install id stays too, so signing in again reuses this install's devices on the hub.
    }

    func loadInitial() async {
        guard let client else { return }
        do {
            async let loadedConversations = client.conversations()
            async let loadedThreads = client.threads()
            async let loadedModels = client.models()
            async let loadedDevices = client.devices()
            conversations = try await loadedConversations.sorted { $0.created > $1.created }
            threads = try await loadedThreads
            models = try await loadedModels
            devices = try await loadedDevices
            online = true
            if selectedConversationId == nil { selectedConversationId = conversations.first?.id }
            if let id = selectedConversationId { await selectConversation(id) }
            await loadApprovals()
            followAccountEvents()
            initialLoadDone = true
            await processPendingRoute()
        } catch {
            online = false
            errorMessage = error.localizedDescription
        }
    }

    func selectConversation(_ id: String) async {
        guard let client else { return }
        if selectedConversationId != id {
            // Back in a chat, its dock starts at the oldest request again.
            selectedApprovalId = nil
            settledApproval = nil
            if chatNotice?.conversationId != id { chatNotice = nil }
        }
        selectedConversationId = id
        newChatModel = nil
        rememberRecent(PaletteItem(target: .chat(id: id), title: "", subtitle: "").id)
        streamTask.cancel()
        do {
            async let events = client.events(conversationId: id)
            async let scopedThreads = client.threads(conversationId: id)
            let fetchedEvents = try await events
            let scoped = try await scopedThreads
            guard selectedConversationId == id else { return }
            folders[id] = TranscriptFolder(conversationId: id, events: fetchedEvents)
            threads = ThreadInfo.merge(threads, fetched: scoped, conversationId: id)
            online = true
            followEvents(conversationId: id)
        } catch {
            guard selectedConversationId == id else { return }
            online = false
            errorMessage = error.localizedDescription
        }
    }

    func send(_ text: String, model: String? = nil) async -> Bool {
        guard let client else { return false }
        do {
            let response = try await client.sendChat(
                text: text,
                conversationId: selectedConversationId,
                mode: selectedConversationId == nil ? newChatMode : nil,
                model: selectedConversationId == nil ? model : nil
            )
            if selectedConversationId != response.conversationId {
                await refreshConversations()
                await selectConversation(response.conversationId)
            }
            return true
        } catch {
            errorMessage = error.localizedDescription
            return false
        }
    }

    func setConversationModel(_ modelId: String?) async {
        guard let client, let conversationId = selectedConversationId else { return }
        do {
            let updated = try await client.setConversationModel(modelId, conversationId: conversationId)
            if let index = conversations.firstIndex(where: { $0.id == updated.id }) {
                conversations[index] = updated
            }
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Reloads the registry and the providers behind it, for the model picker.
    func loadProviders() async {
        guard let client else { return }
        if let loaded = try? await client.models() { models = loaded }
        do {
            providers = try await client.providers().sorted { $0.id < $1.id }
            providersLoaded = true
            providersFailed = false
        } catch {
            // The picker still groups by provider id; it just cannot show kind or key status.
            providersFailed = true
        }
    }

    func refreshThreads(conversationId: String) async {
        guard let client else { return }
        do {
            let updated = try await client.threads(conversationId: conversationId)
            threads = ThreadInfo.merge(threads, fetched: updated, conversationId: conversationId)
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func refreshConversations() async {
        guard let client else { return }
        do { conversations = try await client.conversations().sorted { $0.created > $1.created } }
        catch { errorMessage = error.localizedDescription }
    }

    func beginNewChat(mode: String) {
        streamTask.cancel()
        selectedConversationId = nil
        selectedThreadId = nil
        selectedApprovalId = nil
        settledApproval = nil
        chatNotice = nil
        newChatMode = mode
        newChatModel = nil
    }

    func messageThread(_ id: String, text: String) async -> Bool {
        guard let client else { return false }
        do { _ = try await client.messageThread(id: id, text: text); threadMessageDrafts[id] = ""; return true }
        catch { errorMessage = error.localizedDescription; return false }
    }

    @discardableResult
    func stopThread(_ id: String) async -> Bool {
        guard let client else { return false }
        do { _ = try await client.stopThread(id: id); return true }
        catch { errorMessage = error.localizedDescription; return false }
    }

    func rateThread(_ id: String, rating: String) async {
        guard let client else { return }
        do { _ = try await client.rateThread(id: id, rating: rating) }
        catch { errorMessage = error.localizedDescription }
    }

    // MARK: - Approvals

    /// Answers one request. Never retried automatically: on a network error the
    /// pending list is fetched again before the dock offers the buttons again.
    func answerApproval(_ id: String, approve: Bool) async -> ApprovalAnswer {
        guard let client else { return .failed("Not connected to the hub.") }
        approvalBook.beginAnswer(id)
        do {
            _ = try await client.resolveApproval(id: id, approve: approve)
            updateApprovals { $0.settle(id, as: .answeredHere(approved: approve)) }
            return .done
        } catch HubClientError.notFound(_) {
            updateApprovals { $0.settle(id, as: .answeredElsewhere) }
            return .settledElsewhere
        } catch {
            approvalBook.cancelAnswer(id)
            let message = error.localizedDescription
            approvalsReconciled = false
            await loadApprovals()
            return .failed(message)
        }
    }

    /// `GET /v1/approvals`: the hub's pending list is the source of truth.
    func loadApprovals() async {
        guard let client else { return }
        let mark = approvalBook.fetchMark()
        do {
            let fetched = try await client.approvals()
            let pending = fetched.map { $0.pending }
            updateApprovals { $0.finishFetch(pending, since: mark) }
            approvalsReconciled = true
        } catch HubClientError.notFound(_) {
            // A hub without the route: events and loaded chats still keep the list.
            approvalsReconciled = online
        } catch {
            // Keep what we have; the dock waits for a successful check before answering.
        }
    }

    func dismissSettledApproval() {
        settleTask.cancel()
        settledApproval = nil
    }

    /// A tapped notification: open its chat with that request in the dock. The
    /// request is looked up in the hub's current list, never rebuilt from the
    /// notification's text.
    func openApprovalRoute(_ route: ApprovalRoute) {
        pendingRoute = route
        guard client != nil, initialLoadDone else { return } // loadInitial picks it up
        Task { await processPendingRoute() }
    }

    private func processPendingRoute() async {
        guard let route = pendingRoute else { return }
        pendingRoute = nil
        await loadApprovals()
        if let approval = pendingApprovals.first(where: { $0.id == route.approvalId }) {
            if selectedConversationId != approval.conversationId { await selectConversation(approval.conversationId) }
            if selectedThreadId != nil, selectedThreadId != approval.threadId { selectedThreadId = nil }
            selectedApprovalId = approval.id
            settledApproval = nil
            chatNotice = nil
            dockFocusRequest = approval.id
        } else {
            if let id = route.conversationId, conversations.contains(where: { $0.id == id }), selectedConversationId != id {
                await selectConversation(id)
            }
            let text: String
            switch approvalBook.outcome(of: route.approvalId) {
            case .answeredHere?: text = "You already answered this request."
            case .some(let outcome): text = outcome.message
            case nil: text = ApprovalOutcome.noLongerWaiting.message
            }
            chatNotice = ChatNotice(conversationId: route.conversationId ?? selectedConversationId, text: text)
        }
        chatRevealRequest += 1
    }

    /// Applies a change to the pending list and keeps the open chat's dock in step:
    /// when the request it showed leaves, the dock says why and moves to the next.
    private func updateApprovals(_ mutate: (inout ApprovalBook) -> ApprovalBook.Change) {
        let conversationId = selectedConversationId
        let queueBefore: [PendingApproval] = conversationId.map { approvals(in: $0) } ?? []
        let shownBefore = ApprovalQueue.displayed(selected: selectedApprovalId, in: queueBefore)
        var book = approvalBook
        let change = mutate(&book)
        approvalBook = book
        switch change {
        case .none:
            break
        case .added(let approval):
            notify(approval)
        case .removed(let ids):
            ApprovalNotifications.remove(approvalIds: ids)
            let removed = Set(ids)
            if let shown = shownBefore, removed.contains(shown.id) {
                let remaining: [PendingApproval] = conversationId.map { approvals(in: $0) } ?? []
                selectedApprovalId = ApprovalQueue.successor(of: shown.id, previousOrder: queueBefore.map { $0.id }, remaining: remaining)
                showSettled(SettledApproval(approval: shown, outcome: approvalBook.outcome(of: shown.id) ?? .noLongerWaiting))
            } else if let selected = selectedApprovalId, removed.contains(selected) {
                selectedApprovalId = nil
            }
        }
    }

    private func showSettled(_ settled: SettledApproval) {
        settledApproval = settled
        var seconds = 4.0
        if case .answeredHere = settled.outcome { seconds = 1.5 }
        let id = settled.id
        settleTask.replace(with: Task { [weak self] in
            try? await Task.sleep(for: .seconds(seconds))
            guard !Task.isCancelled, let self, self.settledApproval?.id == id else { return }
            self.settledApproval = nil
        })
    }

    private func applyApprovalEvent(_ event: Event) {
        guard event.seq > 0 else { return }
        switch event.body {
        case .approvalRequested, .approvalResolved, .threadFinished:
            updateApprovals { $0.apply(event) }
        default:
            break
        }
    }

    /// Fetches the pending list soon (coalesced): after a reconnect, and after a new
    /// request so its timeout, which events don't carry, is known.
    private func scheduleApprovalRefresh() {
        guard !approvalRefreshQueued else { return }
        approvalRefreshQueued = true
        Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(300))
            guard let self else { return }
            self.approvalRefreshQueued = false
            await self.loadApprovals()
        }
    }

    /// A local notification for a new request, unless its chat is on screen in the
    /// foreground (the dock is the notice there). Names only, never input or reason.
    private func notify(_ approval: PendingApproval) {
        scheduleApprovalRefresh()
        if chatViewsOnScreen > 0, selectedConversationId == approval.conversationId, ApprovalNotifications.appIsActive() { return }
        guard ApprovalNotifications.optedIn else { return }
        let id = approval.id
        let conversationId = approval.conversationId
        let threadId = approval.threadId
        let tool = approval.tool
        let deviceId = approval.deviceId
        Task { [weak self] in
            guard let self else { return }
            if !self.conversations.contains(where: { $0.id == conversationId }) { await self.refreshConversations() }
            let body = ApprovalNotificationText.body(tool: tool, device: self.deviceName(deviceId),
                                                     thread: self.threadTitle(threadId), chat: self.chatTitle(conversationId))
            await ApprovalNotifications.post(approvalId: id, conversationId: conversationId, threadId: threadId, body: body)
            // Answered while the notification was being posted: take it back down.
            if self.approvalBook.pending[id] == nil { ApprovalNotifications.remove(approvalIds: [id]) }
        }
    }

    /// Every chat's events, for approvals in chats that aren't open (the open chat
    /// has its own stream) and thread rows. There is no account-wide catch-up route,
    /// so each (re)connect re-fetches the pending list instead.
    private func followAccountEvents() {
        guard let client else { return }
        accountStreamTask.replace(with: Task { [weak self] in
            do {
                for try await update in client.eventStream().updates() {
                    guard let self, !Task.isCancelled else { return }
                    switch update {
                    case .state(let state):
                        if state == .connected { self.scheduleApprovalRefresh() }
                    case .stored(let event):
                        self.handleAccountEvent(event)
                    case .live:
                        break
                    }
                }
            } catch {
                // The open chat's stream reports connection problems.
            }
        })
    }

    private func handleAccountEvent(_ event: Event) {
        switch event.body {
        case .threadStarted, .threadWaiting, .threadHandedOver, .threadFinished:
            ThreadInfo.upsert(&threads, with: event)
        default:
            break
        }
        applyApprovalEvent(event)
    }

    // MARK: - Command palette

    func rememberRecent(_ id: String) {
        paletteRecents = CommandPalette.remember(id, in: paletteRecents)
        UserDefaults.standard.set(paletteRecents, forKey: Self.recentsKey)
    }

    func refreshDevices() async {
        guard let client else { return }
        do { devices = try await client.devices(); online = true }
        catch { online = false; errorMessage = error.localizedDescription }
    }

    func loadTools(deviceId: String) async {
        guard let client else { return }
        do { toolsByDevice[deviceId] = try await client.tools(deviceId: deviceId, includeUnavailable: true) }
        catch { errorMessage = error.localizedDescription }
    }

    func setPermission(deviceId: String, tool: ToolInfo, permission: Permission) async {
        guard let client else { return }
        do {
            _ = try await client.setPermission(permission, deviceId: deviceId, tool: tool.name)
            await loadTools(deviceId: deviceId)
        } catch { errorMessage = error.localizedDescription; await loadTools(deviceId: deviceId) }
    }

    func activity(threadId: String) -> [ThreadActivityRow] {
        guard let conversationId = threads.first(where: { $0.id == threadId })?.conversationId else { return [] }
        return folders[conversationId]?.activity(threadId: threadId) ?? []
    }

    private func followEvents(conversationId: String) {
        guard let client else { return }
        let last = folders[conversationId]?.storedEvents.last?.seq ?? 0
        streamTask.replace(with: Task { [weak self] in
            do {
                for try await update in client.eventStream(conversationId: conversationId).updates(after: last) {
                    guard let self, !Task.isCancelled else { return }
                    switch update {
                    case .state(let state):
                        self.streamState = state
                        self.online = state == .connected
                        if state == .connected { self.scheduleApprovalRefresh() }
                    case .stored(let event), .live(let event): self.ingest(event)
                    }
                }
            } catch {
                guard !Task.isCancelled else { return }
                self?.online = false
                self?.errorMessage = error.localizedDescription
            }
        })
    }

    private func ingest(_ event: Event) {
        var folder = folders[event.conversationId] ?? TranscriptFolder(conversationId: event.conversationId)
        let stored = folder.ingest(event)
        folders[event.conversationId] = folder
        guard stored else { return }
        // Update the thread row now: the views read status, elapsed time and
        // existence from `threads`, and the refresh below may be slow or fail.
        ThreadInfo.upsert(&threads, with: event)
        #if os(iOS)
        switch event.body {
        case .threadStarted(let started):
            if let threadId = event.threadId, !threadId.isEmpty {
                let device = deviceName(started.deviceId)
                Task { try? await LiveActivityCoordinator.start(threadId: threadId, title: started.title, device: device, status: "running", phase: "Running on \(device)") }
            }
        case .threadWaiting(let waiting):
            if let threadId = event.threadId, !threadId.isEmpty {
                let device = deviceName(waiting.deviceId)
                Task { await LiveActivityCoordinator.update(threadId: threadId, status: "waiting_device", phase: "Waiting for \(device)", streamedLine: nil) }
            }
        case .threadCheckpoint:
            if let threadId = event.threadId, !threadId.isEmpty {
                Task { await LiveActivityCoordinator.update(threadId: threadId, status: "waiting_device", phase: "Checkpoint on the hub", streamedLine: nil) }
            }
        case .threadHandedOver(let handover):
            if let threadId = event.threadId, !threadId.isEmpty {
                let device = deviceName(handover.deviceId)
                Task { await LiveActivityCoordinator.update(threadId: threadId, status: "running", phase: "Running on \(device)", streamedLine: nil) }
            }
        case .threadFinished(let status, _):
            if let threadId = event.threadId, !threadId.isEmpty {
                Task { await LiveActivityCoordinator.end(threadId: threadId, status: status) }
            }
        default: break
        }
        #endif
        applyApprovalEvent(event)
        switch event.body {
        case .threadStarted, .threadWaiting, .threadHandedOver, .threadFinished:
            let conversationId = event.conversationId
            Task { [weak self] in await self?.refreshThreads(conversationId: conversationId) }
        default: break
        }
    }

    private func deviceName(_ id: String) -> String { devices.first { $0.id == id }?.name ?? id }
}
