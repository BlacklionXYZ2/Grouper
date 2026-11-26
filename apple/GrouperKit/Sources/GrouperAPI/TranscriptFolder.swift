import Foundation

public enum TranscriptRow: Equatable, Identifiable, Sendable {
    case user(seq: Int64, ts: UInt64, text: String)
    case assistant(seq: Int64, ts: UInt64, text: String)
    case thread(ThreadCard)
    case error(seq: Int64, ts: UInt64, message: String)
    case budgetWarning(seq: Int64, ts: UInt64, warning: BudgetWarning)

    public var id: String {
        switch self {
        case .user(let seq, _, _): return "user:\(seq)"
        case .assistant(let seq, _, _): return "assistant:\(seq)"
        case .thread(let card): return "thread:\(card.threadId)"
        case .error(let seq, _, _): return "error:\(seq)"
        case .budgetWarning(let seq, _, _): return "budget:\(seq)"
        }
    }
}

public struct ThreadCard: Equatable, Identifiable, Sendable {
    public let threadId: String
    public var seq: Int64
    public var ts: UInt64
    public var title: String
    public var deviceId: String
    public var model: String
    public var reason: String?
    public var status: String
    public var latestText: String?
    public var liveDraft: String
    public var openTool: String?
    public var checkpoint: String?
    public var summary: String?
    public var pendingApprovals: UInt32
    public var id: String { threadId }
}

public struct PendingApproval: Equatable, Identifiable, Sendable {
    public let approvalId: String
    public let conversationId: String
    public let threadId: String?
    public let deviceId: String
    public let tool: String
    public let input: JSONValue
    public let reason: String
    public let seq: Int64
    public let ts: UInt64
    public var id: String { approvalId }
}

public enum ThreadActivityRow: Equatable, Identifiable, Sendable {
    case started(seq: Int64, ts: UInt64, value: ThreadStarted)
    case text(seq: Int64, ts: UInt64, text: String)
    case tool(seq: Int64, ts: UInt64, call: ToolCall, result: ToolResult?)
    case input(seq: Int64, ts: UInt64, from: String, text: String)
    case notice(seq: Int64, ts: UInt64, text: String)
    case waiting(seq: Int64, ts: UInt64, value: ThreadWaiting)
    case checkpoint(seq: Int64, ts: UInt64, value: ThreadCheckpoint)
    case handedOver(seq: Int64, ts: UInt64, value: ThreadHandedOver)
    case approval(seq: Int64, ts: UInt64, request: ApprovalRequest, approved: Bool?)
    case error(seq: Int64, ts: UInt64, message: String)
    case finished(seq: Int64, ts: UInt64, status: String, summary: String)

    public var id: String {
        switch self {
        case .tool(_, _, let call, _): return "tool:\(call.callId)"
        case .approval(_, _, let request, _): return "approval:\(request.approvalId)"
        default: return "activity:\(sequence):\(kind)"
        }
    }

    private var sequence: Int64 {
        switch self {
        case .started(let v, _, _), .text(let v, _, _), .tool(let v, _, _, _), .input(let v, _, _, _),
             .notice(let v, _, _), .waiting(let v, _, _), .checkpoint(let v, _, _), .handedOver(let v, _, _),
             .approval(let v, _, _, _), .error(let v, _, _), .finished(let v, _, _, _): return v
        }
    }

    private var kind: String {
        switch self {
        case .started: return "started"; case .text: return "text"; case .tool: return "tool"
        case .input: return "input"; case .notice: return "notice"; case .waiting: return "waiting"
        case .checkpoint: return "checkpoint"; case .handedOver: return "handedOver"
        case .approval: return "approval"; case .error: return "error"; case .finished: return "finished"
        }
    }
}

/// Conversation-scoped event reducer. Durable records and live drafts have separate
/// storage, so a caller can persist `storedEvents` without accidentally saving deltas.
public struct TranscriptFolder: Sendable {
    public let conversationId: String
    public private(set) var storedEvents: [Event]
    public private(set) var assistantDraft = ""
    public private(set) var threadDrafts: [String: String] = [:]

    public init(conversationId: String, events: [Event] = []) {
        self.conversationId = conversationId
        self.storedEvents = Self.normalized(events.filter { $0.conversationId == conversationId && $0.seq > 0 })
    }

    /// Returns true only when a durable event was newly inserted.
    @discardableResult
    public mutating func ingest(_ event: Event) -> Bool {
        guard event.conversationId == conversationId else { return false }
        applyLiveState(event)
        guard event.seq > 0, !storedEvents.contains(where: { $0.seq == event.seq }) else { return false }
        storedEvents.append(event)
        storedEvents.sort { $0.seq < $1.seq }
        return true
    }

    public mutating func replaceStoredEvents(_ events: [Event]) {
        storedEvents = Self.normalized(events.filter { $0.conversationId == conversationId && $0.seq > 0 })
        assistantDraft = ""
        threadDrafts.removeAll()
    }

    public func transcript(threads allThreads: [ThreadInfo]) -> [TranscriptRow] {
        let threads = allThreads.filter { $0.conversationId == conversationId }
        let rowsById = Dictionary(uniqueKeysWithValues: threads.map { ($0.id, $0) })
        var rows: [TranscriptRow] = []
        var cardIndex: [String: Int] = [:]
        var approvals: [String: String?] = [:]

        func blankCard(_ event: Event, threadId: String) -> ThreadCard {
            ThreadCard(threadId: threadId, seq: event.seq, ts: event.ts, title: "", deviceId: "", model: "", reason: nil,
                       status: "running", latestText: nil, liveDraft: threadDrafts[threadId] ?? "", openTool: nil,
                       checkpoint: nil, summary: nil, pendingApprovals: 0)
        }

        func ensureCard(_ event: Event, threadId: String) -> Int {
            if let index = cardIndex[threadId] { return index }
            let index = rows.count
            rows.append(.thread(blankCard(event, threadId: threadId)))
            cardIndex[threadId] = index
            return index
        }

        for event in storedEvents {
            let threadId = event.threadId
            switch (event.body, threadId) {
            case (.userMessage(let text), nil): rows.append(.user(seq: event.seq, ts: event.ts, text: text))
            case (.assistantMessage(let text), nil): rows.append(.assistant(seq: event.seq, ts: event.ts, text: text))
            case (.error(let message), nil): rows.append(.error(seq: event.seq, ts: event.ts, message: message))
            case (.budgetWarning(let warning), _): rows.append(.budgetWarning(seq: event.seq, ts: event.ts, warning: warning))
            case (.threadStarted(let started), .some(let id)):
                let index = ensureCard(event, threadId: id)
                guard case .thread(var card) = rows[index] else { break }
                card.title = started.title; card.deviceId = started.deviceId; card.model = started.model
                card.reason = started.reason; card.status = "running"; rows[index] = .thread(card)
            case (.threadActivity(let activity), .some(let id)):
                let index = ensureCard(event, threadId: id)
                guard case .thread(var card) = rows[index] else { break }
                switch activity {
                case .text(let text): card.latestText = text
                case .toolCall(let call): card.openTool = call.name
                case .toolResult: card.openTool = nil
                default: break
                }
                rows[index] = .thread(card)
            case (.threadWaiting, .some(let id)):
                let index = ensureCard(event, threadId: id)
                guard case .thread(var card) = rows[index] else { break }; card.status = "waiting_device"; rows[index] = .thread(card)
            case (.threadCheckpoint(let checkpoint), .some(let id)):
                let index = ensureCard(event, threadId: id)
                guard case .thread(var card) = rows[index] else { break }; card.checkpoint = checkpoint.summary; rows[index] = .thread(card)
            case (.threadHandedOver(let handover), .some(let id)):
                let index = ensureCard(event, threadId: id)
                guard case .thread(var card) = rows[index] else { break }
                card.status = "running"; card.deviceId = handover.deviceId; rows[index] = .thread(card)
            case (.threadFinished(let status, let summary), .some(let id)):
                approvals = approvals.filter { $0.value != id }
                let index = ensureCard(event, threadId: id)
                guard case .thread(var card) = rows[index] else { break }
                card.status = status; card.summary = summary; card.openTool = nil; card.liveDraft = ""; rows[index] = .thread(card)
            case (.approvalRequested(let request), let id):
                approvals[request.approvalId] = id
                if let id { _ = ensureCard(event, threadId: id) }
            case (.approvalResolved(let id, _), _): approvals.removeValue(forKey: id)
            default: break
            }
        }

        for (id, index) in cardIndex {
            guard case .thread(var card) = rows[index] else { continue }
            if let source = rowsById[id] {
                // The row is usually newer, but a stored thread_finished is final: a row
                // fetched before the thread ended must not make the card look live again.
                let staleRow = card.status.isTerminal && !source.status.isTerminal
                if !staleRow {
                    card.status = source.status
                    card.deviceId = source.deviceId
                }
                if card.title.isEmpty { card.title = source.title }
                if card.model.isEmpty { card.model = source.model }
                if card.reason == nil { card.reason = source.reason }
                if source.summary != nil { card.summary = source.summary }
            }
            card.liveDraft = threadDrafts[id] ?? ""
            card.pendingApprovals = card.status.isTerminal ? 0 : UInt32(approvals.values.filter { $0 == id }.count)
            rows[index] = .thread(card)
        }
        return rows
    }

    public func pendingApprovals(threads allThreads: [ThreadInfo]) -> [PendingApproval] {
        let terminal = Set(allThreads.filter { $0.conversationId == conversationId && $0.isTerminal }.map(\.id))
        var open: [String: PendingApproval] = [:]
        for event in storedEvents {
            switch event.body {
            case .approvalRequested(let request):
                open[request.approvalId] = PendingApproval(
                    approvalId: request.approvalId, conversationId: event.conversationId, threadId: event.threadId,
                    deviceId: request.deviceId, tool: request.tool, input: request.input, reason: request.reason,
                    seq: event.seq, ts: event.ts
                )
            case .approvalResolved(let id, _): open.removeValue(forKey: id)
            case .threadFinished:
                if let id = event.threadId { open = open.filter { $0.value.threadId != id } }
            default: break
            }
        }
        return open.values.filter { $0.threadId.map { !terminal.contains($0) } ?? true }.sorted { $0.seq < $1.seq }
    }

    public func activity(threadId: String) -> [ThreadActivityRow] {
        var rows: [ThreadActivityRow] = []
        var tools: [String: Int] = [:]
        var approvals: [String: Int] = [:]
        for event in storedEvents where event.threadId == threadId {
            let seq = event.seq, ts = event.ts
            switch event.body {
            case .threadStarted(let value): rows.append(.started(seq: seq, ts: ts, value: value))
            case .threadActivity(let agent):
                switch agent {
                case .text(let text): rows.append(.text(seq: seq, ts: ts, text: text))
                case .toolCall(let call): tools[call.callId] = rows.count; rows.append(.tool(seq: seq, ts: ts, call: call, result: nil))
                case .toolResult(let result):
                    if let index = tools[result.callId], case .tool(let firstSeq, let firstTs, let call, _) = rows[index] {
                        rows[index] = .tool(seq: firstSeq, ts: firstTs, call: call, result: result)
                    } else {
                        let call = ToolCall(callId: result.callId, name: result.name, input: .null)
                        rows.append(.tool(seq: seq, ts: ts, call: call, result: result))
                    }
                case .notice(let text): rows.append(.notice(seq: seq, ts: ts, text: text))
                case .textDelta, .usage, .unknown: break
                }
            case .threadInput(let from, let text): rows.append(.input(seq: seq, ts: ts, from: from, text: text))
            case .threadWaiting(let value): rows.append(.waiting(seq: seq, ts: ts, value: value))
            case .threadCheckpoint(let value): rows.append(.checkpoint(seq: seq, ts: ts, value: value))
            case .threadHandedOver(let value): rows.append(.handedOver(seq: seq, ts: ts, value: value))
            case .approvalRequested(let request): approvals[request.approvalId] = rows.count; rows.append(.approval(seq: seq, ts: ts, request: request, approved: nil))
            case .approvalResolved(let id, let approved):
                if let index = approvals[id], case .approval(let firstSeq, let firstTs, let request, _) = rows[index] {
                    rows[index] = .approval(seq: firstSeq, ts: firstTs, request: request, approved: approved)
                }
            case .error(let message): rows.append(.error(seq: seq, ts: ts, message: message))
            case .threadFinished(let status, let summary): rows.append(.finished(seq: seq, ts: ts, status: status, summary: summary))
            default: break
            }
        }
        return rows
    }

    private mutating func applyLiveState(_ event: Event) {
        switch event.body {
        case .assistantDelta(let text): assistantDraft += text
        case .assistantMessage, .turnFinished, .error: assistantDraft = ""
        case .threadActivity(let agent):
            guard let threadId = event.threadId else { return }
            switch agent {
            case .textDelta(let text): threadDrafts[threadId, default: ""] += text
            case .text: threadDrafts[threadId] = ""
            default: break
            }
        case .threadFinished:
            if let threadId = event.threadId { threadDrafts[threadId] = "" }
        default: break
        }
    }

    private static func normalized(_ events: [Event]) -> [Event] {
        var seen = Set<Int64>()
        return events.sorted { $0.seq < $1.seq }.filter { seen.insert($0.seq).inserted }
    }
}

private extension String {
    var isTerminal: Bool { ["done", "failed", "stopped"].contains(self) }
}
