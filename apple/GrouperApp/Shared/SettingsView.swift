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
            Section { Button("Sign out on this device", role: .destructive) { model.signOut() } }
        }
        #if os(macOS)
        .formStyle(.grouped)
        #endif
        .navigationTitle("Settings")
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
