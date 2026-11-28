import AuthenticationServices
import Foundation
import GrouperAPI
#if os(iOS)
import UIKit
#else
import AppKit
#endif

/// Google sign-in for the apps (docs/accounts-design.md): this app's Google client ID and the
/// browser step. The person signs in on Google's page in `ASWebAuthenticationSession`; the
/// PKCE code comes back on the reversed-client-ID scheme and is exchanged for an ID token
/// carrying the hub's nonce. No Google SDK, no client secret.
enum GoogleSignIn {
    /// `GrouperGoogleClientID` from Info.plist, when it's a usable Google client ID. Nil hides
    /// Google sign-in. The Info.plist must also list `oauth.redirectScheme` in `CFBundleURLTypes`.
    static let oauth: GoogleOAuth? = {
        guard let id = Bundle.main.object(forInfoDictionaryKey: "GrouperGoogleClientID") as? String else { return nil }
        return GoogleOAuth(clientID: id)
    }()

    /// This device's own name (the phone's, or the Mac's, as the Mac's node registers under it), for
    /// the device row the hub makes for this app install. The hub lists one device per machine,
    /// so the app's sign-in and the Mac's node show as that one device. iOS 16 and later report
    /// just "iPhone" unless the app holds Apple's user-assigned-device-name entitlement.
    @MainActor static var deviceName: String {
        #if os(iOS)
        let name = UIDevice.current.name
        #else
        let name = MacNodeController.computerName() ?? "Mac"
        #endif
        return DeviceRegistration.name(name, fallback: "Grouper app")
    }

    /// Runs Google's sign-in page for `nonce` (from the hub) and returns the Google ID token.
    /// Throws `CancellationError` when the person closes the page.
    @MainActor
    static func idToken(oauth: GoogleOAuth, nonce: String) async throws -> String {
        let request = oauth.authorizationRequest(nonce: nonce)
        let callback = try await GoogleWebAuthSession().start(url: request.url, callbackScheme: oauth.redirectScheme)
        let code = try oauth.authorizationCode(from: callback, for: request)
        return try await oauth.exchange(code: code, codeVerifier: request.codeVerifier)
    }

    /// A plain-language message for a failed Google sign-in or link, or nil when the person cancelled.
    static func message(for error: Error) -> String? {
        if error is CancellationError { return nil }
        if (error as? GoogleOAuthError) == .google("access_denied") { return nil }
        switch error as? HubClientError {
        case .notFound?:
            return "This hub doesn't support Google sign-in, or it's turned off."
        case .unauthorized(let message)?:
            return "The hub didn't accept that Google sign-in: \(message)"
        case .forbidden(let message)?:
            return "The hub refused that Google account: \(message)"
        case .conflict(let message)?:
            return message
        default:
            return error.localizedDescription
        }
    }
}

/// One `ASWebAuthenticationSession`, kept alive (with its presentation anchor) until it ends.
@MainActor
private final class GoogleWebAuthSession: NSObject {
    private let anchor: ASPresentationAnchor
    private var session: ASWebAuthenticationSession?

    /// Throws when the app has no window to present over.
    init(presentingOver anchor: ASPresentationAnchor? = nil) throws {
        self.anchor = try anchor ?? Self.currentAnchor()
        super.init()
    }

    func start(url: URL, callbackScheme: String) async throws -> URL {
        defer { session = nil }
        return try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<URL, Error>) in
            // @Sendable: the session may call back off the main thread; this only resumes.
            let session = ASWebAuthenticationSession(url: url, callbackURLScheme: callbackScheme) { @Sendable callback, error in
                if let callback {
                    continuation.resume(returning: callback)
                } else if let error = error as? ASWebAuthenticationSessionError, error.code == .canceledLogin {
                    continuation.resume(throwing: CancellationError())
                } else {
                    continuation.resume(throwing: error ?? GoogleOAuthError.missingCode as Error)
                }
            }
            session.presentationContextProvider = self
            session.prefersEphemeralWebBrowserSession = false
            self.session = session
            if !session.start() {
                self.session = nil
                continuation.resume(throwing: GoogleOAuthError.transport("Couldn't open Google sign-in."))
            }
        }
    }

    private static func currentAnchor() throws -> ASPresentationAnchor {
        #if os(iOS)
        let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
        let scene = scenes.first(where: { $0.activationState == .foregroundActive }) ?? scenes.first
        guard let scene else { throw GoogleOAuthError.transport("Grouper has no window to show Google sign-in in.") }
        return scene.keyWindow ?? scene.windows.first ?? UIWindow(windowScene: scene)
        #else
        let app = NSApplication.shared
        return app.keyWindow ?? app.mainWindow ?? app.windows.first(where: { $0.isVisible }) ?? NSWindow()
        #endif
    }
}

// The SDK (Xcode 27) already declares this requirement main-actor isolated.
extension GoogleWebAuthSession: ASWebAuthenticationPresentationContextProviding {
    func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor { anchor }
}
