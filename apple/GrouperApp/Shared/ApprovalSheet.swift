import SwiftUI
import GrouperAPI

struct ApprovalSheet: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    let approval: PendingApproval
    @State private var busy = false
    @State private var answeredElsewhere = false

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(approval.tool).font(.title2.bold())
                        Text("\(deviceName) · \(threadTitle)").foregroundStyle(.secondary)
                    }
                    VStack(alignment: .leading, spacing: 8) {
                        Label("Why Grouper asked", systemImage: "exclamationmark.shield.fill").font(.headline)
                        Text(approval.reason.isEmpty ? "This tool is set to Manual, so every call waits for you." : approval.reason)
                            .font(.body.weight(.medium)).fixedSize(horizontal: false, vertical: true)
                    }
                    .padding().frame(maxWidth: .infinity, alignment: .leading)
                    .background(.orange.opacity(0.12), in: .rect(cornerRadius: 14))

                    VStack(alignment: .leading, spacing: 8) {
                        Text("What will run").font(.headline)
                        Text(approval.input.prettyPrinted).font(.system(.body, design: .monospaced)).textSelection(.enabled)
                            .padding().frame(maxWidth: .infinity, alignment: .leading)
                            .background(Color.grouperSecondaryBackground, in: .rect(cornerRadius: 12))
                    }
                    Text("The call waits until someone answers or the thread is stopped.").font(.callout).foregroundStyle(.secondary)
                    if answeredElsewhere { Label("Answered on another device.", systemImage: "checkmark.circle").foregroundStyle(.secondary) }
                    Button("Stop the thread instead", role: .destructive) {
                        Task { if let id = approval.threadId { await model.stopThread(id) }; dismiss() }
                    }.disabled(!model.online || busy)
                }.padding()
            }
            .safeAreaInset(edge: .bottom) { actions }
            .navigationTitle("Run this tool?")
            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Close") { dismiss() } } }
            .onChange(of: model.pendingApprovals) { _, approvals in
                if !approvals.contains(where: { $0.id == approval.id }) { answeredElsewhere = true }
            }
        }
    }

    private var actions: some View {
        VStack(spacing: 10) {
            HStack(spacing: 12) {
                Button { answer(false) } label: { Label("Deny", systemImage: "xmark") }.buttonStyle(.glass).grouperMinimumControlSize()
                    .accessibilityLabel("Deny \(approval.tool) on \(deviceName)")
                Button { answer(true) } label: { Label("Approve", systemImage: "checkmark") }.buttonStyle(.glassProminent).grouperMinimumControlSize()
                    .accessibilityLabel("Approve \(approval.tool) on \(deviceName)")
            }
            Button("Change it: deny and tell the thread what to do instead") { changeIt() }.font(.callout)
        }
        .disabled(!model.online || busy || answeredElsewhere)
        .padding().frame(maxWidth: .infinity)
        .background(.bar)
    }

    private var deviceName: String { model.devices.first { $0.id == approval.deviceId }?.name ?? approval.deviceId }
    private var threadTitle: String { model.threads.first { $0.id == approval.threadId }?.title ?? "Thread" }

    private func answer(_ approve: Bool) {
        busy = true
        Task { await model.resolveApproval(approval.id, approve: approve); busy = false; dismiss() }
    }

    private func changeIt() {
        guard let threadId = approval.threadId else { answer(false); return }
        busy = true
        Task {
            await model.resolveApproval(approval.id, approve: false)
            model.threadMessageDrafts[threadId] = "Don’t run that. Instead: "
            model.selectedThreadId = threadId
            busy = false
            dismiss()
        }
    }
}
