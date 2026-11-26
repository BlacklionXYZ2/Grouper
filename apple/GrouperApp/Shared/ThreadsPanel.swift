import SwiftUI
import GrouperAPI

/// Threads for one project chat, opened from the chat's Threads button: a sheet on
/// iPhone, an inspector beside the transcript on Mac. Threads belong to their chat,
/// so there is no global thread list. Rows group as Needs you / Running / Finished;
/// selecting one pushes the chat-style thread view inside the panel.
struct ThreadsPanel: View {
    @EnvironmentObject private var model: AppModel
    let conversationId: String
    let onClose: () -> Void
    @State private var path: [String] = []
    @State private var reviewing: PendingApproval?

    var body: some View {
        NavigationStack(path: $path) {
            ThreadsList(conversationId: conversationId, reviewing: $reviewing, onClose: onClose)
                .navigationTitle("Threads")
                #if os(iOS)
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) { Button("Close", action: onClose) }
                }
                #endif
                .navigationDestination(for: String.self) { id in
                    Group {
                        if let thread = model.threads.first(where: { $0.id == id && $0.conversationId == conversationId }) {
                            ThreadDetailView(thread: thread)
                        } else {
                            ContentUnavailableView("Thread not found on this hub", systemImage: "questionmark.circle",
                                                   description: Text("It may have been removed or belong to another chat."))
                        }
                    }
                    #if os(macOS)
                    // A stack inside the inspector has no reliable system Back on the Mac.
                    .safeAreaInset(edge: .top, spacing: 0) { backBar }
                    #endif
                }
        }
        // Approvals reviewed from the panel present from the panel, so they never
        // compete with the chat's own sheets.
        .sheet(item: $reviewing) { ApprovalSheet(approval: $0) }
        // A thread chosen elsewhere (a chat card on Mac, "Change it" in an approval)
        // opens here when the panel is showing.
        .onChange(of: model.selectedThreadId, initial: true) { _, id in open(id) }
        #if os(iOS)
        // On iPhone the chat's own approval sheet waits while this sheet is up, so an
        // approval opened from a notification or banner is shown here instead.
        .onChange(of: model.selectedApprovalId, initial: true) { _, id in review(id) }
        #endif
    }

    #if os(macOS)
    private var backBar: some View {
        HStack {
            Button { if !path.isEmpty { path.removeLast() } } label: {
                Label("Threads", systemImage: "chevron.left")
            }
            .keyboardShortcut("[", modifiers: .command)
            .accessibilityLabel("Back to threads")
            Spacer()
            Button("Close", action: onClose).keyboardShortcut(.cancelAction)
        }
        .padding(.horizontal).padding(.vertical, 6)
        .background(.bar)
    }
    #endif

    #if os(iOS)
    private func review(_ id: String?) {
        guard let id, let approval = model.approvals(in: conversationId).first(where: { $0.id == id }) else { return }
        reviewing = approval
        model.selectedApprovalId = nil
    }
    #endif

    private func open(_ id: String?) {
        guard let id, model.threads.contains(where: { $0.id == id && $0.conversationId == conversationId }) else { return }
        if path.last != id { path = [id] }
        model.selectedThreadId = nil
    }
}

/// The three groups of the panel. A thread is listed once: one with a pending
/// approval or waiting for a device is in Needs you, not Running or Finished.
struct ThreadGroups {
    let approvals: [PendingApproval]
    let needsYou: [ThreadInfo]
    let running: [ThreadInfo]
    let finished: [ThreadInfo]
    let withApproval: Set<String>

    init(threads: [ThreadInfo], approvals: [PendingApproval]) {
        let withApproval = Set(approvals.compactMap(\.threadId))
        let needs = threads
            .filter { withApproval.contains($0.id) || $0.status == "waiting_device" }
            .sorted { lhs, rhs in
                let l = withApproval.contains(lhs.id), r = withApproval.contains(rhs.id)
                return l != r ? l : lhs.created > rhs.created
            }
        let needsIds = Set(needs.map(\.id))
        self.approvals = approvals.sorted { $0.seq > $1.seq }
        self.needsYou = needs
        self.running = threads.filter { $0.status == "running" && !needsIds.contains($0.id) }.sorted { $0.created > $1.created }
        self.finished = threads.filter { !$0.isActive && !needsIds.contains($0.id) }
            .sorted { ($0.finished ?? $0.created) > ($1.finished ?? $1.created) }
        self.withApproval = withApproval
    }

    var needsCount: Int { approvals.count + needsYou.filter { !withApproval.contains($0.id) }.count }
    var isEmpty: Bool { approvals.isEmpty && needsYou.isEmpty && running.isEmpty && finished.isEmpty }
}

private struct ThreadsList: View {
    @EnvironmentObject private var model: AppModel
    let conversationId: String
    @Binding var reviewing: PendingApproval?
    let onClose: () -> Void
    @State private var showAllFinished = false

    private static let finishedLimit = 20

    var body: some View {
        let groups = ThreadGroups(threads: model.threads(in: conversationId), approvals: model.approvals(in: conversationId))
        List {
            Section { header(groups) }
            if !groups.approvals.isEmpty || !groups.needsYou.isEmpty {
                Section("Needs you") {
                    ForEach(groups.approvals) { approval in approvalRow(approval) }
                    ForEach(groups.needsYou) { thread in threadLink(thread, needsApproval: groups.withApproval.contains(thread.id)) }
                }
            }
            if !groups.running.isEmpty {
                Section("Running") {
                    ForEach(groups.running) { thread in threadLink(thread, needsApproval: false) }
                }
            }
            if !groups.finished.isEmpty {
                Section("Finished") {
                    ForEach(visibleFinished(groups.finished)) { thread in threadLink(thread, needsApproval: false) }
                    if groups.finished.count > Self.finishedLimit {
                        Button(showAllFinished ? "Show recent only" : "Show all \(groups.finished.count)") { showAllFinished.toggle() }
                    }
                }
            }
        }
        .overlay {
            if groups.isEmpty { emptyState }
        }
        .refreshable { await model.refreshThreads(conversationId: conversationId) }
    }

    private func visibleFinished(_ finished: [ThreadInfo]) -> [ThreadInfo] {
        showAllFinished ? finished : Array(finished.prefix(Self.finishedLimit))
    }

    private func header(_ groups: ThreadGroups) -> some View {
        HStack(alignment: .firstTextBaseline) {
            VStack(alignment: .leading, spacing: 2) {
                Text("In \(chatTitle)").font(.subheadline.weight(.semibold)).lineLimit(2)
                Text(caption(groups)).font(.caption).foregroundStyle(.secondary)
            }
            #if os(macOS)
            Spacer()
            Button("Close", action: onClose).keyboardShortcut(.cancelAction)
            #endif
        }
    }

    private func caption(_ groups: ThreadGroups) -> String {
        var parts: [String] = []
        if groups.needsCount > 0 { parts.append(groups.needsCount == 1 ? "1 needs you" : "\(groups.needsCount) need you") }
        if !groups.running.isEmpty { parts.append("\(groups.running.count) running") }
        if !groups.finished.isEmpty { parts.append("\(groups.finished.count) finished") }
        return parts.isEmpty ? "Needs you, then running, then finished" : parts.joined(separator: " · ")
    }

    private var chatTitle: String { model.conversations.first { $0.id == conversationId }?.title ?? "this chat" }

    @ViewBuilder private var emptyState: some View {
        if model.conversations.first(where: { $0.id == conversationId })?.mode == "standard" {
            ContentUnavailableView("Standard chat", systemImage: "bubble.left",
                                   description: Text("Standard chats use one model and do not start threads."))
        } else {
            ContentUnavailableView("No threads yet", systemImage: "point.3.connected.trianglepath.dotted",
                                   description: Text("Threads appear here when this chat hands work to one of your devices or the hub."))
        }
    }

    private func threadLink(_ thread: ThreadInfo, needsApproval: Bool) -> some View {
        NavigationLink(value: thread.id) {
            ThreadRow(thread: thread, needsApproval: needsApproval)
        }
    }

    private func approvalRow(_ approval: PendingApproval) -> some View {
        Button { reviewing = approval } label: {
            HStack(alignment: .top, spacing: 10) {
                Image(systemName: "exclamationmark.shield.fill").foregroundStyle(.orange)
                VStack(alignment: .leading, spacing: 3) {
                    Text("\(approval.tool) on \(model.deviceDisplayName(approval.deviceId))").font(.headline).foregroundStyle(.primary)
                    if !approval.reason.isEmpty {
                        Text(approval.reason).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                    }
                    HStack(spacing: 4) {
                        Text(threadTitle(approval.threadId))
                        Text("·")
                        RelativeAgeText(seconds: approval.ts)
                    }
                    .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }
                Spacer(minLength: 8)
                Text("Review").font(.callout.weight(.semibold)).foregroundStyle(.tint)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Approval needed for \(approval.tool) on \(model.deviceDisplayName(approval.deviceId))")
        .accessibilityHint("Opens the approval")
    }

    private func threadTitle(_ id: String?) -> String {
        guard let id else { return "Thread" }
        let title = model.threads.first { $0.id == id }?.title ?? ""
        return title.isEmpty ? "Thread" : title
    }
}

/// One thread: title, model name with its family accent and glyph, device, elapsed
/// time (live while running or waiting, total once finished) and status.
struct ThreadRow: View {
    @EnvironmentObject private var model: AppModel
    let thread: ThreadInfo
    let needsApproval: Bool

    var body: some View {
        let family = model.family(ofModel: thread.model)
        HStack(alignment: .top, spacing: 8) {
            VStack(alignment: .leading, spacing: 4) {
                Text(thread.title.isEmpty ? "Thread" : thread.title).font(.headline).lineLimit(2)
                HStack(spacing: 6) {
                    FamilyMark(family: family)
                    Text(thread.model.isEmpty ? "model not recorded" : thread.model)
                        .font(.caption.monospaced()).lineLimit(1)
                }
                HStack(spacing: 10) {
                    Label(model.deviceDisplayName(thread.deviceId), systemImage: "desktopcomputer").lineLimit(1)
                    ElapsedText(thread: thread)
                }
                .font(.caption).foregroundStyle(.secondary)
                if !thread.isActive, let summary = thread.summary, !summary.isEmpty {
                    Text(Self.clip(summary)).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                }
            }
            Spacer(minLength: 4)
            if needsApproval {
                Label("Needs approval", systemImage: "exclamationmark.shield.fill")
                    .font(.caption.weight(.semibold)).foregroundStyle(.orange)
            } else {
                StatusLabel(status: thread.status)
            }
        }
        .padding(.leading, 10)
        .background(alignment: .leading) {
            RoundedRectangle(cornerRadius: 2).fill(family.color).frame(width: 4)
        }
        .accessibilityElement(children: .combine)
    }

    /// One plain line of a Markdown summary, like the web panel's preview.
    static func clip(_ text: String) -> String {
        let plain = text.filter { !"*_`#>".contains($0) }
            .split(whereSeparator: \.isWhitespace).joined(separator: " ")
        return plain.count > 180 ? String(plain.prefix(180)) + "…" : plain
    }
}
