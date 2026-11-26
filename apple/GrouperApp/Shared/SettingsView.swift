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
            #if os(macOS)
            Section("This Mac") {
                SecureField("Device token", text: $model.deviceToken)
                    .textContentType(.password)
                Button(savingDeviceToken ? "Verifying…" : "Save device token") {
                    savingDeviceToken = true
                    Task {
                        let saved = await model.saveDeviceToken()
                        savingDeviceToken = false
                        if saved {
                            node.stop()
                            node.start(hubURL: model.hubURL, token: model.deviceToken)
                        }
                    }
                }
                .disabled(savingDeviceToken || model.deviceToken.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                Button("Forget device token", role: .destructive) {
                    node.stop()
                    model.forgetDeviceToken()
                }
                if let error = model.errorMessage {
                    Text(error).font(.caption).foregroundStyle(.red)
                }
                Text("Forgetting removes the token from this Mac. The hub continues to accept it until that device is revoked there.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            #endif
            NotificationSettingsSection()
            Section { Button("Sign out on this device", role: .destructive) { model.signOut() } }
        }
        #if os(macOS)
        .formStyle(.grouped)
        #endif
        .navigationTitle("Settings")
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
