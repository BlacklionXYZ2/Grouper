import SwiftUI
import GrouperAPI

struct ThreadDetailView: View {
    @EnvironmentObject private var model: AppModel
    let thread: ThreadInfo
    @Environment(\.dismiss) private var dismiss
    @State private var message = ""
    @State private var stopping = false
    @State private var showStopConfirmation = false

    private var liveThread: ThreadInfo {
        model.threads.first { $0.id == thread.id } ?? thread
    }

    var body: some View {
        NavigationStack {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 16) {
                    header
                    if liveThread.status == "waiting_device" { waitingSection }
                    ForEach(model.activity(threadId: liveThread.id)) { row in activity(row) }
                    if let summary = liveThread.summary, !summary.isEmpty {
                        GroupBox("Summary") { Text(LocalizedStringKey(summary)).frame(maxWidth: .infinity, alignment: .leading).textSelection(.enabled) }
                    }
                    if liveThread.isTerminal { rating }
                }.padding()
            }
            .safeAreaInset(edge: .bottom) { controls }
            .navigationTitle(liveThread.title)
            .toolbar {
                #if os(iOS)
                ToolbarItem(placement: .cancellationAction) { Button("Done") { dismiss() } }
                #else
                ToolbarItem(placement: .automatic) { Button("Close") { model.selectedThreadId = nil } }
                #endif
            }
            .confirmationDialog("Stop \(liveThread.title)?", isPresented: $showStopConfirmation) {
                Button("Stop thread", role: .destructive) { stop() }
            } message: { Text("Its current tool or model turn will be stopped.") }
            .task(id: thread.id) { message = model.threadMessageDrafts[thread.id] ?? "" }
            .onChange(of: message) { _, value in model.threadMessageDrafts[thread.id] = value }
        }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack { StatusLabel(status: liveThread.status); Text("· \(deviceName) · \(liveThread.model)").foregroundStyle(.secondary) }
            if let reason = liveThread.reason { Text(reason).font(.callout).foregroundStyle(.secondary) }
        }.frame(maxWidth: .infinity, alignment: .leading)
    }

    private var waitingSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Waiting for \(deviceName)", systemImage: "clock.fill").font(.headline).foregroundStyle(.orange)
            if let device = model.devices.first(where: { $0.id == liveThread.deviceId }), let last = device.lastSeen {
                Text("Last seen \(Date(timeIntervalSince1970: TimeInterval(last)), style: .relative) ago").font(.caption).foregroundStyle(.secondary)
            }
            if let checkpoint = latestCheckpoint { Text("Checkpoint").font(.subheadline.bold()); Text(checkpoint) }
            else { Text("The hub will work to a checkpoint while this device is offline.").foregroundStyle(.secondary) }
        }.padding().background(Color.orange.opacity(0.09), in: .rect(cornerRadius: 14))
    }

    @ViewBuilder private func activity(_ row: ThreadActivityRow) -> some View {
        switch row {
        case .started(_, _, let value):
            GroupBox("Brief") { Text(LocalizedStringKey(value.brief)).frame(maxWidth: .infinity, alignment: .leading).textSelection(.enabled) }
        case .text(_, _, let text): Text(LocalizedStringKey(text)).frame(maxWidth: .infinity, alignment: .leading).textSelection(.enabled)
        case .tool(_, _, let call, let result): ToolActivityView(call: call, result: result)
        case .input(_, _, let from, let text): Label("\(from): \(text)", systemImage: "person.crop.circle.badge.checkmark").font(.callout)
        case .notice(_, _, let text): Label(text, systemImage: "info.circle").font(.callout).foregroundStyle(.secondary)
        case .waiting(_, _, let value): Label(value.reason, systemImage: "clock").foregroundStyle(.orange)
        case .checkpoint(_, _, let value): Label(value.summary, systemImage: "flag.checkered").foregroundStyle(.teal)
        case .handedOver(_, _, let value):
            VStack(alignment: .leading) {
                Label("Continued on \(value.deviceId)", systemImage: "arrow.triangle.2.circlepath")
                if !value.files.isEmpty { Text("Copied: \(value.files.joined(separator: ", "))").font(.caption) }
                if !value.skipped.isEmpty { Text("Skipped: \(value.skipped.joined(separator: ", "))").font(.caption).foregroundStyle(.secondary) }
            }
        case .approval(_, _, let request, let approved):
            Label(approved.map { $0 ? "Approved \(request.tool)" : "Denied \(request.tool)" } ?? "Waiting to run \(request.tool)", systemImage: "exclamationmark.shield")
                .foregroundStyle(approved == nil ? Color.orange : Color.secondary)
        case .error(_, _, let message): ErrorBanner(message: message)
        case .finished(_, _, let status, let summary): Label(summary.isEmpty ? status.capitalized : summary, systemImage: status == "done" ? "checkmark.circle" : "xmark.circle")
        }
    }

    private var controls: some View {
        VStack(spacing: 8) {
            if !liveThread.isTerminal {
                GlassSurface {
                    HStack(alignment: .bottom) {
                        TextField("Message this thread", text: $message, axis: .vertical).lineLimit(1...4).textFieldStyle(.plain).grouperMinimumControlSize()
                        Button { stepIn() } label: { Image(systemName: "arrow.up") }
                            .buttonStyle(.glassProminent).grouperMinimumControlSize().accessibilityLabel("Send to this thread")
                            .disabled(message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !model.online)
                    }.padding(10)
                }
                Button(role: .destructive) {
                    if liveThread.status == "running" { showStopConfirmation = true } else { stop() }
                } label: {
                    if stopping { ProgressView() } else { Label("Stop thread", systemImage: "stop.circle") }
                }.disabled(!model.online || stopping).grouperMinimumControlSize()
            }
        }.padding()
    }

    private var rating: some View {
        GlassEffectContainer(spacing: 8) {
            HStack {
                Text("Rate this result").font(.headline); Spacer()
                Button { Task { await model.rateThread(liveThread.id, rating: "up") } } label: { Image(systemName: "hand.thumbsup") }.accessibilityLabel("Rate up")
                Button { Task { await model.rateThread(liveThread.id, rating: "down") } } label: { Image(systemName: "hand.thumbsdown") }.accessibilityLabel("Rate down")
            }.buttonStyle(.glass)
        }
    }

    private var latestCheckpoint: String? {
        model.activity(threadId: liveThread.id).reversed().compactMap { row in
            if case .checkpoint(_, _, let value) = row { return value.summary }
            return nil
        }.first
    }

    private var deviceName: String { model.devices.first { $0.id == liveThread.deviceId }?.name ?? liveThread.deviceId }

    private func stepIn() {
        let text = message.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        Task { if await model.messageThread(liveThread.id, text: text) { message = "" } }
    }

    private func stop() {
        stopping = true
        Task { await model.stopThread(liveThread.id); stopping = false }
    }
}

private struct ToolActivityView: View {
    let call: ToolCall
    let result: ToolResult?
    @State private var expanded = false
    var body: some View {
        DisclosureGroup(isExpanded: $expanded) {
            VStack(alignment: .leading, spacing: 8) {
                Text(call.input.prettyPrinted).font(.system(.body, design: .monospaced)).textSelection(.enabled)
                if let result { Text(result.output).font(.system(.body, design: .monospaced)).textSelection(.enabled) }
            }.padding(.top, 8)
        } label: {
            HStack { Label(call.name, systemImage: "wrench.and.screwdriver"); Spacer(); Text(result.map { $0.ok ? "OK" : "Failed" } ?? "Running").foregroundStyle(result?.ok == false ? Color.red : Color.secondary) }
        }.padding().background(Color.grouperSecondaryBackground, in: .rect(cornerRadius: 12))
    }
}
