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

    /// A subscription whose usage this Mac can read (its sign-in lives on this Mac).
    struct UsageProvider: Identifiable {
        let id: String
        let name: String
    }

    nonisolated static let usageProviders = [
        UsageProvider(id: "claude", name: "Claude"),
        UsageProvider(id: "codex", name: "ChatGPT (Codex)"),
        UsageProvider(id: "grok", name: "Grok"),
        UsageProvider(id: "antigravity", name: "Antigravity"),
    ]
    nonisolated private static let readUsageKey = "grouper.usageReading"
    nonisolated private static let usageOffKey = "grouper.usageReadingOff"

    /// "Read subscription usage on this device". On by default: usage belongs to the whole
    /// account, so this is the device owner's local off switch (nothing can flip it remotely).
    @Published private(set) var readUsage: Bool = UserDefaults.standard.object(forKey: MacNodeController.readUsageKey) as? Bool ?? true
    /// Providers switched off one by one.
    @Published private(set) var usageProvidersOff: Set<String> = Set(UserDefaults.standard.stringArray(forKey: MacNodeController.usageOffKey) ?? [])

    /// What the node is told: `nil` means every provider.
    private var usageProbeProviders: [String]? {
        usageProvidersOff.isEmpty ? nil : Self.usageProviders.map(\.id).filter { !usageProvidersOff.contains($0) }
    }

    /// Reading is on, and at least one provider is left on.
    private var usageProbesOn: Bool {
        readUsage && usageProbeProviders?.isEmpty != true
    }

    func setReadUsage(_ on: Bool) {
        guard on != readUsage else { return }
        readUsage = on
        UserDefaults.standard.set(on, forKey: Self.readUsageKey)
        restartForUsageChange()
    }

    func setReadUsage(provider: String, _ on: Bool) {
        var off = usageProvidersOff
        if on { off.remove(provider) } else { off.insert(provider) }
        guard off != usageProvidersOff else { return }
        usageProvidersOff = off
        UserDefaults.standard.set(off.sorted(), forKey: Self.usageOffKey)
        restartForUsageChange()
    }

    /// Reconnects a running node so the hub sees the new advertisement. A paused node stays
    /// paused and picks the setting up when it resumes.
    private func restartForUsageChange() {
        guard running, let target else { return }
        stop()
        start(hubURL: target.hubURL, token: target.token, relayToken: target.relayToken)
    }

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
        if let newTarget { start(hubURL: newTarget.hubURL, token: newTarget.token, relayToken: newTarget.relayToken) }
    }

    /// Menu bar > Resume this Mac.
    func resume() {
        guard let target, !running else { return }
        stop()
        start(hubURL: target.hubURL, token: target.token, relayToken: target.relayToken)
    }

    /// `relayToken`: the relay sign-in token when `hubURL` is a relay; the Rust core sends it as
    /// `Grouper-Relay` on its WebSocket handshake.
    private func start(hubURL: String, token: String, relayToken: String?) {
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
                usageProbes: usageProbesOn,
                usageProbeProviders: usageProbeProviders,
                relayToken: relayToken,
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
