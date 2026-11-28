import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

// The relay (docs/relay-design.md, "Clients"): phones and Macs reach the hub through
// `https://grouper.clockmillbarn.uk`, which asks for its own sign-in first. A client signs in to
// the relay (Google, with a relay-specific nonce), then sends `Grouper-Relay: <relay_token>` on
// every request and WebSocket handshake to that address, then signs in to the hub as before.

/// The request header that carries the relay token.
public enum RelayHeader {
    public static let name = "Grouper-Relay"
}

/// Nonces for the two sign-ins. A relay nonce goes to Google hashed, and a hub nonce goes to
/// Google as the hub sent it, but only when it has the one shape a hub makes, so a hub can never
/// hand the app a nonce that is really a relay one.
public enum RelayNonce {
    /// What the relay expects in the Google ID token's `nonce` claim for `relayNonce`:
    /// `b64url_nopad(SHA-256("grouper-relay-v1:" + relayNonce))`.
    public static func hashed(_ relayNonce: String) -> String {
        GoogleOAuth.base64URL(SHA256Digest.hash(Array(("grouper-relay-v1:" + relayNonce).utf8)))
    }

    /// True for a nonce a hub makes: exactly 32 lowercase hex characters (`^[0-9a-f]{32}$`).
    public static func isHubNonce(_ nonce: String) -> Bool {
        let bytes = Array(nonce.utf8)
        return bytes.count == 32 && bytes.allSatisfy { ($0 >= 0x30 && $0 <= 0x39) || ($0 >= 0x61 && $0 <= 0x66) }
    }
}

/// `GET /relay/v1/me`: the relay's answer. `signedIn` is false without a (valid) relay token.
public struct RelayStatus: Codable, Equatable, Sendable {
    public let signedIn: Bool
    public let email: String?
    /// The account's hub is connected to the relay right now.
    public let hubConnected: Bool?
    public let hubId: String?

    public init(signedIn: Bool, email: String? = nil, hubConnected: Bool? = nil, hubId: String? = nil) {
        self.signedIn = signedIn
        self.email = email
        self.hubConnected = hubConnected
        self.hubId = hubId
    }
}

/// `POST /relay/v1/nonce`.
public struct RelayNonceResponse: Codable, Equatable, Sendable {
    public let nonce: String
}

/// `POST /relay/v1/sign-in` for an app. Keep `relayToken` in the Keychain, never in logs.
public struct RelaySignInResult: Codable, Equatable, Sendable {
    public let relayToken: String
    public let email: String?
    public let hubConnected: Bool?
    /// Unix seconds.
    public let expires: Int64?
}

private struct RelaySignInRequest: Codable { let idToken: String; let relayNonce: String; let client: String }
private struct RelayEmpty: Codable {}

/// Talks to a relay's own API (`/relay/v1/...`). Not for the hub's API: use `HubClient`, with
/// `relayToken` set, for that.
public actor RelayClient {
    public nonisolated let baseURL: URL
    private nonisolated let relayToken: String?
    private let session: URLSession
    private let encoder = JSONEncoder.grouper
    private let decoder = JSONDecoder.grouper

    public init(baseURL: URL, relayToken: String? = nil, session: URLSession = .shared) throws {
        guard let scheme = baseURL.scheme?.lowercased(), scheme == "http" || scheme == "https", baseURL.host != nil else {
            throw HubClientError.invalidBaseURL
        }
        self.baseURL = baseURL
        self.relayToken = relayToken.flatMap { $0.isEmpty ? nil : $0 }
        self.session = session
    }

    public init(baseURL: String, relayToken: String? = nil, session: URLSession = .shared) throws {
        guard let url = URL(string: baseURL.trimmingCharacters(in: .whitespacesAndNewlines)) else { throw HubClientError.invalidBaseURL }
        try self.init(baseURL: url, relayToken: relayToken, session: session)
    }

    /// Is this address a relay? `GET /relay/v1/me` answers JSON with a `signed_in` field, which
    /// no hub does. Nil when the address isn't a relay (a LAN hub, localhost, Tailscale). Throws
    /// when nothing answers, so callers can tell "not a relay" from "can't reach it".
    public func status() async throws -> RelayStatus? {
        let (data, response) = try await perform(path: ["relay", "v1", "me"], method: "GET", body: nil)
        guard (200..<300).contains(response.statusCode),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              object["signed_in"] is Bool else { return nil }
        return try? decoder.decode(RelayStatus.self, from: data)
    }

    /// `POST /relay/v1/nonce`: a fresh relay nonce. Hash it with `RelayNonce.hashed` for Google.
    public func nonce() async throws -> RelayNonceResponse {
        try await send(["relay", "v1", "nonce"], body: RelayEmpty())
    }

    /// `POST /relay/v1/sign-in` with `client: "app"`: trades the Google ID token (whose nonce is
    /// the hash of `relayNonce`) for a relay token.
    public func signIn(idToken: String, relayNonce: String) async throws -> RelaySignInResult {
        try await send(["relay", "v1", "sign-in"], body: RelaySignInRequest(idToken: idToken, relayNonce: relayNonce, client: "app"))
    }

    /// `POST /relay/v1/sign-out`: ends this relay token's session.
    public func signOut(everywhere: Bool = false) async throws {
        struct Body: Codable { let everywhere: Bool }
        let _: JSONValue = try await send(["relay", "v1", "sign-out"], body: Body(everywhere: everywhere))
    }

    private func send<T: Decodable, Body: Encodable>(_ path: [String], body: Body) async throws -> T {
        let encoded: Data
        do { encoded = try encoder.encode(body) }
        catch { throw HubClientError.decoding(String(describing: error)) }
        let (data, response) = try await perform(path: path, method: "POST", body: encoded)
        guard (200..<300).contains(response.statusCode) else { throw HubClientError.map(status: response.statusCode, data: data) }
        do { return try decoder.decode(T.self, from: data) }
        catch { throw HubClientError.decoding(String(describing: error)) }
    }

    private func perform(path: [String], method: String, body: Data?) async throws -> (Data, HTTPURLResponse) {
        var url = baseURL
        for component in path { url.appendPathComponent(component) }
        var request = URLRequest(url: url)
        request.httpMethod = method
        request.timeoutInterval = 15
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if let relayToken { request.setValue(relayToken, forHTTPHeaderField: RelayHeader.name) }
        if let body {
            request.httpBody = body
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        let data: Data
        let response: URLResponse
        do { (data, response) = try await session.data(for: request) }
        catch { throw HubClientError.transport(String(describing: error)) }
        guard let http = response as? HTTPURLResponse else { throw HubClientError.transport("The relay did not return HTTP.") }
        return (data, http)
    }
}

extension HubClientError {
    /// The error for a non-2xx answer. A body with `"relay": true` is the relay speaking, not
    /// the hub: 401 means the relay sign-in is missing or expired, 503 that the hub is offline.
    static func map(status: Int, data: Data) -> HubClientError {
        struct Envelope: Decodable { let error: String?; let relay: Bool? }
        let envelope = try? JSONDecoder.grouper.decode(Envelope.self, from: data)
        let message = envelope?.error ?? String(decoding: data, as: UTF8.self)
        if envelope?.relay == true {
            if status == 401 { return .relayUnauthorized(message) }
            if status == 503 { return .hubOffline(message) }
        }
        switch status {
        case 400: return .badRequest(message)
        case 401: return .unauthorized(message)
        case 403: return .forbidden(message)
        case 404: return .notFound(message)
        case 409: return .conflict(message)
        default: return .server(status: status, message: message)
        }
    }

    /// Plain words for the person, when this is the relay speaking; nil for anything else.
    public var relayProblem: String? {
        switch self {
        case .relayUnauthorized:
            return "Your sign-in to the Grouper relay expired. Sign in again."
        case .hubOffline:
            return "Your hub is offline. Check that Grouper is running on the computer that hosts it."
        default:
            return nil
        }
    }
}
