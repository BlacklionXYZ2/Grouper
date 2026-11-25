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

@MainActor
final class AppModel: ObservableObject {
    @Published var hubURL = ""
    @Published var adminToken = ""
    @Published var deviceToken = ""
    @Published private(set) var client: HubClient?
    @Published private(set) var signedIn = false
    @Published private(set) var online = false
    @Published var errorMessage: String?

    @Published private(set) var conversations: [ConversationInfo] = []
    @Published var selectedConversationId: String?
    @Published private(set) var folders: [String: TranscriptFolder] = [:]
    @Published private(set) var threads: [ThreadInfo] = []
    @Published private(set) var models: [ModelEntry] = []
    @Published private(set) var devices: [DeviceInfo] = []
    @Published private(set) var toolsByDevice: [String: [ToolInfo]] = [:]
    @Published private(set) var streamState: EventStreamState = .disconnected
    @Published var selectedThreadId: String?
    @Published var selectedApprovalId: String?
    @Published var selectedDeviceId: String?
    @Published var threadMessageDrafts: [String: String] = [:]
    @Published var newChatMode = "project"

    private let streamTask = TaskBox()

    init() {
        hubURL = UserDefaults.standard.string(forKey: "grouper.hubURL") ?? ""
        adminToken = KeychainStore.read(.admin) ?? ""
        deviceToken = KeychainStore.read(.device) ?? ""
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

    var pendingApprovals: [PendingApproval] {
        folders.values.flatMap { folder in
            folder.pendingApprovals(threads: threads.filter { $0.conversationId == folder.conversationId })
        }.sorted { $0.seq < $1.seq }
    }

    var selectedThread: ThreadInfo? { threads.first { $0.id == selectedThreadId } }
    var selectedApproval: PendingApproval? { pendingApprovals.first { $0.id == selectedApprovalId } }

    func connect(save: Bool = true) async {
        errorMessage = nil
        let trimmedHub = hubURL.trimmingCharacters(in: .whitespacesAndNewlines)
        let trimmedAdmin = adminToken.trimmingCharacters(in: .whitespacesAndNewlines)
        let trimmedDevice = deviceToken.trimmingCharacters(in: .whitespacesAndNewlines)
        do {
            let api = try HubClient(baseURL: trimmedHub, token: trimmedAdmin)
            try await api.health()
            _ = try await api.providers() // proves this is the admin token, not merely a valid device token
            #if os(macOS)
            if !trimmedDevice.isEmpty {
                try await verifyDeviceToken(trimmedDevice, hubURL: trimmedHub)
            }
            #endif
            hubURL = trimmedHub
            adminToken = trimmedAdmin
            deviceToken = trimmedDevice
            client = api
            signedIn = true
            online = true
            if save {
                UserDefaults.standard.set(trimmedHub, forKey: "grouper.hubURL")
                try KeychainStore.save(trimmedAdmin, as: .admin)
                if !trimmedDevice.isEmpty { try KeychainStore.save(trimmedDevice, as: .device) }
            }
            await loadInitial()
        } catch {
            signedIn = false
            online = false
            errorMessage = error.localizedDescription
        }
    }

    #if os(macOS)
    func saveDeviceToken() async -> Bool {
        errorMessage = nil
        let trimmedDevice = deviceToken.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedDevice.isEmpty else { return false }
        do {
            try await verifyDeviceToken(trimmedDevice, hubURL: hubURL)
            deviceToken = trimmedDevice
            try KeychainStore.save(trimmedDevice, as: .device)
            return true
        } catch {
            errorMessage = error.localizedDescription
            return false
        }
    }

    private func verifyDeviceToken(_ token: String, hubURL: String) async throws {
        let deviceAPI = try HubClient(baseURL: hubURL, token: token)
        do {
            _ = try await deviceAPI.providers()
            throw HubClientError.badRequest("The device field contains an admin token. Use a separate non-admin device token for the Mac node.")
        } catch HubClientError.forbidden(_) {
            // Expected: a device token can read, but cannot access providers.
        }
    }
    #endif

    func signOut() {
        streamTask.cancel()
        client = nil
        signedIn = false
        online = false
        adminToken = ""
        errorMessage = nil
        conversations.removeAll()
        threads.removeAll()
        folders.removeAll()
        selectedConversationId = nil
        selectedThreadId = nil
        selectedApprovalId = nil
        streamState = .disconnected
        KeychainStore.delete(.admin)
    }

    func forgetDeviceToken() {
        deviceToken = ""
        KeychainStore.delete(.device)
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
        } catch {
            online = false
            errorMessage = error.localizedDescription
        }
    }

    func selectConversation(_ id: String) async {
        guard let client else { return }
        selectedConversationId = id
        streamTask.cancel()
        do {
            async let events = client.events(conversationId: id)
            async let scopedThreads = client.threads(conversationId: id)
            let fetchedEvents = try await events
            let scoped = try await scopedThreads
            guard selectedConversationId == id else { return }
            folders[id] = TranscriptFolder(conversationId: id, events: fetchedEvents)
            threads.removeAll { $0.conversationId == id }
            threads.append(contentsOf: scoped)
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

    func refreshConversations() async {
        guard let client else { return }
        do { conversations = try await client.conversations().sorted { $0.created > $1.created } }
        catch { errorMessage = error.localizedDescription }
    }

    func beginNewChat(mode: String) {
        streamTask.cancel()
        selectedConversationId = nil
        selectedThreadId = nil
        newChatMode = mode
    }

    func messageThread(_ id: String, text: String) async -> Bool {
        guard let client else { return false }
        do { _ = try await client.messageThread(id: id, text: text); threadMessageDrafts[id] = ""; return true }
        catch { errorMessage = error.localizedDescription; return false }
    }

    func stopThread(_ id: String) async {
        guard let client else { return }
        do { _ = try await client.stopThread(id: id) }
        catch { errorMessage = error.localizedDescription }
    }

    func rateThread(_ id: String, rating: String) async {
        guard let client else { return }
        do { _ = try await client.rateThread(id: id, rating: rating) }
        catch { errorMessage = error.localizedDescription }
    }

    @discardableResult
    func resolveApproval(_ id: String, approve: Bool) async -> Bool {
        guard let client else { return false }
        do {
            _ = try await client.resolveApproval(id: id, approve: approve)
            selectedApprovalId = nil
            return true
        } catch HubClientError.notFound(_) {
            selectedApprovalId = nil
            errorMessage = "Answered on another device."
            return true
        } catch {
            errorMessage = error.localizedDescription
            return false
        }
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
                    case .state(let state): self.streamState = state; self.online = state == .connected
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
        case .approvalRequested(let request):
            let threadTitle = threads.first { $0.id == event.threadId }?.title ?? "Thread"
            let device = deviceName(request.deviceId)
            Task { await ApprovalNotificationCoordinator.post(request, threadTitle: threadTitle, deviceName: device) }
        case .approvalResolved(let approvalId, _):
            ApprovalNotificationCoordinator.remove(approvalId: approvalId)
        default: break
        }
        #endif
        switch event.body {
        case .threadStarted, .threadWaiting, .threadHandedOver, .threadFinished:
            Task { [weak self] in
                guard let self, let client = self.client else { return }
                if let updated = try? await client.threads(conversationId: event.conversationId) {
                    self.threads.removeAll { $0.conversationId == event.conversationId }
                    self.threads.append(contentsOf: updated)
                }
            }
        default: break
        }
    }

    private func deviceName(_ id: String) -> String { devices.first { $0.id == id }?.name ?? id }
}
