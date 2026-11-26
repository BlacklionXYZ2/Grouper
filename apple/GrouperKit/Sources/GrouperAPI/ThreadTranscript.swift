import Foundation

/// One item of a thread shown as a chat, in event order.
///
/// Mirrors the web client's thread view (`views/thread.js`): the orchestrator's
/// brief is the first message, the thread model's text is the assistant's (with a
/// model label whenever someone else spoke last), tool calls are compact rows in
/// between, and step-ins, questions to the orchestrator and its answers are
/// messages. Waiting, checkpoint and the final result are shown around the
/// transcript, not in it.
public enum ThreadMessage: Equatable, Identifiable, Sendable {
    public enum Sender: Equatable, Sendable { case owner, orchestrator }

    /// A message to the thread: the brief, a step-in, or the orchestrator's answer.
    case inbound(id: String, sender: Sender, label: String, text: String)
    /// The thread model is about to speak after someone else did.
    case modelLabel(id: String)
    /// The thread model's text.
    case text(id: String, text: String)
    /// A tool call, with its result once it lands.
    case tool(id: String, call: ToolCall, result: ToolResult?)
    /// The thread asked the orchestrator a question (`ask_orchestrator`).
    case ask(id: String, question: String)
    case notice(id: String, text: String)
    case handedOver(id: String, deviceId: String)
    case approvalResolved(id: String, tool: String, approved: Bool)
    case askFailed(id: String, text: String)
    case error(id: String, message: String)

    public var id: String {
        switch self {
        case .inbound(let id, _, _, _), .modelLabel(let id), .text(let id, _), .tool(let id, _, _), .ask(let id, _),
             .notice(let id, _), .handedOver(let id, _), .approvalResolved(let id, _, _), .askFailed(let id, _),
             .error(let id, _):
            return id
        }
    }
}

/// A thread's activity folded into chat messages.
public struct ThreadTranscript: Equatable, Sendable {
    public static let askTool = "ask_orchestrator"

    public let messages: [ThreadMessage]
    /// True when the thread model spoke last, so a streaming draft needs no model label.
    public let threadSpokeLast: Bool
    /// The final summary from the stored `thread_finished` event, if any.
    public let finishedSummary: String?

    public init(activity rows: [ThreadActivityRow]) {
        var messages: [ThreadMessage] = []
        var threadSpoke = false
        var finished: String?
        // Inputs (by row index) that are the orchestrator's answer to an earlier question.
        var answerInputs = Set<Int>()

        func threadSpeaks(before id: String) {
            guard !threadSpoke else { return }
            threadSpoke = true
            messages.append(.modelLabel(id: "who:\(id)"))
        }

        func inbound(_ id: String, _ sender: ThreadMessage.Sender, _ label: String, _ text: String) {
            threadSpoke = false
            messages.append(.inbound(id: id, sender: sender, label: label, text: text))
        }

        for (index, row) in rows.enumerated() {
            switch row {
            case .started(let seq, _, let value):
                if !value.brief.isEmpty { inbound("brief:\(seq)", .orchestrator, "Orchestrator · brief", value.brief) }
            case .text(_, _, let text):
                threadSpeaks(before: row.id)
                messages.append(.text(id: row.id, text: text))
            case .tool(_, _, let call, _) where call.name == ThreadPlan.toolName:
                // The plan is shown as a checklist above the chat, not as a tool row.
                break
            case .tool(_, _, let call, let result) where call.name == Self.askTool:
                threadSpeaks(before: row.id)
                messages.append(.ask(id: "ask:\(call.callId)", question: Self.question(from: call.input)))
                guard let result else { break }
                let output = result.output.trimmingCharacters(in: .whitespacesAndNewlines)
                if !result.ok {
                    messages.append(.askFailed(id: "answer:\(call.callId)", text: output.isEmpty ? "the question failed" : output))
                    break
                }
                // The hub delivers the answer both as a message to the thread and as the
                // tool result; label that message instead of repeating it.
                if let later = Self.matchingInput(in: rows, after: index, text: output, excluding: answerInputs) {
                    answerInputs.insert(later)
                } else {
                    inbound("answer:\(call.callId)", .orchestrator, "Orchestrator · answer", output.isEmpty ? "(empty answer)" : output)
                }
            case .tool(_, _, let call, let result):
                threadSpeaks(before: row.id)
                messages.append(.tool(id: row.id, call: call, result: result))
            case .input(_, _, let from, let text):
                if from == "user" {
                    inbound(row.id, .owner, "You", text)
                } else {
                    let label = answerInputs.contains(index) ? "Orchestrator · answer" : Self.capitalized(from.isEmpty ? "orchestrator" : from)
                    inbound(row.id, .orchestrator, label, text)
                }
            case .notice(_, _, let text):
                messages.append(.notice(id: row.id, text: text))
            case .handedOver(_, _, let value):
                messages.append(.handedOver(id: row.id, deviceId: value.deviceId))
            case .approval(_, _, let request, let approved):
                if let approved { messages.append(.approvalResolved(id: row.id, tool: request.tool, approved: approved)) }
            case .error(_, _, let message):
                messages.append(.error(id: row.id, message: message))
            case .finished(_, _, _, let summary):
                if !summary.isEmpty { finished = summary }
            case .waiting, .checkpoint:
                break
            }
        }
        self.messages = messages
        self.threadSpokeLast = threadSpoke
        self.finishedSummary = finished
    }

    /// The question of an `ask_orchestrator` call: its `question` string, else the input as JSON.
    static func question(from input: JSONValue) -> String {
        if case .object(let fields) = input, case .string(let question)? = fields["question"] { return question }
        return input.prettyPrinted
    }

    private static func matchingInput(in rows: [ThreadActivityRow], after index: Int, text: String, excluding used: Set<Int>) -> Int? {
        guard index + 1 < rows.count else { return nil }
        for later in (index + 1)..<rows.count where !used.contains(later) {
            if case .input(_, _, let from, let body) = rows[later], from != "user",
               body.trimmingCharacters(in: .whitespacesAndNewlines) == text {
                return later
            }
        }
        return nil
    }

    private static func capitalized(_ s: String) -> String {
        guard let first = s.first else { return s }
        return first.uppercased() + s.dropFirst()
    }
}

/// How long a thread has run: live (now − created) while it runs or waits, the
/// total (finished − created) once it is over. `seconds` is nil when unknown.
public struct ThreadElapsed: Equatable, Sendable {
    public let seconds: Double?
    public let live: Bool

    /// "4m 07s" while live, "took 4m 07s" once finished, "—" when unknown.
    public var text: String {
        guard let seconds else { return "—" }
        return live ? Self.format(seconds) : "took \(Self.format(seconds))"
    }

    /// The web client's `elapsed()`: 42s, 4m 07s, 2h 05m, 3d 4h.
    public static func format(_ seconds: Double) -> String {
        let s = seconds.isFinite ? max(0, Int(seconds.rounded(.down))) : 0
        if s < 60 { return "\(s)s" }
        if s < 3600 { return "\(s / 60)m \(twoDigits(s % 60))s" }
        if s < 86400 { return "\(s / 3600)h \(twoDigits((s % 3600) / 60))m" }
        return "\(s / 86400)d \((s % 86400) / 3600)h"
    }

    private static func twoDigits(_ n: Int) -> String { n < 10 ? "0\(n)" : "\(n)" }
}

public extension ThreadInfo {
    /// Running or waiting for a device.
    var isActive: Bool { status == "running" || status == "waiting_device" }

    func elapsed(now: Date = Date()) -> ThreadElapsed {
        guard created > 0 else { return ThreadElapsed(seconds: nil, live: false) }
        let start = Double(created)
        if isActive { return ThreadElapsed(seconds: max(0, now.timeIntervalSince1970 - start), live: true) }
        if let finished { return ThreadElapsed(seconds: max(0, Double(finished) - start), live: false) }
        return ThreadElapsed(seconds: nil, live: false)
    }
}
