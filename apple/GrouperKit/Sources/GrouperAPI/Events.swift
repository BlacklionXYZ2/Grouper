import Foundation

public struct ToolCall: Codable, Equatable, Sendable {
    public let callId: String
    public let name: String
    public let input: JSONValue
}

public struct ToolResult: Codable, Equatable, Sendable {
    public let callId: String
    public let name: String
    public let ok: Bool
    public let output: String
}

public struct AgentUsage: Codable, Equatable, Sendable {
    public let providerId: String
    public let model: String
    public let inputTokens: UInt64
    public let outputTokens: UInt64
    public let cacheReadTokens: UInt64
    public let cacheWriteTokens: UInt64
}

public enum AgentEvent: Codable, Equatable, Sendable {
    case text(String)
    case textDelta(String)
    case toolCall(ToolCall)
    case toolResult(ToolResult)
    case usage(AgentUsage)
    case notice(String)
    case unknown(type: String, payload: [String: JSONValue])

    private enum Keys: String, CodingKey {
        case type, text, callId, name, input, ok, output, providerId, model
        case inputTokens, outputTokens, cacheReadTokens, cacheWriteTokens
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        let type = try c.decode(String.self, forKey: .type)
        switch type {
        case "text": self = .text(try c.decode(String.self, forKey: .text))
        case "text_delta": self = .textDelta(try c.decode(String.self, forKey: .text))
        case "tool_call": self = .toolCall(ToolCall(
            callId: try c.decode(String.self, forKey: .callId),
            name: try c.decode(String.self, forKey: .name),
            input: try c.decode(JSONValue.self, forKey: .input)
        ))
        case "tool_result": self = .toolResult(ToolResult(
            callId: try c.decode(String.self, forKey: .callId),
            name: try c.decode(String.self, forKey: .name),
            ok: try c.decode(Bool.self, forKey: .ok),
            output: try c.decode(String.self, forKey: .output)
        ))
        case "usage": self = .usage(AgentUsage(
            providerId: try c.decode(String.self, forKey: .providerId),
            model: try c.decode(String.self, forKey: .model),
            inputTokens: try c.decode(UInt64.self, forKey: .inputTokens),
            outputTokens: try c.decode(UInt64.self, forKey: .outputTokens),
            cacheReadTokens: try c.decode(UInt64.self, forKey: .cacheReadTokens),
            cacheWriteTokens: try c.decode(UInt64.self, forKey: .cacheWriteTokens)
        ))
        case "notice": self = .notice(try c.decode(String.self, forKey: .text))
        default:
            self = .unknown(type: type, payload: try decoder.singleValueContainer().decode([String: JSONValue].self))
        }
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case .unknown(_, let payload): try payload.encode(to: encoder)
        default:
            var c = encoder.container(keyedBy: Keys.self)
            switch self {
            case .text(let text): try c.encode("text", forKey: .type); try c.encode(text, forKey: .text)
            case .textDelta(let text): try c.encode("text_delta", forKey: .type); try c.encode(text, forKey: .text)
            case .toolCall(let value):
                try c.encode("tool_call", forKey: .type); try c.encode(value.callId, forKey: .callId)
                try c.encode(value.name, forKey: .name); try c.encode(value.input, forKey: .input)
            case .toolResult(let value):
                try c.encode("tool_result", forKey: .type); try c.encode(value.callId, forKey: .callId)
                try c.encode(value.name, forKey: .name); try c.encode(value.ok, forKey: .ok); try c.encode(value.output, forKey: .output)
            case .usage(let value):
                try c.encode("usage", forKey: .type); try c.encode(value.providerId, forKey: .providerId)
                try c.encode(value.model, forKey: .model); try c.encode(value.inputTokens, forKey: .inputTokens)
                try c.encode(value.outputTokens, forKey: .outputTokens); try c.encode(value.cacheReadTokens, forKey: .cacheReadTokens)
                try c.encode(value.cacheWriteTokens, forKey: .cacheWriteTokens)
            case .notice(let text): try c.encode("notice", forKey: .type); try c.encode(text, forKey: .text)
            case .unknown: break
            }
        }
    }
}

public struct ThreadStarted: Codable, Equatable, Sendable {
    public let title: String
    public let deviceId: String
    public let model: String
    public let reason: String?
    public let brief: String
}

public struct ThreadWaiting: Codable, Equatable, Sendable { public let deviceId: String; public let reason: String }
public struct ThreadCheckpoint: Codable, Equatable, Sendable { public let deviceId: String; public let summary: String }
public struct ThreadHandedOver: Codable, Equatable, Sendable {
    public let deviceId: String; public let files: [String]; public let skipped: [String]; public let brief: String
}
public struct ApprovalRequest: Codable, Equatable, Sendable {
    public let approvalId: String; public let deviceId: String; public let tool: String; public let input: JSONValue; public let reason: String
}
public struct BudgetWarning: Codable, Equatable, Sendable { public let scope: String; public let spent: Double; public let limit: Double }

public enum EventBody: Codable, Equatable, Sendable {
    case userMessage(String)
    case assistantMessage(String)
    case assistantDelta(String)
    case threadStarted(ThreadStarted)
    case threadActivity(AgentEvent)
    case threadInput(from: String, text: String)
    case threadFinished(status: String, summary: String)
    case threadWaiting(ThreadWaiting)
    case threadCheckpoint(ThreadCheckpoint)
    case threadHandedOver(ThreadHandedOver)
    case approvalRequested(ApprovalRequest)
    /// `reason` is set when no one answered: "timeout", "timeout_stop" or "dropped" (absent from older hubs).
    case approvalResolved(approvalId: String, approved: Bool, reason: String? = nil)
    case turnFinished(runningThreads: UInt32)
    case error(String)
    case budgetWarning(BudgetWarning)
    case unknown(kind: String, payload: [String: JSONValue])

    private enum Keys: String, CodingKey {
        case kind, text, title, deviceId, model, reason, brief, event, from, status, summary
        case files, skipped, approvalId, tool, input, approved, runningThreads, message, scope, spent, limit
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        let kind = try c.decode(String.self, forKey: .kind)
        switch kind {
        case "user_message": self = .userMessage(try c.decode(String.self, forKey: .text))
        case "assistant_message": self = .assistantMessage(try c.decode(String.self, forKey: .text))
        case "assistant_delta": self = .assistantDelta(try c.decode(String.self, forKey: .text))
        case "thread_started": self = .threadStarted(ThreadStarted(
            title: try c.decode(String.self, forKey: .title), deviceId: try c.decode(String.self, forKey: .deviceId),
            model: try c.decode(String.self, forKey: .model), reason: try c.decodeIfPresent(String.self, forKey: .reason),
            brief: try c.decode(String.self, forKey: .brief)
        ))
        case "thread_activity": self = .threadActivity(try c.decode(AgentEvent.self, forKey: .event))
        case "thread_input": self = .threadInput(from: try c.decode(String.self, forKey: .from), text: try c.decode(String.self, forKey: .text))
        case "thread_finished": self = .threadFinished(status: try c.decode(String.self, forKey: .status), summary: try c.decode(String.self, forKey: .summary))
        case "thread_waiting": self = .threadWaiting(ThreadWaiting(deviceId: try c.decode(String.self, forKey: .deviceId), reason: try c.decode(String.self, forKey: .reason)))
        case "thread_checkpoint": self = .threadCheckpoint(ThreadCheckpoint(deviceId: try c.decode(String.self, forKey: .deviceId), summary: try c.decode(String.self, forKey: .summary)))
        case "thread_handed_over": self = .threadHandedOver(ThreadHandedOver(
            deviceId: try c.decode(String.self, forKey: .deviceId), files: try c.decode([String].self, forKey: .files),
            skipped: try c.decode([String].self, forKey: .skipped), brief: try c.decode(String.self, forKey: .brief)
        ))
        case "approval_requested": self = .approvalRequested(ApprovalRequest(
            approvalId: try c.decode(String.self, forKey: .approvalId), deviceId: try c.decode(String.self, forKey: .deviceId),
            tool: try c.decode(String.self, forKey: .tool), input: try c.decode(JSONValue.self, forKey: .input),
            reason: try c.decode(String.self, forKey: .reason)
        ))
        case "approval_resolved": self = .approvalResolved(
            approvalId: try c.decode(String.self, forKey: .approvalId), approved: try c.decode(Bool.self, forKey: .approved),
            reason: try c.decodeIfPresent(String.self, forKey: .reason)
        )
        case "turn_finished": self = .turnFinished(runningThreads: try c.decode(UInt32.self, forKey: .runningThreads))
        case "error": self = .error(try c.decode(String.self, forKey: .message))
        case "budget_warning": self = .budgetWarning(BudgetWarning(scope: try c.decode(String.self, forKey: .scope), spent: try c.decode(Double.self, forKey: .spent), limit: try c.decode(Double.self, forKey: .limit)))
        default: self = .unknown(kind: kind, payload: try decoder.singleValueContainer().decode([String: JSONValue].self))
        }
    }

    public func encode(to encoder: Encoder) throws {
        if case .unknown(_, let payload) = self { try payload.encode(to: encoder); return }
        var c = encoder.container(keyedBy: Keys.self)
        switch self {
        case .userMessage(let text): try c.encode("user_message", forKey: .kind); try c.encode(text, forKey: .text)
        case .assistantMessage(let text): try c.encode("assistant_message", forKey: .kind); try c.encode(text, forKey: .text)
        case .assistantDelta(let text): try c.encode("assistant_delta", forKey: .kind); try c.encode(text, forKey: .text)
        case .threadStarted(let v):
            try c.encode("thread_started", forKey: .kind); try c.encode(v.title, forKey: .title); try c.encode(v.deviceId, forKey: .deviceId)
            try c.encode(v.model, forKey: .model); try c.encodeIfPresent(v.reason, forKey: .reason); try c.encode(v.brief, forKey: .brief)
        case .threadActivity(let event): try c.encode("thread_activity", forKey: .kind); try c.encode(event, forKey: .event)
        case .threadInput(let from, let text): try c.encode("thread_input", forKey: .kind); try c.encode(from, forKey: .from); try c.encode(text, forKey: .text)
        case .threadFinished(let status, let summary): try c.encode("thread_finished", forKey: .kind); try c.encode(status, forKey: .status); try c.encode(summary, forKey: .summary)
        case .threadWaiting(let v): try c.encode("thread_waiting", forKey: .kind); try c.encode(v.deviceId, forKey: .deviceId); try c.encode(v.reason, forKey: .reason)
        case .threadCheckpoint(let v): try c.encode("thread_checkpoint", forKey: .kind); try c.encode(v.deviceId, forKey: .deviceId); try c.encode(v.summary, forKey: .summary)
        case .threadHandedOver(let v):
            try c.encode("thread_handed_over", forKey: .kind); try c.encode(v.deviceId, forKey: .deviceId); try c.encode(v.files, forKey: .files)
            try c.encode(v.skipped, forKey: .skipped); try c.encode(v.brief, forKey: .brief)
        case .approvalRequested(let v):
            try c.encode("approval_requested", forKey: .kind); try c.encode(v.approvalId, forKey: .approvalId); try c.encode(v.deviceId, forKey: .deviceId)
            try c.encode(v.tool, forKey: .tool); try c.encode(v.input, forKey: .input); try c.encode(v.reason, forKey: .reason)
        case .approvalResolved(let id, let approved, let reason):
            try c.encode("approval_resolved", forKey: .kind); try c.encode(id, forKey: .approvalId); try c.encode(approved, forKey: .approved)
            try c.encodeIfPresent(reason, forKey: .reason)
        case .turnFinished(let count): try c.encode("turn_finished", forKey: .kind); try c.encode(count, forKey: .runningThreads)
        case .error(let message): try c.encode("error", forKey: .kind); try c.encode(message, forKey: .message)
        case .budgetWarning(let v): try c.encode("budget_warning", forKey: .kind); try c.encode(v.scope, forKey: .scope); try c.encode(v.spent, forKey: .spent); try c.encode(v.limit, forKey: .limit)
        case .unknown: break
        }
    }
}

public struct Event: Codable, Equatable, Identifiable, Sendable {
    public let seq: Int64
    public let conversationId: String
    public let threadId: String?
    public let ts: UInt64
    public let body: EventBody
    public var id: String { seq > 0 ? "\(conversationId):\(seq)" : "live:\(conversationId):\(threadId ?? "-"):\(ts)" }

    private enum Keys: String, CodingKey { case seq, conversationId, threadId, ts }

    public init(seq: Int64, conversationId: String, threadId: String?, ts: UInt64, body: EventBody) {
        self.seq = seq; self.conversationId = conversationId; self.threadId = threadId; self.ts = ts; self.body = body
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        seq = try c.decode(Int64.self, forKey: .seq)
        conversationId = try c.decode(String.self, forKey: .conversationId)
        threadId = try c.decodeIfPresent(String.self, forKey: .threadId)
        ts = try c.decode(UInt64.self, forKey: .ts)
        body = try EventBody(from: decoder)
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: Keys.self)
        try c.encode(seq, forKey: .seq); try c.encode(conversationId, forKey: .conversationId)
        try c.encodeIfPresent(threadId, forKey: .threadId); try c.encode(ts, forKey: .ts)
        try body.encode(to: encoder)
    }
}
