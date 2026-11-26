import SwiftUI
import GrouperAPI

/// The approval dock (docs/gui-spec.md 5.13). It takes the composer's place at the
/// bottom of a chat while that chat has pending requests, and the step-in composer's
/// place in a thread while that thread has one. Inline, not a modal: it can't be
/// collapsed and has no "Reply instead"; the owner answers, stops the thread, or
/// goes somewhere else. The chat's draft is kept by the chat and comes back after.
///
/// - One request at a time, "1 of N", oldest first; Previous/Next stop at the ends.
/// - Order: "Run this tool?", tool and device, thread with its model, the chat (in a
///   thread), the reason (most prominent), the exact input (collapsed), timeout, actions.
/// - Approve stays off until "View exact input" has been opened.
///   No keyboard shortcut approves; Return does nothing here.
/// - A risk-rule request gives Approve and Deny equal weight; Manual makes Approve primary.
/// - Look (docs/gui-concept-plan.md V9): an opaque card in the composer's place, shield,
///   title, tool · device, thread, reason, "View exact input" row, Deny and Approve at
///   equal width; queue, timeout, Change it and Stop as small controls underneath.
///   No close control, never a floating dialog.
struct ApprovalDock: View {
    enum Scope: Equatable {
        case chat
        case thread(String)
    }

    @EnvironmentObject private var model: AppModel
    /// This dock's requests, oldest first.
    let queue: [PendingApproval]
    let scope: Scope
    /// Thread scope: requests elsewhere in the chat.
    var otherInChat = 0
    /// Thread scope, when the thread is its own sheet: back to the chat's dock.
    var onShowChatQueue: (() -> Void)? = nil

    @State private var expanded: Set<String> = []
    /// Requests whose exact input has been opened at least once: Approve is on for them.
    @State private var revealed: Set<String> = []
    @State private var busyId: String?
    @State private var failure: String?
    @State private var stopCandidate: PendingApproval?
    @State private var announced: Set<String> = []
    @AccessibilityFocusState private var headingFocused: Bool

    private static let detailsMaxHeight: CGFloat = 320

    private var isThreadScope: Bool {
        if case .thread = scope { return true }
        return false
    }

    var body: some View {
        let settled = settledHere
        let current = ApprovalQueue.displayed(selected: model.selectedApprovalId, in: queue)
        VStack(alignment: .leading, spacing: 10) {
            if let settled {
                settledCard(settled)
            } else if let current {
                card(current)
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.surface, in: RoundedRectangle(cornerRadius: Theme.Radius.card))
        .overlay { RoundedRectangle(cornerRadius: Theme.Radius.card).strokeBorder(Theme.borderStrong, lineWidth: 1) }
        .frame(maxWidth: Theme.Size.transcriptMax)
        .padding(.horizontal).padding(.bottom, 6)
        .frame(maxWidth: .infinity)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Approval request")
        .onChange(of: queue.map(\.id), initial: true) { _, ids in announceNew(ids) }
        .onChange(of: model.dockFocusRequest, initial: true) { _, id in focusIfRequested(id) }
        .confirmationDialog(stopTitle, isPresented: stopDialogShown, titleVisibility: .visible) {
            Button("Stop thread", role: .destructive) { if let candidate = stopCandidate { stop(candidate) } }
            Button("Cancel", role: .cancel) { stopCandidate = nil }
        } message: {
            Text("Its current tool or model turn will be stopped, and work it hasn’t saved is lost. This request won’t run. The orchestrator is told you stopped it.")
        }
    }

    // MARK: - Pending request

    private func card(_ approval: PendingApproval) -> some View {
        let riskRule = ApprovalQueue.isRiskRule(reason: approval.reason)
        return VStack(alignment: .leading, spacing: 10) {
            header(approval)
            ViewThatFits(in: .vertical) {
                details(approval, riskRule: riskRule)
                ScrollView { details(approval, riskRule: riskRule) }
            }
            .frame(maxHeight: Self.detailsMaxHeight)
            actions(approval, riskRule: riskRule)
            statusLine(approval)
            footer(approval)
        }
    }

    private func header(_ approval: PendingApproval) -> some View {
        HStack(spacing: 10) {
            Image(systemName: "checkmark.shield.fill")
                .font(.title2)
                .foregroundStyle(Theme.accent)
                .accessibilityHidden(true)
            Text("Run this tool?")
                .font(Theme.Typeface.dockTitle)
                .foregroundStyle(Theme.text)
                .accessibilityAddTraits(.isHeader)
                .accessibilityLabel("Run this tool? \(approval.tool) on \(model.deviceDisplayName(approval.deviceId))")
                .accessibilityFocused($headingFocused)
            Spacer(minLength: 8)
        }
    }

    private func queueControls(_ approval: PendingApproval) -> some View {
        let index = queue.firstIndex(where: { $0.id == approval.id }) ?? 0
        let previous = ApprovalQueue.neighbour(of: approval.id, step: -1, in: queue)
        let next = ApprovalQueue.neighbour(of: approval.id, step: 1, in: queue)
        return HStack(spacing: 4) {
            Button { model.selectedApprovalId = previous } label: { Image(systemName: "chevron.left") }
                .disabled(previous == nil)
                .accessibilityLabel("Previous request")
                .grouperMinimumControlSize()
            Text("\(index + 1) of \(queue.count)").font(Theme.Typeface.meta.monospacedDigit()).foregroundStyle(Theme.text2)
                .accessibilityLabel("Request \(index + 1) of \(queue.count)")
            Button { model.selectedApprovalId = next } label: { Image(systemName: "chevron.right") }
                .disabled(next == nil)
                .accessibilityLabel("Next request")
                .grouperMinimumControlSize()
        }
        .buttonStyle(.borderless)
        .foregroundStyle(Theme.accent)
    }

    private func details(_ approval: PendingApproval, riskRule: Bool) -> some View {
        let threadModel = model.threads.first(where: { $0.id == approval.threadId })?.model ?? ""
        let open = inputExpanded(approval.id)
        return VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 6) {
                Image(systemName: "wrench.and.screwdriver").foregroundStyle(Theme.text2).accessibilityHidden(true)
                Text(approval.tool).fontWeight(.semibold).foregroundStyle(Theme.text)
                Text("·").foregroundStyle(Theme.text3).accessibilityHidden(true)
                Text("via \(model.deviceDisplayName(approval.deviceId))").foregroundStyle(Theme.text2)
            }
            .font(.subheadline)
            .lineLimit(1)
            .accessibilityElement(children: .combine)
            HStack(spacing: 6) {
                FamilyMark(family: model.family(ofModel: threadModel))
                Text("Thread: \(model.threadTitle(approval.threadId))").foregroundStyle(Theme.text3).lineLimit(2)
                if !threadModel.isEmpty {
                    Text(threadModel).font(Theme.Typeface.meta).foregroundStyle(Theme.text3).lineLimit(1)
                }
            }
            .font(.subheadline)
            .accessibilityElement(children: .combine)
            if isThreadScope {
                Text("In \(model.chatTitle(approval.conversationId))").font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
            }
            Rectangle().fill(Theme.border).frame(height: 1).padding(.vertical, 2)
            reason(approval, riskRule: riskRule)
            Button {
                open.wrappedValue.toggle()
            } label: {
                HStack(spacing: 8) {
                    Image(systemName: "eye").foregroundStyle(Theme.text2).accessibilityHidden(true)
                    Text("View exact input").font(.callout.weight(.semibold)).foregroundStyle(Theme.text)
                    Spacer()
                    Image(systemName: "chevron.right")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(Theme.text3)
                        .rotationEffect(.degrees(open.wrappedValue ? 90 : 0))
                        .accessibilityHidden(true)
                }
                .padding(.horizontal, 12)
                .frame(maxWidth: .infinity, minHeight: 44)
                .background(Theme.surface2, in: RoundedRectangle(cornerRadius: Theme.Radius.sm))
                .overlay { RoundedRectangle(cornerRadius: Theme.Radius.sm).strokeBorder(Theme.border, lineWidth: 1) }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityValue(open.wrappedValue ? "Expanded" : "Collapsed")
            .accessibilityHint(open.wrappedValue ? "Hides the exact input" : "Shows the exact input. Approve turns on once it has been opened.")
            if open.wrappedValue {
                Text(approval.input.prettyPrinted)
                    .font(Theme.Typeface.mono)
                    .foregroundStyle(Theme.text)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(10)
                    .background(Theme.surface2, in: RoundedRectangle(cornerRadius: Theme.Radius.sm))
                    .accessibilityLabel("Exact input for \(approval.tool)")
            }
        }
    }

    private func reason(_ approval: PendingApproval, riskRule: Bool) -> some View {
        let text = approval.reason.isEmpty ? "This tool is set to Manual, so every call waits for you." : approval.reason
        return VStack(alignment: .leading, spacing: 4) {
            if riskRule {
                Label("A safety rule asked", systemImage: "exclamationmark.triangle.fill")
                    .font(.caption.weight(.semibold)).foregroundStyle(Theme.warn)
            }
            Text(text)
                .font(Theme.Typeface.body.weight(.medium))
                .foregroundStyle(Theme.text)
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
    }

    @ViewBuilder private func statusLine(_ approval: PendingApproval) -> some View {
        if !model.online {
            Label("Offline. These are the last synced details; you can answer once the hub reconnects.", systemImage: "wifi.slash")
                .font(.caption).foregroundStyle(Theme.text2)
        } else if !model.approvalsReconciled {
            HStack(spacing: 8) {
                Label("Checking with the hub before answering…", systemImage: "arrow.triangle.2.circlepath")
                    .font(.caption).foregroundStyle(Theme.text2)
                Button("Check again") { Task { await model.loadApprovals() } }.font(.caption)
            }
        } else if let failure {
            Label(failure, systemImage: "exclamationmark.triangle.fill").font(.caption).foregroundStyle(Theme.danger)
        } else if busyId == approval.id {
            HStack(spacing: 6) {
                ProgressView().controlSize(.small)
                Text("Sending your answer…").font(.caption).foregroundStyle(Theme.text2)
            }
        } else if !revealed.contains(approval.id) {
            Label("Open “View exact input” to turn on Approve.", systemImage: "lock.fill")
                .font(.caption).foregroundStyle(Theme.text2)
        }
    }

    private func actions(_ approval: PendingApproval, riskRule: Bool) -> some View {
        let writable = model.canAnswerApprovals && busyId == nil
        return HStack(spacing: 10) {
            Button { answer(approval, approve: false) } label: {
                Label("Deny", systemImage: "xmark")
            }
            .buttonStyle(GrouperActionButtonStyle(kind: .outline))
            .disabled(!writable)
            .accessibilityLabel("Deny \(approval.tool) on \(model.deviceDisplayName(approval.deviceId))")
            approveButton(approval, prominent: !riskRule)
                .disabled(!writable || !revealed.contains(approval.id))
                .accessibilityLabel("Approve \(approval.tool) on \(model.deviceDisplayName(approval.deviceId))")
                .accessibilityHint(revealed.contains(approval.id) ? "" : "Open the exact input first")
        }
    }

    /// Never the default action: Return must not approve. A risk-rule request keeps
    /// both buttons outline (equal weight); Manual makes Approve the teal fill.
    private func approveButton(_ approval: PendingApproval, prominent: Bool) -> some View {
        Button { answer(approval, approve: true) } label: {
            Label("Approve", systemImage: revealed.contains(approval.id) ? "checkmark" : "lock.fill")
        }
        .buttonStyle(GrouperActionButtonStyle(kind: prominent ? .primary : .outline))
    }

    /// Small text controls under the buttons: Change it and Stop, the timeout, the
    /// queue, and (in a thread) the chat's other requests.
    private func footer(_ approval: PendingApproval) -> some View {
        let writable = model.canAnswerApprovals && busyId == nil
        return VStack(alignment: .leading, spacing: 6) {
            if approval.threadId != nil {
                HStack {
                    Button("Change it") { answer(approval, approve: false, thenChange: true) }
                        .disabled(!writable)
                        .foregroundStyle(Theme.accent)
                        .accessibilityHint("Denies this call, then opens the thread with a message to tell it what to do instead")
                    Spacer()
                    Button("Stop thread", role: .destructive) { stopCandidate = approval }
                        .disabled(!writable)
                        .foregroundStyle(Theme.danger)
                }
                .font(.callout)
                .buttonStyle(.borderless)
            }
            HStack(spacing: 8) {
                TimelineView(.periodic(from: .now, by: 30)) { context in
                    Text(ApprovalTimeoutText.describe(approval, now: context.date))
                        .font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
                }
                Spacer(minLength: 8)
                if queue.count > 1 { queueControls(approval) }
            }
            if isThreadScope, otherInChat > 0 {
                HStack {
                    Text(otherInChat == 1 ? "1 more request in this chat" : "\(otherInChat) more requests in this chat")
                        .font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
                    if let onShowChatQueue {
                        Spacer()
                        Button("Show in chat", action: onShowChatQueue).font(.caption).buttonStyle(.borderless)
                            .foregroundStyle(Theme.accent)
                    }
                }
            }
        }
    }

    // MARK: - Settled

    private var settledHere: SettledApproval? {
        guard let settled = model.settledApproval else { return nil }
        switch scope {
        case .chat:
            return settled.approval.conversationId == model.selectedConversationId ? settled : nil
        case .thread(let threadId):
            return settled.approval.threadId == threadId ? settled : nil
        }
    }

    private func settledCard(_ settled: SettledApproval) -> some View {
        let more = !queue.isEmpty
        return VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 10) {
                Image(systemName: "checkmark.shield.fill").font(.title2).foregroundStyle(Theme.accent).accessibilityHidden(true)
                Text("Run this tool?").font(Theme.Typeface.dockTitle).foregroundStyle(Theme.text).accessibilityAddTraits(.isHeader)
            }
            Text("\(settled.approval.tool) on \(model.deviceDisplayName(settled.approval.deviceId)) · \(model.threadTitle(settled.approval.threadId))")
                .font(.subheadline).foregroundStyle(Theme.text2)
            HStack(alignment: .firstTextBaseline) {
                Label(settled.outcome.message, systemImage: icon(for: settled.outcome))
                    .font(.body.weight(.semibold)).foregroundStyle(Theme.text)
                Spacer()
                Button(more ? "Next request" : "OK") { model.dismissSettledApproval() }
                    .buttonStyle(.borderless)
                    .foregroundStyle(Theme.accent)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .task(id: settled.id) {
            AccessibilityNotification.Announcement(settled.outcome.message).post()
        }
    }

    private func icon(for outcome: ApprovalOutcome) -> String {
        switch outcome {
        case .answeredHere(let approved): return approved ? "checkmark.circle.fill" : "xmark.circle.fill"
        case .answeredElsewhere: return "checkmark.circle"
        case .threadEnded: return "stop.circle"
        case .timedOut: return "timer"
        case .noLongerWaiting: return "clock.badge.xmark"
        }
    }

    // MARK: - Actions

    private func inputExpanded(_ id: String) -> Binding<Bool> {
        Binding(
            get: { expanded.contains(id) },
            set: { open in
                if open {
                    expanded.insert(id)
                    revealed.insert(id)
                } else {
                    expanded.remove(id)
                }
            }
        )
    }

    private func answer(_ approval: PendingApproval, approve: Bool, thenChange: Bool = false) {
        guard busyId == nil else { return } // one write at a time; never a double request
        if approve && !revealed.contains(approval.id) { return }
        busyId = approval.id
        failure = nil
        Task {
            let result = await model.answerApproval(approval.id, approve: approve)
            busyId = nil
            switch result {
            case .done:
                if thenChange, let threadId = approval.threadId {
                    model.threadMessageDrafts[threadId] = "Don’t run that. Instead: "
                    model.selectedThreadId = threadId
                }
            case .settledElsewhere:
                break
            case .failed(let message):
                failure = "\(message) Nothing was sent twice; the request list was checked again."
            }
        }
    }

    private var stopDialogShown: Binding<Bool> {
        Binding(get: { stopCandidate != nil }, set: { shown in if !shown { stopCandidate = nil } })
    }

    private var stopTitle: String {
        "Stop \(model.threadTitle(stopCandidate?.threadId))?"
    }

    private func stop(_ approval: PendingApproval) {
        stopCandidate = nil
        guard let threadId = approval.threadId, busyId == nil else { return }
        busyId = approval.id
        failure = nil
        Task {
            let stopped = await model.stopThread(threadId)
            busyId = nil
            if !stopped { failure = model.errorMessage ?? "The thread could not be stopped." }
        }
    }

    // MARK: - VoiceOver

    /// One announcement per new request in the open chat, without moving focus.
    private func announceNew(_ ids: [String]) {
        let fresh = ids.filter { !announced.contains($0) }
        announced.formUnion(fresh)
        guard scope == .chat, let newest = fresh.last, model.dockFocusRequest == nil,
              let approval = queue.first(where: { $0.id == newest }) else { return }
        let waiting = queue.count == 1 ? "" : " \(queue.count) requests waiting."
        AccessibilityNotification.Announcement(
            "Needs review: \(approval.tool) on \(model.deviceDisplayName(approval.deviceId)).\(waiting)"
        ).post()
    }

    /// Opened from a notification or the threads list: focus the heading, not Approve.
    private func focusIfRequested(_ id: String?) {
        guard let id, queue.contains(where: { $0.id == id }) else { return }
        model.dockFocusRequest = nil
        Task {
            try? await Task.sleep(for: .milliseconds(400))
            headingFocused = true
        }
    }
}
