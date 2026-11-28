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
    /// The relay (not the hub) refused: its sign-in is missing or expired. Sign in to the relay again.
    case relayUnauthorized(String)
    /// The relay is up but the hub behind it isn't connected ("Your hub is offline").
    case hubOffline(String)
    case transport(String)
    case decoding(String)

    public var errorDescription: String? {
        switch self {
        case .invalidBaseURL: return "The hub URL is invalid."
        case .unauthorized(let m), .forbidden(let m), .notFound(let m), .conflict(let m), .badRequest(let m): return m
        case .relayUnauthorized: return "Your sign-in to the Grouper relay expired. Sign in again."
        case .hubOffline: return "Your hub is offline. Check that Grouper is running on the computer that hosts it."
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
    /// Sent as `Grouper-Relay` on every request (and the event stream's handshake) when the
    /// hub address is a relay. Nil for a plain hub.
    public nonisolated let relayToken: String?
    private let session: URLSession
    private let encoder = JSONEncoder.grouper
    private let decoder = JSONDecoder.grouper

    public init(baseURL: URL, token: String, relayToken: String? = nil, session: URLSession = .shared) throws {
        guard let scheme = baseURL.scheme?.lowercased(), scheme == "http" || scheme == "https", baseURL.host != nil else {
            throw HubClientError.invalidBaseURL
        }
        self.baseURL = baseURL
        self.token = token
        self.relayToken = relayToken.flatMap { $0.isEmpty ? nil : $0 }
        self.session = session
    }

    public init(baseURL: String, token: String, relayToken: String? = nil, session: URLSession = .shared) throws {
        guard let url = URL(string: baseURL.trimmingCharacters(in: .whitespacesAndNewlines)) else { throw HubClientError.invalidBaseURL }
        try self.init(baseURL: url, token: token, relayToken: relayToken, session: session)
    }

    /// A client with no token, for the calls a signed-out app makes (`authStatus`,
    /// `googleNonce`, `signInWithGoogle`). Authenticated calls on it get 401.
    public init(baseURL: String, relayToken: String? = nil, session: URLSession = .shared) throws {
        try self.init(baseURL: baseURL, token: "", relayToken: relayToken, session: session)
    }

    public nonisolated func eventStream(conversationId: String? = nil) -> EventStream {
        EventStream(baseURL: baseURL, token: token, conversationId: conversationId, relayToken: relayToken)
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

    /// `GET /v1/usage/summary` (docs/usage-tab-spec.md 7.2): the donut and key list for `[since, until)`,
    /// as Unix seconds (the client works out local midnights, see `UsagePeriod.range`).
    public func usageSummary(since: Int64, until: Int64, conversationId: String? = nil) async throws -> UsageSummary {
        var query = [URLQueryItem(name: "since", value: String(since)), URLQueryItem(name: "until", value: String(until))]
        if let conversationId { query.append(URLQueryItem(name: "conversation", value: conversationId)) }
        return try await get(["v1", "usage", "summary"], query: query)
    }

    /// `PUT /v1/providers/{id}/color`: gives a key one of the ten colour names (`KeyColorName`), or
    /// nil to let the hub pick one again. 400 for a name outside the palette, 404 for an unknown key.
    public func setProviderColor(id: String, color: String?) async throws -> ProviderInfo {
        try await send(["v1", "providers", id, "color"], method: "PUT", body: ProviderColorRequest(color: color))
    }

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
    /// (`DeviceRegistration.hubLacksRegistration`). With `installId`, a hub that already has a
    /// live device for it issues that device a fresh token (revoking the old one) instead of
    /// adding a device; older hubs ignore it. An id the hub wouldn't accept is left out.
    public func registerDevice(name: String, platform: String, installId: String? = nil) async throws -> RegisteredDevice {
        try await send(["v1", "devices"], method: "POST",
                       body: DeviceRegistrationRequest(name: name, platform: platform, installId: InstallID.sendable(installId)))
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

    // MARK: Sign in with Google (docs/accounts-design.md)

    /// `GET /v1/auth/status` (no token): whether the hub is claimed and allows Google sign-in.
    public func authStatus() async throws -> AuthStatus { try await get(["v1", "auth", "status"], authenticated: false) }

    /// `POST /v1/auth/google/nonce` (no token): a one-time nonce to put in the Google request.
    /// A nonce that isn't 32 lowercase hex characters is refused: the app never sends anything
    /// else to Google as a hub nonce, so a hub can't pass off a relay nonce as its own.
    public func googleNonce() async throws -> GoogleNonce {
        let nonce: GoogleNonce = try await send(["v1", "auth", "google", "nonce"], method: "POST", body: EmptyRequest(), authenticated: false)
        guard RelayNonce.isHubNonce(nonce.nonce) else {
            throw HubClientError.badRequest("The hub sent a sign-in nonce this app won't use.")
        }
        return nonce
    }

    /// `POST /v1/auth/google` (no token): trades a Google ID token (with this hub's nonce) for a
    /// new admin token for this app install. An unclaimed hub needs `setupCode`. 401: bad token
    /// or nonce; 403: setup code missing or wrong, or another Google account; 409: just claimed.
    /// With `installId` (`InstallID`), signing in again from the same install reuses its device
    /// (same id, fresh token, old token revoked); older hubs ignore it. An id the hub wouldn't
    /// accept is left out.
    public func signInWithGoogle(idToken: String, setupCode: String? = nil, client: GoogleSignInClient,
                                 deviceName: String? = nil, installId: String? = nil) async throws -> GoogleSignInResult {
        let code = setupCode?.trimmingCharacters(in: .whitespacesAndNewlines)
        let body = GoogleSignInRequest(idToken: idToken, setupCode: code?.isEmpty == false ? code : nil,
                                       client: client, deviceName: deviceName, installId: InstallID.sendable(installId))
        return try await send(["v1", "auth", "google"], method: "POST", body: body, authenticated: false)
    }

    /// `GET /v1/auth/google/link` (admin): the Google account that owns the hub, if any.
    public func linkedGoogleAccount() async throws -> LinkedGoogleAccount? {
        let response: GoogleLinkResponse = try await get(["v1", "auth", "google", "link"])
        return response.owner
    }

    /// `POST /v1/auth/google/link` (admin): makes the ID token's Google account the hub's owner.
    /// 409 when another account is linked.
    @discardableResult
    public func linkGoogle(idToken: String) async throws -> LinkedGoogleAccount? {
        let response: GoogleLinkResponse = try await send(["v1", "auth", "google", "link"], method: "POST",
                                                          body: GoogleLinkRequest(idToken: idToken))
        return response.owner
    }

    /// `DELETE /v1/auth/google/link` (admin): unlinks the owner; tokens keep working. False when none was linked.
    @discardableResult
    public func unlinkGoogle() async throws -> Bool {
        let response: GoogleUnlinkResponse = try await delete(["v1", "auth", "google", "link"])
        return response.unlinked
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

    private func send<T: Decodable, Body: Encodable>(_ path: [String], method: String, body: Body, authenticated: Bool = true) async throws -> T {
        let encoded: Data
        do { encoded = try encoder.encode(body) }
        catch { throw HubClientError.decoding(String(describing: error)) }
        let (data, _) = try await perform(path: path, method: method, body: encoded, authenticated: authenticated)
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
        if authenticated, !token.isEmpty { request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization") }
        // Every call to a relay address carries the relay token, signed-out calls included.
        if let relayToken { request.setValue(relayToken, forHTTPHeaderField: RelayHeader.name) }
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
        HubClientError.map(status: status, data: data)
    }
}

private struct EmptyRequest: Codable {}
private struct ChatRequest: Codable { let conversationId: String?; let mode: String?; let text: String; let model: String? }
private struct ConversationModelRequest: Codable { let model: String? }
private struct TextRequest: Codable { let text: String }
private struct RatingRequest: Codable { let rating: String }
private struct ApprovalDecision: Codable { let approve: Bool }
private struct PermissionRequest: Codable { let permission: Permission }
private struct ToolCallRequest: Codable { let tool: String; let args: JSONValue }
private struct DeviceRegistrationRequest: Codable { let name: String; let platform: String; let installId: String? }
/// `{"color": "purple"}`, or `{"color": null}` to re-run the hub's automatic choice (nil must be sent, not left out).
private struct ProviderColorRequest: Encodable {
    let color: String?
    enum CodingKeys: String, CodingKey { case color }
    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        if let color { try container.encode(color, forKey: .color) } else { try container.encodeNil(forKey: .color) }
    }
}
private struct ProposalDecision: Codable { let accept: Bool }
private struct RuleEnabled: Codable { let enabled: Bool }
