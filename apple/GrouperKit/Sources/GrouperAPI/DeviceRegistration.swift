import Foundation

/// `POST /v1/devices` (admin): a new plain (non-admin) device and its token. The hub
/// shows the token only in this response; keep it in the Keychain, never in logs.
public struct RegisteredDevice: Codable, Equatable, Sendable {
    public let device: DeviceInfo
    public let token: String

    public init(device: DeviceInfo, token: String) {
        self.device = device
        self.token = token
    }
}

/// `DELETE /v1/devices/{id}` (admin).
public struct DeviceRevocation: Codable, Equatable, Sendable {
    public let revoked: String
}

/// Device registration helpers that don't need a network.
public enum DeviceRegistration {
    /// The hub's limit on a device name.
    public static let maxNameLength = 64

    /// A name the hub accepts: trimmed, no control characters, at most 64 characters, and
    /// not "hub" (reserved for the hub itself). Falls back to `fallback` when nothing is left.
    public static func name(_ raw: String?, fallback: String = "Mac") -> String {
        let cleaned = String((raw ?? "").unicodeScalars.filter { !CharacterSet.controlCharacters.contains($0) })
            .trimmingCharacters(in: .whitespacesAndNewlines)
        let clipped = String(cleaned.prefix(maxNameLength)).trimmingCharacters(in: .whitespacesAndNewlines)
        if clipped.isEmpty || clipped.lowercased() == "hub" { return fallback }
        return clipped
    }

    /// The device id a token belongs to: tokens are `<device id>.<secret>`.
    public static func deviceId(ofToken token: String) -> String? {
        let trimmed = token.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let dot = trimmed.firstIndex(of: "."), dot != trimmed.startIndex,
              trimmed.index(after: dot) != trimmed.endIndex else { return nil }
        return String(trimmed[..<dot])
    }

    /// Whether two hub URLs as typed name the same hub (ignoring case, spaces and trailing slashes),
    /// so a saved device token is kept for it. Empty never matches.
    public static func sameHub(_ a: String, _ b: String) -> Bool {
        func normal(_ url: String) -> String {
            var text = url.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
            while text.hasSuffix("/") { text.removeLast() }
            return text
        }
        let left = normal(a)
        return !left.isEmpty && left == normal(b)
    }

    /// True when the hub predates `POST /v1/devices`: an unknown route (404) or a route
    /// that only allows GET (405).
    public static func hubLacksRegistration(_ error: Error) -> Bool {
        switch error as? HubClientError {
        case .notFound?: return true
        case .server(let status, _)?: return status == 405
        default: return false
        }
    }
}
