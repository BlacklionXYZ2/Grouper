import GrouperAPI
import SwiftUI

/// What the sign-in screen offers for the hub address as typed (docs/accounts-design.md,
/// "Google is the only way you sign in").
private enum SignInMode: Equatable {
    /// No address yet.
    case enterHub
    /// Asking the hub whether it allows Google sign-in.
    case checking
    /// No hub answered at that address.
    case unreachable
    /// The hub allows Google sign-in and the app has a client ID: the only visible way in.
    case google
    /// The hub has no Google sign-in (or the app has no client ID): the admin-token field, as before.
    case token
}

struct SignInView: View {
    @EnvironmentObject private var model: AppModel
    @State private var signingIn = false
    @State private var setupCode = ""
    /// Recovery only: the toolbar's ⋯ menu shows the token field under Google sign-in.
    @State private var showRecovery = false
    /// A hub address was saved when this screen opened, so the field starts collapsed.
    @State private var hubWasSaved = !(UserDefaults.standard.string(forKey: "grouper.hubURL") ?? "").isEmpty
    @State private var editingHub = false

    var body: some View {
        NavigationStack {
            Form {
                Section { brandHeader }
                    .listRowBackground(Color.clear)
                // First run: the address is the first thing to type. Once one is saved it's
                // pre-filled and tucked under "Hub" below (hub discovery will replace it).
                if !hubWasSaved {
                    Section("Hub") { hubField }
                }
                switch mode {
                case .enterHub:
                    Section {
                        Text("Enter your hub's address, then sign in with Google.")
                            .font(.callout).foregroundStyle(Theme.text2)
                    }
                case .checking:
                    Section {
                        HStack(spacing: 8) { ProgressView().controlSize(.small); Text("Checking the hub…") }
                            .foregroundStyle(Theme.text2)
                    }
                case .unreachable:
                    Section {
                        Text("Couldn't reach a Grouper hub at this address. Check it, and that the hub is running.")
                            .font(.callout).foregroundStyle(Theme.text2)
                        Button("Try again") { Task { await model.checkAuthStatus(for: model.hubURL) } }
                    }
                case .google:
                    googleSignIn
                case .token:
                    Section("Admin token") { tokenSignIn }
                }
                if hubWasSaved {
                    Section {
                        DisclosureGroup(isExpanded: $editingHub) {
                            hubField
                        } label: {
                            LabeledContent("Hub") {
                                Text(hubHost).lineLimit(1).truncationMode(.middle)
                            }
                        }
                    }
                }
                if showRecovery, mode != .token {
                    Section {
                        tokenSignIn
                    } header: {
                        Text("Recovery token")
                    } footer: {
                        Text("Only for getting back in when Google sign-in isn't working: an admin token made on the hub.")
                    }
                }
                if let error = model.errorMessage {
                    Section { Text(error).foregroundStyle(Theme.danger).accessibilityLabel("Sign in failed. \(error)") }
                }
            }
            #if os(macOS)
            .formStyle(.grouped)
            #endif
            .grouperListBackground()
            .navigationTitle("Connect to Grouper")
            // A saved hub that's gone, or an emptied address: open the field so it can be fixed.
            .onChange(of: mode, initial: true) { _, newMode in
                if hubWasSaved, newMode == .unreachable || newMode == .enterHub { editingHub = true }
            }
            .toolbar {
                // Token sign-in is a recovery route, kept off the main screen (owner decision).
                if mode != .token {
                    ToolbarItem(placement: .primaryAction) {
                        Menu {
                            Button(showRecovery ? "Hide recovery token" : "Sign in with a recovery token", systemImage: "key") {
                                showRecovery.toggle()
                            }
                        } label: {
                            Label("More", systemImage: "ellipsis.circle")
                        }
                        .accessibilityLabel("More sign-in options")
                    }
                }
            }
            // Ask the hub whether it is claimed and allows Google, once typing pauses.
            .task(id: model.hubURL) {
                try? await Task.sleep(for: .milliseconds(400))
                guard !Task.isCancelled else { return }
                await model.checkAuthStatus(for: model.hubURL)
            }
        }
    }

    /// The brand on the icon's gradient, with the tagline (Q2: sign-in only).
    private var brandHeader: some View {
        HStack(spacing: 14) {
            GrouperAvatar(size: 56)
            VStack(alignment: .leading, spacing: 2) {
                Text("Grouper").font(.largeTitle.weight(.bold)).foregroundStyle(Theme.text)
                Text("Together stronger.").font(.title3).foregroundStyle(Theme.text2)
            }
        }
        .padding(.vertical, 8)
        .accessibilityElement(children: .combine)
    }

    private var hubField: some View {
        TextField("https://grouper.example", text: $model.hubURL)
            #if os(iOS)
            .textContentType(.URL)
            .textInputAutocapitalization(.never)
            .keyboardType(.URL)
            #endif
            .autocorrectionDisabled()
            .accessibilityLabel("Hub address")
    }

    /// The saved hub as a short label: its host, or the address as typed.
    private var hubHost: String {
        let typed = model.hubURL.trimmingCharacters(in: .whitespacesAndNewlines)
        return URL(string: typed)?.host() ?? typed
    }

    /// The setup code while the hub is unclaimed, and "Sign in with Google".
    @ViewBuilder private var googleSignIn: some View {
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
                    .font(.caption).foregroundStyle(Theme.text2)
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
    }

    private var mode: SignInMode {
        // Without a client ID this app can't sign in with Google at all: tokens, as before.
        guard GoogleSignIn.oauth != nil else { return .token }
        let hub = model.hubURL.trimmingCharacters(in: .whitespacesAndNewlines)
        if hub.isEmpty { return .enterHub }
        if model.authStatusCheckedFor != hub { return .checking }
        if model.hubUnreachable { return .unreachable }
        return model.googleSignInAvailable ? .google : .token
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
            .font(.caption).foregroundStyle(Theme.text2)
        #endif
        Button {
            signingIn = true
            Task { await model.connect(); signingIn = false }
        } label: {
            HStack { Spacer(); if signingIn { ProgressView() } else { Text("Connect") }; Spacer() }
        }
        .disabled(busy || model.hubURL.isEmpty || model.adminToken.isEmpty)
        .buttonStyle(GrouperActionButtonStyle(kind: .primary))
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
