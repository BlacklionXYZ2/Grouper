import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

public enum HubClientError: Error, Equatable, Sendable, LocalizedError {
    case invalidBaseURL
    case unauthorized(String)
    case forbidden(String)
    case notFound(String)
    case conflict(String)
    case badRequest(String)
    case server(status: Int, message: String)
    case transport(String)
    case decoding(String)

    public var errorDescription: String? {
        switch self {
        case .invalidBaseURL: return "The hub URL is invalid."
        case .unauthorized(let m), .forbidden(let m), .notFound(let m), .conflict(let m), .badRequest(let m): return m
        case .server(let status, let message): return "Hub error \(status): \(message)"
        case .transport(let message): return message
        case .decoding(let message): return "The hub returned an unexpected response: \(message)"
        }
    }
}

public struct ChatResponse: Codable, Equatable, Sendable { public let conversationId: String }
public struct MutationResponse: Codable, Equatable, Sendable {
    public let sent: Bool?
    public let stopped: Bool?
    public let saved: Bool?
    public let approved: Bool?
    public let approvalId: String?
    public let deleted: JSONValue?
}
public struct ToolCallResponse: Codable, Equatable, Sendable { public let ok: Bool; public let output: String }
public struct ModelImportResult: Codable, Equatable, Sendable { public let model: ModelEntry; public let proposalId: Int64? }

public struct ModelImportRequest: Codable, Equatable, Sendable {
    public let provider: String; public let model: String; public let id: String?; public let roles: [String]?; public let local: Bool?
    public init(provider: String, model: String, id: String? = nil, roles: [String]? = nil, local: Bool? = nil) {
        self.provider = provider; self.model = model; self.id = id; self.roles = roles; self.local = local
    }
}

public actor HubClient {
    public nonisolated let baseURL: URL
    private nonisolated let token: String
    private let session: URLSession
    private let encoder = JSONEncoder.grouper
    private let decoder = JSONDecoder.grouper

    public init(baseURL: URL, token: String, session: URLSession = .shared) throws {
        guard let scheme = baseURL.scheme?.lowercased(), scheme == "http" || scheme == "https", baseURL.host != nil else {
            throw HubClientError.invalidBaseURL
        }
        self.baseURL = baseURL
        self.token = token
        self.session = session
    }

    public init(baseURL: String, token: String, session: URLSession = .shared) throws {
        guard let url = URL(string: baseURL.trimmingCharacters(in: .whitespacesAndNewlines)) else { throw HubClientError.invalidBaseURL }
        try self.init(baseURL: url, token: token, session: session)
    }

    public nonisolated func eventStream(conversationId: String? = nil) -> EventStream {
        EventStream(baseURL: baseURL, token: token, conversationId: conversationId)
    }

    public func health() async throws {
        let (data, _) = try await perform(path: ["healthz"], authenticated: false)
        guard String(decoding: data, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines) == "ok" else {
            throw HubClientError.decoding("health check did not return ok")
        }
    }

    public func hubInfo() async throws -> HubInfo { try await get(["v1", "hub"], authenticated: false) }
    public func providers() async throws -> [ProviderInfo] { try await get(["v1", "providers"]) }
    public func conversations() async throws -> [ConversationInfo] { try await get(["v1", "conversations"]) }
    public func devices() async throws -> [DeviceInfo] { try await get(["v1", "devices"]) }
    public func models() async throws -> [ModelEntry] { try await get(["v1", "models"]) }
    public func registryStats() async throws -> RegistryStats { try await get(["v1", "registry", "stats"]) }
    public func limits() async throws -> LimitsReport { try await get(["v1", "limits"]) }
    public func usageSnapshots() async throws -> [UsageSnapshot] { try await get(["v1", "usage", "snapshots"]) }

    public func events(conversationId: String, after: Int64 = 0, threadId: String? = nil) async throws -> [Event] {
        var query = [URLQueryItem(name: "after", value: String(after))]
        if let threadId { query.append(URLQueryItem(name: "thread", value: threadId)) }
        return try await get(["v1", "conversations", conversationId, "events"], query: query)
    }

    public func threads(conversationId: String? = nil) async throws -> [ThreadInfo] {
        try await get(["v1", "threads"], query: conversationId.map { [URLQueryItem(name: "conversation", value: $0)] } ?? [])
    }

    public func thread(id: String) async throws -> ThreadInfo { try await get(["v1", "threads", id]) }

    /// Pending approvals across every chat (admin only), oldest first.
    public func approvals(conversationId: String? = nil) async throws -> [HubApproval] {
        let query = conversationId.map { [URLQueryItem(name: "conversation", value: $0)] } ?? []
        return try await get(["v1", "approvals"], query: query)
    }

    public func tools(deviceId: String? = nil, includeUnavailable: Bool = false) async throws -> [ToolInfo] {
        var query: [URLQueryItem] = []
        if let deviceId { query.append(URLQueryItem(name: "device", value: deviceId)) }
        if includeUnavailable { query.append(URLQueryItem(name: "all", value: "true")) }
        return try await get(["v1", "tools"], query: query)
    }

    public func usage(since: String? = nil, until: String? = nil, group: String = "day", conversationId: String? = nil) async throws -> [UsageTotal] {
        var query = [URLQueryItem(name: "group", value: group)]
        if let since { query.append(URLQueryItem(name: "since", value: since)) }
        if let until { query.append(URLQueryItem(name: "until", value: until)) }
        if let conversationId { query.append(URLQueryItem(name: "conversation", value: conversationId)) }
        return try await get(["v1", "usage"], query: query)
    }

    public func memories(query text: String? = nil, limit: Int = 50) async throws -> [MemoryEntry] {
        var query = [URLQueryItem(name: "limit", value: String(limit))]
        if let text, !text.isEmpty { query.append(URLQueryItem(name: "q", value: text)) }
        return try await get(["v1", "memories"], query: query)
    }

    public func skills(query text: String? = nil, limit: Int = 50) async throws -> [Skill] {
        var query = [URLQueryItem(name: "limit", value: String(limit))]
        if let text, !text.isEmpty { query.append(URLQueryItem(name: "q", value: text)) }
        return try await get(["v1", "skills"], query: query)
    }

    public func skill(id: Int64) async throws -> Skill { try await get(["v1", "skills", String(id)]) }
    public func proposals(status: String = "pending") async throws -> [RegistryProposal] {
        try await get(["v1", "registry", "proposals"], query: [URLQueryItem(name: "status", value: status)])
    }
    public func rules() async throws -> [PriorityRule] { try await get(["v1", "registry", "rules"]) }

    @discardableResult
    public func sendChat(text: String, conversationId: String? = nil, mode: String? = nil, model: String? = nil) async throws -> ChatResponse {
        try await send(["v1", "chat"], method: "POST", body: ChatRequest(conversationId: conversationId, mode: mode, text: text, model: model))
    }

    public func setConversationModel(_ model: String?, conversationId: String) async throws -> ConversationInfo {
        try await send(["v1", "conversations", conversationId, "model"], method: "POST", body: ConversationModelRequest(model: model))
    }

    @discardableResult public func messageThread(id: String, text: String) async throws -> MutationResponse {
        try await send(["v1", "threads", id, "message"], method: "POST", body: TextRequest(text: text))
    }
    @discardableResult public func stopThread(id: String) async throws -> MutationResponse {
        try await send(["v1", "threads", id, "stop"], method: "POST", body: EmptyRequest())
    }
    @discardableResult public func rateThread(id: String, rating: String) async throws -> JSONValue {
        try await send(["v1", "threads", id, "rating"], method: "POST", body: RatingRequest(rating: rating))
    }
    @discardableResult public func resolveApproval(id: String, approve: Bool) async throws -> MutationResponse {
        try await send(["v1", "approvals", id], method: "POST", body: ApprovalDecision(approve: approve))
    }
    @discardableResult public func setPermission(_ permission: Permission, deviceId: String, tool: String) async throws -> JSONValue {
        try await send(["v1", "devices", deviceId, "tools", tool], method: "PUT", body: PermissionRequest(permission: permission))
    }
    /// Registers a plain (non-admin) device; needs an admin token. The returned token is
    /// shown only once. A hub without the route answers 404 or 405
    /// (`DeviceRegistration.hubLacksRegistration`).
    public func registerDevice(name: String, platform: String) async throws -> RegisteredDevice {
        try await send(["v1", "devices"], method: "POST", body: DeviceRegistrationRequest(name: name, platform: platform))
    }
    /// Revokes a device's token on the hub (admin). 404 when the hub doesn't know it, or it
    /// was already revoked; 409 for the token making the request.
    @discardableResult public func revokeDevice(id: String) async throws -> DeviceRevocation {
        try await delete(["v1", "devices", id])
    }
    public func callDevice(id: String, tool: String, args: JSONValue = .object([:])) async throws -> ToolCallResponse {
        try await send(["v1", "devices", id, "call"], method: "POST", body: ToolCallRequest(tool: tool, args: args))
    }
    public func saveProvider(_ provider: ProviderConfig) async throws -> JSONValue {
        try await send(["v1", "providers", provider.id], method: "PUT", body: provider)
    }
    public func saveModel(_ model: ModelEntry) async throws -> ModelEntry {
        try await send(["v1", "models", model.id], method: "PUT", body: model)
    }
    public func importModel(_ request: ModelImportRequest) async throws -> ModelImportResult {
        try await send(["v1", "models", "import"], method: "POST", body: request)
    }
    public func refreshModelPrices(id: String) async throws -> ModelEntry {
        try await send(["v1", "models", id, "refresh-prices"], method: "POST", body: EmptyRequest())
    }
    public func decideProposal(id: Int64, accept: Bool) async throws -> RegistryProposal {
        try await send(["v1", "registry", "proposals", String(id)], method: "POST", body: ProposalDecision(accept: accept))
    }
    public func addRule(_ input: RuleInput) async throws -> PriorityRule {
        try await send(["v1", "registry", "rules"], method: "POST", body: input)
    }
    @discardableResult public func setRule(id: Int64, enabled: Bool) async throws -> JSONValue {
        try await send(["v1", "registry", "rules", String(id)], method: "POST", body: RuleEnabled(enabled: enabled))
    }
    public func addMemory(_ input: MemoryInput) async throws -> MemoryEntry { try await send(["v1", "memories"], method: "POST", body: input) }
    public func updateMemory(id: Int64, input: MemoryInput) async throws -> MemoryEntry { try await send(["v1", "memories", String(id)], method: "PUT", body: input) }
    public func updateSkill(id: Int64, input: SkillInput) async throws -> Skill { try await send(["v1", "skills", String(id)], method: "PUT", body: input) }
    public func setLimits(_ values: [String: JSONValue]) async throws -> LimitsReport { try await send(["v1", "limits"], method: "PUT", body: values) }

    public func refreshUsage() async throws -> JSONValue {
        try await send(["v1", "usage", "refresh"], method: "POST", body: EmptyRequest())
    }

    public func workspaceFiles(threadId: String) async throws -> WorkspaceListing { try await get(["v1", "threads", threadId, "files"]) }
    public func workspaceFile(threadId: String, path: String) async throws -> Data {
        let pieces = path.split(separator: "/").map(String.init)
        return try await perform(path: ["v1", "threads", threadId, "files"] + pieces).0
    }

    @discardableResult public func deleteModel(id: String) async throws -> JSONValue { try await delete(["v1", "models", id]) }
    @discardableResult public func deleteMemory(id: Int64) async throws -> JSONValue { try await delete(["v1", "memories", String(id)]) }
    @discardableResult public func deleteSkill(id: Int64) async throws -> JSONValue { try await delete(["v1", "skills", String(id)]) }
    @discardableResult public func deleteRule(id: Int64) async throws -> JSONValue { try await delete(["v1", "registry", "rules", String(id)]) }

    private func get<T: Decodable>(_ path: [String], query: [URLQueryItem] = [], authenticated: Bool = true) async throws -> T {
        let (data, _) = try await perform(path: path, query: query, authenticated: authenticated)
        do { return try decoder.decode(T.self, from: data) }
        catch { throw HubClientError.decoding(String(describing: error)) }
    }

    private func send<T: Decodable, Body: Encodable>(_ path: [String], method: String, body: Body) async throws -> T {
        let encoded: Data
        do { encoded = try encoder.encode(body) }
        catch { throw HubClientError.decoding(String(describing: error)) }
        let (data, _) = try await perform(path: path, method: method, body: encoded)
        do { return try decoder.decode(T.self, from: data) }
        catch { throw HubClientError.decoding(String(describing: error)) }
    }

    private func delete<T: Decodable>(_ path: [String]) async throws -> T {
        let (data, _) = try await perform(path: path, method: "DELETE")
        do { return try decoder.decode(T.self, from: data) }
        catch { throw HubClientError.decoding(String(describing: error)) }
    }

    private func perform(path: [String], query: [URLQueryItem] = [], method: String = "GET", body: Data? = nil, authenticated: Bool = true) async throws -> (Data, HTTPURLResponse) {
        var url = baseURL
        for component in path { url.appendPathComponent(component) }
        if !query.isEmpty {
            guard var components = URLComponents(url: url, resolvingAgainstBaseURL: false) else { throw HubClientError.invalidBaseURL }
            components.queryItems = query
            guard let built = components.url else { throw HubClientError.invalidBaseURL }
            url = built
        }
        var request = URLRequest(url: url)
        request.httpMethod = method
        request.timeoutInterval = 30
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if authenticated { request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization") }
        if let body { request.httpBody = body; request.setValue("application/json", forHTTPHeaderField: "Content-Type") }
        let data: Data
        let response: URLResponse
        do { (data, response) = try await session.data(for: request) }
        catch { throw HubClientError.transport(String(describing: error)) }
        guard let http = response as? HTTPURLResponse else { throw HubClientError.transport("The hub did not return HTTP.") }
        guard (200..<300).contains(http.statusCode) else { throw error(status: http.statusCode, data: data) }
        return (data, http)
    }

    private func error(status: Int, data: Data) -> HubClientError {
        let message = (try? decoder.decode(ErrorEnvelope.self, from: data).error) ?? String(decoding: data, as: UTF8.self)
        switch status {
        case 400: return .badRequest(message)
        case 401: return .unauthorized(message)
        case 403: return .forbidden(message)
        case 404: return .notFound(message)
        case 409: return .conflict(message)
        default: return .server(status: status, message: message)
        }
    }
}

private struct ErrorEnvelope: Codable { let error: String }
private struct EmptyRequest: Codable {}
private struct ChatRequest: Codable { let conversationId: String?; let mode: String?; let text: String; let model: String? }
private struct ConversationModelRequest: Codable { let model: String? }
private struct TextRequest: Codable { let text: String }
private struct RatingRequest: Codable { let rating: String }
private struct ApprovalDecision: Codable { let approve: Bool }
private struct PermissionRequest: Codable { let permission: Permission }
private struct ToolCallRequest: Codable { let tool: String; let args: JSONValue }
private struct DeviceRegistrationRequest: Codable { let name: String; let platform: String }
private struct ProposalDecision: Codable { let accept: Bool }
private struct RuleEnabled: Codable { let enabled: Bool }
