import Foundation

public struct ConversationInfo: Codable, Equatable, Identifiable, Sendable {
    public let id: String
    public let mode: String
    public let title: String
    public let created: UInt64
    public let model: String?
}

public struct ThreadInfo: Codable, Equatable, Identifiable, Sendable {
    public let id: String
    public let conversationId: String
    public let title: String
    public let deviceId: String
    public let model: String
    public let status: String
    public let reason: String?
    public let summary: String?
    public let created: UInt64
    public let finished: UInt64?

    public var isTerminal: Bool { ["done", "failed", "stopped"].contains(status) }
}

public struct ToolSummary: Codable, Equatable, Sendable {
    public let name: String
    public let description: String
}

public enum FolderMode: String, Codable, Equatable, Sendable { case read, readWrite = "read_write" }

public struct FolderInfo: Codable, Equatable, Sendable {
    public let name: String
    public let mode: FolderMode
}

public struct DeviceInfo: Codable, Equatable, Identifiable, Sendable {
    public let id: String
    public let name: String
    public let platform: String
    public let online: Bool
    public let capabilities: [ToolSummary]
    public let lastSeen: UInt64?
    public let folders: [FolderInfo]

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        name = try c.decode(String.self, forKey: .name)
        platform = try c.decode(String.self, forKey: .platform)
        online = try c.decode(Bool.self, forKey: .online)
        capabilities = try c.decodeIfPresent([ToolSummary].self, forKey: .capabilities) ?? []
        lastSeen = try c.decodeIfPresent(UInt64.self, forKey: .lastSeen)
        folders = try c.decodeIfPresent([FolderInfo].self, forKey: .folders) ?? []
    }
}

public enum Permission: String, Codable, CaseIterable, Equatable, Sendable {
    case manual, auto, bypass
}

public struct ToolInfo: Codable, Equatable, Identifiable, Sendable {
    public let deviceId: String
    public let name: String
    public let description: String
    public let permission: Permission
    public let available: Bool
    public let inputSchema: JSONValue
    public var id: String { "\(deviceId):\(name)" }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        deviceId = try c.decode(String.self, forKey: .deviceId)
        name = try c.decode(String.self, forKey: .name)
        description = try c.decode(String.self, forKey: .description)
        permission = try c.decode(Permission.self, forKey: .permission)
        available = try c.decodeIfPresent(Bool.self, forKey: .available) ?? true
        inputSchema = try c.decodeIfPresent(JSONValue.self, forKey: .inputSchema) ?? .object(["type": .string("object"), "properties": .object([:])])
    }
}

public struct ProviderInfo: Codable, Equatable, Identifiable, Sendable {
    public let id: String
    public let kind: String
    public let baseUrl: String?
    public let hasKey: Bool
}

public struct ProviderConfig: Codable, Equatable, Identifiable, Sendable {
    public let id: String
    public let kind: String
    public let baseUrl: String?
    public let apiKey: String?
    public let options: JSONValue

    public init(id: String, kind: String, baseUrl: String? = nil, apiKey: String? = nil, options: JSONValue = .object([:])) {
        self.id = id; self.kind = kind; self.baseUrl = baseUrl; self.apiKey = apiKey; self.options = options
    }
}

public struct ModelEntry: Codable, Equatable, Identifiable, Sendable {
    public let id: String
    public let providerId: String
    public let model: String
    public let inputPrice: Double
    public let outputPrice: Double
    public let cacheReadPrice: Double?
    public let cacheWritePrice: Double?
    public let contextWindow: UInt64
    public let useCases: String
    public let free: Bool
    public let roles: [String]
    public let local: Bool
    public let modalities: [String]

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        providerId = try c.decode(String.self, forKey: .providerId)
        model = try c.decode(String.self, forKey: .model)
        inputPrice = try c.decodeIfPresent(Double.self, forKey: .inputPrice) ?? 0
        outputPrice = try c.decodeIfPresent(Double.self, forKey: .outputPrice) ?? 0
        cacheReadPrice = try c.decodeIfPresent(Double.self, forKey: .cacheReadPrice)
        cacheWritePrice = try c.decodeIfPresent(Double.self, forKey: .cacheWritePrice)
        contextWindow = try c.decodeIfPresent(UInt64.self, forKey: .contextWindow) ?? 0
        useCases = try c.decodeIfPresent(String.self, forKey: .useCases) ?? ""
        free = try c.decodeIfPresent(Bool.self, forKey: .free) ?? false
        roles = try c.decodeIfPresent([String].self, forKey: .roles) ?? []
        local = try c.decodeIfPresent(Bool.self, forKey: .local) ?? false
        modalities = try c.decodeIfPresent([String].self, forKey: .modalities) ?? []
    }
}

public struct OutcomeStats: Codable, Equatable, Sendable {
    public let subject: String
    public let taskType: String
    public let done: UInt64
    public let failed: UInt64
    public let stopped: UInt64
    public let successes: UInt64
    public let successRate: Double?
    public let medianDurationS: Double?
    public let meanCost: Double?
    public let meanGrade: Double?
    public let netRating: Int64
}

public struct RegistryStats: Codable, Equatable, Sendable {
    public let models: [OutcomeStats]
    public let devices: [OutcomeStats]
}

public struct RegistryProposal: Codable, Equatable, Identifiable, Sendable {
    public let id: Int64
    public let modelId: String
    public let field: String
    public let text: String
    public let reason: String
    public let status: String
    public let source: String
    public let created: UInt64
    public let decided: UInt64?
}

public struct RuleSpec: Codable, Equatable, Sendable {
    public let whenTag: String?
    public let whenTaskType: String?
    public let require: String?
    public let forbid: String?
    public let prefer: String?

    public init(whenTag: String? = nil, whenTaskType: String? = nil, require: String? = nil, forbid: String? = nil, prefer: String? = nil) {
        self.whenTag = whenTag; self.whenTaskType = whenTaskType; self.require = require; self.forbid = forbid; self.prefer = prefer
    }
}

public struct PriorityRule: Codable, Equatable, Identifiable, Sendable {
    public let id: Int64
    public let text: String
    public let rule: RuleSpec
    public let enabled: Bool
    public let created: UInt64
}

public struct RuleInput: Codable, Equatable, Sendable {
    public let text: String
    public let rule: RuleSpec
    public init(text: String, rule: RuleSpec) { self.text = text; self.rule = rule }
}

public struct UsageTotal: Codable, Equatable, Sendable {
    public let key: String
    public let calls: UInt64
    public let inputTokens: UInt64
    public let outputTokens: UInt64
    public let cacheReadTokens: UInt64
    public let cacheWriteTokens: UInt64
    public let cost: Double
}

public struct UsageSnapshot: Codable, Equatable, Sendable {
    public let account: String
    public let provider: String
    public let kind: String
    public let window: String
    public let usedPct: Double?
    public let usedAmount: Double?
    public let limitAmount: Double?
    public let currency: String?
    public let resetsAt: UInt64?
    public let source: String
    public let deviceId: String?
    public let fetchedAt: UInt64
}

public enum ApprovalTimeoutAction: String, Codable, Equatable, Sendable { case deny, stop }

public struct Limits: Codable, Equatable, Sendable {
    public let maxRunningThreads: UInt32?
    public let maxRunningThreadsPerDevice: UInt32?
    public let threadBudgetUsd: Double?
    public let conversationBudgetUsd: Double?
    public let dailyBudgetUsd: Double?
    public let warnAtFraction: Double?
    public let approvalTimeoutSecs: UInt64?
    public let approvalTimeoutAction: ApprovalTimeoutAction
    public let askTimeoutSecs: UInt64?
}

public struct LimitsReport: Codable, Equatable, Sendable {
    public let limits: Limits
    public let warnAtFraction: Double
    public let spentTodayUsd: Double
    public let dayResetsAt: UInt64
    public let runningThreads: UInt32
}

public struct MemoryEntry: Codable, Equatable, Identifiable, Sendable {
    public let id: Int64
    public let content: String
    public let tags: [String]
    public let conversationId: String?
    public let created: UInt64
    public let updated: UInt64
    public let lastUsed: UInt64?
}

public struct MemoryInput: Codable, Equatable, Sendable {
    public let content: String
    public let tags: [String]?
    public init(content: String, tags: [String]? = nil) { self.content = content; self.tags = tags }
}

public struct Skill: Codable, Equatable, Identifiable, Sendable {
    public let id: Int64
    public let name: String
    public let description: String
    public let triggers: [String]
    public let steps: String
    public let tools: [String]
    public let models: [String]
    public let devices: [String]
    public let successCount: UInt64
    public let lastUsed: UInt64?
    public let created: UInt64
    public let updated: UInt64
}

public struct SkillInput: Codable, Equatable, Sendable {
    public let name: String?
    public let description: String?
    public let triggers: [String]?
    public let steps: String?
    public let tools: [String]?
    public let models: [String]?
    public let devices: [String]?

    public init(name: String? = nil, description: String? = nil, triggers: [String]? = nil, steps: String? = nil, tools: [String]? = nil, models: [String]? = nil, devices: [String]? = nil) {
        self.name = name; self.description = description; self.triggers = triggers; self.steps = steps
        self.tools = tools; self.models = models; self.devices = devices
    }
}

public struct HubInfo: Codable, Equatable, Sendable {
    public let hubId: String
    public let hubVersion: String
    public let protocolVersion: UInt32
}

public struct WorkspaceFile: Codable, Equatable, Sendable {
    public let path: String
    public let size: UInt64
}

public struct WorkspaceListing: Codable, Equatable, Sendable {
    public let threadId: String
    public let deviceId: String
    public let files: [WorkspaceFile]
    public let truncated: Bool
}
