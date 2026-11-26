import SwiftUI
import GrouperAPI

struct ChatView: View {
    @EnvironmentObject private var model: AppModel
    @State private var message = ""
    @State private var selectedModel: String?
    @State private var sending = false
    @State private var lastAnnouncement = Date.distantPast
    /// The chat-scoped threads list: a sheet on iPhone, an inspector on Mac.
    @State private var showThreads = false
    @State private var showModelPicker = false

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 16) {
                    if !model.online { offlineBanner }
                    if let approval = scopedApprovals.first { approvalBanner(approval) }
                    if model.selectedConversationId == nil { newConversationIntro }
                    ForEach(model.transcript) { row in transcriptRow(row) }
                    if let draft = model.selectedFolder?.assistantDraft, !draft.isEmpty {
                        assistantMessage(draft, streaming: true)
                            .id("streaming")
                            .onChange(of: draft) { _, value in announceIfNeeded(value) }
                    }
                    Color.clear.frame(height: 1).id("end")
                }
                .padding()
            }
            .onChange(of: model.transcript.count) { _, _ in withAnimation(.easeOut(duration: 0.2)) { proxy.scrollTo("end") } }
            // VERIFY ON MAC: Confirm safeAreaBar(edge: .bottom) scroll-edge blur and Reduce Transparency
            // with GlassSurface + .glassProminent Send button on both iOS 26 and macOS 26.
            .safeAreaBar(edge: .bottom) { composer }
        }
        .navigationTitle(model.selectedConversation?.title ?? "New chat")
        .toolbar {
            threadsButton
            chatMenu
        }
        .modifier(ThreadPresentation(showThreads: $showThreads, conversationId: threadsConversationId))
        // While the iPhone threads sheet is up it presents its own approvals and threads.
        .sheet(item: Binding(get: { threadsSheetUp ? nil : model.selectedApproval }, set: { if $0 == nil { model.selectedApprovalId = nil } })) { approval in
            ApprovalSheet(approval: approval)
        }
        .sheet(isPresented: $showModelPicker) {
            ModelPickerSheet(role: chatRole, current: currentModelId) { chooseModel($0) }
        }
        .onChange(of: model.selectedConversationId) { _, _ in showThreads = false }
    }

    /// On iPhone the threads list is a sheet, which the chat's other sheets must not cover.
    private var threadsSheetUp: Bool {
        #if os(iOS)
        return showThreads && threadsConversationId != nil
        #else
        return false
        #endif
    }

    /// The open project chat, if it can have threads (standard chats cannot).
    private var threadsConversationId: String? {
        guard let conversation = model.selectedConversation, conversation.mode != "standard" else { return nil }
        return conversation.id
    }

    @ToolbarContentBuilder private var threadsButton: some ToolbarContent {
        ToolbarItem(placement: .primaryAction) {
            Button { showThreads.toggle() } label: {
                let id = threadsConversationId ?? ""
                let count = model.threads(in: id).count
                let needs = model.needsYouCount(in: id)
                HStack(spacing: 4) {
                    Image(systemName: "point.3.connected.trianglepath.dotted")
                    Text("\(count)").monospacedDigit()
                    if needs > 0 {
                        Circle().fill(Color.orange).frame(width: 8, height: 8)
                    }
                }
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(needs > 0 ? "Threads, \(count), \(needs) need you" : "Threads, \(count)")
            }
            .disabled(threadsConversationId == nil)
            .help("Threads in this chat")
        }
    }

    private var scopedApprovals: [PendingApproval] {
        model.pendingApprovals.filter { $0.conversationId == model.selectedConversationId }
    }

    private var offlineBanner: some View {
        Label("Offline. This is the last stored copy; live controls are disabled.", systemImage: "wifi.slash")
            .font(.callout).padding().frame(maxWidth: .infinity, alignment: .leading)
            .background(Color.orange.opacity(0.12), in: .rect(cornerRadius: 12))
    }

    private var newConversationIntro: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(model.newChatMode == "project" ? "New project chat" : "New standard chat").font(.title2.bold())
            Text(model.newChatMode == "project"
                 ? "Ask Grouper to hand work to your Mac, PC, or the hub and follow each thread here."
                 : "Ask one model a short question without delegated threads.")
                .foregroundStyle(.secondary)
        }.padding(.vertical, 24)
    }

    @ViewBuilder private func transcriptRow(_ row: TranscriptRow) -> some View {
        switch row {
        case .user(_, _, let text):
            HStack { Spacer(minLength: 36); Text(text).padding(12).background(Color.grouperSecondaryBackground, in: .rect(cornerRadius: 16)) }
                .accessibilityElement(children: .combine).accessibilityLabel("You. \(text)")
        case .assistant(_, _, let text): assistantMessage(text, streaming: false)
        case .thread(let card):
            Button { model.selectedThreadId = card.threadId } label: {
                ThreadChip(card: card, deviceName: deviceName(card.deviceId))
            }.buttonStyle(.plain)
        case .error(_, _, let message): ErrorBanner(message: message)
        case .budgetWarning(_, _, let warning):
            let spent = warning.spent.formatted(.number.precision(.fractionLength(2)))
            let limit = warning.limit.formatted(.number.precision(.fractionLength(2)))
            Label("Estimated \(warning.scope) spend is $\(spent) of $\(limit).", systemImage: "dollarsign.circle")
                .font(.callout).foregroundStyle(.orange)
        }
    }

    private func assistantMessage(_ text: String, streaming: Bool) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(LocalizedStringKey(text)).textSelection(.enabled)
            if streaming { Label("Replying", systemImage: "ellipsis").font(.caption).foregroundStyle(.teal) }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(streaming ? "Reply so far. \(text)" : "Grouper. \(text)")
    }

    private func approvalBanner(_ approval: PendingApproval) -> some View {
        Button { model.selectedApprovalId = approval.id } label: {
            HStack(alignment: .top) {
                Image(systemName: "exclamationmark.shield.fill").foregroundStyle(.orange)
                VStack(alignment: .leading) {
                    Text("Approval needed").font(.headline)
                    Text("\(approval.tool) on \(deviceName(approval.deviceId))").font(.subheadline)
                    Text(approval.reason).font(.callout).foregroundStyle(.secondary).lineLimit(2)
                }
                Spacer(); Image(systemName: "chevron.right")
            }.padding().background(Color.orange.opacity(0.1), in: .rect(cornerRadius: 14))
        }.buttonStyle(.plain)
    }

    private var composer: some View {
        GlassSurface {
            HStack(alignment: .bottom, spacing: 10) {
                TextField("Message", text: $message, axis: .vertical)
                    .lineLimit(1...4).textFieldStyle(.plain).padding(.horizontal, 8).grouperMinimumControlSize()
                // A covering picker, not a small menu: there are many models.
                Button { showModelPicker = true } label: {
                    Group {
                        if let id = currentModelId {
                            FamilyMark(family: model.family(ofModel: id))
                        } else {
                            Image(systemName: "cpu")
                        }
                    }
                    .grouperMinimumControlSize()
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Choose model")
                .accessibilityValue(currentModelId ?? "Default model")
                .help(currentModelId.map { "Model: \($0)" } ?? "Model: default")
                Button { send() } label: {
                    if sending { ProgressView().frame(width: 22, height: 22) } else { Image(systemName: "arrow.up") }
                }
                .buttonStyle(.glassProminent).grouperMinimumControlSize()
                .accessibilityLabel("Send message")
                .disabled(message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !model.online || sending)
            }.padding(10)
        }.padding(.horizontal).padding(.bottom, 6)
    }

    /// Project chats pick an orchestrator model; standard chats a standard one.
    private var chatRole: String {
        (model.selectedConversation?.mode ?? model.newChatMode) == "standard" ? "standard" : "orchestrator"
    }

    /// The chat's pinned model, or the one chosen for a chat not yet started. Nil is the default.
    private var currentModelId: String? {
        let id = model.selectedConversationId == nil ? selectedModel : model.selectedConversation?.model
        guard let id, !id.isEmpty else { return nil }
        return id
    }

    @ToolbarContentBuilder private var chatMenu: some ToolbarContent {
        ToolbarItem(placement: .primaryAction) {
            Menu {
                Button("New project") { model.beginNewChat(mode: "project") }
                Button("New standard") { model.beginNewChat(mode: "standard") }
                Divider()
                ForEach(model.conversations) { conversation in
                    Button(conversation.title) { Task { await model.selectConversation(conversation.id) } }
                }
            } label: { Image(systemName: "ellipsis.circle") }.accessibilityLabel("Chats and options")
        }
    }

    private func chooseModel(_ modelId: String?) {
        selectedModel = modelId
        if model.selectedConversationId != nil {
            Task { await model.setConversationModel(modelId) }
        }
    }

    private func send() {
        let text = message.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        sending = true
        Task {
            if await model.send(text, model: selectedModel) { message = "" }
            sending = false
        }
    }

    private func announceIfNeeded(_ text: String) {
        let sentenceEnded = text.last.map { ".!?\n".contains($0) } ?? false
        guard sentenceEnded || Date().timeIntervalSince(lastAnnouncement) >= 8 else { return }
        AccessibilityNotification.Announcement(text).post()
        lastAnnouncement = Date()
    }

    private func deviceName(_ id: String) -> String { model.devices.first { $0.id == id }?.name ?? id }
}

/// How threads open from a chat.
///
/// iPhone: the Threads button opens a sheet with this chat's threads; a thread card
/// opens that thread as its own full-height sheet (unless the threads sheet is up,
/// which then pushes it instead).
///
/// Mac: the Threads button toggles an inspector beside the transcript, and a thread
/// card opens the inspector on that thread.
private struct ThreadPresentation: ViewModifier {
    @EnvironmentObject private var model: AppModel
    @Binding var showThreads: Bool
    let conversationId: String?

    @ViewBuilder func body(content: Content) -> some View {
        #if os(iOS)
        content
            .sheet(isPresented: Binding(get: { showThreads && conversationId != nil }, set: { showThreads = $0 })) {
                if let conversationId {
                    ThreadsPanel(conversationId: conversationId) { showThreads = false }
                        .id(conversationId)
                }
            }
            .sheet(item: Binding(get: { showThreads ? nil : model.selectedThread }, set: { if $0 == nil { model.selectedThreadId = nil } })) { thread in
                NavigationStack {
                    ThreadDetailView(thread: thread) { model.selectedThreadId = nil }
                }
            }
        #else
        content
            .inspector(isPresented: Binding(get: { showThreads && conversationId != nil }, set: { showThreads = $0 })) {
                Group {
                    if let conversationId {
                        ThreadsPanel(conversationId: conversationId) { showThreads = false }
                            .id(conversationId)
                    }
                }
                .inspectorColumnWidth(min: 320, ideal: 400, max: 520)
            }
            .onChange(of: model.selectedThreadId) { _, id in
                guard let id, let conversationId, model.threads.contains(where: { $0.id == id && $0.conversationId == conversationId }) else { return }
                showThreads = true
            }
        #endif
    }
}
