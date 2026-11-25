import Foundation
import GrouperKit

@MainActor
final class MacNodeController: ObservableObject {
    @Published private(set) var statusText = "Paused"
    @Published private(set) var running = false
    private var client: GrouperClient?
    private let bridge = MacNodeBridge()

    func start(hubURL: String, token: String) {
        guard !token.isEmpty, client == nil else { return }
        do {
            let workspace = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Grouper/workspaces").path
            bridge.owner = self
            let created = try GrouperClient(
                hubUrl: hubURL,
                token: token,
                capabilities: [Capability("mac_info", "Mac hostname, OS, uptime, processors and memory", schema: #"{"type":"object","properties":{}}"#)],
                workspaceRoot: workspace,
                allowShell: true,
                handler: bridge,
                listener: bridge
            )
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
        running = false
        statusText = "Paused"
    }

    fileprivate func received(status: ConnectionStatus) {
        statusText = String(describing: status)
        if case .unauthorized = status { running = false }
    }
}

private final class MacNodeBridge: ToolHandler, ClientListener, @unchecked Sendable {
    weak var owner: MacNodeController?

    func callTool(tool: String, argsJson: String) throws -> String {
        guard tool == "mac_info" else { throw ToolError.failed("unknown tool \(tool)") }
        let process = ProcessInfo.processInfo
        return try toolJSON([
            "hostname": Host.current().localizedName ?? process.hostName,
            "os": process.operatingSystemVersionString,
            "uptime_seconds": Int(process.systemUptime),
            "processors": process.activeProcessorCount,
            "memory_gb": Double(process.physicalMemory) / 1_073_741_824,
            "low_power_mode": process.isLowPowerModeEnabled,
            "implemented_in": "swift",
        ])
    }

    func onStatus(status: ConnectionStatus) { Task { @MainActor [weak owner] in owner?.received(status: status) } }
    func onPresence(devices: [GrouperKit.Device]) {}
    func onThread(threadId: String, title: String, state: String) {}
}
