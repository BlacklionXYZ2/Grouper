import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import XCTest
@testable import GrouperAPI

final class RelayTests: XCTestCase {
    // MARK: Nonces

    func testRelayNonceIsHashedWithTheRelayPrefix() {
        // b64url_nopad(SHA-256("grouper-relay-v1:" + nonce)), checked against another implementation.
        XCTAssertEqual(RelayNonce.hashed("abc123"), "rK6W1KJyV8s4o2KPIptTJTWQ4WRNvAnYtgx253mioOA")
        XCTAssertEqual(RelayNonce.hashed("0123456789abcdef0123456789abcdef"), "d_WInGKG39AMJr3NoxsbClS0sT5HHnvoZzYF56LscBw")
        XCTAssertFalse(RelayNonce.hashed("abc123").contains("="))
        XCTAssertFalse(RelayNonce.hashed("abc123").contains("+"))
        XCTAssertFalse(RelayNonce.hashed("abc123").contains("/"))
    }

    func testOnlyAHubShapedNonceGoesToGoogleAsIs() {
        XCTAssertTrue(RelayNonce.isHubNonce("0123456789abcdef0123456789abcdef"))
        XCTAssertFalse(RelayNonce.isHubNonce(""))
        XCTAssertFalse(RelayNonce.isHubNonce("0123456789ABCDEF0123456789ABCDEF"), "uppercase is refused")
        XCTAssertFalse(RelayNonce.isHubNonce("0123456789abcdef0123456789abcde"), "31 characters")
        XCTAssertFalse(RelayNonce.isHubNonce("0123456789abcdef0123456789abcdef0"), "33 characters")
        XCTAssertFalse(RelayNonce.isHubNonce("0123456789abcdef0123456789abcdeg"))
        XCTAssertFalse(RelayNonce.isHubNonce("0123456789abcdef0123456789abcde\n"))
        // What a relay nonce hashes to is base64url, never 32 hex characters.
        XCTAssertFalse(RelayNonce.isHubNonce(RelayNonce.hashed("abc123")))
    }

    func testHubNonceShapeIsEnforcedWhereTheNonceIsFetched() async throws {
        let session = RelayStub.session { request in
            XCTAssertEqual(request.url?.path, "/v1/auth/google/nonce")
            return (200, #"{"nonce":"relay-looking-nonce_AbC","expires":1795000000}"#)
        }
        let hub = try HubClient(baseURL: "https://hub.test", session: session)
        do {
            _ = try await hub.googleNonce()
            XCTFail("a nonce that isn't 32 hex characters must be refused")
        } catch {
            guard case .badRequest? = error as? HubClientError else { return XCTFail("\(error)") }
        }
    }

    // MARK: Header injection

    func testHubClientSendsTheRelayHeaderOnEveryCall() async throws {
        let session = RelayStub.session { request in
            switch request.url?.path {
            case "/v1/auth/status": return (200, #"{"claimed":true,"google":true}"#)
            case "/v1/conversations": return (200, "[]")
            default: return (404, #"{"error":"no"}"#)
            }
        }
        let hub = try HubClient(baseURL: "https://relay.test", token: "dev.secret", relayToken: "rt_abc", session: session)
        _ = try await hub.authStatus() // a signed-out call: still goes through the relay
        _ = try await hub.conversations()
        XCTAssertEqual(RelayStub.requests.map(\.relay), ["rt_abc", "rt_abc"])
        XCTAssertEqual(RelayStub.requests.map(\.authorization), [nil, "Bearer dev.secret"])
    }

    func testNoRelayHeaderForAPlainHub() async throws {
        let session = RelayStub.session { _ in (200, "[]") }
        let hub = try HubClient(baseURL: "http://192.168.1.5:8080", token: "dev.secret", session: session)
        _ = try await hub.conversations()
        let blank = try HubClient(baseURL: "http://192.168.1.5:8080", token: "dev.secret", relayToken: "", session: session)
        _ = try await blank.conversations()
        XCTAssertEqual(RelayStub.requests.map(\.relay), [nil, nil])
    }

    func testMoveClientSendsTheRelayHeader() async throws {
        let session = RelayStub.session { _ in (200, #"{"hub_id":"h","account":"a@example.com","hub_device_id":"d","epoch":1}"#) }
        let client = try HubMoveClient(baseURL: "https://relay.test", token: "dev.secret", relayToken: "rt_abc", session: session)
        _ = try? await client.hubIdentity()
        XCTAssertEqual(RelayStub.requests.first?.relay, "rt_abc")
    }

    func testEventStreamHandshakeCarriesBothTokens() throws {
        let url = try XCTUnwrap(URL(string: "https://relay.test"))
        let relayed = EventStream(baseURL: url, token: "dev.secret", relayToken: "rt_abc")
        let request = relayed.handshakeRequest(url: try XCTUnwrap(URL(string: "wss://relay.test/v1/events")))
        XCTAssertEqual(request.value(forHTTPHeaderField: "Authorization"), "Bearer dev.secret")
        XCTAssertEqual(request.value(forHTTPHeaderField: "Grouper-Relay"), "rt_abc")

        let plain = EventStream(baseURL: url, token: "dev.secret")
        XCTAssertNil(plain.handshakeRequest(url: url).value(forHTTPHeaderField: "Grouper-Relay"))
    }

    // MARK: Relay API

    func testDetectionAcceptsOnlyJSONWithSignedIn() async throws {
        var body = #"{"signed_in":false}"#
        var status = 200
        let session = RelayStub.session { _ in (status, body) }
        let relay = try RelayClient(baseURL: "https://relay.test", session: session)

        let signedOut = try await relay.status()
        XCTAssertEqual(signedOut, RelayStatus(signedIn: false))
        body = #"{"signed_in":true,"email":"a@example.com","hub_connected":true,"hub_id":"h1"}"#
        let signedIn = try await relay.status()
        XCTAssertEqual(signedIn, RelayStatus(signedIn: true, email: "a@example.com", hubConnected: true, hubId: "h1"))
        // A hub (or anything else) has no such route or answers something else: not a relay.
        status = 404; body = #"{"error":"not found"}"#
        let missing = try await relay.status()
        XCTAssertNil(missing)
        status = 200; body = "<html>sign in</html>"
        let html = try await relay.status()
        XCTAssertNil(html)
        body = #"{"ok":true}"#
        let other = try await relay.status()
        XCTAssertNil(other)
    }

    func testRelaySignInCalls() async throws {
        let session = RelayStub.session { request in
            switch request.url?.path {
            case "/relay/v1/nonce": return (200, #"{"nonce":"n-123","expires":1795000000}"#)
            case "/relay/v1/sign-in":
                return (200, #"{"relay_token":"rt_new","email":"a@example.com","hub_connected":false,"expires":1795000000}"#)
            default: return (404, "{}")
            }
        }
        let relay = try RelayClient(baseURL: "https://relay.test", session: session)
        let nonce = try await relay.nonce()
        XCTAssertEqual(nonce.nonce, "n-123")
        let result = try await relay.signIn(idToken: "eyJ.id.token", relayNonce: nonce.nonce)
        XCTAssertEqual(result.relayToken, "rt_new")
        XCTAssertEqual(result.hubConnected, false)

        let post = try XCTUnwrap(RelayStub.requests.first { $0.path == "/relay/v1/sign-in" })
        let sent = try XCTUnwrap(JSONSerialization.jsonObject(with: post.body) as? [String: String])
        XCTAssertEqual(sent, ["id_token": "eyJ.id.token", "relay_nonce": "n-123", "client": "app"])
        XCTAssertNil(post.relay, "signing in sends no relay token")
    }

    // MARK: Relay errors

    func testRelayErrorsAreToldApartFromTheHubs() async throws {
        let relay401 = HubClientError.map(status: 401, data: Data(#"{"error":"sign in to the relay","relay":true}"#.utf8))
        XCTAssertEqual(relay401, .relayUnauthorized("sign in to the relay"))
        let relay503 = HubClientError.map(status: 503, data: Data(#"{"error":"hub offline","relay":true}"#.utf8))
        XCTAssertEqual(relay503, .hubOffline("hub offline"))
        // The hub's own errors keep their meaning.
        XCTAssertEqual(HubClientError.map(status: 401, data: Data(#"{"error":"bad token"}"#.utf8)), .unauthorized("bad token"))
        XCTAssertEqual(HubClientError.map(status: 503, data: Data(#"{"error":"moving"}"#.utf8)), .server(status: 503, message: "moving"))
        XCTAssertEqual(HubClientError.map(status: 403, data: Data(#"{"error":"no","relay":true}"#.utf8)), .forbidden("no"))

        XCTAssertEqual(relay503.relayProblem, "Your hub is offline. Check that Grouper is running on the computer that hosts it.")
        XCTAssertEqual(relay401.relayProblem, "Your sign-in to the Grouper relay expired. Sign in again.")
        XCTAssertNil(HubClientError.unauthorized("x").relayProblem)

        let session = RelayStub.session { _ in (503, #"{"error":"hub offline","relay":true}"#) }
        let hub = try HubClient(baseURL: "https://relay.test", token: "dev.secret", relayToken: "rt_abc", session: session)
        do {
            _ = try await hub.conversations()
            XCTFail("expected the relay's 503")
        } catch {
            XCTAssertEqual(error as? HubClientError, .hubOffline("hub offline"))
        }
    }
}

/// Answers requests from a closure, and records them (path, Authorization, Grouper-Relay, body).
private final class RelayStub: URLProtocol, @unchecked Sendable {
    struct Seen { let path: String; let authorization: String?; let relay: String?; let body: Data }

    nonisolated(unsafe) static var handler: ((URLRequest) -> (Int, String))?
    nonisolated(unsafe) static var requests: [Seen] = []

    static func session(_ handler: @escaping (URLRequest) -> (Int, String)) -> URLSession {
        self.handler = handler
        requests = []
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [RelayStub.self]
        return URLSession(configuration: configuration)
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        let body = request.httpBody ?? request.httpBodyStream.map(Self.read) ?? Data()
        Self.requests.append(Seen(path: request.url?.path ?? "",
                                  authorization: request.value(forHTTPHeaderField: "Authorization"),
                                  relay: request.value(forHTTPHeaderField: "Grouper-Relay"), body: body))
        let (status, text) = Self.handler?(request) ?? (500, "")
        let response = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: "HTTP/1.1",
                                       headerFields: ["Content-Type": "application/json"])!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(text.utf8))
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {}

    private static func read(_ stream: InputStream) -> Data {
        stream.open()
        defer { stream.close() }
        var data = Data()
        var buffer = [UInt8](repeating: 0, count: 4096)
        while stream.hasBytesAvailable {
            let count = stream.read(&buffer, maxLength: buffer.count)
            if count <= 0 { break }
            data.append(buffer, count: count)
        }
        return data
    }
}
