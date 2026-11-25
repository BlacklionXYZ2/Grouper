import SwiftUI

struct SignInView: View {
    @EnvironmentObject private var model: AppModel
    @State private var signingIn = false

    var body: some View {
        NavigationStack {
            Form {
                Section("Hub") {
                    TextField("https://grouper.example", text: $model.hubURL)
                        .textContentType(.URL)
                        #if os(iOS)
                        .textInputAutocapitalization(.never)
                        .keyboardType(.URL)
                        #endif
                    SecureField("Admin token", text: $model.adminToken)
                        .textContentType(.password)
                    #if os(macOS)
                    SecureField("Device token for this Mac", text: $model.deviceToken)
                        .textContentType(.password)
                    Text("The admin token powers the window. The separate non-admin device token is passed only to the local Rust node.")
                        .font(.caption).foregroundStyle(.secondary)
                    #endif
                }
                if let error = model.errorMessage {
                    Section { Text(error).foregroundStyle(.red).accessibilityLabel("Sign in failed. \(error)") }
                }
                Section {
                    Button {
                        signingIn = true
                        Task { await model.connect(); signingIn = false }
                    } label: {
                        HStack { Spacer(); if signingIn { ProgressView() } else { Text("Connect") }; Spacer() }
                    }
                    .disabled(signingIn || model.hubURL.isEmpty || model.adminToken.isEmpty)
                    .buttonStyle(.glassProminent)
                    .controlSize(.large)
                }
            }
            .navigationTitle("Connect to Grouper")
        }
    }
}
