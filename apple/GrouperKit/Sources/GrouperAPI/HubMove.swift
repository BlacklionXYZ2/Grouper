import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

// Hub identity, install-level device listing, and hub move API (docs/local-first-design.md §4a).
// Matches:
// - GET /v1/hub (crates/hub/src/server.rs::hub_info, core_protocol::HubInfo)
// - GET /v1/devices/installs (crates/hub/src/devices.rs::InstallDevicesResponse)
// - GET/POST/DELETE /v1/hub/move (crates/hub/src/migrate.rs::{MoveStatus, StartMoveRequest})

/// `GET /v1/hub`: public identity of the hub currently answering at the configured address.
/// Older hubs omit `account`, `hub_device_id`, `epoch`, and `key_sha256`; they default cleanly.
public struct HubIdentity: Codable, Equatable, Sendable {
    public let hubId: String
    public let hubVersion: String
    public let protocolVersion: UInt32
    /// Google owner's email, or nil when unclaimed.
    public let account: String?
    /// Device row currently acting as the hub (empty on pre-local-first hubs).
    public let hubDeviceId: String
    /// Monotonic counter incremented on every hub reassignment (0 on pre-local-first hubs).
    public let epoch: UInt64
    /// Hex SHA-256 of the hub's identity key, when present.
    public let keySha256: String?

    public init(
        hubId: String,
        hubVersion: String,
        protocolVersion: UInt32,
        account: String? = nil,
        hubDeviceId: String = "",
        epoch: UInt64 = 0,
        keySha256: String? = nil
    ) {
        self.hubId = hubId
        self.hubVersion = hubVersion
        self.protocolVersion = protocolVersion
        self.account = account
        self.hubDeviceId = hubDeviceId
        self.epoch = epoch
        self.keySha256 = keySha256
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        hubId = try c.decode(String.self, forKey: .hubId)
        hubVersion = try c.decode(String.self, forKey: .hubVersion)
        protocolVersion = try c.decode(UInt32.self, forKey: .protocolVersion)
        account = try c.decodeIfPresent(String.self, forKey: .account)
        hubDeviceId = try c.decodeIfPresent(String.self, forKey: .hubDeviceId) ?? ""
        epoch = try c.decodeIfPresent(UInt64.self, forKey: .epoch) ?? 0
        keySha256 = try c.decodeIfPresent(String.self, forKey: .keySha256)
    }
}

/// Last-seen hub identity cached by a client for one hub address, so it can detect
/// a hub move (higher epoch), a stale hub (lower epoch), or a replacement (different `hub_id`).
public struct CachedHubIdentity: Codable, Equatable, Sendable {
    public let hubId: String
    public let account: String?
    public let hubDeviceId: String
    public let epoch: UInt64
    /// Unix seconds when this identity was recorded.
    public let recordedAt: UInt64

    public init(
        hubId: String,
        account: String? = nil,
        hubDeviceId: String,
        epoch: UInt64,
        recordedAt: UInt64
    ) {
        self.hubId = hubId
        self.account = account
        self.hubDeviceId = hubDeviceId
        self.epoch = epoch
        self.recordedAt = recordedAt
    }

    public init(identity: HubIdentity, recordedAt: UInt64) {
        self.init(
            hubId: identity.hubId,
            account: identity.account,
            hubDeviceId: identity.hubDeviceId,
            epoch: identity.epoch,
            recordedAt: recordedAt
        )
    }
}

/// Outcome of comparing a live `GET /v1/hub` response against the client's cached hub identity.
public enum HubIdentityComparison: Equatable, Sendable {
    /// No previous identity was cached for this hub address.
    case initial
    /// Same hub and same (or compatible) epoch.
    case unchanged
    /// Same hub account/id moved to a newer epoch (and possibly a new device id).
    case moved(previousDeviceId: String, newDeviceId: String, newEpoch: UInt64)
    /// A hub offered a strictly lower epoch than the highest epoch this client has seen.
    case staleEpoch(cachedEpoch: UInt64, offeredEpoch: UInt64)
    /// A different `hub_id` answered at this address: never accept silently.
    case replaced(previousHubId: String, newHubId: String, detectedAt: UInt64)
}

extension HubIdentity {
    /// Compares this live identity against the highest-epoch cached identity for the same hub address.
    public func compare(against cached: CachedHubIdentity?, now: UInt64 = UInt64(Date().timeIntervalSince1970)) -> HubIdentityComparison {
        guard let cached else { return .initial }
        if hubId != cached.hubId {
            return .replaced(previousHubId: cached.hubId, newHubId: hubId, detectedAt: now)
        }
        if cached.epoch > 0, epoch > 0, epoch < cached.epoch {
            return .staleEpoch(cachedEpoch: cached.epoch, offeredEpoch: epoch)
        }
        if epoch > cached.epoch || (!hubDeviceId.isEmpty && !cached.hubDeviceId.isEmpty && hubDeviceId != cached.hubDeviceId) {
            return .moved(previousDeviceId: cached.hubDeviceId, newDeviceId: hubDeviceId, newEpoch: epoch)
        }
        return .unchanged
    }
}

/// Host capability metadata reported by a node in `NodeMessage::Hello`.
public struct NodeHostInfo: Codable, Equatable, Sendable {
    public let canHost: Bool
    public let appVersion: String
    public let freeBytes: UInt64?
    public let cloudflared: Bool

    public init(canHost: Bool, appVersion: String, freeBytes: UInt64? = nil, cloudflared: Bool = false) {
        self.canHost = canHost
        self.appVersion = appVersion
        self.freeBytes = freeBytes
        self.cloudflared = cloudflared
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        canHost = try c.decode(Bool.self, forKey: .canHost)
        appVersion = try c.decode(String.self, forKey: .appVersion)
        freeBytes = try c.decodeIfPresent(UInt64.self, forKey: .freeBytes)
        cloudflared = try c.decodeIfPresent(Bool.self, forKey: .cloudflared) ?? false
    }
}

/// Machine-readable reasons an install is not selectable as a hub move target.
public enum InstallUnselectableReason: Equatable, Sendable {
    case currentHub
    case cannotHost
    case offline
    case olderVersion
    case other(String)

    public init(rawValue: String) {
        switch rawValue {
        case "current_hub": self = .currentHub
        case "cannot_host": self = .cannotHost
        case "offline": self = .offline
        case "older_version": self = .olderVersion
        default: self = .other(rawValue)
        }
    }
}

/// One install-level device entry returned by `GET /v1/devices/installs`.
/// An app's admin client row and plain node row share an `install_id` and appear once.
public struct InstallDevice: Codable, Equatable, Identifiable, Sendable {
    /// Representative device id (the node row if present, else the hub row, else the first row).
    public let deviceId: String
    /// All non-revoked device row ids belonging to this install.
    public let deviceIds: [String]
    /// Node device id that accepts WebSocket connections and move tickets, if present.
    public let nodeDeviceId: String?
    public let installId: String?
    public let name: String
    public let platform: String
    public let isHub: Bool
    public let online: Bool
    /// Unix seconds when last heard from, if ever.
    public let lastSeen: UInt64?
    public let host: NodeHostInfo?
    /// True when the platform can host (`macos`, `windows`, `linux`) and the node reported `can_host: true`
    /// (or this install is the current hub).
    public let eligible: Bool
    /// True when this install can be picked as a hub-move target right now.
    public let selectable: Bool
    /// Machine-readable reason when `selectable` is false (`current_hub`, `cannot_host`, `offline`, `older_version`).
    public let reason: String?
    /// Non-blocking advisories (e.g. missing `cloudflared` or low disk space).
    public let warnings: [String]

    public var id: String { installId ?? deviceId }

    public init(
        deviceId: String,
        deviceIds: [String] = [],
        nodeDeviceId: String? = nil,
        installId: String? = nil,
        name: String,
        platform: String,
        isHub: Bool,
        online: Bool,
        lastSeen: UInt64? = nil,
        host: NodeHostInfo? = nil,
        eligible: Bool,
        selectable: Bool,
        reason: String? = nil,
        warnings: [String] = []
    ) {
        self.deviceId = deviceId
        self.deviceIds = deviceIds.isEmpty ? [deviceId] : deviceIds
        self.nodeDeviceId = nodeDeviceId
        self.installId = installId
        self.name = name
        self.platform = platform
        self.isHub = isHub
        self.online = online
        self.lastSeen = lastSeen
        self.host = host
        self.eligible = eligible
        self.selectable = selectable
        self.reason = reason
        self.warnings = warnings
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        deviceId = try c.decode(String.self, forKey: .deviceId)
        deviceIds = try c.decodeIfPresent([String].self, forKey: .deviceIds) ?? [deviceId]
        nodeDeviceId = try c.decodeIfPresent(String.self, forKey: .nodeDeviceId)
        installId = try c.decodeIfPresent(String.self, forKey: .installId)
        name = try c.decode(String.self, forKey: .name)
        platform = try c.decode(String.self, forKey: .platform)
        isHub = try c.decode(Bool.self, forKey: .isHub)
        online = try c.decode(Bool.self, forKey: .online)
        lastSeen = try c.decodeIfPresent(UInt64.self, forKey: .lastSeen)
        host = try c.decodeIfPresent(NodeHostInfo.self, forKey: .host)
        eligible = try c.decode(Bool.self, forKey: .eligible)
        selectable = try c.decode(Bool.self, forKey: .selectable)
        reason = try c.decodeIfPresent(String.self, forKey: .reason)
        warnings = try c.decodeIfPresent([String].self, forKey: .warnings) ?? []
    }

    /// Typed view of `reason`, if present.
    public var parsedReason: InstallUnselectableReason? {
        reason.map(InstallUnselectableReason.init(rawValue:))
    }

    /// Platforms that can ever host the hub (`macos`, `windows`, `linux`). Phones and browsers never can.
    public var isHostPlatform: Bool {
        HubMoveFormatting.isHostPlatform(platform)
    }

    /// True only when this install may be chosen as the target of a hub move.
    /// Enforces client-side that phones, browsers, and the current hub can never be selected as a target.
    public var canBeHubTarget: Bool {
        !isHub && eligible && selectable && isHostPlatform
    }

    /// Human-readable platform label.
    public var platformDisplayName: String {
        HubMoveFormatting.platformDisplayName(platform)
    }
}

/// Response payload for `GET /v1/devices/installs`.
public struct InstallDevicesResponse: Codable, Equatable, Sendable {
    public let hubDeviceId: String
    public let epoch: UInt64
    public let hubVersion: String
    /// Unix seconds when the current hub device became the hub, if recorded.
    public let hubSince: UInt64?
    public let estimatedSnapshotBytes: UInt64
    public let installs: [InstallDevice]

    public init(
        hubDeviceId: String,
        epoch: UInt64,
        hubVersion: String,
        hubSince: UInt64? = nil,
        estimatedSnapshotBytes: UInt64 = 0,
        installs: [InstallDevice] = []
    ) {
        self.hubDeviceId = hubDeviceId
        self.epoch = epoch
        self.hubVersion = hubVersion
        self.hubSince = hubSince
        self.estimatedSnapshotBytes = estimatedSnapshotBytes
        self.installs = installs
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        hubDeviceId = try c.decode(String.self, forKey: .hubDeviceId)
        epoch = try c.decode(UInt64.self, forKey: .epoch)
        hubVersion = try c.decode(String.self, forKey: .hubVersion)
        hubSince = try c.decodeIfPresent(UInt64.self, forKey: .hubSince)
        estimatedSnapshotBytes = try c.decodeIfPresent(UInt64.self, forKey: .estimatedSnapshotBytes) ?? 0
        installs = try c.decodeIfPresent([InstallDevice].self, forKey: .installs) ?? []
    }

    /// The install currently acting as the hub.
    public var currentHub: InstallDevice? {
        installs.first(where: \.isHub) ?? installs.first(where: { $0.deviceId == hubDeviceId || $0.deviceIds.contains(hubDeviceId) })
    }

    /// Installs eligible to host (current hub + desktop hosts, online or offline).
    public var eligibleInstalls: [InstallDevice] {
        installs.filter { ($0.eligible || $0.isHub) && ($0.isHub || $0.isHostPlatform) }
    }

    /// Installs that can never be the hub (phones, browsers, CLI-only tokens).
    public var ineligibleInstalls: [InstallDevice] {
        installs.filter { !$0.isHub && (!$0.eligible || !$0.isHostPlatform) }
    }
}

/// Phase of the hub move state machine (`idle -> preparing -> copying -> switching -> done | rolled_back`).
public enum MovePhase: String, Codable, Equatable, Hashable, Sendable {
    case idle
    case preparing
    case copying
    case switching
    case done
    case rolledBack = "rolled_back"

    /// True while a move is actively in progress and writes are paused.
    public var isActive: Bool {
        switch self {
        case .preparing, .copying, .switching: return true
        case .idle, .done, .rolledBack: return false
        }
    }

    /// True before the Switch commit point, when `DELETE /v1/hub/move` can cancel the move.
    public var canCancel: Bool {
        switch self {
        case .preparing, .copying: return true
        case .idle, .switching, .done, .rolledBack: return false
        }
    }

    public var displayLabel: String {
        switch self {
        case .idle: return "Idle"
        case .preparing: return "Preparing"
        case .copying: return "Copying"
        case .switching: return "Switching"
        case .done: return "Done"
        case .rolledBack: return "Rolled back"
        }
    }
}

/// What to do with threads currently running on the hub during Step 2 (Pause).
public enum MoveThreadsMode: String, Codable, Equatable, Hashable, Sendable {
    case wait
    case stop
}

/// Request body for `POST /v1/hub/move` (admin + fresh Google ID token).
/// `idToken` is the Google ID token from a sign-in that used a nonce just issued by
/// `POST /v1/auth/google/nonce`: the hub requires it (issued within 5 minutes, bound to
/// that one-time nonce, and from the hub owner's account). Wire name: `id_token`.
public struct StartMoveRequest: Codable, Equatable, Sendable {
    public let targetDevice: String
    public let idToken: String
    public let threads: MoveThreadsMode

    public init(targetDevice: String, idToken: String, threads: MoveThreadsMode = .wait) {
        self.targetDevice = targetDevice
        self.idToken = idToken
        self.threads = threads
    }
}

/// Public snapshot of the hub move state machine (`GET`/`POST`/`DELETE /v1/hub/move`).
public struct MoveStatus: Codable, Equatable, Sendable {
    public let moveId: String?
    public let state: MovePhase
    public let targetDeviceId: String?
    public let targetName: String?
    public let threadsMode: MoveThreadsMode?
    public let progressPct: UInt8?
    public let snapshotBytes: UInt64?
    public let message: String?
    public let error: String?
    public let startedAt: UInt64?
    public let updatedAt: UInt64?
    public let newEpoch: UInt64?
    public let preMoveBackup: String?

    public static let idle = MoveStatus(state: .idle)

    public init(
        moveId: String? = nil,
        state: MovePhase,
        targetDeviceId: String? = nil,
        targetName: String? = nil,
        threadsMode: MoveThreadsMode? = nil,
        progressPct: UInt8? = nil,
        snapshotBytes: UInt64? = nil,
        message: String? = nil,
        error: String? = nil,
        startedAt: UInt64? = nil,
        updatedAt: UInt64? = nil,
        newEpoch: UInt64? = nil,
        preMoveBackup: String? = nil
    ) {
        self.moveId = moveId
        self.state = state
        self.targetDeviceId = targetDeviceId
        self.targetName = targetName
        self.threadsMode = threadsMode
        self.progressPct = progressPct
        self.snapshotBytes = snapshotBytes
        self.message = message
        self.error = error
        self.startedAt = startedAt
        self.updatedAt = updatedAt
        self.newEpoch = newEpoch
        self.preMoveBackup = preMoveBackup
    }

    /// Plain-language progress summary, e.g. `Moving your hub to Oscar's Mac… copying (62%)`.
    public var summaryLine: String {
        let target = targetName ?? targetDeviceId ?? "new hub"
        switch state {
        case .idle:
            return "No hub move in progress."
        case .preparing:
            return "Moving your hub to \(target)… preparing"
        case .copying:
            if let pct = progressPct {
                return "Moving your hub to \(target)… copying (\(pct)%)"
            }
            return "Moving your hub to \(target)… copying"
        case .switching:
            return "Moving your hub to \(target)… switching"
        case .done:
            return "Hub moved to \(target)."
        case .rolledBack:
            return error ?? message ?? "Hub move rolled back; your previous hub resumed serving."
        }
    }
}

/// Live `hub_move_progress` event parsed from the account event stream (`EventBody.unknown`).
public struct HubMoveProgressUpdate: Equatable, Sendable {
    public let moveId: String
    public let state: MovePhase
    public let targetDeviceId: String?
    public let targetName: String?
    public let progressPct: UInt8?
    public let message: String?
    public let error: String?

    public init?(event: Event) {
        guard case .unknown(let kind, let payload) = event.body, kind == "hub_move_progress" else {
            return nil
        }
        guard case .string(let rawState)? = payload["state"],
              let phase = MovePhase(rawValue: rawState) else {
            return nil
        }
        let moveId: String
        if case .string(let id)? = payload["move_id"] {
            moveId = id
        } else {
            moveId = ""
        }
        func stringField(_ key: String) -> String? {
            if case .string(let value)? = payload[key] { return value }
            return nil
        }
        let pct: UInt8?
        if case .number(let n)? = payload["progress_pct"], n >= 0, n <= 100 {
            pct = UInt8(n.rounded())
        } else {
            pct = nil
        }
        self.moveId = moveId
        self.state = phase
        self.targetDeviceId = stringField("target_device_id")
        self.targetName = stringField("target_name")
        self.progressPct = pct
        self.message = stringField("message")
        self.error = stringField("error")
    }

    /// Merges this live update onto an existing `MoveStatus`, preserving fields not carried by the event.
    public func applying(to previous: MoveStatus?) -> MoveStatus {
        let sameMove = previous?.moveId == moveId || (previous?.moveId == nil)
        return MoveStatus(
            moveId: moveId.isEmpty ? previous?.moveId : moveId,
            state: state,
            targetDeviceId: targetDeviceId ?? (sameMove ? previous?.targetDeviceId : nil),
            targetName: targetName ?? (sameMove ? previous?.targetName : nil),
            threadsMode: sameMove ? previous?.threadsMode : nil,
            progressPct: progressPct ?? (state == .done ? 100 : nil),
            snapshotBytes: sameMove ? previous?.snapshotBytes : nil,
            message: message ?? (sameMove ? previous?.message : nil),
            error: error,
            startedAt: sameMove ? previous?.startedAt : nil,
            updatedAt: UInt64(Date().timeIntervalSince1970),
            newEpoch: sameMove ? previous?.newEpoch : nil,
            preMoveBackup: sameMove ? previous?.preMoveBackup : nil
        )
    }
}

/// Pure formatting & eligibility helpers for the Hub card and Change hub… flow.
public enum HubMoveFormatting: Sendable {
    public static func isHostPlatform(_ platform: String) -> Bool {
        switch platform.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
        case "macos", "windows", "linux":
            return true
        default:
            return false
        }
    }

    public static func platformDisplayName(_ platform: String) -> String {
        switch platform.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
        case "ios": return "iOS"
        case "macos": return "macOS"
        case "windows": return "Windows"
        case "linux": return "Linux"
        case "web": return "Web"
        case "cli": return "Command line"
        default: return platform
        }
    }

    /// Human-readable byte size (e.g. `12.4 MB`, `512 KB`, `1.2 GB`).
    public static func formattedBytes(_ bytes: UInt64) -> String {
        if bytes < 1024 { return "\(bytes) B" }
        let units = ["KB", "MB", "GB", "TB"]
        var value = Double(bytes) / 1024.0
        var unitIndex = 0
        while value >= 1024.0 && unitIndex < units.count - 1 {
            value /= 1024.0
            unitIndex += 1
        }
        if value >= 100 {
            return "\(Int(value.rounded())) \(units[unitIndex])"
        }
        return String(format: "%.1f %@", value, units[unitIndex])
    }

    /// Rough move duration estimate from the uncompressed snapshot size.
    public static func estimatedDuration(snapshotBytes: UInt64) -> String {
        // Conservative throughput (~5 MB/s over home/tunnel link) plus ~20s for backup + switch.
        let copySeconds = Double(snapshotBytes) / (5.0 * 1024.0 * 1024.0)
        let totalSeconds = Int((copySeconds + 20.0).rounded())
        if totalSeconds < 60 {
            return "Less than a minute"
        }
        let minutes = max(1, Int((Double(totalSeconds) / 60.0).rounded()))
        return minutes == 1 ? "About 1 minute" : "About \(minutes) minutes"
    }

    /// `"Hub since 26 Nov"` (using `timeZone` so tests are deterministic).
    public static func hubSinceLabel(timestamp: UInt64, timeZone: TimeZone = .current) -> String {
        "Hub since \(shortDayMonth(timestamp: timestamp, timeZone: timeZone))"
    }

    /// `"26 Nov"` for hub-since and replaced-on messages.
    public static func shortDayMonth(timestamp: UInt64, timeZone: TimeZone = .current) -> String {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_GB")
        formatter.timeZone = timeZone
        formatter.dateFormat = "d MMM"
        return formatter.string(from: Date(timeIntervalSince1970: TimeInterval(timestamp)))
    }

    /// `"21:40"` for offline last-seen messages.
    public static func shortTime(timestamp: UInt64, timeZone: TimeZone = .current) -> String {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_GB")
        formatter.timeZone = timeZone
        formatter.dateFormat = "HH:mm"
        return formatter.string(from: Date(timeIntervalSince1970: TimeInterval(timestamp)))
    }

    /// Plain-language status line for an install row in Change hub….
    public static func statusSubtitle(for install: InstallDevice, timeZone: TimeZone = .current) -> String {
        if install.isHub {
            return "\(install.platformDisplayName) · Hub"
        }
        if !install.eligible || !install.isHostPlatform {
            return "\(install.platformDisplayName) · Can't be the hub"
        }
        switch install.parsedReason {
        case .offline?:
            if let lastSeen = install.lastSeen {
                return "Offline · last seen \(shortTime(timestamp: lastSeen, timeZone: timeZone)). Turn it on to move the hub there."
            }
            return "Offline. Turn it on to move the hub there."
        case .olderVersion?:
            return "Update Grouper on this device first."
        case .cannotHost?:
            return "\(install.platformDisplayName) · Can't be the hub"
        case .currentHub?:
            return "\(install.platformDisplayName) · Hub"
        case .other(let text)?:
            return text
        case nil:
            return install.online ? "Online · \(install.platformDisplayName)" : "Offline · \(install.platformDisplayName)"
        }
    }
}

/// HTTP client for hub identity (`GET /v1/hub`), install listing (`GET /v1/devices/installs`),
/// and hub moves (`GET`/`POST`/`DELETE /v1/hub/move`).
public actor HubMoveClient {
    public nonisolated let baseURL: URL
    private nonisolated let token: String
    private let session: URLSession
    private let encoder = JSONEncoder.grouper
    private let decoder = JSONDecoder.grouper

    public init(baseURL: URL, token: String, session: URLSession = .shared) throws {
        guard let scheme = baseURL.scheme?.lowercased(), scheme == "http" || scheme == "https", baseURL.host != nil else {
            throw HubClientError.invalidBaseURL
        }
        self.baseURL = baseURL
        self.token = token
        self.session = session
    }

    public init(baseURL: String, token: String, session: URLSession = .shared) throws {
        guard let url = URL(string: baseURL.trimmingCharacters(in: .whitespacesAndNewlines)) else {
            throw HubClientError.invalidBaseURL
        }
        try self.init(baseURL: url, token: token, session: session)
    }

    /// `GET /v1/hub` (unauthenticated): hub id, account, `hub_device_id`, and `epoch`.
    public func hubIdentity() async throws -> HubIdentity {
        try await get(["v1", "hub"], authenticated: false)
    }

    /// `GET /v1/devices/installs` (authenticated): installs grouped by `install_id` with eligibility and warnings.
    public func installDevices() async throws -> InstallDevicesResponse {
        try await get(["v1", "devices", "installs"])
    }

    /// `GET /v1/hub/move` (authenticated): current move phase and progress.
    public func moveStatus() async throws -> MoveStatus {
        try await get(["v1", "hub", "move"])
    }

    /// `POST /v1/hub/move` (admin + fresh Google ID token): preflight and start a hub move.
    /// `idToken` must be the token from `GoogleSignIn.idToken` for a nonce from `POST /v1/auth/google/nonce`.
    public func startMove(
        targetDevice: String,
        idToken: String,
        threads: MoveThreadsMode = .wait
    ) async throws -> MoveStatus {
        let token = idToken.trimmingCharacters(in: .whitespacesAndNewlines)
        if token.isEmpty {
            throw HubClientError.badRequest("A fresh Google sign-in is required to move the hub.")
        }
        let body = StartMoveRequest(targetDevice: targetDevice, idToken: token, threads: threads)
        return try await send(["v1", "hub", "move"], method: "POST", body: body)
    }

    /// `DELETE /v1/hub/move` (admin): cancel an active move before the Switch commit point.
    public func cancelMove() async throws -> MoveStatus {
        try await delete(["v1", "hub", "move"])
    }

    /// True when the hub answered 404 or 405 (an older hub without hub-move routes).
    public static func hubLacksMoveAPI(_ error: Error) -> Bool {
        switch error as? HubClientError {
        case .notFound?: return true
        case .server(let status, _)?: return status == 405
        default: return false
        }
    }

    /// True when the hub refused a request with HTTP 503 because a hub move is currently paused/switching.
    public static func isHubMovingError(_ error: Error) -> Bool {
        guard case .server(let status, let message)? = error as? HubClientError else { return false }
        return status == 503 && message.localizedCaseInsensitiveContains("moving")
    }

    // MARK: - HTTP plumbing

    private func get<T: Decodable>(_ path: [String], authenticated: Bool = true) async throws -> T {
        let (data, _) = try await perform(path: path, method: "GET", body: nil, authenticated: authenticated)
        do { return try decoder.decode(T.self, from: data) }
        catch { throw HubClientError.decoding(String(describing: error)) }
    }

    private func send<T: Decodable, Body: Encodable>(_ path: [String], method: String, body: Body, authenticated: Bool = true) async throws -> T {
        let encoded: Data
        do { encoded = try encoder.encode(body) }
        catch { throw HubClientError.decoding(String(describing: error)) }
        let (data, _) = try await perform(path: path, method: method, body: encoded, authenticated: authenticated)
        do { return try decoder.decode(T.self, from: data) }
        catch { throw HubClientError.decoding(String(describing: error)) }
    }

    private func delete<T: Decodable>(_ path: [String]) async throws -> T {
        let (data, _) = try await perform(path: path, method: "DELETE", body: nil, authenticated: true)
        do { return try decoder.decode(T.self, from: data) }
        catch { throw HubClientError.decoding(String(describing: error)) }
    }

    private func perform(path: [String], method: String, body: Data?, authenticated: Bool) async throws -> (Data, HTTPURLResponse) {
        var url = baseURL
        for component in path { url.appendPathComponent(component) }
        var request = URLRequest(url: url)
        request.httpMethod = method
        request.timeoutInterval = 30
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if authenticated, !token.isEmpty {
            request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        }
        if let body {
            request.httpBody = body
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        let data: Data
        let response: URLResponse
        do { (data, response) = try await session.data(for: request) }
        catch { throw HubClientError.transport(String(describing: error)) }
        guard let http = response as? HTTPURLResponse else {
            throw HubClientError.transport("The hub did not return HTTP.")
        }
        guard (200..<300).contains(http.statusCode) else {
            throw mapError(status: http.statusCode, data: data)
        }
        return (data, http)
    }

    private func mapError(status: Int, data: Data) -> HubClientError {
        let message = (try? decoder.decode(MoveErrorEnvelope.self, from: data).error) ?? String(decoding: data, as: UTF8.self)
        switch status {
        case 400: return .badRequest(message)
        case 401: return .unauthorized(message)
        case 403: return .forbidden(message)
        case 404: return .notFound(message)
        case 409: return .conflict(message)
        default: return .server(status: status, message: message)
        }
    }
}

private struct MoveErrorEnvelope: Codable {
    let error: String
}
