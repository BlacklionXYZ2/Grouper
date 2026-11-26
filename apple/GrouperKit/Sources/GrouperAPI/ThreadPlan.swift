import Foundation

/// A thread's live plan: the checklist it keeps with the built-in `update_plan` tool.
///
/// Contract (docs/gui-concept-plan.md, owner answers 26 Nov): the tool's input is
/// `{"items":[{"content", "status": "pending"|"in_progress"|"completed", "active_form"?}]}`,
/// always the whole list. The plan is the input of the thread's latest such call the
/// hub accepted (`ok: true` result); no call means no plan, an empty list clears it. Clients show it as a checklist, never as a tool row.
public struct ThreadPlan: Equatable, Sendable {
    public static let toolName = "update_plan"

    public enum Status: String, Equatable, Sendable {
        case pending, inProgress = "in_progress", completed
    }

    public struct Item: Equatable, Sendable {
        public let content: String
        public let status: Status
        /// What the thread is doing, e.g. "Running the tests" for "Run the tests".
        public let activeForm: String?

        public init(content: String, status: Status, activeForm: String? = nil) {
            self.content = content
            self.status = status
            self.activeForm = activeForm
        }

        /// The words to show: the active form while in progress, if there is one.
        public var displayText: String {
            if status == .inProgress, let activeForm, !activeForm.isEmpty { return activeForm }
            return content
        }
    }

    public let items: [Item]

    public init(items: [Item]) { self.items = items }

    public var completedCount: Int { items.filter { $0.status == .completed }.count }

    /// "2 of 5 done".
    public var progressText: String { "\(completedCount) of \(items.count) done" }

    /// The plan in one `update_plan` input. Items without text are skipped and an
    /// unknown status reads as pending; nil when the input has no `items` list.
    public init?(input: JSONValue) {
        guard case .object(let fields) = input, case .array(let raw)? = fields["items"] else { return nil }
        items = raw.compactMap { value -> Item? in
            guard case .object(let item) = value, case .string(let content)? = item["content"] else { return nil }
            let text = content.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !text.isEmpty else { return nil }
            var status = Status.pending
            if case .string(let s)? = item["status"], let known = Status(rawValue: s) { status = known }
            var active: String?
            if case .string(let a)? = item["active_form"] {
                let trimmed = a.trimmingCharacters(in: .whitespacesAndNewlines)
                if !trimmed.isEmpty { active = trimmed }
            }
            return Item(content: text, status: status, activeForm: active)
        }
    }

    /// The thread's current plan: the latest `update_plan` call the hub accepted (its
    /// result has `ok: true`, "Plan updated: N of M done."). A rejected call (`ok:
    /// false`) or one still without a result keeps the previous plan. Nil when there
    /// is none, or when the latest accepted call cleared the list ("Plan cleared.").
    public static func latest(in rows: [ThreadActivityRow]) -> ThreadPlan? {
        for row in rows.reversed() {
            guard case .tool(_, _, let call, let result) = row, call.name == toolName,
                  result?.ok == true else { continue }
            guard let plan = ThreadPlan(input: call.input) else { continue }
            return plan.items.isEmpty ? nil : plan
        }
        return nil
    }
}
