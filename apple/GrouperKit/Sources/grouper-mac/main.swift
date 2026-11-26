// grouper-mac: a minimal macOS node proving the Swift ⇄ Rust seam.
//
// Swift → Rust: creates the Rust GrouperClient, which connects to the hub.
// Rust → Swift: the hub's tool calls arrive in Rust and are answered by MacTools below.
//
//   GROUPER_HUB=http://127.0.0.1:8080 GROUPER_TOKEN=<token> swift run grouper-mac
//
// It also runs subagent threads the orchestrator starts on this Mac (in the Rust core),
// and those threads can use `mac_info`. The core serves these tools to the hub over MCP,
// with the input schemas given below.
import Foundation
import GrouperKit

final class MacTools: ToolHandler, @unchecked Sendable {
    func callTool(tool: String, argsJson: String) throws -> String {
        switch tool {
        case "echo":
            return argsJson
        case "mac_info":
            let p = ProcessInfo.processInfo
            return try toolJSON([
                "hostname": Host.current().localizedName ?? p.hostName,
                "os": p.operatingSystemVersionString,
                "uptime_seconds": Int(p.systemUptime),
                "processors": p.activeProcessorCount,
                "memory_gb": Double(p.physicalMemory) / 1_073_741_824,
                "low_power_mode": p.isLowPowerModeEnabled,
                "implemented_in": "swift",
            ])
        default:
            throw ToolError.failed("unknown tool \(tool)")
        }
    }
}

final class Printer: ClientListener, @unchecked Sendable {
    func onStatus(status: ConnectionStatus) {
        print("status: \(status)")
        if case .unauthorized = status {
            print("The hub rejected this token.")
            exit(1)
        }
    }

    func onPresence(devices: [Device]) {
        print("devices:")
        for d in devices {
            let tools = d.capabilities.map(\.name).joined(separator: ", ")
            print("  \(d.id) \(d.name) (\(d.platform)) \(d.online ? "online" : "offline") [\(tools)]")
        }
    }

    func onThread(threadId: String, title: String, state: String) {
        print("thread \(threadId) \"\(title)\": \(state)")
    }
}

let env = ProcessInfo.processInfo.environment
let hub = env["GROUPER_HUB"] ?? "http://127.0.0.1:8080"
guard let token = env["GROUPER_TOKEN"] else {
    print("Set GROUPER_TOKEN to this device's token (grouper-hub device add).")
    exit(2)
}

let workspace = env["GROUPER_WORKSPACE"]
    ?? FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Grouper/workspaces").path
print("Rust core \(coreVersion()), thread workspaces in \(workspace)")
let client: GrouperClient
do {
    client = try GrouperClient(
        hubUrl: hub,
        token: token,
        capabilities: [
            Capability("echo", "Returns its arguments",
                       schema: #"{"type":"object","additionalProperties":true}"#),
            Capability("mac_info", "Mac hostname, OS version, uptime and hardware (Swift)",
                       schema: #"{"type":"object","properties":{}}"#),
        ],
        workspaceRoot: workspace,
        allowShell: true,
        handler: MacTools(),
        listener: Printer()
    )
} catch {
    print("Can't start: \(error)")
    exit(2)
}
client.start()

signal(SIGINT, SIG_IGN)
let sigint = DispatchSource.makeSignalSource(signal: SIGINT, queue: .main)
sigint.setEventHandler {
    client.stop()
    exit(0)
}
sigint.resume()
dispatchMain()
