import Foundation

/// `GET /v1/auth/status` (no token): whether the hub has an owner and which Google sign-ins it allows.
public struct AuthStatus: Codable, Equatable, Sendable {
    /// The hub has an owner. An unclaimed hub needs a setup code with the first Google sign-in.
    public let claimed: Bool
    /// App Google sign-in is on (the hub trusts at least one app client ID).
    public let google: Bool
    /// Browser Google sign-in is on.
    public let googleWeb: Bool?

    public init(claimed: Bool, google: Bool, googleWeb: Bool? = nil) {
        self.claimed = claimed
        self.google = google
        self.googleWeb = googleWeb
    }
}

/// `POST /v1/auth/google/nonce` (no token): a one-time nonce for one Google sign-in, good for 10 minutes.
public struct GoogleNonce: Codable, Equatable, Sendable {
    public let nonce: String
    /// Unix seconds.
    public let expires: Int64
}

/// Which app is signing in; the hub names the new device's platform after it.
public enum GoogleSignInClient: String, Codable, Sendable {
    case ios
    case macos
    case cli
}

/// `POST /v1/auth/google` for an app: a new admin token for this app install, shown only
/// once. Keep it in the Keychain, never in logs.
public struct GoogleSignInResult: Codable, Equatable, Sendable {
    public struct Device: Codable, Equatable, Sendable {
        public let id: String
        public let name: String
        public let platform: String
        public let admin: Bool
    }

    public let token: String
    public let device: Device
}

/// The Google account that owns a hub (`GET|POST /v1/auth/google/link`, admin).
public struct LinkedGoogleAccount: Codable, Equatable, Sendable {
    public let provider: String
    public let email: String
    public let name: String
    /// Unix seconds.
    public let claimedAt: Int64

    public init(provider: String = "google", email: String, name: String, claimedAt: Int64) {
        self.provider = provider
        self.email = email
        self.name = name
        self.claimedAt = claimedAt
    }
}

/// `{"owner": {...} | null}`.
struct GoogleLinkResponse: Codable, Sendable { let owner: LinkedGoogleAccount? }
/// `{"unlinked": bool}`.
struct GoogleUnlinkResponse: Codable, Sendable { let unlinked: Bool }
struct GoogleSignInRequest: Codable, Sendable {
    let idToken: String
    let setupCode: String?
    let client: GoogleSignInClient
    let deviceName: String?
    /// This app install's id (`InstallID`); the hub reuses the install's device when it has one.
    let installId: String?
}
struct GoogleLinkRequest: Codable, Sendable { let idToken: String }

/// A hub's one-time setup code, as the hub logs it: three groups of four, e.g. `K7PM-3XQR-T9WD`.
public enum SetupCode {
    /// The characters a setup code uses (no 0/O, 1/I/L).
    public static let alphabet = Set("ABCDEFGHJKMNPQRSTUVWXYZ23456789")

    /// The code in the hub's own form (`XXXX-XXXX-XXXX`), ignoring case, spaces and dashes,
    /// or nil when it can't be a setup code.
    public static func normalized(_ raw: String) -> String? {
        let chars = raw.uppercased().filter { $0 != "-" && !$0.isWhitespace }
        guard chars.count == 12, chars.allSatisfy({ alphabet.contains($0) }) else { return nil }
        let array = Array(chars)
        return [array[0..<4], array[4..<8], array[8..<12]].map { String($0) }.joined(separator: "-")
    }
}

extension GoogleSignInClient {
    /// The app running this code: `macos` on a Mac, `ios` otherwise.
    public static var current: GoogleSignInClient {
        #if os(macOS)
        return .macos
        #else
        return .ios
        #endif
    }
}
