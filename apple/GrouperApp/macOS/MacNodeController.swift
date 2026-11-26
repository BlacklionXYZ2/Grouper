import Foundation
import GrouperKit
import SystemConfiguration

@MainActor
final class MacNodeController: ObservableObject {
    @Published private(set) var statusText = "Paused"
    @Published private(set) var running = false
    /// The hub refused the node's token (401). Cleared when the node starts again.
    @Published private(set) var tokenRejected = false
    /// The hub and device token last applied; Pause and Resume keep it.
    private(set) var target: NodeTarget?
    private var client: GrouperClient?
    private var bridge: MacNodeBridge?
    /// Bumped on every start and stop, so status from an earlier client can't change this one's.
    private var generation = 0

    /// This Mac's name as System Settings > General > Sharing shows it, if available.
    nonisolated static func computerName() -> String? {
        SCDynamicStoreCopyComputerName(nil, nil) as String?
    }

    /// Follows the app's node target: restarts the node when the hub or token changes and stops
    /// it when there is none. Applying the same target again does nothing (a paused node stays paused).
    func apply(_ newTarget: NodeTarget?) {
        guard newTarget != target else { return }
        target = newTarget
        stop()
        if let newTarget { start(hubURL: newTarget.hubURL, token: newTarget.token) }
    }

    /// Menu bar > Resume this Mac.
    func resume() {
        guard let target, !running else { return }
        stop()
        start(hubURL: target.hubURL, token: target.token)
    }

    private func start(hubURL: String, token: String) {
        guard !token.isEmpty, client == nil else { return }
        tokenRejected = false
        generation += 1
        let current = self.generation
        do {
            let workspace = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Grouper/workspaces").path
            let createdBridge = MacNodeBridge { [weak self] status in
                Task { @MainActor [weak self] in
                    self?.received(status: status, generation: current)
                }
            }
            let created = try GrouperClient(
                hubUrl: hubURL,
                token: token,
                capabilities: [Capability("mac_info", "Mac hostname, OS, uptime, processors and memory", schema: #"{"type":"object","properties":{}}"#)],
                workspaceRoot: workspace,
                allowShell: true,
                handler: createdBridge,
                listener: createdBridge
            )
            bridge = createdBridge
            client = created
            created.start()
            running = true
            statusText = "Connecting"
        } catch {
            statusText = error.localizedDescription
        }
    }

    func stop() {
        generation += 1 // late status from the stopped client is ignored
        client?.stop()
        client = nil
        bridge = nil
        running = false
        statusText = "Paused"
    }

    private func received(status: ConnectionStatus, generation: Int) {
        guard generation == self.generation else { return }
        switch status {
        case .connecting:
            statusText = "Connecting"
        case .connected:
            statusText = "This Mac online"
        case .disconnected(let reason):
            statusText = reason.isEmpty ? "Disconnected" : "Disconnected: \(reason)"
        case .unauthorized:
            statusText = "Token rejected"
            running = false
            tokenRejected = true
        }
    }
}

private final class MacNodeBridge: ToolHandler, ClientListener, Sendable {
    private let onStatusUpdate: @Sendable (ConnectionStatus) -> Void

    init(onStatusUpdate: @escaping @Sendable (ConnectionStatus) -> Void) {
        self.onStatusUpdate = onStatusUpdate
    }

    func callTool(tool: String, argsJson: String) throws -> String {
        guard tool == "mac_info" else { throw ToolError.failed("unknown tool \(tool)") }
        let process = ProcessInfo.processInfo
        let hostname = (SCDynamicStoreCopyComputerName(nil, nil) as String?) ?? process.hostName
        return try toolJSON([
            "hostname": hostname,
            "os": process.operatingSystemVersionString,
            "uptime_seconds": Int(process.systemUptime),
            "processors": process.activeProcessorCount,
            "memory_gb": Double(process.physicalMemory) / 1_073_741_824,
            "low_power_mode": process.isLowPowerModeEnabled,
            "implemented_in": "swift",
        ])
    }

    func onStatus(status: ConnectionStatus) { onStatusUpdate(status) }
    func onPresence(devices: [GrouperKit.Device]) {}
    func onThread(threadId: String, title: String, state: String) {}
}
