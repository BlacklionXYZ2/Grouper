import SwiftUI
import GrouperAPI

struct ThreadsView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        NavigationStack {
            List {
                if !model.pendingApprovals.isEmpty {
                    Section("Needs you") {
                        ForEach(model.pendingApprovals) { approval in
                            Button { model.selectedApprovalId = approval.id } label: {
                                VStack(alignment: .leading, spacing: 4) {
                                    Label(approval.tool, systemImage: "exclamationmark.shield.fill").foregroundStyle(.orange)
                                    Text(approval.reason).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                                }
                            }.accessibilityLabel("Approval needed for \(approval.tool) on \(approval.deviceId)")
                        }
                    }
                }
                Section("Running") { threadRows(activeThreads) }
                Section("Finished") { threadRows(finishedThreads) }
            }
            .navigationTitle("Threads")
            .refreshable { await model.loadInitial() }
            .sheet(item: Binding(get: { model.selectedThread }, set: { if $0 == nil { model.selectedThreadId = nil } })) { ThreadDetailView(thread: $0) }
            .sheet(item: Binding(get: { model.selectedApproval }, set: { if $0 == nil { model.selectedApprovalId = nil } })) { ApprovalSheet(approval: $0) }
        }
    }

    @ViewBuilder private func threadRows(_ threads: [ThreadInfo]) -> some View {
        if threads.isEmpty { Text("None").foregroundStyle(.secondary) }
        ForEach(threads) { thread in
            Button { model.selectedThreadId = thread.id } label: {
                HStack {
                    VStack(alignment: .leading) {
                        Text(thread.title).foregroundStyle(.primary)
                        Text(conversationTitle(thread.conversationId)).font(.caption).foregroundStyle(.secondary)
                    }
                    Spacer(); StatusLabel(status: thread.status)
                }
            }
        }
    }

    private var activeThreads: [ThreadInfo] { model.threads.filter { !$0.isTerminal }.sorted { $0.created > $1.created } }
    private var finishedThreads: [ThreadInfo] { model.threads.filter(\.isTerminal).sorted { ($0.finished ?? 0) > ($1.finished ?? 0) } }
    private func conversationTitle(_ id: String) -> String { model.conversations.first { $0.id == id }?.title ?? "Conversation" }
}
