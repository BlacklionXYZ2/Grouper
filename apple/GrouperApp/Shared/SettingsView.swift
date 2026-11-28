import GrouperAPI
import SwiftUI

struct SettingsView: View {
    @EnvironmentObject private var model: AppModel
    @State private var editingHubAddress = false
    #if os(macOS)
    @EnvironmentObject private var node: MacNodeController
    @State private var savingDeviceToken = false
    #endif

    var body: some View {
        Form {
            Section("Connection") {
                Button {
                    editingHubAddress = true
                } label: {
                    LabeledContent("Hub address") {
                        HStack(spacing: 6) {
                            Text(model.hubURL)
                                .foregroundStyle(Theme.text2)
                                .lineLimit(1)
                                .truncationMode(.middle)
                            Image(systemName: "chevron.right")
                                .font(.caption.weight(.semibold))
                                .foregroundStyle(Theme.text3)
                                .accessibilityHidden(true)
                        }
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityHint("Changes the hub address")
                if !model.isDefaultHubAddress {
                    Button("Use default") {
                        model.resetHubAddressToDefault()
                    }
                }
                LabeledContent("Status", value: model.online ? "Connected" : "Offline")
                Button("Check now") { Task { await model.connect(save: false) } }
                    .disabled(model.adminToken.isEmpty)
            }
            AccountSettingsSection()
            #if os(macOS)
            Section("This Mac") {
                if model.registeringDevice {
                    HStack(spacing: 8) { ProgressView().controlSize(.small); Text("Registering this Mac with the hub…") }
                } else if let name = model.thisMacName {
                    Text("This Mac is registered as \(name)")
                } else if model.deviceToken.isEmpty {
                    Text("This Mac isn't registered with the hub yet, so it can't run threads.")
                } else {
                    Text("This Mac is registered with the hub.")
                }
                LabeledContent("Node", value: node.statusText)
                Toggle("Read subscription usage on this device", isOn: Binding(
                    get: { node.readUsage },
                    set: { node.setReadUsage($0) }
                ))
                Text("Lets this Mac read how much of your Claude, ChatGPT, Grok and Antigravity plans is used, from the sign-ins those apps keep here. Only percentages and reset times go to your hub.")
                    .font(.caption).foregroundStyle(Theme.text2)
                if node.readUsage {
                    ForEach(MacNodeController.usageProviders) { provider in
                        Toggle(provider.name, isOn: Binding(
                            get: { !node.usageProvidersOff.contains(provider.id) },
                            set: { node.setReadUsage(provider: provider.id, $0) }
                        ))
                        .padding(.leading, 16)
                    }
                }
                Button(model.registeringDevice ? "Registering…" : "Register again") {
                    Task { await model.registerThisMacAgain() }
                }
                .disabled(model.registeringDevice || model.client == nil)
                Text("Reconnects this Mac to the hub as the same device; its old credentials stop working.")
                    .font(.caption).foregroundStyle(Theme.text2)
                if let message = model.deviceMessage {
                    Text(message).font(.caption).foregroundStyle(Theme.danger)
                }
                // Recovery only: offered when automatic registration didn't work.
                if model.deviceMessage != nil || model.showManualDeviceToken {
                    DisclosureGroup("Use an existing device token", isExpanded: $model.showManualDeviceToken) {
                        SecureField("Device token", text: $model.manualDeviceToken)
                            .textContentType(.password)
                        Button(savingDeviceToken ? "Verifying…" : "Use this token") {
                            savingDeviceToken = true
                            Task {
                                _ = await model.saveManualDeviceToken()
                                savingDeviceToken = false
                            }
                        }
                        .disabled(savingDeviceToken || model.client == nil
                                  || model.manualDeviceToken.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                        Text("For a plain device token made on the hub with grouper-hub device add. Never paste an admin token here.")
                            .font(.caption).foregroundStyle(Theme.text2)
                    }
                }
            }
            #endif
            NotificationSettingsSection()
            Section {
                Button("Sign out on this device", role: .destructive) { model.signOut() }
                #if os(macOS)
                Button("Sign out and remove this Mac", role: .destructive) {
                    Task { await model.signOutAndRemoveThisMac() }
                }
                .disabled(model.registeringDevice)
                Text("Signing out keeps this Mac registered, so it can keep running threads, and signing in again reuses this Mac's devices. Removing this Mac also takes it off the hub.")
                    .font(.caption).foregroundStyle(Theme.text2)
                #else
                Text("Signing in again on this iPhone reuses its device on the hub.")
                    .font(.caption).foregroundStyle(Theme.text2)
                #endif
            }
        }
        #if os(macOS)
        .formStyle(.grouped)
        #endif
        .grouperListBackground()
        .navigationTitle("Settings")
        .sheet(isPresented: $editingHubAddress) {
            HubAddressEditorSheet()
                .environmentObject(model)
        }
    }
}

/// Sheet for editing the hub address from Settings or when sign-in can't reach or join the hub.
struct HubAddressEditorSheet: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var draft = ""
    @State private var validationError: String?

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField(HubAddress.defaultAddress, text: $draft)
                        #if os(iOS)
                        .textContentType(.URL)
                        .textInputAutocapitalization(.never)
                        .keyboardType(.URL)
                        #endif
                        .autocorrectionDisabled()
                        .accessibilityLabel("Hub address")
                        .onSubmit { save() }
                    Button("Use default") {
                        draft = HubAddress.defaultAddress
                        validationError = nil
                    }
                    .disabled(HubAddress.isDefault(draft))
                } header: {
                    Text("Hub address")
                } footer: {
                    Text(footerText)
                }
                if let validationError {
                    Section {
                        Text(validationError)
                            .foregroundStyle(Theme.danger)
                            .accessibilityLabel("Invalid hub address. \(validationError)")
                    }
                }
            }
            #if os(macOS)
            .formStyle(.grouped)
            #endif
            .grouperListBackground()
            .navigationTitle("Hub address")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button(saveButtonTitle) { save() }
                        .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                }
            }
            .onAppear {
                draft = model.hubURL
                validationError = nil
            }
        }
        #if os(macOS)
        .frame(minWidth: 440, minHeight: 240)
        #endif
    }

    private var saveButtonTitle: String {
        if model.signedIn && !DeviceRegistration.sameHub(model.hubURL, draft) {
            return "Save & Sign In"
        }
        return "Save"
    }

    private var footerText: String {
        if model.signedIn {
            return "Default: \(HubAddress.defaultAddress). Changing the hub address signs out and signs back in at the new address. HTTPS is required except for localhost and local network addresses."
        }
        return "Default: \(HubAddress.defaultAddress). HTTPS is required except for localhost and local network addresses."
    }

    private func save() {
        if let error = model.changeHubAddress(to: draft) {
            validationError = error
            return
        }
        validationError = nil
        dismiss()
    }
}

/// Settings > Account (docs/accounts-design.md): the Google account you're signed in as (the
/// hub's owner) as the identity, this install's device (name and platform) under it, then
/// Unlink, or "Link your Google account" so the owner can sign in with Google instead of a token.
/// Device ids and tokens are internal and never shown.
struct AccountSettingsSection: View {
    @EnvironmentObject private var model: AppModel
    @State private var confirmUnlink = false

    var body: some View {
        Section("Account") {
            if let account = model.linkedGoogle {
                LabeledContent("Signed in as") {
                    VStack(alignment: .trailing, spacing: 2) {
                        Text(account.email).textSelection(.enabled)
                        if !account.name.isEmpty {
                            Text(account.name).font(.caption).foregroundStyle(Theme.text2)
                        }
                    }
                }
                deviceDetails
                Button(model.googleBusy ? "Unlinking…" : "Unlink", role: .destructive) { confirmUnlink = true }
                    .disabled(model.googleBusy)
                    .confirmationDialog("Unlink \(account.email)?", isPresented: $confirmUnlink, titleVisibility: .visible) {
                        Button("Unlink", role: .destructive) { Task { await model.unlinkGoogle() } }
                        Button("Cancel", role: .cancel) {}
                    } message: {
                        Text("Signing in to this hub with Google stops working until you link an account again. Tokens and devices keep working.")
                    }
            } else if model.client == nil {
                Text("Sign in to see the Google account linked to the hub.")
                    .font(.callout).foregroundStyle(Theme.text2)
            } else if !model.googleLinkLoaded {
                HStack(spacing: 8) { ProgressView().controlSize(.small); Text("Checking the linked account…") }
            } else {
                if GoogleSignIn.oauth == nil {
                    Text("This app has no Google client ID, so Google sign-in is off.")
                        .font(.callout).foregroundStyle(Theme.text2)
                } else if model.authStatus?.google != true {
                    Text("Google sign-in isn't turned on for this hub.")
                        .font(.callout).foregroundStyle(Theme.text2)
                } else {
                    Button(model.googleBusy ? "Linking…" : "Link your Google account") {
                        Task { await model.linkGoogle() }
                    }
                    .disabled(model.googleBusy || model.client == nil)
                    Text("Then you can sign in to this hub with Google.")
                        .font(.caption).foregroundStyle(Theme.text2)
                }
                deviceDetails
            }
            if let message = model.accountMessage {
                Text(message).font(.caption).foregroundStyle(Theme.danger)
            }
        }
        .task(id: model.signedIn) { await model.refreshGoogleLink() }
    }

    /// This install's own device on the hub: its name and platform, never its id or token.
    @ViewBuilder private var deviceDetails: some View {
        if let device = model.thisAppDevice {
            LabeledContent("This \(deviceWord)", value: device.name)
            LabeledContent("Platform", value: device.platformName)
        }
    }

    private var deviceWord: String {
        #if os(iOS)
        "iPhone"
        #else
        "Mac app"
        #endif
    }
}

/// Settings > Notifications (docs/gui-spec.md 5.11, 5.14): the explicit opt-in for
/// approval notifications on this device. The system permission prompt only ever
/// comes from the Enable button here, never at launch or when a request arrives.
struct NotificationSettingsSection: View {
    @Environment(\.openURL) private var openURL
    @State private var optedIn = ApprovalNotifications.optedIn
    @State private var permission: ApprovalNotifications.Permission?
    @State private var working = false

    var body: some View {
        Section("Notifications") {
            Text("Grouper can notify this \(deviceWord) when a tool needs your review. Notifications show tool, device, thread and chat names, never the command or its contents. You must open Grouper to inspect and approve.")
                .font(.callout).foregroundStyle(Theme.text2)
            if permission == .denied {
                Text("Notifications for Grouper are turned off in system settings.").font(.callout)
                if let url = ApprovalNotifications.systemSettingsURL {
                    Button("Open notification settings") { openURL(url) }
                }
            } else if optedIn && permission == .allowed {
                LabeledContent("Approval notifications", value: "On")
                Button("Turn off approval notifications") {
                    ApprovalNotifications.optedIn = false
                    optedIn = false
                }
            } else {
                Button(working ? "Asking…" : "Enable approval notifications") { enable() }
                    .disabled(working || permission == nil)
            }
            // TODO(APNs): background delivery needs APNs and a paid Apple Developer account.
            Text("They arrive only while Grouper is running. Delivery while it is closed needs Apple push notifications, which aren’t set up yet.")
                .font(.caption).foregroundStyle(Theme.text2)
        }
        .task { permission = await ApprovalNotifications.permission() }
    }

    private var deviceWord: String {
        #if os(iOS)
        "iPhone"
        #else
        "Mac"
        #endif
    }

    private func enable() {
        working = true
        Task {
            var status = await ApprovalNotifications.permission()
            if status == .notDetermined {
                _ = await ApprovalNotifications.requestPermission()
                status = await ApprovalNotifications.permission()
            }
            permission = status
            if status == .allowed {
                ApprovalNotifications.optedIn = true
                optedIn = true
            }
            working = false
        }
    }
}

struct MoreView: View {
    var body: some View {
        NavigationStack {
            List {
                NavigationLink { SettingsView() } label: { Label("Settings", systemImage: "gear") }
                    .listRowBackground(Theme.surface)
                Section("Later milestones") {
                    Label("Models", systemImage: "cpu")
                    Label("Memory", systemImage: "brain")
                    Label("Skills", systemImage: "sparkles")
                }
                .foregroundStyle(Theme.text3)
                .listRowBackground(Theme.surface)
            }
            .grouperListBackground()
            .navigationTitle("More")
        }
    }
}
