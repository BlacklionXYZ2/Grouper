import Foundation
import GrouperKit
import SystemConfiguration

@MainActor
final class MacNodeController: ObservableObject {
    @Published private(set) var statusText = "Paused"
    @Published private(set) var running = false
    private var client: GrouperClient?
    private var bridge: MacNodeBridge?

    func start(hubURL: String, token: String) {
        guard !token.isEmpty, client == nil else { return }
        do {
            let workspace = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Grouper/workspaces").path
            let createdBridge = MacNodeBridge { [weak self] status in
                Task { @MainActor [weak self] in
                    self?.received(status: status)
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
        client?.stop()
        client = nil
        bridge = nil
        running = false
        statusText = "Paused"
    }

    private func received(status: ConnectionStatus) {
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
