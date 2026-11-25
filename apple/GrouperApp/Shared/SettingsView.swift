import SwiftUI

struct SettingsView: View {
    @EnvironmentObject private var model: AppModel
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
                Button("Save device token") { if !model.deviceToken.isEmpty { try? KeychainStore.save(model.deviceToken, as: .device) } }
                Button("Forget device token", role: .destructive) { model.forgetDeviceToken() }
                Text("Forgetting removes the token from this Mac. The hub continues to accept it until that device is revoked there.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            #endif
            Section { Button("Sign out on this device", role: .destructive) { model.signOut() } }
        }.navigationTitle("Settings")
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
