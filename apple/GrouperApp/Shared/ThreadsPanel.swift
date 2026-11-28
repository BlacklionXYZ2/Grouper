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

    var body: some View {
        NavigationStack(path: $path) {
            ThreadsList(conversationId: conversationId, onClose: onClose)
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
        // A thread chosen elsewhere (a chat card on Mac, "Change it" in the approval
        // dock) opens here when the panel is showing.
        .onChange(of: model.selectedThreadId, initial: true) { _, id in open(id) }
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
        .background(Theme.bgTint)
        .overlay(alignment: .bottom) { Rectangle().fill(Theme.border).frame(height: 1) }
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
        self.approvals = ApprovalQueue.ordered(approvals) // oldest first, like the dock
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
    let onClose: () -> Void
    @State private var showAllFinished = false

    private static let finishedLimit = 20

    var body: some View {
        let groups = ThreadGroups(threads: model.threads(in: conversationId), approvals: model.approvals(in: conversationId))
        List {
            Section { header(groups) }
                .listRowBackground(Theme.bgTint)
            if !groups.approvals.isEmpty || !groups.needsYou.isEmpty {
                Section {
                    ForEach(groups.approvals) { approval in approvalRow(approval) }
                    ForEach(groups.needsYou) { thread in threadLink(thread, needsApproval: groups.withApproval.contains(thread.id)) }
                } header: {
                    sectionLabel("Needs you", tint: Theme.brand)
                }
                .listRowBackground(Theme.surface)
            }
            if !groups.running.isEmpty {
                Section {
                    ForEach(groups.running) { thread in threadLink(thread, needsApproval: false) }
                } header: {
                    sectionLabel("Running", tint: Theme.text)
                }
                .listRowBackground(Theme.surface)
            }
            if !groups.finished.isEmpty {
                Section {
                    ForEach(visibleFinished(groups.finished)) { thread in threadLink(thread, needsApproval: false) }
                    if groups.finished.count > Self.finishedLimit {
                        Button(showAllFinished ? "Show recent only" : "Show all \(groups.finished.count)") { showAllFinished.toggle() }
                            .foregroundStyle(Theme.accent)
                    }
                } header: {
                    sectionLabel("Finished", tint: Theme.text)
                }
                .listRowBackground(Theme.surface)
            }
        }
        .scrollContentBackground(.hidden)
        .background(Theme.bgTint)
        .overlay {
            if groups.isEmpty { emptyState }
        }
        .refreshable { await model.refreshThreads(conversationId: conversationId) }
    }

    private func sectionLabel(_ title: String, tint: Color) -> some View {
        Text(title).font(Theme.Typeface.sectionLabel).foregroundStyle(tint).textCase(nil)
    }

    private func visibleFinished(_ finished: [ThreadInfo]) -> [ThreadInfo] {
        showAllFinished ? finished : Array(finished.prefix(Self.finishedLimit))
    }

    private func header(_ groups: ThreadGroups) -> some View {
        HStack(alignment: .firstTextBaseline) {
            VStack(alignment: .leading, spacing: 2) {
                Text("In \(chatTitle)").font(Theme.Typeface.title).foregroundStyle(Theme.text).lineLimit(2)
                Text(caption(groups)).font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
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

    /// Review never answers here: it shows the request in the chat's approval dock.
    private func approvalRow(_ approval: PendingApproval) -> some View {
        Button {
            model.selectedApprovalId = approval.id
            model.dockFocusRequest = approval.id
            #if os(iOS)
            onClose() // the dock is in the chat, under this sheet
            #endif
        } label: {
            HStack(alignment: .top, spacing: 10) {
                Image(systemName: "exclamationmark.shield.fill").foregroundStyle(Theme.brand)
                VStack(alignment: .leading, spacing: 3) {
                    Text("\(approval.tool) on \(model.deviceDisplayName(approval.deviceId))").font(Theme.Typeface.row.weight(.semibold)).foregroundStyle(Theme.text)
                    if !approval.reason.isEmpty {
                        Text(approval.reason).font(.caption).foregroundStyle(Theme.text2).lineLimit(2)
                    }
                    HStack(spacing: 4) {
                        Text(threadTitle(approval.threadId))
                        Text("·")
                        RelativeAgeText(seconds: approval.ts)
                    }
                    .font(Theme.Typeface.meta).foregroundStyle(Theme.text3).lineLimit(1)
                }
                Spacer(minLength: 8)
                Text("Review").font(.callout.weight(.semibold)).foregroundStyle(Theme.accent)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Approval needed for \(approval.tool) on \(model.deviceDisplayName(approval.deviceId))")
        .accessibilityHint("Shows the request in the chat's approval dock")
    }

    private func threadTitle(_ id: String?) -> String {
        guard let id else { return "Thread" }
        let title = model.threads.first { $0.id == id }?.title ?? ""
        return title.isEmpty ? "Thread" : title
    }
}

/// One thread, like a row of the chat's threads card: family dot, title, model tag,
/// status pill; underneath, the device and elapsed time (live while running or
/// waiting, total once finished) and, once finished, the summary's first line.
struct ThreadRow: View {
    @EnvironmentObject private var model: AppModel
    let thread: ThreadInfo
    let needsApproval: Bool

    var body: some View {
        ThreadLine(title: thread.title, modelId: thread.model, family: model.family(ofModel: thread.model),
                   status: thread.status, needsApproval: needsApproval, showsChevron: false) {
            HStack(spacing: 10) {
                Label(model.deviceDisplayName(thread.deviceId), systemImage: "desktopcomputer").lineLimit(1)
                ElapsedText(thread: thread)
            }
            .font(Theme.Typeface.meta).foregroundStyle(Theme.text3)
            if !thread.isActive, let summary = thread.summary, !summary.isEmpty {
                Text(Self.clip(summary)).font(Theme.Typeface.meta).foregroundStyle(Theme.text2).lineLimit(2)
            }
        }
        .padding(.vertical, 2)
        .accessibilityElement(children: .combine)
    }

    /// One plain line of a Markdown summary, like the web panel's preview.
    static func clip(_ text: String) -> String {
        let plain = text.filter { !"*_`#>".contains($0) }
            .split(whereSeparator: \.isWhitespace).joined(separator: " ")
        return plain.count > 180 ? String(plain.prefix(180)) + "…" : plain
    }
}