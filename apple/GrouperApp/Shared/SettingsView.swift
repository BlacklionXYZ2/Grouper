import GrouperAPI
import SwiftUI

struct SettingsView: View {
    @EnvironmentObject private var model: AppModel
    #if os(macOS)
    @EnvironmentObject private var node: MacNodeController
    @State private var savingDeviceToken = false
    #endif

    var body: some View {
        Form {
            Section("Connection") {
                LabeledContent("Hub", value: model.hubURL)
                LabeledContent("Status", value: model.online ? "Connected" : "Offline")
                Button("Check now") { Task { await model.connect(save: false) } }
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
                    Text("This Mac uses a device token you entered.")
                }
                if let id = model.deviceId, !model.deviceToken.isEmpty {
                    LabeledContent("Device id") { Text(id).monospaced().textSelection(.enabled) }
                }
                LabeledContent("Node", value: node.statusText)
                Button(model.registeringDevice ? "Registering…" : "Register again") {
                    Task { await model.registerThisMacAgain() }
                }
                .disabled(model.registeringDevice || model.client == nil)
                Text("Makes a new device token for this Mac and revokes the old one on the hub.")
                    .font(.caption).foregroundStyle(.secondary)
                if let message = model.deviceMessage {
                    Text(message).font(.caption).foregroundStyle(.red)
                }
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
                        .font(.caption).foregroundStyle(.secondary)
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
                Text("Signing out keeps this Mac's device token, so it can keep running threads. Removing this Mac also revokes its token on the hub.")
                    .font(.caption).foregroundStyle(.secondary)
                #endif
            }
        }
        #if os(macOS)
        .formStyle(.grouped)
        #endif
        .navigationTitle("Settings")
    }
}

/// Settings > Account (docs/accounts-design.md): the Google account that owns the hub, with
/// Unlink, or "Link your Google account" so the owner can sign in with Google instead of a token.
struct AccountSettingsSection: View {
    @EnvironmentObject private var model: AppModel
    @State private var confirmUnlink = false

    var body: some View {
        Section("Account") {
            if let account = model.linkedGoogle {
                LabeledContent("Google account") {
                    VStack(alignment: .trailing, spacing: 2) {
                        Text(account.email).textSelection(.enabled)
                        if !account.name.isEmpty {
                            Text(account.name).font(.caption).foregroundStyle(.secondary)
                        }
                    }
                }
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
                    .font(.callout).foregroundStyle(.secondary)
            } else if !model.googleLinkLoaded {
                HStack(spacing: 8) { ProgressView().controlSize(.small); Text("Checking the linked account…") }
            } else if GoogleSignIn.oauth == nil {
                Text("This app has no Google client ID, so Google sign-in is off.")
                    .font(.callout).foregroundStyle(.secondary)
            } else if model.authStatus?.google != true {
                Text("Google sign-in isn't turned on for this hub.")
                    .font(.callout).foregroundStyle(.secondary)
            } else {
                Button(model.googleBusy ? "Linking…" : "Link your Google account") {
                    Task { await model.linkGoogle() }
                }
                .disabled(model.googleBusy || model.client == nil)
                Text("Then you can sign in to this hub with Google instead of pasting a token.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if let message = model.accountMessage {
                Text(message).font(.caption).foregroundStyle(.red)
            }
        }
        .task(id: model.signedIn) { await model.refreshGoogleLink() }
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
                .font(.callout).foregroundStyle(.secondary)
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
                .font(.caption).foregroundStyle(.secondary)
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
                Section("Later milestones") {
                    Label("Models", systemImage: "cpu")
                    Label("Usage", systemImage: "chart.bar")
                    Label("Memory", systemImage: "brain")
                    Label("Skills", systemImage: "sparkles")
                }.foregroundStyle(.secondary)
            }.navigationTitle("More")
        }
    }
}
