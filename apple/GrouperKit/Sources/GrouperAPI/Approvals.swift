import Foundation

/// One row of `GET /v1/approvals`: the hub's authoritative list of pending requests.
public struct HubApproval: Codable, Equatable, Sendable {
    public let approvalId: String
    public let deviceId: String
    public let threadId: String
    public let conversationId: String
    public let tool: String
    public let input: JSONValue
    public let reason: String
    /// Unix seconds.
    public let requestedAt: UInt64
    public let timeoutSecs: UInt64?
    public let timeoutAt: UInt64?
    public let timeoutAction: ApprovalTimeoutAction?

    public var pending: PendingApproval {
        PendingApproval(approvalId: approvalId, conversationId: conversationId, threadId: threadId.isEmpty ? nil : threadId,
                        deviceId: deviceId, tool: tool, input: input, reason: reason, seq: 0, ts: requestedAt,
                        timeoutSecs: timeoutSecs, timeoutAt: timeoutAt, timeoutAction: timeoutAction)
    }
}

/// Why a request left the pending list. The dock shows it in place of the actions.
public enum ApprovalOutcome: Equatable, Sendable {
    case answeredHere(approved: Bool)
    case answeredElsewhere
    case threadEnded
    case noLongerWaiting

    public var message: String {
        switch self {
        case .answeredHere(let approved): return approved ? "Approved." : "Denied."
        case .answeredElsewhere: return "Answered on another device."
        case .threadEnded: return "Thread ended; this request can’t run."
        case .noLongerWaiting: return "This request is no longer waiting."
        }
    }
}

/// A request that just left the dock, kept briefly so the dock can say what happened.
public struct SettledApproval: Equatable, Identifiable, Sendable {
    public let approval: PendingApproval
    public let outcome: ApprovalOutcome
    public var id: String { approval.id }

    public init(approval: PendingApproval, outcome: ApprovalOutcome) {
        self.approval = approval
        self.outcome = outcome
    }
}

/// Ordering and navigation of the approval dock's queue (docs/gui-spec.md 5.13):
/// oldest request first, ties broken by approval id so every client agrees;
/// Previous/Next stop at the ends instead of wrapping.
public enum ApprovalQueue {
    public static func ordered(_ approvals: [PendingApproval]) -> [PendingApproval] {
        approvals.sorted { lhs, rhs in
            lhs.ts != rhs.ts ? lhs.ts < rhs.ts : lhs.approvalId < rhs.approvalId
        }
    }

    /// The request the dock shows: the selected one if it is still queued, else the oldest.
    public static func displayed(selected: String?, in queue: [PendingApproval]) -> PendingApproval? {
        if let selected, let match = queue.first(where: { $0.id == selected }) { return match }
        return queue.first
    }

    /// The request `step` places away from `current` (−1 Previous, +1 Next), or nil at an end.
    public static func neighbour(of current: String?, step: Int, in queue: [PendingApproval]) -> String? {
        guard let shown = displayed(selected: current, in: queue),
              let index = queue.firstIndex(where: { $0.id == shown.id }) else { return nil }
        let target = index + step
        guard queue.indices.contains(target) else { return nil }
        return queue[target].id
    }

    /// Where the dock goes after `removed` leaves: the next request that was after it,
    /// else the one before it, else nothing.
    public static func successor(of removed: String, previousOrder: [String], remaining: [PendingApproval]) -> String? {
        let left = Set(remaining.map(\.id))
        guard let index = previousOrder.firstIndex(of: removed) else { return remaining.first?.id }
        if let after = previousOrder[(index + 1)...].first(where: { left.contains($0) }) { return after }
        if let before = previousOrder[..<index].last(where: { left.contains($0) }) { return before }
        return remaining.first?.id
    }

    /// A tool on Manual asks with "<tool> is set to Manual" (core-agent permission.rs);
    /// anything else is a risk rule, where Approve and Deny get equal weight. This reads
    /// the hub's reason, not the tool input.
    public static func isRiskRule(reason: String) -> Bool {
        let trimmed = reason.trimmingCharacters(in: .whitespacesAndNewlines)
        return !trimmed.isEmpty && !trimmed.hasSuffix(" is set to Manual")
    }
}

/// The client's set of pending approvals across every chat. `GET /v1/approvals` is
/// the source of truth; stored `approval_requested` / `approval_resolved` /
/// `thread_finished` events keep it current in between. Requests that left are
/// remembered (bounded) with why, so a late or repeated event can't bring one back
/// and the dock can say "Answered on another device" or "Thread ended".
public struct ApprovalBook: Equatable, Sendable {
    public enum Change: Equatable, Sendable {
        case none
        case added(PendingApproval)
        case removed([String])
    }

    public static let outcomeLimit = 256

    public private(set) var pending: [String: PendingApproval] = [:]
    public private(set) var outcomes: [String: ApprovalOutcome] = [:]
    /// True once a fetch of `GET /v1/approvals` has succeeded.
    public private(set) var loaded = false
    private var outcomeOrder: [String] = []
    private var arrivals: [String: Int] = [:]
    private var answering: Set<String> = []
    private var clock = 0

    public init() {}

    public var ordered: [PendingApproval] { ApprovalQueue.ordered(Array(pending.values)) }

    public func ordered(in conversationId: String) -> [PendingApproval] {
        ApprovalQueue.ordered(pending.values.filter { $0.conversationId == conversationId })
    }

    public func outcome(of id: String) -> ApprovalOutcome? { outcomes[id] }

    /// Call before `GET /v1/approvals`; pass the mark to `finishFetch`.
    public func fetchMark() -> Int { clock }

    /// Replaces the pending set with a fetched list. Requests that arrived on the
    /// socket after `mark` are kept even if the fetch missed them; requests already
    /// settled are not revived. Returns the ids that left.
    @discardableResult
    public mutating func finishFetch(_ fetched: [PendingApproval], since mark: Int) -> Change {
        var next: [String: PendingApproval] = [:]
        for approval in fetched where outcomes[approval.id] == nil {
            var merged = approval
            if let known = pending[approval.id], merged.seq == 0 {
                merged = PendingApproval(approvalId: merged.approvalId, conversationId: merged.conversationId,
                                         threadId: merged.threadId ?? known.threadId, deviceId: merged.deviceId,
                                         tool: merged.tool, input: merged.input, reason: merged.reason, seq: known.seq,
                                         ts: merged.ts, timeoutSecs: merged.timeoutSecs, timeoutAt: merged.timeoutAt,
                                         timeoutAction: merged.timeoutAction)
            }
            next[approval.id] = merged
        }
        for (id, approval) in pending where next[id] == nil && (arrivals[id] ?? Int.min) > mark {
            next[id] = approval
        }
        let removed = pending.keys.filter { next[$0] == nil }.sorted()
        for id in removed where outcomes[id] == nil { record(id, .noLongerWaiting) }
        pending = next
        arrivals = arrivals.filter { next[$0.key] != nil }
        loaded = true
        return removed.isEmpty ? .none : .removed(removed)
    }

    /// Folds one event. Only approval and thread-end events change anything.
    @discardableResult
    public mutating func apply(_ event: Event) -> Change {
        switch event.body {
        case .approvalRequested(let request):
            guard pending[request.approvalId] == nil, outcomes[request.approvalId] == nil else { return .none }
            let approval = PendingApproval(approvalId: request.approvalId, conversationId: event.conversationId,
                                           threadId: event.threadId, deviceId: request.deviceId, tool: request.tool,
                                           input: request.input, reason: request.reason, seq: event.seq, ts: event.ts)
            clock += 1
            pending[approval.id] = approval
            arrivals[approval.id] = clock
            return .added(approval)
        case .approvalResolved(let id, let approved):
            if outcomes[id] == nil {
                record(id, answering.contains(id) ? .answeredHere(approved: approved) : .answeredElsewhere)
            }
            return remove([id])
        case .threadFinished:
            guard let threadId = event.threadId else { return .none }
            let ids = pending.values.filter { $0.threadId == threadId }.map(\.id).sorted()
            for id in ids where outcomes[id] == nil { record(id, .threadEnded) }
            return remove(ids)
        default:
            return .none
        }
    }

    /// Marks an answer from this client as in flight, so its own `approval_resolved`
    /// is not mistaken for an answer from another device.
    public mutating func beginAnswer(_ id: String) { answering.insert(id) }

    /// The answer failed: the request stays pending.
    public mutating func cancelAnswer(_ id: String) { answering.remove(id) }

    /// Records how a request ended (an answer here, or a 404 meaning someone else
    /// answered) and drops it from the pending set.
    @discardableResult
    public mutating func settle(_ id: String, as outcome: ApprovalOutcome) -> Change {
        answering.remove(id)
        if case .answeredHere = outcome {
            record(id, outcome)
        } else if outcomes[id] == nil {
            record(id, outcome)
        }
        return remove([id])
    }

    public mutating func reset() { self = ApprovalBook() }

    private mutating func remove(_ ids: [String]) -> Change {
        let present = ids.filter { pending[$0] != nil }
        guard !present.isEmpty else { return .none }
        for id in present {
            pending.removeValue(forKey: id)
            arrivals.removeValue(forKey: id)
            answering.remove(id)
        }
        return .removed(present)
    }

    private mutating func record(_ id: String, _ outcome: ApprovalOutcome) {
        if outcomes[id] == nil { outcomeOrder.append(id) }
        outcomes[id] = outcome
        if outcomeOrder.count > Self.outcomeLimit {
            let dropped = outcomeOrder.removeFirst()
            outcomes.removeValue(forKey: dropped)
        }
    }
}

/// Copy for the dock's timeout line. Says only what the hub reported.
public enum ApprovalTimeoutText {
    public static func describe(_ approval: PendingApproval, now: Date = Date()) -> String {
        guard let secs = approval.timeoutSecs, secs > 0 else { return "Waiting for an answer." }
        let deadline = approval.timeoutAt ?? approval.ts &+ secs
        let left = max(0, Int64(deadline) - Int64(now.timeIntervalSince1970.rounded()))
        let then = approval.timeoutAction == .stop
            ? "it’s denied and the thread is stopped"
            : "it’s denied and the thread goes on"
        if left == 0 { return "The \(duration(secs)) timeout has passed; waiting for the hub. Then \(then)." }
        return "If nobody answers within \(duration(secs)) (about \(duration(UInt64(left))) left), \(then)."
    }

    /// The web client's `duration()`: 45s, 5m, 1.5h.
    public static func duration(_ secs: UInt64) -> String {
        if secs < 60 { return "\(secs)s" }
        if secs < 3600 { return "\(Int((Double(secs) / 60).rounded()))m" }
        let tenths = Int((Double(secs) / 360).rounded())
        return "\(tenths / 10).\(tenths % 10)h"
    }
}

/// Local approval notification text (docs/gui-spec.md 5.14). Names only: never the
/// tool input or the reason, which can quote it.
public enum ApprovalNotificationText {
    public static let title = "Grouper needs your review"

    public static func body(tool: String, device: String, thread: String, chat: String) -> String {
        "\(tool) on \(device) · \(thread) · \(chat)"
    }

    /// Stable per approval, so a repeat replaces instead of stacking.
    public static func identifier(approvalId: String) -> String { "grouper-approval-\(approvalId)" }
}
