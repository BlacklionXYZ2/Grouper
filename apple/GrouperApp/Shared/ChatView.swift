import SwiftUI
import GrouperAPI

struct ChatView: View {
    @EnvironmentObject private var model: AppModel
    @State private var message = ""
    @State private var sending = false
    @State private var lastAnnouncement = Date.distantPast
    /// The chat-scoped threads list: a sheet on iPhone, an inspector on Mac.
    @State private var showThreads = false
    @State private var showModelPicker = false
    @FocusState private var composerFocused: Bool
    /// The one sheet this chat shows (see `ChatSheet`), and the one actually on
    /// screen. `shownSheet` is cleared only when a dismissal finishes, so a new
    /// request waits for the old sheet to go instead of being dropped.
    @State private var sheet: ChatSheet?
    @State private var shownSheet: ChatSheet?

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 16) {
                    if !model.online { offlineBanner }
                    if let notice = visibleNotice { noticeBanner(notice) }
                    if model.selectedConversationId == nil { newConversationIntro }
                    ForEach(Self.chatItems(model.transcript)) { item in chatItem(item) }
                    if let draft = model.selectedFolder?.assistantDraft, !draft.isEmpty {
                        assistantMessage(draft, streaming: true)
                            .id("streaming")
                            .onChange(of: draft) { _, value in announceIfNeeded(value) }
                    }
                    Color.clear.frame(height: 1).id("end")
                }
                .padding()
                .frame(maxWidth: Theme.Size.transcriptMax)
                .frame(maxWidth: .infinity)
            }
            .background(Theme.bg)
            .dismissesKeyboardOnTap($composerFocused)
            .onChange(of: model.transcript.count) { _, _ in withAnimation(.easeOut(duration: 0.2)) { proxy.scrollTo("end") } }
            // VERIFY ON MAC: Confirm safeAreaBar(edge: .bottom) scroll-edge blur and Reduce Transparency
            // with the GlassSurface composer card and its round send button on iOS 26 and macOS 26.
            // While the chat has a pending approval the dock takes the composer's place;
            // `message` lives on this view, so the draft comes back afterwards.
            .safeAreaBar(edge: .bottom) { bottomBar }
        }
        .navigationTitle(model.selectedConversation?.title ?? "New chat")
        .toolbar {
            threadsButton
            chatMenu
        }
        #if os(macOS)
        .modifier(ThreadsInspector(showThreads: $showThreads, conversationId: threadsConversationId))
        #endif
        .sheet(item: $sheet, onDismiss: sheetDismissed) { item in sheetContent(item) }
        .onChange(of: requestedSheet, initial: true) { _, wanted in route(wanted) }
        .onChange(of: model.selectedConversationId) { _, _ in showThreads = false }
        .onAppear { model.chatViewsOnScreen += 1 }
        .onDisappear { model.chatViewsOnScreen = max(0, model.chatViewsOnScreen - 1) }
    }

    // MARK: - Approval dock

    @ViewBuilder private var bottomBar: some View {
        if let conversationId = model.selectedConversationId, showsDock(conversationId) {
            ApprovalDock(queue: model.approvals(in: conversationId), scope: .chat)
        } else {
            composer
        }
    }

    private func showsDock(_ conversationId: String) -> Bool {
        if !model.approvals(in: conversationId).isEmpty { return true }
        return model.settledApproval?.approval.conversationId == conversationId
    }

    private var visibleNotice: ChatNotice? {
        guard let notice = model.chatNotice else { return nil }
        guard let id = notice.conversationId else { return notice }
        return id == model.selectedConversationId ? notice : nil
    }

    private func noticeBanner(_ notice: ChatNotice) -> some View {
        HStack(alignment: .firstTextBaseline) {
            Label(notice.text, systemImage: "info.circle").font(.callout).foregroundStyle(Theme.text)
            Spacer()
            Button("Dismiss") { model.chatNotice = nil }.font(.callout).buttonStyle(.borderless).foregroundStyle(Theme.accent)
        }
        .grouperCard()
        .accessibilityElement(children: .combine)
    }

    // MARK: - Sheets

    /// What the chat wants on screen, in priority order. One `.sheet` shows it, so two
    /// requests in one turn (e.g. the threads sheet closing as a thread opens) queue
    /// instead of one being dropped. Approvals are not sheets: they use the dock.
    private var requestedSheet: ChatSheet? {
        if showModelPicker { return .modelPicker }
        #if os(iOS)
        // While the threads sheet is up it pushes its own threads.
        if showThreads, let id = threadsConversationId { return .threads(id) }
        if let thread = model.selectedThread { return .thread(thread.id) }
        #endif
        return nil
    }

    private func route(_ wanted: ChatSheet?) {
        if shownSheet == nil {
            guard let wanted else { return }
            shownSheet = wanted
            sheet = wanted
        } else if wanted != shownSheet, sheet != nil {
            sheet = nil // sheetDismissed presents whatever is wanted next
        }
    }

    private func sheetDismissed() {
        let dismissed = shownSheet
        // Dismissed by the person (or by the sheet itself) while still wanted: clear
        // its request. If the request had already moved on, leave the new one alone.
        if let dismissed, requestedSheet == dismissed {
            switch dismissed {
            case .modelPicker: showModelPicker = false
            case .threads: showThreads = false
            case .thread(let id): if model.selectedThreadId == id { model.selectedThreadId = nil }
            }
        }
        shownSheet = nil
        route(requestedSheet)
    }

    @ViewBuilder private func sheetContent(_ item: ChatSheet) -> some View {
        switch item {
        case .modelPicker:
            ModelPickerSheet(role: chatRole, current: currentModelId) { chooseModel($0) }
        case .threads(let conversationId):
            ThreadsPanel(conversationId: conversationId) { showThreads = false }
        case .thread(let id):
            NavigationStack {
                if let thread = model.threads.first(where: { $0.id == id }) {
                    ThreadDetailView(thread: thread) { model.selectedThreadId = nil }
                } else {
                    ContentUnavailableView("Thread not found on this hub", systemImage: "questionmark.circle")
                }
            }
        }
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
                        Circle().fill(Theme.brandFill).frame(width: 8, height: 8)
                    }
                }
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(needs > 0 ? "Threads, \(count), \(needs) need you" : "Threads, \(count)")
            }
            .disabled(threadsConversationId == nil)
            .help("Threads in this chat")
        }
    }

    private var offlineBanner: some View {
        Label("Offline. This is the last stored copy; live controls are disabled.", systemImage: "wifi.slash")
            .font(.callout).foregroundStyle(Theme.warn)
            .grouperCard(fill: Theme.warnSoft, stroke: Theme.warnSoft)
    }

    /// A new chat: Grouper's mark and the Project / Standard choice as two cards
    /// (the choice is fixed after the first message, so it isn't in the composer).
    private var newConversationIntro: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 12) {
                GrouperAvatar(size: 44)
                Text("New chat").font(.title2.bold()).foregroundStyle(Theme.text)
                    .accessibilityAddTraits(.isHeader)
            }
            HStack(alignment: .top, spacing: 12) {
                modeCard(mode: "project", title: "Project", systemImage: "point.3.connected.trianglepath.dotted",
                         text: "Grouper hands work to your Mac, PC or the hub as threads you can follow here.")
                modeCard(mode: "standard", title: "Standard", systemImage: "bubble.left",
                         text: "Ask one model a short question, without threads.")
            }
        }
        .padding(.vertical, 20)
    }

    private func modeCard(mode: String, title: String, systemImage: String, text: String) -> some View {
        let selected = model.newChatMode == mode
        return Button {
            if !selected { model.beginNewChat(mode: mode) }
        } label: {
            VStack(alignment: .leading, spacing: 6) {
                Label(title, systemImage: systemImage)
                    .font(Theme.Typeface.title)
                    .foregroundStyle(selected ? Theme.accent : Theme.text)
                Text(text).font(.callout).foregroundStyle(Theme.text2)
                    .multilineTextAlignment(.leading)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .grouperCard(fill: selected ? Theme.accentSoft : Theme.surface,
                         stroke: selected ? Theme.accentFillEdge : Theme.border)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(selected ? .isSelected : [])
        .accessibilityHint("\(title) chat")
    }

    @ViewBuilder private func chatItem(_ item: ChatItem) -> some View {
        switch item {
        case .threads(let cards):
            ThreadsCard(cards: cards, onOpen: { openThread($0) }, onDetails: { showThreads = true })
        case .row(let row):
            transcriptRow(row)
        }
    }

    @ViewBuilder private func transcriptRow(_ row: TranscriptRow) -> some View {
        switch row {
        case .user(_, let ts, let text):
            HStack(alignment: .top, spacing: 10) {
                UserAvatar(initial: ownerInitial)
                Text(text)
                    .font(Theme.Typeface.body)
                    .foregroundStyle(Theme.text)
                    .textSelection(.enabled)
                    .padding(.horizontal, 14).padding(.vertical, 10)
                    .background(Theme.surface, in: RoundedRectangle(cornerRadius: Theme.Radius.card))
                    .overlay { RoundedRectangle(cornerRadius: Theme.Radius.card).strokeBorder(Theme.border, lineWidth: 1) }
                Spacer(minLength: 8)
                timeText(ts)
            }
            .accessibilityElement(children: .combine).accessibilityLabel("You. \(text)")
        case .assistant(_, let ts, let text): assistantMessage(text, streaming: false, ts: ts)
        case .thread(let card):
            // Grouped into a ThreadsCard by `chatItems`; a lone card still renders.
            ThreadsCard(cards: [card], onOpen: { openThread($0) }, onDetails: { showThreads = true })
        case .error(_, _, let message): ErrorBanner(message: message)
        case .budgetWarning(_, _, let warning):
            let spent = warning.spent.formatted(.number.precision(.fractionLength(2)))
            let limit = warning.limit.formatted(.number.precision(.fractionLength(2)))
            Label("Estimated \(warning.scope) spend is $\(spent) of $\(limit).", systemImage: "dollarsign.circle")
                .font(.callout).foregroundStyle(Theme.warn)
        }
    }

    /// Grouper's turn: the mark, its name, a "Thinking…" chip while the reply
    /// streams, then the text on the canvas (no bubble).
    private func assistantMessage(_ text: String, streaming: Bool, ts: UInt64 = 0) -> some View {
        HStack(alignment: .top, spacing: 10) {
            GrouperAvatar()
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 8) {
                    Text("Grouper").font(Theme.Typeface.row.weight(.semibold)).foregroundStyle(Theme.text)
                    if streaming {
                        Label("Thinking…", systemImage: "ellipsis")
                            .font(Theme.Typeface.chip)
                            .foregroundStyle(Theme.brand)
                            .padding(.horizontal, 8).padding(.vertical, 2)
                            .background(Theme.brandSoft, in: RoundedRectangle(cornerRadius: Theme.Radius.sm))
                    }
                    Spacer(minLength: 8)
                    timeText(ts)
                }
                .frame(minHeight: Theme.Size.avatar)
                Text(LocalizedStringKey(text))
                    .font(Theme.Typeface.body)
                    .foregroundStyle(Theme.text)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(streaming ? "Reply so far. \(text)" : "Grouper. \(text)")
    }

    /// "21:14" beside a message; nothing when the event has no time.
    @ViewBuilder private func timeText(_ ts: UInt64) -> some View {
        if ts > 0 {
            Text(Date(timeIntervalSince1970: TimeInterval(ts)), format: .dateTime.hour().minute())
                .font(Theme.Typeface.meta)
                .foregroundStyle(Theme.text3)
                .padding(.top, 8)
        }
    }

    /// The owner's initial for the avatar: the linked Google account's name or email.
    private var ownerInitial: String? {
        let name = model.linkedGoogle.map { $0.name.isEmpty ? $0.email : $0.name } ?? ""
        return name.trimmingCharacters(in: .whitespaces).first.map { String($0).uppercased() }
    }

    private func openThread(_ id: String) {
        model.selectedThreadId = id
        #if os(macOS)
        showThreads = true // the inspector mounts and opens this thread
        #endif
    }

    /// The composer card (V8): model chip and the round send button, the text below.
    /// iPhone keeps it one line and shows the model chip while typing; the chat
    /// menu also has the model choice. Nothing else sits in it (no dead buttons).
    private var composer: some View {
        GlassSurface {
            #if os(iOS)
            VStack(alignment: .leading, spacing: 2) {
                if composerFocused || !message.isEmpty { modelChip }
                HStack(alignment: .bottom, spacing: 4) {
                    messageField("Message Grouper…")
                    sendButton
                }
            }
            .padding(.leading, 12).padding(.trailing, 4).padding(.vertical, 2)
            #else
            VStack(alignment: .leading, spacing: 0) {
                HStack {
                    modelChip
                    Spacer()
                    sendButton
                }
                messageField("Ask Grouper anything…")
            }
            .padding(.horizontal, 10).padding(.vertical, 4)
            #endif
        }
        .frame(maxWidth: Theme.Size.transcriptMax)
        .padding(.horizontal).padding(.bottom, 6)
        .frame(maxWidth: .infinity)
    }

    private func messageField(_ placeholder: String) -> some View {
        TextField("Message", text: $message, prompt: Text(placeholder).foregroundStyle(Theme.text4), axis: .vertical)
            .lineLimit(1...4)
            .textFieldStyle(.plain)
            .font(Theme.Typeface.body)
            .foregroundStyle(Theme.text)
            .focused($composerFocused)
            .frame(minHeight: 44)
    }

    /// A covering picker, not a small menu: there are many models.
    private var modelChip: some View {
        Button { showModelPicker = true } label: {
            HStack(spacing: 6) {
                if let id = currentModelId {
                    FamilyMark(family: model.family(ofModel: id))
                } else {
                    Image(systemName: "sparkles").foregroundStyle(Theme.brand)
                }
                Text(currentModelId ?? "Default model")
                    .font(Theme.Typeface.chip).foregroundStyle(Theme.text).lineLimit(1)
                Image(systemName: "chevron.down").font(.caption2.weight(.semibold)).foregroundStyle(Theme.text3)
            }
            .padding(.horizontal, 8).padding(.vertical, 4)
            .background(Theme.surfaceSelect, in: RoundedRectangle(cornerRadius: Theme.Radius.sm))
            .frame(minHeight: 44)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Choose model")
        .accessibilityValue(currentModelId ?? "Default model")
        .help(currentModelId.map { "Model: \($0)" } ?? "Model: default")
    }

    private var sendButton: some View {
        Button { send() } label: {
            if sending { ProgressView().controlSize(.small) } else { Image(systemName: "arrow.up") }
        }
        .buttonStyle(SendButtonStyle())
        .accessibilityLabel("Send message")
        .disabled(message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !model.online || sending)
    }
    /// Project chats pick an orchestrator model; standard chats a standard one.
    private var chatRole: String {
        (model.selectedConversation?.mode ?? model.newChatMode) == "standard" ? "standard" : "orchestrator"
    }

    /// The chat's pinned model, or the one chosen for a chat not yet started. Nil is the default.
    private var currentModelId: String? {
        let id = model.selectedConversationId == nil ? model.newChatModel : model.selectedConversation?.model
        guard let id, !id.isEmpty else { return nil }
        return id
    }

    @ToolbarContentBuilder private var chatMenu: some ToolbarContent {
        ToolbarItem(placement: .primaryAction) {
            Menu {
                Button("New project") { model.beginNewChat(mode: "project") }
                Button("New standard") { model.beginNewChat(mode: "standard") }
                Button("Choose model…") { showModelPicker = true }
                Divider()
                ForEach(model.conversations) { conversation in
                    Button(conversation.title) { Task { await model.selectConversation(conversation.id) } }
                }
            } label: { Image(systemName: "ellipsis.circle") }.accessibilityLabel("Chats and options")
        }
    }

    private func chooseModel(_ modelId: String?) {
        if model.selectedConversationId != nil {
            Task { await model.setConversationModel(modelId) }
        } else {
            model.newChatModel = modelId
        }
    }

    private func send() {
        let text = message.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        sending = true
        Task {
            if await model.send(text, model: model.newChatModel) { message = "" }
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


/// The transcript as shown: consecutive thread cards (one delegation burst) become
/// one `ThreadsCard`; everything else is a row of its own.
enum ChatItem: Identifiable {
    case row(TranscriptRow)
    case threads([ThreadCard])

    var id: String {
        switch self {
        case .row(let row): return row.id
        case .threads(let cards): return "threads:\(cards.first?.threadId ?? "")"
        }
    }
}

extension ChatView {
    static func chatItems(_ rows: [TranscriptRow]) -> [ChatItem] {
        var items: [ChatItem] = []
        for row in rows {
            guard case .thread(let card) = row else {
                items.append(.row(row))
                continue
            }
            if case .threads(let cards)? = items.last {
                items[items.count - 1] = .threads(cards + [card])
            } else {
                items.append(.threads([card]))
            }
        }
        return items
    }
}

/// The chat's sheets. One `.sheet(item:)` shows them all, so they never compete.
/// Approvals are not among them: they take the composer's place (`ApprovalDock`).
///
/// iPhone: the Threads button opens `.threads` (which pushes its own threads); a
/// thread card or "Change it" opens that thread as `.thread`.
/// Mac: threads live in an inspector instead, so only `.modelPicker` is requested.
enum ChatSheet: Identifiable, Equatable {
    case modelPicker
    case threads(String)
    case thread(String)

    var id: String {
        switch self {
        case .modelPicker: return "model-picker"
        case .threads(let conversationId): return "threads:\(conversationId)"
        case .thread(let threadId): return "thread:\(threadId)"
        }
    }
}

#if os(macOS)
/// Mac: the Threads button toggles an inspector beside the transcript, and a thread
/// card (or "Change it" in the approval dock) opens the inspector on that thread. The
/// panel exists only while the inspector is shown, so nothing ticks while it is
/// hidden and the panel always sees the thread it was opened for.
private struct ThreadsInspector: ViewModifier {
    @EnvironmentObject private var model: AppModel
    @Binding var showThreads: Bool
    let conversationId: String?

    func body(content: Content) -> some View {
        content
            .inspector(isPresented: Binding(get: { showThreads && conversationId != nil }, set: { showThreads = $0 })) {
                Group {
                    if showThreads, let conversationId {
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
    }
}
#endif