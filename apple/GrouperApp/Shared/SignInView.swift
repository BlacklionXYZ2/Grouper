import SwiftUI

struct SignInView: View {
    @EnvironmentObject private var model: AppModel
    @State private var signingIn = false

    var body: some View {
        NavigationStack {
            Form {
                Section("Hub") {
                    TextField("https://grouper.example", text: $model.hubURL)
                        #if os(iOS)
                        .textContentType(.URL)
                        .textInputAutocapitalization(.never)
                        .keyboardType(.URL)
                        #endif
                    SecureField("Admin token", text: $model.adminToken)
                        .textContentType(.password)
                    #if os(macOS)
                    Text("The admin token stays in this app. When you connect, this Mac registers itself with the hub and gets its own non-admin device token for the local node.")
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
            #if os(macOS)
            .formStyle(.grouped)
            #endif
            .navigationTitle("Connect to Grouper")
        }
    }
}
