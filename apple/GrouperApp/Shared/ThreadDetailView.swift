import SwiftUI
import GrouperAPI

/// One thread, read like a chat (owner, 26 Nov: threads behave like a chat): the
/// orchestrator's brief is the first message, the thread model's text is the
/// assistant's (labelled with the model), tool calls are collapsible rows with the
/// exact input and output, and step-ins and the orchestrator's answers are messages.
/// The fixed header shows status with live elapsed time, device and the model the
/// orchestrator chose (no picker), then the thread's live plan checklist when it
/// keeps one (`update_plan`; those calls are not tool rows). The
/// waiting/checkpoint card, the result and rating sit around the transcript; the
/// step-in composer with Stop sits at the bottom, replaced by the approval dock
/// while this thread has a pending request.
///
/// The view has no NavigationStack of its own: the iPhone chat wraps it in one for
/// its sheet, and the threads panel pushes it.
struct ThreadDetailView: View {
    @EnvironmentObject private var model: AppModel
    let thread: ThreadInfo
    /// Set when the view is a sheet of its own: adds Done. A pushed view uses Back.
    var onClose: (() -> Void)? = nil
    @State private var message = ""
    @State private var stopping = false
    @State private var sending = false
    @State private var showStopConfirmation = false
    @State private var openCalls: Set<String> = []
    @State private var planExpanded = true

    private var liveThread: ThreadInfo {
        model.threads.first { $0.id == thread.id } ?? thread
    }

    var body: some View {
        let rows = model.activity(threadId: liveThread.id)
        let transcript = ThreadTranscript(activity: rows)
        let draft = model.threadDraft(liveThread.id)
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 14) {
                    attention(rows)
                    ForEach(transcript.messages) { item in messageView(item) }
                    if !draft.isEmpty {
                        if !transcript.threadSpokeLast { modelLabel(started: started(in: rows)) }
                        draftView(draft)
                    }
                    if transcript.messages.isEmpty && draft.isEmpty {
                        Text(liveThread.isActive ? "Working… output appears here as it streams." : "No output.")
                            .font(.callout).foregroundStyle(Theme.text3)
                    }
                    if liveThread.isTerminal { result(transcript.finishedSummary) }
                    Color.clear.frame(height: 1).id("end")
                }
                .padding()
            }
            .background(Theme.bg)
            .onChange(of: transcript.messages.count) { _, _ in
                withAnimation(.easeOut(duration: 0.2)) { proxy.scrollTo("end") }
            }
        }
        .safeAreaInset(edge: .top, spacing: 0) { header(started: started(in: rows), plan: ThreadPlan.latest(in: rows)) }
        .safeAreaInset(edge: .bottom, spacing: 0) { footer }
        .navigationTitle(liveThread.title.isEmpty ? "Thread" : liveThread.title)
        #if os(iOS)
        .navigationBarTitleDisplayMode(.inline)
        #endif
        .modifier(DoneToolbar(onClose: onClose))
        .confirmationDialog("Stop \(liveThread.title)?", isPresented: $showStopConfirmation) {
            Button("Stop thread", role: .destructive) { stop() }
        } message: {
            Text("Its current tool or model turn will be stopped, and work it hasn’t saved is lost. The orchestrator is told you stopped it.")
        }
        .task(id: thread.id) { message = model.threadMessageDrafts[thread.id] ?? "" }
        .onChange(of: message) { _, value in model.threadMessageDrafts[thread.id] = value }
        // "Change it" on an approval puts a prefixed draft here.
        .onChange(of: model.threadMessageDrafts[thread.id] ?? "") { _, value in
            if value != message { message = value }
        }
    }

    // MARK: - Header (fixed)

    private func header(started: ThreadStarted?, plan: ThreadPlan?) -> some View {
        let modelId = liveThread.model.isEmpty ? (started?.model ?? "") : liveThread.model
        let reason = liveThread.reason ?? started?.reason
        let family = model.family(ofModel: modelId)
        return VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 10) {
                StatusPill(status: liveThread.status, family: family, needsApproval: !approvals.isEmpty)
                ElapsedText(thread: liveThread)
                    .font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
            }
            HStack(spacing: 8) {
                Label(deviceName, systemImage: "desktopcomputer")
                    .font(Theme.Typeface.chip).foregroundStyle(Theme.text2).lineLimit(1)
                    .padding(.horizontal, 8).padding(.vertical, 4)
                    .background(Theme.surface2, in: RoundedRectangle(cornerRadius: Theme.Radius.sm))
                    .accessibilityLabel("Device: \(deviceName)")
                FixedModelChip(modelId: modelId, family: family, providerId: model.modelEntry(modelId)?.providerId)
            }
            if let reason, !reason.isEmpty {
                Text(reason).font(Theme.Typeface.meta).foregroundStyle(Theme.text3).lineLimit(3)
            }
            if let plan {
                PlanChecklist(plan: plan, expanded: $planExpanded)
            }
        }
        .padding(.horizontal).padding(.vertical, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.bgTint)
        .overlay(alignment: .bottom) { Rectangle().fill(Theme.border).frame(height: 1) }
    }

    // MARK: - Attention: approvals, waiting, checkpoint, hand-over

    /// This thread's pending requests, oldest first.
    private var approvals: [PendingApproval] {
        model.approvals(in: liveThread.conversationId).filter { $0.threadId == liveThread.id }
    }

    /// Pending requests from the chat's other threads; they don't block this thread.
    private var otherApprovalsInChat: Int {
        model.approvals(in: liveThread.conversationId).filter { $0.threadId != liveThread.id }.count
    }

    @ViewBuilder private func attention(_ rows: [ThreadActivityRow]) -> some View {
        let latest = LatestAttention(rows)
        let waiting = latest.waiting
        let checkpoint = latest.checkpoint
        let handover = latest.handover
        if liveThread.status == "waiting_device" {
            VStack(alignment: .leading, spacing: 6) {
                Label("\(deviceName) is offline", systemImage: "clock.fill").font(.headline).foregroundStyle(Theme.warn)
                if let device = model.devices.first(where: { $0.id == liveThread.deviceId }), let last = device.lastSeen {
                    HStack(spacing: 4) {
                        Text("Last seen")
                        RelativeAgeText(seconds: last)
                    }
                    .font(.caption).foregroundStyle(Theme.text2)
                } else {
                    Text("Not seen yet.").font(.caption).foregroundStyle(Theme.text2)
                }
                if let waiting, !waiting.reason.isEmpty { Text(waiting.reason).font(.callout).foregroundStyle(Theme.text) }
                if checkpoint == nil {
                    Text("Meanwhile the hub works towards a checkpoint, then waits for the device.").font(.callout).foregroundStyle(Theme.text2)
                }
            }
            .grouperCard(fill: Theme.warnSoft, stroke: Theme.warnSoft)
        }
        // The checkpoint stays visible after the thread resumes, so the hand-over is clear.
        if let checkpoint {
            VStack(alignment: .leading, spacing: 6) {
                Label("Checkpoint on the hub", systemImage: "flag.checkered").font(.headline).foregroundStyle(Theme.accent)
                Text(LocalizedStringKey(checkpoint.summary)).textSelection(.enabled).foregroundStyle(Theme.text)
            }
            .grouperCard()
        }
        if let handover {
            VStack(alignment: .leading, spacing: 4) {
                Label("Handed over to \(model.deviceDisplayName(handover.deviceId))", systemImage: "arrow.triangle.2.circlepath").font(.headline).foregroundStyle(Theme.text)
                Text(handover.files.isEmpty ? "No files to copy." : "Copied: \(handover.files.joined(separator: ", "))").font(.caption).foregroundStyle(Theme.text2)
                if !handover.skipped.isEmpty {
                    Text("Left out: \(handover.skipped.joined(separator: "; "))").font(.caption).foregroundStyle(Theme.text3)
                }
            }
            .grouperCard()
        }
    }

    // MARK: - Transcript

    @ViewBuilder private func messageView(_ item: ThreadMessage) -> some View {
        switch item {
        case .inbound(_, let sender, let label, let text):
            inboundBubble(sender: sender, label: label, text: text)
        case .modelLabel:
            modelLabel(started: nil)
        case .text(_, let text):
            Text(LocalizedStringKey(text))
                .foregroundStyle(Theme.text)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        case .tool(_, let call, let result):
            ToolCallRow(call: call, result: result, threadActive: liveThread.isActive, isExpanded: expanded(call.callId))
        case .ask(_, let question):
            VStack(alignment: .leading, spacing: 4) {
                Label("Asked the orchestrator", systemImage: "bubble.left.and.text.bubble.right").font(.caption.weight(.semibold)).foregroundStyle(Theme.text2)
                Text(question).textSelection(.enabled).foregroundStyle(Theme.text)
            }
            .grouperCard()
            .accessibilityElement(children: .combine)
        case .notice(_, let text):
            note(text, systemImage: "info.circle")
        case .handedOver(_, let deviceId):
            note("Continued on \(model.deviceDisplayName(deviceId)).", systemImage: "arrow.triangle.2.circlepath")
        case .approvalResolved(_, let tool, let approved):
            note(approved ? "You approved \(tool)." : "\(tool) was denied.", systemImage: "exclamationmark.shield")
        case .askFailed(_, let text):
            note("No answer: \(text)", systemImage: "exclamationmark.triangle")
        case .error(_, let message):
            ErrorBanner(message: message)
        }
    }

    /// A message to the thread, on the right like the owner's messages in the chat.
    private func inboundBubble(sender: ThreadMessage.Sender, label: String, text: String) -> some View {
        HStack {
            Spacer(minLength: 36)
            VStack(alignment: .leading, spacing: 4) {
                Text(label).font(.caption.weight(.semibold)).foregroundStyle(Theme.text2)
                Text(LocalizedStringKey(text)).textSelection(.enabled).foregroundStyle(Theme.text)
            }
            .padding(12)
            .background(sender == .owner ? Theme.surface : Theme.accentSoft, in: RoundedRectangle(cornerRadius: Theme.Radius.card))
            .overlay {
                RoundedRectangle(cornerRadius: Theme.Radius.card).strokeBorder(sender == .owner ? Theme.border : Theme.accentSoft, lineWidth: 1)
            }
        }
        .accessibilityElement(children: .combine)
    }

    /// Who is speaking: the thread's model, with its family accent.
    private func modelLabel(started: ThreadStarted?) -> some View {
        let modelId = liveThread.model.isEmpty ? (started?.model ?? "") : liveThread.model
        return HStack(spacing: 6) {
            FamilyMark(family: model.family(ofModel: modelId))
            Text(modelId.isEmpty ? "Thread model" : modelId).font(Theme.Typeface.chip).foregroundStyle(Theme.text2)
        }
        .accessibilityElement(children: .combine)
    }

    private func draftView(_ draft: String) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(draft).textSelection(.enabled).foregroundStyle(Theme.text)
            Label("Working…", systemImage: "ellipsis")
                .font(Theme.Typeface.chip).foregroundStyle(Theme.brand)
                .padding(.horizontal, 8).padding(.vertical, 2)
                .background(Theme.brandSoft, in: RoundedRectangle(cornerRadius: Theme.Radius.sm))
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Thread output so far. \(draft)")
    }

    private func note(_ text: String, systemImage: String) -> some View {
        Label(text, systemImage: systemImage).font(.callout).foregroundStyle(Theme.text3)
    }

    private func expanded(_ callId: String) -> Binding<Bool> {
        Binding(
            get: { openCalls.contains(callId) },
            set: { open in
                if open { openCalls.insert(callId) } else { openCalls.remove(callId) }
            }
        )
    }

    private func started(in rows: [ThreadActivityRow]) -> ThreadStarted? {
        for row in rows {
            if case .started(_, _, let value) = row { return value }
        }
        return nil
    }

    // MARK: - Result and rating

    private func result(_ finishedSummary: String?) -> some View {
        let summary = liveThread.summary ?? finishedSummary
        return VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("Result").font(.headline).foregroundStyle(Theme.text)
                Spacer()
                StatusPill(status: liveThread.status)
            }
            if let summary, !summary.isEmpty {
                Text(LocalizedStringKey(summary)).textSelection(.enabled).foregroundStyle(Theme.text)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            rating
        }
        .grouperCard(padding: 16)
    }

    private var rating: some View {
        GlassEffectContainer(spacing: 8) {
            HStack {
                Text("Rate this result").font(.subheadline.weight(.semibold)).foregroundStyle(Theme.text); Spacer()
                Button { Task { await model.rateThread(liveThread.id, rating: "up") } } label: { Image(systemName: "hand.thumbsup") }.accessibilityLabel("Rate up")
                Button { Task { await model.rateThread(liveThread.id, rating: "down") } } label: { Image(systemName: "hand.thumbsdown") }.accessibilityLabel("Rate down")
            }.buttonStyle(.glass)
        }
    }

    // MARK: - Footer: step in, Stop (or the approval dock)

    private var showsDock: Bool {
        if !approvals.isEmpty { return true }
        return model.settledApproval?.approval.threadId == liveThread.id
    }

    private var footer: some View {
        Group {
            if showsDock {
                ApprovalDock(queue: approvals, scope: .thread(liveThread.id),
                             otherInChat: otherApprovalsInChat, onShowChatQueue: onClose)
            } else if liveThread.isTerminal {
                Text("This thread has finished. Ask about it in the chat.")
                    .font(.callout).foregroundStyle(Theme.text3)
                    .frame(maxWidth: .infinity)
                    .padding()
            } else {
                GlassSurface {
                    VStack(alignment: .leading, spacing: 6) {
                        TextField("Message this thread", text: $message,
                                  prompt: Text("Message this thread").foregroundStyle(Theme.text3), axis: .vertical)
                            .lineLimit(1...4).textFieldStyle(.plain).font(Theme.Typeface.body).foregroundStyle(Theme.text)
                            .padding(.horizontal, 8).grouperMinimumControlSize()
                        HStack {
                            Button(role: .destructive) {
                                if liveThread.status == "running" { showStopConfirmation = true } else { stop() }
                            } label: {
                                if stopping { ProgressView() } else { Label("Stop", systemImage: "stop.circle") }
                            }
                            .buttonStyle(.borderless)
                            .foregroundStyle(Theme.danger)
                            .disabled(!model.online || stopping)
                            .grouperMinimumControlSize()
                            .accessibilityLabel("Stop thread")
                            Spacer()
                            Button { stepIn() } label: {
                                if sending { ProgressView().controlSize(.small) } else { Image(systemName: "arrow.up") }
                            }
                            .buttonStyle(SendButtonStyle())
                            .accessibilityLabel("Send to this thread")
                            .disabled(message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !model.online || sending)
                        }
                    }
                    .padding(10)
                }
                .padding(.horizontal).padding(.bottom, 6)
            }
        }
    }

    private var deviceName: String { model.deviceDisplayName(liveThread.deviceId) }

    private func stepIn() {
        let text = message.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        sending = true
        Task {
            // On failure (e.g. 409) the typed text stays put.
            if await model.messageThread(liveThread.id, text: text) { message = "" }
            sending = false
        }
    }

    private func stop() {
        stopping = true
        Task { await model.stopThread(liveThread.id); stopping = false }
    }
}

/// The newest waiting, checkpoint and hand-over records of a thread.
private struct LatestAttention {
    var waiting: ThreadWaiting?
    var checkpoint: ThreadCheckpoint?
    var handover: ThreadHandedOver?

    init(_ rows: [ThreadActivityRow]) {
        for row in rows {
            switch row {
            case .waiting(_, _, let value): waiting = value
            case .checkpoint(_, _, let value): checkpoint = value
            case .handedOver(_, _, let value): handover = value
            default: break
            }
        }
    }
}

/// Adds Done when the thread view is its own sheet.
private struct DoneToolbar: ViewModifier {
    let onClose: (() -> Void)?

    @ViewBuilder func body(content: Content) -> some View {
        if let onClose {
            content.toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done", action: onClose) }
            }
        } else {
            content
        }
    }
}

/// A tool call: collapsed by default, expanding to the exact input and output in
/// monospace. The expanded state is kept by the thread view while it is mounted.
private struct ToolCallRow: View {
    let call: ToolCall
    let result: ToolResult?
    let threadActive: Bool
    @Binding var isExpanded: Bool

    var body: some View {
        DisclosureGroup(isExpanded: $isExpanded) {
            VStack(alignment: .leading, spacing: 8) {
                if call.input != .null {
                    Text("Input").font(.caption.weight(.semibold)).foregroundStyle(Theme.text2)
                    Text(call.input.prettyPrinted).font(Theme.Typeface.mono).foregroundStyle(Theme.text).textSelection(.enabled)
                }
                if let result {
                    Text("Output").font(.caption.weight(.semibold)).foregroundStyle(Theme.text2)
                    Text(result.output.isEmpty ? "(empty)" : result.output).font(Theme.Typeface.mono).foregroundStyle(Theme.text).textSelection(.enabled)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.top, 8)
        } label: {
            HStack {
                Label(call.name, systemImage: "wrench.and.screwdriver").lineLimit(1).foregroundStyle(Theme.text)
                Spacer()
                Label(state, systemImage: stateIcon).font(.caption.weight(.semibold)).foregroundStyle(stateColor)
            }
        }
        .grouperCard()
    }

    private var state: String {
        if let result { return result.ok ? "OK" : "Failed" }
        return threadActive ? "Running" : "No result"
    }

    private var stateColor: Color {
        if let result { return result.ok ? Theme.ok : Theme.danger }
        return threadActive ? Theme.accent : Theme.text3
    }

    private var stateIcon: String {
        if let result { return result.ok ? "checkmark" : "xmark" }
        return threadActive ? "circle.dotted" : "minus"
    }
}

/// The thread's live plan, pinned above its chat: "Plan · 2 of 5 done", then one
/// row per step. Pending is an empty circle; the step in progress is highlighted in
/// the brand purple with a spinner and its active form; done is a tick with quieter
/// text. Collapsible; every row says its state in words for VoiceOver.
struct PlanChecklist: View {
    let plan: ThreadPlan
    @Binding var expanded: Bool

    /// Longer plans scroll inside the pinned header instead of pushing the chat away.
    private static let scrollAfter = 6

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Button {
                withAnimation(.easeOut(duration: 0.15)) { expanded.toggle() }
            } label: {
                HStack(spacing: 8) {
                    Image(systemName: "checklist").foregroundStyle(Theme.brand)
                    Text("Plan").font(Theme.Typeface.row.weight(.semibold)).foregroundStyle(Theme.text)
                    Text("· \(plan.progressText)").font(Theme.Typeface.row).foregroundStyle(Theme.text2)
                    Spacer(minLength: 8)
                    Image(systemName: "chevron.down")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(Theme.text3)
                        .rotationEffect(.degrees(expanded ? 0 : -90))
                }
                .frame(minHeight: 32)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("Plan, \(plan.progressText)")
            .accessibilityValue(expanded ? "Expanded" : "Collapsed")
            .accessibilityHint(expanded ? "Hides the steps" : "Shows the steps")
            .accessibilityAddTraits(.isButton)

            ProgressView(value: Double(plan.completedCount), total: Double(max(plan.items.count, 1)))
                .tint(Theme.brandFill)
                .accessibilityHidden(true)

            if expanded {
                if plan.items.count > Self.scrollAfter {
                    ScrollView { rows }.frame(maxHeight: 220)
                } else {
                    rows
                }
            }
        }
        .grouperCard(padding: 10)
        // Progress is visual; say it too when a step is ticked off.
        .onChange(of: plan.completedCount) { old, new in
            if new > old { AccessibilityNotification.Announcement("Plan: \(plan.progressText)").post() }
        }
    }

    private var rows: some View {
        VStack(alignment: .leading, spacing: 2) {
            ForEach(Array(plan.items.enumerated()), id: \.offset) { _, item in row(item) }
        }
    }

    private func row(_ item: ThreadPlan.Item) -> some View {
        let inProgress = item.status == .inProgress
        return HStack(spacing: 10) {
            Group {
                switch item.status {
                case .pending:
                    Image(systemName: "circle").foregroundStyle(Theme.text3)
                case .inProgress:
                    ProgressView().controlSize(.small).tint(Theme.brand)
                case .completed:
                    Image(systemName: "checkmark.circle.fill").foregroundStyle(Theme.ok)
                }
            }
            .frame(width: 18, height: 18)
            Text(item.displayText)
                .font(inProgress ? Theme.Typeface.row.weight(.semibold) : Theme.Typeface.row)
                .foregroundStyle(item.status == .completed ? Theme.text3 : inProgress ? Theme.brand : Theme.text)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 8).padding(.vertical, 5)
        .background(inProgress ? Theme.brandSoft : Color.clear, in: RoundedRectangle(cornerRadius: Theme.Radius.sm))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(item.displayText), \(Self.spoken(item.status))")
    }

    private static func spoken(_ status: ThreadPlan.Status) -> String {
        switch status {
        case .pending: return "not started"
        case .inProgress: return "in progress"
        case .completed: return "done"
        }
    }
}