import GrouperAPI
import SwiftUI

struct SignInView: View {
    @EnvironmentObject private var model: AppModel
    @State private var signingIn = false
    @State private var setupCode = ""
    @State private var showTokenSignIn = false

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
                        .autocorrectionDisabled()
                }
                if model.googleSignInAvailable {
                    if model.authStatus?.claimed == false {
                        Section {
                            TextField("XXXX-XXXX-XXXX", text: $setupCode)
                                .monospaced()
                                .autocorrectionDisabled()
                                #if os(iOS)
                                .textInputAutocapitalization(.characters)
                                #endif
                                .accessibilityLabel("Setup code")
                            Text("This hub has no owner yet. Enter the setup code from the hub's log (or run grouper-hub setup-code on the hub), then sign in with Google to claim it.")
                                .font(.caption).foregroundStyle(.secondary)
                        } header: {
                            Text("Setup code")
                        }
                    }
                    Section {
                        GoogleSignInButton(busy: model.googleBusy) {
                            Task { await model.signInWithGoogle(setupCode: setupCode) }
                        }
                        .disabled(!canSignInWithGoogle)
                        .frame(maxWidth: .infinity)
                    }
                    Section {
                        DisclosureGroup("Use a token instead", isExpanded: $showTokenSignIn) { tokenSignIn }
                    }
                } else {
                    Section("Admin token") { tokenSignIn }
                }
                if let error = model.errorMessage {
                    Section { Text(error).foregroundStyle(.red).accessibilityLabel("Sign in failed. \(error)") }
                }
            }
            #if os(macOS)
            .formStyle(.grouped)
            #endif
            .navigationTitle("Connect to Grouper")
            // Ask the hub whether it is claimed and allows Google, once typing pauses.
            .task(id: model.hubURL) {
                try? await Task.sleep(for: .milliseconds(400))
                guard !Task.isCancelled else { return }
                await model.checkAuthStatus(for: model.hubURL)
            }
        }
    }

    private var busy: Bool { signingIn || model.googleBusy }

    private var canSignInWithGoogle: Bool {
        guard !busy, !model.hubURL.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return false }
        return model.authStatus?.claimed != false || SetupCode.normalized(setupCode) != nil
    }

    @ViewBuilder private var tokenSignIn: some View {
        SecureField("Admin token", text: $model.adminToken)
            .textContentType(.password)
        #if os(macOS)
        Text("The admin token stays in this app. When you connect, this Mac registers itself with the hub and gets its own non-admin device token for the local node.")
            .font(.caption).foregroundStyle(.secondary)
        #endif
        Button {
            signingIn = true
            Task { await model.connect(); signingIn = false }
        } label: {
            HStack { Spacer(); if signingIn { ProgressView() } else { Text("Connect") }; Spacer() }
        }
        .disabled(busy || model.hubURL.isEmpty || model.adminToken.isEmpty)
        .buttonStyle(.glassProminent)
        .controlSize(.large)
    }
}

/// Google's "Sign in with Google" button: white, the four-colour "G", Google's wording
/// (developers.google.com/identity/branding-guidelines).
struct GoogleSignInButton: View {
    var busy = false
    let action: () -> Void
    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        Button(action: action) {
            HStack(spacing: 10) {
                if busy {
                    ProgressView().controlSize(.small).frame(width: 18, height: 18)
                } else {
                    GoogleGMark().frame(width: 18, height: 18)
                }
                Text("Sign in with Google")
                    .font(.system(size: 14, weight: .medium))
                    .foregroundStyle(Color(red: 0x1F / 255, green: 0x1F / 255, blue: 0x1F / 255))
            }
            .padding(.horizontal, 12)
            .frame(minHeight: 40)
            .background(Color.white, in: Capsule())
            .overlay(Capsule().strokeBorder(Color(red: 0x74 / 255, green: 0x77 / 255, blue: 0x75 / 255), lineWidth: 1))
            .opacity(isEnabled ? 1 : 0.5)
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .environment(\.colorScheme, .light)
        .accessibilityLabel("Sign in with Google")
    }
}

/// Google's "G" drawn as four arcs and a bar. TODO(owner): swap for Google's official
/// "G" asset from the branding guidelines once the app has an asset catalogue.
struct GoogleGMark: View {
    private static let blue = Color(red: 0x42 / 255, green: 0x85 / 255, blue: 0xF4 / 255)
    private static let green = Color(red: 0x34 / 255, green: 0xA8 / 255, blue: 0x53 / 255)
    private static let yellow = Color(red: 0xFB / 255, green: 0xBC / 255, blue: 0x05 / 255)
    private static let red = Color(red: 0xEA / 255, green: 0x43 / 255, blue: 0x35 / 255)

    var body: some View {
        GeometryReader { proxy in
            let size = min(proxy.size.width, proxy.size.height)
            let line = size * 0.2
            ZStack {
                // Trim runs clockwise from three o'clock; the gap is at the upper right.
                arc(0.0, 0.125, Self.blue, line)
                arc(0.125, 0.40, Self.green, line)
                arc(0.40, 0.60, Self.yellow, line)
                arc(0.60, 0.875, Self.red, line)
                Rectangle()
                    .fill(Self.blue)
                    .frame(width: size / 2, height: line)
                    .offset(x: size / 4 - line / 4)
            }
            .frame(width: size, height: size)
        }
        .aspectRatio(1, contentMode: .fit)
        .accessibilityHidden(true)
    }

    private func arc(_ from: CGFloat, _ to: CGFloat, _ color: Color, _ line: CGFloat) -> some View {
        Circle()
            .inset(by: line / 2)
            .trim(from: from, to: to)
            .stroke(color, style: StrokeStyle(lineWidth: line, lineCap: .butt))
    }
}
