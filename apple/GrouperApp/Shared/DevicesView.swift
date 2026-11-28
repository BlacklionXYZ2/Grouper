import SwiftUI
import GrouperAPI

struct DevicesView: View {
    @EnvironmentObject private var model: AppModel
    @StateObject private var moveStore = HubMoveStore()
    @State private var path: [String] = []
    @State private var showingChangeHub = false
    @State private var preselectedTargetId: String?

    var body: some View {
        NavigationStack(path: $path) {
            List {
                if let replaced = moveStore.replacedInfo {
                    Section {
                        HubReplacedCard(
                            host: HubAddress.displayHost(model.hubURL),
                            detectedAt: replaced.detectedAt,
                            onSignInAgain: {
                                moveStore.clearCachedIdentity(for: model.hubURL)
                                model.signOut()
                            }
                        )
                    }
                    .listRowInsets(EdgeInsets(top: 6, leading: 16, bottom: 6, trailing: 16))
                    .listRowBackground(Color.clear)
                }

                if let stale = moveStore.staleEpochInfo {
                    Section {
                        StaleHubCard(cachedEpoch: stale.cached, offeredEpoch: stale.offered)
                    }
                    .listRowInsets(EdgeInsets(top: 6, leading: 16, bottom: 6, trailing: 16))
                    .listRowBackground(Color.clear)
                }

                if moveStore.showsMoveBanner {
                    Section {
                        HubMoveBannerCard(
                            status: moveStore.moveStatus,
                            completedMessage: moveStore.moveCompletedBanner,
                            rolledBackMessage: moveStore.moveRolledBackBanner,
                            cancelling: moveStore.cancelling,
                            onCancel: {
                                Task { await moveStore.cancelMove(model: model) }
                            },
                            onDismiss: {
                                moveStore.dismissFinishedBanners()
                            }
                        )
                    }
                    .listRowInsets(EdgeInsets(top: 6, leading: 16, bottom: 6, trailing: 16))
                    .listRowBackground(Color.clear)
                }

                Section("Hub") {
                    HubSummaryCard(
                        hubInstall: moveStore.currentHubInstall,
                        hubIdentity: moveStore.hubIdentity,
                        hubSince: moveStore.installsResponse?.hubSince,
                        online: model.online,
                        moving: moveStore.moveStatus.state.isActive,
                        replaced: moveStore.replacedInfo != nil,
                        staleEpoch: moveStore.staleEpochInfo != nil,
                        moveAPIAvailable: moveStore.moveAPIAvailable,
                        loading: moveStore.loading && moveStore.installsResponse == nil && moveStore.hubIdentity == nil,
                        onChangeHub: {
                            preselectedTargetId = nil
                            showingChangeHub = true
                        }
                    )
                }
                .listRowInsets(EdgeInsets(top: 6, leading: 16, bottom: 6, trailing: 16))
                .listRowBackground(Color.clear)

                #if os(macOS)
                if let prompt = moveStore.moveHerePrompt(thisMacDeviceId: model.deviceId) {
                    Section {
                        MoveHerePromptCard(
                            hubName: prompt.hubName,
                            onMoveHere: {
                                preselectedTargetId = prompt.thisMacInstallId
                                showingChangeHub = true
                            },
                            onDismiss: {
                                moveStore.dismissMoveHerePrompt(forHubDeviceId: prompt.hubDeviceId)
                            }
                        )
                    }
                    .listRowInsets(EdgeInsets(top: 4, leading: 16, bottom: 6, trailing: 16))
                    .listRowBackground(Color.clear)
                }
                #endif

                Section("Devices") {
                    if model.devices.isEmpty {
                        Text("Run grouper connect on a computer to make it available.")
                            .font(.callout)
                            .foregroundStyle(Theme.text2)
                            .listRowBackground(Theme.surface)
                    } else {
                        ForEach(model.devices) { device in
                            NavigationLink(value: device.id) {
                                VStack(alignment: .leading, spacing: 5) {
                                    HStack(spacing: 8) {
                                        Circle()
                                            .fill(device.online ? Theme.ok : Theme.text3)
                                            .frame(width: 8, height: 8)
                                            .accessibilityHidden(true)
                                        Text(device.name)
                                            .font(.headline)
                                            .foregroundStyle(Theme.text)
                                        if moveStore.isHubDeviceRow(device.id) {
                                            HubBadge()
                                        }
                                    }
                                    Text(device.online ? "Online · \(HubMoveFormatting.platformDisplayName(device.platform))" : offlineText(device))
                                        .font(.caption)
                                        .foregroundStyle(device.online ? Theme.ok : Theme.text3)
                                    Text(device.id == "hub" ? "Hub threads have file tools only."
                                         : device.runsThreads == false ? "Signs in here; threads don't run on it."
                                         : "\(device.capabilities.count) tools")
                                        .font(.caption)
                                        .foregroundStyle(Theme.text3)
                                }
                            }
                            .listRowBackground(Theme.surface)
                        }
                    }
                }
            }
            .grouperListBackground()
            .navigationTitle("Devices")
            .navigationDestination(for: String.self) { id in
                if let device = model.devices.first(where: { $0.id == id }) { DeviceDetailView(device: device) }
            }
            .refreshable {
                await model.refreshDevices()
                await moveStore.refresh(model: model)
            }
            .sheet(isPresented: $showingChangeHub, onDismiss: {
                preselectedTargetId = nil
                moveStore.actionError = nil
                Task {
                    await model.refreshDevices()
                    await moveStore.refresh(model: model)
                }
            }) {
                ChangeHubSheet(moveStore: moveStore, preselectedTargetId: preselectedTargetId)
                    .environmentObject(model)
            }
            // A device chosen elsewhere (the Mac command palette) opens its detail.
            .onChange(of: model.selectedDeviceId, initial: true) { _, id in
                guard let id else { return }
                path = [id]
                model.selectedDeviceId = nil
            }
            .task(id: model.hubURL) {
                while !Task.isCancelled {
                    await model.refreshDevices()
                    await moveStore.refresh(model: model)
                    try? await Task.sleep(for: .seconds(15))
                }
            }
            // Poll faster while a hub move is active so progress and switch completion update promptly.
            .task(id: moveStore.moveStatus.state.isActive) {
                guard moveStore.moveStatus.state.isActive else { return }
                while !Task.isCancelled && moveStore.moveStatus.state.isActive {
                    try? await Task.sleep(for: .milliseconds(1500))
                    guard !Task.isCancelled else { break }
                    await moveStore.refresh(model: model)
                }
                if !Task.isCancelled {
                    await model.refreshDevices()
                }
            }
            // Listen for live hub_move_progress events from the hub's event stream.
            .task(id: "\(model.hubURL)|\(model.signedIn)") {
                guard let client = model.client, model.signedIn else { return }
                do {
                    for try await update in client.eventStream().updates() {
                        guard !Task.isCancelled else { return }
                        if case .live(let event) = update,
                           let progress = HubMoveProgressUpdate(event: event) {
                            moveStore.applyLiveProgress(progress)
                            if !progress.state.isActive {
                                await moveStore.refresh(model: model)
                                await model.refreshDevices()
                            }
                        }
                    }
                } catch {
                    // Polling covers state transitions whenever the stream drops (including during Switch).
                }
            }
        }
    }

    private func offlineText(_ device: DeviceInfo) -> String {
        guard let last = device.lastSeen else { return "Offline · never seen" }
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .abbreviated
        return "Offline · seen \(formatter.localizedString(for: Date(timeIntervalSince1970: TimeInterval(last)), relativeTo: Date()))"
    }
}

// MARK: - Hub Move Store

struct ReplacedHubInfo: Equatable {
    let detectedAt: UInt64
    let newHubId: String
}

#if os(macOS)
struct MoveHerePrompt: Equatable {
    let hubDeviceId: String
    let hubName: String
    let thisMacInstallId: String
}
#endif

@MainActor
final class HubMoveStore: ObservableObject {
    @Published private(set) var hubIdentity: HubIdentity?
    @Published private(set) var installsResponse: InstallDevicesResponse?
    @Published private(set) var moveStatus: MoveStatus = .idle
    @Published private(set) var replacedInfo: ReplacedHubInfo?
    @Published private(set) var staleEpochInfo: (cached: UInt64, offered: UInt64)?
    @Published private(set) var moveAPIAvailable = true
    @Published private(set) var loading = false
    @Published private(set) var authenticating = false
    @Published private(set) var cancelling = false
    @Published var actionError: String?
    @Published var moveCompletedBanner: String?
    @Published var moveRolledBackBanner: String?
    #if os(macOS)
    @Published private var dismissedMoveHereHubDeviceId: String? = UserDefaults.standard.string(
        forKey: HubMoveStore.dismissedMoveHereKey
    )
    private static let dismissedMoveHereKey = "grouper.dismissedMoveHereHubDeviceId"
    #endif

    private static let cachedIdentityPrefix = "grouper.cachedHubIdentity."

    var currentHubInstall: InstallDevice? {
        installsResponse?.currentHub
    }

    var showsMoveBanner: Bool {
        moveStatus.state.isActive || moveCompletedBanner != nil || moveRolledBackBanner != nil
    }

    func isHubDeviceRow(_ deviceId: String) -> Bool {
        if let hub = currentHubInstall, hub.deviceId == deviceId || hub.deviceIds.contains(deviceId) {
            return true
        }
        if let hubId = installsResponse?.hubDeviceId, !hubId.isEmpty, hubId == deviceId {
            return true
        }
        if let hubId = hubIdentity?.hubDeviceId, !hubId.isEmpty, hubId == deviceId {
            return true
        }
        return false
    }

    func dismissFinishedBanners() {
        moveCompletedBanner = nil
        moveRolledBackBanner = nil
        if !moveStatus.state.isActive {
            moveStatus = .idle
        }
    }

    #if os(macOS)
    func moveHerePrompt(thisMacDeviceId: String?) -> MoveHerePrompt? {
        guard let thisMacDeviceId, !thisMacDeviceId.isEmpty,
              let response = installsResponse,
              !moveStatus.state.isActive,
              replacedInfo == nil,
              staleEpochInfo == nil,
              dismissedMoveHereHubDeviceId != response.hubDeviceId,
              let currentHub = response.currentHub,
              let thisMac = response.installs.first(where: {
                  $0.deviceId == thisMacDeviceId || $0.nodeDeviceId == thisMacDeviceId || $0.deviceIds.contains(thisMacDeviceId)
              }),
              thisMac.canBeHubTarget else {
            return nil
        }
        return MoveHerePrompt(hubDeviceId: response.hubDeviceId, hubName: currentHub.name, thisMacInstallId: thisMac.id)
    }

    func dismissMoveHerePrompt(forHubDeviceId hubDeviceId: String) {
        dismissedMoveHereHubDeviceId = hubDeviceId
        UserDefaults.standard.set(hubDeviceId, forKey: Self.dismissedMoveHereKey)
    }
    #endif

    func applyLiveProgress(_ update: HubMoveProgressUpdate) {
        let updated = update.applying(to: moveStatus)
        moveStatus = updated
        if updated.state == .done {
            moveCompletedBanner = updated.summaryLine
            moveRolledBackBanner = nil
        } else if updated.state == .rolledBack {
            moveRolledBackBanner = updated.summaryLine
            moveCompletedBanner = nil
        }
    }

    func refresh(model: AppModel) async {
        guard model.signedIn, !model.adminToken.isEmpty else { return }
        guard let moveClient = try? HubMoveClient(baseURL: model.hubURL, token: model.adminToken) else { return }
        loading = true
        defer { loading = false }

        let wasActive = moveStatus.state.isActive
        let previousTargetName = moveStatus.targetName

        // 1. Check GET /v1/hub first so replaced/stale-epoch hubs are detected even before authenticated calls.
        do {
            let identity = try await moveClient.hubIdentity()
            hubIdentity = identity
            reconcile(identity: identity, hubURL: model.hubURL, wasMoving: wasActive, targetName: previousTargetName)
        } catch {
            // During Step 5 (switching), temporary disconnects or 503s are expected while the new hub starts.
            if wasActive && (moveStatus.state == .switching || HubMoveClient.isHubMovingError(error)) {
                return
            }
        }

        // Don't fetch or mutate against a replaced hub.
        if replacedInfo != nil {
            return
        }

        // 2. Fetch install-level device list (`GET /v1/devices/installs`).
        do {
            installsResponse = try await moveClient.installDevices()
            moveAPIAvailable = true
        } catch {
            if HubMoveClient.hubLacksMoveAPI(error) {
                moveAPIAvailable = false
                return
            }
        }

        // 3. Fetch move state (`GET /v1/hub/move`).
        do {
            let fetched = try await moveClient.moveStatus()
            if wasActive && fetched.state == .idle && moveCompletedBanner != nil {
                // We just switched to the new hub, whose move state starts at idle.
                moveStatus = .idle
            } else {
                moveStatus = fetched
                if fetched.state == .done {
                    moveCompletedBanner = fetched.summaryLine
                    moveRolledBackBanner = nil
                } else if fetched.state == .rolledBack {
                    moveRolledBackBanner = fetched.summaryLine
                    moveCompletedBanner = nil
                }
            }
        } catch {
            if HubMoveClient.hubLacksMoveAPI(error) {
                moveAPIAvailable = false
            } else if HubMoveClient.isHubMovingError(error), !moveStatus.state.isActive {
                moveStatus = MoveStatus(state: .switching, message: "Your hub is moving…")
            }
        }
    }

    func startMove(to target: InstallDevice, threads: MoveThreadsMode, model: AppModel) async -> Bool {
        guard target.canBeHubTarget else {
            actionError = "\(target.name) can't be the hub."
            return false
        }
        guard let client = model.client else {
            actionError = "Not connected to the hub."
            return false
        }
        guard let oauth = GoogleSignIn.oauth else {
            actionError = "Google sign-in isn't configured in this build."
            return false
        }
        guard !authenticating else { return false }

        authenticating = true
        actionError = nil
        moveCompletedBanner = nil
        moveRolledBackBanner = nil
        defer { authenticating = false }

        do {
            // Section 4a: confirming a hub move always requires a fresh Google sign-in with a hub nonce.
            let nonce = try await client.googleNonce()
            let idToken = try await GoogleSignIn.idToken(oauth: oauth, nonce: nonce.nonce)
            let moveClient = try HubMoveClient(baseURL: model.hubURL, token: model.adminToken)
            let targetId = target.nodeDeviceId ?? target.deviceId
            let started = try await moveClient.startMove(targetDevice: targetId, idToken: idToken, threads: threads)
            moveStatus = started
            return true
        } catch {
            actionError = GoogleSignIn.message(for: error)
            return false
        }
    }

    func cancelMove(model: AppModel) async {
        guard moveStatus.state.canCancel, !cancelling else { return }
        guard let moveClient = try? HubMoveClient(baseURL: model.hubURL, token: model.adminToken) else { return }
        cancelling = true
        actionError = nil
        defer { cancelling = false }
        do {
            let updated = try await moveClient.cancelMove()
            moveStatus = updated
            if updated.state == .rolledBack {
                moveRolledBackBanner = updated.summaryLine
            }
        } catch {
            actionError = error.localizedDescription
        }
    }

    func clearCachedIdentity(for hubURL: String) {
        UserDefaults.standard.removeObject(forKey: Self.cacheKey(for: hubURL))
        replacedInfo = nil
        staleEpochInfo = nil
    }

    // MARK: - Cached hub identity

    private func reconcile(identity: HubIdentity, hubURL: String, wasMoving: Bool, targetName: String?) {
        let now = UInt64(Date().timeIntervalSince1970)
        let cached = loadCachedIdentity(for: hubURL)
        switch identity.compare(against: cached, now: now) {
        case .initial:
            replacedInfo = nil
            staleEpochInfo = nil
            saveCachedIdentity(CachedHubIdentity(identity: identity, recordedAt: now), for: hubURL)
        case .unchanged:
            replacedInfo = nil
            staleEpochInfo = nil
            if cached?.hubDeviceId.isEmpty == true && !identity.hubDeviceId.isEmpty {
                saveCachedIdentity(CachedHubIdentity(identity: identity, recordedAt: now), for: hubURL)
            }
        case .moved(_, _, _):
            replacedInfo = nil
            staleEpochInfo = nil
            saveCachedIdentity(CachedHubIdentity(identity: identity, recordedAt: now), for: hubURL)
            if wasMoving {
                let name = targetName ?? "the new computer"
                moveCompletedBanner = "Hub moved to \(name)."
                moveRolledBackBanner = nil
                moveStatus = .idle
            }
        case .staleEpoch(let cachedEpoch, let offeredEpoch):
            replacedInfo = nil
            staleEpochInfo = (cachedEpoch, offeredEpoch)
        case .replaced(_, let newHubId, let detectedAt):
            staleEpochInfo = nil
            if replacedInfo?.newHubId != newHubId {
                replacedInfo = ReplacedHubInfo(detectedAt: detectedAt, newHubId: newHubId)
            }
            moveStatus = .idle
        }
    }

    private static func cacheKey(for hubURL: String) -> String {
        let normalized = (try? HubAddress.validate(hubURL)) ?? hubURL.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        return cachedIdentityPrefix + normalized
    }

    private func loadCachedIdentity(for hubURL: String) -> CachedHubIdentity? {
        guard let data = UserDefaults.standard.data(forKey: Self.cacheKey(for: hubURL)) else { return nil }
        return try? JSONDecoder().decode(CachedHubIdentity.self, from: data)
    }

    private func saveCachedIdentity(_ cached: CachedHubIdentity, for hubURL: String) {
        if let data = try? JSONEncoder().encode(cached) {
            UserDefaults.standard.set(data, forKey: Self.cacheKey(for: hubURL))
        }
    }
}

// MARK: - Hub Card & Status Banners

private struct HubBadge: View {
    var body: some View {
        Text("Hub")
            .font(Theme.Typeface.chip)
            .foregroundStyle(Theme.accent)
            .padding(.horizontal, 7)
            .padding(.vertical, 2)
            .background(Theme.accentSoft, in: RoundedRectangle(cornerRadius: Theme.Radius.xs))
    }
}

private struct HubSummaryCard: View {
    let hubInstall: InstallDevice?
    let hubIdentity: HubIdentity?
    let hubSince: UInt64?
    let online: Bool
    let moving: Bool
    let replaced: Bool
    let staleEpoch: Bool
    let moveAPIAvailable: Bool
    let loading: Bool
    let onChangeHub: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .top, spacing: 10) {
                Image(systemName: platformIcon)
                    .font(.title2)
                    .foregroundStyle(Theme.accent)
                    .frame(width: 34, height: 34)
                    .background(Theme.accentSoft, in: RoundedRectangle(cornerRadius: Theme.Radius.sm))
                    .accessibilityHidden(true)

                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 8) {
                        Text(hubTitle)
                            .font(Theme.Typeface.title)
                            .foregroundStyle(Theme.text)
                        Spacer(minLength: 4)
                        statusPill
                    }
                    Text(subtitleLine)
                        .font(Theme.Typeface.meta)
                        .foregroundStyle(Theme.text2)
                    // `account` from /v1/hub is an opaque id (no email), so it isn't shown;
                    // Settings > Account shows the signed-in Google email.
                }
            }

            if moveAPIAvailable {
                Button(action: onChangeHub) {
                    HStack(spacing: 6) {
                        Image(systemName: "arrow.left.arrow.right")
                            .accessibilityHidden(true)
                        Text("Change hub…")
                    }
                }
                .buttonStyle(GrouperActionButtonStyle(kind: .outline))
                .disabled(!online || moving || replaced || staleEpoch)
            } else {
                Text("Update your hub to move it to another computer.")
                    .font(Theme.Typeface.meta)
                    .foregroundStyle(Theme.text3)
            }
        }
        .grouperCard(padding: 14)
    }

    private var hubTitle: String {
        if let name = hubInstall?.name, !name.isEmpty { return name }
        if loading { return "Checking hub…" }
        return "Grouper Hub"
    }

    private var subtitleLine: String {
        var parts: [String] = []
        if let platform = hubInstall?.platformDisplayName, !platform.isEmpty {
            parts.append(platform)
        }
        if let hubSince {
            parts.append(HubMoveFormatting.hubSinceLabel(timestamp: hubSince))
        } else if let version = hubIdentity?.hubVersion, !version.isEmpty {
            parts.append("v\(version)")
        }
        return parts.isEmpty ? "Primary hub device" : parts.joined(separator: " · ")
    }

    private var platformIcon: String {
        switch hubInstall?.platform.lowercased() {
        case "macos": return "laptopcomputer"
        case "windows": return "desktopcomputer"
        case "linux": return "server.rack"
        default: return "server.rack"
        }
    }

    private var statusPillInfo: (word: String, icon: String, ink: Color, fill: Color) {
        if replaced { return ("Replaced", "exclamationmark.triangle.fill", Theme.danger, Theme.dangerSoft) }
        if moving { return ("Moving", "arrow.triangle.2.circlepath", Theme.warn, Theme.warnSoft) }
        if online { return ("Online", "checkmark.circle.fill", Theme.ok, Theme.okSoft) }
        return ("Offline", "wifi.slash", Theme.danger, Theme.dangerSoft)
    }

    private var statusPill: some View {
        let info = statusPillInfo
        return HStack(spacing: 4) {
            Image(systemName: info.icon).font(.caption2.weight(.bold))
            Text(info.word)
        }
        .font(Theme.Typeface.chip)
        .foregroundStyle(info.ink)
        .padding(.horizontal, 8)
        .padding(.vertical, 3)
        .background(info.fill, in: RoundedRectangle(cornerRadius: Theme.Radius.sm))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Hub status: \(info.word)")
    }
}

private struct HubMoveBannerCard: View {
    let status: MoveStatus
    let completedMessage: String?
    let rolledBackMessage: String?
    let cancelling: Bool
    let onCancel: () -> Void
    let onDismiss: () -> Void

    var body: some View {
        if status.state.isActive {
            VStack(alignment: .leading, spacing: 10) {
                HStack(spacing: 8) {
                    Image(systemName: "arrow.triangle.2.circlepath")
                        .foregroundStyle(Theme.warn)
                        .accessibilityHidden(true)
                    Text(status.summaryLine)
                        .font(Theme.Typeface.row)
                        .foregroundStyle(Theme.text)
                }
                if let pct = status.progressPct {
                    ProgressView(value: Double(pct), total: 100)
                        .tint(Theme.accent)
                } else {
                    ProgressView()
                        .tint(Theme.accent)
                }
                Text("Grouper pauses for everyone during the move.")
                    .font(Theme.Typeface.meta)
                    .foregroundStyle(Theme.text2)
                if status.state.canCancel {
                    Button(cancelling ? "Cancelling…" : "Cancel move", action: onCancel)
                        .buttonStyle(GrouperActionButtonStyle(kind: .outline))
                        .disabled(cancelling)
                }
            }
            .grouperCard(padding: 14, fill: Theme.warnSoft, stroke: Theme.warn)
        } else if let completedMessage {
            HStack(alignment: .top, spacing: 10) {
                Image(systemName: "checkmark.circle.fill")
                    .foregroundStyle(Theme.ok)
                    .accessibilityHidden(true)
                Text(completedMessage)
                    .font(.callout)
                    .foregroundStyle(Theme.text)
                Spacer(minLength: 8)
                Button("Dismiss", action: onDismiss)
                    .font(Theme.Typeface.chip)
                    .foregroundStyle(Theme.ok)
                    .buttonStyle(.plain)
            }
            .grouperCard(padding: 14, fill: Theme.okSoft, stroke: Theme.ok)
        } else if let rolledBackMessage {
            HStack(alignment: .top, spacing: 10) {
                Image(systemName: "exclamationmark.triangle.fill")
                    .foregroundStyle(Theme.warn)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 3) {
                    Text("Hub move did not finish")
                        .font(Theme.Typeface.row)
                        .foregroundStyle(Theme.text)
                    Text(rolledBackMessage)
                        .font(Theme.Typeface.meta)
                        .foregroundStyle(Theme.text2)
                }
                Spacer(minLength: 8)
                Button("Dismiss", action: onDismiss)
                    .font(Theme.Typeface.chip)
                    .foregroundStyle(Theme.warn)
                    .buttonStyle(.plain)
            }
            .grouperCard(padding: 14, fill: Theme.warnSoft, stroke: Theme.warn)
        }
    }
}

private struct HubReplacedCard: View {
    let host: String
    let detectedAt: UInt64
    let onSignInAgain: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Label(
                "Your hub was replaced on \(HubMoveFormatting.shortDayMonth(timestamp: detectedAt)). Sign in again.",
                systemImage: "exclamationmark.triangle.fill"
            )
            .font(Theme.Typeface.row)
            .foregroundStyle(Theme.danger)
            Text("A different hub is answering at \(host). Your cached chats are from the previous hub, so sign in again before making changes.")
                .font(Theme.Typeface.meta)
                .foregroundStyle(Theme.text2)
            Button("Sign in again", action: onSignInAgain)
                .buttonStyle(GrouperActionButtonStyle(kind: .primary))
        }
        .grouperCard(padding: 14, fill: Theme.dangerSoft, stroke: Theme.danger)
    }
}

private struct StaleHubCard: View {
    let cachedEpoch: UInt64
    let offeredEpoch: UInt64

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Label("Older hub detected", systemImage: "clock.badge.exclamationmark")
                .font(Theme.Typeface.row)
                .foregroundStyle(Theme.warn)
            Text("This hub offered epoch \(offeredEpoch), older than the hub this device already connected to (epoch \(cachedEpoch)). Waiting for your current hub to answer.")
                .font(Theme.Typeface.meta)
                .foregroundStyle(Theme.text2)
        }
        .grouperCard(padding: 14, fill: Theme.warnSoft, stroke: Theme.warn)
    }
}

#if os(macOS)
private struct MoveHerePromptCard: View {
    let hubName: String
    let onMoveHere: () -> Void
    let onDismiss: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("\(hubName) is your hub. Move it to this Mac?")
                .font(Theme.Typeface.row)
                .foregroundStyle(Theme.text)
            HStack(spacing: 10) {
                Button("Move to this Mac", action: onMoveHere)
                    .buttonStyle(GrouperActionButtonStyle(kind: .primary))
                Button("Not now", action: onDismiss)
                    .buttonStyle(GrouperActionButtonStyle(kind: .outline))
            }
        }
        .grouperCard(padding: 14, fill: Theme.accentSoft, stroke: Theme.accent)
    }
}
#endif

// MARK: - Change Hub Flow (Picker -> Confirmation + Google Sign-In -> Progress)

private struct ChangeHubSheet: View {
    @EnvironmentObject private var model: AppModel
    @ObservedObject var moveStore: HubMoveStore
    let preselectedTargetId: String?

    @Environment(\.dismiss) private var dismiss
    @State private var selectedTargetId: String?
    @State private var showingProgress = false

    var body: some View {
        NavigationStack {
            Group {
                if showingProgress || moveStore.moveStatus.state.isActive {
                    HubMoveProgressView(moveStore: moveStore, onClose: { dismiss() })
                } else {
                    installPickerList
                }
            }
            .navigationDestination(item: $selectedTargetId) { installId in
                if let target = moveStore.installsResponse?.installs.first(where: { $0.id == installId }) {
                    ConfirmHubMoveView(
                        moveStore: moveStore,
                        target: target,
                        snapshotBytes: moveStore.installsResponse?.estimatedSnapshotBytes ?? 0,
                        onMoveStarted: {
                            selectedTargetId = nil
                            showingProgress = true
                        }
                    )
                }
            }
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button(showingProgress || moveStore.moveStatus.state.isActive ? "Hide" : "Close") {
                        dismiss()
                    }
                }
            }
        }
        #if os(macOS)
        .frame(minWidth: 480, minHeight: 420)
        #endif
        .onChange(of: moveStore.moveStatus.state.isActive, initial: true) { _, active in
            if active { showingProgress = true }
        }
        .task {
            await moveStore.refresh(model: model)
            if moveStore.moveStatus.state.isActive {
                showingProgress = true
            } else if let preselectedTargetId,
                      selectedTargetId == nil,
                      moveStore.installsResponse?.installs.contains(where: { $0.id == preselectedTargetId && $0.canBeHubTarget }) == true {
                selectedTargetId = preselectedTargetId
            }
        }
    }

    private var installPickerList: some View {
        List {
            if let response = moveStore.installsResponse {
                Section {
                    ForEach(response.eligibleInstalls) { install in
                        InstallCandidateRow(install: install) {
                            moveStore.actionError = nil
                            selectedTargetId = install.id
                        }
                    }
                } header: {
                    Text("Computers")
                } footer: {
                    Text("Both your current hub and the destination computer must be online to move.")
                }

                if !response.ineligibleInstalls.isEmpty {
                    Section("Can't be the hub") {
                        ForEach(response.ineligibleInstalls) { install in
                            IneligibleInstallRow(install: install)
                        }
                    }
                }
            } else if moveStore.loading {
                Section {
                    HStack(spacing: 10) {
                        ProgressView().controlSize(.small)
                        Text("Loading devices…")
                            .foregroundStyle(Theme.text2)
                    }
                    .listRowBackground(Theme.surface)
                }
            } else {
                Section {
                    Text("Couldn't load the device list from the hub.")
                        .font(.callout)
                        .foregroundStyle(Theme.text2)
                        .listRowBackground(Theme.surface)
                    Button("Try again") {
                        Task { await moveStore.refresh(model: model) }
                    }
                    .listRowBackground(Theme.surface)
                }
            }
        }
        .grouperListBackground()
        .navigationTitle("Change hub")
        #if os(iOS)
        .navigationBarTitleDisplayMode(.inline)
        #endif
        .refreshable {
            await moveStore.refresh(model: model)
        }
    }
}

private struct InstallCandidateRow: View {
    let install: InstallDevice
    let onSelect: () -> Void

    var body: some View {
        Group {
            if install.canBeHubTarget {
                Button(action: onSelect) {
                    rowContent
                }
                .buttonStyle(.plain)
            } else {
                rowContent
            }
        }
        .listRowBackground(Theme.surface)
    }

    private var rowContent: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Circle()
                    .fill(install.online ? Theme.ok : Theme.text3)
                    .frame(width: 8, height: 8)
                    .accessibilityHidden(true)
                Text(install.name)
                    .font(Theme.Typeface.row)
                    .foregroundStyle(install.canBeHubTarget || install.isHub ? Theme.text : Theme.text3)
                Spacer(minLength: 8)
                if install.isHub {
                    Image(systemName: "checkmark.circle.fill")
                        .foregroundStyle(Theme.accent)
                        .accessibilityLabel("Current hub")
                } else if install.canBeHubTarget {
                    Image(systemName: "chevron.right")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(Theme.text3)
                        .accessibilityHidden(true)
                }
            }
            Text(HubMoveFormatting.statusSubtitle(for: install))
                .font(Theme.Typeface.meta)
                .foregroundStyle(subtitleColor)
            ForEach(install.warnings, id: \.self) { warning in
                Label(warning, systemImage: "exclamationmark.triangle.fill")
                    .font(Theme.Typeface.meta)
                    .foregroundStyle(Theme.warn)
            }
        }
        .padding(.vertical, 4)
        .contentShape(Rectangle())
    }

    private var subtitleColor: Color {
        if install.isHub { return Theme.accent }
        if install.canBeHubTarget { return Theme.ok }
        if install.parsedReason == .olderVersion { return Theme.warn }
        return Theme.text3
    }
}

private struct IneligibleInstallRow: View {
    let install: InstallDevice

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: icon)
                .foregroundStyle(Theme.text3)
                .frame(width: 20)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(install.name)
                    .font(Theme.Typeface.row)
                    .foregroundStyle(Theme.text3)
                Text(install.platformDisplayName)
                    .font(Theme.Typeface.meta)
                    .foregroundStyle(Theme.text3)
            }
            Spacer()
        }
        .padding(.vertical, 2)
        .listRowBackground(Theme.surface)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(install.name), \(install.platformDisplayName), can't be the hub")
    }

    private var icon: String {
        switch install.platform.lowercased() {
        case "ios": return "iphone"
        case "web": return "globe"
        case "cli": return "terminal"
        default: return "desktopcomputer"
        }
    }
}

private struct ConfirmHubMoveView: View {
    @EnvironmentObject private var model: AppModel
    @ObservedObject var moveStore: HubMoveStore
    let target: InstallDevice
    let snapshotBytes: UInt64
    let onMoveStarted: () -> Void

    @State private var threadsMode: MoveThreadsMode = .wait

    private var runningHubThreads: [ThreadInfo] {
        model.threads.filter { $0.deviceId == "hub" && !$0.isTerminal }
    }

    var body: some View {
        List {
            Section {
                VStack(alignment: .leading, spacing: 8) {
                    Text("Move your hub to \(target.name)?")
                        .font(Theme.Typeface.title)
                        .foregroundStyle(Theme.text)
                    Text("Your conversations, threads, memory, skills, provider keys, and device registrations will copy to \(target.name) (\(target.platformDisplayName)).")
                        .font(.callout)
                        .foregroundStyle(Theme.text2)
                }
                .padding(.vertical, 4)
                .listRowBackground(Theme.surface)

                LabeledContent("Data to copy", value: HubMoveFormatting.formattedBytes(snapshotBytes))
                    .listRowBackground(Theme.surface)
                LabeledContent("Estimated time", value: HubMoveFormatting.estimatedDuration(snapshotBytes: snapshotBytes))
                    .listRowBackground(Theme.surface)
            }

            Section {
                Label("Grouper pauses for everyone during the move.", systemImage: "pause.circle")
                    .font(.callout)
                    .foregroundStyle(Theme.text2)
                    .listRowBackground(Theme.surface)
            }

            if !target.warnings.isEmpty {
                Section("Notes") {
                    ForEach(target.warnings, id: \.self) { warning in
                        Label(warning, systemImage: "exclamationmark.triangle.fill")
                            .font(.callout)
                            .foregroundStyle(Theme.warn)
                            .listRowBackground(Theme.warnSoft)
                    }
                }
            }

            if !runningHubThreads.isEmpty {
                Section {
                    Picker("Running hub threads (\(runningHubThreads.count))", selection: $threadsMode) {
                        Text("Wait for them").tag(MoveThreadsMode.wait)
                        Text("Stop them and move now").tag(MoveThreadsMode.stop)
                    }
                    .pickerStyle(.inline)
                    .listRowBackground(Theme.surface)
                } header: {
                    Text("Hub threads running")
                } footer: {
                    Text("Threads running on other devices keep running and reconnect automatically.")
                }
            }

            Section {
                if GoogleSignIn.oauth != nil {
                    VStack(alignment: .leading, spacing: 10) {
                        Text("Confirm with a fresh Google sign-in on this device to start the move.")
                            .font(Theme.Typeface.meta)
                            .foregroundStyle(Theme.text2)
                        GoogleSignInButton(busy: moveStore.authenticating) {
                            Task {
                                let started = await moveStore.startMove(to: target, threads: threadsMode, model: model)
                                if started { onMoveStarted() }
                            }
                        }
                        .disabled(moveStore.authenticating || !target.canBeHubTarget)
                        .frame(maxWidth: .infinity)
                    }
                    .padding(.vertical, 4)
                    .listRowBackground(Theme.surface)
                } else {
                    Text("Google sign-in isn't configured in this build, so a hub move can't be confirmed here.")
                        .font(.callout)
                        .foregroundStyle(Theme.text2)
                        .listRowBackground(Theme.surface)
                }
            } header: {
                Text("Confirm")
            }

            if let error = moveStore.actionError {
                Section {
                    ErrorBanner(message: error)
                }
                .listRowInsets(EdgeInsets(top: 6, leading: 16, bottom: 6, trailing: 16))
                .listRowBackground(Color.clear)
            }
        }
        .grouperListBackground()
        .navigationTitle("Confirm move")
        #if os(iOS)
        .navigationBarTitleDisplayMode(.inline)
        #endif
    }
}

private struct HubMoveProgressView: View {
    @EnvironmentObject private var model: AppModel
    @ObservedObject var moveStore: HubMoveStore
    let onClose: () -> Void

    var body: some View {
        List {
            Section {
                VStack(alignment: .leading, spacing: 14) {
                    HStack(spacing: 10) {
                        statusIcon
                        Text(moveStore.moveStatus.summaryLine)
                            .font(Theme.Typeface.title)
                            .foregroundStyle(Theme.text)
                    }

                    if moveStore.moveStatus.state.isActive {
                        if let pct = moveStore.moveStatus.progressPct {
                            ProgressView(value: Double(pct), total: 100)
                                .tint(Theme.accent)
                        } else {
                            ProgressView()
                                .tint(Theme.accent)
                        }
                        if let detail = moveStore.moveStatus.message, !detail.isEmpty {
                            Text(detail)
                                .font(.callout)
                                .foregroundStyle(Theme.text2)
                        }
                        Text("Grouper pauses for everyone during the move. You can close this sheet and keep watching progress in Devices.")
                            .font(Theme.Typeface.meta)
                            .foregroundStyle(Theme.text3)
                    } else if moveStore.moveStatus.state == .done || moveStore.moveCompletedBanner != nil {
                        Text(moveStore.moveCompletedBanner ?? moveStore.moveStatus.summaryLine)
                            .font(.callout)
                            .foregroundStyle(Theme.text2)
                    } else if moveStore.moveStatus.state == .rolledBack || moveStore.moveRolledBackBanner != nil {
                        Text(moveStore.moveRolledBackBanner ?? moveStore.moveStatus.summaryLine)
                            .font(.callout)
                            .foregroundStyle(Theme.text2)
                    }
                }
                .padding(.vertical, 6)
                .listRowBackground(Theme.surface)
            }

            if let error = moveStore.actionError {
                Section {
                    ErrorBanner(message: error)
                }
                .listRowInsets(EdgeInsets(top: 6, leading: 16, bottom: 6, trailing: 16))
                .listRowBackground(Color.clear)
            }

            Section {
                if moveStore.moveStatus.state.canCancel {
                    Button(moveStore.cancelling ? "Cancelling…" : "Cancel move") {
                        Task { await moveStore.cancelMove(model: model) }
                    }
                    .buttonStyle(GrouperActionButtonStyle(kind: .outline))
                    .disabled(moveStore.cancelling)
                    .listRowBackground(Color.clear)
                    .listRowInsets(EdgeInsets(top: 6, leading: 16, bottom: 6, trailing: 16))
                } else if !moveStore.moveStatus.state.isActive {
                    Button("Done", action: onClose)
                        .buttonStyle(GrouperActionButtonStyle(kind: .primary))
                        .listRowBackground(Color.clear)
                        .listRowInsets(EdgeInsets(top: 6, leading: 16, bottom: 6, trailing: 16))
                }
            }
        }
        .grouperListBackground()
        .navigationTitle("Moving hub")
        #if os(iOS)
        .navigationBarTitleDisplayMode(.inline)
        #endif
        .navigationBarBackButtonHidden(true)
    }

    @ViewBuilder private var statusIcon: some View {
        switch moveStore.moveStatus.state {
        case .preparing, .copying, .switching:
            Image(systemName: "arrow.triangle.2.circlepath")
                .font(.title3)
                .foregroundStyle(Theme.warn)
                .accessibilityHidden(true)
        case .done:
            Image(systemName: "checkmark.circle.fill")
                .font(.title3)
                .foregroundStyle(Theme.ok)
                .accessibilityHidden(true)
        case .rolledBack:
            Image(systemName: "exclamationmark.triangle.fill")
                .font(.title3)
                .foregroundStyle(Theme.warn)
                .accessibilityHidden(true)
        case .idle:
            if moveStore.moveCompletedBanner != nil {
                Image(systemName: "checkmark.circle.fill")
                    .font(.title3)
                    .foregroundStyle(Theme.ok)
                    .accessibilityHidden(true)
            } else {
                Image(systemName: "server.rack")
                    .font(.title3)
                    .foregroundStyle(Theme.accent)
                    .accessibilityHidden(true)
            }
        }
    }
}

// MARK: - Device Detail

struct DeviceDetailView: View {
    @EnvironmentObject private var model: AppModel
    let device: DeviceInfo
    @State private var bypassCandidate: ToolInfo?

    var body: some View {
        List {
            Section {
                Text("Workspace file tools and known agent tools start on Auto; everything else starts on Manual. Only you can change these permissions.")
                    .font(.callout).foregroundStyle(Theme.text2)
                if device.id == "hub" { Text("Hub threads have file tools only.").font(.callout) }
            }
            .listRowBackground(Theme.surface)
            Section("Available") { toolRows(tools.filter(\.available)) }
                .listRowBackground(Theme.surface)
            if tools.contains(where: { !$0.available }) {
                Section("Unavailable") { toolRows(tools.filter { !$0.available }) }
                    .listRowBackground(Theme.surface)
            }
        }
        .grouperListBackground()
        .navigationTitle(device.name)
        .task(id: device.id) { await model.loadTools(deviceId: device.id) }
        .confirmationDialog("Always run \(bypassCandidate?.name ?? "this tool")?", isPresented: Binding(
            get: { bypassCandidate != nil }, set: { if !$0 { bypassCandidate = nil } }
        ), titleVisibility: .visible) {
            Button("Set to Bypass", role: .destructive) {
                guard let tool = bypassCandidate else { return }
                bypassCandidate = nil
                Task { await model.setPermission(deviceId: device.id, tool: tool, permission: .bypass) }
            }
            Button("Cancel", role: .cancel) { bypassCandidate = nil }
        } message: {
            Text("Every call runs without asking, including deletes and commands outside the workspace. Only you can put it back.")
        }
    }

    private var tools: [ToolInfo] { model.toolsByDevice[device.id] ?? [] }

    @ViewBuilder private func toolRows(_ tools: [ToolInfo]) -> some View {
        if tools.isEmpty { Text("No tools").foregroundStyle(Theme.text3) }
        ForEach(tools) { tool in
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    Text(tool.name).font(.headline).foregroundStyle(Theme.text)
                    if !tool.available { Text("Unavailable").font(.caption).foregroundStyle(Theme.text3) }
                    if tool.permission == .bypass { Label("Bypass", systemImage: "exclamationmark.triangle.fill").font(.caption).foregroundStyle(Theme.danger) }
                }
                if !tool.description.isEmpty { Text(LocalizedStringKey(tool.description)).font(.callout).foregroundStyle(Theme.text2) }
                Picker("Permission for \(tool.name)", selection: Binding(
                    get: { tool.permission },
                    set: { permission in
                        if permission == .bypass { bypassCandidate = tool }
                        else { Task { await model.setPermission(deviceId: device.id, tool: tool, permission: permission) } }
                    }
                )) {
                    Text("Manual").tag(Permission.manual)
                    Text("Auto").tag(Permission.auto)
                    Text("Bypass").tag(Permission.bypass)
                }
                .pickerStyle(.segmented).disabled(!tool.available || !model.online)
                .accessibilityHint(policy(tool: tool, permission: tool.permission))
                Text(policy(tool: tool, permission: tool.permission)).font(.caption)
                    .foregroundStyle(tool.permission == .bypass ? Theme.danger : isUnconfined(tool) ? Theme.warn : Theme.text2)
            }.padding(.vertical, 6)
        }
    }

    private func policy(tool: ToolInfo, permission: Permission) -> String {
        switch permission {
        case .manual: return "Every call waits for you."
        case .bypass: return "Every call runs, including deletes and commands outside the workspace."
        case .auto:
            let server = tool.name.split(separator: "__").first.map(String.init) ?? ""
            if ["hermes", "grok", "opencode"].contains(server) {
                return "Auto does not confine this agent. It has no safety mode, so Auto runs it unconfined."
            }
            if ["claude-code", "codex", "cursor", "antigravity", "pi"].contains(server) {
                return "Runs in the thread workspace unless a risk rule asks; leaving it, requesting bypass, or a destructive instruction still asks."
            }
            return "Runs unless a risk rule asks."
        }
    }

    private func isUnconfined(_ tool: ToolInfo) -> Bool {
        let server = tool.name.split(separator: "__").first.map(String.init) ?? ""
        return tool.permission == .auto && ["hermes", "grok", "opencode"].contains(server)
    }
}
