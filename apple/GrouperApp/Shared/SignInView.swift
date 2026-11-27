import GrouperAPI
import SwiftUI

/// What the sign-in screen shows for the configured hub (docs/local-first-design.md).
/// The screen never shows a hub URL field or an admin token field by default.
private enum SignInState: Equatable {
    /// Asking the hub whether it is claimed and allows Google sign-in.
    case checking
    /// No hub answered at the configured address.
    case unreachable
    /// The hub allows Google sign-in and the app has a client ID: the one visible way in.
    case google
    /// The hub answered, but Google sign-in isn't available on this hub or build.
    case unavailable
}

struct SignInView: View {
    @EnvironmentObject private var model: AppModel
    @State private var signingIn = false
    @State private var setupCode = ""
    /// Recovery only: the toolbar's ⋯ menu shows the token field.
    @State private var showRecovery = false
    /// Opens the hub address editor when the hub is unreachable, owned by another account,
    /// or reached via the ⋯ menu.
    @State private var editingHubAddress = false

    var body: some View {
        NavigationStack {
            Form {
                Section { brandHeader }
                    .listRowBackground(Color.clear)
                switch state {
                case .checking:
                    Section {
                        HStack(spacing: 8) {
                            ProgressView().controlSize(.small)
                            Text("Checking your Grouper hub…")
                        }
                        .foregroundStyle(Theme.text2)
                    }
                case .unreachable:
                    unreachableSection
                case .google:
                    googleSignIn
                case .unavailable:
                    Section {
                        Text(unavailableMessage)
                            .font(.callout)
                            .foregroundStyle(Theme.text2)
                        Button("Change hub address") { editingHubAddress = true }
                    }
                }
                if showRecovery {
                    Section {
                        tokenSignIn
                    } header: {
                        Text("Recovery token")
                    } footer: {
                        Text("Only for getting back in when Google sign-in isn't working: an admin token made on the hub.")
                    }
                }
                if let error = model.errorMessage, state != .unreachable || showRecovery {
                    Section {
                        Text(error)
                            .foregroundStyle(Theme.danger)
                            .accessibilityLabel("Sign in failed. \(error)")
                    }
                }
            }
            #if os(macOS)
            .formStyle(.grouped)
            #endif
            .grouperListBackground()
            .navigationTitle("Sign in")
            .toolbar {
                // Recovery token sign-in and hub address override stay off the main screen.
                ToolbarItem(placement: .primaryAction) {
                    Menu {
                        Button(showRecovery ? "Hide recovery token" : "Sign in with a recovery token", systemImage: "key") {
                            showRecovery.toggle()
                        }
                        Button("Change hub address…", systemImage: "network") {
                            editingHubAddress = true
                        }
                    } label: {
                        Label("More", systemImage: "ellipsis.circle")
                    }
                    .accessibilityLabel("More sign-in options")
                }
            }
            .sheet(isPresented: $editingHubAddress) {
                HubAddressEditorSheet()
                    .environmentObject(model)
            }
            .task(id: model.hubURL) {
                await model.checkAuthStatus()
            }
            #if os(iOS)
            // Phone waiting screen: recheck the hub every few seconds while open so it continues
            // as soon as a computer finishes setting up the hub.
            .task(id: unreachablePollKey) {
                guard state == .unreachable else { return }
                while !Task.isCancelled {
                    try? await Task.sleep(for: .seconds(5))
                    guard !Task.isCancelled, !model.signedIn, !editingHubAddress else { continue }
                    await model.checkAuthStatus(force: true)
                    if !model.hubUnreachable { break }
                }
            }
            #endif
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

    @ViewBuilder private var unreachableSection: some View {
        Section {
            Text("Can't reach your Grouper hub")
                .font(.headline)
                .foregroundStyle(Theme.text)
            #if os(iOS)
            Text("Grouper runs on one of your computers. Install it on a Mac or PC and sign in with your Google account, or check that your hub is online.")
                .font(.callout)
                .foregroundStyle(Theme.text2)
            HStack(spacing: 8) {
                ProgressView().controlSize(.small)
                Text("Checking again automatically…")
            }
            .font(.caption)
            .foregroundStyle(Theme.text3)
            #else
            Text("Check that your hub is running and reachable at \(HubAddress.displayHost(model.hubURL)).")
                .font(.callout)
                .foregroundStyle(Theme.text2)
            #endif
            Button("Try again") {
                Task { await model.checkAuthStatus(force: true) }
            }
            Button("Change hub address") {
                editingHubAddress = true
            }
        }
    }

    /// The setup code only when the hub reports it's unclaimed, any different-account refusal,
    /// and the single "Sign in with Google" button.
    @ViewBuilder private var googleSignIn: some View {
        if model.refusedDifferentAccount {
            Section {
                Label(
                    "The hub at \(HubAddress.displayHost(model.hubURL)) belongs to a different Google account.",
                    systemImage: "person.crop.circle.badge.xmark"
                )
                .font(.callout)
                .foregroundStyle(Theme.danger)
                Text("Sign in with that Google account, or change the hub address.")
                    .font(.caption)
                    .foregroundStyle(Theme.text2)
                Button("Change hub address") {
                    editingHubAddress = true
                }
            }
        }
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

    private var state: SignInState {
        if model.authStatusCheckedFor != model.hubURL { return .checking }
        if model.hubUnreachable { return .unreachable }
        return model.googleSignInAvailable ? .google : .unavailable
    }

    #if os(iOS)
    private var unreachablePollKey: String {
        "\(model.hubURL)|\(state == .unreachable)"
    }
    #endif

    private var unavailableMessage: String {
        if GoogleSignIn.oauth == nil {
            return "Google sign-in isn't configured in this build."
        }
        return "Google sign-in isn't turned on for the hub at \(HubAddress.displayHost(model.hubURL))."
    }

    private var busy: Bool { signingIn || model.googleBusy }

    private var canSignInWithGoogle: Bool {
        guard !busy else { return false }
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
        .disabled(busy || model.adminToken.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
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
